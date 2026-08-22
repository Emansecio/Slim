use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

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
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        );
        let path = self.root.join(&id);
        fs::write(&path, content)?;
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
