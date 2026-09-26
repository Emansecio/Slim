//! McpManager behavior: laziness, reconciliation, lifecycle actions, and the
//! fake-connection hook the agent loop uses. Most cases install a Ready entry
//! directly; controlled subprocess fixtures exercise stdio end to end.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::mcp::{
    McpCancellation, McpConnection, McpError, McpInterruption, McpManager, McpRequestOutcome,
    McpServerSpec, McpServerStatus, McpToolSummary, McpTransport,
};
use slim_core::process::ExecutableResolver;

fn stdio_spec(name: &str) -> McpServerSpec {
    McpServerSpec {
        name: name.into(),
        transport: McpTransport::Stdio {
            command: "definitely-not-a-real-mcp-binary-xyz".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_millis(1_000),
    }
}

fn http_spec(name: &str) -> McpServerSpec {
    McpServerSpec {
        name: name.into(),
        transport: McpTransport::Http {
            url: "https://mcp.example.com/rpc".into(),
            headers: BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_millis(1_000),
    }
}

fn manager_with(specs: Vec<McpServerSpec>) -> Arc<McpManager> {
    Arc::new(McpManager::new(
        specs
            .into_iter()
            .map(|spec| (spec.name.clone(), spec))
            .collect(),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ))
}

fn spawn_http_fixture(
    blocked_method: &'static str,
) -> (
    String,
    mpsc::Receiver<String>,
    mpsc::Sender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local HTTP fixture");
    let url = format!("http://{}", listener.local_addr().expect("fixture address"));
    let (method_tx, method_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = thread::spawn(move || loop {
        let (mut stream, _) = listener.accept().expect("accept local HTTP request");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("bound fixture read");
        let request = read_http_request(&stream);
        let method = request["method"]
            .as_str()
            .expect("JSON-RPC method")
            .to_owned();
        if method_tx.send(method.clone()).is_err() {
            return;
        }
        if method == blocked_method {
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if method == "notifications/initialized" {
            stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("reply to initialized notification");
            continue;
        }
        let result = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": {"name": "fixture", "version": "1"},
            }),
            "tools/list" => json!({
                "tools": [{"name": "ping", "inputSchema": {"type": "object"}}],
            }),
            other => panic!("unexpected HTTP fixture method {other}"),
        };
        let response = json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": result,
        });
        write_http_response(&mut stream, &response);
    });
    (url, method_rx, release_tx, worker)
}

fn read_http_request(stream: &TcpStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().expect("clone fixture stream"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("read request line");
    assert!(line.starts_with("POST "), "{line:?}");
    let mut content_length = 0usize;
    loop {
        line.clear();
        reader.read_line(&mut line).expect("read request header");
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().expect("content length");
            }
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("read JSON-RPC body");
    serde_json::from_slice(&body).expect("parse JSON-RPC request")
}

fn write_http_response(stream: &mut TcpStream, body: &Value) {
    let body = serde_json::to_vec(body).expect("serialize JSON-RPC response");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write HTTP response headers");
    stream.write_all(&body).expect("write HTTP response body");
}

struct FakeConnection {
    closed: AtomicBool,
    calls: Mutex<Vec<(String, Value)>>,
    tools_stale: AtomicBool,
    fail_next_tools_list: AtomicBool,
    die_on_call: AtomicBool,
}

impl FakeConnection {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            calls: Mutex::new(Vec::new()),
            tools_stale: AtomicBool::new(false),
            fail_next_tools_list: AtomicBool::new(false),
            die_on_call: AtomicBool::new(false),
        })
    }

    fn recorded(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl McpConnection for FakeConnection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((method.to_owned(), params));
        if self.closed.load(Ordering::Relaxed) {
            return Err(McpError::Closed);
        }
        match method {
            "tools/list" => {
                if self.fail_next_tools_list.swap(false, Ordering::Relaxed) {
                    return Err(McpError::Protocol("boom".into()));
                }
                Ok(json!({
                    "tools": [{"name": "fresh", "inputSchema": {"type": "object"}}],
                }))
            }
            "tools/call" => {
                if self.die_on_call.swap(false, Ordering::Relaxed) {
                    self.closed.store(true, Ordering::Relaxed);
                    return Err(McpError::Closed);
                }
                Ok(json!({
                    "content": [{"type": "text", "text": "pong"}],
                    "isError": false,
                }))
            }
            _ => Ok(json!({})),
        }
    }

    async fn notify(&self, _method: &str, _params: Value) {}

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn take_tools_stale(&self) -> bool {
        self.tools_stale.swap(false, Ordering::Relaxed)
    }

    fn mark_tools_stale(&self) {
        self.tools_stale.store(true, Ordering::Relaxed);
    }
}

fn ready_tool(name: &str) -> McpToolSummary {
    McpToolSummary {
        name: name.into(),
        description: Some("fake tool".into()),
        schema: json!({"type": "object"}),
    }
}

#[test]
fn construction_is_lazy_and_list_servers_never_connects() {
    let manager = manager_with(vec![stdio_spec("fs"), http_spec("web")]);
    let text = manager.list_servers();
    assert!(text.contains("fs [stdio] disconnected"), "{text}");
    assert!(text.contains("web [http] disconnected"), "{text}");
    // Still disconnected: listing servers is a pure config read.
    for info in manager.statuses() {
        assert!(matches!(info.status, McpServerStatus::Disconnected));
    }
}

#[test]
fn disabled_server_reports_disabled_without_connecting() {
    let mut spec = stdio_spec("off");
    spec.enabled = false;
    let manager = manager_with(vec![spec]);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let error = runtime
        .block_on(manager.list_tools("off"))
        .expect_err("disabled server errors");
    assert!(matches!(error, McpError::Disabled(_)));
    let info = manager.statuses().pop().expect("one server");
    assert!(matches!(info.status, McpServerStatus::Disabled));
}

#[test]
fn failed_connect_marks_server_failed_with_error() {
    let manager = manager_with(vec![stdio_spec("fs")]);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    runtime
        .block_on(manager.test("fs"))
        .expect_err("missing binary fails");
    let info = manager.statuses().pop().expect("one server");
    match info.status {
        McpServerStatus::Failed { error } => {
            assert!(
                error.contains("definitely-not-a-real-mcp-binary-xyz"),
                "{error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn inserted_connection_serves_calls_and_tracks_revisions() {
    let manager = manager_with(vec![]);
    let baseline = manager.revision();
    let connection = FakeConnection::new();
    manager.insert_connection(
        stdio_spec("fake"),
        connection.clone() as Arc<dyn McpConnection>,
        vec![ready_tool("ping")],
    );
    assert!(manager.revision() > baseline);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    assert_eq!(runtime.block_on(manager.test("fake")).expect("ok"), 1);
    let reply = runtime
        .block_on(manager.call("fake", "ping", json!({"x": 1})))
        .expect("call succeeds");
    assert_eq!(reply["isError"], false);
    let calls = connection.recorded();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "tools/call");
    assert_eq!(calls[0].1["name"], "ping");
    assert!(manager
        .list_servers()
        .contains("fake [stdio] ready, 1 tools"));
}

#[test]
fn call_never_retries_a_possibly_executed_tool() {
    let manager = manager_with(vec![]);
    let connection = FakeConnection::new();
    manager.insert_connection(
        stdio_spec("s"),
        connection.clone() as Arc<dyn McpConnection>,
        vec![ready_tool("ping")],
    );
    connection.die_on_call.store(true, Ordering::Relaxed);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let error = runtime
        .block_on(manager.call("s", "ping", json!({})))
        .expect_err("dead transport errors");
    assert!(matches!(error, McpError::OutcomeUncertain { .. }));
    // Exactly one tools/call reached the wire: the failure must surface
    // instead of replaying a tool that may already have run server-side.
    let calls = connection.recorded();
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| method == "tools/call")
            .count(),
        1,
        "{calls:?}"
    );
    let info = manager.statuses().pop().expect("one server");
    assert!(matches!(info.status, McpServerStatus::Disconnected));
}

#[test]
fn http_tools_call_timeout_is_uncertain_and_disconnects_without_replay() {
    let (url, methods, release, worker) = spawn_http_fixture("tools/call");
    let timeout = Duration::from_millis(200);
    let spec = McpServerSpec {
        name: "web".into(),
        transport: McpTransport::Http {
            url,
            headers: BTreeMap::new(),
        },
        enabled: true,
        timeout,
    };
    let manager = manager_with(vec![spec]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let outcome = runtime.block_on(manager.call_cancellable(
        "web",
        "ping",
        json!({}),
        McpCancellation::new(),
    ));

    assert!(
        matches!(
            outcome,
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::TimedOut(elapsed),
                cleanup: slim_core::mcp::McpCleanupStatus::Unconfirmed,
            } if elapsed == timeout
        ),
        "expected timeout uncertainty, got {outcome:?}"
    );
    assert!(matches!(
        manager
            .statuses()
            .pop()
            .expect("configured HTTP server")
            .status,
        McpServerStatus::Disconnected
    ));
    release.send(()).expect("release HTTP fixture");
    worker.join().expect("HTTP fixture exits");
    let methods = methods.try_iter().collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call",
        ]
    );
}

#[test]
fn cancelling_http_initialized_notification_returns_before_timeout_without_tool_call() {
    let (url, methods, release, worker) = spawn_http_fixture("notifications/initialized");
    let spec = McpServerSpec {
        name: "web".into(),
        transport: McpTransport::Http {
            url,
            headers: BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_secs(10),
    };
    let manager = manager_with(vec![spec]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let cancellation = McpCancellation::new();
    let request_manager = Arc::clone(&manager);
    let request_cancellation = cancellation.clone();
    let request = runtime.spawn(async move {
        request_manager
            .call_cancellable("web", "ping", json!({}), request_cancellation)
            .await
    });

    assert_eq!(
        methods
            .recv_timeout(Duration::from_secs(3))
            .expect("initialize reached fixture"),
        "initialize"
    );
    assert_eq!(
        methods
            .recv_timeout(Duration::from_secs(3))
            .expect("initialized notification reached fixture"),
        "notifications/initialized"
    );
    let cancelled_at = std::time::Instant::now();
    cancellation.cancel();
    let outcome = runtime
        .block_on(request)
        .expect("cancelled call task completes");
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        outcome,
        McpRequestOutcome::InterruptedBeforeSend {
            interruption: McpInterruption::Cancelled,
            cleanup: slim_core::mcp::McpCleanupStatus::Unconfirmed,
        }
    ));
    assert!(matches!(
        manager
            .statuses()
            .pop()
            .expect("configured HTTP server")
            .status,
        McpServerStatus::Disconnected
    ));
    release.send(()).expect("release HTTP fixture");
    worker.join().expect("HTTP fixture exits");
    assert!(methods.try_iter().next().is_none());
}

#[test]
fn stale_flag_survives_a_failed_refresh() {
    let manager = manager_with(vec![]);
    let connection = FakeConnection::new();
    manager.insert_connection(
        stdio_spec("s"),
        connection.clone() as Arc<dyn McpConnection>,
        vec![ready_tool("cached")],
    );
    connection.tools_stale.store(true, Ordering::Relaxed);
    connection
        .fail_next_tools_list
        .store(true, Ordering::Relaxed);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    runtime
        .block_on(manager.list_tools("s"))
        .expect_err("failing refresh errors");
    // The failed refresh must have re-armed the stale flag: this call retries
    // tools/list instead of serving "cached" forever.
    let tools = runtime
        .block_on(manager.list_tools("s"))
        .expect("second refresh succeeds");
    assert!(tools.iter().any(|tool| tool.name == "fresh"), "{tools:?}");
    assert!(!tools.iter().any(|tool| tool.name == "cached"), "{tools:?}");
}

#[test]
fn list_tools_text_paginates_with_offset() {
    let manager = manager_with(vec![]);
    let tools: Vec<McpToolSummary> = (0..40)
        .map(|index| ready_tool(&format!("t{index}")))
        .collect();
    manager.insert_connection(stdio_spec("s"), FakeConnection::new(), tools);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let first = runtime
        .block_on(manager.list_tools_text("s", 0))
        .expect("first page");
    assert_eq!(first.lines().count(), 33, "{first}");
    assert!(first.contains("t0 — fake tool"), "{first}");
    assert!(first.contains("t31 — fake tool"), "{first}");
    assert!(!first.contains("t32 — fake tool"), "{first}");
    assert!(first.contains("8 more tools"), "{first}");
    assert!(first.contains("\"offset\": 32"), "{first}");
    let second = runtime
        .block_on(manager.list_tools_text("s", 32))
        .expect("second page");
    assert_eq!(second.lines().count(), 8, "{second}");
    assert!(second.contains("t32 — fake tool"), "{second}");
    assert!(second.contains("t39 — fake tool"), "{second}");
    assert!(!second.contains("more tools"), "{second}");
    let third = runtime
        .block_on(manager.list_tools_text("s", 40))
        .expect("past-end page");
    assert_eq!(third, "(no tools at offset 40; 40 total)");
}

#[test]
fn reconcile_replaces_changed_specs_and_drops_removed() {
    let manager = manager_with(vec![stdio_spec("keep"), http_spec("gone")]);
    manager.insert_connection(
        stdio_spec("keep"),
        FakeConnection::new(),
        vec![ready_tool("a")],
    );
    let mut replacement = stdio_spec("keep");
    replacement.timeout = Duration::from_secs(42);
    let mut next = BTreeMap::new();
    next.insert("keep".to_owned(), replacement);
    next.insert("new".to_owned(), http_spec("new"));
    manager.reconcile(next);
    let infos = manager.statuses();
    assert_eq!(infos.len(), 2);
    for info in &infos {
        // The changed spec lost its Ready state; the new server starts
        // disconnected; the removed one is gone entirely.
        assert!(matches!(info.status, McpServerStatus::Disconnected));
    }
    assert!(infos.iter().all(|info| info.name != "gone"));
}

#[test]
fn describe_truncates_on_a_utf8_char_boundary() {
    // Regression: slicing a pretty-printed schema at MAX_DESCRIBE_BYTES used
    // to panic when a multibyte char straddled the boundary.
    let manager = manager_with(vec![]);
    // to_string_pretty renders {"x": "<content>"}: the opening quote sits at
    // byte 9, so content begins at byte 10. "a"*16373 + "世" puts 世's second
    // byte exactly at byte 16384 (MAX_DESCRIBE_BYTES).
    let content = format!("{}{}", "a".repeat(16_373), "世".repeat(64));
    let tool = McpToolSummary {
        name: "big".into(),
        description: None,
        schema: json!({"x": content}),
    };
    manager.insert_connection(stdio_spec("s"), FakeConnection::new(), vec![tool]);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let rendered = runtime
        .block_on(manager.describe("s", "big"))
        .expect("describe succeeds");
    assert!(rendered.ends_with("…(truncated)"), "{rendered}");
    assert!(rendered.is_char_boundary(rendered.len()));
}

#[test]
fn disconnect_remove_and_unknown_server_errors() {
    let manager = manager_with(vec![stdio_spec("fs")]);
    manager.insert_connection(stdio_spec("fs"), FakeConnection::new(), vec![]);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    runtime
        .block_on(manager.disconnect("fs"))
        .expect("disconnect");
    let info = manager.statuses().pop().expect("one server");
    assert!(matches!(info.status, McpServerStatus::Disconnected));
    assert!(manager.remove("fs"));
    assert!(manager.statuses().is_empty());
    assert!(matches!(
        runtime.block_on(manager.disconnect("fs")),
        Err(McpError::UnknownServer(_))
    ));
}

/// Subprocess fixture: a real MCP server speaking newline-delimited JSON-RPC
/// on stdio. Spawned as `test-binary --exact mcp_stdio_fixture --ignored`;
/// libtest's own stdout banner is skipped by the framer as noise.
#[test]
#[ignore = "subprocess fixture"]
#[allow(clippy::while_let_on_iterator)]
fn mcp_stdio_fixture() {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut lines = stdin.lock().lines();
    let write_line = |value: &Value| {
        let mut out = stdout.lock();
        out.write_all(serde_json::to_string(value).expect("json").as_bytes())
            .expect("write");
        out.write_all(b"\n").expect("newline");
        out.flush().expect("flush");
    };
    // `while let` (not `for`): the loop body re-enters the iterator to await
    // the reply to its own server→client request.
    while let Some(line) = lines.next() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let result = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "0"},
            }),
            Some("tools/list") => json!({
                "tools": [{
                    "name": "ping",
                    "description": "replies pong",
                    "inputSchema": {"type": "object"},
                }],
            }),
            Some("tools/call") => {
                if message["params"]["arguments"]["probe"] == true {
                    // Server→client request with a string id: the client must
                    // answer (JSON-RPC) or the fixture hangs waiting for it.
                    write_line(&json!({
                        "jsonrpc": "2.0",
                        "id": "srv-1",
                        "method": "sampling/createMessage",
                        "params": {},
                    }));
                    let mut code = String::from("no-response");
                    for line in lines.by_ref() {
                        let Ok(line) = line else { break };
                        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if reply.get("id") == Some(&json!("srv-1")) {
                            code = reply["error"]["code"].to_string();
                            break;
                        }
                    }
                    json!({
                        "content": [{"type": "text", "text": code}],
                        "isError": false,
                    })
                } else {
                    json!({
                        "content": [{"type": "text", "text": "pong"}],
                        "isError": false,
                    })
                }
            }
            _ => continue,
        };
        write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
}

/// Controlled subprocess fixture for lazy-handshake and call cancellation.
/// The first child records the selected method and blocks until transport
/// cleanup kills it; the next child serves normally. The test exchanges state
/// through this directory so ordering is independent of scheduler timing.
#[test]
#[ignore = "subprocess fixture"]
#[allow(clippy::while_let_on_iterator)]
fn mcp_lazy_cancel_fixture() {
    use std::io::{BufRead, Write};

    const CONTROL_ENV: &str = "SLIM_MCP_LAZY_CANCEL_FIXTURE_DIR";
    const BLOCKED_METHOD_ENV: &str = "SLIM_MCP_LAZY_CANCEL_BLOCKED_METHOD";
    let directory = PathBuf::from(std::env::var_os(CONTROL_ENV).expect("fixture control dir"));
    let blocked_method = std::env::var(BLOCKED_METHOD_ENV).unwrap_or_else(|_| "tools/list".into());
    let first_child = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("first-child"))
    {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => panic!("create first-child marker: {error}"),
    };

    let record_method = |method: &str| {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("methods.log"))
            .expect("open methods log");
        writeln!(log, "{method}").expect("record method");
        log.flush().expect("flush methods log");
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut lines = stdin.lock().lines();
    let write_line = |value: &Value| {
        let mut out = stdout.lock();
        out.write_all(serde_json::to_string(value).expect("json").as_bytes())
            .expect("write");
        out.write_all(b"\n").expect("newline");
        out.flush().expect("flush");
    };

    while let Some(line) = lines.next() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        record_method(method);
        if method == blocked_method && first_child {
            let marker_name = format!("first-{}-blocked", blocked_method.replace('/', "-"));
            fs::write(directory.join(marker_name), b"ready").expect("write blocked request marker");
            // A correct cancellation closes the transport while the selected
            // request is in flight. Should another request arrive, record it;
            // never answer the blocked operation.
            for later in lines.by_ref() {
                let Ok(later) = later else { break };
                if let Ok(later) = serde_json::from_str::<Value>(&later) {
                    if let Some(method) = later.get("method").and_then(Value::as_str) {
                        record_method(method);
                    }
                }
            }
            break;
        }
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "lazy-cancel-fixture", "version": "0"},
            }),
            "tools/list" => json!({
                "tools": [{
                    "name": "ping",
                    "description": "replies pong",
                    "inputSchema": {"type": "object"},
                }],
            }),
            "tools/call" => json!({
                "content": [{"type": "text", "text": "pong"}],
                "isError": false,
            }),
            _ => continue,
        };
        write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
}

struct FixtureDirectory(PathBuf);

impl FixtureDirectory {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slim-mcp-lazy-cancel-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create fixture directory");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cancellation_during_lazy_tools_list_disconnects_without_call_and_next_call_reconnects() {
    const CONTROL_ENV: &str = "SLIM_MCP_LAZY_CANCEL_FIXTURE_DIR";
    let fixture_directory = FixtureDirectory::new();
    let spec = McpServerSpec {
        name: "fixture".into(),
        transport: McpTransport::Stdio {
            command: std::env::current_exe()
                .expect("current exe")
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".into(),
                "mcp_lazy_cancel_fixture".into(),
                "--ignored".into(),
            ],
            env: BTreeMap::from([(
                CONTROL_ENV.into(),
                fixture_directory.path().to_string_lossy().into_owned(),
            )]),
        },
        enabled: true,
        timeout: Duration::from_secs(10),
    };
    let manager = Arc::new(McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ));
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    runtime.block_on(async {
        let cancellation = McpCancellation::new();
        let request_manager = Arc::clone(&manager);
        let request_cancellation = cancellation.clone();
        let request = tokio::spawn(async move {
            request_manager
                .call_cancellable("fixture", "ping", json!({}), request_cancellation)
                .await
        });
        let blocked_marker = fixture_directory.path().join("first-tools-list-blocked");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !blocked_marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture reached blocked tools/list");

        cancellation.cancel();
        let cleanup = match request.await.expect("cancelled call task") {
            McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup,
            } => cleanup,
            other => panic!("expected lazy handshake cancellation, got {other:?}"),
        };
        assert_eq!(cleanup, slim_core::mcp::McpCleanupStatus::Confirmed);
        let status = manager.statuses().pop().expect("configured server").status;
        assert!(matches!(status, McpServerStatus::Disconnected));

        let methods_path = fixture_directory.path().join("methods.log");
        let before_reconnect = fs::read_to_string(&methods_path).expect("read first child log");
        assert_eq!(
            before_reconnect
                .lines()
                .filter(|m| *m == "initialize")
                .count(),
            1
        );
        assert_eq!(
            before_reconnect
                .lines()
                .filter(|m| *m == "tools/list")
                .count(),
            1
        );
        assert!(!before_reconnect.lines().any(|m| m == "tools/call"));

        let reply = manager
            .call("fixture", "ping", json!({}))
            .await
            .expect("next call reconnects and succeeds");
        assert_eq!(reply["content"][0]["text"], "pong");
        manager
            .disconnect("fixture")
            .await
            .expect("disconnect fixture");

        let methods = fs::read_to_string(methods_path).expect("read both child logs");
        assert_eq!(methods.lines().filter(|m| *m == "initialize").count(), 2);
        assert_eq!(methods.lines().filter(|m| *m == "tools/list").count(), 2);
        assert_eq!(methods.lines().filter(|m| *m == "tools/call").count(), 1);
        assert_eq!(
            methods
                .lines()
                .filter(|m| *m == "notifications/initialized")
                .count(),
            2
        );
    });
}

#[test]
fn cancellation_during_stdio_tool_call_closes_child_without_replaying_effect() {
    const CONTROL_ENV: &str = "SLIM_MCP_LAZY_CANCEL_FIXTURE_DIR";
    const BLOCKED_METHOD_ENV: &str = "SLIM_MCP_LAZY_CANCEL_BLOCKED_METHOD";
    let fixture_directory = FixtureDirectory::new();
    let spec = McpServerSpec {
        name: "fixture".into(),
        transport: McpTransport::Stdio {
            command: std::env::current_exe()
                .expect("current exe")
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".into(),
                "mcp_lazy_cancel_fixture".into(),
                "--ignored".into(),
            ],
            env: BTreeMap::from([
                (
                    CONTROL_ENV.into(),
                    fixture_directory.path().to_string_lossy().into_owned(),
                ),
                (BLOCKED_METHOD_ENV.into(), "tools/call".into()),
            ]),
        },
        enabled: true,
        timeout: Duration::from_secs(10),
    };
    let manager = Arc::new(McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ));
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    runtime.block_on(async {
        let cancellation = McpCancellation::new();
        let request_manager = Arc::clone(&manager);
        let request_cancellation = cancellation.clone();
        let request = tokio::spawn(async move {
            request_manager
                .call_cancellable("fixture", "ping", json!({}), request_cancellation)
                .await
        });
        let blocked_marker = fixture_directory.path().join("first-tools-call-blocked");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !blocked_marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture received tools/call and is waiting for its result");

        let cancelled_at = std::time::Instant::now();
        cancellation.cancel();
        let cleanup = match request.await.expect("cancelled tools/call task") {
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup,
            } => cleanup,
            other => panic!("expected uncertain tools/call outcome, got {other:?}"),
        };
        assert!(cancelled_at.elapsed() < Duration::from_secs(1));
        assert_eq!(cleanup, slim_core::mcp::McpCleanupStatus::Confirmed);
        assert!(matches!(
            manager.statuses().pop().expect("configured server").status,
            McpServerStatus::Disconnected
        ));

        let methods_path = fixture_directory.path().join("methods.log");
        let before_reconnect = fs::read_to_string(&methods_path).expect("read first child log");
        assert_eq!(
            before_reconnect
                .lines()
                .filter(|method| *method == "tools/call")
                .count(),
            1,
            "the uncertain call must not replay before explicit reconnection"
        );

        let reply = manager
            .call("fixture", "ping", json!({}))
            .await
            .expect("next call reconnects and succeeds");
        assert_eq!(reply["content"][0]["text"], "pong");
        manager
            .disconnect("fixture")
            .await
            .expect("disconnect fixture");

        let methods = fs::read_to_string(methods_path).expect("read both child logs");
        assert_eq!(
            methods
                .lines()
                .filter(|method| *method == "initialize")
                .count(),
            2
        );
        assert_eq!(
            methods
                .lines()
                .filter(|method| *method == "tools/list")
                .count(),
            2
        );
        assert_eq!(
            methods
                .lines()
                .filter(|method| *method == "tools/call")
                .count(),
            2
        );
        assert_eq!(
            methods
                .lines()
                .filter(|method| *method == "notifications/initialized")
                .count(),
            2
        );
    });
}

#[test]
fn cancelled_stdio_batch_reaps_only_its_own_children_and_threads() {
    const BATCH_SIZE: usize = 3;
    const CONTROL_ENV: &str = "SLIM_MCP_LAZY_CANCEL_FIXTURE_DIR";
    const BLOCKED_METHOD_ENV: &str = "SLIM_MCP_LAZY_CANCEL_BLOCKED_METHOD";

    let fixtures = (0..BATCH_SIZE)
        .map(|index| (format!("fixture-{index}"), FixtureDirectory::new()))
        .collect::<Vec<_>>();
    let executable = std::env::current_exe().expect("current test executable");
    let specs = fixtures
        .iter()
        .map(|(name, directory)| {
            (
                name.clone(),
                McpServerSpec {
                    name: name.clone(),
                    transport: McpTransport::Stdio {
                        command: executable.to_string_lossy().into_owned(),
                        args: vec![
                            "--exact".into(),
                            "mcp_lazy_cancel_fixture".into(),
                            "--ignored".into(),
                        ],
                        env: BTreeMap::from([
                            (
                                CONTROL_ENV.into(),
                                directory.path().to_string_lossy().into_owned(),
                            ),
                            (BLOCKED_METHOD_ENV.into(), "tools/call".into()),
                        ]),
                    },
                    enabled: true,
                    timeout: Duration::from_secs(10),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let manager = Arc::new(McpManager::new(
        specs,
        PathBuf::from("."),
        ExecutableResolver::default(),
    ));
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    runtime.block_on(async {
        let (sentinel_started_tx, sentinel_started_rx) = mpsc::sync_channel(1);
        let (sentinel_release_tx, sentinel_release_rx) = mpsc::sync_channel(1);
        let sentinel_done = Arc::new(AtomicBool::new(false));
        let sentinel_done_in_thread = Arc::clone(&sentinel_done);
        let sentinel = thread::spawn(move || {
            sentinel_started_tx
                .send(())
                .expect("test is waiting for sentinel startup");
            sentinel_release_rx
                .recv()
                .expect("test releases unrelated sentinel");
            sentinel_done_in_thread.store(true, Ordering::Release);
        });
        sentinel_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("unrelated sentinel started");

        let requests = fixtures
            .iter()
            .map(|(name, _)| {
                let cancellation = McpCancellation::new();
                let task_cancellation = cancellation.clone();
                let task_manager = Arc::clone(&manager);
                let task_name = name.clone();
                let task = tokio::spawn(async move {
                    task_manager
                        .call_cancellable(&task_name, "ping", json!({}), task_cancellation)
                        .await
                });
                (name.clone(), cancellation, task)
            })
            .collect::<Vec<_>>();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if fixtures.iter().all(|(_, directory)| {
                    directory.path().join("first-tools-call-blocked").exists()
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("each child reached its own blocked tools/call");

        for (_, cancellation, _) in &requests {
            cancellation.cancel();
        }
        for (name, _, task) in requests {
            match task.await.expect("abandoned call task") {
                // Confirmed is per connection: the fixture child exited and
                // this StdioConnection joined its own transport threads.
                McpRequestOutcome::OutcomeUncertain {
                    interruption: McpInterruption::Cancelled,
                    cleanup: slim_core::mcp::McpCleanupStatus::Confirmed,
                } => {}
                other => panic!("expected confirmed cleanup for {name}, got {other:?}"),
            }
        }

        assert!(manager
            .statuses()
            .iter()
            .all(|server| { matches!(server.status, McpServerStatus::Disconnected) }));
        for (_, directory) in &fixtures {
            let methods = fs::read_to_string(directory.path().join("methods.log"))
                .expect("read methods recorded by this child's fixture");
            assert_eq!(
                methods
                    .lines()
                    .filter(|method| *method == "tools/call")
                    .count(),
                1,
                "a cancelled side effect must not be replayed"
            );
            assert_eq!(
                methods
                    .lines()
                    .filter(|method| *method == "initialize")
                    .count(),
                1
            );
            assert_eq!(
                methods
                    .lines()
                    .filter(|method| *method == "tools/list")
                    .count(),
                1
            );
            assert_eq!(
                methods
                    .lines()
                    .filter(|method| *method == "notifications/initialized")
                    .count(),
                1
            );
        }

        assert!(
            !sentinel_done.load(Ordering::Acquire),
            "cleanup should reap only resources owned by the cancelled connections"
        );
        sentinel_release_tx
            .send(())
            .expect("release unrelated sentinel after verifying isolation");
        sentinel.join().expect("join unrelated sentinel");
        assert!(sentinel_done.load(Ordering::Acquire));
    });
}

#[test]
fn stdio_end_to_end_initialize_list_call_and_disconnect() {
    let exe = std::env::current_exe().expect("current exe");
    let spec = McpServerSpec {
        name: "fixture".into(),
        transport: McpTransport::Stdio {
            command: exe.to_string_lossy().into_owned(),
            args: vec![
                "--exact".into(),
                "mcp_stdio_fixture".into(),
                "--ignored".into(),
            ],
            env: BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_secs(15),
    };
    let manager = manager_with(vec![spec]);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    // First real op spawns the child, initializes, and lists tools.
    assert_eq!(
        runtime.block_on(manager.test("fixture")).expect("connect"),
        1
    );
    let reply = runtime
        .block_on(manager.call("fixture", "ping", json!({})))
        .expect("call");
    assert_eq!(reply["content"][0]["text"], "pong");
    // The fixture issues a server→client request with a string id and echoes
    // back the JSON-RPC error code it received; a silent client hangs here.
    let reply = runtime
        .block_on(manager.call("fixture", "ping", json!({"probe": true})))
        .expect("probe call");
    assert_eq!(reply["content"][0]["text"], "-32601");
    runtime
        .block_on(manager.disconnect("fixture"))
        .expect("disconnect");
    let info = manager.statuses().pop().expect("one server");
    assert!(matches!(info.status, McpServerStatus::Disconnected));
    drop(manager);
}
