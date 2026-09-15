use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::tools::digest_bytes;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactHandle {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        // Validate an existing destination, but do not populate the workspace
        // with runtime state until an output actually needs externalization.
        if root.try_exists()? {
            fs::create_dir_all(&root)?;
        }
        Ok(Self { root })
    }

    /// Stores `content` under a content-addressed id (`{label}-{sha256}`).
    /// Byte-identical repeats (e.g. the same >16 KiB tool output produced on
    /// consecutive turns) reuse the existing file instead of writing another
    /// timestamped duplicate: the write is skipped when a file with the same
    /// id already holds exactly `content.len()` bytes.
    pub fn put(&self, label: &str, content: &[u8]) -> io::Result<ArtifactHandle> {
        let safe_label: String = label
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let id = format!(
            "{}-{}",
            safe_label,
            digest_bytes(b"slim-artifact-v1", content)
        );
        let path = self.root.join(&id);
        let already_stored = fs::metadata(&path)
            .map(|metadata| metadata.len() == content.len() as u64)
            .unwrap_or(false);
        if !already_stored {
            fs::create_dir_all(&self.root)?;
            fs::write(&path, content)?;
        }
        Ok(ArtifactHandle {
            id,
            path,
            size: content.len() as u64,
        })
    }

    pub fn read(&self, handle: &ArtifactHandle) -> io::Result<Vec<u8>> {
        fs::read(&handle.path)
    }
}

#[cfg(test)]
mod tests {
    use super::ArtifactStore;

    #[test]
    fn identical_content_reuses_the_same_artifact_file() {
        let root = std::env::temp_dir().join(format!(
            "slim-artifact-dedup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root).expect("store");
        assert!(
            !root.exists(),
            "unused artifact stores must not create workspace entries"
        );
        let first = store.put("tool-read", b"repeated output").expect("put");
        let second = store.put("tool-read", b"repeated output").expect("put");
        let other = store.put("tool-read", b"different output").expect("put");
        assert_eq!(first.id, second.id);
        assert_eq!(first.path, second.path);
        assert_ne!(first.id, other.id);
        assert_eq!(store.read(&second).expect("read"), b"repeated output");
        assert_eq!(
            std::fs::read_dir(&root).expect("readdir").count(),
            2,
            "identical content must not write a second file"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn conflicting_artifact_destination_is_an_error_at_initialization_or_first_write() {
        let root = std::env::temp_dir().join(format!(
            "slim-artifact-conflict-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&root, b"preserve").unwrap();
        assert!(ArtifactStore::new(&root).is_err());
        std::fs::remove_file(&root).unwrap();
        let store = ArtifactStore::new(&root).unwrap();
        std::fs::write(&root, b"preserve").unwrap();
        assert!(store.put("output", b"payload").is_err());
        assert_eq!(std::fs::read(&root).unwrap(), b"preserve");
        std::fs::remove_file(&root).unwrap();
    }
}
