//! Property tests (DESIGN-SLIM-TUI §28.2, gate C9): scroll invariants, draft
//! preservation under effect failure, and unseen counting under random
//! action sequences.

use proptest::prelude::*;

use slim_tui::api::UiEvent;
use slim_tui::app::{AppState, FollowMode};
use slim_tui::reducer::{reduce, Action, ScrollIntent};
use slim_tui::render::{HeightIndex, WrapCache};
use slim_tui::runtime::measure_scrollback;

fn scroll(state: &mut AppState, cache: &mut WrapCache, intent: ScrollIntent) {
    let metrics = measure_scrollback(state, 80, 24, cache);
    reduce(state, Action::Scroll { intent, metrics });
}

fn overflow_state(state: &mut AppState) {
    for index in 0..12 {
        reduce(
            state,
            Action::UiEventReceived(UiEvent::UserMessageAdded {
                text: format!("q{index}"),
            }),
        );
        reduce(
            state,
            Action::UiEventReceived(UiEvent::AssistantDelta {
                text: format!("a{index}"),
            }),
        );
        reduce(state, Action::UiEventReceived(UiEvent::AssistantEnded));
    }
}

#[test]
fn height_index_resolves_every_grouped_block_without_rescan() {
    let mut state = AppState::new();
    for index in 0..3 {
        state.apply_event(UiEvent::ToolStarted {
            batch_id: slim_tui::api::ToolBatchId("batch".into()),
            call_id: slim_tui::api::ToolCallId(format!("call-{index}").into()),
            name: "read".into(),
            arguments_summary: String::new(),
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id: slim_tui::api::ToolBatchId("batch".into()),
            call_id: slim_tui::api::ToolCallId(format!("call-{index}").into()),
            name: "read".into(),
            success: true,
            duration_ms: 1,
        });
    }
    let ids = state
        .blocks()
        .iter()
        .map(|block| block.id.clone())
        .collect::<Vec<_>>();
    let mut cache = WrapCache::default();
    let index = HeightIndex::build(state.blocks(), 80, &mut cache);
    assert_eq!(index.len(), 1, "tools should aggregate visually");
    for id in ids {
        assert_eq!(index.prefix_for_block(&id), Some(0));
    }
}

#[test]
fn process_progress_keeps_existing_inspector_handle() {
    let mut state = AppState::new();
    let batch_id = slim_tui::api::ToolBatchId("batch".into());
    let call_id = slim_tui::api::ToolCallId("call".into());
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch_id.clone(),
        call_id: call_id.clone(),
        name: "shell".into(),
        arguments_summary: String::new(),
    });
    state.apply_event(UiEvent::ToolProgress {
        content_handle: Some(slim_tui::api::ContentHandle("artifact".into())),
        batch_id: batch_id.clone(),
        call_id: call_id.clone(),
        name: "shell".into(),
        preview: "captured output".into(),
    });
    state.apply_event(UiEvent::ToolProgress {
        content_handle: None,
        batch_id,
        call_id,
        name: "shell".into(),
        preview: "exit 0".into(),
    });
    let tool = match state.blocks().first().map(|block| block.kind()) {
        Some(slim_tui::block::BlockKind::Tool(tool)) => tool,
        _ => panic!("tool block"),
    };
    assert_eq!(tool.preview, "exit 0");
    assert_eq!(
        tool.content_handle,
        Some(slim_tui::api::ContentHandle("artifact".into()))
    );
}

#[test]
fn end_always_returns_to_live_edge() {
    let mut state = AppState::new();
    let mut cache = WrapCache::default();
    overflow_state(&mut state);
    for intent in [
        ScrollIntent::Up,
        ScrollIntent::Down,
        ScrollIntent::PageUp,
        ScrollIntent::PageDown,
        ScrollIntent::Top,
        ScrollIntent::LiveEdge,
    ] {
        scroll(&mut state, &mut cache, intent);
        scroll(&mut state, &mut cache, ScrollIntent::LiveEdge);
        assert!(state.scroll.is_live_edge());
        assert_eq!(state.scroll.unseen, 0);
    }
}

proptest! {
    #[test]
    fn unseen_only_counts_while_pinned(count in 1usize..20) {
        let mut state = AppState::new();
        let mut cache = WrapCache::default();
        overflow_state(&mut state);
        scroll(&mut state, &mut cache, ScrollIntent::Up);
        prop_assert!(state.scroll.is_pinned());
        for index in 0..count {
            reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::UserMessageAdded {
                    text: format!("delta-{index}"),
                }),
            );
            prop_assert_eq!(state.scroll.unseen, (index + 1) as u32);
        }
        // Live edge resets and stops counting.
        scroll(&mut state, &mut cache, ScrollIntent::LiveEdge);
        let before = state.blocks().len();
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::UserMessageAdded { text: "late".into() }),
        );
        prop_assert_eq!(state.blocks().len(), before + 1);
        prop_assert_eq!(state.scroll.unseen, 0);
    }

    #[test]
    fn pinned_anchor_and_viewport_stay_within_measured_rows(
        intents in proptest::collection::vec(0..4u8, 0..40),
    ) {
        let mut state = AppState::new();
        let mut cache = WrapCache::default();
        overflow_state(&mut state);
        scroll(&mut state, &mut cache, ScrollIntent::PageUp);
        for intent in intents {
            scroll(
                &mut state,
                &mut cache,
                match intent {
                    0 => ScrollIntent::Up,
                    1 => ScrollIntent::Down,
                    2 => ScrollIntent::PageUp,
                    _ => ScrollIntent::PageDown,
                },
            );
            if let FollowMode::Pinned(anchor) = &state.scroll.mode {
                let index = HeightIndex::build(state.blocks(), 79, &mut cache);
                let prefix = index.prefix_for_block(&anchor.block_id).expect("anchor block");
                let row = index.row_for_anchor(anchor).expect("anchor row");
                prop_assert!(row >= prefix);
                prop_assert!(row < index.total_rows);
                let metrics = measure_scrollback(&state, 80, 24, &mut cache);
                prop_assert!(metrics.viewport_start <= metrics.bottom_start);
            }
        }
    }

    #[test]
    fn draft_survives_oversized_paste_and_failed_send(
        draft in "[a-z0-9 ]{1,64}",
    ) {
        let mut state = AppState::new();
        let original = draft.clone();
        state.composer.insert_text(draft);
        // Oversized paste is rejected without touching the draft.
        reduce(&mut state, Action::Paste("x".repeat(slim_tui::composer::MAX_DRAFT_CHARS + 1)));
        prop_assert_eq!(state.composer.payload(), original.as_str());
        // Signed-out send keeps the draft (spec §15.3).
        reduce(
            &mut state,
            Action::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
        );
        prop_assert_eq!(state.composer.payload(), original.as_str());
    }

    #[test]
    fn terminal_layout_with_optional_session_rail_never_panics(
        width in 0u16..121,
        height in 0u16..40,
        todo in 0u16..7,
        working in any::<bool>(),
        show_session_rail in any::<bool>(),
    ) {
        let regions = slim_tui::layout::plan_with_session_rail(
            width,
            height,
            todo,
            working,
            show_session_rail,
        );
        let total = regions.session_rail.height
            + regions.activity_rail.height
            + regions.scrollback.height
            + regions.todo.height
            + regions.todo_divider.height
            + regions.composer.height
            + regions.op_divider.height
            + regions.operational.height;
        prop_assert!(total <= height, "regions must never exceed the viewport");
        if regions.session_rail.height > 0 {
            prop_assert!(show_session_rail);
            prop_assert!(width >= 80);
            prop_assert!(height >= 12);
        }
    }
}
