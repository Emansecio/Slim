//! Golden scroll tests (DESIGN-SLIM-TUI §13.3, gate A2): the newest message
//! must stay visible at live edge, pinning must keep the view stable while new
//! content arrives, and End must return to the live edge.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action, ScrollIntent};
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::Capabilities;

fn render_to_string(state: &AppState) -> String {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut cache = WrapCache::default();
    terminal
        .draw(|frame| render_frame(frame, state, Capabilities {
            color_depth: slim_tui::theme::ColorDepth::TrueColor,
            mouse: false,
            clipboard: false,
            images: false,
            reduced_motion: false,
        }, &mut cache))
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
    assert!(frame.contains("answer number 59"), "newest answer must be visible");
    assert!(!frame.contains("question number 0"), "oldest content may scroll off");
}

#[test]
fn scrolling_up_pins_and_counts_unseen_content() {
    let mut state = overflow_state();
    reduce(&mut state, Action::Scroll(ScrollIntent::Up));
    assert!(state.scroll.pinned);

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
        "pinned view must not jump to live edge"
    );
    assert!(frame.contains("End latest"), "unseen hint must be visible");
}

#[test]
fn end_key_returns_to_live_edge() {
    let mut state = overflow_state();
    reduce(&mut state, Action::Scroll(ScrollIntent::PageUp));
    assert!(state.scroll.pinned);
    reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
    assert!(!state.scroll.pinned);
    let frame = render_to_string(&state);
    assert!(frame.contains("answer number 59"));
}

#[test]
fn scrolling_back_to_bottom_returns_to_live_edge() {
    let mut state = overflow_state();
    reduce(&mut state, Action::Scroll(ScrollIntent::Up));
    reduce(&mut state, Action::Scroll(ScrollIntent::Down));
    assert!(!state.scroll.pinned);
    assert_eq!(state.scroll.unseen, 0);
}
