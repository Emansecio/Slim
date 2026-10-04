use std::collections::{BTreeMap, HashMap, VecDeque};
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
pub(crate) const MAX_CONTEXT_LINES: usize = 3;
const MAX_CONTEXT_TEXT_BYTES: usize = 1024;
const MAX_HIT_TEXT_BYTES: usize = 8 * 1024;
const TRUNCATION_MARKER_RESERVE_BYTES: usize = 64;
pub(crate) const MAX_SEARCH_PATTERNS: usize = 32;
const MAX_SEARCH_SNAPSHOT_HITS: usize = MAX_HITS_CAP;
// A multi-pattern search may continue past the result cap while it checks
// patterns that have not produced a retained hit yet. Keep that recovery pass
// bounded independently from the per-file and retained-hit limits.
const MAX_SEARCH_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SEARCH_SCAN_FILES: usize = 4096;
const MAX_SEARCH_SNAPSHOTS: usize = 8;
const SEARCH_SNAPSHOT_TTL: Duration = Duration::from_secs(120);
const MAX_SEARCH_IO_EXAMPLES: usize = 3;
const MAX_SEARCH_IO_EXAMPLE_BYTES: usize = 96;

pub(crate) const SKIP_DIR_NAMES: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    ".git",
    ".slim",
    ".pi",
    ".venv",
];
// `.gitignore` is honored only inside a git repository; `.ignore` anywhere.
const SEARCH_SKIP_FOOTER: &str =
    "[skipped: node_modules, target, dist, .git, .slim, .pi, .venv, *.lock/*.map/*.min.js files and .ignore'd files, plus .gitignore'd files inside a git repository, are not searched in directory scans — use read/list/shell to reach them]";
const CASE_VARIANT_NOTE_CHARS: usize = 80;
const HIT_WINDOW_CONTEXT_BYTES: usize = 256;

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
    pub context: Vec<Arc<(usize, String)>>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchScan {
    pub hits: Vec<MatchedSearchHit>,
    pub capped: bool,
    pub scan_complete: bool,
    pub work_limited: bool,
    /// Binary, oversized or invalid UTF-8 files whose text was not (fully)
    /// searched; absence claims must not cover them.
    pub unsearched_files: usize,
    pub coverage: Vec<SearchPatternCoverage>,
    pub io_failures: SearchIoSummary,
    pub dependency: DependencyObservation,
    pub bytes_read: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SearchPatternCoverage {
    /// Matching lines observed by the shared scan, including lines omitted
    /// after the retained-hit budget was reached.
    pub observed: usize,
    /// Matching lines retained in the snapshot for pagination.
    pub retained: usize,
    /// Matching lines observed but omitted by the retained-hit budget.
    pub omitted: usize,
    /// Only for a pattern a complete scan never matched: a spelling that
    /// differs in letter case and does occur.
    pub case_variant: Option<CaseVariant>,
}

#[derive(Clone, Debug)]
pub(crate) struct CaseVariant {
    pub path: PathBuf,
    pub line: usize,
    pub spelling: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchIoFailureKind {
    Walk,
    Open,
    Metadata,
    Read,
}

impl SearchIoFailureKind {
    const fn index(self) -> usize {
        match self {
            Self::Walk => 0,
            Self::Open => 1,
            Self::Metadata => 2,
            Self::Read => 3,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Open => "open",
            Self::Metadata => "metadata",
            Self::Read => "read",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SearchIoSummary {
    counts: [u16; 4],
    examples: Vec<String>,
}

impl SearchIoSummary {
    fn record(&mut self, kind: SearchIoFailureKind, root: &Path, path: &Path) {
        let count = &mut self.counts[kind.index()];
        *count = (*count).saturating_add(1);
        if self.examples.len() == MAX_SEARCH_IO_EXAMPLES {
            return;
        }
        let example = bounded_search_error_path(root, path);
        if !self.examples.contains(&example) {
            self.examples.push(example);
        }
    }

    fn has_failures(&self) -> bool {
        self.counts.iter().any(|count| *count > 0)
    }

    fn total(&self) -> u32 {
        self.counts
            .iter()
            .fold(0u32, |total, count| total.saturating_add(u32::from(*count)))
    }

    fn category_counts(&self) -> String {
        [
            SearchIoFailureKind::Walk,
            SearchIoFailureKind::Open,
            SearchIoFailureKind::Metadata,
            SearchIoFailureKind::Read,
        ]
        .into_iter()
        .filter_map(|kind| {
            let count = self.counts[kind.index()];
            (count > 0).then(|| format!("{} {count}", kind.label()))
        })
        .collect::<Vec<_>>()
        .join(", ")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SearchBatchPage {
    pub hits: Vec<MatchedSearchHit>,
    pub patterns: Arc<Vec<String>>,
    pub context_lines: usize,
    pub first: usize,
    pub total: usize,
    pub capped: bool,
    pub scan_complete: bool,
    pub work_limited: bool,
    pub unsearched_files: usize,
    pub coverage: Arc<Vec<SearchPatternCoverage>>,
    pub io_failures: Arc<SearchIoSummary>,
    pub next_cursor: Option<String>,
    snapshot_id: String,
    pub dependency: DependencyObservation,
    pub bytes_read: u64,
}

impl SearchBatchPage {
    pub(crate) fn present(&self, max_bytes: usize, display_root: &Path) -> super::ToolPresentation {
        let full = format_search_batch_page(self, display_root);
        if full.len() <= max_bytes {
            return super::ToolPresentation {
                text: full,
                delivered_records: self.hits.len(),
                complete: true,
                oversized_record: false,
            };
        }
        // Slice snapshot records, not rendered lines: several pattern matches
        // can share one line, and each hit owns its surrounding context.
        let render = |count: usize| {
            let mut page = self.clone();
            page.hits.truncate(count);
            let next = self.first.saturating_add(count);
            page.next_cursor =
                (next <= self.total).then(|| format!("{}:{next:x}", self.snapshot_id));
            format_search_batch_page(&page, display_root)
        };
        let mut low = 0;
        let mut high = self.hits.len();
        while low < high {
            let count = low + (high - low).div_ceil(2);
            if render(count).len() <= max_bytes {
                low = count;
            } else {
                high = count - 1;
            }
        }
        let mut text = render(low);
        if low == 0 {
            let omitted_hit = "[hit with context and continuation exceeds presentation budget; no hit delivered; narrow the query or use the continuation; not safe for patch.expected]\n";
            if self.io_failures.has_failures() || self.work_limited || !self.scan_complete {
                let minimal = format_minimal_search_presentation(self);
                if minimal.len() < text.len() {
                    text = minimal;
                }
            }
            // A clean, complete scan always explains the omission; otherwise
            // only when it fits the budget.
            let clean_scan =
                !self.io_failures.has_failures() && !self.work_limited && self.scan_complete;
            if text.len().saturating_add(omitted_hit.len()) <= max_bytes || clean_scan {
                text.insert_str(0, omitted_hit);
            }
        }
        super::ToolPresentation {
            text,
            delivered_records: low,
            complete: false,
            oversized_record: low == 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SearchPageOptions {
    pub offset: usize,
    pub max_hits: usize,
    pub context_lines: usize,
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
    context_lines: usize,
    root: PathBuf,
    patterns: Arc<Vec<String>>,
    hits: Arc<Vec<MatchedSearchHit>>,
    capped: bool,
    scan_complete: bool,
    work_limited: bool,
    unsearched_files: usize,
    coverage: Arc<Vec<SearchPatternCoverage>>,
    io_failures: Arc<SearchIoSummary>,
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
        options: SearchPageOptions,
        cursor: Option<&str>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<SearchBatchPage, ToolExecutionError> {
        let SearchPageOptions {
            offset,
            max_hits,
            context_lines,
        } = options;
        validate_patterns(&patterns)?;
        if context_lines > MAX_CONTEXT_LINES {
            return Err(ToolError::InvalidInput {
                message: format!("search context_lines must be 0..={MAX_CONTEXT_LINES}"),
            }
            .into());
        }
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
            return self.page_from_cursor(
                root,
                &patterns,
                max_hits,
                context_lines,
                cursor,
                cancellation,
            );
        }
        check_cancelled(cancellation)?;
        // Offset pagination without a cursor reuses the live snapshot for the
        // same (root, patterns, context_lines) instead of rescanning the workspace. Fresh
        // searches (offset == 1) always rescan so the governor still observes
        // external workspace changes between turns; a revision-only guard
        // would miss edits made outside Slim.
        if offset > 1 {
            if let Some((id, snapshot)) = self.reuse_snapshot(root, &patterns, context_lines) {
                return Ok(snapshot.page(&id, offset, max_hits, 0));
            }
        }

        let canonical = root.to_path_buf();
        let mut scan = search_with_walker(
            &canonical,
            &patterns,
            MAX_SEARCH_SNAPSHOT_HITS,
            context_lines,
            cancellation,
        )?;
        note_case_variants(&canonical, &patterns, &mut scan, cancellation)?;
        Ok(self.cache_scan(canonical, patterns, options, scan))
    }

    fn cache_scan(
        &self,
        root: PathBuf,
        patterns: Vec<String>,
        options: SearchPageOptions,
        scan: SearchScan,
    ) -> SearchBatchPage {
        let SearchPageOptions {
            offset,
            max_hits,
            context_lines,
        } = options;
        let id = new_snapshot_id();
        let patterns = Arc::new(patterns);
        let hits = Arc::new(scan.hits);
        let coverage = Arc::new(scan.coverage);
        let io_failures = Arc::new(scan.io_failures);
        let now = Instant::now();
        let snapshot = SearchSnapshot {
            context_lines,
            root,
            patterns: Arc::clone(&patterns),
            hits: Arc::clone(&hits),
            capped: scan.capped,
            scan_complete: scan.scan_complete,
            work_limited: scan.work_limited,
            unsearched_files: scan.unsearched_files,
            coverage: Arc::clone(&coverage),
            io_failures,
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
        snapshot.page(&id, offset, max_hits, scan.bytes_read)
    }

    fn page_from_cursor(
        &self,
        root: &Path,
        patterns: &[String],
        max_hits: usize,
        context_lines: usize,
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
                return Err(expired_cursor_error().into());
            };
            if snapshot.root != canonical
                || snapshot.patterns.as_ref() != patterns
                || snapshot.context_lines != context_lines
            {
                return Err(ToolError::InvalidInput {
                    message: "search cursor does not match this path/query/context_lines".into(),
                }
                .into());
            }
            snapshot.last_used = now;
            snapshot.clone()
        };
        Ok(snapshot.page(id, offset, max_hits, 0))
    }

    /// Latest live snapshot for the same (root, patterns, context_lines). No I/O,
    /// TTL-bounded, same match contract as the cursor path.
    fn reuse_snapshot(
        &self,
        root: &Path,
        patterns: &[String],
        context_lines: usize,
    ) -> Option<(String, SearchSnapshot)> {
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
                snapshot.root == canonical
                    && snapshot.patterns.as_ref() == patterns
                    && snapshot.context_lines == context_lines
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
            context_lines: self.context_lines,
            hits: page_hits,
            patterns: Arc::clone(&self.patterns),
            first: first_index.saturating_add(1),
            total: self.hits.len(),
            capped: self.capped,
            scan_complete: self.scan_complete,
            work_limited: self.work_limited,
            unsearched_files: self.unsearched_files,
            coverage: Arc::clone(&self.coverage),
            io_failures: Arc::clone(&self.io_failures),
            next_cursor,
            snapshot_id: id.to_owned(),
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
            SearchPageOptions {
                offset: opts.offset,
                max_hits: opts.max_hits,
                context_lines: 0,
            },
            None,
            None,
        )
        .map_err(|failure| failure.error)?;
    let total_seen = page.first.saturating_sub(1).saturating_add(page.hits.len());
    Ok(SearchPage {
        hits: page.hits.into_iter().map(|hit| hit.hit).collect(),
        total_seen,
        truncated: page.next_cursor.is_some() || page.capped || page.work_limited,
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
    if page.hits.is_empty() {
        append_skip_footer(&mut output);
    }
    output
}

pub(crate) fn format_search_batch_page(page: &SearchBatchPage, display_root: &Path) -> String {
    let multiple = page.patterns.len() > 1;
    // Hits arrive contiguous per file in scan order: emit each file run as one
    // merged, line-ordered block so the path and shared context lines appear
    // once instead of per hit. `N:` marks a hit line, `N-` marks context —
    // the copy contract for patch.expected stays identical.
    let mut merged = Vec::<(usize, BTreeMap<usize, (&str, Vec<usize>)>)>::new();
    {
        let mut start = 0usize;
        while start < page.hits.len() {
            let mut end = start + 1;
            while end < page.hits.len() && page.hits[end].hit.path == page.hits[start].hit.path {
                end += 1;
            }
            let mut lines = BTreeMap::new();
            for matched in &page.hits[start..end] {
                lines
                    .entry(matched.hit.line)
                    .and_modify(|entry: &mut (&str, Vec<usize>)| {
                        // The line may already exist as a context row from an
                        // earlier hit: context caps at 1 KiB while hit text
                        // keeps 8 KiB, so upgrade to the fuller rendering.
                        entry.0 = matched.hit.text.as_str();
                        if !entry.1.contains(&matched.pattern_index) {
                            entry.1.push(matched.pattern_index);
                        }
                    })
                    .or_insert((matched.hit.text.as_str(), vec![matched.pattern_index]));
                for line in &matched.context {
                    lines.entry(line.0).or_insert((line.1.as_str(), Vec::new()));
                }
            }
            merged.push((start, lines));
            start = end;
        }
    }
    // Keep the mapping in this page, so interpreting a hit never needs a
    // previous tool result. Small pages retain the cheaper inline labels.
    let mut legend = String::new();
    if multiple {
        let mut seen = [false; MAX_SEARCH_PATTERNS];
        let mut repeated_bytes = 0usize;
        for (_, lines) in &merged {
            for (_, patterns) in lines.values() {
                for &index in patterns {
                    let pattern = &page.patterns[index];
                    repeated_bytes = repeated_bytes.saturating_add(pattern.len() + 2);
                    if !seen[index] {
                        legend.push_str(&format!("[pattern {}: {pattern}]\n", index + 1));
                        seen[index] = true;
                    }
                }
            }
        }
        if legend.len() >= repeated_bytes {
            legend.clear();
        }
    }
    let mut output = String::new();
    for (first, lines) in &merged {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!(
            "[{}]\n",
            display_path(display_root, &page.hits[*first].hit.path).display()
        ));
        for (number, (text, patterns)) in lines {
            if patterns.is_empty() {
                output.push_str(&format!("{number}- {text}\n"));
                continue;
            }
            let label = if !legend.is_empty() {
                format!(
                    "[pattern {}] ",
                    patterns
                        .iter()
                        .map(|index| (index + 1).to_string())
                        .collect::<Vec<_>>()
                        .join("|")
                )
            } else if multiple {
                format!(
                    "[{}] ",
                    patterns
                        .iter()
                        .map(|&index| format!("pattern {}: {}", index + 1, page.patterns[index]))
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
            } else {
                String::new()
            };
            output.push_str(&format!("{label}{number}: {text}\n"));
        }
    }
    if output.ends_with('\n') {
        output.pop();
    }
    if !legend.is_empty() {
        legend.push_str(&output);
        output = legend;
    }
    // Multi-pattern absence is reported per pattern by the coverage notice;
    // incomplete scans carry their own notices below.
    if page.total == 0
        && page.patterns.len() == 1
        && page.scan_complete
        && !page.work_limited
        && !page.io_failures.has_failures()
    {
        output = match page.unsearched_files {
            0 => "[no matches after full scan]".to_owned(),
            count => format!(
                "[no matches in searched text ({})]",
                unsearched_notice(count)
            ),
        };
        output.push_str(regex_note(&page.patterns[0]));
        output.push_str(&case_variant_note(page.coverage.first(), display_root));
    }
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
    if page.work_limited {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!(
            "\n[search scan work budget exhausted at {MAX_SEARCH_SCAN_BYTES} bytes or {MAX_SEARCH_SCAN_FILES} files; uncovered patterns are not confirmed absent]"
        ));
    }
    if page.io_failures.has_failures() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format_search_io_notice(&page.io_failures));
    }
    if let Some(coverage) = format_search_coverage(page, display_root) {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&coverage);
    }
    if page.context_lines > 0 {
        let mut notice = format!(
            "\n[context: up to {} lines each side; '-' marks context, ':' marks hits; context lines capped at {MAX_CONTEXT_TEXT_BYTES} bytes; file boundaries/unreadable data may shorten context.",
            page.context_lines
        );
        if page.next_cursor.is_some() {
            notice.push_str(
                " Snapshot, not a current-file guarantee; keep context_lines with cursor.",
            );
        }
        notice.push(']');
        output.push_str(&notice);
    }
    if page.total == 0 {
        append_skip_footer(&mut output);
    }
    output
}

/// Matching is literal, so a pattern written as a regular expression finds
/// nothing and that absence proves nothing. A note for the not-found case
/// only, never a rejection: the text may be meant literally.
fn regex_note(pattern: &str) -> &'static str {
    const REGEX_ONLY: &[&str] = &[".*", ".+", "\\w", "\\s", "\\d", "\\b", "\\.", "\\("];
    let alternation = pattern.replace("||", "").contains('|');
    if alternation
        || pattern.starts_with('^')
        || pattern.ends_with('$')
        || REGEX_ONLY.iter().any(|token| pattern.contains(token))
    {
        " [note: search is literal, not regex, and this pattern looks like one; retry with plain text, one alternative per `patterns` entry]"
    } else {
        ""
    }
}

/// Matching is case-sensitive, so a pattern with the wrong letter case finds
/// nothing and that absence proves little. Names one spelling that does occur.
fn case_variant_note(coverage: Option<&SearchPatternCoverage>, display_root: &Path) -> String {
    let Some(variant) = coverage.and_then(|coverage| coverage.case_variant.as_ref()) else {
        return String::new();
    };
    format!(
        " [note: search is case-sensitive; `{}` occurs at {}:{} — retry with that spelling if it is what you meant]",
        variant.spelling,
        display_path(display_root, &variant.path).display(),
        variant.line
    )
}

fn unsearched_notice(count: usize) -> String {
    format!(
        "{count} file(s) not searched: binary, over {} MiB or invalid UTF-8",
        MAX_FILE_BYTES / (1024 * 1024)
    )
}

fn format_search_coverage(page: &SearchBatchPage, display_root: &Path) -> Option<String> {
    if page.patterns.len() <= 1 && !page.io_failures.has_failures() {
        return None;
    }
    let needs_summary = page.capped
        || page.work_limited
        || page.io_failures.has_failures()
        || page.coverage.iter().any(|coverage| coverage.observed == 0);
    if !needs_summary {
        return None;
    }
    let mut notices = Vec::new();
    for (index, pattern) in page.patterns.iter().enumerate() {
        let coverage = page.coverage.get(index).cloned().unwrap_or_default();
        let notice = if coverage.observed == 0 {
            if page.scan_complete && !page.io_failures.has_failures() {
                match page.unsearched_files {
                    0 => format!(
                        "pattern {} `{pattern}`: not found after full scan{}{}",
                        index + 1,
                        regex_note(pattern),
                        case_variant_note(Some(&coverage), display_root)
                    ),
                    count => format!(
                        "pattern {} `{pattern}`: not found in searched text ({}){}{}",
                        index + 1,
                        unsearched_notice(count),
                        regex_note(pattern),
                        case_variant_note(Some(&coverage), display_root)
                    ),
                }
            } else if page.io_failures.has_failures() {
                format!(
                    "pattern {}: not confirmed absent; I/O errors left part of the scan unread",
                    index + 1
                )
            } else if page.work_limited {
                format!(
                    "pattern {} `{pattern}`: not covered; search work budget exhausted",
                    index + 1
                )
            } else {
                format!(
                    "pattern {} `{pattern}`: not covered; result budget ended before this pattern was reached",
                    index + 1
                )
            }
        } else if coverage.retained == 0 {
            format!(
                "pattern {} `{pattern}`: {} matches observed but none retained; result budget ended",
                index + 1,
                coverage.observed
            )
        } else if coverage.omitted > 0 {
            format!(
                "pattern {} `{pattern}`: {} retained, {} additional matches omitted by the result budget",
                index + 1,
                coverage.retained,
                coverage.omitted
            )
        } else if page.scan_complete && !page.io_failures.has_failures() {
            match page.unsearched_files {
                0 => format!(
                    "pattern {} `{pattern}`: {} retained; full coverage",
                    index + 1,
                    coverage.retained
                ),
                count => format!(
                    "pattern {} `{pattern}`: {} retained ({})",
                    index + 1,
                    coverage.retained,
                    unsearched_notice(count)
                ),
            }
        } else {
            format!(
                "pattern {} `{pattern}`: {} retained; scan incomplete",
                index + 1,
                coverage.retained
            )
        };
        notices.push(notice);
    }
    (!notices.is_empty()).then(|| format!("[search coverage: {}]", notices.join("; ")))
}

fn format_search_io_notice(failures: &SearchIoSummary) -> String {
    let mut notice = format!(
        "[search incomplete: {} I/O failure(s) ({}); coverage is partial and unmatched patterns are not confirmed absent",
        failures.total(),
        failures.category_counts()
    );
    if !failures.examples.is_empty() {
        notice.push_str("; example paths: ");
        notice.push_str(&failures.examples.join(", "));
    }
    notice.push(']');
    notice
}

fn format_minimal_search_presentation(page: &SearchBatchPage) -> String {
    let mut causes = Vec::new();
    if page.io_failures.has_failures() {
        causes.push("I/O errors");
    }
    if page.work_limited {
        causes.push("search work budget exhausted");
    }
    if page.capped {
        causes.push("result cap reached");
    }
    if causes.is_empty() {
        causes.push("scan stopped early");
    }
    let mut output = format!(
        "[search incomplete: {}; unmatched patterns are not confirmed absent]\n[no hit record delivered within presentation budget]",
        causes.join("; ")
    );
    let next = page.first;
    if next <= page.total {
        let cursor = format!("{}:{next:x}", page.snapshot_id);
        output.push_str(&format!(
            "\n[pass \"cursor\": \"{cursor}\" for the next page]"
        ));
    }
    output
}

fn bounded_search_error_path(root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(root)
        .unwrap_or_else(|_| Path::new("<outside-root>"));
    let display = relative.to_string_lossy();
    let display = if display.is_empty() {
        "."
    } else {
        display.as_ref()
    };
    let mut output = String::with_capacity(MAX_SEARCH_IO_EXAMPLE_BYTES);
    let mut truncated = false;
    let prefix_limit = MAX_SEARCH_IO_EXAMPLE_BYTES.saturating_sub(3);
    for character in display.chars() {
        let character = if character.is_control() {
            '?'
        } else {
            character
        };
        if output.len().saturating_add(character.len_utf8()) > prefix_limit {
            truncated = true;
            break;
        }
        output.push(character);
    }
    if truncated {
        output.push_str("...");
    }
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

pub(super) fn validate_patterns(patterns: &[String]) -> Result<(), ToolError> {
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
    // Matching is per line: such a pattern can never match, and the scan would
    // still report "no matches".
    if patterns
        .iter()
        .any(|pattern| pattern.contains(['\n', '\r']))
    {
        return Err(ToolError::InvalidInput {
            message: "search matches single lines, so a pattern cannot contain a line break (\\n or \\r); search one line of the text".into(),
        });
    }
    Ok(())
}

#[derive(Debug)]
struct SearchWalkEntry {
    path: PathBuf,
    is_dir: bool,
}

#[derive(Debug)]
struct SearchOpenedFile<R> {
    reader: R,
    len: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchOpenFailure {
    Open,
    Metadata,
}

impl SearchOpenFailure {
    const fn io_kind(self) -> SearchIoFailureKind {
        match self {
            Self::Open => SearchIoFailureKind::Open,
            Self::Metadata => SearchIoFailureKind::Metadata,
        }
    }
}

fn map_search_walk_entry(
    entry: Result<ignore::DirEntry, ignore::Error>,
    root: &Path,
) -> Result<SearchWalkEntry, PathBuf> {
    let entry =
        entry.map_err(|error| search_walk_error_path(&error).unwrap_or(root).to_path_buf())?;
    Ok(SearchWalkEntry {
        path: entry.path().to_path_buf(),
        is_dir: entry.file_type().is_some_and(|kind| kind.is_dir()),
    })
}

fn search_walk_error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            search_walk_error_path(err)
        }
        ignore::Error::Partial(errors) => errors.iter().find_map(search_walk_error_path),
        ignore::Error::Loop { child, .. } => Some(child),
        _ => None,
    }
}

fn walk_search_entries(root: &Path) -> impl Iterator<Item = Result<SearchWalkEntry, PathBuf>> + '_ {
    let mut walker = WalkBuilder::new(root);
    walker
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .hidden(false)
        .follow_links(false);
    // Links are neither followed nor opened: a file symlink would expose a
    // target outside the workspace that `read` rejects. On Windows std also
    // reports junctions as symlinks.
    walker.filter_entry(|entry| {
        !should_skip_entry(entry.path()) && !entry.file_type().is_some_and(|kind| kind.is_symlink())
    });
    walker
        .build()
        .map(|entry| map_search_walk_entry(entry, root))
}

fn search_with_walker(
    root: &Path,
    patterns: &[String],
    hit_limit: usize,
    context_lines: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<SearchScan, ToolExecutionError> {
    search_with_entries(
        root,
        patterns,
        hit_limit,
        context_lines,
        cancellation,
        walk_search_entries(root),
        open_search_file,
    )
}

fn open_search_file(
    path: &Path,
    evidence: &mut SearchEvidence,
) -> Result<SearchOpenedFile<BufReader<File>>, SearchOpenFailure> {
    let file = File::open(path).map_err(|_| SearchOpenFailure::Open)?;
    let metadata = file.metadata().map_err(|_| SearchOpenFailure::Metadata)?;
    evidence.record_file(path, &file, &metadata);
    Ok(SearchOpenedFile {
        reader: BufReader::new(file),
        len: metadata.len(),
    })
}

fn search_with_entries<I, F, R>(
    root: &Path,
    patterns: &[String],
    hit_limit: usize,
    context_lines: usize,
    cancellation: Option<&CancellationToken>,
    entries: I,
    mut open_file: F,
) -> Result<SearchScan, ToolExecutionError>
where
    I: IntoIterator<Item = Result<SearchWalkEntry, PathBuf>>,
    F: FnMut(&Path, &mut SearchEvidence) -> Result<SearchOpenedFile<R>, SearchOpenFailure>,
    R: BufRead,
{
    let mut hits: Vec<MatchedSearchHit> = Vec::new();
    let mut capped = false;
    let mut scan_complete = true;
    let mut work_limited = false;
    let mut unsearched_files = 0usize;
    let mut io_failures = SearchIoSummary::default();
    let mut coverage = vec![SearchPatternCoverage::default(); patterns.len()];
    let mut scanned_files = 0usize;
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
    'walk: for entry in entries {
        if let Err(error) = check_cancelled(cancellation) {
            return Err(evidence.error(error));
        }
        // Per-entry walk errors (permission, transient lock) skip the entry
        // instead of failing the whole search; the root itself is guarded
        // above.
        let entry = match entry {
            Ok(entry) => entry,
            Err(path) => {
                scan_complete = false;
                io_failures.record(SearchIoFailureKind::Walk, root, &path);
                continue;
            }
        };
        let path = entry.path;
        if entry.is_dir || should_skip_file(&path, root) {
            continue;
        }
        if scanned_files >= MAX_SEARCH_SCAN_FILES {
            scan_complete = false;
            work_limited = true;
            break 'walk;
        }
        scanned_files = scanned_files.saturating_add(1);
        // Unreadable files are recorded and skipped: one bad file must not
        // invalidate hits already collected.
        let opened = match open_file(&path, &mut evidence) {
            Ok(opened) => opened,
            Err(failure) => {
                scan_complete = false;
                io_failures.record(failure.io_kind(), root, &path);
                continue;
            }
        };
        if opened.len > MAX_FILE_BYTES {
            unsearched_files = unsearched_files.saturating_add(1);
            continue;
        }
        let mut reader = opened.reader;
        let sample = match reader.fill_buf() {
            Ok(sample) => sample,
            Err(_) => {
                scan_complete = false;
                io_failures.record(SearchIoFailureKind::Read, root, &path);
                continue;
            }
        };
        if sample.contains(&0) {
            if !record_search_bytes(&mut evidence, sample) {
                scan_complete = false;
                work_limited = true;
                break 'walk;
            }
            unsearched_files = unsearched_files.saturating_add(1);
            continue;
        }

        let mut line_bytes = Vec::new();
        let mut line_number = 0usize;
        let file_hits_start = hits.len();
        let mut preceding = VecDeque::new();
        loop {
            if let Err(error) = check_cancelled(cancellation) {
                return Err(evidence.error(error));
            }
            line_bytes.clear();
            match reader.read_until(b'\n', &mut line_bytes) {
                Ok(0) => break,
                Ok(_) => {
                    if !record_search_bytes(&mut evidence, &line_bytes) {
                        scan_complete = false;
                        work_limited = true;
                        break 'walk;
                    }
                }
                Err(_) => {
                    // An unreadable chunk ends this file, not the search. The
                    // partially accumulated line is not a complete hit.
                    scan_complete = false;
                    io_failures.record(SearchIoFailureKind::Read, root, &path);
                    if !record_search_bytes(&mut evidence, &line_bytes) {
                        work_limited = true;
                        break 'walk;
                    }
                    break;
                }
            }
            line_number = line_number.saturating_add(1);
            // Context shares the scan; bounded lines are shared by overlapping hits.
            let context = (context_lines > 0).then(|| {
                let text = match std::str::from_utf8(&line_bytes) {
                    Ok(text) => bound_text(
                        strip_line_ending_str(text).to_owned(),
                        MAX_CONTEXT_TEXT_BYTES,
                    ),
                    Err(_) => "[context unavailable: invalid UTF-8]".into(),
                };
                Arc::new((line_number, text))
            });
            if let Some(context) = &context {
                for hit in hits[file_hits_start..].iter_mut().rev() {
                    if line_number.saturating_sub(hit.hit.line) > context_lines {
                        break;
                    }
                    hit.context.push(Arc::clone(context));
                }
                preceding.push_back(Arc::clone(context));
                if preceding.len() > context_lines + 1 {
                    preceding.pop_front();
                }
            }
            if capped
                && hits.len() >= hit_limit
                && coverage.iter().all(|pattern| pattern.retained > 0)
            {
                let context_drained = context_lines == 0
                    || hits.last().is_none_or(|hit| {
                        line_number >= hit.hit.line.saturating_add(context_lines)
                    });
                if context_drained {
                    // The retained snapshot is full and every pattern has a
                    // representative hit. Context for the final hit was
                    // drained above; no further scan can improve coverage.
                    scan_complete = false;
                    break 'walk;
                }
                continue;
            }
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
                unsearched_files = unsearched_files.saturating_add(1);
                break;
            };
            let line = strip_line_ending_str(line_str);
            let mut matched = patterns
                .iter()
                .enumerate()
                .filter_map(|(pattern_index, pattern)| {
                    line.contains(pattern).then_some(pattern_index)
                })
                .collect::<Vec<_>>();
            if matched.is_empty() {
                continue;
            }
            for &pattern_index in &matched {
                coverage[pattern_index].observed =
                    coverage[pattern_index].observed.saturating_add(1);
            }
            // Give patterns without a retained representative first choice of
            // the shared result budget. Covered patterns then use only slots
            // left after one slot per uncovered pattern is reserved.
            matched.sort_by_key(|&pattern_index| coverage[pattern_index].retained > 0);
            let mut hit_text = None;
            for pattern_index in matched.iter().copied() {
                let uncovered = coverage
                    .iter()
                    .filter(|pattern| pattern.retained == 0)
                    .count();
                let remaining = hit_limit.saturating_sub(hits.len());
                let retain = if coverage[pattern_index].retained == 0 {
                    remaining > 0
                } else {
                    remaining > uncovered
                };
                if !retain {
                    coverage[pattern_index].omitted =
                        coverage[pattern_index].omitted.saturating_add(1);
                    capped = true;
                    if context_lines == 0 && coverage.iter().all(|pattern| pattern.retained > 0) {
                        scan_complete = false;
                        break 'walk;
                    }
                    continue;
                }
                hits.push(MatchedSearchHit {
                    hit: SearchHit {
                        path: path.to_path_buf(),
                        line: line_number,
                        text: hit_text
                            .get_or_insert_with(|| bound_hit_text(line, patterns, &matched))
                            .clone(),
                    },
                    pattern_index,
                    context: preceding
                        .iter()
                        .filter(|line| line.0 < line_number)
                        .cloned()
                        .collect(),
                });
                coverage[pattern_index].retained =
                    coverage[pattern_index].retained.saturating_add(1);
            }
        }
        if capped && hits.len() >= hit_limit && coverage.iter().all(|pattern| pattern.retained > 0)
        {
            scan_complete = false;
            break;
        }
    }
    let (dependency, bytes_read) = evidence.finish();
    Ok(SearchScan {
        hits,
        capped,
        scan_complete,
        work_limited,
        unsearched_files,
        coverage,
        io_failures,
        dependency,
        bytes_read,
    })
}

/// Wall-clock allowance for the whole case-insensitive recheck. It exists to
/// explain a miss; it must not turn a fast search into a slow one.
const CASE_RECHECK_BUDGET: Duration = Duration::from_millis(250);
/// Lines between looks at the clock and the cancellation token.
const CASE_RECHECK_CHECK_INTERVAL: usize = 256;

/// After a complete scan, looks once more, ignoring letter case, for the
/// patterns that matched nothing. The recheck re-reads the same files under the
/// same work budgets and a wall-clock budget; when it cannot finish inside them
/// it adds nothing.
fn note_case_variants(
    root: &Path,
    patterns: &[String],
    scan: &mut SearchScan,
    cancellation: Option<&CancellationToken>,
) -> Result<(), ToolExecutionError> {
    if !scan.scan_complete || scan.work_limited || scan.io_failures.has_failures() {
        return Ok(());
    }
    let missing = scan
        .coverage
        .iter()
        .enumerate()
        .filter(|(_, coverage)| coverage.observed == 0)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    for (index, variant) in
        find_case_variants(root, patterns, &missing, cancellation, CASE_RECHECK_BUDGET)?
    {
        scan.coverage[index].case_variant = Some(variant);
    }
    Ok(())
}

/// A pattern lowercased once, with its ASCII form when it has one: an ASCII
/// pattern is matched on bytes, so a line that does not hold it costs one pass.
struct CaseNeedle {
    index: usize,
    lowered: Vec<char>,
    ascii: Option<Vec<u8>>,
}

impl CaseNeedle {
    fn new(index: usize, pattern: &str) -> Self {
        Self {
            index,
            lowered: pattern.chars().flat_map(char::to_lowercase).collect(),
            ascii: pattern
                .is_ascii()
                .then(|| pattern.to_ascii_lowercase().into_bytes()),
        }
    }

    /// The text of `line` that equals the pattern ignoring case.
    fn find_in<'a>(&self, line: &'a str) -> Option<&'a str> {
        match &self.ascii {
            Some(ascii) => find_ascii_ignoring_case(line, ascii),
            None => find_ignoring_case(line, &self.lowered),
        }
    }
}

fn find_case_variants(
    root: &Path,
    patterns: &[String],
    missing: &[usize],
    cancellation: Option<&CancellationToken>,
    budget: Duration,
) -> Result<Vec<(usize, CaseVariant)>, ToolExecutionError> {
    let mut needles = missing
        .iter()
        .filter(|&&index| patterns[index].chars().any(char::is_alphabetic))
        .map(|&index| CaseNeedle::new(index, &patterns[index]))
        .collect::<Vec<_>>();
    if needles.is_empty() {
        return Ok(Vec::new());
    }
    let started = Instant::now();
    let mut found = Vec::new();
    let mut scanned_files = 0usize;
    let mut scanned_bytes = 0u64;
    let mut lines_until_check = 0usize;
    let mut line_bytes = Vec::new();
    for entry in walk_search_entries(root) {
        check_cancelled(cancellation)?;
        let Ok(entry) = entry else {
            return Ok(Vec::new());
        };
        if entry.is_dir || should_skip_file(&entry.path, root) {
            continue;
        }
        scanned_files = scanned_files.saturating_add(1);
        if scanned_files > MAX_SEARCH_SCAN_FILES {
            return Ok(Vec::new());
        }
        let Ok(file) = File::open(&entry.path) else {
            return Ok(Vec::new());
        };
        let Ok(metadata) = file.metadata() else {
            return Ok(Vec::new());
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let mut reader = BufReader::new(file);
        match reader.fill_buf() {
            Ok(sample) if sample.contains(&0) => continue,
            Ok(_) => {}
            Err(_) => return Ok(Vec::new()),
        }
        let mut line_number = 0usize;
        loop {
            if lines_until_check == 0 {
                check_cancelled(cancellation)?;
                if started.elapsed() >= budget {
                    return Ok(Vec::new());
                }
                lines_until_check = CASE_RECHECK_CHECK_INTERVAL;
            }
            lines_until_check -= 1;
            line_bytes.clear();
            match reader.read_until(b'\n', &mut line_bytes) {
                Ok(0) => break,
                Ok(read) => {
                    scanned_bytes = scanned_bytes.saturating_add(read as u64);
                    if scanned_bytes > MAX_SEARCH_SCAN_BYTES {
                        return Ok(Vec::new());
                    }
                }
                Err(_) => return Ok(Vec::new()),
            }
            line_number = line_number.saturating_add(1);
            // Invalid UTF-8 ends the file, as in the scan itself.
            let Ok(text) = std::str::from_utf8(&line_bytes) else {
                break;
            };
            let line = strip_line_ending_str(text);
            needles.retain(|needle| {
                let Some(spelling) = needle
                    .find_in(line)
                    .filter(|found| *found != patterns[needle.index])
                else {
                    return true;
                };
                found.push((
                    needle.index,
                    CaseVariant {
                        path: entry.path.clone(),
                        line: line_number,
                        spelling: spelling
                            .chars()
                            .take(CASE_VARIANT_NOTE_CHARS)
                            .map(|character| {
                                if character.is_control() {
                                    '?'
                                } else {
                                    character
                                }
                            })
                            .collect(),
                    },
                ));
                false
            });
            if needles.is_empty() {
                return Ok(found);
            }
        }
    }
    Ok(found)
}

/// `needle` (ASCII, lowercase) in `line` ignoring ASCII case, on bytes. A match
/// is all ASCII, so its ends are character boundaries.
fn find_ascii_ignoring_case<'a>(line: &'a str, needle: &[u8]) -> Option<&'a str> {
    let bytes = line.as_bytes();
    let first = *needle.first()?;
    let mut from = 0;
    while from + needle.len() <= bytes.len() {
        let offset = bytes[from..=bytes.len() - needle.len()]
            .iter()
            .position(|byte| byte.to_ascii_lowercase() == first)?;
        let start = from + offset;
        if bytes[start..start + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(&line[start..start + needle.len()]);
        }
        from = start + 1;
    }
    None
}

/// The text of `line` that equals `needle` (already lowercased) ignoring case.
fn find_ignoring_case<'a>(line: &'a str, needle: &[char]) -> Option<&'a str> {
    let first = *needle.first()?;
    for (start, character) in line.char_indices() {
        if character.to_lowercase().next() != Some(first) {
            continue;
        }
        let mut matched = 0;
        'candidate: for (offset, character) in line[start..].char_indices() {
            for lowered in character.to_lowercase() {
                if needle.get(matched) != Some(&lowered) {
                    break 'candidate;
                }
                matched += 1;
            }
            if matched == needle.len() {
                return Some(&line[start..start + offset + character.len_utf8()]);
            }
        }
    }
    None
}

/// Record only the portion of a read that fits inside the aggregate search
/// work budget. Returning false tells the caller that the scan must stop
/// without claiming that remaining patterns are absent.
fn record_search_bytes(evidence: &mut SearchEvidence, bytes: &[u8]) -> bool {
    let remaining = MAX_SEARCH_SCAN_BYTES.saturating_sub(evidence.bytes_read);
    let keep = bytes
        .len()
        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
    evidence.record_bytes(&bytes[..keep]);
    keep == bytes.len()
}

/// A hit line as the model sees it. A line over the limit is windowed around
/// the earliest match of `matched`, so the match is always in view; what is cut
/// is marked as truncated, and such text is not safe for patch.expected.
fn bound_hit_text(line: &str, patterns: &[String], matched: &[usize]) -> String {
    if line.len() <= MAX_HIT_TEXT_BYTES {
        return line.to_owned();
    }
    let anchor = matched
        .iter()
        .filter_map(|&index| {
            let pattern = patterns[index].as_str();
            line.find(pattern).map(|start| start..start + pattern.len())
        })
        .min_by_key(|range| range.start)
        .unwrap_or(0..0);
    let mut start = anchor.start.saturating_sub(HIT_WINDOW_CONTEXT_BYTES);
    while !line.is_char_boundary(start) {
        start += 1;
    }
    let mut end = anchor
        .end
        .saturating_add(HIT_WINDOW_CONTEXT_BYTES)
        .min(line.len());
    end = end.min(start + MAX_HIT_TEXT_BYTES - 2 * TRUNCATION_MARKER_RESERVE_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = String::with_capacity(end - start + 2 * TRUNCATION_MARKER_RESERVE_BYTES);
    if start > 0 {
        text.push_str(&format!("[truncated {start} bytes] "));
    }
    text.push_str(&line[start..end]);
    if end < line.len() {
        text.push_str(&format!("\n[truncated {} bytes]", line.len() - end));
    }
    text
}

fn bound_text(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit - TRUNCATION_MARKER_RESERVE_BYTES;
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

/// Generated and lock files are skipped in directory scans; a file named as
/// the search root was asked for explicitly and is always searched.
fn should_skip_file(path: &Path, root: &Path) -> bool {
    if path == root {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("map" | "lock")
    ) || name.ends_with(".min.js")
}

fn strip_line_ending_str(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(line) => line.strip_suffix('\r').unwrap_or(line),
        None => line,
    }
}

fn bytes_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let rest = &needle[1..];
    let mut offset = 0;
    while let Some(pos) = haystack[offset..].iter().position(|&b| b == first) {
        let start = offset + pos;
        if start + needle.len() > haystack.len() {
            return false;
        }
        if &haystack[start + 1..start + needle.len()] == rest {
            return true;
        }
        offset = start + 1;
    }
    false
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
    use std::io::{self, BufRead, Cursor, Read};

    #[test]
    fn presentation_pages_preserve_multipattern_records_context_and_final_cursor() {
        let root = temp_root("presentation-records");
        fs::create_dir_all(&root).unwrap();
        let content = (0..20)
            .map(|n| format!("COMMON RARE row{n:03} {}\n", "界".repeat(80)))
            .collect::<String>();
        fs::write(root.join("data.txt"), content).unwrap();
        let service = SearchService::default();
        let patterns = vec!["COMMON".into(), "RARE".into()];
        let options = SearchPageOptions {
            offset: 1,
            max_hits: 500,
            context_lines: 1,
        };
        let mut page = service
            .page(&root, patterns.clone(), options, None, None)
            .unwrap();
        assert_eq!(page.total, 40);
        assert!(page.next_cursor.is_none());
        let zero = page.present(1, &root);
        assert_eq!(zero.delivered_records, 0);
        assert!(zero.text.contains(&format!("{}:1", page.snapshot_id)));
        let mut seen = 0;
        loop {
            let projection = page.present(2200, &root);
            assert!(projection.text.len() <= 2200);
            assert!(projection.delivered_records > 0);
            for hit in &page.hits[..projection.delivered_records] {
                assert!(projection.text.contains(&hit.hit.text));
                for line in &hit.context {
                    assert!(projection.text.contains(&line.1));
                }
            }
            seen += projection.delivered_records;
            if seen == 40 {
                break;
            }
            let next = page.first + projection.delivered_records;
            let cursor = format!("{}:{next:x}", page.snapshot_id);
            assert!(projection.text.contains(&cursor));
            page = service
                .page(&root, patterns.clone(), options, Some(&cursor), None)
                .unwrap();
            assert_eq!(page.first, next);
            assert!(seen < 40);
        }
        assert_eq!(seen, 40);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observed_pattern_is_not_complete_when_snapshot_stops_early() {
        let root = temp_root("partial-coverage");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("data.txt"),
            format!("RARE\n{}RARE\n", "COMMON\n".repeat(600)),
        )
        .unwrap();
        let page = SearchService::default()
            .page(
                &root,
                vec!["COMMON".into(), "RARE".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 500,
                    context_lines: 0,
                },
                None,
                None,
            )
            .unwrap();
        assert!(!page.scan_complete);
        let output = format_search_batch_page(&page, &root);
        assert!(output.contains("`RARE`: 1 retained; scan incomplete"));
        assert!(!output.contains("full coverage"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expired_search_snapshot_does_not_reapply_its_offset_to_new_hits() {
        let root = temp_root("expired-cursor");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data.txt"), "needle A\nneedle B\nneedle C\n").unwrap();
        let service = SearchService::default();
        let first = service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 0,
                },
                None,
                None,
            )
            .unwrap();
        let cursor = first.next_cursor.as_deref().expect("cursor");
        let (id, _) = parse_cursor(cursor).unwrap();
        service
            .snapshots
            .lock()
            .unwrap()
            .snapshots
            .get_mut(id)
            .unwrap()
            .expires_at = Instant::now();
        fs::write(root.join("data.txt"), "needle B\nneedle C\n").unwrap();
        let error = service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 0,
                },
                Some(cursor),
                None,
            )
            .unwrap_err();
        assert!(
            matches!(error.error, ToolError::InvalidInput { message } if message.contains("start a new search"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn context_snapshot_is_bounded_historical_and_option_specific() {
        let root = temp_root("context-snapshot");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("data.txt");
        fs::write(&path, "ação\r\nneedle A\r\nafter\r\nneedle B\r\nend").unwrap();
        let service = SearchService::default();
        let first = service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 1,
                },
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            first.hits[0]
                .context
                .iter()
                .map(|x| (x.0, x.1.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "ação"), (3, "after")]
        );
        let cursor = first.next_cursor.as_deref().unwrap();
        let plain_offset = service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 2,
                    max_hits: 1,
                    context_lines: 0,
                },
                None,
                None,
            )
            .unwrap();
        assert!(
            plain_offset.bytes_read > 0,
            "different context must not reuse the snapshot"
        );
        assert!(plain_offset.hits[0].context.is_empty());
        let historical_offset = service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 2,
                    max_hits: 1,
                    context_lines: 1,
                },
                None,
                None,
            )
            .unwrap();
        assert_eq!(historical_offset.bytes_read, 0);
        assert!(!historical_offset.hits[0].context.is_empty());
        fs::write(&path, "changed\nneedle fresh\n").unwrap();
        let historical = service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 1,
                },
                Some(cursor),
                None,
            )
            .unwrap();
        assert_eq!(historical.bytes_read, 0);
        assert_eq!(historical.hits[0].hit.text, "needle B");
        assert_eq!(historical.hits[0].context[1].1, "end");
        assert!(service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 0
                },
                Some(cursor),
                None
            )
            .is_err());
        let fresh = service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 1,
                },
                None,
                None,
            )
            .unwrap();
        assert_eq!(fresh.hits[0].context[0].1, "changed");
        let token = CancellationToken::default();
        token.cancel();
        assert!(service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 1
                },
                Some(cursor),
                Some(&token)
            )
            .is_err());
        service.snapshots.lock().unwrap().snapshots.clear();
        assert!(service
            .page(
                &root,
                vec!["needle".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 1
                },
                Some(cursor),
                None
            )
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_preserves_matches_caps_encoding_and_ignore_rules() {
        let root = temp_root("context-limits");
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/skip.txt"), "needle ignored").unwrap();
        let text = format!(
            "{}\n{}tail\n",
            "needle\n".repeat(MAX_SEARCH_SNAPSHOT_HITS + 1),
            "é".repeat(2048)
        );
        fs::write(root.join("data.txt"), &text).unwrap();
        let plain =
            search_with_walker(&root, &["needle".into()], MAX_SEARCH_SNAPSHOT_HITS, 0, None)
                .unwrap();
        let context =
            search_with_walker(&root, &["needle".into()], MAX_SEARCH_SNAPSHOT_HITS, 3, None)
                .unwrap();
        assert!(plain.capped && context.capped);
        assert_eq!(
            plain.hits.iter().map(|h| &h.hit).collect::<Vec<_>>(),
            context.hits.iter().map(|h| &h.hit).collect::<Vec<_>>()
        );
        assert!(context.hits.iter().all(|h| h.context.len() <= 6
            && h.context
                .iter()
                .all(|line| line.1.len() <= MAX_CONTEXT_TEXT_BYTES)));
        assert!(context
            .hits
            .last()
            .unwrap()
            .context
            .iter()
            .any(|line| line.1.contains("truncated")));
        assert!(context.bytes_read >= plain.bytes_read);
        fs::write(root.join("data.txt"), b"before\xff\nneedle\nafter\n").unwrap();
        let context = search_with_walker(&root, &["needle".into()], 500, 1, None).unwrap();
        assert_eq!(context.hits.len(), 1);
        assert!(context.hits[0].context[0].1.contains("unavailable"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_edges_overlap_unicode_and_snapshot_storage() {
        let root = temp_root("context-edges");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("data.txt");
        fs::write(&path, "needle first\nshared\nneedle last").unwrap();
        let service = SearchService::default();
        let options = SearchPageOptions {
            offset: 1,
            max_hits: 500,
            context_lines: 3,
        };
        let page = service
            .page(&root, vec!["needle".into()], options, None, None)
            .unwrap();
        assert_eq!(
            page.hits[0]
                .context
                .iter()
                .map(|line| line.0)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(
            page.hits[1]
                .context
                .iter()
                .map(|line| line.0)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(Arc::ptr_eq(
            &page.hits[0].context[0],
            &page.hits[1].context[1]
        ));
        let rendered = format_search_batch_page(&page, &root);
        assert!(
            rendered.starts_with("[data.txt]\n1: needle first\n2- shared\n3: needle last\n"),
            "{rendered}"
        );
        // Shared context lines render once, not once per overlapping hit.
        assert_eq!(rendered.matches("2- shared").count(), 1, "{rendered}");
        // A line can be context for one hit and a match in its own record.
        let multi = service
            .page(
                &root,
                vec!["needle".into(), "first".into()],
                options,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            multi
                .hits
                .iter()
                .map(|h| (h.hit.line, h.pattern_index))
                .collect::<Vec<_>>(),
            vec![(1, 0), (1, 1), (3, 0)]
        );
        assert!(multi
            .hits
            .iter()
            .all(|hit| hit.context.iter().all(|line| line.0 != hit.hit.line)));
        let unicode = format!(
            "{}🦀tail",
            "a".repeat(MAX_CONTEXT_TEXT_BYTES - TRUNCATION_MARKER_RESERVE_BYTES - 1)
        );
        let bounded = bound_text(unicode.repeat(2), MAX_CONTEXT_TEXT_BYTES);
        assert!(bounded.len() <= MAX_CONTEXT_TEXT_BYTES);
        assert!(bounded.contains("[truncated "));
        assert!(!bounded.contains('�'));
        for _ in 0..=MAX_SEARCH_SNAPSHOTS {
            service
                .page(&root, vec!["needle".into()], options, None, None)
                .unwrap();
        }
        let cache = service.snapshots.lock().unwrap();
        assert_eq!(cache.snapshots.len(), MAX_SEARCH_SNAPSHOTS);
        assert!(cache.snapshots.values().all(|snapshot| snapshot.hits.len()
            <= MAX_SEARCH_SNAPSHOT_HITS
            && snapshot
                .hits
                .iter()
                .all(|hit| hit.hit.text.len() <= MAX_HIT_TEXT_BYTES
                    && hit.context.len() <= 2 * MAX_CONTEXT_LINES
                    && hit
                        .context
                        .iter()
                        .all(|line| line.1.len() <= MAX_CONTEXT_TEXT_BYTES))));
        drop(cache);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_zero_preserves_literal_trailing_carriage_return() {
        let root = temp_root("context-zero-cr");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data.txt"), "needle\r").unwrap();
        for context in [0, 3] {
            let scan = search_with_walker(&root, &["needle\r".into()], 500, context, None).unwrap();
            assert_eq!(scan.hits.len(), 1);
            assert_eq!(scan.hits[0].hit.text, "needle\r");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_native_contract_and_patch_preconditions() {
        use crate::{tools::ToolRegistry, OperatingMode};
        let root = temp_root("context-contract");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("data.txt"),
            "fn limit() {\n// needle\nreturn 10;\n}\n",
        )
        .unwrap();
        let registry = ToolRegistry::default();
        let run = |name, args: serde_json::Value| {
            registry.execute(OperatingMode::Auto, &root, name, &args.to_string())
        };
        let result = run(
            "search",
            serde_json::json!({"query":"needle", "context_lines":1}),
        );
        assert!(result.success);
        assert!(
            result
                .output
                .contains("[data.txt]\n1- fn limit() {\n2: // needle\n3- return 10;"),
            "{}",
            result.output
        );
        let saturated = run(
            "search",
            serde_json::json!({"query":"needle", "context_lines":4}),
        );
        let bounded = run(
            "search",
            serde_json::json!({"query":"needle", "context_lines":3}),
        );
        assert!(saturated.success && bounded.success);
        assert_eq!(
            saturated
                .output
                .strip_prefix("[admission: context_lines 4 -> 3; maximum]\n"),
            Some(bounded.output.as_str())
        );
        for value in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(null),
            serde_json::json!(true),
        ] {
            assert!(
                !run(
                    "search",
                    serde_json::json!({"query":"needle", "context_lines":value})
                )
                .success
            );
        }
        // An integer-valued string is a number for admission.
        assert!(
            run(
                "search",
                serde_json::json!({"query":"needle", "context_lines":"1"})
            )
            .success
        );
        assert!(
            !run(
                "search",
                serde_json::json!({"path":"../outside", "query":"needle", "context_lines":1})
            )
            .success
        );
        assert!(
            !run(
                "write",
                serde_json::json!({"path":"data.txt", "content":"replace"})
            )
            .success
        );
        fs::write(
            root.join("data.txt"),
            "fn limit() {\n// needle\nreturn 20;\n}\n",
        )
        .unwrap();
        assert!(!run("patch", serde_json::json!({"path":"data.txt", "edits":[{"expected":"// needle\nreturn 10;", "replacement":"// needle\nreturn 30;"}]})).success);
        assert!(fs::read_to_string(root.join("data.txt"))
            .unwrap()
            .contains("return 20;"));
        // Three lines need not make the expected excerpt unique.
        fs::write(
            root.join("data.txt"),
            "needle\nreturn 20;\nneedle\nreturn 20;\n",
        )
        .unwrap();
        assert!(
            run(
                "search",
                serde_json::json!({"query":"needle", "context_lines":3})
            )
            .success
        );
        assert!(!run("patch", serde_json::json!({"path":"data.txt", "edits":[{"expected":"needle\nreturn 20;", "replacement":"changed"}]})).success);
        assert_eq!(
            fs::read_to_string(root.join("data.txt")).unwrap(),
            "needle\nreturn 20;\nneedle\nreturn 20;\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grep_formatted_search_hit_is_not_a_valid_patch_expected() {
        use crate::{tools::ToolRegistry, OperatingMode};
        let root = temp_root("grep-handoff");
        fs::create_dir_all(&root).unwrap();
        let body = "fn limit() {\n    let needle = 1;\n    return needle;\n}\n";
        fs::write(root.join("data.txt"), body).unwrap();
        let registry = ToolRegistry::default();
        let run = |name, args: serde_json::Value| {
            registry.execute(OperatingMode::Auto, &root, name, &args.to_string())
        };
        let located = run(
            "search",
            serde_json::json!({"query":"needle", "context_lines":1}),
        );
        assert!(located.success, "{}", located.output);
        assert!(
            located.output.contains("[data.txt]\n1- fn limit() {"),
            "{}",
            located.output
        );
        assert!(
            located
                .output
                .contains("2:     let needle = 1;\n3:     return needle;\n4- }"),
            "{}",
            located.output
        );

        let prefixed = run(
            "patch",
            serde_json::json!({"path":"data.txt","edits":[{"expected":"data.txt:2:     let needle = 1;","replacement":"    let needle = 2;"}]}),
        );
        assert!(!prefixed.success, "{}", prefixed.output);
        assert!(prefixed.output.contains("got 0"), "{}", prefixed.output);
        assert_eq!(fs::read_to_string(root.join("data.txt")).unwrap(), body);

        let block = run(
            "patch",
            serde_json::json!({"path":"data.txt","edits":[{"expected":"data.txt-1- fn limit() {\ndata.txt:2:     let needle = 1;","replacement":"changed"}]}),
        );
        assert!(!block.success, "{}", block.output);
        assert!(block.output.contains("got 0"), "{}", block.output);

        let raw = run(
            "patch",
            serde_json::json!({"path":"data.txt","edits":[{"expected":"    let needle = 1;","replacement":"    let needle = 2;"}]}),
        );
        assert!(raw.success, "{}", raw.output);
        assert_eq!(
            fs::read_to_string(root.join("data.txt")).unwrap(),
            "fn limit() {\n    let needle = 2;\n    return needle;\n}\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unique_default_search_line_is_enough_for_patch() {
        use crate::{tools::ToolRegistry, OperatingMode};
        let root = temp_root("zero-context-handoff");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data.txt"), "alpha\nunique needle here\nomega\n").unwrap();
        let registry = ToolRegistry::default();
        let run = |name, args: serde_json::Value| {
            registry.execute(OperatingMode::Auto, &root, name, &args.to_string())
        };
        let located = run("search", serde_json::json!({"query":"needle"}));
        assert!(located.success, "{}", located.output);
        assert!(
            located.output.contains("[data.txt]\n2: unique needle here"),
            "{}",
            located.output
        );
        assert!(!located.output.contains("1- alpha"), "{}", located.output);
        let patched = run(
            "patch",
            serde_json::json!({"path":"data.txt","edits":[{"expected":"unique needle here","replacement":"unique needle gone"}]}),
        );
        assert!(patched.success, "{}", patched.output);
        assert_eq!(
            fs::read_to_string(root.join("data.txt")).unwrap(),
            "alpha\nunique needle gone\nomega\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn truncated_search_hit_text_is_not_a_valid_patch_expected() {
        use crate::{tools::ToolRegistry, OperatingMode};
        let root = temp_root("truncated-hit-handoff");
        fs::create_dir_all(&root).unwrap();
        let long = format!("{}needle{}", "A".repeat(9000), "B".repeat(200));
        fs::write(root.join("data.txt"), format!("{long}\n")).unwrap();
        let registry = ToolRegistry::default();
        let run = |name, args: serde_json::Value| {
            registry.execute(OperatingMode::Auto, &root, name, &args.to_string())
        };
        let located = run("search", serde_json::json!({"query":"needle"}));
        assert!(located.success, "{}", located.output);
        assert!(located.output.contains("[truncated "), "{}", located.output);
        let marker_start = located.output.find("\n1: ").expect("hit");
        let after_prefix = &located.output[marker_start + "\n1: ".len()..];
        // A window cut only before the match has its marker inline and no
        // second line.
        let rendered = after_prefix
            .split_once('\n')
            .map(|(head, rest)| {
                if rest.starts_with("[truncated ") {
                    format!("{head}\n{}", rest.lines().next().unwrap_or(""))
                } else {
                    head.to_owned()
                }
            })
            .unwrap_or_else(|| after_prefix.to_owned());
        assert!(rendered.contains("[truncated "), "{rendered}");
        let copied = run(
            "patch",
            serde_json::json!({"path":"data.txt","edits":[{"expected":rendered,"replacement":"x"}]}),
        );
        assert!(!copied.success, "{}", copied.output);
        assert!(copied.output.contains("got 0"), "{}", copied.output);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grep_hit_text_after_colon_space_is_the_patch_expected() {
        use crate::{tools::ToolRegistry, OperatingMode};
        let root = temp_root("copy-after-prefix");
        fs::create_dir_all(&root).unwrap();
        let body = "fn limit() {\n    let needle = 1;\n    return needle;\n}\n";
        fs::write(root.join("data.txt"), body).unwrap();
        let registry = ToolRegistry::default();
        let run = |name, args: serde_json::Value| {
            registry.execute(OperatingMode::Auto, &root, name, &args.to_string())
        };
        let located = run(
            "search",
            serde_json::json!({"query":"let needle", "context_lines":1}),
        );
        assert!(located.success, "{}", located.output);
        let prefix = "\n2: ";
        let excerpt = located
            .output
            .split_once(prefix)
            .map(|(_, rest)| rest.lines().next().unwrap_or(""))
            .expect("hit line");
        assert_eq!(excerpt, "    let needle = 1;");
        let patched = run(
            "patch",
            serde_json::json!({"path":"data.txt","edits":[{"expected":excerpt,"replacement":"    let needle = 2;"}]}),
        );
        assert!(patched.success, "{}", patched.output);
        assert_eq!(
            fs::read_to_string(root.join("data.txt")).unwrap(),
            "fn limit() {\n    let needle = 2;\n    return needle;\n}\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_page_context_footer_does_not_steer_a_read() {
        use crate::{tools::ToolRegistry, OperatingMode};
        let root = temp_root("first-page-footer");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data.txt"), "alpha\nunique needle here\nomega\n").unwrap();
        let located = ToolRegistry::default().execute(
            OperatingMode::Auto,
            &root,
            "search",
            &serde_json::json!({"query":"needle", "context_lines":1}).to_string(),
        );
        assert!(located.success, "{}", located.output);
        assert!(
            located.output.contains("[context: up to 1 lines each side"),
            "{}",
            located.output
        );
        assert!(
            !located.output.contains("Use read for more"),
            "{}",
            located.output
        );
        assert!(
            !located
                .output
                .contains("Snapshot, not a current-file guarantee"),
            "{}",
            located.output
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_page_keeps_snapshot_warning() {
        use crate::{tools::ToolRegistry, OperatingMode};
        let root = temp_root("cursor-page-footer");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("data.txt"), "needle A\nneedle B\nneedle C\n").unwrap();
        let first = ToolRegistry::default().execute(
            OperatingMode::Auto,
            &root,
            "search",
            &serde_json::json!({"query":"needle", "context_lines":1, "max_hits":1}).to_string(),
        );
        assert!(first.success, "{}", first.output);
        assert!(first.output.contains("pass \"cursor\""), "{}", first.output);
        assert!(
            first
                .output
                .contains("Snapshot, not a current-file guarantee"),
            "{}",
            first.output
        );
        assert!(
            !first.output.contains("Use read for more"),
            "{}",
            first.output
        );
        fs::remove_dir_all(root).unwrap();
    }

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

    fn formatted_search(root: &Path, patterns: &[&str]) -> String {
        let canonical = root.canonicalize().expect("canonical");
        let page = SearchService::default()
            .page(
                &canonical,
                patterns
                    .iter()
                    .map(|pattern| (*pattern).to_owned())
                    .collect(),
                SearchPageOptions {
                    offset: 1,
                    max_hits: DEFAULT_MAX_HITS,
                    context_lines: 0,
                },
                None,
                None,
            )
            .expect("search");
        format_search_batch_page(&page, &canonical)
    }

    #[test]
    fn absent_regex_looking_patterns_carry_a_literal_search_note() {
        let root = temp_root("regex-note");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("a.rs"), "fn handle_error() {}\nif a || b {}\n").expect("text");
        for pattern in ["handle.*error", "fn \\w+", "foo|bar", "^fn x", "x {}$"] {
            let single = formatted_search(&root, &[pattern]);
            assert!(single.contains("search is literal, not regex"), "{single}");
        }
        for pattern in ["missing", "c || d", "v[0]", "a.b"] {
            let single = formatted_search(&root, &[pattern]);
            assert!(!single.contains("not regex"), "{single}");
        }
        // A hit carries no note, and the note names only the absent pattern.
        assert!(!formatted_search(&root, &["a || b"]).contains("not regex"));
        let multiple = formatted_search(&root, &["handle_error", "handle.*error"]);
        assert_eq!(multiple.matches("not regex").count(), 1, "{multiple}");
        assert!(
            multiple.contains("`handle.*error`: not found after full scan [note:"),
            "{multiple}"
        );
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn empty_search_states_absence_only_when_every_file_was_searched() {
        let root = temp_root("absence");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("a.txt"), "nothing here\n").expect("text");
        let clean = formatted_search(&root, &["marker"]);
        assert!(
            clean.starts_with("[no matches after full scan]\n"),
            "{clean}"
        );

        fs::write(root.join("blob.bin"), b"marker\0binary").expect("binary");
        fs::write(root.join("bad.txt"), b"\xff marker\n").expect("invalid utf-8");
        let partial = formatted_search(&root, &["marker"]);
        assert!(
            partial.starts_with(
                "[no matches in searched text (2 file(s) not searched: binary, over 10 MiB or invalid UTF-8)]\n"
            ),
            "{partial}"
        );
        let multiple = formatted_search(&root, &["marker", "nothing"]);
        assert!(!multiple.contains("full scan"), "{multiple}");
        assert!(!multiple.contains("full coverage"), "{multiple}");
        assert!(
            multiple
                .contains("pattern 1 `marker`: not found in searched text (2 file(s) not searched"),
            "{multiple}"
        );
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    /// Links inside the workspace are not searched: a file symlink would
    /// expose a target outside the workspace that `read` rejects.
    #[test]
    fn search_does_not_follow_links_out_of_the_workspace() {
        let base = temp_root("links");
        let root = base.join("workspace");
        let outside = base.join("outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(root.join("inside.txt"), "marker inside\n").expect("inside");
        fs::write(outside.join("secret.txt"), "marker outside\n").expect("secret");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("file-link.txt"))
                .expect("file symlink");
            std::os::unix::fs::symlink(&outside, root.join("dir-link")).expect("dir symlink");
        }
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_file(
                outside.join("secret.txt"),
                root.join("file-link.txt"),
            ) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(1314) => {
                    use std::io::Write as _;
                    let _ = writeln!(
                        io::stderr().lock(),
                        "SKIPPED file symlink case: requires Developer Mode or \
                         SeCreateSymbolicLinkPrivilege ({error})"
                    );
                }
                Err(error) => panic!("file symlink: {error}"),
            }
            // Junctions need no privilege; std reports them as symlinks.
            let junction = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(root.join("dir-link"))
                .arg(&outside)
                .output()
                .expect("mklink");
            assert!(junction.status.success(), "{junction:?}");
        }
        let canonical = root.canonicalize().expect("canonical");
        let page = SearchService::default()
            .page(
                &canonical,
                vec!["marker".into()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: DEFAULT_MAX_HITS,
                    context_lines: 0,
                },
                None,
                None,
            )
            .expect("search");
        let paths = page
            .hits
            .iter()
            .map(|hit| hit.hit.path.display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert!(paths[0].ends_with("inside.txt"), "{paths:?}");
        assert!(!page.io_failures.has_failures(), "{:?}", page.io_failures);
        let _ = fs::remove_dir_all(base);
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
        assert!(formatted.contains("use read/list/shell"));
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
            .page(
                &root,
                patterns.clone(),
                SearchPageOptions {
                    offset: 1,
                    max_hits: 2,
                    context_lines: 0,
                },
                None,
                None,
            )
            .expect("first page");
        assert!(first.bytes_read > 0);
        assert_eq!(first.hits.len(), 2);
        let second = service
            .page(
                &root,
                patterns,
                SearchPageOptions {
                    offset: 3,
                    max_hits: 2,
                    context_lines: 0,
                },
                None,
                None,
            )
            .expect("offset page");
        assert_eq!(second.bytes_read, 0);
        assert_eq!(second.hits.len(), 1);
        assert_eq!(second.total, first.total);
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    fn synthetic_entry(root: &Path, name: &str) -> Result<SearchWalkEntry, PathBuf> {
        Ok(SearchWalkEntry {
            path: root.join(name),
            is_dir: false,
        })
    }

    #[test]
    fn synthetic_walker_open_and_metadata_failures_keep_hits_and_mark_absence_unknown() {
        let root = temp_root("synthetic-walk-open-metadata");
        fs::create_dir_all(&root).unwrap();
        let patterns = vec!["needle".to_owned(), "missing".to_owned()];
        let scan = search_with_entries(
            &root,
            &patterns,
            10,
            0,
            None,
            vec![
                synthetic_entry(&root, "hit.txt"),
                Err(root.join("walk-denied.txt")),
                synthetic_entry(&root, "denied.txt"),
                synthetic_entry(&root, "metadata.txt"),
            ],
            |path, _| match path.file_name().and_then(|name| name.to_str()) {
                Some("denied.txt") => Err(SearchOpenFailure::Open),
                Some("metadata.txt") => Err(SearchOpenFailure::Metadata),
                _ => Ok(SearchOpenedFile {
                    reader: Cursor::new(b"needle retained\n".as_slice()),
                    len: 16,
                }),
            },
        )
        .unwrap();

        assert_eq!(scan.hits.len(), 1);
        assert_eq!(scan.hits[0].hit.text, "needle retained");
        assert!(!scan.scan_complete);
        assert_eq!(
            scan.io_failures.counts[SearchIoFailureKind::Walk.index()],
            1
        );
        assert_eq!(
            scan.io_failures.counts[SearchIoFailureKind::Open.index()],
            1
        );
        assert_eq!(
            scan.io_failures.counts[SearchIoFailureKind::Metadata.index()],
            1
        );
        assert_eq!(
            scan.io_failures.counts[SearchIoFailureKind::Read.index()],
            0
        );
        assert!(scan
            .io_failures
            .examples
            .iter()
            .any(|example| example == "walk-denied.txt"));

        let page = SearchService::default().cache_scan(
            root.clone(),
            patterns,
            SearchPageOptions {
                offset: 1,
                max_hits: 10,
                context_lines: 0,
            },
            scan,
        );
        let output = format_search_batch_page(&page, &root);
        assert!(output.contains("search incomplete"), "{output}");
        assert!(output.contains("not confirmed absent"), "{output}");
        assert!(!output.contains("not found after full scan"), "{output}");
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn single_pattern_with_io_failure_does_not_confirm_zero_hit_absence() {
        let root = temp_root("single-pattern-partial-absence");
        fs::create_dir_all(&root).unwrap();
        let patterns = vec!["needle".to_owned()];
        let scan = search_with_entries(
            &root,
            &patterns,
            10,
            0,
            None,
            vec![synthetic_entry(&root, "unreadable.txt")],
            |_path, _| -> Result<SearchOpenedFile<Cursor<&'static [u8]>>, SearchOpenFailure> {
                Err(SearchOpenFailure::Open)
            },
        )
        .unwrap();
        assert!(scan.hits.is_empty());
        assert!(!scan.scan_complete);

        let page = SearchService::default().cache_scan(
            root.clone(),
            patterns,
            SearchPageOptions {
                offset: 1,
                max_hits: 10,
                context_lines: 0,
            },
            scan,
        );
        let output = format_search_batch_page(&page, &root);
        assert!(output.contains("search incomplete"), "{output}");
        assert!(output.contains("not confirmed absent"), "{output}");
        assert!(!output.contains("not found after full scan"), "{output}");
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    struct FillBufFailure;

    impl Read for FillBufFailure {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "synthetic read failure",
            ))
        }
    }

    impl BufRead for FillBufFailure {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "synthetic fill_buf failure",
            ))
        }

        fn consume(&mut self, _amount: usize) {}
    }

    struct FailAfterBytes {
        bytes: &'static [u8],
        position: usize,
    }

    impl Read for FailAfterBytes {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.position == self.bytes.len() {
                return Err(io::Error::other("synthetic mid-read failure"));
            }
            let count = buffer.len().min(self.bytes.len() - self.position);
            buffer[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        }
    }

    #[test]
    fn fill_buf_and_mid_read_errors_are_partial_and_discard_incomplete_line() {
        let root = temp_root("synthetic-read-errors");
        fs::create_dir_all(&root).unwrap();
        let fill_scan = search_with_entries(
            &root,
            &["needle".to_owned()],
            10,
            0,
            None,
            vec![synthetic_entry(&root, "fill.txt")],
            |_path, _| {
                Ok(SearchOpenedFile {
                    reader: FillBufFailure,
                    len: 8,
                })
            },
        )
        .unwrap();
        assert!(!fill_scan.scan_complete);
        assert_eq!(
            fill_scan.io_failures.counts[SearchIoFailureKind::Read.index()],
            1
        );
        assert!(fill_scan.hits.is_empty());

        let partial = b"needle complete\npartial needle";
        let mid_read_scan = search_with_entries(
            &root,
            &["needle".to_owned()],
            10,
            0,
            None,
            vec![
                synthetic_entry(&root, "partial.txt"),
                synthetic_entry(&root, "later.txt"),
            ],
            |path, _| -> Result<SearchOpenedFile<Box<dyn BufRead>>, SearchOpenFailure> {
                match path.file_name().and_then(|name| name.to_str()) {
                    Some("partial.txt") => Ok(SearchOpenedFile {
                        reader: Box::new(BufReader::new(FailAfterBytes {
                            bytes: partial,
                            position: 0,
                        })),
                        len: partial.len() as u64,
                    }),
                    _ => Ok(SearchOpenedFile {
                        reader: Box::new(Cursor::new(b"needle after error\n".as_slice())),
                        len: 19,
                    }),
                }
            },
        )
        .unwrap();
        assert!(!mid_read_scan.scan_complete);
        assert_eq!(
            mid_read_scan.io_failures.counts[SearchIoFailureKind::Read.index()],
            1
        );
        assert_eq!(mid_read_scan.hits.len(), 2);
        assert_eq!(mid_read_scan.hits[0].hit.text, "needle complete");
        assert_eq!(mid_read_scan.hits[1].hit.text, "needle after error");
        assert!(!mid_read_scan
            .hits
            .iter()
            .any(|hit| hit.hit.text == "partial needle"));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn partial_snapshot_cursor_and_tiny_presentation_keep_incomplete_notice() {
        let root = temp_root("partial-snapshot-presentation");
        fs::create_dir_all(&root).unwrap();
        let patterns = vec!["needle".to_owned(), "missing".to_owned()];
        let scan = search_with_entries(
            &root,
            &patterns,
            10,
            0,
            None,
            vec![
                synthetic_entry(&root, "one.txt"),
                synthetic_entry(&root, "two.txt"),
                synthetic_entry(&root, "unreadable.txt"),
            ],
            |path, _| match path.file_name().and_then(|name| name.to_str()) {
                Some("unreadable.txt") => Err(SearchOpenFailure::Open),
                Some("one.txt") => Ok(SearchOpenedFile {
                    reader: Cursor::new(b"needle first\n".as_slice()),
                    len: 13,
                }),
                _ => Ok(SearchOpenedFile {
                    reader: Cursor::new(b"needle second\n".as_slice()),
                    len: 14,
                }),
            },
        )
        .unwrap();
        let service = SearchService::default();
        let options = SearchPageOptions {
            offset: 1,
            max_hits: 1,
            context_lines: 0,
        };
        let first = service.cache_scan(root.clone(), patterns.clone(), options, scan);
        assert_eq!(first.total, 2);
        assert!(first.next_cursor.is_some());
        assert!(!first.scan_complete);

        let formatted = format_search_batch_page(&first, &root);
        assert!(formatted.contains("search incomplete"), "{formatted}");
        assert!(formatted.contains("not confirmed absent"), "{formatted}");
        assert!(
            !formatted.contains("not found after full scan"),
            "{formatted}"
        );

        let intermediate_budget = format_minimal_search_presentation(&first).len();
        let intermediate = first.present(intermediate_budget, &root);
        assert_eq!(intermediate.delivered_records, 0);
        assert!(intermediate.text.len() <= intermediate_budget);
        assert!(!intermediate.complete);
        assert!(intermediate.oversized_record);
        assert!(intermediate.text.contains("search incomplete"));
        assert!(intermediate.text.contains("pass \"cursor\""));
        assert!(std::str::from_utf8(intermediate.text.as_bytes()).is_ok());

        let tiny = first.present(1, &root);
        assert_eq!(tiny.delivered_records, 0);
        assert!(!tiny.complete);
        assert!(tiny.oversized_record);
        assert!(
            tiny.text.len() > 1,
            "mandatory warning/cursor markers may overrun the budget"
        );
        assert!(std::str::from_utf8(tiny.text.as_bytes()).is_ok());
        assert!(tiny.text.contains("search incomplete"), "{}", tiny.text);
        assert!(tiny.text.contains("pass \"cursor\""), "{}", tiny.text);
        assert!(
            !tiny.text.contains("not found after full scan"),
            "{}",
            tiny.text
        );

        let cursor = first.next_cursor.as_deref().unwrap();
        let second = service
            .page(&root, patterns, options, Some(cursor), None)
            .unwrap();
        assert_eq!(second.first, 2);
        assert_eq!(second.hits[0].hit.text, "needle second");
        assert!(second.next_cursor.is_none());
        let second_text = format_search_batch_page(&second, &root);
        assert!(second_text.contains("search incomplete"), "{second_text}");
        assert!(
            !second_text.contains("not found after full scan"),
            "{second_text}"
        );
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn partial_zero_hit_snapshot_present_keeps_incomplete_notice() {
        let root = temp_root("partial-zero-hit-presentation");
        fs::create_dir_all(&root).unwrap();
        let patterns = vec!["needle".to_owned()];
        let scan = search_with_entries(
            &root,
            &patterns,
            10,
            0,
            None,
            vec![synthetic_entry(&root, "unreadable.txt")],
            |_path, _| -> Result<SearchOpenedFile<Cursor<&'static [u8]>>, SearchOpenFailure> {
                Err(SearchOpenFailure::Open)
            },
        )
        .unwrap();
        let page = SearchService::default().cache_scan(
            root.clone(),
            patterns,
            SearchPageOptions {
                offset: 1,
                max_hits: 10,
                context_lines: 0,
            },
            scan,
        );
        assert_eq!(page.total, 0);
        let presented = page.present(1, &root);
        assert!(!presented.complete);
        assert!(presented.oversized_record);
        assert!(presented.text.contains("search incomplete"));
        assert!(presented.text.contains("not confirmed absent"));
        assert!(!presented.text.contains("not found after full scan"));
        assert!(std::str::from_utf8(presented.text.as_bytes()).is_ok());
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn tool_registry_presents_an_injected_partial_search_snapshot() {
        use crate::tools::{PresentationBudget, ToolRegistry};
        use crate::OperatingMode;

        let root = temp_root("tool-registry-partial-search");
        fs::create_dir_all(&root).unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap();
        let patterns = vec!["needle".to_owned(), "missing".to_owned()];
        let scan = search_with_entries(
            &canonical_root,
            &patterns,
            10,
            0,
            None,
            vec![
                synthetic_entry(&canonical_root, "one.txt"),
                synthetic_entry(&canonical_root, "two.txt"),
                synthetic_entry(&canonical_root, "three.txt"),
                synthetic_entry(&canonical_root, "unreadable.txt"),
            ],
            |path, _| match path.file_name().and_then(|name| name.to_str()) {
                Some("unreadable.txt") => Err(SearchOpenFailure::Open),
                Some("one.txt") => Ok(SearchOpenedFile {
                    reader: Cursor::new(b"needle one\n".as_slice()),
                    len: 11,
                }),
                Some("two.txt") => Ok(SearchOpenedFile {
                    reader: Cursor::new(b"needle two\n".as_slice()),
                    len: 11,
                }),
                _ => Ok(SearchOpenedFile {
                    reader: Cursor::new(b"needle three\n".as_slice()),
                    len: 13,
                }),
            },
        )
        .unwrap();

        let mut registry = ToolRegistry::default();
        let first = Arc::get_mut(&mut registry.services)
            .expect("registry has a unique service bundle")
            .search
            .cache_scan(
                canonical_root,
                patterns.clone(),
                SearchPageOptions {
                    offset: 1,
                    max_hits: 1,
                    context_lines: 0,
                },
                scan,
            );
        let cursor = first.next_cursor.expect("first page has continuation");
        let arguments = serde_json::json!({
            "path": ".",
            "patterns": patterns,
            "max_hits": 1,
            "cursor": cursor,
        })
        .to_string();
        let prepared =
            registry.prepare_invocation(OperatingMode::ReadOnly, &root, "search", &arguments);
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        assert!(outcome.result.success, "{}", outcome.result.output);
        assert!(
            outcome.result.output.contains("search incomplete"),
            "{}",
            outcome.result.output
        );
        assert!(
            outcome.result.output.contains("not confirmed absent"),
            "{}",
            outcome.result.output
        );

        let presentation = outcome
            .receipt
            .presentation
            .as_ref()
            .expect("search provides its structured presentation")
            .present(PresentationBudget { max_bytes: 1 });
        assert_eq!(presentation.delivered_records, 0);
        assert!(presentation.oversized_record);
        assert!(presentation.text.contains("search incomplete"));
        assert!(presentation.text.contains("pass \"cursor\""));
        assert!(!presentation.text.contains("not found after full scan"));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn work_limit_hit_cap_and_io_failure_are_reported_together() {
        let root = temp_root("combined-search-limits");
        fs::create_dir_all(&root).unwrap();
        let entries = (0..=MAX_SEARCH_SCAN_FILES)
            .map(|index| synthetic_entry(&root, &format!("file-{index:04}.txt")))
            .collect::<Vec<_>>();
        let scan = search_with_entries(
            &root,
            &["common".to_owned(), "rare".to_owned()],
            1,
            0,
            None,
            entries,
            |path, _| match path.file_name().and_then(|name| name.to_str()) {
                Some("file-0000.txt" | "file-0001.txt") => Ok(SearchOpenedFile {
                    reader: Cursor::new(b"common\n".to_vec()),
                    len: 7,
                }),
                Some("file-0002.txt") => Err(SearchOpenFailure::Open),
                _ => Ok(SearchOpenedFile {
                    reader: Cursor::new(Vec::<u8>::new()),
                    len: 0,
                }),
            },
        )
        .unwrap();

        assert!(scan.capped);
        assert!(scan.work_limited);
        assert!(!scan.scan_complete);
        assert_eq!(scan.hits.len(), 1);
        assert_eq!(scan.coverage[0].omitted, 1);
        assert_eq!(
            scan.io_failures.counts[SearchIoFailureKind::Open.index()],
            1
        );

        let page = SearchService::default().cache_scan(
            root.clone(),
            vec!["common".to_owned(), "rare".to_owned()],
            SearchPageOptions {
                offset: 1,
                max_hits: 1,
                context_lines: 0,
            },
            scan,
        );
        let output = format_search_batch_page(&page, &root);
        assert!(output.contains("work budget exhausted"), "{output}");
        assert!(output.contains("I/O failure"), "{output}");
        assert!(output.contains("1 additional matches omitted"), "{output}");
        assert_eq!(
            page.total, 1,
            "I/O failures must not rewrite the retained hit count"
        );
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn io_failure_counts_saturate_and_example_paths_stay_bounded() {
        let root = temp_root("bounded-io-summary");
        let mut summary = SearchIoSummary::default();
        let long_path = root.join(format!("{}\nfile.txt", "界".repeat(100)));
        for index in 0..(usize::from(u16::MAX) + 10) {
            let path = if index == 0 {
                long_path.clone()
            } else {
                root.join(format!("failure-{index}.txt"))
            };
            summary.record(SearchIoFailureKind::Open, &root, &path);
        }

        assert_eq!(summary.counts[SearchIoFailureKind::Open.index()], u16::MAX);
        assert_eq!(summary.total(), u32::from(u16::MAX));
        assert_eq!(summary.examples.len(), MAX_SEARCH_IO_EXAMPLES);
        assert!(summary
            .examples
            .iter()
            .all(|example| example.len() <= MAX_SEARCH_IO_EXAMPLE_BYTES));
        assert!(!summary.examples[0].contains('\n'));
        assert!(summary.category_counts().contains("open 65535"));
    }

    #[test]
    fn cancelled_fresh_scan_still_returns_cancelled() {
        let root = temp_root("cancelled-fresh-search");
        fs::create_dir_all(&root).unwrap();
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let error = SearchService::default()
            .page(
                &root,
                vec!["needle".to_owned()],
                SearchPageOptions {
                    offset: 1,
                    max_hits: 10,
                    context_lines: 0,
                },
                None,
                Some(&cancellation),
            )
            .unwrap_err();

        assert!(matches!(error.error, ToolError::Cancelled));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn ignore_walk_adapter_preserves_nested_error_path_in_relative_sample() {
        let root = temp_root("nested-walker-error-path");
        fs::create_dir_all(&root).unwrap();
        let denied_path = root.join("nested").join("blocked.txt");
        let error = ignore::Error::WithDepth {
            depth: 2,
            err: Box::new(ignore::Error::WithPath {
                path: denied_path,
                err: Box::new(ignore::Error::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "synthetic walker failure",
                ))),
            }),
        };
        let walker_event = map_search_walk_entry(Err(error), &root);
        let scan = search_with_entries(
            &root,
            &["needle".to_owned()],
            10,
            0,
            None,
            vec![walker_event],
            |_path, _| -> Result<SearchOpenedFile<Cursor<&'static [u8]>>, SearchOpenFailure> {
                unreachable!("walker failure does not open a file")
            },
        )
        .unwrap();

        assert!(!scan.scan_complete);
        assert_eq!(
            scan.io_failures.counts[SearchIoFailureKind::Walk.index()],
            1
        );
        let expected_example = Path::new("nested")
            .join("blocked.txt")
            .to_string_lossy()
            .into_owned();
        assert!(scan
            .io_failures
            .examples
            .iter()
            .any(|example| example == &expected_example));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn a_pattern_with_a_line_break_is_rejected_instead_of_never_matching() {
        for pattern in ["one\ntwo", "one\r\ntwo", "one\rtwo"] {
            let error = validate_patterns(&[pattern.to_owned()]).unwrap_err();
            assert!(
                matches!(&error, ToolError::InvalidInput { message }
                    if message.contains("single lines") && message.contains("one line")),
                "{error:?}"
            );
        }
        assert!(validate_patterns(&["one two".to_owned()]).is_ok());
    }

    #[test]
    fn an_explicit_file_root_is_searched_even_when_directory_scans_skip_its_kind() {
        let root = temp_root("explicit-root-file");
        fs::create_dir_all(&root).expect("root");
        for name in ["Cargo.lock", "app.js.map", "app.min.js"] {
            fs::write(root.join(name), "needle here\n").expect("fixture");
        }
        let directory = formatted_search(&root, &["needle"]);
        assert!(
            directory.contains("no matches after full scan"),
            "{directory}"
        );
        for name in ["Cargo.lock", "app.js.map", "app.min.js"] {
            let file = formatted_search(&root.join(name), &["needle"]);
            assert!(file.contains("1: needle here"), "{name}: {file}");
        }
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn the_skip_footer_is_only_on_results_without_hits() {
        let root = temp_root("skip-footer");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("a.txt"), "needle\n").expect("fixture");
        let hit = formatted_search(&root, &["needle"]);
        assert!(!hit.contains("[skipped:"), "{hit}");
        let miss = formatted_search(&root, &["absent"]);
        assert!(miss.contains(".gitignore'd files"), "{miss}");
        assert!(miss.contains("*.lock/*.map/*.min.js"), "{miss}");
        let page = SearchPage {
            hits: vec![SearchHit {
                path: root.join("a.txt"),
                line: 1,
                text: "needle".into(),
            }],
            total_seen: 1,
            truncated: false,
        };
        assert!(!format_search_page(&page, &root).contains("[skipped:"));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    /// The footer names `.gitignore` only inside a git repository because that
    /// is the only place the walker honors it.
    #[test]
    fn the_footer_claims_gitignore_only_where_the_walker_honors_it() {
        let root = temp_root("footer-gitignore");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join(".gitignore"), "ignored.txt\n").expect("gitignore");
        fs::write(root.join("ignored.txt"), "needle\n").expect("ignored");
        fs::write(root.join("hidden-by-ignore.txt"), "needle\n").expect("ignore-file");
        fs::write(root.join(".ignore"), "hidden-by-ignore.txt\n").expect("ignore");
        // Not a repository: `.gitignore` is not honored, `.ignore` is.
        let outside = formatted_search(&root, &["needle"]);
        assert!(outside.contains("ignored.txt"), "{outside}");
        assert!(!outside.contains("hidden-by-ignore.txt"), "{outside}");
        fs::create_dir_all(root.join(".git")).expect("git");
        let inside = formatted_search(&root, &["needle"]);
        assert!(!inside.contains("ignored.txt"), "{inside}");
        let miss = formatted_search(&root, &["absent"]);
        assert!(
            miss.contains(".ignore'd files, plus .gitignore'd files inside a git repository"),
            "{miss}"
        );
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn a_miss_names_a_spelling_that_differs_only_in_case() {
        let root = temp_root("case-variant");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("a.rs"), "// intro\nstruct SearchService;\nação\n").expect("fixture");
        let single = formatted_search(&root, &["searchservice"]);
        assert!(
            single.starts_with("[no matches after full scan]"),
            "{single}"
        );
        assert!(
            single.contains("`SearchService` occurs at a.rs:2"),
            "{single}"
        );
        let unicode = formatted_search(&root, &["AÇÃO"]);
        assert!(unicode.contains("`ação` occurs at a.rs:3"), "{unicode}");
        // Per absent pattern, and nothing for a pattern that is truly absent
        // or that has hits.
        let multiple = formatted_search(&root, &["SEARCHSERVICE", "intro", "nothing"]);
        assert_eq!(multiple.matches("case-sensitive").count(), 1, "{multiple}");
        assert!(
            multiple.contains("pattern 1 `SEARCHSERVICE`: not found after full scan [note:"),
            "{multiple}"
        );
        assert!(!formatted_search(&root, &["nothing"]).contains("case-sensitive"));
        assert!(!formatted_search(&root, &["12345"]).contains("case-sensitive"));
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn the_case_recheck_adds_nothing_when_the_scan_was_not_complete() {
        let root = temp_root("case-variant-incomplete");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("a.txt"), "Needle\n").expect("fixture");
        let patterns = vec!["needle".to_owned()];
        let mut scan = search_with_walker(&root, &patterns, 10, 0, None).expect("scan");
        scan.work_limited = true;
        note_case_variants(&root, &patterns, &mut scan, None).expect("recheck");
        assert!(scan.coverage[0].case_variant.is_none());
        scan.work_limited = false;
        note_case_variants(&root, &patterns, &mut scan, None).expect("recheck");
        assert!(scan.coverage[0].case_variant.is_some());
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn the_case_recheck_is_bounded_by_a_wall_clock_budget_and_by_cancellation() {
        let root = temp_root("case-variant-budget");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("a.txt"), "Needle\n").expect("fixture");
        let patterns = vec!["needle".to_owned()];
        let run = |budget, cancellation: Option<&CancellationToken>| {
            find_case_variants(&root, &patterns, &[0], cancellation, budget)
        };
        assert_eq!(run(Duration::from_secs(10), None).unwrap().len(), 1);
        // Out of time: nothing, not a partial answer.
        assert!(run(Duration::ZERO, None).unwrap().is_empty());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            run(Duration::from_secs(10), Some(&cancelled)),
            Err(ToolExecutionError {
                error: ToolError::Cancelled,
                ..
            })
        ));
        // Repetitive lines that nearly match cost about one pass, not a
        // quadratic one.
        fs::write(
            root.join("a.txt"),
            format!("{}\n", "aaaaaaaaab".repeat(200_000)),
        )
        .expect("long");
        let nearly = vec!["aaaaaaaaac".to_owned()];
        let started = Instant::now();
        let none = find_case_variants(&root, &nearly, &[0], None, Duration::from_secs(30)).unwrap();
        assert!(none.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn ascii_and_unicode_case_matching_agree_and_return_the_spelling_found() {
        for (line, pattern, expected) in [
            (
                "let SearchService = 1;",
                "searchservice",
                Some("SearchService"),
            ),
            ("xxNEEDLE", "needle", Some("NEEDLE")),
            ("needl", "needle", None),
            ("ação AÇÃO", "açÃo", Some("ação")),
            ("é ÉÉ", "éé", Some("ÉÉ")),
            ("aaab", "aaac", None),
        ] {
            let needle = CaseNeedle::new(0, pattern);
            assert_eq!(needle.find_in(line), expected, "{line} / {pattern}");
        }
    }

    #[test]
    fn a_hit_on_a_very_long_line_is_windowed_around_the_match() {
        let root = temp_root("long-line-window");
        fs::create_dir_all(&root).expect("root");
        let line = format!("{}needle{}", "a".repeat(20_000), "b".repeat(20_000));
        fs::write(root.join("min.txt"), format!("{line}\nshort needle\n")).expect("fixture");
        let output = formatted_search(&root, &["needle"]);
        assert!(output.contains(&format!("{}needle{}", "a".repeat(256), "b".repeat(256))));
        assert!(!output.contains(&"a".repeat(257)), "window must stay small");
        assert!(output.contains("[truncated 19744 bytes] "), "{output}");
        assert!(output.contains("\n[truncated 19744 bytes]\n"), "{output}");
        assert!(output.contains("2: short needle"), "{output}");
        assert!(output.len() < 2048, "{}", output.len());
        // A line under the limit is never windowed.
        let patterns = vec!["needle".to_owned()];
        let short = "x".repeat(MAX_HIT_TEXT_BYTES - 6) + "needle";
        assert_eq!(bound_hit_text(&short, &patterns, &[0]), short);
        let _ = fs::remove_dir_all(root.parent().expect("parent"));
    }
}
