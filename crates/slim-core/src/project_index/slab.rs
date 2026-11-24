//! Shared UTF-8 file slab with line-start offsets, SHA-256 digests, and
//! lazy-cached structural [`OutlineSymbol`] extraction.

use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::tools::outline::{extract_outline, OutlineSymbol};

/// Total RAM budget per project for cached UTF-8 file contents (128 MiB).
pub const MAX_SLAB_BYTES: usize = 128 * 1024 * 1024;
/// Individual file size cap for caching in the slab (2 MiB).
pub const MAX_SLAB_FILE_BYTES: usize = 2 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Lightweight file version stamp (length + mtime + platform file identity).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedFileVersion {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub dev_id: Option<u64>,
    pub file_id: Option<u64>,
}

impl CachedFileVersion {
    pub fn from_metadata(meta: &Metadata) -> Self {
        #[cfg(unix)]
        let (dev_id, file_id) = {
            use std::os::unix::fs::MetadataExt;
            (Some(meta.dev()), Some(meta.ino()))
        };
        #[cfg(not(unix))]
        let (dev_id, file_id) = (None, None);
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            dev_id,
            file_id,
        }
    }

    pub fn fast_stamp_file_id(&self) -> Option<String> {
        #[cfg(unix)]
        {
            return Some(format!("{}:{}", self.dev_id?, self.file_id?));
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

/// Immutable cached representation of a UTF-8 text file in RAM.
#[derive(Debug)]
pub struct CachedFile {
    pub path: PathBuf,
    pub version: CachedFileVersion,
    pub text: Arc<str>,
    pub line_starts: Arc<[u32]>,
    pub sha256: [u8; 32],
    outline: OnceLock<Arc<[OutlineSymbol]>>,
}

impl CachedFile {
    pub(crate) fn from_utf8(path: PathBuf, version: CachedFileVersion, text: String) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let sha256: [u8; 32] = hasher.finalize().into();
        let line_starts = compute_line_starts(&text);
        Self {
            path,
            version,
            text: Arc::from(text),
            line_starts: Arc::from(line_starts),
            sha256,
            outline: OnceLock::new(),
        }
    }

    /// Number of lines in `self.text` (0 for an empty file).
    pub fn line_count(&self) -> usize {
        if self.text.is_empty() {
            0
        } else {
            self.line_starts.len()
        }
    }

    /// Returns the cached structural outline for this file, extracting it once
    /// on first demand.
    pub fn outline(&self) -> Arc<[OutlineSymbol]> {
        Arc::clone(self.outline.get_or_init(|| {
            let symbols = extract_outline(&self.path, &self.text);
            Arc::from(symbols)
        }))
    }

    /// Returns a 1-based line slice (`start_line..=end_line`) if within bounds.
    pub fn line_slice(&self, start_line: usize, end_line: usize) -> &str {
        if self.text.is_empty() || start_line == 0 || end_line < start_line {
            return "";
        }
        let total = self.line_starts.len();
        if start_line > total {
            return "";
        }
        let start_byte = self.line_starts[start_line - 1] as usize;
        let end_byte = if end_line >= total {
            self.text.len()
        } else {
            self.line_starts[end_line] as usize
        };
        self.text.get(start_byte..end_byte).unwrap_or("")
    }
}

fn compute_line_starts(text: &str) -> Vec<u32> {
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut starts = Vec::with_capacity((bytes.len() / 40).max(8));
    starts.push(0);
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' && idx + 1 < bytes.len() {
            if let Ok(pos) = u32::try_from(idx + 1) {
                starts.push(pos);
            }
        }
    }
    starts
}

#[derive(Debug)]
struct SlabSlot {
    file: Arc<CachedFile>,
    last_used: u64,
    bytes: usize,
}

/// Bounded LRU cache of [`CachedFile`] entries shared by all sessions of a project.
#[derive(Debug)]
pub(crate) struct FileSlab {
    entries: HashMap<PathBuf, SlabSlot>,
    total_bytes: usize,
    max_bytes: usize,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl FileSlab {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            total_bytes: 0,
            max_bytes,
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    pub(crate) fn stats(&self) -> (usize, usize, u64, u64) {
        (self.entries.len(), self.total_bytes, self.hits, self.misses)
    }

    /// Looks up `path` without calling `stat()` when `trust_watcher` is true.
    pub(crate) fn get_if_valid(
        &mut self,
        path: &Path,
        trust_watcher: bool,
    ) -> Option<Arc<CachedFile>> {
        if !trust_watcher {
            let meta = fs::metadata(path).ok()?;
            let current = CachedFileVersion::from_metadata(&meta);
            let matches = self
                .entries
                .get(path)
                .is_some_and(|slot| slot.file.version == current);
            if !matches {
                self.remove(path);
                return None;
            }
        }
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        if let Some(slot) = self.entries.get_mut(path) {
            slot.last_used = clock;
            self.hits = self.hits.saturating_add(1);
            return Some(Arc::clone(&slot.file));
        }
        None
    }

    /// Looks up `path` verifying against already-obtained `meta`.
    pub(crate) fn get_matching_meta(
        &mut self,
        path: &Path,
        meta: &Metadata,
    ) -> Option<Arc<CachedFile>> {
        let current = CachedFileVersion::from_metadata(meta);
        let matches = self
            .entries
            .get(path)
            .is_some_and(|slot| slot.file.version == current);
        if !matches {
            self.remove(path);
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        if let Some(slot) = self.entries.get_mut(path) {
            slot.last_used = clock;
            self.hits = self.hits.saturating_add(1);
            return Some(Arc::clone(&slot.file));
        }
        None
    }

    pub(crate) fn record_miss(&mut self) {
        self.misses = self.misses.saturating_add(1);
    }

    pub(crate) fn insert(&mut self, file: Arc<CachedFile>) {
        let bytes = file.text.len();
        if bytes > MAX_SLAB_FILE_BYTES || bytes > self.max_bytes {
            self.remove(&file.path);
            return;
        }
        if let Some(prev) = self.entries.remove(&file.path) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.bytes);
        }
        while self.total_bytes.saturating_add(bytes) > self.max_bytes && !self.entries.is_empty() {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest_key) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            }
        }
        self.clock = self.clock.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.entries.insert(
            file.path.clone(),
            SlabSlot {
                file,
                last_used: self.clock,
                bytes,
            },
        );
    }

    pub(crate) fn remove(&mut self, path: &Path) {
        if let Some(removed) = self.entries.remove(path) {
            self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }
}

/// Reads a UTF-8 file from disk if it fits [`MAX_SLAB_FILE_BYTES`] and is not binary.
pub(crate) fn load_utf8_for_slab(path: &Path) -> io::Result<Option<CachedFile>> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.len() as usize > MAX_SLAB_FILE_BYTES {
        return Ok(None);
    }
    let raw = fs::read(path)?;
    let sniff = &raw[..raw.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(raw) else {
        return Ok(None);
    };
    let version = CachedFileVersion::from_metadata(&meta);
    Ok(Some(CachedFile::from_utf8(path.to_path_buf(), version, text)))
}
