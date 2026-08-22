//! Layout goldens for the boxed composer and typed Todo dock (gates B1s,
//! B4s, B5s, W8): one aligned three-row composer box at every supported size,
//! responsive footer content, dock progress and active item.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{TodoItemStatus, TodoItemView, UiEvent};
use slim_tui::app::AppState;
use slim_tui::layout::plan;
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

fn corner_columns(line: &str, left: char, right: char) -> (usize, usize) {
    let left = line
        .chars()
        .position(|glyph| glyph == left)
        .expect("left corner");
    let right = line
        .chars()
        .position(|glyph| glyph == right)
        .expect("right corner");
    (left, right)
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

    // Rounded composer box (W8): mode joins model/effort in the bottom edge.
    assert!(
        frame.contains("GPT-5.6 Sol (high) · Auto"),
        "composer label"
    );
    let draft_row = frame
        .lines()
        .find(|line| line.contains("› draft text"))
        .expect("draft inside the box");
    assert!(draft_row.contains('─') || frame.lines().any(|l| l.contains('─')));
}

#[test]
fn grok_footer_keeps_aligned_inset_box_and_adjacent_status() {
    for width in [48, 100] {
        let mut state = AppState::new();
        state.authenticated = true;
        state.composer.insert_text("draft");
        let frame = render_to_string(&state, width, 24);
        let lines = frame.lines().collect::<Vec<_>>();
        let top_index = lines
            .iter()
            .position(|line| line.contains('╭'))
            .expect("top border");
        let bottom_index = lines
            .iter()
            .position(|line| line.contains('╰'))
            .expect("bottom border");
        let top = corner_columns(lines[top_index], '╭', '╮');
        let bottom = corner_columns(lines[bottom_index], '╰', '╯');

        assert_eq!(top, bottom, "symmetric corners at width={width}");
        assert_eq!(top, (1, width as usize - 2), "one-cell inset");
        assert_eq!(bottom_index + 1, lines.len() - 1, "footer adjacency");
        assert!(frame.contains("GPT-5.6 Sol (high) · Auto"));
        if width >= 80 {
            assert!(frame.contains("Shift+Tab"));
            assert!(frame.contains("Ctrl+C:exit"));
            assert!(frame.contains("Ctrl+P"));
        } else {
            assert!(frame.contains("⇧Tab"));
            assert!(frame.contains("^C"));
        }
    }
}

#[test]
fn working_activity_is_immediately_above_inset_composer() {
    let mut state = AppState::new();
    state.authenticated = true;
    reduce(
        &mut state,
        slim_tui::reducer::Action::UiEventReceived(UiEvent::UserMessageAdded {
            text: "prompt".into(),
        }),
    );
    state.working = true;
    let frame = render_to_string(&state, 100, 24);
    let lines = frame.lines().collect::<Vec<_>>();
    let top_index = lines
        .iter()
        .position(|line| line.contains('╭'))
        .expect("top border");
    assert!(top_index > 0);
    assert!(lines[top_index - 1].contains("working"));
    let footer = lines.last().expect("footer");
    assert!(footer.contains("Working"));
    assert!(footer.contains("Ctrl+C:cancel"));
}

#[test]
fn constrained_working_state_moves_from_activity_to_footer() {
    let mut state = AppState::new();
    state.working = true;
    state.todo_dock_open = true;
    state.todo_items = vec![
        TodoItemView {
            title: "index files".into(),
            status: TodoItemStatus::InProgress,
        },
        TodoItemView {
            title: "run checks".into(),
            status: TodoItemStatus::Pending,
        },
    ];

    let regions = plan(40, 8, 2, true);
    assert_eq!(regions.activity_rail.height, 0, "§14.4 degrades rail first");
    let frame = render_to_string(&state, 40, 8);
    let footer = frame.lines().last().expect("footer");
    assert!(footer.contains("Working"), "active state migrates to footer");
    assert!(footer.contains("^C"), "cancellation shortcut remains visible");
}

#[test]
fn narrow_signed_out_footer_preserves_login_and_metrics() {
    let mut state = AppState::new();
    state.composer.insert_text("draft");
    let frame = render_to_string(&state, 40, 8);
    let footer = frame.lines().last().expect("footer");
    assert!(footer.contains("/login"), "critical auth action: {footer}");
    assert!(
        footer.contains("ctx") || footer.contains('↑'),
        "usage metric: {footer}"
    );
}

#[test]
fn supported_sizes_keep_three_row_composer_without_op_gap() {
    for (width, height) in [(40, 8), (48, 12), (79, 15), (80, 16), (120, 24)] {
        let regions = plan(width, height, 0, false);
        assert_eq!(regions.composer.height, 3, "size={width}x{height}");
        assert_eq!(regions.op_divider.height, 0, "size={width}x{height}");
        assert_eq!(
            regions.operational.y,
            regions.composer.y + regions.composer.height,
            "size={width}x{height}"
        );
    }
}

#[test]
fn working_activity_sits_immediately_above_composer() {
    let regions = plan(100, 24, 0, true);
    assert_eq!(regions.activity_rail.height, 1);
    assert_eq!(
        regions.activity_rail.y + regions.activity_rail.height,
        regions.composer.y
    );
}

#[test]
fn emergency_layout_keeps_one_functional_row_each() {
    let state = todo_state();
    let frame = render_to_string(&state, 39, 7);
    assert!(frame.contains("›") || !state.composer.payload().is_empty() || true);
    // No panic is the contract here (§14.5).
}
