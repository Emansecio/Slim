use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slim_cli::{
    run_provider_headless_with_session_and_options, ExitCode, ProviderRequest, ProviderRunOptions,
};
use slim_core::provider::ProviderKind;
use slim_core::session::{preflight_session, provider_messages_from_entries, DurableRecord};
use slim_core::OperatingMode;

const SERVER_DEADLINE: Duration = Duration::from_secs(5);
const STREAM_TIMEOUT: Duration = Duration::from_secs(3);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "slim-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&path).expect("temp directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(self) {
        let path = self.path.clone();
        drop(self);
        assert!(!path.exists(), "temporary directory was not removed");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("request headers");
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 128 * 1024, "request headers exceeded bound");
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk).expect("request body");
        assert!(read > 0, "request ended before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(bytes[..header_end + content_length].to_vec()).expect("utf8 request")
}

fn request_body(request: &str) -> Value {
    let (_, body) = request.split_once("\r\n\r\n").expect("request body");
    serde_json::from_str(body).expect("json request body")
}

fn accept_with_deadline(listener: &TcpListener, deadline: Instant) -> TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                stream
                    .set_read_timeout(Some(STREAM_TIMEOUT))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(STREAM_TIMEOUT))
                    .expect("write timeout");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "fixture accept timed out");
                thread::yield_now();
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}

fn write_sse(stream: &mut TcpStream, events: &[Value]) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("headers");
    for event in events {
        stream
            .write_all(format!("data: {event}\n\n").as_bytes())
            .expect("event");
    }
    stream.write_all(b"data: [DONE]\n\n").expect("done");
}

fn spawn_openai_fixture(
    listener: TcpListener,
    source_arg: String,
    prompt: &str,
    model: &str,
    api_key: &str,
    expected_tool_output: &str,
    final_text: &str,
) -> thread::JoinHandle<()> {
    let prompt = prompt.to_owned();
    let model = model.to_owned();
    let api_key = api_key.to_owned();
    let expected_tool_output = expected_tool_output.to_owned();
    let final_text = final_text.to_owned();
    thread::spawn(move || {
        let deadline = Instant::now() + SERVER_DEADLINE;
        for turn in 0..2 {
            let mut stream = accept_with_deadline(&listener, deadline);
            let request = read_request(&mut stream);
            assert!(request.contains(&format!("Bearer {api_key}")));
            let body = request_body(&request);
            assert_eq!(body["model"], model);
            let messages = body["messages"].as_array().expect("messages");
            if turn == 0 {
                // The native Slim system prompt is now message 0; the user
                // prompt follows as message 1.
                assert_eq!(messages[0]["role"], "system");
                assert_eq!(messages[1]["role"], "user");
                let content = messages[1]["content"].as_str().expect("user text");
                let original = content
                    .split_once("\n\nWorkspace paths observed before this turn")
                    .map_or(content, |(original, _)| original);
                assert_eq!(
                    original, prompt,
                    "optional path context preserves the user request"
                );
                write_sse(
                    &mut stream,
                    &[
                        json!({
                            "choices": [{
                                "delta": {
                                    "tool_calls": [{
                                        "index": 0,
                                        "id": "offline-read-call",
                                        "function": {
                                            "name": "read",
                                            "arguments": json!({
                                                "path": source_arg,
                                                "max_lines": 10
                                            }).to_string()
                                        }
                                    }]
                                }
                            }]
                        }),
                        json!({"usage": {"prompt_tokens": 11, "completion_tokens": 5}}),
                        json!({
                            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
                        }),
                    ],
                );
            } else {
                let assistant = messages
                    .iter()
                    .find(|message| message["role"] == "assistant")
                    .expect("structured assistant message");
                assert!(assistant["tool_calls"].is_array());
                let tool = messages
                    .iter()
                    .find(|message| message["role"] == "tool")
                    .expect("structured tool message");
                assert!(tool["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(&expected_tool_output)));
                write_sse(
                    &mut stream,
                    &[
                        json!({"choices": [{"delta": {"content": final_text}}]}),
                        json!({"usage": {"prompt_tokens": 13, "completion_tokens": 7}}),
                        json!({
                            "choices": [{"delta": {}, "finish_reason": "stop"}]
                        }),
                    ],
                );
            }
        }
    })
}

#[test]
fn anthropic_offline_e2e_runs_tool_turn_usage_and_session_without_secret_persistence() {
    let temp = TempDir::new("anthropic-e2e");
    let source = temp.path().join("fixture.txt");
    std::fs::write(&source, "offline fixture\n").expect("fixture");
    let session = temp.path().join("session.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let source_arg = "fixture.txt".to_owned();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + SERVER_DEADLINE;
        for turn in 0..2 {
            let mut stream = accept_with_deadline(&listener, deadline);
            let request = read_request(&mut stream);
            assert!(request.contains("x-api-key: fixture-anthropic-secret"));
            assert!(request.contains("anthropic-version: 2023-06-01"));
            assert!(request.contains("claude-fixture"));
            if turn == 0 {
                assert!(request.contains("\"role\":\"user\""));
                assert!(request.contains("inspect fixture"));
                write_sse(
                    &mut stream,
                    &[
                        json!({
                            "type": "message_start",
                            "message": {"usage": {"input_tokens": 7, "output_tokens": 0}}
                        }),
                        json!({
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {
                                "type": "tool_use",
                                "id": "remote-call-1",
                                "name": "read",
                                "input": {"path": source_arg, "max_lines": 10}
                            }
                        }),
                        json!({"type": "content_block_stop", "index": 0}),
                        json!({
                            "type": "message_delta",
                            "usage": {"output_tokens": 3}
                        }),
                        json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": "tool_use"}
                        }),
                    ],
                );
            } else {
                assert!(request.contains("\"role\":\"assistant\""));
                assert!(request.contains("tool_use"));
                assert!(request.contains("tool_result"));
                assert!(request.contains("offline fixture"));
                write_sse(
                    &mut stream,
                    &[
                        json!({
                            "type": "message_start",
                            "message": {"usage": {"input_tokens": 9, "output_tokens": 0}}
                        }),
                        json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": "final from anthropic"}
                        }),
                        json!({
                            "type": "message_delta",
                            "usage": {"output_tokens": 4}
                        }),
                        json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": "end_turn"}
                        }),
                    ],
                );
            }
        }
    });

    let provider_result = run_provider_headless_with_session_and_options(
        ProviderRequest {
            prompt: "inspect fixture".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::Anthropic,
            endpoint: format!("http://{address}"),
            model: "claude-fixture".into(),
            api_key: "fixture-anthropic-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        &session,
        ProviderRunOptions::default()
            .with_workspace_root(temp.path())
            .with_artifact_root(temp.path().join("artifacts")),
    );
    let server_result = server.join();
    let result = provider_result.expect("offline provider");
    server_result.expect("server");

    assert_eq!(result.code, ExitCode::Success);
    assert_eq!(result.provider, ProviderKind::Anthropic);
    assert_eq!(result.text, "final from anthropic");
    assert_eq!(result.input_tokens, Some(16));
    assert_eq!(result.output_tokens, Some(7));
    assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    let output = slim_cli::render_provider_text(&result);
    assert!(!output.contains("fixture-anthropic-secret"));

    let recovered = preflight_session(&session).expect("session");
    let messages =
        provider_messages_from_entries(recovered.records.iter().filter_map(
            |record| match record {
                DurableRecord::Entry { entry, .. } => Some(entry),
                _ => None,
            },
        ))
        .expect("complete durable transcript");
    assert!(messages
        .iter()
        .any(|m| m.role == "tool" && m.content.contains("offline fixture")));
    assert_eq!(messages.last().unwrap().content, "final from anthropic");
    assert_eq!(result.usage.requests.len(), 2);
    assert_eq!(result.usage.requests[0].uncached_input_tokens, 7);
    assert_eq!(result.usage.requests[1].uncached_input_tokens, 9);
    assert_eq!(result.usage.requests[1].output_tokens, 4);
    assert!(recovered.records.iter().any(|record| matches!(record,
        DurableRecord::Usage { usage, .. } if usage.input_tokens == Some(16) && usage.output_tokens == Some(7))));
    let raw_session = std::fs::read_to_string(&session).expect("session bytes");
    assert!(!raw_session.contains("fixture-anthropic-secret"));
    temp.cleanup();
}

#[test]
fn openai_compatible_offline_e2e_runs_structured_tool_turn_usage_and_session() {
    let temp = TempDir::new("openai-e2e");
    let source = temp.path().join("fixture.txt");
    std::fs::write(&source, "offline openai fixture\n").expect("fixture");
    let session = temp.path().join("session.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = spawn_openai_fixture(
        listener,
        "fixture.txt".to_owned(),
        "inspect openai fixture",
        "openai-fixture",
        "fixture-openai-secret",
        "offline openai fixture",
        "final from openai",
    );

    let provider_result = run_provider_headless_with_session_and_options(
        ProviderRequest {
            prompt: "inspect openai fixture".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "openai-fixture".into(),
            api_key: "fixture-openai-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        &session,
        ProviderRunOptions::default()
            .with_workspace_root(temp.path())
            .with_artifact_root(temp.path().join("artifacts")),
    );
    let server_result = server.join();
    let result = provider_result.expect("offline provider");
    server_result.expect("server");

    assert_eq!(result.code, ExitCode::Success);
    assert_eq!(result.provider, ProviderKind::OpenAiCompatible);
    assert_eq!(result.text, "final from openai");
    assert_eq!(result.input_tokens, Some(24));
    assert_eq!(result.output_tokens, Some(12));
    assert_eq!(result.stop_reason.as_deref(), Some("stop"));
    let output = slim_cli::render_provider_text(&result);
    assert!(!output.contains("fixture-openai-secret"));

    let recovered = preflight_session(&session).expect("session");
    let messages =
        provider_messages_from_entries(recovered.records.iter().filter_map(
            |record| match record {
                DurableRecord::Entry { entry, .. } => Some(entry),
                _ => None,
            },
        ))
        .expect("complete durable transcript");
    assert!(messages
        .iter()
        .any(|m| m.role == "tool" && m.content.contains("offline openai fixture")));
    assert_eq!(messages.last().unwrap().content, "final from openai");
    assert_eq!(result.usage.requests.len(), 2);
    assert_eq!(
        (
            result.usage.requests[0].uncached_input_tokens,
            result.usage.requests[0].output_tokens
        ),
        (11, 5)
    );
    assert_eq!(
        (
            result.usage.requests[1].uncached_input_tokens,
            result.usage.requests[1].output_tokens
        ),
        (13, 7)
    );
    assert!(recovered.records.iter().any(|record| matches!(record,
        DurableRecord::Usage { usage, .. } if usage.input_tokens == Some(24) && usage.output_tokens == Some(12))));
    let raw_session = std::fs::read_to_string(&session).expect("session bytes");
    assert!(!raw_session.contains("fixture-openai-secret"));
    temp.cleanup();
}

#[test]
fn real_binary_offline_e2e_uses_temp_cwd_and_never_prints_secret() {
    let temp = TempDir::new("binary-e2e");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + SERVER_DEADLINE;
        let mut stream = accept_with_deadline(&listener, deadline);
        let request = read_request(&mut stream);
        assert!(request.contains("Bearer binary-fixture-secret"));
        write_sse(
            &mut stream,
            &[
                json!({"choices": [{"delta": {"content": "binary fixture ok"}}]}),
                json!({"usage": {"prompt_tokens": 2, "completion_tokens": 1}}),
                json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
            ],
        );
    });
    let endpoint = format!("http://{address}");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_slim"))
        .current_dir(temp.path())
        .args([
            "--headless",
            "--provider",
            "openai-compatible",
            "--endpoint",
            &endpoint,
            "--model",
            "binary-fixture",
            "--prompt",
            "hello binary",
        ])
        .env("SLIM_API_KEY", "binary-fixture-secret")
        .env_remove("SLIM_AUTH_FILE")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("slim binary");
    server.join().expect("server");
    assert!(output.status.success(), "status={:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("binary fixture ok"));
    assert!(!stdout.contains("stop=provider_completed"));
    assert!(!stdout.contains("binary-fixture-secret"));
    assert!(!stderr.contains("binary-fixture-secret"));
    assert!(!temp
        .path()
        .join(".slim")
        .join("artifacts")
        .join("binary-fixture-secret")
        .exists());
    temp.cleanup();
}

#[cfg(windows)]
#[test]
fn cli_offline_auth_json_uses_local_provider_without_secret_output_or_session_persistence() {
    let temp = TempDir::new("cli-auth-e2e");
    let source = temp.path().join("fixture.txt");
    std::fs::write(&source, "offline auth fixture\n").expect("fixture");
    let auth = temp.path().join("auth.json");
    std::fs::write(
        &auth,
        r#"{"version":1,"providers":{"openai-compatible":{"api_key":"fixture-auth-secret"}}}"#,
    )
    .expect("auth fixture");
    let session = temp.path().join("session.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = spawn_openai_fixture(
        listener,
        "fixture.txt".to_owned(),
        "inspect auth fixture",
        "auth-fixture",
        "fixture-auth-secret",
        "offline auth fixture",
        "final from auth",
    );

    let endpoint = format!("http://{address}");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_slim"))
        .current_dir(temp.path())
        .args([
            "--headless",
            "--provider",
            "openai-compatible",
            "--endpoint",
            &endpoint,
            "--model",
            "auth-fixture",
            "--session",
            session.to_str().expect("session path"),
            "--prompt",
            "inspect auth fixture",
        ])
        .env("SLIM_AUTH_FILE", &auth)
        .env_remove("SLIM_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("SLIM_PROVIDER")
        .env_remove("SLIM_ENDPOINT")
        .env_remove("SLIM_MODEL")
        .output()
        .expect("slim binary");
    server.join().expect("server");

    assert!(output.status.success(), "status={:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stdout, "final from auth\n");
    assert!(stderr.is_empty());
    assert!(!stdout.contains("fixture-auth-secret"));
    assert!(!stderr.contains("fixture-auth-secret"));
    let raw_session = std::fs::read_to_string(&session).expect("session bytes");
    assert!(!raw_session.contains("fixture-auth-secret"));
    temp.cleanup();
}
