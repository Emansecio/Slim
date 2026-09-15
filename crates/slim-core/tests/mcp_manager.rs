//! McpManager behavior: laziness, reconciliation, lifecycle actions, and the
//! fake-connection hook the agent loop uses. No real process or socket is
//! spawned here — `insert_connection` installs a Ready entry directly.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use slim_core::mcp::{
    McpConnection, McpError, McpManager, McpServerSpec, McpServerStatus, McpToolSummary,
    McpTransport,
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
    assert!(matches!(error, McpError::Closed));
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
