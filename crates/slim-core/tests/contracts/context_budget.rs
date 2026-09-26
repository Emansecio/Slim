use slim_core::context::{
    build_bounded_summary_prompt, estimate_provider_message_tokens, ContextBudget,
};
use slim_core::provider::ProviderMessage;

#[test]
fn ordinary_windows_compact_at_eighty_five_percent() {
    assert!(!ContextBudget::new(32_000, 27_199, 0).should_compact());
    assert!(ContextBudget::new(32_000, 27_200, 0).should_compact());
}

#[test]
fn very_large_windows_use_the_fifty_percent_threshold() {
    assert!(!ContextBudget::new(1_000_000, 499_999, 0).should_compact());
    assert!(ContextBudget::new(1_000_000, 500_000, 0).should_compact());
}

#[test]
fn reserve_does_not_trigger_compaction_before_usage_threshold() {
    let budget = ContextBudget::new(32_000, 26_000, 2_100);
    assert!(!budget.should_compact());
    assert!(budget.can_fit(5_999));
    assert!(!budget.can_fit(6_001));
}

#[test]
fn large_output_reserve_does_not_compact_low_conversation_usage() {
    assert!(!ContextBudget::new(1_000_000, 126_000, 384_000).should_compact());
    assert!(!ContextBudget::new(1_000_000, 299_999, 384_000).should_compact());
    assert!(ContextBudget::new(1_000_000, 500_000, 384_000).should_compact());
}

#[test]
fn reserve_triggers_compaction_only_when_output_would_not_fit() {
    assert!(!ContextBudget::new(32_000, 16_000, 16_000).should_compact());
    assert!(ContextBudget::new(32_000, 16_001, 16_000).should_compact());
}

#[test]
fn provider_message_estimator_is_deterministic_and_named_as_an_estimate() {
    let messages = [ProviderMessage::user("abcd")];
    let first = estimate_provider_message_tokens(&messages);
    let second = estimate_provider_message_tokens(&messages);
    assert_eq!(first, second);
    assert!(first > 0);
    assert!(!ContextBudget::new(first * 100, first, 0).should_compact());
}

#[test]
fn bounded_summary_prompt_respects_window_and_reserve_or_fails_explicitly() {
    let messages = [ProviderMessage::user("root instruction")];
    let prompt = build_bounded_summary_prompt(&messages, 100, 20).expect("bounded prompt");
    assert!(estimate_provider_message_tokens(&[ProviderMessage::user(prompt)]) + 20 <= 100);

    let error = build_bounded_summary_prompt(&messages, 8, 0).expect_err("no viable budget");
    assert!(error.contains("summary request"));
}
