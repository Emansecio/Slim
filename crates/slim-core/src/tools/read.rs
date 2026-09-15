use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::{DependencyKind, DependencyObservation, FastStamp, ToolError, ToolExecutionError};
use crate::runtime::CancellationToken;

pub const DEFAULT_MAX_READ_LINES: usize = 200;
pub const MAX_READ_LINES_CAP: usize = 4096;
const MAX_READ_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_READ_PAGE_BYTES: usize = 1024 * 1024;
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
    clock: u64,
}

#[derive(Clone, Debug)]
struct ReadIndex {
    version: FileVersion,
    checkpoints: Vec<Checkpoint>,
    last_used: u64,
    complete_digest: Option<[u8; 32]>,
    read_progress: Option<ReadProgress>,
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
        let file =
            std::fs::File::open(canonical).map_err(|error| read_path_error(canonical, error))?;
        super::execution::verify_opened_path(&file, canonical)?;
        let metadata = file.metadata()?;
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
            return Err(observed_read_error(
                ToolError::InvalidInput {
                    message: format!(
                        "read file exceeds the {MAX_READ_FILE_BYTES}-byte safety limit"
                    ),
                },
                &dependency,
                0,
            ));
        }
        let requested_first = start_line.max(1);
        // Default first page of a file that fits the page/line caps returns
        // the whole file so one read records complete_digest. Explicit
        // max_lines and numbered public pages keep their requested window.
        let max_lines = if !numbered
            && requested_first == 1
            && max_lines.is_none()
            && metadata.len() <= MAX_READ_PAGE_BYTES.saturating_sub(128) as u64
        {
            MAX_READ_LINES_CAP
        } else {
            max_lines.unwrap_or(DEFAULT_MAX_READ_LINES)
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
            check_cancelled(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            if read_utf8_line(&mut reader, &mut line, &dependency, &mut bytes_read)?.is_none() {
                self.merge_checkpoints(canonical, version, discovered);
                return Ok(ReadPage {
                    output: String::new(),
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
        let mut content_hasher = if requested_first == 1 {
            Some(Sha256::new())
        } else {
            index
                .read_progress
                .as_ref()
                .filter(|progress| progress.next_line == requested_first)
                .map(|progress| progress.hasher.clone())
        };
        while returned < max_lines {
            check_cancelled(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            let Some(line) = read_utf8_line(&mut reader, &mut line, &dependency, &mut bytes_read)?
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
                return Err(observed_read_error(
                    ToolError::InvalidInput {
                        message: format!(
                            "read page exceeds the {MAX_READ_PAGE_BYTES}-byte safety limit"
                        ),
                    },
                    &dependency,
                    bytes_read,
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
            check_cancelled(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            // Capture the next page's start before lookahead consumes its first line.
            let next_page = Checkpoint {
                line: current_line,
                offset: checkpoint.offset.saturating_add(bytes_read),
            };
            let more =
                read_utf8_line(&mut reader, &mut line, &dependency, &mut bytes_read)?.is_some();
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
        if has_more {
            let first = requested_first;
            let last = current_line.saturating_sub(1);
            let footer = format!(
                "\n[showing lines {first}-{last}; more content available; pass \"offset\": {} for the next page]",
                last.saturating_add(1)
            );
            if output.len().saturating_add(footer.len()) > MAX_READ_PAGE_BYTES {
                return Err(observed_read_error(
                    ToolError::InvalidInput {
                        message: format!(
                            "read page exceeds the {MAX_READ_PAGE_BYTES}-byte safety limit"
                        ),
                    },
                    &dependency,
                    bytes_read,
                ));
            }
            output.push_str(&footer);
        }
        if let Some(hasher) = content_hasher {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in cache_lookup_paths(path) {
            cache.entries.remove(&key);
        }
    }

    /// A complete successful read, or a successful overwrite/patch of this
    /// path, authorizes an implicit overwrite. Create and failed writes do
    /// not. The writer rechecks this digest against the locked current file.
    pub(crate) fn complete_digest(&self, path: &Path) -> Option<[u8; 32]> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.clock = cache.clock.wrapping_add(1);
        let index = ReadIndex {
            version,
            checkpoints: vec![Checkpoint { line: 1, offset: 0 }],
            last_used: cache.clock,
            complete_digest: Some(digest),
            read_progress: None,
        };
        for key in cache_lookup_paths(path) {
            cache.entries.insert(key, index.clone());
        }
        evict_old_indexes(&mut cache);
    }

    fn index_for(&self, path: &Path, version: FileVersion) -> ReadIndex {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        };
        cache.entries.insert(path.to_path_buf(), index.clone());
        evict_old_indexes(&mut cache);
        index
    }

    fn merge_checkpoints(&self, path: &Path, version: FileVersion, discovered: Vec<Checkpoint>) {
        if discovered.is_empty() {
            return;
        }
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    #[cfg(test)]
    fn checkpoint_count(&self, path: &Path) -> usize {
        let canonical = path.canonicalize().expect("canonical path");
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .get(&canonical)
            .map_or(0, |index| index.checkpoints.len())
    }
}

fn read_path_error(path: &Path, error: io::Error) -> ToolExecutionError {
    let missing = error.kind() == io::ErrorKind::NotFound;
    let mut failure = ToolExecutionError::from(ToolError::from(error));
    if missing {
        failure.context = Some(format!(
            "{}: file does not exist. If the task requires creating it, use write with expected omitted or null; an existing file is not required for creation.",
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
    std::str::from_utf8(buffer).map(Some).map_err(|error| {
        observed_read_error(
            io::Error::new(io::ErrorKind::InvalidData, error).into(),
            dependency,
            *bytes_read,
        )
    })
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), ToolError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(ToolError::Cancelled);
    }
    Ok(())
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
        let checkpoints = service.checkpoint_count(&path);
        let second = service
            .read_file_range(&path, 770, 1, None)
            .expect("adjacent high offset");

        assert!(first.output.starts_with("769: line-769"));
        assert!(second.output.starts_with("770: line-770"));
        assert!(checkpoints >= 4);
        assert_eq!(second.bytes_read, b"line-770\nline-771\n".len() as u64);
        let _ = std::fs::remove_file(path);
    }
}
