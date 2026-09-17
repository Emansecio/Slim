//! Security reproductions (round 2 audit — role Shroud): the provider-side
//! gate that rejects tool calls carrying registered secrets runs on the raw
//! buffered argument text, but `normalize_tool_arguments` can unwrap one
//! JSON-string layer at publish time. A double-encoded payload therefore
//! delivers the decoded secret to tool execution.
//!
//! Evidence (runtime/mod.rs): the gate at ~7688 calls
//! `sensitive_tool_arguments(&call.arguments, ...)` on the pre-normalized
//! string; `sensitive_tool_arguments` (8291-8311) decodes a single JSON level;
//! `normalize_tool_arguments` (7093-7103) unwraps a second level;
//! `publish_buffered_call` (8484) publishes the normalized arguments.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slim_cli::{
    run_provider_headless_with_session_and_options, ExitCode, ProviderRequest, ProviderRunOptions,
};
use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;

const SERVER_DEADLINE: Duration = Duration::from_secs(8);
const STREAM_TIMEOUT: Duration = Duration::from_secs(3);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "slim-sec-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("temp directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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

fn accept_one(listener: &TcpListener, deadline: Instant) -> Option<TcpStream> {
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
                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
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

/// Reproduction for finding S-1: a `tool_calls` delta whose `arguments` field
/// is a JSON string that itself decodes to a JSON object carrying the
/// registered secret behind a `\uXXXX` escape defeats both sensitive-material
/// gates. They inspect the raw buffered text plus one JSON decode; the
/// publish path then calls `normalize_tool_arguments`, which unwraps the
/// inner object, so the tool runs with the real secret.
///
/// Proof of execution: a file named after the real secret exists in the
/// workspace. If the gate is bypassed the read tool opens that file and its
/// canary content reaches the provider on the next turn.
///
/// Secure contract: the run must abort with the "tool call contains
/// registered sensitive material" error before any tool executes — the mock
/// then observes exactly one request and no `role: "tool"` message.
///
/// Today's behavior (bug): the read tool executes `path = <real secret>`,
/// the canary content is returned to the provider, and the run returns
/// `ExitCode::Success`.
#[test]
fn sec_tool_call_double_encoded_secret_is_rejected_before_execution() {
    let temp = TempDir::new("double-encode");
    let session = temp.path().join("session.jsonl");
    let secret = "sk-fake-Ab7cd";
    let canary = "sec-canary-execution-proof";
    // A file named by the secret itself: only a decoded `path` argument can
    // open it.
    std::fs::write(temp.path().join(secret), canary).expect("canary file");
    // Level 2 (inner) JSON object whose `path` value decodes to the secret:
    // `\u0041` is a literal backslash-u escape inside `inner`.
    let inner = r#"{"path":"sk-fake-\u0041b7cd"}"#;
    // Level 1 (outer) arguments string: a JSON string literal whose decoded
    // content is `inner`. This is what reaches `call.arguments`.
    let wire_arguments = serde_json::to_string(inner).expect("encode arguments");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");

    let tool_message_body = Arc::new(std::sync::Mutex::new(String::new()));
    let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server = {
        let tool_message_body = tool_message_body.clone();
        let request_count = request_count.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + SERVER_DEADLINE;
            for turn in 0..2 {
                let Some(mut stream) = accept_one(&listener, deadline) else {
                    break;
                };
                let request = read_request(&mut stream);
                request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if turn == 0 {
                    write_sse(
                        &mut stream,
                        &[
                            json!({
                                "choices": [{
                                    "delta": {
                                        "tool_calls": [{
                                            "index": 0,
                                            "id": "sec-double-encoded",
                                            "function": {
                                                "name": "read",
                                                "arguments": wire_arguments
                                            }
                                        }]
                                    }
                                }]
                            }),
                            json!({"usage": {"prompt_tokens": 5, "completion_tokens": 3}}),
                            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                        ],
                    );
                } else {
                    *tool_message_body.lock().expect("request slot") = request;
                    write_sse(
                        &mut stream,
                        &[
                            json!({"choices": [{"delta": {"content": "turn two"}}]}),
                            json!({"usage": {"prompt_tokens": 6, "completion_tokens": 2}}),
                            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                        ],
                    );
                }
            }
        })
    };

    let provider_result = run_provider_headless_with_session_and_options(
        ProviderRequest {
            prompt: "read the fixture".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "sec-fixture".into(),
            api_key: secret.into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        &session,
        ProviderRunOptions::default()
            .with_workspace_root(temp.path())
            .with_artifact_root(temp.path().join("artifacts")),
    );
    server.join().expect("server thread");

    let requests = request_count.load(std::sync::atomic::Ordering::SeqCst);
    let second = tool_message_body.lock().expect("request slot").clone();
    match provider_result {
        Err(error) => {
            // Secure path: the gate rejected the call.
            let rendered = format!("{error:?}");
            assert!(
                rendered.contains("sensitive material"),
                "rejection should name the guarantee, got: {rendered}"
            );
            assert_eq!(requests, 1, "no second turn may run after rejection");
        }
        Ok(result) => {
            // Bug path: the call was normalized and executed. The canary
            // content proves the tool opened the file named by the decoded
            // secret.
            panic!(
                "sensitive-material gate bypassed: tool executed with double-encoded \
                 secret (code={:?}, requests={}, canary_reached_provider={}, \
                 redacted_marker_in_tool_message={})",
                result.code,
                requests,
                second.contains(canary),
                second.contains("[REDACTED]")
            );
        }
    }
}

/// Regression guard for audited hypothesis S-3 (REJECTED): oversized tool
/// output is externalized to `.slim/artifacts/` via `materialize_results`
/// (runtime/mod.rs ~5585), which stores `result.output` verbatim — but
/// `result.output` is already redacted at dispatch
/// (runtime/mod.rs ~3905/4170/5248/5339). This test proves the artifact file
/// is written (non-vacuous) and carries only the redacted copy.
///
/// The secret doubles as the API key, so it legitimately appears in the
/// `Authorization` header of every request; the assertions therefore inspect
/// only the request BODY, where the tool result travels.
#[test]
fn sec_tool_output_artifact_must_not_persist_secret() {
    let temp = TempDir::new("artifact-raw");
    let session = temp.path().join("session.jsonl");
    let secret = "sk-fake-ART1FACT9";
    let mut content = String::from("leaked config: ");
    content.push_str(secret);
    content.push('\n');
    while content.len() < 2 * 1024 {
        content.push_str("filler line\n");
    }
    std::fs::write(temp.path().join("dump.txt"), &content).expect("fixture file");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let second_request = Arc::new(std::sync::Mutex::new(String::new()));
    let server = {
        let second_request = second_request.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + SERVER_DEADLINE;
            for turn in 0..2 {
                let Some(mut stream) = accept_one(&listener, deadline) else {
                    break;
                };
                let request = read_request(&mut stream);
                if turn == 0 {
                    write_sse(
                        &mut stream,
                        &[
                            json!({
                                "choices": [{
                                    "delta": {
                                        "tool_calls": [{
                                            "index": 0,
                                            "id": "sec-artifact",
                                            "function": {
                                                "name": "read",
                                                "arguments": "{\"path\":\"dump.txt\"}"
                                            }
                                        }]
                                    }
                                }]
                            }),
                            json!({"usage": {"prompt_tokens": 5, "completion_tokens": 3}}),
                            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                        ],
                    );
                } else {
                    *second_request.lock().expect("request slot") = request;
                    write_sse(
                        &mut stream,
                        &[
                            json!({"choices": [{"delta": {"content": "done"}}]}),
                            json!({"usage": {"prompt_tokens": 6, "completion_tokens": 2}}),
                            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                        ],
                    );
                }
            }
        })
    };

    let provider_result = run_provider_headless_with_session_and_options(
        ProviderRequest {
            prompt: "inspect dump".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "sec-fixture".into(),
            api_key: secret.into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        &session,
        ProviderRunOptions::default()
            .with_workspace_root(temp.path())
            .with_artifact_root(temp.path().join("artifacts"))
            .with_max_result_bytes(16),
    );
    server.join().expect("server thread");
    let result = provider_result.expect("provider run");
    assert_eq!(result.code, ExitCode::Success);

    // Provider boundary: the second request BODY can only carry a pointer or
    // masked content, never the secret itself. (The Authorization header
    // legitimately contains it; headers are excluded from the check.)
    let second = second_request.lock().expect("request slot").clone();
    assert!(
        !second.is_empty(),
        "tool turn must complete to isolate the artifact surface"
    );
    let second_body = second.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    assert!(
        !second_body.contains(secret),
        "provider-facing tool message must not carry the secret"
    );

    // The durable artifact copy must obey the same contract. Require at
    // least one externalized artifact so a missing surface cannot fake a
    // pass.
    let artifact_root = temp.path().join("artifacts");
    let mut artifacts = Vec::new();
    let mut offending = Vec::new();
    if artifact_root.exists() {
        for entry in std::fs::read_dir(&artifact_root).expect("artifact dir") {
            let entry = entry.expect("entry");
            let bytes = std::fs::read(entry.path()).expect("artifact bytes");
            artifacts.push(entry.file_name());
            if String::from_utf8_lossy(&bytes).contains(secret) {
                offending.push(entry.file_name());
            }
        }
    }
    assert!(
        !artifacts.is_empty(),
        "2 KiB tool output must be externalized under max_result_bytes=16"
    );
    assert!(
        offending.is_empty(),
        "raw secret persisted in artifacts: {offending:?} (all: {artifacts:?})"
    );
}
