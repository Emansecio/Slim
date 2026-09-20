use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

pub(crate) struct StagedArtifact {
    handle: ArtifactHandle,
    temp_path: Option<PathBuf>,
}

static STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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
        let staged = self.stage(label, content)?;
        self.commit_staged(staged)
    }

    pub(crate) fn stage(&self, label: &str, content: &[u8]) -> io::Result<StagedArtifact> {
        let handle = self.preview(label, content);
        let path = handle.path.clone();
        let already_stored = fs::metadata(&path)
            .map(|metadata| metadata.len() == content.len() as u64)
            .unwrap_or(false);
        if already_stored {
            return Ok(StagedArtifact {
                handle,
                temp_path: None,
            });
        }
        fs::create_dir_all(&self.root)?;
        let temp_path = loop {
            let sequence = STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = self.root.join(format!(
                ".slim-artifact-stage-{}-{sequence}",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(content) {
                        drop(file);
                        let _ = fs::remove_file(&candidate);
                        return Err(error);
                    }
                    break candidate;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        Ok(StagedArtifact {
            handle,
            temp_path: Some(temp_path),
        })
    }

    pub(crate) fn discard_staged(staged: StagedArtifact) -> io::Result<()> {
        match staged.temp_path {
            Some(path) => fs::remove_file(path),
            None => Ok(()),
        }
    }

    pub(crate) fn commit_staged(&self, staged: StagedArtifact) -> io::Result<ArtifactHandle> {
        let Some(temp_path) = staged.temp_path else {
            return Ok(staged.handle);
        };
        if fs::metadata(&staged.handle.path)
            .map(|metadata| metadata.len() == staged.handle.size)
            .unwrap_or(false)
        {
            fs::remove_file(temp_path)?;
            return Ok(staged.handle);
        }
        match fs::rename(&temp_path, &staged.handle.path) {
            Ok(()) => Ok(staged.handle),
            Err(_error)
                if fs::metadata(&staged.handle.path)
                    .map(|metadata| metadata.len() == staged.handle.size)
                    .unwrap_or(false) =>
            {
                fs::remove_file(temp_path)?;
                Ok(staged.handle)
            }
            Err(error) => {
                let _ = fs::remove_file(temp_path);
                Err(error)
            }
        }
    }

    /// Computes the stable content-addressed identity without touching disk.
    /// Callers can validate references and budgets before committing a file.
    pub(crate) fn preview(&self, label: &str, content: &[u8]) -> ArtifactHandle {
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
        ArtifactHandle {
            id,
            path,
            size: content.len() as u64,
        }
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
    fn discarding_one_private_stage_cannot_remove_another_committed_artifact() {
        let root = std::env::temp_dir().join(format!(
            "slim-artifact-stage-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root).unwrap();
        let cancelled = store.stage("context-history", b"same transcript").unwrap();
        let committed = store.stage("context-history", b"same transcript").unwrap();
        let handle = store.commit_staged(committed).unwrap();

        ArtifactStore::discard_staged(cancelled).unwrap();

        assert_eq!(store.read(&handle).unwrap(), b"same transcript");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
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
