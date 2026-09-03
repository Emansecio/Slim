//! Model overlay interaction: unified provider-grouped list, key routing
//! (G233) and typed filtering with Space fold/unfold (G234).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{LoginProvider, OpenCodeModelView, UiCommand};
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::render::WrapCache;
use slim_tui::runtime::{render_frame, terminal_action};
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

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn spc() -> KeyEvent {
    press(KeyCode::Char(' '))
}

fn type_text(state: &mut AppState, text: &str) {
    for character in text.chars() {
        reduce(state, Action::Key(press(KeyCode::Char(character))));
    }
}

fn state_with_opencode_catalog() -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state.auth_provider = Some(LoginProvider::OpenCodeGo);
    state.open_code_models = vec![
        OpenCodeModelView {
            id: "grok-4".into(),
            name: "Grok 4".into(),
            context_window_tokens: 256_000,
            max_output_tokens: 32_000,
            reasoning_levels: Vec::new(),
            accepts_images: false,
        },
        OpenCodeModelView {
            id: "qwen3-coder".into(),
            name: "Qwen3 Coder".into(),
            context_window_tokens: 262_144,
            max_output_tokens: 65_536,
            reasoning_levels: Vec::new(),
            accepts_images: false,
        },
    ];
    type_text(&mut state, "/model");
    reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    state
}

fn state_with_long_opencode_catalog() -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state.auth_provider = Some(LoginProvider::OpenCodeGo);
    state.model = "model-000".into();
    state.open_code_models = (0..100)
        .map(|index| OpenCodeModelView {
            id: format!("model-{index:03}"),
            name: format!("Model {index:03}"),
            context_window_tokens: 128_000,
            max_output_tokens: 16_000,
            reasoning_levels: Vec::new(),
            accepts_images: false,
        })
        .collect();
    type_text(&mut state, "/model");
    reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    state
}

/// The unified overlay always lists both provider groups; arrows reach it
/// instead of scrolling the transcript (G233).
#[test]
fn arrows_reach_the_unified_overlay() {
    let state = state_with_opencode_catalog();
    assert!(state.model_overlay.is_some());

    let mut cache = WrapCache::default();
    let down = terminal_action(
        crossterm::event::Event::Key(press(KeyCode::Down)),
        &state,
        (80, 24),
        &mut cache,
    )
    .expect("action");
    assert!(
        matches!(down, Action::Key(key) if key.code == KeyCode::Down),
        "Down must route to the overlay, got {down:?}"
    );

    let mut moved = state.clone();
    reduce(&mut moved, down);
    assert_eq!(
        moved.model_overlay.expect("overlay").selected,
        2,
        "Down moves from the active Sol row into Terra"
    );
}

/// Typing filters across groups; Enter on a Codex alias opens the effort
/// step; non-matching providers hide their models (G233).
#[test]
fn codex_alias_filters_and_enter_opens_effort() {
    let mut state = state_with_opencode_catalog();
    type_text(&mut state, "lu");

    let frame_text = render_to_string(&state);
    assert!(frame_text.contains("Filter: lu"), "filter is visible");
    assert!(frame_text.contains("Luna"), "match stays visible");
    assert!(
        !frame_text.contains("(gpt-5.6-sol)"),
        "non-matches hide from the list"
    );
    assert!(
        !frame_text.contains("Grok 4"),
        "mismatched provider models hide"
    );

    // Filtered rows: Header(0), Alias(Luna), Header(1) → Down to Luna.
    reduce(&mut state, Action::Key(press(KeyCode::Down)));
    reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    let effort = state.effort_overlay.expect("effort step");
    assert_eq!(effort.model.id(), "gpt-5.6-luna");
}

#[test]
fn opencode_catalog_filters_and_enter_sends_selection() {
    let mut state = state_with_opencode_catalog();
    type_text(&mut state, "qwen");
    // Filtered rows: Header(0), Header(1), Catalog(qwen3-coder) → two Downs.
    reduce(&mut state, Action::Key(press(KeyCode::Down)));
    reduce(&mut state, Action::Key(press(KeyCode::Down)));
    let effects = reduce(&mut state, Action::Key(press(KeyCode::Enter)));

    assert!(state.model_overlay.is_none(), "closed on select");
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::SetOpenCodeModel { model, .. }) if model == "qwen3-coder"
    )));
}

/// Space folds and unfolds the group under the cursor; a collapsed group
/// keeps its header visible and hides its models (G234).
#[test]
fn space_folds_and_unfolds_provider_groups() {
    let mut state = state_with_opencode_catalog();
    // selected starts at the active model (Sol, row 1). Move up to focus the
    // Codex header, then fold it.
    reduce(&mut state, Action::Key(press(KeyCode::Up)));
    reduce(&mut state, Action::Key(spc()));

    let overlay = state.model_overlay.as_ref().expect("overlay");
    assert!(
        overlay.collapsed[0],
        "Space on the Codex header folds the group"
    );

    let frame_text = render_to_string(&state);
    assert!(
        frame_text.contains("▸ OpenAI Codex"),
        "collapsed header shows expand glyph"
    );
    assert!(
        !frame_text.contains("(gpt-5.6-"),
        "folded group hides its models from the overlay list"
    );
    assert!(
        frame_text.contains("▸ OpenCode Go"),
        "non-current providers stay collapsed by default"
    );
    assert!(
        !frame_text.contains("Grok 4"),
        "collapsed non-current provider hides its catalog"
    );

    // Space again unfolds.
    reduce(&mut state, Action::Key(spc()));
    let overlay = state.model_overlay.as_ref().expect("overlay");
    assert!(!overlay.collapsed[0], "Space toggles back to expanded");
    let frame_text = render_to_string(&state);
    assert!(
        frame_text.contains("▾ OpenAI Codex"),
        "expanded header shows collapse glyph"
    );
    assert!(frame_text.contains("GPT-5.6 Sol"), "models visible again");
}

#[test]
fn selected_model_stays_visible_in_a_hundred_row_catalog() {
    let mut state = state_with_long_opencode_catalog();
    for _ in 0..80 {
        reduce(&mut state, Action::Key(press(KeyCode::Down)));
    }

    let frame = render_at(&state, 80, 24);

    assert!(
        frame.contains("> Model 080"),
        "selected model must remain visible in the picker viewport:\n{frame}"
    );
    assert!(frame.contains('/'), "picker must expose position feedback");
}

/// The picker box hugs its rows (no 22-row padding), keeps the footer on the
/// last inner row and uses rounded corners like the composer.
#[test]
fn overlay_box_fits_content_with_rounded_corners() {
    let state = state_with_opencode_catalog();
    let frame = render_to_string(&state);
    let lines: Vec<&str> = frame.lines().collect();
    let top = lines
        .iter()
        .position(|line| line.contains("╭ Select model"))
        .unwrap_or_else(|| panic!("rounded title row missing:\n{frame}"));
    let bottom = top
        + lines[top..]
            .iter()
            .position(|line| line.contains('╰'))
            .expect("bottom border");
    assert!(!frame.contains('┌'), "square corners must be gone:\n{frame}");
    assert!(
        lines[bottom - 1].contains("Enter select"),
        "footer must sit on the last inner row:\n{frame}"
    );
    assert!(
        bottom - top - 1 < 12,
        "box must be sized to content, not the fixed cap:\n{frame}"
    );
}

fn render_to_string(state: &AppState) -> String {
    render_at(state, 100, 30)
}

fn render_at(state: &AppState, width: u16, height: u16) -> String {
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

