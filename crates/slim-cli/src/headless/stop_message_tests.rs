use super::{format_run_stop_message, ToolLoopLimits};
use slim_core::tool_call_is_read_only;
use slim_core::tools::ToolResult;

fn tool_result(name: &str) -> ToolResult {
    ToolResult {
        name: name.into(),
        success: true,
        output: "ok".into(),
        artifact: None,
        media: Vec::new(),
    }
}

#[test]
fn tool_limit_message_includes_bucket_counts_and_breakdown() {
    let results = vec![
        tool_result("search"),
        tool_result("read"),
        tool_result("write"),
    ];
    let message = format_run_stop_message(
        "tool_limit",
        &results,
        ToolLoopLimits {
            max_mutating_tool_calls: 32,
            max_read_tool_calls: 96,
            max_total_tool_calls: 256,
            max_turns: 128,
            max_output_tokens: 4096,
            max_result_bytes: 16 * 1024,
            context_window_tokens: 32_000,
        },
    );
    assert!(message.starts_with("Tool budget exhausted (read 2/96, mutating 1/32):"));
    assert!(message.contains("1 read, 1 search, 1 write"));
    assert!(message.contains("Send a follow-up to continue."));
    assert!(tool_call_is_read_only("search"));
}

#[test]
fn turn_limit_and_repeated_failed_tool_messages_are_humanized() {
    let limits = ToolLoopLimits {
        max_mutating_tool_calls: 32,
        max_read_tool_calls: 96,
        max_total_tool_calls: 256,
        max_turns: 128,
        max_output_tokens: 4096,
        max_result_bytes: 16 * 1024,
        context_window_tokens: 32_000,
    };
    assert_eq!(
        format_run_stop_message("turn_limit", &[], limits),
        "Turn limit reached (128/128). Send a follow-up to continue."
    );
    assert_eq!(
        format_run_stop_message("repeated_failed_tool", &[], limits),
        "Repeated failed tool blocked."
    );
    assert_eq!(
        format_run_stop_message("no_progress", &[], limits),
        "Stopped: no progress in recent turns. Send a follow-up to continue."
    );
    assert_eq!(
        format_run_stop_message("provider_truncated", &[], limits),
        "Output truncated (initial max_output_tokens=4096). Automatic recovery is bounded by retry, turn, model and context limits. Progress is preserved. Increase max_output_tokens within the model limit or request a smaller next step before continuing."
    );
    assert_eq!(
        format_run_stop_message(
            "tool_limit",
            &[],
            ToolLoopLimits {
                max_mutating_tool_calls: 0,
                max_read_tool_calls: 0,
                max_total_tool_calls: 0,
                max_turns: 128,
                max_output_tokens: 4096,
                max_result_bytes: 16 * 1024,
                context_window_tokens: 32_000,
            },
        ),
        "Configured tool budgets are zero. Increase the tool limits to continue with tools."
    );
}
