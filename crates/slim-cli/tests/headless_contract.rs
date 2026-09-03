use slim_cli::{
    redact, render_jsonl, render_provider_jsonl, render_provider_text,
    render_provider_verbose_text, render_text, run_fake_headless, Config, ExitCode,
    HeadlessRequest, OutputFormat, ProviderHeadlessResult,
};
use slim_core::provider::ProviderKind;
use slim_core::runtime::CancellationToken;
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
fn provider_jsonl_preserves_usage_completeness_and_overflow() {
    let result = ProviderHeadlessResult {
        code: ExitCode::Provider,
        provider: ProviderKind::OpenAiCompatible,
        model: "fixture".into(),
        text: "partial".into(),
        input_tokens: Some(u64::MAX),
        output_tokens: Some(3),
        stop_reason: Some("cancelled".into()),
        stop: "cancelled".into(),
        cost_micros: None,
        usage_complete: false,
        usage_overflowed: true,
        usage: slim_core::UsageTotals {
            uncached_input_tokens: 10,
            cache_read_tokens: 30,
            usage_unknown: true,
            overflowed: true,
            ..slim_core::UsageTotals::default()
        },
        costs: Default::default(),
        validation_source: None,
        tool_summary_lines: Vec::new(),
    };
    let value: serde_json::Value = serde_json::from_str(
        render_provider_jsonl(&result)
            .expect("provider JSONL")
            .trim(),
    )
    .expect("JSON");
    assert_eq!(value["usage_complete"], false);
    assert_eq!(value["usage_overflowed"], true);
    assert_eq!(value["input_tokens"], u64::MAX);
    assert_eq!(value["usage"]["compaction_tokens_saved_estimated"], true);
    assert!(value.get("cache_hit_ratio").is_none());
    assert!(render_provider_verbose_text(&result).contains("estimation_error=?"));
    assert_eq!(render_provider_text(&result), "partial\n");
}

#[test]
fn verbose_provider_text_prepends_only_redacted_tool_summaries() {
    let result = ProviderHeadlessResult {
        code: ExitCode::Success,
        provider: ProviderKind::OpenAiCompatible,
        model: "fixture".into(),
        text: "done".into(),
        input_tokens: Some(4),
        output_tokens: Some(2),
        stop_reason: Some("stop".into()),
        stop: "provider_completed".into(),
        cost_micros: None,
        usage_complete: true,
        usage_overflowed: false,
        usage: slim_core::UsageTotals {
            uncached_input_tokens: 4,
            output_tokens: 2,
            provider_turns: 1,
            validated_completion: true,
            ..slim_core::UsageTotals::default()
        },
        costs: Default::default(),
        validation_source: Some("derived_runtime".into()),
        tool_summary_lines: vec!["✓ 2 tools · read, shell · 12ms".into()],
    };

    assert_eq!(
        render_provider_verbose_text(&result),
        concat!(
            "done\n\ntimeline:\n✓ 2 tools · read, shell · 12ms\n\n",
            "stop=provider_completed validation=derived_runtime\n",
            "usage_complete=true usage_unknown=false usage_overflowed=false input_tokens=4 output_tokens=2\n",
            "ledger uncached_input=4 cache_write=0 cache_read=0 reasoning=0 cache_hit_ratio=0.00%\n",
            "execution provider_turns=1 tool_calls_executed=0 tool_calls_reused=0 tool_calls_suppressed=0 no_progress_turns=0 no_progress_tokens=0\n",
            "economy duplicate_evidence_bytes_avoided=0 compaction_input=0 compaction_output=0 compaction_saved_estimated=0 post_compaction_reacquisitions=0 estimation_error=0\n",
            "cost total=? per_validated_completion=? failed_attempts=? compaction=? cancelled_estimated=?\n"
        )
    );
    assert!(!render_provider_text(&result).contains("timeline:"));
    assert!(!render_provider_jsonl(&result)
        .expect("jsonl")
        .contains("timeline"));
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

#[test]
fn cancelled_provider_headless_run_reports_cancelled_stop_and_exit() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = slim_cli::run_provider_headless_with_options(
        slim_cli::ProviderRequest {
            prompt: "must not reach provider".into(),
            mode: OperatingMode::Auto,
            kind: slim_core::provider::ProviderKind::OpenAiCompatible,
            endpoint: "http://127.0.0.1:1".into(),
            model: "fixture".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: std::time::Duration::from_millis(100),
        },
        slim_cli::ProviderRunOptions {
            cancellation: Some(cancellation),
            ..Default::default()
        },
    )
    .expect("cooperative cancellation is a headless result");

    assert_eq!(result.code, ExitCode::Cancelled);
    assert_eq!(result.stop, "cancelled");
    assert!(!result.usage_complete);
    assert!(result.usage.requests.is_empty());
}
