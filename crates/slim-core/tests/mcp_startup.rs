//! Connection timing (M4): enabled, trusted, non-lazy servers connect in the
//! background without blocking the caller; the first model request waits only
//! for direct-exposure servers (bounded); a gateway call waits for the server
//! it names and nothing else. A scriptable HTTP server with slow handshakes
//! stands in for the remote side.

#[path = "../../../tests/support/mcp_http_mock.rs"]
mod mock;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use mock::{serve, Mock, Script};
use serde_json::json;
use slim_core::mcp::{
    McpCancellation, McpExposure, McpManager, McpServerBlock, McpServerSpec, McpServerStatus,
    McpTransport,
};
use slim_core::process::ExecutableResolver;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// A server whose handshake takes `delay` to answer.
fn slow_server(delay: Duration) -> Mock {
    let mut script = Script::new();
    script.init = Box::new(move |_, _, _| {
        thread::sleep(delay);
        true
    });
    serve(script)
}

fn http_spec(name: &str, mock: &Mock) -> McpServerSpec {
    let mut spec = McpServerSpec::new(
        name,
        McpTransport::Http {
            url: mock.url.clone(),
            headers: BTreeMap::new(),
        },
    );
    spec.timeout = Duration::from_secs(10);
    spec
}

fn manager_of(specs: Vec<McpServerSpec>) -> Arc<McpManager> {
    Arc::new(McpManager::new(
        specs
            .into_iter()
            .map(|spec| (spec.name.clone(), spec))
            .collect(),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ))
}

fn status_of(manager: &McpManager, name: &str) -> McpServerStatus {
    manager
        .statuses()
        .into_iter()
        .find(|info| info.name == name)
        .expect("server")
        .status
}

fn is_ready(manager: &McpManager, name: &str) -> bool {
    matches!(status_of(manager, name), McpServerStatus::Ready { .. })
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn only_enabled_trusted_non_lazy_visible_servers_connect_at_session_start() {
    let eager = serve(Script::new());
    let lazy = serve(Script::new());
    let off = serve(Script::new());
    let hidden = serve(Script::new());
    let untrusted = serve(Script::new());
    let broken = serve(Script::new());
    let mut lazy_spec = http_spec("lazy", &lazy);
    lazy_spec.options.lazy = true;
    let mut off_spec = http_spec("off", &off);
    off_spec.enabled = false;
    let mut hidden_spec = http_spec("hidden", &hidden);
    hidden_spec.options.exposure = McpExposure::Hidden;
    let mut untrusted_spec = http_spec("untrusted", &untrusted);
    untrusted_spec.options.block = Some(McpServerBlock::Untrusted);
    let mut broken_spec = http_spec("broken", &broken);
    broken_spec.options.block = Some(McpServerBlock::Invalid("missing variable".into()));
    let manager = manager_of(vec![
        http_spec("eager", &eager),
        lazy_spec,
        off_spec,
        hidden_spec,
        untrusted_spec,
        broken_spec,
    ]);
    let rt = runtime();
    let started = rt.block_on(async { manager.start_background_connect() });
    assert_eq!(started, 1, "only the eager server qualifies");
    wait_until("the eager server", || is_ready(&manager, "eager"));
    thread::sleep(Duration::from_millis(300));
    for (name, mock) in [
        ("lazy", &lazy),
        ("off", &off),
        ("hidden", &hidden),
        ("untrusted", &untrusted),
        ("broken", &broken),
    ] {
        assert!(mock.requests().is_empty(), "{name} must not be contacted");
    }
    assert!(matches!(
        status_of(&manager, "lazy"),
        McpServerStatus::Disconnected
    ));
    assert!(matches!(
        status_of(&manager, "untrusted"),
        McpServerStatus::Untrusted
    ));
    assert!(matches!(
        status_of(&manager, "off"),
        McpServerStatus::Disabled
    ));
}

#[test]
fn starting_returns_before_a_slow_server_answers() {
    let slow = slow_server(Duration::from_millis(700));
    let manager = manager_of(vec![http_spec("slow", &slow)]);
    let rt = runtime();
    let begun = Instant::now();
    rt.block_on(async { manager.start_background_connect() });
    assert!(
        begun.elapsed() < Duration::from_millis(300),
        "starting must not wait for the handshake: {:?}",
        begun.elapsed()
    );
    assert!(!is_ready(&manager, "slow"));
    wait_until("the slow server", || is_ready(&manager, "slow"));
}

#[test]
fn starting_without_a_runtime_is_a_no_op() {
    let mock = serve(Script::new());
    let manager = manager_of(vec![http_spec("web", &mock)]);
    assert_eq!(manager.start_background_connect(), 0);
    thread::sleep(Duration::from_millis(100));
    assert!(mock.requests().is_empty());
}

#[test]
fn starting_again_leaves_connecting_connected_and_failed_servers_alone() {
    let good = slow_server(Duration::from_millis(300));
    let mut bad_script = Script::new();
    bad_script.init = Box::new(|_, _, stream| {
        mock::write_status(stream, 400, "Bad Request");
        false
    });
    let bad = serve(bad_script);
    let manager = manager_of(vec![http_spec("good", &good), http_spec("bad", &bad)]);
    let rt = runtime();
    assert_eq!(rt.block_on(async { manager.start_background_connect() }), 2);
    assert_eq!(
        rt.block_on(async { manager.start_background_connect() }),
        0,
        "still connecting"
    );
    wait_until("both to settle", || {
        is_ready(&manager, "good")
            && matches!(status_of(&manager, "bad"), McpServerStatus::Failed { .. })
    });
    assert_eq!(
        rt.block_on(async { manager.start_background_connect() }),
        0,
        "connected and failed servers are not restarted"
    );
    assert_eq!(bad.count("POST", "initialize"), 1);
}

#[test]
fn a_reload_connects_servers_that_appeared_since() {
    let first = serve(Script::new());
    let second = serve(Script::new());
    let manager = manager_of(vec![http_spec("first", &first)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    wait_until("the first server", || is_ready(&manager, "first"));
    manager.reconcile(BTreeMap::from([
        ("first".to_owned(), http_spec("first", &first)),
        ("second".to_owned(), http_spec("second", &second)),
    ]));
    assert_eq!(rt.block_on(async { manager.start_background_connect() }), 1);
    wait_until("the new server", || is_ready(&manager, "second"));
    assert_eq!(
        first.count("POST", "initialize"),
        1,
        "the first stayed warm"
    );
}

#[test]
fn the_first_wait_covers_direct_servers_only() {
    let direct = slow_server(Duration::from_millis(400));
    let gateway = slow_server(Duration::from_millis(2_500));
    let mut direct_spec = http_spec("direct", &direct);
    direct_spec.options.exposure = McpExposure::Direct;
    let manager = manager_of(vec![direct_spec, http_spec("gateway", &gateway)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    let begun = Instant::now();
    let report = rt.block_on(manager.wait_for_direct_servers(Duration::from_secs(5)));
    let waited = begun.elapsed();
    assert!(report.still_connecting.is_empty(), "{report:?}");
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    assert!(
        waited < Duration::from_millis(1_800),
        "the gateway server is not waited for: {waited:?}"
    );
    assert!(is_ready(&manager, "direct"));
    assert!(!is_ready(&manager, "gateway"));
}

#[test]
fn a_wait_that_times_out_names_the_servers_and_happens_only_once() {
    let direct = slow_server(Duration::from_millis(2_500));
    let mut spec = http_spec("direct", &direct);
    spec.options.exposure = McpExposure::Direct;
    let manager = manager_of(vec![spec]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    let report = rt.block_on(manager.wait_for_direct_servers(Duration::from_millis(150)));
    assert_eq!(report.still_connecting, ["direct"]);
    let begun = Instant::now();
    let again = rt.block_on(manager.wait_for_direct_servers(Duration::from_secs(5)));
    assert!(again.still_connecting.is_empty());
    assert!(
        begun.elapsed() < Duration::from_millis(200),
        "a slow server delays one run, not every run"
    );
}

#[test]
fn a_failed_direct_server_ends_the_wait_early() {
    let mut script = Script::new();
    script.init = Box::new(|_, _, stream| {
        mock::write_status(stream, 400, "Bad Request");
        false
    });
    let broken = serve(script);
    let mut spec = http_spec("direct", &broken);
    spec.options.exposure = McpExposure::Direct;
    let manager = manager_of(vec![spec]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    let begun = Instant::now();
    let report = rt.block_on(manager.wait_for_direct_servers(Duration::from_secs(5)));
    assert!(report.still_connecting.is_empty());
    assert!(begun.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        status_of(&manager, "direct"),
        McpServerStatus::Failed { .. }
    ));
}

#[test]
fn waiting_with_nothing_started_returns_at_once() {
    let mock = serve(Script::new());
    let mut spec = http_spec("direct", &mock);
    spec.options.exposure = McpExposure::Direct;
    let manager = manager_of(vec![spec]);
    let rt = runtime();
    let begun = Instant::now();
    let report = rt.block_on(manager.wait_for_direct_servers(Duration::from_secs(5)));
    assert!(report.still_connecting.is_empty());
    assert!(begun.elapsed() < Duration::from_millis(200));
}

#[test]
fn a_gateway_call_waits_for_its_server_and_does_not_connect_twice() {
    let slow = slow_server(Duration::from_millis(500));
    let manager = manager_of(vec![http_spec("web", &slow)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    let begun = Instant::now();
    let result = rt
        .block_on(manager.call("web", "echo", json!({})))
        .expect("call");
    assert_eq!(result["content"][0]["text"], "ok");
    assert!(begun.elapsed() >= Duration::from_millis(400));
    assert_eq!(
        slow.count("POST", "initialize"),
        1,
        "the call joined the connection in flight"
    );
}

#[test]
fn a_gateway_call_is_not_held_up_by_other_servers() {
    let slow = slow_server(Duration::from_millis(2_500));
    let quick = serve(Script::new());
    let manager = manager_of(vec![http_spec("slow", &slow), http_spec("quick", &quick)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    let begun = Instant::now();
    rt.block_on(manager.call("quick", "echo", json!({})))
        .expect("call");
    assert!(begun.elapsed() < Duration::from_millis(1_500));
}

#[test]
fn shutdown_cancels_connects_still_in_flight() {
    let hung = slow_server(Duration::from_secs(5));
    let mut spec = http_spec("direct", &hung);
    spec.options.exposure = McpExposure::Direct;
    let manager = manager_of(vec![spec]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    hung.wait_for("the handshake to start", |requests| {
        (!requests.is_empty()).then_some(())
    });
    let begun = Instant::now();
    rt.block_on(manager.disconnect_all());
    let report = rt.block_on(manager.wait_for_direct_servers(Duration::from_secs(5)));
    assert!(report.still_connecting.is_empty());
    assert!(begun.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        status_of(&manager, "direct"),
        McpServerStatus::Disconnected
    ));
}

#[test]
fn a_server_removed_during_its_connect_does_not_publish_a_connection() {
    let slow = slow_server(Duration::from_millis(600));
    let manager = manager_of(vec![http_spec("web", &slow)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    slow.wait_for("the handshake to start", |requests| {
        (!requests.is_empty()).then_some(())
    });
    assert!(manager.remove("web"));
    // The handshake completes for nobody: the orphaned session is closed
    // instead of being kept as a connection nobody owns.
    let delete = slow.wait_for("the orphaned session to be closed", |requests| {
        requests
            .iter()
            .find(|request| request.verb == "DELETE")
            .cloned()
    });
    assert_eq!(delete.header("mcp-session-id"), Some("s1"));
    assert!(manager.statuses().is_empty());
}

#[test]
fn a_server_replaced_during_its_connect_does_not_publish_the_old_connection() {
    let old = slow_server(Duration::from_millis(600));
    let replacement = serve(Script::new());
    let manager = manager_of(vec![http_spec("web", &old)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    old.wait_for("the handshake to start", |requests| {
        (!requests.is_empty()).then_some(())
    });
    manager.upsert(http_spec("web", &replacement));
    old.wait_for("the orphaned session to be closed", |requests| {
        requests
            .iter()
            .find(|request| request.verb == "DELETE")
            .cloned()
    });
    assert!(
        matches!(status_of(&manager, "web"), McpServerStatus::Disconnected),
        "the replacement has not connected and must not inherit the old connection"
    );
    assert!(replacement.requests().is_empty());
}

#[test]
fn a_reload_that_drops_a_connecting_server_closes_its_session() {
    let slow = slow_server(Duration::from_millis(600));
    let manager = manager_of(vec![http_spec("web", &slow)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    slow.wait_for("the handshake to start", |requests| {
        (!requests.is_empty()).then_some(())
    });
    manager.reconcile(BTreeMap::new());
    slow.wait_for("the orphaned session to be closed", |requests| {
        requests
            .iter()
            .find(|request| request.verb == "DELETE")
            .cloned()
    });
    assert!(manager.statuses().is_empty());
}

#[test]
fn a_cancelled_call_does_not_cancel_the_background_connect() {
    let slow = slow_server(Duration::from_millis(600));
    let manager = manager_of(vec![http_spec("web", &slow)]);
    let rt = runtime();
    rt.block_on(async { manager.start_background_connect() });
    let cancellation = McpCancellation::new();
    let trigger = cancellation.clone();
    rt.block_on(async {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let outcome = manager
            .call_cancellable("web", "echo", json!({}), cancellation)
            .await;
        assert!(
            !matches!(outcome, slim_core::mcp::McpRequestOutcome::Completed(Ok(_))),
            "{outcome:?}"
        );
    });
    wait_until("the background connect to finish", || {
        is_ready(&manager, "web")
    });
}
