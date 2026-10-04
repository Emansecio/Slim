//! `/mcp` against a real TUI worker: project servers start untrusted, the
//! startup notice says so, `/mcp trust` and `/mcp untrust` persist the
//! decision and reload the live manager, and project paths follow the
//! workspace root.

use std::sync::Arc;
use std::time::{Duration, Instant};

use slim_tui::api::{McpServerView, McpStatusView, UiChannels, UiCommand, UiEvent};

use super::{spawn_tui_session, TuiStartup};
use crate::mcp::trust::{TrustDecision, TrustStore};
use crate::oauth::{BrowserLauncher, OAuthEndpoints, OAuthError, OAuthService, OAuthStore};

struct NoBrowser;

impl BrowserLauncher for NoBrowser {
    fn open(&self, _url: &str) -> Result<(), OAuthError> {
        Ok(())
    }
}

struct Harness {
    runtime: Option<super::TuiRuntimeHandle>,
    channels: UiChannels,
    root: std::path::PathBuf,
    seen: Vec<UiEvent>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.runtime.take());
        // The decision is keyed by this temp workspace; forget it.
        if let Ok(store) = TrustStore::default_store() {
            let _ = store.set(&self.root, None);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn harness(label: &str, slim_toml: &str, trust_project: bool) -> Harness {
    let root = std::env::temp_dir().join(format!(
        "slim-mcp-trust-tui-{label}-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    std::fs::create_dir_all(&root).expect("workspace");
    std::fs::write(root.join("slim.toml"), slim_toml).expect("project config");
    let oauth = OAuthService::new(
        OAuthEndpoints::default(),
        Arc::new(NoBrowser),
        OAuthStore::at(root.join("auth.json")),
    )
    .expect("oauth");
    let options = crate::ProviderRunOptions {
        workspace_root: Some(root.clone()),
        trust_project,
        ..crate::ProviderRunOptions::default()
    };
    let mode = slim_core::OperatingMode::Auto;
    let startup = TuiStartup {
        request: Some(crate::ProviderRequest {
            prompt: String::new(),
            mode,
            kind: slim_core::provider::ProviderKind::OpenCodeGo,
            endpoint: slim_core::provider::OPENCODE_GO_BASE_URL.into(),
            model: "deepseek-v4-flash".into(),
            api_key: "fixture-key".into(),
            account_id: None,
            timeout: Duration::from_secs(120),
        }),
        oauth_session: None,
        options,
        initial_prompt: None,
        image_labels: Vec::new(),
        resume_path: None,
        resume_preflight: None,
        pending_session_title: None,
        persist_sessions: false,
        mode,
        effort: super::ReasoningEffort::High,
        endpoint_override: None,
        model_override: None,
        timeout: Duration::from_secs(120),
    };
    let (runtime, channels) = spawn_tui_session(startup, oauth).expect("runtime");
    Harness {
        runtime: Some(runtime),
        channels,
        root,
        seen: Vec::new(),
    }
}

impl Harness {
    fn send(&self, command: UiCommand) {
        self.channels.commands.send(command).expect("send command");
    }

    fn until(&mut self, what: &str, done: impl Fn(&UiEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let mut received = false;
            let batch: Vec<UiEvent> = self
                .channels
                .events
                .try_iter()
                .chain(self.channels.events_data.try_iter())
                .collect();
            // Keep the whole batch: control and data lanes interleave, so an
            // early return would drop events that arrived in the same poll.
            let mut finished = false;
            for event in batch {
                received = true;
                finished |= done(&event);
                self.seen.push(event);
            }
            if finished {
                return;
            }
            if !received {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        panic!("timed out waiting for {what}; saw {:?}", self.seen);
    }

    /// Latest `/mcp` snapshot after asking for a refresh.
    fn snapshot(&mut self) -> Vec<McpServerView> {
        let before = self.seen.len();
        self.send(UiCommand::McpRefresh);
        self.until("an MCP snapshot", |event| {
            matches!(event, UiEvent::McpServersChanged { .. })
        });
        self.seen[before..]
            .iter()
            .rev()
            .find_map(|event| match event {
                UiEvent::McpServersChanged { servers } => Some(servers.clone()),
                _ => None,
            })
            .expect("snapshot")
    }

    /// Waits until a notification containing `needle` has been seen (earlier
    /// ones count: startup notices arrive before the first snapshot).
    fn await_notification(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self
            .notifications()
            .iter()
            .any(|message| message.contains(needle))
        {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for notification {needle:?}; saw {:?}",
                self.seen
            );
            let batch: Vec<UiEvent> = self
                .channels
                .events
                .try_iter()
                .chain(self.channels.events_data.try_iter())
                .collect();
            if batch.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.seen.extend(batch);
        }
    }

    fn notifications(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                UiEvent::Notification { message } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Lazy, so these tests see the trust state change alone: a lazy server stays
/// `Disconnected` until first use whether or not its project is trusted.
const PROJECT_SERVER: &str =
    "[mcp.servers.proj]\ncommand = \"definitely-not-a-real-binary\"\nlazy = true\n";

/// Not lazy: once trusted it connects in the background (and, for lack of a
/// binary, ends `Failed`).
const EAGER_PROJECT_SERVER: &str =
    "[mcp.servers.proj]\ncommand = \"definitely-not-a-real-binary\"\n";

fn status(servers: &[McpServerView], name: &str) -> McpStatusView {
    servers
        .iter()
        .find(|server| server.name == name)
        .unwrap_or_else(|| panic!("{name} listed in {servers:?}"))
        .status
}

#[test]
fn project_servers_start_untrusted_and_the_startup_notice_says_how_to_trust() {
    let mut harness = harness("untrusted", PROJECT_SERVER, false);
    let servers = harness.snapshot();
    assert_eq!(status(&servers, "proj"), McpStatusView::Untrusted);
    harness.await_notification("projeto sem confiança: proj");
    assert!(
        harness
            .notifications()
            .iter()
            .any(|message| message.starts_with("MCP \u{b7} ") && message.ends_with(". Use /mcp")),
        "{:?}",
        harness.notifications()
    );
}

#[test]
fn trust_command_persists_the_decision_and_enables_the_project_servers() {
    let mut harness = harness("trust", PROJECT_SERVER, false);
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Untrusted
    );

    harness.send(UiCommand::McpTrust {
        trust: true,
        name: None,
    });
    harness.until("the trust confirmation", |event| {
        matches!(event, UiEvent::Notification { message } if message.contains("projeto confiável"))
    });
    let store = TrustStore::default_store().expect("store");
    assert_eq!(
        store.decision(&harness.root).expect("decision"),
        Some(TrustDecision::Trusted)
    );
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Disconnected,
        "trusted servers are listed as startable (connect lazily on first use)"
    );

    harness.send(UiCommand::McpTrust {
        trust: false,
        name: None,
    });
    harness.until("the untrust confirmation", |event| {
        matches!(event, UiEvent::Notification { message } if message.contains("ficam desativados"))
    });
    assert_eq!(
        store.decision(&harness.root).expect("decision"),
        Some(TrustDecision::Denied)
    );
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Untrusted
    );
}

#[test]
fn a_denied_workspace_is_remembered_and_gets_no_nag() {
    let mut harness = harness("denied", PROJECT_SERVER, false);
    harness.send(UiCommand::McpTrust {
        trust: false,
        name: None,
    });
    harness.until("the untrust confirmation", |event| {
        matches!(event, UiEvent::Notification { message } if message.contains("ficam desativados"))
    });
    // A later session on this workspace loads with `denied`, which is what
    // suppresses the startup notice.
    let load = crate::mcp::load_mcp(&harness.root, false).expect("load");
    assert!(load.denied);
    assert_eq!(load.untrusted, ["proj"]);
    assert!(crate::mcp::load_diagnostics(&load, "hint").is_empty());
}

#[test]
fn trust_project_flag_runs_project_servers_without_a_stored_decision() {
    let mut harness = harness("flag", PROJECT_SERVER, true);
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Disconnected
    );
    assert!(
        !harness
            .notifications()
            .iter()
            .any(|message| message.contains("projeto sem confiança")),
        "{:?}",
        harness.notifications()
    );
    let store = TrustStore::default_store().expect("store");
    assert_eq!(store.decision(&harness.root).expect("decision"), None);
}

#[test]
fn add_and_remove_edit_the_workspace_project_file_and_merge_existing_entries() {
    let mut harness = harness(
        "add",
        "[mcp.servers.keep]\ncommand = \"keep-cmd\"\nenabled = false\ntimeout_ms = 5000\n",
        true,
    );
    harness.send(UiCommand::McpAdd {
        name: "keep".into(),
        command: Some("keep-cmd".into()),
        args: vec!["--new".into()],
        url: None,
        global: false,
    });
    harness.until("the add confirmation", |event| {
        matches!(event, UiEvent::Notification { message } if message.contains("mcp keep: salvo em"))
    });
    let written = std::fs::read_to_string(harness.root.join("slim.toml")).expect("project file");
    let parsed: toml::Table = written.parse().expect("toml");
    let keep = parsed["mcp"]["servers"]["keep"].as_table().expect("keep");
    assert_eq!(keep["enabled"].as_bool(), Some(false), "kept: {written}");
    assert_eq!(
        keep["timeout_ms"].as_integer(),
        Some(5000),
        "kept: {written}"
    );
    assert_eq!(keep["args"].as_array().map(Vec::len), Some(1));

    // Names that differ only in - and _ would make the config unloadable.
    harness.send(UiCommand::McpAdd {
        name: "ke-ep".into(),
        command: Some("x".into()),
        args: Vec::new(),
        url: None,
        global: false,
    });
    harness.until("the first sibling saved", |event| {
        matches!(event, UiEvent::Notification { message } if message.contains("mcp ke-ep: salvo em"))
    });
    harness.send(UiCommand::McpAdd {
        name: "ke_ep".into(),
        command: Some("x".into()),
        args: Vec::new(),
        url: None,
        global: false,
    });
    harness.until(
        "the collision refusal",
        |event| matches!(event, UiEvent::Notification { message } if message.contains("collides")),
    );
    let written = std::fs::read_to_string(harness.root.join("slim.toml")).expect("project file");
    assert!(!written.contains("ke_ep"), "{written}");

    harness.send(UiCommand::McpRemove {
        name: "keep".into(),
    });
    harness.until("the remove confirmation", |event| {
        matches!(event, UiEvent::Notification { message } if message.contains("mcp keep: removido"))
    });
    let written = std::fs::read_to_string(harness.root.join("slim.toml")).expect("project file");
    assert!(!written.contains("[mcp.servers.keep]"), "{written}");
}

#[test]
fn invalid_project_config_disables_mcp_with_a_notification_instead_of_silence() {
    let mut harness = harness(
        "invalid",
        "[mcp.servers.bad]\ncommand = \"x\"\nurl = \"https://x/mcp\"\n",
        true,
    );
    harness.await_notification("mcp desativado: erro de configuração");
    assert!(harness
        .notifications()
        .iter()
        .any(|message| message.contains("not both")));
}

/// Polls `/mcp` snapshots until `name` reaches `wanted`.
fn await_status(harness: &mut Harness, name: &str, wanted: McpStatusView) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let current = status(&harness.snapshot(), name);
        if current == wanted {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} stuck in {current:?}, wanted {wanted:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn trusting_the_project_connects_its_servers_in_the_background() {
    let mut harness = harness("eager-trust", EAGER_PROJECT_SERVER, false);
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Untrusted
    );
    harness.send(UiCommand::McpTrust {
        trust: true,
        name: None,
    });
    harness.until("the trust confirmation", |event| {
        matches!(event, UiEvent::Notification { message } if message.contains("projeto confiável"))
    });
    // Nobody used the server: its connect attempt still happened.
    await_status(&mut harness, "proj", McpStatusView::Failed);
}

#[test]
fn a_trusted_session_connects_non_lazy_servers_at_start() {
    let mut harness = harness("eager-flag", EAGER_PROJECT_SERVER, true);
    await_status(&mut harness, "proj", McpStatusView::Failed);
}

fn project_file(harness: &Harness) -> toml::Table {
    std::fs::read_to_string(harness.root.join("slim.toml"))
        .expect("project file")
        .parse()
        .expect("toml")
}

#[test]
fn disable_and_enable_write_back_to_the_defining_file_and_keep_the_rest() {
    let mut harness = harness(
        "enable",
        "[mcp.servers.a]\ncommand = \"definitely-not-a-real-binary\"\nlazy = true\ntimeout_ms = 5000\ndescription = \"keep me\"\n[mcp.servers.a.env]\nTOKEN = \"kept-value\"\n[mcp.servers.b]\ncommand = \"definitely-not-a-real-binary\"\nlazy = true\n",
        true,
    );
    assert_eq!(
        status(&harness.snapshot(), "a"),
        McpStatusView::Disconnected
    );

    harness.send(UiCommand::McpEnable {
        name: "a".into(),
        enabled: false,
    });
    harness.await_notification("mcp a: desativado (gravado no slim.toml do projeto)");
    assert_eq!(status(&harness.snapshot(), "a"), McpStatusView::Disabled);
    let written = project_file(&harness);
    let a = written["mcp"]["servers"]["a"].as_table().expect("a");
    assert_eq!(a["enabled"].as_bool(), Some(false));
    assert_eq!(a["timeout_ms"].as_integer(), Some(5000));
    assert_eq!(a["description"].as_str(), Some("keep me"));
    assert_eq!(a["env"]["TOKEN"].as_str(), Some("kept-value"));
    assert!(
        written["mcp"]["servers"]["b"]
            .as_table()
            .expect("b")
            .get("enabled")
            .is_none(),
        "the sibling stays untouched"
    );

    harness.send(UiCommand::McpEnable {
        name: "a".into(),
        enabled: true,
    });
    harness.await_notification("mcp a: ativado (gravado no slim.toml do projeto)");
    assert_eq!(
        status(&harness.snapshot(), "a"),
        McpStatusView::Disconnected
    );
    assert_eq!(
        project_file(&harness)["mcp"]["servers"]["a"]["enabled"].as_bool(),
        Some(true)
    );

    harness.send(UiCommand::McpEnable {
        name: "ghost".into(),
        enabled: false,
    });
    harness.await_notification("mcp ghost: não está definido em nenhum slim.toml");
}

#[test]
fn disabling_needs_no_trust_and_enabling_an_untrusted_server_says_it_still_waits() {
    let mut harness = harness("enable-untrusted", PROJECT_SERVER, false);
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Untrusted
    );
    harness.send(UiCommand::McpEnable {
        name: "proj".into(),
        enabled: false,
    });
    harness.await_notification("mcp proj: desativado");
    assert_eq!(status(&harness.snapshot(), "proj"), McpStatusView::Disabled);
    harness.send(UiCommand::McpEnable {
        name: "proj".into(),
        enabled: true,
    });
    harness.await_notification("projeto sem confiança: /mcp trust para iniciar");
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Untrusted
    );
}

#[test]
fn trust_with_a_name_validates_it_and_still_trusts_the_whole_project() {
    let mut harness = harness("trust-name", PROJECT_SERVER, false);
    harness.snapshot();
    let store = TrustStore::default_store().expect("store");
    harness.send(UiCommand::McpTrust {
        trust: true,
        name: Some("ghost".into()),
    });
    harness.await_notification("servidor ghost não existe na configuração");
    assert_eq!(store.decision(&harness.root).expect("decision"), None);

    harness.send(UiCommand::McpTrust {
        trust: true,
        name: Some("proj".into()),
    });
    harness.await_notification("projeto confiável (proj)");
    assert_eq!(
        store.decision(&harness.root).expect("decision"),
        Some(TrustDecision::Trusted)
    );
    assert_eq!(
        status(&harness.snapshot(), "proj"),
        McpStatusView::Disconnected
    );
}

#[test]
fn one_startup_notice_lists_what_failed_once_the_connections_settle() {
    let mut harness = harness("startup-failed", EAGER_PROJECT_SERVER, true);
    harness.await_notification("falhou: proj");
    // Settled and announced: more snapshots add nothing.
    await_status(&mut harness, "proj", McpStatusView::Failed);
    std::thread::sleep(Duration::from_millis(300));
    harness.snapshot();
    let notices: Vec<String> = harness
        .notifications()
        .into_iter()
        .filter(|message| message.starts_with("MCP \u{b7} "))
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].ends_with(". Use /mcp"), "{}", notices[0]);
    assert!(notices[0].contains("falhou: proj"), "{}", notices[0]);
}

/// A minimal streamable-HTTP MCP server that offers one tool, two resources
/// and one resource template. One request per connection.
fn serve_mcp_with_resources() -> (String, Arc<std::sync::atomic::AtomicBool>) {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            std::thread::spawn(move || {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0_u8; length];
                if length > 0 && stream.read_exact(&mut body).is_err() {
                    return;
                }
                let respond = |stream: &mut std::net::TcpStream, status: &str, body: &str| {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                };
                if !head.starts_with("post ") {
                    respond(&mut stream, "405 Method Not Allowed", "");
                    return;
                }
                let message: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let Some(id) = message.get("id").cloned() else {
                    respond(&mut stream, "202 Accepted", "");
                    return;
                };
                let result = match message["method"].as_str().unwrap_or("") {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {"tools": {}, "resources": {}},
                        "serverInfo": {"name": "fixture", "version": "1"}
                    }),
                    "tools/list" => serde_json::json!({"tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object"}
                    }]}),
                    "resources/list" => serde_json::json!({"resources": [
                        {"uri": "file:///a.txt", "name": "a.txt"},
                        {"uri": "file:///b.txt", "name": "b.txt"}
                    ]}),
                    "resources/templates/list" => serde_json::json!({"resourceTemplates": [
                        {"uriTemplate": "file:///{path}", "name": "files"}
                    ]}),
                    _ => serde_json::json!({}),
                };
                respond(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
                );
            });
        }
    });
    (url, stop)
}

#[test]
fn testing_a_server_fills_its_resource_counts_and_exposure_shows_in_the_view() {
    let (url, stop) = serve_mcp_with_resources();
    let mut harness = harness(
        "counts",
        &format!(
            "[mcp.servers.web]\nurl = \"{url}\"\nlazy = true\nexposure = \"direct\"\n[mcp.servers.hid]\ncommand = \"definitely-not-a-real-binary\"\nlazy = true\nexposure = \"hidden\"\n"
        ),
        true,
    );
    let servers = harness.snapshot();
    let web = servers.iter().find(|s| s.name == "web").expect("web");
    assert_eq!(
        (web.resources, web.resource_templates, web.tools),
        (None, None, None)
    );
    assert_eq!(web.exposure, "direct");
    assert_eq!(
        servers
            .iter()
            .find(|s| s.name == "hid")
            .expect("hid")
            .exposure,
        "hidden"
    );

    harness.send(UiCommand::McpTest { name: "web".into() });
    harness.await_notification("mcp web: ok \u{2014} 1 ferramenta(s), 2 recurso(s)");
    let servers = harness.snapshot();
    let web = servers.iter().find(|s| s.name == "web").expect("web");
    assert_eq!(web.status, McpStatusView::Ready);
    assert_eq!(web.tools, Some(1));
    assert_eq!(web.resources, Some(2));
    assert_eq!(web.resource_templates, Some(1));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn reconnecting_a_server_also_fills_its_resource_counts() {
    let (url, stop) = serve_mcp_with_resources();
    let mut harness = harness(
        "counts-reconnect",
        &format!("[mcp.servers.web]\nurl = \"{url}\"\nlazy = true\n"),
        true,
    );
    harness.send(UiCommand::McpReconnect { name: "web".into() });
    harness.await_notification("mcp web: reconectado");
    let servers = harness.snapshot();
    let web = servers.iter().find(|s| s.name == "web").expect("web");
    assert_eq!(web.status, McpStatusView::Ready);
    assert_eq!(web.resources, Some(2));
    assert_eq!(web.resource_templates, Some(1));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}
