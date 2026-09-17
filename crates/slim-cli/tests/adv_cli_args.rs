//! Adversarial coverage for the hand-rolled CLI parser and recovery gates in
//! `cli.rs` (RODADA 2 — Sifter). All cases stay hermetic: they exit before
//! provider config/auth resolution, so no network or user environment is
//! touched. The session fixtures are crafted v1/v2 JSONL files in unique temp
//! directories.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use slim_cli::{run_cli, ExitCode};
use slim_core::session::{DurableSessionHeader, JsonlRepo, SessionHeader};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let dir = root.join(format!(
        "slim-adv-cli-{}-{nanos}-{nonce}-{label}",
        std::process::id()
    ));
    fs::create_dir(&dir).expect("create unique test directory");
    fs::canonicalize(dir).expect("canonical test directory")
}

fn missing_path(dir: &std::path::Path) -> String {
    dir.join("does-not-exist.jsonl").display().to_string()
}

// ---------------------------------------------------------------------------
// Value-flag boundaries
// ---------------------------------------------------------------------------

/// Every value-taking flag consumes the next argument verbatim — including
/// another flag. `--prompt --jsonl` makes "--jsonl" the prompt and silently
/// drops the JSONL format request. POSIX-style parsers reject flag-looking
/// values; Slim does not. Spec gap: `--prompt --jsonl` runs the prompt
/// "--jsonl" in text mode instead of erroring.
#[test]
fn value_flags_eat_the_next_flag() {
    let output = run_cli(["--fake", "--prompt", "--jsonl"], "");
    assert_eq!(output.code, ExitCode::Success);
    // Fake headless renders text "success"; the --jsonl flag was consumed as
    // the prompt value so no JSONL envelope is emitted.
    assert_eq!(output.stdout, "success\n");
}

/// Missing values surface `Internal` (30) while every other user-input
/// problem surfaces `InputRequired` (11). Spec gap: a CLI usage error should
/// not read as an internal failure.
#[test]
fn missing_flag_value_is_internal_not_input_required() {
    for flag in [
        "--prompt",
        "--provider",
        "--model",
        "--effort",
        "--endpoint",
        "--session",
        "--resume",
        "--recover",
        "--image",
    ] {
        let output = run_cli([flag], "");
        assert_eq!(output.code, ExitCode::Internal, "{flag}");
        assert!(
            output.stderr.contains(&format!("missing value for {flag}")),
            "{flag}: {}",
            output.stderr
        );
    }
}

#[test]
fn unknown_option_is_internal() {
    let output = run_cli(["--bogus"], "");
    assert_eq!(output.code, ExitCode::Internal);
    assert!(output.stderr.contains("unknown option: --bogus"));
}

/// The `--` end-of-options separator is not supported: it falls into the
/// unknown-option branch. Spec gap for scripts that pass literal prompts
/// starting with `-`.
#[test]
fn double_dash_separator_is_rejected() {
    let output = run_cli(["--fake", "--", "-hello"], "");
    assert_eq!(output.code, ExitCode::Internal);
    assert!(output.stderr.contains("unknown option: --"));
}

/// `--flag=value` is not supported either: only `--flag value`.
#[test]
fn equals_form_is_rejected() {
    let output = run_cli(["--fake", "--prompt=hello"], "");
    assert_eq!(output.code, ExitCode::Internal);
    assert!(output.stderr.contains("unknown option: --prompt=hello"));
}

/// `-h`/`--help` is scanned before parsing, so it short-circuits even an
/// otherwise-invalid invocation and even mid-recovery.
#[test]
fn help_short_circuits_recovery_and_parse_errors() {
    let output = run_cli(["--recover", "/nonexistent/path", "--help"], "");
    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stdout.contains("Usage"));

    let output = run_cli(["--prompt", "--help"], "");
    assert_eq!(output.code, ExitCode::Success);
    assert!(output.stdout.contains("Usage"));
}

// ---------------------------------------------------------------------------
// Mode/flag combination matrix
// ---------------------------------------------------------------------------

#[test]
fn session_flags_are_mutually_exclusive() {
    for pair in [
        ["--session", "--resume"],
        ["--session", "--recover"],
        ["--resume", "--recover"],
    ] {
        let output = run_cli([pair[0], "a", pair[1], "b"], "");
        assert_eq!(output.code, ExitCode::InputRequired, "{pair:?}");
        assert!(output.stderr.contains("mutually exclusive"));
    }
}

#[test]
fn abandon_pending_requires_recover() {
    let output = run_cli(["--abandon-pending"], "");
    assert_eq!(output.code, ExitCode::InputRequired);
    assert!(output
        .stderr
        .contains("--abandon-pending requires --recover"));
}

#[test]
fn verbose_conflicts_with_jsonl() {
    let output = run_cli(["--fake", "--verbose", "--jsonl", "--prompt", "x"], "");
    assert_eq!(output.code, ExitCode::InputRequired);
    assert!(output.stderr.contains("--verbose"));
}

#[test]
fn recover_rejects_prompt_positional_and_stdin_input() {
    let dir = temp_dir("recover-inputs");
    let path = missing_path(&dir);
    for args in [
        vec!["--recover", &path, "--prompt", "x"],
        vec!["--recover", &path, "positional"],
    ] {
        let output = run_cli(args.iter().map(|s| s.to_string()), "");
        assert_eq!(output.code, ExitCode::InputRequired, "{args:?}");
        assert!(output.stderr.contains("recovery-only"));
    }
    // stdin content becomes the prompt, so even it trips recovery-only.
    let output = run_cli(["--recover", &path], "stdin text");
    assert_eq!(output.code, ExitCode::InputRequired);
    assert!(output.stderr.contains("recovery-only"));
}

#[test]
fn unsupported_provider_is_rejected_before_auth() {
    let output = run_cli(["--provider", "bogus", "--prompt", "x"], "");
    assert_eq!(output.code, ExitCode::Provider);
    assert!(output.stderr.contains("unsupported provider"));
}

#[test]
fn fast_flag_requires_codex_provider() {
    let output = run_cli(["--fast", "--provider", "anthropic", "--prompt", "x"], "");
    assert_eq!(output.code, ExitCode::InputRequired);
    assert!(output.stderr.contains("openai-codex"));
}

// ---------------------------------------------------------------------------
// --fake execution boundaries (fully offline)
// ---------------------------------------------------------------------------

#[test]
fn fake_empty_prompt_from_any_source_is_input_required() {
    let output = run_cli(["--fake"], "");
    assert_eq!(output.code, ExitCode::InputRequired);
    assert_eq!(output.stdout, "input_required\n");

    // Whitespace-only is still empty after trim.
    let output = run_cli(["--fake", "--prompt", "   "], "");
    assert_eq!(output.code, ExitCode::InputRequired);

    // An explicit empty --prompt falls back to positionals/stdin? No: it is
    // Some("") which stays empty — still input_required.
    let output = run_cli(["--fake", "--prompt", ""], "");
    assert_eq!(output.code, ExitCode::InputRequired);
}

#[test]
fn fake_plan_mode_needs_approval() {
    let output = run_cli(["--fake", "--plan", "--prompt", "x"], "");
    assert_eq!(output.code, ExitCode::ApprovalRequired);
    assert_eq!(output.stdout, "approval_required\n");
}

#[test]
fn fake_jsonl_and_text_rendering() {
    let text = run_cli(["--fake", "--prompt", "hi"], "");
    assert_eq!(text.stdout, "success\n");
    let jsonl = run_cli(["--fake", "--prompt", "hi", "--jsonl"], "");
    assert_eq!(jsonl.stdout, "{\"version\":1,\"kind\":\"success\"}\n");
}

#[test]
fn fake_accepts_unicode_and_large_prompts() {
    let output = run_cli(["--fake", "--prompt", "🚀 emoji 日本語"], "");
    assert_eq!(output.code, ExitCode::Success);
    let big = "x".repeat(1024 * 1024);
    let output = run_cli(["--fake", "--prompt", big.as_str()], "");
    assert_eq!(output.code, ExitCode::Success);
}

// ---------------------------------------------------------------------------
// --resume / --recover adversarial paths
// ---------------------------------------------------------------------------

#[test]
fn resume_requires_nonempty_prompt_but_accepts_positional() {
    let dir = temp_dir("resume-prompt");
    let path = missing_path(&dir);
    // No prompt at all: InputRequired.
    let output = run_cli(["--resume", &path], "");
    assert_eq!(output.code, ExitCode::InputRequired);
    assert!(output.stderr.contains("--prompt"));

    // Whitespace-only prompt is empty.
    let output = run_cli(["--resume", &path, "--prompt", "  "], "");
    assert_eq!(output.code, ExitCode::InputRequired);

    // Spec gap: the error says "requires an explicit --prompt" yet a
    // positional argument satisfies the check — the message misleads.
    let output = run_cli(["--resume", &path, "positional"], "");
    assert_eq!(
        output.code,
        ExitCode::Blocked,
        "positional satisfies prompt"
    );
    assert!(output.stderr.contains("resume preflight failed"));
}

#[test]
fn resume_missing_path_is_blocked() {
    let dir = temp_dir("resume-missing");
    let output = run_cli(["--resume", &missing_path(&dir), "--prompt", "x"], "");
    assert_eq!(output.code, ExitCode::Blocked);
    assert!(output.stderr.contains("resume preflight failed"));
}

#[test]
fn resume_legacy_v1_session_is_blocked_not_migrated() {
    let dir = temp_dir("resume-v1");
    let path = dir.join("v1.jsonl");
    let header = SessionHeader {
        record_type: "session".into(),
        schema_version: 1,
        id: "legacy".into(),
        timestamp: "2026-01-01T00:00:00Z".into(),
        cwd: "D:\\Slim".into(),
        parent_id: None,
        cutoff_seq: None,
    };
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&header).expect("header")),
    )
    .expect("write v1");
    let output = run_cli(
        ["--resume", &path.display().to_string(), "--prompt", "x"],
        "",
    );
    assert_eq!(output.code, ExitCode::Blocked);
    assert!(
        output.stderr.contains("durable schema v2"),
        "{}",
        output.stderr
    );
}

#[test]
fn resume_garbage_and_unknown_schema_are_blocked() {
    let dir = temp_dir("resume-garbage");
    for (label, contents) in [
        ("empty", String::new()),
        ("notjson", "not json\n".into()),
        (
            "v99",
            format!(
                "{}\n",
                json!({"type":"session","schema_version":99,"id":"x","timestamp":"t","cwd":"."})
            ),
        ),
    ] {
        let path = dir.join(format!("{label}.jsonl"));
        fs::write(&path, contents).expect("write");
        let output = run_cli(
            ["--resume", &path.display().to_string(), "--prompt", "x"],
            "",
        );
        assert_eq!(output.code, ExitCode::Blocked, "{label}: {}", output.stderr);
    }
}

#[test]
fn resume_healthy_v2_with_torn_tail_requires_explicit_recover() {
    let dir = temp_dir("resume-torn");
    let path = dir.join("s.jsonl");
    {
        let mut repo =
            JsonlRepo::create(&path, DurableSessionHeader::new("s", "t", ".", None, None))
                .expect("create");
        slim_core::session::DurableRepo::append(
            &mut repo,
            slim_core::session::DurableRecord::Entry {
                seq: 0,
                entry: slim_core::session::DurableEntry {
                    entry_id: "e0".into(),
                    role: slim_core::session::DurableEntryRole::User,
                    content: "hello".into(),
                    parent_entry_id: None,
                    operation_id: "op-1".into(),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    content_blocks: Vec::new(),
                },
            },
        )
        .expect("append");
    }
    // Simulate a crash mid-write.
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, b"{\"type\":\"entry\",\"seq\":1,\"en")
        })
        .expect("append torn tail");

    let output = run_cli(
        ["--resume", &path.display().to_string(), "--prompt", "x"],
        "",
    );
    assert_eq!(output.code, ExitCode::Blocked);
    assert!(
        output.stderr.contains("explicit recovery"),
        "{}",
        output.stderr
    );
}

#[test]
fn recover_nonexistent_and_directory_paths_are_blocked() {
    let dir = temp_dir("recover-missing");
    let output = run_cli(["--recover", &missing_path(&dir)], "");
    assert_eq!(output.code, ExitCode::Blocked);
    assert!(output.stderr.contains("explicit recovery rejected"));

    let output = run_cli(["--recover", &dir.display().to_string()], "");
    assert_eq!(output.code, ExitCode::Blocked);
}

#[test]
fn recover_repairs_torn_tail_and_reports_completion() {
    let dir = temp_dir("recover-torn");
    let path = dir.join("s.jsonl");
    JsonlRepo::create(&path, DurableSessionHeader::new("s", "t", ".", None, None)).expect("create");
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, b"{\"type\":\"entry\""))
        .expect("append torn");

    let output = run_cli(["--recover", &path.display().to_string()], "");
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(output.stdout, "recovery_complete\n");
    assert!(path.with_file_name("s.jsonl.quarantine").exists());
}

#[test]
fn recover_healthy_session_reports_completion_in_jsonl() {
    let dir = temp_dir("recover-ok");
    let path = dir.join("s.jsonl");
    JsonlRepo::create(&path, DurableSessionHeader::new("s", "t", ".", None, None)).expect("create");
    let output = run_cli(["--recover", &path.display().to_string(), "--jsonl"], "");
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(
        output.stdout,
        "{\"version\":1,\"kind\":\"recovery_complete\"}\n"
    );
}
