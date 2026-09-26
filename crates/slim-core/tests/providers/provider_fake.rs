use slim_core::{FakeProvider, ProviderError, ProviderEvent};

#[test]
fn fake_provider_emits_normalized_success_stream_without_network() {
    let mut provider = FakeProvider::success();

    assert_eq!(provider.model(), "fake-model");
    assert_eq!(
        provider.next_event(),
        Some(ProviderEvent::TextDelta("hello".into()))
    );
    assert_eq!(
        provider.next_event(),
        Some(ProviderEvent::ReasoningDelta("thinking".into()))
    );
    assert_eq!(
        provider.next_event(),
        Some(ProviderEvent::ToolCall {
            name: "read".into(),
            arguments: "{}".into(),
        })
    );
    assert_eq!(
        provider.next_event(),
        Some(ProviderEvent::Usage {
            input_tokens: 3,
            output_tokens: 2,
        })
    );
    assert_eq!(
        provider.next_event(),
        Some(ProviderEvent::Stopped {
            reason: "end_turn".into(),
        })
    );
    assert_eq!(provider.next_event(), None);
}

#[test]
fn fake_provider_marks_only_safe_transport_failures_as_retryable() {
    assert!(FakeProvider::transport_failure(true)
        .next_error()
        .expect("transport error")
        .is_retryable());
    assert!(!FakeProvider::transport_failure(false)
        .next_error()
        .expect("transport error")
        .is_retryable());
    assert!(!ProviderError::MalformedToolCall.is_retryable());
    assert!(!ProviderError::Cancelled.is_retryable());
}
