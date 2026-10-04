//! Worker side of the `/mcp` manager beyond sign-in (`mcp_login.rs`):
//! enable/disable written back to the config file that defines the server,
//! the project trust decision, resource-count priming and the one startup
//! notice (DESIGN-SLIM-TUI §15.7.2).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slim_core::mcp::{
    McpCancellation, McpManager, McpRequestOutcome, McpServerInfo, McpServerStatus,
};
use slim_tui::api::{UiCommand, UiEvent};

use super::{mcp_server_views, mcp_workspace, redact_mcp_text, reload_mcp, EventSink, TuiStartup};
use crate::config::{self, FileMcpServerConfig};

/// How long the startup notice waits for background connections to settle
/// before listing what already failed.
const STARTUP_NOTICE_WAIT: Duration = Duration::from_secs(15);
const STARTUP_NOTICE_POLL: Duration = Duration::from_millis(100);
/// Names listed per group in the startup notice.
const STARTUP_NOTICE_NAMES: usize = 5;

fn notify(sink: &EventSink, message: String) {
    let _ = sink.send(UiEvent::Notification { message });
}

fn publish(manager: &McpManager, seen_revision: &AtomicU64, sink: &EventSink) {
    seen_revision.store(manager.revision(), Ordering::Relaxed);
    let _ = sink.send(UiEvent::McpServersChanged {
        servers: mcp_server_views(manager),
    });
}

/// The config file that defines `name`: the workspace's project file when it
/// does, else the global one. Returns the path and how to name it to the
/// user.
fn defining_file(workspace: &Path, name: &str) -> Result<(PathBuf, &'static str), String> {
    defining_file_in(workspace, config::global_config_path(), name)
}

fn defining_file_in(
    workspace: &Path,
    global: Option<PathBuf>,
    name: &str,
) -> Result<(PathBuf, &'static str), String> {
    let project = config::project_config_path(workspace);
    if config::mcp_server_defined_in(&project, name)? {
        return Ok((project, "slim.toml do projeto"));
    }
    if let Some(global) = global {
        if config::mcp_server_defined_in(&global, name)? {
            return Ok((global, "slim.toml global"));
        }
    }
    Err("não está definido em nenhum slim.toml (nada a gravar)".into())
}

/// What a run in progress answers to `/mcp` actions that change servers.
const RUN_ACTIVE_NOTICE: &str = "Aguarde ou cancele a execução antes de alterar servidores MCP";

/// Whether `command` changes MCP state (servers, trust, sign-in) and so must
/// wait for the run that owns the manager. Read-only commands and finishing
/// or dismissing a sign-in already waiting for the browser are not mutations.
pub(super) fn is_mutation(command: &UiCommand) -> bool {
    matches!(
        command,
        UiCommand::McpTest { .. }
            | UiCommand::McpReconnect { .. }
            | UiCommand::McpDisconnect { .. }
            | UiCommand::McpRemove { .. }
            | UiCommand::McpAdd { .. }
            | UiCommand::McpTrust { .. }
            | UiCommand::McpEnable { .. }
            | UiCommand::McpLogout { .. }
            | UiCommand::McpLogin {
                redirect_url: None,
                ..
            }
    )
}

/// Says why a mutating `/mcp` action was not carried out.
pub(super) fn refuse_mutation(sink: &EventSink) {
    notify(sink, RUN_ACTIVE_NOTICE.to_owned());
}

/// `a` / `/mcp enable|disable <name>`: writes `enabled` into the file that
/// defines the server (merging into its entry, everything else untouched) and
/// reloads the live manager.
pub(super) fn set_enabled(
    startup: &mut TuiStartup,
    mcp_manager: &mut Option<Arc<McpManager>>,
    seen_revision: &AtomicU64,
    sink: &EventSink,
    name: &str,
    enabled: bool,
) {
    let workspace = mcp_workspace(startup);
    let verb = if enabled { "ativado" } else { "desativado" };
    let message = match defining_file(&workspace, name).and_then(|(path, layer)| {
        // A project entry that only disables a global server is lifted by
        // deleting it: `enabled = true` would make it a project definition
        // that needs the workspace to be trusted.
        if enabled
            && path == config::project_config_path(&workspace)
            && config::clear_mcp_disable_override_to(&path, name)?
        {
            return Ok(layer);
        }
        let update = FileMcpServerConfig {
            enabled: Some(enabled),
            ..FileMcpServerConfig::default()
        };
        config::upsert_mcp_server_to(&path, name, &update).map(|()| layer)
    }) {
        Ok(layer) => match reload_mcp(startup, mcp_manager) {
            Ok(load) => {
                let mut message = format!("mcp {name}: {verb} (gravado no {layer})");
                if enabled && load.untrusted.contains(&name.to_owned()) && !load.denied {
                    message.push_str(" \u{b7} projeto sem confiança: /mcp trust para iniciar");
                }
                message
            }
            Err(error) => format!(
                "mcp {name}: {verb} no {layer}, mas recarregar a configuração falhou: {error}"
            ),
        },
        Err(error) => format!("mcp {name}: {error}"),
    };
    if let Some(manager) = mcp_manager.as_ref() {
        publish(manager, seen_revision, sink);
    }
    notify(sink, message);
}

/// `t` / `/mcp trust|untrust [name]`: records the project decision and
/// reloads. The decision covers the whole project; `name` is validated and
/// echoed so the notice says what the user pointed at.
pub(super) fn set_trust(
    startup: &mut TuiStartup,
    mcp_manager: &mut Option<Arc<McpManager>>,
    seen_revision: &AtomicU64,
    sink: &EventSink,
    trust: bool,
    name: Option<&str>,
) {
    if let Some(name) = name {
        let known = mcp_manager
            .as_ref()
            .is_some_and(|manager| manager.statuses().iter().any(|info| info.name == name));
        if !known {
            notify(
                sink,
                format!(
                    "mcp {}: servidor {name} não existe na configuração",
                    if trust { "trust" } else { "untrust" }
                ),
            );
            return;
        }
    }
    let workspace = mcp_workspace(startup);
    let decision = if trust {
        crate::mcp::trust::TrustDecision::Trusted
    } else {
        crate::mcp::trust::TrustDecision::Denied
    };
    let message = match crate::mcp::set_project_trust(&workspace, Some(decision)) {
        Ok(_) => {
            let outcome = reload_mcp(startup, mcp_manager);
            if let Some(manager) = mcp_manager.as_ref() {
                publish(manager, seen_revision, sink);
            }
            let subject = name.map_or_else(String::new, |name| format!(" ({name})"));
            match (outcome, trust) {
                (Ok(_), true) => format!(
                    "mcp: projeto confiável{subject}; vale para todos os servidores do slim.toml do projeto, que conectam em segundo plano (os lazy no primeiro uso)"
                ),
                (Ok(_), false) => format!(
                    "mcp: projeto marcado como nunca confiar{subject}; os servidores do slim.toml do projeto ficam desativados (/mcp trust para permitir)"
                ),
                (Err(error), _) => format!(
                    "mcp: decisão de confiança salva, mas recarregar a configuração falhou: {error}"
                ),
            }
        }
        Err(error) => format!("mcp trust: {error}"),
    };
    notify(sink, message);
}

/// After a successful test or reconnect: fills the manager's resource-list
/// caches (when the server offers resources) so `/mcp` can show their counts.
/// Best effort: a refusal or failure just leaves the counts unknown.
pub(super) async fn prime_resources(manager: &McpManager, name: &str) -> Option<usize> {
    let offers = manager
        .handshake(name)
        .is_some_and(|handshake| handshake.has_resources());
    if !offers {
        return None;
    }
    let count = match manager
        .all_resources_cancellable(name, McpCancellation::new())
        .await
    {
        McpRequestOutcome::Completed(Ok(listing)) => Some(listing.items.len()),
        _ => None,
    };
    // Templates are optional for servers; ignore the outcome.
    let _ = manager
        .all_resource_templates_cancellable(name, McpCancellation::new())
        .await;
    count
}

fn listed_names(names: &[String]) -> String {
    let shown: Vec<String> = names
        .iter()
        .take(STARTUP_NOTICE_NAMES)
        .map(|name| name.chars().filter(|ch| !ch.is_control()).collect())
        .collect();
    let mut text = shown.join(", ");
    if names.len() > STARTUP_NOTICE_NAMES {
        text.push_str(&format!(" +{}", names.len() - STARTUP_NOTICE_NAMES));
    }
    text
}

/// The one notice of a session start: what failed, what needs a login and
/// what waits for the project to be trusted. `None` when all is well.
pub(super) fn startup_notice(
    servers: &[McpServerInfo],
    denied: bool,
    trust_unreadable: bool,
) -> Option<String> {
    let mut failed = Vec::new();
    let mut needs_auth = Vec::new();
    let mut untrusted = Vec::new();
    for info in servers.iter().filter(|info| info.enabled) {
        match info.status {
            McpServerStatus::Failed { .. } => failed.push(info.name.clone()),
            McpServerStatus::NeedsAuth { .. } => needs_auth.push(info.name.clone()),
            // "never" was the user's decision: nothing to announce.
            McpServerStatus::Untrusted if !denied => untrusted.push(info.name.clone()),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if !failed.is_empty() {
        parts.push(format!("falhou: {}", listed_names(&failed)));
    }
    if !needs_auth.is_empty() {
        parts.push(format!("requer login: {}", listed_names(&needs_auth)));
    }
    if !untrusted.is_empty() {
        parts.push(format!(
            "projeto sem confiança: {}",
            listed_names(&untrusted)
        ));
    }
    if trust_unreadable {
        parts.push("confiança do projeto indisponível".into());
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("MCP \u{b7} {}. Use /mcp", parts.join(" \u{b7} ")))
    }
}

/// Whether connections are still being established. A background connect is
/// pending from the moment it is scheduled but only shows as `Connecting`
/// once its task runs, so `Connecting` alone can report "settled" too early.
fn settling(manager: &McpManager, servers: &[McpServerInfo]) -> bool {
    manager.startup_in_progress()
        || servers
            .iter()
            .any(|info| matches!(info.status, McpServerStatus::Connecting))
}

/// Sends the startup notice once the background connections settle (or after
/// [`STARTUP_NOTICE_WAIT`], listing only what already ended badly).
pub(super) fn spawn_startup_notice(
    manager: Arc<McpManager>,
    sink: EventSink,
    denied: bool,
    trust_unreadable: bool,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + STARTUP_NOTICE_WAIT;
        loop {
            let servers = manager.statuses();
            if !settling(&manager, &servers) || tokio::time::Instant::now() >= deadline {
                if let Some(notice) = startup_notice(&servers, denied, trust_unreadable) {
                    notify(&sink, redact_mcp_text(&manager, &notice));
                }
                return;
            }
            tokio::time::sleep(STARTUP_NOTICE_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, status: McpServerStatus) -> McpServerInfo {
        McpServerInfo {
            name: name.into(),
            transport: "stdio",
            target: "cmd".into(),
            enabled: true,
            description: None,
            exposure: slim_core::mcp::McpExposure::Gateway,
            status,
            handshake: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_scheduled_connect_counts_as_settling_before_its_task_marks_connecting() {
        let mut spec = slim_core::mcp::McpServerSpec::new(
            "bad",
            slim_core::mcp::McpTransport::Stdio {
                command: "definitely-not-a-real-mcp-binary-xyz".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        );
        spec.timeout = Duration::from_secs(5);
        let manager = Arc::new(McpManager::new(
            std::collections::BTreeMap::from([("bad".to_owned(), spec)]),
            PathBuf::from("."),
            Default::default(),
        ));
        assert_eq!(manager.start_background_connect(), 1);
        // The connect task has not run yet: nothing says `Connecting` ...
        let servers = manager.statuses();
        assert!(servers
            .iter()
            .all(|info| matches!(info.status, McpServerStatus::Disconnected)));
        // ... yet the startup notice must not decide on this snapshot.
        assert!(settling(&manager, &servers));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while settling(&manager, &manager.statuses()) {
            assert!(tokio::time::Instant::now() < deadline, "never settled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let notice = startup_notice(&manager.statuses(), false, false).expect("failed server");
        assert!(notice.contains("falhou: bad"), "{notice}");
    }

    #[test]
    fn startup_notice_lists_each_group_and_ends_with_the_pointer() {
        let servers = vec![
            info("ok", McpServerStatus::Disconnected),
            info(
                "bad",
                McpServerStatus::Failed {
                    error: "boom".into(),
                },
            ),
            info("web", McpServerStatus::NeedsAuth { reason: "x".into() }),
            info("proj", McpServerStatus::Untrusted),
        ];
        let notice = startup_notice(&servers, false, false).expect("notice");
        assert_eq!(
            notice,
            "MCP \u{b7} falhou: bad \u{b7} requer login: web \u{b7} projeto sem confiança: proj. Use /mcp"
        );
    }

    #[test]
    fn startup_notice_is_silent_when_all_is_well_or_the_user_said_never() {
        let servers = vec![
            info("ok", McpServerStatus::Disconnected),
            info("proj", McpServerStatus::Untrusted),
        ];
        assert_eq!(startup_notice(&servers[..1], false, false), None);
        assert_eq!(startup_notice(&servers, true, false), None);
        let mut disabled = info("off", McpServerStatus::Failed { error: "x".into() });
        disabled.enabled = false;
        assert_eq!(startup_notice(&[disabled], false, false), None);
    }

    #[test]
    fn startup_notice_caps_names_and_strips_control_characters() {
        let servers: Vec<McpServerInfo> = (0..7)
            .map(|index| {
                info(
                    &format!("s{index}\u{1b}[31m"),
                    McpServerStatus::Failed { error: "x".into() },
                )
            })
            .collect();
        let notice = startup_notice(&servers, false, false).expect("notice");
        assert!(notice.contains(" +2"), "{notice}");
        assert!(!notice.contains('\u{1b}'), "{notice:?}");
        assert!(notice.ends_with("Use /mcp"));
    }

    #[test]
    fn unreadable_trust_store_is_mentioned() {
        let notice = startup_notice(&[], false, true).expect("notice");
        assert!(
            notice.contains("confiança do projeto indisponível"),
            "{notice}"
        );
    }
}

#[cfg(test)]
mod defining_file_tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slim-mcp-defining-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn the_project_file_wins_when_it_defines_the_server_else_the_global_one() {
        let root = temp_dir("pick");
        let global = root.join("global.toml");
        std::fs::write(
            &global,
            "[mcp.servers.shared]\ncommand = \"g\"\n[mcp.servers.only-global]\ncommand = \"g\"\n",
        )
        .expect("global");
        std::fs::write(
            root.join("slim.toml"),
            "[mcp.servers.shared]\nenabled = false\n[mcp.servers.only-project]\ncommand = \"p\"\n",
        )
        .expect("project");

        let (path, layer) =
            defining_file_in(&root, Some(global.clone()), "shared").expect("shared");
        assert_eq!(path, root.join("slim.toml"));
        assert_eq!(layer, "slim.toml do projeto");
        let (path, layer) =
            defining_file_in(&root, Some(global.clone()), "only-project").expect("project");
        assert_eq!(path, root.join("slim.toml"));
        assert_eq!(layer, "slim.toml do projeto");
        let (path, layer) =
            defining_file_in(&root, Some(global.clone()), "only-global").expect("global");
        assert_eq!(path, global);
        assert_eq!(layer, "slim.toml global");
        let error = defining_file_in(&root, Some(global), "ghost").expect_err("ghost");
        assert!(error.contains("nenhum slim.toml"), "{error}");
        let error = defining_file_in(&root, None, "only-global").expect_err("no global");
        assert!(error.contains("nenhum slim.toml"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writing_enabled_merges_into_the_entry_and_keeps_the_rest_of_the_file() {
        let root = temp_dir("merge");
        let global = root.join("global.toml");
        std::fs::write(
            &global,
            "[model]\nname = \"keep-me\"\n\n[mcp.servers.g]\ncommand = \"cmd\"\nargs = [\"a\"]\ntimeout_ms = 7000\ndescription = \"d\"\n[mcp.servers.g.env]\nTOKEN = \"$KEEP\"\n",
        )
        .expect("global");
        let (path, _) = defining_file_in(&root, Some(global.clone()), "g").expect("g");
        let update = FileMcpServerConfig {
            enabled: Some(false),
            ..FileMcpServerConfig::default()
        };
        config::upsert_mcp_server_to(&path, "g", &update).expect("write");
        let written: toml::Table = std::fs::read_to_string(&global)
            .expect("read")
            .parse()
            .expect("toml");
        assert_eq!(written["model"]["name"].as_str(), Some("keep-me"));
        let server = written["mcp"]["servers"]["g"].as_table().expect("g");
        assert_eq!(server["enabled"].as_bool(), Some(false));
        assert_eq!(server["command"].as_str(), Some("cmd"));
        assert_eq!(server["timeout_ms"].as_integer(), Some(7000));
        assert_eq!(server["description"].as_str(), Some("d"));
        assert_eq!(server["env"]["TOKEN"].as_str(), Some("$KEEP"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
