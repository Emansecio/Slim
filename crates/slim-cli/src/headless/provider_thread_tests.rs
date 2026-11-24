use super::{provider_runtime_block_on, AGENT_LOOP_STACK_BYTES};

#[test]
fn provider_futures_poll_on_a_dedicated_large_stack() {
    let used = provider_runtime_block_on(async {
        // Twice the Windows main-thread stack, in one poll.
        let frame = std::hint::black_box([1_u8; 2 * 1024 * 1024]);
        frame.iter().map(|&byte| usize::from(byte)).sum::<usize>()
    })
    .expect("provider runtime");
    assert_eq!(used, 2 * 1024 * 1024);
    const { assert!(AGENT_LOOP_STACK_BYTES >= 8 * 1024 * 1024) };
}

#[test]
fn provider_future_panic_resumes_on_the_caller() {
    let panic =
        std::panic::catch_unwind(|| provider_runtime_block_on(async { panic!("fixture panic") }))
            .expect_err("panic must reach the caller");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"fixture panic"));
}

#[test]
fn devin_only_off_effort_is_rejected_elsewhere_before_network() {
    let request = super::ProviderRequest {
        prompt: "test".into(),
        mode: slim_core::OperatingMode::Auto,
        kind: slim_core::provider::ProviderKind::Anthropic,
        endpoint: "http://127.0.0.1:9".into(),
        model: "claude-opus-4-6".into(),
        api_key: String::new(),
        account_id: None,
        timeout: std::time::Duration::from_secs(1),
    };
    let options = super::ProviderRunOptions {
        reasoning_effort: Some("off".into()),
        ..Default::default()
    };
    let result = super::provider_runtime_block_on(super::execute_provider_turn_async(
        request, false, options, None, None, None,
    ))
    .expect("provider runtime");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("off must not reach a non-Devin provider"),
    };
    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { ref message }
            if message.contains("only supported by Devin")
    ));
}
