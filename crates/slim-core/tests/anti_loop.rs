use slim_core::runtime::LoopGuard;

#[test]
fn identical_failed_tool_call_is_blocked_on_second_attempt() {
    let mut guard = LoopGuard::default();
    assert!(guard.accept("read", "{}", "missing"));
    assert!(!guard.accept("read", "{}", "missing"));
    assert!(guard.accept("read", "{\"path\":\"other\"}", "missing"));
}
