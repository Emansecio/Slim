//! Builds the application-scoped MCP manager from layered `[mcp]` config.
//! The TUI owns one manager for its lifetime; headless runs own one per turn.
//! Connections stay lazy: building a manager never spawns a process or
//! opens a socket.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use slim_core::mcp::{McpManager, McpServerSpec, McpTransport};
use slim_core::process::ExecutableResolver;

use crate::config::{McpConfig, McpServerConfig};

/// Converts merged file config into runtime specs. `load_layered` validates
/// before this runs; a server with neither transport is skipped defensively.
pub fn specs_from_config(config: &McpConfig) -> BTreeMap<String, McpServerSpec> {
    config
        .servers
        .iter()
        .filter_map(|(name, server)| spec_from(name, server).map(|spec| (name.clone(), spec)))
        .collect()
}

/// One server's runtime spec; `name`/`server` must already be validated.
pub fn server_spec(name: &str, server: &McpServerConfig) -> McpServerSpec {
    spec_from(name, server).expect("validated server config always maps to a spec")
}

fn spec_from(name: &str, server: &McpServerConfig) -> Option<McpServerSpec> {
    let transport = match (&server.command, &server.url) {
        (Some(command), None) => McpTransport::Stdio {
            command: command.clone(),
            args: server.args.clone(),
            env: server.env.clone(),
        },
        (None, Some(url)) => McpTransport::Http {
            url: url.clone(),
            headers: server.headers.clone(),
        },
        _ => return None,
    };
    Some(McpServerSpec {
        name: name.to_owned(),
        transport,
        enabled: server.enabled,
        timeout: Duration::from_millis(server.timeout_ms),
    })
}

/// Creates the shared manager, or `None` when no server is configured.
/// `cwd` is the workspace root stdio servers are spawned in.
pub fn build_mcp_manager(config: &McpConfig, cwd: &Path) -> Option<Arc<McpManager>> {
    if config.servers.is_empty() {
        return None;
    }
    Some(Arc::new(McpManager::new(
        specs_from_config(config),
        cwd.to_path_buf(),
        ExecutableResolver::default(),
    )))
}
