//! Fault injection (DESIGN-SLIM-TUI §28.5 subset, gate C8): data-lane
//! overflow coalesces without losing control events; stale content pages
//! do not corrupt transcript state.

use std::time::Duration;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::block::{Block, BlockKind, BlockLifecycle};
use slim_tui::render::EventCoalescer;

#[test]
fn data_overflow_coalesces_deltas_and_never_loses_control() {
    let mut coalescer = EventCoalescer::new(2, Duration::from_millis(16));
    let mut events = coalescer.push_data(UiEvent::AssistantDelta { text: "a".into() });
    events.extend(coalescer.push_data(UiEvent::AssistantDelta { text: "b".into() }));
    events.extend(coalescer.push_data(UiEvent::AssistantDelta { text: "c".into() }));
    coalescer.push_control(UiEvent::RunCancelled { run_id: 1 });
    events.extend(coalescer.flush());
    assert_eq!(events[0], UiEvent::RunCancelled { run_id: 1 });
    let merged: String = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(merged, "abc", "deltas must concatenate under overflow");
}

#[test]
fn data_capacity_flushes_chunks_without_losing_lifecycle_events() {
    let mut coalescer = EventCoalescer::new(2, Duration::from_millis(16));
    let mut events = Vec::new();
    events.extend(coalescer.push_data(UiEvent::ThinkingDelta { text: "a".into() }));
    events.extend(coalescer.push_data(UiEvent::ToolProgress {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("call".into()),
        name: "read".into(),
        preview: "half".into(),
        content_handle: None,
    }));
    events.extend(coalescer.push_data(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("call".into()),
        name: "read".into(),
        success: true,
        duration_ms: 1,
    }));
    events.extend(coalescer.flush());

    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], UiEvent::ThinkingDelta { .. }));
    assert!(matches!(events[1], UiEvent::ToolProgress { .. }));
    assert!(matches!(
        events[2],
        UiEvent::ToolEnded { success: true, .. }
    ));
}

#[test]
fn stale_content_page_is_bounded_notification_not_state_corruption() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ContentPageLoaded {
        handle: slim_tui::api::ContentHandle("stale-handle".into()),
        request_id: slim_tui::api::ContentRequestId(1),
        cursor: None,
        text: "x".repeat(10_000),
        next_cursor: None,
    });
    assert!(
        state.blocks().is_empty(),
        "content page must not fabricate transcript blocks"
    );
}

#[test]
fn block_lifecycle_states_are_expressible_for_fault_paths() {
    for lifecycle in [
        BlockLifecycle::Pending,
        BlockLifecycle::Streaming,
        BlockLifecycle::Complete,
        BlockLifecycle::Failed,
        BlockLifecycle::Cancelled,
    ] {
        let block = Block::new("f1", BlockKind::System("s".into()), lifecycle);
        assert_eq!(block.lifecycle, lifecycle);
    }
}
