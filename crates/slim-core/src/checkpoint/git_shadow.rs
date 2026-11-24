use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SHADOW_GIT_TIMEOUT: Duration = Duration::from_secs(3);

use super::{
    is_safe_workspace_rel_path, now_ms, remove_file_and_empty_parents, sanitize_ref_component,
    slim_data_root, CheckpointPayload, RestoreSummary, TurnCheckpoint, CHECKPOINT_VERSION,
};

static TMP_INDEX_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    fn new(prefix: &str) -> Result<Self, String> {
        let base = slim_data_root()
            .map(|r| r.join("checkpoints").join("tmp"))
            .unwrap_or_else(|| std::env::temp_dir().join("slim-checkpoints"));
        std::fs::create_dir_all(&base)
            .map_err(|e| format!("checkpoint temp dir creation failed: {e}"))?;
        let seq = TMP_INDEX_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("{prefix}-{}-{seq}.idx", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Ok(Self { path })
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) fn git_repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(&raw);
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

fn run_git_with_index(
    repo_root: &Path,
    index_path: &Path,
    args: &[&str],
) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .env("GIT_INDEX_FILE", index_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git {:?} failed: {e}", args))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + SHADOW_GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(format!(
                        "git {:?} timed out after {}s",
                        args,
                        SHADOW_GIT_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("git {:?} wait failed: {e}", args));
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("git {:?} exited with {}", args, status)
        } else {
            stderr
        })
    }
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {:?} failed: {e}", args))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("git {:?} exited with {}", args, out.status)
        } else {
            stderr
        })
    }
}

fn write_transient_tree(repo_root: &Path) -> Result<String, String> {
    let tmp = TempIndex::new("shadow")?;
    run_git_with_index(repo_root, &tmp.path, &["read-tree", "--empty"])?;
    run_git_with_index(repo_root, &tmp.path, &["add", "-A"])?;
    // Ensure `.slim/` session journals/artifacts are never captured or overwritten.
    let _ = run_git_with_index(
        repo_root,
        &tmp.path,
        &["rm", "-r", "--cached", "--ignore-unmatch", "-q", "--", ".slim"],
    );
    let tree_bytes = run_git_with_index(repo_root, &tmp.path, &["write-tree"])?;
    let sha = String::from_utf8_lossy(&tree_bytes).trim().to_owned();
    if sha.len() < 40 {
        return Err(format!("unexpected git write-tree output: {sha}"));
    }
    Ok(sha)
}

pub(super) fn capture(
    repo_root: &Path,
    session_id: &str,
    first_seq: u64,
) -> Result<CheckpointPayload, String> {
    let tree_sha = write_transient_tree(repo_root)?;
    let ref_name = format!(
        "refs/slim/checkpoints/{}/{}",
        sanitize_ref_component(session_id),
        first_seq
    );
    run_git(repo_root, &["update-ref", &ref_name, &tree_sha])?;
    Ok(CheckpointPayload::GitShadow {
        repo_root: repo_root.display().to_string(),
        tree_sha,
        ref_name,
    })
}

fn diff_trees(
    repo_root: &Path,
    from_tree: &str,
    to_tree: &str,
) -> Result<(Vec<String>, Vec<String>), String> {
    let raw = run_git(
        repo_root,
        &["diff-tree", "-r", "--name-status", "--no-renames", "-z", from_tree, to_tree],
    )?;
    let mut restored = Vec::new();
    let mut removed = Vec::new();
    let mut tokens = raw.split(|b| *b == 0);
    while let Some(status_bytes) = tokens.next() {
        if status_bytes.is_empty() {
            continue;
        }
        let Some(path_bytes) = tokens.next() else {
            break;
        };
        let status = String::from_utf8_lossy(status_bytes);
        let rel = String::from_utf8_lossy(path_bytes).into_owned();
        if !is_safe_workspace_rel_path(&rel) {
            continue;
        }
        match status.chars().next() {
            Some('D') => removed.push(rel),
            Some('A' | 'M' | 'T') => restored.push(rel),
            _ => {}
        }
    }
    Ok((restored, removed))
}

pub(super) fn preview(repo_root: &Path, target_tree: &str) -> Result<RestoreSummary, String> {
    let cur_tree = write_transient_tree(repo_root)?;
    let (restored_files, removed_files) = diff_trees(repo_root, &cur_tree, target_tree)?;
    Ok(RestoreSummary {
        restored_files,
        removed_files,
        undo_checkpoint: None,
    })
}

pub(super) fn restore(
    repo_root: &Path,
    session_id: &str,
    undo_seq: u64,
    target_tree: &str,
) -> Result<RestoreSummary, String> {
    let undo_ref = format!(
        "refs/slim/checkpoints/{}/undo-{}",
        sanitize_ref_component(session_id),
        undo_seq
    );
    let undo_tree = write_transient_tree(repo_root)?;
    run_git(repo_root, &["update-ref", &undo_ref, &undo_tree])?;
    let undo_checkpoint = TurnCheckpoint {
        version: CHECKPOINT_VERSION,
        first_seq: undo_seq,
        created_at_ms: now_ms(),
        cwd: repo_root.display().to_string(),
        payload: CheckpointPayload::GitShadow {
            repo_root: repo_root.display().to_string(),
            tree_sha: undo_tree.clone(),
            ref_name: undo_ref,
        },
    };

    let (restored_files, removed_files) = diff_trees(repo_root, &undo_tree, target_tree)?;
    for rel in &removed_files {
        remove_file_and_empty_parents(repo_root, rel)?;
    }

    if !restored_files.is_empty() {
        let tmp = TempIndex::new("restore")?;
        run_git_with_index(repo_root, &tmp.path, &["read-tree", target_tree])?;
        for chunk in restored_files.chunks(200) {
            let mut cmd = Command::new("git");
            cmd.args(["checkout-index", "-f", "--"])
                .args(chunk)
                .current_dir(repo_root)
                .env("GIT_INDEX_FILE", &tmp.path)
                .stdin(Stdio::null());
            let out = cmd
                .output()
                .map_err(|e| format!("git checkout-index failed: {e}"))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                return Err(if stderr.is_empty() {
                    format!("git checkout-index exited with {}", out.status)
                } else {
                    stderr
                });
            }
        }
    }

    Ok(RestoreSummary {
        restored_files,
        removed_files,
        undo_checkpoint: Some(undo_checkpoint),
    })
}

pub(super) fn prune(repo_root: &Path, keep_latest: usize) {
    let Ok(out) = run_git(
        repo_root,
        &[
            "for-each-ref",
            "--sort=-creatordate",
            "--format=%(refname)",
            "refs/slim/checkpoints",
        ],
    ) else {
        return;
    };
    let refs: Vec<&str> = std::str::from_utf8(&out)
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for old_ref in refs.into_iter().skip(keep_latest) {
        let _ = run_git(repo_root, &["update-ref", "-d", old_ref]);
    }
}
