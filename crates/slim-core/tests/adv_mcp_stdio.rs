//! Adversarial coverage for `mcp/stdio.rs` (RODADA 2 — Sifter): newline-
//! delimited JSON-RPC over a real child process. The fixture is this same
//! test binary re-spawned with `--ignored`, mirroring `mcp_manager.rs`; an
//! env knob selects the misbehavior so one fixture covers many cases.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use slim_core::mcp::{McpError, McpManager, McpServerSpec, McpTransport};
use slim_core::process::ExecutableResolver;

/// Mode selected via the `ADV_STDIO_MODE` env var.
const MODES: &[&str] = &[
    "healthy",      // normal result replies
    "null-error",   // replies carry "error": null next to a valid result
    "noise",        // non-JSON log lines interleaved with replies
    "huge-line",    // one >16 MiB stdout line, then silence
    "string-id",    // replies echo the request id as a string
    "exit-on-init", // child exits before answering initialize
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
            ],
            env: BTreeMap::from([("ADV_STDIO_MODE".to_owned(), mode.to_owned())]),
        },
        enabled: true,
        timeout,
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

/// BUG: an initialize response carrying `"error": null` plus a valid result
/// fails the handshake with `server error 0: unknown server error`.
/// `dispatch_inbound` (stdio.rs:423) checks `message.get("error").is_some()`
/// without excluding the null sentinel. Expected: connect succeeds.
/// Actual today: `Err(McpError::Server { code: 0, .. })`.
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

/// BUG: a newline-free line larger than `MAX_MESSAGE_BYTES` (16 MiB,
/// stdio.rs:16) should close the transport promptly, but
/// `JsonLineFramer::push` rescans the whole pending buffer for `b'\n'` on
/// every 8 KiB chunk (stdio.rs:36-63) — an O(n^2) scan. The reader only checks
/// the size bound *before* pushing (stdio.rs:376), so it reaches the break
/// ~33 s late for 17 MiB in debug builds. Every pending request therefore hits
/// its spec timeout instead of a prompt Closed/Protocol failure.
///
/// Evidence: spec timeout 15 s -> `McpError::Timeout(15s)` (deterministic);
/// spec timeout 60 s -> `Protocol("connection closed; stdout noise: running 1
/// test")` only after 32.9 s elapsed.
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
/// timeout.
#[test]
fn stdio_string_id_response_never_resolves_numeric_pending() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let result = runtime.block_on(manager("string-id", Duration::from_secs(3)).test("adv"));
    let error = result.expect_err("string id must not resolve");
    assert!(
        format!("{error:?}").contains("imeout") || format!("{error:?}").contains("losed"),
        "expected Timeout/Closed, got: {error:?}"
    );
}

#[test]
fn stdio_child_exit_before_response_fails_fast() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let result = runtime.block_on(manager("exit-on-init", Duration::from_secs(15)).test("adv"));
    assert!(result.is_err(), "silent child must fail the handshake");
}
