use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;

use slim_core::context::CompactionHandle;
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
    std::fs::remove_dir_all(root).expect("cleanup");
}
