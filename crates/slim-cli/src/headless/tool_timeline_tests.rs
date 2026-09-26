use super::{collect_tool_job_outputs, summarize_tool_events};
use slim_core::{EventKind, SessionEvent};

fn finished(seq: u64, batch_id: &str, name: &str, success: bool, duration_ms: u64) -> SessionEvent {
    SessionEvent::new(
        seq,
        EventKind::ToolFinished {
            batch_id: batch_id.into(),
            call_id: format!("call-{seq}"),
            name: name.into(),
            success,
            duration_ms,
        },
    )
}

#[test]
fn groups_only_consecutive_successful_members_of_one_batch() {
    let events = vec![
        finished(1, "a", "read", true, 7),
        finished(2, "a", "read", true, 8),
        finished(3, "a", "shell", true, 9),
        finished(4, "a", "write", false, 4),
        finished(5, "a", "read", true, 5),
        finished(6, "b", "search", true, 6),
    ];
    assert_eq!(
        summarize_tool_events(&events),
        [
            "✓ 3 tools · read ×2, shell · 24ms",
            "✕ write · failed · 4ms",
            "✓ read · 5ms",
            "✓ search · 6ms",
        ]
    );
}

#[test]
fn timeline_never_includes_arguments_or_output_events() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ToolStarted {
                batch_id: "a".into(),
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: "secret-path".into(),
            },
        ),
        SessionEvent::new(
            2,
            EventKind::ToolOutput {
                batch_id: "a".into(),
                call_id: "call-1".into(),
                name: "read".into(),
                output: "secret-output".into(),
            },
        ),
        finished(3, "a", "read", true, 2),
    ];
    let timeline = summarize_tool_events(&events).join("\n");
    assert_eq!(timeline, "✓ read · 2ms");
    assert!(!timeline.contains("secret"));
}

#[test]
fn failed_tool_line_carries_first_output_line_as_reason() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ToolOutput {
                batch_id: "a".into(),
                call_id: "call-9".into(),
                name: "shell".into(),
                output: "exit 1: file not found\nmore details here".into(),
            },
        ),
        SessionEvent::new(
            2,
            EventKind::ToolFinished {
                batch_id: "a".into(),
                call_id: "call-9".into(),
                name: "shell".into(),
                success: false,
                duration_ms: 3,
            },
        ),
    ];
    assert_eq!(
        summarize_tool_events(&events),
        ["✕ shell · failed · exit 1: file not found · 3ms"]
    );
}

#[test]
fn failed_background_job_uses_final_output_instead_of_running_acknowledgment() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ToolOutput {
                batch_id: "a".into(),
                call_id: "call-9".into(),
                name: "shell".into(),
                output: "job_id=shell-1 state=running".into(),
            },
        ),
        SessionEvent::new(
            2,
            EventKind::ToolJobOutput {
                batch_id: "a".into(),
                call_id: "call-9".into(),
                name: "shell".into(),
                output: "exit 1: final failure\nmore details".into(),
            },
        ),
        SessionEvent::new(
            3,
            EventKind::ToolFinished {
                batch_id: "a".into(),
                call_id: "call-9".into(),
                name: "shell".into(),
                success: false,
                duration_ms: 42,
            },
        ),
    ];
    assert_eq!(
        summarize_tool_events(&events),
        ["✕ shell · failed · exit 1: final failure · 42ms"]
    );
    let outputs = collect_tool_job_outputs(&events);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].call_id, "call-9");
    assert_eq!(outputs[0].output, "exit 1: final failure\nmore details");
}

#[test]
fn process_facts_add_bounded_status_to_timeline_without_output_text() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ToolOutput {
                batch_id: "a".into(),
                call_id: "call-1".into(),
                name: "shell".into(),
                output: "secret output".into(),
            },
        ),
        SessionEvent::new(
            2,
            EventKind::ToolProcessFinished {
                batch_id: "a".into(),
                call_id: "call-1".into(),
                name: "shell".into(),
                process: slim_core::process::ProcessExecutionFacts {
                    exit_code: Some(3),
                    timed_out: false,
                    cancelled: true,
                    capture_may_be_incomplete: false,
                    stdout_bytes: 4,
                    stderr_bytes: 2,
                    stdout_discarded_bytes: 1,
                    stderr_discarded_bytes: 2,
                },
            },
        ),
        SessionEvent::new(
            3,
            EventKind::ToolFinished {
                batch_id: "a".into(),
                call_id: "call-1".into(),
                name: "shell".into(),
                success: false,
                duration_ms: 4,
            },
        ),
    ];
    let timeline = summarize_tool_events(&events).join("\n");
    assert_eq!(
        timeline,
        "✕ shell · failed · secret output · process: exit 3 · cancelled · discarded 3 B · 4ms"
    );
    assert!(!timeline.contains("stdout"));
}
