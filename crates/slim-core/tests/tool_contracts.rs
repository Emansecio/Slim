use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use slim_core::runtime::CancellationToken;
use slim_core::tools::{
    apply_exact_patch, read_file, read_file_range, run_shell_timeout,
    run_shell_timeout_cancellable, search_literal, write_file, FilePrecondition, ToolError,
    ToolRegistry,
};
use slim_core::OperatingMode;

fn temp_path(name: &str) -> PathBuf {
    let unique = format!(
        "slim-tools-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    std::env::temp_dir().join(unique).join(name)
}

#[test]
fn read_is_numbered_and_stale_write_never_mutates() {
    let path = temp_path("file.txt");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, "alpha\nbeta\n").expect("write");

    assert_eq!(read_file(&path, 10).expect("read"), "1: alpha\n2: beta\n");
    let stale = write_file(
        &path,
        "changed\n",
        Some(FilePrecondition::ExactText("other\n".into())),
    );
    assert!(matches!(stale, Err(ToolError::StaleRead { .. })));
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        "alpha\nbeta\n"
    );

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn exact_patch_rejects_ambiguous_matches_without_writing() {
    let path = temp_path("patch.txt");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, "same\nsame\n").expect("write");

    let result = apply_exact_patch(&path, "same\n", "new\n");
    assert!(matches!(result, Err(ToolError::MatchCount { count: 2 })));
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        "same\nsame\n"
    );

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn literal_search_returns_matching_paths_and_lines() {
    let root = temp_path("tree");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("a.txt"), "hello\nworld\n").expect("write");
    let hits = search_literal(&root, "world").expect("search");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].path.ends_with("a.txt"));
    assert_eq!(hits[0].line, 2);
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn shell_timeout_terminates_long_running_processes() {
    let result = run_shell_timeout(
        std::env::temp_dir(),
        "Start-Sleep -Milliseconds 250",
        Duration::from_millis(25),
    )
    .expect("shell");
    assert!(result.timed_out);
}

#[test]
fn shell_cancellation_kills_process_tree_before_late_mutation() {
    let root = temp_path("cancel-shell");
    fs::create_dir_all(&root).expect("mkdir");
    let marker = root.join("late.txt");
    let command = format!(
        "Start-Sleep -Seconds 30; Set-Content -LiteralPath '{}' -Value late",
        marker.display()
    );
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker_root = root.clone();
    let started = Instant::now();
    let worker = thread::spawn(move || {
        run_shell_timeout_cancellable(
            worker_root,
            &command,
            Duration::from_secs(60),
            Some(&worker_cancellation),
        )
        .expect("shell")
    });
    thread::sleep(Duration::from_millis(250));
    cancellation.cancel();
    let result = worker.join().expect("worker");
    assert!(result.cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    thread::sleep(Duration::from_millis(100));
    assert!(!marker.exists());
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn read_range_paginates_and_appends_offset_footer_when_truncated() {
    let path = temp_path("paged.txt");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let body = (1..=10)
        .map(|line| format!("line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, &body).expect("write");

    let first_page = read_file_range(&path, 1, 4).expect("first page");
    assert!(first_page.starts_with("1: line-1\n2: line-2\n3: line-3\n4: line-4\n"));
    assert!(first_page.contains("[showing lines 1-4 of 10"));
    assert!(first_page.contains("\"offset\": 5"));

    let second_page = read_file_range(&path, 5, 4).expect("second page");
    assert!(second_page.contains("5: line-5\n"));
    assert!(second_page.contains("[showing lines 5-8 of 10"));

    let last_page = read_file_range(&path, 9, 4).expect("last page");
    assert_eq!(
        last_page,
        "9: line-9\n10: line-10\n",
        "final page carries no footer"
    );

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn shell_drains_large_stdout_and_stderr_while_child_runs() {
    const STREAM_BYTES: usize = 5 * 1024 * 1024;
    let result = run_shell_timeout(
        std::env::temp_dir(),
        "$chunk = 'x' * 1024; 1..5120 | ForEach-Object { [Console]::Out.Write($chunk); [Console]::Error.Write($chunk) }",
        Duration::from_secs(10),
    )
    .expect("shell");

    assert!(!result.timed_out, "full pipes must not block the child");
    assert_eq!(result.output.stdout.len(), STREAM_BYTES);
    assert_eq!(result.output.stderr.len(), STREAM_BYTES);
}

#[test]
fn shell_stream_above_cap_is_truncated_with_marker() {
    let result = run_shell_timeout(
        std::env::temp_dir(),
        "Write-Output ('x' * 20000)",
        Duration::from_secs(30),
    )
    .expect("shell");
    let output = String::from_utf8_lossy(&result.output.stdout).to_string();
    assert!(
        output.starts_with(&"x".repeat(100)) && output.len() >= 20000,
        "raw stdout must be uncapped at the shell layer"
    );

    let registry = ToolRegistry::default();
    let tool_result = registry.execute(
        OperatingMode::Auto,
        std::env::temp_dir(),
        "shell",
        r#"{"command":"Write-Output ('x' * 20000)"}"#,
    );
    assert!(tool_result.success);
    assert!(tool_result.output.contains("[truncated "));
    assert!(tool_result.output.len() < 20000);
}

#[test]
fn registry_executes_json_tools_and_blocks_mutations_outside_auto() {
    let root = temp_path("registry");
    fs::create_dir_all(&root).expect("mkdir");
    let file = root.join("file.txt");
    fs::write(&file, "before\n").expect("write");
    let registry = ToolRegistry::default();

    let read = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "read",
        r#"{"path":"file.txt","max_lines":10}"#,
    );
    assert!(read.success);
    assert_eq!(read.output, "1: before\n");

    let blocked = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "write",
        r#"{"path":"file.txt","content":"after\n","expected":"before\n"}"#,
    );
    assert!(!blocked.success);
    assert!(blocked.output.contains("unavailable"));
    assert_eq!(fs::read_to_string(&file).expect("read"), "before\n");

    let write = registry.execute(
        OperatingMode::Auto,
        &root,
        "write",
        r#"{"path":"file.txt","content":"after\n","expected":"before\n"}"#,
    );
    assert!(write.success);
    assert_eq!(fs::read_to_string(&file).expect("read"), "after\n");

    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}
