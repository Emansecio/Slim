//! Incremental in-memory trigram inverted index with atomic disk snapshot
//! (`.slim/cache/trigram.v1.idx`).
//!
//! Trigrams are normalized to ASCII lowercase so the same index accelerates
//! both case-sensitive literal searches and case-insensitive variant lookups.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAGIC: &[u8; 8] = b"SLIMTRI1";
pub(crate) const MAX_INDEXED_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_UNIQUE_TRIGRAMS_PER_FILE: usize = 65_536;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

pub(crate) fn is_binary_bytes(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

#[derive(Clone, Debug)]
struct DocMeta {
    rel_path: String,
    abs_path: PathBuf,
    len: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
    alive: bool,
}

/// Inverted trigram index over UTF-8 workspace files.
#[derive(Debug, Default)]
pub(crate) struct TrigramIndex {
    docs: Vec<DocMeta>,
    path_to_id: HashMap<PathBuf, u32>,
    postings: HashMap<[u8; 3], Vec<u32>>,
    live_docs: usize,
    tombstones: usize,
    ready: bool,
    dirty_since_save: usize,
}

impl TrigramIndex {
    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    pub(crate) fn mark_ready(&mut self) {
        self.ready = true;
    }

    pub(crate) fn doc_count(&self) -> usize {
        self.live_docs
    }

    pub(crate) fn dirty_count(&self) -> usize {
        self.dirty_since_save
    }

    /// Indexes or re-indexes a single UTF-8 file from already-loaded bytes.
    pub(crate) fn upsert_bytes(
        &mut self,
        rel_path: String,
        abs_path: PathBuf,
        len: u64,
        mtime_secs: u64,
        mtime_nanos: u32,
        bytes: &[u8],
    ) {
        if bytes.len() > MAX_INDEXED_FILE_BYTES || is_binary_bytes(bytes) {
            self.remove_path(&abs_path);
            return;
        }
        if let Some(&old_id) = self.path_to_id.get(&abs_path) {
            if let Some(doc) = self.docs.get_mut(old_id as usize) {
                if doc.alive {
                    doc.alive = false;
                    self.live_docs = self.live_docs.saturating_sub(1);
                    self.tombstones = self.tombstones.saturating_add(1);
                }
            }
        }

        let Ok(doc_id) = u32::try_from(self.docs.len()) else {
            return;
        };
        self.docs.push(DocMeta {
            rel_path,
            abs_path: abs_path.clone(),
            len,
            mtime_secs,
            mtime_nanos,
            alive: true,
        });
        self.path_to_id.insert(abs_path, doc_id);
        self.live_docs = self.live_docs.saturating_add(1);
        self.dirty_since_save = self.dirty_since_save.saturating_add(1);

        let trigrams = extract_unique_trigrams(bytes);
        for tri in trigrams {
            let list = self.postings.entry(tri).or_default();
            if list.last().copied() != Some(doc_id) {
                list.push(doc_id);
            }
        }

        if self.tombstones > 128 && self.tombstones * 4 > self.docs.len() {
            self.compact();
        }
    }

    pub(crate) fn remove_path(&mut self, abs_path: &Path) {
        if let Some(old_id) = self.path_to_id.remove(abs_path) {
            if let Some(doc) = self.docs.get_mut(old_id as usize) {
                if doc.alive {
                    doc.alive = false;
                    self.live_docs = self.live_docs.saturating_sub(1);
                    self.tombstones = self.tombstones.saturating_add(1);
                    self.dirty_since_save = self.dirty_since_save.saturating_add(1);
                }
            }
        }
    }

    pub(crate) fn retain_live(&mut self, live: &HashSet<PathBuf>) -> bool {
        let stale: Vec<PathBuf> = self
            .path_to_id
            .keys()
            .filter(|p| !live.contains(*p))
            .cloned()
            .collect();
        if stale.is_empty() {
            return false;
        }
        for path in stale {
            self.remove_path(&path);
        }
        true
    }

    /// Returns true if the index already holds an up-to-date live entry for `abs_path`.
    pub(crate) fn is_fresh(&self, abs_path: &Path, len: u64, mtime_secs: u64, mtime_nanos: u32) -> bool {
        self.path_to_id
            .get(abs_path)
            .and_then(|&id| self.docs.get(id as usize))
            .is_some_and(|d| {
                d.alive && d.len == len && d.mtime_secs == mtime_secs && d.mtime_nanos == mtime_nanos
            })
    }

    /// Returns candidate file paths that contain every trigram in `needle`.
    /// Returns `None` if the index is not ready or `needle` has fewer than 3 bytes.
    pub(crate) fn query_candidates(&self, needle: &str) -> Option<Vec<PathBuf>> {
        if !self.ready || needle.len() < 3 {
            return None;
        }
        let query_tris = extract_unique_trigrams(needle.as_bytes());
        if query_tris.is_empty() {
            return None;
        }
        let mut lists: Vec<&[u32]> = Vec::with_capacity(query_tris.len());
        for tri in &query_tris {
            match self.postings.get(tri) {
                Some(list) if !list.is_empty() => lists.push(list.as_slice()),
                _ => return Some(Vec::new()),
            }
        }
        lists.sort_by_key(|list| list.len());

        let mut acc: Vec<u32> = lists[0]
            .iter()
            .copied()
            .filter(|&id| self.docs.get(id as usize).is_some_and(|d| d.alive))
            .collect();
        for list in &lists[1..] {
            if acc.is_empty() {
                break;
            }
            acc = intersect_sorted(&acc, list);
        }

        let mut paths: Vec<PathBuf> = acc
            .into_iter()
            .filter_map(|id| self.docs.get(id as usize).filter(|d| d.alive).map(|d| d.abs_path.clone()))
            .collect();
        paths.sort();
        Some(paths)
    }

    /// True when the trigram index is ready and currently tracks `abs_path` as
    /// a live clean UTF-8 document. Any unindexed file (binary, >2 MiB, or
    /// newly created and not yet ingested) returns `false` so search falls back
    /// to scanning it on disk instead of skipping it blindly.
    pub(crate) fn tracks_clean_file(&self, abs_path: &Path) -> bool {
        self.ready
            && self
                .path_to_id
                .get(abs_path)
                .and_then(|&id| self.docs.get(id as usize))
                .is_some_and(|doc| doc.alive)
    }

    /// Compacts dead tombstones out of `docs` and rewrites `postings` IDs densely.
    pub(crate) fn compact(&mut self) {
        if self.tombstones == 0 {
            return;
        }
        let mut remap: Vec<Option<u32>> = vec![None; self.docs.len()];
        let mut new_docs = Vec::with_capacity(self.live_docs);
        self.path_to_id.clear();
        for (old_id, doc) in self.docs.drain(..).enumerate() {
            if doc.alive {
                let new_id = new_docs.len() as u32;
                remap[old_id] = Some(new_id);
                self.path_to_id.insert(doc.abs_path.clone(), new_id);
                new_docs.push(doc);
            }
        }
        self.docs = new_docs;
        self.tombstones = 0;
        self.postings.retain(|_, list| {
            let mut compacted = Vec::with_capacity(list.len());
            for &old_id in list.iter() {
                if let Some(Some(new_id)) = remap.get(old_id as usize) {
                    if compacted.last().copied() != Some(*new_id) {
                        compacted.push(*new_id);
                    }
                }
            }
            *list = compacted;
            !list.is_empty()
        });
    }

    /// Persists a compact snapshot to `~/.slim/cache/trigrams/<root-sha256>.v1.idx`
    /// without mutating the project workspace directory itself.
    pub(crate) fn save_snapshot(&mut self, canonical_root: &Path) -> io::Result<()> {
        let target = snapshot_path_for(canonical_root)?;
        self.save_snapshot_to(&target)
    }

    pub(crate) fn save_snapshot_to(&mut self, target: &Path) -> io::Result<()> {
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        self.compact();
        let Some(cache_dir) = target.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no snapshot parent"));
        };
        fs::create_dir_all(cache_dir)?;
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = target.with_extension(format!("idx.{}.{seq}.tmp", std::process::id()));

        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(self.docs.len() as u32).to_le_bytes());
        for doc in &self.docs {
            let path_bytes = doc.rel_path.as_bytes();
            buf.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(path_bytes);
            buf.extend_from_slice(&doc.len.to_le_bytes());
            buf.extend_from_slice(&doc.mtime_secs.to_le_bytes());
            buf.extend_from_slice(&doc.mtime_nanos.to_le_bytes());
        }
        buf.extend_from_slice(&(self.postings.len() as u32).to_le_bytes());
        for (tri, list) in &self.postings {
            buf.extend_from_slice(tri);
            write_varint(&mut buf, list.len() as u32);
            let mut prev = 0u32;
            for &doc_id in list {
                let delta = doc_id.saturating_sub(prev);
                write_varint(&mut buf, delta);
                prev = doc_id;
            }
        }

        let mut file = fs::File::create(&tmp)?;
        file.write_all(&buf)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, target)?;
        self.dirty_since_save = 0;
        Ok(())
    }

    /// Loads a previously saved snapshot if intact and matching `MAGIC`.
    pub(crate) fn load_snapshot(canonical_root: &Path) -> io::Result<Self> {
        let path = snapshot_path_for(canonical_root)?;
        Self::load_snapshot_from(canonical_root, &path)
    }

    pub(crate) fn load_snapshot_from(canonical_root: &Path, path: &Path) -> io::Result<Self> {
        let mut raw = Vec::new();
        fs::File::open(path)?.read_to_end(&mut raw)?;
        let mut cur = raw.as_slice();
        if cur.len() < MAGIC.len() || &cur[..MAGIC.len()] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid trigram magic"));
        }
        cur = &cur[MAGIC.len()..];

        let doc_count = read_u32(&mut cur)? as usize;
        let mut docs = Vec::with_capacity(doc_count);
        let mut path_to_id = HashMap::with_capacity(doc_count);
        for id in 0..doc_count {
            let path_len = read_u32(&mut cur)? as usize;
            if cur.len() < path_len {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated path"));
            }
            let rel_path = std::str::from_utf8(&cur[..path_len])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "utf8 path"))?
                .to_owned();
            cur = &cur[path_len..];
            let len = read_u64(&mut cur)?;
            let mtime_secs = read_u64(&mut cur)?;
            let mtime_nanos = read_u32(&mut cur)?;
            let abs_path = canonical_root.join(&rel_path);
            path_to_id.insert(abs_path.clone(), id as u32);
            docs.push(DocMeta {
                rel_path,
                abs_path,
                len,
                mtime_secs,
                mtime_nanos,
                alive: true,
            });
        }

        let posting_count = read_u32(&mut cur)? as usize;
        let mut postings = HashMap::with_capacity(posting_count);
        for _ in 0..posting_count {
            if cur.len() < 3 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated trigram"));
            }
            let tri = [cur[0], cur[1], cur[2]];
            cur = &cur[3..];
            let count = read_varint(&mut cur)? as usize;
            let mut list = Vec::with_capacity(count);
            let mut acc = 0u32;
            for _ in 0..count {
                let delta = read_varint(&mut cur)?;
                acc = acc.saturating_add(delta);
                if (acc as usize) < docs.len() {
                    list.push(acc);
                }
            }
            if !list.is_empty() {
                postings.insert(tri, list);
            }
        }

        Ok(Self {
            live_docs: docs.len(),
            docs,
            path_to_id,
            postings,
            tombstones: 0,
            ready: false,
            dirty_since_save: 0,
        })
    }
}

pub(crate) fn cache_subdir_path(
    subdir: &str,
    domain_tag: &str,
    canonical_root: &Path,
    ext: &str,
) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let base = if let Some(slim_home) = std::env::var_os("SLIM_HOME") {
        PathBuf::from(slim_home).join("cache").join(subdir)
    } else if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        PathBuf::from(home).join(".slim").join("cache").join(subdir)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no home directory for project index cache",
        ));
    };
    let mut hasher = Sha256::new();
    hasher.update(domain_tag.as_bytes());
    hasher.update(canonical_root.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for b in &digest[..16] {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(base.join(format!("{hex}.{ext}")))
}

fn snapshot_path_for(canonical_root: &Path) -> io::Result<PathBuf> {
    cache_subdir_path("trigrams", "slim-trigram-root-v1\n", canonical_root, "v1.idx")
}

pub(crate) fn extract_mtime_parts(meta: &fs::Metadata) -> (u64, u32) {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or((0, 0), |d| (d.as_secs(), d.subsec_nanos()))
}

fn extract_unique_trigrams(bytes: &[u8]) -> Vec<[u8; 3]> {
    if bytes.len() < 3 {
        return Vec::new();
    }
    let mut set: HashSet<[u8; 3]> = HashSet::with_capacity((bytes.len() / 2).min(4096));
    for win in bytes.windows(3) {
        let tri = [
            win[0].to_ascii_lowercase(),
            win[1].to_ascii_lowercase(),
            win[2].to_ascii_lowercase(),
        ];
        set.insert(tri);
        if set.len() >= MAX_UNIQUE_TRIGRAMS_PER_FILE {
            break;
        }
    }
    set.into_iter().collect()
}

fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    out
}

fn write_varint(buf: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn read_varint(cur: &mut &[u8]) -> io::Result<u32> {
    let mut out = 0u32;
    let mut shift = 0u32;
    loop {
        if cur.is_empty() || shift > 28 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "bad varint"));
        }
        let b = cur[0];
        *cur = &cur[1..];
        out |= u32::from(b & 0x7f) << shift;
        if (b & 0x80) == 0 {
            return Ok(out);
        }
        shift += 7;
    }
}

fn read_u32(cur: &mut &[u8]) -> io::Result<u32> {
    if cur.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated u32"));
    }
    let bytes: [u8; 4] = [cur[0], cur[1], cur[2], cur[3]];
    *cur = &cur[4..];
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(cur: &mut &[u8]) -> io::Result<u64> {
    if cur.len() < 8 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated u64"));
    }
    let bytes: [u8; 8] = [
        cur[0], cur[1], cur[2], cur[3], cur[4], cur[5], cur[6], cur[7],
    ];
    *cur = &cur[8..];
    Ok(u64::from_le_bytes(bytes))
}
