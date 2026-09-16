use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;

use slim_tui::api::{ToolBatchId, ToolCallId, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::{palette_matches, reduce, Action, ScrollIntent};
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

fn no_color_caps() -> Capabilities {
    Capabilities {
        color_depth: ColorDepth::None,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Action {
    Action::Key(KeyEvent::new(code, modifiers))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn render(state: &AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn inspector_key(state: &mut AppState, cache: &mut WrapCache, code: KeyCode) {
    let action = terminal_action(
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        state,
        (120, 24),
        cache,
    )
    .expect("inspector key action");
    reduce(state, action);
}

#[test]
fn palette_end_keeps_the_last_grouped_command_visible() {
    let mut state = AppState::new();
    reduce(&mut state, key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    reduce(&mut state, key(KeyCode::End, KeyModifiers::NONE));

    let frame = render(&state, 100, 30);
    assert_eq!(state.palette_selected, palette_matches("").len() - 1);
    assert!(
        frame.contains("inspeção"),
        "group heading must render:\n{frame}"
    );
    assert!(
        frame.contains("> /diagnostics"),
        "End-selected command must remain visible:\n{frame}"
    );
}

#[test]
fn login_end_keeps_the_last_provider_visible_in_a_short_viewport() {
    let mut state = AppState::new();
    state.login_overlay = Some(Default::default());
    reduce(&mut state, key(KeyCode::End, KeyModifiers::NONE));

    let frame = render(&state, 80, 12);
    assert_eq!(
        state.login_overlay.as_ref().map(|overlay| overlay.selected),
        Some(6)
    );
    assert!(
        frame.contains("> OpenCode Zen"),
        "last provider must be visible after End:\n{frame}"
    );
    assert!(
        frame.contains("Esc cancelar"),
        "action hint must remain visible:\n{frame}"
    );
}

#[test]
fn model_cursor_highlight_does_not_move_the_active_model_marker() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.auth_provider = Some(slim_tui::api::LoginProvider::OpenAiCodex);
    reduce(&mut state, key(KeyCode::Char('l'), KeyModifiers::CONTROL));
    let active_model = state.model.clone();
    reduce(&mut state, key(KeyCode::Down, KeyModifiers::NONE));

    let frame = render(&state, 100, 30);
    assert_eq!(
        state.model, active_model,
        "cursor movement must not activate"
    );
    let active_row = frame
        .lines()
        .find(|line| line.contains("GPT-5.6 Sol"))
        .expect("active model row");
    assert!(
        active_row.contains("●"),
        "active model keeps its static marker:\n{frame}"
    );
    let selected_row = frame
        .lines()
        .find(|line| line.contains("GPT-5.6 Terra"))
        .expect("selected model row");
    assert!(
        selected_row.contains("> GPT-5.6 Terra"),
        "cursor highlight follows the selected row:\n{frame}"
    );
    assert!(
        !selected_row.contains("●"),
        "active marker must not follow the cursor:\n{frame}"
    );
}

#[test]
fn inspector_input_measurement_does_not_leak_truecolor_into_no_color_render() {
    let mut state = AppState::new();
    reduce(&mut state, key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    let mut cache = WrapCache::default();
    let action = terminal_action(
        Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
        &state,
        (120, 24),
        &mut cache,
    )
    .expect("inspector key action");
    reduce(&mut state, action);

    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, &state, no_color_caps(), &mut cache))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let has_rgb = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .any(|(x, y)| {
            let cell = &buffer[(x, y)];
            matches!(cell.fg, Color::Rgb(..)) || matches!(cell.bg, Color::Rgb(..))
        });
    assert!(
        !has_rgb,
        "NoColor inspector render must not contain RGB styles"
    );
}

#[test]
fn inspector_end_and_arrows_scroll_without_moving_the_transcript() {
    let mut state = AppState::new();
    for index in 0..24 {
        let batch_id = ToolBatchId(format!("batch-{index}").into());
        let call_id = ToolCallId(format!("call-{index}").into());
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch_id.clone(),
            call_id: call_id.clone(),
            name: format!("tool-{index:02}"),
            arguments_summary: String::new(),
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id,
            call_id,
            name: format!("tool-{index:02}"),
            success: true,
            duration_ms: 1,
        });
    }
    reduce(&mut state, key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    let transcript_before = state.scroll.clone();
    let mut cache = WrapCache::default();
    inspector_key(&mut state, &mut cache, KeyCode::End);
    let at_end = render(&state, 120, 24);
    assert!(
        at_end.contains("tool-23"),
        "End must reveal the last tool:\n{at_end}"
    );

    let action = terminal_action(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &state,
        (120, 24),
        &mut cache,
    )
    .expect("inspector key action");
    assert!(
        matches!(
            action,
            Action::InspectorScroll {
                intent: ScrollIntent::Up,
                ..
            }
        ),
        "inspector arrows must not become transcript scroll: {action:?}"
    );
    reduce(&mut state, action);
    let one_row_up = render(&state, 120, 24);
    assert!(
        one_row_up.contains("tool-22"),
        "Up must reveal earlier tools:\n{one_row_up}"
    );
    assert_ne!(
        at_end, one_row_up,
        "inspector movement must change its viewport"
    );

    inspector_key(&mut state, &mut cache, KeyCode::Home);
    for _ in 0..100 {
        inspector_key(&mut state, &mut cache, KeyCode::Down);
    }
    let many_down = render(&state, 120, 24);
    assert!(
        many_down.contains("tool-23"),
        "repeated Down reaches the end:\n{many_down}"
    );
    inspector_key(&mut state, &mut cache, KeyCode::Up);
    let after_down_up = render(&state, 120, 24);
    assert!(
        after_down_up.contains("tool-22"),
        "Up after a saturated Down moves one row back:\n{after_down_up}"
    );
    assert_ne!(
        many_down, after_down_up,
        "Up after a saturated Down must change the inspector viewport"
    );

    inspector_key(&mut state, &mut cache, KeyCode::Home);
    for _ in 0..100 {
        inspector_key(&mut state, &mut cache, KeyCode::Up);
    }
    let many_up = render(&state, 120, 24);
    inspector_key(&mut state, &mut cache, KeyCode::Down);
    let after_up_down = render(&state, 120, 24);
    assert_ne!(
        many_up, after_up_down,
        "Down after a saturated Up moves one row forward"
    );
    assert_eq!(
        state.scroll, transcript_before,
        "inspector navigation must not move the transcript"
    );
}

#[test]
fn inspector_wheel_uses_the_real_panel_and_respects_modal_precedence() {
    let mut state = AppState::new();
    reduce(&mut state, key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    let mut cache = WrapCache::default();

    let inside = terminal_action(
        mouse(MouseEventKind::ScrollDown, 100, 5),
        &state,
        (120, 24),
        &mut cache,
    )
    .expect("inspector wheel action");
    assert!(matches!(
        inside,
        Action::InspectorScroll {
            intent: ScrollIntent::Down,
            ..
        }
    ));

    let outside = terminal_action(
        mouse(MouseEventKind::ScrollDown, 10, 5),
        &state,
        (120, 24),
        &mut cache,
    )
    .expect("transcript wheel action");
    assert!(matches!(
        outside,
        Action::Scroll {
            intent: ScrollIntent::Down,
            ..
        }
    ));

    state.model_overlay = Some(Default::default());
    assert_eq!(
        terminal_action(
            mouse(MouseEventKind::ScrollUp, 10, 5),
            &state,
            (120, 24),
            &mut cache,
        ),
        None,
        "a modal must consume wheel input instead of scrolling behind it"
    );
}

#[test]
fn inspector_navigation_keeps_warm_rows_across_palette_paints() {
    let mut state = AppState::new();
    for index in 0..24 {
        let batch_id = ToolBatchId(format!("warm-batch-{index}").into());
        let call_id = ToolCallId(format!("warm-call-{index}").into());
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch_id.clone(),
            call_id: call_id.clone(),
            name: format!("warm-tool-{index:02}"),
            arguments_summary: String::new(),
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id,
            call_id,
            name: format!("warm-tool-{index:02}"),
            success: true,
            duration_ms: 1,
        });
    }
    reduce(&mut state, key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    let transcript_before = state.scroll.clone();
    let mut cache = WrapCache::default();

    for code in [
        KeyCode::End,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
    ] {
        inspector_key(&mut state, &mut cache, code);
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
        terminal
            .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
            .expect("truecolor draw");
        let has_rgb = (0..24).any(|row| {
            (75..120).any(|column| {
                let cell = &terminal.backend().buffer()[(column, row)];
                matches!(cell.fg, Color::Rgb(..)) || matches!(cell.bg, Color::Rgb(..))
            })
        });
        assert!(has_rgb, "paint projection must keep truecolor styles");

        terminal
            .draw(|frame| render_frame(frame, &state, no_color_caps(), &mut cache))
            .expect("no-color draw");
        let has_rgb = (0..24).any(|row| {
            (75..120).any(|column| {
                let cell = &terminal.backend().buffer()[(column, row)];
                matches!(cell.fg, Color::Rgb(..)) || matches!(cell.bg, Color::Rgb(..))
            })
        });
        assert!(
            !has_rgb,
            "NoColor paint must not inherit the previous projection"
        );
    }
    assert_eq!(state.scroll, transcript_before);
}

#[test]
fn narrow_search_bar_preserves_the_query_before_hints() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: "hierarquia de navegação".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    reduce(&mut state, key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for character in "hierarquia".chars() {
        reduce(
            &mut state,
            key(KeyCode::Char(character), KeyModifiers::NONE),
        );
    }

    let frame = render(&state, 40, 16);
    let search_row = frame
        .lines()
        .find(|line| line.contains("Buscar:"))
        .expect("search bar row");
    assert!(
        search_row.contains("hierarquia"),
        "the edited query must survive narrow hint elision:\n{frame}"
    );
    assert!(
        search_row.contains("1/1"),
        "result position remains visible in the search bar:\n{frame}"
    );
}
