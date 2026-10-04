//! Adversarial coverage for `mcp/http.rs` driven through the public
//! `McpManager` (RODADA 2 — Sifter). A raw TCP fixture serves controlled HTTP
//! responses — JSON bodies, SSE streams, and deliberately malformed wire
//! shapes — while `McpManager::test` performs the real initialize handshake
//! and `tools/list` pagination.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use slim_core::mcp::{McpManager, McpServerSpec, McpTransport};
use slim_core::process::ExecutableResolver;

/// What the fixture writes back for one request.
enum Reply {
    /// `200 OK` + `application/json` body.
    Json(String),
    /// `200 OK` + `text/event-stream` body, written verbatim.
    Sse(String),
    /// `202 Accepted` with an empty body (MCP notification response).
    Accepted,
    /// Raw response bytes verbatim — the socket is closed right after.
    Raw(&'static [u8]),
}

struct Fixture {
    url: String,
    requests: mpsc::Receiver<Value>,
}

fn read_http_request(stream: &mut TcpStream) -> Option<Value> {
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 64 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&headers);
    if text.starts_with("GET ") {
        // The client's server-to-client stream: this fixture offers none.
        stream
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n")
            .ok()?;
        return read_http_request(stream);
    }
    let length = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })?;
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Serves `respond` for every request on every accepted connection. Requests
/// are mirrored to the returned channel in arrival order.
fn spawn_fixture(respond: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let (tx, rx) = mpsc::channel::<Value>();
    let respond = Arc::new(respond);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let tx = tx.clone();
            let respond = Arc::clone(&respond);
            thread::spawn(move || {
                stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                while let Some(request) = read_http_request(&mut stream) {
                    let _ = tx.send(request.clone());
                    match respond(&request) {
                        Reply::Json(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                                body.len()
                            );
                            if stream
                                .write_all(head.as_bytes())
                                .and_then(|_| stream.write_all(body.as_bytes()))
                                .is_err()
                            {
                                return;
                            }
                        }
                        Reply::Sse(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                                body.len()
                            );
                            if stream
                                .write_all(head.as_bytes())
                                .and_then(|_| stream.write_all(body.as_bytes()))
                                .is_err()
                            {
                                return;
                            }
                        }
                        Reply::Accepted => {
                            if stream
                                .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n")
                                .is_err()
                            {
                                return;
                            }
                        }
                        Reply::Raw(bytes) => {
                            let _ = stream.write_all(bytes);
                            return;
                        }
                    }
                }
            });
        }
    });
    Fixture { url, requests: rx }
}

fn manager(url: &str) -> McpManager {
    let spec = McpServerSpec {
        name: "adv".into(),
        transport: McpTransport::Http {
            url: url.into(),
            headers: BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_secs(5),
        options: Default::default(),
    };
    McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    )
}

fn result_reply(request: &Value, result: Value) -> Reply {
    Reply::Json(
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": result,
        })
        .to_string(),
    )
}

fn healthy(request: &Value) -> Reply {
    match request.get("method").and_then(Value::as_str) {
        Some("initialize") => result_reply(
            request,
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "adv", "version": "0"},
            }),
        ),
        Some("tools/list") => result_reply(request, json!({"tools": []})),
        _ => Reply::Accepted,
    }
}

#[tokio::test]
async fn healthy_json_responses_complete_initialize_and_tools_list() {
    let fixture = spawn_fixture(healthy);
    let manager = manager(&fixture.url);
    assert_eq!(manager.test("adv").await.expect("connect"), 0);
    let methods: Vec<String> = (0..3)
        .filter_map(|_| fixture.requests.recv_timeout(Duration::from_secs(5)).ok())
        .filter_map(|request| {
            request
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    assert!(methods.contains(&"initialize".to_owned()));
    assert!(methods.contains(&"tools/list".to_owned()));
}

#[tokio::test]
async fn rejected_initialized_notification_stops_both_connection_paths() {
    for cancellable in [false, true] {
        let fixture = spawn_fixture(|request| {
            if request["method"] == "notifications/initialized" {
                // 501 is the one 5xx that connect retries never repeat, so the
                // rejection stops the connect after a single attempt.
                Reply::Raw(b"HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                healthy(request)
            }
        });
        let manager = manager(&fixture.url);
        let result = if cancellable {
            manager
                .list_tools_cancellable("adv", Default::default())
                .await
                .into_result()
        } else {
            manager.list_tools("adv").await
        };
        assert!(
            matches!(
                result,
                Err(slim_core::mcp::McpError::Server { code: 501, .. })
            ),
            "{result:?}"
        );
        let methods: Vec<_> = fixture
            .requests
            .try_iter()
            .map(|request| request["method"].clone())
            .collect();
        assert_eq!(
            methods,
            vec![json!("initialize"), json!("notifications/initialized")]
        );
    }
}

#[tokio::test]
async fn invalid_utf8_in_sse_is_rejected_without_replacing_data() {
    let fixture = spawn_fixture(|request| {
        if request["method"] == "initialize" {
            Reply::Raw(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"text\":\"\xff\"}}\n\n")
        } else {
            healthy(request)
        }
    });
    let error = manager(&fixture.url)
        .test("adv")
        .await
        .expect_err("invalid SSE bytes must fail");
    assert!(matches!(error, slim_core::mcp::McpError::Protocol(_)));
    assert!(error.to_string().contains("UTF-8"), "{error}");
}

#[tokio::test]
async fn large_sse_lines_preserve_unicode_across_http_chunks() {
    let text = "ação🐾".repeat(16 * 1024);
    let expected = text.clone();
    let fixture = spawn_fixture(move |request| {
        if request["method"] == "tools/call" {
            let response = json!({"jsonrpc":"2.0", "id":request["id"],
                "result":{"structuredContent":{"text":text}, "content":[]}});
            Reply::Sse(format!(
                "{}data: {response}\n\n",
                ": keepalive\r\n".repeat(4096)
            ))
        } else {
            healthy(request)
        }
    });
    let result = manager(&fixture.url)
        .call("adv", "unicode", json!({}))
        .await
        .unwrap();
    assert_eq!(result["structuredContent"]["text"], expected);
}

/// BUG: an initialize response carrying `"error": null` plus a valid result
/// is rejected with `server error 0: unknown server error`. `extract_result`
/// (http.rs:282) checks `message.get("error").is_some()` without excluding the
/// null sentinel — many JSON-RPC stacks serialize `Option<Error>` verbatim and
/// emit `"error": null` on every success. Expected: connect succeeds.
/// Actual today: `Err(McpError::Server { code: 0, .. })`.
#[tokio::test]
async fn initialize_with_null_error_member_completes_connect() {
    let fixture = spawn_fixture(|request| {
        match request.get("method").and_then(Value::as_str) {
        Some("initialize") => Reply::Json(
            json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "result": {"protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {"name": "adv", "version": "0"}},
                "error": null,
            })
            .to_string(),
        ),
        Some("tools/list") => result_reply(request, json!({"tools": []})),
        _ => Reply::Accepted,
    }
    });
    let manager = manager(&fixture.url);
    manager
        .test("adv")
        .await
        .expect("null error member must not fail the handshake");
}

/// Control for the bug above: `"result": null` *is* accepted — the asymmetry
/// shows the null-error rejection is an oversight, not a strictness policy.
#[tokio::test]
async fn initialize_with_null_result_member_is_accepted() {
    let fixture = spawn_fixture(
        |request| match request.get("method").and_then(Value::as_str) {
            Some("initialize") => result_reply(request, Value::Null),
            Some("tools/list") => result_reply(request, json!({"tools": []})),
            _ => Reply::Accepted,
        },
    );
    let manager = manager(&fixture.url);
    assert_eq!(manager.test("adv").await.expect("connect"), 0);
}

#[tokio::test]
async fn response_with_wrong_id_is_rejected() {
    let fixture = spawn_fixture(|request| {
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        Reply::Json(json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string())
    });
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("mismatched id");
    assert!(
        error.to_string().contains("id mismatch"),
        "expected id mismatch, got: {error}"
    );
}

/// Documents strict id typing: an id echoed back as a JSON string does not
/// satisfy the numeric pending id and is rejected as a mismatch.
#[tokio::test]
async fn response_with_string_id_is_rejected() {
    let fixture = spawn_fixture(|request| {
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .to_string();
        Reply::Json(json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string())
    });
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("string id");
    assert!(
        error.to_string().contains("id mismatch"),
        "expected id mismatch, got: {error}"
    );
}

#[tokio::test]
async fn response_without_result_or_error_is_rejected() {
    let fixture = spawn_fixture(|request| {
        Reply::Json(
            json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
            })
            .to_string(),
        )
    });
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("empty response");
    assert!(
        error.to_string().contains("neither result nor error"),
        "expected protocol error, got: {error}"
    );
}

#[tokio::test]
async fn oversized_content_length_is_rejected_before_body() {
    let fixture = spawn_fixture(|_| {
        Reply::Raw(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 999999999\r\n\r\n")
    });
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("oversized body");
    assert!(
        error.to_string().contains("16 MiB"),
        "expected size rejection, got: {error}"
    );
}

#[tokio::test]
async fn non_json_body_is_rejected() {
    let fixture = spawn_fixture(|_| Reply::Json("this is not json".into()));
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("garbage body");
    assert!(
        error.to_string().contains("invalid JSON response"),
        "expected parse rejection, got: {error}"
    );
}

// ---------------------------------------------------------------------------
// SSE-framed responses (Content-Type: text/event-stream)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_response_delivers_matching_result() {
    let fixture = spawn_fixture(
        |request| match request.get("method").and_then(Value::as_str) {
            Some("initialize") => Reply::Sse(format!(
                "data: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": request.get("id").cloned().unwrap_or(Value::Null),
                    "result": {"protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {"name": "adv", "version": "0"}},
                })
            )),
            Some("tools/list") => result_reply(request, json!({"tools": []})),
            _ => Reply::Accepted,
        },
    );
    let manager = manager(&fixture.url);
    assert_eq!(manager.test("adv").await.expect("sse connect"), 0);
}

#[tokio::test]
async fn sse_multiline_data_is_concatenated_with_newlines() {
    let fixture = spawn_fixture(
        |request| match request.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                // SSE spec: multiple `data:` lines join with '\n'; JSON survives
                // because a bare newline is legal whitespace inside a document.
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                Reply::Sse(format!(
                    "data: {{\"jsonrpc\": \"2.0\",\ndata: \"id\": {},\"result\": {{}}}}\n\n",
                    id
                ))
            }
            Some("tools/list") => result_reply(request, json!({"tools": []})),
            _ => Reply::Accepted,
        },
    );
    let manager = manager(&fixture.url);
    assert_eq!(manager.test("adv").await.expect("sse connect"), 0);
}

/// BUG: an SSE event made of a bare `data:` line (a keep-alive form some
/// servers and proxies emit) is fed to `serde_json::from_str` as an empty
/// document, and `read_sse_response` (http.rs:196-199) turns it into a fatal
/// `invalid SSE message` for the pending request — the connection cannot be
/// used. Expected: an empty data event is skipped like an `event:`/`id:`/
/// comment line, and the matching response still resolves.
/// Actual today: `Err(McpError::Protocol("invalid SSE message: EOF..."))`.
#[tokio::test]
async fn sse_empty_data_event_is_skipped() {
    let fixture = spawn_fixture(
        |request| match request.get("method").and_then(Value::as_str) {
            Some("initialize") => Reply::Sse(format!(
                "data:\n\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": request.get("id").cloned().unwrap_or(Value::Null),
                    "result": {"protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {"name": "adv", "version": "0"}},
                })
            )),
            Some("tools/list") => result_reply(request, json!({"tools": []})),
            _ => Reply::Accepted,
        },
    );
    let manager = manager(&fixture.url);
    manager
        .test("adv")
        .await
        .expect("empty data event must not kill the request");
}

/// Control: SSE comment lines (`:`-prefixed keep-alives) are correctly
/// skipped between events.
#[tokio::test]
async fn sse_comment_lines_are_ignored() {
    let fixture = spawn_fixture(
        |request| match request.get("method").and_then(Value::as_str) {
            Some("initialize") => Reply::Sse(format!(
                ": keepalive\n\nevent: message\ndata: {}\n\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": request.get("id").cloned().unwrap_or(Value::Null),
                    "result": {"protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {"name": "adv", "version": "0"}},
                })
            )),
            Some("tools/list") => result_reply(request, json!({"tools": []})),
            _ => Reply::Accepted,
        },
    );
    let manager = manager(&fixture.url);
    assert_eq!(manager.test("adv").await.expect("sse connect"), 0);
}

#[tokio::test]
async fn sse_stream_ending_without_matching_response_closes() {
    let fixture = spawn_fixture(
        |request| match request.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                // Deliver a response for a different id, then end the stream.
                let wrong = request
                    .get("id")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(9);
                Reply::Sse(format!(
                    "data: {}\n\n",
                    json!({"jsonrpc": "2.0", "id": wrong, "result": {}})
                ))
            }
            _ => Reply::Accepted,
        },
    );
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("unmatched stream");
    assert!(
        error.to_string().contains("closed"),
        "expected Closed after stream end, got: {error}"
    );
}

#[tokio::test]
async fn tools_list_without_tools_array_is_rejected() {
    let fixture = spawn_fixture(
        |request| match request.get("method").and_then(Value::as_str) {
            Some("initialize") => result_reply(
                request,
                json!({"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "adv", "version": "0"}}),
            ),
            Some("tools/list") => result_reply(request, json!({})),
            _ => Reply::Accepted,
        },
    );
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("missing tools");
    assert!(
        error.to_string().contains("missing tools array"),
        "expected tools contract error, got: {error}"
    );
}

#[tokio::test]
async fn error_object_response_becomes_server_error() {
    let fixture = spawn_fixture(|request| {
        Reply::Json(
            json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32000, "message": "upstream exploded"},
            })
            .to_string(),
        )
    });
    let manager = manager(&fixture.url);
    let error = manager.test("adv").await.expect_err("server error");
    assert!(
        error.to_string().contains("-32000"),
        "expected server error code, got: {error}"
    );
}

fn tool_page(request: &Value, names: &[&str], next_cursor: Option<&str>) -> Reply {
    let tools: Vec<Value> = names
        .iter()
        .map(|name| json!({"name": name, "inputSchema": {"type": "object"}}))
        .collect();
    let mut page = json!({"tools": tools});
    if let Some(cursor) = next_cursor {
        page["nextCursor"] = json!(cursor);
    }
    result_reply(request, page)
}

fn tools_list_requests(fixture: &Fixture) -> Vec<Value> {
    fixture
        .requests
        .try_iter()
        .filter(|request| request["method"] == "tools/list")
        .collect()
}

#[tokio::test]
async fn empty_next_cursor_ends_tools_pagination() {
    let fixture = spawn_fixture(|request| {
        if request["method"] == "tools/list" {
            tool_page(request, &["a", "b"], Some(""))
        } else {
            healthy(request)
        }
    });
    let manager = manager(&fixture.url);
    let tools = manager.list_tools("adv").await.expect("connect");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(tools_list_requests(&fixture).len(), 1);
}

#[tokio::test]
async fn repeated_next_cursor_stops_pagination_and_keeps_the_tools_received() {
    let fixture = spawn_fixture(|request| {
        if request["method"] != "tools/list" {
            return healthy(request);
        }
        match request["params"].get("cursor").and_then(Value::as_str) {
            None => tool_page(request, &["a"], Some("c1")),
            Some(_) => tool_page(request, &["b"], Some("c1")),
        }
    });
    let manager = manager(&fixture.url);
    let tools = manager.list_tools("adv").await.expect("connect");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(tools_list_requests(&fixture).len(), 2);
}

#[tokio::test]
async fn a_dropped_initialized_notification_is_retried_like_any_transient_failure() {
    let resets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&resets);
    let fixture = spawn_fixture(move |request| {
        if request["method"] == "notifications/initialized"
            && counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        {
            // Closes the socket without answering: a network-level failure.
            Reply::Raw(b"")
        } else {
            healthy(request)
        }
    });
    let manager = manager(&fixture.url);
    assert_eq!(manager.test("adv").await.expect("retried connect"), 0);
    let methods: Vec<_> = fixture
        .requests
        .try_iter()
        .map(|request| request["method"].clone())
        .collect();
    assert_eq!(
        methods
            .iter()
            .filter(|method| **method == json!("initialize"))
            .count(),
        2,
        "{methods:?}"
    );
}
