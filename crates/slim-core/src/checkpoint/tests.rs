use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use super::*;

pub(crate) static SLIM_HOME_LOCK: Mutex<()> = Mutex::new(());

fn lock_slim_home(path: &Path) -> MutexGuard<'static, ()> {
    let guard = SLIM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("SLIM_HOME", path);
    guard
}

fn run_git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("git invocation");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn git_shadow_capture_restore_and_undo_preserves_git_status_and_ignored_files() {
    let root = std::env::temp_dir().join(format!("slim-cp-git-{}", now_ms()));
    let slim_home = root.join(".slim-home");
    let repo = root.join("repo");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join(".slim")).unwrap();

    run_git_ok(&repo, &["init"]);
    run_git_ok(&repo, &["config", "user.name", "Slim Test"]);
    run_git_ok(&repo, &["config", "user.email", "slim@example.com"]);
    std::fs::write(repo.join(".gitignore"), "ignored.log\n").unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() { println!(\"v1\"); }\n").unwrap();
    std::fs::write(repo.join("src/keep.rs"), "pub fn keep() {}\n").unwrap();
    std::fs::write(repo.join("src/remove_me_later.rs"), "pub fn temp() {}\n").unwrap();
    run_git_ok(&repo, &["add", ".gitignore", "src"]);
    run_git_ok(&repo, &["commit", "-m", "initial"]);

    // Dirty tracked edit + ignored file + internal `.slim/session.jsonl`.
    std::fs::write(repo.join("src/main.rs"), "fn main() { println!(\"v2\"); }\n").unwrap();
    std::fs::write(repo.join("ignored.log"), "secret-ignored-data").unwrap();
    std::fs::write(repo.join(".slim/session.jsonl"), "keep-session-intact").unwrap();

    let _guard = lock_slim_home(&slim_home);
    let keep_mtime_before = std::fs::metadata(repo.join("src/keep.rs"))
        .unwrap()
        .modified()
        .unwrap();

    let cp = capture_turn_checkpoint(&repo, "tui-1", 10).expect("capture git shadow");
    assert!(matches!(cp.payload, CheckpointPayload::GitShadow { .. }));

    // Simulate an agent turn making destructive changes.
    std::fs::write(repo.join("src/main.rs"), "broken syntax").unwrap();
    std::fs::remove_file(repo.join("src/remove_me_later.rs")).unwrap();
    std::fs::create_dir_all(repo.join("src/nested")).unwrap();
    std::fs::write(repo.join("src/nested/new_untracked.rs"), "// extra").unwrap();
    std::fs::write(repo.join(".slim/session.jsonl"), "keep-session-updated").unwrap();

    // Preview must not touch disk.
    let preview = preview_restore(&repo, &cp).expect("preview");
    assert!(preview.restored_files.contains(&"src/main.rs".to_owned()));
    assert!(preview
        .restored_files
        .contains(&"src/remove_me_later.rs".to_owned()));
    assert!(preview
        .removed_files
        .contains(&"src/nested/new_untracked.rs".to_owned()));
    assert_eq!(std::fs::read_to_string(repo.join("src/main.rs")).unwrap(), "broken syntax");

    // Restore back to checkpoint.
    let summary = restore_turn_checkpoint(&repo, "tui-1", &cp).expect("restore");
    assert!(summary.undo_checkpoint.is_some());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "fn main() { println!(\"v2\"); }\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/remove_me_later.rs")).unwrap(),
        "pub fn temp() {}\n"
    );
    assert!(!repo.join("src/nested/new_untracked.rs").exists());
    assert!(!repo.join("src/nested").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("ignored.log")).unwrap(),
        "secret-ignored-data"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join(".slim/session.jsonl")).unwrap(),
        "keep-session-updated"
    );
    assert_eq!(
        std::fs::metadata(repo.join("src/keep.rs"))
            .unwrap()
            .modified()
            .unwrap(),
        keep_mtime_before,
        "untouched file mtime must be preserved"
    );

    // Git stash list must remain empty; HEAD untouched.
    let stash = Command::new("git")
        .args(["stash", "list"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(stash.stdout.is_empty());

    // Undo rewind brings back post-turn state!
    let undo_cp = summary.undo_checkpoint.unwrap();
    let _ = restore_turn_checkpoint(&repo, "tui-1", &undo_cp).expect("undo");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "broken syntax"
    );
    assert!(!repo.join("src/remove_me_later.rs").exists());
    assert!(repo.join("src/nested/new_untracked.rs").exists());

    std::env::remove_var("SLIM_HOME");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn file_backup_capture_restore_and_undo_works_outside_git() {
    let root = std::env::temp_dir().join(format!("slim-cp-nogit-{}", now_ms()));
    let slim_home = root.join(".slim-home");
    let work = root.join("workspace");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(work.join("sub")).unwrap();
    std::fs::write(work.join("sub/a.txt"), "hello-v1").unwrap();
    std::fs::write(work.join("b.txt"), "stay").unwrap();

    let _guard = lock_slim_home(&slim_home);
    let cp = capture_turn_checkpoint(&work, "sess-ng", 3).expect("capture non-git");
    assert!(matches!(cp.payload, CheckpointPayload::FileBackup { .. }));

    std::fs::write(work.join("sub/a.txt"), "mutated-v2").unwrap();
    std::fs::write(work.join("added.txt"), "created").unwrap();
    std::fs::remove_file(work.join("b.txt")).unwrap();

    let summary = restore_turn_checkpoint(&work, "sess-ng", &cp).expect("restore non-git");
    assert_eq!(std::fs::read_to_string(work.join("sub/a.txt")).unwrap(), "hello-v1");
    assert_eq!(std::fs::read_to_string(work.join("b.txt")).unwrap(), "stay");
    assert!(!work.join("added.txt").exists());

    let undo = summary.undo_checkpoint.unwrap();
    let _ = restore_turn_checkpoint(&work, "sess-ng", &undo).expect("undo non-git");
    assert_eq!(std::fs::read_to_string(work.join("sub/a.txt")).unwrap(), "mutated-v2");
    assert!(work.join("added.txt").exists());
    assert!(!work.join("b.txt").exists());

    std::env::remove_var("SLIM_HOME");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn subdir_inside_parent_git_repo_uses_scoped_file_backup_not_parent_git_shadow() {
    let root = std::env::temp_dir().join(format!("slim-cp-subdir-{}", now_ms()));
    let slim_home = root.join(".slim-home");
    let parent_repo = root.join("documents");
    let project_dir = parent_repo.join("desconto-pro");
    let sibling_dir = parent_repo.join("unrelated-sibling");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::create_dir_all(&sibling_dir).unwrap();

    run_git_ok(&parent_repo, &["init"]);
    run_git_ok(&parent_repo, &["config", "user.name", "Slim Test"]);
    run_git_ok(&parent_repo, &["config", "user.email", "slim@example.com"]);
    std::fs::write(parent_repo.join("root.txt"), "root").unwrap();
    run_git_ok(&parent_repo, &["add", "root.txt"]);
    run_git_ok(&parent_repo, &["commit", "-m", "initial"]);

    std::fs::write(project_dir.join("app.txt"), "project-v1").unwrap();
    std::fs::write(sibling_dir.join("huge.bin"), "sibling-untouched").unwrap();

    let _guard = lock_slim_home(&slim_home);
    let cp = capture_turn_checkpoint(&project_dir, "sess-sub", 1).expect("capture subdir");
    assert!(
        matches!(cp.payload, CheckpointPayload::FileBackup { .. }),
        "child folder inside ancestor git repo must not hijack parent repo via git_shadow"
    );

    // Parent repo must have no refs/slim/checkpoints created.
    let refs = Command::new("git")
        .args(["for-each-ref", "refs/slim/checkpoints"])
        .current_dir(&parent_repo)
        .output()
        .unwrap();
    assert!(refs.stdout.is_empty());

    // Restore only touches files inside `project_dir`, never `sibling_dir`.
    std::fs::write(project_dir.join("app.txt"), "project-v2").unwrap();
    std::fs::write(sibling_dir.join("huge.bin"), "sibling-modified").unwrap();
    let _ = restore_turn_checkpoint(&project_dir, "sess-sub", &cp).expect("restore subdir");
    assert_eq!(std::fs::read_to_string(project_dir.join("app.txt")).unwrap(), "project-v1");
    assert_eq!(std::fs::read_to_string(sibling_dir.join("huge.bin")).unwrap(), "sibling-modified");

    std::env::remove_var("SLIM_HOME");
    let _ = std::fs::remove_dir_all(&root);
}
