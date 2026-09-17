use ratatui::backend::{Backend, TestBackend};
use ratatui::layout::Position;
use ratatui::Terminal;

use slim_tui::api::{LoginProvider, SensitiveText};
use slim_tui::app::{AppState, EffortOverlay, LoginOverlay, LoginStage, ModelOverlay};
use slim_tui::inspector::SearchState;
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

struct RenderedFrame {
    backend: TestBackend,
    text: String,
    cursor: Position,
}

fn render(state: &AppState, width: u16, height: u16) -> RenderedFrame {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
        .expect("draw");
    let cursor = terminal.get_cursor_position().expect("cursor");
    let buffer = terminal.backend().buffer();
    let text = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let backend = terminal.backend().clone();
    RenderedFrame {
        backend,
        text,
        cursor,
    }
}

fn assert_cursor_visible(frame: &RenderedFrame) {
    let mut expected = frame.backend.clone();
    expected.show_cursor().expect("show cursor");
    assert_eq!(frame.backend, expected, "cursor should be visible");
}

fn assert_cursor_hidden(frame: &RenderedFrame) {
    let mut expected = frame.backend.clone();
    expected.hide_cursor().expect("hide cursor");
    assert_eq!(frame.backend, expected, "cursor should be hidden");
}

fn login_api_key(value: &str) -> LoginOverlay {
    LoginOverlay {
        selected: 2,
        stage: LoginStage::ApiKey(SensitiveText::from(value.to_owned())),
        ..LoginOverlay::default()
    }
}

#[test]
fn api_key_cursor_is_visible_without_exposing_the_secret() {
    let mut state = AppState::new();
    state.login_overlay = Some(login_api_key("super-secret"));

    let frame = render(&state, 80, 16);
    assert!(frame.text.contains("••••"), "masked key is visible");
    assert!(
        !frame.text.contains("super-secret"),
        "secret leaked\n{}",
        frame.text
    );
    assert!(frame.cursor.x < 80 && frame.cursor.y < 16);
    assert_cursor_visible(&frame);
    assert_eq!(
        frame
            .text
            .lines()
            .nth(frame.cursor.y as usize)
            .unwrap()
            .chars()
            .nth(frame.cursor.x.saturating_sub(1) as usize),
        Some('•')
    );
}

#[test]
fn empty_api_key_cursor_starts_before_placeholder() {
    let mut state = AppState::new();
    state.login_overlay = Some(login_api_key(""));

    let frame = render(&state, 50, 12);
    assert!(frame.text.contains("Cole sua API key…"));
    assert!(frame.cursor.x < 50 && frame.cursor.y < 12);
    assert_cursor_visible(&frame);
    let row = frame
        .text
        .lines()
        .nth(frame.cursor.y as usize)
        .expect("field row");
    assert_eq!(row.chars().nth(frame.cursor.x as usize), Some('C'));
}

#[test]
fn long_unicode_api_key_keeps_cursor_inside_the_modal() {
    let mut state = AppState::new();
    state.login_overlay = Some(login_api_key(&"界".repeat(256)));

    let frame = render(&state, 40, 10);
    assert!(frame.cursor.x < 40 && frame.cursor.y < 10);
    assert!(!frame.text.contains('界'), "raw key leaked\n{}", frame.text);
    assert!(frame.text.contains('•') || frame.text.contains('…'));
}

#[test]
fn login_failure_keeps_error_visible_and_releases_the_cursor() {
    let mut state = AppState::new();
    let mut overlay = login_api_key("secret");
    overlay.progress = Some("falha ao autenticar".into());
    state.login_overlay = Some(overlay);

    let failed = render(&state, 80, 16);
    assert!(failed.text.contains("falha ao autenticar"));
    assert!(failed.cursor.x < 80 && failed.cursor.y < 16);
    assert_cursor_visible(&failed);

    state.login_overlay.as_mut().unwrap().in_progress = true;
    let saving = render(&state, 80, 16);
    assert!(saving.text.contains("Salvando…"));
    assert_cursor_hidden(&saving);
}

#[test]
fn text_overlays_place_cursor_in_their_filter_field() {
    let mut state = AppState::new();
    state.search = Some(SearchState {
        query: "界".into(),
        ..SearchState::default()
    });
    let search = render(&state, 80, 20);
    assert!(search.text.contains("Buscar:"));
    assert!(search.cursor.x < 80 && search.cursor.y < 20);
    assert_cursor_visible(&search);

    state.search = None;
    state.palette_query = Some("界".into());
    let palette = render(&state, 80, 20);
    assert!(palette.text.contains("界"));
    assert!(palette.cursor.x < 80 && palette.cursor.y < 20);
    assert_cursor_visible(&palette);

    state.palette_query = None;
    state.model_overlay = Some(ModelOverlay {
        filter: "界".into(),
        ..ModelOverlay::default()
    });
    let model = render(&state, 80, 20);
    assert!(model.text.contains("Filtro: …") || model.text.contains("Filtro: 界"));
    assert!(model.cursor.x < 80 && model.cursor.y < 20);
    assert_cursor_visible(&model);
}

#[test]
fn non_text_overlay_does_not_leave_a_cursor_on_a_list() {
    let mut state = AppState::new();
    state.effort_overlay = Some(EffortOverlay {
        model: slim_tui::api::ModelAlias::Sol,
        selected: 0,
        fast: false,
    });

    let frame = render(&state, 80, 20);
    assert_cursor_hidden(&frame);
}

#[test]
fn search_does_not_leave_a_cursor_under_a_non_text_modal() {
    let mut state = AppState::new();
    state.search = Some(SearchState::default());
    state.effort_overlay = Some(EffortOverlay {
        model: slim_tui::api::ModelAlias::Sol,
        selected: 0,
        fast: false,
    });

    let frame = render(&state, 80, 20);
    assert_cursor_hidden(&frame);
}

#[test]
fn focused_inputs_remain_in_bounds_on_tiny_frames() {
    let mut login = AppState::new();
    login.login_overlay = Some(login_api_key(&"key".repeat(256)));
    let login_frame = render(&login, 3, 3);
    assert!(login_frame.cursor.x < 3 && login_frame.cursor.y < 3);

    let mut palette = AppState::new();
    palette.palette_query = Some("界".into());
    let palette_frame = render(&palette, 3, 3);
    assert!(palette_frame.cursor.x < 3 && palette_frame.cursor.y < 3);
}

#[test]
fn api_key_overlay_uses_the_selected_provider_title() {
    let mut state = AppState::new();
    let mut overlay = login_api_key("key");
    overlay.selected = 0;
    overlay.stage = LoginStage::ApiKey(SensitiveText::from("key".to_owned()));
    state.login_overlay = Some(overlay);
    let frame = render(&state, 80, 16);
    assert!(frame.text.contains("Anthropic API key"));
    assert_eq!(
        state.login_overlay.as_ref().unwrap().provider(),
        LoginProvider::Anthropic
    );
}
