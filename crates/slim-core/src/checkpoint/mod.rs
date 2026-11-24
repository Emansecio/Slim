//! Automatic per-turn workspace checkpoints and reversible disk rewind.
//!
//! Uses an isolated shadow Git index (`GIT_INDEX_FILE` + `git write-tree` +
//! `refs/slim/checkpoints/*`) inside Git repositories so user `index`, `stash`,
//! and commit history remain untouched. Falls back to content-addressed file
//! snapshots under `~/.slim/checkpoints/` outside Git repositories.

mod file_backup;
mod git_shadow;
#[cfg(test)]
pub(crate) mod tests;

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const TURN_CHECKPOINT_NAMESPACE: &str = "turn.checkpoint.v1";
const CHECKPOINT_VERSION: u8 = 1;
pub const DEFAULT_KEEP_LATEST: usize = 200;
pub const DEFAULT_MAX_AGE_DAYS: u64 = 30;

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum CheckpointPayload {
    GitShadow {
        repo_root: String,
        tree_sha: String,
        ref_name: String,
    },
    FileBackup {
        backup_dir: String,
        file_count: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct TurnCheckpoint {
    pub version: u8,
    pub first_seq: u64,
    pub created_at_ms: u64,
    pub cwd: String,
    pub payload: CheckpointPayload,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RestoreSummary {
    pub restored_files: Vec<String>,
    pub removed_files: Vec<String>,
    pub undo_checkpoint: Option<TurnCheckpoint>,
}

impl RestoreSummary {
    pub fn total_touched(&self) -> usize {
        self.restored_files.len() + self.removed_files.len()
    }
}

/// Captures a reversible workspace checkpoint prior to executing a turn.
pub fn capture_turn_checkpoint(
    cwd: &Path,
    session_id: &str,
    first_seq: u64,
) -> Result<TurnCheckpoint, String> {
    let created_at_ms = now_ms();
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    // Only use Git shadow indexing when the workspace itself is the repository root;
    // otherwise an ancestor `.git` (e.g. ~/Documents/.git) would run `git add -A`
    // across unrelated sibling folders. Subdirectories fall back to scoped file_backup.
    if let Some(repo_root) =
        git_shadow::git_repo_root(&canonical_cwd).filter(|root| *root == canonical_cwd)
    {
        let payload = git_shadow::capture(&repo_root, session_id, first_seq)?;
        return Ok(TurnCheckpoint {
            version: CHECKPOINT_VERSION,
            first_seq,
            created_at_ms,
            cwd: canonical_cwd.display().to_string(),
            payload,
        });
    }
    let payload = file_backup::capture(&canonical_cwd, session_id, first_seq)?;
    Ok(TurnCheckpoint {
        version: CHECKPOINT_VERSION,
        first_seq,
        created_at_ms,
        cwd: canonical_cwd.display().to_string(),
        payload,
    })
}

/// Computes which files would be restored or removed without mutating disk.
pub fn preview_restore(cwd: &Path, checkpoint: &TurnCheckpoint) -> Result<RestoreSummary, String> {
    match &checkpoint.payload {
        CheckpointPayload::GitShadow {
            repo_root,
            tree_sha,
            ..
        } => {
            let root = resolve_git_root(cwd, repo_root);
            git_shadow::preview(&root, tree_sha)
        }
        CheckpointPayload::FileBackup { backup_dir, .. } => {
            let target_dir = resolve_backup_cwd(cwd, &checkpoint.cwd);
            file_backup::preview(&target_dir, Path::new(backup_dir))
        }
    }
}

/// Restores disk state to `checkpoint` after capturing an `undo_checkpoint` of
/// the current workspace so the rewind itself can always be undone.
pub fn restore_turn_checkpoint(
    cwd: &Path,
    session_id: &str,
    checkpoint: &TurnCheckpoint,
) -> Result<RestoreSummary, String> {
    let undo_seq = now_ms();
    match &checkpoint.payload {
        CheckpointPayload::GitShadow {
            repo_root,
            tree_sha,
            ..
        } => {
            let root = resolve_git_root(cwd, repo_root);
            git_shadow::restore(&root, session_id, undo_seq, tree_sha)
        }
        CheckpointPayload::FileBackup { backup_dir, .. } => {
            let target_dir = resolve_backup_cwd(cwd, &checkpoint.cwd);
            file_backup::restore(&target_dir, session_id, undo_seq, Path::new(backup_dir))
        }
    }
}

/// Best-effort cleanup of stale checkpoints beyond retention bounds.
pub fn prune_old_checkpoints(cwd: &Path, keep_latest: usize, max_age_days: u64) {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if let Some(repo_root) = git_shadow::git_repo_root(&canonical_cwd) {
        git_shadow::prune(&repo_root, keep_latest);
    } else {
        file_backup::prune(&canonical_cwd, keep_latest, max_age_days);
    }
}

fn resolve_git_root(cwd: &Path, stored_root: &str) -> PathBuf {
    let stored = PathBuf::from(stored_root);
    if stored.exists() {
        return std::fs::canonicalize(&stored).unwrap_or(stored);
    }
    git_shadow::git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

fn resolve_backup_cwd(cwd: &Path, stored_cwd: &str) -> PathBuf {
    let cur = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if cur.is_dir() {
        cur
    } else {
        PathBuf::from(stored_cwd)
    }
}

pub(crate) fn slim_data_root() -> Option<PathBuf> {
    if let Some(overridden) = std::env::var_os("SLIM_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(overridden));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".slim"))
}

pub(crate) fn project_hash(canonical_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_root.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for b in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub(crate) fn now_ms() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

fn sanitize_ref_component(raw: &str) -> String {
    let mut cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    while cleaned.contains("--") {
        cleaned = cleaned.replace("--", "-");
    }
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "session".to_owned()
    } else {
        trimmed.chars().take(64).collect()
    }
}

fn is_safe_workspace_rel_path(rel: &str) -> bool {
    if rel.is_empty() || rel == ".slim" || rel.starts_with(".slim/") || rel == ".git" || rel.starts_with(".git/") {
        return false;
    }
    let path = Path::new(rel);
    !path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn remove_file_and_empty_parents(root: &Path, rel: &str) -> Result<(), String> {
    if !is_safe_workspace_rel_path(rel) {
        return Ok(());
    }
    let target = root.join(rel);
    if target.is_file() || target.is_symlink() {
        std::fs::remove_file(&target)
            .map_err(|e| format!("failed to remove {rel}: {e}"))?;
    }
    let mut parent = target.parent();
    while let Some(dir) = parent {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        parent = dir.parent();
    }
    Ok(())
}
