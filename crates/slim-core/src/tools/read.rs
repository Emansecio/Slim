use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::{DependencyKind, DependencyObservation, FastStamp, ToolError, ToolExecutionError};
use crate::runtime::CancellationToken;

pub const DEFAULT_MAX_READ_LINES: usize = 80;
pub const MAX_READ_LINES_CAP: usize = 4096;
const MAX_READ_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_READ_PAGE_BYTES: usize = 1024 * 1024;
const CHECKPOINT_INTERVAL_LINES: usize = 256;
const MAX_CHECKPOINTS_PER_FILE: usize = 4096;
const MAX_INDEXED_FILES: usize = 32;

#[derive(Clone, Debug, Default)]
pub(crate) struct ReadService {
    cache: Arc<Mutex<ReadCache>>,
}

#[derive(Debug)]
pub(crate) struct ReadPage {
    pub(crate) output: String,
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
        let canonical = path.canonicalize()?;
        self.read_file_range_resolved(&canonical, start_line, max_lines, cancellation)
    }

    pub(crate) fn read_file_range_resolved(
        &self,
        canonical: &Path,
        start_line: usize,
        max_lines: usize,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ReadPage, ToolExecutionError> {
        let file = std::fs::File::open(canonical)?;
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
                    dependency,
                    bytes_read,
                });
            }
            current_line = current_line.saturating_add(1);
            let position = reader
                .stream_position()
                .map_err(|error| observed_read_error(error.into(), &dependency, bytes_read))?;
            record_checkpoint(&mut discovered, current_line, position);
        }

        let mut output = String::new();
        let mut returned = 0usize;
        while returned < max_lines {
            check_cancelled(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            let Some(line) = read_utf8_line(&mut reader, &mut line, &dependency, &mut bytes_read)?
            else {
                break;
            };
            let line = strip_line_ending(line);
            let rendered_bytes = decimal_digits(current_line)
                .saturating_add(2)
                .saturating_add(line.len())
                .saturating_add(1);
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
            output.push_str(&format!("{current_line}: {line}\n"));
            returned = returned.saturating_add(1);
            current_line = current_line.saturating_add(1);
            let position = reader
                .stream_position()
                .map_err(|error| observed_read_error(error.into(), &dependency, bytes_read))?;
            record_checkpoint(&mut discovered, current_line, position);
        }

        let has_more = if returned == max_lines && returned > 0 {
            check_cancelled(cancellation)
                .map_err(|error| observed_read_error(error, &dependency, bytes_read))?;
            let more =
                read_utf8_line(&mut reader, &mut line, &dependency, &mut bytes_read)?.is_some();
            if more {
                let following_line = current_line.saturating_add(1);
                let position = reader
                    .stream_position()
                    .map_err(|error| observed_read_error(error.into(), &dependency, bytes_read))?;
                record_checkpoint(&mut discovered, following_line, position);
            }
            more
        } else {
            false
        };
        self.merge_checkpoints(canonical, version, discovered);

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
        Ok(ReadPage {
            output,
            dependency,
            bytes_read,
        })
    }

    pub(crate) fn invalidate(&self, path: &Path) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .remove(&canonical);
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

fn evict_old_indexes(cache: &mut ReadCache) {
    while cache.entries.len() > MAX_INDEXED_FILES {
        let Some(oldest) = cache
            .entries
            .iter()
            .min_by_key(|(_, index)| index.last_used)
            .map(|(path, _)| path.clone())
        else {
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
        assert_eq!(service.checkpoint_count(&path), checkpoints);
        let _ = std::fs::remove_file(path);
    }
}
