use std::time::Duration;

use slim_core::{EventKind, SessionEvent};
use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::cache::BoundedCache;
use slim_tui::layout::{plan, plan_checked, visible_range, LayoutError};
use slim_tui::render::{render, EventCoalescer};
use slim_tui::view_model::format_context;

#[test]
fn data_deltas_coalesce_but_control_events_are_preserved_first() {
    let mut coalescer = EventCoalescer::new(2, Duration::from_millis(16));
    let mut events = coalescer.push_data(UiEvent::AssistantDelta { text: "a".into() });
    events.extend(coalescer.push_data(UiEvent::AssistantDelta { text: "b".into() }));
    coalescer.push_control(UiEvent::RunCancelled { run_id: 1 });
    events.extend(coalescer.flush());
    assert_eq!(events[0], UiEvent::RunCancelled { run_id: 1 });
    assert_eq!(events[1], UiEvent::AssistantDelta { text: "ab".into() });
}

#[test]
fn completed_run_retains_pending_tasks_without_marking_the_run_failed() {
    use slim_tui::api::{TodoItemStatus, TodoItemView};
    use slim_tui::block::BlockKind;

    for (status, expected) in [
        (TodoItemStatus::Pending, true),
        (TodoItemStatus::InProgress, true),
        (TodoItemStatus::Blocked, true),
        (TodoItemStatus::Completed, false),
        (TodoItemStatus::Cancelled, false),
    ] {
        let mut state = AppState::new();
        state.apply_event(UiEvent::RunStarted {
            run_id: 1,
            max_mutating_tool_calls: 32,
            max_read_tool_calls: 96,
            max_turns: 128,
        });
        state.apply_event(UiEvent::TodoChanged {
            items: vec![TodoItemView {
                title: "verify changes".into(),
                status,
            }],
        });
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        assert!(!state.working);
        assert_eq!(state.todo_items[0].status, status);
        let notices = |state: &AppState| {
            state.blocks().iter().filter(|block| {
            matches!(block.kind(), BlockKind::System(text) if text.contains("tarefa(s) registrada(s) continuam pendentes"))
        }).count()
        };
        assert_eq!(notices(&state), usize::from(expected));
        let frame = render(&state, 100, 24).lines.join("\n");
        assert_eq!(
            frame.contains("tarefa(s) registrada(s) continuam pendentes"),
            expected
        );
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        assert_eq!(
            notices(&state),
            usize::from(expected),
            "duplicate terminal must not repeat the notice"
        );
    }
}

#[test]
fn usage_estimate_coalesces_monotonically_per_window() {
    let mut coalescer = EventCoalescer::new(8, Duration::from_millis(16));
    coalescer.push_data(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 10,
        context_window_tokens: 100,
    });
    coalescer.push_data(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 8,
        context_window_tokens: 100,
    });
    coalescer.push_data(UiEvent::UsageEstimate {
        request_id: 2,
        context_tokens: 3,
        context_window_tokens: 100,
    });

    assert_eq!(
        coalescer.flush(),
        vec![
            UiEvent::UsageEstimate {
                request_id: 1,
                context_tokens: 10,
                context_window_tokens: 100,
            },
            UiEvent::UsageEstimate {
                request_id: 2,
                context_tokens: 3,
                context_window_tokens: 100,
            },
        ]
    );
}

#[test]
fn tool_output_notification_preview_is_bounded() {
    let event = UiEvent::from_core(SessionEvent::new(
        1,
        EventKind::ToolOutput {
            batch_id: "batch".into(),
            call_id: "call".into(),
            name: "tool".repeat(2_500),
            output: "x".repeat(10_000),
        },
    ))
    .expect("projected event");
    let UiEvent::ToolProgress { preview, .. } = event else {
        panic!("expected tool progress");
    };
    assert!(preview.chars().count() <= 513); // 512 + ellipsis
}

#[test]
fn layout_keeps_todo_composer_and_operational_rows_bounded() {
    let regions = plan(100, 24, 2, false);
    assert_eq!(regions.todo.height, 2);
    assert_eq!(regions.todo_divider.height, 1);
    assert_eq!(regions.composer.height, 3);
    assert_eq!(regions.op_divider.height, 0);
    assert_eq!(regions.operational.height, 2);
    assert_eq!(regions.operational.y, regions.composer.y + 3);
    assert_eq!(plan(100, 12, 0, false).operational.height, 2);
    assert_eq!(plan(100, 11, 0, false).operational.height, 1);
    assert_eq!(visible_range(100, 98, 10), 98..100);
}

#[test]
fn bounded_cache_evicts_old_entries_without_changing_new_values() {
    let mut cache = BoundedCache::new(2);
    cache.insert("a", 1);
    cache.insert("b", 2);
    cache.insert("c", 3);
    assert_eq!(cache.len(), 2);
    assert!(cache.get(&"a").is_none());
    assert_eq!(cache.get(&"c"), Some(&3));
}

#[test]
fn render_uses_same_state_as_m0_view_model() {
    let state = AppState::new();
    assert_eq!(
        render(&state, 80, 24).lines.last(),
        Some(&"desconectado · /login".to_string())
    );
}

#[test]
fn estimate_is_monotonic_and_final_usage_removes_tilde() {
    let mut state = AppState::new();
    for context_tokens in [10, 8] {
        state.apply_event(UiEvent::UsageEstimate {
            request_id: 1,
            context_tokens,
            context_window_tokens: 100,
        });
    }
    assert_eq!(state.context_tokens, 10);
    assert!(!state.context_exact);

    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "reasoning".into(),
    });
    let after_reasoning = state.context_tokens;
    assert!(after_reasoning > 10);
    state.apply_event(UiEvent::ThinkingEnded);
    state.apply_event(UiEvent::AssistantDelta {
        text: "streamed output".into(),
    });
    assert!(state.context_tokens > after_reasoning);
    assert!(format_context(&state, false).contains('~'));

    state.apply_event(UiEvent::UsagePartial {
        input_tokens: 7,
        output_tokens: 0,
    });
    state.apply_event(UiEvent::UsagePartial {
        input_tokens: 0,
        output_tokens: 5,
    });
    state.apply_event(UiEvent::Usage {
        input_tokens: 0,
        output_tokens: 0,
    });
    state.apply_event(UiEvent::AssistantEnded);

    assert_eq!(state.context_tokens, 12);
    assert!(
        !state.context_exact,
        "usage fence awaits successful outcome"
    );
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    assert!(state.context_exact);
    assert_eq!(format_context(&state, false), "ctx 12% · 12/100");
    assert!(!format_context(&state, false).contains('~'));

    state.apply_event(UiEvent::UsageEstimate {
        request_id: 2,
        context_tokens: 4,
        context_window_tokens: 100,
    });
    assert_eq!(
        state.context_tokens, 4,
        "new request may reflect compaction"
    );
    assert_eq!(format_context(&state, false), "ctx ~4% · 4/100");

    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 3,
        context_tokens: 2,
        context_window_tokens: 100,
    });
    assert_eq!(
        state.context_tokens, 2,
        "provider without usage still closes the request boundary"
    );
}

#[test]
fn stale_usage_estimates_cannot_replace_newer_or_terminal_identity() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(2));
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 2,
        request_id: 2,
        context_tokens: 20,
        context_window_tokens: 100,
    });

    for stale in [
        UiEvent::UsageEstimateForRun {
            run_id: 1,
            request_id: 9,
            context_tokens: 91,
            context_window_tokens: 100,
        },
        UiEvent::UsageEstimateForRun {
            run_id: 2,
            request_id: 1,
            context_tokens: 81,
            context_window_tokens: 100,
        },
        UiEvent::UsageEstimateForRun {
            run_id: 2,
            request_id: 2,
            context_tokens: 72,
            context_window_tokens: 200,
        },
    ] {
        state.apply_event(stale);
    }
    assert_eq!(state.context_tokens, 20);
    assert_eq!(state.context_window_tokens, 100);

    state.apply_event(UiEvent::RunCancelled { run_id: 2 });
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 2,
        request_id: 3,
        context_tokens: 63,
        context_window_tokens: 100,
    });
    assert_eq!(
        state.context_tokens, 20,
        "terminal run rejects late estimate"
    );
}

#[test]
fn saturated_request_usage_never_becomes_exact() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 0,
        context_window_tokens: u64::MAX,
    });
    state.apply_event(UiEvent::UsagePartial {
        input_tokens: u64::MAX,
        output_tokens: 0,
    });
    state.apply_event(UiEvent::UsagePartial {
        input_tokens: 1,
        output_tokens: 1,
    });
    state.apply_event(UiEvent::Usage {
        input_tokens: 0,
        output_tokens: 0,
    });
    state.apply_event(UiEvent::AssistantEnded);

    assert_eq!(state.context_tokens, u64::MAX);
    assert!(state.input_tokens_overflowed);
    assert!(
        !state.context_exact,
        "saturated context must retain approximation"
    );
}

#[test]
fn non_success_outcome_never_promotes_usage_or_assistant_to_complete() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(7));
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 7,
        request_id: 1,
        context_tokens: 20,
        context_window_tokens: 100,
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "partial".into(),
    });
    state.apply_event(UiEvent::Usage {
        input_tokens: 7,
        output_tokens: 5,
    });
    state.apply_event(UiEvent::AssistantEnded);
    assert!(!state.context_exact);

    state.apply_event(UiEvent::RunStopped {
        run_id: 7,
        message: "provider response was truncated".into(),
    });
    assert!(!state.context_exact);
    assert!(state.blocks().iter().any(|block| {
        matches!(block.kind(), slim_tui::block::BlockKind::Assistant(text) if text == "partial")
            && block.lifecycle == slim_tui::block::BlockLifecycle::Cancelled
    }));
}

#[test]
fn complete_zero_input_usage_is_exact() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 20,
        context_window_tokens: 100,
    });
    state.apply_event(UiEvent::Usage {
        input_tokens: 0,
        output_tokens: 5,
    });
    state.apply_event(UiEvent::AssistantEnded);
    assert!(!state.context_exact);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    assert!(state.context_exact);
    assert_eq!(state.context_tokens, 5);
}

#[test]
fn anthropic_cache_tokens_without_base_input_never_become_exact() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 20,
        context_window_tokens: 100,
    });
    state.apply_event(UiEvent::UsagePartial {
        input_tokens: 5,
        output_tokens: 0,
    });
    state.apply_event(UiEvent::UsagePartial {
        input_tokens: 0,
        output_tokens: 4,
    });
    state.apply_event(UiEvent::AssistantEnded);

    assert!(!state.context_exact);
    assert!(format_context(&state, false).contains('~'));
}

#[test]
fn providerless_run_preserves_prior_exact_context() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 20,
        context_window_tokens: 100,
    });
    state.apply_event(UiEvent::Usage {
        input_tokens: 12,
        output_tokens: 8,
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RunCompleted { run_id: 0 });
    assert_eq!(format_context(&state, false), "ctx 20% · 20/100");

    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::RunStopped {
        run_id: 1,
        message: "plan approval required".into(),
    });
    assert_eq!(format_context(&state, false), "ctx 20% · 20/100");
}

#[test]
fn partial_usage_never_removes_the_estimate_marker() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 20,
        context_window_tokens: 100,
    });
    state.apply_event(UiEvent::UsagePartial {
        input_tokens: 7,
        output_tokens: 0,
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "streamed output".into(),
    });
    let provisional = state.context_tokens;
    state.apply_event(UiEvent::AssistantEnded);

    assert_eq!(state.context_tokens, provisional);
    assert!(format_context(&state, false).starts_with("ctx ~"));
    assert_eq!(state.input_tokens, 7);
}

#[test]
fn changed_or_unknown_window_resets_estimate_identity() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 10,
        context_window_tokens: 100,
    });
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 3,
        context_window_tokens: 200,
    });
    assert_eq!(
        (state.context_tokens, state.context_window_tokens),
        (3, 200)
    );

    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 2,
        context_window_tokens: 0,
    });
    assert_eq!(state.context_window_tokens, 0);
    assert_eq!(format_context(&state, false), "ctx --");
}

#[test]
fn context_percentage_floors_to_one_when_tokens_are_nonzero() {
    let mut state = AppState::new();
    state.context_tokens = 6_800;
    state.context_window_tokens = 1_000_000;
    assert_eq!(format_context(&state, false), "ctx ~1% · 6.8k/1M");
}

#[test]
fn context_percentage_is_stable_at_u64_boundaries() {
    let mut state = AppState::new();
    state.context_tokens = u64::MAX / 2 + 1;
    state.context_window_tokens = u64::MAX;
    assert_eq!(format_context(&state, true), "ctx ~50%");

    state.context_tokens = u64::MAX;
    state.context_window_tokens = 1;
    assert_eq!(format_context(&state, true), "ctx ~999%");
}

#[test]
fn tiny_terminal_returns_explicit_error_and_emergency_layout() {
    assert_eq!(
        plan_checked(39, 7, 2, false),
        Err(LayoutError::TerminalTooSmall)
    );
    let emergency = plan(39, 7, 2, false);
    assert_eq!(emergency.composer.height, 1);
    assert_eq!(emergency.operational.height, 1);
}

#[test]
fn activity_queue_todo_and_composer_are_projected_into_one_frame() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ActivityChanged {
        label: "child running".into(),
    });
    state.apply_event(UiEvent::QueuedUserAdded {
        text: "second prompt".into(),
        position: 1,
    });
    state.apply_event(UiEvent::TodoChanged {
        items: vec![
            slim_tui::api::TodoItemView {
                title: "first task".into(),
                status: slim_tui::api::TodoItemStatus::Completed,
            },
            slim_tui::api::TodoItemView {
                title: "second prompt".into(),
                status: slim_tui::api::TodoItemStatus::InProgress,
            },
        ],
    });
    state.composer.paste("long\npaste");
    let lines = render(&state, 80, 24).lines;
    assert!(lines.iter().any(|line| line == "activity: child running"));
    assert!(lines.iter().any(|line| line == "> second prompt"));
    assert!(lines.iter().any(|line| line == "todo: 1/2 second prompt"));
    assert!(lines
        .iter()
        .any(|line| line == "composer: [Pasted Content 0 10 chars]"));
    assert!(lines.iter().any(|line| line == "desconectado · /login"));
}
