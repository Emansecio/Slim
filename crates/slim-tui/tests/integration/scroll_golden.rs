//! Golden scroll tests (DESIGN-SLIM-TUI §13.3, gate A2): the newest message
//! must stay visible at live edge, pinning must keep the view stable while new
//! content arrives, and End must return to the live edge.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::UiEvent;
use slim_tui::app::{AppState, FollowMode, ScrollAnchor};
use slim_tui::reducer::{reduce, Action, ScrollIntent};
use slim_tui::render::WrapCache;
use slim_tui::runtime::{measure_scrollback, render_frame};
use slim_tui::theme::Capabilities;

fn render_to_string(state: &AppState) -> String {
    render_at_size(state, 80, 24)
}

fn render_at_size(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut cache = WrapCache::default();
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                state,
                Capabilities {
                    color_depth: slim_tui::theme::ColorDepth::TrueColor,
                    mouse: false,
                    clipboard: false,
                    images: false,
                    reduced_motion: false,
                },
                &mut cache,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        out.push('\n');
    }
    out
}

fn scroll_at_size(state: &mut AppState, intent: ScrollIntent, width: u16, height: u16) {
    let mut cache = WrapCache::default();
    let metrics = measure_scrollback(state, width, height, &mut cache);
    reduce(state, Action::Scroll { intent, metrics });
}

fn transcript_rows(frame: &str, width: u16, height: u16) -> Vec<String> {
    let rows = slim_tui::layout::plan(width, height, 0, false)
        .scrollback
        .height as usize;
    frame
        .lines()
        .take(rows)
        .map(|line| {
            line.chars()
                .take(width.saturating_sub(1) as usize)
                .collect()
        })
        .collect()
}

fn overflow_state() -> AppState {
    let mut state = AppState::new();
    for index in 0..60 {
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::UserMessageAdded {
                text: format!("question number {index}"),
            }),
        );
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::AssistantDelta {
                text: format!("answer number {index}"),
            }),
        );
        reduce(&mut state, Action::UiEventReceived(UiEvent::AssistantEnded));
    }
    state
}

#[test]
fn live_edge_shows_newest_message_when_content_overflows_viewport() {
    let state = overflow_state();
    let frame = render_to_string(&state);
    assert!(
        frame.contains("answer number 59"),
        "newest answer must be visible"
    );
    assert!(
        !frame.contains("question number 0"),
        "oldest content may scroll off"
    );
}

#[test]
fn live_edge_uses_physical_rows_inside_wrapped_user_block() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::UserMessageAdded {
            text: format!("{}TAIL_VISIBLE", "0".repeat(304)),
        }),
    );

    let frame = render_at_size(&state, 40, 8);
    assert!(
        frame.contains("TAIL_VISIBLE"),
        "live edge must skip physical wrapped rows, not logical lines\n{frame}"
    );
}

#[test]
fn multiline_error_and_queued_prompt_keep_their_tail_visible() {
    for event in [
        UiEvent::FatalError {
            run_id: None,
            message: format!("{}ERROR_TAIL", "0".repeat(288)),
        },
        UiEvent::QueuedUserAdded {
            text: format!("{}QUEUE_TAIL", "0".repeat(288)),
            position: 1,
        },
    ] {
        let mut state = AppState::new();
        state.apply_event(event);
        let frame = render_at_size(&state, 40, 8);
        assert!(
            frame.contains("_TAIL"),
            "multiline block tail must remain reachable\n{frame}"
        );
    }
}

#[test]
fn sent_prompt_stays_at_top_until_response_overflows() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "short answer".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let frame = render_at_size(&state, 80, 12);
    let question_row = frame
        .lines()
        .position(|line| line.contains("question"))
        .expect("question");
    assert!(
        question_row <= 2,
        "prompt must stay near top: {question_row}"
    );
    assert!(frame.contains("short answer"));
}

#[test]
fn latest_short_turn_page_fills_from_its_prompt_after_long_history() {
    let mut state = AppState::new();
    for index in 0..4 {
        state.apply_event(UiEvent::UserMessageAdded {
            text: format!("OLD_QUESTION_{index}"),
        });
        state.apply_event(UiEvent::AssistantDelta {
            text: format!("OLD_ANSWER_{index}"),
        });
        state.apply_event(UiEvent::AssistantEnded);
    }
    state.apply_event(UiEvent::UserMessageAdded {
        text: "LATEST_QUESTION".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "LATEST_ANSWER".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);

    let frame = render_at_size(&state, 80, 12);
    let transcript = transcript_rows(&frame, 80, 12);
    let question_row = transcript
        .iter()
        .position(|row| row.contains("LATEST_QUESTION"))
        .unwrap_or_else(|| panic!("latest prompt missing\n{frame}"));

    assert_eq!(
        question_row, 0,
        "an overflowing history starts the live tail at the top of the transcript\n{frame}"
    );
    assert!(frame.contains("LATEST_ANSWER"));
    assert!(
        !frame.contains("OLD_ANSWER_3"),
        "history leaked above prompt\n{frame}"
    );
}

#[test]
fn turn_boundary_adds_one_row_only_before_later_user() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "FIRST_QUESTION".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "FIRST_ANSWER".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::UserMessageAdded {
        text: "SECOND_QUESTION".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "SECOND_ANSWER".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.scroll.mode = slim_tui::app::FollowMode::Top;

    // Keep both turns visible, including the new persistent agent headings.
    let frame = render_at_size(&state, 80, 24);
    let rows = transcript_rows(&frame, 80, 24);
    let first_question = rows
        .iter()
        .position(|row| row.contains("FIRST_QUESTION"))
        .expect("first question");
    let second_question = rows
        .iter()
        .position(|row| row.contains("SECOND_QUESTION"))
        .expect("second question");

    assert!(
        first_question > 0,
        "user label precedes first body\n{frame}"
    );
    assert!(
        rows[first_question - 1].trim().is_empty(),
        "a short conversation gets one page-fill row above its first turn\n{frame}"
    );
    assert!(
        second_question >= 2,
        "second turn has boundary rows\n{frame}"
    );
    assert!(
        rows[second_question - 1].trim().is_empty(),
        "exactly one spacer must precede the next user label\n{frame}"
    );
    assert!(
        !rows[second_question - 2].trim().is_empty(),
        "the turn boundary must not add a second spacer\n{frame}"
    );
}

#[test]
fn overflowing_response_switches_to_live_tail() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let frame = render_at_size(&state, 80, 12);
    assert!(!frame.contains("question"));
    assert!(
        frame.contains("line 39"),
        "tail must remain visible\n{frame}"
    );
}

#[test]
fn pinned_anchor_does_not_move_when_content_arrives() {
    let mut state = overflow_state();
    scroll_at_size(&mut state, ScrollIntent::PageUp, 80, 24);
    let before = transcript_rows(&render_at_size(&state, 80, 24), 80, 24);
    state.apply_event(UiEvent::AssistantDelta {
        text: "fresh answer".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let after = transcript_rows(&render_at_size(&state, 80, 24), 80, 24);
    assert_eq!(before, after, "pinned viewport moved after append");
}

#[test]
fn pinned_anchor_survives_resize_and_unrelated_thinking_expansion() {
    let mut state = overflow_state();
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "expandable reasoning body".repeat(8),
    });
    state.apply_event(UiEvent::ThinkingEnded);

    let anchor_id = state.blocks()[10].id.clone();
    let expanded_id = state.blocks().last().expect("thinking block").id.clone();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: anchor_id.clone(),
        row_offset: 0,
    });

    let mut cache = WrapCache::default();
    let before = measure_scrollback(&state, 80, 24, &mut cache);
    assert_eq!(
        before
            .top_anchor
            .as_ref()
            .map(|anchor| anchor.block_id.clone()),
        Some(anchor_id.clone())
    );

    let resized = measure_scrollback(&state, 100, 18, &mut cache);
    assert_eq!(
        resized
            .top_anchor
            .as_ref()
            .map(|anchor| anchor.block_id.clone()),
        Some(anchor_id.clone()),
        "resizing must keep the pinned block as the viewport anchor"
    );

    assert!(state.toggle_block(&expanded_id), "thinking block expands");
    let expanded = measure_scrollback(&state, 100, 18, &mut cache);
    assert_eq!(
        expanded
            .top_anchor
            .as_ref()
            .map(|anchor| anchor.block_id.clone()),
        Some(anchor_id),
        "expanding content after the anchor must not retarget the viewport"
    );
}

#[test]
fn home_goes_to_first_block() {
    let mut state = overflow_state();
    scroll_at_size(&mut state, ScrollIntent::Top, 80, 24);
    assert!(render_to_string(&state).contains("question number 0"));
}

#[test]
fn scrollbar_appears_only_with_overflow() {
    let mut short = AppState::new();
    short.apply_event(UiEvent::UserMessageAdded { text: "q".into() });
    short.apply_event(UiEvent::AssistantDelta { text: "a".into() });
    short.apply_event(UiEvent::AssistantEnded);
    let short_frame = render_at_size(&short, 80, 24);
    let long_frame = render_at_size(&overflow_state(), 80, 24);
    let rows = slim_tui::layout::plan(80, 24, 0, false).scrollback.height as usize;
    let right_column = |frame: &str| {
        frame
            .lines()
            .take(rows)
            .filter_map(|line| line.chars().nth(79))
            .collect::<String>()
    };
    assert!(!right_column(&short_frame).contains('┃'));
    assert!(right_column(&long_frame).contains('┃'));
}

#[test]
fn scrolling_up_pins_and_counts_unseen_content() {
    let mut state = overflow_state();
    scroll_at_size(&mut state, ScrollIntent::Up, 80, 24);
    assert!(state.scroll.is_pinned());

    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::AssistantDelta {
            text: "fresh answer".into(),
        }),
    );
    reduce(&mut state, Action::UiEventReceived(UiEvent::AssistantEnded));
    assert_eq!(state.scroll.unseen, 1);

    let frame = render_to_string(&state);
    assert!(
        !frame.contains("fresh answer"),
        "pinned view must not jump to live edge\n{frame}"
    );
    assert!(frame.contains("End latest"), "unseen hint must be visible");
}

#[test]
fn end_key_returns_to_live_edge() {
    let mut state = overflow_state();
    scroll_at_size(&mut state, ScrollIntent::PageUp, 80, 24);
    assert!(state.scroll.is_pinned());
    scroll_at_size(&mut state, ScrollIntent::LiveEdge, 80, 24);
    assert!(state.scroll.is_live_edge());
    let frame = render_to_string(&state);
    assert!(frame.contains("answer number 59"));
}

#[test]
fn scrolling_back_to_bottom_returns_to_live_edge() {
    let mut state = overflow_state();
    scroll_at_size(&mut state, ScrollIntent::Up, 80, 24);
    scroll_at_size(&mut state, ScrollIntent::Down, 80, 24);
    assert!(state.scroll.is_live_edge());
    assert_eq!(state.scroll.unseen, 0);
}
