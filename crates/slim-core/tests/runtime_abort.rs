use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;

use slim_core::context::CompactionHandle;
use slim_core::mcp::{
    McpCancellation, McpCleanupStatus, McpConnection, McpError, McpInterruption, McpManager,
    McpRequestOutcome, McpServerSpec, McpToolSummary, McpTransport,
};
use slim_core::process::ExecutableResolver;
use slim_core::runtime::{AgentLoopConfig, AgentLoopStop, CancellationToken};
use slim_core::{
    EventKind, HttpProviderClient, OpenAiCompatibleAdapter, OperatingMode, ProviderAdapter,
    ProviderConfig, ProviderMessage, Runtime, SessionEventSender,
};

fn accept_with_deadline(listener: &TcpListener) -> TcpStream {
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture accept deadline"
                );
                thread::yield_now();
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}

#[test]
fn dropping_a_pending_provider_future_preserves_app_state() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{}", listener.local_addr().expect("address")),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut runtime = Runtime::new();
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let future = runtime.run_provider(&client, "pending request", 1);
            tokio::pin!(future);
            assert!(futures_util::poll!(future).is_pending());
        });
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ContextSnapshot { .. })));
}

#[test]
fn cancellation_observed_before_the_first_provider_call_stops_without_new_effects() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "http://127.0.0.1:1",
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_millis(100)).expect("client");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation);

    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "must not reach provider",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ))
        .expect("cooperative cancellation is a loop result");

    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(result.turns, 0);
    assert!(runtime.app.events().is_empty());
}

#[test]
fn cancellation_precedes_zero_turn_limit() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "http://127.0.0.1:1",
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_millis(100)).expect("client");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation);

    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "cancelled before turn budget",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("cooperative cancellation is a loop result");

    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(result.turns, 0);
}

#[test]
fn cancellation_after_provider_completed_blocks_tool_execution_boundary() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let tool = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "write-1",
                        "function": {
                            "name": "write",
                            "arguments": json!({
                                "path": "cancelled-after-provider.txt",
                                "content": "must not be written"
                            }).to_string()
                        }
                    }]
                }
            }]
        });
        stream
            .write_all(format!("data: {tool}\n\n").as_bytes())
            .expect("tool delta");
        stream
            .write_all(
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("finish");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let cancellation = CancellationToken::new();
    let cancellation_after_provider = cancellation.clone();
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation);
    let root = std::env::temp_dir().join(format!("slim-runtime-abort-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    tokio_runtime
        .block_on(runtime.run_provider(&client, "write one file", 1))
        .expect("provider completed before cancellation");
    server.join().expect("server");

    cancellation_after_provider.cancel();
    let error = runtime
        .execute_tool(
            OperatingMode::Auto,
            &root,
            "write",
            &json!({
                "path": root.join("cancelled-after-provider.txt"),
                "content": "must not be written"
            })
            .to_string(),
            4,
        )
        .expect_err("cancelled boundary must not start a tool");

    assert!(matches!(error, slim_core::ProviderError::Cancelled));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ToolStarted { .. })));
    assert!(!root.join("cancelled-after-provider.txt").exists());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn cancellation_interrupts_an_idle_provider_stream_before_request_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (content_tx, content_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":0}}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"before-cancel\"}}]}\n\n",
            )
            .expect("usage and content");
        stream.flush().expect("content flush");
        content_tx.send(()).expect("content signal");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancellation completed before fixture release");
        let _ = stream.write_all(b"data: [DONE]\n\n");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let cancellation = CancellationToken::new();
    let cancel_later = cancellation.clone();
    let canceller = thread::spawn(move || {
        content_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("content arrived");
        thread::sleep(Duration::from_millis(50));
        cancel_later.cancel();
    });
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation);
    let started = Instant::now();
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "idle stream",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig::default(),
        ))
        .expect("cooperative cancellation is a loop result");
    let elapsed = started.elapsed();

    canceller.join().expect("canceller");
    release_tx.send(()).expect("release fixture");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(result.usage.total_input_tokens(), 7);
    assert_eq!(result.usage.output_tokens, 0);
    assert_eq!(
        result.next_seq,
        runtime.app.events().last().expect("last event").seq + 1
    );
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantTextDelta { .. })));
    assert!(elapsed < Duration::from_millis(450));
}

#[test]
fn cancellation_interrupts_an_idle_compaction_provider_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (request_started_tx, request_started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 64 * 1024];
        let size = stream.read(&mut request).expect("request");
        let body = String::from_utf8_lossy(&request[..size]);
        assert!(body.contains("You are a context compactor"));
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":0}}\n\n",
            )
            .expect("summary usage");
        stream.flush().expect("summary usage flush");
        request_started_tx.send(()).expect("request signal");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancellation completed before fixture release");
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
    let cancellation = CancellationToken::new();
    let cancel_later = cancellation.clone();
    let canceller = thread::spawn(move || {
        request_started_rx.recv().expect("request started");
        thread::sleep(Duration::from_millis(50));
        cancel_later.cancel();
    });
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation);
    let handle = CompactionHandle::default();
    handle
        .request_manual("")
        .expect("queue summary so cancellation can interrupt the provider request");
    runtime.set_compaction_handle(handle);
    let messages = vec![
        ProviderMessage::user("root instruction"),
        ProviderMessage::assistant("prior transcript ".repeat(200), Vec::new()),
    ];
    let started = Instant::now();
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop_with_messages(
            &client,
            &messages,
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: fixed_tokens + 256,
                context_reserve_tokens: 100,
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("cooperative cancellation is a loop result");
    let elapsed = started.elapsed();

    canceller.join().expect("canceller");
    release_tx.send(()).expect("release fixture");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(result.usage.total_input_tokens(), 0);
    assert_eq!(result.usage.compaction_input_tokens, 7);
    assert_eq!(result.usage.cancelled_requests, 1);
    let request = result.usage.requests.first().expect("compaction request");
    assert_eq!(request.total_input_tokens(), 7);
    assert!(request.cancelled);
    assert!(request.failed);
    assert_eq!(
        result.next_seq,
        runtime.app.events().last().expect("last event").seq + 1
    );
    assert!(elapsed < Duration::from_millis(450));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
}

#[test]
fn cancellation_wins_provider_error_without_losing_emitted_sequence() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"emitted\"}}]}\n\n")
            .expect("content");
        stream.flush().expect("content flush");
        cancelled_rx.recv().expect("cancellation signal");
        stream
            .write_all(b"data: not-json\n\n")
            .expect("provider error");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let cancellation = CancellationToken::new();
    let cancel_later = cancellation.clone();
    let (event_tx, event_rx) = SessionEventSender::bounded(16, cancellation.clone());
    let canceller = thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            if matches!(event.kind, EventKind::AssistantTextDelta { .. }) {
                cancel_later.cancel();
                cancelled_tx.send(()).expect("server cancellation signal");
                break;
            }
        }
    });
    let mut runtime = Runtime::new();
    runtime.app.set_event_sender(event_tx);
    runtime.set_cancellation_token(cancellation);
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "error after emitted event",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("cancellation is a loop result");

    canceller.join().expect("canceller");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(
        result.next_seq,
        runtime
            .app
            .events()
            .last()
            .map_or(1, |event| event.seq.saturating_add(1))
    );
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantTextDelta { .. })));
}

#[test]
fn cancellation_during_tool_closes_the_tool_lifecycle_and_preserves_next_seq() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let tool = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "slow-shell-1",
                        "function": {
                            "name": "shell",
                            "arguments": json!({"command": "Start-Sleep -Milliseconds 5000"}).to_string()
                        }
                    }]
                }
            }]
        });
        let finish = json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]});
        let body = format!("data: {tool}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
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
    let cancellation = CancellationToken::new();
    let cancel_later = cancellation.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        cancel_later.cancel();
    });
    let root = std::env::temp_dir().join(format!("slim-runtime-tool-abort-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation);
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "run the slow shell",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("cooperative cancellation is a loop result");

    canceller.join().expect("canceller");
    server.join().expect("server");
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(result.tool_results.len(), 1);
    assert!(!result.tool_results[0].success);
    assert!(result.next_seq >= 5);
    let kinds = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::ToolStarted { .. } => Some("started"),
            EventKind::ToolOutput { .. } => Some("output"),
            EventKind::ToolFinished { .. } => Some("finished"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["started", "output", "finished"]);
    assert_eq!(
        runtime
            .conversation()
            .iter()
            .filter(|m| m.role == "tool")
            .count(),
        1
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

struct CancelledMcpConnection {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    calls: AtomicUsize,
    closed: AtomicBool,
}

#[async_trait::async_trait]
impl McpConnection for CancelledMcpConnection {
    async fn request(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        Err(McpError::Protocol(format!(
            "unexpected non-cancellable MCP request: {method}"
        )))
    }

    async fn request_cancellable(
        &self,
        method: &str,
        _params: serde_json::Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<serde_json::Value> {
        if method != "tools/call" {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(format!(
                "unexpected MCP method: {method}"
            ))));
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(started) = self
            .started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = started.send(());
        }
        cancellation.cancelled().await;
        McpRequestOutcome::OutcomeUncertain {
            interruption: McpInterruption::Cancelled,
            cleanup: McpCleanupStatus::NotRequired,
        }
    }

    async fn close_for_cleanup(&self) -> McpCleanupStatus {
        self.closed.store(true, Ordering::Release);
        McpCleanupStatus::Confirmed
    }

    async fn notify(&self, _method: &str, _params: serde_json::Value) {}

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[test]
fn cancellation_during_mcp_call_finishes_lifecycle_with_uncertain_output_and_consistent_sequence() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut stream = accept_with_deadline(&listener);
        let mut request = [0_u8; 16 * 1024];
        let size = stream.read(&mut request).expect("provider request");
        assert!(size > 0, "provider request reached fixture");

        let arguments = json!({
            "server": "fixture",
            "tool": "ping",
            "arguments": {"operation": "non-replayable"},
        });
        let call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "mcp-call-uncertain",
                        "function": {
                            "name": "mcp",
                            "arguments": arguments.to_string(),
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let body = format!("data: {call}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream
            .write_all(response.as_bytes())
            .expect("provider response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let connection = Arc::new(CancelledMcpConnection {
        started: Mutex::new(Some(started_tx)),
        calls: AtomicUsize::new(0),
        closed: AtomicBool::new(false),
    });
    let spec = McpServerSpec {
        name: "fixture".into(),
        transport: McpTransport::Stdio {
            command: "controlled-test-connection".into(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_secs(5),
    };
    let manager = Arc::new(McpManager::new(
        std::collections::BTreeMap::from([(spec.name.clone(), spec.clone())]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ));
    manager.insert_connection(
        spec,
        connection.clone() as Arc<dyn McpConnection>,
        vec![McpToolSummary {
            name: "ping".into(),
            description: Some("controlled cancellation fixture".into()),
            schema: json!({"type":"object"}),
        }],
    );

    let cancellation = CancellationToken::new();
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation.clone());
    runtime.set_mcp_manager(Some(manager));
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime.block_on(async {
        let run = runtime.run_agent_loop(
            &client,
            "make one non-replayable MCP call",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        );
        let cancel_when_called = async {
            tokio::time::timeout(Duration::from_secs(3), started_rx)
                .await
                .expect("MCP call starts before timeout")
                .expect("MCP call start signal");
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(run, cancel_when_called);
        result
    });

    server.join().expect("provider fixture");
    let result = result.expect("cooperative cancellation is a loop result");
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(connection.calls.load(Ordering::Relaxed), 1);
    assert!(connection.is_closed());
    assert_eq!(result.tool_results.len(), 1);
    assert!(!result.tool_results[0].success);
    assert!(result.tool_results[0]
        .output
        .contains("outcome is uncertain"));
    assert!(result.tool_results[0].output.contains("do not replay"));

    let events = runtime.app.events();
    let lifecycle = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { call_id, name, .. } if call_id == "mcp-call-uncertain" => {
                Some(("started", event.seq, name.as_str()))
            }
            EventKind::ToolOutput {
                call_id,
                name,
                output,
                ..
            } if call_id == "mcp-call-uncertain" => {
                assert!(output.contains("outcome is uncertain"));
                assert!(output.contains("do not replay"));
                Some(("output", event.seq, name.as_str()))
            }
            EventKind::ToolFinished {
                call_id,
                name,
                success,
                ..
            } if call_id == "mcp-call-uncertain" => {
                assert!(!success);
                Some(("finished", event.seq, name.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle
            .iter()
            .map(|(kind, _, _)| *kind)
            .collect::<Vec<_>>(),
        ["started", "output", "finished"]
    );
    assert!(lifecycle.iter().all(|(_, _, name)| *name == "mcp"));
    assert_eq!(lifecycle[1].1, lifecycle[0].1 + 1);
    assert_eq!(lifecycle[2].1, lifecycle[1].1 + 1);
    assert!(events.windows(2).all(|pair| pair[1].seq > pair[0].seq));
    assert_eq!(
        result.next_seq,
        events.last().expect("event journal").seq + 1
    );
}

fn read_mcp_http_request(mut stream: TcpStream) -> (TcpStream, serde_json::Value) {
    let body = {
        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("request line");
        assert!(request_line.starts_with("POST "), "{request_line:?}");

        let mut content_length = None;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("request header");
            if header == "\r\n" || header == "\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = Some(value.trim().parse::<usize>().expect("content length"));
                }
            }
        }
        let mut body = vec![0; content_length.expect("content-length header")];
        reader.read_exact(&mut body).expect("request body");
        body
    };
    (
        stream,
        serde_json::from_slice(&body).expect("JSON-RPC request body"),
    )
}

fn write_mcp_http_json(stream: &mut TcpStream, body: &serde_json::Value) {
    let body = serde_json::to_vec(body).expect("serialize MCP response");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).expect("MCP headers");
    stream.write_all(&body).expect("MCP JSON body");
    let _ = stream.flush();
}

#[test]
fn cancellation_during_initialized_notification_reports_unconfirmed_cleanup_without_call() {
    let provider_listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
    let provider_address = provider_listener.local_addr().expect("provider address");
    let provider = thread::spawn(move || {
        let mut stream = accept_with_deadline(&provider_listener);
        let mut request = [0_u8; 16 * 1024];
        let size = stream.read(&mut request).expect("provider request");
        assert!(size > 0, "provider request reaches local fixture");

        let arguments = json!({
            "server": "fixture",
            "tool": "ping",
            "arguments": {"operation": "no-call-before-init"},
        });
        let call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "mcp-init-cancel",
                        "function": {
                            "name": "mcp",
                            "arguments": arguments.to_string(),
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let body = format!("data: {call}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream
            .write_all(response.as_bytes())
            .expect("provider tool-call response");
    });

    let mcp_listener = TcpListener::bind("127.0.0.1:0").expect("bind MCP fixture");
    let mcp_address = mcp_listener.local_addr().expect("MCP address");
    let (notification_seen_tx, notification_seen_rx) = tokio::sync::oneshot::channel();
    let (release_mcp_tx, release_mcp_rx) = mpsc::sync_channel(1);
    let mcp_server = thread::spawn(move || {
        let mut methods = Vec::new();
        let init_stream = accept_with_deadline(&mcp_listener);
        let (mut init_stream, initialize) = read_mcp_http_request(init_stream);
        methods.push(
            initialize["method"]
                .as_str()
                .expect("initialize method")
                .to_owned(),
        );
        assert_eq!(methods[0], "initialize");
        write_mcp_http_json(
            &mut init_stream,
            &json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "runtime-abort-fixture", "version": "1"},
                },
            }),
        );

        let notify_stream = accept_with_deadline(&mcp_listener);
        let (mut notify_stream, notification) = read_mcp_http_request(notify_stream);
        methods.push(
            notification["method"]
                .as_str()
                .expect("notification method")
                .to_owned(),
        );
        assert_eq!(methods[1], "notifications/initialized");
        notification_seen_tx
            .send(())
            .expect("runtime cancellation task waits for notification");
        release_mcp_rx
            .recv()
            .expect("test releases local MCP HTTP fixture");
        let response = b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = notify_stream.write_all(response);

        // The cancellation outcome is returned only after the lazy connect
        // path has stopped. Drain any already-issued request to make a
        // regression to tools/list or tools/call visible in this fixture.
        mcp_listener
            .set_nonblocking(true)
            .expect("nonblocking MCP fixture listener");
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            match mcp_listener.accept() {
                Ok((stream, _)) => {
                    let (_, unexpected) = read_mcp_http_request(stream);
                    methods.push(
                        unexpected["method"]
                            .as_str()
                            .expect("unexpected MCP method")
                            .to_owned(),
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::yield_now();
                }
                Err(error) => panic!("MCP fixture accept: {error}"),
            }
        }
        methods
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{provider_address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("provider adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let spec = McpServerSpec {
        name: "fixture".into(),
        transport: McpTransport::Http {
            url: format!("http://{mcp_address}"),
            headers: std::collections::BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_secs(5),
    };
    let manager = Arc::new(McpManager::new(
        std::collections::BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ));
    let cancellation = CancellationToken::new();
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(cancellation.clone());
    runtime.set_mcp_manager(Some(manager));
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let result = tokio_runtime.block_on(async {
        let run = runtime.run_agent_loop(
            &client,
            "make one MCP call",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        );
        let cancel_during_initialization = async {
            tokio::time::timeout(Duration::from_secs(3), notification_seen_rx)
                .await
                .expect("initialized notification reaches MCP fixture")
                .expect("notification readiness signal");
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(run, cancel_during_initialization);
        result
    });

    provider.join().expect("provider fixture exits");
    release_mcp_tx
        .send(())
        .expect("release MCP fixture after cancellation completes");
    let mcp_methods = mcp_server.join().expect("MCP HTTP fixture exits");
    let result = result.expect("cooperative cancellation is a loop result");
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(result.tool_results.len(), 1);
    assert!(!result.tool_results[0].success);
    let tool_output = result.tool_results[0].output.to_lowercase();
    assert!(tool_output.contains("cleanup"));
    assert!(tool_output.contains("unconfirmed") || tool_output.contains("not confirmed"));

    let events = runtime.app.events();
    let lifecycle = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { call_id, name, .. } if call_id == "mcp-init-cancel" => {
                Some(("started", event.seq, name.as_str()))
            }
            EventKind::ToolOutput {
                call_id,
                name,
                output,
                ..
            } if call_id == "mcp-init-cancel" => {
                let output = output.to_lowercase();
                assert!(output.contains("cleanup"));
                assert!(output.contains("unconfirmed") || output.contains("not confirmed"));
                Some(("output", event.seq, name.as_str()))
            }
            EventKind::ToolFinished {
                call_id,
                name,
                success,
                ..
            } if call_id == "mcp-init-cancel" => {
                assert!(!success);
                Some(("finished", event.seq, name.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle
            .iter()
            .map(|(kind, _, _)| *kind)
            .collect::<Vec<_>>(),
        ["started", "output", "finished"]
    );
    assert!(lifecycle.iter().all(|(_, _, name)| *name == "mcp"));
    assert_eq!(lifecycle[1].1, lifecycle[0].1 + 1);
    assert_eq!(lifecycle[2].1, lifecycle[1].1 + 1);
    assert!(events.windows(2).all(|pair| pair[1].seq > pair[0].seq));
    assert_eq!(
        result.next_seq,
        events.last().expect("event journal").seq + 1
    );

    assert_eq!(
        mcp_methods,
        vec![
            "initialize".to_owned(),
            "notifications/initialized".to_owned()
        ]
    );
}
