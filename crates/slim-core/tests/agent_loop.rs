use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slim_core::provider::{
    AnthropicAdapter, HttpProviderClient, OpenAiCodexAdapter, OpenAiCompatibleAdapter,
    ProviderConfig,
};
use slim_core::runtime::{AgentLoopConfig, AgentLoopStop};
use slim_core::{EventKind, OperatingMode, Runtime};

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
                max_tool_calls: 8,
                max_result_bytes: 4096,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop");
    server.join().expect("server");

    assert_eq!(result.stop, AgentLoopStop::RepeatedFailedTool);
    assert_eq!(result.turns, 2);
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(result.usage.input_tokens, 10);
    assert_eq!(result.usage.output_tokens, 4);
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::TerminalError { .. })));
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
                max_tool_calls: 1,
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
    let path = root.join("fixture.txt").to_string_lossy().to_string();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let mut stream = accept_with_deadline(&listener);
            let mut request = [0_u8; 32 * 1024];
            let size = stream.read(&mut request).expect("request");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.contains("chatgpt-account-id: account-1"));
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
    assert_eq!(result.usage.input_tokens, 5);
    assert_eq!(result.usage.output_tokens, 3);
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
    let source_arg = source.to_string_lossy().to_string();
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
                max_tool_calls: 4,
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
    assert!(!body.contains("Summarize the prior agent transcript"));
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::CompactionCompleted)));
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
                context_window_tokens: 64,
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
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"summary from fixture\"}}]}\n\n",
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
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut runtime = Runtime::new();
    let result = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "start",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: 200,
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
    // The compaction summary request carries the transcript as its user
    // content; with the native system prompt the transcript sits at index 1.
    assert!(bodies[1]["messages"][1]["content"]
        .as_str()
        .expect("summary prompt")
        .contains("Summarize the prior agent transcript"));
    assert!(bodies[2]["messages"][1]["content"]
        .as_str()
        .expect("compacted context")
        .contains("[Compacted context]"));
    // With the native system prompt the compacted request is: system,
    // compacted user context, assistant tool call, tool result.
    assert_eq!(bodies[2]["messages"].as_array().expect("messages").len(), 4);
    assert_eq!(
        bodies[2]["messages"][2]["tool_calls"][0]["id"],
        "slim-call-0-0"
    );
    assert_eq!(bodies[2]["messages"][3]["role"], "tool");
    assert_eq!(bodies[2]["messages"][3]["name"], "read");
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
    assert!(runtime.app.events()[..compaction_position]
        .iter()
        .any(|event| matches!(event.kind, EventKind::Usage { .. })));
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
}

#[test]
fn failed_summary_does_not_record_a_compaction_event() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for index in 0..1 {
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
    let mut runtime = Runtime::new();
    let error = tokio_runtime
        .block_on(runtime.run_agent_loop(
            &client,
            "start",
            OperatingMode::Auto,
            std::env::temp_dir(),
            1,
            AgentLoopConfig {
                context_window_tokens: 32,
                context_reserve_tokens: 0,
                ..AgentLoopConfig::default()
            },
        ))
        .expect_err("empty summary must fail");
    server.join().expect("server");

    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { .. }
    ));
    assert!(!runtime.app.events().is_empty());
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

    let second_read = bodies[2]["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("deduped tool message");
    assert_eq!(second_read["role"], "tool");
    assert!(second_read["content"]
        .as_str()
        .expect("content")
        .contains("[duplicate read result omitted"));

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        EventKind::ContextSnapshot { tools_bytes, .. } if *tools_bytes > 0
    )));
    let _ = std::fs::remove_dir_all(root);
}

