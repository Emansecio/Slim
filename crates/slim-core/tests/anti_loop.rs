use slim_core::runtime::LoopGuard;

#[test]
fn identical_failed_tool_call_is_blocked_on_second_attempt() {
    let mut guard = LoopGuard::default();
    assert!(guard.accept("read", "{}", "missing"));
    assert!(!guard.accept("read", "{}", "missing"));
    assert!(guard.accept("read", "{\"path\":\"other\"}", "missing"));
    assert!(guard.accept("read", r#"{"path":"a","offset":1}"#, "missing"));
    assert!(!guard.accept("read", r#"{ "offset": 1, "path": "a" }"#, "missing"));
}

#[test]
fn repeated_failed_shell_command_is_blocked_even_with_differing_output() {
    let mut guard = LoopGuard::default();
    assert!(guard.accept(
        "shell",
        "{\"command\":\"cargo test\"}",
        "failed in 1.2s at 12:00:00"
    ));
    // Same command, differing error/timestamp output -> blocked
    assert!(!guard.accept(
        "shell",
        "{\"command\":\"cargo test\"}",
        "failed in 1.4s at 12:00:05"
    ));
    assert!(guard.accept(
        "shell",
        r#"{"command":"cargo test","timeout_ms":120000}"#,
        "timeout"
    ));
}

#[test]
fn failed_shell_command_is_unblocked_after_mutation_or_success() {
    let mut guard = LoopGuard::default();
    assert!(guard.accept("shell", "{\"command\":\"cargo test\"}", "err1"));
    assert!(guard.accept("patch", "{}", "expected text not found"));
    // Mutation happens
    guard.record_success("patch");
    // Allowed to retry after fix
    assert!(guard.accept("shell", "{\"command\":\"cargo test\"}", "err2"));
    assert!(guard.accept("patch", "{}", "expected text not found"));
    guard.record_success("ask_question");
    assert!(guard.accept("shell", "{\"command\":\"cargo test\"}", "err3"));
}

#[test]
fn identical_failed_write_is_blocked_only_when_consecutive() {
    let mut guard = LoopGuard::default();
    assert!(guard.accept(
        "write",
        r#"{"path":"a","content":"x"}"#,
        "precondition required"
    ));
    assert!(guard.accept("read", r#"{"path":"b"}"#, "missing"));
    assert!(guard.accept(
        "write",
        r#"{"path":"a","content":"x"}"#,
        "precondition required"
    ));
    assert!(!guard.accept(
        "write",
        r#"{"path":"a","content":"x"}"#,
        "precondition required"
    ));
    guard.record_success("write");
    assert!(guard.accept(
        "write",
        r#"{"path":"a","content":"x"}"#,
        "precondition required"
    ));
}

#[test]
fn identical_failed_patch_is_blocked_only_when_consecutive() {
    let mut guard = LoopGuard::default();
    assert!(guard.accept("patch", r#"{"path":"a","edits":[]}"#, "no match"));
    assert!(guard.accept("search", r#"{"query":"a"}"#, "no hits"));
    assert!(guard.accept("patch", r#"{"path":"a","edits":[]}"#, "no match"));
    assert!(!guard.accept("patch", r#"{"path":"a","edits":[]}"#, "no match"));
}
