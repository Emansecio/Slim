use slim_core::context::ContextBudget;

#[test]
fn the_request_fits_while_usage_and_the_addition_stay_inside_the_window() {
    let budget = ContextBudget::new(32_000, 26_000, 2_100);
    assert!(budget.can_fit(5_999));
    assert!(!budget.can_fit(6_001));
}
