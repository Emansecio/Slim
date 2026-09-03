//! Server discovery: which server serves a workspace and where its binary
//! lives. The v1 vertical slice ships rust-analyzer only; the table is
//! intentionally small and additive. Discovery never installs anything and
//! never spawns processes: a missing binary is reported, and the pool circuit
//! breaker handles a binary that exists but fails to start.

use std::path::{Path, PathBuf};

pub const MAX_ROOT_WALK_UP: usize = 12;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSpec {
    pub id: String,
    pub label: String,
    /// Resolved binary (absolute when found) or plain name when not found.
    pub command: String,
    pub args: Vec<String>,
    pub root_markers: Vec<String>,
    /// (file extension, LSP languageId) pairs used to sync documents.
    pub language_ids: Vec<(String, String)>,
    /// Section name used to answer workspace/configuration.
    pub settings_section: String,
}

impl ServerSpec {
    pub fn language_id_for(&self, path: &Path) -> Option<&str> {
        let ext = path.extension().and_then(|value| value.to_str())?;
        self.language_ids
            .iter()
            .find(|(candidate, _)| candidate == ext)
            .map(|(_, language)| language.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryResult {
    /// Nearest directory containing a root marker (or the workspace itself).
    pub root: Option<PathBuf>,
    pub spec: Option<ServerSpec>,
    pub binary_missing: bool,
}

/// Walks up from the workspace looking for a directory holding any marker file.
pub fn find_root_marker(start: &Path, markers: &[&str]) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    for _ in 0..=MAX_ROOT_WALK_UP {
        if markers.iter().any(|marker| dir.join(marker).is_file()) {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Resolves a server binary from an explicit configured path, then from a
/// set of PATH-style directories. Windows extensions are probed first so the
/// plain name also works on Unix.
pub fn resolve_binary_from_paths(
    configured: Option<&Path>,
    bin_name: &str,
    path_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(path) = configured {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    let extensions = ["", ".exe", ".cmd", ".bat"];
    for dir in path_dirs {
        for ext in extensions {
            let candidate = if ext.is_empty() {
                dir.join(bin_name)
            } else {
                dir.join(format!("{bin_name}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn resolve_binary_on_path(configured: Option<&Path>, bin_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    resolve_binary_from_paths(configured, bin_name, &dirs)
}

pub fn rust_analyzer_spec(command: String) -> ServerSpec {
    ServerSpec {
        id: "rust-analyzer".into(),
        label: "Rust Analyzer".into(),
        command,
        args: vec![],
        root_markers: vec!["Cargo.toml".into()],
        language_ids: vec![("rs".into(), "rust".into())],
        settings_section: "rust-analyzer".into(),
    }
}

/// Resolves discovery for the rust-analyzer server against a workspace dir.
pub fn discover_for_workspace(workspace: &Path, configured: Option<&Path>) -> DiscoveryResult {
    let markers = ["Cargo.toml"];
    let Some(root) = find_root_marker(workspace, &markers) else {
        return DiscoveryResult {
            root: None,
            spec: None,
            binary_missing: true,
        };
    };
    match resolve_binary_on_path(configured, "rust-analyzer") {
        Some(binary) => DiscoveryResult {
            root: Some(root),
            spec: Some(rust_analyzer_spec(binary.to_string_lossy().into_owned())),
            binary_missing: false,
        },
        None => DiscoveryResult {
            root: Some(root),
            // Keep a spec so status can explain what is missing.
            spec: Some(rust_analyzer_spec("rust-analyzer".into())),
            binary_missing: true,
        },
    }
}

/// Stable config hash for pooling: same root + server + config share a process.
pub fn config_hash(payload: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.to_string().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_marker_at_workspace_and_walks_up() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(find_root_marker(&dir, &["Cargo.toml"]), Some(dir.clone()));
        assert_eq!(
            find_root_marker(&dir.join("sub"), &["Cargo.toml"]),
            Some(dir.clone())
        );
        assert_eq!(
            find_root_marker(&dir.join("sub"), &["pyproject.toml"]),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_binary_from_configured_first() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-bin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let configured = dir.join("custom-ra.exe");
        std::fs::write(&configured, "x").unwrap();
        let resolved = resolve_binary_from_paths(
            Some(&configured),
            "rust-analyzer",
            &[PathBuf::from("C:\\does-not-exist")],
        )
        .expect("configured binary");
        assert_eq!(resolved, configured);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_binary_from_path_dirs_with_windows_extension() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-bin2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rust-analyzer.exe"), "x").unwrap();
        let resolved = resolve_binary_from_paths(None, "rust-analyzer", std::slice::from_ref(&dir))
            .expect("path binary");
        assert_eq!(resolved, dir.join("rust-analyzer.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_reports_missing_binary_without_root_change() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        let result = discover_for_workspace(&dir, None);
        assert_eq!(result.root, Some(dir.clone()));
        assert!(result.spec.is_some());
        // binary_missing depends on whether rust-analyzer is on PATH; either
        // outcome is correct as long as root and spec are populated.
        assert!(
            result.binary_missing == (result.spec.as_ref().unwrap().command == "rust-analyzer")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_hash_changes_with_config() {
        use serde_json::json;
        let a = config_hash(&json!({ "checkOnSave": false }));
        let b = config_hash(&json!({ "checkOnSave": true }));
        assert_ne!(a, b);
        assert_eq!(a, config_hash(&json!({ "checkOnSave": false })));
    }
}
