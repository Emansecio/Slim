//! Model overlay interaction: provider groups, inline effort/speed and
//! stable selection through filtering and catalog refreshes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{
    LoginProvider, ModelAlias, OpenCodeCatalogSource, OpenCodeModelView, ReasoningEffort,
    UiCommand, UiEvent,
};
use slim_tui::app::{AppState, EffortTarget, ModelRow};
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

/// Filtering lands on an actionable model. Effort changes in that same list.
#[test]
fn codex_alias_filters_and_adjusts_effort_inline() {
    let mut state = state_with_opencode_catalog();
    type_text(&mut state, "lu");

    let frame_text = render_to_string(&state);
    assert!(
        frame_text.contains("Modelo · lu"),
        "filter is visible in the title"
    );
    assert!(frame_text.contains("Luna"), "match stays visible");
    assert!(
        !frame_text.contains("(gpt-5.6-sol)"),
        "non-matches hide from the list"
    );
    assert!(
        !frame_text.contains("Grok 4"),
        "mismatched provider models hide"
    );

    assert_eq!(state.model_overlay.as_ref().unwrap().selected, 0);
    reduce(&mut state, Action::Key(press(KeyCode::Right)));
    let overlay = state.model_overlay.as_ref().expect("picker stays open");
    assert_eq!(
        overlay.pending_efforts,
        vec![(
            EffortTarget::Alias(ModelAlias::Luna),
            ReasoningEffort::XHigh
        )]
    );
    let effects = reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    assert!(effects.contains(&Effect::Send(UiCommand::SetModel {
        model: ModelAlias::Luna,
        effort: ReasoningEffort::XHigh,
        fast: false,
    })));
}

#[test]
fn opencode_catalog_filters_and_enter_sends_selection() {
    let mut state = state_with_opencode_catalog();
    type_text(&mut state, "qwen");
    // Position by predicate: other provider groups also match "qwen" and
    // group headers are rows, so counting Downs is not stable.
    let go_ids: Vec<String> = state
        .open_code_models
        .iter()
        .map(|model| model.id.clone())
        .collect();
    select_row(
        &mut state,
        |row| matches!(row, ModelRow::Catalog(index) if go_ids[*index] == "qwen3-coder"),
    );
    let effects = reduce(&mut state, Action::Key(press(KeyCode::Enter)));

    assert!(state.model_overlay.is_none(), "closed on select");
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::SetOpenCodeModel { model, .. }) if model == "qwen3-coder"
    )));
}

#[test]
fn astra_picker_selects_reasoning_and_speed_and_cancel_does_not_apply_speed() {
    use slim_tui::api::{ModelAlias, ReasoningEffort};
    let mut state = state_with_opencode_catalog();
    type_text(&mut state, "astra");
    assert!(render_to_string(&state).contains("GPT-6 Astra"));
    let normal = render_to_string(&state);
    assert!(normal.contains("velocidade Normal"));
    assert!(!normal.contains("Ultra"));
    reduce(&mut state, Action::Key(press(KeyCode::Tab)));
    let fast = render_to_string(&state);
    assert!(fast.contains("velocidade Rápida"));
    assert!(fast.contains("Tab velocidade"), "{fast}");
    reduce(&mut state, Action::Key(press(KeyCode::Esc)));
    assert!(
        !state.codex_fast,
        "cancel must not change the session speed"
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
    );
    type_text(&mut state, "astra");
    assert!(!state.model_overlay.as_ref().unwrap().pending_fast);
    reduce(&mut state, Action::Key(press(KeyCode::Tab)));
    reduce(&mut state, Action::Key(press(KeyCode::Right)));
    reduce(&mut state, Action::Key(press(KeyCode::Right)));
    let effects = reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    assert!(effects.contains(&Effect::Send(UiCommand::SetModel {
        model: ModelAlias::Astra,
        effort: ReasoningEffort::Max,
        fast: true,
    })));
}

fn state_with_zen_catalog() -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state.auth_provider = Some(LoginProvider::OpenCodeZen);
    // The picker expands the group of the model in use (G234).
    state.model = "muse-spark-1.3-contributor-free".into();
    state.zen_models = vec![OpenCodeModelView {
        id: "muse-spark-1.3-contributor-free".into(),
        name: "Muse Spark 1.3 Free".into(),
        context_window_tokens: 1_048_576,
        max_output_tokens: 131_072,
        reasoning_levels: vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ],
        accepts_images: true,
    }];
    type_text(&mut state, "/model");
    reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    assert!(state.model_overlay.is_some(), "picker opens");
    state
}

/// Headers are rows, so tests position on a row by predicate instead of
/// counting Downs.
fn select_row(state: &mut AppState, predicate: impl Fn(&ModelRow) -> bool) {
    let rows = state.model_overlay.clone().expect("picker").rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    );
    let index = rows.iter().position(predicate).expect("row in the picker");
    reduce(state, Action::Key(press(KeyCode::Home)));
    for _ in 0..index {
        reduce(state, Action::Key(press(KeyCode::Down)));
    }
    assert_eq!(
        state.model_overlay.as_ref().expect("picker").selected,
        index
    );
}

/// Catalog levels stay in the model list; Codex speed is absent there.
#[test]
fn catalog_effort_is_inline_without_codex_speed() {
    let mut state = state_with_zen_catalog();
    select_row(&mut state, |row| matches!(row, ModelRow::Zen(0)));
    let frame = render_to_string(&state);
    // The focused row carries its effort as a meter over the offered levels.
    let focused = frame
        .lines()
        .find(|line| line.contains('>') && line.contains("High"))
        .unwrap_or_else(|| panic!("effort control is rendered:\n{frame}"));
    assert!(focused.contains("▰▰▰▱ High"), "{focused}");
    // Context and output read as compact token counts in the detail pane.
    assert!(frame.contains("contexto 1M · saída 131k"), "{frame}");
    assert!(
        !frame.contains("velocidade"),
        "catalog models have no service tier:\n{frame}"
    );
    reduce(&mut state, Action::Key(press(KeyCode::Left)));
    assert!(
        render_to_string(&state).contains("▰▰▱▱ Medium"),
        "the meter follows the arrows"
    );
    let effects = reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    assert!(state.effort_overlay.is_none());
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::SetZenModel {
            effort: ReasoningEffort::Medium,
            ..
        })
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
fn spaces_in_a_model_search_are_text_and_enter_uses_the_first_match() {
    let mut state = state_with_opencode_catalog();
    type_text(&mut state, "grok 4");
    let overlay = state.model_overlay.as_ref().expect("picker");
    assert_eq!(overlay.filter, "grok 4");
    assert_eq!(
        overlay.selected, 0,
        "filtered headers are not focus targets"
    );
    let effects = reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    assert!(effects.iter().any(|effect| matches!(effect,
        Effect::Send(UiCommand::SetOpenCodeModel { model, .. }) if model == "grok-4"
    )));
}

#[test]
fn catalog_refresh_keeps_model_identity_and_removed_focus_cannot_apply() {
    let mut state = state_with_long_opencode_catalog();
    select_row(&mut state, |row| matches!(row, ModelRow::Catalog(50)));
    let mut updated = state.open_code_models.clone();
    updated.insert(
        0,
        OpenCodeModelView {
            id: "new-model".into(),
            name: "New Model".into(),
            context_window_tokens: 128_000,
            max_output_tokens: 16_000,
            reasoning_levels: Vec::new(),
            accepts_images: false,
        },
    );
    state.apply_event(UiEvent::OpenCodeCatalogLoaded {
        models: updated.clone(),
        source: OpenCodeCatalogSource::Live,
    });
    let overlay = state.model_overlay.as_ref().unwrap();
    let rows = overlay.rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    );
    assert!(matches!(rows[overlay.selected], ModelRow::Catalog(51)));
    assert!(!overlay.selection_lost);

    updated.retain(|model| model.id != "model-050");
    state.apply_event(UiEvent::OpenCodeCatalogLoaded {
        models: updated.clone(),
        source: OpenCodeCatalogSource::Live,
    });
    assert!(state.model_overlay.as_ref().unwrap().selection_lost);
    state.apply_event(UiEvent::OpenCodeCatalogLoaded {
        models: updated,
        source: OpenCodeCatalogSource::Live,
    });
    assert!(
        state.model_overlay.as_ref().unwrap().selection_lost,
        "another refresh cannot silently validate a different numeric row"
    );
    let effects = reduce(&mut state, Action::Key(press(KeyCode::Enter)));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::SetOpenCodeModel { .. }))));
    assert!(state.model_overlay.is_some());
}

#[test]
fn active_model_identity_includes_the_provider_when_catalog_ids_overlap() {
    let mut state = state_with_opencode_catalog();
    state.model = "gpt-5.6-sol".into();
    state.auth_provider = Some(slim_tui::api::LoginProvider::OpenCodeGo);
    state.open_code_models.insert(
        0,
        OpenCodeModelView {
            id: "gpt-5.6-sol".into(),
            name: "Catalog Sol".into(),
            context_window_tokens: 128_000,
            max_output_tokens: 16_000,
            reasoning_levels: Vec::new(),
            accepts_images: false,
        },
    );
    reduce(&mut state, Action::Key(press(KeyCode::Esc)));
    reduce(
        &mut state,
        Action::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('l'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    let overlay = state.model_overlay.as_ref().expect("picker");
    let rows = overlay.rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    );
    assert!(matches!(rows[overlay.selected], ModelRow::Catalog(0)));
    let frame = render_to_string(&state);
    assert!(frame.contains("● Catalog Sol"), "{frame}");
}

#[test]
fn selected_model_stays_visible_in_a_hundred_row_catalog() {
    let mut state = state_with_long_opencode_catalog();
    for _ in 0..80 {
        reduce(&mut state, Action::Key(press(KeyCode::Down)));
    }

    let frame = render_at(&state, 80, 24);

    assert!(
        frame.contains(">     Model 080"),
        "selected model must remain visible in the picker viewport:\n{frame}"
    );
    assert!(frame.contains('/'), "picker must expose position feedback");
}

/// Provider groups read as headings with their size; models sit one step in
/// under them; the position and the detail pane speak about models only.
#[test]
fn groups_are_headings_and_models_sit_under_them() {
    let mut state = state_with_opencode_catalog();
    let frame = render_to_string(&state);
    let rows: Vec<&str> = frame.lines().collect();
    let heading = rows
        .iter()
        .find(|row| row.contains("▾ OpenAI Codex"))
        .unwrap_or_else(|| panic!("{frame}"));
    assert!(heading.contains("OpenAI Codex ─"), "{heading}");
    assert!(
        heading
            .trim_end()
            .trim_end_matches('│')
            .trim_end()
            .ends_with(" 4"),
        "the heading counts the group's models\n{heading}"
    );
    let collapsed = rows
        .iter()
        .find(|row| row.contains("▸ OpenCode Go"))
        .unwrap_or_else(|| panic!("{frame}"));
    assert!(
        collapsed
            .trim_end()
            .trim_end_matches('│')
            .trim_end()
            .ends_with(" 2"),
        "{collapsed}"
    );
    let column = |row: &str, needle: &str| row[..row.find(needle).unwrap()].chars().count();
    let title_column = column(heading, "OpenAI");
    let model = rows
        .iter()
        .find(|row| row.contains("GPT-5.6 Terra"))
        .unwrap_or_else(|| panic!("{frame}"));
    assert!(
        column(model, "GPT-5.6") > title_column,
        "models sit one step in under their heading\n{frame}"
    );
    // The cursor starts on the active model, the first of four models.
    assert!(frame.contains("1/4 · "), "{frame}");
    assert!(frame.contains("gpt-5.6-sol · OpenAI Codex"), "{frame}");
    assert!(frame.contains("só nesta sessão"), "{frame}");

    // On a heading, the pane describes the group.
    reduce(&mut state, Action::Key(press(KeyCode::Up)));
    let on_heading = render_to_string(&state);
    assert!(
        on_heading.contains("OpenAI Codex · 4 modelos"),
        "{on_heading}"
    );
    assert!(on_heading.contains("Enter recolhe o grupo"), "{on_heading}");
    assert!(on_heading.contains("4 modelos · "), "{on_heading}");
}

/// Filtering does not move the panel or its search cursor.
#[test]
fn overlay_geometry_is_stable_while_filtering() {
    let mut state = state_with_opencode_catalog();
    let frame = render_to_string(&state);
    let lines: Vec<&str> = frame.lines().collect();
    let top = lines
        .iter()
        .position(|line| line.contains("╭ Modelo ·"))
        .unwrap_or_else(|| panic!("rounded title row missing:\n{frame}"));
    let bottom = top
        + lines[top..]
            .iter()
            .position(|line| line.contains('╰'))
            .expect("bottom border");
    assert!(
        !frame.contains('┌'),
        "square corners must be gone:\n{frame}"
    );
    assert!(
        lines[bottom - 1].contains("Enter aplicar"),
        "footer must sit on the last inner row:\n{frame}"
    );
    type_text(&mut state, "astra");
    let filtered = render_to_string(&state);
    let filtered_lines: Vec<&str> = filtered.lines().collect();
    assert!(filtered_lines[top].contains("╭ Modelo · astra"));
    assert!(filtered_lines[bottom].contains('╰'));
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
