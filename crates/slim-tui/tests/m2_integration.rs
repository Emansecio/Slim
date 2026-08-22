use std::time::Duration;

use slim_core::{EventKind, SessionEvent};
use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::cache::BoundedCache;
use slim_tui::layout::{plan, plan_checked, visible_range, LayoutError};
use slim_tui::render::{render, EventCoalescer};

#[test]
fn data_deltas_coalesce_but_control_events_are_preserved_first() {
    let mut coalescer = EventCoalescer::new(2, Duration::from_millis(16));
    coalescer.push_data(UiEvent::AssistantDelta { text: "a".into() });
    coalescer.push_data(UiEvent::AssistantDelta { text: "b".into() });
    coalescer.push_control(UiEvent::FatalError {
        message: "fatal".into(),
    });
    let events = coalescer.flush();
    assert!(matches!(events[0], UiEvent::FatalError { .. }));
    assert_eq!(events[1], UiEvent::AssistantDelta { text: "ab".into() });
}

#[test]
fn tool_output_notification_preview_is_bounded() {
    let event = UiEvent::from_core(SessionEvent::new(
        1,
        EventKind::ToolOutput {
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
    assert_eq!(regions.operational.height, 1);
    assert_eq!(regions.operational.y, regions.composer.y + 3);
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
        Some(&"signed out · /login  ctx 0% · 0k/128k · ↑0 ↓0".to_string())
    );
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
    assert!(lines.iter().any(|line| line == "> queued[1] second prompt"));
    assert!(lines.iter().any(|line| line == "todo: 1/2 second prompt"));
    assert!(lines
        .iter()
        .any(|line| line == "composer: [Pasted Content 0 10 chars]"));
    assert!(lines
        .iter()
        .any(|line| line.contains("signed out · /login") && line.contains("ctx 0%")));
}
