use std::process::Command;

use slim_cli::{run_cli, run_tui, ExitCode};

#[test]
fn stdin_is_used_when_prompt_argument_is_absent() {
    let output = run_cli(["--read-only"], "hello from stdin");
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(output.stdout, "success\n");
}

#[test]
fn plan_and_jsonl_flags_are_executable_contracts() {
    let output = run_cli(["--plan", "--jsonl", "--prompt", "make a plan"], "");
    assert_eq!(output.code, ExitCode::ApprovalRequired);
    assert_eq!(
        output.stdout,
        "{\"version\":1,\"kind\":\"approval_required\"}\n"
    );
}

#[test]
fn binary_defaults_to_tui_and_headless_requires_its_flag() {
    let tui = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--provider", "unsupported"])
        .output()
        .expect("default tui");
    assert_eq!(tui.status.code(), Some(ExitCode::Provider.as_i32()));
    assert!(String::from_utf8_lossy(&tui.stderr).contains("tui error: unsupported provider"));

    let headless = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--headless", "--read-only", "--prompt", "hello"])
        .output()
        .expect("headless");
    assert!(headless.status.success());
    assert_eq!(String::from_utf8_lossy(&headless.stdout), "success\n");
}

#[test]
fn tui_startup_preserves_provider_error_class() {
    let error = run_tui(vec![
        "--tui".into(),
        "--provider".into(),
        "unsupported".into(),
    ])
    .expect_err("unsupported provider");
    assert_eq!(error.code(), ExitCode::Provider);
}

#[test]
fn tui_help_uses_the_normal_cli_contract_without_opening_fullscreen() {
    let output = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--tui", "--help"])
        .output()
        .expect("binary help");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("Slim coding agent\nUsage:"));
}

#[test]
fn help_version_and_unknown_flags_are_stable() {
    assert_eq!(
        run_cli(["--help"], "").stdout,
        "Slim coding agent\nUsage: Slim [TUI OPTIONS]\n       Slim --headless [--plan|--read-only|--jsonl|--provider NAME|--model MODEL|--endpoint URL|--session PATH|--image PATH|--prompt TEXT]\n"
    );
    assert_eq!(run_cli(["--version"], "").stdout, "slim 0.1.0\n");
    assert_eq!(run_cli(["--unknown"], "").code, ExitCode::Internal);
}
