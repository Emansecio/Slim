use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::{
    check_cancellation, lock_mutex, DependencyKind, DependencyObservation, FastStamp, ToolError,
    ToolExecutionError,
};
use crate::runtime::CancellationToken;

pub const DEFAULT_MAX_READ_LINES: usize = 200;
pub const MAX_READ_LINES_CAP: usize = 4096;
const SMART_READ_OUTLINE_BYTES: u64 = 32 * 1024;
const SMART_READ_OUTLINE_LINES: usize = 250;
const SMART_READ_PREVIEW_LINES: usize = 120;
const DELTA_READ_MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_DELIVERED_SNAPSHOTS: usize = 128;
const MAX_READ_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_READ_PAGE_BYTES: usize = 1024 * 1024;
pub(super) const DIRECTORY_PATH_MESSAGE: &str = "path is a directory; use list to see its entries";
const CHECKPOINT_INTERVAL_LINES: usize = 256;
const MAX_CHECKPOINTS_PER_FILE: usize = 4096;
/// One slot per recently touched file. Stays above the per-turn read cap (96)
/// so a full read batch does not evict a complete_digest before the following
/// write; the headroom covers scan-heavy sessions touching many more files.
const MAX_INDEXED_FILES: usize = 512;

#[derive(Clone, Debug, Default)]
pub(crate) struct ReadService {
    cache: Arc<Mutex<ReadCache>>,
}

#[derive(Debug)]
pub(crate) struct ReadPage {
    pub(crate) output: String,
    pub(crate) first_line: usize,
    pub(crate) records: Vec<String>,
    pub(crate) next_offset: Option<usize>,
    pub(crate) dependency: DependencyObservation,
    pub(crate) bytes_read: u64,
}

#[derive(Debug, Default)]
struct ReadCache {
    entries: HashMap<PathBuf, ReadIndex>,
    delivered: HashMap<PathBuf, DeliveredSnapshot>,
    clock: u64,
}

#[derive(Clone, Debug)]
struct DeliveredSnapshot {
    version: FileVersion,
    first_line: usize,
    lines: Vec<String>,
    last_used: u64,
}

#[derive(Clone, Debug)]
struct ReadIndex {
    version: FileVersion,
    checkpoints: Vec<Checkpoint>,
    last_used: u64,
    complete_digest: Option<[u8; 32]>,
    read_progress: Option<ReadProgress>,
    cached_digest_progress: Option<usize>,
}

#[derive(Clone, Debug)]
struct ReadProgress {
    next_line: usize,
    hasher: Sha256,
}

#[derive(Clone, Copy, Debug)]
struct Checkpoint {
    line: usize,
    offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileVersion {
    len: u64,
    modified: Option<SystemTime>,
    file_id: Option<String>,
}

/// Lines a model-facing read starting at `first_line` returns at most, for a
/// file of `len` bytes: an omitted limit widens the first page only for small
/// files (`<= SMART_READ_OUTLINE_BYTES`) so large files default to bounded reads.
pub(crate) fn page_lines(first_line: usize, max_lines: Option<usize>, len: u64) -> usize {
    if first_line <= 1 && max_lines.is_none() && len <= SMART_READ_OUTLINE_BYTES {
        MAX_READ_LINES_CAP
    } else {
        max_lines.unwrap_or(DEFAULT_MAX_READ_LINES)
    }
}

pub fn read_file(path: impl AsRef<Path>, max_lines: usize) -> Result<String, ToolError> {
    read_file_range(path, 1, max_lines)
}

/// Reads a numbered page and stops after one lookahead line. Exact total line
/// counts are intentionally not computed on the hot path.
pub fn read_file_range(
    path: impl AsRef<Path>,
    start_line: usize,
    max_lines: usize,
) -> Result<String, ToolError> {
    ReadService::default()
        .read_file_range(path.as_ref(), start_line, max_lines, None)
        .map(|page| page.output)
        .map_err(|failure| failure.error)
}

impl ReadService {
    pub(crate) fn read_file_range(
        &self,
        path: &Path,
        start_line: usize,
        max_lines: usize,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ReadPage, ToolExecutionError> {
        let canonical = path
            .canonicalize()
            .map_err(|error| read_path_error(path, error))?;
        self.read_file_range_resolved(&canonical, start_line, Some(max_lines), true, cancellation)
    }

    /// `max_lines: None` is the omitted-limit form: its default first page may
    /// widen to a full-file read. `Some` is an explicit limit, always literal.
    pub(crate) fn read_file_range_resolved(
        &self,
        canonical: &Path,
        start_line: usize,
        max_lines: Option<usize>,
        numbered: bool,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ReadPage, ToolExecutionError> {
        self.read_file_range_inner(canonical, start_line, max_lines, numbered, true, cancellation)
    }

    /// Reads a line range without updating `complete_digest` (used by `ranges`).
    pub(crate) fn read_file_range_no_digest(
        &self,
        canonical: &Path,
        start_line: usize,
        max_lines: usize,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ReadPage, ToolExecutionError> {
        self.read_file_range_inner(
            canonical,
            start_line,
            Some(max_lines),
            false,
            false,
            cancellation,
        )
    }

    /// Reads an entire UTF-8 file up to `MAX_READ_FILE_BYTES` for structural outline
    /// or symbol resolution without setting `complete_digest`.
    pub(crate) fn read_full_utf8_for_outline(
        &self,
        canonical: &Path,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(String, DependencyObservation, u64), ToolExecutionError> {
        ensure_not_special_file(canonical)?;
        if let Some(cached) = try_read_via_slab(canonical) {
            check_cancellation(cancellation)?;
            let dependency = dependency_from_cached(canonical, &cached);
            let bytes = cached.version.len;
            return Ok(((*cached.text).to_owned(), dependency, bytes));
        }
        let file =
            std::fs::File::open(canonical).map_err(|error| read_path_error(canonical, error))?;
        super::execution::verify_opened_path(&file, canonical)?;
        let metadata = file.metadata()?;
        if metadata.is_dir() {
            return Err(directory_read_error());
        }
        let stamp = FastStamp::from_file(DependencyKind::File, &file, &metadata, None);
        let dependency = DependencyObservation {
            path: canonical.to_path_buf(),
            stamp,
        };
        if metadata.len() > MAX_READ_FILE_BYTES {
            return Err(with_context(
                observed_read_error(
                    ToolError::InvalidInput {
                        message: format!(
                            "read file exceeds the {MAX_READ_FILE_BYTES}-byte safety limit"
                        ),
                    },
                    &dependency,
                    0,
                ),
                slice_hint(canonical),
            ));
        }
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut bytes_read = 0u64;
        let mut line_no = 1usize;
        let mut content = String::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        while let Some(chunk) = read_utf8_line(
            &mut reader,
            &mut line,
            &dependency,
            &mut bytes_read,
            line_no,
        )? {
            check_cancellation(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            content.push_str(chunk);
            line_no = line_no.saturating_add(1);
        }
        Ok((content, dependency, bytes_read))
    }

    fn read_file_range_inner(
        &self,
        canonical: &Path,
        start_line: usize,
        max_lines: Option<usize>,
        numbered: bool,
        record_digest: bool,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ReadPage, ToolExecutionError> {
        ensure_not_special_file(canonical)?;
        if let Some(cached) = try_read_via_slab(canonical) {
            return self.read_file_range_from_cached(
                canonical,
                &cached,
                start_line,
                max_lines,
                numbered,
                record_digest,
                cancellation,
            );
        }
        let file =
            std::fs::File::open(canonical).map_err(|error| read_path_error(canonical, error))?;
        super::execution::verify_opened_path(&file, canonical)?;
        let metadata = file.metadata()?;
        // Unix opens directories; Windows fails in `read_path_error`.
        if metadata.is_dir() {
            return Err(directory_read_error());
        }
        let stamp = FastStamp::from_file(DependencyKind::File, &file, &metadata, None);
        let version = FileVersion {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            file_id: stamp.file_id.clone(),
        };
        let dependency = DependencyObservation {
            path: canonical.to_path_buf(),
            stamp,
        };
        if metadata.len() > MAX_READ_FILE_BYTES {
            // search skips files over this size too, so it cannot help here.
            return Err(with_context(
                observed_read_error(
                    ToolError::InvalidInput {
                        message: format!(
                            "read file exceeds the {MAX_READ_FILE_BYTES}-byte safety limit"
                        ),
                    },
                    &dependency,
                    0,
                ),
                slice_hint(canonical),
            ));
        }
        let requested_first = start_line.max(1);
        // Default first page of a file that fits the page/line caps returns
        // the whole file so one read records complete_digest. Explicit
        // max_lines and numbered public pages keep their requested window.
        let max_lines = if numbered {
            max_lines.unwrap_or(DEFAULT_MAX_READ_LINES)
        } else {
            page_lines(requested_first, max_lines, metadata.len())
        };
        let index = self.index_for(canonical, version.clone());
        let checkpoint = index
            .checkpoints
            .iter()
            .rev()
            .find(|checkpoint| checkpoint.line <= requested_first)
            .copied()
            .unwrap_or(Checkpoint { line: 1, offset: 0 });

        let mut reader = BufReader::new(file);
        reader
            .seek(SeekFrom::Start(checkpoint.offset))
            .map_err(|error| observed_read_error(error.into(), &dependency, 0))?;
        let mut current_line = checkpoint.line;
        let mut discovered = Vec::new();
        let mut line = Vec::new();
        let mut bytes_read = 0u64;

        while current_line < requested_first {
            check_cancellation(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            if read_utf8_line(
                &mut reader,
                &mut line,
                &dependency,
                &mut bytes_read,
                current_line,
            )?
            .is_none()
            {
                self.merge_checkpoints(canonical, version, discovered);
                return Ok(ReadPage {
                    output: if numbered {
                        String::new()
                    } else {
                        empty_page_note(metadata.len(), requested_first, current_line - 1)
                    },
                    first_line: requested_first,
                    records: Vec::new(),
                    next_offset: None,
                    dependency,
                    bytes_read,
                });
            }
            current_line = current_line.saturating_add(1);
            record_checkpoint(
                &mut discovered,
                current_line,
                checkpoint.offset.saturating_add(bytes_read),
            );
        }

        let mut output = String::new();
        let mut records = Vec::new();
        let mut returned = 0usize;
        let mut content_hasher = if !record_digest {
            None
        } else if requested_first == 1 {
            Some(Sha256::new())
        } else {
            index
                .read_progress
                .as_ref()
                .filter(|progress| progress.next_line == requested_first)
                .map(|progress| progress.hasher.clone())
        };
        while returned < max_lines {
            check_cancellation(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            let Some(line) = read_utf8_line(
                &mut reader,
                &mut line,
                &dependency,
                &mut bytes_read,
                current_line,
            )?
            else {
                break;
            };
            if let Some(hasher) = content_hasher.as_mut() {
                hasher.update(line.as_bytes());
            }
            let rendered_bytes = if numbered {
                decimal_digits(current_line)
                    .saturating_add(3)
                    .saturating_add(strip_line_ending(line).len())
            } else {
                line.len()
            };
            if output.len().saturating_add(rendered_bytes) > MAX_READ_PAGE_BYTES {
                return Err(page_too_large(
                    &dependency,
                    bytes_read,
                    if returned == 0 {
                        long_line_hint(canonical, current_line)
                    } else {
                        SMALLER_PAGE_HINT.into()
                    },
                ));
            }
            if numbered {
                let record = format!("{current_line}: {}", strip_line_ending(line));
                records.push(record.clone());
                writeln!(&mut output, "{record}").expect("writing to String cannot fail");
            } else {
                // Keep the exact chunk (including its line ending) so a later
                // model-facing projection cannot rewrite CRLF or the final
                // newline while selecting complete records.
                records.push(line.to_owned());
                output.push_str(line);
            }
            returned = returned.saturating_add(1);
            current_line = current_line.saturating_add(1);
            record_checkpoint(
                &mut discovered,
                current_line,
                checkpoint.offset.saturating_add(bytes_read),
            );
        }

        let has_more = if returned == max_lines && returned > 0 {
            check_cancellation(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            // Capture the next page's start before lookahead consumes its first line.
            let next_page = Checkpoint {
                line: current_line,
                offset: checkpoint.offset.saturating_add(bytes_read),
            };
            // Only the presence of a next line matters here; its encoding is
            // checked when that page is read, so it cannot fail this one.
            line.clear();
            let next_read = reader.read_until(b'\n', &mut line).map_err(|error| {
                bytes_read =
                    bytes_read.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
                observed_read_error(error.into(), &dependency, bytes_read)
            })?;
            bytes_read = bytes_read.saturating_add(u64::try_from(next_read).unwrap_or(u64::MAX));
            let more = next_read > 0;
            if more {
                discovered.push(next_page);
                let following_line = current_line.saturating_add(1);
                record_checkpoint(
                    &mut discovered,
                    following_line,
                    checkpoint.offset.saturating_add(bytes_read),
                );
            }
            more
        } else {
            false
        };
        if has_more && record_digest {
            let first = requested_first;
            let last = current_line.saturating_sub(1);
            let footer = format!(
                "\n[showing lines {first}-{last}; more content available; pass \"offset\": {} for the next page]",
                last.saturating_add(1)
            );
            if output.len().saturating_add(footer.len()) > MAX_READ_PAGE_BYTES {
                return Err(page_too_large(
                    &dependency,
                    bytes_read,
                    SMALLER_PAGE_HINT.into(),
                ));
            }
            output.push_str(&footer);
        }
        if let Some(hasher) = content_hasher {
            let mut cache = lock_mutex(&self.cache);
            if let Some(index) = cache.entries.get_mut(canonical) {
                if index.version == version {
                    if has_more {
                        index.read_progress = Some(ReadProgress {
                            next_line: current_line,
                            hasher,
                        });
                    } else {
                        index.complete_digest = Some(hasher.finalize().into());
                        index.read_progress = None;
                    }
                }
            }
        }

        self.merge_checkpoints(canonical, version, discovered);
        if returned == 0 && !numbered {
            output = empty_page_note(metadata.len(), requested_first, current_line - 1);
        }
        Ok(ReadPage {
            output,
            first_line: requested_first,
            records,
            next_offset: has_more.then_some(current_line),
            dependency,
            bytes_read,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn read_file_range_from_cached(
        &self,
        canonical: &Path,
        cached: &crate::project_index::CachedFile,
        start_line: usize,
        max_lines: Option<usize>,
        numbered: bool,
        record_digest: bool,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ReadPage, ToolExecutionError> {
        let dependency = dependency_from_cached(canonical, cached);
        let version = FileVersion {
            len: cached.version.len,
            modified: cached.version.modified,
            file_id: dependency.stamp.file_id.clone(),
        };
        let requested_first = start_line.max(1);
        let max_lines = if numbered {
            max_lines.unwrap_or(DEFAULT_MAX_READ_LINES)
        } else {
            page_lines(requested_first, max_lines, cached.version.len)
        };
        let total_lines = cached.line_count();
        let _ = self.index_for(canonical, version.clone());

        if requested_first > total_lines {
            return Ok(ReadPage {
                output: if numbered {
                    String::new()
                } else {
                    empty_page_note(cached.version.len, requested_first, total_lines)
                },
                first_line: requested_first,
                records: Vec::new(),
                next_offset: None,
                dependency,
                bytes_read: 0,
            });
        }

        let mut output = String::new();
        let mut records = Vec::new();
        let mut returned = 0usize;
        let mut bytes_read = 0u64;
        let mut current_line = requested_first;

        while returned < max_lines && current_line <= total_lines {
            check_cancellation(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            let line = cached.line_slice(current_line, current_line);
            bytes_read = bytes_read.saturating_add(line.len() as u64);
            let rendered_bytes = if numbered {
                decimal_digits(current_line)
                    .saturating_add(3)
                    .saturating_add(strip_line_ending(line).len())
            } else {
                line.len()
            };
            if output.len().saturating_add(rendered_bytes) > MAX_READ_PAGE_BYTES {
                return Err(page_too_large(
                    &dependency,
                    bytes_read,
                    if returned == 0 {
                        long_line_hint(canonical, current_line)
                    } else {
                        SMALLER_PAGE_HINT.into()
                    },
                ));
            }
            if numbered {
                let record = format!("{current_line}: {}", strip_line_ending(line));
                records.push(record.clone());
                writeln!(&mut output, "{record}").expect("writing to String cannot fail");
            } else {
                records.push(line.to_owned());
                output.push_str(line);
            }
            returned = returned.saturating_add(1);
            current_line = current_line.saturating_add(1);
        }

        let has_more = returned == max_lines && returned > 0 && current_line <= total_lines;
        if has_more && record_digest {
            let first = requested_first;
            let last = current_line.saturating_sub(1);
            let footer = format!(
                "\n[showing lines {first}-{last}; more content available; pass \"offset\": {} for the next page]",
                last.saturating_add(1)
            );
            if output.len().saturating_add(footer.len()) > MAX_READ_PAGE_BYTES {
                return Err(page_too_large(
                    &dependency,
                    bytes_read,
                    SMALLER_PAGE_HINT.into(),
                ));
            }
            output.push_str(&footer);
        }
        if record_digest {
            let mut cache = lock_mutex(&self.cache);
            if let Some(index) = cache.entries.get_mut(canonical) {
                if index.version == version {
                    let continuous = requested_first == 1
                        || index.cached_digest_progress == Some(requested_first);
                    if continuous {
                        if has_more {
                            index.cached_digest_progress = Some(current_line);
                        } else {
                            index.complete_digest = Some(cached.sha256);
                            index.cached_digest_progress = None;
                            index.read_progress = None;
                        }
                    }
                }
            }
        }
        Ok(ReadPage {
            output,
            first_line: requested_first,
            records,
            next_offset: has_more.then_some(current_line),
            dependency,
            bytes_read,
        })
    }

    pub(crate) fn invalidate(&self, path: &Path) {
        let mut cache = lock_mutex(&self.cache);
        for key in cache_lookup_paths(path) {
            cache.entries.remove(&key);
        }
    }

    /// Returns a compact structural outline + top preview lines when `read` is
    /// called unbounded on a large file with supported language symbols. Returns
    /// `Ok(None)` when the file is small or lacks structural symbols so standard
    /// pagination applies.
    pub(crate) fn smart_unbounded_overview(
        &self,
        canonical: &Path,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<(String, DependencyObservation, u64)>, ToolExecutionError> {
        let version = file_version_best_effort(canonical);
        if version.len <= SMART_READ_OUTLINE_BYTES {
            return Ok(None);
        }
        let (content, dependency, bytes_read) =
            self.read_full_utf8_for_outline(canonical, cancellation)?;
        let syms = super::outline::extract_outline(canonical, &content);
        if syms.is_empty() {
            return Ok(None);
        }
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let total_lines = lines.len();
        if total_lines <= SMART_READ_OUTLINE_LINES {
            return Ok(None);
        }
        let preview_end = SMART_READ_PREVIEW_LINES.min(total_lines);
        let preview_records: Vec<String> = lines[..preview_end]
            .iter()
            .map(|l| (*l).to_owned())
            .collect();
        self.record_delivered_snapshot(canonical, version, 1, &preview_records);
        let outline_text = super::outline::format_outline(&syms, total_lines);
        let preview_body = preview_records.concat();
        let out = format!(
            "[smart read: {total_lines} lines; structural outline + lines 1-{preview_end} (pass symbols:[...], ranges:[[start,end]], max_lines/offset, or raw:true for full text)]\n{outline_text}\n\n[lines 1-{preview_end}]\n{preview_body}"
        );
        Ok(Some((out, dependency, bytes_read)))
    }

    /// Records the delivered slice for `canonical` and returns a compact diff
    /// against the previous delivered read when the file changed on disk,
    /// `raw == false`, and the diff saves at least 60% of the response size.
    pub(crate) fn maybe_compact_delta(
        &self,
        canonical: &Path,
        first_line: usize,
        records: &[String],
        full_output_bytes: usize,
        raw: bool,
    ) -> Option<String> {
        let version = file_version_best_effort(canonical);
        if version.len > DELTA_READ_MAX_FILE_BYTES || records.is_empty() {
            return None;
        }
        let delta = if !raw {
            let cache = lock_mutex(&self.cache);
            cache.delivered.get(canonical).and_then(|prev| {
                if prev.version == version || prev.first_line != first_line {
                    return None;
                }
                render_compact_delta(first_line, &prev.lines, records, full_output_bytes)
            })
        } else {
            None
        };
        self.record_delivered_snapshot(canonical, version, first_line, records);
        delta
    }

    fn record_delivered_snapshot(
        &self,
        canonical: &Path,
        version: FileVersion,
        first_line: usize,
        lines: &[String],
    ) {
        if version.len > DELTA_READ_MAX_FILE_BYTES || lines.is_empty() {
            return;
        }
        let mut cache = lock_mutex(&self.cache);
        cache.clock = cache.clock.wrapping_add(1);
        let now = cache.clock;
        cache.delivered.insert(
            canonical.to_path_buf(),
            DeliveredSnapshot {
                version,
                first_line,
                lines: lines.to_vec(),
                last_used: now,
            },
        );
        while cache.delivered.len() > MAX_DELIVERED_SNAPSHOTS {
            let Some(oldest) = cache
                .delivered
                .iter()
                .min_by_key(|(_, snap)| snap.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            cache.delivered.remove(&oldest);
        }
    }

    /// A complete successful read, or a successful overwrite/patch of this
    /// path, authorizes an implicit overwrite. Create and failed writes do
    /// not. The writer rechecks this digest against the locked current file.
    pub(crate) fn complete_digest(&self, path: &Path) -> Option<[u8; 32]> {
        let cache = lock_mutex(&self.cache);
        cache_lookup_paths(path).into_iter().find_map(|key| {
            cache
                .entries
                .get(&key)
                .and_then(|index| index.complete_digest)
        })
    }

    /// Replace paging state with the bytes just written so the next overwrite
    /// can omit expected without a confirmation read.
    pub(crate) fn remember_complete_digest(&self, path: &Path, digest: [u8; 32]) {
        let version = file_version_best_effort(path);
        let mut cache = lock_mutex(&self.cache);
        cache.clock = cache.clock.wrapping_add(1);
        let index = ReadIndex {
            version,
            checkpoints: vec![Checkpoint { line: 1, offset: 0 }],
            last_used: cache.clock,
            complete_digest: Some(digest),
            read_progress: None,
            cached_digest_progress: None,
        };
        for key in cache_lookup_paths(path) {
            cache.entries.insert(key, index.clone());
        }
        evict_old_indexes(&mut cache);
    }

    fn index_for(&self, path: &Path, version: FileVersion) -> ReadIndex {
        let mut cache = lock_mutex(&self.cache);
        cache.clock = cache.clock.wrapping_add(1);
        let now = cache.clock;
        if let Some(index) = cache.entries.get_mut(path) {
            if index.version == version {
                index.last_used = now;
                return index.clone();
            }
        }
        let index = ReadIndex {
            version,
            checkpoints: vec![Checkpoint { line: 1, offset: 0 }],
            last_used: now,
            complete_digest: None,
            read_progress: None,
            cached_digest_progress: None,
        };
        cache.entries.insert(path.to_path_buf(), index.clone());
        evict_old_indexes(&mut cache);
        index
    }

    fn merge_checkpoints(&self, path: &Path, version: FileVersion, discovered: Vec<Checkpoint>) {
        if discovered.is_empty() {
            return;
        }
        let mut cache = lock_mutex(&self.cache);
        let Some(index) = cache.entries.get_mut(path) else {
            return;
        };
        if index.version != version {
            return;
        }
        index.checkpoints.extend(discovered);
        index.checkpoints.sort_by_key(|checkpoint| checkpoint.line);
        index.checkpoints.dedup_by_key(|checkpoint| checkpoint.line);
        if index.checkpoints.len() > MAX_CHECKPOINTS_PER_FILE {
            let keep_from = index.checkpoints.len() - MAX_CHECKPOINTS_PER_FILE;
            index.checkpoints.drain(1..=keep_from);
        }
    }
}

fn directory_read_error() -> ToolExecutionError {
    ToolError::InvalidInput {
        message: DIRECTORY_PATH_MESSAGE.into(),
    }
    .into()
}

/// Empty output would be ambiguous: say whether the file is empty or the
/// offset is past its last line.
fn empty_page_note(file_len: u64, offset: usize, lines: usize) -> String {
    if file_len == 0 {
        "[empty file]".into()
    } else {
        format!(
            "[offset {offset} is past the end; file has {lines} line{}]",
            if lines == 1 { "" } else { "s" }
        )
    }
}

/// `path` as a PowerShell single-quoted literal: a quote inside it is doubled.
#[cfg(windows)]
fn shell_literal(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

/// `path` as a POSIX single-quoted literal: a quote inside it becomes `'\''`.
#[cfg(not(windows))]
fn shell_literal(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(windows)]
fn slice_hint(path: &Path) -> String {
    format!(
        "Slice it with shell instead, for example `Get-Content -LiteralPath {path} -TotalCount 200`, `Get-Content -LiteralPath {path} -Tail 200` or `Select-String -LiteralPath {path} -Pattern <text>`.",
        path = shell_literal(path)
    )
}

#[cfg(not(windows))]
fn slice_hint(path: &Path) -> String {
    format!(
        "Slice it with shell instead, for example `head -n 200 {path}`, `tail -n 200 {path}` or `grep -n -F -- <text> {path}`.",
        path = shell_literal(path)
    )
}

#[cfg(windows)]
fn long_line_hint(path: &Path, line: usize) -> String {
    format!(
        "Line {line} alone is over the limit. Search for a distinctive substring of it (a hit on a long line is windowed around the match), or slice it with shell, for example `(Get-Content -LiteralPath {path})[{index}].Substring(0, 4000)`.",
        path = shell_literal(path),
        index = line - 1
    )
}

#[cfg(not(windows))]
fn long_line_hint(path: &Path, line: usize) -> String {
    format!(
        "Line {line} alone is over the limit. Search for a distinctive substring of it (a hit on a long line is windowed around the match), or slice it with shell, for example `sed -n '{line}p' {path} | cut -c 1-4000`.",
        path = shell_literal(path)
    )
}

const SMALLER_PAGE_HINT: &str = "Pass a smaller max_lines to read fewer lines per page.";

fn page_too_large(
    dependency: &DependencyObservation,
    bytes_read: u64,
    context: String,
) -> ToolExecutionError {
    with_context(
        observed_read_error(
            ToolError::InvalidInput {
                message: format!("read page exceeds the {MAX_READ_PAGE_BYTES}-byte safety limit"),
            },
            dependency,
            bytes_read,
        ),
        context,
    )
}

pub(super) const NOT_REGULAR_FILE_MESSAGE: &str =
    "path is not a regular file (FIFO, socket or device); read and write handle regular files only";

/// Opening a FIFO with no writer blocks forever, so special files are refused
/// from their metadata before any `open`. Missing paths and directories pass:
/// their own errors are produced where they are opened.
pub(super) fn ensure_not_special_file(path: &Path) -> Result<(), ToolError> {
    match std::fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() && !metadata.is_dir() => Err(ToolError::InvalidInput {
            message: NOT_REGULAR_FILE_MESSAGE.into(),
        }),
        _ => Ok(()),
    }
}

fn with_context(mut failure: ToolExecutionError, context: String) -> ToolExecutionError {
    failure.context = Some(context);
    failure
}

/// A file that is not UTF-8 text: what it is, how to read it, and how to
/// replace it. `bytes` starts at line `first_line`; `valid_up_to` is the
/// first invalid byte.
pub(super) fn non_utf8_message(
    path: &Path,
    bytes: &[u8],
    valid_up_to: usize,
    first_line: usize,
) -> String {
    // UTF-32 LE begins with the UTF-16 LE mark: test the longer one first.
    // (PowerShell `-Encoding`, iconv `-f`) for each detected encoding.
    let (what, (encoding, iconv)) = if first_line == 1
        && bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
    {
        (
            "a UTF-32 LE file (BOM detected)".to_owned(),
            ("UTF32", "UTF-32LE"),
        )
    } else if first_line == 1 && bytes.starts_with(&[0xFF, 0xFE]) {
        (
            "a UTF-16 LE file (BOM detected)".to_owned(),
            ("Unicode", "UTF-16LE"),
        )
    } else if first_line == 1 && bytes.starts_with(&[0xFE, 0xFF]) {
        (
            "a UTF-16 BE file (BOM detected)".to_owned(),
            ("BigEndianUnicode", "UTF-16BE"),
        )
    } else {
        let line = first_line + bytes[..valid_up_to].iter().filter(|b| **b == b'\n').count();
        (
            format!(
                "not valid UTF-8 (invalid byte 0x{:02x} at line {line}); probably a legacy 8-bit encoding",
                bytes.get(valid_up_to).copied().unwrap_or_default()
            ),
            ("Default", "WINDOWS-1252"),
        )
    };
    let reader = if cfg!(windows) {
        format!(
            "Get-Content -LiteralPath {literal} -Encoding {encoding}",
            literal = shell_literal(path)
        )
    } else {
        format!(
            "iconv -f {iconv} -t UTF-8 {literal}",
            literal = shell_literal(path)
        )
    };
    format!(
        "{path} is {what}; read it with shell `{reader}`; to replace it with UTF-8 text, remove or rename it with shell, then create it with the write tool",
        path = path.display()
    )
}

fn read_path_error(path: &Path, error: io::Error) -> ToolExecutionError {
    if path.is_dir() {
        return directory_read_error();
    }
    let missing = error.kind() == io::ErrorKind::NotFound;
    let mut failure = ToolExecutionError::from(ToolError::from(error));
    if missing {
        failure.context = Some(format!(
            "{}: file does not exist; use list or search to locate it. If the task requires creating it, use write with expected omitted or null.",
            path.display()
        ));
    }
    failure
}

fn observed_read_error(
    error: ToolError,
    dependency: &DependencyObservation,
    bytes_read: u64,
) -> ToolExecutionError {
    ToolExecutionError::observed(error, vec![dependency.clone()], bytes_read)
}

fn read_utf8_line<'a>(
    reader: &mut BufReader<std::fs::File>,
    buffer: &'a mut Vec<u8>,
    dependency: &DependencyObservation,
    bytes_read: &mut u64,
    line_number: usize,
) -> Result<Option<&'a str>, ToolExecutionError> {
    buffer.clear();
    let read = match reader.read_until(b'\n', buffer) {
        Ok(read) => read,
        Err(error) => {
            *bytes_read =
                bytes_read.saturating_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
            return Err(observed_read_error(error.into(), dependency, *bytes_read));
        }
    };
    *bytes_read = bytes_read.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    if read == 0 {
        return Ok(None);
    }
    // NUL bytes are valid UTF-8, so UTF-16/UTF-32 text without a byte-order
    // mark would come back as letters interleaved with NULs. `search` skips
    // such files as binary; `read` says so instead of returning them. Bytes
    // that are not UTF-8 at all keep their own, more specific message.
    let has_nul = buffer.contains(&0);
    match std::str::from_utf8(buffer) {
        Ok(_) if has_nul => Err(observed_read_error(
            ToolError::InvalidInput {
                message: format!(
                    "{} has NUL bytes at line {line_number}: it is binary, or UTF-16/UTF-32 text without a byte-order mark; convert it to UTF-8 first (iconv -f UTF-16LE -t UTF-8) or inspect it with a shell tool",
                    dependency.path.display()
                ),
            },
            dependency,
            *bytes_read,
        )),
        Ok(text) => Ok(Some(text)),
        Err(error) => Err(observed_read_error(
            ToolError::InvalidInput {
                message: non_utf8_message(
                    &dependency.path,
                    buffer,
                    error.valid_up_to(),
                    line_number,
                ),
            },
            dependency,
            *bytes_read,
        )),
    }
}

fn record_checkpoint(checkpoints: &mut Vec<Checkpoint>, line: usize, offset: u64) {
    if line
        .saturating_sub(1)
        .is_multiple_of(CHECKPOINT_INTERVAL_LINES)
    {
        checkpoints.push(Checkpoint { line, offset });
    }
}

fn cache_lookup_paths(path: &Path) -> Vec<PathBuf> {
    let raw = path.to_path_buf();
    match path.canonicalize() {
        Ok(canonical) if canonical != raw => vec![raw, canonical],
        Ok(canonical) => vec![canonical],
        Err(_) => vec![raw],
    }
}

fn file_version_best_effort(path: &Path) -> FileVersion {
    let opened = path
        .canonicalize()
        .ok()
        .and_then(|canonical| std::fs::File::open(canonical).ok());
    let Some(file) = opened else {
        return FileVersion {
            len: 0,
            modified: None,
            file_id: None,
        };
    };
    match file.metadata() {
        Ok(metadata) => {
            let stamp = FastStamp::from_file(DependencyKind::File, &file, &metadata, None);
            FileVersion {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                file_id: stamp.file_id,
            }
        }
        Err(_) => FileVersion {
            len: 0,
            modified: None,
            file_id: None,
        },
    }
}

fn dependency_from_cached(
    canonical: &Path,
    cached: &crate::project_index::CachedFile,
) -> DependencyObservation {
    let stamp = FastStamp {
        kind: DependencyKind::File,
        len: cached.version.len,
        modified_nanos: cached
            .version
            .modified
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos()),
        file_id: cached.version.fast_stamp_file_id(),
        operation_digest: None,
    };
    DependencyObservation {
        path: canonical.to_path_buf(),
        stamp,
    }
}

fn try_read_via_slab(canonical: &Path) -> Option<Arc<crate::project_index::CachedFile>> {
    let meta = std::fs::symlink_metadata(canonical).ok()?;
    if meta.file_type().is_symlink()
        || !meta.is_file()
        || meta.len() as usize > crate::project_index::MAX_SLAB_FILE_BYTES
    {
        return None;
    }
    let daemon = crate::project_index::existing_for_path(canonical)?;
    let cached = daemon.read_file_cached_with_meta(canonical, &meta).ok()??;
    if cached.text.as_bytes().contains(&0) {
        return None;
    }
    Some(cached)
}

fn evict_old_indexes(cache: &mut ReadCache) {
    while cache.entries.len() > MAX_INDEXED_FILES {
        let oldest = cache
            .entries
            .iter()
            .filter(|(_, index)| index.complete_digest.is_none())
            .min_by_key(|(_, index)| index.last_used)
            .or_else(|| {
                cache
                    .entries
                    .iter()
                    .min_by_key(|(_, index)| index.last_used)
            })
            .map(|(path, _)| path.clone());
        let Some(oldest) = oldest else {
            break;
        };
        cache.entries.remove(&oldest);
    }
}

fn strip_line_ending(line: &str) -> &str {
    let Some(line) = line.strip_suffix('\n') else {
        return line;
    };
    line.strip_suffix('\r').unwrap_or(line)
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// Computes a compact unified-style diff between `prev` and `cur` when the
/// change is small (<= 40% of `full_output_bytes`), stripping common prefix and
/// suffix first so local edits inside large files stay O(changed_span^2).
fn render_compact_delta(
    first_line: usize,
    prev: &[String],
    cur: &[String],
    full_output_bytes: usize,
) -> Option<String> {
    let mut prefix = 0usize;
    let min_len = prev.len().min(cur.len());
    while prefix < min_len && prev[prefix] == cur[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < (min_len - prefix)
        && prev[prev.len() - 1 - suffix] == cur[cur.len() - 1 - suffix]
    {
        suffix += 1;
    }
    if prefix + suffix == prev.len() && prefix + suffix == cur.len() {
        return None;
    }

    let old_mid = &prev[prefix..prev.len() - suffix];
    let new_mid = &cur[prefix..cur.len() - suffix];
    if old_mid.len().saturating_mul(new_mid.len()) > 250_000 {
        return None;
    }

    #[derive(Clone, Copy)]
    enum Op<'a> {
        Eq(&'a str),
        Del(&'a str),
        Add(&'a str),
    }

    // Standard LCS DP table on the trimmed changed middle only.
    let m = old_mid.len();
    let n = new_mid.len();
    let mut dp = vec![0u32; (m + 1) * (n + 1)];
    let stride = n + 1;
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i * stride + j] = if old_mid[i] == new_mid[j] {
                dp[(i + 1) * stride + (j + 1)] + 1
            } else {
                dp[(i + 1) * stride + j].max(dp[i * stride + (j + 1)])
            };
        }
    }

    let mut mid_ops = Vec::with_capacity(m + n);
    let (mut i, mut j) = (0usize, 0usize);
    while i < m && j < n {
        if old_mid[i] == new_mid[j] {
            mid_ops.push(Op::Eq(new_mid[j].as_str()));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * stride + j] >= dp[i * stride + (j + 1)] {
            mid_ops.push(Op::Del(old_mid[i].as_str()));
            i += 1;
        } else {
            mid_ops.push(Op::Add(new_mid[j].as_str()));
            j += 1;
        }
    }
    while i < m {
        mid_ops.push(Op::Del(old_mid[i].as_str()));
        i += 1;
    }
    while j < n {
        mid_ops.push(Op::Add(new_mid[j].as_str()));
        j += 1;
    }

    // Group ops into context-bounded hunks across the whole window.
    const CONTEXT: usize = 3;
    let mut all_ops: Vec<(usize, usize, Op<'_>)> = Vec::with_capacity(cur.len() + m);
    let mut old_ln = first_line;
    let mut new_ln = first_line;
    for line in &cur[..prefix] {
        all_ops.push((old_ln, new_ln, Op::Eq(line.as_str())));
        old_ln += 1;
        new_ln += 1;
    }
    for op in mid_ops {
        all_ops.push((old_ln, new_ln, op));
        match op {
            Op::Eq(_) => {
                old_ln += 1;
                new_ln += 1;
            }
            Op::Del(_) => old_ln += 1,
            Op::Add(_) => new_ln += 1,
        }
    }
    for line in &cur[cur.len() - suffix..] {
        all_ops.push((old_ln, new_ln, Op::Eq(line.as_str())));
        old_ln += 1;
        new_ln += 1;
    }

    let changed_indices: Vec<usize> = all_ops
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, _, op))| (!matches!(op, Op::Eq(_))).then_some(idx))
        .collect();
    if changed_indices.is_empty() {
        return None;
    }

    let mut spans: Vec<(usize, usize)> = Vec::new();
    for idx in changed_indices {
        let start = idx.saturating_sub(CONTEXT);
        let end = (idx + CONTEXT).min(all_ops.len().saturating_sub(1));
        if let Some(last) = spans.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        spans.push((start, end));
    }

    let mut hunks = String::new();
    for &(start, end) in &spans {
        let (hunk_old, hunk_new, _) = all_ops[start];
        let _ = writeln!(hunks, "@@ -{},{} +{},{} @@", hunk_old, end - start + 1, hunk_new, end - start + 1);
        for &(_, _, op) in &all_ops[start..=end] {
            let (sigil, text) = match op {
                Op::Eq(t) => (' ', t),
                Op::Del(t) => ('-', t),
                Op::Add(t) => ('+', t),
            };
            hunks.push(sigil);
            hunks.push_str(text);
            if !hunks.ends_with('\n') {
                hunks.push('\n');
            }
        }
    }

    let rendered = format!(
        "[delta since previous read (file modified); {} hunk(s); pass raw:true for full text]\n{}",
        spans.len(),
        hunks.trim_end_matches('\n')
    );
    (rendered.len() <= (full_output_bytes.saturating_mul(2)) / 5).then_some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn paged_read_records_complete_digest_after_last_page() {
        let path =
            std::env::temp_dir().join(format!("slim-read-paged-digest-{}.txt", std::process::id()));
        const PAGE: usize = 64;
        let lines: Vec<String> = (1..=200).map(|line| format!("line {line}\n")).collect();
        let body = lines.concat();
        std::fs::write(&path, &body).expect("fixture");
        let canonical = path.canonicalize().expect("canonical");
        let service = ReadService::default();
        for start in (0..lines.len()).step_by(PAGE) {
            let end = (start + PAGE).min(lines.len());
            let page = service
                .read_file_range_resolved(&canonical, start + 1, Some(PAGE), false, None)
                .expect("page");
            let mut expected = lines[start..end].concat();
            if end < lines.len() {
                expected.push_str(&format!(
                    "\n[showing lines {}-{end}; more content available; pass \"offset\": {} for the next page]",
                    start + 1,
                    end + 1
                ));
            }
            assert_eq!(page.output, expected);
            let expected_bytes = lines[start..(end + 1).min(lines.len())].concat().len() as u64;
            assert_eq!(
                page.bytes_read,
                expected_bytes,
                "page starting at {}",
                start + 1
            );
        }
        assert_eq!(
            service.complete_digest(&canonical),
            Some(Sha256::digest(body.as_bytes()).into())
        );
        std::fs::remove_file(path).expect("remove fixture");
    }

    #[test]
    fn default_first_page_completes_a_file_that_fits_the_caps() {
        let path = std::env::temp_dir().join(format!(
            "slim-read-complete-if-small-{}.txt",
            std::process::id()
        ));
        let body = (1..=250)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(&path, &body).expect("fixture");
        let canonical = path.canonicalize().expect("canonical");
        let service = ReadService::default();
        let page = service
            .read_file_range_resolved(&canonical, 1, None, false, None)
            .expect("page");
        assert_eq!(page.output, body);
        assert!(!page.output.contains("showing lines"));
        assert_eq!(
            service.complete_digest(&canonical),
            Some(Sha256::digest(body.as_bytes()).into())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn explicit_max_lines_keeps_the_requested_window() {
        let path = std::env::temp_dir().join(format!(
            "slim-read-explicit-limit-{}.txt",
            std::process::id()
        ));
        let lines: Vec<String> = (1..=250).map(|line| format!("line {line}\n")).collect();
        let body = lines.concat();
        std::fs::write(&path, &body).expect("fixture");
        let canonical = path.canonicalize().expect("canonical");
        let service = ReadService::default();
        // Same numeric value as the omitted-limit default, but explicit: the
        // page must stay at 200 lines and must not record complete_digest.
        let page = service
            .read_file_range_resolved(&canonical, 1, Some(DEFAULT_MAX_READ_LINES), false, None)
            .expect("page");
        let expected = format!(
            "{}\n[showing lines 1-200; more content available; pass \"offset\": 201 for the next page]",
            lines[..200].concat()
        );
        assert_eq!(page.output, expected);
        assert_eq!(service.complete_digest(&canonical), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_utf8_after_a_full_page_does_not_fail_that_page() {
        let path = std::env::temp_dir().join(format!(
            "slim-read-bad-lookahead-{}.txt",
            std::process::id()
        ));
        let lines: Vec<String> = (1..=200).map(|line| format!("line {line}\n")).collect();
        let mut body = lines.concat().into_bytes();
        body.extend_from_slice(b"bad \xff byte\nline 202\n");
        std::fs::write(&path, &body).expect("fixture");
        let canonical = path.canonicalize().expect("canonical");
        let service = ReadService::default();
        let page = service
            .read_file_range_resolved(&canonical, 1, Some(200), false, None)
            .expect("the requested lines are valid UTF-8");
        assert_eq!(
            page.output,
            format!(
                "{}\n[showing lines 1-200; more content available; pass \"offset\": 201 for the next page]",
                lines.concat()
            )
        );
        assert!(service
            .read_file_range_resolved(&canonical, 201, Some(1), false, None)
            .is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn high_offsets_reuse_incremental_checkpoints() {
        let path =
            std::env::temp_dir().join(format!("slim-read-checkpoints-{}.txt", std::process::id()));
        let body = (1..=1024)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).expect("write");
        let service = ReadService::default();

        let first = service
            .read_file_range(&path, 769, 1, None)
            .expect("first high offset");
        let second = service
            .read_file_range(&path, 770, 1, None)
            .expect("adjacent high offset");

        assert!(first.output.starts_with("769: line-769"));
        assert!(second.output.starts_with("770: line-770"));
        assert_eq!(second.bytes_read, b"line-770\nline-771\n".len() as u64);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn smart_unbounded_overview_returns_outline_plus_preview_for_large_code_files() {
        let path =
            std::env::temp_dir().join(format!("slim-read-smart-overview-{}.rs", std::process::id()));
        let mut content = String::new();
        for idx in 1..=700 {
            content.push_str(&format!(
                "pub fn helper_{idx}(arg: usize) -> usize {{\n    arg + {idx}\n}}\n"
            ));
        }
        assert!(content.len() as u64 > SMART_READ_OUTLINE_BYTES);
        std::fs::write(&path, &content).expect("write");
        let canonical = path.canonicalize().expect("canonical");
        let service = ReadService::default();

        let (overview, _dep, _bytes) = service
            .smart_unbounded_overview(&canonical, None)
            .expect("overview")
            .expect("large rust file should yield smart overview");
        assert!(overview.contains("[smart read: 2100 lines; structural outline + lines 1-120"));
        assert!(overview.contains("helper_700"));
        assert!(overview.contains("    arg + 1\n"));
        assert!(!overview.contains("    arg + 200\n"));
        assert_eq!(service.complete_digest(&canonical), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn maybe_compact_delta_returns_hunks_on_modified_file_and_respects_raw() {
        let path =
            std::env::temp_dir().join(format!("slim-read-delta-{}.txt", std::process::id()));
        let orig_lines: Vec<String> = (1..=80).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, orig_lines.concat()).expect("write v1");
        let canonical = path.canonicalize().expect("canonical");
        let service = ReadService::default();

        let p1 = service
            .read_file_range_resolved(&canonical, 1, None, false, None)
            .expect("v1");
        assert!(service
            .maybe_compact_delta(&canonical, p1.first_line, &p1.records, p1.output.len(), false)
            .is_none());

        let mut mod_lines = orig_lines;
        mod_lines[40] = "line 41 MODIFIED\n".to_string();
        std::fs::write(&path, mod_lines.concat()).expect("write v2");

        let p2 = service
            .read_file_range_resolved(&canonical, 1, None, false, None)
            .expect("v2");
        let delta = service
            .maybe_compact_delta(&canonical, p2.first_line, &p2.records, p2.output.len(), false)
            .expect("small local edit should produce compact delta");
        assert!(delta.contains("[delta since previous read"));
        assert!(delta.contains("-line 41"));
        assert!(delta.contains("+line 41 MODIFIED"));

        // `raw: true` forces full page by returning None for delta suppression.
        mod_lines[41] = "line 42 MODIFIED\n".to_string();
        std::fs::write(&path, mod_lines.concat()).expect("write v3");
        let p3 = service
            .read_file_range_resolved(&canonical, 1, None, false, None)
            .expect("v3");
        assert!(service
            .maybe_compact_delta(&canonical, p3.first_line, &p3.records, p3.output.len(), true)
            .is_none());
        let _ = std::fs::remove_file(path);
    }
}
