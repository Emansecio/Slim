use slim_core::tools::{summarize_tool_arguments, summarize_tool_arguments_for};

#[test]
fn empty_object_and_array_summarize_to_blank() {
    assert_eq!(summarize_tool_arguments("{}"), "");
    assert_eq!(summarize_tool_arguments("[]"), "");
}

#[test]
fn preferred_keys_beat_raw_json() {
    assert_eq!(
        summarize_tool_arguments(r#"{"path":"src/lib.rs","unused":true}"#),
        "path=src/lib.rs"
    );
    assert_eq!(
        summarize_tool_arguments(r#"{"command":"cargo test"}"#),
        "command=cargo test"
    );
    assert_eq!(
        summarize_tool_arguments(r#"{"id":"investigate"}"#),
        "id=investigate"
    );
}

#[test]
fn in_progress_todo_uses_content() {
    assert_eq!(
        summarize_tool_arguments(
            r#"{"todos":[{"id":"investigate","content":"map the leak","status":"in_progress"}]}"#
        ),
        "map the leak"
    );
}

#[test]
fn shell_summary_keeps_the_effective_timeout_visible() {
    let summary = summarize_tool_arguments_for(
        "shell",
        r#"{"command":"cargo test --workspace --all-targets","timeout_ms":120000}"#,
    );
    assert!(summary.starts_with("command="), "{summary}");
    assert!(summary.ends_with("limit 120s"), "{summary}");
    assert!(summary.chars().count() <= 48, "{summary}");
}
