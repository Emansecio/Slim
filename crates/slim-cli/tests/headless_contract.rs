use slim_cli::{
    redact, render_jsonl, render_text, run_fake_headless, Config, ExitCode, HeadlessRequest,
    OutputFormat,
};
use slim_core::OperatingMode;
use std::sync::{Mutex, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
fn plan_headless_stops_at_explicit_approval_without_running_tools() {
    let result = run_fake_headless(HeadlessRequest {
        prompt: "inspect workspace".into(),
        mode: OperatingMode::Plan,
    });

    assert_eq!(result.code, ExitCode::ApprovalRequired);
    assert_eq!(result.message, "approval_required");
}

#[test]
fn text_and_jsonl_renderers_are_stable() {
    let result = run_fake_headless(HeadlessRequest {
        prompt: "hello".into(),
        mode: OperatingMode::Auto,
    });
    assert_eq!(render_text(&result), "success\n");
    assert_eq!(
        render_jsonl(&result).expect("jsonl"),
        "{\"version\":1,\"kind\":\"success\"}\n"
    );
    assert_eq!(OutputFormat::default(), OutputFormat::Text);
}

#[test]
fn configuration_precedence_is_cli_then_environment_then_project_then_global() {
    let value = Config::resolve(Some("cli"), Some("env"), Some("project"), Some("global"));
    assert_eq!(value, Some("cli"));
    assert_eq!(
        Config::resolve(None, Some("env"), Some("project"), Some("global")),
        Some("env")
    );
    assert_eq!(
        Config::resolve(None, None, Some("project"), Some("global")),
        Some("project")
    );
    assert_eq!(
        Config::resolve(None, None, None, Some("global")),
        Some("global")
    );
}

#[test]
fn known_auth_values_are_redacted() {
    assert_eq!(
        redact("Authorization: Bearer secret"),
        "Authorization: [REDACTED]"
    );
    assert_eq!(
        redact("x-api-key: secret\nbody"),
        "x-api-key: [REDACTED]\nbody"
    );
}

#[test]
fn invalid_token_caps_fail_without_echoing_environment_values() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let previous_context = std::env::var_os("SLIM_CONTEXT_WINDOW_TOKENS");
    let previous_output = std::env::var_os("SLIM_MAX_OUTPUT_TOKENS");
    std::env::set_var("SLIM_CONTEXT_WINDOW_TOKENS", "not-a-secret-context-value");
    std::env::remove_var("SLIM_MAX_OUTPUT_TOKENS");
    let result = slim_cli::run_provider_headless(slim_cli::ProviderRequest {
        prompt: "hello".into(),
        mode: OperatingMode::Auto,
        kind: slim_core::provider::ProviderKind::OpenAiCompatible,
        endpoint: "http://127.0.0.1:1".into(),
        model: "fixture".into(),
        api_key: "fixture-secret".into(),
        account_id: None,
        timeout: std::time::Duration::from_millis(20),
    });
    if let Some(value) = previous_context {
        std::env::set_var("SLIM_CONTEXT_WINDOW_TOKENS", value);
    } else {
        std::env::remove_var("SLIM_CONTEXT_WINDOW_TOKENS");
    }
    if let Some(value) = previous_output {
        std::env::set_var("SLIM_MAX_OUTPUT_TOKENS", value);
    } else {
        std::env::remove_var("SLIM_MAX_OUTPUT_TOKENS");
    }
    let error = result.expect_err("invalid context cap");
    match error {
        slim_core::ProviderError::InvalidResponse { message } => {
            assert!(message.contains("SLIM_CONTEXT_WINDOW_TOKENS"));
            assert!(!message.contains("not-a-secret-context-value"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
