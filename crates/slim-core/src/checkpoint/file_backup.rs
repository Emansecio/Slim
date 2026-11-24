use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    is_safe_workspace_rel_path, now_ms, project_hash, remove_file_and_empty_parents,
    sanitize_ref_component, slim_data_root, CheckpointPayload, RestoreSummary, TurnCheckpoint,
    CHECKPOINT_VERSION,
};
use crate::tools::SKIP_DIR_NAMES;

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOTAL_BACKUP_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
struct FileEntryMeta {
    sha256: String,
    size: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
struct BackupManifest {
    files: BTreeMap<String, FileEntryMeta>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn project_backup_root(cwd: &Path) -> Result<PathBuf, String> {
    let data = slim_data_root().ok_or_else(|| "cannot resolve HOME or SLIM_HOME".to_owned())?;
    let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    Ok(data.join("checkpoints").join(project_hash(&canon)))
}

fn walk_workspace_files(cwd: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut builder = WalkBuilder::new(cwd);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if name == ".git" || name == ".slim" {
                return false;
            }
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                return !SKIP_DIR_NAMES.contains(&name.as_ref());
            }
            true
        });

    let mut out = Vec::new();
    for entry in builder.build().flatten() {
        let Ok(ft) = entry.file_type().ok_or(()) else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let Ok(rel_path) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        if !is_safe_workspace_rel_path(&rel) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        if size <= MAX_FILE_BYTES {
            out.push((rel, entry.path().to_path_buf(), size));
        }
    }
    out
}

pub(super) fn capture(cwd: &Path, session_id: &str, first_seq: u64) -> Result<CheckpointPayload, String> {
    capture_at(cwd, &format!("{}-{}", sanitize_ref_component(session_id), first_seq))
}

fn capture_at(cwd: &Path, slot_name: &str) -> Result<CheckpointPayload, String> {
    let proj_dir = project_backup_root(cwd)?;
    let blobs_dir = proj_dir.join("blobs");
    let snap_dir = proj_dir.join("snaps").join(slot_name);
    std::fs::create_dir_all(&blobs_dir)
        .map_err(|e| format!("failed to create checkpoint blobs dir: {e}"))?;
    std::fs::create_dir_all(&snap_dir)
        .map_err(|e| format!("failed to create checkpoint snap dir: {e}"))?;

    let mut manifest = BackupManifest::default();
    let mut total_bytes = 0u64;
    for (rel, full, size) in walk_workspace_files(cwd) {
        if total_bytes.saturating_add(size) > MAX_TOTAL_BACKUP_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        let hash = sha256_hex(&bytes);
        let blob_path = blobs_dir.join(&hash);
        if !blob_path.exists() {
            let _ = std::fs::write(&blob_path, &bytes);
        }
        manifest.files.insert(
            rel,
            FileEntryMeta {
                sha256: hash,
                size: bytes.len() as u64,
            },
        );
    }

    let manifest_path = snap_dir.join("manifest.json");
    let encoded = serde_json::to_vec(&manifest)
        .map_err(|e| format!("failed to serialize checkpoint manifest: {e}"))?;
    std::fs::write(&manifest_path, encoded)
        .map_err(|e| format!("failed to write checkpoint manifest: {e}"))?;

    Ok(CheckpointPayload::FileBackup {
        backup_dir: snap_dir.display().to_string(),
        file_count: manifest.files.len(),
    })
}

fn load_manifest(backup_dir: &Path) -> Result<(BackupManifest, PathBuf), String> {
    let manifest_path = backup_dir.join("manifest.json");
    let data = std::fs::read(&manifest_path)
        .map_err(|e| format!("failed to read checkpoint manifest: {e}"))?;
    let manifest: BackupManifest = serde_json::from_slice(&data)
        .map_err(|e| format!("corrupt checkpoint manifest: {e}"))?;
    let blobs_dir = backup_dir
        .parent()
        .and_then(|snaps| snaps.parent())
        .map(|proj| proj.join("blobs"))
        .ok_or_else(|| "invalid checkpoint backup path layout".to_owned())?;
    Ok((manifest, blobs_dir))
}

fn compute_diff(cwd: &Path, manifest: &BackupManifest) -> (Vec<String>, Vec<String>) {
    let mut cur_map = BTreeMap::new();
    for (rel, full, size) in walk_workspace_files(cwd) {
        cur_map.insert(rel, (full, size));
    }
    let mut restored_files = Vec::new();
    let mut removed_files = Vec::new();

    for rel in cur_map.keys() {
        if !manifest.files.contains_key(rel) {
            removed_files.push(rel.clone());
        }
    }
    for (rel, meta) in &manifest.files {
        match cur_map.get(rel) {
            None => restored_files.push(rel.clone()),
            Some((full, size)) => {
                let unchanged = *size == meta.size
                    && std::fs::read(full)
                        .map(|bytes| sha256_hex(&bytes) == meta.sha256)
                        .unwrap_or(false);
                if !unchanged {
                    restored_files.push(rel.clone());
                }
            }
        }
    }
    (restored_files, removed_files)
}

pub(super) fn preview(cwd: &Path, backup_dir: &Path) -> Result<RestoreSummary, String> {
    let (manifest, _) = load_manifest(backup_dir)?;
    let (restored_files, removed_files) = compute_diff(cwd, &manifest);
    Ok(RestoreSummary {
        restored_files,
        removed_files,
        undo_checkpoint: None,
    })
}

pub(super) fn restore(
    cwd: &Path,
    session_id: &str,
    undo_seq: u64,
    backup_dir: &Path,
) -> Result<RestoreSummary, String> {
    let (manifest, blobs_dir) = load_manifest(backup_dir)?;
    let undo_slot = format!("{}-undo-{}", sanitize_ref_component(session_id), undo_seq);
    let undo_payload = capture_at(cwd, &undo_slot)?;
    let undo_checkpoint = TurnCheckpoint {
        version: CHECKPOINT_VERSION,
        first_seq: undo_seq,
        created_at_ms: now_ms(),
        cwd: cwd.display().to_string(),
        payload: undo_payload,
    };

    let (restored_files, removed_files) = compute_diff(cwd, &manifest);
    for rel in &removed_files {
        remove_file_and_empty_parents(cwd, rel)?;
    }
    for rel in &restored_files {
        if !is_safe_workspace_rel_path(rel) {
            continue;
        }
        let Some(meta) = manifest.files.get(rel) else {
            continue;
        };
        let blob_path = blobs_dir.join(&meta.sha256);
        let bytes = std::fs::read(&blob_path)
            .map_err(|e| format!("missing checkpoint blob for {rel}: {e}"))?;
        let dest = cwd.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to recreate parent dir for {rel}: {e}"))?;
        }
        std::fs::write(&dest, bytes)
            .map_err(|e| format!("failed to restore {rel}: {e}"))?;
    }

    Ok(RestoreSummary {
        restored_files,
        removed_files,
        undo_checkpoint: Some(undo_checkpoint),
    })
}

pub(super) fn prune(cwd: &Path, keep_latest: usize, max_age_days: u64) {
    let Ok(proj_dir) = project_backup_root(cwd) else {
        return;
    };
    let snaps_dir = proj_dir.join("snaps");
    let Ok(entries) = std::fs::read_dir(&snaps_dir) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(max_age_days.saturating_mul(86_400)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut dirs: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
        .map(|e| {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (mtime, e.path())
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (idx, (mtime, path)) in dirs.into_iter().enumerate() {
        if idx >= keep_latest || mtime < cutoff {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
