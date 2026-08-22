//! Property tests (DESIGN-SLIM-TUI §28.2, gate C9): scroll invariants, draft
//! preservation under effect failure, and unseen counting under random
//! action sequences.

use proptest::prelude::*;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action, ScrollIntent};

fn overflow_state(state: &mut AppState) {
    for index in 0..30 {
        reduce(
            state,
            Action::UiEventReceived(UiEvent::UserMessageAdded { text: format!("q{index}") }),
        );
        reduce(
            state,
            Action::UiEventReceived(UiEvent::AssistantDelta { text: format!("a{index}") }),
        );
        reduce(state, Action::UiEventReceived(UiEvent::AssistantEnded));
    }
}

proptest! {
    #[test]
    fn end_always_returns_to_live_edge(intents in proptest::collection::vec(0..6u8, 0..40)) {
        let mut state = AppState::new();
        overflow_state(&mut state);
        for intent in intents {
            let action = match intent {
                0 => Action::Scroll(ScrollIntent::Up),
                1 => Action::Scroll(ScrollIntent::Down),
                2 => Action::Scroll(ScrollIntent::PageUp),
                3 => Action::Scroll(ScrollIntent::PageDown),
                4 => Action::Scroll(ScrollIntent::Top),
                _ => Action::Scroll(ScrollIntent::LiveEdge),
            };
            reduce(&mut state, action);
            // Invariant: offset is non-negative by construction; End resets all.
            reduce(&mut state, Action::Scroll(ScrollIntent::LiveEdge));
            prop_assert!(!state.scroll.pinned);
            prop_assert_eq!(state.scroll.offset_from_end, 0);
            prop_assert_eq!(state.scroll.unseen, 0);
        }
    }

    #[test]
    fn unseen_only_counts_while_pinned(deltas in proptest::collection::vec("[a-z]{1,8}", 1..20)) {
        let mut state = AppState::new();
        overflow_state(&mut state);
        reduce(&mut state, Action::Scroll(ScrollIntent::Up));
        prop_assert!(state.scroll.pinned);
        for (index, text) in deltas.into_iter().enumerate() {
            reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::UserMessageAdded { text }),
            );
            prop_assert_eq!(state.scroll.unseen, (index + 1) as u32);
        }
        // Live edge resets and stops counting.
        reduce(&mut state, Action::Scroll(ScrollIntent::LiveEdge));
        let before = state.blocks.len();
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::UserMessageAdded { text: "late".into() }),
        );
        prop_assert_eq!(state.blocks.len(), before + 1);
        prop_assert_eq!(state.scroll.unseen, 0);
    }

    #[test]
    fn draft_survives_oversized_paste_and_failed_send(
        draft in "[a-z0-9 ]{1,64}",
        oversized in proptest::collection::vec(any::<u8>(), 10),
    ) {
        let mut state = AppState::new();
        let original = draft.clone();
        state.composer.insert_text(draft);
        // Oversized paste is rejected without touching the draft.
        let _ = oversized.len();
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
    fn tiny_terminal_layout_never_panics(
        width in 0u16..60,
        height in 0u16..30,
        todo in 0u16..7,
        working in any::<bool>(),
    ) {
        let regions = slim_tui::layout::plan(width, height, todo, working);
        let total = regions.activity_rail.height
            + regions.scrollback.height
            + regions.todo.height
            + regions.todo_divider.height
            + regions.composer.height
            + regions.op_divider.height
            + regions.operational.height;
        prop_assert!(total <= height, "regions must never exceed the viewport");
    }
}
