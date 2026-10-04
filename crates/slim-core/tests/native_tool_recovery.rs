use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::provider::{
    AnthropicAdapter, ClinePassAdapter, CommandCodeAdapter, OpenAiCodexAdapter,
    OpenAiCompatibleAdapter, OpenCodeGoAdapter, ProviderAdapter, ProviderConfig, ProviderMessage,
    XaiAdapter,
};
use slim_core::tools::ToolRegistry;
use slim_core::OperatingMode;

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        Self::with_stamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
    }

    fn with_stamp(stamp: u128) -> Self {
        for suffix in 0..u64::MAX {
            let path = std::env::temp_dir().join(format!(
                "slim-native-recovery-{}-{stamp}-{suffix}",
                std::process::id(),
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create exclusive test workspace: {error}"),
            }
        }
        panic!("test workspace suffix exhausted");
    }

    fn call(&self, name: &str, args: Value) -> slim_core::tools::ToolResult {
        ToolRegistry::default().execute(OperatingMode::Auto, &self.0, name, &args.to_string())
    }
}

#[test]
fn same_clock_tick_workspaces_do_not_share_files_or_cleanup() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let first = Workspace::with_stamp(stamp);
    let second = Workspace::with_stamp(stamp);
    let marker = second.0.join("owned-by-second.txt");
    fs::write(&marker, "preserve").unwrap();
    drop(first);
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "preserve",
        "one fixture must never remove another fixture's files"
    );
}

#[test]
fn unicode_recovery_keeps_write_and_patch_errors_recoverable() {
    for content in [
        format!("x{}", "á".repeat(40_000)),
        format!("{}x", "á".repeat(40_000)),
        "€".repeat(30_000),
        format!("x{}", "🦀".repeat(20_000)),
        "a".repeat(80_000),
    ] {
        let root = Workspace::new();
        fs::write(root.0.join("data.txt"), &content).unwrap();
        for (name, args) in [
            (
                "write",
                json!({"path":"data.txt", "content":"new", "expected":"missing"}),
            ),
            (
                "patch",
                json!({"path":"data.txt", "expected":"missing", "replacement":"new"}),
            ),
        ] {
            let result = root.call(name, args);
            assert!(!result.success);
            assert!(result.output.contains("truncated"), "{}", result.output);
            assert!(!result.output.contains("task failed"));
            assert_eq!(
                fs::read_to_string(root.0.join("data.txt")).unwrap(),
                content
            );
        }
    }
}

/// Receipts and error headers name workspace paths relative to the workspace:
/// the canonical absolute form costs tokens on every call and exposes the host.
#[test]
fn receipts_and_errors_use_workspace_relative_paths() {
    let root = Workspace::new();
    let canonical = fs::canonicalize(&root.0).unwrap();
    let absolute = canonical.to_str().unwrap().to_owned();
    fs::create_dir_all(root.0.join("src")).unwrap();
    fs::write(root.0.join("src/data.txt"), "one\ntwo\n").unwrap();
    let shown = std::path::Path::new("src").join("data.txt");
    let shown = shown.to_str().unwrap();
    let outputs = [
        (
            "patch",
            json!({"path":"src/data.txt", "edits":[{"expected":"two", "replacement":"2"}]}),
            format!("patched {shown}:2; replaced 3 bytes with 1 bytes"),
        ),
        (
            "patch",
            json!({"path":"src/data.txt", "edits":[{"expected":"missing", "replacement":"x"}]}),
            format!("expected one match; got 0\n{shown}: file unchanged."),
        ),
        (
            "write",
            json!({"path":"src/data.txt", "content":"x", "expected":"stale"}),
            format!("stale read: {shown}; precondition differs"),
        ),
        (
            "read",
            json!({"path":"src/missing.txt"}),
            format!(
                "{}: file does not exist; use list or search to locate it.",
                std::path::Path::new("src").join("missing.txt").display()
            ),
        ),
        (
            "search",
            json!({"path":"src/nope", "query":"x"}),
            "search root is not a readable directory".to_owned(),
        ),
    ];
    for (name, args, expected) in outputs {
        let result = root.call(name, args);
        assert!(
            result.output.contains(&expected),
            "{name}: {}",
            result.output
        );
        assert!(
            !result.output.contains(&absolute),
            "{name}: {}",
            result.output
        );
    }
}

/// Recovery text is only useful when the model receives it whole: a failure on
/// a file between the result cap and the old 64 KiB recovery cap must not
/// promise the current file ("Do not read again") and then lose its end.
#[test]
fn mutation_recovery_fits_the_default_result_budget() {
    let budget = slim_core::runtime::AgentLoopConfig::default().max_result_bytes;
    // Room for the artifact reference and an admission prefix added later.
    let headroom = 1024;
    let mut content = String::new();
    for index in 0..1000 {
        content.push_str(&format!("pub const VALUE_{index:04}: usize = {index};\n"));
    }
    assert!(content.len() > budget && content.len() < 64 * 1024);
    let root = Workspace::new();
    fs::write(root.0.join("big.rs"), &content).unwrap();
    for (name, args) in [
        ("write", json!({"path":"big.rs", "content":"x\n"})),
        (
            "patch",
            json!({"path":"big.rs", "edits":[{"expected":"VALUE_9999", "replacement":"x"}]}),
        ),
    ] {
        let result = root.call(name, args);
        assert!(!result.success);
        assert!(
            result.output.len() + headroom <= budget,
            "{name}: {} bytes",
            result.output.len()
        );
        assert!(
            result.output.contains("Current file edges are below"),
            "{name}: {}",
            result.output
        );
        assert!(result.output.contains("VALUE_0000"), "{name}");
        assert!(result.output.contains("VALUE_0999"), "{name}");
    }
    let small = "fn a() {}\n".repeat(100);
    fs::write(root.0.join("small.rs"), &small).unwrap();
    let result = root.call(
        "patch",
        json!({"path":"small.rs", "edits":[{"expected":"missing", "replacement":"x"}]}),
    );
    assert!(
        result.output.contains("Current file is below"),
        "{}",
        result.output
    );
    assert!(result.output.ends_with(&small));
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn shell_direct_arguments_are_literal_and_legacy_scripts_stay_available() {
    let root = Workspace::new();
    let program = slim_core::process::ProcessRunner::default()
        .resolve_powershell()
        .unwrap()
        .expect("PowerShell test runtime");
    let script = root.0.join("literal arguments.ps1");
    fs::write(&script, "param([string]$value)\n[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n[Console]::Out.Write($value)\n[Console]::Out.Write(\" stdio=$env:PYTHONIOENCODING\")\nexit 7\n").unwrap();
    let value = "ação with spaces, \"quotes\", $variable & semicolon; literal";
    let result = root.call("shell", json!({
        "command":program, "args":["-NoLogo","-NoProfile","-NonInteractive","-File",script,value]
    }));
    assert!(!result.success);
    assert!(result.output.starts_with("exit 7\n"), "{}", result.output);
    assert!(result.output.contains(value), "{}", result.output);
    let expected_encoding = std::env::var("PYTHONIOENCODING").unwrap_or_else(|_| "utf-8".into());
    assert!(
        result
            .output
            .contains(&format!("stdio={expected_encoding}")),
        "{}",
        result.output
    );

    let legacy = root.call(
        "shell",
        json!({
            "command":"[Console]::Out.Write('script-mode')", "args":null
        }),
    );
    assert!(legacy.success, "{}", legacy.output);
    assert!(legacy.output.contains("script-mode"));
    let powershell = program.to_string_lossy().replace('\'', "''");
    let script_path = script.to_string_lossy().replace('\'', "''");
    let check = root.call("shell", json!({
        "command": format!(
            "& '{powershell}' -NoLogo -NoProfile -NonInteractive -File '{script_path}' check; $validationExit = $LASTEXITCODE; Write-Output 'check completed'; exit $validationExit"
        ),
    }));
    assert!(!check.success, "{}", check.output);
    assert!(check.output.starts_with("exit 7\n"), "{}", check.output);
    assert!(check.output.contains("check completed"), "{}", check.output);
    for args in [json!("not-an-array"), json!([1]), json!([null])] {
        let invalid = root.call(
            "shell",
            json!({"command":"Set-Content forbidden.txt bad", "args":args}),
        );
        assert!(!invalid.success);
        assert!(invalid.output.contains("shell args"), "{}", invalid.output);
    }
    assert!(!root.0.join("forbidden.txt").exists());
    let missing = root.call(
        "shell",
        json!({"command":"slim-missing-program-fixture.exe", "args":[]}),
    );
    assert!(!missing.success);
    assert!(
        missing.output.contains("executable not found"),
        "{}",
        missing.output
    );
}

#[cfg(windows)]
#[test]
fn script_shell_uses_utf8_for_native_pipeline_input_and_output() {
    let root = Workspace::new();
    let powershell = slim_core::process::ProcessRunner::default()
        .resolve_powershell()
        .unwrap()
        .expect("PowerShell test runtime");
    let powershell = powershell.to_string_lossy().replace('\'', "''");
    let result = root.call(
        "shell",
        json!({
            "command": format!(
                "$payload = 'ação日本語'; $payload | & '{powershell}' -NoLogo -NoProfile -NonInteractive -Command '$input | ForEach-Object {{ $_ }}'; exit 17"
            )
        }),
    );

    assert!(!result.success, "{}", result.output);
    assert!(result.output.contains("exit 17"), "{}", result.output);
    assert!(result.output.contains("ação日本語"), "{}", result.output);
}

#[test]
fn default_read_finishes_medium_files_and_preserves_explicit_pagination() {
    let root = Workspace::new();
    let text = (1..=200).map(|i| format!("line {i}\n")).collect::<String>();
    fs::write(root.0.join("medium.txt"), &text).unwrap();
    let complete = root.call("read", json!({"path": "medium.txt"}));
    assert!(complete.success, "{}", complete.output);
    assert_eq!(complete.output, text);

    let over_default = (1..=250).map(|i| format!("line {i}\n")).collect::<String>();
    fs::write(root.0.join("fits.txt"), &over_default).unwrap();
    let fitted = root.call("read", json!({"path": "fits.txt"}));
    assert!(fitted.success, "{}", fitted.output);
    assert_eq!(fitted.output, over_default);
    assert!(!fitted.output.contains("showing lines"));

    let excerpt = root.call(
        "read",
        json!({"path": "medium.txt", "offset": 199, "max_lines": 1}),
    );
    assert!(excerpt.success, "{}", excerpt.output);
    assert!(excerpt.output.starts_with("line 199\n"));
    assert!(excerpt.output.contains("showing lines 199-199"));
    assert!(!excerpt.output.contains("line 200\n"));

    let long = (1..=4097)
        .map(|i| format!("line {i}\n"))
        .collect::<String>();
    fs::write(root.0.join("long.txt"), &long).unwrap();
    let first = root.call("read", json!({"path": "long.txt"}));
    assert!(first.success, "{}", first.output);
    assert!(first.output.contains("showing lines 1-4096"));
    assert!(!first.output.contains("line 4097\n"));
    let last = root.call("read", json!({"path": "long.txt", "offset": 4097}));
    assert!(last.success, "{}", last.output);
    assert_eq!(last.output, "line 4097\n");
}

#[test]
fn patch_inserted_lines_preserve_crlf_and_remain_matchable() {
    let root = Workspace::new();
    let path = root.0.join("text.txt");
    fs::write(&path, "first\r\nitem\r\nlast\r\n").unwrap();
    let inserted = root.call(
        "patch",
        json!({"path":"text.txt", "edits":[
            {"expected":"item", "replacement":"one\ntwo"}
        ]}),
    );
    assert!(inserted.success, "{}", inserted.output);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "first\r\none\r\ntwo\r\nlast\r\n"
    );
    let followup = root.call(
        "patch",
        json!({"path":"text.txt", "edits":[
            {"expected":"two\nlast", "replacement":"done\nlast"}
        ]}),
    );
    assert!(followup.success, "{}", followup.output);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "first\r\none\r\ndone\r\nlast\r\n"
    );

    fs::write(&path, "first\nitem\r\nlast\r\n").unwrap();
    let mixed = root.call(
        "patch",
        json!({"path":"text.txt", "edits":[
            {"expected":"item", "replacement":"one\ntwo"}
        ]}),
    );
    assert!(mixed.success, "{}", mixed.output);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "first\none\ntwo\r\nlast\r\n"
    );

    fs::write(&path, "item\r\n").unwrap();
    let oversized = root.call(
        "patch",
        json!({"path":"text.txt", "edits":[
            {"expected":"item", "replacement":"\n".repeat(6 * 1024 * 1024)}
        ]}),
    );
    assert!(!oversized.success);
    assert!(
        oversized.output.contains("mutation safety limit"),
        "{}",
        oversized.output
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "item\r\n");
}

#[test]
fn complete_read_guards_native_overwrite_without_repeating_the_file() {
    for (expected, paged) in [
        (Value::Null, false),
        (Value::Null, true),
        (json!("ação\nsecond\n"), false),
    ] {
        let root = Workspace::new();
        let tools = ToolRegistry::default();
        let path = root.0.join("text.txt");
        fs::write(&path, "ação\r\nsecond\r\n").unwrap();
        let read = tools.execute(
            OperatingMode::Auto,
            &root.0,
            "read",
            if paged {
                r#"{"path":"text.txt","max_lines":1}"#
            } else {
                r#"{"path":"text.txt"}"#
            },
        );
        assert!(read.success, "{}", read.output);
        if !paged {
            assert_eq!(read.output, "ação\r\nsecond\r\n");
        } else {
            assert!(read.output.starts_with("ação\r\n"));
            assert!(read.output.contains("showing lines 1-1"));
        }
        if paged {
            let rest = tools.execute(
                OperatingMode::Auto,
                &root.0,
                "read",
                r#"{"path":"text.txt","offset":2,"max_lines":1}"#,
            );
            assert!(rest.success, "{}", rest.output);
            assert_eq!(rest.output, "second\r\n");
        }
        let written = tools.execute(
            OperatingMode::Auto,
            &root.0,
            "write",
            &json!({"path":"text.txt", "content":"ação\nupdated\n", "expected":expected})
                .to_string(),
        );
        assert!(written.success, "{}", written.output);
        assert_eq!(fs::read_to_string(&path).unwrap(), "ação\r\nupdated\r\n");
    }
}

#[test]
fn native_overwrite_recovers_from_missing_final_newline_without_weakening_guard() {
    let root = Workspace::new();
    let tools = ToolRegistry::default();
    let path = root.0.join("pager.cjs");
    fs::write(&path, "old\r\n").unwrap();
    let call = |name: &str, args: Value| {
        tools.execute(OperatingMode::Auto, &root.0, name, &args.to_string())
    };
    assert!(call("read", json!({"path":"pager.cjs"})).success);
    let stale = call(
        "write",
        json!({"path":"pager.cjs", "content":"new\n", "expected":"old"}),
    );
    assert!(!stale.success);
    assert!(
        stale.output.contains("Current file is below"),
        "{}",
        stale.output
    );
    assert_eq!(fs::read(&path).unwrap(), b"old\r\n");
    assert!(!call("write", json!({"path":"pager.cjs", "content":"new\n"})).success);
    assert!(call("read", json!({"path":"pager.cjs"})).success);
    assert!(call("write", json!({"path":"pager.cjs", "content":"new\n"})).success);
    assert_eq!(fs::read(&path).unwrap(), b"new\r\n");
}

#[test]
fn patch_batch_is_ordered_atomic_and_preserves_crlf() {
    let root = Workspace::new();
    let tools = ToolRegistry::default();
    let path = root.0.join("batch.txt");
    fs::write(&path, "ação\r\nold\r\nsame\r\nsame\r\ntail").unwrap();
    let call = |edits: Value| {
        tools.execute(
            OperatingMode::Auto,
            &root.0,
            "patch",
            &json!({"path":"batch.txt","edits":edits}).to_string(),
        )
    };
    for bad in [
        json!([]),
        json!([{"expected":"", "replacement":"x"}]),
        json!([{"expected":"old", "replacement":null}]),
        json!([{"expected":"old", "replacement":"new"}, {"expected":"missing", "replacement":"x"}]),
        json!([{"expected":"old", "replacement":"new"}, {"expected":"same", "replacement":"x"}]),
    ] {
        let result = call(bad);
        assert!(!result.success, "{}", result.output);
        assert_eq!(
            fs::read(&path).unwrap(),
            "ação\r\nold\r\nsame\r\nsame\r\ntail".as_bytes()
        );
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    }
    let result = call(json!([
        {"expected":"ação\nold", "replacement":"ação\nnew"},
        {"expected":"new", "replacement":"final"},
        {"expected":"same\r\nsame", "replacement":"one"}
    ]));
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("3 edits applied atomically"));
    assert_eq!(
        fs::read(&path).unwrap(),
        "ação\r\nfinal\r\none\r\ntail".as_bytes()
    );
}

#[test]
fn patch_batch_rejects_late_oversize_and_invalidates_cached_reads_on_success() {
    let root = Workspace::new();
    let tools = ToolRegistry::default();
    let path = root.0.join("batch.txt");
    fs::write(&path, "before\nend").unwrap();
    let read = || {
        tools.execute(
            OperatingMode::Auto,
            &root.0,
            "read",
            r#"{"path":"batch.txt"}"#,
        )
    };
    assert_eq!(read().output, "before\nend");
    let result = tools.execute(
        OperatingMode::Auto,
        &root.0,
        "patch",
        &json!({"path":"batch.txt","edits":[
            {"expected":"before", "replacement":"after"},
            {"expected":"end", "replacement":"x".repeat(10*1024*1024)}
        ]})
        .to_string(),
    );
    assert!(!result.success);
    assert!(
        result.output.contains("mutation safety limit"),
        "{}",
        result.output
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "before\nend");
    let result = tools.execute(
        OperatingMode::Auto,
        &root.0,
        "patch",
        &json!({"path":"batch.txt","edits":[
            {"expected":"before", "replacement":"after"},
            {"expected":"end", "replacement":"done"}
        ]})
        .to_string(),
    );
    assert!(result.success, "{}", result.output);
    assert_eq!(read().output, "after\ndone");
}

#[test]
fn observed_write_requires_a_complete_fresh_read_of_the_target() {
    let root = Workspace::new();
    let tools = ToolRegistry::default();
    fs::write(root.0.join("mixed.txt"), "first\r\nsecond\n").unwrap();
    let mixed = tools.execute(
        OperatingMode::Auto,
        &root.0,
        "write",
        &json!({"path":"mixed.txt", "content":"lost", "expected":"first\nsecond\n"}).to_string(),
    );
    assert!(!mixed.success);
    assert_eq!(
        fs::read_to_string(root.0.join("mixed.txt")).unwrap(),
        "first\r\nsecond\n"
    );
    let path = root.0.join("text.txt");
    fs::write(&path, "first\nsecond\n").unwrap();
    fs::write(root.0.join("other.txt"), "other").unwrap();
    let call =
        |name, args: Value| tools.execute(OperatingMode::Auto, &root.0, name, &args.to_string());
    assert!(call("read", json!({"path":"text.txt", "max_lines":1})).success);
    assert!(!call("write", json!({"path":"text.txt", "content":"lost"})).success);
    assert!(call("read", json!({"path":"text.txt"})).success);
    assert!(!call("write", json!({"path":"other.txt", "content":"lost"})).success);
    assert!(
        !call(
            "write",
            json!({"path":"text.txt", "content":"lost", "expected":"wrong"})
        )
        .success
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    assert!(call("read", json!({"path":"text.txt"})).success);
    fs::write(&path, "external change\n").unwrap();
    let stale = call("write", json!({"path":"text.txt", "content":"lost"}));
    assert!(!stale.success);
    assert!(stale.output.contains("stale read"), "{}", stale.output);
    assert_eq!(fs::read_to_string(&path).unwrap(), "external change\n");
    assert!(call("read", json!({"path":"text.txt"})).success);
    assert!(call("write", json!({"path":"text.txt", "content":"verified\n"})).success);
    assert_eq!(fs::read_to_string(&path).unwrap(), "verified\n");
    assert_eq!(
        fs::read_to_string(root.0.join("other.txt")).unwrap(),
        "other"
    );
}

#[test]
fn failed_overwrite_includes_current_file_for_retry_without_a_new_read() {
    let root = Workspace::new();
    fs::write(root.0.join("text.txt"), "current\n").unwrap();
    let missing = root.call("write", json!({"path":"text.txt","content":"next\n"}));
    assert!(!missing.success);
    assert!(
        missing.output.contains("precondition required"),
        "{}",
        missing.output
    );
    assert!(missing.output.contains("current\n"), "{}", missing.output);
    assert!(
        missing.output.contains("Do not read again"),
        "{}",
        missing.output
    );
    let retried = root.call(
        "write",
        json!({"path":"text.txt","content":"next\n","expected":"current\n"}),
    );
    assert!(retried.success, "{}", retried.output);
    assert!(retried.output.starts_with("written "), "{}", retried.output);
    assert!(retried.output.contains("text.txt"), "{}", retried.output);
    assert!(retried.output.contains("exists=true"), "{}", retried.output);
    assert!(retried.output.contains("sha256="), "{}", retried.output);
    assert_eq!(
        fs::read_to_string(root.0.join("text.txt")).unwrap(),
        "next\n"
    );

    let stale = root.call(
        "write",
        json!({"path":"text.txt","content":"third\n","expected":"wrong"}),
    );
    assert!(!stale.success);
    assert!(stale.output.contains("stale read"), "{}", stale.output);
    assert!(stale.output.contains("next\n"), "{}", stale.output);
    assert!(
        stale.output.contains("Current file is below"),
        "{}",
        stale.output
    );
    assert!(
        !root
            .call("write", json!({"path":"text.txt","content":"lost\n"}))
            .success
    );
    let recovered = root.call(
        "write",
        json!({"path":"text.txt","content":"third\n","expected":"next\n"}),
    );
    assert!(recovered.success, "{}", recovered.output);
    assert_ne!(retried.output, recovered.output);
}

#[test]
fn successful_overwrite_authorizes_next_write_without_expected() {
    let root = Workspace::new();
    let tools = ToolRegistry::default();
    let call =
        |name, args: Value| tools.execute(OperatingMode::Auto, &root.0, name, &args.to_string());
    fs::write(root.0.join("text.txt"), "first\n").unwrap();
    let first = call(
        "write",
        json!({"path":"text.txt","content":"second\n","expected":"first\n"}),
    );
    assert!(first.success, "{}", first.output);
    assert!(first.output.contains("do not re-read"), "{}", first.output);
    let second = call("write", json!({"path":"text.txt","content":"third\n"}));
    assert!(second.success, "{}", second.output);
    assert_eq!(
        fs::read_to_string(root.0.join("text.txt")).unwrap(),
        "third\n"
    );
}

#[test]
fn successful_patch_authorizes_next_write_without_expected() {
    let root = Workspace::new();
    let tools = ToolRegistry::default();
    let call =
        |name, args: Value| tools.execute(OperatingMode::Auto, &root.0, name, &args.to_string());
    fs::write(root.0.join("text.txt"), "alpha\nbeta\n").unwrap();
    let patched = call(
        "patch",
        json!({"path":"text.txt","edits":[{"expected":"beta","replacement":"gamma"}]}),
    );
    assert!(patched.success, "{}", patched.output);
    assert!(
        patched.output.contains("do not re-read"),
        "{}",
        patched.output
    );
    let written = call("write", json!({"path":"text.txt","content":"done\n"}));
    assert!(written.success, "{}", written.output);
    assert_eq!(
        fs::read_to_string(root.0.join("text.txt")).unwrap(),
        "done\n"
    );
}

#[test]
fn write_null_creates_nested_unicode_file_without_relaxing_overwrite_checks() {
    let root = Workspace::new();
    let path = "sub folder/ação.txt";
    let created = root.call(
        "write",
        json!({"path":path,"content":"ação\n","expected":null}),
    );
    assert!(created.success, "{}", created.output);
    for expected in [Value::Null, json!(""), json!("stale")] {
        let rejected = root.call(
            "write",
            json!({"path":path,"content":"wrong","expected":expected}),
        );
        assert!(!rejected.success, "{}", rejected.output);
        assert_eq!(fs::read_to_string(root.0.join(path)).unwrap(), "ação\n");
    }
    let replaced = root.call(
        "write",
        json!({"path":path,"content":"ok","expected":"ação\n"}),
    );
    assert!(replaced.success, "{}", replaced.output);
    assert_eq!(fs::read_to_string(root.0.join(path)).unwrap(), "ok");
}

#[test]
fn deleted_observed_file_can_be_recreated_without_explicit_expected() {
    for expected in [
        None,
        Some(Value::Null),
        Some(json!("before")),
        Some(json!("")),
    ] {
        let root = Workspace::new();
        let registry = ToolRegistry::default();
        let path = root.0.join("data.txt");
        fs::write(&path, "before").unwrap();
        let read = registry.execute(
            OperatingMode::Auto,
            &root.0,
            "read",
            &json!({"path":"data.txt"}).to_string(),
        );
        assert!(read.success, "{}", read.output);
        fs::remove_file(&path).unwrap();
        let explicit = expected.as_ref().is_some_and(|value| !value.is_null());
        let mut args = json!({"path":"data.txt","content":"after"});
        if let Some(expected) = expected {
            args["expected"] = expected;
        }
        let result = registry.execute(OperatingMode::Auto, &root.0, "write", &args.to_string());
        if explicit {
            assert!(!result.success, "{}", result.output);
            assert!(!path.exists());
            args.as_object_mut().unwrap().remove("expected");
            let retry = registry.execute(OperatingMode::Auto, &root.0, "write", &args.to_string());
            assert!(retry.success, "{}", retry.output);
        } else {
            assert!(result.success, "{}", result.output);
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "after");
        fs::write(&path, "external").unwrap();
        args.as_object_mut().unwrap().remove("expected");
        let unobserved = registry.execute(OperatingMode::Auto, &root.0, "write", &args.to_string());
        assert!(!unobserved.success, "{}", unobserved.output);
        assert_eq!(fs::read_to_string(&path).unwrap(), "external");
    }
}

#[test]
fn missing_file_precondition_explains_recovery_and_preserves_empty_file_semantics() {
    let root = Workspace::new();
    let missing_read = root.call("read", json!({"path":"new.txt"}));
    assert!(!missing_read.success);
    assert!(
        missing_read.output.contains("new.txt: file does not exist"),
        "{}",
        missing_read.output
    );
    assert!(missing_read
        .output
        .contains("use write with expected omitted or null"));
    for expected in ["", "previous content"] {
        let rejected = root.call(
            "write",
            json!({"path":"new.txt","content":"after","expected":expected}),
        );
        assert!(!rejected.success);
        assert!(
            rejected.output.contains("file does not exist"),
            "{}",
            rejected.output
        );
        assert!(
            rejected.output.contains("omit expected or set it to null"),
            "{}",
            rejected.output
        );
        assert!(!root.0.join("new.txt").exists());
    }
    let created = root.call("write", json!({"path":"new.txt","content":""}));
    assert!(created.success, "{}", created.output);
    let replaced = root.call(
        "write",
        json!({"path":"new.txt","content":"after","expected":""}),
    );
    assert!(replaced.success, "{}", replaced.output);
    assert_eq!(fs::read_to_string(root.0.join("new.txt")).unwrap(), "after");
}

#[test]
fn shell_has_one_interpreter_for_commands_with_and_without_heuristic_markers() {
    let root = Workspace::new();
    // Neither Get-Location nor this literal contains the old PowerShell markers.
    let location = root.call("shell", json!({"command":"Get-Location"}));
    assert!(location.success, "{}", location.output);
    let literal = root.call(
        "shell",
        json!({"command":"'dollar $content and :: stay literal'"}),
    );
    assert!(literal.success, "{}", literal.output);
    assert!(literal
        .output
        .contains("dollar $content and :: stay literal"));
    let variable = root.call("shell", json!({"command":"$content = 'ação'; [IO.File]::WriteAllText((Join-Path (Get-Location) 'ação.txt'), $content); Get-Content -LiteralPath 'ação.txt'"}));
    assert!(variable.success, "{}", variable.output);
    assert_eq!(fs::read_to_string(root.0.join("ação.txt")).unwrap(), "ação");
    let failed = root.call("shell", json!({"command":"Write-Output 'failure'; exit 7"}));
    assert!(!failed.success);
    assert!(failed.output.starts_with("exit 7\n"), "{}", failed.output);
    let parse_error = root.call("shell", json!({"command":"if ("}));
    assert!(!parse_error.success);
    #[cfg(windows)]
    {
        let native_failed = root.call("shell", json!({"command":"cmd /d /c exit 9"}));
        assert!(!native_failed.success);
        assert!(
            native_failed.output.starts_with("exit 9\n"),
            "{}",
            native_failed.output
        );
    }
}

#[test]
fn write_rejects_expected_above_the_mutation_budget() {
    let root = Workspace::new();
    fs::write(root.0.join("keep.txt"), "keep\n").unwrap();
    let oversized = root.call(
        "write",
        json!({
            "path": "keep.txt",
            "content": "after\n",
            "expected": "x".repeat(10 * 1024 * 1024 + 1)
        }),
    );
    assert!(!oversized.success, "{}", oversized.output);
    assert!(
        oversized.output.contains("mutation safety limit"),
        "{}",
        oversized.output
    );
    assert_eq!(
        fs::read_to_string(root.0.join("keep.txt")).unwrap(),
        "keep\n"
    );
}

#[test]
fn shared_native_contracts_reach_chat_messages_and_responses() {
    let mut tools = slim_core::Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
    let advertised = &tools;
    let read = advertised
        .iter()
        .find(|tool| tool["name"] == "read")
        .unwrap();
    assert!(read["input_schema"]["properties"]["max_lines"]
        .get("default")
        .is_none());
    assert_eq!(
        read["input_schema"]["properties"]["max_lines"]["maximum"],
        slim_core::tools::MAX_READ_LINES_CAP
    );
    assert!(read["input_schema"]["properties"].get("lines").is_none());
    let patch = advertised
        .iter()
        .find(|tool| tool["name"] == "patch")
        .unwrap();
    assert_eq!(patch["input_schema"]["required"], json!(["path", "edits"]));
    assert!(patch["input_schema"]["properties"]
        .get("expected")
        .is_none());
    assert!(patch["input_schema"]["properties"]
        .get("replacement")
        .is_none());
    assert!(patch["input_schema"].get("oneOf").is_none());
    let search = advertised
        .iter()
        .find(|tool| tool["name"] == "search")
        .unwrap();
    assert_eq!(
        search["input_schema"]["properties"]["context_lines"]["default"],
        0
    );
    let context = &search["input_schema"]["properties"]["context_lines"];
    assert_eq!(context["type"], "integer");
    assert_eq!(context["minimum"], 0);
    assert!(context.get("maximum").is_none());
    assert!(context["description"].as_str().unwrap().contains('3'));
    assert!(!search["input_schema"]["required"]
        .as_array()
        .unwrap()
        .contains(&json!("context_lines")));
    assert_eq!(
        search["input_schema"]["anyOf"],
        json!([
            {"required":["query"]}, {"required":["patterns"]}
        ])
    );
    assert!(search["input_schema"].get("oneOf").is_none());
    println!(
        "NATIVE_PREFIX system_bytes={} schema_bytes={}",
        slim_core::provider::NATIVE_SYSTEM_PROMPT.len(),
        serde_json::to_vec(&advertised).unwrap().len(),
    );
    // Runtime only advertises semantic navigation with an available backend;
    // serialize its shared contract here without starting one.
    let intel = slim_core::tools::code_intel_definition();
    let branches = &intel["input_schema"]["oneOf"];
    assert_eq!(
        branches[0]["properties"]["action"]["enum"],
        json!(["definition", "references", "hover"])
    );
    assert_eq!(branches[0]["required"], json!(["path", "line", "column"]));
    assert_eq!(
        branches[1]["anyOf"],
        json!([{"required":["path"]}, {"required":["query"]}])
    );
    assert_eq!(
        branches[2]["properties"]["action"]["enum"],
        json!(["diagnostics"])
    );
    assert_eq!(
        branches[3]["properties"]["action"]["enum"],
        json!(["status"])
    );
    assert_eq!(branches[0]["not"], json!({"required":["server"]}));
    assert_eq!(branches[3]["not"], json!({"required":["server"]}));
    assert_eq!(
        intel["input_schema"]["properties"]["server"]["type"],
        "string"
    );
    tools.push(intel);
    let mut adapters: Vec<Box<dyn ProviderAdapter>> = vec![
        Box::new(
            OpenAiCompatibleAdapter::new(ProviderConfig::openai(
                "https://example.invalid/v1/chat/completions",
                "model",
                "fixture",
            ))
            .unwrap(),
        ),
        Box::new(
            AnthropicAdapter::new(ProviderConfig::anthropic(
                "https://example.invalid/v1/messages",
                "claude-test",
                "fixture",
            ))
            .unwrap(),
        ),
        Box::new(
            OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
                "https://chatgpt.com/backend-api",
                "gpt-5.6-luna",
                "fixture",
                "fixture-account",
            ))
            .unwrap(),
        ),
    ];
    let endpoint = "https://example.invalid/v1";
    adapters.push(Box::new(
        XaiAdapter::new(endpoint, "grok-4.5", "fixture", None).unwrap(),
    ));
    adapters.push(Box::new(
        ClinePassAdapter::new(endpoint, "cline-pass/qwen3.7-max", "fixture", None).unwrap(),
    ));
    for model in ["deepseek-v4-flash", "gpt-5.6-luna", "minimax-m3"] {
        adapters.push(Box::new(
            OpenCodeGoAdapter::new(endpoint, model, "fixture", None).unwrap(),
        ));
    }
    for model in ["gpt-5.6-sol", "claude-sonnet-4-6"] {
        adapters.push(Box::new(
            CommandCodeAdapter::new(endpoint, model, "fixture", None).unwrap(),
        ));
    }
    for adapter in adapters {
        let request = adapter
            .prepare_messages_request_with_tools_checked(&[ProviderMessage::user("work")], &tools)
            .unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let wire_tools = body["tools"].as_array().unwrap();
        for name in ["read", "patch", "write", "shell", "search", "code_intel"] {
            let original = tools.iter().find(|tool| tool["name"] == name).unwrap();
            let wire = wire_tools
                .iter()
                .map(|tool| tool.get("function").unwrap_or(tool))
                .find(|tool| tool["name"] == name)
                .unwrap();
            assert_eq!(wire["description"], original["description"]);
            if wire["type"] == "function" {
                assert_eq!(
                    wire["strict"], false,
                    "optional fields must remain optional on Responses"
                );
            }
            assert_eq!(
                wire.get("parameters")
                    .or_else(|| wire.get("input_schema"))
                    .unwrap(),
                &original["input_schema"]
            );
        }
        let write = tools.iter().find(|tool| tool["name"] == "write").unwrap();
        let patch = tools.iter().find(|tool| tool["name"] == "patch").unwrap();
        assert_eq!(patch["input_schema"]["required"], json!(["path", "edits"]));
        assert_eq!(patch["input_schema"]["properties"]["edits"]["minItems"], 1);
        assert_eq!(
            patch["input_schema"]["properties"]["edits"]["items"]["required"],
            json!(["expected", "replacement"])
        );
        if body.get("input").is_some() {
            if adapter.model() == "gpt-5.6-luna" {
                assert_eq!(body["text"]["verbosity"], "low");
            } else {
                assert!(
                    body.get("text").is_none(),
                    "do not send GPT verbosity controls to other models"
                );
            }
        }
        assert_eq!(
            write["input_schema"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["content", "expected", "path", "then_run"]
        );
    }
}

#[test]
fn unadvertised_legacy_arguments_remain_compatible() {
    let root = Workspace::new();
    fs::write(root.0.join("data.txt"), "first\nsecond\n").unwrap();
    let legacy_read = root.call("read", json!({"path":"data.txt", "lines":1}));
    let canonical_read = root.call("read", json!({"path":"data.txt", "max_lines":1}));
    assert!(legacy_read.success, "{}", legacy_read.output);
    assert!(canonical_read.success, "{}", canonical_read.output);
    assert_eq!(
        legacy_read
            .output
            .strip_prefix("[admission: lines -> max_lines; limit 1]\n"),
        Some(canonical_read.output.as_str())
    );
    for args in [
        json!({"path":"data.txt", "expected":"first", "replacement":"changed"}),
        json!({"path":"data.txt", "edits":[{"expected":"changed", "replacement":"final"}]}),
    ] {
        let result = root.call("patch", args);
        assert!(result.success, "{}", result.output);
    }
    assert_eq!(
        fs::read_to_string(root.0.join("data.txt")).unwrap(),
        "final\nsecond\n"
    );
    let mixed = root.call(
        "patch",
        json!({
            "path":"data.txt", "expected":"final", "replacement":"bad",
            "edits":[{"expected":"final", "replacement":"bad"}]
        }),
    );
    assert!(!mixed.success);
    assert_eq!(
        fs::read_to_string(root.0.join("data.txt")).unwrap(),
        "final\nsecond\n"
    );
}

#[test]
fn patch_ambiguous_match_describes_example_and_allows_selecting_second() {
    let root = Workspace::new();
    let original = "first\nsame\nmiddle\nsame\nlast\n";
    fs::write(root.0.join("t.txt"), original).unwrap();
    let result = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"same\n","replacement":"x\n"}]}),
    );
    assert!(!result.success, "{}", result.output);
    assert!(
        result
            .output
            .contains("Example context only for the first match at line 2"),
        "{}",
        result.output
    );
    assert!(
        result.output.contains("first\nsame\nmiddle\n"),
        "{}",
        result.output
    );
    assert!(
        result
            .output
            .contains("choose the intended occurrence explicitly"),
        "{}",
        result.output
    );
    assert!(!result.output.contains("Suggested unique expected"));
    assert_eq!(fs::read_to_string(root.0.join("t.txt")).unwrap(), original);

    let second = root.call(
        "patch",
        json!({
            "path":"t.txt",
            "edits":[{"expected":"middle\nsame\nlast\n","replacement":"middle\nchanged\nlast\n"}]
        }),
    );
    assert!(second.success, "{}", second.output);
    assert_eq!(
        fs::read_to_string(root.0.join("t.txt")).unwrap(),
        "first\nsame\nmiddle\nchanged\nlast\n"
    );
}

#[test]
fn patch_unique_identical_match_succeeds_without_changing_file() {
    let root = Workspace::new();
    let path = root.0.join("t.txt");
    let original = "only\nmatch\n";
    fs::write(&path, original).unwrap();

    let result = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"only\nmatch\n","replacement":"only\nmatch\n"}]}),
    );
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("expected equals replacement"));
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn failed_patch_differing_only_in_whitespace_returns_the_exact_lines_to_copy() {
    let root = Workspace::new();
    let filler = "// filler line\n".repeat(40);
    let body = format!("{filler}fn a() {{\n\tlet x = 1;  \n\treturn x;\n}}\n{filler}");
    fs::write(root.0.join("t.rs"), &body).unwrap();
    let patch = |expected: &str| {
        root.call(
            "patch",
            json!({"path":"t.rs","edits":[{"expected":expected,"replacement":"y"}]}),
        )
    };
    let near = patch("    let x = 1;\n    return x;\n");
    assert!(!near.success, "{}", near.output);
    assert!(
        near.output.ends_with(
            "Closest text is at lines 42-43 and differs from expected only in whitespace. Retry patch with it copied exactly; do not read again:\n\tlet x = 1;  \n\treturn x;"
        ),
        "{}",
        near.output
    );
    assert!(!near.output.contains("filler"), "{}", near.output);
    // The lines it names are the excerpt that applies.
    let exact = root.call(
        "patch",
        json!({"path":"t.rs","edits":[{"expected":"\tlet x = 1;  \n\treturn x;","replacement":"\treturn 1;"}]}),
    );
    assert!(exact.success, "{}", exact.output);
    // Several candidate runs, or none, keep the whole-file recovery.
    for expected in ["//  filler line", "let y = 2;"] {
        let other = patch(expected);
        assert!(!other.output.contains("Closest text"), "{}", other.output);
        assert!(
            other.output.contains("Current file is below"),
            "{}",
            other.output
        );
    }
    assert_eq!(
        fs::read_to_string(root.0.join("t.rs")).unwrap().len(),
        body.len() - 14
    );
}

#[test]
fn missing_file_failures_name_the_same_file_elsewhere() {
    let root = Workspace::new();
    fs::create_dir_all(root.0.join("crates/core/src")).unwrap();
    fs::write(root.0.join("crates/core/src/Config.rs"), "x\n").unwrap();
    for (name, args) in [
        ("read", json!({"path":"src/config.rs"})),
        (
            "patch",
            json!({"path":"src/config.rs","edits":[{"expected":"x","replacement":"y"}]}),
        ),
        (
            "write",
            json!({"path":"src/config.rs","content":"y","expected":"x\n"}),
        ),
    ] {
        let result = root.call(name, args);
        assert!(!result.success, "{name}: {}", result.output);
        assert!(
            result
                .output
                .ends_with("\nSame file name elsewhere: crates/core/src/Config.rs"),
            "{name}: {}",
            result.output
        );
    }
    // No namesake, or a failure on a file that exists: no note.
    let absent = root.call("read", json!({"path":"src/other.rs"}));
    assert!(
        !absent.output.contains("Same file name"),
        "{}",
        absent.output
    );
    let present = root.call(
        "patch",
        json!({"path":"crates/core/src/Config.rs","edits":[{"expected":"zzz","replacement":"y"}]}),
    );
    assert!(
        !present.output.contains("Same file name"),
        "{}",
        present.output
    );
}

#[test]
fn failed_patch_zero_match_includes_current_file_for_retry_without_a_new_read() {
    let root = Workspace::new();
    fs::write(root.0.join("t.txt"), "hello\nworld\n").unwrap();
    let missing = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"missing\n","replacement":"x\n"}]}),
    );
    assert!(!missing.success, "{}", missing.output);
    assert!(missing.output.contains("got 0"), "{}", missing.output);
    assert!(
        missing.output.contains("hello\nworld\n"),
        "{}",
        missing.output
    );
    assert!(
        missing.output.contains("Do not read again"),
        "{}",
        missing.output
    );
    assert!(
        !missing.output.contains("Read the current text"),
        "{}",
        missing.output
    );
    let recovered = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"hello\n","replacement":"hi\n"}]}),
    );
    assert!(recovered.success, "{}", recovered.output);
    assert_eq!(
        fs::read_to_string(root.0.join("t.txt")).unwrap(),
        "hi\nworld\n"
    );
}

#[test]
fn patch_repetitive_ambiguous_match_includes_current_file_when_excerpt_is_not_unique() {
    let root = Workspace::new();
    let body = "same\n".repeat(20);
    fs::write(root.0.join("t.txt"), &body).unwrap();
    let result = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"same\n","replacement":"x\n"}]}),
    );
    assert!(!result.success, "{}", result.output);
    assert!(result.output.contains("got 20"), "{}", result.output);
    assert!(
        !result.output.contains("Suggested unique expected"),
        "{}",
        result.output
    );
    assert!(
        !result
            .output
            .contains("Example context only for the first match"),
        "{}",
        result.output
    );
    assert!(
        result.output.contains("Current file is below"),
        "{}",
        result.output
    );
    assert!(result.output.contains(&body), "{}", result.output);
    assert_eq!(fs::read_to_string(root.0.join("t.txt")).unwrap(), body);
}

#[test]
fn patch_zero_match_explains_corrupted_or_non_ascii_excerpt() {
    let root = Workspace::new();
    fs::write(root.0.join("t.txt"), "regressões em módulos\n").unwrap();

    let corrupted = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"regress\u{FFFD}es em","replacement":"x"}]}),
    );
    assert!(!corrupted.success, "{}", corrupted.output);
    assert!(corrupted.output.contains("U+FFFD"), "{}", corrupted.output);
    assert!(
        corrupted.output.contains("copy current text verbatim"),
        "{}",
        corrupted.output
    );

    let non_ascii = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"regressões em outros","replacement":"x"}]}),
    );
    assert!(!non_ascii.success, "{}", non_ascii.output);
    assert!(
        non_ascii.output.contains("non-ASCII"),
        "{}",
        non_ascii.output
    );
    assert_eq!(
        fs::read_to_string(root.0.join("t.txt")).unwrap(),
        "regressões em módulos\n"
    );
}

#[test]
fn python_environment_search_noise_is_skipped_but_explicit_reads_work() {
    let root = Workspace::new();
    fs::create_dir_all(root.0.join(".venv/Lib/site-packages/pkg")).unwrap();
    fs::write(
        root.0.join(".venv/Lib/site-packages/pkg/data.txt"),
        "needle DEPENDENCY_NOISE",
    )
    .unwrap();
    fs::write(
        root.0.join("source.py"),
        "needle PROJECT_SOURCE\nneedle SECOND_SOURCE\n",
    )
    .unwrap();
    let tools = ToolRegistry::default();
    let args = json!({"path":".","query":"needle","max_hits":1});
    let first = tools.execute(OperatingMode::Auto, &root.0, "search", &args.to_string());
    assert!(first.success, "{}", first.output);
    assert!(first.output.contains("PROJECT_SOURCE"), "{}", first.output);
    assert!(
        !first.output.contains("DEPENDENCY_NOISE"),
        "{}",
        first.output
    );
    // A result with hits does not repeat the exclusions; a miss discloses them.
    assert!(!first.output.contains("[skipped:"), "{}", first.output);
    let miss = tools.execute(
        OperatingMode::Auto,
        &root.0,
        "search",
        &json!({"path":".","query":"absent-term"}).to_string(),
    );
    assert!(
        miss.output.contains(".venv"),
        "exclusions must be disclosed"
    );
    let marker = "pass \"cursor\": \"";
    let tail = first.output.split_once(marker).expect("continuation").1;
    let cursor = tail.split('"').next().unwrap();
    let second = tools.execute(
        OperatingMode::Auto,
        &root.0,
        "search",
        &json!({"path":".","query":"needle","max_hits":1,"cursor":cursor}).to_string(),
    );
    assert!(second.success, "{}", second.output);
    assert!(second.output.contains("SECOND_SOURCE"), "{}", second.output);
    assert!(!second.output.contains("DEPENDENCY_NOISE"));
    let explicit = root.call(
        "read",
        json!({"path":".venv/Lib/site-packages/pkg/data.txt"}),
    );
    assert!(explicit.success && explicit.output.contains("DEPENDENCY_NOISE"));
    let listed = root.call("list", json!({"path":".venv/Lib/site-packages/pkg"}));
    assert!(listed.success && listed.output.contains("data.txt"));
}

#[test]
fn json_write_reports_syntax_without_rolling_back_or_claiming_task_validation() {
    let root = Workspace::new();
    let invalid = "{\n  \"first\": 1\n  \"second\": 2\n}\n";
    let result = root.call("write", json!({"path":"new.json","content":invalid}));
    assert!(
        result.success,
        "a diagnostic must not lie about a completed write"
    );
    assert!(
        result.output.contains("JSON syntax diagnostic"),
        "{}",
        result.output
    );
    assert!(result.output.contains("line 3"), "{}", result.output);
    assert_eq!(
        fs::read_to_string(root.0.join("new.json")).unwrap(),
        invalid
    );
    for (path, content) in [
        ("valid.json", "{\"x\":1e9999}"),
        ("comments.jsonc", "{// supported elsewhere\n}"),
        ("plain.txt", invalid),
    ] {
        let result = root.call("write", json!({"path":path,"content":content}));
        assert!(result.success, "{}", result.output);
        assert!(
            !result.output.contains("JSON syntax diagnostic"),
            "{}",
            result.output
        );
    }
    fs::write(root.0.join("template.json"), "{ {{template}} }").unwrap();
    let template = root.call(
        "write",
        json!({"path":"template.json","expected":"{ {{template}} }","content":"{ {{changed}} }"}),
    );
    assert!(template.success && !template.output.contains("JSON syntax diagnostic"));
}

#[test]
fn json_patch_checks_final_content_once_and_recovers_with_crlf_intact() {
    let root = Workspace::new();
    let original = "{\r\n  \"a\": 1,\r\n  \"b\": 2\r\n}\r\n";
    fs::write(root.0.join("config.json"), original).unwrap();
    let transient = root.call(
        "patch",
        json!({"path":"config.json","edits":[
            {"expected":"\"a\": 1,","replacement":"\"a\": 2"},
            {"expected":"\"a\": 2","replacement":"\"a\": 3,"}
        ]}),
    );
    assert!(transient.success && !transient.output.contains("JSON syntax diagnostic"));
    let malformed = root.call(
        "patch",
        json!({"path":"config.json","expected":"\"a\": 3,","replacement":"\"a\": 3"}),
    );
    assert!(
        malformed.success && malformed.output.contains("JSON syntax diagnostic"),
        "{}",
        malformed.output
    );
    let fixed = root.call(
        "patch",
        json!({"path":"config.json","expected":"\"a\": 3","replacement":"\"a\": 4,"}),
    );
    assert!(fixed.success && !fixed.output.contains("JSON syntax diagnostic"));
    let text = fs::read_to_string(root.0.join("config.json")).unwrap();
    assert_eq!(text, original.replace("\"a\": 1", "\"a\": 4"));
    assert!(serde_json::from_str::<Value>(&text).is_ok());
}

#[test]
fn recovery_guidance_is_bounded_without_duplicate_actions_or_evidence_loss() {
    let root = Workspace::new();
    fs::write(root.0.join("write.txt"), "current\n").unwrap();
    let stale = root.call(
        "write",
        json!({"path":"write.txt", "content":"next\n", "expected":"wrong\n"}),
    );
    assert!(!stale.success, "{}", stale.output);
    let stale_path = stale
        .output
        .strip_prefix("stale read: ")
        .and_then(|text| {
            text.split_once("; precondition differs")
                .map(|(path, _)| path)
        })
        .expect("stale path");
    let normalized_stale = stale.output.replace(stale_path, "<path>");
    let old_stale = "stale read: <path>; the precondition differs from current bytes. Retry write with expected set to the current file below, or patch an exact current excerpt. No write applied.\nCurrent file is below; retry write with expected set to this full text, or patch a unique excerpt. Do not read again.\ncurrent\n";
    assert!(normalized_stale.len() <= old_stale.len());
    let (_, stale_evidence) = stale
        .output
        .split_once("Current file is below; ")
        .and_then(|(_, rest)| rest.split_once('\n'))
        .expect("stale recovery context");
    assert_eq!(stale_evidence, "current\n");
    assert_eq!(stale.output.matches("Do not read again").count(), 1);

    fs::write(root.0.join("patch.txt"), "hello\nworld\n").unwrap();
    let missing = root.call(
        "patch",
        json!({"path":"patch.txt", "edits":[{"expected":"missing\n", "replacement":"x\n"}]}),
    );
    assert!(!missing.success, "{}", missing.output);
    let patch_path = missing
        .output
        .lines()
        .nth(1)
        .and_then(|line| line.split_once(": file unchanged.").map(|(path, _)| path))
        .expect("patch path");
    let normalized_patch = missing.output.replace(patch_path, "<path>");
    let old_patch = "expected exactly one match, got 0\n<path>: file unchanged. Use a unique exact excerpt, including its whitespace and line endings.\nCurrent file is below; retry patch with a unique exact excerpt from this text, including whitespace and line endings. Do not read again.\nhello\nworld\n";
    assert!(normalized_patch.len() <= old_patch.len());
    let (_, patch_evidence) = missing
        .output
        .split_once("Current file is below; ")
        .and_then(|(_, rest)| rest.split_once('\n'))
        .expect("patch recovery context");
    assert_eq!(patch_evidence, "hello\nworld\n");
    assert_eq!(missing.output.matches("Current file is below").count(), 1);

    assert!(recovery_guidance_bytes(&stale.output, &root.0.join("write.txt")) <= 256);
    assert!(recovery_guidance_bytes(&missing.output, &root.0.join("patch.txt")) <= 256);

    let mut large = "🦀".repeat(40_000);
    large.push('\n');
    for index in 0..63 {
        large.push_str(&format!("line{index}\n"));
    }
    fs::write(root.0.join("large.txt"), &large).unwrap();
    let mut edits = Vec::with_capacity(64);
    for index in 0..63 {
        edits.push(json!({
            "expected": format!("line{index}\n"),
            "replacement": format!("line{index}\n")
        }));
    }
    edits.push(json!({"expected":"🦀missing", "replacement":"x"}));
    let batch = root.call("patch", json!({"path":"large.txt", "edits":edits}));
    assert!(!batch.success, "{}", batch.output);
    assert!(
        batch
            .output
            .contains("Edit 64 rejected in proposed content; no edits applied."),
        "{}",
        batch.output
    );
    assert!(batch
        .output
        .contains("non-ASCII; copy current text verbatim."));
    assert!(batch.output.contains("Current file edges are below"));
    assert!(
        // 256 plus the sentence that says the line numbers count the proposed
        // content after edits 1-63.
        recovery_guidance_bytes(&batch.output, &root.0.join("large.txt")) <= 320,
        "guidance bytes={}",
        recovery_guidance_bytes(&batch.output, &root.0.join("large.txt"))
    );
    assert!(batch.output.contains("🦀🦀🦀🦀"));
    assert!(batch.output.contains("line62\n"));
    assert!(batch.output.contains("[truncated"));
    assert_eq!(fs::read_to_string(root.0.join("large.txt")).unwrap(), large);

    let partial_write = root.call(
        "write",
        json!({"path":"large.txt", "content":"next", "expected":"wrong"}),
    );
    assert!(!partial_write.success);
    assert!(partial_write
        .output
        .contains("read the complete file before write"));
    assert!(recovery_guidance_bytes(&partial_write.output, &root.0.join("large.txt")) <= 256);

    let complete = (0..63)
        .map(|index| format!("line{index}\n"))
        .collect::<String>();
    fs::write(root.0.join("large.txt"), &complete).unwrap();
    let complete_batch = root.call("patch", json!({"path":"large.txt", "edits":edits}));
    assert!(!complete_batch.success);
    assert!(complete_batch.output.ends_with(&complete));
    assert!(
        // Same allowance for the proposed-content sentence as above.
        recovery_guidance_bytes(&complete_batch.output, &root.0.join("large.txt")) <= 320,
        "guidance bytes={}",
        recovery_guidance_bytes(&complete_batch.output, &root.0.join("large.txt"))
    );
    assert_eq!(
        fs::read_to_string(root.0.join("large.txt")).unwrap(),
        complete
    );

    let repetitive = "same\n".repeat(40_000);
    fs::write(root.0.join("ambiguous.txt"), &repetitive).unwrap();
    let ambiguous = root.call(
        "patch",
        json!({"path":"ambiguous.txt", "edits":[
            {"expected":"same\n", "replacement":"new\n"},
            {"expected":"later", "replacement":"x"}
        ]}),
    );
    assert!(!ambiguous.success, "{}", ambiguous.output);
    assert!(ambiguous.output.contains("Current file edges are below"));
    assert!(!ambiguous
        .output
        .contains("Example context only for the first match"));
    assert!(
        recovery_guidance_bytes(&ambiguous.output, &root.0.join("ambiguous.txt")) <= 256,
        "guidance bytes={}",
        recovery_guidance_bytes(&ambiguous.output, &root.0.join("ambiguous.txt"))
    );
    assert!(ambiguous.output.contains("same\n"));
}

fn recovery_guidance_bytes(output: &str, path: &std::path::Path) -> usize {
    let mut prefix = String::new();
    let mut found_evidence = false;
    for line in output.split_inclusive('\n') {
        prefix.push_str(line);
        if line.starts_with("Current file is below")
            || line.starts_with("Current file edges are below")
            || line.starts_with("Example context only for the first match at line ")
        {
            found_evidence = true;
            break;
        }
    }
    assert!(found_evidence, "missing evidence boundary: {output}");
    // Headers name the workspace-relative path, never the canonical one.
    let canonical = fs::canonicalize(path).unwrap();
    assert!(!prefix.contains(canonical.to_str().unwrap()), "{prefix}");
    let shown = path.file_name().unwrap().to_str().unwrap();
    assert!(prefix.contains(shown), "{prefix}");
    let mut guidance = prefix.replace(shown, "");
    // Line locations are diagnostic evidence; retain every surrounding label,
    // header, separator and instruction, including the fixed 'first 8' notice.
    if let Some(start) = guidance.find("Matches at lines ") {
        let start = start + "Matches at lines ".len();
        let length = guidance[start..]
            .bytes()
            .take_while(|byte| byte.is_ascii_digit() || matches!(byte, b',' | b' '))
            .count();
        guidance.replace_range(start..start + length, "");
    }
    guidance.len()
}

/// Empty output used to be the answer both to an empty file and to an offset
/// past its last line.
#[test]
fn read_says_whether_the_file_is_empty_or_the_offset_is_past_the_end() {
    let root = Workspace::new();
    fs::write(root.0.join("empty.txt"), "").unwrap();
    fs::write(root.0.join("three.txt"), "a\nb\nc\n").unwrap();
    let read = |path: &str, offset: u64| {
        let result = root.call("read", json!({"path": path, "offset": offset}));
        assert!(result.success, "{}", result.output);
        result.output
    };
    assert_eq!(read("empty.txt", 1), "[empty file]");
    assert_eq!(read("empty.txt", 5), "[empty file]");
    assert_eq!(read("three.txt", 3), "c\n");
    assert_eq!(
        read("three.txt", 4),
        "[offset 4 is past the end; file has 3 lines]"
    );
    assert_eq!(
        read("three.txt", 99),
        "[offset 99 is past the end; file has 3 lines]"
    );
}

#[test]
fn read_failures_on_unreadable_files_say_what_to_do_instead() {
    let root = Workspace::new();
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend("ação\n".encode_utf16().flat_map(u16::to_le_bytes));
    fs::write(root.0.join("wide.txt"), &utf16).unwrap();
    fs::write(root.0.join("latin.txt"), b"fine\nbad \xe9 byte\n").unwrap();
    let mut big_endian = vec![0xFE, 0xFF];
    big_endian.extend("x".encode_utf16().flat_map(u16::to_be_bytes));
    fs::write(root.0.join("be.txt"), &big_endian).unwrap();

    for (name, args) in [
        ("read", json!({"path": "wide.txt"})),
        (
            "write",
            json!({"path": "wide.txt", "content": "x", "expected": "y"}),
        ),
        (
            "patch",
            json!({"path": "wide.txt", "edits": [{"expected": "x", "replacement": "y"}]}),
        ),
    ] {
        let result = root.call(name, args);
        assert!(!result.success, "{name}");
        assert!(
            result
                .output
                .contains("wide.txt is a UTF-16 LE file (BOM detected)"),
            "{name}: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("Get-Content -LiteralPath 'wide.txt' -Encoding Unicode"),
            "{name}: {}",
            result.output
        );
        assert!(
            result.output.contains("write tool"),
            "{name}: {}",
            result.output
        );
        assert!(
            !result.output.contains("io error"),
            "{name}: {}",
            result.output
        );
    }
    let big = root.call("read", json!({"path": "be.txt"}));
    assert!(big.output.contains("UTF-16 BE"), "{}", big.output);
    assert!(
        big.output.contains("-Encoding BigEndianUnicode"),
        "{}",
        big.output
    );
    let latin = root.call("read", json!({"path": "latin.txt"}));
    assert!(!latin.success);
    assert!(
        latin
            .output
            .contains("latin.txt is not valid UTF-8 (invalid byte 0xe9 at line 2)"),
        "{}",
        latin.output
    );
}

#[test]
fn read_limit_failures_name_a_way_around_them() {
    let root = Workspace::new();
    let one_line = root.0.join("one-line.txt");
    fs::write(&one_line, "x".repeat(1024 * 1024 + 1)).unwrap();
    let long = root.call("read", json!({"path": "one-line.txt"}));
    assert!(!long.success);
    assert!(
        long.output
            .contains("read page exceeds the 1048576-byte safety limit"),
        "{}",
        long.output
    );
    assert!(
        long.output.contains("Line 1 alone is over the limit")
            && long.output.contains("Search for a distinctive substring")
            && long
                .output
                .contains("(Get-Content -LiteralPath 'one-line.txt')[0].Substring(0, 4000)"),
        "{}",
        long.output
    );
    // Many lines over the page budget: fewer lines per page is the way.
    fs::write(
        root.0.join("wide-page.txt"),
        format!("{}\n", "y".repeat(300 * 1024)).repeat(4),
    )
    .unwrap();
    let wide = root.call("read", json!({"path": "wide-page.txt", "max_lines": 4}));
    assert!(!wide.success);
    assert!(wide.output.contains("smaller max_lines"), "{}", wide.output);

    let huge = fs::File::create(root.0.join("huge.log")).unwrap();
    huge.set_len(10 * 1024 * 1024 + 1).unwrap();
    drop(huge);
    let huge = root.call("read", json!({"path": "huge.log"}));
    assert!(!huge.success);
    assert!(huge.output.contains("safety limit"), "{}", huge.output);
    assert!(
        huge.output
            .contains("Slice it with shell instead, for example `Get-Content -LiteralPath 'huge.log' -TotalCount 200`"),
        "{}",
        huge.output
    );
}

#[test]
fn write_and_patch_aimed_at_a_directory_say_it_is_a_directory() {
    let root = Workspace::new();
    fs::create_dir(root.0.join("folder")).unwrap();
    for (name, args) in [
        ("write", json!({"path": "folder", "content": "x"})),
        (
            "write",
            json!({"path": "folder", "content": "x", "expected": "y"}),
        ),
        (
            "patch",
            json!({"path": "folder", "edits": [{"expected": "x", "replacement": "y"}]}),
        ),
        ("read", json!({"path": "folder"})),
    ] {
        let result = root.call(name, args);
        assert!(!result.success, "{name}");
        assert!(
            result
                .output
                .contains("path is a directory; use list to see its entries"),
            "{name}: {}",
            result.output
        );
        assert!(
            !result.output.contains("os error"),
            "{name}: {}",
            result.output
        );
    }
}

#[test]
fn a_utf8_bom_neither_makes_expected_stale_nor_is_dropped_by_an_overwrite() {
    let root = Workspace::new();
    let path = root.0.join("bom.txt");
    fs::write(&path, "\u{feff}one\ntwo\n").unwrap();
    // `expected` is what a read shows: the BOM is invisible there.
    let first = root.call(
        "write",
        json!({"path": "bom.txt", "content": "three\n", "expected": "one\ntwo\n"}),
    );
    assert!(first.success, "{}", first.output);
    assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBFthree\n");
    // Spelling it is accepted too, and the new text may spell it.
    let second = root.call(
        "write",
        json!({"path": "bom.txt", "content": "\u{feff}four\n", "expected": "\u{feff}three\n"}),
    );
    assert!(second.success, "{}", second.output);
    assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBFfour\n");
    // A write authorized by the digest of a complete read keeps the BOM too
    // (one registry: the digest lives in its read cache).
    let tools = ToolRegistry::default();
    let run = |name: &str, args: Value| {
        tools.execute(OperatingMode::Auto, &root.0, name, &args.to_string())
    };
    let read = run("read", json!({"path": "bom.txt"}));
    assert!(read.success, "{}", read.output);
    let third = run("write", json!({"path": "bom.txt", "content": "five\n"}));
    assert!(third.success, "{}", third.output);
    assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBFfive\n");
    // A different expected is still stale.
    let stale = root.call(
        "write",
        json!({"path": "bom.txt", "content": "x", "expected": "other\n"}),
    );
    assert!(
        !stale.success && stale.output.contains("stale read"),
        "{}",
        stale.output
    );
    // A patch that spells the BOM in `expected` does not remove it either.
    let patched = root.call(
        "patch",
        json!({"path": "bom.txt", "edits": [{"expected": "\u{feff}five", "replacement": "six"}]}),
    );
    assert!(patched.success, "{}", patched.output);
    assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBFsix\n");
    // A file without a BOM does not gain one.
    fs::write(root.0.join("plain.txt"), "one\n").unwrap();
    let plain = root.call(
        "write",
        json!({"path": "plain.txt", "content": "two\n", "expected": "one\n"}),
    );
    assert!(plain.success, "{}", plain.output);
    assert_eq!(fs::read(root.0.join("plain.txt")).unwrap(), b"two\n");
}

#[test]
fn list_marks_directories_with_a_trailing_slash() {
    let root = Workspace::new();
    fs::create_dir_all(root.0.join("src/inner")).unwrap();
    fs::write(root.0.join("a.txt"), "x").unwrap();
    fs::write(root.0.join("src/lib.rs"), "x").unwrap();
    let top = root.call("list", json!({"path": "."}));
    assert!(top.success, "{}", top.output);
    assert_eq!(top.output, "a.txt\nsrc/");
    let sub = root.call("list", json!({"path": "src"}));
    let separator = std::path::MAIN_SEPARATOR;
    assert_eq!(
        sub.output,
        format!("src{separator}inner/\nsrc{separator}lib.rs"),
        "{}",
        sub.output
    );
}

#[test]
fn search_arguments_in_common_spellings_run_with_an_admission_note() {
    let root = Workspace::new();
    fs::write(root.0.join("a.txt"), "needle one\nother\n").unwrap();
    let aliased = root.call("search", json!({"pattern": "needle", "path": "a.txt"}));
    assert!(aliased.success, "{}", aliased.output);
    assert!(
        aliased.output.contains("pattern -> query"),
        "{}",
        aliased.output
    );
    assert!(
        aliased.output.contains("1: needle one"),
        "{}",
        aliased.output
    );
    let encoded = root.call(
        "search",
        json!({"patterns": "[\"needle\", \"other\"]", "path": "a.txt", "max_hits": "10"}),
    );
    assert!(encoded.success, "{}", encoded.output);
    assert!(
        encoded.output.contains("patterns JSON string decoded once"),
        "{}",
        encoded.output
    );
    assert!(
        encoded.output.contains("max_hits coerced to integer 10"),
        "{}",
        encoded.output
    );
    let file = root.call("read", json!({"file_path": "a.txt", "limit": 1}));
    assert!(file.success, "{}", file.output);
    assert!(file.output.contains("file_path -> path"), "{}", file.output);
    assert!(file.output.contains("needle one"), "{}", file.output);
    let multiline = root.call("search", json!({"query": "needle one\nother"}));
    assert!(!multiline.success);
    assert!(
        multiline.output.contains("single lines"),
        "{}",
        multiline.output
    );
    let unknown = root.call("read", json!({"path": "a.txt", "whole": true}));
    assert!(
        unknown
            .output
            .contains("valid fields: path, offset, max_lines, lines"),
        "{}",
        unknown.output
    );
}

#[test]
fn failed_patch_recovery_prefers_a_small_located_excerpt_to_the_whole_file() {
    let root = Workspace::new();
    let filler = "// filler line\n".repeat(60);
    let body = format!(
        "{filler}fn main() {{\n    let value = compute(1, 2);\n    println!(\"{{}}\", value);\n}}\n{filler}fn tail() {{\n    let unique_marker_value = 42;\n    keep();\n}}\n{filler}"
    );
    fs::write(root.0.join("t.rs"), &body).unwrap();
    let patch = |expected: &str| {
        root.call(
            "patch",
            json!({"path":"t.rs","edits":[{"expected":expected,"replacement":"y"}]}),
        )
    };

    // Whitespace differs inside the lines and the excerpt starts mid-line.
    let substring = patch("let value=compute(1,2);println!(\"{}\",value);");
    assert!(!substring.success, "{}", substring.output);
    assert!(
        substring.output.ends_with(
            "Closest text is at lines 62-63 and differs from expected only in whitespace. Retry patch with it copied exactly; do not read again:\n    let value = compute(1, 2);\n    println!(\"{}\", value);"
        ),
        "{}",
        substring.output
    );
    assert!(!substring.output.contains("Current file is below"));

    // One line is right and the rest is not: the neighbourhood, as nearest text.
    let nearest = patch("fn not_there() {\n    let unique_marker_value = 42;\n    changed();\n}");
    assert!(!nearest.success, "{}", nearest.output);
    assert!(
        nearest.output.contains("Nearest text is around line 126, where one line of expected occurs exactly once (lines 122-131)"),
        "{}",
        nearest.output
    );
    assert!(
        nearest
            .output
            .contains("    let unique_marker_value = 42;\n    keep();"),
        "{}",
        nearest.output
    );
    assert!(!nearest.output.contains("Current file is below"));
    assert!(nearest.output.len() < 1024, "{}", nearest.output.len());

    // Copied search markers are not file text.
    let marked = patch("62: fn main() {\n63-     let value = compute(1, 2);");
    assert!(!marked.success, "{}", marked.output);
    assert!(
        marked.output.contains("starts with a search marker"),
        "{}",
        marked.output
    );
    assert!(!marked.output.contains("Current file is below"));

    // Nothing to anchor on: the whole-file recovery stays.
    let none = patch("nothing like it\nat all, anywhere");
    assert!(
        none.output.contains("Current file is below")
            || none.output.contains("Current file edges are below"),
        "{}",
        none.output
    );
    assert_eq!(fs::read_to_string(root.0.join("t.rs")).unwrap(), body);
}

/// "Same file name elsewhere" answers a wrong directory, nothing else.
#[test]
fn the_same_name_note_is_only_for_a_missing_file_and_never_names_secrets() {
    let root = Workspace::new();
    fs::create_dir_all(root.0.join("crates/core/src")).unwrap();
    fs::write(root.0.join("crates/core/src/Config.rs"), "x\n").unwrap();
    fs::write(root.0.join("crates/core/src/.env"), "SECRET=1\n").unwrap();
    let eleven_mib = "x".repeat(11 * 1024 * 1024);
    // Creating a file, or failing on its size, is not a wrong directory.
    for args in [
        json!({"path":"src/config.rs","content":"y"}),
        json!({"path":"src/config.rs","content": eleven_mib}),
        json!({"path":"src/config.rs","content": eleven_mib, "expected":"x\n"}),
    ] {
        let result = root.call("write", args);
        assert!(
            !result.output.contains("Same file name"),
            "{}",
            &result.output[..result.output.len().min(300)]
        );
    }
    fs::remove_file(root.0.join("src/config.rs")).ok();
    // A write that checks an EXISTING file is the wrong-directory case.
    let wrong = root.call(
        "write",
        json!({"path":"src/config.rs","content":"y","expected":"x\n"}),
    );
    assert!(
        wrong
            .output
            .ends_with("\nSame file name elsewhere: crates/core/src/Config.rs"),
        "{}",
        wrong.output
    );
    // A secret with the same name is never suggested.
    let secret = root.call("read", json!({"path":"src/.env"}));
    assert!(!secret.success);
    assert!(
        !secret.output.contains("Same file name"),
        "{}",
        secret.output
    );
    assert!(!secret.output.contains(".env\n"), "{}", secret.output);
}

#[test]
fn truthful_recovery_text_for_one_line_files_quotes_and_utf32() {
    let root = Workspace::new();
    fs::write(root.0.join("one.txt"), "only\n").unwrap();
    let past = root.call("read", json!({"path":"one.txt","offset":2}));
    assert_eq!(past.output, "[offset 2 is past the end; file has 1 line]");
    let mut utf32 = vec![0xFF, 0xFE, 0x00, 0x00];
    utf32.extend("a".chars().flat_map(|c| (c as u32).to_le_bytes()));
    fs::write(root.0.join("it's utf32.txt"), &utf32).unwrap();
    let wide = root.call("read", json!({"path":"it's utf32.txt"}));
    assert!(!wide.success);
    assert!(
        wide.output.contains("a UTF-32 LE file (BOM detected)"),
        "{}",
        wide.output
    );
    assert!(!wide.output.contains("UTF-16"), "{}", wide.output);
    // The quote inside the name is doubled in the command to run.
    assert!(
        wide.output
            .contains("Get-Content -LiteralPath 'it''s utf32.txt' -Encoding UTF32"),
        "{}",
        wide.output
    );
    fs::write(root.0.join("it's long.txt"), "x".repeat(1024 * 1024 + 1)).unwrap();
    let long = root.call("read", json!({"path":"it's long.txt"}));
    assert!(
        long.output
            .contains("(Get-Content -LiteralPath 'it''s long.txt')[0]"),
        "{}",
        long.output
    );
    let huge = fs::File::create(root.0.join("it's huge.log")).unwrap();
    huge.set_len(10 * 1024 * 1024 + 1).unwrap();
    drop(huge);
    let huge = root.call("read", json!({"path":"it's huge.log"}));
    assert!(
        huge.output
            .contains("-LiteralPath 'it''s huge.log' -TotalCount 200"),
        "{}",
        huge.output
    );
}

#[test]
fn multi_edit_recovery_says_its_line_numbers_count_the_proposed_content() {
    let root = Workspace::new();
    fs::write(root.0.join("m.txt"), "a\nb\nc\n  target( x )\nd\n").unwrap();
    let edits = |first: &str| {
        root.call(
            "patch",
            json!({"path":"m.txt","edits":[
                {"expected":"a\n","replacement":first},
                {"expected":"target(x)","replacement":"y"}
            ]}),
        )
    };
    let failed = edits("a\nnew1\nnew2\n");
    assert!(!failed.success, "{}", failed.output);
    // Line 4 on disk, line 6 once the first edit has added two lines.
    assert!(
        failed
            .output
            .contains("Edit 2 rejected in proposed content; no edits applied. Line numbers below count the proposed content (edit 1 applied)."),
        "{}",
        failed.output
    );
    assert!(
        failed.output.contains("Closest text is at line 6"),
        "{}",
        failed.output
    );
    // The first edit's own failure counts the file on disk: no such sentence.
    let first = root.call(
        "patch",
        json!({"path":"m.txt","edits":[
            {"expected":"target(x)","replacement":"y"},
            {"expected":"a\n","replacement":"z\n"}
        ]}),
    );
    assert!(first.output.contains("Edit 1 rejected"), "{}", first.output);
    assert!(
        !first.output.contains("Line numbers below"),
        "{}",
        first.output
    );
    assert!(
        first.output.contains("Closest text is at line 4"),
        "{}",
        first.output
    );
    assert_eq!(
        fs::read_to_string(root.0.join("m.txt")).unwrap(),
        "a\nb\nc\n  target( x )\nd\n"
    );
}

/// The excerpt functions share one work budget: a huge file with a long
/// near-match falls back to the current-file text instead of stalling.
#[test]
fn patch_recovery_on_a_huge_near_match_is_bounded() {
    let root = Workspace::new();
    fs::write(root.0.join("big.txt"), "x\n".repeat(200_000)).unwrap();
    let expected = format!("{}y", "x \n".repeat(1_999));
    let started = std::time::Instant::now();
    let result = root.call(
        "patch",
        json!({"path":"big.txt","edits":[{"expected": expected, "replacement":"z"}]}),
    );
    let elapsed = started.elapsed();
    assert!(!result.success);
    assert!(
        result.output.contains("Current file edges are below"),
        "{}",
        &result.output[..result.output.len().min(400)]
    );
    assert!(elapsed < std::time::Duration::from_secs(4), "{elapsed:?}");
    assert_eq!(
        fs::read_to_string(root.0.join("big.txt")).unwrap().len(),
        400_000
    );
}
