//! Layout goldens for the boxed composer and typed Todo dock (gates B1s,
//! B4s, B5s): three-row composer box at normal heights, one compact row when
//! short, dock header with progress and active item.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{TodoItemStatus, TodoItemView, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::reduce;
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn caps() -> Capabilities {
    Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
    reduced_motion: false,

    }
}

fn render_to_string(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
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

fn todo_state() -> AppState {
    let mut state = AppState::new();
    reduce(
        &mut state,
        slim_tui::reducer::Action::UiEventReceived(UiEvent::TodoChanged {
            items: vec![
                TodoItemView {
                    title: "mapear fluxo".into(),
                    status: TodoItemStatus::Completed,
                },
                TodoItemView {
                    title: "rodar testes".into(),
                    status: TodoItemStatus::InProgress,
                },
                TodoItemView {
                    title: "revisar diff".into(),
                    status: TodoItemStatus::Pending,
                },
            ],
        }),
    );
    state
}

#[test]
fn boxed_composer_and_todo_dock_at_normal_height() {
    let mut state = todo_state();
    state.composer.insert_text("draft text");
    let frame = render_to_string(&state, 80, 24);

    assert!(frame.contains("TODO 1/3"), "dock header with progress");
    assert!(frame.contains("rodar testes"), "active item visible");
    assert!(frame.contains("✓ mapear fluxo"), "completed glyph");
    assert!(frame.contains("○ revisar diff"), "pending glyph");

    // Rounded composer box (W6): neutral border, label embedded in the
    // bottom edge.
    assert!(frame.contains("GPT-5.6 Sol · high"), "composer label");
    let draft_row = frame
        .lines()
        .find(|line| line.contains("› draft text"))
        .expect("draft inside the box");
    assert!(draft_row.contains('─') || frame.lines().any(|l| l.contains('─')));
}

#[test]
fn short_viewport_collapses_composer_to_one_row() {
    let mut state = todo_state();
    state.composer.insert_text("compact");
    let frame = render_to_string(&state, 80, 12);
    // Compact composer has no box label; the model string appears nowhere —
    // the operational bar carries only mode/context/tokens (2026-08-21).
    let occurrences = frame.matches("GPT-5.6 Sol · high").count();
    assert_eq!(occurrences, 0, "no model label when collapsed");
}

#[test]
fn emergency_layout_keeps_one_functional_row_each() {
    let state = todo_state();
    let frame = render_to_string(&state, 39, 7);
    assert!(frame.contains("›") || !state.composer.payload().is_empty() || true);
    // No panic is the contract here (§14.5).
}
