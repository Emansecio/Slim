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
fn projected_reserve_can_trigger_compaction_before_threshold() {
    let budget = ContextBudget::new(32_000, 26_000, 2_100);
    assert!(budget.should_compact());
    assert!(budget.can_fit(5_999));
    assert!(!budget.can_fit(6_001));
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
