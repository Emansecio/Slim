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
        stale.output.contains("current file below"),
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
        stale.output.contains("current file below"),
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
    let tools = slim_core::Runtime::new().advertised_tool_definitions(OperatingMode::Auto);
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
    let lines = &read["input_schema"]["properties"]["lines"];
    assert_eq!(lines["type"], "integer");
    assert_eq!(lines["minimum"], 1);
    assert_eq!(lines["maximum"], slim_core::tools::MAX_READ_LINES_CAP);
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
    println!(
        "NATIVE_PREFIX system_bytes={} schema_bytes={}",
        slim_core::provider::NATIVE_SYSTEM_PROMPT.len(),
        serde_json::to_vec(&advertised).unwrap().len(),
    );
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
        for name in ["write", "shell", "search"] {
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
        assert_eq!(patch["input_schema"]["required"], json!(["path"]));
        assert_eq!(patch["input_schema"]["properties"]["edits"]["minItems"], 1);
        let patch_variants = patch["input_schema"]["oneOf"]
            .as_array()
            .expect("patch schema alternatives");
        assert_eq!(patch_variants.len(), 2);
        assert_eq!(patch_variants[0]["required"], json!(["edits"]));
        assert_eq!(
            patch_variants[1]["required"],
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
            vec!["content", "expected", "path"]
        );
    }
}

#[test]
fn patch_ambiguous_match_includes_suggested_excerpt() {
    let root = Workspace::new();
    fs::write(root.0.join("t.txt"), "same\nsame\n").unwrap();
    let result = root.call(
        "patch",
        json!({"path":"t.txt","edits":[{"expected":"same\n","replacement":"x\n"}]}),
    );
    assert!(!result.success, "{}", result.output);
    assert!(
        result.output.contains("Suggested unique expected"),
        "{}",
        result.output
    );
    assert!(result.output.contains("same\nsame\n"), "{}", result.output);
    assert_eq!(
        fs::read_to_string(root.0.join("t.txt")).unwrap(),
        "same\nsame\n"
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
        corrupted.output.contains("verbatim"),
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
