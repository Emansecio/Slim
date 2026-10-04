//! Streamable HTTP completeness (M3) through `McpManager`: the server-to-
//! client `GET` stream (notifications, server requests, progress), reconnect
//! with `Last-Event-ID`, `DELETE` on close, session expiry and the transient
//! connect retries. A scriptable raw-TCP server plays the remote side.

#[path = "../../../tests/support/mcp_http_mock.rs"]
mod mock;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mock::{
    hold_stream, result_of, serve, sse_event, sse_open, text_result, write_json, write_status,
    Mock, Req, Script,
};
use serde_json::{json, Value};
use slim_core::mcp::{
    McpCancellation, McpError, McpInterruption, McpManager, McpProgress, McpProgressSink,
    McpRequestOutcome, McpServerSpec, McpServerStatus, McpTransport,
};
use slim_core::process::ExecutableResolver;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slim-mcp-http-stream-{}-{unique}",
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
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn poll<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn manager_for(mock: &Mock, timeout: Duration, log: Option<PathBuf>) -> Arc<McpManager> {
    let mut spec = McpServerSpec::new(
        "web",
        McpTransport::Http {
            url: mock.url.clone(),
            headers: BTreeMap::new(),
        },
    );
    spec.timeout = timeout;
    let mut manager = McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    );
    if let Some(log) = log {
        manager = manager.with_log_path(log);
    }
    Arc::new(manager)
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn tool_names(manager: &McpManager, rt: &tokio::runtime::Runtime) -> Vec<String> {
    rt.block_on(manager.list_tools("web"))
        .expect("tools")
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

fn first_get(mock: &Mock) -> Req {
    mock.wait_for("a GET request", |requests| {
        requests
            .iter()
            .find(|request| request.verb == "GET")
            .cloned()
    })
}

// ---------------------------------------------------------------------------
// The server-to-client GET stream
// ---------------------------------------------------------------------------

#[test]
fn the_get_stream_opens_after_initialized_and_serves_notifications_and_requests() {
    let mut script = Script::new();
    script.tools = Box::new(|index| {
        if index == 0 {
            vec!["a"]
        } else {
            vec!["a", "b"]
        }
    });
    script.get = hold_stream();
    let mock = serve(script);
    let dir = TempDir::new();
    let log = dir.path().join("mcp.log");
    let manager = manager_for(&mock, Duration::from_secs(5), Some(log.clone()));
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["a"]);

    let get = first_get(&mock);
    assert_eq!(get.header("accept"), Some("text/event-stream"));
    assert_eq!(get.header("mcp-session-id"), Some("s1"));
    assert_eq!(get.header("mcp-protocol-version"), Some("2025-11-25"));
    assert_eq!(get.header("last-event-id"), None);
    let requests = mock.requests();
    let position = |predicate: &dyn Fn(&Req) -> bool| requests.iter().position(predicate);
    assert!(
        position(&|request| request.is("POST", "notifications/initialized"))
            < position(&|request| request.verb == "GET"),
        "the stream may only open once the server saw notifications/initialized"
    );

    let events = mock.shared();
    events.push_event(&notification(
        "notifications/message",
        json!({"level": "info", "data": "from the stream"}),
    ));
    events.push_event(&notification("notifications/tools/list_changed", json!({})));
    events.push_event(&json!({"jsonrpc": "2.0", "id": "srv-1", "method": "roots/list"}));
    events.push_event(&json!({"jsonrpc": "2.0", "id": "srv-2", "method": "ping"}));
    events
        .push_event(&json!({"jsonrpc": "2.0", "id": "srv-3", "method": "sampling/createMessage"}));

    let answer = |id: &str| {
        let id = id.to_owned();
        mock.wait_for("an answer to a server request", move |requests| {
            requests
                .iter()
                .find(|request| {
                    request.verb == "POST"
                        && request.body["id"] == id
                        && request.body.get("method").is_none()
                })
                .cloned()
        })
    };
    let roots = answer("srv-1");
    let uri = roots.body["result"]["roots"][0]["uri"]
        .as_str()
        .expect("uri");
    assert!(uri.starts_with("file://"), "{uri}");
    assert_eq!(roots.header("mcp-session-id"), Some("s1"));
    assert_eq!(answer("srv-2").body["result"], json!({}));
    assert_eq!(answer("srv-3").body["error"]["code"], -32601);

    // Events are handled in order: the list change was seen, so the next
    // listing refetches instead of serving the cache.
    assert_eq!(tool_names(&manager, &rt), ["a", "b"]);
    let logged = poll("the server log entry", || {
        fs::read_to_string(&log)
            .ok()
            .filter(|text| text.contains("from the stream"))
    });
    assert!(logged.contains("[web] info"), "{logged}");
}

#[test]
fn a_405_means_no_stream_and_is_not_retried() {
    let mock = serve(Script::new());
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    first_get(&mock);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(mock.gets().len(), 1, "405 is final");
    // The connection itself is unaffected.
    rt.block_on(manager.call("web", "echo", json!({})))
        .expect("call");
}

#[test]
fn a_permanent_get_failure_stops_the_stream_but_not_the_connection() {
    let mut script = Script::new();
    script.get = Box::new(|_, stream, _| write_status(stream, 400, "Bad Request"));
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    first_get(&mock);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(mock.gets().len(), 1, "a 4xx other than 404 is not retried");
    rt.block_on(manager.call("web", "echo", json!({})))
        .expect("call");
}

#[test]
fn the_stream_reconnects_with_last_event_id_and_honors_the_retry_field() {
    let mut script = Script::new();
    let gets = Arc::new(AtomicUsize::new(0));
    script.get = Box::new(move |_, stream, shared| {
        let attempt = gets.fetch_add(1, Ordering::SeqCst);
        sse_open(stream);
        if attempt == 0 {
            sse_event(
                stream,
                Some("7"),
                Some(30),
                &notification("notifications/message", json!({"data": "first"})),
            );
            // Returning closes the stream: the client has to reconnect.
        } else {
            sse_event(
                stream,
                Some("8"),
                None,
                &notification("notifications/message", json!({"data": "second"})),
            );
            shared.register_stream(stream);
            thread::sleep(Duration::from_secs(8));
        }
    });
    let mock = serve(script);
    let dir = TempDir::new();
    let log = dir.path().join("mcp.log");
    let manager = manager_for(&mock, Duration::from_secs(5), Some(log.clone()));
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);

    let gets = mock.wait_for("the reconnect", |requests| {
        let gets: Vec<Req> = requests
            .iter()
            .filter(|request| request.verb == "GET")
            .cloned()
            .collect();
        (gets.len() >= 2).then_some(gets)
    });
    assert_eq!(gets[0].header("last-event-id"), None);
    assert_eq!(gets[1].header("last-event-id"), Some("7"));
    assert_eq!(gets[1].header("mcp-session-id"), Some("s1"));
    let gap = gets[1].at.duration_since(gets[0].at);
    assert!(
        gap < Duration::from_millis(900),
        "server retry: 30 replaces the 1 s default, took {gap:?}"
    );
    let logged = poll("both stream messages", || {
        fs::read_to_string(&log)
            .ok()
            .filter(|text| text.contains("first") && text.contains("second"))
    });
    assert!(logged.contains("first"));
}

#[test]
fn a_get_404_closes_the_connection_so_the_next_use_opens_a_new_session() {
    let mut script = Script::new();
    let gets = Arc::new(AtomicUsize::new(0));
    script.get = Box::new(move |_, stream, _| {
        // Only the first session's stream finds its session gone.
        if gets.fetch_add(1, Ordering::SeqCst) == 0 {
            write_status(stream, 404, "Not Found");
        } else {
            write_status(stream, 405, "Method Not Allowed");
        }
    });
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    first_get(&mock);
    thread::sleep(Duration::from_millis(300));

    rt.block_on(manager.call("web", "echo", json!({})))
        .expect("call on the new session");
    assert_eq!(mock.count("POST", "initialize"), 2);
    let call = mock.wait_for("tools/call", |requests| {
        requests
            .iter()
            .find(|request| request.is("POST", "tools/call"))
            .cloned()
    });
    assert_eq!(call.header("mcp-session-id"), Some("s2"));
}

#[test]
fn progress_on_the_get_stream_renews_the_timeout_and_reaches_the_sink() {
    let mut script = Script::new();
    script.get = hold_stream();
    script.call = Box::new(|req, stream, shared| {
        let token = req.body["params"]["_meta"]["progressToken"].clone();
        for step in 1..=3 {
            thread::sleep(Duration::from_millis(250));
            shared.push_event(&notification(
                "notifications/progress",
                json!({"progressToken": token, "progress": step, "total": 3, "message": "step"}),
            ));
        }
        thread::sleep(Duration::from_millis(250));
        write_json(stream, None, &result_of(req, text_result("done")));
    });
    let mock = serve(script);
    // The call takes a full second; each gap is far below the 500 ms timeout.
    let manager = manager_for(&mock, Duration::from_millis(500), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    first_get(&mock);

    let updates: Arc<Mutex<Vec<McpProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&updates);
    let sink: McpProgressSink = Arc::new(move |update| {
        collected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(update);
    });
    let started = Instant::now();
    let outcome = rt.block_on(manager.call_with_progress(
        "web",
        "echo",
        json!({}),
        McpCancellation::new(),
        Some(sink),
    ));
    let McpRequestOutcome::Completed(Ok(result)) = outcome else {
        panic!("progress from the GET stream must keep the call alive: {outcome:?}");
    };
    assert_eq!(result["content"][0]["text"], "done");
    assert!(started.elapsed() > Duration::from_millis(900));
    let updates = updates
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(updates.len(), 3, "{updates:?}");
    assert_eq!(updates[2].progress, 3.0);
    assert_eq!(updates[0].message.as_deref(), Some("step"));
}

#[test]
fn a_dropped_response_stream_resumes_with_last_event_id_without_replaying_the_call() {
    let mut script = Script::new();
    let call_id = Arc::new(AtomicU64::new(0));
    let call_seen = Arc::clone(&call_id);
    script.call = Box::new(move |req, stream, _| {
        call_seen.store(req.body["id"].as_u64().expect("id"), Ordering::SeqCst);
        sse_open(stream);
        sse_event(
            stream,
            Some("1"),
            Some(20),
            &notification(
                "notifications/progress",
                json!({"progressToken": req.body["id"], "progress": 1}),
            ),
        );
        // The stream closes before the response arrives.
    });
    script.get = Box::new(move |req, stream, _| match req.header("last-event-id") {
        Some("1") => {
            sse_open(stream);
            sse_event(
                stream,
                Some("2"),
                None,
                &json!({"jsonrpc": "2.0", "id": call_id.load(Ordering::SeqCst), "result": text_result("resumed")}),
            );
        }
        _ => write_status(stream, 405, "Method Not Allowed"),
    });
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    let result = rt
        .block_on(manager.call("web", "echo", json!({})))
        .expect("resumed response");
    assert_eq!(result["content"][0]["text"], "resumed");
    assert_eq!(
        mock.count("POST", "tools/call"),
        1,
        "resuming is not replay"
    );
    let resume = mock
        .gets()
        .into_iter()
        .find(|request| request.header("last-event-id").is_some())
        .expect("resume GET");
    assert_eq!(resume.header("last-event-id"), Some("1"));
}

// ---------------------------------------------------------------------------
// DELETE on close
// ---------------------------------------------------------------------------

#[test]
fn disconnect_all_deletes_the_session_before_returning() {
    let mock = serve(Script::new());
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    rt.block_on(manager.disconnect_all());
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].header("mcp-session-id"), Some("s1"));
    assert_eq!(
        deletes[0].header("mcp-protocol-version"),
        Some("2025-11-25")
    );
}

#[test]
fn an_unanswered_delete_holds_shutdown_for_about_one_second() {
    let mut script = Script::new();
    script.delete = Box::new(|_, _, _| thread::sleep(Duration::from_secs(6)));
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    let started = Instant::now();
    rt.block_on(manager.disconnect_all());
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(900), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    assert_eq!(mock.deletes().len(), 1);
}

#[test]
fn dropping_the_manager_deletes_the_session_in_the_background() {
    let mock = serve(Script::new());
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    drop(manager);
    let delete = mock.wait_for("the DELETE", |requests| {
        requests
            .iter()
            .find(|request| request.verb == "DELETE")
            .cloned()
    });
    assert_eq!(delete.header("mcp-session-id"), Some("s1"));
}

#[test]
fn a_server_without_sessions_gets_no_delete() {
    let mut script = Script::new();
    script.sessions = false;
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    rt.block_on(manager.disconnect_all());
    drop(manager);
    thread::sleep(Duration::from_millis(300));
    assert!(mock.deletes().is_empty());
}

// ---------------------------------------------------------------------------
// Session expiry
// ---------------------------------------------------------------------------

#[test]
fn an_expired_session_never_replays_tools_call_and_the_next_call_reconnects() {
    let mut script = Script::new();
    script.get = hold_stream();
    let expired = Arc::clone(&script.expired);
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    expired.store(true, Ordering::SeqCst);

    let outcome =
        rt.block_on(manager.call_cancellable("web", "echo", json!({}), McpCancellation::new()));
    assert!(
        matches!(outcome, McpRequestOutcome::OutcomeUncertain { .. }),
        "a call the server may have executed is never reported as a clean failure: {outcome:?}"
    );
    assert_eq!(mock.count("POST", "tools/call"), 1, "never replayed");
    assert_eq!(mock.count("POST", "initialize"), 1);

    // The next call finds the connection closed and opens a new session.
    let result = rt
        .block_on(manager.call("web", "echo", json!({})))
        .expect("call on the new session");
    assert_eq!(result["content"][0]["text"], "ok");
    assert_eq!(mock.count("POST", "initialize"), 2);
    assert_eq!(mock.count("POST", "tools/call"), 2);
    let second = mock
        .requests()
        .into_iter()
        .rfind(|request| request.is("POST", "tools/call"))
        .expect("second call");
    assert_eq!(second.header("mcp-session-id"), Some("s2"));
}

#[test]
fn listing_after_a_session_expiry_recovers_on_a_new_session() {
    let mut script = Script::new();
    script.tools = Box::new(|index| {
        if index == 0 {
            vec!["a"]
        } else {
            vec!["a", "b"]
        }
    });
    script.get = hold_stream();
    let expired = Arc::clone(&script.expired);
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    assert_eq!(tool_names(&manager, &rt), ["a"]);
    first_get(&mock);

    // A list change makes the next listing refetch; the ping behind it tells
    // us the change was processed.
    let events = mock.shared();
    events.push_event(&notification("notifications/tools/list_changed", json!({})));
    events.push_event(&json!({"jsonrpc": "2.0", "id": "sync", "method": "ping"}));
    mock.wait_for("the ping answer", |requests| {
        requests
            .iter()
            .find(|request| request.body["id"] == "sync" && request.body.get("method").is_none())
            .cloned()
    });
    expired.store(true, Ordering::SeqCst);

    assert_eq!(tool_names(&manager, &rt), ["a", "b"]);
    assert_eq!(mock.count("POST", "initialize"), 2, "one new session");
    assert_eq!(
        mock.count("POST", "tools/list"),
        3,
        "the original, the refused one, and exactly one retry"
    );
    let retried = mock
        .requests()
        .into_iter()
        .rfind(|request| request.is("POST", "tools/list"))
        .expect("retried listing");
    assert_eq!(retried.header("mcp-session-id"), Some("s2"));
    // The server-to-client stream follows the new session.
    mock.wait_for("the new session's stream", |requests| {
        requests
            .iter()
            .find(|request| request.verb == "GET" && request.header("mcp-session-id") == Some("s2"))
            .cloned()
    });
}

// ---------------------------------------------------------------------------
// Transient connect failures
// ---------------------------------------------------------------------------

#[test]
fn connect_retries_transient_statuses_and_then_succeeds() {
    let mut script = Script::new();
    script.init = Box::new(|attempt, _, stream| match attempt {
        0 => {
            write_status(stream, 503, "Service Unavailable");
            false
        }
        1 => {
            write_status(stream, 429, "Too Many Requests");
            false
        }
        _ => true,
    });
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    let started = Instant::now();
    assert_eq!(tool_names(&manager, &rt), ["echo"]);
    let waited = started.elapsed();
    assert_eq!(mock.count("POST", "initialize"), 3);
    assert!(
        waited >= Duration::from_millis(1_200),
        "250 ms then 1000 ms between attempts, took {waited:?}"
    );
    assert!(matches!(
        manager.statuses()[0].status,
        McpServerStatus::Ready { .. }
    ));
}

#[test]
fn connect_gives_up_after_two_retries_with_the_last_error() {
    let mut script = Script::new();
    script.init = Box::new(|_, _, stream| {
        write_status(stream, 500, "Internal Server Error");
        false
    });
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    let error = rt
        .block_on(manager.list_tools("web"))
        .expect_err("500 never recovers");
    assert!(
        matches!(error, McpError::Server { code: 500, .. }),
        "{error:?}"
    );
    assert_eq!(mock.count("POST", "initialize"), 3, "1 attempt + 2 retries");
    assert!(matches!(
        manager.statuses()[0].status,
        McpServerStatus::Failed { .. }
    ));
}

#[test]
fn connect_does_not_retry_permanent_failures() {
    for status in [400_u16, 401, 404, 501] {
        let mut script = Script::new();
        script.init = Box::new(move |_, _, stream| {
            write_status(stream, status, "Nope");
            false
        });
        let mock = serve(script);
        let manager = manager_for(&mock, Duration::from_secs(5), None);
        let rt = runtime();
        let started = Instant::now();
        let error = rt
            .block_on(manager.list_tools("web"))
            .expect_err("permanent failure");
        assert!(
            matches!(&error, McpError::Server { code, .. } if *code == i64::from(status)),
            "{status}: {error:?}"
        );
        assert_eq!(mock.count("POST", "initialize"), 1, "{status} is final");
        assert!(started.elapsed() < Duration::from_millis(900), "{status}");
    }
}

#[test]
fn connect_retries_network_failures() {
    // A port nothing listens on: refused connections are transient.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let spec = McpServerSpec::new(
        "web",
        McpTransport::Http {
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: BTreeMap::new(),
        },
    );
    let manager = McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    );
    let rt = runtime();
    let started = Instant::now();
    let error = rt
        .block_on(manager.list_tools("web"))
        .expect_err("nothing listens");
    assert!(matches!(error, McpError::Io(_)), "{error:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(1_200),
        "retried after 250 ms and 1000 ms"
    );
}

#[test]
fn the_wait_between_connect_retries_is_cancellable() {
    let mut script = Script::new();
    script.init = Box::new(|_, _, stream| {
        write_status(stream, 503, "Service Unavailable");
        false
    });
    let mock = serve(script);
    let manager = manager_for(&mock, Duration::from_secs(5), None);
    let rt = runtime();
    let cancellation = McpCancellation::new();
    let trigger = cancellation.clone();
    let started = Instant::now();
    let outcome = rt.block_on(async {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        manager.list_tools_cancellable("web", cancellation).await
    });
    assert!(
        matches!(
            outcome,
            McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(900));
    assert_eq!(mock.count("POST", "initialize"), 1);
}
