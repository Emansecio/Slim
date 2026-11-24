//! Clean configuration snapshot and mutation helpers for desktop/UI MCP pages.
//! Reads and writes the exact same layered `slim.toml` and `mcp-trust.json`
//! files used by the CLI and embedded TUI runtimes.

use std::path::{Path, PathBuf};

use slim_core::mcp::McpExposure;

use crate::config::{
    clear_mcp_disable_override_to, global_config_path, load_layered_for,
    load_layered_lenient_for, mcp_server_defined_in, project_config_path, remove_mcp_server_from,
    upsert_mcp_server_to, FileConfig, FileMcpServerConfig, McpConfig, McpOrigin, McpServerConfig,
};
use crate::mcp::trust::{TrustDecision, TrustStore};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum McpScope {
    Global,
    Project,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpConfigEntry {
    pub name: String,
    pub transport: &'static str,
    pub target: String,
    pub enabled: bool,
    pub scope: McpScope,
    pub exposure: &'static str,
    pub description: Option<String>,
    pub untrusted: bool,
    pub invalid: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpConfigSnapshot {
    pub entries: Vec<McpConfigEntry>,
    pub project_trusted: bool,
    pub project_denied: bool,
    pub global_path: Option<PathBuf>,
    pub project_path: Option<PathBuf>,
    pub trust_path: Option<PathBuf>,
    pub error: Option<String>,
}

fn exposure_label(exposure: McpExposure) -> &'static str {
    match exposure {
        McpExposure::Gateway => "gateway",
        McpExposure::Direct => "direct",
        McpExposure::Hidden => "hidden",
    }
}

fn format_target(server: &McpServerConfig) -> (&'static str, String) {
    match (&server.command, &server.url) {
        (Some(cmd), _) => {
            if server.args.is_empty() {
                ("stdio", cmd.clone())
            } else {
                ("stdio", format!("{cmd} {}", server.args.join(" ")))
            }
        }
        (None, Some(url)) => ("http", url.clone()),
        (None, None) => ("stdio", String::new()),
    }
}

/// Returns a complete snapshot of configured MCP servers (global + optional project),
/// together with workspace trust status and any invalid server entries.
pub fn mcp_snapshot(workspace: Option<&Path>) -> McpConfigSnapshot {
    let global_path = global_config_path();
    let project_path = workspace.map(project_config_path);
    let trust_store = TrustStore::default_store().ok();
    let trust_path = trust_store.as_ref().map(|s| s.path().to_path_buf());

    let (project_trusted, project_denied) = match (workspace, trust_store.as_ref()) {
        (Some(ws), Some(store)) => match store.decision(ws) {
            Ok(Some(TrustDecision::Trusted)) => (true, false),
            Ok(Some(TrustDecision::Denied)) => (false, true),
            _ => (false, false),
        },
        _ => (false, false),
    };

    let loaded = match workspace {
        Some(ws) => load_layered_lenient_for(ws),
        None => {
            let mut layered = crate::config::LayeredConfig::default();
            let mut invalid = Vec::new();
            let mut err = None;
            if let Some(path) = global_path.as_deref() {
                match FileConfig::load(path) {
                    Ok(Some(cfg)) => crate::config::merge_layer(&mut layered, cfg),
                    Ok(None) => {}
                    Err(e) => err = Some(e),
                }
            }
            if let Some(e) = err {
                Err(e)
            } else {
                let candidates = std::mem::take(&mut layered.mcp.servers);
                let mut accepted = McpConfig {
                    startup_wait_ms: layered.mcp.startup_wait_ms,
                    ..McpConfig::default()
                };
                for (name, server) in candidates {
                    accepted.servers.insert(name.clone(), server);
                    if let Err(error) = accepted.validate() {
                        accepted.servers.remove(&name);
                        invalid.push((name, error));
                    }
                }
                layered.mcp = accepted;
                Ok((layered, invalid))
            }
        }
    };

    match loaded {
        Ok((layered, invalid)) => {
            let mut entries = Vec::with_capacity(layered.mcp.servers.len() + invalid.len());
            for (name, server) in layered.mcp.servers {
                let scope = match server.origin {
                    McpOrigin::Global => McpScope::Global,
                    McpOrigin::Project => McpScope::Project,
                };
                let (transport, target) = format_target(&server);
                entries.push(McpConfigEntry {
                    name,
                    transport,
                    target,
                    enabled: server.enabled,
                    scope,
                    exposure: exposure_label(server.exposure),
                    description: server.description,
                    untrusted: scope == McpScope::Project && !project_trusted,
                    invalid: None,
                });
            }
            for (name, reason) in invalid {
                let in_project = project_path
                    .as_deref()
                    .and_then(|p| mcp_server_defined_in(p, &name).ok())
                    .unwrap_or(false);
                entries.push(McpConfigEntry {
                    name,
                    transport: "stdio",
                    target: String::new(),
                    enabled: false,
                    scope: if in_project {
                        McpScope::Project
                    } else {
                        McpScope::Global
                    },
                    exposure: "gateway",
                    description: None,
                    untrusted: false,
                    invalid: Some(reason),
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            McpConfigSnapshot {
                entries,
                project_trusted,
                project_denied,
                global_path,
                project_path,
                trust_path,
                error: None,
            }
        }
        Err(error) => McpConfigSnapshot {
            entries: Vec::new(),
            project_trusted,
            project_denied,
            global_path,
            project_path,
            trust_path,
            error: Some(error),
        },
    }
}

/// Enables or disables a configured MCP server in the appropriate `slim.toml` layer.
pub fn mcp_set_enabled(
    workspace: Option<&Path>,
    name: &str,
    scope: McpScope,
    enabled: bool,
) -> Result<(), String> {
    let target_path = match scope {
        McpScope::Project => workspace
            .map(project_config_path)
            .ok_or_else(|| "nenhum projeto selecionado".to_owned())?,
        McpScope::Global => {
            // If a global server was disabled in the project layer, lift that override when re-enabling.
            if enabled {
                if let Some(ws) = workspace {
                    let _ = clear_mcp_disable_override_to(&project_config_path(ws), name);
                }
            }
            global_config_path().ok_or_else(|| "caminho de configuração global indisponível".to_owned())?
        }
    };

    if enabled && scope == McpScope::Project && clear_mcp_disable_override_to(&target_path, name)? {
        return Ok(());
    }

    let update = FileMcpServerConfig {
        enabled: Some(enabled),
        ..FileMcpServerConfig::default()
    };
    upsert_mcp_server_to(&target_path, name, &update)
}

/// Adds or updates an MCP server definition in global or project `slim.toml`.
pub fn mcp_add_server(
    workspace: Option<&Path>,
    name: &str,
    command: Option<String>,
    args: Vec<String>,
    url: Option<String>,
    global: bool,
) -> Result<PathBuf, String> {
    let name = name.trim();
    let valid_name = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid_name {
        return Err("Nome inválido (use letras, dígitos, _ e -, até 64 caracteres).".to_owned());
    }

    let file = FileMcpServerConfig {
        command: command.filter(|s| !s.trim().is_empty()),
        args: if args.is_empty() { None } else { Some(args) },
        url: url.filter(|s| !s.trim().is_empty()),
        enabled: Some(true),
        ..FileMcpServerConfig::default()
    };
    let server = McpServerConfig {
        command: file.command.clone(),
        args: file.args.clone().unwrap_or_default(),
        url: file.url.clone(),
        enabled: true,
        ..McpServerConfig::default()
    };

    let mut check = McpConfig {
        servers: match workspace {
            Some(ws) => load_layered_for(ws)
                .map(|l| l.mcp.servers)
                .unwrap_or_default(),
            None => global_config_path()
                .and_then(|p| FileConfig::load(&p).ok().flatten())
                .map(|cfg| {
                    let mut layered = crate::config::LayeredConfig::default();
                    crate::config::merge_layer(&mut layered, cfg);
                    layered.mcp.servers
                })
                .unwrap_or_default(),
        },
        ..McpConfig::default()
    };
    check.servers.insert(name.to_owned(), server);
    check.validate()?;

    let path = if global {
        global_config_path()
            .ok_or_else(|| "caminho de configuração global indisponível".to_owned())?
    } else {
        workspace
            .map(project_config_path)
            .ok_or_else(|| "selecione um projeto ou salve como global".to_owned())?
    };
    upsert_mcp_server_to(&path, name, &file)?;
    Ok(path)
}

/// Ensures a global stdio MCP server exists in the global `slim.toml` without overwriting an
/// existing entry (including one the user disabled). Returns `Ok(true)` if the server was added.
pub fn mcp_ensure_global_server(
    name: &str,
    command: String,
    args: Vec<String>,
    description: Option<String>,
) -> Result<bool, String> {
    let name = name.trim();
    let valid_name = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid_name {
        return Err("Nome inválido (use letras, dígitos, _ e -, até 64 caracteres).".to_owned());
    }
    let path = global_config_path()
        .ok_or_else(|| "caminho de configuração global indisponível".to_owned())?;
    if path.exists() && mcp_server_defined_in(&path, name)? {
        return Ok(false);
    }

    let description = description.filter(|s| !s.trim().is_empty());
    let file = FileMcpServerConfig {
        command: Some(command.clone()).filter(|s| !s.trim().is_empty()),
        args: if args.is_empty() { None } else { Some(args.clone()) },
        description: description.clone(),
        enabled: Some(true),
        ..FileMcpServerConfig::default()
    };
    let server = McpServerConfig {
        command: file.command.clone(),
        args: file.args.clone().unwrap_or_default(),
        description,
        enabled: true,
        ..McpServerConfig::default()
    };
    let mut check = McpConfig::default();
    check.servers.insert(name.to_owned(), server);
    check.validate()?;

    upsert_mcp_server_to(&path, name, &file)?;
    Ok(true)
}

/// Removes an MCP server from the specified configuration layer.
pub fn mcp_remove_server(
    workspace: Option<&Path>,
    name: &str,
    scope: McpScope,
) -> Result<bool, String> {
    let path = match scope {
        McpScope::Global => global_config_path()
            .ok_or_else(|| "caminho de configuração global indisponível".to_owned())?,
        McpScope::Project => workspace
            .map(project_config_path)
            .ok_or_else(|| "nenhum projeto selecionado".to_owned())?,
    };
    remove_mcp_server_from(&path, name)
}

/// Persists the workspace trust decision for project-defined MCP servers.
pub fn mcp_set_project_trust(workspace: &Path, trusted: bool) -> Result<(), String> {
    let store = TrustStore::default_store()?;
    let decision = if trusted {
        TrustDecision::Trusted
    } else {
        TrustDecision::Denied
    };
    store.set(workspace, Some(decision))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_desktop_add_toggle_and_remove_project_server() {
        let dir = std::env::temp_dir().join(format!("slim-mcp-desktop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = mcp_add_server(
            Some(&dir),
            "demo_docs",
            Some("npx".into()),
            vec!["-y".into(), "@example/docs".into()],
            None,
            false,
        )
        .expect("add server");
        assert!(path.exists());

        let snap = mcp_snapshot(Some(&dir));
        let entry = snap.entries.iter().find(|e| e.name == "demo_docs").expect("present");
        assert_eq!(entry.scope, McpScope::Project);
        assert_eq!(entry.transport, "stdio");
        assert_eq!(entry.target, "npx -y @example/docs");
        assert!(entry.enabled);

        mcp_set_enabled(Some(&dir), "demo_docs", McpScope::Project, false).expect("disable");
        let snap2 = mcp_snapshot(Some(&dir));
        let entry2 = snap2.entries.iter().find(|e| e.name == "demo_docs").expect("present");
        assert!(!entry2.enabled);

        mcp_set_enabled(Some(&dir), "demo_docs", McpScope::Project, true).expect("enable");
        let snap3 = mcp_snapshot(Some(&dir));
        let entry3 = snap3.entries.iter().find(|e| e.name == "demo_docs").expect("present");
        assert!(entry3.enabled);

        assert!(mcp_remove_server(Some(&dir), "demo_docs", McpScope::Project).expect("remove"));
        let snap4 = mcp_snapshot(Some(&dir));
        assert!(!snap4.entries.iter().any(|e| e.name == "demo_docs"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_ensure_global_server_adds_once_with_description_and_respects_existing() {
        let added = mcp_ensure_global_server(
            "chrome-devtools-test",
            "chrome-devtools-mcp".into(),
            Vec::new(),
            Some("Chrome DevTools automation".into()),
        )
        .expect("ensure");
        assert!(added);

        let snap = mcp_snapshot(None);
        let entry = snap
            .entries
            .iter()
            .find(|e| e.name == "chrome-devtools-test")
            .expect("present");
        assert_eq!(entry.scope, McpScope::Global);
        assert_eq!(entry.transport, "stdio");
        assert_eq!(entry.target, "chrome-devtools-mcp");
        assert_eq!(entry.description.as_deref(), Some("Chrome DevTools automation"));
        assert!(entry.enabled);

        // Disabling and calling ensure again must preserve the user's disabled state.
        mcp_set_enabled(None, "chrome-devtools-test", McpScope::Global, false).expect("disable");
        let added_again = mcp_ensure_global_server(
            "chrome-devtools-test",
            "other-bin".into(),
            Vec::new(),
            Some("Other".into()),
        )
        .expect("ensure again");
        assert!(!added_again);
        let snap2 = mcp_snapshot(None);
        let entry2 = snap2
            .entries
            .iter()
            .find(|e| e.name == "chrome-devtools-test")
            .expect("still present");
        assert!(!entry2.enabled);
        assert_eq!(entry2.target, "chrome-devtools-mcp");

        let _ = mcp_remove_server(None, "chrome-devtools-test", McpScope::Global);
    }
}
