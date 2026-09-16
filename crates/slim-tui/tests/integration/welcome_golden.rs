use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

use slim_tui::api::{LoginProvider, UiEvent};
use slim_tui::app::AppState;
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{resolve_theme, Capabilities, ColorDepth};

fn caps(color_depth: ColorDepth, reduced_motion: bool) -> Capabilities {
    Capabilities {
        color_depth,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion,
    }
}

fn render(state: &AppState, width: u16, height: u16, capabilities: Capabilities) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, capabilities, &mut WrapCache::default()))
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..buffer.area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            row
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cell_at_token<'a>(buffer: &'a Buffer, token: &str) -> &'a Cell {
    for y in 0..buffer.area.height {
        let row = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>();
        if let Some(byte_index) = row.find(token) {
            let x = UnicodeWidthStr::width(&row[..byte_index]) as u16;
            return &buffer[(x, y)];
        }
    }
    panic!("token not rendered: {token}");
}

#[test]
fn wide_disconnected_welcome_has_only_name_state_and_action() {
    let buffer = render(
        &AppState::new(),
        120,
        30,
        caps(ColorDepth::TrueColor, false),
    );
    let frame = text(&buffer);

    assert!(frame.contains("SLIM"));
    assert!(frame.contains("○  Não conectado"));
    assert!(frame.contains("Use /login para conectar"));
    assert!(!frame.contains("Native coding agent"));
    assert!(!frame
        .chars()
        .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)));
    let theme = resolve_theme(caps(ColorDepth::TrueColor, false));
    assert_eq!(
        cell_at_token(&buffer, "SLIM").fg,
        Color::Rgb(theme.foreground.0, theme.foreground.1, theme.foreground.2)
    );
    assert!(cell_at_token(&buffer, "SLIM")
        .modifier
        .contains(Modifier::BOLD));
    assert_eq!(
        cell_at_token(&buffer, "/login").fg,
        Color::Rgb(theme.accent.0, theme.accent.1, theme.accent.2)
    );
}

#[test]
fn ansi16_connected_welcome_colors_only_the_status_dot() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::OpenAiCodex),
        authenticated: true,
    });
    let buffer = render(&state, 60, 16, caps(ColorDepth::Ansi16, false));
    let frame = text(&buffer);

    assert!(frame.contains("Conectado · OpenAI Codex — ChatGPT Plus/Pro"));
    assert!(frame.contains("Descreva uma tarefa para começar"));
    assert_eq!(cell_at_token(&buffer, "●").fg, Color::LightGreen);
    assert_ne!(cell_at_token(&buffer, "Conectado").fg, Color::LightGreen);
}

#[test]
fn no_color_and_reduced_motion_keep_the_same_compact_content_without_color() {
    let normal = render(&AppState::new(), 32, 10, caps(ColorDepth::None, false));
    let reduced = render(&AppState::new(), 32, 10, caps(ColorDepth::None, true));

    assert_eq!(text(&normal), text(&reduced));
    for token in ["SLIM", "○", "/login"] {
        let fg = cell_at_token(&normal, token).fg;
        assert!(
            matches!(fg, Color::Reset | Color::White),
            "{token} fg must stay monochrome-safe, got {fg:?}"
        );
    }
}

#[test]
fn welcome_keeps_path_and_leaves_shortcuts_to_the_footer() {
    let mut state = AppState::new();
    state.cwd = r"D:\Slim".into();
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::Xai),
        authenticated: true,
    });
    let frame = text(&render(&state, 120, 30, caps(ColorDepth::TrueColor, false)));
    let lines: Vec<&str> = frame.lines().collect();
    let body = lines[..lines.len().saturating_sub(2)].join("\n");
    assert!(body.contains("Descreva uma tarefa para começar"), "{body}");
    assert!(body.contains(r"D:\Slim"), "{body}");
    assert!(
        !body.contains("Ctrl+P commands"),
        "welcome must not repeat footer shortcuts\n{body}"
    );
    assert!(
        !body.contains("Shift+Tab"),
        "welcome must not repeat footer shortcuts\n{body}"
    );
    let footer = lines.iter().rev().take(2).copied().collect::<Vec<_>>();
    assert!(
        footer.iter().any(|line| line.contains("Ctrl+P")),
        "footer still owns commands\n{frame}"
    );
}

#[test]
fn very_short_welcome_removes_spacing_before_content() {
    let two_rows = text(&render(
        &AppState::new(),
        24,
        6,
        caps(ColorDepth::TrueColor, false),
    ));
    assert!(two_rows.contains("SLIM"));
    assert!(two_rows.contains("Não conectado"));
    assert!(two_rows.contains("/login"));

    let one_row = text(&render(
        &AppState::new(),
        24,
        5,
        caps(ColorDepth::TrueColor, false),
    ));
    assert!(one_row.contains("SLIM"));
    assert!(one_row.contains("Não conectado"));
}
