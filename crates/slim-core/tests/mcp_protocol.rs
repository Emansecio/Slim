//! MCP protocol core on both transports: version negotiation, client
//! capabilities (`roots/list`, `ping`), `notifications/cancelled`, progress
//! (timeout renewal and events), server logging to `mcp.log`, and list-change
//! notifications. Stdio runs against this test binary re-spawned with
//! `--ignored` as a scripted server; HTTP runs against a raw TCP fixture.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::mcp::{
    McpCancellation, McpError, McpInterruption, McpManager, McpProgress, McpProgressSink,
    McpRequestOutcome, McpServerSpec, McpServerStatus, McpTransport,
};
use slim_core::process::ExecutableResolver;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slim-mcp-proto-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime")
}

fn collecting_sink() -> (McpProgressSink, Arc<Mutex<Vec<McpProgress>>>) {
    let updates = Arc::new(Mutex::new(Vec::new()));
    let sink_updates = Arc::clone(&updates);
    let sink: McpProgressSink = Arc::new(move |update| {
        sink_updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(update);
    });
    (sink, updates)
}

fn poll<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + deadline;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= end {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Stdio: scripted server
// ---------------------------------------------------------------------------

/// Scripted stdio server. Every received message is appended to
/// `$MCP_PROTO_DIR/messages.jsonl`. `initialize` reports
/// `$MCP_PROTO_VERSION` / `$MCP_PROTO_CAPS` / `$MCP_PROTO_INSTRUCTIONS`
/// (`$MCP_PROTO_HANG_INIT` makes it never answer). `tools/call` behavior
/// comes from its arguments: `progress`, `log`, `list_changed`, `roots`,
/// `hang`.
#[test]
#[ignore = "subprocess fixture"]
#[allow(clippy::while_let_on_iterator)]
fn mcp_protocol_fixture() {
    let dir = PathBuf::from(std::env::var_os("MCP_PROTO_DIR").expect("fixture dir"));
    let version = std::env::var("MCP_PROTO_VERSION").unwrap_or_else(|_| "2025-11-25".into());
    let capabilities: Value = serde_json::from_str(
        &std::env::var("MCP_PROTO_CAPS").unwrap_or_else(|_| r#"{"tools":{}}"#.into()),
    )
    .expect("caps json");
    let instructions = std::env::var("MCP_PROTO_INSTRUCTIONS").ok();
    let hang_init = std::env::var_os("MCP_PROTO_HANG_INIT").is_some();

    let record = |value: &Value| {
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("messages.jsonl"))
            .expect("open messages log");
        writeln!(log, "{value}").expect("record message");
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut lines = stdin.lock().lines();
    let write_line = |value: &Value| {
        let mut out = stdout.lock();
        out.write_all(value.to_string().as_bytes()).expect("write");
        out.write_all(b"\n").expect("newline");
        out.flush().expect("flush");
    };
    let notify = |method: &str, params: Value| {
        write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    };

    while let Some(line) = lines.next() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        record(&message);
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let result = match method {
            "initialize" => {
                if hang_init {
                    continue;
                }
                let mut result = json!({
                    "protocolVersion": version,
                    "capabilities": capabilities,
                    "serverInfo": {"name": "proto-fixture", "version": "9.9"},
                });
                if let Some(instructions) = &instructions {
                    result["instructions"] = json!(instructions);
                }
                result
            }
            "tools/list" => json!({
                "tools": [{"name": "ping", "description": "pong", "inputSchema": {"type": "object"}}],
            }),
            "tools/call" => {
                let arguments = message["params"]["arguments"].clone();
                let own_token = message["params"]["_meta"]["progressToken"].clone();
                if let Some(progress) = arguments.get("progress") {
                    let count = progress["count"].as_u64().unwrap_or(1);
                    let interval =
                        Duration::from_millis(progress["interval_ms"].as_u64().unwrap_or(10));
                    let token = progress.get("token").cloned().unwrap_or(own_token);
                    for step in 1..=count {
                        thread::sleep(interval);
                        notify(
                            "notifications/progress",
                            json!({
                                "progressToken": token,
                                "progress": step,
                                "total": count,
                                "message": progress["message"],
                            }),
                        );
                    }
                }
                if arguments["log"] == true {
                    notify(
                        "notifications/message",
                        json!({
                            "level": "warning",
                            "logger": "db",
                            "data": format!("connected using {}\nsecond line", arguments["secret"].as_str().unwrap_or("")),
                        }),
                    );
                }
                if arguments["list_changed"] == true {
                    notify("notifications/resources/list_changed", json!({}));
                    notify("notifications/tools/list_changed", json!({}));
                }
                let mut text = String::from("done");
                if arguments["roots"] == true {
                    write_line(&json!({"jsonrpc": "2.0", "id": "r1", "method": "roots/list"}));
                    write_line(&json!({"jsonrpc": "2.0", "id": "p1", "method": "ping"}));
                    let mut answers = BTreeMap::new();
                    for line in lines.by_ref() {
                        let Ok(line) = line else { break };
                        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        record(&reply);
                        if let Some(reply_id) = reply.get("id").and_then(Value::as_str) {
                            answers.insert(reply_id.to_owned(), reply);
                        }
                        if answers.len() == 2 {
                            break;
                        }
                    }
                    text = json!(answers).to_string();
                }
                if arguments["hang"] == true {
                    continue;
                }
                json!({"content": [{"type": "text", "text": text}], "isError": false})
            }
            _ => continue,
        };
        write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
}

struct StdioHarness {
    dir: TempDir,
    manager: Arc<McpManager>,
}

#[derive(Default)]
struct StdioOptions {
    env: Vec<(&'static str, String)>,
    timeout: Option<Duration>,
    log: Option<PathBuf>,
    workspace: Option<PathBuf>,
}

impl StdioHarness {
    fn new(label: &str, options: StdioOptions) -> Self {
        let dir = TempDir::new(label);
        let mut env = BTreeMap::from([(
            "MCP_PROTO_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        )]);
        for (key, value) in options.env {
            env.insert(key.to_owned(), value);
        }
        let exe = std::env::current_exe().expect("current exe");
        let mut spec = McpServerSpec::new(
            "fixture",
            McpTransport::Stdio {
                command: exe.to_string_lossy().into_owned(),
                args: vec![
                    "--exact".into(),
                    "mcp_protocol_fixture".into(),
                    "--ignored".into(),
                ],
                env,
            },
        );
        spec.timeout = options.timeout.unwrap_or(Duration::from_secs(15));
        let workspace = options
            .workspace
            .unwrap_or_else(|| dir.path().to_path_buf());
        let mut manager = McpManager::new(
            BTreeMap::from([(spec.name.clone(), spec)]),
            workspace,
            ExecutableResolver::default(),
        );
        if let Some(log) = options.log {
            manager = manager.with_log_path(log);
        }
        Self {
            dir,
            manager: Arc::new(manager),
        }
    }

    fn messages(&self) -> Vec<Value> {
        read_log(&self.dir.path().join("messages.jsonl"))
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn wait_for_method(&self, method: &str) -> Value {
        poll(Duration::from_secs(10), || {
            self.messages()
                .into_iter()
                .find(|message| message["method"] == method)
        })
        .unwrap_or_else(|| panic!("fixture never received {method}"))
    }

    fn methods(&self) -> Vec<String> {
        self.messages()
            .iter()
            .filter_map(|message| message["method"].as_str().map(str::to_owned))
            .collect()
    }
}

#[test]
fn stdio_negotiates_a_supported_older_revision_and_keeps_server_details() {
    let harness = StdioHarness::new(
        "neg",
        StdioOptions {
            env: vec![
                ("MCP_PROTO_VERSION", "2025-06-18".into()),
                ("MCP_PROTO_INSTRUCTIONS", "  Use ping first.  ".into()),
                ("MCP_PROTO_CAPS", r#"{"tools":{},"resources":{}}"#.into()),
            ],
            ..Default::default()
        },
    );
    assert!(harness.manager.handshake("fixture").is_none());
    assert_eq!(
        runtime().block_on(harness.manager.test("fixture")).unwrap(),
        1
    );
    let handshake = harness.manager.handshake("fixture").expect("handshake");
    assert_eq!(handshake.protocol_version, "2025-06-18");
    assert_eq!(handshake.server_name.as_deref(), Some("proto-fixture"));
    assert_eq!(handshake.server_version.as_deref(), Some("9.9"));
    assert_eq!(handshake.instructions.as_deref(), Some("Use ping first."));
    assert!(handshake.has_tools() && handshake.has_resources());
    let info = harness.manager.statuses().pop().expect("server");
    assert_eq!(
        info.handshake.expect("status carries it").protocol_version,
        "2025-06-18"
    );

    let initialize = harness.messages().remove(0);
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(initialize["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(initialize["params"]["capabilities"], json!({"roots": {}}));

    runtime()
        .block_on(harness.manager.disconnect("fixture"))
        .unwrap();
    assert!(
        harness.manager.handshake("fixture").is_none(),
        "details belong to the live connection"
    );
}

#[test]
fn stdio_rejects_an_unsupported_revision_with_a_clear_error() {
    let harness = StdioHarness::new(
        "badver",
        StdioOptions {
            env: vec![("MCP_PROTO_VERSION", "1999-01-01".into())],
            ..Default::default()
        },
    );
    let error = runtime()
        .block_on(harness.manager.test("fixture"))
        .expect_err("unsupported revision");
    let text = error.to_string();
    assert!(
        text.contains("unsupported protocol version 1999-01-01")
            && text.contains("2025-11-25")
            && text.contains("2024-11-05"),
        "{text}"
    );
    let info = harness.manager.statuses().pop().expect("server");
    assert!(matches!(info.status, McpServerStatus::Failed { .. }));
    assert!(info.handshake.is_none());
    assert!(!harness.methods().contains(&"tools/list".to_owned()));
}

#[test]
fn stdio_skips_tools_list_without_the_tools_capability() {
    let harness = StdioHarness::new(
        "notools",
        StdioOptions {
            env: vec![("MCP_PROTO_CAPS", r#"{"resources":{}}"#.into())],
            ..Default::default()
        },
    );
    assert_eq!(
        runtime().block_on(harness.manager.test("fixture")).unwrap(),
        0
    );
    let methods = harness.methods();
    assert!(methods.contains(&"initialize".to_owned()), "{methods:?}");
    assert!(
        !methods.contains(&"tools/list".to_owned()),
        "tools/list must not be sent: {methods:?}"
    );
    assert!(matches!(
        harness.manager.statuses().pop().unwrap().status,
        McpServerStatus::Ready { .. }
    ));
}

#[test]
fn stdio_answers_roots_list_with_the_workspace_and_ping() {
    let workspace = TempDir::new("workspace");
    let harness = StdioHarness::new(
        "roots",
        StdioOptions {
            workspace: Some(workspace.path().to_path_buf()),
            ..Default::default()
        },
    );
    let reply = runtime()
        .block_on(
            harness
                .manager
                .call("fixture", "ping", json!({"roots": true})),
        )
        .expect("call");
    let answers: Value =
        serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(answers["p1"]["result"], json!({}));
    let root = &answers["r1"]["result"]["roots"][0];
    let uri = root["uri"].as_str().expect("root uri");
    let name = workspace.path().file_name().unwrap().to_string_lossy();
    assert!(uri.starts_with("file:///"), "{uri}");
    assert!(!uri.contains('\\'), "{uri}");
    assert!(uri.ends_with(name.as_ref()), "{uri} vs {name}");
    assert_eq!(root["name"], name.as_ref());
}

#[test]
fn stdio_sends_a_progress_token_matching_the_request_id() {
    let harness = StdioHarness::new("token", StdioOptions::default());
    runtime()
        .block_on(harness.manager.call("fixture", "ping", json!({"x": 1})))
        .unwrap();
    let call = harness.wait_for_method("tools/call");
    assert_eq!(
        call["params"]["_meta"]["progressToken"], call["id"],
        "token must be the request id: {call}"
    );
    assert_eq!(call["params"]["arguments"], json!({"x": 1}));
    // Requests that are not tool calls carry no token.
    let list = harness.wait_for_method("tools/list");
    assert!(list["params"].get("_meta").is_none(), "{list}");
}

#[test]
fn stdio_progress_renews_the_timeout_and_reaches_the_sink() {
    let harness = StdioHarness::new(
        "progress",
        StdioOptions {
            timeout: Some(Duration::from_millis(700)),
            ..Default::default()
        },
    );
    let (sink, updates) = collecting_sink();
    let started = Instant::now();
    let outcome = runtime().block_on(harness.manager.call_with_progress(
        "fixture",
        "ping",
        json!({"progress": {"count": 8, "interval_ms": 200, "message": "step\u{7}\nname"}}),
        McpCancellation::new(),
        Some(sink),
    ));
    assert!(
        matches!(&outcome, McpRequestOutcome::Completed(Ok(_))),
        "a call outliving its timeout must survive on progress: {outcome:?}"
    );
    assert!(
        started.elapsed() > Duration::from_millis(1_400),
        "the call must have outlived the 700ms timeout"
    );
    let updates = updates.lock().unwrap().clone();
    assert_eq!(updates.len(), 8);
    assert_eq!(updates[0].progress, 1.0);
    assert_eq!(updates[7].progress, 8.0);
    assert_eq!(updates[7].total, Some(8.0));
    assert_eq!(updates[0].message.as_deref(), Some("step  name"));
}

#[test]
fn stdio_call_without_a_sink_still_gets_its_timeout_renewed() {
    let harness = StdioHarness::new(
        "progress-nosink",
        StdioOptions {
            timeout: Some(Duration::from_millis(500)),
            ..Default::default()
        },
    );
    let outcome = runtime().block_on(harness.manager.call_cancellable(
        "fixture",
        "ping",
        json!({"progress": {"count": 6, "interval_ms": 200}}),
        McpCancellation::new(),
    ));
    assert!(
        matches!(&outcome, McpRequestOutcome::Completed(Ok(_))),
        "{outcome:?}"
    );
}

#[test]
fn stdio_progress_for_another_token_does_not_renew_the_timeout() {
    let harness = StdioHarness::new(
        "progress-foreign",
        StdioOptions {
            timeout: Some(Duration::from_millis(500)),
            ..Default::default()
        },
    );
    let (sink, updates) = collecting_sink();
    let rt = runtime();
    rt.block_on(harness.manager.test("fixture")).unwrap();
    let started = Instant::now();
    let outcome = rt.block_on(harness.manager.call_with_progress(
        "fixture",
        "ping",
        json!({"progress": {"count": 20, "interval_ms": 100, "token": 424242}}),
        McpCancellation::new(),
        Some(sink),
    ));
    assert!(
        matches!(
            &outcome,
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::TimedOut(timeout),
                ..
            } if *timeout == Duration::from_millis(500)
        ),
        "{outcome:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(1_800));
    assert!(updates.lock().unwrap().is_empty());
}

#[test]
fn stdio_timeout_after_progress_stalls_sends_cancelled_and_stays_uncertain() {
    let harness = StdioHarness::new(
        "progress-stall",
        StdioOptions {
            timeout: Some(Duration::from_millis(500)),
            ..Default::default()
        },
    );
    let started = Instant::now();
    let outcome = runtime().block_on(harness.manager.call_with_progress(
        "fixture",
        "ping",
        json!({"progress": {"count": 2, "interval_ms": 200}, "hang": true}),
        McpCancellation::new(),
        None,
    ));
    assert!(
        matches!(
            &outcome,
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::TimedOut(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(850),
        "the deadline moved with the last progress: {:?}",
        started.elapsed()
    );
    let call = harness.wait_for_method("tools/call");
    let cancelled = harness.wait_for_method("notifications/cancelled");
    assert_eq!(cancelled["params"]["requestId"], call["id"]);
    assert!(cancelled["params"]["reason"]
        .as_str()
        .unwrap()
        .contains("timed out"));
    assert_eq!(
        harness
            .methods()
            .iter()
            .filter(|method| *method == "tools/call")
            .count(),
        1,
        "an uncertain call is never replayed"
    );
}

#[test]
fn stdio_cancellation_sends_cancelled_before_teardown_without_replay() {
    let harness = StdioHarness::new("cancel", StdioOptions::default());
    let rt = runtime();
    let cancellation = McpCancellation::new();
    let call = rt.spawn({
        let manager = Arc::clone(&harness.manager);
        let cancellation = cancellation.clone();
        async move {
            manager
                .call_cancellable("fixture", "ping", json!({"hang": true}), cancellation)
                .await
        }
    });
    let sent = harness.wait_for_method("tools/call");
    cancellation.cancel();
    let outcome = rt.block_on(call).expect("call task");
    assert!(
        matches!(
            outcome,
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                ..
            }
        ),
        "{outcome:?}"
    );
    let cancelled = harness.wait_for_method("notifications/cancelled");
    assert_eq!(cancelled["params"]["requestId"], sent["id"]);
    assert!(cancelled["params"]["reason"]
        .as_str()
        .unwrap()
        .contains("cancelled"));
    assert_eq!(
        harness
            .methods()
            .iter()
            .filter(|method| *method == "tools/call")
            .count(),
        1,
        "an uncertain call is never replayed"
    );
    assert!(matches!(
        harness.manager.statuses().pop().unwrap().status,
        McpServerStatus::Disconnected
    ));
}

#[test]
fn stdio_never_cancels_initialize() {
    let harness = StdioHarness::new(
        "init-hang",
        StdioOptions {
            env: vec![("MCP_PROTO_HANG_INIT", "1".into())],
            timeout: Some(Duration::from_millis(400)),
            ..Default::default()
        },
    );
    let error = runtime()
        .block_on(harness.manager.test("fixture"))
        .expect_err("initialize never answered");
    assert!(
        matches!(
            error,
            McpError::OutcomeUncertain {
                interruption: McpInterruption::TimedOut(_),
                ..
            }
        ),
        "{error:?}"
    );
    harness.wait_for_method("initialize");
    thread::sleep(Duration::from_millis(300));
    assert!(
        !harness
            .methods()
            .contains(&"notifications/cancelled".to_owned()),
        "the spec forbids cancelling initialize"
    );
}

#[test]
fn stdio_server_log_messages_land_redacted_in_the_log_file() {
    let logs = TempDir::new("logs");
    let log = logs.path().join("logs").join("mcp.log");
    let harness = StdioHarness::new(
        "log",
        StdioOptions {
            env: vec![("MCP_API_TOKEN", "tok-s3cr3t-value".into())],
            log: Some(log.clone()),
            ..Default::default()
        },
    );
    runtime()
        .block_on(harness.manager.call(
            "fixture",
            "ping",
            json!({"log": true, "secret": "tok-s3cr3t-value"}),
        ))
        .unwrap();
    let text = read_log(&log);
    assert!(!text.contains("tok-s3cr3t-value"), "{text}");
    assert!(
        text.contains("[fixture] warning db: connected using [REDACTED]"),
        "{text}"
    );
    assert!(text.contains("\n    second line"), "{text}");
    let stamp = text.split_whitespace().next().unwrap();
    assert!(stamp.ends_with('Z') && stamp.contains('T'), "{stamp}");
    assert_eq!(text.lines().count(), 2);
}

#[test]
fn stdio_log_rotates_to_a_numbered_file_past_five_megabytes() {
    let logs = TempDir::new("rotate");
    let log = logs.path().join("mcp.log");
    fs::write(&log, vec![b'x'; 5 * 1024 * 1024 + 1]).unwrap();
    let harness = StdioHarness::new(
        "rotate",
        StdioOptions {
            log: Some(log.clone()),
            ..Default::default()
        },
    );
    runtime()
        .block_on(
            harness
                .manager
                .call("fixture", "ping", json!({"log": true, "secret": "s"})),
        )
        .unwrap();
    let rotated = logs.path().join("mcp.log.1");
    assert_eq!(fs::metadata(&rotated).unwrap().len(), 5 * 1024 * 1024 + 1);
    let fresh = read_log(&log);
    assert!(fresh.contains("[fixture] warning db:"), "{fresh}");
    assert!(fresh.len() < 1024);
}

#[test]
fn stdio_list_changed_notifications_mark_resources_and_tools_stale() {
    let harness = StdioHarness::new(
        "stale",
        StdioOptions {
            env: vec![("MCP_PROTO_CAPS", r#"{"tools":{},"resources":{}}"#.into())],
            ..Default::default()
        },
    );
    let rt = runtime();
    rt.block_on(harness.manager.test("fixture")).unwrap();
    assert!(!harness.manager.take_resources_stale("fixture"));
    rt.block_on(
        harness
            .manager
            .call("fixture", "ping", json!({"list_changed": true})),
    )
    .unwrap();
    assert!(harness.manager.take_resources_stale("fixture"));
    assert!(
        !harness.manager.take_resources_stale("fixture"),
        "the flag is consumed"
    );
    assert!(!harness.manager.take_resources_stale("unknown"));
    // tools/list_changed still forces a catalog refresh on the next listing.
    rt.block_on(harness.manager.list_tools("fixture")).unwrap();
    assert_eq!(
        harness
            .methods()
            .iter()
            .filter(|method| *method == "tools/list")
            .count(),
        2
    );
}

// ---------------------------------------------------------------------------
// Streamable HTTP: raw TCP fixture
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Seen {
    headers: BTreeMap<String, String>,
    body: Value,
}

type Received = Arc<Mutex<Vec<Seen>>>;
type CallHandler = dyn Fn(&Seen, &mut TcpStream, &Received) + Send + Sync;

struct HttpFixture {
    url: String,
    received: Received,
}

impl HttpFixture {
    fn seen(&self) -> Vec<Seen> {
        self.received
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn methods(&self) -> Vec<String> {
        self.seen()
            .iter()
            .filter_map(|seen| seen.body["method"].as_str().map(str::to_owned))
            .collect()
    }

    fn wait_for_method(&self, method: &str) -> Seen {
        poll(Duration::from_secs(10), || {
            self.seen()
                .into_iter()
                .find(|seen| seen.body["method"] == method)
        })
        .unwrap_or_else(|| panic!("fixture never received {method}"))
    }
}

struct HttpScript {
    version: &'static str,
    capabilities: Value,
    hang_initialize: bool,
    on_call: Arc<CallHandler>,
}

impl HttpScript {
    fn new(on_call: impl Fn(&Seen, &mut TcpStream, &Received) + Send + Sync + 'static) -> Self {
        Self {
            version: "2025-11-25",
            capabilities: json!({"tools": {}}),
            hang_initialize: false,
            on_call: Arc::new(on_call),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !line.starts_with("POST ") {
        return None;
    }
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length: usize = headers.get("content-length")?.parse().ok()?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Seen {
        headers,
        body: serde_json::from_slice(&body).ok()?,
    })
}

fn write_json(stream: &mut TcpStream, body: &Value) {
    let body = body.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn write_accepted(stream: &mut TcpStream) {
    let _ = stream
        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn sse_open(stream: &mut TcpStream) {
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

fn sse_event(stream: &mut TcpStream, message: &Value) {
    let _ = write!(stream, "data: {message}\n\n");
    let _ = stream.flush();
}

fn result_of(request: &Seen, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request.body["id"], "result": result})
}

fn spawn_http(script: HttpScript) -> HttpFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let received: Received = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&received);
    let script = Arc::new(script);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let received = Arc::clone(&shared);
            let script = Arc::clone(&script);
            thread::spawn(move || {
                stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let Some(request) = read_request(&mut stream) else {
                    return;
                };
                received
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request.clone());
                match request.body["method"].as_str() {
                    Some("initialize") => {
                        if script.hang_initialize {
                            thread::sleep(Duration::from_secs(5));
                            return;
                        }
                        write_json(
                            &mut stream,
                            &result_of(
                                &request,
                                json!({
                                    "protocolVersion": script.version,
                                    "capabilities": script.capabilities,
                                    "serverInfo": {"name": "http-proto", "version": "3"},
                                    "instructions": "http usage notes",
                                }),
                            ),
                        );
                    }
                    Some("tools/list") => write_json(
                        &mut stream,
                        &result_of(
                            &request,
                            json!({"tools": [{"name": "ping", "inputSchema": {"type": "object"}}]}),
                        ),
                    ),
                    Some("tools/call") => (script.on_call)(&request, &mut stream, &received),
                    _ => write_accepted(&mut stream),
                }
            });
        }
    });
    HttpFixture { url, received }
}

fn http_manager(
    fixture: &HttpFixture,
    timeout: Duration,
    workspace: PathBuf,
    log: Option<PathBuf>,
    headers: BTreeMap<String, String>,
) -> Arc<McpManager> {
    let mut spec = McpServerSpec::new(
        "web",
        McpTransport::Http {
            url: fixture.url.clone(),
            headers,
        },
    );
    spec.timeout = timeout;
    let mut manager = McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        workspace,
        ExecutableResolver::default(),
    );
    if let Some(log) = log {
        manager = manager.with_log_path(log);
    }
    Arc::new(manager)
}

fn echo_call(request: &Seen, stream: &mut TcpStream, _: &Received) {
    write_json(
        stream,
        &result_of(
            request,
            json!({"content": [{"type": "text", "text": "ok"}], "isError": false}),
        ),
    );
}

#[test]
fn http_uses_the_negotiated_revision_in_the_protocol_header() {
    let mut script = HttpScript::new(echo_call);
    script.version = "2025-03-26";
    let fixture = spawn_http(script);
    let workspace = TempDir::new("http-neg");
    let manager = http_manager(
        &fixture,
        Duration::from_secs(5),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let rt = runtime();
    assert_eq!(rt.block_on(manager.test("web")).unwrap(), 1);
    rt.block_on(manager.call("web", "ping", json!({}))).unwrap();

    let handshake = manager.handshake("web").expect("handshake");
    assert_eq!(handshake.protocol_version, "2025-03-26");
    assert_eq!(handshake.server_name.as_deref(), Some("http-proto"));
    assert_eq!(handshake.instructions.as_deref(), Some("http usage notes"));

    let seen = fixture.seen();
    let version_of = |method: &str| {
        seen.iter()
            .find(|seen| seen.body["method"] == method)
            .unwrap_or_else(|| panic!("no {method}"))
            .headers
            .get("mcp-protocol-version")
            .cloned()
    };
    assert_eq!(
        version_of("initialize").as_deref(),
        Some("2025-11-25"),
        "initialize announces the newest revision"
    );
    for method in ["notifications/initialized", "tools/list", "tools/call"] {
        assert_eq!(
            version_of(method).as_deref(),
            Some("2025-03-26"),
            "{method} must use the negotiated revision"
        );
    }
    let initialize = &seen[0].body;
    assert_eq!(initialize["params"]["capabilities"], json!({"roots": {}}));
}

#[test]
fn http_rejects_an_unsupported_revision() {
    let mut script = HttpScript::new(echo_call);
    script.version = "2031-12-31";
    let fixture = spawn_http(script);
    let workspace = TempDir::new("http-bad");
    let manager = http_manager(
        &fixture,
        Duration::from_secs(5),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let error = runtime().block_on(manager.test("web")).expect_err("reject");
    assert!(
        error
            .to_string()
            .contains("unsupported protocol version 2031-12-31"),
        "{error}"
    );
    assert!(!fixture.methods().contains(&"tools/list".to_owned()));
}

#[test]
fn http_skips_tools_list_without_the_tools_capability() {
    let mut script = HttpScript::new(echo_call);
    script.capabilities = json!({"resources": {"listChanged": true}});
    let fixture = spawn_http(script);
    let workspace = TempDir::new("http-notools");
    let manager = http_manager(
        &fixture,
        Duration::from_secs(5),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    assert_eq!(runtime().block_on(manager.test("web")).unwrap(), 0);
    assert!(!fixture.methods().contains(&"tools/list".to_owned()));
    assert!(manager.handshake("web").unwrap().has_resources());
}

#[test]
fn http_answers_roots_list_and_ping_sent_on_the_response_stream() {
    let script = HttpScript::new(|request, stream, received| {
        sse_open(stream);
        sse_event(
            stream,
            &json!({"jsonrpc": "2.0", "id": "r1", "method": "roots/list"}),
        );
        sse_event(
            stream,
            &json!({"jsonrpc": "2.0", "id": "p1", "method": "ping"}),
        );
        sse_event(
            stream,
            &json!({"jsonrpc": "2.0", "id": "s1", "method": "sampling/createMessage", "params": {}}),
        );
        let answers = poll(Duration::from_secs(5), || {
            let answers: BTreeMap<String, Value> = received
                .lock()
                .unwrap()
                .iter()
                .filter(|seen| seen.body.get("method").is_none())
                .filter_map(|seen| Some((seen.body["id"].as_str()?.to_owned(), seen.body.clone())))
                .collect();
            (answers.len() == 3).then_some(answers)
        })
        .unwrap_or_default();
        sse_event(
            stream,
            &result_of(
                request,
                json!({"content": [{"type": "text", "text": json!(answers).to_string()}]}),
            ),
        );
    });
    let fixture = spawn_http(script);
    let workspace = TempDir::new("http-roots");
    let manager = http_manager(
        &fixture,
        Duration::from_secs(10),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let reply = runtime()
        .block_on(manager.call("web", "ping", json!({})))
        .expect("call");
    let answers: Value =
        serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(answers["p1"]["result"], json!({}));
    assert_eq!(answers["s1"]["error"]["code"], -32601);
    let uri = answers["r1"]["result"]["roots"][0]["uri"].as_str().unwrap();
    let name = workspace.path().file_name().unwrap().to_string_lossy();
    assert!(
        uri.starts_with("file:///") && uri.ends_with(name.as_ref()),
        "{uri}"
    );
}

fn hang_call(_: &Seen, _: &mut TcpStream, _: &Received) {
    thread::sleep(Duration::from_secs(8));
}

#[test]
fn http_cancellation_posts_cancelled_with_the_request_id_and_never_replays() {
    let fixture = spawn_http(HttpScript::new(hang_call));
    let workspace = TempDir::new("http-cancel");
    let manager = http_manager(
        &fixture,
        Duration::from_secs(20),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let rt = runtime();
    let cancellation = McpCancellation::new();
    let call = rt.spawn({
        let manager = Arc::clone(&manager);
        let cancellation = cancellation.clone();
        async move {
            manager
                .call_cancellable("web", "ping", json!({}), cancellation)
                .await
        }
    });
    let sent = fixture.wait_for_method("tools/call");
    cancellation.cancel();
    let outcome = rt.block_on(call).unwrap();
    assert!(
        matches!(
            outcome,
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                ..
            }
        ),
        "{outcome:?}"
    );
    let cancelled = fixture.wait_for_method("notifications/cancelled");
    assert_eq!(cancelled.body["params"]["requestId"], sent.body["id"]);
    assert!(cancelled.body["params"]["reason"]
        .as_str()
        .unwrap()
        .contains("cancelled"));
    assert_eq!(
        cancelled
            .headers
            .get("mcp-protocol-version")
            .map(String::as_str),
        Some("2025-11-25")
    );
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "tools/call")
            .count(),
        1
    );
}

#[test]
fn http_timeout_posts_cancelled_and_reports_an_uncertain_outcome() {
    let fixture = spawn_http(HttpScript::new(hang_call));
    let workspace = TempDir::new("http-timeout");
    let manager = http_manager(
        &fixture,
        Duration::from_millis(400),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let outcome = runtime().block_on(manager.call_cancellable(
        "web",
        "ping",
        json!({}),
        McpCancellation::new(),
    ));
    assert!(
        matches!(
            outcome,
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::TimedOut(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    let sent = fixture.wait_for_method("tools/call");
    let cancelled = fixture.wait_for_method("notifications/cancelled");
    assert_eq!(cancelled.body["params"]["requestId"], sent.body["id"]);
    assert!(cancelled.body["params"]["reason"]
        .as_str()
        .unwrap()
        .contains("timed out"));
}

#[test]
fn http_never_cancels_initialize() {
    let mut script = HttpScript::new(echo_call);
    script.hang_initialize = true;
    let fixture = spawn_http(script);
    let workspace = TempDir::new("http-init");
    let manager = http_manager(
        &fixture,
        Duration::from_millis(300),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let error = runtime()
        .block_on(manager.test("web"))
        .expect_err("initialize hangs");
    assert!(
        matches!(
            error,
            McpError::Timeout(_) | McpError::OutcomeUncertain { .. }
        ),
        "{error:?}"
    );
    fixture.wait_for_method("initialize");
    thread::sleep(Duration::from_millis(300));
    assert!(!fixture
        .methods()
        .contains(&"notifications/cancelled".to_owned()));
}

fn progress_call(request: &Seen, stream: &mut TcpStream, _: &Received) {
    let arguments = &request.body["params"]["arguments"];
    let count = arguments["count"].as_u64().unwrap_or(1);
    let interval = Duration::from_millis(arguments["interval_ms"].as_u64().unwrap_or(10));
    let token = arguments
        .get("token")
        .cloned()
        .unwrap_or_else(|| request.body["params"]["_meta"]["progressToken"].clone());
    sse_open(stream);
    for step in 1..=count {
        thread::sleep(interval);
        sse_event(
            stream,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": token, "progress": step, "total": count, "message": "working"},
            }),
        );
    }
    if arguments["hang"] == true {
        thread::sleep(Duration::from_secs(8));
        return;
    }
    sse_event(
        stream,
        &result_of(
            request,
            json!({"content": [{"type": "text", "text": "finished"}], "isError": false}),
        ),
    );
}

#[test]
fn http_progress_on_the_response_stream_renews_the_timeout_and_reaches_the_sink() {
    let fixture = spawn_http(HttpScript::new(progress_call));
    let workspace = TempDir::new("http-progress");
    let manager = http_manager(
        &fixture,
        Duration::from_millis(700),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let (sink, updates) = collecting_sink();
    let started = Instant::now();
    let outcome = runtime().block_on(manager.call_with_progress(
        "web",
        "ping",
        json!({"count": 8, "interval_ms": 200}),
        McpCancellation::new(),
        Some(sink),
    ));
    assert!(
        matches!(&outcome, McpRequestOutcome::Completed(Ok(value)) if value["content"][0]["text"] == "finished"),
        "{outcome:?}"
    );
    assert!(started.elapsed() > Duration::from_millis(1_400));
    let updates = updates.lock().unwrap().clone();
    assert_eq!(updates.len(), 8);
    assert_eq!(updates[7].progress, 8.0);
    assert_eq!(updates[0].message.as_deref(), Some("working"));
    let call = fixture.wait_for_method("tools/call");
    assert_eq!(
        call.body["params"]["_meta"]["progressToken"],
        call.body["id"]
    );
}

#[test]
fn http_progress_for_another_token_does_not_renew_the_timeout() {
    let fixture = spawn_http(HttpScript::new(progress_call));
    let workspace = TempDir::new("http-foreign");
    let manager = http_manager(
        &fixture,
        Duration::from_millis(500),
        workspace.path().to_path_buf(),
        None,
        BTreeMap::new(),
    );
    let (sink, updates) = collecting_sink();
    let rt = runtime();
    rt.block_on(manager.test("web")).unwrap();
    let started = Instant::now();
    let outcome = rt.block_on(manager.call_with_progress(
        "web",
        "ping",
        json!({"count": 20, "interval_ms": 100, "token": 31337}),
        McpCancellation::new(),
        Some(sink),
    ));
    assert!(
        matches!(
            &outcome,
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::TimedOut(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(1_800));
    assert!(updates.lock().unwrap().is_empty());
}

#[test]
fn http_server_logs_land_redacted_in_the_log_file() {
    let script = HttpScript::new(|request, stream, _| {
        sse_open(stream);
        sse_event(
            stream,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/message",
                "params": {"level": "error", "data": "header Bearer hdr-secret-123 rejected"},
            }),
        );
        sse_event(
            stream,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/resources/list_changed",
            }),
        );
        sse_event(
            stream,
            &result_of(
                request,
                json!({"content": [{"type": "text", "text": "ok"}]}),
            ),
        );
    });
    let fixture = spawn_http(script);
    let workspace = TempDir::new("http-log");
    let logs = TempDir::new("http-logs");
    let log = logs.path().join("deep").join("mcp.log");
    let manager = http_manager(
        &fixture,
        Duration::from_secs(5),
        workspace.path().to_path_buf(),
        Some(log.clone()),
        BTreeMap::from([(
            "Authorization".to_owned(),
            "Bearer hdr-secret-123".to_owned(),
        )]),
    );
    let rt = runtime();
    rt.block_on(manager.call("web", "ping", json!({}))).unwrap();
    let text = read_log(&log);
    assert!(!text.contains("hdr-secret-123"), "{text}");
    assert!(
        text.contains("[web] error header [REDACTED] rejected"),
        "{text}"
    );
    assert!(manager.take_resources_stale("web"));
    assert!(!manager.take_resources_stale("web"));
}
