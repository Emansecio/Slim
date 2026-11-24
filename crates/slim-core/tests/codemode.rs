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
fn code_mode_program_reads_searches_and_writes_returning_only_its_result() {
    let root = TempRoot::new("codemode-code");
    std::fs::write(
        root.path().join("notes.txt"),
        "INTERMEDIATE_MARKER first\nTODO: ship\n",
    )
    .unwrap();
    let (client, provider) = provider(
        vec![(
            "codemode",
            json!({"code": r#"
            const [notes, hits] = await Promise.all([
                slim.read({path: 'notes.txt'}),
                slim.search({query: 'TODO'}),
            ]);
            await slim.write({path: 'summary.txt', content: notes.includes('first') ? 'seen\n' : 'missing\n'});
            console.log('wrote summary');
            const summary = await slim.read({path: 'summary.txt'});
            return {todo: hits.includes('TODO: ship'), summary: summary.includes('seen')};
        "#}),
        )],
        true,
    );
    let repo = JsonlRepo::create(
        root.path().join("session.jsonl"),
        DurableSessionHeader::new("code", "now", root.path().to_string_lossy(), None, None),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        ManualRunJournal::start(
            repo,
            ManualRunSpec::new("one", "attempt", "input", "final", "summarize notes", 0),
        )
        .unwrap(),
    ));
    let mut runtime = Runtime::new();
    runtime.app.set_run_journal(journal.clone());
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "summarize notes",
            OperatingMode::Code,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    let output = &result.tool_results[0];
    assert!(output.success, "{}", output.output);
    let (value, console) = output.output.split_once("\nconsole:\n").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(value).unwrap(),
        json!({"todo": true, "summary": true})
    );
    assert_eq!(console, "wrote summary");
    assert_eq!(
        std::fs::read_to_string(root.path().join("summary.txt")).unwrap(),
        "seen\n"
    );
    let requests = provider.join().unwrap();
    let names: Vec<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    // Workspace tools reach the program only through `codemode`.
    assert!(names.contains(&"codemode"), "{names:?}");
    for workspace_tool in ["read", "list", "search", "write", "patch", "shell", "mcp"] {
        assert!(!names.contains(&workspace_tool), "{names:?}");
    }
    // What the program read stayed in the program.
    assert!(!requests[1].to_string().contains("INTERMEDIATE_MARKER"));
    let facts: Vec<_> = journal
        .lock()
        .unwrap()
        .repo()
        .records()
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Fact { fact, .. } if fact.namespace == "codemode.call.v1" => {
                Some(fact.value.clone())
            }
            _ => None,
        })
        .collect();
    let finished: Vec<_> = facts
        .iter()
        .filter(|fact| fact["status"] != "started")
        .map(|fact| {
            (
                fact["name"].as_str().unwrap(),
                fact["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(facts.len(), 8, "{facts:?}");
    assert_eq!(finished.len(), 4);
    assert!(finished.iter().all(|(_, status)| *status == "completed"));
    assert_eq!(finished[2], ("slim.write", "completed"));
    journal
        .lock()
        .unwrap()
        .finish(ProviderResponse::new("done", None))
        .unwrap();
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

#[test]
fn code_mode_finish_concludes_in_one_turn_and_verifies_goal_assurance() {
    let root = TempRoot::new("codemode-finish");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "pub fn before() {}\n").unwrap();

    let (client, provider) = provider(
        vec![(
            "codemode",
            json!({
                "code": r#"
                    await slim.write({
                        path: 'src/lib.rs',
                        expected: "pub fn before() {}\n",
                        content: "pub fn after() -> usize {\n    1\n}\n"
                    });
                    await slim.shell({ command: 'cargo', args: ['fmt', '--check'] });
                    finish('All green');
                    return { ok: true };
                "#
            }),
        )],
        false,
    );
    let mut runtime = Runtime::new();
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "implement after",
            OperatingMode::Code,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    let requests = provider.join().unwrap();

    assert_eq!(requests.len(), 1);
    assert_eq!(result.turns, 1);
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    let assistant_text: String = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            slim_core::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_text, "All green");
    assert_eq!(
        runtime.conversation().last().map(|m| m.content.as_str()),
        Some("All green")
    );
    assert_eq!(
        runtime
            .app
            .events()
            .iter()
            .filter_map(|event| match event.kind {
                slim_core::EventKind::GoalAssurance { verified } => Some(verified),
                _ => None,
            })
            .next_back(),
        Some(true)
    );
}

#[test]
fn code_mode_governor_stops_stagnant_read_loops_and_read_plus_error_streaks() {
    let root = TempRoot::new("codemode-guards");
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();

    // Part A: repeated identical read-only cells stop via CausalGovernor (NoProgress).
    let (client, provider_a) = provider(
        vec![
            ("codemode", json!({"code": "return await slim.read({ path: 'a.txt' });"})),
            ("codemode", json!({"code": "return await slim.read({ path: 'a.txt' });"})),
            ("codemode", json!({"code": "return await slim.read({ path: 'a.txt' });"})),
            ("codemode", json!({"code": "return await slim.read({ path: 'a.txt' });"})),
        ],
        true,
    );
    let mut runtime = Runtime::new();
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result_a = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "read repeatedly",
            OperatingMode::Code,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    let requests_a = provider_a.join().unwrap();
    assert_eq!(result_a.stop, AgentLoopStop::NoProgress);
    assert_eq!(result_a.turns, 4);
    assert_eq!(requests_a.len(), 5);

    // Part B: 3 zero-effect cells that read then throw the same error stop via LoopGuard.
    let (client, provider_b) = provider(
        vec![
            (
                "codemode",
                json!({"code": "const step = 1;\nawait slim.read({ path: 'a.txt' });\nthrow new TypeError('boom');"}),
            ),
            (
                "codemode",
                json!({"code": "const step = 2;\nawait slim.read({ path: 'a.txt' });\nthrow new TypeError('boom');"}),
            ),
            (
                "codemode",
                json!({"code": "const step = 3;\nawait slim.read({ path: 'a.txt' });\nthrow new TypeError('boom');"}),
            ),
        ],
        true,
    );
    let mut runtime = Runtime::new();
    let result_b = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "fail repeatedly after read",
            OperatingMode::Code,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    let requests_b = provider_b.join().unwrap();
    assert_eq!(result_b.stop, AgentLoopStop::RepeatedFailedTool);
    assert_eq!(result_b.turns, 3);
    assert_eq!(requests_b.len(), 4);
}

#[test]
fn code_mode_finish_yields_when_user_steer_is_pending() {
    let root = TempRoot::new("codemode-finish-steer");
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();
    let inbox = slim_core::runtime::SteerInbox::default();
    let inbox_for_server = inbox.clone();
    let (listener, address) = bind_listener();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..2 {
            let mut stream = accept_within(&listener, Duration::from_secs(8));
            let request = read_http_request(&mut stream);
            requests.push(serde_json::from_str::<Value>(request.split_once("\r\n\r\n").unwrap().1).unwrap());
            if index == 0 {
                // Simulate user sending a steer after turn 0 began.
                inbox_for_server.push("Wait, also check b.txt".into());
                let args = json!({
                    "code": r#"
                        await slim.read({ path: 'a.txt' });
                        finish('premature finish');
                        return { ok: true };
                    "#
                });
                let chunk = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-0","function":{"name":"codemode","arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]});
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
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).unwrap();
    let mut runtime = Runtime::new();
    runtime.set_steer_inbox(inbox);
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "check a.txt",
            OperatingMode::Code,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    let requests = server.join().unwrap();
    // Because a steer arrived during turn 0, finish() must NOT close early;
    // turn 1 must receive the steer message.
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(requests.len(), 2, "expected second turn to handle pending steer");
    assert!(
        requests[1].to_string().contains("Wait, also check b.txt"),
        "second turn request must include delivered steer: {}",
        requests[1]
    );
}

#[test]
fn multiple_codemode_calls_in_same_turn_keep_isolated_effect_summaries() {
    let root = TempRoot::new("codemode-multi-cell-turn");
    std::fs::write(root.path().join("a.txt"), "before\n").unwrap();
    let (listener, address) = bind_listener();
    let server = thread::spawn(move || {
        for index in 0..2 {
            let mut stream = accept_within(&listener, Duration::from_secs(8));
            let _ = read_http_request(&mut stream);
            if index == 0 {
                // First cell mutates a.txt and calls finish('all done'); second cell
                // in the same turn is read-only. Cell 1's mutation & finish must not
                // be clobbered by cell 2.
                let args0 = json!({
                    "code": "await slim.write({ path: 'a.txt', expected: 'before\\n', content: 'after\\n' }); finish('all done'); return 'w';"
                });
                let args1 = json!({
                    "code": "return await slim.read({ path: 'a.txt' });"
                });
                let chunk = json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call-mut","function":{"name":"codemode","arguments":args0.to_string()}},
                    {"index":1,"id":"call-ro","function":{"name":"codemode","arguments":args1.to_string()}}
                ]},"finish_reason":"tool_calls"}]});
                write_sse(&mut stream, &format!("data: {chunk}\n\ndata: [DONE]\n\n"));
                break;
            }
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).unwrap();
    let mut runtime = Runtime::new();
    let executor = tokio::runtime::Runtime::new().unwrap();
    let result = executor
        .block_on(runtime.run_agent_loop(
            &client,
            "edit and read",
            OperatingMode::Code,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    server.join().unwrap();
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.turns, 1, "finish() from first cell should survive second read-only cell");
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(
        runtime.conversation().last().map(|m| m.content.as_str()),
        Some("all done")
    );
}
