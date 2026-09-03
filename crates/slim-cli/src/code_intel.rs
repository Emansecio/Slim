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
    if !config.enabled {
        return None;
    }
    let rust_analyzer_path = config
        .servers
        .get("rust-analyzer")
        .filter(|server| server.enabled)
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
