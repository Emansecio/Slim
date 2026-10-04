//! Persistent project trust decisions for MCP servers defined by a
//! workspace's `slim.toml`.
//!
//! A project file can name any executable, so its servers do not start until
//! the user trusts the workspace. Decisions live in `mcp-trust.json` in the
//! Slim config directory (owner-only file, same discipline as `auth.json`),
//! keyed by the canonical workspace path. Nothing inside a workspace can grant
//! trust to itself: the store is outside every workspace and read-only to
//! project configuration.
//!
//! Every failure to read the store is reported and treated as "not trusted".

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::auth::{read_secure_json, update_secure_json, AuthError};

#[cfg(not(test))]
const TRUST_FILE_NAME: &str = "mcp-trust.json";
const TRUST_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustDecision {
    Trusted,
    /// "Never": do not start project servers and stop asking.
    Denied,
}

#[derive(Clone, Debug)]
pub(crate) struct TrustStore {
    path: PathBuf,
}

/// Stable store key for a workspace: canonical path, without the Windows
/// verbatim prefix, case-folded where the file system is case-insensitive.
pub(crate) fn workspace_key(workspace: &Path) -> Result<String, String> {
    let canonical = std::fs::canonicalize(workspace)
        .map_err(|error| format!("workspace {}: {error}", workspace.display()))?;
    let mut key = canonical.to_string_lossy().into_owned();
    if let Some(rest) = key.strip_prefix(r"\\?\UNC\") {
        key = format!(r"\\{rest}");
    } else if let Some(rest) = key.strip_prefix(r"\\?\") {
        key = rest.to_owned();
    }
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    Ok(key)
}

impl TrustStore {
    pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `SLIM_MCP_TRUST_FILE`, else `mcp-trust.json` beside the global config.
    pub(crate) fn default_store() -> Result<Self, String> {
        if let Some(path) = std::env::var_os("SLIM_MCP_TRUST_FILE").filter(|path| !path.is_empty())
        {
            return Ok(Self::at(path));
        }
        #[cfg(test)]
        {
            Ok(Self::at(std::env::temp_dir().join(format!(
                "slim-test-mcp-trust-{}.json",
                std::process::id()
            ))))
        }
        #[cfg(not(test))]
        {
            let config = crate::config::global_config_path()
                .ok_or_else(|| "unable to resolve the Slim config directory".to_owned())?;
            let directory = config
                .parent()
                .ok_or_else(|| "unable to resolve the Slim config directory".to_owned())?;
            Ok(Self::at(directory.join(TRUST_FILE_NAME)))
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn describe(&self, error: AuthError) -> String {
        format!("MCP trust store {}: {error}", self.path.display())
    }

    fn projects(document: &Value) -> Result<&Map<String, Value>, String> {
        if document.get("version").and_then(Value::as_u64) != Some(TRUST_VERSION) {
            return Err("unsupported trust store version".to_owned());
        }
        document
            .get("projects")
            .and_then(Value::as_object)
            .ok_or_else(|| "trust store has no projects table".to_owned())
    }

    /// Recorded decision for `workspace`, `None` when never decided.
    pub(crate) fn decision(&self, workspace: &Path) -> Result<Option<TrustDecision>, String> {
        let key = workspace_key(workspace)?;
        let Some(document) = read_secure_json(&self.path).map_err(|error| self.describe(error))?
        else {
            return Ok(None);
        };
        let projects = Self::projects(&document)
            .map_err(|error| format!("MCP trust store {}: {error}", self.path.display()))?;
        match projects.get(&key) {
            None => Ok(None),
            Some(Value::Bool(true)) => Ok(Some(TrustDecision::Trusted)),
            Some(Value::Bool(false)) => Ok(Some(TrustDecision::Denied)),
            Some(_) => Err(format!(
                "MCP trust store {}: invalid decision for {key}",
                self.path.display()
            )),
        }
    }

    /// Records (or with `None` clears) the decision for `workspace`.
    pub(crate) fn set(
        &self,
        workspace: &Path,
        decision: Option<TrustDecision>,
    ) -> Result<(), String> {
        let key = workspace_key(workspace)?;
        let mut failure: Option<String> = None;
        let result = update_secure_json(&self.path, |current| {
            let mut document =
                current.unwrap_or_else(|| json!({ "version": TRUST_VERSION, "projects": {} }));
            if let Err(error) = Self::projects(&document) {
                failure = Some(format!("MCP trust store {}: {error}", self.path.display()));
                return Err(AuthError::InvalidSchema);
            }
            let projects = document
                .get_mut("projects")
                .and_then(Value::as_object_mut)
                .ok_or(AuthError::InvalidSchema)?;
            match decision {
                Some(decision) => {
                    projects.insert(key.clone(), Value::Bool(decision == TrustDecision::Trusted));
                }
                None => {
                    projects.remove(&key);
                }
            }
            Ok(document)
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(failure.unwrap_or_else(|| self.describe(error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slim-mcp-trust-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn decisions_round_trip_per_canonical_workspace() {
        let root = temp_dir("roundtrip");
        let workspace_a = root.join("a");
        let workspace_b = root.join("b");
        std::fs::create_dir_all(&workspace_a).unwrap();
        std::fs::create_dir_all(&workspace_b).unwrap();
        let store = TrustStore::at(root.join("store").join("mcp-trust.json"));

        assert_eq!(store.decision(&workspace_a).unwrap(), None);
        store
            .set(&workspace_a, Some(TrustDecision::Trusted))
            .unwrap();
        store
            .set(&workspace_b, Some(TrustDecision::Denied))
            .unwrap();
        assert_eq!(
            store.decision(&workspace_a).unwrap(),
            Some(TrustDecision::Trusted)
        );
        assert_eq!(
            store.decision(&workspace_b).unwrap(),
            Some(TrustDecision::Denied)
        );
        // A non-canonical spelling of the same directory shares the decision.
        let dotted = workspace_a.join("..").join("a");
        assert_eq!(
            store.decision(&dotted).unwrap(),
            Some(TrustDecision::Trusted)
        );
        store.set(&workspace_a, None).unwrap();
        assert_eq!(store.decision(&workspace_a).unwrap(), None);
        assert_eq!(
            store.decision(&workspace_b).unwrap(),
            Some(TrustDecision::Denied)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_or_foreign_store_fails_closed_without_being_overwritten() {
        let root = temp_dir("corrupt");
        let workspace = root.join("w");
        std::fs::create_dir_all(&workspace).unwrap();
        let path = root.join("mcp-trust.json");
        let store = TrustStore::at(&path);
        // Create a valid secure file first, then corrupt its schema through
        // the same secure writer so permissions stay valid.
        store.set(&workspace, Some(TrustDecision::Trusted)).unwrap();
        crate::auth::update_secure_json(&path, |_| Ok(json!({ "version": 9, "projects": {} })))
            .unwrap();
        let error = store.decision(&workspace).unwrap_err();
        assert!(error.contains("unsupported trust store version"), "{error}");
        let error = store
            .set(&workspace, Some(TrustDecision::Trusted))
            .unwrap_err();
        assert!(error.contains("unsupported trust store version"), "{error}");
        let document = read_secure_json(&path).unwrap().unwrap();
        assert_eq!(document["version"], 9, "store must not be rewritten");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_key_is_stable_and_lacks_the_verbatim_prefix() {
        let root = temp_dir("key");
        let key = workspace_key(&root).unwrap();
        assert!(!key.starts_with(r"\\?\"), "{key}");
        assert_eq!(key, workspace_key(&root.join(".")).unwrap());
        assert!(workspace_key(&root.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
