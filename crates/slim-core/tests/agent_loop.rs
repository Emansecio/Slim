#[path = "../../../tests/support/budget_finalization.rs"]
mod budget_finalization;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use slim_core::context::{CompactionHandle, CompactionStatus};
use slim_core::provider::{
    AnthropicAdapter, HttpProviderClient, OpenAiCodexAdapter, OpenAiCompatibleAdapter,
    ProviderAdapter, ProviderConfig, ProviderError, ProviderMessage,
};
use slim_core::runtime::{AgentLoopConfig, AgentLoopStop, CancellationToken};
use slim_core::{
    interaction_route, CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelMeta,
    CodeIntelOutcome, CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery,
    CodeIntelligence, EventKind, InteractionRequestId, OperatingMode, ProviderPhase,
    QuestionAnswer, Runtime, SessionEventSender,
};

struct DelayedCodeIntel;

fn code_intel_fixture_outcome(label: &str, elapsed_ms: u64) -> CodeIntelOutcome {
    CodeIntelOutcome {
        meta: CodeIntelMeta {
            server: "fixture".into(),
            state: CodeIntelServerState::Ready,
            completeness: CodeIntelCompleteness::Complete,
            document_version: Some(1),
            stale: false,
            elapsed_ms,
        },
        payload: json!({"kind": "workspace", "query": label, "symbols": []}),
    }
}

#[async_trait]
impl CodeIntelligence for DelayedCodeIntel {
    fn supports_workspace(&self, workspace: &Path) -> bool {
        std::fs::read_to_string(workspace.join("intel-availability.txt"))
            .map_or(true, |value| value == "on")
    }

    async fn status(&self, _workspace: &Path) -> CodeIntelOutcome {
        code_intel_fixture_outcome("status", 0)
    }

    async fn definition(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        code_intel_fixture_outcome("definition", 0)
    }

    async fn references(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        code_intel_fixture_outcome("references", 0)
    }

    async fn hover(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        code_intel_fixture_outcome("hover", 0)
    }

    async fn symbols(&self, query: &CodeIntelSymbolQuery) -> CodeIntelOutcome {
        let label = query.query.as_deref().unwrap_or("none");
        let delay_ms = match label {
            "first" => 200,
            "second" => 40,
            _ => 120,
        };
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        code_intel_fixture_outcome(label, delay_ms)
    }

    async fn diagnostics(&self, _query: &CodeIntelDiagnosticsQuery) -> CodeIntelOutcome {
        code_intel_fixture_outcome("diagnostics", 0)
    }

    async fn notify_file_changed(&self, _workspace: &Path, _path: &Path, _text: Option<String>) {}
}

fn strip_channel_from_user_items(json: &mut Value, items: &str) {
    const MARKER: &str = "\n\nHarness channel:";
    let Some(array) = json.get_mut(items).and_then(Value::as_array_mut) else {
        return;
    };
    for item in array {
        if item.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(content) = item.get("content").and_then(Value::as_str) {
            if let Some(at) = content.find(MARKER) {
                item["content"] = Value::String(content[..at].into());
            }
        }
    }
}

fn accept_with_deadline(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "fixture accept deadline exceeded"
                );
                thread::yield_now();
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("request timeout");
    let mut request = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let size = stream.read(&mut chunk).expect("request chunk");
        assert!(size > 0, "request ended before its body was complete");
        request.extend_from_slice(&chunk[..size]);
        assert!(
            request.len() <= 2 * 1024 * 1024,
            "fixture request too large"
        );

        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("content-length header");
        if request.len() >= header_end + 4 + content_length {
            return String::from_utf8(request).expect("utf-8 request");
        }
    }
}

#[test]
fn provider_catalog_refreshes_when_tool_work_changes_backend_availability() {
    let root = std::env::temp_dir().join(format!("slim-catalog-refresh-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("workspace");
    std::fs::write(root.join("intel-availability.txt"), "off").expect("initial state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let server = thread::spawn(move || {
        for turn in 0..3 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let wire: Value = serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("JSON");
            let advertised = wire["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .any(|tool| tool["function"]["name"] == "code_intel");
            assert_eq!(advertised, turn == 1, "catalog at turn {turn}");
            let (delta, stop) = if turn == 2 {
                (json!({"content":"done"}), "stop")
            } else {
                let (expected, content) = if turn == 0 {
                    ("off", "on")
                } else {
                    ("on", "off")
                };
                (
                    json!({"tool_calls":[{"index":0,"id":format!("catalog-{turn}"),"function":{"name":"write",
                    "arguments":json!({"path":"intel-availability.txt","content":content,"expected":expected}).to_string()}}]}),
                    "tool_calls",
                )
            };
            let event = json!({"choices":[{"delta":delta,"finish_reason":stop}]});
            let response = format!("data: {event}\n\ndata: [DONE]\n\n");
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).expect("response");
        }
    });
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(&endpoint, "fixture", "fixture"))
            .expect("adapter"),
        Duration::from_secs(3),
    )
    .expect("client");
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(Arc::new(DelayedCodeIntel));
    let result = tokio::runtime::Runtime::new()
        .expect("tokio")
        .block_on(runtime.run_agent_loop(
            &client,
            "Update the project.",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 2);
    assert!(result.tool_results.iter().all(|tool| tool.success));
    std::fs::remove_dir_all(&root).expect("remove own fixture");
}

#[test]
fn deepseek_thinking_replays_exact_scoped_state_after_native_tools() {
    use slim_core::provider::OpenCodeGoAdapter;
    let root = std::env::temp_dir().join(format!("slim-chat-thinking-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("workspace");
    std::fs::write(root.join("source.txt"), "verified source\n").expect("source");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let wire: Value = serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("JSON");
            assert_eq!(wire["thinking"], json!({"type":"enabled"}));
            assert_eq!(wire["reasoning_effort"], "high");
            let deltas = if turn == 0 {
                vec![
                    json!({"reasoning_content":"state fixture-"}),
                    json!({"reasoning_content":"key kept intact"}),
                    json!({"tool_calls":[{"index":0,"id":"read-1","function":{"name":"read","arguments":"{\"path\":\"source.txt\"}"}}]}),
                ]
            } else {
                let history = wire["messages"].as_array().expect("messages");
                let assistant = history
                    .iter()
                    .find(|m| m["role"] == "assistant")
                    .expect("assistant");
                assert_eq!(
                    assistant["reasoning_content"],
                    "state fixture-key kept intact"
                );
                assert_eq!(assistant["tool_calls"][0]["id"], "read-1");
                assert!(history.last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("verified source"));
                vec![
                    json!({"reasoning_content":"final state"}),
                    json!({"content":"done"}),
                ]
            };
            let mut response = String::new();
            for delta in deltas {
                response.push_str(&format!(
                    "data: {}\n\n",
                    json!({"choices":[{"delta":delta,"finish_reason":null}]})
                ));
            }
            response.push_str(&format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"delta":{},"finish_reason":if turn == 0 {"tool_calls"} else {"stop"}}]})));
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).expect("response");
        }
    });
    let adapter =
        OpenCodeGoAdapter::new(&endpoint, "deepseek-v4-flash", "fixture-key", Some("high"))
            .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .expect("tokio")
        .block_on(runtime.run_agent_loop(
            &client,
            "Read source.txt.",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert!(result.tool_results[0].success);
    let history = runtime.conversation();
    assert!(history[1].chat_reasoning.is_some());
    assert!(history.last().unwrap().chat_reasoning.is_some());
    assert!(!format!("{history:?}").contains("fixture-key"));
    assert!(!format!("{:?}", runtime.app.events()).contains("fixture-key"));
    assert!(!slim_core::context::has_compactable_history(&history[..3]));
    for (endpoint, model, key) in [
        (endpoint.as_str(), "deepseek-v4-flash", "different-key"),
        (endpoint.as_str(), "deepseek-v4-pro", "fixture-key"),
        (
            "https://elsewhere.invalid/v1",
            "deepseek-v4-flash",
            "fixture-key",
        ),
    ] {
        let foreign = OpenCodeGoAdapter::new(endpoint, model, key, Some("high")).unwrap();
        let request = foreign
            .prepare_messages_request_with_tools_checked(history, &[])
            .unwrap();
        let wire: Value = serde_json::from_slice(request.body()).unwrap();
        assert!(wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .all(|m| m["reasoning_content"] == ""));
    }
    std::fs::remove_dir_all(&root).expect("remove own fixture");
}

#[test]
fn compact_tool_results_preserve_a_complete_task_across_provider_wires() {
    use slim_core::provider::{
        ClinePassAdapter, CommandCodeAdapter, OpenCodeGoAdapter, ProviderKind, XaiAdapter,
    };

    let root = std::env::temp_dir().join(format!("slim-token-economy-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).expect("src");
    let paths = (0..20)
        .map(|index| Path::new("src").join(format!("ação-{index:02}.rs")))
        .collect::<Vec<_>>();
    let line = "fn reconstruir_contexto() { persistir_checkpoint(); }";
    let source = format!("{line}\n").repeat(20);
    for (index, path) in paths.iter().enumerate() {
        std::fs::write(root.join(path), if index == 0 { &source } else { "" }).expect("file");
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let server = thread::spawn(move || {
        let mut captures = Vec::new();
        for turn in 0..4 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let body = request.split_once("\r\n\r\n").expect("body").1.to_owned();
            let wire: Value = serde_json::from_str(&body).expect("json");
            let (delta, stop) = if turn == 3 {
                let text = wire["messages"]
                    .as_array()
                    .expect("messages")
                    .last()
                    .expect("read")["content"]
                    .as_str()
                    .expect("text");
                assert_eq!(text, format!("{line}\n").repeat(20));
                (
                    json!({"content":"Found and read the implementation."}),
                    "stop",
                )
            } else {
                let (name, args) = match turn {
                    0 => ("list", json!({"path":"src"})),
                    1 => (
                        "search",
                        json!({"path":"src","patterns":["reconstruir_contexto","persistir_checkpoint"]}),
                    ),
                    _ => {
                        // Use the returned list entry verbatim, as the next tool argument.
                        let listing = wire["messages"]
                            .as_array()
                            .expect("messages")
                            .iter()
                            .find(|message| message["role"] == "tool" && message["name"] == "list")
                            .expect("list result");
                        let path = listing["content"]
                            .as_str()
                            .expect("list")
                            .lines()
                            .next()
                            .expect("entry");
                        ("read", json!({"path":path}))
                    }
                };
                (
                    json!({"tool_calls":[{"index":0,"id":format!("economy-{turn}"),"function":{"name":name,"arguments":args.to_string()}}]}),
                    "tool_calls",
                )
            };
            captures.push(body);
            let event = json!({"choices":[{"delta":delta,"finish_reason":stop}]});
            let response = format!("data: {event}\n\ndata: [DONE]\n\n");
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).expect("response");
        }
        captures
    });
    let config = ProviderConfig::openai(&endpoint, "fixture-model", "fixture-key");
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            &endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("chat"),
        Duration::from_secs(3),
    )
    .expect("client");
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .expect("tokio")
        .block_on(runtime.run_agent_loop(
            &client,
            "Locate and read the implementation. Preserve files.",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    let captures = server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.turns, 4);
    assert_eq!(result.tool_results.len(), 3);
    assert!(result
        .tool_results
        .iter()
        .all(|tool| tool.success && tool.artifact.is_none()));
    assert_eq!(
        std::fs::read_to_string(root.join(&paths[0])).expect("source"),
        source
    );
    let snapshot_chars = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::ContextSnapshot {
                serialized_chars, ..
            } => Some(serialized_chars),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        snapshot_chars,
        captures
            .iter()
            .map(|body| body.chars().count() as u64)
            .collect::<Vec<_>>()
    );

    // Reconstruct only the old renderings; keep requests, arguments and task
    // sequence identical. This is a byte comparison, not model-quality usage.
    let canonical = std::fs::canonicalize(&root).expect("canonical");
    let old_list = paths
        .iter()
        .map(|path| canonical.join(path).display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let new_list = &result.tool_results[0].output;
    assert_eq!(
        new_list,
        &paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut old_hits = Vec::new();
    for number in 1..=20 {
        for (index, pattern) in ["reconstruir_contexto", "persistir_checkpoint"]
            .iter()
            .enumerate()
        {
            old_hits.push(format!(
                "[pattern {}: {pattern}] {}:{number}: {line}\n",
                index + 1,
                paths[0].display()
            ));
        }
    }
    let new_search = &result.tool_results[1].output;
    let footer = new_search.rsplit_once('\n').expect("skip footer").1;
    let old_search = format!("{}\n{footer}", old_hits.join("\n"));
    println!(
        "tool content: list before={} after={}; search before={} after={} bytes",
        old_list.len(),
        new_list.len(),
        old_search.len(),
        new_search.len()
    );
    let history = runtime.conversation();
    let mut old_history = history.to_vec();
    old_history[2].content.clone_from(&old_list);
    old_history[4].content.clone_from(&old_search);
    let old_read = (1..=20)
        .map(|number| format!("{number}: {line}\n"))
        .collect::<String>();
    old_history[6].content.clone_from(&old_read);
    let tools = runtime.advertised_tool_definitions(OperatingMode::Auto);
    let mut adapters: Vec<Box<dyn ProviderAdapter>> = vec![
        Box::new(OpenAiCompatibleAdapter::new(config).expect("chat")),
        Box::new(
            OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
                &endpoint,
                "gpt-5.6-terra",
                "fixture-key",
                "fixture-account",
            ))
            .expect("codex"),
        ),
        Box::new(
            AnthropicAdapter::new(ProviderConfig::anthropic(
                &endpoint,
                "claude-sonnet-4-6",
                "fixture-key",
            ))
            .expect("messages"),
        ),
        Box::new(XaiAdapter::new(&endpoint, "grok-4.5", "fixture-key", Some("high")).expect("xai")),
        Box::new(
            ClinePassAdapter::new(
                &endpoint,
                "cline-pass/qwen3.7-max",
                "fixture-key",
                Some("high"),
            )
            .expect("cline"),
        ),
    ];
    for model in ["deepseek-v4-flash", "gpt-5.6-luna", "minimax-m3"] {
        adapters.push(Box::new(
            OpenCodeGoAdapter::new(&endpoint, model, "fixture-key", Some("high")).expect("go"),
        ));
    }
    for model in ["gpt-5.6-sol", "claude-sonnet-4-6"] {
        adapters.push(Box::new(
            CommandCodeAdapter::new(&endpoint, model, "fixture-key", Some("high"))
                .expect("command"),
        ));
    }
    for adapter in adapters {
        let mut before_bytes = 0;
        let mut after_bytes = 0;
        for (turn, length) in [1, 3, 5, 7].into_iter().enumerate() {
            let before = adapter
                .prepare_messages_request_with_tools_checked(&old_history[..length], &tools)
                .expect("before");
            let after = adapter
                .prepare_messages_request_with_tools_checked(&history[..length], &tools)
                .expect("after");
            let mut before_json: Value =
                serde_json::from_slice(before.body()).expect("before json");
            let after_json: Value = serde_json::from_slice(after.body()).expect("after json");
            // Restore only the changed tool-result fields and compare all
            // other wire content, including IDs, schemas and native cache intent.
            let items = if adapter.wire_kind() == ProviderKind::OpenAiCodex {
                "input"
            } else {
                "messages"
            };
            for item in before_json[items].as_array_mut().expect("items") {
                let output = match adapter.wire_kind() {
                    ProviderKind::OpenAiCodex if item["type"] == "function_call_output" => {
                        Some(&mut item["output"])
                    }
                    ProviderKind::Anthropic if item["content"][0]["type"] == "tool_result" => {
                        Some(&mut item["content"][0]["content"])
                    }
                    ProviderKind::OpenAiCompatible if item["role"] == "tool" => {
                        Some(&mut item["content"])
                    }
                    _ => None,
                };
                if let Some(output) = output {
                    if output.as_str() == Some(&old_list) {
                        *output = json!(new_list);
                    }
                    if output.as_str() == Some(&old_search) {
                        *output = json!(new_search);
                    }
                    if output.as_str() == Some(&old_read) {
                        *output = json!(source);
                    }
                }
            }
            assert_eq!(before_json, after_json, "{:?} turn {turn}", adapter.kind());
            if adapter.kind() == ProviderKind::OpenAiCompatible {
                let mut captured: Value =
                    serde_json::from_slice(captures[turn].as_bytes()).expect("captured json");
                strip_channel_from_user_items(&mut captured, items);
                assert_eq!(
                    after_json, captured,
                    "prepared body is the actual HTTP body"
                );
            }
            before_bytes += before.body().len();
            after_bytes += after.body().len();
        }
        assert!(after_bytes < before_bytes);
        println!(
            "{:?}/{:?}/{}: requests=4 before={} after={} avoided={} bytes",
            adapter.kind(),
            adapter.wire_kind(),
            adapter.model(),
            before_bytes,
            after_bytes,
            before_bytes - after_bytes
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn patch_recovery_returns_locations_and_crlf_receipt_to_the_next_request() {
    let root = std::env::temp_dir().join(format!("slim-patch-recovery-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("fixture.txt"), "header\r\nsame\r\nsame\r\ntail").expect("fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for turn in 0..4 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let wire: Value = serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("json");
            let messages = wire["messages"].as_array().expect("messages");
            if turn > 0 {
                let output = messages.last().expect("tool result");
                assert_eq!(output["role"], "tool");
                assert_eq!(output["tool_call_id"], format!("edit-{}", turn - 1));
                let text = output["content"].as_str().expect("content");
                match turn {
                    1 => assert_eq!(text, "header\r\nsame\r\nsame\r\ntail"),
                    2 => {
                        assert!(text.contains("lines 2, 3"), "{text}");
                        assert!(text.contains("unchanged"), "{text}");
                    }
                    3 => {
                        assert!(text.contains("fixture.txt:1"), "{text}");
                        assert!(text.contains("LF input normalized to CRLF"), "{text}");
                        let call = &messages[messages.len() - 2]["tool_calls"][0];
                        let args: Value = serde_json::from_str(
                            call["function"]["arguments"].as_str().expect("arguments"),
                        )
                        .expect("args");
                        assert_eq!(args["expected"], "header\nsame\n");
                        assert_eq!(args["replacement"], "header\nnew\n");
                    }
                    _ => unreachable!(),
                }
            }
            let (delta, stop) = if turn == 3 {
                (json!({"content":"Edited the first occurrence."}), "stop")
            } else {
                let (name, args) = match turn {
                    0 => ("read", json!({"path":"fixture.txt"})),
                    1 => (
                        "patch",
                        json!({"path":"fixture.txt", "expected":"same\n", "replacement":"new\n"}),
                    ),
                    _ => (
                        "patch",
                        json!({"path":"fixture.txt", "expected":"header\nsame\n", "replacement":"header\nnew\n"}),
                    ),
                };
                (
                    json!({"tool_calls":[{"index":0,"id":format!("edit-{turn}"),"function":{"name":name,"arguments":args.to_string()}}]}),
                    "tool_calls",
                )
            };
            let event = json!({"choices":[{"delta":delta}]});
            let terminal = json!({"choices":[{"delta":{},"finish_reason":stop}]});
            let body = format!("data: {event}\n\ndata: {terminal}\n\ndata: [DONE]\n\n");
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).expect("response");
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "Edit the first occurrence",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.turns, 4);
    assert_eq!(
        result
            .tool_results
            .iter()
            .map(|result| result.success)
            .collect::<Vec<_>>(),
        [true, false, true]
    );
    assert_eq!(
        std::fs::read(root.join("fixture.txt")).expect("bytes"),
        b"header\r\nnew\r\nsame\r\ntail"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn superseded_read_output_is_elided_from_later_requests() {
    let root = std::env::temp_dir().join(format!("slim-elision-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("fixture.txt"),
        "original bytes that will be overwritten entirely\n".repeat(4),
    )
    .expect("fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for turn in 0..3 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let wire: Value = serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("json");
            let messages = wire["messages"].as_array().expect("messages");
            if turn == 2 {
                let read_result = messages
                    .iter()
                    .find(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "read-0"
                    })
                    .expect("retained read result");
                assert_eq!(
                    read_result["content"].as_str().expect("content"),
                    "[superseded read output elided; fixture.txt was overwritten by a later write]"
                );
            }
            let (delta, stop) = match turn {
                0 => (
                    json!({"tool_calls":[{"index":0,"id":"read-0","function":{"name":"read","arguments":"{\"path\":\"fixture.txt\"}"}}]}),
                    "tool_calls",
                ),
                1 => (
                    json!({"tool_calls":[{"index":0,"id":"write-1","function":{"name":"write","arguments":"{\"path\":\"fixture.txt\",\"content\":\"replaced\\n\"}"}}]}),
                    "tool_calls",
                ),
                _ => (json!({"content":"Done."}), "stop"),
            };
            let event = json!({"choices":[{"delta":delta}]});
            let terminal = json!({"choices":[{"delta":{},"finish_reason":stop}]});
            let body = format!("data: {event}\n\ndata: {terminal}\n\ndata: [DONE]\n\n");
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).expect("response");
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "Replace the fixture",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.turns, 3);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn ask_question_returns_the_selected_answer_to_the_provider_in_the_same_loop() {
    question_round_trip(false);
}

#[test]
fn truncation_after_question_recovers_without_asking_again() {
    question_round_trip(true);
}

fn question_round_trip(truncate_after_answer: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first_stream = accept_with_deadline(&listener);
        let mut first_request = [0_u8; 64 * 1024];
        let first_size = first_stream
            .read(&mut first_request)
            .expect("first request");
        let first_request = String::from_utf8_lossy(&first_request[..first_size]);
        assert!(first_request.contains("\"name\":\"ask_question\""));
        assert!(first_request.contains("Harness channel: Auto, interactive"));
        assert!(!first_request.contains("unattended"));
        let tool_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "question-call-1",
                        "function": {
                            "name": "ask_question",
                            "arguments": json!({
                                "question": "Which module should I change?",
                                "options": [
                                    {"label": "core", "description": "Change the runtime"},
                                    {"label": "tui", "description": "Change only the interface"}
                                ]
                            }).to_string()
                        }
                    }]
                }
            }]
        });
        let first_body = format!(
            "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let first_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            first_body.len(),
            first_body
        );
        first_stream
            .write_all(first_response.as_bytes())
            .expect("first response");

        let mut second_stream = accept_with_deadline(&listener);
        let mut second_request = [0_u8; 64 * 1024];
        let second_size = second_stream
            .read(&mut second_request)
            .expect("second request");
        let second_request = String::from_utf8_lossy(&second_request[..second_size]);
        assert!(second_request.contains("question-call-1"));
        assert!(second_request.contains("\\\"answer\\\":\\\"core\\\""));
        assert!(second_request.contains("\\\"source\\\":\\\"option\\\""));
        assert!(second_request.contains("\\\"option_index\\\":0"));
        if truncate_after_answer {
            let body = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Planning the selected change\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
            second_stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).expect("truncated response");
            drop(second_stream);
            second_stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut second_stream);
            let wire: Value = serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("JSON");
            assert_eq!(wire["max_tokens"], 16_384);
            let messages = wire["messages"].as_array().expect("messages");
            assert_eq!(messages.iter().filter(|m| m["role"] == "tool").count(), 1);
            assert!(request.contains("question-call-1"));
            assert!(request.contains("\\\"answer\\\":\\\"core\\\""));
            assert!(messages.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("one small next step"));
        }
        let final_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Selected core\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let final_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            final_body.len(),
            final_body
        );
        second_stream
            .write_all(final_response.as_bytes())
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (route, responder) = interaction_route();
    let answerer = thread::spawn(move || {
        let request_id = InteractionRequestId::new("question-call-1").expect("request id");
        let answer = QuestionAnswer::option(0, "core").expect("answer");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match responder.answer(request_id.clone(), answer.clone()) {
                Ok(()) => break,
                Err(slim_core::InteractionError::StaleRequest { .. })
                    if Instant::now() < deadline =>
                {
                    thread::yield_now();
                }
                Err(error) => panic!("answer question: {error}"),
            }
        }
    });
    let mut runtime = Runtime::new();
    runtime.set_interaction_route(route);
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "change the right module",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: if truncate_after_answer { 3 } else { 2 },
                context_window_tokens: 128_000,
                max_mutating_tool_calls: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    answerer.join().expect("answerer");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        result.tool_results[0].output,
        r#"{"answer":"core","source":"option","option_index":0}"#
    );
    let preparing_position = runtime
        .app
        .events()
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::ProviderPhase {
                    phase: ProviderPhase::PreparingTool,
                    detail: Some(name),
                    ..
                } if name == "ask_question"
            )
        })
        .expect("OpenAI named tool fragment must announce preparation");
    let started_position = runtime
        .app
        .events()
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::ToolStarted { name, .. } if name == "ask_question"
            )
        })
        .expect("tool started");
    assert!(preparing_position < started_position);
    let event_kinds = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { name, .. } if name == "ask_question" => Some("started"),
            EventKind::QuestionRequired { request_id, .. } if request_id == "question-call-1" => {
                Some("question")
            }
            EventKind::InteractionAcknowledged { request_id, .. }
                if request_id == "question-call-1" =>
            {
                Some("acknowledged")
            }
            EventKind::ToolOutput { name, .. } if name == "ask_question" => Some("output"),
            EventKind::ToolFinished { name, .. } if name == "ask_question" => Some("finished"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        event_kinds,
        ["started", "question", "acknowledged", "output", "finished"]
    );
}

#[test]
fn cancelling_while_ask_question_waits_closes_the_tool_and_rejects_a_late_answer() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 64 * 1024];
        let size = stream.read(&mut request).expect("request");
        let request = String::from_utf8_lossy(&request[..size]);
        assert!(request.contains("\"name\":\"ask_question\""));
        let tool_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "question-cancel-1",
                        "function": {
                            "name": "ask_question",
                            "arguments": json!({"question": "Continue?"}).to_string()
                        }
                    }]
                }
            }]
        });
        let body = format!(
            "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let cancellation = CancellationToken::new();
    let (event_sender, event_receiver) = SessionEventSender::bounded(64, cancellation.clone());
    let cancellation_watcher = cancellation.clone();
    let watcher = thread::spawn(move || loop {
        let event = event_receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("question event");
        if matches!(event.kind, EventKind::QuestionRequired { .. }) {
            cancellation_watcher.cancel();
            break;
        }
    });
    let (route, responder) = interaction_route();
    let mut runtime = Runtime::new();
    runtime.app.set_event_sender(event_sender);
    runtime.set_cancellation_token(cancellation);
    runtime.set_interaction_route(route);
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "ask before continuing",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ))
        .expect("cancelled loop");
    watcher.join().expect("watcher");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert!(runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::ToolFinished {
            ref name,
            success: false,
            ..
        } if name == "ask_question"
    )));
    let late = responder.answer(
        InteractionRequestId::new("question-cancel-1").expect("request id"),
        QuestionAnswer::custom("late").expect("answer"),
    );
    assert!(matches!(
        late,
        Err(slim_core::InteractionError::StaleRequest { .. })
    ));
}

#[test]
fn agent_loop_rejects_max_minus_one_before_network_or_snapshot() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_millis(100)).expect("client");
    let mut runtime = Runtime::new();
    let error = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop(
            &client,
            "must not send",
            OperatingMode::Auto,
            std::env::temp_dir(),
            u64::MAX - 1,
            AgentLoopConfig::default(),
        ))
        .expect_err("snapshot and terminal sequences must fit");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert!(runtime.app.events().is_empty());
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn repeated_failed_tool_call_stops_the_multi_turn_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 16 * 1024];
            let size = stream.read(&mut request).expect("request");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.contains("\"tools\""));
            assert!(request.contains("\"name\":\"read\""));
            assert!(request.contains("\"name\":\"shell\""));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            let mut event = json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "failed-read-call",
                            "function": {
                                "name": "read",
                                "arguments": json!({
                                    "path": "missing-file.txt",
                                    "max_lines": 10
                                })
                                .to_string()
                            }
                        }]
                    }
                }]
            });
            event["choices"][0]["delta"]["tool_calls"].as_array_mut().unwrap().push(json!({
                "index": 1, "id": "later-failed-read", "function": {
                    "name": "read", "arguments": json!({"path": "another-missing-file.txt"}).to_string()
                }
            }));
            stream
                .write_all(format!("data: {event}\n\n").as_bytes())
                .expect("tool");
            stream
                .write_all(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n")
                .expect("usage");
            stream
                .write_all(
                    b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                )
                .expect("finish");
        }
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read missing file",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 4,
                max_mutating_tool_calls: 8,
                max_result_bytes: 4096,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::RepeatedFailedTool);
    assert_eq!(result.turns, 2);
    assert_eq!(result.tool_results.len(), 4);
    assert_eq!(
        runtime
            .conversation()
            .iter()
            .filter(|message| message.role == "tool")
            .count(),
        4,
        "every executed call must retain its result before stopping"
    );
    assert_eq!(result.usage.total_input_tokens(), 10);
    assert_eq!(result.usage.output_tokens, 4);
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::TerminalError { .. })));
    assert!(runtime.app.events().iter().any(|event| {
        matches!(
            event.kind,
            EventKind::CausalAnomalyDetected {
                ref batch_id,
                ref call_id,
                kind: slim_core::CausalAnomalyKind::RepeatedFailure,
                action: slim_core::CausalShadowAction::WouldReject,
                occurrence: 1,
                ..
            } if batch_id.starts_with("slim-batch-") && call_id.as_ref() == "failed-read-call"
        )
    }));
}

#[test]
fn structural_rejection_aliases_share_identity_and_valid_call_still_runs() {
    let root =
        std::env::temp_dir().join(format!("slim-structural-rejection-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("source.txt"), "stable\n").expect("source");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let _ = read_http_request(&mut stream);
        let calls = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "invalid-a", "function": {"name": "read", "arguments": json!({"path":"source.txt", "offset":0}).to_string()}},
                        {"index": 1, "id": "invalid-b", "function": {"name": "read", "arguments": json!({"path":"./source.txt", "offset":0}).to_string()}},
                        {"index": 2, "id": "valid-read", "function": {"name": "read", "arguments": json!({"path":"source.txt", "offset":1}).to_string()}}
                    ]
                }
            }]
        });
        let body = format!(
            "data: {calls}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("tool calls");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "repair invalid reads",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::RepeatedFailedTool);
    assert_eq!(result.tool_results.len(), 3);
    assert!(result.tool_results[0]
        .output
        .contains("read offset must be at least 1"));
    assert!(result.tool_results[1]
        .output
        .contains("read offset must be at least 1"));
    assert!(result.tool_results[2].output.contains("stable"));
    assert_eq!(
        std::fs::read_to_string(root.join("source.txt")).expect("source"),
        "stable\n"
    );

    let invalid_fingerprints = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::CausalProgressObserved {
                call_id,
                kind: slim_core::CausalProgressKind::DistinctFailure,
                call_fingerprint,
                ..
            } if call_id.as_ref() == "invalid-a" => Some(call_fingerprint.as_ref()),
            EventKind::CausalAnomalyDetected {
                call_id,
                kind: slim_core::CausalAnomalyKind::RepeatedFailure,
                call_fingerprint,
                ..
            } if call_id.as_ref() == "invalid-b" => Some(call_fingerprint.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(invalid_fingerprints.len(), 2);
    assert_eq!(invalid_fingerprints[0], invalid_fingerprints[1]);
    assert!(!runtime.app.events().iter().any(|event| {
        matches!(
            &event.kind,
            EventKind::CausalBoundaryObserved { call_id, .. }
                if call_id.as_ref() == "invalid-a" || call_id.as_ref() == "invalid-b"
        )
    }));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn tool_limit_blocks_excess_mutating_calls_before_execution() {
    let root = std::env::temp_dir().join(format!("slim-tool-limit-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let first = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "write-1", "function": {"name": "write", "arguments": json!({"path": "first.txt", "content": "first"}).to_string()}},
                        {"index": 1, "id": "write-2", "function": {"name": "write", "arguments": json!({"path": "second.txt", "content": "second"}).to_string()}}
                    ]
                }
            }]
        });
        let body = format!(
            "data: {first}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("response");
        budget_finalization::reject_budget_finalization(&listener);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "write two files",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                max_mutating_tool_calls: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ToolLimit);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        std::fs::read_to_string(root.join("first.txt")).expect("first"),
        "first"
    );
    assert!(!root.join("second.txt").exists());
    let started = runtime
        .app
        .events()
        .iter()
        .filter(|event| matches!(event.kind, EventKind::ToolStarted { .. }))
        .count();
    assert_eq!(started, 1);
    let started_position = runtime
        .app
        .events()
        .iter()
        .position(|event| matches!(event.kind, EventKind::ToolStarted { .. }))
        .expect("tool started");
    let output_position = runtime
        .app
        .events()
        .iter()
        .position(|event| matches!(event.kind, EventKind::ToolOutput { .. }))
        .expect("tool output");
    assert!(started_position < output_position);
    let provider_ids = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ProviderToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(provider_ids, ["write-1", "write-2"]);
    let executor_ids = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted {
                batch_id, call_id, ..
            }
            | EventKind::ToolOutput {
                batch_id, call_id, ..
            }
            | EventKind::ToolFinished {
                batch_id, call_id, ..
            } => Some((batch_id.as_str(), call_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(executor_ids.len(), 3);
    assert!(executor_ids
        .iter()
        .all(|(batch_id, call_id)| !batch_id.is_empty() && *call_id == "write-1"));
    assert!(executor_ids
        .windows(2)
        .all(|pair| pair[0].0 == pair[1].0 && pair[0].1 == pair[1].1));
    let tool_messages = runtime
        .conversation()
        .iter()
        .filter(|msg| msg.role == "tool")
        .count();
    assert_eq!(tool_messages, 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn one_provider_batch_runs_disjoint_mutations_with_ordered_results() {
    let root = std::env::temp_dir().join(format!("slim-serial-tool-order-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let calls = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "write-first", "function": {"name": "write", "arguments": json!({"path": "first.txt", "content": "first"}).to_string()}},
                        {"index": 1, "id": "write-second", "function": {"name": "write", "arguments": json!({"path": "second.txt", "content": "second"}).to_string()}}
                    ]
                }
            }]
        });
        let body = format!(
            "data: {calls}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("response");
        budget_finalization::reject_budget_finalization(&listener);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "write two files",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                max_mutating_tool_calls: 2,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::TurnLimit);
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        std::fs::read_to_string(root.join("first.txt")).expect("first"),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("second.txt")).expect("second"),
        "second"
    );

    let lifecycle = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted {
                batch_id,
                call_id,
                name,
                ..
            } => Some(("start", batch_id.as_str(), call_id.as_str(), name.as_str())),
            EventKind::ToolOutput {
                batch_id,
                call_id,
                name,
                ..
            } => Some(("output", batch_id.as_str(), call_id.as_str(), name.as_str())),
            EventKind::ToolFinished {
                batch_id,
                call_id,
                name,
                ..
            } => Some(("finish", batch_id.as_str(), call_id.as_str(), name.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Disjoint-path mutations form one independent segment: starts announce
    // together, execution overlaps, and results keep source order.
    let batch_id = lifecycle.first().expect("first lifecycle event").1;
    assert_eq!(
        lifecycle
            .iter()
            .take(2)
            .map(|(phase, _, call_id, name)| (*phase, *call_id, *name))
            .collect::<Vec<_>>(),
        [
            ("start", "write-first", "write"),
            ("start", "write-second", "write"),
        ]
    );
    let position = |phase: &str, call_id: &str| {
        lifecycle
            .iter()
            .position(|candidate| *candidate == (phase, batch_id, call_id, "write"))
            .expect("lifecycle event")
    };
    for call_id in ["write-first", "write-second"] {
        assert!(
            position("start", call_id) < position("output", call_id)
                && position("output", call_id) < position("finish", call_id),
            "call {call_id} must complete its own lifecycle in order: {lifecycle:?}"
        );
    }
    assert!(
        result.tool_results[0].output.contains("first")
            && result.tool_results[1].output.contains("second"),
        "results must keep provider source order: {:?}",
        result.tool_results
    );
    let batch_id = lifecycle.first().expect("first lifecycle event").1;
    assert!(!batch_id.is_empty());
    assert!(lifecycle
        .iter()
        .all(|(_, candidate_batch, _, _)| *candidate_batch == batch_id));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn json_diagnostics_from_two_patches_reach_the_next_model_request_together() {
    let root = std::env::temp_dir().join(format!(
        "slim-json-batch-diagnostics-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    for name in ["development.json", "production.json"] {
        std::fs::write(root.join(name), "{\r\n  \"a\": 1,\r\n  \"b\": 2\r\n}\r\n").unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let wire: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let delta = if turn == 0 {
                json!({"tool_calls":[
                    {"index":0,"id":"patch-dev","function":{"name":"patch","arguments":json!({"path":"development.json","expected":"\"a\": 1,","replacement":"\"a\": 1"}).to_string()}},
                    {"index":1,"id":"patch-prod","function":{"name":"patch","arguments":json!({"path":"production.json","edits":[{"expected":"\"a\": 1,","replacement":"\"a\": 1"}]}).to_string()}}
                ]})
            } else {
                let messages = wire["messages"].as_array().unwrap();
                let results = messages
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .collect::<Vec<_>>();
                assert_eq!(results.len(), 2);
                for (result, name) in results.iter().zip(["development.json", "production.json"]) {
                    let content = result["content"].as_str().unwrap();
                    assert!(
                        content.contains(name) && content.contains("JSON syntax diagnostic"),
                        "{content}"
                    );
                    assert!(content.contains("line 3"), "{content}");
                }
                json!({"content":"Both edited files need correction."})
            };
            let finish = if turn == 0 { "tool_calls" } else { "stop" };
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":delta}]}),
                json!({"choices":[{"delta":{},"finish_reason":finish}]})
            );
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).unwrap();
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).unwrap();
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "Edit the two configurations.",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    server.join().unwrap();
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert!(result.tool_results.iter().all(|r| r.success));
    assert!(!result.usage.validated_completion);
    for name in ["development.json", "production.json"] {
        let content = std::fs::read_to_string(root.join(name)).unwrap();
        assert!(content.contains("\r\n"));
        assert!(serde_json::from_str::<Value>(&content).is_err());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn length_stop_blocks_all_tool_side_effects() {
    let root = std::env::temp_dir().join(format!("slim-length-stop-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let destination = root.join("should-not-exist.txt");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let destination_arg = destination.to_string_lossy().to_string();
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let tool = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "length-write-1",
                        "function": {
                            "name": "write",
                            "arguments": json!({
                                "path": destination_arg,
                                "content": "must not be written"
                            }).to_string()
                        }
                    }]
                }
            }]
        });
        let finish = json!({
            "choices": [{"delta": {}, "finish_reason": "length"}]
        });
        let body = format!("data: {tool}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("length");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "write one file",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert!(result.tool_results.is_empty());
    assert!(!destination.exists());
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ToolStarted { .. })));
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::RequestCompleted { failed: true, .. })));
    assert!(runtime.app.events().iter().any(|event| {
        matches!(
            &event.kind,
            EventKind::AssistantEnded { reason } if reason == "[REDACTED]"
        )
    }));
    assert_eq!(result.stop, AgentLoopStop::ProviderTruncated);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn repeated_truncation_stops_after_two_recoveries_and_preserves_usage() {
    bounded_truncation_fixture(false);
}

#[test]
fn rejected_output_growth_recovers_at_original_limit() {
    bounded_truncation_fixture(true);
}

fn bounded_truncation_fixture(reject_growth: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let limits = if reject_growth {
            [4096, 16_384, 4096]
        } else {
            [4096, 16_384, 32_768]
        };
        for (index, limit) in limits.into_iter().enumerate() {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let wire: Value = serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1)
                .expect("JSON");
            assert_eq!(wire["max_tokens"], limit);
            if reject_growth && index == 1 {
                let body = r#"{"error":{"type":"invalid_request_error","message":"max_tokens must be <= 4096"}}"#;
                stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).expect("rejection");
                continue;
            }
            let body = if reject_growth && index == 2 {
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":4096,\"total_tokens\":4196}}\n\ndata: [DONE]\n\n"
            } else {
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Still planning\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":4096,\"total_tokens\":4196}}\n\ndata: [DONE]\n\n"
            };
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).expect("response");
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop(
            &client,
            "Continue the task",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 10,
                context_window_tokens: 128_000,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");
    assert_eq!(
        result.stop,
        if reject_growth {
            AgentLoopStop::ProviderCompleted
        } else {
            AgentLoopStop::ProviderTruncated
        }
    );
    assert_eq!(result.turns, 3);
    assert!(result.tool_results.is_empty());
    assert_eq!(
        result.usage.output_tokens,
        if reject_growth { 2 * 4096 } else { 3 * 4096 }
    );
}

#[test]
fn content_filter_stop_is_not_reported_as_provider_success() {
    let root =
        std::env::temp_dir().join(format!("slim-content-filter-stop-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let body = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        stream.write_all(response.as_bytes()).expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "filtered prompt",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderFiltered);
    assert!(result.tool_results.is_empty());
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ToolStarted { .. })));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn unknown_stop_reason_is_rejected_without_tool_execution() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let body = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"future_reason\"}]}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        stream.write_all(response.as_bytes()).expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let error = tokio_runtime
        .block_on(runtime.run_provider_turn(
            &client,
            "unknown stop",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
        ))
        .expect_err("unknown provider stop must not report success");
    server.join().expect("server");

    assert!(matches!(
        error,
        slim_core::ProviderError::InvalidResponse { message }
            if message.contains("unsupported stop reason")
    ));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ToolStarted { .. })));
}

#[test]
fn max_tokens_stop_blocks_provider_turn_tool_side_effects() {
    let root = std::env::temp_dir().join(format!("slim-max-tokens-turn-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let destination = root.join("should-not-exist.txt");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let destination_arg = destination.to_string_lossy().to_string();
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let tool = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "max-tokens-write-1",
                        "function": {
                            "name": "write",
                            "arguments": json!({
                                "path": destination_arg,
                                "content": "must not be written"
                            }).to_string()
                        }
                    }]
                }
            }]
        });
        let finish = json!({
            "choices": [{
                "delta": {},
                "finish_reason": "  MaX_ToKeNs  "
            }]
        });
        let body = format!("data: {tool}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let error = tokio_runtime
        .block_on(runtime.run_provider_turn(
            &client,
            "write one file",
            OperatingMode::Auto,
            &root,
            1,
        ))
        .expect_err("truncated provider turn must not report success");
    server.join().expect("server");

    assert!(matches!(
        error,
        slim_core::ProviderError::InvalidResponse { message }
            if message == "provider response was truncated"
    ));
    assert!(!destination.exists());
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ToolStarted { .. })));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_subscription_responses_execute_tool_and_send_function_output() {
    let root = std::env::temp_dir().join(format!("slim-codex-loop-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("fixture.txt"), "codex content\n").expect("fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let path = "fixture.txt".to_owned();
    let reasoning = json!({"type":"reasoning", "id":"rs-1", "summary":[], "encrypted_content":"opaque-fixture-token=="});
    let server_reasoning = reasoning;
    let server = thread::spawn(move || {
        let mut first_body = Value::Null;
        for turn in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 32 * 1024];
            let size = stream.read(&mut request).expect("request");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.contains("chatgpt-account-id: account-1"));
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .expect("HTTP body");
            assert!(!body.contains("\"max_output_tokens\""));
            let wire: Value = serde_json::from_str(body).expect("wire");
            if turn == 1 {
                let items = wire["input"].as_array().expect("input");
                assert_eq!(
                    items.iter().find(|item| item["type"] == "reasoning"),
                    Some(&server_reasoning)
                );
                let call = items
                    .iter()
                    .find(|item| item["type"] == "function_call")
                    .expect("call");
                assert_eq!(
                    call["arguments"],
                    json!({"path":path,"content":"codex content\n"}).to_string()
                );
                assert!(items
                    .iter()
                    .any(|item| item["type"] == "function_call_output"
                        && item["call_id"] == "call-1"));
                assert_eq!(wire["instructions"], first_body["instructions"]);
                assert_eq!(wire["tools"], first_body["tools"]);
                assert_eq!(wire["prompt_cache_key"], first_body["prompt_cache_key"]);
            } else {
                first_body = wire;
            }
            let events = if turn == 0 {
                vec![
                    json!({"type":"response.output_item.done","output_index":0,"item":server_reasoning}),
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"write","arguments":""}}),
                    json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":serde_json::json!({"path":path,"content":"codex content\n"}).to_string()}),
                    json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"write","arguments":serde_json::json!({"path":path,"content":"codex content\n"}).to_string()}}),
                    json!({"type":"response.completed","response":{"usage":{"input_tokens":2,"output_tokens":1}}}),
                ]
            } else {
                vec![
                    json!({"type":"response.output_text.delta","delta":"codex done"}),
                    json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}),
                ]
            };
            let body = events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream.write_all(response.as_bytes()).expect("response");
        }
    });
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        format!("http://{address}"),
        "gpt-5.3-codex",
        "fixture-token",
        "account-1",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "write fixture",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    server.join().expect("server");
    assert_eq!(
        std::fs::read_to_string(root.join("fixture.txt")).expect("written"),
        "codex content\n"
    );
    assert!(!format!("{:?}", runtime.app.events()).contains("opaque-fixture-token"));
    assert!(
        !slim_core::context::build_summary_prompt(runtime.conversation())
            .contains("opaque-fixture-token")
    );
    assert!(!format!("{:?}", runtime.conversation()).contains("opaque-fixture-token"));
    assert!(!slim_core::context::has_compactable_history(
        &runtime.conversation()[..3]
    ));
    let same = client
        .adapter()
        .build_messages_request(runtime.conversation());
    assert!(same.body.contains("opaque-fixture-token=="));
    let switched = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        format!("http://{address}"),
        "other-model",
        "other-token",
        "other-account",
    ))
    .expect("switched");
    assert!(!switched
        .build_messages_request(runtime.conversation())
        .body
        .contains("opaque-fixture-token"));
    let chat =
        OpenAiCompatibleAdapter::new(ProviderConfig::openai("http://localhost", "fixture", "key"))
            .expect("chat");
    assert!(!chat
        .build_messages_request(runtime.conversation())
        .body
        .contains("opaque-fixture-token"));
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(result.usage.total_input_tokens(), 5);
    assert_eq!(result.usage.output_tokens, 3);
    assert_eq!(result.usage.tool_calls_executed, 1);
    assert_eq!(
        runtime
            .app
            .events()
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::GoalAssurance { verified } => Some(verified),
                _ => None,
            })
            .next_back(),
        Some(false)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn anthropic_tool_block_is_published_only_after_stop_with_provider_id() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let events = [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"anthropic-call-1","name":"write"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"input_json_delta":{"partial_json":"{\"path\":\"anthropic.txt\",\"content\":\"ok\"}"}}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        ];
        let body = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("response");
    });
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        format!("http://{address}"),
        "claude-fixture",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    tokio_runtime
        .block_on(runtime.run_provider(&client, "write one file", 1))
        .expect("provider");
    server.join().expect("server");
    let preparing_position = runtime
        .app
        .events()
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::ProviderPhase {
                    phase: ProviderPhase::PreparingTool,
                    detail: Some(name),
                    ..
                } if name == "write"
            )
        })
        .expect("preparing tool phase");
    let published_position = runtime
        .app
        .events()
        .iter()
        .position(|event| matches!(event.kind, EventKind::ProviderToolCall { .. }))
        .expect("published tool call");
    assert!(preparing_position < published_position);
    assert_eq!(
        runtime
            .app
            .events()
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ProviderToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.as_str(), name.as_str(), arguments.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![(
            "anthropic-call-1",
            "write",
            r#"{"path":"anthropic.txt","content":"ok"}"#
        )]
    );
}

#[test]
fn incomplete_openai_tool_delta_is_rejected_without_publishing_a_call() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let event = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-incomplete",
                        "function": {"name": "read", "arguments": "{\"path\":"}
                    }]
                }
            }]
        });
        let finish = json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        });
        let body = format!("data: {event}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let error = tokio_runtime
        .block_on(runtime.run_provider(&client, "read", 1))
        .expect_err("incomplete JSON");
    server.join().expect("server");
    assert!(matches!(error, slim_core::ProviderError::MalformedToolCall));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
}

#[test]
fn large_tool_output_is_materialized_and_referenced_in_the_next_turn() {
    let root = std::env::temp_dir().join(format!("slim-agent-artifact-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let source = root.join("large.txt");
    // Complete reads under 64 KiB pass through in full by design; the output
    // must exceed COMPLETE_READ_PROMPT_BYTES to exercise artifact materialization.
    let content = "large-output-".repeat(6000);
    std::fs::write(&source, &content).expect("source");
    let artifact_root = root.join("artifacts");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let source_arg = "large.txt".to_owned();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 16 * 1024];
            let size = stream.read(&mut request).expect("request");
            if turn == 1 {
                assert!(String::from_utf8_lossy(&request[..size]).contains("artifact id="));
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            if turn == 0 {
                let event = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "large-read-call",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": source_arg,
                                        "max_lines": 100
                                    })
                                    .to_string()
                                }
                            }]
                        }
                    }]
                });
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("tool");
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("finish");
            } else {
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("finish");
            }
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::with_artifact_store(&artifact_root).expect("artifacts");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read large file",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 3,
                max_mutating_tool_calls: 4,
                max_result_bytes: 16,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    let handle = result.tool_results[0].artifact.as_ref().expect("handle");
    assert_eq!(
        std::fs::read_to_string(&handle.path).expect("artifact"),
        content
    );
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ArtifactStored { .. })));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn context_below_threshold_sends_only_the_normal_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let size = stream.read(&mut request).expect("request");
        let body = String::from_utf8_lossy(&request[..size]).into_owned();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream.write_all(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ).expect("response");
        body
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "small",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    let body = server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert!(!body.contains("You are a context compactor"));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
    // Optional initial paths are real provider context and must be included
    // in accounting. Compare against the captured wire, not a bare prompt.
    let request_body = body.split_once("\r\n\r\n").expect("HTTP body").1;
    let request = serde_json::from_str::<Value>(request_body).expect("request JSON");
    assert_eq!(request["messages"].as_array().unwrap().len(), 2);
    assert!(request["messages"][1]["content"]
        .as_str()
        .unwrap()
        .starts_with("small"));
    let expected = slim_core::context::AdaptiveTokenEstimator::default().estimate(
        "openai-compatible",
        client.adapter().model(),
        request_body.chars().count() as u64,
    );
    let snapshot = runtime
        .app
        .events()
        .iter()
        .find_map(|event| match event.kind {
            EventKind::ContextSnapshot {
                estimated_tokens,
                tool_schema_bytes: tools_bytes,
                ..
            } => Some((estimated_tokens, tools_bytes)),
            _ => None,
        })
        .expect("context snapshot");
    assert_eq!(snapshot.0, expected);
    assert!(snapshot.1 > 0, "auto tool schema is present");
}

#[test]
fn sole_prompt_above_threshold_but_within_window_is_sent_unchanged() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let size = stream.read(&mut request).expect("read request");
        let body = String::from_utf8_lossy(&request[..size]).into_owned();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
        body
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let prompt = "initial instruction ".repeat(8);
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            &prompt,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: fixed_tokens + 256,
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let request = server.join().expect("server");
    let body = request.split("\r\n\r\n").nth(1).expect("body");
    let body = serde_json::from_str::<Value>(body).expect("json");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    // messages[0] is the native Slim system prompt; the sole user prompt is
    // messages[1] and must be sent unchanged (no compaction).
    assert_eq!(body["messages"][0]["role"], "system");
    let sent = body["messages"][1]["content"].as_str().expect("prompt");
    assert_eq!(slim_core::without_workspace_snapshot(sent), prompt);
    assert!(sent.contains("Harness channel: Auto, unattended"));
    assert!(!sent.contains("Summarize the prior agent transcript"));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
}

#[test]
fn sole_prompt_beyond_window_fails_without_a_provider_call() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_millis(200)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let error = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            &"oversized initial instruction ".repeat(20),
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: 64,
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect_err("oversized sole prompt must fail before sending");

    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { ref message }
            if message.contains("context window") && message.contains("compaction is unavailable")
    ));
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(runtime.app.events().is_empty());
}

#[test]
fn threshold_requests_summary_then_real_turn_with_same_provider_model() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..3 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 32 * 1024];
            let size = stream.read(&mut request).expect("request");
            requests.push(String::from_utf8_lossy(&request[..size]).into_owned());
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            ).expect("headers");
            if index == 0 {
                let event = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "c",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": "missing-file.txt",
                                        "max_lines": 10
                                    }).to_string()
                                }
                            }]
                        }
                    }]
                });
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("summary");
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("tool finish");
            } else if index == 1 {
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"## Goal\\nsummary from fixture\\n## Constraints\\nNone\\n## Progress\\nDone\\n## Blocked\\nNone\\n## Decisions\\nKeep context\\n## Next steps\\nContinue\\n## Critical context\\nFixture\"}}]}\n\n",
                    )
                    .expect("summary");
            } else {
                stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n")
                    .expect("answer");
            }
            stream.write_all(
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ).expect("response");
        }
        requests
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let handle = CompactionHandle::default();
    handle.request_manual("").expect("queue summary protocol");
    runtime.set_compaction_handle(handle);
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "start",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: fixed_tokens + 512,
                context_reserve_tokens: 120,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let requests = server.join().expect("server");

    assert_eq!(requests.len(), 3);
    let bodies = requests
        .iter()
        .map(|request| {
            let body = request.split("\r\n\r\n").nth(1).expect("body");
            serde_json::from_str::<Value>(body).expect("json")
        })
        .collect::<Vec<_>>();
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    let first_user = bodies[0]["messages"][1]["content"]
        .as_str()
        .expect("first user");
    assert_eq!(slim_core::without_workspace_snapshot(first_user), "start");
    assert!(first_user.contains("Harness channel: Auto, unattended"));
    assert_eq!(
        bodies[1]["messages"][0]["content"],
        slim_core::context::COMPACTION_SYSTEM_PROMPT
    );
    assert!(bodies[1]["messages"][1]["content"]
        .as_str()
        .expect("summary transcript")
        .contains("user: start"));
    assert!(!bodies[1]["messages"][1]["content"]
        .as_str()
        .expect("summary transcript")
        .contains("Summarize the prior agent transcript"));
    let resumed_user = bodies[2]["messages"][1]["content"]
        .as_str()
        .expect("resumed user");
    assert_eq!(slim_core::without_workspace_snapshot(resumed_user), "start");
    assert!(
        serde_json::to_string(&bodies[2])
            .expect("third request")
            .contains("Harness channel: Auto, unattended"),
        "compacted follow-up must keep the unattended channel"
    );
    assert!(bodies[2]["messages"][2]["content"]
        .as_str()
        .expect("compacted context")
        .contains("[Compacted context]"));
    // With the native system prompt the compacted request is: system,
    // literal root, compacted context, assistant tool call, tool result.
    assert_eq!(bodies[2]["messages"].as_array().expect("messages").len(), 5);
    assert_eq!(bodies[2]["messages"][3]["tool_calls"][0]["id"], "c");
    assert_eq!(bodies[2]["messages"][4]["role"], "tool");
    assert_eq!(bodies[2]["messages"][4]["name"], "read");
    assert!(bodies.iter().all(|body| body["model"] == "fixture-model"));
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
    let compaction_position = runtime
        .app
        .events()
        .iter()
        .position(|event| matches!(event.kind, EventKind::CompactionCompleted))
        .expect("compaction event");
    let compacting_position = runtime
        .app
        .events()
        .iter()
        .position(|event| {
            matches!(
                event.kind,
                EventKind::ProviderPhase {
                    phase: ProviderPhase::Compacting,
                    ..
                }
            )
        })
        .expect("compacting phase");
    assert!(compacting_position < compaction_position);
    let snapshots = runtime
        .app
        .events()
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event.kind, EventKind::ContextSnapshot { .. }).then_some(index)
        })
        .collect::<Vec<_>>();
    assert!(snapshots.len() >= 3, "one snapshot per provider request");
    assert!(
        snapshots.iter().any(|index| *index < compaction_position),
        "summary snapshot precedes compaction completion"
    );
    assert!(
        snapshots.iter().any(|index| *index > compaction_position),
        "main snapshot follows compaction"
    );
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
}

#[test]
fn manual_compaction_retains_native_execution_facts_without_model_summary() {
    let root = std::env::temp_dir().join(format!(
        "slim-native-facts-compaction-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("workspace");
    let mutation_path = std::fs::canonicalize(&root)
        .expect("canonical workspace")
        .join("written.txt")
        .to_string_lossy()
        .replace('\\', "/");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let handle = CompactionHandle::default();
    let server_handle = handle.clone();
    let server_mutation_path = mutation_path.clone();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for turn in 0..4 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let wire: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").expect("request body").1)
                    .expect("request JSON");

            let body = if turn == 0 {
                let calls = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [
                                {"index": 0, "id": "write-fact", "function": {"name": "write", "arguments": json!({"path": "written.txt", "content": "native fact\n"}).to_string()}},
                                {"index": 1, "id": "validation-fact", "function": {"name": "shell", "arguments": json!({"command": "cargo", "args": ["check", "--offline"]}).to_string()}},
                                {"index": 2, "id": "failure-fact", "function": {"name": "read", "arguments": json!({"path": "missing.txt"}).to_string()}}
                            ]
                        }
                    }]
                });
                let terminal = json!({
                    "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
                });
                server_handle
                    .request_manual("")
                    .expect("manual compaction request");
                format!("data: {calls}\n\ndata: {terminal}\n\ndata: [DONE]\n\n")
            } else if turn == 1 {
                assert_eq!(
                    wire["messages"][0]["content"],
                    slim_core::context::COMPACTION_SYSTEM_PROMPT,
                    "the next iteration must consume the shared manual compaction request"
                );
                let summary = "## Goal\nRecord the requested change.\n## Constraints\nUse the existing workspace.\n## Progress\nThe transcript was compacted.\n## Blocked\nNone.\n## Decisions\nKeep going.\n## Next steps\nReturn the final result.\n## Critical context\nNo additional context.";
                assert!(!summary.contains("execution_facts"));
                assert!(!summary.contains("write-fact"));
                let event = json!({
                    "choices": [{"delta": {"content": summary}, "finish_reason": "stop"}]
                });
                format!("data: {event}\n\ndata: [DONE]\n\n")
            } else if turn == 2 {
                assert_ne!(
                    wire["messages"][0]["content"],
                    slim_core::context::COMPACTION_SYSTEM_PROMPT,
                    "third request must be the resumed model turn"
                );
                let messages = wire["messages"].as_array().expect("messages");
                let compacted = messages
                    .iter()
                    .find_map(|message| {
                        (message["role"] == "user"
                            && message["content"].as_str().is_some_and(|content| {
                                content.starts_with("[Compacted context]\n")
                            }))
                        .then(|| message["content"].as_str().expect("compacted context"))
                    })
                    .expect("compacted context");
                assert!(compacted.contains("[Runtime facts at compaction;"));
                assert!(compacted.contains("execution_facts scope=compaction run_start_seq=1"));
                assert!(compacted.contains(&format!(
                    "mutation path=\"{server_mutation_path}\" revision=1"
                )));
                assert!(compacted.contains("failure "));
                assert!(compacted.contains("call_id=\"failure-fact\""));
                assert!(compacted.contains("pending=true"));
                assert!(compacted.contains("validation "));
                assert!(compacted.contains("call_id=\"validation-fact\""));
                assert!(compacted.contains("success=false"));
                assert!(compacted.contains("validation_revision=1"));
                let read_path = compacted
                    .split_once("[Prior visible transcript: use read on ")
                    .and_then(|(_, suffix)| suffix.split_once(" with offset=1"))
                    .map(|(path, _)| {
                        serde_json::from_str::<String>(path).expect("quoted recovery path")
                    })
                    .expect("recovery read pointer");
                assert!(
                    !Path::new(&read_path).is_absolute(),
                    "recovery pointer must stay workspace-relative: {read_path}"
                );
                assert!(
                    read_path.starts_with("artifacts/context-history-"),
                    "recovery pointer must target the indexed archive: {read_path}"
                );
                let call = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "recovery-read",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": read_path,
                                        "offset": 1,
                                        "max_lines": 20
                                    }).to_string()
                                }
                            }]
                        }
                    }]
                });
                let terminal = json!({
                    "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
                });
                format!("data: {call}\n\ndata: {terminal}\n\ndata: [DONE]\n\n")
            } else {
                let messages = wire["messages"].as_array().expect("messages");
                let read_result = messages
                    .iter()
                    .find(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "recovery-read"
                    })
                    .expect("recovery read result");
                assert!(
                    read_result["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("[Recovery transcript index]")),
                    "native read should return the indexed archive"
                );
                let event = json!({
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]
                });
                format!("data: {event}\n\ndata: [DONE]\n\n")
            };
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("fixture response");
            requests.push(request);
        }
        requests
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let mut runtime = Runtime::with_artifact_store(root.join("artifacts")).expect("artifacts");
    runtime.set_compaction_handle(handle.clone());
    let result = tokio::runtime::Runtime::new()
        .expect("tokio")
        .block_on(runtime.run_agent_loop(
            &client,
            "Record the requested change.",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 3,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let requests = server.join().expect("server");

    assert_eq!(requests.len(), 4);
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(
        std::fs::read_to_string(root.join("written.txt")).expect("written file"),
        "native fact\n"
    );
    assert_eq!(result.tool_results.len(), 4);
    assert_eq!(
        result
            .tool_results
            .iter()
            .map(|result| (result.name.as_str(), result.success))
            .collect::<Vec<_>>(),
        [
            ("write", true),
            ("shell", false),
            ("read", false),
            ("read", true),
        ]
    );

    let commits = handle.take_commits();
    assert_eq!(commits.len(), 1);
    let checkpoint = &commits[0].summary;
    assert!(checkpoint.contains("execution_facts scope=compaction run_start_seq=1"));
    assert!(checkpoint.contains(&format!("mutation path=\"{mutation_path}\" revision=1")));
    assert!(checkpoint.contains("call_id=\"failure-fact\""));
    assert!(checkpoint.contains("call_id=\"validation-fact\""));
    assert!(checkpoint.contains("success=false"));
    assert!(checkpoint.contains("[Prior visible transcript: use read on "));

    let archive = std::fs::read_dir(root.join("artifacts"))
        .expect("archive directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("context-history-"))
        })
        .expect("indexed context-history archive");
    let archive_text = std::fs::read_to_string(archive).expect("archive text");
    assert!(archive_text.starts_with("[Recovery transcript index]\n"));
    assert!(archive_text.contains("\"kind\":\"user_message\""));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn soft_threshold_final_response_does_not_start_background_compaction() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let request = read_http_request(&mut stream);
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"final answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .expect("final response");
        request
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let messages = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old context ".repeat(20_000), Vec::new()),
        ProviderMessage::user("recent request"),
    ];
    let used = slim_core::context::estimate_provider_message_tokens(&messages) + fixed_tokens;
    let handle = CompactionHandle::default();
    let mut runtime = Runtime::new();
    runtime.set_compaction_handle(handle.clone());
    runtime.set_background_compaction_enabled(true);
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &messages,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 3,
                context_window_tokens: used.saturating_mul(100).div_ceil(80),
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let request = server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(handle.status(), CompactionStatus::Idle);
    assert!(!request.contains("You are a context compactor"));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionAttemptStarted { .. })));
}

#[test]
fn fast_main_tool_turn_does_not_wait_for_slow_background_summary() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let (release_summary, summary_release) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut summary_handlers = Vec::new();
        let mut summary_release = Some(summary_release);
        let mut saw_initial = false;
        let mut saw_summary = false;
        let mut saw_final = false;
        for _ in 0..3 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            if request.contains("You are a context compactor") {
                saw_summary = true;
                let summary_release = summary_release
                    .take()
                    .expect("only one background summary request is expected");
                summary_handlers.push(thread::spawn(move || {
                    summary_release.recv().expect("release background summary");
                    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"slow summary\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":19,\"completion_tokens\":4}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                }));
            } else if request.contains("\"role\":\"tool\"") {
                saw_final = true;
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"final answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("final response");
            } else {
                saw_initial = true;
                let tool_call = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "fast-read-call",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": "missing-file.txt",
                                        "max_lines": 1
                                    }).to_string()
                                }
                            }]
                        }
                    }]
                });
                let body = format!(
                    "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("tool response");
            }
        }
        assert!(saw_initial && saw_summary && saw_final);
        for handler in summary_handlers {
            handler.join().expect("summary handler");
        }
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(
                &[slim_core::provider::ProviderMessage::user("slim")],
                &tools,
            )
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let initial = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old context ".repeat(20_000), Vec::new()),
        ProviderMessage::user("recent request"),
    ];
    let used = slim_core::context::estimate_provider_message_tokens(&initial) + fixed_tokens;
    let context_window_tokens = used.saturating_mul(100).div_ceil(80);
    let config = AgentLoopConfig {
        max_turns: 3,
        context_window_tokens,
        context_reserve_tokens: 0,
        ..AgentLoopConfig::default()
    };
    let handle = CompactionHandle::default();
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    runtime.set_compaction_handle(handle.clone());
    runtime.set_background_compaction_enabled(true);
    let started = Instant::now();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &initial,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            config,
        ))
        .expect("fast main/tool loop");
    let elapsed = started.elapsed();
    eprintln!(
        "SLIM_LATENCY background_blocked=causal main_tool_turn_us={}",
        elapsed.as_micros()
    );
    release_summary
        .send(())
        .expect("release background summary");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(handle.status(), CompactionStatus::Discarded);
    assert!(
        elapsed < Duration::from_millis(1_000),
        "loop took {elapsed:?}"
    );
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ToolStarted { .. })));
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantEnded { .. })));
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionAttemptStarted { .. })));
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionAttemptCancelled { .. })));
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionUsageUnknown { .. })));
    let rebuilt = slim_core::runtime::UsageTotals::from_events(runtime.app.events(), false);
    assert_eq!(result.usage, rebuilt);
    assert_eq!(
        rebuilt
            .requests
            .iter()
            .find(|request| request.cancelled)
            .expect("cancelled compaction request")
            .estimation_error_tokens,
        0
    );
}

#[test]
fn tool_call_below_break_even_skips_background_compaction() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            assert!(!request.contains("You are a context compactor"));
            let body = if request.contains("\"role\":\"tool\"") {
                "data: {\"choices\":[{\"delta\":{\"content\":\"final answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned()
            } else {
                let tool_call = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "break-even-read-call",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": "missing-file.txt",
                                        "max_lines": 1
                                    }).to_string()
                                }
                            }]
                        }
                    }]
                });
                format!(
                    "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                )
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("response");
            requests.push(request);
        }
        requests
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(
                &[slim_core::provider::ProviderMessage::user("slim")],
                &tools,
            )
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let messages = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old context ".repeat(20_000), Vec::new()),
        ProviderMessage::user("recent request"),
    ];
    let used = slim_core::context::estimate_provider_message_tokens(&messages) + fixed_tokens;
    let mut runtime = Runtime::new();
    let handle = CompactionHandle::default();
    runtime.set_compaction_handle(handle.clone());
    runtime.set_background_compaction_enabled(true);
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &messages,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 2,
                context_window_tokens: used.saturating_mul(100).div_ceil(80),
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let requests = server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(handle.status(), CompactionStatus::Idle);
    assert_eq!(requests.len(), 2);
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionAttemptStarted { .. })));
    assert!(runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::CompactionSkippedBelowBreakEven {
            future_turns: 1,
            ..
        }
    )));
}

#[test]
fn completed_background_summary_is_reused_before_overflow_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut saw_initial = false;
        let mut saw_summary = false;
        let mut saw_overflow = false;
        let mut saw_retry = false;
        for _ in 0..4 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            let (status, body) = if request.contains("You are a context compactor") {
                saw_summary = true;
                (
                    "200 OK",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"## Goal\\noverflow prepared\\n## Constraints\\nNone\\n## Progress\\nPrepared\\n## Blocked\\nNone\\n## Decisions\\nReuse summary\\n## Next steps\\nRetry\\n## Critical context\\nOverflow\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":29,\"completion_tokens\":6}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
                )
            } else if request.contains("[Compacted context]") {
                saw_retry = true;
                assert!(request.contains("overflow prepared"));
                (
                    "200 OK",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
                )
            } else if request.contains("\"role\":\"tool\"") {
                saw_overflow = true;
                thread::sleep(Duration::from_millis(100));
                (
                    "400 Bad Request",
                    "maximum context length exceeded".to_owned(),
                )
            } else {
                saw_initial = true;
                let tool_call = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "overflow-read-call",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": "missing-file.txt",
                                        "max_lines": 1
                                    }).to_string()
                                }
                            }]
                        }
                    }]
                });
                (
                    "200 OK",
                    format!(
                        // Calibrate below the soft threshold, so a fast summary
                        // cannot be applied before the request that must overflow.
                        "data: {tool_call}\n\ndata: {{\"usage\":{{\"prompt_tokens\":10000,\"completion_tokens\":1}}}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                    ),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("response");
        }
        assert!(saw_initial && saw_summary && saw_overflow && saw_retry);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let messages = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old context ".repeat(20_000), Vec::new()),
        ProviderMessage::user("recent request"),
    ];
    let used = slim_core::context::estimate_provider_message_tokens(&messages) + fixed_tokens;
    let mut runtime = Runtime::new();
    let handle = CompactionHandle::default();
    runtime.set_compaction_handle(handle.clone());
    runtime.set_background_compaction_enabled(true);
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &messages,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 3,
                context_window_tokens: used.saturating_mul(100).div_ceil(80),
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("overflow retry");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(handle.status(), CompactionStatus::Applied);
    assert_eq!(result.usage.compaction_input_tokens, 29);
    assert_eq!(result.usage.compaction_output_tokens, 6);
    assert!(runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::CompactionAttemptCompleted {
            uncached_input_tokens: 29,
            output_tokens: 6,
            usage_known: true,
            ..
        }
    )));
    assert!(runtime
        .conversation()
        .iter()
        .any(|message| message.content.contains("recovered")));
}

#[test]
fn volatile_code_intel_calls_remain_serial_barriers_in_provider_order() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first = accept_with_deadline(&listener);
        let _ = read_http_request(&mut first);
        let labels = [
            "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
        ];
        let tool_calls = labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                json!({
                    "index": index,
                    "id": format!("intel-{}", index + 1),
                    "function": {
                        "name": "code_intel",
                        "arguments": json!({"action": "symbol", "query": label}).to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        let calls = json!({
            "choices": [{
                "delta": {
                    "tool_calls": tool_calls
                }
            }]
        });
        let body = format!(
            "data: {calls}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        first
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("tool calls");

        let mut second = accept_with_deadline(&listener);
        let request = read_http_request(&mut second);
        let mut previous = 0;
        for label in labels {
            let position = request.find(label).expect("tool result");
            assert!(
                previous < position,
                "provider result order changed for {label}"
            );
            previous = position;
        }
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        second
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(Arc::new(DelayedCodeIntel));
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "inspect symbols",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 2,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 9);
    assert!(result.tool_results[0].output.contains("first"));
    assert!(result.tool_results[1].output.contains("second"));
    assert!(result.tool_results[2].output.contains("third"));
    let lifecycle = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolPrepared { call_id, .. } => Some(("prepared", call_id.as_str())),
            EventKind::ToolAdmitted { call_id, .. } => Some(("admitted", call_id.as_str())),
            EventKind::ToolStarted { call_id, .. } => Some(("start", call_id.as_str())),
            EventKind::ToolFinished { call_id, .. } => Some(("finish", call_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let position = |phase, call_id| {
        lifecycle
            .iter()
            .position(|candidate| *candidate == (phase, call_id))
            .expect("lifecycle event")
    };
    for call_id in [
        "intel-1", "intel-2", "intel-3", "intel-4", "intel-5", "intel-6", "intel-7", "intel-8",
        "intel-9",
    ] {
        assert!(
            position("prepared", call_id) < position("admitted", call_id)
                && position("admitted", call_id) < position("start", call_id)
                && position("start", call_id) < position("finish", call_id),
            "pool lifecycle must publish prepared→admitted→started→finished per call: {lifecycle:?}"
        );
    }
    let finished: Vec<_> = lifecycle
        .iter()
        .filter_map(|(phase, id)| (*phase == "finish").then_some(*id))
        .collect();
    assert_eq!(finished.len(), 9);
    for call_id in [
        "intel-1", "intel-2", "intel-3", "intel-4", "intel-5", "intel-6", "intel-7", "intel-8",
        "intel-9",
    ] {
        assert!(finished.contains(&call_id));
    }
    assert!(
        position("finish", "intel-2") < position("start", "intel-9"),
        "pool must not announce every call as started before admitting work: {lifecycle:?}"
    );
}

#[test]
fn mixed_batch_hoists_independent_reads_and_preserves_result_order() {
    let root = std::env::temp_dir().join(format!("slim-mixed-segments-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("a.txt"), "alpha").expect("a");
    std::fs::write(root.join("b.txt"), "bravo").expect("b");
    std::fs::write(root.join("d.txt"), "delta").expect("d");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let _ = read_http_request(&mut stream);
        let calls = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "read-a", "function": {"name": "read", "arguments": json!({"path": "a.txt", "max_lines": 1}).to_string()}},
                        {"index": 1, "id": "read-b", "function": {"name": "read", "arguments": json!({"path": "b.txt", "max_lines": 1}).to_string()}},
                        {"index": 2, "id": "write-barrier", "function": {"name": "write", "arguments": json!({"path": "barrier.txt", "content": "written"}).to_string()}},
                        {"index": 3, "id": "read-d", "function": {"name": "read", "arguments": json!({"path": "d.txt", "max_lines": 1}).to_string()}}
                    ]
                }
            }]
        });
        let body = format!(
            "data: {calls}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("tool calls");
        budget_finalization::reject_budget_finalization(&listener);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop(
            &client,
            "mixed segments",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    let lifecycle = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolPrepared { call_id, .. } => Some(("prepared", call_id.as_str())),
            EventKind::ToolAdmitted { call_id, .. } => Some(("admitted", call_id.as_str())),
            EventKind::ToolStarted { call_id, .. } => Some(("start", call_id.as_str())),
            EventKind::ToolOutput { call_id, .. } => Some(("output", call_id.as_str())),
            EventKind::ToolFinished { call_id, .. } => Some(("finish", call_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let position = |phase, call_id| {
        lifecycle
            .iter()
            .position(|candidate| *candidate == (phase, call_id))
            .expect("lifecycle event")
    };
    for call_id in ["read-a", "read-b", "read-d"] {
        assert!(
            position("prepared", call_id) < position("admitted", call_id)
                && position("admitted", call_id) < position("start", call_id)
                && position("start", call_id) < position("finish", call_id),
            "pool lifecycle must publish prepared→admitted→started→finished per call: {lifecycle:?}"
        );
    }
    let write_start = position("start", "write-barrier");
    // Snapshot reads are hoisted across mutations they cannot observe
    // (read-d targets a path disjoint from the write); the mutation itself
    // still starts only after every snapshot read has completed.
    assert!(
        write_start > position("finish", "read-a")
            && write_start > position("finish", "read-b")
            && write_start > position("finish", "read-d")
            && position("start", "read-d") < write_start,
        "reads run ahead of an unrelated write; the write waits for all of them: {lifecycle:?}"
    );
    let ordered_results = result
        .tool_results
        .iter()
        .map(|result| {
            let label = ["alpha", "bravo", "written", "delta"]
                .into_iter()
                .find(|label| result.output.contains(label))
                .expect("result label");
            (result.name.as_str(), label)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_results,
        [
            ("read", "alpha"),
            ("read", "bravo"),
            ("write", "written"),
            ("read", "delta"),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(root.join("barrier.txt")).expect("barrier"),
        "written"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn validation_shell_is_serialized_as_a_workspace_boundary() {
    let root = std::env::temp_dir().join(format!("slim-validation-segment-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("a.txt"), "alpha").expect("a");
    std::fs::write(root.join("b.txt"), "bravo").expect("b");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let _ = read_http_request(&mut stream);
        let calls = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "read-a", "function": {"name": "read", "arguments": json!({"path": "a.txt", "max_lines": 1}).to_string()}},
                        {"index": 1, "id": "cargo-check", "function": {"name": "shell", "arguments": json!({"command": "cargo check"}).to_string()}},
                        {"index": 2, "id": "read-b", "function": {"name": "read", "arguments": json!({"path": "b.txt", "max_lines": 1}).to_string()}}
                    ]
                }
            }]
        });
        let body = format!(
            "data: {calls}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("tool calls");
        budget_finalization::reject_budget_finalization(&listener);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(10)).expect("client");
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop(
            &client,
            "validation segment",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    let lifecycle = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { call_id, .. } => Some(("start", call_id.as_str())),
            EventKind::ToolOutput { call_id, .. } => Some(("output", call_id.as_str())),
            EventKind::ToolFinished { call_id, .. } => Some(("finish", call_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let position = |phase, call_id| {
        lifecycle
            .iter()
            .position(|candidate| *candidate == (phase, call_id))
            .expect("lifecycle event")
    };
    assert!(
        position("start", "cargo-check") > position("finish", "read-a"),
        "validation must not overlap preceding snapshot read: {lifecycle:?}"
    );
    assert!(
        position("start", "read-b") > position("finish", "cargo-check"),
        "read after validation must wait for validation barrier: {lifecycle:?}"
    );
    let ordered_names = result
        .tool_results
        .iter()
        .map(|result| result.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ordered_names, ["read", "shell", "read"]);
    assert!(result.tool_results[0].output.contains("alpha"));
    assert!(result.tool_results[2].output.contains("bravo"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn default_max_result_bytes_is_sixteen_kib() {
    assert_eq!(AgentLoopConfig::default().max_result_bytes, 16 * 1024);
}

#[test]
fn default_max_turns_is_one_hundred_twenty_eight() {
    assert_eq!(
        AgentLoopConfig::default().max_turns,
        AgentLoopConfig::DEFAULT_MAX_TURNS
    );
    assert_eq!(AgentLoopConfig::DEFAULT_MAX_TURNS, 128);
}

#[test]
fn hard_threshold_without_prepared_compacts_locally_without_summary_post() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let request = read_http_request(&mut stream);
        assert!(
            !request.contains("You are a context compactor"),
            "hard threshold without prepared must not wait for a summary POST"
        );
        assert!(request.contains("[Compacted context]"));
        assert!(request.contains("local extract"));
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let initial = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old context ".repeat(20_000), Vec::new()),
        ProviderMessage::user("recent request"),
    ];
    let used = slim_core::context::estimate_provider_message_tokens(&initial) + fixed_tokens;
    let context_window_tokens = used.saturating_mul(100).div_ceil(90);
    let config = AgentLoopConfig {
        max_turns: 1,
        context_window_tokens,
        context_reserve_tokens: 0,
        ..AgentLoopConfig::default()
    };
    let handle = CompactionHandle::default();
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifact_root =
        std::env::temp_dir().join(format!("slim-context-recovery-{}", std::process::id()));
    let mut runtime = Runtime::with_artifact_store(&artifact_root).expect("store");
    runtime.set_compaction_handle(handle.clone());
    runtime.set_background_compaction_enabled(false);
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &initial,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            config,
        ))
        .expect("loop");
    server.join().expect("server");

    let artifacts = std::fs::read_dir(&artifact_root)
        .expect("artifacts")
        .collect::<Result<Vec<_>, _>>()
        .expect("entries");
    assert_eq!(artifacts.len(), 1);
    let transcript_path = artifacts[0].path();
    let restored = std::fs::read_to_string(&transcript_path).expect("full transcript");
    assert!(restored.contains(&initial[1].content));
    let workspace = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
    let read_path = std::fs::canonicalize(&transcript_path)
        .expect("canonical transcript")
        .strip_prefix(workspace)
        .expect("transcript under workspace")
        .to_string_lossy()
        .replace('\\', "/");
    let read_pointer = serde_json::to_string(&read_path).expect("quoted transcript path");
    assert!(runtime
        .conversation()
        .iter()
        .any(|message| message.content.contains(&format!(
            "[Prior visible transcript: use read on {read_pointer} with offset=1"
        ))));
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(handle.status(), CompactionStatus::Applied);
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
    std::fs::remove_dir_all(artifact_root).expect("cleanup");
}
#[test]
fn discarded_background_summary_still_counts_observed_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut initial = accept_with_deadline(&listener);
        let initial_request = read_http_request(&mut initial);
        assert!(!initial_request.contains("You are a context compactor"));
        let tool_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "discarded-read-call",
                        "function": {
                            "name": "read",
                            "arguments": json!({
                                "path": "missing-file.txt",
                                "max_lines": 1
                            }).to_string()
                        }
                    }]
                }
            }]
        });
        let initial_body = format!(
            "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        initial
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    initial_body.len(),
                    initial_body
                )
                .as_bytes(),
            )
            .expect("tool response");

        let mut final_stream = None;
        for _ in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            if request.contains("You are a context compactor") {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"incomplete\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .expect("summary response");
            } else {
                assert!(request.contains("\"role\":\"tool\""));
                final_stream = Some(stream);
            }
        }

        let mut final_stream = final_stream.expect("final request");
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"normal\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        final_stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .expect("final response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let messages = vec![
        ProviderMessage::user("root"),
        ProviderMessage::assistant("old context ".repeat(20_000), Vec::new()),
        ProviderMessage::user("recent"),
    ];
    let used = slim_core::context::estimate_provider_message_tokens(&messages) + fixed_tokens;
    let handle = CompactionHandle::default();
    let mut runtime = Runtime::new();
    runtime.set_compaction_handle(handle.clone());
    runtime.set_background_compaction_enabled(true);
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &messages,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 3,
                context_window_tokens: used.saturating_mul(100).div_ceil(80),
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(handle.status(), CompactionStatus::Discarded);
    assert_eq!(result.usage.compaction_input_tokens, 11);
    assert_eq!(result.usage.compaction_output_tokens, 2);
    assert!(runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::CompactionAttemptCompleted {
            uncached_input_tokens: 11,
            output_tokens: 2,
            usage_known: true,
            ..
        }
    )));
}

#[test]
fn context_overflow_compacts_and_retries_once_without_consuming_turn_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first = accept_with_deadline(&listener);
        let first_request = read_http_request(&mut first);
        assert!(!first_request.contains("You are a context compactor"));
        let error_body = "maximum context length exceeded";
        first
            .write_all(
                format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    error_body.len(),
                    error_body
                )
                .as_bytes(),
            )
            .expect("overflow response");

        let mut summary = accept_with_deadline(&listener);
        let summary_request = read_http_request(&mut summary);
        assert!(summary_request.contains("You are a context compactor"));
        let summary_body = "data: {\"choices\":[{\"delta\":{\"content\":\"## Goal\\noverflow summary\\n## Constraints\\nNone\\n## Progress\\nRecovered\\n## Blocked\\nNone\\n## Decisions\\nCompact\\n## Next steps\\nRetry\\n## Critical context\\nOverflow\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        summary
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    summary_body.len(),
                    summary_body
                )
                .as_bytes(),
            )
            .expect("summary response");

        let mut retry = accept_with_deadline(&listener);
        let retry_request = read_http_request(&mut retry);
        assert!(retry_request.contains("[Compacted context]"));
        assert!(retry_request.contains("overflow summary"));
        let retry_body = "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        retry
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    retry_body.len(),
                    retry_body
                )
                .as_bytes(),
            )
            .expect("retry response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let handle = CompactionHandle::default();
    let mut runtime = Runtime::new();
    runtime.set_compaction_handle(handle.clone());
    let messages = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old context", Vec::new()),
        ProviderMessage::user("recent request"),
    ];
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &messages,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 1,
                context_window_tokens: 64_000,
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("overflow recovery");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.turns, 1);
    assert_eq!(result.usage.retry_count, 1);
    assert_eq!(handle.status(), CompactionStatus::Applied);
    assert_eq!(handle.generation(), 1);
    assert!(runtime
        .conversation()
        .iter()
        .any(|message| message.content.contains("recovered")));
}

#[test]
fn truncated_summary_publishes_usage_but_never_compacts() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for index in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 32 * 1024];
            let _ = stream.read(&mut request).expect("request");
            stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        ).expect("headers");
            if index == 0 {
                let event = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "truncated-read-call",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": "missing-file.txt",
                                        "max_lines": 10
                                    }).to_string()
                                }
                            }]
                        }
                    }]
                });
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("tool");
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("tool finish");
            } else {
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"truncated summary\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("truncated summary");
            }
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let handle = CompactionHandle::default();
    handle.request_manual("").expect("queue summary protocol");
    runtime.set_compaction_handle(handle);
    let error = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "start",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: fixed_tokens + 256,
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect_err("truncated summary must fail");
    server.join().expect("server");

    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { .. }
    ));
    assert!(!runtime.app.events().is_empty());
    assert!(runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::Usage {
            input_tokens: 5,
            output_tokens: 2
        }
    )));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
    let ledger = slim_core::runtime::UsageTotals::from_events(runtime.app.events(), false);
    assert!(ledger.requests.iter().any(|request| {
        request.request_kind == slim_core::RequestKind::Compaction && request.failed
    }));
}

#[test]
fn tool_call_summary_is_rejected_without_replacing_the_transcript() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for index in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 32 * 1024];
            let _ = stream.read(&mut request).expect("request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            if index == 0 {
                let event = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "summary-base-read-call",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": "missing-file.txt",
                                        "max_lines": 10
                                    }).to_string()
                                }
                            }]
                        }
                    }]
                });
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("tool");
            } else {
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"partial summary\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"summary-invalid\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n",
                    )
                    .expect("invalid summary tool");
            }
            stream
                .write_all(
                    b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                )
                .expect("tool finish");
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let handle = CompactionHandle::default();
    handle.request_manual("").expect("queue summary protocol");
    runtime.set_compaction_handle(handle);
    let error = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "start",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: fixed_tokens + 256,
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect_err("tool-call summary must fail");
    server.join().expect("server");

    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { ref message }
            if message.contains("summary provider") && message.contains("tool")
    ));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
}

#[test]
fn duplicate_tool_output_goes_on_the_wire_as_a_pointer_and_context_snapshot_is_logged() {
    let root = std::env::temp_dir().join(format!(
        "slim-dedup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(root.join("dup.txt"), "alpha".repeat(40)).expect("fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..3 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 32 * 1024];
            let size = stream.read(&mut request).expect("request");
            requests.push(String::from_utf8_lossy(&request[..size]).into_owned());
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            if index < 2 {
                let event = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "id": format!("call-{index}"),
                                "function": {
                                    "name": "read",
                                    "arguments": json!({"path": "dup.txt", "max_lines": 10}).to_string()
                                }
                            }]
                        }
                    }]
                });
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("tool call");
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("tool finish");
            } else {
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("final");
            }
        }
        requests
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read the same file twice",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 4,
                context_window_tokens: 64_000,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let requests = server.join().expect("server");
    assert_eq!(requests.len(), 3);
    let bodies = requests
        .iter()
        .map(|request| {
            let body = request.split("\r\n\r\n").nth(1).expect("body");
            serde_json::from_str::<Value>(body).expect("json")
        })
        .collect::<Vec<_>>();

    let first_read = bodies[1]["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("tool message");
    assert_eq!(first_read["role"], "tool");
    assert_eq!(first_read["content"], "alpha".repeat(40));

    let second_messages = bodies[2]["messages"].as_array().expect("messages");
    assert!(second_messages.iter().any(|message| {
        message["role"] == "tool"
            && message["content"]
                .as_str()
                .is_some_and(|content| content.contains("[duplicate read result omitted"))
    }));
    assert_eq!(
        second_messages.last().expect("steer message")["role"],
        "user"
    );

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ContextSnapshot {
            tool_schema_bytes: tools_bytes,
            estimated_tokens,
            context_window_tokens,
            ..
        } if *tools_bytes > 0 && *estimated_tokens > 0 && *context_window_tokens == 64_000
    )));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn resumed_turn_deduplicates_retained_read_but_sends_changed_content_in_full() {
    let root = std::env::temp_dir().join(format!(
        "slim-resumed-dedup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let original = "alpha".repeat(40);
    let changed = "bravo".repeat(40);
    std::fs::write(root.join("dup.txt"), &original).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut bodies = Vec::new();
        for index in 0..6 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            bodies.push(
                serde_json::from_str::<Value>(request.split_once("\r\n\r\n").unwrap().1).unwrap(),
            );
            let (delta, finish) = if index % 2 == 0 {
                (
                    json!({"tool_calls": [{"id": format!("call-{index}"), "function": {
                        "name": "read", "arguments": r#"{"path":"dup.txt","max_lines":10}"#
                    }}]}),
                    "tool_calls",
                )
            } else {
                (json!({"content": "done"}), "stop")
            };
            let response = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": delta}]}),
                json!({"choices": [{"delta": {}, "finish_reason": finish}]})
            );
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).unwrap();
        }
        bodies
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).unwrap();
    let executor = tokio::runtime::Runtime::new().unwrap();
    let mut messages = Vec::new();
    let tools = slim_core::tools::ToolRegistry::default();
    for turn in 0..3 {
        if turn == 2 {
            std::fs::rename(root.join("dup.txt"), root.join("old.txt")).unwrap();
            std::fs::write(root.join("dup.txt"), &changed).unwrap();
        }
        messages.push(ProviderMessage::user("Read dup.txt again"));
        let mut runtime = Runtime::new();
        runtime.set_tool_registry(tools.clone());
        let result = executor
            .block_on(runtime.run_agent_loop_with_messages(
                &client,
                &messages,
                OperatingMode::ReadOnly,
                &root,
                1,
                AgentLoopConfig {
                    max_turns: 4,
                    context_window_tokens: 64_000,
                    ..AgentLoopConfig::default()
                },
            ))
            .unwrap();
        assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
        messages = runtime.conversation().to_vec();
    }
    let bodies = server.join().unwrap();
    let outputs = |index: usize| {
        bodies[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(outputs(1), vec![original.as_str()]);
    let repeated = outputs(3);
    assert_eq!(repeated.len(), 2);
    assert_eq!(repeated[0], original);
    assert_eq!(
        repeated[1],
        "[duplicate read result omitted; identical output already in context]"
    );
    assert!(repeated[1].len() < original.len());
    let updated = outputs(5);
    assert_eq!(updated.len(), 3);
    assert_eq!(updated[0], original);
    assert_eq!(updated[2], changed);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn post_compaction_identical_reread_reuses_retained_full_output() {
    let root = std::env::temp_dir().join(format!(
        "slim-compact-dedup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("mkdir");
    let original = "alpha".repeat(40);
    std::fs::write(root.join("dup.txt"), &original).expect("fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let read_args = json!({"path": "dup.txt", "max_lines": 10}).to_string();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..4 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 32 * 1024];
            let size = stream.read(&mut request).expect("request");
            requests.push(String::from_utf8_lossy(&request[..size]).into_owned());
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            if index == 0 || index == 2 {
                let event = json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "id": format!("call-{index}"),
                                "function": {
                                    "name": "read",
                                    "arguments": read_args
                                }
                            }]
                        }
                    }]
                });
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("tool call");
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("tool finish");
            } else if index == 1 {
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"## Goal\\nsummary\\n## Constraints\\nNone\\n## Progress\\nDone\\n## Blocked\\nNone\\n## Decisions\\nKeep\\n## Next steps\\nContinue\\n## Critical context\\nFixture\"}}]}\n\n",
                    )
                    .expect("summary");
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("summary finish");
            } else {
                stream
                    .write_all(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    )
                    .expect("final");
            }
        }
        requests
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let fixed_tokens = slim_core::context::estimate_text_tokens_from_chars(
        client
            .adapter()
            .build_messages_request_with_tools_checked(&[], &tools)
            .expect("fixed request")
            .body
            .chars()
            .count() as u64,
    );
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let handle = CompactionHandle::default();
    handle.request_manual("").expect("queue summary protocol");
    runtime.set_compaction_handle(handle);
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read the same file twice",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 4,
                // Slack must cover the retained post-compaction content: the
                // workspace snapshot inside the root user message, the summary,
                // and the kept tool-result suffix.
                context_window_tokens: fixed_tokens + 2048,
                context_reserve_tokens: 120,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let requests = server.join().expect("server");
    assert_eq!(requests.len(), 4);
    let bodies = requests
        .iter()
        .map(|request| {
            let body = request.split("\r\n\r\n").nth(1).expect("body");
            serde_json::from_str::<Value>(body).expect("json")
        })
        .collect::<Vec<_>>();

    for index in [2, 3] {
        assert!(bodies[index]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["role"] == "tool" && message["content"] == original));
    }
    let post_compaction_read = bodies[3]["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("post-compaction tool message");
    assert_eq!(post_compaction_read["role"], "tool");
    assert_eq!(
        post_compaction_read["content"],
        "[duplicate read result omitted; identical output already in context]"
    );

    assert!(!runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::ToolEvidenceReused {
            post_compaction: true,
            ..
        }
    )));
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn legacy_context_snapshot_defaults_live_usage_fields() {
    let event: slim_core::SessionEvent = serde_json::from_str(
        r#"{"seq":1,"kind":{"type":"ContextSnapshot","tools_bytes":1,"history_bytes":2}}"#,
    )
    .expect("legacy event");

    assert!(matches!(
        event.kind,
        EventKind::ContextSnapshot {
            estimated_tokens: 0,
            context_window_tokens: 0,
            ..
        }
    ));
}

#[test]
fn agent_loop_announces_todo_and_applies_add_with_ledger_event() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first_stream = accept_with_deadline(&listener);
        let mut first_request = [0_u8; 64 * 1024];
        let first_size = first_stream
            .read(&mut first_request)
            .expect("first request");
        let first_request = String::from_utf8_lossy(&first_request[..first_size]);
        assert!(
            first_request.contains("\"name\":\"todo\""),
            "first request should announce todo: {first_request}"
        );
        assert!(
            first_request.contains("Harness channel: Auto, unattended"),
            "headless Auto must state the unattended channel: {first_request}"
        );
        assert!(
            !first_request.contains("\"name\":\"ask_question\""),
            "unattended Auto must not advertise ask_question: {first_request}"
        );
        let tool_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "todo-call-1",
                        "function": {
                            "name": "todo",
                            "arguments": json!({"todos":[{"title":"ship n2"},{"title":"verify gate"}]}).to_string()
                        }
                    }]
                }
            }]
        });
        let first_body = format!(
            "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let first_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            first_body.len(),
            first_body
        );
        first_stream
            .write_all(first_response.as_bytes())
            .expect("first response");

        let mut second_stream = accept_with_deadline(&listener);
        let mut second_request = [0_u8; 64 * 1024];
        let second_size = second_stream
            .read(&mut second_request)
            .expect("second request");
        let second_request = String::from_utf8_lossy(&second_request[..second_size]);
        assert!(second_request.contains("todo-call-1"));
        assert!(second_request.contains("ship n2"));
        assert!(second_request.contains("todo 0 [pending]: ship n2"));
        assert!(second_request.contains("todo 1 [pending]: verify gate"));
        assert!(
            second_request.contains("verify gate"),
            "second todo in the array must be applied: {second_request}"
        );
        let final_body = "data: {\"choices\":[{\"delta\":{\"content\":\"todo noted\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let final_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            final_body.len(),
            final_body
        );
        second_stream
            .write_all(final_response.as_bytes())
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "track the work",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 2,
                max_mutating_tool_calls: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].success);
    assert!(result.tool_results[0].output.contains("ship n2"));
    assert!(result.tool_results[0].output.contains("verify gate"));
    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::TodoChanged { items }
            if items.iter().any(|item| item.title == "ship n2")
                && items.iter().any(|item| item.title == "verify gate")
    )));
}

#[test]
fn agent_loop_lazy_skill_is_not_injected_until_called() {
    let root = std::env::temp_dir().join(format!(
        "slim-agent-skill-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let skill_dir = root.join(".slim").join("skills").join("hello");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: hello\ndescription: offline fixture skill\n---\nbody\n",
    )
    .expect("skill metadata");
    std::fs::write(
        skill_dir.join("run.ps1"),
        "Write-Output \"skill-hello-ok\"\n",
    )
    .expect("skill script");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first_stream = accept_with_deadline(&listener);
        let mut first_request = [0_u8; 64 * 1024];
        let first_size = first_stream
            .read(&mut first_request)
            .expect("first request");
        let first_request = String::from_utf8_lossy(&first_request[..first_size]);
        assert!(
            first_request.contains("\"name\":\"skill\""),
            "first request should announce dispatcher skill: {first_request}"
        );
        assert!(
            !first_request.contains("skill_hello"),
            "per-skill tools must not be injected: {first_request}"
        );
        assert!(
            !first_request.contains("offline fixture skill"),
            "skill descriptions must not be injected: {first_request}"
        );
        let tool_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "skill-call-1",
                        "function": {
                            "name": "skill",
                            "arguments": "{\"name\":\"hello\"}"
                        }
                    }]
                }
            }]
        });
        let first_body = format!(
            "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let first_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            first_body.len(),
            first_body
        );
        first_stream
            .write_all(first_response.as_bytes())
            .expect("first response");

        let mut second_stream = accept_with_deadline(&listener);
        let mut second_request = [0_u8; 64 * 1024];
        let second_size = second_stream
            .read(&mut second_request)
            .expect("second request");
        let second_request = String::from_utf8_lossy(&second_request[..second_size]);
        assert!(second_request.contains("skill-call-1"));
        assert!(
            second_request.contains("skill-hello-ok"),
            "second request should include skill stdout: {second_request}"
        );
        let final_body = "data: {\"choices\":[{\"delta\":{\"content\":\"skill ran\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let final_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            final_body.len(),
            final_body
        );
        second_stream
            .write_all(final_response.as_bytes())
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(8)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime.block_on(runtime.run_agent_loop(
        &client,
        "run the hello skill",
        OperatingMode::Auto,
        &root,
        1,
        AgentLoopConfig {
            max_turns: 2,
            max_mutating_tool_calls: 1,
            ..AgentLoopConfig::default()
        },
    ));
    let _ = std::fs::remove_dir_all(&root);
    let result = result.expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].success);
    assert!(
        result.tool_results[0].output.contains("skill-hello-ok"),
        "tool output: {}",
        result.tool_results[0].output
    );
}

#[test]
fn agent_loop_skill_list_returns_names_only_after_explicit_call() {
    let root = std::env::temp_dir().join(format!(
        "slim-agent-skill-list-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let skill_dir = root.join(".slim").join("skills").join("hello");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: hello\ndescription: listed only on demand\n---\nsecret-body\n",
    )
    .expect("skill metadata");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first_stream = accept_with_deadline(&listener);
        let mut first_request = [0_u8; 64 * 1024];
        let first_size = first_stream
            .read(&mut first_request)
            .expect("first request");
        let first_request = String::from_utf8_lossy(&first_request[..first_size]);
        assert!(
            !first_request.contains("listed only on demand"),
            "list descriptions must stay out of the first schema: {first_request}"
        );
        assert!(
            !first_request.contains("secret-body"),
            "skill body must stay out of the first schema: {first_request}"
        );
        let tool_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "skill-list-1",
                        "function": {
                            "name": "skill",
                            "arguments": "{\"list\":true}"
                        }
                    }]
                }
            }]
        });
        let first_body = format!(
            "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let first_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            first_body.len(),
            first_body
        );
        first_stream
            .write_all(first_response.as_bytes())
            .expect("first response");

        let mut second_stream = accept_with_deadline(&listener);
        let mut second_request = [0_u8; 64 * 1024];
        let second_size = second_stream
            .read(&mut second_request)
            .expect("second request");
        let second_request = String::from_utf8_lossy(&second_request[..second_size]);
        assert!(second_request.contains("skill-list-1"));
        assert!(
            second_request.contains("hello") && second_request.contains("listed only on demand"),
            "list output should include name+description: {second_request}"
        );
        assert!(
            !second_request.contains("secret-body"),
            "list must not include SKILL.md body: {second_request}"
        );
        let final_body = "data: {\"choices\":[{\"delta\":{\"content\":\"listed\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let final_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            final_body.len(),
            final_body
        );
        second_stream
            .write_all(final_response.as_bytes())
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(8)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime.block_on(runtime.run_agent_loop(
        &client,
        "list slim skills",
        OperatingMode::Auto,
        &root,
        1,
        AgentLoopConfig {
            max_turns: 2,
            max_mutating_tool_calls: 1,
            ..AgentLoopConfig::default()
        },
    ));
    let _ = std::fs::remove_dir_all(&root);
    let result = result.expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].success);
}

#[test]
fn read_calls_do_not_consume_mutating_budget() {
    let root = std::env::temp_dir().join(format!("slim-read-mut-budget-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("probe.txt"), "probe").expect("probe");
    std::fs::write(root.join("probe2.txt"), "probe2").expect("probe2");
    std::fs::write(root.join("probe3.txt"), "probe3").expect("probe3");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first_stream = accept_with_deadline(&listener);
        let mut first_request = [0_u8; 16 * 1024];
        let _ = first_stream
            .read(&mut first_request)
            .expect("first request");
        let reads = json!({
            "choices": [{
                "delta": {
                    "tool_calls": (0..3).map(|index| {
                        let path = match index {
                            0 => "probe.txt",
                            1 => "probe2.txt",
                            _ => "probe3.txt",
                        };
                        json!({
                        "index": index,
                        "id": format!("read-{index}"),
                        "function": {
                            "name": "read",
                            "arguments": json!({"path": path, "max_lines": 1}).to_string()
                        }
                    })
                    }).collect::<Vec<_>>()
                }
            }]
        });
        let first_body = format!(
            "data: {reads}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        first_stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    first_body.len(),
                    first_body
                )
                .as_bytes(),
            )
            .expect("first response");

        let mut second_stream = accept_with_deadline(&listener);
        let mut second_request = [0_u8; 16 * 1024];
        let _ = second_stream
            .read(&mut second_request)
            .expect("second request");
        let write_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "write-1",
                        "function": {
                            "name": "write",
                            "arguments": json!({"path": "out.txt", "content": "done"}).to_string()
                        }
                    }]
                }
            }]
        });
        let second_body = format!(
            "data: {write_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        second_stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    second_body.len(),
                    second_body
                )
                .as_bytes(),
            )
            .expect("second response");

        let mut third_stream = accept_with_deadline(&listener);
        let mut third_request = [0_u8; 16 * 1024];
        let _ = third_stream
            .read(&mut third_request)
            .expect("third request");
        let final_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        third_stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    final_body.len(),
                    final_body
                )
                .as_bytes(),
            )
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read then write",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 3,
                max_mutating_tool_calls: 1,
                max_read_tool_calls: 96,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 4);
    assert_eq!(
        result
            .tool_results
            .iter()
            .filter(|result| result.name == "read")
            .count(),
        3
    );
    assert_eq!(
        result
            .tool_results
            .iter()
            .filter(|result| result.name == "write")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(root.join("out.txt")).expect("out"),
        "done"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn read_exhaustion_stops_with_tool_limit() {
    let root = std::env::temp_dir().join(format!("slim-read-limit-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("probe.txt"), "probe").expect("probe");
    std::fs::write(root.join("probe2.txt"), "probe2").expect("probe2");
    std::fs::write(root.join("probe3.txt"), "probe3").expect("probe3");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let reads = json!({
            "choices": [{
                "delta": {
                    "tool_calls": (0..3).map(|index| {
                        let path = match index {
                            0 => "probe.txt",
                            1 => "probe2.txt",
                            _ => "probe3.txt",
                        };
                        json!({
                        "index": index,
                        "id": format!("read-{index}"),
                        "function": {
                            "name": "read",
                            "arguments": json!({"path": path, "max_lines": 1}).to_string()
                        }
                    })
                    }).collect::<Vec<_>>()
                }
            }]
        });
        let body = format!(
            "data: {reads}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .expect("response");
        budget_finalization::reject_budget_finalization(&listener);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read thrice",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                max_read_tool_calls: 2,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ToolLimit);
    assert_eq!(result.tool_results.len(), 2);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn max_total_tool_calls_stops_across_multiple_turns_with_tool_limit() {
    let root = std::env::temp_dir().join(format!("slim-total-tool-limit-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("f1.txt"), "f1").expect("f1");
    std::fs::write(root.join("f2.txt"), "f2").expect("f2");
    std::fs::write(root.join("f3.txt"), "f3").expect("f3");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        // Turn 1: 2 reads
        let mut stream = accept_with_deadline(&listener);
        let _ = read_http_request(&mut stream);
        let first = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "read-1", "function": {"name": "read", "arguments": json!({"path": "f1.txt"}).to_string()}},
                        {"index": 1, "id": "read-2", "function": {"name": "read", "arguments": json!({"path": "f2.txt"}).to_string()}}
                    ]
                }
            }]
        });
        let body = format!(
            "data: {first}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("turn 1 response");

        // Turn 2: 2 reads (but max_total_tool_calls = 3, so only 1 should run, stopping with ToolLimit)
        let mut stream2 = accept_with_deadline(&listener);
        let _ = read_http_request(&mut stream2);
        let second = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "read-3", "function": {"name": "read", "arguments": json!({"path": "f3.txt"}).to_string()}},
                        {"index": 1, "id": "read-4", "function": {"name": "read", "arguments": json!({"path": "f1.txt"}).to_string()}}
                    ]
                }
            }]
        });
        let body2 = format!(
            "data: {second}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream2
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body2}",
                    body2.len()
                )
                .as_bytes(),
            )
            .expect("turn 2 response");
        budget_finalization::reject_budget_finalization(&listener);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read across turns",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 4,
                max_read_tool_calls: 10,
                max_total_tool_calls: 3,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ToolLimit);
    assert_eq!(result.tool_results.len(), 3);
    let tool_messages = runtime
        .conversation()
        .iter()
        .filter(|msg| msg.role == "tool")
        .count();
    assert_eq!(tool_messages, 3);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn mixed_batch_budget_preserves_the_execution_prefix() {
    let root = std::env::temp_dir().join(format!("slim-mixed-batch-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("probe.txt"), "probe").expect("probe");
    std::fs::write(root.join("probe2.txt"), "probe2").expect("probe2");
    std::fs::write(root.join("probe3.txt"), "probe3").expect("probe3");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let batch = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "read-1", "function": {"name": "read", "arguments": json!({"path": "probe.txt", "max_lines": 1}).to_string()}},
                        {"index": 1, "id": "read-2", "function": {"name": "read", "arguments": json!({"path": "probe2.txt", "max_lines": 1}).to_string()}},
                        {"index": 2, "id": "read-3", "function": {"name": "read", "arguments": json!({"path": "probe3.txt", "max_lines": 1}).to_string()}},
                        {"index": 3, "id": "write-1", "function": {"name": "write", "arguments": json!({"path": "out.txt", "content": "done"}).to_string()}}
                    ]
                }
            }]
        });
        let body = format!(
            "data: {batch}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .expect("response");
        budget_finalization::reject_budget_finalization(&listener);
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "mixed batch",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                max_read_tool_calls: 2,
                max_mutating_tool_calls: 5,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ToolLimit);
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        result
            .tool_results
            .iter()
            .filter(|result| result.name == "read")
            .count(),
        2
    );
    assert_eq!(
        result
            .tool_results
            .iter()
            .filter(|result| result.name == "write")
            .count(),
        0
    );
    assert!(
        !root.join("out.txt").exists(),
        "do not cross a suppressed operation"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn sequential_read_calls_refresh_per_turn_budget() {
    let root = std::env::temp_dir().join(format!("slim-per-turn-read-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("a.txt"), "a").expect("a");
    std::fs::write(root.join("b.txt"), "b").expect("b");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let write_read = |stream: &mut TcpStream, path: &str, id: &str| {
            let payload = json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": id,
                            "function": {
                                "name": "read",
                                "arguments": json!({"path": path, "max_lines": 1}).to_string()
                            }
                        }]
                    }
                }]
            });
            let body = format!(
                "data: {payload}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("tool response");
        };

        let mut first = accept_with_deadline(&listener);
        let _ = read_http_request(&mut first);
        write_read(&mut first, "a.txt", "read-a");

        let mut second = accept_with_deadline(&listener);
        let _ = read_http_request(&mut second);
        write_read(&mut second, "b.txt", "read-b");

        let mut third = accept_with_deadline(&listener);
        let _ = read_http_request(&mut third);
        let done = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        third
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{done}",
                    done.len()
                )
                .as_bytes(),
            )
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read twice sequentially",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 3,
                max_read_tool_calls: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 2);
    assert!(result
        .tool_results
        .iter()
        .all(|result| result.name == "read"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn per_turn_mutating_overflow_continues_when_turns_remain() {
    let root = std::env::temp_dir().join(format!("slim-per-turn-overflow-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let write_batch = |stream: &mut TcpStream, calls: Value| {
            let payload = json!({
                "choices": [{
                    "delta": {
                        "tool_calls": calls
                    }
                }]
            });
            let body = format!(
                "data: {payload}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("tool response");
        };

        let mut first = accept_with_deadline(&listener);
        let _ = read_http_request(&mut first);
        write_batch(
            &mut first,
            json!([
                {"index": 0, "id": "write-1", "function": {"name": "write", "arguments": json!({"path":"first.txt","content":"first"}).to_string()}},
                {"index": 1, "id": "write-2", "function": {"name": "write", "arguments": json!({"path":"second.txt","content":"second"}).to_string()}}
            ]),
        );

        let mut second = accept_with_deadline(&listener);
        let _ = read_http_request(&mut second);
        write_batch(
            &mut second,
            json!([
                {"index": 0, "id": "write-3", "function": {"name": "write", "arguments": json!({"path":"second.txt","content":"second"}).to_string()}}
            ]),
        );

        let mut third = accept_with_deadline(&listener);
        let _ = read_http_request(&mut third);
        let done = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        third
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{done}",
                    done.len()
                )
                .as_bytes(),
            )
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "write two files",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 3,
                max_mutating_tool_calls: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(
        std::fs::read_to_string(root.join("first.txt")).expect("first"),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("second.txt")).expect("second"),
        "second"
    );
    assert_eq!(
        result
            .tool_results
            .iter()
            .filter(|result| result.name == "write" && result.success)
            .count(),
        2
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn budget_exhaustion_still_returns_final_answer_without_tools() {
    let root = std::env::temp_dir().join(format!("slim-budget-final-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("a.txt"), "a").expect("a");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first = accept_with_deadline(&listener);
        let _ = read_http_request(&mut first);
        let payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "read-a",
                        "function": {
                            "name": "read",
                            "arguments": json!({"path": "a.txt", "max_lines": 1}).to_string()
                        }
                    }]
                }
            }]
        });
        let body = format!(
            "data: {payload}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        first
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("tool response");

        let mut second = accept_with_deadline(&listener);
        let finalize_request = read_http_request(&mut second);
        let done = "data: {\"choices\":[{\"delta\":{\"content\":\"budget-final-paragraph\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        second
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{done}",
                    done.len()
                )
                .as_bytes(),
            )
            .expect("final response");
        finalize_request
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read once",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let finalize_request = server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::TurnLimit);
    assert_eq!(result.tool_results.len(), 1);
    assert!(finalize_request.contains("Budget exhausted"));
    assert!(finalize_request.contains("\"max_tokens\":2048"));
    let conversation_text = runtime
        .conversation()
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(conversation_text.contains("budget-final-paragraph"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn duplicate_read_injects_between_turns_steer_once() {
    let root = std::env::temp_dir().join(format!("slim-budget-steer-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("a.txt"), "a").expect("a");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let write_read = |stream: &mut TcpStream| {
            let payload = json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "read-a",
                            "function": {
                                "name": "read",
                                "arguments": json!({"path": "a.txt", "max_lines": 1}).to_string()
                            }
                        }]
                    }
                }]
            });
            let body = format!(
                "data: {payload}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("tool response");
        };

        let mut first = accept_with_deadline(&listener);
        let _ = read_http_request(&mut first);
        write_read(&mut first);

        let mut second = accept_with_deadline(&listener);
        let second_request = read_http_request(&mut second);
        write_read(&mut second);

        let mut third = accept_with_deadline(&listener);
        let third_request = read_http_request(&mut third);
        let done = "data: {\"choices\":[{\"delta\":{\"content\":\"done-steer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        third
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{done}",
                    done.len()
                )
                .as_bytes(),
            )
            .expect("final response");
        (second_request, third_request)
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read twice",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 3,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    let (second_request, third_request) = server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 2);
    assert!(
        !runtime
            .app
            .events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolEvidenceReused { .. })),
        "a pointer must not enlarge short output"
    );
    assert!(!second_request.contains("repeated evidence without a relevant state change"));
    assert!(third_request.contains("repeated evidence without a relevant state change"));
    let conversation_text = runtime
        .conversation()
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(conversation_text.contains("done-steer"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn repeated_identical_reads_stop_with_no_progress_and_final_answer() {
    let root = std::env::temp_dir().join(format!("slim-no-progress-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("a.txt"), "a").expect("a");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for index in 0..4 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            if index == 3 {
                assert!(request.contains(
                    "Tool read call read-a repeated evidence without a relevant state change"
                ));
            }
            let payload = json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "read-a",
                            "function": {
                                "name": "read",
                                "arguments": json!({"path": "a.txt", "max_lines": 1}).to_string()
                            }
                        }]
                    }
                }]
            });
            let body = format!(
                "data: {payload}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("tool response");
        }

        let mut final_stream = accept_with_deadline(&listener);
        let final_request = read_http_request(&mut final_stream);
        assert!(final_request.contains("Execution stopped because repeated tool work"));
        assert!(!final_request.contains("Budget exhausted"));
        let done = "data: {\"choices\":[{\"delta\":{\"content\":\"no-progress-final\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        final_stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{done}",
                    done.len()
                )
                .as_bytes(),
            )
            .expect("final response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "read the same file",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 8,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::NoProgress);
    assert_eq!(result.turns, 4);
    assert_eq!(result.tool_results.len(), 4);
    assert!(result
        .tool_results
        .iter()
        .all(|result| result.name == "read"));
    let conversation_text = runtime
        .conversation()
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(conversation_text.contains("no-progress-final"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn failed_validation_can_retry_after_an_observed_external_change() {
    let root = std::env::temp_dir().join(format!("slim-validation-retry-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("state.txt"), "before").unwrap();
    let server_root = root.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..5 {
            let mut stream = accept_with_deadline(&listener);
            let request = read_http_request(&mut stream);
            if index == 2 {
                std::fs::write(server_root.join("state.txt"), "externally changed").unwrap();
            }
            let (delta, reason) = if index == 4 {
                assert!(!request.contains("repeated evidence without a relevant state change"));
                (
                    json!({"content":"validation still fails; report the blocker"}),
                    "stop",
                )
            } else {
                let (name, arguments) = if index % 2 == 0 {
                    ("read", json!({"path":"state.txt"}))
                } else {
                    // Empty workspace: cargo fails locally without building or accessing a provider.
                    ("shell", json!({"command":"cargo check"}))
                };
                (
                    json!({"tool_calls":[{"index":0,"id":format!("call-{index}"),
                    "function":{"name":name,"arguments":arguments.to_string()}}]}),
                    "tool_calls",
                )
            };
            let event = json!({"choices":[{"delta":delta}]});
            let stop = json!({"choices":[{"delta":{},"finish_reason":reason}]});
            let body = format!("data: {event}\n\ndata: {stop}\n\ndata: [DONE]\n\n");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).unwrap();
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "check after the external edit",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 6,
                ..AgentLoopConfig::default()
            },
        ))
        .unwrap();
    server.join().unwrap();
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 4);
    assert!(!result.tool_results[1].success);
    assert!(!result.tool_results[3].success);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancellation_during_budget_finalization_reports_cancelled() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let server = thread::spawn(move || {
        let mut first = accept_with_deadline(&listener);
        read_http_request(&mut first);
        let event = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"read",
            "function":{"name":"read","arguments":r#"{"path":"slim-finalization-missing.txt"}"#}}]}}]});
        let body = format!("data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n");
        write!(first, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut final_stream = accept_with_deadline(&listener);
        let request = read_http_request(&mut final_stream);
        assert!(request.contains("Budget exhausted"));
        server_cancel.cancel();
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).unwrap();
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation);
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "one attempt",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .unwrap();
    server.join().unwrap();
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(result.tool_results.len(), 1);
}

#[test]
fn failed_volatile_shell_keeps_its_side_effects_and_allows_continuation() {
    let root = std::env::temp_dir().join(format!("slim-volatile-retry-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for index in 0..3 {
            let mut stream = accept_with_deadline(&listener);
            read_http_request(&mut stream);
            let (delta, reason) = if index == 2 {
                (json!({"content":"partial changes recorded"}), "stop")
            } else {
                let command = "Add-Content -LiteralPath 'progress.txt' -Value 'progressed'; exit 1";
                (
                    json!({"tool_calls":[{"index":0,"id":format!("shell-{index}"),
                    "function":{"name":"shell","arguments":json!({"command":command}).to_string()}}]}),
                    "tool_calls",
                )
            };
            let event = json!({"choices":[{"delta":delta}]});
            let stop = json!({"choices":[{"delta":{},"finish_reason":reason}]});
            let body = format!("data: {event}\n\ndata: {stop}\n\ndata: [DONE]\n\n");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).unwrap();
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "continue after partial effects",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 4,
                ..AgentLoopConfig::default()
            },
        ))
        .unwrap();
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    server.join().unwrap();
    assert_eq!(result.tool_results.len(), 2);
    assert!(result.tool_results.iter().all(|result| !result.success));
    assert_eq!(
        std::fs::read_to_string(root.join("progress.txt"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn structured_budget_errors_stop_without_retrying() {
    for body in [
        json!({"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"Quota exhausted"}}),
        json!({"error":{"type":"rate_limit_error","code":"credit_balance_exhausted","message":"Credit balance exhausted"}}),
        json!({"error":{"type":"rate_limit_error","details":{"error_code":"enforced_spend_limit_reached"},"message":"Spend limit reached"}}),
    ] {
        let (client, done, server) = recovery_fixture(vec![(429, body.to_string()); 3]);
        let mut runtime = Runtime::new();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime.run_agent_loop(
                &client,
                "Answer",
                OperatingMode::ReadOnly,
                std::env::temp_dir(),
                1,
                AgentLoopConfig::default(),
            ));
        let _ = done.send(());
        let requests = server.join().unwrap();
        assert!(result.is_err(), "budget exhaustion cannot succeed");
        assert_eq!(
            requests.len(),
            1,
            "budget error must not be retried: {body}"
        );
    }
}

#[test]
fn chat_stream_recovers_structured_overload_before_output() {
    let failure = format!(
        "data: {}\n\n",
        json!({"error":{"type":"server_error","code":null,"message":"Upstream overloaded"}})
    );
    let success = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"Recovered"},"finish_reason":"stop"}]})
    );
    let (client, done, server) = recovery_fixture(vec![(200, failure), (200, success)]);
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "Answer",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    assert_eq!(
        result.expect("explicit overload is recoverable").stop,
        AgentLoopStop::ProviderCompleted
    );
    assert_eq!(requests.len(), 2);
}

fn recovery_fixture(
    responses: Vec<(u16, String)>,
) -> (
    HttpProviderClient<OpenAiCompatibleAdapter>,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<Vec<String>>,
) {
    recovery_fixture_with_headers(responses, "")
}

fn recovery_fixture_with_headers(
    responses: Vec<(u16, String)>,
    headers: &str,
) -> (
    HttpProviderClient<OpenAiCompatibleAdapter>,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<Vec<String>>,
) {
    let headers = headers.to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (done, stop) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                if stop.try_recv().is_ok() {
                    return requests;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "recovery fixture deadline");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            requests.push(read_http_request(&mut stream));
            let response = format!("HTTP/1.1 {status} Fixture\r\n{headers}Content-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).expect("response");
        }
        requests
    });
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(&endpoint, "fixture", "fixture"))
            .unwrap(),
        Duration::from_secs(2),
    )
    .unwrap();
    (client, done, server)
}

#[test]
fn reasoning_only_completion_is_not_success() {
    let body = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (client, done, server) = recovery_fixture(vec![(200, body.into())]);
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "Answer the question",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    assert!(
        matches!(result, Err(ProviderError::InvalidResponse { message }) if message.contains("without assistant text"))
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn transient_recovery_preserves_completed_tools_and_retries_only_the_failed_request() {
    let root = std::env::temp_dir().join(format!("slim-recovery-tools-{}", std::process::id()));
    std::fs::create_dir(&root).expect("exclusive fixture");
    let call = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"write-once","function":{"name":"write","arguments":json!({"path":"result.txt","content":"saved once"}).to_string()}}]},"finish_reason":"tool_calls"}]});
    let incomplete =
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"still thinking\"}}]}\n\n";
    let final_answer = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (client, done, server) = recovery_fixture(vec![
        (200, format!("data: {call}\n\ndata: [DONE]\n\n")),
        (503, "temporarily unavailable".into()),
        (200, incomplete.into()),
        (200, final_answer.into()),
    ]);
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "Write once, then answer",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    let result = result.expect("recovered");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].success);
    assert_eq!(
        std::fs::read_to_string(root.join("result.txt")).unwrap(),
        "saved once"
    );
    assert_eq!(requests.len(), 4);
    let bodies: Vec<&str> = requests
        .iter()
        .map(|r| r.split_once("\r\n\r\n").unwrap().1)
        .collect();
    assert_eq!(bodies[1], bodies[2]);
    assert_eq!(bodies[2], bodies[3]);
    let history: Value = serde_json::from_str(bodies[3]).unwrap();
    assert_eq!(
        history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .count(),
        1
    );
    assert_eq!(result.usage.retry_count, 2);
    std::fs::remove_dir_all(&root).expect("remove own fixture");
}

#[test]
fn provider_recovery_is_bounded_and_does_not_retry_permanent_errors() {
    for (status, max_turns, expected_requests) in [
        (503, 10, 3),
        (408, 10, 3),
        (401, 10, 1),
        (403, 10, 1),
        (503, 1, 1),
    ] {
        let (client, done, server) = recovery_fixture(vec![(status, "failed".into()); 3]);
        let mut runtime = Runtime::new();
        let config = AgentLoopConfig {
            max_turns,
            ..AgentLoopConfig::default()
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime.run_agent_loop(
                &client,
                "Answer",
                OperatingMode::ReadOnly,
                std::env::temp_dir(),
                1,
                config,
            ));
        let _ = done.send(());
        let requests = server.join().unwrap();
        assert!(
            matches!(result, Err(ProviderError::Http { .. })),
            "status {status}: {result:?}"
        );
        assert_eq!(requests.len(), expected_requests);
        assert_eq!(
            runtime
                .app
                .events()
                .iter()
                .filter(|event| matches!(event.kind, EventKind::RequestCompleted { .. }))
                .count(),
            expected_requests
        );
    }
}

#[test]
fn provider_recovery_continues_from_a_visible_partial_answer() {
    let partial = format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"content":"visible answer ".repeat(20)}}]})
    );
    let success = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":" continued"},"finish_reason":"stop"}]})
    );
    let (client, done, server) = recovery_fixture(vec![(200, partial), (200, success)]);
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "Answer",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    assert_eq!(
        result.expect("partial stream is recoverable").stop,
        AgentLoopStop::ProviderCompleted
    );
    assert_eq!(requests.len(), 2);
    let retry_body = requests[1].split_once("\r\n\r\n").unwrap().1;
    let history: Value = serde_json::from_str(retry_body).unwrap();
    let messages = history["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| {
        message["role"] == "assistant"
            && message["content"].as_str().is_some_and(|content| {
                content.contains("visible answer") && content.contains("[Interrupted turn]")
            })
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|content| content.contains("Continue from the preserved partial"))
    }));
    assert!(runtime
        .conversation()
        .iter()
        .any(|message| message.role == "assistant" && message.content.contains("continued")));
}

#[test]
fn cancellation_interrupts_provider_recovery_backoff() {
    let (client, done, server) = recovery_fixture_with_headers(
        vec![(503, "temporarily unavailable".into()); 3],
        "Retry-After: 30\r\n",
    );
    let started = Instant::now();
    let cancellation = CancellationToken::new();
    let (sender, receiver) = SessionEventSender::bounded(64, cancellation.clone());
    let cancel = cancellation.clone();
    let watcher = thread::spawn(move || loop {
        let event = receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("retry event");
        if matches!(event.kind, EventKind::ProviderPhase { detail: Some(ref detail), .. } if detail.starts_with("Retrying provider"))
        {
            cancel.cancel();
            break;
        }
    });
    let mut runtime = Runtime::new();
    runtime.app.set_event_sender(sender);
    runtime.set_cancellation_token(cancellation);
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "Answer",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    watcher.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancellation must interrupt server delay"
    );
    assert_eq!(result.unwrap().stop, AgentLoopStop::Cancelled);
    assert_eq!(requests.len(), 1);
}

#[test]
fn artifact_failure_keeps_completed_batch_in_transcript() {
    let root = std::env::temp_dir().join(format!(
        "slim-interrupted-artifact-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.txt"), "evidence ".repeat(1000)).unwrap();
    let mut runtime = Runtime::with_artifact_store(root.join("artifacts")).unwrap();
    runtime.capture_turn_transcript();
    std::fs::write(root.join("artifacts"), "blocked destination").unwrap();
    let calls = json!({"choices":[{"delta":{"tool_calls":[
        {"index":0,"id":"write-once","function":{"name":"write","arguments":r#"{"path":"done.txt","content":"once"}"#}},
        {"index":1,"id":"read-evidence","function":{"name":"read","arguments":r#"{"path":"source.txt"}"#}}
    ]},"finish_reason":"tool_calls"}]});
    let (client, done, server) =
        recovery_fixture(vec![(200, format!("data: {calls}\n\ndata: [DONE]\n\n"))]);
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "write then inspect",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_result_bytes: 128,
                ..AgentLoopConfig::default()
            },
        ));
    let _ = done.send(());
    assert_eq!(server.join().unwrap().len(), 1);
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(root.join("done.txt")).unwrap(),
        "once"
    );
    let transcript = runtime.take_turn_transcript();
    assert_eq!(transcript.iter().filter(|m| m.role == "tool").count(), 2);
    assert!(transcript
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("read-evidence")
            && m.content.contains("evidence")));
    let entries = transcript
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            slim_core::session::DurableEntry::from_provider_message(
                index.to_string(),
                None,
                "interrupted".into(),
                message,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    slim_core::session::provider_messages_from_entries(&entries).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn provider_recovery_respects_retry_after() {
    let final_body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"recovered"},"finish_reason":"stop"}]})
    );
    let (client, done, server) = recovery_fixture_with_headers(
        vec![(429, "slow down".into()), (200, final_body)],
        "Retry-After: 1\r\n",
    );
    let started = Instant::now();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Runtime::new().run_agent_loop(
            &client,
            "answer",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    assert_eq!(server.join().unwrap().len(), 2);
    assert_eq!(result.unwrap().stop, AgentLoopStop::ProviderCompleted);
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "retry ignored server delay: {:?}",
        started.elapsed()
    );
}

#[test]
fn provider_recovery_retries_headers_timeout_when_no_tools_ran() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (done, stop) = std::sync::mpsc::channel();
    let success = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"recovered"},"finish_reason":"stop"}]})
    );
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for (index, body) in [None, Some(success)].into_iter().enumerate() {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                if stop.try_recv().is_ok() {
                    return requests;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "timeout fixture deadline {index}"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            requests.push(read_http_request(&mut stream));
            if let Some(body) = body {
                let response = format!(
                    "HTTP/1.1 200 Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).expect("response");
            } else {
                thread::sleep(Duration::from_millis(400));
            }
        }
        requests
    });
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(&endpoint, "fixture", "fixture"))
            .unwrap(),
        Duration::from_millis(150),
    )
    .unwrap();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(Runtime::new().run_agent_loop(
            &client,
            "answer",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    assert_eq!(
        result.expect("headers timeout is recoverable").stop,
        AgentLoopStop::ProviderCompleted
    );
    assert_eq!(requests.len(), 2);
}

#[test]
fn malformed_arguments_are_repaired_without_executing_the_rejected_batch() {
    let root = std::env::temp_dir().join(format!("slim-argument-repair-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.txt"), "retained evidence").unwrap();
    let malformed = json!({"choices":[{"delta":{"content":"Inspecting the source.", "tool_calls":[
        {"index":0,"id":"rejected-write","function":{"name":"write","arguments":json!({"path":"must-not-exist.txt","content":"not authorized by valid batch"}).to_string()}},
        {"index":1,"id":"invalid-read","function":{"name":"read","arguments":"{\"path\":\"source.txt\""}}
    ]},"finish_reason":"tool_calls"}]});
    let corrected = json!({"choices":[{"delta":{"tool_calls":[
        {"index":0,"id":"corrected-read","function":{"name":"read","arguments":json!({"path":"source.txt"}).to_string()}}
    ]},"finish_reason":"tool_calls"}]});
    let final_answer = json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]});
    let (client, done, server) = recovery_fixture(
        vec![malformed, corrected, final_answer]
            .into_iter()
            .map(|event| (200, format!("data: {event}\n\ndata: [DONE]\n\n")))
            .collect(),
    );
    let mut runtime = Runtime::new();
    runtime.capture_turn_transcript();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "inspect",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    let result = result.expect("arguments can be repaired without effects");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("[Tool argument validation]"));
    assert!(requests[1].contains("invalid-read"));
    assert!(requests[1].contains("Inspecting the source."));
    assert!(!root.join("must-not-exist.txt").exists());
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(result.tool_results[0].name, "read");
    assert!(result.tool_results[0].success);
    assert!(runtime
        .app
        .events()
        .iter()
        .all(|event| !matches!(&event.kind,
        EventKind::ToolStarted { call_id, .. } if call_id == "rejected-write")));
    std::fs::remove_dir_all(root).unwrap();
}

fn anthropic_recovery_fixture(
    responses: Vec<(u16, String)>,
) -> (
    HttpProviderClient<AnthropicAdapter>,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<Vec<String>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (done, stop) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                if stop.try_recv().is_ok() {
                    return requests;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "anthropic recovery fixture deadline"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            requests.push(read_http_request(&mut stream));
            let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).expect("response");
        }
        requests
    });
    let client = HttpProviderClient::new(
        AnthropicAdapter::new(ProviderConfig::anthropic(
            &endpoint,
            "claude-fixture",
            "fixture",
        ))
        .unwrap(),
        Duration::from_secs(2),
    )
    .unwrap();
    (client, done, server)
}

fn anthropic_tool_sse(
    calls: &[(&str, &str, &str)],
    stop_reason: &str,
    include_block_stop: bool,
) -> String {
    let mut events = Vec::new();
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        events.push(json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name}
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"partial_json": arguments}
        }));
        if include_block_stop {
            events.push(json!({"type": "content_block_stop", "index": index}));
        }
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}}));
    events.push(json!({"type": "message_stop"}));
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn anthropic_text_sse(text: &str) -> String {
    let events = [
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ];
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

#[test]
fn anthropic_malformed_arguments_are_repaired_without_executing_the_rejected_batch() {
    let root = std::env::temp_dir().join(format!(
        "slim-anthropic-argument-repair-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.txt"), "retained evidence").unwrap();
    let malformed = anthropic_tool_sse(
        &[
            (
                "rejected-write",
                "write",
                &json!({"path":"must-not-exist.txt","content":"not authorized by valid batch"})
                    .to_string(),
            ),
            ("invalid-read", "read", "{\"path\":\"source.txt\""),
        ],
        "end_turn",
        false,
    );
    let corrected = anthropic_tool_sse(
        &[(
            "corrected-read",
            "read",
            &json!({"path":"source.txt"}).to_string(),
        )],
        "tool_use",
        true,
    );
    let (client, done, server) = anthropic_recovery_fixture(vec![
        (200, malformed),
        (200, corrected),
        (200, anthropic_text_sse("done")),
    ]);
    let mut runtime = Runtime::new();
    runtime.capture_turn_transcript();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "inspect",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ));
    let _ = done.send(());
    let requests = server.join().unwrap();
    let result = result.expect("Anthropic arguments can be repaired without effects");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("[Tool argument validation]"));
    assert!(requests[1].contains("invalid-read"));
    assert!(!root.join("must-not-exist.txt").exists());
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(result.tool_results[0].name, "read");
    assert!(result.tool_results[0].success);
    assert!(runtime.app.events().iter().all(|event| !matches!(
        &event.kind,
        EventKind::ToolStarted { call_id, .. } if call_id == "rejected-write"
    )));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn argument_repair_is_bounded_and_requires_known_identity() {
    for (id, max_turns, expected) in [
        (Some("bad-args"), 10, 3),
        (Some("bad-args"), 1, 1),
        (None, 10, 1),
    ] {
        let call = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"function":{"name":"read","arguments":"{"}}]},"finish_reason":"tool_calls"}]});
        let (client, done, server) =
            recovery_fixture(vec![(200, format!("data: {call}\n\ndata: [DONE]\n\n")); 3]);
        let mut runtime = Runtime::new();
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime.run_agent_loop(
                &client,
                "inspect",
                OperatingMode::Auto,
                std::env::temp_dir(),
                1,
                AgentLoopConfig {
                    max_turns,
                    ..AgentLoopConfig::default()
                },
            ))
            .unwrap_err();
        let _ = done.send(());
        assert_eq!(server.join().unwrap().len(), expected);
        if id.is_some() {
            assert!(
                matches!(error, ProviderError::InvalidResponse { message } if message.contains("task remains pending"))
            );
        } else {
            assert!(matches!(error, ProviderError::MalformedToolCall));
        }
        assert!(runtime
            .app
            .events()
            .iter()
            .all(|event| !matches!(event.kind, EventKind::ToolStarted { .. })));
    }
}

#[test]
fn foreground_compaction_recovers_transient_failure_and_preserves_original_on_permanent_error() {
    let text = |content: &str| {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":content},"finish_reason":"stop"}]})
        )
    };
    let history = vec![
        ProviderMessage::user("original task"),
        ProviderMessage::assistant("prior evidence", Vec::new()),
        ProviderMessage::user("continue"),
    ];
    for status in [503, 401] {
        let (client, done, server) = recovery_fixture(vec![(status, "temporary failure".into()), (200, text("## Goal\noriginal task\n## Constraints\nNone\n## Progress\nprior evidence\n## Blocked\nNone\n## Decisions\nKeep evidence\n## Next steps\nContinue\n## Critical context\nFixture")), (200, text("done"))]);
        let handle = CompactionHandle::default();
        handle.request_manual("").unwrap();
        let mut runtime = Runtime::new();
        runtime.set_compaction_handle(handle);
        let result =
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(runtime.run_agent_loop_with_messages(
                    &client,
                    &history,
                    OperatingMode::ReadOnly,
                    std::env::temp_dir(),
                    1,
                    AgentLoopConfig::default(),
                ));
        let _ = done.send(());
        let requests = server.join().unwrap();
        if status == 503 {
            assert_eq!(
                result.expect("recover compaction").stop,
                AgentLoopStop::ProviderCompleted
            );
            assert_eq!(requests.len(), 3);
            assert_eq!(
                requests[0].split_once("\r\n\r\n").unwrap().1,
                requests[1].split_once("\r\n\r\n").unwrap().1
            );
            assert!(runtime
                .app
                .events()
                .iter()
                .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
        } else {
            assert!(matches!(
                result,
                Err(ProviderError::Http { status: 401, .. })
            ));
            assert_eq!(requests.len(), 1);
            assert_eq!(runtime.conversation(), history.as_slice());
            assert!(!runtime
                .app
                .events()
                .iter()
                .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
        }
    }
}

#[test]
fn foreground_compaction_backoff_is_cancellable_and_does_not_replace_history() {
    let (client, done, server) =
        recovery_fixture_with_headers(vec![(503, "unavailable".into()); 3], "Retry-After: 30\r\n");
    let history = vec![
        ProviderMessage::user("original task"),
        ProviderMessage::assistant("prior evidence", Vec::new()),
        ProviderMessage::user("continue"),
    ];
    let handle = CompactionHandle::default();
    handle.request_manual("").unwrap();
    let cancellation = CancellationToken::new();
    let (sender, receiver) = SessionEventSender::bounded(64, cancellation.clone());
    let cancel = cancellation.clone();
    let watcher = thread::spawn(move || loop {
        let event = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        if matches!(event.kind, EventKind::ProviderPhase { detail: Some(ref detail), .. } if detail.starts_with("Retrying foreground compaction"))
        {
            cancel.cancel();
            break;
        }
    });
    let mut runtime = Runtime::new();
    runtime.set_compaction_handle(handle);
    runtime.app.set_event_sender(sender);
    runtime.set_cancellation_token(cancellation);
    let started = Instant::now();
    let result =
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime.run_agent_loop_with_messages(
                &client,
                &history,
                OperatingMode::ReadOnly,
                std::env::temp_dir(),
                1,
                AgentLoopConfig::default(),
            ));
    let _ = done.send(());
    assert_eq!(server.join().unwrap().len(), 1);
    watcher.join().unwrap();
    assert_eq!(result.unwrap().stop, AgentLoopStop::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(runtime.conversation(), history.as_slice());
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
}

#[test]
fn foreground_compaction_stops_after_two_retries_without_replacing_history() {
    let (client, done, server) = recovery_fixture(vec![(503, "unavailable".into()); 4]);
    let history = vec![
        ProviderMessage::user("original task"),
        ProviderMessage::assistant("prior evidence", Vec::new()),
        ProviderMessage::user("continue"),
    ];
    let handle = CompactionHandle::default();
    handle.request_manual("").unwrap();
    let mut runtime = Runtime::new();
    runtime.set_compaction_handle(handle);
    let result =
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime.run_agent_loop_with_messages(
                &client,
                &history,
                OperatingMode::ReadOnly,
                std::env::temp_dir(),
                1,
                AgentLoopConfig::default(),
            ));
    let _ = done.send(());
    assert_eq!(server.join().unwrap().len(), 3);
    assert!(matches!(
        result,
        Err(ProviderError::Http { status: 503, .. })
    ));
    assert_eq!(runtime.conversation(), history.as_slice());
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
}

#[test]
fn responses_server_error_continues_partial_without_replaying_completed_tools() {
    use slim_core::provider::OpenCodeGoAdapter;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let complete = json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5}}});
        let batches = [
            vec![
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"listed-once","name":"list","arguments":"{\"path\":\".\",\"max_entries\":1}"}}),
                complete.clone(),
            ],
            vec![
                json!({"type":"response.output_text.delta","delta":"Preserved partial answer."}),
                json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"upstream failed"}}}),
            ],
            vec![
                json!({"type":"response.output_text.delta","delta":"Recovered final answer."}),
                complete,
            ],
        ];
        let mut requests = Vec::new();
        for batch in batches {
            let mut stream = accept_with_deadline(&listener);
            requests.push(read_http_request(&mut stream));
            let body = batch
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    let adapter = OpenCodeGoAdapter::new(
        &endpoint,
        "muse-spark-1.3-contributor",
        "fixture",
        Some("high"),
    )
    .unwrap();
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).unwrap();
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "Inspect",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert!(requests[2].contains("listed-once"));
    assert!(requests[2].contains("function_call_output"));
    assert!(requests[2].contains("Preserved partial answer."));
    assert!(requests[2].contains("Do not repeat completed actions"));
    assert!(runtime
        .conversation()
        .iter()
        .any(|m| m.content.contains("Recovered final answer.")));
    assert_eq!(runtime.app.events().iter().filter(|event| matches!(&event.kind, EventKind::AssistantTextDelta { text } if text == "Preserved partial answer.")).count(),1);
}

/// A provider that hallucinates an `mcp` call in ReadOnly must get the
/// explicit mode gate, not a real dispatch: the meta-tool is not advertised
/// outside Auto and the worker path refuses it anyway.
#[test]
fn mcp_call_in_readonly_mode_is_rejected_by_the_gate() {
    use slim_core::mcp::{McpManager, McpServerSpec, McpTransport};
    use slim_core::process::ExecutableResolver;
    use std::collections::BTreeMap;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let server = thread::spawn(move || {
        // Turn 0: hallucinated mcp call. The request must not advertise it.
        let mut stream = accept_with_deadline(&listener);
        let request = read_http_request(&mut stream);
        let wire: Value =
            serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1).expect("JSON");
        let advertised = wire["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["function"]["name"] == "mcp");
        assert!(!advertised, "mcp must not be advertised in ReadOnly");
        let tool_call = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"mcp-1","function":{"name":"mcp","arguments":"{\"list\":true}"}}]},"finish_reason":"tool_calls"}]});
        let response = format!("data: {tool_call}\n\ndata: [DONE]\n\n");
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).expect("response");

        // Turn 1: the tool output came back; finish.
        let mut stream = accept_with_deadline(&listener);
        let request = read_http_request(&mut stream);
        assert!(request.contains("mcp-1"));
        assert!(request.contains("Auto mode"), "{request}");
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).expect("final");
    });

    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(&endpoint, "fixture", "fixture"))
            .expect("adapter"),
        Duration::from_secs(3),
    )
    .expect("client");
    let mut runtime = Runtime::new();
    // An enabled server is configured so nothing but the gate stops dispatch.
    runtime.set_mcp_manager(Some(Arc::new(McpManager::new(
        BTreeMap::from([(
            "srv".to_owned(),
            McpServerSpec {
                name: "srv".into(),
                transport: McpTransport::Stdio {
                    command: "cmd".into(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                },
                enabled: true,
                timeout: Duration::from_millis(1_000),
            },
        )]),
        std::env::temp_dir(),
        ExecutableResolver::default(),
    ))));
    let result = tokio::runtime::Runtime::new()
        .expect("tokio")
        .block_on(runtime.run_agent_loop(
            &client,
            "list servers",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 2,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 1);
    assert!(!result.tool_results[0].success);
    assert!(result.tool_results[0].output.contains("Auto mode"));
}
