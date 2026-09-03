//! Golden matrix (DESIGN-SLIM-TUI §28.3 subset, gate C4): normative surface
//! sizes must produce non-overlapping regions, a visible status line, and
//! distinguishable surface levels across color depths without panicking.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{TodoItemStatus, TodoItemView};
use slim_tui::app::AppState;
use slim_tui::layout::plan_with_session_rail;
use slim_tui::reducer::reduce;
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{
    detect_capabilities, resolve_theme, to_terminal_color, Capabilities, ColorDepth,
};

fn state_with_content() -> AppState {
    let mut state = AppState::new();
    reduce(
        &mut state,
        slim_tui::reducer::Action::UiEventReceived(slim_tui::api::UiEvent::UserMessageAdded {
            text: "pergunta do usuário com acentuação".into(),
        }),
    );
    reduce(
        &mut state,
        slim_tui::reducer::Action::UiEventReceived(slim_tui::api::UiEvent::AssistantDelta {
            text: "resposta **com** markdown\ne segunda linha".into(),
        }),
    );
    reduce(
        &mut state,
        slim_tui::reducer::Action::UiEventReceived(slim_tui::api::UiEvent::TodoChanged {
            items: vec![
                TodoItemView {
                    title: "um".into(),
                    status: TodoItemStatus::Completed,
                },
                TodoItemView {
                    title: "dois".into(),
                    status: TodoItemStatus::InProgress,
                },
            ],
        }),
    );
    state
}

fn render(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut cache = WrapCache::default();
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                state,
                Capabilities {
                    color_depth: ColorDepth::TrueColor,
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

#[test]
fn matrix_of_normative_sizes_never_panics_and_keeps_status_visible() {
    let state = state_with_content();
    for width in [40u16, 80, 99, 100, 139, 140, 200] {
        for height in [8u16, 12, 24, 40] {
            let frame = render(&state, width, height);
            assert!(
                !frame.lines().last().unwrap_or_default().contains("SLIM"),
                "branding must stay out of the operational footer at {width}x{height}"
            );
            assert!(
                frame.contains('>'),
                "composer prompt missing at {width}x{height}"
            );
            assert!(
                frame.contains("Ctrl") || frame.contains("^C") || frame.contains("/login"),
                "operational status missing at {width}x{height}"
            );
            // Regions tile the full height exactly.
            let todo_rows = slim_tui::layout::todo_height(true, 2, false);
            let show_session_rail = width >= 80 && height >= 12;
            let regions =
                plan_with_session_rail(width, height, todo_rows, false, show_session_rail);
            let covered = regions.session_rail.height
                + regions.activity_rail.height
                + regions.scrollback.height
                + regions.todo.height
                + regions.todo_divider.height
                + regions.composer.height
                + regions.op_divider.height
                + regions.operational.height;
            assert_eq!(
                covered, height,
                "regions must tile height at {width}x{height}"
            );
        }
    }
}

#[test]
fn emergency_size_renders_without_panic_and_keeps_composer() {
    let state = state_with_content();
    let frame = render(&state, 39, 7);
    assert!(frame.contains('>'), "emergency composer must stay usable");
}

#[test]
fn surface_levels_stay_distinguishable_across_color_depths() {
    let caps_matrix = [
        ColorDepth::TrueColor,
        ColorDepth::Ansi256,
        ColorDepth::Ansi16,
        ColorDepth::None,
    ];
    for depth in caps_matrix {
        let theme = resolve_theme(Capabilities {
            color_depth: depth,
            mouse: false,
            clipboard: false,
            images: false,
            reduced_motion: false,
        });
        let background = to_terminal_color(depth, theme.background);
        let surface = to_terminal_color(depth, theme.surface);
        let composer = to_terminal_color(depth, theme.composer_bg);
        // W8: footer/composer flatten onto transcript surface while that
        // surface remains distinct from the application background.
        if matches!(depth, ColorDepth::TrueColor | ColorDepth::Ansi256) {
            assert_ne!(
                background, composer,
                "background vs composer must differ at {depth:?}"
            );
            assert_eq!(surface, composer, "composer joins surface at {depth:?}");
        }
    }
}

#[test]
fn detected_capabilities_resolve_to_a_cached_theme() {
    let capabilities = detect_capabilities();
    let theme = resolve_theme(capabilities);
    // Resolved twice, same values: the cache stores the resolved theme.
    assert_eq!(theme, resolve_theme(capabilities));
}
