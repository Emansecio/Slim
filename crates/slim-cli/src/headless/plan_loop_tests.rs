use super::{execute_provider_turn, ProviderRequest, ProviderRunOptions};
use crate::exit_codes::ExitCode;
use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;

#[test]
fn headless_plan_still_aborts_without_allow_plan_loop() {
    let execution = execute_provider_turn(
        ProviderRequest {
            prompt: "plan this".into(),
            mode: OperatingMode::Plan,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: "http://127.0.0.1:1".into(),
            model: "unused".into(),
            api_key: "secret".into(),
            account_id: None,
            timeout: std::time::Duration::from_secs(1),
        },
        None,
        ProviderRunOptions::default(),
    )
    .expect("plan abort");
    assert_eq!(execution.result.code, ExitCode::ApprovalRequired);
    assert_eq!(execution.result.text, "approval_required");
    assert!(execution.events.is_empty());
}
