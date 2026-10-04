//! `/mcp login` and `/mcp logout` against a real TUI worker and a loopback
//! mock MCP + authorization server. The user's browser is a fake that either
//! performs the redirect itself or only records the URL (the pasted redirect
//! URL path).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slim_tui::api::{McpServerView, McpStatusView, UiChannels, UiCommand, UiEvent};

use super::{spawn_tui_session, TuiStartup};
use crate::mcp::oauth::oauth_mock as mock;
use crate::oauth::{BrowserLauncher, OAuthEndpoints, OAuthError, OAuthService, OAuthStore};

/// What the fake browser does with the authorization URL.
#[derive(Clone, Copy)]
enum Visit {
    /// Follows the redirect with the code.
    Approve,
    /// Only records the URL.
    Record,
}

struct FakeBrowser {
    visit: Visit,
    opened: Arc<Mutex<Vec<String>>>,
}

impl BrowserLauncher for FakeBrowser {
    fn open(&self, url: &str) -> Result<(), OAuthError> {
        self.opened.lock().unwrap().push(url.to_owned());
        if let Visit::Approve = self.visit {
            let parsed = reqwest::Url::parse(url).expect("authorization URL");
            let query: std::collections::BTreeMap<String, String> =
                parsed.query_pairs().into_owned().collect();
            let redirect = reqwest::Url::parse(&query["redirect_uri"]).expect("redirect uri");
            let address = format!(
                "{}:{}",
                redirect.host_str().unwrap(),
                redirect.port().unwrap()
            );
            let state = query["state"].clone();
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(&address).expect("callback listener");
                let _ = write!(
                    stream,
                    "GET /callback?code=code-1&state={state} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
                );
                let mut sink = String::new();
                let _ = stream.read_to_string(&mut sink);
            });
        }
        Ok(())
    }
}

struct Harness {
    runtime: Option<super::TuiRuntimeHandle>,
    channels: UiChannels,
    root: std::path::PathBuf,
    seen: Vec<UiEvent>,
    opened: Arc<Mutex<Vec<String>>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.runtime.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn harness(label: &str, slim_toml: &str, visit: Visit) -> Harness {
    let root = std::env::temp_dir().join(format!(
        "slim-mcp-login-tui-{label}-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    std::fs::create_dir_all(&root).expect("workspace");
    std::fs::write(root.join("slim.toml"), slim_toml).expect("project config");
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let oauth = OAuthService::new(
        OAuthEndpoints::default(),
        Arc::new(FakeBrowser {
            visit,
            opened: Arc::clone(&opened),
        }),
        OAuthStore::at(root.join("auth.json")),
    )
    .expect("oauth");
    let options = crate::ProviderRunOptions {
        workspace_root: Some(root.clone()),
        trust_project: true,
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
        opened,
    }
}

impl Harness {
    fn send(&self, command: UiCommand) {
        self.channels.commands.send(command).expect("send command");
    }

    fn drain(&mut self) -> bool {
        let batch: Vec<UiEvent> = self
            .channels
            .events
            .try_iter()
            .chain(self.channels.events_data.try_iter())
            .collect();
        let received = !batch.is_empty();
        self.seen.extend(batch);
        received
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

    /// Waits until a notification containing `needle` has been seen.
    fn await_notification(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(found) = self
                .notifications()
                .into_iter()
                .find(|message| message.contains(needle))
            {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for notification {needle:?}; saw {:?}",
                self.notifications()
            );
            if !self.drain() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Waits for the sign-in panel event: `(server, full URL, browser opened)`.
    fn await_authorization(&mut self, seen_before: usize) -> (String, String, bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(found) = self.seen[seen_before..]
                .iter()
                .find_map(|event| match event {
                    UiEvent::McpAuthorization {
                        name,
                        url,
                        browser_opened,
                    } => Some((name.clone(), url.expose().to_owned(), *browser_opened)),
                    _ => None,
                })
            {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the sign-in panel; saw {:?}",
                self.seen
            );
            if !self.drain() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn count_ended(&self, name: &str) -> usize {
        self.seen
            .iter()
            .filter(
                |event| matches!(event, UiEvent::McpLoginEnded { name: ended } if ended == name),
            )
            .count()
    }

    /// Waits until the panel for `name` was told to close.
    fn await_ended(&mut self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.count_ended(name) == 0 {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the end of the sign-in; saw {:?}",
                self.seen
            );
            if !self.drain() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Latest `/mcp` snapshot after asking for a refresh.
    fn snapshot(&mut self) -> Vec<McpServerView> {
        self.drain();
        let before = self.seen.len();
        self.send(UiCommand::McpRefresh);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.drain();
            if let Some(servers) = self.seen[before..]
                .iter()
                .rev()
                .find_map(|event| match event {
                    UiEvent::McpServersChanged { servers } => Some(servers.clone()),
                    _ => None,
                })
            {
                return servers;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a snapshot"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn server(&mut self, name: &str) -> McpServerView {
        self.snapshot()
            .into_iter()
            .find(|server| server.name == name)
            .unwrap_or_else(|| panic!("{name} listed"))
    }

    /// Waits until the server shows `wanted`.
    fn await_status(&mut self, name: &str, wanted: McpStatusView) -> McpServerView {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let server = self.server(name);
            if server.status == wanted {
                return server;
            }
            assert!(
                Instant::now() < deadline,
                "{name} stayed {:?}, wanted {wanted:?}",
                server.status
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn config_for(world: &mock::World) -> String {
    format!(
        "[mcp.servers.web]\nurl = \"{}\"\nlazy = true\n[mcp.servers.local]\ncommand = \"definitely-not-a-real-binary\"\nlazy = true\n",
        world.mcp_url()
    )
}

#[test]
fn login_signs_in_through_the_browser_and_logout_forgets_it() {
    let world = mock::World::new();
    let mut harness = harness("login", &config_for(&world), Visit::Approve);
    assert_eq!(harness.server("web").status, McpStatusView::Disconnected);

    // Connecting without credentials: needs-auth, with the way out.
    harness.send(UiCommand::McpTest { name: "web".into() });
    let web = harness.await_status("web", McpStatusView::NeedsAuth);
    let hint = web.error.expect("a reason");
    assert!(hint.contains("/mcp login web"), "{hint}");
    // Nothing opened a browser by itself.
    assert!(harness.opened.lock().unwrap().is_empty());
    assert_eq!(world.server.count("/as/authorize"), 0);

    let before = harness.seen.len();
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: None,
    });
    // The URL reaches the sign-in panel whole (not a toast), exactly as the
    // browser was asked to open it.
    let (panel_name, panel_url, browser_opened) = harness.await_authorization(before);
    assert_eq!(panel_name, "web");
    assert!(browser_opened);
    harness.await_notification("mcp web: login feito e servidor conectado");
    harness.await_ended("web");
    let web = harness.await_status("web", McpStatusView::Ready);
    assert_eq!(web.tools, Some(1));
    let opened = harness.opened.lock().unwrap().clone();
    assert_eq!(opened, vec![panel_url.clone()]);
    assert!(panel_url.contains("code_challenge="), "{panel_url}");
    assert!(
        !harness
            .notifications()
            .iter()
            .any(|message| message.contains(&panel_url)),
        "the URL no longer rides in a toast"
    );

    harness.send(UiCommand::McpLogout { name: "web".into() });
    harness.await_notification("saiu; credenciais salvas apagadas");
    harness.await_status("web", McpStatusView::NeedsAuth);
}

#[test]
fn a_pasted_redirect_url_completes_a_sign_in_the_browser_could_not() {
    let world = mock::World::new();
    let mut harness = harness("paste", &config_for(&world), Visit::Record);
    let before = harness.seen.len();
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: None,
    });
    let (_, url, _) = harness.await_authorization(before);
    assert_eq!(harness.opened.lock().unwrap()[0], url);
    let query: std::collections::BTreeMap<String, String> = reqwest::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: Some(format!(
            "{}?code=code-1&state={}",
            query["redirect_uri"], query["state"]
        )),
    });
    harness.await_notification("mcp web: login feito e servidor conectado");
    harness.await_ended("web");
    harness.await_status("web", McpStatusView::Ready);
    harness.send(UiCommand::McpLogout { name: "web".into() });
    harness.await_notification("saiu; credenciais salvas apagadas");
}

#[test]
fn login_explains_what_cannot_sign_in_and_what_is_not_running() {
    let world = mock::World::new();
    let mut harness = harness("explain", &config_for(&world), Visit::Record);
    harness.send(UiCommand::McpLogin {
        name: "local".into(),
        redirect_url: None,
    });
    harness.await_notification("sem login OAuth para este servidor");
    harness.send(UiCommand::McpLogin {
        name: "missing".into(),
        redirect_url: None,
    });
    harness.await_notification("mcp missing: sem login OAuth");
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: Some("http://127.0.0.1:1/callback?code=x&state=y".into()),
    });
    harness.await_notification("nenhum login em andamento");
    assert!(harness.opened.lock().unwrap().is_empty());
    harness.send(UiCommand::McpLogout {
        name: "local".into(),
    });
    harness.await_notification("mcp local: sem login OAuth para este servidor");
}

#[test]
fn dismissing_the_panel_cancels_the_sign_in_and_a_newer_one_keeps_the_panel() {
    let world = mock::World::new();
    let mut harness = harness("cancel", &config_for(&world), Visit::Record);

    // Dismissed: the sign-in stops, the panel is told it ended, nothing is
    // reported as a failure.
    let before = harness.seen.len();
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: None,
    });
    harness.await_authorization(before);
    harness.send(UiCommand::McpLoginCancel { name: "web".into() });
    harness.await_ended("web");
    assert!(
        !harness
            .notifications()
            .iter()
            .any(|message| message.contains("falha") || message.contains("tempo")),
        "{:?}",
        harness.notifications()
    );
    assert_eq!(harness.server("web").status, McpStatusView::Disconnected);

    // A second login for the same server replaces the first silently: only
    // the newer one may end the panel.
    let ended_before = harness.count_ended("web");
    let before = harness.seen.len();
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: None,
    });
    harness.await_authorization(before);
    let before = harness.seen.len();
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: None,
    });
    harness.await_authorization(before);
    std::thread::sleep(Duration::from_millis(300));
    harness.drain();
    assert_eq!(
        harness.count_ended("web"),
        ended_before,
        "the replaced sign-in must not close the new panel"
    );
    harness.send(UiCommand::McpLoginCancel { name: "web".into() });
    harness.await_ended("web");
}

#[test]
fn a_pasted_url_that_does_not_fit_warns_and_keeps_the_sign_in_waiting() {
    let world = mock::World::new();
    let mut harness = harness("badpaste", &config_for(&world), Visit::Record);
    let before = harness.seen.len();
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: None,
    });
    harness.await_authorization(before);
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: Some("not a redirect".into()),
    });
    harness.await_notification("mcp web: ");
    // Still waiting: no end, no failure.
    std::thread::sleep(Duration::from_millis(200));
    harness.drain();
    assert_eq!(harness.count_ended("web"), 0);
    harness.send(UiCommand::McpLoginCancel { name: "web".into() });
    harness.await_ended("web");
}
