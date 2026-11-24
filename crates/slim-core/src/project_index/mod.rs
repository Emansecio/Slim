//! Unified per-project index daemon (`L1` / `L3` local cache layer).
//!
//! One [`ProjectIndexDaemon`] exists per canonical workspace root in the
//! process and is shared across all interactive sessions, subagents, swarm
//! workers, headless invocations, and the Desktop UI sidebar.
//!
//! Responsibilities:
//! - Native macOS FSEvents watcher (zero external crates) driving monotonic
//!   `fs_epoch` and `git_dirty_epoch` counters.
//! - Shared UTF-8 file slab (128 MiB LRU) with line-start offsets, SHA-256
//!   digests, and lazily cached [`crate::tools::outline::OutlineSymbol`] lists.
//! - Incremental trigram inverted index with atomic `.slim/cache/trigram.v1.idx`
//!   snapshots for sub-millisecond literal/case-variant search candidate pruning.
//! - Epoch-validated LRU cache for read-only `code_intel` responses.
//! - Fast O(1)-cached `workspace_tree_digest` used by `ValidationReceiptCache`.

mod code_intel_cache;
mod fsevents;
pub mod graph;
pub mod slab;
mod trigram;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, OnceLock, RwLock, Weak,
};
use std::thread;
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use sha2::{Digest, Sha256};

use crate::tools::{is_macos_metadata_file, SKIP_DIR_NAMES};

pub(crate) use code_intel_cache::CachedCodeIntelOutcome;
pub use graph::{GraphEdgeHit, GraphSymbol, ImpactReport};
pub use slab::{CachedFile, CachedFileVersion, MAX_SLAB_BYTES, MAX_SLAB_FILE_BYTES};

const TREE_WALK_MAX_FILES: usize = 50_000;
const TRIGRAM_WARM_MAX_FILES: usize = 12_000;
const MAX_DAEMONS_PER_PROCESS: usize = 24;
const FALLBACK_TREE_TTL: Duration = Duration::from_secs(2);

/// Direct child entry inside a workspace directory, ready for UI/tool listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirChild {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Default)]
pub struct IndexedFileTree {
    /// Workspace-relative forward-slash paths sorted shallower-first then lexicographic.
    pub files: Vec<String>,
    /// True if the initial scan hit `TREE_WALK_MAX_FILES`.
    pub truncated: bool,
    /// Immediate children per canonical directory (populated lazily or on scan).
    dirs: HashMap<PathBuf, Arc<[DirChild]>>,
    built_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectIndexStats {
    pub fs_epoch: u64,
    pub git_dirty_epoch: u64,
    pub indexed_files: usize,
    pub trigram_docs: usize,
    pub trigram_ready: bool,
    pub graph_files: usize,
    pub graph_symbols: usize,
    pub graph_ready: bool,
    pub slab_files: usize,
    pub slab_bytes: usize,
    pub slab_hits: u64,
    pub slab_misses: u64,
    pub code_intel_entries: usize,
    pub code_intel_hits: u64,
    pub code_intel_misses: u64,
    pub watcher_active: bool,
}

struct WakeHookEntry {
    id: u64,
    hook: Arc<dyn Fn() + Send + Sync>,
}

/// Drops a registered wake hook when the owner (e.g. UI sidebar) switches projects.
pub struct WakeHookGuard {
    daemon: Weak<ProjectIndexDaemon>,
    id: u64,
}

impl Drop for WakeHookGuard {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.upgrade() {
            let mut hooks = lock_or_recover(&daemon.wake_hooks);
            hooks.retain(|entry| entry.id != self.id);
        }
    }
}

/// Process-wide singleton index for a single canonical workspace root.
pub struct ProjectIndexDaemon {
    canonical_root: PathBuf,
    fs_epoch: AtomicU64,
    external_fs_epoch: AtomicU64,
    git_dirty_epoch: AtomicU64,
    tree_dirty: AtomicBool,
    watcher_active: AtomicBool,
    tree: RwLock<Arc<IndexedFileTree>>,
    slab: Mutex<slab::FileSlab>,
    trigrams: RwLock<trigram::TrigramIndex>,
    graph: RwLock<graph::SemanticGraph>,
    code_intel_cache: Mutex<code_intel_cache::CodeIntelLru>,
    tree_digest_cache: Mutex<Option<(u64, [u8; 32])>>,
    wake_hooks: Mutex<Vec<WakeHookEntry>>,
    next_hook_id: AtomicU64,
    _watcher: Mutex<Option<fsevents::WatcherGuard>>,
}

impl std::fmt::Debug for ProjectIndexDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectIndexDaemon")
            .field("canonical_root", &self.canonical_root)
            .field("fs_epoch", &self.fs_epoch.load(Ordering::Relaxed))
            .field("watcher_active", &self.watcher_active.load(Ordering::Relaxed))
            .finish()
    }
}

static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<ProjectIndexDaemon>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<ProjectIndexDaemon>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn read_or_recover<T>(rw: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match rw.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_or_recover<T>(rw: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match rw.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Returns (or lazily spawns) the unique [`ProjectIndexDaemon`] for `root`.
pub fn for_workspace(root: &Path) -> io::Result<Arc<ProjectIndexDaemon>> {
    let canonical = fs::canonicalize(root)?;
    {
        let map = lock_or_recover(registry());
        if let Some(existing) = map.get(&canonical) {
            return Ok(Arc::clone(existing));
        }
    }

    let daemon = ProjectIndexDaemon::create(canonical.clone());
    let mut map = lock_or_recover(registry());
    if let Some(existing) = map.get(&canonical) {
        return Ok(Arc::clone(existing));
    }
    if map.len() >= MAX_DAEMONS_PER_PROCESS {
        // Evict one entry whose Arc is only referenced by the registry, if any.
        let evict_key = map
            .iter()
            .find(|(_, v)| Arc::strong_count(v) == 1)
            .map(|(k, _)| k.clone());
        if let Some(key) = evict_key {
            map.remove(&key);
        }
    }
    map.insert(canonical, Arc::clone(&daemon));
    Ok(daemon)
}

/// Returns an already-registered daemon for `canonical_root` without spawning one.
pub fn existing_for_workspace(canonical_root: &Path) -> Option<Arc<ProjectIndexDaemon>> {
    let map = lock_or_recover(registry());
    map.get(canonical_root).cloned()
}

/// Finds an already-registered daemon whose canonical root encloses `abs_path`.
pub fn existing_for_path(abs_path: &Path) -> Option<Arc<ProjectIndexDaemon>> {
    let map = lock_or_recover(registry());
    map.iter()
        .filter(|(root, _)| abs_path.starts_with(root))
        .max_by_key(|(root, _)| root.as_os_str().len())
        .map(|(_, daemon)| Arc::clone(daemon))
}

/// Fast path to read a cached UTF-8 file if any registered project daemon owns `abs_path`.
pub fn read_cached_for_path(abs_path: &Path) -> Option<Arc<CachedFile>> {
    let daemon = existing_for_path(abs_path)?;
    daemon.read_file_cached(abs_path).ok().flatten()
}

/// Aggregated statistics across all active project daemons (for X-Ray UI).
pub fn aggregate_stats() -> ProjectIndexStats {
    let map = lock_or_recover(registry());
    let mut total = ProjectIndexStats::default();
    for daemon in map.values() {
        let s = daemon.stats();
        total.indexed_files = total.indexed_files.saturating_add(s.indexed_files);
        total.trigram_docs = total.trigram_docs.saturating_add(s.trigram_docs);
        total.trigram_ready |= s.trigram_ready;
        total.graph_files = total.graph_files.saturating_add(s.graph_files);
        total.graph_symbols = total.graph_symbols.saturating_add(s.graph_symbols);
        total.graph_ready |= s.graph_ready;
        total.slab_files = total.slab_files.saturating_add(s.slab_files);
        total.slab_bytes = total.slab_bytes.saturating_add(s.slab_bytes);
        total.slab_hits = total.slab_hits.saturating_add(s.slab_hits);
        total.slab_misses = total.slab_misses.saturating_add(s.slab_misses);
        total.code_intel_entries = total.code_intel_entries.saturating_add(s.code_intel_entries);
        total.code_intel_hits = total.code_intel_hits.saturating_add(s.code_intel_hits);
        total.code_intel_misses = total.code_intel_misses.saturating_add(s.code_intel_misses);
        total.watcher_active |= s.watcher_active;
    }
    total
}

impl ProjectIndexDaemon {
    fn create(canonical_root: PathBuf) -> Arc<Self> {
        let daemon = Arc::new(Self {
            canonical_root: canonical_root.clone(),
            fs_epoch: AtomicU64::new(1),
            external_fs_epoch: AtomicU64::new(1),
            git_dirty_epoch: AtomicU64::new(1),
            tree_dirty: AtomicBool::new(true),
            watcher_active: AtomicBool::new(false),
            tree: RwLock::new(Arc::new(IndexedFileTree::default())),
            slab: Mutex::new(slab::FileSlab::new(MAX_SLAB_BYTES)),
            trigrams: RwLock::new(trigram::TrigramIndex::default()),
            graph: RwLock::new(graph::SemanticGraph::default()),
            code_intel_cache: Mutex::new(code_intel_cache::CodeIntelLru::default()),
            tree_digest_cache: Mutex::new(None),
            wake_hooks: Mutex::new(Vec::new()),
            next_hook_id: AtomicU64::new(1),
            _watcher: Mutex::new(None),
        });

        let weak_for_fs = Arc::downgrade(&daemon);
        let cb: fsevents::FsChangeCallback = Arc::new(move |paths: &[PathBuf]| {
            if let Some(d) = weak_for_fs.upgrade() {
                d.handle_fs_events(paths);
            }
        });
        if let Some(guard) = fsevents::start_watcher(&canonical_root, cb) {
            daemon.watcher_active.store(true, Ordering::Release);
            *lock_or_recover(&daemon._watcher) = Some(guard);
        }

        // Kick off background trigram warm-up so searches become sub-ms
        // without blocking session creation.
        let weak_for_warm = Arc::downgrade(&daemon);
        let _ = thread::Builder::new()
            .name("slim-trigram-warm".to_owned())
            .spawn(move || {
                if let Some(d) = weak_for_warm.upgrade() {
                    d.warm_trigram_index();
                }
            });

        daemon
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn fs_epoch(&self) -> u64 {
        self.fs_epoch.load(Ordering::Acquire)
    }

    pub fn git_dirty_epoch(&self) -> u64 {
        self.git_dirty_epoch.load(Ordering::Acquire)
    }

    pub fn watcher_active(&self) -> bool {
        self.watcher_active.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> ProjectIndexStats {
        let tree = read_or_recover(&self.tree);
        let (slab_files, slab_bytes, slab_hits, slab_misses) =
            lock_or_recover(&self.slab).stats();
        let (trigram_docs, trigram_ready) = {
            let tri = read_or_recover(&self.trigrams);
            (tri.doc_count(), tri.is_ready())
        };
        let (graph_files, graph_symbols, graph_ready) = {
            let g = read_or_recover(&self.graph);
            (g.file_count(), g.symbol_count(), g.is_ready())
        };
        let (ci_entries, ci_hits, ci_misses) =
            lock_or_recover(&self.code_intel_cache).stats();
        ProjectIndexStats {
            fs_epoch: self.fs_epoch(),
            git_dirty_epoch: self.git_dirty_epoch(),
            indexed_files: tree.files.len(),
            trigram_docs,
            trigram_ready,
            graph_files,
            graph_symbols,
            graph_ready,
            slab_files,
            slab_bytes,
            slab_hits,
            slab_misses,
            code_intel_entries: ci_entries,
            code_intel_hits: ci_hits,
            code_intel_misses: ci_misses,
            watcher_active: self.watcher_active(),
        }
    }

    /// Registers a lightweight callback invoked when FSEvents or local writes
    /// advance `fs_epoch` / `git_dirty_epoch`.
    pub fn register_wake_hook(
        self: &Arc<Self>,
        hook: Arc<dyn Fn() + Send + Sync>,
    ) -> WakeHookGuard {
        let id = self.next_hook_id.fetch_add(1, Ordering::Relaxed);
        lock_or_recover(&self.wake_hooks).push(WakeHookEntry { id, hook });
        WakeHookGuard {
            daemon: Arc::downgrade(self),
            id,
        }
    }

    fn fire_wake_hooks(&self) {
        let hooks: Vec<Arc<dyn Fn() + Send + Sync>> = {
            let guard = lock_or_recover(&self.wake_hooks);
            guard.iter().map(|e| Arc::clone(&e.hook)).collect()
        };
        for hook in hooks {
            hook();
        }
    }

    /// Returns a fresh snapshot of workspace-relative forward-slash file paths
    /// sorted shallower-first then lexicographic.
    pub fn snapshot_files(&self, limit: usize) -> (Vec<String>, bool) {
        let tree = self.ensure_tree();
        if limit == 0 {
            return (Vec::new(), !tree.files.is_empty());
        }
        let truncated = tree.truncated || tree.files.len() > limit;
        let out = tree.files.iter().take(limit).cloned().collect();
        (out, truncated)
    }

    /// Returns all indexed workspace-relative paths without copying when
    /// callers only need to iterate/filter.
    pub fn all_files_arc(&self) -> (Arc<IndexedFileTree>, usize) {
        let tree = self.ensure_tree();
        let len = tree.files.len();
        (tree, len)
    }

    /// Returns direct children of `abs_dir` from the cached tree (or scans that
    /// single directory once if not yet materialized).
    pub fn list_dir(&self, abs_dir: &Path) -> io::Result<Arc<[DirChild]>> {
        let canonical_dir = fs::canonicalize(abs_dir).unwrap_or_else(|_| abs_dir.to_path_buf());
        let needs_refresh = self.tree_needs_refresh();
        if !needs_refresh {
            let tree = read_or_recover(&self.tree);
            if let Some(children) = tree.dirs.get(&canonical_dir) {
                return Ok(Arc::clone(children));
            }
        }
        let children = scan_single_dir(&canonical_dir)?;
        let arc: Arc<[DirChild]> = Arc::from(children);
        {
            let mut tree_guard = write_or_recover(&self.tree);
            let current = Arc::make_mut(&mut *tree_guard);
            current.dirs.insert(canonical_dir, Arc::clone(&arc));
        }
        Ok(arc)
    }

    /// Reads a UTF-8 file from the shared RAM slab, verifying against `meta`
    /// and populating on miss. Used by [`crate::tools::read`] to avoid extra
    /// `open`/`read`/`Sha256` calls while remaining immune to sub-ms FSEvents lag.
    pub fn read_file_cached_with_meta(
        &self,
        abs_path: &Path,
        meta: &fs::Metadata,
    ) -> io::Result<Option<Arc<CachedFile>>> {
        if !meta.is_file() || meta.len() as usize > MAX_SLAB_FILE_BYTES {
            return Ok(None);
        }
        {
            let mut slab = lock_or_recover(&self.slab);
            if let Some(hit) = slab.get_matching_meta(abs_path, meta) {
                return Ok(Some(hit));
            }
            slab.record_miss();
        }
        let loaded = slab::load_utf8_for_slab(abs_path)?;
        if let Some(file) = loaded {
            let arc = Arc::new(file);
            lock_or_recover(&self.slab).insert(Arc::clone(&arc));
            return Ok(Some(arc));
        }
        Ok(None)
    }

    /// Reads a UTF-8 file from the shared RAM slab, populating on miss.
    /// Returns `Ok(None)` for binary files or files exceeding [`MAX_SLAB_FILE_BYTES`].
    pub fn read_file_cached(&self, abs_path: &Path) -> io::Result<Option<Arc<CachedFile>>> {
        let trust = self.watcher_active();
        {
            let mut slab = lock_or_recover(&self.slab);
            if let Some(hit) = slab.get_if_valid(abs_path, trust) {
                return Ok(Some(hit));
            }
            slab.record_miss();
        }
        let loaded = slab::load_utf8_for_slab(abs_path)?;
        if let Some(file) = loaded {
            let arc = Arc::new(file);
            lock_or_recover(&self.slab).insert(Arc::clone(&arc));
            return Ok(Some(arc));
        }
        Ok(None)
    }

    /// Returns candidate file paths for a literal or case-insensitive search
    /// needle using the in-memory trigram index. Returns `None` if the index is
    /// still warming up or `needle.len() < 3`.
    pub fn search_candidates(&self, needle: &str) -> Option<Vec<PathBuf>> {
        let tri = read_or_recover(&self.trigrams);
        tri.query_candidates(needle)
    }

    /// Returns `true` when `abs_path` is already tracked in the ready trigram
    /// index as a live clean UTF-8 document (so absence from `search_candidates`
    /// proves the file cannot contain the needle).
    pub fn trigram_tracks_clean_file(&self, abs_path: &Path) -> bool {
        read_or_recover(&self.trigrams).tracks_clean_file(abs_path)
    }

    pub fn graph_definitions(&self, symbol: &str, limit: usize) -> Vec<GraphEdgeHit> {
        read_or_recover(&self.graph).definitions(symbol, limit)
    }

    pub fn graph_callers(&self, symbol: &str, limit: usize) -> Vec<GraphEdgeHit> {
        read_or_recover(&self.graph).callers(symbol, limit)
    }

    pub fn graph_callees(&self, rel_path: &str, symbol: &str, limit: usize) -> Vec<GraphEdgeHit> {
        read_or_recover(&self.graph).callees(rel_path, symbol, limit)
    }

    pub fn graph_imports(&self, rel_path: &str) -> (Vec<String>, Vec<String>) {
        read_or_recover(&self.graph).imports_and_importers(rel_path)
    }

    pub fn graph_tests_covering(&self, rel_path: &str, limit: usize) -> Vec<String> {
        read_or_recover(&self.graph).tests_covering(rel_path, limit)
    }

    pub fn graph_impact(&self, target: &str, depth: usize, limit: usize) -> ImpactReport {
        read_or_recover(&self.graph).impact(target, depth, limit)
    }

    pub fn graph_file_symbols(&self, rel_path: &str) -> Vec<GraphSymbol> {
        read_or_recover(&self.graph).file_symbols(rel_path)
    }

    pub fn query_context_hint(&self, prompt: &str, max_bytes: usize) -> Option<String> {
        // Always lock trigrams before graph to keep lock ordering consistent.
        let tri = read_or_recover(&self.trigrams);
        let graph = read_or_recover(&self.graph);
        graph.query_context_hint(
            prompt,
            |needle| tri.query_candidates(needle),
            &self.canonical_root,
            max_bytes,
        )
    }

    /// Looks up a cached `code_intel` outcome valid for the current `fs_epoch`.
    pub(crate) fn lookup_code_intel(&self, key: &str) -> Option<CachedCodeIntelOutcome> {
        let epoch = self.fs_epoch();
        lock_or_recover(&self.code_intel_cache).get(key, epoch)
    }

    /// Stores a clean, read-only `code_intel` outcome tagged with `epoch`.
    pub(crate) fn store_code_intel(
        &self,
        key: String,
        epoch: u64,
        outcome: CachedCodeIntelOutcome,
    ) {
        if self.fs_epoch() != epoch {
            return;
        }
        lock_or_recover(&self.code_intel_cache).insert(key, epoch, outcome);
    }

    /// Synchronous invalidation/update invoked by `ToolRegistry` immediately
    /// after `write`, `patch`, or rollback — avoids any race with async FSEvents.
    pub fn notify_local_write(&self, abs_path: &Path, new_bytes: Option<&[u8]>) {
        let epoch = self.fs_epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        self.git_dirty_epoch.fetch_add(1, Ordering::AcqRel);
        self.tree_dirty.store(true, Ordering::Release);
        *lock_or_recover(&self.tree_digest_cache) = None;

        // Update or evict slab entry immediately.
        let mut updated_bytes: Option<(Vec<u8>, u64, u64, u32)> = None;
        if let Some(bytes) = new_bytes {
            if bytes.len() <= MAX_SLAB_FILE_BYTES && !bytes[..bytes.len().min(8192)].contains(&0) {
                if let (Ok(meta), Ok(text)) = (fs::metadata(abs_path), std::str::from_utf8(bytes)) {
                    let version = CachedFileVersion::from_metadata(&meta);
                    let (mtime_secs, mtime_nanos) = trigram::extract_mtime_parts(&meta);
                    let cached = Arc::new(CachedFile::from_utf8(
                        abs_path.to_path_buf(),
                        version,
                        text.to_owned(),
                    ));
                    lock_or_recover(&self.slab).insert(cached);
                    updated_bytes = Some((bytes.to_vec(), meta.len(), mtime_secs, mtime_nanos));
                } else {
                    lock_or_recover(&self.slab).remove(abs_path);
                }
            } else {
                lock_or_recover(&self.slab).remove(abs_path);
            }
        } else {
            lock_or_recover(&self.slab).remove(abs_path);
        }

        // Incrementally update trigram and graph indexes if ready (single disk read at most).
        if let Ok(rel) = abs_path.strip_prefix(&self.canonical_root) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if updated_bytes.is_none() {
                if let Ok(meta) = fs::metadata(abs_path) {
                    if meta.is_file() && meta.len() <= trigram::MAX_INDEXED_FILE_BYTES as u64 {
                        if let Ok(bytes) = fs::read(abs_path) {
                            let (secs, nanos) = trigram::extract_mtime_parts(&meta);
                            updated_bytes = Some((bytes, meta.len(), secs, nanos));
                        }
                    }
                }
            }
            {
                let mut tri = write_or_recover(&self.trigrams);
                if tri.is_ready() {
                    if let Some((ref bytes, len, secs, nanos)) = updated_bytes {
                        tri.upsert_bytes(rel_str.clone(), abs_path.to_path_buf(), len, secs, nanos, bytes);
                    } else {
                        tri.remove_path(abs_path);
                    }
                }
            }
            {
                let mut g = write_or_recover(&self.graph);
                if g.is_ready() {
                    if let Some((ref bytes, len, secs, nanos)) = updated_bytes {
                        g.upsert_bytes(&rel_str, len, secs, nanos, bytes);
                    } else {
                        g.remove_rel(&rel_str);
                    }
                }
            }
        }

        let _ = epoch;
        self.fire_wake_hooks();
    }

    pub fn notify_local_delete(&self, abs_path: &Path) {
        self.fs_epoch.fetch_add(1, Ordering::AcqRel);
        self.external_fs_epoch.fetch_add(1, Ordering::AcqRel);
        self.git_dirty_epoch.fetch_add(1, Ordering::AcqRel);
        self.tree_dirty.store(true, Ordering::Release);
        *lock_or_recover(&self.tree_digest_cache) = None;
        lock_or_recover(&self.slab).remove(abs_path);
        write_or_recover(&self.trigrams).remove_path(abs_path);
        if let Ok(rel) = abs_path.strip_prefix(&self.canonical_root) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            write_or_recover(&self.graph).remove_rel(&rel_str);
        }
        self.fire_wake_hooks();
    }

    /// Epoch bumped only on external FSEvents, deletes, or opaque workspace
    /// invalidations (not local path-tracked `notify_local_write` updates).
    pub fn external_fs_epoch(&self) -> u64 {
        self.external_fs_epoch.load(Ordering::Acquire)
    }

    /// Marks the whole workspace index dirty (e.g. after an opaque shell mutation).
    pub fn invalidate_workspace(&self) {
        self.fs_epoch.fetch_add(1, Ordering::AcqRel);
        self.external_fs_epoch.fetch_add(1, Ordering::AcqRel);
        self.git_dirty_epoch.fetch_add(1, Ordering::AcqRel);
        self.tree_dirty.store(true, Ordering::Release);
        *lock_or_recover(&self.tree_digest_cache) = None;
        lock_or_recover(&self.slab).clear();
        lock_or_recover(&self.code_intel_cache).clear();
        self.fire_wake_hooks();
    }

    /// Computes (and caches per `fs_epoch`) a deterministic SHA-256 digest of
    /// the workspace tree (all tracked/untracked non-ignored files' relative
    /// path + length + mtime + `.git/HEAD` / `.git/index` metadata).
    ///
    /// Returns `None` if the workspace is truncated (`> TREE_WALK_MAX_FILES`).
    pub fn workspace_tree_digest(&self) -> Option<[u8; 32]> {
        let epoch = self.fs_epoch();
        if self.watcher_active() {
            let guard = lock_or_recover(&self.tree_digest_cache);
            if let Some((cached_epoch, digest)) = *guard {
                if cached_epoch == epoch {
                    return Some(digest);
                }
            }
        }

        let tree = self.ensure_tree();
        if tree.truncated {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(b"slim-tree-digest-v1\n");

        // Include git HEAD + index state when present.
        for git_rel in [".git/HEAD", ".git/index"] {
            let p = self.canonical_root.join(git_rel);
            if let Ok(meta) = fs::metadata(&p) {
                let (secs, nanos) = trigram::extract_mtime_parts(&meta);
                hasher.update(git_rel.as_bytes());
                hasher.update(b":");
                hasher.update(meta.len().to_le_bytes());
                hasher.update(secs.to_le_bytes());
                hasher.update(nanos.to_le_bytes());
            }
        }

        for rel in &tree.files {
            let abs = self.canonical_root.join(rel);
            let Ok(meta) = fs::metadata(&abs) else {
                continue;
            };
            let (secs, nanos) = trigram::extract_mtime_parts(&meta);
            hasher.update(rel.as_bytes());
            hasher.update([0u8]);
            hasher.update(meta.len().to_le_bytes());
            hasher.update(secs.to_le_bytes());
            hasher.update(nanos.to_le_bytes());
        }
        let digest: [u8; 32] = hasher.finalize().into();
        if self.fs_epoch() == epoch {
            *lock_or_recover(&self.tree_digest_cache) = Some((epoch, digest));
        }
        Some(digest)
    }

    fn tree_needs_refresh(&self) -> bool {
        if self.tree_dirty.load(Ordering::Acquire) {
            return true;
        }
        if !self.watcher_active() {
            let tree = read_or_recover(&self.tree);
            return tree
                .built_at
                .is_none_or(|t| t.elapsed() >= FALLBACK_TREE_TTL);
        }
        let tree = read_or_recover(&self.tree);
        tree.built_at.is_none()
    }

    fn ensure_tree(&self) -> Arc<IndexedFileTree> {
        if !self.tree_needs_refresh() {
            return Arc::clone(&read_or_recover(&self.tree));
        }
        let epoch_before = self.fs_epoch();
        let rebuilt = Arc::new(build_indexed_tree(&self.canonical_root));
        let mut guard = write_or_recover(&self.tree);
        *guard = Arc::clone(&rebuilt);
        if self.fs_epoch() == epoch_before {
            self.tree_dirty.store(false, Ordering::Release);
        }
        rebuilt
    }

    fn handle_fs_events(&self, paths: &[PathBuf]) {
        let mut relevant_paths = Vec::new();
        let mut git_touched = false;
        for raw in paths {
            let rel = match raw.strip_prefix(&self.canonical_root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if is_ignored_fsevent_rel(rel, &mut git_touched) {
                continue;
            }
            relevant_paths.push(raw.clone());
        }
        if relevant_paths.is_empty() && !git_touched {
            return;
        }
        if !relevant_paths.is_empty() {
            // Skip FSEvents echoes of our own `notify_local_write` whose on-disk
            // metadata already matches the updated slab entry.
            let mut external_paths = Vec::with_capacity(relevant_paths.len());
            {
                let mut slab = lock_or_recover(&self.slab);
                for path in &relevant_paths {
                    let echoed_local = fs::metadata(path)
                        .ok()
                        .is_some_and(|meta| slab.get_matching_meta(path, &meta).is_some());
                    if !echoed_local {
                        slab.remove(path);
                        external_paths.push(path.clone());
                    }
                }
            }
            if external_paths.is_empty() {
                if git_touched {
                    self.git_dirty_epoch.fetch_add(1, Ordering::AcqRel);
                    *lock_or_recover(&self.tree_digest_cache) = None;
                    self.fire_wake_hooks();
                }
                return;
            }
            let relevant_paths = external_paths;
            self.fs_epoch.fetch_add(1, Ordering::AcqRel);
            self.external_fs_epoch.fetch_add(1, Ordering::AcqRel);
            self.git_dirty_epoch.fetch_add(1, Ordering::AcqRel);
            self.tree_dirty.store(true, Ordering::Release);
            *lock_or_recover(&self.tree_digest_cache) = None;
            let mut fs_payloads: Vec<(String, PathBuf, Option<(Vec<u8>, u64, u64, u32)>)> =
                Vec::with_capacity(relevant_paths.len());
            for path in &relevant_paths {
                let Ok(rel) = path.strip_prefix(&self.canonical_root) else {
                    continue;
                };
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let payload = fs::metadata(path).ok().and_then(|meta| {
                    if !meta.is_file() || meta.len() > trigram::MAX_INDEXED_FILE_BYTES as u64 {
                        return None;
                    }
                    let bytes = fs::read(path).ok()?;
                    let (secs, nanos) = trigram::extract_mtime_parts(&meta);
                    Some((bytes, meta.len(), secs, nanos))
                });
                fs_payloads.push((rel_str, path.clone(), payload));
            }
            {
                let mut tri = write_or_recover(&self.trigrams);
                if tri.is_ready() {
                    for (rel_str, path, payload) in &fs_payloads {
                        if let Some((bytes, len, secs, nanos)) = payload {
                            tri.upsert_bytes(rel_str.clone(), path.clone(), *len, *secs, *nanos, bytes);
                        } else {
                            tri.remove_path(path);
                        }
                    }
                    if tri.dirty_count() >= 64 {
                        let _ = tri.save_snapshot(&self.canonical_root);
                    }
                }
            }
            {
                let mut g = write_or_recover(&self.graph);
                if g.is_ready() {
                    for (rel_str, _, payload) in &fs_payloads {
                        if let Some((bytes, len, secs, nanos)) = payload {
                            g.upsert_bytes(rel_str, *len, *secs, *nanos, bytes);
                        } else {
                            g.remove_rel(rel_str);
                        }
                    }
                    if g.dirty_count() >= 64 {
                        let _ = g.save_snapshot(&self.canonical_root);
                    }
                }
            }
        } else if git_touched {
            self.git_dirty_epoch.fetch_add(1, Ordering::AcqRel);
            *lock_or_recover(&self.tree_digest_cache) = None;
        }
        self.fire_wake_hooks();
    }

    fn warm_trigram_index(&self) {
        let mut idx = trigram::TrigramIndex::load_snapshot(&self.canonical_root)
            .unwrap_or_default();
        let mut sem_graph = graph::SemanticGraph::load_snapshot(&self.canonical_root)
            .unwrap_or_default();
        let tree = self.ensure_tree();
        let mut live_set = std::collections::HashSet::with_capacity(tree.files.len());
        let mut live_rels = std::collections::HashSet::with_capacity(tree.files.len());
        let mut tri_changed = false;
        let mut graph_changed = false;

        for rel in tree.files.iter().take(TRIGRAM_WARM_MAX_FILES) {
            let abs = self.canonical_root.join(rel);
            let Ok(meta) = fs::metadata(&abs) else {
                continue;
            };
            if !meta.is_file() || meta.len() > trigram::MAX_INDEXED_FILE_BYTES as u64 {
                continue;
            }
            live_set.insert(abs.clone());
            live_rels.insert(rel.clone());
            let (secs, nanos) = trigram::extract_mtime_parts(&meta);
            let tri_fresh = idx.is_fresh(&abs, meta.len(), secs, nanos);
            let graph_fresh = sem_graph.is_fresh(rel, meta.len(), secs, nanos);
            if tri_fresh && graph_fresh {
                continue;
            }
            if let Ok(bytes) = fs::read(&abs) {
                if !tri_fresh {
                    idx.upsert_bytes(rel.clone(), abs, meta.len(), secs, nanos, &bytes);
                    tri_changed = true;
                }
                if !graph_fresh {
                    sem_graph.upsert_bytes(rel, meta.len(), secs, nanos, &bytes);
                    graph_changed = true;
                }
            }
        }

        // Prune stale paths loaded from older disk snapshots.
        if idx.retain_live(&live_set) {
            tri_changed = true;
        }
        if sem_graph.retain_live_rel(&live_rels) {
            graph_changed = true;
        }

        idx.mark_ready();
        sem_graph.mark_ready();
        if tri_changed && idx.doc_count() > 0 {
            let _ = idx.save_snapshot(&self.canonical_root);
        }
        if graph_changed && sem_graph.file_count() > 0 {
            let _ = sem_graph.save_snapshot(&self.canonical_root);
        }
        *write_or_recover(&self.trigrams) = idx;
        *write_or_recover(&self.graph) = sem_graph;
    }
}

fn is_ignored_fsevent_rel(rel: &Path, git_touched: &mut bool) -> bool {
    let mut comps = rel.components();
    let Some(first) = comps.next().and_then(|c| c.as_os_str().to_str()) else {
        return true;
    };
    if first == ".git" {
        if let Some(second) = comps.next().and_then(|c| c.as_os_str().to_str()) {
            if matches!(second, "HEAD" | "index" | "refs" | "packed-refs" | "MERGE_HEAD") {
                *git_touched = true;
            }
        }
        return true;
    }
    if first == ".slim" {
        return true;
    }
    if SKIP_DIR_NAMES.contains(&first) {
        return true;
    }
    if rel
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_macos_metadata_file)
    {
        return true;
    }
    false
}

fn build_indexed_tree(canonical_root: &Path) -> IndexedFileTree {
    let mut builder = WalkBuilder::new(canonical_root);
    builder
        .hidden(false)
        .follow_links(false)
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_str().unwrap_or("");
            if entry.depth() > 0 {
                if name == ".slim" || SKIP_DIR_NAMES.contains(&name) {
                    return false;
                }
                if is_macos_metadata_file(name) {
                    return false;
                }
            }
            true
        });

    let mut ranked: Vec<(usize, String)> = Vec::new();
    let mut dirs_map: HashMap<PathBuf, BTreeMap<String, DirChild>> = HashMap::new();
    let mut truncated = false;

    for entry in builder.build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
        if !is_dir && !is_file {
            continue;
        }
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) {
            dirs_map
                .entry(parent.to_path_buf())
                .or_default()
                .insert(
                    name.to_owned(),
                    DirChild {
                        name: name.to_owned(),
                        path: path.to_path_buf(),
                        is_dir,
                    },
                );
        }
        if is_file {
            if ranked.len() >= TREE_WALK_MAX_FILES {
                truncated = true;
                break;
            }
            if let Ok(rel) = path.strip_prefix(canonical_root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let depth = rel_str.bytes().filter(|&b| b == b'/').count();
                ranked.push((depth, rel_str));
            }
        }
    }

    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let files = ranked.into_iter().map(|(_, p)| p).collect();
    let dirs = dirs_map
        .into_iter()
        .map(|(dir, entries)| {
            let mut list: Vec<DirChild> = entries.into_values().collect();
            list.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
            (dir, Arc::from(list))
        })
        .collect();

    IndexedFileTree {
        files,
        truncated,
        dirs,
        built_at: Some(Instant::now()),
    }
}

fn scan_single_dir(dir: &Path) -> io::Result<Vec<DirChild>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".slim" || SKIP_DIR_NAMES.contains(&name.as_str()) || is_macos_metadata_file(&name) {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());
        out.push(DirChild {
            name,
            path: entry.path(),
            is_dir,
        });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "slim-project-index-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn slab_caches_utf8_outline_and_respects_local_writes() {
        let dir = make_temp_dir("slab");
        let file = dir.join("lib.rs");
        fs::write(&file, "pub fn alpha() -> u32 {\n    1\n}\n").unwrap();

        let daemon = for_workspace(&dir).expect("daemon");
        let canon_file = fs::canonicalize(&file).unwrap();
        let first = daemon.read_file_cached(&canon_file).unwrap().expect("cached");
        assert_eq!(first.line_count(), 3);
        assert_eq!(first.outline().len(), 1);
        assert_eq!(first.outline()[0].name, "alpha");

        // Second read must hit the RAM slab.
        let second = daemon.read_file_cached(&canon_file).unwrap().expect("hit");
        assert!(Arc::ptr_eq(&first, &second));
        assert!(daemon.stats().slab_hits >= 1);

        // Local write advances fs_epoch and updates slab synchronously.
        let epoch_before = daemon.fs_epoch();
        let new_src = "pub fn beta() -> u32 {\n    2\n}\n";
        fs::write(&canon_file, new_src).unwrap();
        daemon.notify_local_write(&canon_file, Some(new_src.as_bytes()));
        assert!(daemon.fs_epoch() > epoch_before);
        let third = daemon.read_file_cached(&canon_file).unwrap().expect("updated");
        assert_eq!(third.outline()[0].name, "beta");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trigram_index_filters_candidates_and_roundtrips_snapshot() {
        let dir = make_temp_dir("trigram");
        let a = dir.join("alpha.rs");
        let b = dir.join("beta.rs");
        fs::write(&a, "fn unique_needle_xyz() {}\n").unwrap();
        fs::write(&b, "fn other_symbol() {}\n").unwrap();

        let canon = fs::canonicalize(&dir).unwrap();
        let canon_a = fs::canonicalize(&a).unwrap();
        let canon_b = fs::canonicalize(&b).unwrap();

        let mut idx = trigram::TrigramIndex::default();
        idx.upsert_bytes("alpha.rs".into(), canon_a.clone(), 26, 1, 0, b"fn unique_needle_xyz() {}\n");
        idx.upsert_bytes("beta.rs".into(), canon_b.clone(), 21, 1, 0, b"fn other_symbol() {}\n");
        idx.mark_ready();

        let hits = idx.query_candidates("unique_needle").expect("candidates");
        assert_eq!(hits, vec![canon_a.clone()]);
        // Case-insensitive match uses the same lowercase trigram postings.
        let ci_hits = idx.query_candidates("UNIQUE_NEEDLE").expect("ci candidates");
        assert_eq!(ci_hits, vec![canon_a.clone()]);

        let snapshot_path = canon.join(".trigram-test.idx");
        idx.save_snapshot_to(&snapshot_path).expect("save snapshot");
        let mut reloaded =
            trigram::TrigramIndex::load_snapshot_from(&canon, &snapshot_path).expect("load snapshot");
        reloaded.mark_ready();
        assert_eq!(reloaded.query_candidates("other_symbol"), Some(vec![canon_b]));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_tree_digest_changes_on_file_edit() {
        let dir = make_temp_dir("digest");
        let f = dir.join("a.txt");
        fs::write(&f, "hello").unwrap();

        let daemon = for_workspace(&dir).expect("daemon");
        let d1 = daemon.workspace_tree_digest().expect("digest 1");
        let canon_f = fs::canonicalize(&f).unwrap();
        fs::write(&canon_f, "hello world!").unwrap();
        daemon.notify_local_write(&canon_f, Some(b"hello world!"));
        let d2 = daemon.workspace_tree_digest().expect("digest 2");
        assert_ne!(d1, d2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_fsevents_detects_external_write() {
        let dir = make_temp_dir("fsevents");
        let daemon = for_workspace(&dir).expect("daemon");
        assert!(daemon.watcher_active(), "FSEvents watcher should start on macOS");

        let epoch_before = daemon.fs_epoch();
        let target = dir.join("external.rs");
        fs::write(&target, "pub fn from_outside() {}\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && daemon.fs_epoch() == epoch_before {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            daemon.fs_epoch() > epoch_before,
            "FSEvents should advance fs_epoch after external write"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
