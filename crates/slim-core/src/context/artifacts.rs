use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

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
    /// timestamped duplicate: the write is skipped only when the existing
    /// regular file holds the same bytes.
    pub fn put(&self, label: &str, content: &[u8]) -> io::Result<ArtifactHandle> {
        let staged = self.stage(label, content)?;
        self.commit_staged(staged)
    }

    pub(crate) fn stage(&self, label: &str, content: &[u8]) -> io::Result<StagedArtifact> {
        let handle = self.preview(label, content);
        match self.read(&handle) {
            Ok(existing) if existing == content => {
                return Ok(StagedArtifact {
                    handle,
                    temp_path: None,
                });
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "artifact id already contains different content",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
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
            self.read(&staged.handle)?;
            return Ok(staged.handle);
        };
        match self.read(&staged.handle) {
            Ok(_) => {
                fs::remove_file(temp_path)?;
                return Ok(staged.handle);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                let _ = fs::remove_file(temp_path);
                return Err(error);
            }
        }
        // Create the final name only if absent; rename would replace a file
        // another process published between the read above and this step.
        match fs::hard_link(&temp_path, &staged.handle.path) {
            Ok(()) => {
                fs::remove_file(temp_path)?;
                Ok(staged.handle)
            }
            Err(error) => {
                let result = match self.read(&staged.handle) {
                    Ok(_) => Ok(staged.handle),
                    Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => Err(error),
                    Err(read_error) => Err(read_error),
                };
                let _ = fs::remove_file(temp_path);
                result
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
        let (label, expected_digest) = handle
            .id
            .rsplit_once('-')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid artifact id"))?;
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || expected_digest.len() != 64
            || !expected_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || handle.path != self.root.join(&handle.id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid artifact handle",
            ));
        }

        let metadata = fs::symlink_metadata(&handle.path)?;
        if !regular_artifact(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "artifact is not a regular file",
            ));
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        #[cfg(windows)]
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        let mut file = options.open(&handle.path)?;
        let metadata = file.metadata()?;
        if !regular_artifact(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "artifact is not a regular file",
            ));
        }
        if metadata.len() != handle.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "artifact size differs from handle",
            ));
        }
        let mut content = Vec::new();
        file.read_to_end(&mut content)?;
        if content.len() as u64 != handle.size
            || digest_bytes(b"slim-artifact-v1", &content) != expected_digest
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "artifact content differs from id",
            ));
        }
        Ok(content)
    }

    /// Resolve an opaque id only within this store, then verify its contents.
    pub(crate) fn read_id(&self, id: &str) -> io::Result<Vec<u8>> {
        let (label, digest) = id
            .rsplit_once('-')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid artifact id"))?;
        if label.is_empty()
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid artifact id",
            ));
        }
        let path = self.root.join(id);
        let size = fs::symlink_metadata(&path)?.len();
        self.read(&ArtifactHandle {
            id: id.to_owned(),
            path,
            size,
        })
    }
}

fn regular_artifact(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        metadata.is_file() && metadata.file_attributes() & 0x400 == 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.is_file() && !metadata.file_type().is_symlink()
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
    fn same_size_corruption_is_not_read_or_reused() {
        let root = std::env::temp_dir().join(format!(
            "slim-artifact-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root).unwrap();
        let handle = store.put("output", b"original").unwrap();
        std::fs::write(&handle.path, b"corrupt!").unwrap();

        assert_eq!(
            store.read(&handle).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.put("output", b"original").unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&handle.path).unwrap(), b"corrupt!");

        std::fs::remove_file(&handle.path).unwrap();
        let staged = store.stage("output", b"original").unwrap();
        std::fs::write(&handle.path, b"corrupt!").unwrap();
        assert_eq!(
            store.commit_staged(staged).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&handle.path).unwrap(), b"corrupt!");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlink_with_valid_content_is_not_an_artifact() {
        let root = std::env::temp_dir().join(format!(
            "slim-artifact-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        let handle = store.preview("output", b"original");
        let target = root.join("target");
        std::fs::write(&target, b"original").unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &handle.path);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, &handle.path);
        if let Err(error) = linked {
            std::fs::remove_dir_all(root).unwrap();
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || cfg!(windows) && error.raw_os_error() == Some(1314)
            {
                return; // Symlink creation can require elevated privileges on Windows.
            }
            panic!("symlink creation failed: {error}");
        }

        assert_eq!(
            store.read(&handle).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.put("output", b"original").unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        std::fs::remove_dir_all(root).unwrap();
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
