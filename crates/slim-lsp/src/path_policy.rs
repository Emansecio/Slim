//! Bidirectional filesystem boundary for LSP traffic.
//!
//! Inputs from the agent and file URIs returned by a language server are both
//! canonicalized through the filesystem. A result is usable only when the
//! resolved target remains under the canonical workspace root. This rejects
//! `..`, alternate spellings and symlink/junction escapes on both Unix and
//! Windows.

use std::path::{Path, PathBuf};

use url::Url;

pub(crate) fn canonical_root(root: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    root.is_dir().then_some(root)
}

pub(crate) fn existing_workspace_path(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let root = canonical_root(root)?;
    let candidate = std::fs::canonicalize(candidate).ok()?;
    candidate.starts_with(&root).then_some(candidate)
}

pub(crate) fn url_workspace_path(root: &Path, uri: &Url) -> Option<PathBuf> {
    let candidate = uri.to_file_path().ok()?;
    existing_workspace_path(root, &candidate)
}

#[cfg(test)]
pub(crate) fn relative_workspace_path(root: &Path, candidate: &Path) -> Option<String> {
    let root = canonical_root(root)?;
    let candidate = existing_workspace_path(&root, candidate)?;
    candidate
        .strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "slim-lsp-path-policy-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn accepts_existing_file_below_root() {
        let root = TestDir::new("inside");
        let nested = root.0.join("src");
        std::fs::create_dir_all(&nested).expect("create nested");
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write file");
        let resolved = existing_workspace_path(&root.0, &file).expect("inside path");
        assert_eq!(
            relative_workspace_path(&root.0, &resolved).as_deref(),
            Some(if cfg!(windows) {
                "src\\main.rs"
            } else {
                "src/main.rs"
            })
        );
    }

    #[test]
    fn rejects_existing_file_outside_root() {
        let root = TestDir::new("root");
        let outside = TestDir::new("outside");
        let file = outside.0.join("secret.rs");
        std::fs::write(&file, "secret\n").expect("write file");
        assert!(existing_workspace_path(&root.0, &file).is_none());
        let uri = Url::from_file_path(&file).expect("file URI");
        assert!(url_workspace_path(&root.0, &uri).is_none());
    }
}
