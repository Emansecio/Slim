//! Builds and installs the language-server-backed code intelligence facade
//! from layered [lsp] configuration. The TUI owns one facade for its complete
//! lifetime; one-shot headless commands own one for the duration of the run.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use slim_core::runtime::Runtime;
use slim_lsp::discovery::{ServerOptions, RUST_ANALYZER, TYPESCRIPT_LANGUAGE_SERVER};
use slim_lsp::{LspCodeIntelligence, LspManagerConfig};

use crate::config::LspConfig;
use crate::mcp::trust::{TrustDecision, TrustStore};

/// Whether the project layer's LSP launch overrides may apply, with the notice
/// to show when some are held back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LspProjectTrust {
    pub trusted: bool,
    /// Servers whose project overrides were ignored and why; `None` when
    /// nothing was held back.
    pub notice: Option<String>,
}

/// Applies the project MCP trust decision (session flag or the persistent
/// store) to the project's `[lsp]` launch overrides. A store that cannot be
/// read counts as untrusted and is reported, never silently ignored.
pub(crate) fn project_trust(
    config: &LspConfig,
    workspace: &Path,
    session_trust: bool,
) -> LspProjectTrust {
    project_trust_with(config, workspace, session_trust, TrustStore::default_store)
}

fn project_trust_with(
    config: &LspConfig,
    workspace: &Path,
    session_trust: bool,
    store: impl FnOnce() -> Result<TrustStore, String>,
) -> LspProjectTrust {
    let overridden = config
        .servers
        .iter()
        .filter(|(_, server)| server.has_project_overrides())
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    if overridden.is_empty() || session_trust {
        return LspProjectTrust {
            trusted: true,
            notice: None,
        };
    }
    let reason = match store().and_then(|store| store.decision(workspace)) {
        Ok(Some(TrustDecision::Trusted)) => {
            return LspProjectTrust {
                trusted: true,
                notice: None,
            }
        }
        Ok(Some(TrustDecision::Denied)) => "workspace trust denied".to_owned(),
        Ok(None) => "workspace not trusted".to_owned(),
        Err(error) => error,
    };
    LspProjectTrust {
        trusted: false,
        notice: Some(format!(
            "project LSP overrides ignored ({reason}): {}",
            overridden.join(", ")
        )),
    }
}

/// Creates the application-level manager. Missing binaries remain fail-open at
/// query time; disabled LSP configuration returns no manager at all. Project
/// launch overrides apply only when `project_trusted` (see [`project_trust`]).
pub fn build_code_intelligence(
    config: &LspConfig,
    project_trusted: bool,
) -> Option<Arc<LspCodeIntelligence>> {
    if !config.enabled {
        return None;
    }
    let mut servers = [RUST_ANALYZER, TYPESCRIPT_LANGUAGE_SERVER]
        .into_iter()
        .map(|id| (id.to_owned(), ServerOptions::default()))
        .collect::<std::collections::BTreeMap<_, _>>();
    for (id, server) in &config.servers {
        let server = server.effective(project_trusted);
        servers.insert(
            id.clone(),
            ServerOptions {
                enabled: server.enabled,
                path: server.path.as_ref().map(std::path::PathBuf::from),
                args: server.args.clone(),
                initialization_options: server.initialization_options.clone(),
                settings: server.settings.clone(),
            },
        );
    }
    if [RUST_ANALYZER, TYPESCRIPT_LANGUAGE_SERVER]
        .iter()
        .all(|id| !servers[*id].enabled)
    {
        return None;
    }
    let manager_config = LspManagerConfig {
        idle_shutdown: Some(Duration::from_secs(
            config.idle_shutdown_minutes.saturating_mul(60),
        )),
        max_servers: config.max_servers.max(1),
        request_timeout: Duration::from_millis(config.request_timeout_ms.max(1_000)),
        servers,
        max_open_documents: config.max_open_documents.max(1),
    };
    Some(LspCodeIntelligence::from_config(manager_config))
}

/// Compatibility helper for callers that already own a Runtime. Prefer
/// building once and passing the same manager to every runtime in an app.
pub fn install_code_intel(runtime: &mut Runtime, cwd: &Path, config: &LspConfig) {
    let trust = project_trust(config, cwd, false);
    if let Some(notice) = &trust.notice {
        eprintln!("warning: {notice}");
    }
    if let Some(manager) = build_code_intelligence(config, trust.trusted) {
        runtime.set_code_intelligence(manager);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LspProjectOverrides, LspServerConfig};
    use slim_core::codeintel::CodeIntelligence;

    #[test]
    fn disabling_rust_preserves_the_native_typescript_profile() {
        let mut config = LspConfig::default();
        assert!(build_code_intelligence(&config, true).is_some());
        config.servers.insert(
            "rust-analyzer".into(),
            LspServerConfig {
                enabled: false,
                path: Some("ignored.exe".into()),
                ..Default::default()
            },
        );
        assert!(build_code_intelligence(&config, true).is_some());
        config.servers.insert(
            TYPESCRIPT_LANGUAGE_SERVER.into(),
            LspServerConfig {
                enabled: false,
                ..Default::default()
            },
        );
        assert!(build_code_intelligence(&config, true).is_none());
        config.servers.get_mut("rust-analyzer").unwrap().enabled = true;
        config.enabled = false;
        assert!(build_code_intelligence(&config, true).is_none());
    }

    #[test]
    fn project_launch_overrides_follow_the_project_trust_decision() {
        let workspace = std::env::temp_dir().join(format!(
            "slim-lsp-trust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let store_path = workspace.join("trust.json");
        let store = || Ok(TrustStore::at(&store_path));
        let mut config = LspConfig::default();
        let clean = project_trust_with(&config, &workspace, false, store);
        assert!(clean.trusted && clean.notice.is_none());

        config.servers.insert(
            TYPESCRIPT_LANGUAGE_SERVER.into(),
            LspServerConfig {
                project: LspProjectOverrides {
                    path: Some("repo-controlled.mjs".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let untrusted = project_trust_with(&config, &workspace, false, store);
        assert!(!untrusted.trusted);
        let notice = untrusted.notice.unwrap();
        assert!(notice.contains("not trusted") && notice.contains(TYPESCRIPT_LANGUAGE_SERVER));
        assert!(project_trust_with(&config, &workspace, true, store).trusted);

        TrustStore::at(&store_path)
            .set(&workspace, Some(TrustDecision::Trusted))
            .unwrap();
        let trusted = project_trust_with(&config, &workspace, false, store);
        assert!(trusted.trusted && trusted.notice.is_none());
        TrustStore::at(&store_path)
            .set(&workspace, Some(TrustDecision::Denied))
            .unwrap();
        let denied = project_trust_with(&config, &workspace, false, store);
        assert!(!denied.trusted && denied.notice.unwrap().contains("denied"));

        let unreadable = project_trust_with(&config, &workspace, false, || {
            Err("trust store unavailable".to_owned())
        });
        assert!(!unreadable.trusted);
        assert!(unreadable
            .notice
            .unwrap()
            .contains("trust store unavailable"));
        std::fs::remove_dir_all(&workspace).unwrap();
    }

    #[tokio::test]
    async fn untrusted_project_path_is_not_used() {
        let mut config = LspConfig::default();
        config.servers.insert(
            "rust-analyzer".into(),
            LspServerConfig {
                project: LspProjectOverrides {
                    path: Some("repo-controlled-ra.exe".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let manager = build_code_intelligence(&config, false).unwrap();
        let status = manager.status(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        let rust = status.payload["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["server"] == RUST_ANALYZER)
            .unwrap();
        assert!(!rust["binary"]
            .as_str()
            .is_some_and(|binary| binary.contains("repo-controlled")));
        assert!(!rust["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("configured")));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn configured_path_is_used_without_starting_a_process() {
        let path = std::env::current_exe()
            .unwrap()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut config = LspConfig::default();
        config.servers.insert(
            "rust-analyzer".into(),
            LspServerConfig {
                path: Some(path.clone()),
                ..Default::default()
            },
        );
        let manager = build_code_intelligence(&config, true).unwrap();
        let status = manager.status(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        let rust = status.payload["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["server"] == RUST_ANALYZER)
            .unwrap();
        assert_eq!(rust["binary"], path);
        assert_eq!(manager.pool().running_servers().await, 0);
        manager.shutdown().await;
    }
}
