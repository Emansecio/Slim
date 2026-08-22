use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use serde_json::json;
use slim_core::provider::{
    AnthropicAdapter, HttpProviderClient, OpenAiCompatibleAdapter, ProviderCache, ProviderConfig,
    ProviderContentBlock, ProviderError, ProviderEvent, ProviderMessage,
};
use slim_core::{EventKind, Runtime};

#[test]
fn http_client_normalizes_chunked_sse_from_local_fixture_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let first = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"hi"}}]})
        );
        let second = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
        );
        stream.write_all(first.as_bytes()).expect("first");
        stream.flush().expect("flush");
        stream.write_all(second.as_bytes()).expect("second");
        stream.write_all(b"data: [DONE]\n\n").expect("done");
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
    let next_seq = tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("events");
    server.join().expect("server");
    assert_eq!(next_seq, 3);
    let events = runtime.app.drain_events();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].kind,
        EventKind::AssistantTextDelta { text: "hi".into() }
    );
    assert_eq!(
        events[1].kind,
        EventKind::AssistantEnded {
            reason: "stop".into()
        }
    );
}

#[test]
fn runtime_redacts_a_registered_secret_split_across_text_deltas() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"fixture-\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"split-secret\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-split-secret",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("fixture-split-secret");
    tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("provider run");
    server.join().expect("server");

    let text = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::AssistantTextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "[REDACTED]");
}

#[test]
fn runtime_redacts_registered_secret_echoed_by_provider_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        let body = "proxy echoed fixture-error-secret";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-error-secret",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("fixture-error-secret");
    let error = tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect_err("provider error");
    server.join().expect("server");
    assert!(matches!(
        error,
        ProviderError::Remote { message }
            if message.contains("[REDACTED]") && !message.contains("fixture-error-secret")
    ));
}

#[test]
fn runtime_redacts_a_registered_secret_split_across_tool_arguments() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-secret\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"fixture-\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"tool-secret\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-tool-secret",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("fixture-tool-secret");
    tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("provider run");
    server.join().expect("server");

    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ProviderToolCall { arguments, .. }
            if arguments == r#"{"path":"[REDACTED]"}"#
    )));
}

#[test]
fn runtime_observer_receives_first_delta_before_provider_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let first = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"first"}}]})
        );
        stream.write_all(first.as_bytes()).expect("first delta");
        stream.flush().expect("flush first delta");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release completion");
        stream
            .write_all(
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("completion");
    });

    let (event_tx, event_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter");
        let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
        let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
        let mut runtime = Runtime::new();
        runtime.app.set_event_sender(event_tx);
        tokio_runtime
            .block_on(runtime.run_provider(&client, "hello", 1))
            .expect("provider run");
    });

    let first = event_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first delta before completion");
    assert_eq!(
        first.kind,
        EventKind::AssistantTextDelta {
            text: "first".into()
        }
    );
    release_tx.send(()).expect("release server");
    worker.join().expect("worker");
    server.join().expect("server");
}

#[test]
fn http_client_does_not_follow_cross_origin_redirects() {
    let destination = TcpListener::bind("127.0.0.1:0").expect("destination bind");
    destination.set_nonblocking(true).expect("destination mode");
    let destination_address = destination.local_addr().expect("destination address");
    let (stop_tx, stop_rx) = mpsc::channel();
    let destination_hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let destination_hit_for_thread = Arc::clone(&destination_hit);
    let destination_thread = thread::spawn(move || loop {
        match destination.accept() {
            Ok(_) => {
                destination_hit_for_thread.store(true, std::sync::atomic::Ordering::Release);
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
        if stop_rx.try_recv().is_ok() {
            return;
        }
        thread::yield_now();
    });

    let origin = TcpListener::bind("127.0.0.1:0").expect("origin bind");
    let origin_address = origin.local_addr().expect("origin address");
    let origin_thread = thread::spawn(move || {
        let (mut stream, _) = origin.accept().expect("origin accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{destination_address}/redirect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).expect("redirect");
    });

    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{origin_address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    assert!(matches!(
        runtime.block_on(client.send("redirect")),
        Err(slim_core::ProviderError::Remote { .. })
    ));
    origin_thread.join().expect("origin thread");
    stop_tx.send(()).expect("stop destination");
    destination_thread.join().expect("destination thread");
    assert!(!destination_hit.load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn provider_stream_exposes_identity_preserving_tool_fragments() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let first = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {"name": "read", "arguments": "{\"path\":"}
                    }]
                }
            }]
        });
        let second = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "function": {"arguments": "\"file.txt\"}"}
                    }]
                }
            }]
        });
        let finish = json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        });
        for value in [first, second, finish] {
            let line = format!("data: {value}\n\n");
            stream.write_all(line.as_bytes()).expect("event");
        }
        stream.write_all(b"data: [DONE]\n\n").expect("done");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let events = tokio_runtime
        .block_on(client.send("hello"))
        .expect("events");
    server.join().expect("server");

    assert_eq!(
        events[0],
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: Some("call-1".into()),
            name: Some("read".into()),
            arguments: r#"{"path":"#.into(),
        }
    );
    assert_eq!(
        events[1],
        ProviderEvent::ToolCallDelta {
            index: None,
            id: None,
            name: None,
            arguments: r#""file.txt"}"#.into(),
        }
    );
    assert_eq!(
        events[2],
        ProviderEvent::Stopped {
            reason: "tool_calls".into()
        }
    );
}

#[derive(Clone, Copy)]
enum FixtureMode {
    Success,
    Failure,
    PartialThenSuccess,
    ToolCall,
    DoneOnly,
    DoneStopsReading,
    AnthropicWithoutDone,
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let body_start = loop {
        let read = stream.read(&mut chunk).expect("request");
        if read == 0 {
            break None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let start = index + 4;
            let headers = String::from_utf8_lossy(&bytes[..index]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .or_else(|| line.strip_prefix("content-length:"))
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= start + content_length {
                break Some((start, content_length));
            }
        }
    };
    let Some((start, length)) = body_start else {
        return String::new();
    };
    String::from_utf8_lossy(&bytes[..start + length]).into_owned()
}

fn write_fixture_response(stream: &mut TcpStream, body: &str, status: u16) {
    let reason = if status == 200 {
        "OK"
    } else {
        "Internal Server Error"
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).expect("response");
}

fn success_sse(anthropic: bool, include_done: bool) -> String {
    if anthropic {
        let events = [
            json!({"type":"content_block_delta","delta":{"text":"ok"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        ];
        events
            .into_iter()
            .map(|value| format!("data: {value}\n\n"))
            .chain(include_done.then(|| "data: [DONE]\n\n".to_owned()))
            .collect()
    } else {
        let events = [
            json!({"choices":[{"delta":{"content":"ok"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ];
        events
            .into_iter()
            .map(|value| format!("data: {value}\n\n"))
            .chain(include_done.then(|| "data: [DONE]\n\n".to_owned()))
            .collect()
    }
}

fn spawn_fixture_server(
    expected: usize,
    mode: FixtureMode,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..expected {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_http_request(&mut stream);
            requests.push(request.clone());
            match mode {
                FixtureMode::Failure => write_fixture_response(&mut stream, "fixture failure", 500),
                FixtureMode::PartialThenSuccess if index == 0 => write_fixture_response(
                    &mut stream,
                    &format!(
                        "data: {}\n\n",
                        json!({"choices":[{"delta":{"content":"partial"}}]})
                    ),
                    200,
                ),
                FixtureMode::ToolCall => write_fixture_response(
                    &mut stream,
                    &format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{}"}}]}}]}),
                        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})
                    ),
                    200,
                ),
                FixtureMode::DoneOnly => {
                    write_fixture_response(&mut stream, "data: [DONE]\n\n", 200)
                }
                FixtureMode::DoneStopsReading => write_fixture_response(
                    &mut stream,
                    &format!(
                        "data: {}\n\ndata: [DONE]\n\ndata: not-json\n\n",
                        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
                    ),
                    200,
                ),
                FixtureMode::AnthropicWithoutDone => {
                    write_fixture_response(&mut stream, &success_sse(true, false), 200)
                }
                _ => write_fixture_response(
                    &mut stream,
                    &success_sse(request.contains("anthropic-version"), true),
                    200,
                ),
            }
        }
        requests
    });
    (format!("http://{address}"), server)
}

#[test]
fn cache_hit_skips_network_and_changed_content_misses() {
    let (endpoint, server) = spawn_fixture_server(2, FixtureMode::Success);
    let cache = Arc::new(ProviderCache::new());
    let client = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let first = runtime.block_on(client.send("hello")).expect("first");
    let replay = runtime.block_on(client.send("hello")).expect("replay");
    let changed = runtime.block_on(client.send("changed")).expect("changed");
    let requests = server.join().expect("server");

    assert_eq!(first, replay);
    assert_eq!(first, changed);
    assert_eq!(
        requests.len(),
        2,
        "the replay must not open a second TCP request"
    );
    assert!(requests[0].contains("hello"));
    assert!(requests[1].contains("changed"));
    assert_eq!(cache.len(), 2);
}

#[test]
fn failed_and_partial_streams_are_not_cached() {
    let (endpoint, server) = spawn_fixture_server(2, FixtureMode::Failure);
    let cache = Arc::new(ProviderCache::new());
    let client = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    assert!(runtime.block_on(client.send("hello")).is_err());
    assert!(runtime.block_on(client.send("hello")).is_err());
    assert_eq!(server.join().expect("server").len(), 2);
    assert!(cache.is_empty());

    let (endpoint, server) = spawn_fixture_server(2, FixtureMode::PartialThenSuccess);
    let client = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    assert!(runtime.block_on(client.send("partial")).is_err());
    assert!(runtime.block_on(client.send("partial")).is_ok());
    assert_eq!(server.join().expect("server").len(), 2);
    assert_eq!(cache.len(), 1);
}

#[test]
fn cache_is_namespaced_by_provider_kind_and_exact_model() {
    let (endpoint, server) = spawn_fixture_server(3, FixtureMode::Success);
    let cache = Arc::new(ProviderCache::new());
    let openai_a = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint.clone(),
            "model-a",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    let openai_b = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint.clone(),
            "model-b",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    let anthropic = HttpProviderClient::with_cache(
        AnthropicAdapter::new(ProviderConfig::anthropic(
            endpoint,
            "model-a",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(openai_a.send("same")).expect("openai a");
    runtime.block_on(openai_b.send("same")).expect("openai b");
    runtime.block_on(anthropic.send("same")).expect("anthropic");
    runtime
        .block_on(openai_a.send("same"))
        .expect("openai a replay");
    runtime
        .block_on(openai_b.send("same"))
        .expect("openai b replay");
    runtime
        .block_on(anthropic.send("same"))
        .expect("anthropic replay");
    assert_eq!(server.join().expect("server").len(), 3);
    assert_eq!(cache.len(), 3);
}

#[test]
fn shared_cache_does_not_cross_configured_endpoints() {
    let (endpoint_a, server_a) = spawn_fixture_server(1, FixtureMode::Success);
    let (endpoint_b, server_b) = spawn_fixture_server(1, FixtureMode::Success);
    let cache = Arc::new(ProviderCache::new());
    let client_a = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("{endpoint_a}/?api_key=secret-a"),
            "fixture-model",
            "fixture-key-a",
        ))
        .expect("adapter a"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client a");
    let client_b = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("{endpoint_b}/?api_key=secret-b"),
            "fixture-model",
            "fixture-key-b",
        ))
        .expect("adapter b"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client b");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(client_a.send("same")).expect("endpoint a");
    runtime.block_on(client_b.send("same")).expect("endpoint b");
    assert_eq!(server_a.join().expect("server a").len(), 1);
    assert_eq!(server_b.join().expect("server b").len(), 1);
    assert_eq!(cache.len(), 2);
}

#[test]
fn tool_call_responses_are_never_cached() {
    let (endpoint, server) = spawn_fixture_server(2, FixtureMode::ToolCall);
    let cache = Arc::new(ProviderCache::new());
    let client = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(client.send("same")).expect("first");
    runtime.block_on(client.send("same")).expect("second");
    assert_eq!(server.join().expect("server").len(), 2);
    assert!(cache.is_empty());
}

#[test]
fn anthropic_text_block_stop_is_not_treated_as_a_tool_stop() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            )
            .expect("response");
    });
    let cache = Arc::new(ProviderCache::new());
    let client = HttpProviderClient::new_with_cache(
        AnthropicAdapter::new(ProviderConfig::anthropic(
            format!("http://{address}"),
            "claude-test",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
        cache.clone(),
    )
    .expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("text response");
    let mut cached_runtime = Runtime::new();
    tokio_runtime
        .block_on(cached_runtime.run_provider(&client, "hello", 1))
        .expect("cached text response");
    server.join().expect("server");
    assert_eq!(cache.len(), 1);
    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::AssistantTextDelta { text } if text == "hello"
    )));
}

#[test]
fn done_only_stream_is_incomplete() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::DoneOnly);
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    assert!(runtime.block_on(client.send("done-only")).is_err());
    server.join().expect("server");
}

#[test]
fn done_sentinel_stops_before_trailing_bytes() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::DoneStopsReading);
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    assert!(runtime.block_on(client.send("done")).is_ok());
    server.join().expect("server");
}

#[test]
fn anthropic_stop_completes_without_done_sentinel() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::AnthropicWithoutDone);
    let client = HttpProviderClient::new(
        AnthropicAdapter::new(ProviderConfig::anthropic(
            endpoint,
            "claude-test",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let events = runtime.block_on(client.send("stop")).expect("stop");
    server.join().expect("server");
    assert!(events.contains(&ProviderEvent::Stopped {
        reason: "end_turn".into()
    }));
}

#[test]
fn invalid_multimodal_input_is_rejected_before_tcp_send() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let (stop_tx, stop_rx) = mpsc::channel();
    let server = thread::spawn(move || loop {
        match listener.accept() {
            Ok(_) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return true,
        }
        if stop_rx.try_recv().is_ok() {
            return false;
        }
        thread::yield_now();
    });
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
    )
    .expect("client");
    let message =
        ProviderMessage::user("caption").with_content_blocks(vec![ProviderContentBlock::Image {
            media_type: "image/png".into(),
            data: "not-base64?".into(),
        }]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    assert!(runtime.block_on(client.send_messages(&[message])).is_err());
    stop_tx.send(()).expect("stop server");
    assert!(!server.join().expect("server"));
}
