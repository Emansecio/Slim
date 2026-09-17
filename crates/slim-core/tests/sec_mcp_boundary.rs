//! Security reproductions (round 2 audit — role Shroud) for the MCP trust
//! boundary: server-facing connection errors are surfaced to the model via
//! `list_servers`/`mcp error`, but the HTTP transport embeds the configured
//! URL — including query/userinfo credentials — in error text, and the
//! `credential_key` naming convention never registers URL credentials or
//! several common secret variable names for redaction.
//!
//! Evidence:
//! - mcp/http.rs:38,41 `invalid MCP url {url}` / `MCP url must be http(s): {url}`
//! - mcp/http.rs:116,151,181 reqwest errors formatted without `.without_url()`
//!   (contrast provider.rs:2349/2434 which strip the URL)
//! - mcp/manager.rs:502-504 connect error text -> `McpServerStatus::Failed`
//! - mcp/manager.rs:227 `failed: {error}` is model-visible via the `mcp` tool
//! - mcp/spec.rs:57-64 `sensitive_values` covers only env/header values whose
//!   names match `credential_key` (spec.rs:67-86); URL credentials and names
//!   like `PRIVATE_KEY`/`DB_PASS`/`PASSPHRASE` are never registered

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::time::Duration;

use slim_core::mcp::{McpManager, McpServerSpec, McpTransport};
use slim_core::process::ExecutableResolver;

fn http_spec(name: &str, url: &str) -> McpServerSpec {
    McpServerSpec {
        name: name.into(),
        transport: McpTransport::Http {
            url: url.into(),
            headers: BTreeMap::new(),
        },
        enabled: true,
        timeout: Duration::from_secs(2),
    }
}

fn manager_with(spec: McpServerSpec) -> McpManager {
    let mut specs = BTreeMap::new();
    specs.insert(spec.name.clone(), spec);
    McpManager::new(specs, std::env::temp_dir(), ExecutableResolver::default())
}

/// A malformed configured URL is embedded verbatim in the connection error,
/// which `McpServerStatus::Failed` stores and `list_servers` renders for the
/// model. Credentials placed in the URL (query or userinfo) therefore reach
/// the provider and the `/mcp` view unredacted.
#[test]
fn sec_mcp_invalid_url_error_must_not_echo_url_credentials() {
    let secret = "sk-fake-URLcred1";
    let manager = manager_with(http_spec(
        "bad",
        &format!("notaurl://example/mcp?key={secret}"),
    ));
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let error = runtime
        .block_on(manager.list_tools("bad"))
        .expect_err("invalid URL must fail");
    let rendered = error.to_string();
    let listed = manager.list_servers();
    assert!(
        !rendered.contains(secret),
        "McpError echoes URL credential: {rendered}"
    );
    assert!(
        !listed.contains(secret),
        "list_servers exposes URL credential to the model: {listed}"
    );
}

/// A refused connection produces `http request failed: {reqwest error}`; the
/// reqwest Display embeds the full request URL. Unlike the provider client
/// (which formats errors with `.without_url()`), the MCP transport keeps it.
#[test]
fn sec_mcp_transport_error_must_not_embed_url_credentials() {
    let secret = "sk-fake-URLcred2";
    // Guaranteed-closed localhost port: bind, learn the port, drop.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let manager = manager_with(http_spec(
        "refused",
        &format!("http://127.0.0.1:{port}/mcp?key={secret}"),
    ));
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    let error = runtime
        .block_on(manager.list_tools("refused"))
        .expect_err("refused connection must fail");
    let rendered = error.to_string();
    let listed = manager.list_servers();
    assert!(
        !rendered.contains(secret),
        "transport error echoes URL credential: {rendered}"
    );
    assert!(
        !listed.contains(secret),
        "list_servers exposes URL credential to the model: {listed}"
    );
}

/// `credential_key` decides which configured env/header values become
/// registered redaction secrets. Common credential names that do not match
/// the convention (`PRIVATE_KEY`, `DB_PASS`, `PASSPHRASE`, `SESSION_KEY`,
/// `CREDS`) never register, so nothing downstream can redact them: not the
/// stream redactor, not the journal, not `redact_mcp_text`.
#[test]
fn sec_mcp_credential_key_covers_common_secret_names() {
    let mut env = BTreeMap::new();
    env.insert("AUTH_TOKEN".to_owned(), "sk-fake-covered".to_owned());
    env.insert("PRIVATE_KEY".to_owned(), "sk-fake-privkey".to_owned());
    env.insert("DB_PASS".to_owned(), "sk-fake-dbpass".to_owned());
    env.insert("PASSPHRASE".to_owned(), "sk-fake-passphrase".to_owned());
    let spec = McpServerSpec {
        name: "stdio-srv".into(),
        transport: McpTransport::Stdio {
            command: "definitely-not-a-real-binary".into(),
            args: Vec::new(),
            env,
        },
        enabled: true,
        timeout: Duration::from_secs(2),
    };
    let collected: Vec<String> = spec.transport.sensitive_values().cloned().collect();
    assert!(collected.iter().any(|value| value == "sk-fake-covered"));
    for (name, value) in [
        ("PRIVATE_KEY", "sk-fake-privkey"),
        ("DB_PASS", "sk-fake-dbpass"),
        ("PASSPHRASE", "sk-fake-passphrase"),
    ] {
        assert!(
            collected.iter().any(|item| item == value),
            "{name} value is never registered for redaction"
        );
    }
}
