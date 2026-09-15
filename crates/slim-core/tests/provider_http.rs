use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slim_core::provider::{
    run_http_provider_messages, AnthropicAdapter, HttpProviderClient, HttpRequest,
    OpenAiCodexAdapter, OpenAiCompatibleAdapter, OpenCodeGoAdapter, ProviderAdapter, ProviderCache,
    ProviderConfig, ProviderContentBlock, ProviderError, ProviderEvent, ProviderKind,
    ProviderMessage, ProviderPhase, ProviderTimeouts,
};
use slim_core::runtime::CancellationToken;
use slim_core::{
    AgentLoopConfig, AppHandle, EventKind, OperatingMode, Runtime, SessionEventSender,
};

struct CountingAdapter {
    inner: OpenAiCompatibleAdapter,
    builds: Arc<AtomicUsize>,
    cache_keys: Arc<AtomicUsize>,
}

struct SpareCacheKeyAdapter {
    inner: OpenAiCompatibleAdapter,
    key_capacity: usize,
}

impl ProviderAdapter for SpareCacheKeyAdapter {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.inner.build_request(prompt)
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.inner
            .build_messages_request_with_tools(messages, tools)
    }

    fn cache_key_with_tools(&self, _messages: &[ProviderMessage], _tools: &[Value]) -> String {
        let mut key = String::with_capacity(self.key_capacity);
        key.push_str("spare-key");
        key
    }

    fn cache_key_for_prepared(
        &self,
        _request: &slim_core::provider::PreparedProviderRequest,
    ) -> String {
        let mut key = String::with_capacity(self.key_capacity);
        key.push_str("spare-key");
        key
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.inner.parse_event(value)
    }
}

impl ProviderAdapter for CountingAdapter {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.builds.fetch_add(1, Ordering::Relaxed);
        self.inner.build_request(prompt)
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.builds.fetch_add(1, Ordering::Relaxed);
        self.inner
            .build_messages_request_with_tools(messages, tools)
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.inner.parse_event(value)
    }

    fn cache_key_with_tools(&self, messages: &[ProviderMessage], tools: &[Value]) -> String {
        self.cache_keys.fetch_add(1, Ordering::Relaxed);
        self.inner.cache_key_with_tools(messages, tools)
    }

    fn cache_key_for_prepared(
        &self,
        request: &slim_core::provider::PreparedProviderRequest,
    ) -> String {
        self.cache_keys.fetch_add(1, Ordering::Relaxed);
        self.inner.cache_key_for_prepared(request)
    }
}

#[test]
fn uncached_stream_builds_http_request_once() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::Success);
    let builds = Arc::new(AtomicUsize::new(0));
    let adapter = CountingAdapter {
        inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        builds: Arc::clone(&builds),
        cache_keys: Arc::new(AtomicUsize::new(0)),
    };
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("send");
    server.join().expect("server");

    assert_eq!(builds.load(Ordering::Relaxed), 1);
}

#[test]
fn shared_transport_skips_cache_key_hash_on_live_stream() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::Success);
    let builds = Arc::new(AtomicUsize::new(0));
    let cache_keys = Arc::new(AtomicUsize::new(0));
    let adapter = CountingAdapter {
        inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        builds: Arc::clone(&builds),
        cache_keys: Arc::clone(&cache_keys),
    };
    let client = HttpProviderClient::with_shared_transport(
        adapter,
        ProviderTimeouts::production(Duration::from_secs(2)),
    )
    .expect("client");
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("send");
    server.join().expect("server");

    assert_eq!(builds.load(Ordering::Relaxed), 1);
    assert_eq!(cache_keys.load(Ordering::Relaxed), 0);
}

#[test]
fn connect_timeout_fails_fast_without_waiting_for_idle() {
    for shared in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept TLS connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            let mut hello = [0_u8; 4096];
            assert!(stream.read(&mut hello).expect("TLS ClientHello") > 0);
            // Do not complete TLS: the transport's connection deadline must fire.
            let _ = stream.read(&mut hello);
        });
        let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("https://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter");
        let timeouts = ProviderTimeouts {
            connect: Duration::from_millis(100),
            idle: Duration::from_secs(30),
            first_semantic: Duration::from_secs(30),
            wall: Duration::from_secs(30),
        };
        let client = if shared {
            HttpProviderClient::with_shared_transport(adapter, timeouts)
        } else {
            HttpProviderClient::with_timeouts(adapter, timeouts)
        }
        .expect("client");
        let started = Instant::now();
        let error = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(client.send("hello"))
            .expect_err("TLS must time out");
        let elapsed = started.elapsed();
        server.join().expect("server");
        assert!(
            matches!(error, ProviderError::Transport { .. }),
            "{error:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "connect timeout must fail fast: {elapsed:?}"
        );
    }
}

#[test]
fn accepted_connection_without_headers_fails_at_first_semantic_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_http_request(&mut stream);
        let mut leftover = [0_u8; 1];
        let _ = stream.read(&mut leftover);
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let timeouts = ProviderTimeouts {
        connect: Duration::from_millis(40),
        idle: Duration::from_secs(30),
        first_semantic: Duration::from_millis(150),
        wall: Duration::from_secs(30),
    };
    let client = HttpProviderClient::with_timeouts(adapter, timeouts).expect("client");
    let started = Instant::now();
    let error = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect_err("silent accepted socket must stop at first-semantic budget");
    let elapsed = started.elapsed();
    let _ = server.join();

    assert!(
        matches!(error, ProviderError::Transport { .. }),
        "expected transport error, got {error:?}"
    );
    assert!(
        !error.is_retryable(),
        "the server already received the POST"
    );
    assert!(
        elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2),
        "header wait must use first-semantic budget, elapsed {elapsed:?}"
    );
}

#[test]
fn accepted_connection_waits_for_headers_within_first_semantic_budget() {
    for shared in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("POST "));
            thread::sleep(Duration::from_millis(200));
            let body = success_sse(false, true);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter");
        let timeouts = ProviderTimeouts {
            connect: Duration::from_millis(40),
            idle: Duration::from_secs(2),
            first_semantic: Duration::from_secs(2),
            wall: Duration::from_secs(3),
        };
        let client = if shared {
            HttpProviderClient::with_shared_transport(adapter, timeouts)
        } else {
            HttpProviderClient::with_timeouts(adapter, timeouts)
        }
        .expect("client");
        let result = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(client.send("hello"));
        server.join().expect("server");
        let events = result.expect("server queue time is not a TCP connection timeout");
        assert!(events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TextDelta(text) if text == "ok")));
    }
}

#[test]
fn heartbeat_stream_cannot_extend_first_semantic_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _request = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let heartbeat = b": keepalive\n\n";
        for _ in 0..30 {
            let chunk = format!("{:X}\r\n", heartbeat.len());
            if stream.write_all(chunk.as_bytes()).is_err()
                || stream.write_all(heartbeat).is_err()
                || stream.write_all(b"\r\n").is_err()
            {
                break;
            }
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(15));
        }
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::with_timeouts(
        adapter,
        ProviderTimeouts {
            connect: Duration::from_millis(500),
            idle: Duration::from_millis(500),
            first_semantic: Duration::from_millis(100),
            wall: Duration::from_secs(1),
        },
    )
    .expect("client");
    let started = Instant::now();
    let error = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect_err("heartbeats are not semantic content");
    let elapsed = started.elapsed();
    server.join().expect("server");

    assert!(
        matches!(&error, ProviderError::Transport { message, .. } if message.contains("first semantic")),
        "{error:?}"
    );
    assert!(!error.is_retryable(), "a response already started");
    assert!(
        elapsed < Duration::from_millis(400),
        "semantic deadline must win over idle timeout: {elapsed:?}"
    );
}

#[test]
fn shared_transport_reuses_one_keep_alive_connection_across_clients() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        for index in 0..2 {
            let request = read_http_request(&mut stream);
            assert!(
                !request.is_empty(),
                "request {index} must reuse this connection"
            );
            let body = success_sse(false, true);
            let connection = if index == 0 { "keep-alive" } else { "close" };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n{body}",
                body.len()
            )
            .expect("response");
            stream.flush().expect("flush");
        }
    });
    let endpoint = format!("http://{address}");
    let make_adapter = || {
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint.clone(),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter")
    };
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let first = HttpProviderClient::with_shared_transport(
            make_adapter(),
            ProviderTimeouts::production(Duration::from_secs(2)),
        )
        .expect("first client");
        first.send("one").await.expect("first response");
        let second = HttpProviderClient::with_shared_transport(
            make_adapter(),
            ProviderTimeouts::production(Duration::from_secs(2)),
        )
        .expect("second client");
        second.send("two").await.expect("second response");
    });
    server.join().expect("server");
}

#[test]
fn production_timeouts_allow_active_stream_past_idle_duration() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        for text in ["one", "two", "three"] {
            stream
                .write_all(
                    format!(
                        "data: {}\n\n",
                        json!({"choices":[{"delta":{"content":text}}]})
                    )
                    .as_bytes(),
                )
                .expect("delta");
            stream.flush().expect("flush");
            thread::sleep(Duration::from_millis(30));
        }
        stream
            .write_all(
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
                )
                .as_bytes(),
            )
            .expect("stop");
    });
    let builds = Arc::new(AtomicUsize::new(0));
    let adapter = CountingAdapter {
        inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        builds: Arc::clone(&builds),
        cache_keys: Arc::new(AtomicUsize::new(0)),
    };
    let client = HttpProviderClient::with_timeouts(
        adapter,
        ProviderTimeouts::production(Duration::from_millis(50)),
    )
    .expect("client");
    let events = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("active stream");
    server.join().expect("server");
    let text = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "onetwothree");
}

#[test]
fn provider_stream_reports_content_free_transport_phases() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::Success);
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        endpoint,
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let events = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("response");
    server.join().expect("server");

    let phases = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::Phase { phase, elapsed_ms } => Some((*phase, *elapsed_ms)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        phases.iter().map(|(phase, _)| *phase).collect::<Vec<_>>(),
        vec![
            ProviderPhase::Connecting,
            ProviderPhase::HeadersReceived,
            ProviderPhase::FirstByte,
            ProviderPhase::FirstSemantic,
        ]
    );
    assert!(phases.windows(2).all(|pair| pair[0].1 <= pair[1].1));
}

#[test]
fn compaction_request_preserves_effort_and_bounds_existing_output() {
    let adapter = OpenAiCompatibleAdapter::new(
        ProviderConfig::openai("http://127.0.0.1:9", "fixture-model", "fixture-key")
            .with_reasoning_effort("high")
            .with_max_output_tokens(32_000),
    )
    .expect("adapter");
    let request = adapter
        .build_compaction_request_checked(&[ProviderMessage::user("summary input")])
        .expect("compaction request");
    let body: Value = serde_json::from_str(&request.body).expect("json body");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["max_tokens"], 2_048);
    assert_eq!(
        body["messages"][0]["content"],
        slim_core::context::COMPACTION_SYSTEM_PROMPT
    );
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "summary input");
    assert!(!request
        .body
        .contains(slim_core::provider::NATIVE_SYSTEM_PROMPT));
}

#[test]
fn codex_compaction_request_does_not_invent_output_control() {
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/backend-api",
        "gpt-test",
        "oauth-secret",
        "account-id",
    ))
    .expect("adapter");
    let request = adapter
        .build_compaction_request_checked(&[ProviderMessage::user("summary input")])
        .expect("compaction request");
    let body: Value = serde_json::from_str(&request.body).expect("json body");

    assert!(body.get("max_output_tokens").is_none());
}

#[test]
fn http_client_normalizes_chunked_sse_from_local_fixture_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = read_http_request(&mut stream);
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
        request
    });

    let builds = Arc::new(AtomicUsize::new(0));
    let adapter = CountingAdapter {
        inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        builds: Arc::clone(&builds),
        cache_keys: Arc::new(AtomicUsize::new(0)),
    };
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let next_seq = tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("events");
    let request = server.join().expect("server");
    let wire_chars = request
        .split_once("\r\n\r\n")
        .expect("body")
        .1
        .chars()
        .count() as u64;
    let events = runtime.app.drain_events();
    assert_eq!(next_seq, events.len() as u64 + 1);
    let events = events
        .into_iter()
        .filter(|event| !matches!(event.kind, EventKind::ProviderPhase { .. }))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 4);
    assert!(matches!(
        events[0].kind,
        EventKind::ContextSnapshot {
            estimated_tokens,
            serialized_chars,
            context_window_tokens: 0,
            ..
        } if estimated_tokens > 0 && serialized_chars == wire_chars
    ));
    assert_eq!(
        events[1].kind,
        EventKind::AssistantTextDelta { text: "hi".into() }
    );
    assert_eq!(
        events[2].kind,
        EventKind::AssistantEnded {
            reason: "stop".into()
        }
    );
    assert!(matches!(
        events[3].kind,
        EventKind::RequestCompleted {
            cancelled: false,
            failed: false,
            ..
        }
    ));
    assert_eq!(builds.load(Ordering::Relaxed), 1);
}

#[test]
fn http_client_preserves_utf8_split_across_sse_chunks() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"olá\"}}]}\n\n".as_bytes();
        let split = body
            .windows(2)
            .position(|bytes| bytes == "á".as_bytes())
            .expect("utf-8 marker")
            + 1;
        for chunk in [&body[..split], &body[split..]] {
            write!(stream, "{:X}\r\n", chunk.len()).expect("chunk size");
            stream.write_all(chunk).expect("chunk body");
            stream.write_all(b"\r\n").expect("chunk end");
            stream.flush().expect("flush");
            thread::sleep(Duration::from_millis(20));
        }
        let tail =
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        write!(stream, "{:X}\r\n", tail.len()).expect("tail size");
        stream.write_all(tail).expect("tail");
        stream.write_all(b"\r\n0\r\n\r\n").expect("chunked eof");
    });

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let events = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("response");
    server.join().expect("server");

    let text = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "olá");
}

#[test]
fn http_stream_preserves_whitespace_deltas_in_output_and_chat_history() {
    let reasoning_chunks = [" ", "\n", "\r\n", "\t", "推論"];
    let text_chunks = [" ", "\n", "\r\n", "\t", "café"];
    let expected_reasoning = reasoning_chunks.concat();
    let expected_text = text_chunks.concat();
    let mut body = String::new();
    for chunk in &reasoning_chunks {
        body.push_str(&format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"reasoning_content":chunk}}]})
        ));
    }
    for chunk in &text_chunks {
        body.push_str(&format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":chunk}}]})
        ));
    }
    body.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
    ));

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).expect("response");
    });

    let adapter = OpenAiCompatibleAdapter::new(
        ProviderConfig::openai(
            format!("http://{address}"),
            "deepseek-v4-flash",
            "fixture-key",
        )
        .with_reasoning_effort("high"),
    )
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut runtime = Runtime::new();
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(runtime.run_agent_loop(
            &client,
            "hello",
            OperatingMode::ReadOnly,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("provider run");
    server.join().expect("server");

    assert_eq!(result.stop, slim_core::AgentLoopStop::ProviderCompleted);
    let output = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::AssistantTextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let reasoning = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(output, expected_text);
    assert_eq!(reasoning, expected_reasoning);

    let assistant = runtime
        .conversation()
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .expect("assistant history");
    assert_eq!(assistant.content, expected_text);
    assert!(assistant.chat_reasoning.is_some());
    let request = client
        .adapter()
        .build_messages_request_checked(runtime.conversation())
        .expect("history request");
    let wire: Value = serde_json::from_str(&request.body).expect("history JSON");
    let assistant_wire = wire["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant")
        .expect("assistant wire history");
    assert_eq!(assistant_wire["content"], expected_text);
    assert_eq!(assistant_wire["reasoning_content"], expected_reasoning);
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
        ProviderError::Http { message, .. }
            if message.contains("[REDACTED]") && !message.contains("fixture-error-secret")
    ));
}

#[test]
fn query_credential_is_redacted_and_error_body_read_is_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\nConnection: keep-alive\r\n\r\nquery-error-secret",
            )
            .expect("headers and secret");
        stream
            .write_all(&vec![b'x'; 8 * 1024])
            .expect("bounded excerpt source");
        stream.flush().expect("flush");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("client returned before fixture release");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}?api_key=query-error-secret"),
        "fixture-model",
        "header-secret",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(3)).expect("client");
    let started = std::time::Instant::now();
    let error = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect_err("remote error");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "bounded reader returns without waiting for EOF"
    );
    assert!(matches!(
        error,
        ProviderError::Http { message, .. }
            if message.contains("[REDACTED]") && !message.contains("query-error-secret")
    ));
    release_tx.send(()).expect("release fixture");
    server.join().expect("server");
}

#[test]
fn query_credential_echo_is_redacted_from_success_events() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"query-\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"event-secret\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}?api_key=query-event-secret"),
        "fixture-model",
        "query",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("provider");
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
    assert!(text.contains("[REDACTED]"));
    assert!(!text.contains("query-event-secret"));
    assert!(
        !text.contains("event-secret"),
        "shorter overlapping header secret must not expose query suffix"
    );
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
    let error = tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect_err("sensitive executable input must be rejected");
    server.join().expect("server");

    assert!(format!("{error:?}").contains("registered sensitive material"));
    assert!(!runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::ProviderToolCall { .. } | EventKind::ToolStarted { .. }
    )));
    assert!(!format!("{:?}", runtime.app.events()).contains("fixture-tool-secret"));
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

    let (event_tx, event_rx) = SessionEventSender::bounded(16, CancellationToken::new());
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

    let snapshot = event_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("snapshot before request");
    assert!(matches!(snapshot.kind, EventKind::ContextSnapshot { .. }));
    let first = loop {
        let event = event_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first delta before completion");
        if matches!(event.kind, EventKind::AssistantTextDelta { .. }) {
            break event;
        }
    };
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
fn runtime_accepts_terminal_usage_after_finish_reason_but_before_done() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
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
    tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("usage after finish reason");
    server.join().expect("server");

    let usage = runtime
        .app
        .events()
        .iter()
        .position(|event| matches!(event.kind, EventKind::Usage { .. }))
        .expect("usage");
    let ended = runtime
        .app
        .events()
        .iter()
        .position(|event| matches!(event.kind, EventKind::AssistantEnded { .. }))
        .expect("ended");
    assert!(usage < ended, "terminal usage must precede AssistantEnded");
}

#[test]
fn opencode_go_identical_terminal_usage_after_stop_is_idempotent() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenCodeGoAdapter::new(
        &format!("http://{address}"),
        "deepseek-v4-flash",
        "fixture-key",
        Some("max"),
    )
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("identical terminal usage is idempotent");
    server.join().expect("server");
    assert_eq!(
        runtime
            .app
            .events()
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Usage { .. }))
            .count(),
        1
    );
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantEnded { .. })));
}

#[test]
fn opencode_go_progressive_terminal_usage_keeps_the_largest_snapshot() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"Ola\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1750,\"completion_tokens\":1}}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1750,\"completion_tokens\":9}}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenCodeGoAdapter::new(
        &format!("http://{address}"),
        "deepseek-v4-flash",
        "fixture-key",
        Some("max"),
    )
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    tokio_runtime
        .block_on(runtime.run_provider(&client, "Ola", 1))
        .expect("progressive terminal usage must not abort the answer");
    server.join().expect("server");

    let usage = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::Usage {
                input_tokens,
                output_tokens,
            } => Some((input_tokens, output_tokens)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage, vec![(1750, 9)]);
    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::AssistantTextDelta { text } if text == "Ola"
    )));

    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::UsageThenFailure);
    let adapter = OpenCodeGoAdapter::new(&endpoint, "deepseek-v4-flash", "fixture-key", None)
        .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut runtime = Runtime::new();
    assert!(tokio_runtime
        .block_on(runtime.run_provider(&client, "failure", 1))
        .is_err());
    assert_eq!(server.join().expect("server").len(), 1);
    assert!(
        runtime.app.events().iter().any(|event| matches!(
            event.kind,
            EventKind::Usage {
                input_tokens: 7,
                output_tokens: 3
            }
        )),
        "failure must preserve already observed usage"
    );
    assert!(matches!(
        runtime.app.events().last().expect("completed").kind,
        EventKind::RequestCompleted { failed: true, .. }
    ));
}

#[test]
fn repeated_incomplete_post_stop_usage_component_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7}}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":8}}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
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
    assert!(tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", 1))
        .is_err());
    server.join().expect("server");
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantEnded { .. })));
}

#[test]
fn anthropic_exactness_requires_base_input_and_terminal_output_components() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"cache_read_input_tokens\":5}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
            )
            .expect("response");
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
        .block_on(runtime.run_provider(&client, "hello", 1))
        .expect("anthropic run");
    server.join().expect("server");

    let usage = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::UsagePartial {
                input_tokens,
                output_tokens,
                ..
            } => Some((false, *input_tokens, *output_tokens)),
            EventKind::Usage {
                input_tokens,
                output_tokens,
            } => Some((true, *input_tokens, *output_tokens)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage, vec![(false, 8, 0), (false, 0, 4), (true, 0, 0)]);
    assert!(!runtime.app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::ProviderPhase {
            phase: ProviderPhase::FirstSemantic,
            ..
        }
    )));
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantEnded { .. })));
    assert!(matches!(
        runtime.app.events().last().map(|event| &event.kind),
        Some(EventKind::RequestCompleted {
            cancelled: false,
            failed: false,
            ..
        })
    ));
}

#[test]
fn free_provider_bridge_rejects_non_success_stop_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut app = AppHandle::fake();
    let error = tokio_runtime
        .block_on(run_http_provider_messages(
            &client,
            &mut app,
            &[ProviderMessage::user("hello")],
            1,
        ))
        .expect_err("length is not successful completion");
    server.join().expect("server");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantEnded { .. })));
}

#[test]
fn free_provider_bridge_rejects_tool_required_stop_without_call() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut app = AppHandle::fake();
    let error = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(run_http_provider_messages(
            &client,
            &mut app,
            &[ProviderMessage::user("hello")],
            1,
        ))
        .expect_err("tool-required stop without call");
    server.join().expect("server");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantEnded { .. })));
}

#[test]
fn free_provider_bridge_preserves_parallel_fragmented_tool_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a\"}},{\"index\":1,\"id\":\"call-b\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"b\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"type\":\"function\",\"function\":{\"name\":null,\"arguments\":\".txt\\\"}\"}},{\"index\":1,\"function\":{\"arguments\":\".txt\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut app = AppHandle::fake();
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(run_http_provider_messages(
            &client,
            &mut app,
            &[ProviderMessage::user("hello")],
            1,
        ))
        .expect("parallel calls");
    server.join().expect("server");
    let calls = app
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
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        vec![
            ("call-a", "read", "{\"path\":\"a.txt\"}"),
            ("call-b", "read", "{\"path\":\"b.txt\"}")
        ]
    );
}

#[test]
fn free_provider_bridge_preserves_parallel_identical_tool_calls_by_id() {
    let calls = json!({
        "choices": [{
            "delta": {
                "tool_calls": [
                    {"index": 0, "id": "call-identical-a", "function": {
                        "name": "read", "arguments": r#"{"path":"README.md"}"#
                    }},
                    {"index": 1, "id": "call-identical-b", "function": {
                        "name": "read", "arguments": r#"{"path":"README.md"}"#
                    }}
                ]
            }
        }]
    });
    let finish = json!({
        "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
    });
    let sse = format!("data: {calls}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
    let app = run_openai_fixture_sse(&sse)
        .expect("parallel calls with identical content must retain distinct identities");

    let actual = app
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
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            ("call-identical-a", "read", r#"{"path":"README.md"}"#),
            ("call-identical-b", "read", r#"{"path":"README.md"}"#),
        ]
    );
}

#[test]
fn codex_completed_calls_keep_identity_when_names_and_arguments_repeat() {
    // Reduced from the live Luna capture: list, read, then an identical list
    // with a different call_id. Completion must never match by content alone.
    let mut events = Vec::new();
    let calls = [
        (1, "call-list-a", "list", r#"{"path":"","max_entries":200}"#),
        (
            2,
            "call-read",
            "read",
            r#"{"path":"SPEC.md","offset":1,"max_lines":4096}"#,
        ),
        (3, "call-list-b", "list", r#"{"path":"","max_entries":200}"#),
        (
            4,
            "call-list-done-only",
            "list",
            r#"{"path":"","max_entries":200}"#,
        ),
    ];
    for (index, id, name, arguments) in calls {
        if index != 4 {
            events.push(
                json!({"type":"response.output_item.added","output_index":index,
                "item":{"type":"function_call","call_id":id,"name":name,"arguments":""}}),
            );
            events.push(
                json!({"type":"response.function_call_arguments.delta","output_index":index,
                "delta":arguments}),
            );
        }
        events.push(
            json!({"type":"response.output_item.done","output_index":index,
            "item":{"type":"function_call","call_id":id,"name":name,"arguments":arguments}}),
        );
    }
    events.push(json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5}}}));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        let body = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
    });
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
        "fixture-account",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut app = AppHandle::fake();
    let result =
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(run_http_provider_messages(
                &client,
                &mut app,
                &[ProviderMessage::user("inspect")],
                1,
            ));
    server.join().expect("server");
    result.expect("distinct calls with identical contents are valid");
    let actual = app
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
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        calls
            .iter()
            .map(|(_, id, name, arguments)| (*id, *name, *arguments))
            .collect::<Vec<_>>()
    );
}

#[test]
fn free_provider_bridge_accepts_null_initial_tool_arguments() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":null}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut app = AppHandle::fake();
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(run_http_provider_messages(
            &client,
            &mut app,
            &[ProviderMessage::user("hello")],
            1,
        ))
        .expect("null initial arguments");
    server.join().expect("server");
    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ProviderToolCall { id, name, arguments }
            if id == "call-a" && name == "read" && arguments == r#"{"path":"README.md"}"#
    )));
}

#[test]
fn free_provider_bridge_discards_null_tool_placeholder_on_normal_stop() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-placeholder\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":null}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut app = AppHandle::fake();
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(run_http_provider_messages(
            &client,
            &mut app,
            &[ProviderMessage::user("hello")],
            1,
        ))
        .expect("normal text completion must ignore a non-executable placeholder");
    server.join().expect("server");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::AssistantTextDelta { text } if text == "answer"
    )));
    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::AssistantEnded { reason } if reason == "stop"
    )));
    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
    let expected_history_bytes = serde_json::to_vec(&json!({"role": "user", "content": "hello"}))
        .expect("wire message")
        .len() as u64;
    assert!(app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::ContextSnapshot { history_bytes, .. }
            if history_bytes == expected_history_bytes
    )));
}

#[test]
fn free_provider_bridge_discards_null_tool_array_entry_on_normal_stop() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[null]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("normal completion must ignore a null tool array entry");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::AssistantEnded { reason } if reason == "stop"
    )));
    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
}

#[test]
fn free_provider_bridge_discards_empty_tool_object_on_normal_stop() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("normal completion must ignore an empty tool object");

    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
}

#[test]
fn free_provider_bridge_discards_null_function_placeholder_on_normal_stop() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-placeholder\",\"function\":null}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("normal completion must ignore a null function placeholder");

    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
}

#[test]
fn free_provider_bridge_discards_non_array_tool_placeholder_on_normal_stop() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":{\"unexpected\":true}}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("normal completion must ignore a non-array tool placeholder");

    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
}

#[test]
fn free_provider_bridge_discards_invalid_tool_metadata_on_normal_stop() {
    let malformed_calls = [
        json!([{"index": 0, "id": "call-a", "type": "other", "function": {"name": "read", "arguments": "{}"}}]),
        json!([{"index": u64::MAX, "id": "call-a", "function": {"name": "read", "arguments": "{}"}}]),
        json!([{"index": 0, "id": "call-a", "function": {"name": 7, "arguments": "{}"}}]),
        json!([{"index": 0, "id": "call-a", "function": {"name": "read", "arguments": true}}]),
    ];

    for tool_calls in malformed_calls {
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"answer"}}]}),
            json!({"choices":[{"delta":{"tool_calls":tool_calls}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
        );
        let app = run_openai_fixture_sse(&sse)
            .expect("normal completion must ignore invalid tool metadata");

        assert!(!app
            .events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
    }
}

#[test]
fn free_provider_bridge_discards_conflicting_tool_fragments_on_normal_stop() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":null}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-b\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("normal completion must discard conflicting tool fragments");

    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
}

#[test]
fn free_provider_bridge_recovers_when_valid_tool_call_follows_placeholder() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":{\"unexpected\":true}}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("a later valid tool call must replace an empty placeholder");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ProviderToolCall { id, name, arguments }
            if id == "call-a" && name == "read" && arguments == r#"{"path":"README.md"}"#
    )));
}

#[test]
fn free_provider_bridge_keeps_valid_tool_when_a_later_slot_stays_empty() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"reasoning_content\":\"plan\",\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"name\":\"\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("an unused trailing tool slot must not reject the valid call");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ReasoningDelta { text } if text == "plan"
    )));
    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::AssistantTextDelta { .. })));
    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ProviderToolCall { id, name, arguments }
            if id == "call-a" && name == "read" && arguments == r#"{"path":"README.md"}"#
    )));
}

#[test]
fn free_provider_bridge_never_executes_tool_call_terminated_by_normal_stop() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\",\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"unsafe.txt\\\",\\\"content\\\":\\\"x\\\"}\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("normal stop must make even a complete-looking tool call inert");

    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::ProviderToolCall { .. })));
}

#[test]
fn free_provider_bridge_rejects_malformed_tool_when_execution_is_required() {
    let error = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":7,\"arguments\":true}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect_err("a malformed required tool call must fail closed");

    assert_eq!(error, ProviderError::MalformedToolCall);
}

#[test]
fn free_provider_bridge_rejects_malformed_sibling_when_execution_is_required() {
    let error = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}},{\"index\":1,\"type\":\"other\"}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect_err("a malformed sibling must block the entire tool batch");

    assert_eq!(error, ProviderError::MalformedToolCall);
}

#[test]
fn free_provider_bridge_keeps_valid_tool_when_a_null_sibling_is_present() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}},null]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("a null sibling is unused padding");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ProviderToolCall { id, name, arguments }
            if id == "call-a" && name == "read" && arguments == "{}"
    )));
}

#[test]
fn free_provider_bridge_keeps_valid_tool_when_an_empty_object_sibling_is_present() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}},{}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("an empty tool object is unused padding");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ProviderToolCall { id, name, arguments }
            if id == "call-a" && name == "read" && arguments == "{}"
    )));
}

#[test]
fn free_provider_bridge_null_type_continuation_attaches_to_the_identified_call() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"type\":null,\"function\":{\"name\":null,\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .expect("null type continues the identified call");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ProviderToolCall { id, name, arguments }
            if id == "call-a" && name == "read" && arguments == r#"{"path":"README.md"}"#
    )));
}

#[test]
fn free_provider_bridge_identical_repeated_stop_reason_is_idempotent() {
    let app = run_openai_fixture_sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
         data: [DONE]\n\n",
    )
    .expect("identical stop is idempotent");

    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::AssistantTextDelta { text } if text == "done"
    )));
    assert_eq!(
        app.events()
            .iter()
            .filter(|event| matches!(event.kind, EventKind::AssistantEnded { .. }))
            .count(),
        1
    );
}

#[test]
fn free_provider_bridge_redacts_request_credentials_split_across_tool_arguments() {
    let fragments = [
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-redaction","function":{"name":"read","arguments":"{\"path\":\"fixture-"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"key\"}"}}]},"finish_reason":"tool_calls"}]}),
    ];
    let sse = fragments
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let error = run_openai_fixture_sse(&sse).expect_err("credential-bearing tool call rejected");
    assert!(format!("{error:?}").contains("registered sensitive material"));
    assert!(!format!("{error:?}").contains("fixture-key"));
}

fn run_openai_fixture_sse(sse: &str) -> Result<AppHandle, ProviderError> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream.write_all(response.as_bytes()).expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut app = AppHandle::fake();
    let result =
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(run_http_provider_messages(
                &client,
                &mut app,
                &[ProviderMessage::user("hello")],
                1,
            ));
    server.join().expect("server");
    result.map(|_| app)
}

#[test]
fn multiline_sse_preserves_framing_and_eof_usage() {
    let payload = serde_json::to_string_pretty(&json!({
        "choices":[{"delta":{"content":"á\r\n ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":7,"completion_tokens":3}
    }))
    .unwrap();
    for newline in ["\n", "\r\n"] {
        for terminator in ["", newline] {
            let mut sse = format!(": comment{newline}event: message{newline}id: 42{newline}");
            for line in payload.lines() {
                sse.push_str(&format!("data: {line}{newline}"));
            }
            sse.push_str(terminator);
            let app = run_openai_fixture_sse(&sse).unwrap();
            let text = app
                .events()
                .iter()
                .filter_map(|event| match &event.kind {
                    slim_core::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(text, "á\r\n ok");
        }
    }
}

#[test]
fn multiline_sse_aggregate_is_bounded() {
    let line = format!("data: {}\n", " ".repeat(400_000));
    let error = run_openai_fixture_sse(&format!("{line}{line}{line}\n")).unwrap_err();
    assert!(format!("{error:?}").contains("SSE event exceeded byte limit"));
}

#[test]
fn free_provider_bridge_rejects_incomplete_fragmented_arguments() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("response");
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let mut app = AppHandle::fake();
    let error = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(run_http_provider_messages(
            &client,
            &mut app,
            &[ProviderMessage::user("hello")],
            1,
        ))
        .expect_err("incomplete arguments");
    server.join().expect("server");
    assert_eq!(error, ProviderError::MalformedToolCall);
    assert!(!app.events().iter().any(|event| matches!(
        event.kind,
        EventKind::ProviderToolCall { .. } | EventKind::AssistantEnded { .. }
    )));
}

#[test]
fn public_provider_paths_reject_sequence_exhaustion_before_network_or_event() {
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
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");

    let mut runtime = Runtime::new();
    let error = tokio_runtime
        .block_on(runtime.run_provider(&client, "hello", u64::MAX - 1))
        .expect_err("runtime sequence exhaustion");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert!(runtime.app.events().is_empty());

    let mut app = AppHandle::fake();
    let error = tokio_runtime
        .block_on(run_http_provider_messages(
            &client,
            &mut app,
            &[ProviderMessage::user("hello")],
            u64::MAX - 1,
        ))
        .expect_err("free provider sequence exhaustion");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert!(app.events().is_empty());
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
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
        Err(slim_core::ProviderError::Http { .. })
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
                        "index": 0,
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

    let events = events
        .into_iter()
        .filter(|event| !matches!(event, ProviderEvent::Phase { .. }))
        .collect::<Vec<_>>();
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
            index: Some(0),
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
    ResponsesWithoutDone,
    UsageThenFailure,
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
            json!({"type":"content_block_delta","index":0,"delta":{"text":"ok"}}),
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
                FixtureMode::UsageThenFailure => write_fixture_response(
                    &mut stream,
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\ndata: not-json\n\n",
                    200,
                ),
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
                FixtureMode::AnthropicWithoutDone | FixtureMode::ResponsesWithoutDone => {
                    let body = if matches!(mode, FixtureMode::AnthropicWithoutDone) {
                        format!("{}data: {{\"type\":\"message_stop\"}}\n\n", success_sse(true, false))
                    } else {
                        format!("data: {}\n\n", json!({"type":"response.completed","response":{"usage":{"input_tokens":7,"output_tokens":3}}}))
                    };
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{body}\r\n", body.len()).expect("native terminal");
                    stream.flush().expect("flush");
                    // No HTTP EOF: completion must drop the body on the native event.
                    stream.set_read_timeout(Some(Duration::from_secs(3))).expect("timeout");
                    assert_eq!(stream.read(&mut [0_u8; 1]).expect("client closes"), 0);
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
fn transient_cache_key_capacity_disables_capture_before_retention() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::Success);
    let cache = Arc::new(ProviderCache::new());
    let adapter = SpareCacheKeyAdapter {
        inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        key_capacity: 3 * 1024 * 1024,
    };
    let client =
        HttpProviderClient::with_cache(adapter, Duration::from_secs(2), Arc::clone(&cache))
            .expect("client");
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("live response");
    server.join().expect("server");
    assert!(cache.is_empty(), "transient key capacity is budgeted");
}

#[test]
fn live_capture_accounts_provider_event_vec_growth() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::Success);
    let cache = Arc::new(ProviderCache::new());
    let event_bytes = std::mem::size_of::<ProviderEvent>();
    let adapter = SpareCacheKeyAdapter {
        inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        key_capacity: 2 * 1024 * 1024 - 2 * event_bytes - 64,
    };
    let client =
        HttpProviderClient::with_cache(adapter, Duration::from_secs(2), Arc::clone(&cache))
            .expect("client");
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("live response");
    server.join().expect("server");
    assert!(
        cache.is_empty(),
        "post-push Vec allocation crosses the capture budget"
    );
}

#[test]
fn oversized_stream_stops_cache_capture_before_completion() {
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
        let payload = "x".repeat(700 * 1024);
        for _ in 0..4 {
            write!(
                stream,
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{payload}\"}}}}]}}\n\n"
            )
            .expect("bounded delta");
        }
        stream
            .write_all(
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("terminal");
    });
    let cache = Arc::new(ProviderCache::new());
    let client = HttpProviderClient::with_cache(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter"),
        Duration::from_secs(5),
        Arc::clone(&cache),
    )
    .expect("client");
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("hello"))
        .expect("stream remains deliverable");
    server.join().expect("server");
    assert!(cache.is_empty(), "oversized response is never retained");
}

#[test]
fn cache_hit_skips_network_and_changed_content_misses() {
    let (endpoint, server) = spawn_fixture_server(2, FixtureMode::Success);
    let cache = Arc::new(ProviderCache::new());
    let builds = Arc::new(AtomicUsize::new(0));
    let cache_keys = Arc::new(AtomicUsize::new(0));
    let client = HttpProviderClient::with_cache(
        CountingAdapter {
            inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
                endpoint,
                "fixture-model",
                "fixture-key",
            ))
            .expect("adapter"),
            builds: Arc::clone(&builds),
            cache_keys: Arc::clone(&cache_keys),
        },
        Duration::from_secs(2),
        Arc::clone(&cache),
    )
    .expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let first = runtime.block_on(client.send("hello")).expect("first");
    let replay = runtime.block_on(client.send("hello")).expect("replay");
    let changed = runtime.block_on(client.send("changed")).expect("changed");
    let requests = server.join().expect("server");

    let semantic = |events: Vec<ProviderEvent>| {
        events
            .into_iter()
            .filter(|event| !matches!(event, ProviderEvent::Phase { .. }))
            .collect::<Vec<_>>()
    };
    assert!(matches!(
        replay.first(),
        Some(ProviderEvent::ResponseCacheHit)
    ));
    let replay_semantic = replay
        .into_iter()
        .filter(|event| !matches!(event, ProviderEvent::ResponseCacheHit))
        .collect::<Vec<_>>();
    assert_eq!(semantic(first), replay_semantic);
    assert_eq!(semantic(changed), replay_semantic);
    assert_eq!(
        requests.len(),
        2,
        "the replay must not open a second TCP request"
    );
    assert!(requests[0].contains("hello"));
    assert!(requests[1].contains("changed"));
    assert_eq!(cache.len(), 2);
    assert_eq!(builds.load(Ordering::Relaxed), 3);
    assert_eq!(cache_keys.load(Ordering::Relaxed), 3);
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
fn responses_completion_closes_stream_and_preserves_terminal_usage() {
    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::ResponsesWithoutDone);
    let client = HttpProviderClient::new(
        OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
            endpoint,
            "fixture-model",
            "fixture-key",
            "fixture-account",
        ))
        .expect("adapter"),
        Duration::from_secs(2),
    )
    .expect("client");
    let started = Instant::now();
    let result = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("stop"));
    let elapsed = started.elapsed();
    server.join().expect("server");
    println!(
        "native completion elapsed_ms={} result={result:?}",
        elapsed.as_millis()
    );
    let events = result.expect("native terminal must not wait for HTTP EOF");
    assert!(events.contains(&ProviderEvent::Usage {
        input_tokens: 7,
        output_tokens: 3
    }));
    assert!(elapsed < Duration::from_secs(1));

    let (endpoint, server) = spawn_fixture_server(1, FixtureMode::ResponsesWithoutDone);
    let client = HttpProviderClient::new(
        slim_core::provider::XaiAdapter::new(&endpoint, "grok-4.5", "fixture-key", None)
            .expect("xAI"),
        Duration::from_secs(2),
    )
    .expect("client");
    let events = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(client.send("stop"))
        .expect("xAI native terminal");
    server.join().expect("server");
    assert!(events.contains(&ProviderEvent::Usage {
        input_tokens: 7,
        output_tokens: 3
    }));
}

#[test]
fn pre_cancelled_stream_does_not_start_transport() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "http://127.0.0.1:1",
        "fixture-model",
        "fixture-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(2)).expect("client");
    let result = tokio::runtime::Runtime::new().expect("runtime").block_on(
        client.stream_messages_with_tools_cancellable(
            &[ProviderMessage::user("cancelled")],
            &[],
            std::future::ready(()),
            |_| panic!("transport must not start after cancellation"),
        ),
    );
    assert_eq!(result, Err(ProviderError::Cancelled));
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
