//! Diretorio temporario com limpeza automatica para testes de integracao.
//!
//! O nome combina rotulo, pid, relogio e um contador do processo, entao testes
//! paralelos (e execucoes concorrentes do mesmo binario) nunca compartilham a
//! raiz. `Drop` remove a arvore inclusive quando o teste entra em panico.
#![allow(dead_code)]

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

pub struct TempRoot(PathBuf);

impl TempRoot {
    /// Cria um diretorio novo e vazio sob o diretorio temporario do sistema.
    pub fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "slim-{label}-{}-{nanos}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Deref for TempRoot {
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
