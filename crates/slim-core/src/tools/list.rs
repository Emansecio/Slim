use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::execution::digest_bytes;
use super::{DependencyObservation, FastStamp, ToolError, ToolExecutionError};
use crate::runtime::CancellationToken;

pub const DEFAULT_MAX_ENTRIES: usize = 200;
pub const MAX_ENTRIES_CAP: usize = 500;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_LIST_SNAPSHOTS: usize = 64;
const LIST_SNAPSHOT_TTL: Duration = Duration::from_secs(120);

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default)]
pub(crate) struct ListService {
    snapshots: Arc<Mutex<ListSnapshotCache>>,
}

#[derive(Debug, Default)]
struct ListSnapshotCache {
    snapshots: HashMap<String, ListSnapshot>,
}

#[derive(Debug)]
struct ListSnapshot {
    path: PathBuf,
    entries: Arc<Vec<PathBuf>>,
    stamp: FastStamp,
    expires_at: Instant,
    last_used: Instant,
}

#[derive(Clone, Debug)]
pub(crate) struct ListPage {
    pub entries: Vec<PathBuf>,
    pub first: usize,
    pub total: usize,
    pub next_cursor: Option<String>,
    snapshot_id: String,
    pub(crate) dependency: DependencyObservation,
    pub(crate) bytes_read: u64,
}

impl ListPage {
    pub(crate) fn present(&self, max_bytes: usize, display_root: &Path) -> super::ToolPresentation {
        let render = |count: usize| {
            let mut text = self.entries[..count]
                .iter()
                .map(|entry| {
                    super::search::display_path(display_root, entry)
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let next = self.first.saturating_add(count);
            if next <= self.total {
                text.push_str(&format!("\n\n[showing entries {}-{} of {}; pass \"cursor\": \"{}:{next:x}\" for the next page]", self.first, next.saturating_sub(1), self.total, self.snapshot_id));
            }
            text
        };
        let full = render(self.entries.len());
        if full.len() <= max_bytes {
            return super::ToolPresentation {
                text: full,
                delivered_records: self.entries.len(),
                complete: true,
                oversized_record: false,
            };
        }
        let mut low = 0;
        let mut high = self.entries.len();
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
            text.insert_str(0, "[entry and continuation exceed presentation budget; no entry delivered; narrow the path or use the continuation]");
        }
        super::ToolPresentation {
            text,
            delivered_records: low,
            complete: false,
            oversized_record: low == 0,
        }
    }
}

struct DirectoryScan {
    entries: Vec<PathBuf>,
    stamp: FastStamp,
    bytes_read: u64,
}

fn directory_entry_safety_limit_message() -> String {
    format!(
        "directory exceeds the {MAX_DIRECTORY_ENTRIES} entry safety limit; pass a narrower path"
    )
}

pub fn list_directory(path: impl AsRef<Path>) -> Result<Vec<PathBuf>, ToolError> {
    collect_directory(path.as_ref(), None)
        .map(|scan| scan.entries)
        .map_err(|failure| failure.error)
}

impl ListService {
    pub(crate) fn page(
        &self,
        path: &Path,
        offset: usize,
        max_entries: usize,
        cursor: Option<&str>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ListPage, ToolExecutionError> {
        if let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) {
            return self.page_from_cursor(path, max_entries, cursor, cancellation);
        }
        // Offset pagination without a cursor reuses the live snapshot for the
        // same directory instead of rescanning it. Fresh lists (offset == 1)
        // always rescan so new entries are observed.
        if offset > 1 {
            if let Some((id, entries, stamp)) = self.reuse_snapshot(path) {
                return Ok(build_page(
                    &id,
                    entries.as_ref(),
                    offset,
                    max_entries,
                    path,
                    stamp,
                    0,
                ));
            }
        }
        let canonical = path.to_path_buf();
        let scan = collect_directory(&canonical, cancellation)?;
        let entries = Arc::new(scan.entries);
        let id = new_snapshot_id("list");
        let now = Instant::now();
        {
            let mut cache = self
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remove_expired(&mut cache, now);
            cache
                .snapshots
                .retain(|_, snapshot| snapshot.path != canonical);
            cache.snapshots.insert(
                id.clone(),
                ListSnapshot {
                    path: canonical,
                    entries: Arc::clone(&entries),
                    stamp: scan.stamp.clone(),
                    expires_at: now + LIST_SNAPSHOT_TTL,
                    last_used: now,
                },
            );
            evict_old_snapshots(&mut cache);
        }
        Ok(build_page(
            &id,
            entries.as_ref(),
            offset,
            max_entries,
            path,
            scan.stamp,
            scan.bytes_read,
        ))
    }

    fn page_from_cursor(
        &self,
        path: &Path,
        max_entries: usize,
        cursor: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ListPage, ToolExecutionError> {
        check_cancelled(cancellation)?;
        let (id, offset) = parse_cursor(cursor).ok_or_else(invalid_cursor_error)?;
        if !id.starts_with("list-") {
            return Err(invalid_cursor_error().into());
        }
        let canonical = path.to_path_buf();
        let now = Instant::now();
        let (entries, stamp) = {
            let mut cache = self
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remove_expired(&mut cache, now);
            let Some(snapshot) = cache.snapshots.get_mut(id) else {
                return Err(expired_cursor_error().into());
            };
            if snapshot.path != canonical {
                return Err(ToolError::InvalidInput {
                    message: "list cursor does not match this path; start a new list request"
                        .into(),
                }
                .into());
            }
            snapshot.last_used = now;
            snapshot.expires_at = now + LIST_SNAPSHOT_TTL;
            (Arc::clone(&snapshot.entries), snapshot.stamp.clone())
        };
        check_cancelled(cancellation)?;
        Ok(build_page(
            id,
            entries.as_ref(),
            offset,
            max_entries,
            &canonical,
            stamp,
            0,
        ))
    }

    /// Latest live snapshot for the same directory, if any. No I/O,
    /// TTL-bounded; mirrors `page_from_cursor` bookkeeping.
    fn reuse_snapshot(&self, path: &Path) -> Option<(String, Arc<Vec<PathBuf>>, FastStamp)> {
        let canonical = path.to_path_buf();
        let now = Instant::now();
        let mut cache = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        remove_expired(&mut cache, now);
        let id = cache
            .snapshots
            .iter()
            .filter(|(_, snapshot)| snapshot.path == canonical)
            .max_by_key(|(_, snapshot)| snapshot.last_used)
            .map(|(id, _)| id.clone())?;
        let snapshot = cache.snapshots.get_mut(&id)?;
        snapshot.last_used = now;
        snapshot.expires_at = now + LIST_SNAPSHOT_TTL;
        Some((id, Arc::clone(&snapshot.entries), snapshot.stamp.clone()))
    }
}

fn build_page(
    id: &str,
    entries: &[PathBuf],
    offset: usize,
    max_entries: usize,
    path: &Path,
    stamp: FastStamp,
    bytes_read: u64,
) -> ListPage {
    let first_index = offset.saturating_sub(1).min(entries.len());
    let page_entries = entries
        .iter()
        .skip(first_index)
        .take(max_entries)
        .cloned()
        .collect::<Vec<_>>();
    let next_index = first_index.saturating_add(page_entries.len());
    let next_cursor =
        (next_index < entries.len()).then(|| format!("{id}:{:x}", next_index.saturating_add(1)));
    ListPage {
        entries: page_entries,
        first: first_index.saturating_add(1),
        total: entries.len(),
        next_cursor,
        snapshot_id: id.to_owned(),
        dependency: DependencyObservation {
            path: path.to_path_buf(),
            stamp,
        },
        bytes_read,
    }
}

fn collect_directory(
    path: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<DirectoryScan, ToolExecutionError> {
    let mut entries = Vec::new();
    if let Err(error) = check_cancelled(cancellation) {
        return Err(observed_directory_failure(path, &entries, error));
    }
    for entry in std::fs::read_dir(path)? {
        if let Err(error) = check_cancelled(cancellation) {
            return Err(observed_directory_failure(path, &entries, error));
        }
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(observed_directory_failure(
                path,
                &entries,
                ToolError::InvalidInput {
                    message: directory_entry_safety_limit_message(),
                },
            ));
        }
        let entry = entry
            .map_err(|error| observed_directory_failure(path, &entries, ToolError::from(error)))?;
        entries.push(entry.path());
    }
    if let Err(error) = check_cancelled(cancellation) {
        return Err(observed_directory_failure(path, &entries, error));
    }
    entries.sort();
    let (stamp, bytes_read) = directory_stamp(path, &entries);
    Ok(DirectoryScan {
        entries,
        stamp,
        bytes_read,
    })
}

fn observed_directory_failure(
    path: &Path,
    entries: &[PathBuf],
    error: ToolError,
) -> ToolExecutionError {
    let mut entries = entries.to_vec();
    entries.sort();
    let (stamp, bytes_read) = directory_stamp(path, &entries);
    ToolExecutionError::observed(
        error,
        vec![DependencyObservation {
            path: path.to_path_buf(),
            stamp,
        }],
        bytes_read,
    )
}

fn directory_stamp(path: &Path, entries: &[PathBuf]) -> (FastStamp, u64) {
    let modified_nanos = std::fs::metadata(path).ok().and_then(|metadata| {
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
    });
    let bytes_read = entries.iter().fold(0u64, |total, entry| {
        total.saturating_add(
            u64::try_from(entry.file_name().map(|name| name.len()).unwrap_or(0))
                .unwrap_or(u64::MAX),
        )
    });
    let digest = digest_bytes(
        b"slim-observed-directory-v2",
        format!("{}:{}", entries.len(), modified_nanos.unwrap_or(0)).as_bytes(),
    );
    let mut stamp = FastStamp::observed_directory(entries.len(), digest);
    stamp.modified_nanos = modified_nanos;
    (stamp, bytes_read)
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), ToolError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(ToolError::Cancelled);
    }
    Ok(())
}

fn new_snapshot_id(prefix: &str) -> String {
    let sequence = NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:x}-{:x}", std::process::id(), sequence)
}

fn parse_cursor(cursor: &str) -> Option<(&str, usize)> {
    let (id, offset) = cursor.rsplit_once(':')?;
    let offset = usize::from_str_radix(offset, 16).ok()?;
    (offset > 0 && !id.is_empty()).then_some((id, offset))
}

fn invalid_cursor_error() -> ToolError {
    ToolError::InvalidInput {
        message: "list cursor is invalid; start a new list request".into(),
    }
}

fn expired_cursor_error() -> ToolError {
    ToolError::InvalidInput {
        message: "list cursor expired or was invalidated; start a new list request".into(),
    }
}

fn remove_expired(cache: &mut ListSnapshotCache, now: Instant) {
    cache
        .snapshots
        .retain(|_, snapshot| snapshot.expires_at > now);
}

fn evict_old_snapshots(cache: &mut ListSnapshotCache) {
    while cache.snapshots.len() > MAX_LIST_SNAPSHOTS {
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

#[cfg(test)]
mod tests {
    use super::{directory_entry_safety_limit_message, parse_cursor};

    #[test]
    fn presentation_keeps_final_snapshot_cursor_and_complete_paths() {
        let entries = (0..30)
            .map(|n| std::path::PathBuf::from(format!("folder/{n:03}-{}.txt", "á".repeat(40))))
            .collect::<Vec<_>>();
        let page = super::build_page(
            "list-test",
            &entries,
            8,
            500,
            std::path::Path::new("folder"),
            super::FastStamp::observed_directory(30, "fixture".into()),
            0,
        );
        assert!(page.next_cursor.is_none());
        let result = page.present(400, std::path::Path::new("folder"));
        assert!(result.delivered_records > 0 && result.delivered_records < page.entries.len());
        assert!(result.text.len() <= 400);
        let next = page.first + result.delivered_records;
        assert!(result.text.contains(&format!("list-test:{next:x}")));
        for entry in &page.entries[..result.delivered_records] {
            assert!(result
                .text
                .contains(&entry.file_name().unwrap().to_string_lossy().to_string()));
        }
        let empty = page.present(1, std::path::Path::new("folder"));
        assert_eq!(empty.delivered_records, 0);
        assert!(empty.text.contains("list-test:8"));
        assert!(empty.oversized_record);
    }

    #[test]
    fn safety_limit_copy_names_the_cap_and_narrower_path() {
        let message = directory_entry_safety_limit_message();
        assert!(message.contains("10000"));
        assert!(message.contains("narrower path"));
    }

    #[test]
    fn cursor_is_opaque_but_carries_the_snapshot_position() {
        assert_eq!(parse_cursor("list-1-a:10"), Some(("list-1-a", 16)));
    }
}
