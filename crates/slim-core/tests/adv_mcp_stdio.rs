//! Adversarial coverage for `mcp/stdio.rs` (RODADA 2 — Sifter): newline-
//! delimited JSON-RPC over a real child process. The fixture is this same
//! test binary re-spawned with `--ignored`, mirroring `mcp_manager.rs`; an
//! env knob selects the misbehavior so one fixture covers many cases.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use slim_core::mcp::{
    McpCleanupStatus, McpError, McpInterruption, McpManager, McpServerSpec, McpTransport,
};
use slim_core::process::ExecutableResolver;

/// Mode selected via the `ADV_STDIO_MODE` env var.
const MODES: &[&str] = &[
    "healthy",       // normal result replies
    "null-error",    // replies carry "error": null next to a valid result
    "noise",         // non-JSON log lines interleaved with replies
    "huge-line",     // one >16 MiB stdout line, then silence
    "string-id",     // replies echo the request id as a string
    "exit-on-init",  // child exits before answering initialize
    "crash-on-init", // child prints a diagnostic to stderr, then exits
];

#[test]
#[ignore = "subprocess fixture"]
#[allow(clippy::while_let_on_iterator)]
fn adv_stdio_fixture() {
    use std::io::{BufRead, Write};
    let mode = std::env::var("ADV_STDIO_MODE").unwrap_or_else(|_| "healthy".into());
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
    if mode == "huge-line" {
        // One over-limit stdout line with no newline: the reader must trip
        // MAX_MESSAGE_BYTES and close the transport.
        let mut out = stdout.lock();
        out.write_all(&vec![b'x'; 17 * 1024 * 1024]).expect("huge");
        out.write_all(b"\n").expect("nl");
        out.flush().expect("flush");
        return;
    }
    while let Some(line) = lines.next() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if mode == "exit-on-init" {
            return;
        }
        if mode == "crash-on-init" {
            eprintln!("fixture exploded: bad configuration");
            return;
        }
        if mode == "noise" {
            let mut out = stdout.lock();
            out.write_all(b"fixture log line, not json\n")
                .expect("noise");
            out.flush().expect("flush");
            drop(out);
        }
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let result = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "adv", "version": "0"},
            }),
            Some("tools/list") => json!({"tools": []}),
            _ => continue,
        };
        let reply = match mode.as_str() {
            "null-error" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
                "error": null,
            }),
            "string-id" => json!({
                "jsonrpc": "2.0",
                "id": id.as_u64().map(|v| v.to_string()).unwrap_or_default(),
                "result": result,
            }),
            _ => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        };
        write_line(&reply);
    }
}

fn manager(mode: &str, timeout: Duration) -> Arc<McpManager> {
    let exe = std::env::current_exe().expect("current exe");
    assert!(MODES.contains(&mode));
    let spec = McpServerSpec {
        name: "adv".into(),
        transport: McpTransport::Stdio {
            command: exe.to_string_lossy().into_owned(),
            args: vec![
                "--exact".into(),
                "adv_stdio_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            env: BTreeMap::from([("ADV_STDIO_MODE".to_owned(), mode.to_owned())]),
        },
        enabled: true,
        timeout,
        options: Default::default(),
    };
    Arc::new(McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ))
}

#[test]
fn stdio_healthy_fixture_connects() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    assert_eq!(
        runtime
            .block_on(manager("healthy", Duration::from_secs(15)).test("adv"))
            .expect("connect"),
        0
    );
}

/// Regression: an initialize response carrying `"error": null` plus a valid
/// result must complete the handshake; the null member is not a server error.
#[test]
fn stdio_null_error_member_completes_connect() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let result = runtime.block_on(manager("null-error", Duration::from_secs(15)).test("adv"));
    assert_eq!(result.expect("null error must not fail"), 0);
}

#[test]
fn stdio_noise_lines_between_frames_are_tolerated() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    assert_eq!(
        runtime
            .block_on(manager("noise", Duration::from_secs(15)).test("adv"))
            .expect("noise must be skipped"),
        0
    );
}

/// Regression: a newline-free line larger than `MAX_MESSAGE_BYTES` (16 MiB)
/// must close the transport promptly, so the pending request fails with a
/// Closed/Protocol error instead of waiting out its spec timeout. (It once
/// took ~33 s in debug builds because of an O(n^2) rescan; it now fails in
/// well under a second.)
#[test]
fn stdio_over_limit_line_fails_the_connection() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let result = runtime.block_on(manager("huge-line", Duration::from_secs(15)).test("adv"));
    let error = result.expect_err("over-limit line must fail");
    assert!(
        !matches!(error, McpError::Timeout(_)),
        "over-limit line must fail the transport before the request timeout, got: {error:?}"
    );
}

/// Documents strict id typing on stdio: a response id echoed as a string does
/// not satisfy the numeric pending id and the request stalls until the spec
/// timeout. `initialize` was already written, so the deadline classifies the
/// interruption as an uncertain outcome that closes the transport, not as a
/// pre-send timeout.
#[test]
fn stdio_string_id_response_never_resolves_numeric_pending() {
    // Long enough for the request to be written before its deadline; the
    // fixture never answers with a matching id, so the wait is the timeout.
    let timeout = Duration::from_millis(500);
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let result = runtime.block_on(manager("string-id", timeout).test("adv"));
    let error = result.expect_err("string id must not resolve");
    assert!(
        matches!(
            &error,
            McpError::OutcomeUncertain {
                interruption: McpInterruption::TimedOut(elapsed),
                cleanup: McpCleanupStatus::Confirmed | McpCleanupStatus::Unconfirmed,
            } if *elapsed == timeout
        ),
        "expected the sent request to stay pending until its deadline, got: {error:?}"
    );
}

#[test]
fn stdio_child_exit_before_response_fails_fast() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let result = runtime.block_on(manager("exit-on-init", Duration::from_secs(15)).test("adv"));
    assert!(result.is_err(), "silent child must fail the handshake");
}

/// A server that crashes during the handshake is a failed connect that says
/// why (its stderr tail), not a silent `Disconnected`.
#[test]
fn stdio_crash_during_handshake_is_failed_with_the_stderr_tail() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let manager = manager("crash-on-init", Duration::from_secs(15));
    let error = runtime
        .block_on(manager.test("adv"))
        .expect_err("crashing child must fail the handshake");
    assert!(
        error.to_string().contains("fixture exploded"),
        "error lost the stderr tail: {error}"
    );
    match &manager.statuses()[0].status {
        slim_core::mcp::McpServerStatus::Failed { error } => {
            assert!(error.contains("fixture exploded"), "{error}");
        }
        other => panic!("expected failed with the reason, got {other:?}"),
    }
}
