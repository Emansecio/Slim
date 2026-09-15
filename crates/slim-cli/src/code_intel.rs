//! Builds and installs the language-server-backed code intelligence facade
//! from layered [lsp] configuration. The TUI owns one facade for its complete
//! lifetime; one-shot headless commands own one for the duration of the run.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use slim_core::runtime::Runtime;
use slim_lsp::{LspCodeIntelligence, LspManagerConfig};

use crate::config::{LspConfig, LspServerConfig};

/// Creates the application-level manager. Missing binaries remain fail-open at
/// query time; disabled LSP configuration returns no manager at all.
pub fn build_code_intelligence(config: &LspConfig) -> Option<Arc<LspCodeIntelligence>> {
    if !config.enabled
        || config
            .servers
            .get("rust-analyzer")
            .is_some_and(|server| !server.enabled)
    {
        return None;
    }
    let rust_analyzer_path = config
        .servers
        .get("rust-analyzer")
        .and_then(|server: &LspServerConfig| server.path.clone())
        .map(std::path::PathBuf::from);
    let manager_config = LspManagerConfig {
        idle_shutdown: Some(Duration::from_secs(
            config.idle_shutdown_minutes.saturating_mul(60),
        )),
        max_servers: config.max_servers.max(1),
        request_timeout: Duration::from_millis(config.request_timeout_ms.max(1_000)),
        server_config: serde_json::json!({ "checkOnSave": false }),
        max_open_documents: config.max_open_documents.max(1),
        server_path: rust_analyzer_path,
    };
    Some(LspCodeIntelligence::from_config(manager_config))
}

/// Compatibility helper for callers that already own a Runtime. Prefer
/// building once and passing the same manager to every runtime in an app.
pub fn install_code_intel(runtime: &mut Runtime, _cwd: &Path, config: &LspConfig) {
    if let Some(manager) = build_code_intelligence(config) {
        runtime.set_code_intelligence(manager);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slim_core::codeintel::CodeIntelligence;

    #[test]
    fn disabled_server_builds_no_manager_even_with_global_lsp_enabled() {
        let mut config = LspConfig::default();
        assert!(build_code_intelligence(&config).is_some());
        config.servers.insert(
            "rust-analyzer".into(),
            LspServerConfig {
                enabled: false,
                path: Some("ignored.exe".into()),
            },
        );
        assert!(build_code_intelligence(&config).is_none());
        config.servers.get_mut("rust-analyzer").unwrap().enabled = true;
        config.enabled = false;
        assert!(build_code_intelligence(&config).is_none());
    }

    #[tokio::test]
    async fn configured_path_is_used_without_starting_a_process() {
        let path = std::env::current_exe()
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
        let manager = build_code_intelligence(&config).unwrap();
        let status = manager.status(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        assert_eq!(status.payload["servers"][0]["binary"], path);
        assert_eq!(manager.pool().running_servers().await, 0);
        manager.shutdown().await;
    }
}
