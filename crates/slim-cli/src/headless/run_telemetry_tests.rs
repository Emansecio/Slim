use super::{
    durable_run_telemetry_context, empty_provider_execution, executable_identity,
    executable_identity_from_path, input_required_result,
    run_provider_headless_with_session_and_options, run_telemetry_terminal, ExitCode,
    ProviderRequest, ProviderRunOptions, ToolLoopLimits,
};
use slim_core::runtime::CancellationToken;
use slim_core::session::{preflight_session, DurableOperationKind, DurableOutcome, DurableRecord};
use slim_core::{OperatingMode, ProviderKind, RequestKind, RequestUsage};
use std::fs;
use std::time::Duration;

fn request() -> ProviderRequest {
    ProviderRequest {
        prompt: "prompt-marker".into(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint: "http://127.0.0.1:1".into(),
        model: "fixture-main".into(),
        api_key: "secret-marker".into(),
        account_id: None,
        timeout: Duration::from_secs(1),
    }
}

#[test]
fn context_exposes_explicit_benchmark_ids_without_prompt_data() {
    let options = ProviderRunOptions::default()
        .with_experiment_id("exp-a")
        .with_task_id("task-7");
    let context = durable_run_telemetry_context(&request(), &options).unwrap();
    assert_eq!(context.experiment_id.as_deref(), Some("exp-a"));
    assert_eq!(context.task_id.as_deref(), Some("task-7"));
    assert_eq!(context.mode, OperatingMode::Auto);
    assert_eq!(context.provider, "openai-compatible");
    assert_eq!(context.model, "fixture-main");
    assert!(context.executable_sha256.is_some());
    assert_eq!(context.executable_sha256.as_ref().unwrap().len(), 64);
    assert!(context
        .executable_sha256
        .as_ref()
        .unwrap()
        .chars()
        .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character)));
    assert!(context.executable_identity_error.is_none());
    assert_eq!(context.limits["resolved"], false);
    let debug = format!("{context:?}");
    assert!(!debug.contains("secret-marker"));
    assert!(!debug.contains("prompt-marker"));
}

#[test]
fn executable_identity_is_cached_and_unavailable_identity_is_explicit() {
    let first = executable_identity();
    let second = executable_identity();
    assert!(std::ptr::eq(first, second));
    assert_eq!(first.sha256.as_ref().map(String::len), Some(64));

    let unavailable = executable_identity_from_path(std::path::Path::new("\0invalid"));
    assert_eq!(unavailable.sha256, None);
    assert_eq!(unavailable.error.as_deref(), Some("executable_read_failed"));
}

#[test]
fn terminal_usage_is_aggregate_and_records_limits() {
    let request = request();
    let mut execution = empty_provider_execution(input_required_result(&request));
    execution.result.stop = "provider_completed".into();
    execution.result.validation_source = Some("derived_runtime".into());
    execution.result.usage.requests.push(RequestUsage {
        request_kind: RequestKind::ProviderTurn,
        provider: "openai-compatible".into(),
        model: "fixture-main".into(),
        uncached_input_tokens: 13,
        ..RequestUsage::default()
    });
    execution.result.usage.uncached_input_tokens = 13;
    execution.result.usage.output_tokens = 5;
    execution.result.usage.provider_turns = 2;
    execution.result.usage.validated_completion = true;
    execution.limits = ToolLoopLimits {
        max_mutating_tool_calls: 3,
        max_read_tool_calls: 4,
        max_total_tool_calls: 5,
        max_turns: 6,
        max_output_tokens: 7,
        max_result_bytes: 8,
        context_window_tokens: 9,
    };

    let terminal = run_telemetry_terminal(&execution, DurableOutcome::Success).unwrap();
    assert_eq!(terminal.usage["request_count"], 1);
    assert_eq!(terminal.usage["total_input_tokens"], 13);
    assert_eq!(terminal.usage["total_tokens"], 18);
    assert_eq!(terminal.usage["provider_turns"], 2);
    assert!(terminal.usage.get("requests").is_none());
    assert_eq!(terminal.limits["resolved"], true);
    assert_eq!(terminal.limits["context_window_tokens"], 9);
    assert_eq!(terminal.limits["max_result_bytes"], 8);
    assert!(terminal.validated_completion);
}

#[test]
fn cancelled_durable_run_writes_start_and_terminal_envelopes_end_to_end() {
    let root = std::env::temp_dir().join(format!(
        "slim-run-telemetry-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let session_path = root.join("session.jsonl");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut options = ProviderRunOptions::default()
        .with_context_window_tokens(32_000)
        .with_workspace_root(&root)
        .with_experiment_id("cancel-exp")
        .with_task_id("cancel-task");
    options.cancellation = Some(cancellation);
    let mut request = request();
    request.mode = OperatingMode::Auto;
    request.api_key = "must-not-be-persisted".into();

    let result =
        run_provider_headless_with_session_and_options(request, &session_path, options).unwrap();
    assert_eq!(result.code, ExitCode::Cancelled);

    let report = preflight_session(&session_path).unwrap();
    let telemetry = report
        .records
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Fact { fact, .. } if fact.namespace == "run.telemetry.v1" => Some(fact),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(telemetry.len(), 2);
    assert_eq!(telemetry[0].value["phase"], "started");
    assert_eq!(telemetry[0].value["mode"], "auto");
    assert_eq!(telemetry[0].value["experiment_id"], "cancel-exp");
    assert_eq!(telemetry[0].value["task_id"], "cancel-task");
    assert_eq!(telemetry[1].value["phase"], "terminal");
    assert_eq!(telemetry[1].value["outcome"], "cancelled");
    assert_eq!(telemetry[1].value["stop"], "cancelled");
    assert!(telemetry[1].value["limits"]["resolved"].as_bool().unwrap());
    let aborted = report
        .records
        .iter()
        .position(|record| {
            matches!(record, DurableRecord::Operation { operation, .. }
            if matches!(&operation.kind, DurableOperationKind::Aborted))
        })
        .unwrap();
    let terminal_fact = report
        .records
        .iter()
        .position(|record| {
            matches!(record, DurableRecord::Fact { fact, .. }
            if fact.namespace == "run.telemetry.v1"
                && fact.value["phase"] == "terminal")
        })
        .unwrap();
    assert!(aborted < terminal_fact);
    assert!(!fs::read_to_string(&session_path)
        .unwrap()
        .contains("must-not-be-persisted"));

    fs::remove_dir_all(root).unwrap();
}
