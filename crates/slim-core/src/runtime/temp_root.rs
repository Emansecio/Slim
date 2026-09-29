//! Diretório temporário de teste compartilhado pelos testes internos do runtime.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique temporary directory removed on drop, also when the test panics.
pub(super) struct TempRoot(PathBuf);

impl TempRoot {
    /// Creates the directory.
    pub(super) fn new(label: &str) -> Self {
        let root = Self::reserved(label);
        std::fs::create_dir_all(&root.0).unwrap();
        root
    }

    /// Reserves a unique path without creating it, for tests that assert the
    /// code under test creates (or does not create) it.
    pub(super) fn reserved(label: &str) -> Self {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slim-runtime-{label}-{}-{nanos}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        Self(path)
    }
}

impl std::ops::Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
