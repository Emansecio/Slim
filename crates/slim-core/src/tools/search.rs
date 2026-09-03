use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use sha2::{Digest, Sha256};

use super::execution::hex_digest;
use super::{DependencyKind, DependencyObservation, FastStamp, ToolError, ToolExecutionError};
use crate::runtime::CancellationToken;

pub const DEFAULT_MAX_HITS: usize = 200;
pub const MAX_HITS_CAP: usize = 500;
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_HIT_TEXT_BYTES: usize = 8 * 1024;
const TRUNCATION_MARKER_RESERVE_BYTES: usize = 64;
pub(crate) const MAX_SEARCH_PATTERNS: usize = 32;
const MAX_SEARCH_SNAPSHOT_HITS: usize = MAX_HITS_CAP;
const MAX_SEARCH_SNAPSHOTS: usize = 8;
const SEARCH_SNAPSHOT_TTL: Duration = Duration::from_secs(120);

const SKIP_DIR_NAMES: &[&str] = &["node_modules", "target", "dist", ".git", ".slim", ".pi"];
const SEARCH_SKIP_FOOTER: &str =
    "[skipped: node_modules, target, dist, .git, .slim, .pi — use list/shell in those trees]";

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchOptions {
    pub query: String,
    pub root: PathBuf,
    pub offset: usize,
    pub max_hits: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total_seen: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct MatchedSearchHit {
    pub hit: SearchHit,
    pub pattern_index: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchScan {
    pub hits: Vec<MatchedSearchHit>,
    pub capped: bool,
    pub dependency: DependencyObservation,
    pub bytes_read: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchBatchPage {
    pub hits: Vec<MatchedSearchHit>,
    pub patterns: Arc<Vec<String>>,
    pub first: usize,
    pub total: usize,
    pub capped: bool,
    pub next_cursor: Option<String>,
    pub dependency: DependencyObservation,
    pub bytes_read: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchService {
    snapshots: Arc<Mutex<SearchSnapshotCache>>,
}

#[derive(Debug, Default)]
struct SearchSnapshotCache {
    snapshots: HashMap<String, SearchSnapshot>,
}

#[derive(Clone, Debug)]
struct SearchSnapshot {
    root: PathBuf,
    patterns: Arc<Vec<String>>,
    hits: Arc<Vec<MatchedSearchHit>>,
    capped: bool,
    dependency: DependencyObservation,
    expires_at: Instant,
    last_used: Instant,
}

pub(crate) struct SearchEvidence {
    root: PathBuf,
    hasher: Sha256,
    bytes_read: u64,
    observed_files: usize,
}

impl SearchEvidence {
    pub(crate) fn new(root: &Path, patterns: &[String]) -> Self {
        let mut evidence = Self {
            root: root.to_path_buf(),
            hasher: Sha256::new(),
            bytes_read: 0,
            observed_files: 0,
        };
        evidence.record_field(b"slim-search-observation-v1");
        evidence.record_field(root.to_string_lossy().as_bytes());
        for pattern in patterns {
            evidence.record_field(pattern.as_bytes());
        }
        evidence
    }

    pub(crate) fn record_file(&mut self, path: &Path, file: &File, metadata: &Metadata) {
        self.observed_files = self.observed_files.saturating_add(1);
        self.record_field(path.to_string_lossy().as_bytes());
        let stamp = FastStamp::from_file(DependencyKind::File, file, metadata, None);
        self.record_field(stamp.digest().as_bytes());
    }

    pub(crate) fn record_bytes(&mut self, bytes: &[u8]) {
        self.bytes_read = self
            .bytes_read
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }

    pub(crate) fn error(&self, error: ToolError) -> ToolExecutionError {
        ToolExecutionError::observed(error, vec![self.dependency()], self.bytes_read)
    }

    pub(crate) fn finish(self) -> (DependencyObservation, u64) {
        (self.dependency(), self.bytes_read)
    }

    fn dependency(&self) -> DependencyObservation {
        DependencyObservation {
            path: self.root.clone(),
            stamp: FastStamp::observed_directory(
                self.observed_files,
                hex_digest(self.hasher.clone().finalize()),
            ),
        }
    }

    fn record_field(&mut self, field: &[u8]) {
        self.hasher
            .update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
        self.hasher.update(field);
    }
}

impl Default for SearchService {
    fn default() -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(SearchSnapshotCache::default())),
        }
    }
}

impl SearchService {
    pub(crate) fn page(
        &self,
        root: &Path,
        patterns: Vec<String>,
        offset: usize,
        max_hits: usize,
        cursor: Option<&str>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<SearchBatchPage, ToolExecutionError> {
        validate_patterns(&patterns)?;
        if max_hits == 0 || max_hits > MAX_HITS_CAP {
            return Err(ToolError::InvalidInput {
                message: format!("search max_hits must be 1..={MAX_HITS_CAP}"),
            }
            .into());
        }
        if offset == 0 {
            return Err(ToolError::InvalidInput {
                message: "search offset must be at least 1".into(),
            }
            .into());
        }
        if let Some(cursor) = cursor {
            return self.page_from_cursor(root, &patterns, max_hits, cursor, cancellation);
        }
        check_cancelled(cancellation)?;
        // Offset pagination without a cursor reuses the live snapshot for the
        // same (root, patterns) instead of rescanning the workspace. Fresh
        // searches (offset == 1) always rescan so the governor still observes
        // external workspace changes between turns; a revision-only guard
        // would miss edits made outside Slim.
        if offset > 1 {
            if let Some((id, snapshot)) = self.reuse_snapshot(root, &patterns) {
                return Ok(snapshot.page(&id, offset, max_hits, 0));
            }
        }

        let canonical = root.to_path_buf();
        let scan = search_with_walker(
            &canonical,
            &patterns,
            MAX_SEARCH_SNAPSHOT_HITS,
            cancellation,
        )?;
        let id = new_snapshot_id();
        let patterns = Arc::new(patterns);
        let hits = Arc::new(scan.hits);
        let now = Instant::now();
        let snapshot = SearchSnapshot {
            root: canonical,
            patterns: Arc::clone(&patterns),
            hits: Arc::clone(&hits),
            capped: scan.capped,
            dependency: scan.dependency,
            expires_at: now + SEARCH_SNAPSHOT_TTL,
            last_used: now,
        };
        {
            let mut cache = self
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remove_expired(&mut cache, now);
            cache.snapshots.insert(id.clone(), snapshot.clone());
            evict_old_snapshots(&mut cache);
        }
        Ok(snapshot.page(&id, offset, max_hits, scan.bytes_read))
    }

    fn page_from_cursor(
        &self,
        root: &Path,
        patterns: &[String],
        max_hits: usize,
        cursor: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<SearchBatchPage, ToolExecutionError> {
        check_cancelled(cancellation)?;
        let (id, offset) = parse_cursor(cursor).ok_or_else(expired_cursor_error)?;
        if !id.starts_with("search-") {
            return Err(expired_cursor_error().into());
        }
        let canonical = root.to_path_buf();
        let now = Instant::now();
        let snapshot = {
            let mut cache = self
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remove_expired(&mut cache, now);
            let Some(snapshot) = cache.snapshots.get_mut(id) else {
                drop(cache);
                return self.page(
                    root,
                    patterns.to_vec(),
                    offset,
                    max_hits,
                    None,
                    cancellation,
                );
            };
            if snapshot.root != canonical || snapshot.patterns.as_ref() != patterns {
                return Err(ToolError::InvalidInput {
                    message: "search cursor does not match this path/query".into(),
                }
                .into());
            }
            snapshot.last_used = now;
            snapshot.clone()
        };
        Ok(snapshot.page(id, offset, max_hits, 0))
    }

    /// Latest live snapshot for the same (root, patterns), if any. No I/O,
    /// TTL-bounded, same match contract as the cursor path.
    fn reuse_snapshot(&self, root: &Path, patterns: &[String]) -> Option<(String, SearchSnapshot)> {
        let canonical = root.to_path_buf();
        let now = Instant::now();
        let mut cache = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        remove_expired(&mut cache, now);
        let id = cache
            .snapshots
            .iter()
            .filter(|(_, snapshot)| {
                snapshot.root == canonical && snapshot.patterns.as_ref() == patterns
            })
            .max_by_key(|(_, snapshot)| snapshot.last_used)
            .map(|(id, _)| id.clone())?;
        let snapshot = cache.snapshots.get_mut(&id)?;
        snapshot.last_used = now;
        Some((id, snapshot.clone()))
    }
}

impl SearchSnapshot {
    fn page(&self, id: &str, offset: usize, max_hits: usize, bytes_read: u64) -> SearchBatchPage {
        let first_index = offset.saturating_sub(1).min(self.hits.len());
        let page_hits = self
            .hits
            .iter()
            .skip(first_index)
            .take(max_hits)
            .cloned()
            .collect::<Vec<_>>();
        let next_index = first_index.saturating_add(page_hits.len());
        let next_cursor = (next_index < self.hits.len())
            .then(|| format!("{id}:{:x}", next_index.saturating_add(1)));
        SearchBatchPage {
            hits: page_hits,
            patterns: Arc::clone(&self.patterns),
            first: first_index.saturating_add(1),
            total: self.hits.len(),
            capped: self.capped,
            next_cursor,
            dependency: self.dependency.clone(),
            bytes_read,
        }
    }
}

pub fn search_bounded(opts: SearchOptions) -> Result<SearchPage, ToolError> {
    validate_search_options(&opts)?;
    let page = SearchService::default()
        .page(
            &opts.root,
            vec![opts.query],
            opts.offset,
            opts.max_hits,
            None,
            None,
        )
        .map_err(|failure| failure.error)?;
    let total_seen = page.first.saturating_sub(1).saturating_add(page.hits.len());
    Ok(SearchPage {
        hits: page.hits.into_iter().map(|hit| hit.hit).collect(),
        total_seen,
        truncated: page.next_cursor.is_some() || page.capped,
    })
}

pub fn search_literal(root: impl AsRef<Path>, query: &str) -> Result<Vec<SearchHit>, ToolError> {
    let page = search_bounded(SearchOptions {
        query: query.into(),
        root: root.as_ref().to_path_buf(),
        offset: 1,
        max_hits: DEFAULT_MAX_HITS,
    })?;
    Ok(page.hits)
}

pub fn format_search_page(page: &SearchPage, display_root: &Path) -> String {
    let mut output = page
        .hits
        .iter()
        .map(|hit| format_search_hit(hit, display_root))
        .collect::<Vec<_>>()
        .join("\n");

    if page.truncated {
        let first = page.page_start();
        let last = first.saturating_add(page.hits.len()).saturating_sub(1);
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!(
            "\n[showing hits {first}-{last} of {}+; pass \"offset\": {} for the next page]",
            page.total_seen,
            last.saturating_add(1)
        ));
    }
    append_skip_footer(&mut output);
    output
}

pub(crate) fn format_search_batch_page(page: &SearchBatchPage, display_root: &Path) -> String {
    let multiple = page.patterns.len() > 1;
    let mut output = page
        .hits
        .iter()
        .map(|matched| {
            let rendered = format_search_hit(&matched.hit, display_root);
            if multiple {
                format!(
                    "[pattern {}: {}] {rendered}",
                    matched.pattern_index + 1,
                    page.patterns[matched.pattern_index]
                )
            } else {
                rendered
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(cursor) = &page.next_cursor {
        let last = page.first.saturating_add(page.hits.len()).saturating_sub(1);
        if !output.is_empty() {
            output.push('\n');
        }
        let total = if page.capped {
            format!("{}+", page.total)
        } else {
            page.total.to_string()
        };
        output.push_str(&format!(
            "\n[showing hits {}-{last} of {total}; pass \"cursor\": \"{cursor}\" for the next page]",
            page.first
        ));
    } else if page.capped {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!(
            "\n[search snapshot capped at {MAX_SEARCH_SNAPSHOT_HITS} hits; narrow the path or pattern]"
        ));
    }
    append_skip_footer(&mut output);
    output
}

impl SearchPage {
    pub fn page_start(&self) -> usize {
        self.total_seen
            .saturating_sub(self.hits.len())
            .saturating_add(1)
    }
}

fn validate_search_options(opts: &SearchOptions) -> Result<(), ToolError> {
    validate_patterns(std::slice::from_ref(&opts.query))?;
    if opts.offset == 0 {
        return Err(ToolError::InvalidInput {
            message: "search offset must be at least 1".into(),
        });
    }
    if opts.max_hits == 0 || opts.max_hits > MAX_HITS_CAP {
        return Err(ToolError::InvalidInput {
            message: format!("search max_hits must be 1..={MAX_HITS_CAP}"),
        });
    }
    Ok(())
}

fn validate_patterns(patterns: &[String]) -> Result<(), ToolError> {
    if patterns.is_empty() || patterns.len() > MAX_SEARCH_PATTERNS {
        return Err(ToolError::InvalidInput {
            message: format!("search patterns must contain 1..={MAX_SEARCH_PATTERNS} strings"),
        });
    }
    if patterns.iter().any(String::is_empty) {
        return Err(ToolError::InvalidInput {
            message: "search query/patterns cannot contain an empty string".into(),
        });
    }
    Ok(())
}

fn search_with_walker(
    root: &Path,
    patterns: &[String],
    hit_limit: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<SearchScan, ToolExecutionError> {
    let mut walker = WalkBuilder::new(root);
    walker
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .hidden(false)
        .follow_links(false);
    walker.filter_entry(|entry| !should_skip_entry(entry.path()));

    let mut hits = Vec::new();
    let mut capped = false;
    let mut evidence = SearchEvidence::new(root, patterns);
    // Guard a misconfigured root: per-entry errors below are skipped, so a
    // nonexistent root must fail here instead of returning an empty success.
    // File roots stay searchable as before.
    if !root.is_dir() && !root.is_file() {
        return Err(evidence.error(ToolError::Io {
            message: format!(
                "search root is not a readable directory: {}",
                root.display()
            ),
        }));
    }
    let pattern_bytes = patterns
        .iter()
        .map(|pattern| pattern.as_bytes())
        .collect::<Vec<_>>();
    'walk: for entry in walker.build() {
        if let Err(error) = check_cancelled(cancellation) {
            return Err(evidence.error(error));
        }
        // Per-entry walk errors (permission, transient lock) skip the entry
        // instead of failing the whole search; the root itself is guarded
        // above.
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if entry.file_type().is_some_and(|kind| kind.is_dir()) || should_skip_file(path) {
            continue;
        }
        // Unreadable files are skipped like binary files: one bad file must
        // not invalidate hits already collected.
        let Ok(file) = File::open(path) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        evidence.record_file(path, &file, &metadata);
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let mut reader = BufReader::new(file);
        let Ok(sample) = reader.fill_buf() else {
            continue;
        };
        if sample.contains(&0) {
            evidence.record_bytes(sample);
            continue;
        }

        let mut line_bytes = Vec::new();
        let mut line_number = 0usize;
        loop {
            if let Err(error) = check_cancelled(cancellation) {
                return Err(evidence.error(error));
            }
            line_bytes.clear();
            match reader.read_until(b'\n', &mut line_bytes) {
                Ok(0) => break,
                Ok(_) => evidence.record_bytes(&line_bytes),
                Err(_) => {
                    // Unreadable chunk ends this file, not the search.
                    evidence.record_bytes(&line_bytes);
                    break;
                }
            }
            line_number = line_number.saturating_add(1);
            // Byte prefilter: skip UTF-8 conversion cost on lines no pattern
            // can match. Patterns are non-empty (see validate_patterns) and
            // valid UTF-8, so a byte-substring miss implies a str miss on the
            // stripped line below; candidates are re-checked as str.
            let mut content_bytes = line_bytes.as_slice();
            if content_bytes.ends_with(b"\n") {
                content_bytes = &content_bytes[..content_bytes.len() - 1];
                if content_bytes.ends_with(b"\r") {
                    content_bytes = &content_bytes[..content_bytes.len() - 1];
                }
            }
            if !pattern_bytes
                .iter()
                .any(|needle| bytes_contains(content_bytes, needle))
            {
                continue;
            }
            // Invalid UTF-8 skips the rest of this file (like binary
            // files above) instead of failing the whole search: one bad
            // file must not invalidate hits already collected.
            let Ok(line_str) = std::str::from_utf8(&line_bytes) else {
                break;
            };
            let mut line = line_str.to_owned();
            strip_line_ending(&mut line);
            for (pattern_index, pattern) in patterns.iter().enumerate() {
                if !line.contains(pattern) {
                    continue;
                }
                if hits.len() >= hit_limit {
                    capped = true;
                    break 'walk;
                }
                hits.push(MatchedSearchHit {
                    hit: SearchHit {
                        path: path.to_path_buf(),
                        line: line_number,
                        text: bound_hit_text(line.clone()),
                    },
                    pattern_index,
                });
            }
        }
    }
    let (dependency, bytes_read) = evidence.finish();
    Ok(SearchScan {
        hits,
        capped,
        dependency,
        bytes_read,
    })
}

pub(super) fn bound_hit_text(text: String) -> String {
    if text.len() <= MAX_HIT_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_HIT_TEXT_BYTES - TRUNCATION_MARKER_RESERVE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let discarded = text.len() - end;
    format!("{}\n[truncated {discarded} bytes]", &text[..end])
}

fn format_search_hit(hit: &SearchHit, display_root: &Path) -> String {
    let path = display_path(display_root, &hit.path);
    format!("{}:{}: {}", path.display(), hit.line, hit.text)
}

fn append_skip_footer(output: &mut String) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(SEARCH_SKIP_FOOTER);
}

fn should_skip_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIP_DIR_NAMES.contains(&name))
}

fn should_skip_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("map" | "lock")
    ) || name.ends_with(".min.js")
}

fn strip_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn bytes_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), ToolError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(ToolError::Cancelled);
    }
    Ok(())
}

fn new_snapshot_id() -> String {
    let sequence = NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed);
    format!("search-{:x}-{sequence:x}", std::process::id())
}

fn parse_cursor(cursor: &str) -> Option<(&str, usize)> {
    let (id, offset) = cursor.rsplit_once(':')?;
    let offset = usize::from_str_radix(offset, 16).ok()?;
    (offset > 0 && !id.is_empty()).then_some((id, offset))
}

fn expired_cursor_error() -> ToolError {
    ToolError::InvalidInput {
        message: "search cursor expired or was invalidated; start a new search".into(),
    }
}

fn remove_expired(cache: &mut SearchSnapshotCache, now: Instant) {
    cache
        .snapshots
        .retain(|_, snapshot| snapshot.expires_at > now);
}

fn evict_old_snapshots(cache: &mut SearchSnapshotCache) {
    while cache.snapshots.len() > MAX_SEARCH_SNAPSHOTS {
        let Some(oldest) = cache
            .snapshots
            .iter()
            .min_by_key(|(_, snapshot)| snapshot.last_used)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        cache.snapshots.remove(&oldest);
    }
}

pub(crate) fn display_path(display_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(display_root)
        .map(|relative| relative.to_path_buf())
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(name: &str) -> PathBuf {
        let unique = format!(
            "slim-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join(name)
    }

    #[test]
    fn search_respects_gitignore_and_skip_dirs() {
        let root = temp_root("ignore");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join("dist")).expect("dist");
        fs::create_dir_all(root.join("node_modules/pkg")).expect("node_modules");
        fs::write(root.join(".gitignore"), "ignored.txt\n").expect("gitignore");
        fs::write(root.join("src/onboarding.ts"), "onboarding flow").expect("src");
        fs::write(root.join("dist/bundle.js"), "onboarding noise").expect("dist");
        fs::write(root.join("node_modules/pkg/index.js"), "onboarding noise").expect("node");
        fs::write(root.join("ignored.txt"), "onboarding hidden").expect("ignored");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init");

        let page = search_bounded(SearchOptions {
            query: "onboarding".into(),
            root: root.clone(),
            offset: 1,
            max_hits: DEFAULT_MAX_HITS,
        })
        .expect("search");

        assert_eq!(page.hits.len(), 1);
        assert!(page.hits[0].path.ends_with("src/onboarding.ts"));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn invalid_utf8_skips_file_instead_of_failing_search() {
        let root = temp_root("invalid-utf8");
        fs::create_dir_all(&root).expect("root");
        // Invalid UTF-8 line containing the pattern must not fail the search.
        fs::write(root.join("bad.txt"), b"needle \xff\xfe\n").expect("bad");
        fs::write(root.join("good.txt"), "needle here\n").expect("good");

        let page = search_bounded(SearchOptions {
            query: "needle".into(),
            root: root.clone(),
            offset: 1,
            max_hits: DEFAULT_MAX_HITS,
        })
        .expect("search succeeds despite invalid UTF-8 file");

        assert_eq!(page.hits.len(), 1);
        assert!(page.hits[0].path.ends_with("good.txt"));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn missing_root_still_fails_instead_of_empty_success() {
        let root = temp_root("missing").join("does-not-exist");
        let result = search_bounded(SearchOptions {
            query: "needle".into(),
            root,
            offset: 1,
            max_hits: DEFAULT_MAX_HITS,
        });
        assert!(result.is_err());
    }

    #[test]
    fn empty_search_page_still_explains_skip_dirs() {
        let formatted = format_search_page(
            &SearchPage {
                hits: Vec::new(),
                total_seen: 0,
                truncated: false,
            },
            Path::new("."),
        );
        assert!(formatted.contains(SEARCH_SKIP_FOOTER));
        assert!(formatted.contains("use list/shell"));
    }

    #[test]
    fn offset_without_cursor_reuses_snapshot_without_rescan() {
        let root = temp_root("offset-reuse");
        fs::create_dir_all(&root).expect("root");
        for index in 1..=3 {
            fs::write(root.join(format!("hit{index}.txt")), "needle here\n").expect("hit");
        }
        let service = SearchService::default();
        let patterns = vec!["needle".to_owned()];
        let first = service
            .page(&root, patterns.clone(), 1, 2, None, None)
            .expect("first page");
        assert!(first.bytes_read > 0);
        assert_eq!(first.hits.len(), 2);
        let second = service
            .page(&root, patterns, 3, 2, None, None)
            .expect("offset page");
        assert_eq!(second.bytes_read, 0);
        assert_eq!(second.hits.len(), 1);
        assert_eq!(second.total, first.total);
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }
}
