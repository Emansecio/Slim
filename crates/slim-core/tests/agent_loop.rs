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

fn accept_with_deadline(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(3);
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
fn ask_question_returns_the_selected_answer_to_the_provider_in_the_same_loop() {
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
                max_turns: 2,
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
            let event = json!({
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
    assert_eq!(result.tool_results.len(), 2);
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
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn one_provider_batch_executes_tools_serially_in_source_order() {
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
    assert_eq!(
        lifecycle
            .iter()
            .map(|(phase, _, call_id, name)| (*phase, *call_id, *name))
            .collect::<Vec<_>>(),
        [
            ("start", "write-first", "write"),
            ("output", "write-first", "write"),
            ("finish", "write-first", "write"),
            ("start", "write-second", "write"),
            ("output", "write-second", "write"),
            ("finish", "write-second", "write"),
        ]
    );
    let batch_id = lifecycle.first().expect("first lifecycle event").1;
    assert!(!batch_id.is_empty());
    assert!(lifecycle
        .iter()
        .all(|(_, candidate_batch, _, _)| *candidate_batch == batch_id));

    let _ = std::fs::remove_dir_all(root);
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
    let server = thread::spawn(move || {
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
            if turn == 1 {
                assert!(request.contains("function_call_output"));
                assert!(request.contains("codex content"));
            }
            let events = if turn == 0 {
                vec![
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read","arguments":""}}),
                    json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":serde_json::json!({"path":path,"max_lines":10}).to_string()}),
                    json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read","arguments":serde_json::json!({"path":path,"max_lines":10}).to_string()}}),
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
            "read fixture",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig::default(),
        ))
        .expect("loop");
    server.join().expect("server");
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
    let content = "large-output-".repeat(32);
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
                        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
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
        format!("1: {content}\n")
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
    let server =
        thread::spawn(move || {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 16 * 1024];
            let size = stream.read(&mut request).expect("request");
            let body = String::from_utf8_lossy(&request[..size]).into_owned();
            stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        ).expect("headers");
            stream.write_all(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
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
    let tools = Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let request = client
        .adapter()
        .build_messages_request_with_tools_checked(
            &[slim_core::provider::ProviderMessage::user("small")],
            &tools,
        )
        .expect("request");
    let expected = slim_core::context::AdaptiveTokenEstimator::default().estimate(
        "openai-compatible",
        client.adapter().model(),
        request.body.chars().count() as u64,
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
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
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
                context_window_tokens: fixed_tokens + 64,
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
    assert_eq!(body["messages"][1]["content"], prompt);
    assert!(!body["messages"][1]["content"]
        .as_str()
        .expect("prompt")
        .contains("Summarize the prior agent transcript"));
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
    assert_eq!(bodies[0]["messages"][1]["content"], "start");
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
    assert_eq!(bodies[2]["messages"][1]["content"], "start");
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
                        "data: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
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
        let calls = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "intel-1", "function": {"name": "code_intel", "arguments": json!({"action": "symbol", "query": "first"}).to_string()}},
                        {"index": 1, "id": "intel-2", "function": {"name": "code_intel", "arguments": json!({"action": "symbol", "query": "second"}).to_string()}},
                        {"index": 2, "id": "intel-3", "function": {"name": "code_intel", "arguments": json!({"action": "symbol", "query": "third"}).to_string()}}
                    ]
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
        let first_pos = request.find("first").expect("first result");
        let second_pos = request.find("second").expect("second result");
        let third_pos = request.find("third").expect("third result");
        assert!(first_pos < second_pos && second_pos < third_pos);
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
    assert_eq!(result.tool_results.len(), 3);
    assert!(result.tool_results[0].output.contains("first"));
    assert!(result.tool_results[1].output.contains("second"));
    assert!(result.tool_results[2].output.contains("third"));
    let lifecycle = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { call_id, .. } => Some(("start", call_id.as_str())),
            EventKind::ToolFinished { call_id, .. } => Some(("finish", call_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        &lifecycle[..3],
        [
            ("start", "intel-1"),
            ("start", "intel-2"),
            ("start", "intel-3")
        ],
        "contiguous code_intel calls must all start before any finishes"
    );
    let finished: Vec<_> = lifecycle
        .iter()
        .filter_map(|(phase, id)| (*phase == "finish").then_some(*id))
        .collect();
    assert_eq!(finished.len(), 3);
    assert!(finished.contains(&"intel-1"));
    assert!(finished.contains(&"intel-2"));
    assert!(finished.contains(&"intel-3"));
}

#[test]
fn mixed_batch_parallelizes_contiguous_reads_without_crossing_write_barrier() {
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
            EventKind::ToolStarted { call_id, .. } => Some(("start", call_id.as_str())),
            EventKind::ToolOutput { call_id, .. } => Some(("output", call_id.as_str())),
            EventKind::ToolFinished { call_id, .. } => Some(("finish", call_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        &lifecycle[..2],
        [("start", "read-a"), ("start", "read-b")],
        "the contiguous reads must both start before either completes"
    );
    let position = |phase, call_id| {
        lifecycle
            .iter()
            .position(|candidate| *candidate == (phase, call_id))
            .expect("lifecycle event")
    };
    let write_start = position("start", "write-barrier");
    let write_finish = position("finish", "write-barrier");
    assert!(
        write_start > position("finish", "read-a")
            && write_start > position("finish", "read-b")
            && position("start", "read-d") > write_finish,
        "no segment may cross the write barrier: {lifecycle:?}"
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
fn validation_shell_joins_the_parallel_read_segment() {
    let root = std::env::temp_dir().join(format!(
        "slim-validation-segment-{}",
        std::process::id()
    ));
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
    assert_eq!(
        &lifecycle[..3],
        [("start", "read-a"), ("start", "cargo-check"), ("start", "read-b")],
        "allowlisted validation shell must start inside the parallel read segment: {lifecycle:?}"
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
    let mut runtime = Runtime::new();
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

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(handle.status(), CompactionStatus::Applied);
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
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
                context_window_tokens: fixed_tokens + 32,
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
                context_window_tokens: fixed_tokens + 32,
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
    std::fs::write(root.join("dup.txt"), "alpha\n").expect("fixture");

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
    assert_eq!(first_read["content"], "1: alpha\n");

    let second_messages = bodies[2]["messages"].as_array().expect("messages");
    assert!(second_messages.iter().any(|message| {
        message["role"] == "tool"
            && message["content"]
                .as_str()
                .is_some_and(|content| content.contains("[duplicate read result omitted"))
    }));
    assert_eq!(second_messages.last().expect("steer message")["role"], "user");

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
fn post_compaction_identical_reread_omits_full_output_with_compacted_pointer() {
    let root = std::env::temp_dir().join(format!(
        "slim-compact-dedup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(root.join("dup.txt"), "alpha\n").expect("fixture");

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
                context_window_tokens: fixed_tokens + 512,
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

    let post_compaction_read = bodies[3]["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("post-compaction tool message");
    assert_eq!(post_compaction_read["role"], "tool");
    assert!(post_compaction_read["content"]
        .as_str()
        .expect("content")
        .contains("[compacted read result omitted"));

    assert!(runtime.app.events().iter().any(|event| matches!(
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
fn mixed_batch_truncates_read_bucket_before_mutating_budget() {
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
    assert_eq!(result.tool_results.len(), 3);
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
        1
    );
    assert_eq!(
        std::fs::read_to_string(root.join("out.txt")).expect("out"),
        "done"
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
    assert!(!second_request.contains("Do not re-read"));
    assert!(third_request.contains("Do not re-read"));
    let conversation_text = runtime
        .conversation()
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(conversation_text.contains("done-steer"));
    let _ = std::fs::remove_dir_all(root);
}
