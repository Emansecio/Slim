use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use slim_core::runtime::CancellationToken;
use slim_core::tools::{
    apply_exact_patch, read_file, read_file_range, run_shell_timeout,
    run_shell_timeout_cancellable, run_shell_timeout_cancellable_with_progress, search_literal,
    write_file, FilePrecondition, ToolError, ToolRegistry,
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
    assert!(
        matches!(stale, Err(ToolError::StaleRead { .. })),
        "unexpected stale-write result: {stale:?}"
    );
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
fn exact_patch_rejects_files_above_the_mutation_budget() {
    let path = temp_path("oversized-patch.txt");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::File::create(&path)
        .expect("create")
        .set_len(10 * 1024 * 1024 + 1)
        .expect("extend");

    let result = apply_exact_patch(&path, "before", "after");

    assert!(matches!(
        result,
        Err(ToolError::InvalidInput { message })
            if message == "file exceeds the 10485760-byte mutation safety limit"
    ));
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn write_rejects_content_above_the_mutation_budget() {
    let path = temp_path("oversized-write.txt");

    let result = write_file(&path, &"x".repeat(10 * 1024 * 1024 + 1), None);

    assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
    assert!(!path.exists());
}

#[cfg(windows)]
#[test]
#[allow(clippy::permissions_set_readonly_false)]
fn failed_guarded_write_leaves_no_temporary_file() {
    let path = temp_path("readonly-replace.txt");
    let parent = path.parent().expect("parent");
    fs::create_dir_all(parent).expect("mkdir");
    fs::write(&path, "before").expect("write target");
    let mut permissions = fs::metadata(&path).expect("metadata").permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).expect("readonly");

    let result = write_file(
        &path,
        "after",
        Some(FilePrecondition::ExactText("before".into())),
    );

    assert!(result.is_err(), "read-only replacement must fail");
    let residual = fs::read_dir(parent)
        .expect("read dir")
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains(".slim-"));
    let mut permissions = fs::metadata(&path).expect("metadata").permissions();
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).expect("writable");
    let _ = fs::remove_dir_all(parent);
    assert!(!residual, "failed replacement left a .slim-*.tmp file");
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
fn search_bounds_each_matching_line() {
    let root = temp_path("bounded-search-line");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(
        root.join("large.txt"),
        format!("needle{}", "x".repeat(20_000)),
    )
    .expect("write");

    let hits = search_literal(&root, "needle").expect("search");

    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.len() <= 8 * 1024);
    assert!(hits[0].text.contains("[truncated "));
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn list_tool_paginates_directory_entries() {
    let root = temp_path("list-page");
    fs::create_dir_all(&root).expect("mkdir");
    for index in 0..12 {
        fs::write(root.join(format!("file-{index:02}.txt")), "").expect("entry");
    }
    let registry = ToolRegistry::default();
    let result = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        r#"{"path":".","max_entries":5,"offset":1}"#,
    );

    assert!(result.success);
    assert_eq!(
        result
            .output
            .lines()
            .filter(|line| line.ends_with(".txt"))
            .count(),
        5
    );
    let marker = "pass \"cursor\": \"";
    let cursor_start = result.output.find(marker).expect("cursor") + marker.len();
    let cursor_tail = &result.output[cursor_start..];
    let cursor = &cursor_tail[..cursor_tail.find('"').expect("cursor end")];
    let second = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        &serde_json::json!({"path": ".", "max_entries": 5, "cursor": cursor}).to_string(),
    );
    assert!(second.success, "{}", second.output);
    assert_eq!(
        second
            .output
            .lines()
            .filter(|line| line.ends_with(".txt"))
            .count(),
        5
    );
    assert!(result.output.contains("[showing entries 1-5 of 12"));
    assert!(result.output.contains(r#""cursor": "list-"#));
    let next_start = second.output.find(marker).expect("second cursor") + marker.len();
    let next_tail = &second.output[next_start..];
    let next_cursor = &next_tail[..next_tail.find('"').expect("second cursor end")];
    fs::write(root.join("added.txt"), "").expect("mutate directory");
    let third = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        &serde_json::json!({"path": ".", "max_entries": 5, "cursor": next_cursor}).to_string(),
    );
    assert!(third.success, "{}", third.output);
    assert_eq!(
        third
            .output
            .lines()
            .filter(|line| line.ends_with(".txt"))
            .count(),
        2
    );
    assert!(third.output.contains("file-10.txt"));
    assert!(third.output.contains("file-11.txt"));
    assert!(!third.output.contains("added.txt"));

    fs::create_dir(root.join("other")).expect("other directory");
    let mismatched = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        &serde_json::json!({"path": "other", "max_entries": 5, "cursor": cursor}).to_string(),
    );
    assert!(!mismatched.success);
    assert!(mismatched.output.contains("does not match this path"));

    let refreshed = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        r#"{"path":".","max_entries":500}"#,
    );
    assert!(refreshed.success, "{}", refreshed.output);
    assert!(refreshed.output.contains("added.txt"));
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn search_coerces_empty_or_duplicate_query_fields() {
    let root = temp_path("search-coerce");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("hit.txt"), "needle in hay").expect("write");
    let registry = ToolRegistry::default();

    let both = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"path":".","query":"needle","patterns":["needle"]}"#,
    );
    assert!(both.success, "{}", both.output);
    assert!(both.output.contains("needle"), "{}", both.output);

    let empty_query = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"path":".","query":"","patterns":["needle"]}"#,
    );
    assert!(empty_query.success, "{}", empty_query.output);
    assert!(
        empty_query.output.contains("needle"),
        "{}",
        empty_query.output
    );

    let neither = registry.execute(OperatingMode::ReadOnly, &root, "search", r#"{"path":"."}"#);
    assert!(!neither.success, "{}", neither.output);
    assert!(
        neither
            .output
            .contains("search requires exactly one of query or patterns"),
        "{}",
        neither.output
    );
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn list_blank_path_and_blank_cursor_list_workspace() {
    let root = temp_path("list-blank");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("visible.txt"), "").expect("entry");
    let registry = ToolRegistry::default();

    let empty_path = registry.execute(OperatingMode::ReadOnly, &root, "list", r#"{"path":""}"#);
    assert!(empty_path.success, "{}", empty_path.output);
    assert!(
        empty_path.output.contains("visible.txt"),
        "blank path must list the workspace: {}",
        empty_path.output
    );

    let empty_cursor = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        r#"{"path":".","cursor":""}"#,
    );
    assert!(empty_cursor.success, "{}", empty_cursor.output);
    assert!(
        !empty_cursor.output.contains("expired"),
        "blank cursor must start a new list: {}",
        empty_cursor.output
    );

    let invalid = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        r#"{"path":".","cursor":"not-a-cursor"}"#,
    );
    assert!(!invalid.success, "{}", invalid.output);
    assert!(
        invalid.output.contains("list cursor is invalid"),
        "malformed cursor must not look expired: {}",
        invalid.output
    );
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
fn shell_reports_bounded_progress_before_completion() {
    let root = temp_path("shell-progress");
    fs::create_dir_all(&root).expect("mkdir");
    let release = root.join("release.txt");
    let escaped_release = release.display().to_string().replace('\'', "''");
    let command = format!(
        "Write-Output 'working'; while (-not (Test-Path -LiteralPath '{escaped_release}')) {{ Start-Sleep -Milliseconds 10 }}"
    );
    let mut progress = Vec::new();
    let result = run_shell_timeout_cancellable_with_progress(
        &root,
        &command,
        Duration::from_secs(5),
        None,
        |snapshot| {
            progress.push(snapshot);
            fs::write(&release, b"continue").expect("release shell");
        },
    )
    .expect("shell");

    assert!(result.output.status.success());
    assert!(!progress.is_empty(), "a long shell must report liveness");
    assert!(progress[0].elapsed_ms >= 900);
    assert!(progress[0].stdout_bytes > 0);
    assert_eq!(progress[0].last_line, "working");
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn shell_cancellation_kills_process_tree_before_late_mutation() {
    let root = temp_path("cancel-shell");
    fs::create_dir_all(&root).expect("mkdir");
    let started_marker = root.join("started.txt");
    let marker = root.join("late.txt");
    let command = format!(
        "Set-Content -LiteralPath '{}' -Value started; Start-Sleep -Seconds 30; Set-Content -LiteralPath '{}' -Value late",
        started_marker.display(),
        marker.display(),
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started_marker.exists() {
        assert!(
            Instant::now() < deadline,
            "shell did not start before cancellation deadline"
        );
        thread::sleep(Duration::from_millis(10));
    }
    cancellation.cancel();
    let result = worker.join().expect("worker");
    assert!(result.cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    thread::sleep(Duration::from_millis(100));
    assert!(!marker.exists());
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn cancellation_gate_blocks_list_write_and_patch_before_work() {
    let root = temp_path("cancel-mutating-tools");
    fs::create_dir_all(&root).expect("mkdir");
    fs::create_dir(root.join("empty")).expect("empty directory");
    let write_path = root.join("write.txt");
    let patch_path = root.join("patch.txt");
    fs::write(&patch_path, "before\n").expect("seed");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let registry = ToolRegistry::default();

    let list_result = registry.execute_with_cancellation(
        OperatingMode::ReadOnly,
        &root,
        "list",
        r#"{"path":"empty"}"#,
        Some(&cancellation),
    );

    let write_result = registry.execute_with_cancellation(
        OperatingMode::Auto,
        &root,
        "write",
        &serde_json::json!({
            "path": write_path,
            "content": "must not be written"
        })
        .to_string(),
        Some(&cancellation),
    );
    let patch_result = registry.execute_with_cancellation(
        OperatingMode::Auto,
        &root,
        "patch",
        &serde_json::json!({
            "path": patch_path,
            "expected": "before\n",
            "replacement": "must not be patched\n"
        })
        .to_string(),
        Some(&cancellation),
    );

    assert!(!list_result.success);
    assert!(!write_result.success);
    assert!(!patch_result.success);
    assert!(!write_path.exists());
    assert_eq!(
        fs::read_to_string(&patch_path).expect("patched file"),
        "before\n"
    );
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
    assert!(first_page.contains("[showing lines 1-4; more content available"));
    assert!(first_page.contains("\"offset\": 5"));

    let second_page = read_file_range(&path, 5, 4).expect("second page");
    assert!(second_page.contains("5: line-5\n"));
    assert!(second_page.contains("[showing lines 5-8; more content available"));

    let last_page = read_file_range(&path, 9, 4).expect("last page");
    assert_eq!(
        last_page, "9: line-9\n10: line-10\n",
        "final page carries no footer"
    );

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn read_range_matches_lines_semantics_for_crlf_and_past_eof() {
    let path = temp_path("paged-crlf.txt");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, "alpha\r\nbeta\r\ngamma").expect("write");

    assert_eq!(
        read_file_range(&path, 2, 1).expect("middle page"),
        "2: beta\n\n[showing lines 2-2; more content available; pass \"offset\": 3 for the next page]"
    );
    assert_eq!(read_file_range(&path, 99, 10).expect("past eof"), "");

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn read_rejects_a_page_that_exceeds_the_byte_budget() {
    let path = temp_path("oversized-read-page.txt");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, "x".repeat(1024 * 1024 + 1)).expect("write");

    let result = read_file_range(&path, 1, 1);

    assert!(matches!(
        result,
        Err(ToolError::InvalidInput { message })
            if message == "read page exceeds the 1048576-byte safety limit"
    ));
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn shell_caps_raw_streams_while_draining_the_remainder() {
    const CAPTURED_BYTES: usize = 8 * 1024 * 1024;
    const PRODUCED_BYTES: usize = 8196 * 1024;
    const STDOUT_HEAD: &str = "STDOUT_HEAD";
    const STDOUT_MIDDLE: &str = "STDOUT_MIDDLE";
    const STDOUT_TAIL: &str = "STDOUT_TAIL";
    const STDERR_HEAD: &str = "STDERR_HEAD";
    const STDERR_MIDDLE: &str = "STDERR_MIDDLE";
    const STDERR_TAIL: &str = "STDERR_TAIL";
    let result = run_shell_timeout(
        std::env::temp_dir(),
        "$size = 8196 * 1024; $oh = 'STDOUT_HEAD'; $om = 'STDOUT_MIDDLE'; $ot = 'STDOUT_TAIL'; $eh = 'STDERR_HEAD'; $em = 'STDERR_MIDDLE'; $et = 'STDERR_TAIL'; $ob = $size - $oh.Length - $om.Length - $ot.Length; $eb = $size - $eh.Length - $em.Length - $et.Length; $ol = [int]($ob / 2); $el = [int]($eb / 2); [Console]::Out.Write($oh + ('x' * $ol) + $om + ('x' * ($ob - $ol)) + $ot); [Console]::Error.Write($eh + ('y' * $el) + $em + ('y' * ($eb - $el)) + $et)",
        Duration::from_secs(20),
    )
    .expect("shell");

    assert!(!result.timed_out, "discarded bytes must still be drained");
    assert_eq!(
        result.output.stdout.len(),
        CAPTURED_BYTES,
        "produced {PRODUCED_BYTES} bytes"
    );
    assert_eq!(
        result.output.stderr.len(),
        CAPTURED_BYTES,
        "produced {PRODUCED_BYTES} bytes"
    );
    assert_eq!(
        result.stdout_discarded_bytes,
        PRODUCED_BYTES - CAPTURED_BYTES
    );
    assert_eq!(
        result.stderr_discarded_bytes,
        PRODUCED_BYTES - CAPTURED_BYTES
    );
    assert!(result.output.stdout.starts_with(STDOUT_HEAD.as_bytes()));
    assert!(result.output.stdout.ends_with(STDOUT_TAIL.as_bytes()));
    assert!(!result
        .output
        .stdout
        .windows(STDOUT_MIDDLE.len())
        .any(|window| window == STDOUT_MIDDLE.as_bytes()));
    assert!(result.output.stderr.starts_with(STDERR_HEAD.as_bytes()));
    assert!(result.output.stderr.ends_with(STDERR_TAIL.as_bytes()));
    assert!(!result
        .output
        .stderr
        .windows(STDERR_MIDDLE.len())
        .any(|window| window == STDERR_MIDDLE.as_bytes()));
}

#[test]
fn shell_marker_counts_raw_and_context_discarded_bytes() {
    const PRODUCED_BYTES: usize = 8196 * 1024;
    const RETAINED_CONTEXT_BYTES: usize = 8 * 1024;
    const RETAINED_HEAD_BYTES: usize = RETAINED_CONTEXT_BYTES / 2;
    const RETAINED_TAIL_BYTES: usize = RETAINED_CONTEXT_BYTES / 2;
    const STDOUT_HEAD: &str = "STDOUT_HEAD";
    const STDOUT_MIDDLE: &str = "STDOUT_MIDDLE";
    const STDOUT_TAIL: &str = "STDOUT_TAIL";
    let registry = ToolRegistry::default();
    let arguments = serde_json::json!({
        "command": "$size = 8196 * 1024; $head = 'STDOUT_HEAD'; $middle = 'STDOUT_MIDDLE'; $tail = 'STDOUT_TAIL'; $body = $size - $head.Length - $middle.Length - $tail.Length; $left = [int]($body / 2); [Console]::Out.Write($head + ('x' * $left) + $middle + ('x' * ($body - $left)) + $tail)",
        "timeout_ms": 20_000
    })
    .to_string();

    let result = registry.execute(
        OperatingMode::Auto,
        std::env::temp_dir(),
        "shell",
        &arguments,
    );

    assert!(result.success);
    let marker = format!(
        "[truncated {} bytes; kept first {} and last {} bytes of this stream]",
        PRODUCED_BYTES - RETAINED_CONTEXT_BYTES,
        RETAINED_HEAD_BYTES,
        RETAINED_TAIL_BYTES
    );
    let head = result.output.find(STDOUT_HEAD).expect("stdout head");
    let marker = result.output.find(&marker).expect("truncation marker");
    let tail = result.output.find(STDOUT_TAIL).expect("stdout tail");
    assert!(head < marker && marker < tail);
    assert!(!result.output.contains(STDOUT_MIDDLE));
}

#[test]
fn shell_result_header_is_humanized_for_model_context() {
    let registry = ToolRegistry::default();
    let arguments = serde_json::json!({
        "command": "exit 0",
        "timeout_ms": 5_000
    })
    .to_string();

    let result = registry.execute(
        OperatingMode::Auto,
        std::env::temp_dir(),
        "shell",
        &arguments,
    );

    assert!(result.success);
    assert!(result.output.starts_with("exit 0\nstdout:"));
    assert!(!result.output.contains("exit_code="));
    assert!(!result.output.contains("Some(0)"));
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
    assert!(write.success, "write failed: {}", write.output);
    assert_eq!(fs::read_to_string(&file).expect("read"), "after\n");

    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn registry_write_invalidates_read_checkpoints_when_metadata_version_repeats() {
    let root = temp_path("read-cache-invalidation");
    fs::create_dir_all(&root).expect("workspace");
    let file = root.join("fixture.txt");
    let before = std::iter::repeat_n("aaaa", 512)
        .collect::<Vec<_>>()
        .join("\n");
    let after = std::iter::repeat_n("aaaaa".to_owned(), 256)
        .chain((0..256).map(|index| format!("{index:03}")))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(before.len(), after.len());
    fs::write(&file, &before).expect("fixture");
    let original_modified = fs::metadata(&file)
        .expect("metadata")
        .modified()
        .expect("modified");
    let registry = ToolRegistry::default();

    let first = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "read",
        r#"{"path":"fixture.txt","offset":300,"max_lines":1}"#,
    );
    assert!(first.output.starts_with("300: aaaa"));
    let write = registry.execute(
        OperatingMode::Auto,
        &root,
        "write",
        &serde_json::json!({
            "path": "fixture.txt",
            "content": after,
            "expected": before,
        })
        .to_string(),
    );
    assert!(write.success, "{}", write.output);
    fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .expect("open timestamp")
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .expect("restore timestamp");

    let second = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "read",
        r#"{"path":"fixture.txt","offset":300,"max_lines":1}"#,
    );
    assert!(second.output.starts_with("300: 043"), "{}", second.output);

    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn registry_rejects_paths_outside_workspace_for_every_file_tool() {
    let root = temp_path("workspace");
    let parent = root.parent().expect("parent");
    fs::create_dir_all(&root).expect("workspace");
    let outside = parent.join("outside.txt");
    fs::write(&outside, "outside secret\n").expect("outside fixture");
    let registry = ToolRegistry::default();

    let absolute_read = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "read",
        &serde_json::json!({"path": outside}).to_string(),
    );
    assert!(!absolute_read.success);
    assert!(!absolute_read.output.contains("outside secret"));

    for (name, arguments) in [
        ("read", r#"{"path":"../outside.txt"}"#),
        ("list", r#"{"path":".."}"#),
        ("search", r#"{"path":"..","query":"outside secret"}"#),
        (
            "patch",
            r#"{"path":"../outside.txt","expected":"outside secret\n","replacement":"changed\n"}"#,
        ),
    ] {
        let result = registry.execute(OperatingMode::Auto, &root, name, arguments);
        assert!(!result.success, "{name} escaped workspace");
    }
    let write = registry.execute(
        OperatingMode::Auto,
        &root,
        "write",
        r#"{"path":"../created.txt","content":"created\n"}"#,
    );
    assert!(!write.success);
    assert!(!parent.join("created.txt").exists());
    assert_eq!(
        fs::read_to_string(&outside).expect("outside"),
        "outside secret\n"
    );

    let _ = fs::remove_dir_all(parent);
}
