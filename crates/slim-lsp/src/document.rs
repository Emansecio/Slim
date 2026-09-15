//! DocumentStore: the client-side mirror of the files the server is keeping
//! open. Slim writes real files, so this store exists to synchronise content
//! and versions with the server (didOpen/didChange/didSave/didClose), not to
//! hold divergent editor buffers.
//!
//! Document versions are monotonic per file. Open documents are bounded by an
//! LRU; eviction returns the removed entries so the caller can send didClose.
//! Content deduplication avoids sending didChange when the text has not
//! actually changed.

use std::collections::{HashMap, VecDeque};
use std::fs::Metadata;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use lsp_types::VersionedTextDocumentIdentifier;
use url::Url;

pub const DEFAULT_MAX_OPEN_DOCUMENTS: usize = 64;

/// Aggregate text budget across all open documents of one server. The count
/// cap alone allowed 64 × 4 MiB of mirrored buffers; this bounds the sum so
/// a handful of large files cannot pin the whole allowance. A single document
/// is always kept — its own size is already bounded by the read cap.
pub const DEFAULT_MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;

// Test-only source IO accounting. The current-thread measurement resets this
// between operations; production builds carry no counters or observer hooks.
#[cfg(test)]
pub(crate) mod read_metrics {
    thread_local! {
        static COUNTS: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) };
    }
    pub fn take() -> (usize, usize, usize) {
        COUNTS.with(|counts| counts.replace((0, 0, 0)))
    }
    pub fn opened(bytes: usize) {
        COUNTS.with(|counts| {
            let (opens, total, hits) = counts.get();
            counts.set((opens + 1, total + bytes, hits));
        });
    }
    pub fn store_hit() {
        COUNTS.with(|counts| {
            let (opens, bytes, hits) = counts.get();
            counts.set((opens, bytes, hits + 1));
        });
    }
}

/// Fast content-fingerprint for deduplication (not a cryptographic hash).
fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Cheap identity used to detect likely external changes without rereading a
/// complete file. Content is read only after this stamp changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStamp {
    len: u64,
    modified_nanos: Option<u128>,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileStamp {
    fn from_metadata(metadata: &Metadata) -> Option<Self> {
        if !metadata.is_file() {
            return None;
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        #[cfg(windows)]
        use std::os::windows::fs::MetadataExt as _;
        Some(Self {
            len: metadata.len(),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
            #[cfg(windows)]
            creation_time: metadata.creation_time(),
            #[cfg(windows)]
            last_write_time: metadata.last_write_time(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    pub fn for_path(path: &Path) -> Option<Self> {
        Self::from_metadata(&std::fs::metadata(path).ok()?)
    }
}

/// Immutable text shared by the operation cache and DocumentStore. Physical
/// line boundaries are indexed once, so position/context conversion never
/// needs another filesystem read or a Vec<String> clone.
#[derive(Debug)]
pub struct DocumentContent {
    text: Arc<str>,
    line_starts: Box<[usize]>,
    stamp: Option<FileStamp>,
    content_hash: u64,
}

impl DocumentContent {
    pub fn from_text(text: String, stamp: Option<FileStamp>) -> Arc<Self> {
        let text: Arc<str> = Arc::from(text);
        let mut line_starts = vec![0];
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(index.saturating_add(1));
            }
        }
        let content_hash = content_hash(&text);
        Arc::new(Self {
            text,
            line_starts: line_starts.into_boxed_slice(),
            stamp,
            content_hash,
        })
    }

    pub fn read_capped(path: &Path, max_bytes: usize) -> Option<Arc<Self>> {
        let before = FileStamp::for_path(path)?;
        if before.len > max_bytes as u64 {
            return None;
        }
        let text = read_text_capped(std::fs::File::open(path).ok()?, max_bytes)?;
        #[cfg(test)]
        read_metrics::opened(text.len());
        let after = FileStamp::for_path(path)?;
        if before != after {
            return None;
        }
        Some(Self::from_text(text, Some(after)))
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn line(&self, index: u32) -> Option<&str> {
        let index = index as usize;
        let start = *self.line_starts.get(index)?;
        let end = self
            .line_starts
            .get(index.saturating_add(1))
            .copied()
            .map(|next| next.saturating_sub(1))
            .unwrap_or(self.text.len());
        self.text
            .get(start..end)
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
    }

    pub fn stamp_matches_path(&self, path: &Path) -> bool {
        self.stamp
            .zip(FileStamp::for_path(path))
            .is_some_and(|(stored, current)| stored == current)
    }
}

fn read_text_capped(reader: impl Read, max_bytes: usize) -> Option<String> {
    let mut text = String::new();
    // One lookahead byte distinguishes an exact fit from a growing file.
    reader
        .take((max_bytes as u64).saturating_add(1))
        .read_to_string(&mut text)
        .ok()?;
    (text.len() <= max_bytes).then_some(text)
}

#[derive(Clone, Debug)]
pub struct OpenDocument {
    pub uri: Url,
    pub language_id: String,
    pub version: i64,
    pub text: Arc<str>,
    pub content_hash: u64,
    pub(crate) content: Arc<DocumentContent>,
}

/// Outcome of an upsert operation.
#[derive(Debug)]
pub enum DocumentUpdate {
    /// File was not open before; now open at version 1.
    Opened {
        document: OpenDocument,
        /// Documents evicted by the LRU to make room.
        evicted: Vec<OpenDocument>,
    },
    /// File was open and content changed; version bumped.
    Changed {
        document: OpenDocument,
        evicted: Vec<OpenDocument>,
    },
    /// File was open and content is identical; version unchanged.
    Unchanged { version: i64 },
}

/// LRU of open documents keyed by the absolute filesystem path.
#[derive(Debug, Default)]
pub struct DocumentStore {
    documents: HashMap<PathBuf, OpenDocument>,
    order: VecDeque<PathBuf>,
    max_open: usize,
    max_bytes: usize,
    total_bytes: usize,
}

impl DocumentStore {
    pub fn new(max_open: usize) -> Self {
        Self::with_limits(max_open, DEFAULT_MAX_DOCUMENT_BYTES)
    }

    pub fn with_limits(max_open: usize, max_bytes: usize) -> Self {
        Self {
            documents: HashMap::new(),
            order: VecDeque::new(),
            max_open: max_open.max(1),
            max_bytes: max_bytes.max(1),
            total_bytes: 0,
        }
    }

    pub fn is_open(&self, path: &std::path::Path) -> bool {
        self.documents.contains_key(path)
    }

    pub fn get(&self, path: &std::path::Path) -> Option<&OpenDocument> {
        self.documents.get(path)
    }

    pub(crate) fn get_and_touch(&mut self, path: &Path) -> Option<&OpenDocument> {
        self.touch(path);
        self.documents.get(path)
    }

    pub fn open_documents(&self) -> Vec<(&std::path::Path, &OpenDocument)> {
        self.documents
            .iter()
            .map(|(path, doc)| (path.as_path(), doc))
            .collect()
    }

    /// Number of currently open documents.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Reserve capacity before sending didOpen so didClose reaches the server
    /// first. The caller holds the instance lifecycle barrier. `incoming_bytes`
    /// is the text length of the document about to be opened, so the byte
    /// budget is also enforced before the didOpen goes out.
    pub(crate) fn reserve_open(&mut self, incoming_bytes: usize) -> Vec<OpenDocument> {
        let mut evicted = Vec::new();
        while self.documents.len() >= self.max_open
            || self.total_bytes.saturating_add(incoming_bytes) > self.max_bytes
        {
            let Some(path) = self.order.front().cloned() else {
                break;
            };
            if let Some(document) = self.remove(&path) {
                evicted.push(document);
            }
        }
        evicted
    }

    /// Registers a fresh open or updates an existing document.
    /// Content deduplication: identical text does not bump the version.
    /// Returns evicted documents when the LRU cap is exceeded.
    pub fn upsert(
        &mut self,
        path: PathBuf,
        uri: Url,
        language_id: &str,
        text: String,
    ) -> DocumentUpdate {
        self.upsert_content(
            path,
            uri,
            language_id,
            DocumentContent::from_text(text, None),
        )
    }

    pub(crate) fn upsert_content(
        &mut self,
        path: PathBuf,
        uri: Url,
        language_id: &str,
        content: Arc<DocumentContent>,
    ) -> DocumentUpdate {
        let new_hash = content.content_hash;

        if self.documents.contains_key(&path) {
            // Check content dedup in a scoped block so the immutable borrow
            // ends before we mutate self for touch or get_mut.
            let unchanged = self
                .documents
                .get(&path)
                .map(|e| e.content_hash == new_hash && e.content.text() == content.text())
                .unwrap_or(false);
            if unchanged {
                self.documents
                    .get_mut(&path)
                    .expect("contains_key above")
                    .content = Arc::clone(&content);
                let existing = self.documents.get_mut(&path).expect("contains_key above");
                existing.text = Arc::clone(&content.text);
                existing.content_hash = new_hash;
                self.touch(&path);
                let version = self.documents.get(&path).map(|d| d.version).unwrap_or(0);
                return DocumentUpdate::Unchanged { version };
            }
            // Content changed: bump version.
            let existing = self.documents.get_mut(&path).expect("contains_key above");
            existing.version = existing.version.saturating_add(1);
            self.total_bytes = self
                .total_bytes
                .saturating_sub(existing.text.len())
                .saturating_add(content.text.len());
            existing.text = Arc::clone(&content.text);
            existing.content_hash = new_hash;
            existing.content = content;
            self.touch(&path);
            let doc = self.documents.get(&path).expect("just stored").clone();
            let evicted = self.evict_bytes_only();
            return DocumentUpdate::Changed {
                document: doc,
                evicted,
            };
        }

        let doc = OpenDocument {
            uri,
            language_id: language_id.to_owned(),
            version: 1,
            text: Arc::clone(&content.text),
            content_hash: new_hash,
            content,
        };
        self.total_bytes = self.total_bytes.saturating_add(doc.text.len());
        self.documents.insert(path.clone(), doc.clone());
        self.order.push_back(path.clone());
        let evicted = self.evict_if_needed();
        DocumentUpdate::Opened {
            document: doc,
            evicted,
        }
    }

    /// Removes a document (didClose). Returns the removed entry when present.
    pub fn remove(&mut self, path: &std::path::Path) -> Option<OpenDocument> {
        self.order.retain(|candidate| candidate != path);
        let removed = self.documents.remove(path);
        if let Some(document) = &removed {
            self.total_bytes = self.total_bytes.saturating_sub(document.text.len());
        }
        removed
    }

    /// Internal: marks a path as recently used.
    fn touch(&mut self, path: &std::path::Path) {
        if let Some(index) = self.order.iter().position(|candidate| candidate == path) {
            let item = self.order.remove(index).expect("position found");
            self.order.push_back(item);
        }
    }

    /// Evicts oldest documents until under the caps. Returns a Vec so the
    /// caller can send didClose for each evicted document. The byte budget
    /// never evicts the most recent document: a lone document is already
    /// bounded by the read cap.
    fn evict_if_needed(&mut self) -> Vec<OpenDocument> {
        let mut evicted = Vec::new();
        while self.documents.len() > self.max_open
            || (self.total_bytes > self.max_bytes && self.order.len() > 1)
        {
            let Some(evicted_path) = self.order.pop_front() else {
                break;
            };
            if let Some(doc) = self.documents.remove(&evicted_path) {
                self.total_bytes = self.total_bytes.saturating_sub(doc.text.len());
                evicted.push(doc);
            }
        }
        evicted
    }

    /// Byte-budget eviction for in-place content replacement (no new entry,
    /// so the count cap cannot be exceeded — but a small file may have grown).
    fn evict_bytes_only(&mut self) -> Vec<OpenDocument> {
        let mut evicted = Vec::new();
        while self.total_bytes > self.max_bytes && self.order.len() > 1 {
            let Some(evicted_path) = self.order.pop_front() else {
                break;
            };
            if let Some(doc) = self.documents.remove(&evicted_path) {
                self.total_bytes = self.total_bytes.saturating_sub(doc.text.len());
                evicted.push(doc);
            }
        }
        evicted
    }

    pub fn lsp_document_identifier(
        &self,
        path: &std::path::Path,
    ) -> Option<VersionedTextDocumentIdentifier> {
        self.get(path).map(|doc| VersionedTextDocumentIdentifier {
            uri: crate::instance::to_lsp_uri(&doc.uri),
            version: doc.version.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn capped_read_stops_after_one_overflow_byte() {
        // Models a file that grew after the metadata size check.
        let mut reader = std::io::Cursor::new(vec![b'a'; 4096]);
        assert!(read_text_capped(&mut reader, 8).is_none());
        assert_eq!(reader.position(), 9);
    }

    #[test]
    fn capped_read_preserves_utf8_and_rejects_invalid_or_failed_reads() {
        assert_eq!(read_text_capped(&b""[..], 0), Some(String::new()));
        assert_eq!(
            read_text_capped("é\r\n".as_bytes(), 4).as_deref(),
            Some("é\r\n")
        );
        assert!(read_text_capped("é\r\n".as_bytes(), 3).is_none());
        assert!(read_text_capped(&[0xff][..], 4).is_none());
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("read failed"))
            }
        }
        assert!(read_text_capped(FailedRead, 4).is_none());
    }

    fn uri_for(path: &Path) -> Url {
        Url::from_file_path(path).expect("file url")
    }

    #[test]
    fn upsert_bumps_version_on_change() {
        let mut store = DocumentStore::new(4);
        let p = Path::new("D:/demo/a.rs");
        let up = store.upsert(p.to_path_buf(), uri_for(p), "rust", "one".into());
        match up {
            DocumentUpdate::Opened { document, evicted } => {
                assert_eq!(document.version, 1);
                assert!(evicted.is_empty());
            }
            other => panic!("expected Opened, got {other:?}"),
        }

        let up = store.upsert(p.to_path_buf(), uri_for(p), "rust", "two".into());
        match up {
            DocumentUpdate::Changed { document, evicted } => {
                assert_eq!(document.version, 2);
                assert!(evicted.is_empty());
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn upsert_skips_change_for_identical_content() {
        let mut store = DocumentStore::new(4);
        let p = Path::new("D:/demo/a.rs");
        store.upsert(p.to_path_buf(), uri_for(p), "rust", "same".into());
        let up = store.upsert(p.to_path_buf(), uri_for(p), "rust", "same".into());
        match up {
            DocumentUpdate::Unchanged { version } => {
                assert_eq!(version, 1);
            }
            other => panic!("expected Unchanged, got {other:?}"),
        }
    }

    #[test]
    fn hash_match_without_text_match_is_not_deduplicated() {
        let mut store = DocumentStore::new(4);
        let path = Path::new("D:/demo/collision.rs");
        store.upsert(path.to_path_buf(), uri_for(path), "rust", "original".into());
        let replacement = "replacement";
        store
            .documents
            .get_mut(path)
            .expect("open document")
            .content_hash = content_hash(replacement);

        let update = store.upsert(
            path.to_path_buf(),
            uri_for(path),
            "rust",
            replacement.into(),
        );
        match update {
            DocumentUpdate::Changed { document, .. } => {
                assert_eq!(document.version, 2);
                assert_eq!(document.text.as_ref(), replacement);
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn lru_evicts_oldest_and_returns_them() {
        let mut store = DocumentStore::new(2);
        let p1 = PathBuf::from("D:/demo/a.rs");
        let p2 = PathBuf::from("D:/demo/b.rs");
        let p3 = PathBuf::from("D:/demo/c.rs");

        store.upsert(p1.clone(), uri_for(&p1), "rust", "a".into());
        store.upsert(p2.clone(), uri_for(&p2), "rust", "b".into());

        // Third insert evicts p1.
        let up = store.upsert(p3.clone(), uri_for(&p3), "rust", "c".into());
        match up {
            DocumentUpdate::Opened { document, evicted } => {
                assert_eq!(document.version, 1);
                assert_eq!(evicted.len(), 1);
                assert_eq!(evicted[0].version, 1);
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        assert!(!store.is_open(&p1), "p1 should be evicted");
        assert!(store.is_open(&p2));
        assert!(store.is_open(&p3));

        // Touching p2 moves it to back; p3 becomes LRU.
        store.upsert(p2.clone(), uri_for(&p2), "rust", "b2".into());
        assert!(store.is_open(&p2));
        assert!(store.is_open(&p3));

        // Inserting another evicts p3 (now oldest after touch).
        let p4 = PathBuf::from("D:/demo/d.rs");
        let up2 = store.upsert(p4.clone(), uri_for(&p4), "rust", "d".into());
        match up2 {
            DocumentUpdate::Opened { evicted, .. } => {
                assert_eq!(evicted.len(), 1, "should evict one");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        assert!(!store.is_open(&p3), "p3 should be evicted");
        assert!(store.is_open(&p2));
        assert!(store.is_open(&p4));
    }

    #[test]
    fn byte_budget_evicts_oldest_and_keeps_the_newest_document() {
        let mut store = DocumentStore::with_limits(64, 100);
        let p1 = PathBuf::from("D:/demo/a.rs");
        let p2 = PathBuf::from("D:/demo/b.rs");
        store.upsert(p1.clone(), uri_for(&p1), "rust", "a".repeat(60));
        let up = store.upsert(p2.clone(), uri_for(&p2), "rust", "b".repeat(60));
        match up {
            DocumentUpdate::Opened { evicted, .. } => assert_eq!(evicted.len(), 1),
            other => panic!("expected Opened, got {other:?}"),
        }
        assert!(!store.is_open(&p1), "60+60 exceeds the 100-byte budget");
        assert!(store.is_open(&p2));

        // A document larger than the whole budget is still kept: it is the
        // only entry and a lone buffer is already bounded by the read cap.
        let p3 = PathBuf::from("D:/demo/c.rs");
        let up = store.upsert(p3.clone(), uri_for(&p3), "rust", "c".repeat(200));
        match up {
            DocumentUpdate::Opened { evicted, .. } => assert_eq!(evicted.len(), 1),
            other => panic!("expected Opened, got {other:?}"),
        }
        assert!(store.is_open(&p3));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn reserve_open_and_content_growth_both_observe_the_byte_budget() {
        let mut store = DocumentStore::with_limits(64, 100);
        let p1 = PathBuf::from("D:/demo/a.rs");
        let p2 = PathBuf::from("D:/demo/b.rs");
        store.upsert(p1.clone(), uri_for(&p1), "rust", "a".repeat(60));
        store.upsert(p2.clone(), uri_for(&p2), "rust", "b".repeat(20));

        // 80 + 60 incoming exceeds 100: p1 must go before didOpen is sent.
        let evicted = store.reserve_open(60);
        assert_eq!(evicted.len(), 1);
        assert!(!store.is_open(&p1));
        assert!(store.is_open(&p2));

        // A changed document that grows past the budget evicts its elders too.
        let p3 = PathBuf::from("D:/demo/c.rs");
        store.upsert(p3.clone(), uri_for(&p3), "rust", "c".repeat(50));
        let up = store.upsert(p2.clone(), uri_for(&p2), "rust", "b".repeat(90));
        match up {
            DocumentUpdate::Changed { evicted, .. } => {
                assert_eq!(evicted.len(), 1, "p3 leaves as p2 grows");
            }
            other => panic!("expected Changed, got {other:?}"),
        }
        assert!(store.is_open(&p2));
        assert!(!store.is_open(&p3));
    }

    #[test]
    fn remove_closes_document() {
        let mut store = DocumentStore::new(4);
        let p = Path::new(r"D:\demo\a.rs");
        store.upsert(p.to_path_buf(), uri_for(p), "rust", "one".into());
        let removed = store.remove(p);
        assert!(removed.is_some());
        assert!(!store.is_open(p));
        assert!(store.remove(p).is_none());
    }
}
