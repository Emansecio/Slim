#[path = "../../../tests/support/http_fixture.rs"]
mod http_fixture;
#[path = "../../../tests/support/temp_root.rs"]
mod temp_root;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use http_fixture::{accept_within, bind_listener, read_http_request, write_sse, SSE_FINAL_ANSWER};
use serde_json::{json, Value};
use slim_core::mcp::{McpCancellation, McpManager, McpServerSpec, McpTransport};
use slim_core::process::ExecutableResolver;
use slim_core::provider::{HttpProviderClient, OpenAiCompatibleAdapter, ProviderConfig};
use slim_core::runtime::CancellationToken;
use slim_core::session::{
    DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec,
    ProviderResponse,
};
use slim_core::{AgentLoopConfig, AgentLoopStop, OperatingMode, Runtime};
use temp_root::TempRoot;

fn reply(request: &Value) -> Option<Value> {
    Some(match request["method"].as_str()? {
        "initialize" => {
            json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{}}, "serverInfo":{"name":"fixture","version":"1"}})
        }
        "tools/list" => json!({"tools":[{
            "name":"rows", "description":"List numbered rows with an offset",
            "inputSchema":{"type":"object","properties":{"offset":{"type":"integer"}}},
            "outputSchema":{"type":"object","properties":{"rows":{"type":"array"}}}
        }]}),
        "tools/call" => {
            let offset = request["params"]["arguments"]["offset"]
                .as_u64()
                .unwrap_or(0);
            json!({"content":[{"type":"text","text":"RAW_ROWS_HUMAN_TEXT".repeat(200)}],
                "structuredContent":{"rows":[offset+1, offset+2]}, "isError":false})
        }
        _ => return None,
    })
}

fn http_mcp(
    calls: usize,
    cancel: Option<CancellationToken>,
) -> (McpTransport, thread::JoinHandle<Vec<Value>>) {
    let (listener, address) = bind_listener();
    let worker = thread::spawn(move || {
        let mut received = Vec::new();
        let mut served = 0;
        while served < calls + 3 {
            let mut stream = accept_within(&listener, Duration::from_secs(8));
            // The client's server-to-client GET stream is not one of the
            // scripted exchanges: this server offers none.
            let mut method = [0_u8; 4];
            if stream
                .peek(&mut method)
                .is_ok_and(|read| &method[..read] == b"GET ")
            {
                stream
                    .write_all(
                        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                continue;
            }
            served += 1;
            let text = read_http_request(&mut stream);
            let request: Value =
                serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
            if request["method"] == "tools/call" {
                if let Some(token) = &cancel {
                    token.cancel();
                    thread::sleep(Duration::from_millis(100));
                    received.push(request);
                    break;
                }
            }
            if let Some(result) = reply(&request) {
                let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            } else {
                stream
                    .write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
            received.push(request);
        }
        received
    });
    (
        McpTransport::Http {
            url: format!("http://{address}"),
            headers: BTreeMap::new(),
        },
        worker,
    )
}

fn manager(transport: McpTransport, cwd: &std::path::Path) -> Arc<McpManager> {
    let spec = McpServerSpec {
        name: "fixture".into(),
        transport,
        enabled: true,
        timeout: Duration::from_secs(5),
        options: Default::default(),
    };
    Arc::new(McpManager::new(
        BTreeMap::from([("fixture".into(), spec)]),
        cwd.to_path_buf(),
        ExecutableResolver::default(),
    ))
}

fn provider(
    calls: Vec<(&'static str, Value)>,
    final_answer: bool,
) -> (
    HttpProviderClient<OpenAiCompatibleAdapter>,
    thread::JoinHandle<Vec<Value>>,
) {
    let (listener, address) = bind_listener();
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..calls.len() + usize::from(final_answer) {
            let mut stream = accept_within(&listener, Duration::from_secs(8));
            let request = read_http_request(&mut stream);
            requests.push(serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap());
            if let Some((name, arguments)) = calls.get(index) {
                let chunk = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("call-{index}"),"function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]});
                write_sse(&mut stream, &format!("data: {chunk}\n\ndata: [DONE]\n\n"));
            } else {
                write_sse(&mut stream, SSE_FINAL_ANSWER);
            }
        }
        requests
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    (
        HttpProviderClient::new(adapter, Duration::from_secs(5)).unwrap(),
        worker,
    )
}

fn compose(transport: McpTransport) {
    let root = TempRoot::new("codemode-compose");
    let manager = manager(transport, root.path());
    let (client, provider) = provider(
        vec![
            ("mcp", json!({"server":"fixture","query":"numbered rows"})),
            (
                "codemode",
                json!({"code":r#"
            const first = await tools.call('mcp.fixture.rows', {offset:0});
            const second = await tools.call('mcp.fixture.rows', {offset:2});
            const rows = [...first.rows, ...second.rows];
            store('sum', rows.filter(x => x % 2 === 0).reduce((a,b) => a+b,0));
            return {count:rows.length, sum:load('sum')};
        "#}),
            ),
        ],
        true,
    );
    let repo = JsonlRepo::create(
        root.path().join("session.jsonl"),
        DurableSessionHeader::new("compose", "now", root.path().to_string_lossy(), None, None),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        ManualRunJournal::start(
            repo,
            ManualRunSpec::new("one", "attempt", "input", "final", "compose rows", 0),
        )
        .unwrap(),
    ));
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(manager.clone()));
    runtime.app.set_run_journal(journal.clone());
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "compose rows",
            OperatingMode::Auto,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert!(result.tool_results.iter().all(|result| result.success));
    assert!(result.tool_results[0].output.contains("mcp.fixture.rows"));
    assert_eq!(
        serde_json::from_str::<Value>(&result.tool_results[1].output).unwrap(),
        json!({"count":4,"sum":6})
    );
    let schema: Value = serde_json::from_str(
        &executor
            .block_on(manager.describe("fixture", "rows"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        schema["outputSchema"]["properties"]["rows"]["type"],
        "array"
    );
    let requests = provider.join().unwrap();
    let names: Vec<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"codemode"));
    assert!(!names.contains(&"mcp.fixture.rows"));
    assert!(!requests[2].to_string().contains("RAW_ROWS_HUMAN_TEXT"));
    let facts: Vec<_> = journal
        .lock()
        .unwrap()
        .repo()
        .records()
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Fact { fact, .. } if fact.namespace == "codemode.call.v1" => {
                Some(fact.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(facts.len(), 4);
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.value["status"] == "completed")
            .count(),
        2
    );
    assert!(facts
        .iter()
        .all(|fact| fact.value["parent_call_id"] == "call-1"));
    journal
        .lock()
        .unwrap()
        .finish(ProviderResponse::new("done", None))
        .unwrap();
    executor.block_on(manager.disconnect_all());
}

#[test]
fn composes_http_calls_without_putting_intermediate_payloads_in_model_context() {
    let (transport, worker) = http_mcp(2, None);
    compose(transport);
    let received = worker.join().unwrap();
    assert_eq!(
        received
            .iter()
            .filter(|request| request["method"] == "tools/call")
            .count(),
        2
    );
}

#[test]
fn composes_stdio_calls_and_stops_the_subprocess() {
    compose(McpTransport::Stdio {
        command: std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        args: vec![
            "--ignored".into(),
            "--exact".into(),
            "codemode_stdio_fixture".into(),
            "--nocapture".into(),
        ],
        env: BTreeMap::new(),
    });
}

#[test]
#[ignore = "controlled MCP subprocess"]
fn codemode_stdio_fixture() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if let Some(result) = reply(&request) {
            writeln!(
                stdout,
                "{}",
                json!({"jsonrpc":"2.0","id":request["id"],"result":result})
            )
            .unwrap();
            stdout.flush().unwrap();
        }
    }
}

#[test]
fn nested_calls_consume_both_turn_and_run_budgets() {
    for (turn_cap, total_cap, first_count, second_count, expected_calls, first_ok, second_ok) in
        [(3, 16, 3, 1, 3, false, true), (32, 4, 1, 2, 2, true, false)]
    {
        let root = TempRoot::new("codemode-budget");
        let (transport, worker) = http_mcp(expected_calls, None);
        let manager = manager(transport, root.path());
        let code = |count| json!({"code":format!("for(let i=0;i<{count};i++) await tools.call('mcp.fixture.rows', {{}}); return 1;")});
        let (client, provider) = provider(
            vec![
                ("codemode", code(first_count)),
                ("codemode", code(second_count)),
            ],
            true,
        );
        let mut runtime = Runtime::new();
        runtime.set_mcp_manager(Some(manager.clone()));
        let executor = tokio::runtime::Runtime::new().unwrap();
        let result = executor
            .block_on(runtime.run_agent_loop(
                &client,
                "budget",
                OperatingMode::Auto,
                root.path(),
                0,
                AgentLoopConfig {
                    max_mutating_tool_calls: turn_cap,
                    max_total_tool_calls: total_cap,
                    ..AgentLoopConfig::default()
                },
            ))
            .unwrap();
        assert_eq!(result.tool_results[0].success, first_ok);
        assert_eq!(result.tool_results[1].success, second_ok);
        assert!(result
            .tool_results
            .iter()
            .any(|result| result.output.contains("tool budget exhausted")));
        provider.join().unwrap();
        let requests = worker.join().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "tools/call")
                .count(),
            expected_calls
        );
        executor.block_on(manager.disconnect_all());
    }
}

#[test]
fn global_discovery_does_not_connect_disconnected_servers() {
    let root = TempRoot::new("codemode-lazy-search");
    let manager = manager(
        McpTransport::Http {
            url: "http://127.0.0.1:1".into(),
            headers: BTreeMap::new(),
        },
        root.path(),
    );
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result = executor
        .block_on(manager.search_tools_cancellable(None, "rows", 0, McpCancellation::new()))
        .into_result()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["tools"],
        json!([])
    );
    assert!(manager.list_servers().contains("disconnected"));
}

#[test]
fn cancelling_a_sent_mcp_call_stops_the_cell_without_replay() {
    let root = TempRoot::new("codemode-cancel");
    let token = CancellationToken::new();
    let (transport, worker) = http_mcp(1, Some(token.clone()));
    let manager = manager(transport, root.path());
    let (client, provider) = provider(
        vec![(
            "codemode",
            json!({"code":
                "try { await tools.call('mcp.fixture.rows', {}); } catch(e) {} return await tools.call('mcp.fixture.rows', {});"
            }),
        )],
        false,
    );
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(manager.clone()));
    runtime.set_cancellation_token(token.clone());
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "cancel a cell",
            OperatingMode::Auto,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    executor.block_on(token.wait_for_native_work());
    provider.join().unwrap();
    let requests = worker.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "tools/call")
            .count(),
        1
    );
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(&event.kind,
        slim_core::EventKind::ToolOutput { output, .. } if output.contains("uncertain"))));
    executor.block_on(manager.disconnect_all());
}
