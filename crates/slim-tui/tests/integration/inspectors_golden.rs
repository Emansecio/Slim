use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::style::Color;
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::inspector::InspectorKind;
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{resolve_theme, Capabilities, ColorDepth};

fn ctrl(character: char) -> Action {
    Action::Key(KeyEvent::new(
        KeyCode::Char(character),
        KeyModifiers::CONTROL,
    ))
}

fn press(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn render_buffer(state: &AppState, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    };
    terminal
        .draw(|frame| render_frame(frame, state, capabilities, &mut WrapCache::default()))
        .expect("render");
    terminal.backend().buffer().clone()
}

fn render(state: &AppState, width: u16, height: u16) -> String {
    let buffer = render_buffer(state, width, height);
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
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

fn visible_cells_with_foreground(buffer: &Buffer, color: Color) -> usize {
    (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let cell = &buffer[(x, y)];
            cell.fg == color && !cell.symbol().trim().is_empty()
        })
        .count()
}

fn truecolor_error() -> Color {
    let theme = resolve_theme(Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    });
    Color::Rgb(theme.error.0, theme.error.1, theme.error.2)
}

#[test]
fn restored_tools_are_neutral_history_in_both_inspectors() {
    use slim_tui::api::{SessionId, ToolBatchId, ToolCallId, TranscriptMessage, TranscriptRole};
    let mut state = AppState::new();
    state.apply_event(UiEvent::SessionRestored {
        session_id: SessionId("saved".into()),
        cwd: "workspace".into(),
        messages: vec![TranscriptMessage {
            role: TranscriptRole::Tool {
                batch_id: ToolBatchId("saved-batch".into()),
                call_id: ToolCallId("saved-call".into()),
                name: "write".into(),
                arguments: "path=denied.txt".into(),
            },
            text: "error: access denied".into(),
        }],
        skill_names: Vec::new(),
    });
    for key in ['d', 'j'] {
        reduce(&mut state, ctrl(key));
        let frame = render(&state, 80, 24);
        assert!(frame.contains("- write · history"), "{frame}");
        assert!(!frame.contains('✓'), "{frame}");
        let buffer = render_buffer(&state, 80, 24);
        assert_eq!(
            cell_at_token(&buffer, "- write · history").fg,
            cell_at_token(&buffer, "write · history").fg
        );
    }
}

#[test]
fn inspector_shortcuts_toggle_truthful_responsive_panels() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: format!("{}drawer-tail-marker", "x".repeat(90)),
    });
    state.apply_event(UiEvent::AssistantEnded);
    reduce(&mut state, ctrl('d'));
    assert_eq!(state.inspector.active, Some(InspectorKind::Diff));
    let wide = render(&state, 100, 30);
    assert!(wide.contains("Changes"), "{wide}");
    assert!(wide.contains("No file changes reported"), "{wide}");
    assert!(
        wide.contains("drawer-tail-marker"),
        "wide drawer must reflow rather than erase transcript content\n{wide}"
    );

    reduce(&mut state, ctrl('j'));
    assert_eq!(state.inspector.active, Some(InspectorKind::Activity));
    let narrow = render(&state, 99, 18);
    assert!(narrow.contains("Activity"), "{narrow}");

    reduce(&mut state, press(KeyCode::Esc));
    assert_eq!(state.inspector.active, None);
}

#[test]
fn transcript_search_reports_and_cycles_real_matches() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "review the parser".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "The parser is stable.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);

    reduce(&mut state, ctrl('f'));
    for character in "parser".chars() {
        reduce(&mut state, press(KeyCode::Char(character)));
    }
    let first = render(&state, 80, 24);
    assert!(first.contains("Find: parser"), "{first}");
    assert!(first.contains("1/2"), "{first}");

    reduce(&mut state, press(KeyCode::Enter));
    let second = render(&state, 80, 24);
    assert!(second.contains("2/2"), "{second}");
}

#[test]
fn search_filter_cycles_all_errors_and_tools() {
    use slim_tui::api::{ToolBatchId, ToolCallId};

    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: "boom is here".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::ToolStarted {
        batch_id: ToolBatchId("batch-1".into()),
        call_id: ToolCallId("call-1".into()),
        name: "shell".into(),
        arguments_summary: String::new(),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: ToolBatchId("batch-1".into()),
        call_id: ToolCallId("call-1".into()),
        name: "shell".into(),
        preview: "boom failed here".into(),
        content_handle: None,
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: ToolBatchId("batch-1".into()),
        call_id: ToolCallId("call-1".into()),
        name: "shell".into(),
        success: false,
        duration_ms: 1,
    });
    state.apply_event(UiEvent::RunFailed {
        run_id: None,
        message: "boom fatal".into(),
    });

    reduce(&mut state, ctrl('f'));
    for character in "boom".chars() {
        reduce(&mut state, press(KeyCode::Char(character)));
    }
    let all = render(&state, 80, 24);
    assert!(all.contains("[all]"), "{all}");
    assert!(all.contains("1/3"), "{all}");

    reduce(&mut state, press(KeyCode::Tab));
    let errors = render(&state, 80, 24);
    assert!(errors.contains("[errors]"), "{errors}");
    assert!(errors.contains("1/2"), "{errors}");

    // Cycling the filter resets the selection instead of keeping a stale one.
    reduce(&mut state, press(KeyCode::Enter));
    let advanced = render(&state, 80, 24);
    assert!(advanced.contains("2/2"), "{advanced}");
    reduce(&mut state, press(KeyCode::Tab));
    let tools = render(&state, 80, 24);
    assert!(tools.contains("[tools]"), "{tools}");
    assert!(tools.contains("1/1"), "{tools}");

    reduce(&mut state, press(KeyCode::Tab));
    let back = render(&state, 80, 24);
    assert!(back.contains("[all]"), "{back}");
    assert!(back.contains("1/3"), "{back}");
}

#[test]
fn copy_shortcut_targets_the_latest_assistant_and_reports_the_real_result() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: "copy this answer".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let effects = reduce(&mut state, ctrl('y'));
    assert!(effects.contains(&Effect::CopyToClipboard("copy this answer".into())));

    reduce(&mut state, Action::ClipboardCompleted { success: false });
    assert!(state
        .notifications
        .iter()
        .any(|notification| notification.as_str() == "Clipboard unavailable"));
}

#[test]
fn image_command_routes_a_real_path_to_the_runtime() {
    let mut state = AppState::new();
    state.composer.insert_text("/image C:\\tmp\\screen.png");
    let effects = reduce(&mut state, press(KeyCode::Enter));
    assert!(
        effects.contains(&Effect::Send(slim_tui::api::UiCommand::AttachImage(
            "C:\\tmp\\screen.png".into()
        )))
    );
}

#[test]
fn diagnostics_preserve_provider_phase_timings() {
    let mut state = AppState::new();
    for (phase, elapsed_ms) in [
        (slim_core::ProviderPhase::Connecting, 0),
        (slim_core::ProviderPhase::HeadersReceived, 12),
        (slim_core::ProviderPhase::FirstByte, 20),
        (slim_core::ProviderPhase::FirstSemantic, 35),
    ] {
        state.apply_event(UiEvent::ProviderPhaseChanged {
            phase,
            label: "provider".into(),
            elapsed_ms,
        });
    }
    reduce(&mut state, ctrl('g'));

    let frame = render(&state, 160, 24);
    assert!(frame.contains("hdr 12ms · byte 20ms · sem 35ms"), "{frame}");
}

fn workspace_state() -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state.cwd = r"D:\Slim".into();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 51_900,
        context_window_tokens: 272_000,
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::UserMessageAdded {
        text: "Revise a organização visual".into(),
    });
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "Mapeando hierarquia".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);
    state.apply_event(UiEvent::AssistantDelta {
        text: format!(
            "{} assistant-tail-marker",
            "resposta organizada ".repeat(12)
        ),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state
}

#[test]
fn wide_transcript_is_centered_at_reading_width_without_automatic_inspector() {
    let state = workspace_state();
    let wide = render(&state, 140, 30);
    let header = wide.lines().next().unwrap_or_default();

    assert!(
        header.contains("SLIM") && !header.contains("RUNNING"),
        "{wide}"
    );
    assert!(
        !wide.contains(" Run "),
        "automatic inspector must stay absent\n{wide}"
    );
    assert!(
        wide.lines().any(|row| row.trim() == "Slim"),
        "assistant must retain its explicit role label\n{wide}"
    );
    assert!(wide.contains("assistant-tail-marker"), "{wide}");

    let user_line = wide
        .lines()
        .find(|line| line.contains("You  Revise a organização visual"))
        .expect("user prompt");
    assert_eq!(
        user_line.find("You"),
        Some(2),
        "140-column transcript uses full terminal width\n{wide}"
    );
}

#[test]
fn below_default_breakpoint_uses_full_width_single_column_without_run_inspector() {
    for width in [200, 140, 139, 99] {
        let narrow = render(&workspace_state(), width, 24);

        assert!(!narrow.contains(" Run "), "width {width}\n{narrow}");
        let user_line = narrow
            .lines()
            .find(|line| line.contains("You  Revise a organização visual"))
            .expect("user prompt");
        assert_eq!(
            user_line.find("You"),
            Some(2),
            "width {width} transcript should use the full-width reading area\n{narrow}"
        );
        assert!(
            narrow.lines().any(|row| row.trim() == "Slim"),
            "assistant role must remain visible at width {width}\n{narrow}"
        );
        assert!(
            narrow.contains("assistant-tail-marker"),
            "width {width}\n{narrow}"
        );
    }
}

#[test]
fn error_color_is_reserved_for_the_failure_marker() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::FatalError {
        run_id: None,
        message: "provider failed".into(),
    });
    let buffer = render_buffer(&state, 140, 24);
    let error = truecolor_error();

    assert_eq!(cell_at_token(&buffer, "✕").fg, error);
    assert_ne!(cell_at_token(&buffer, "provider failed").fg, error);
    assert_eq!(visible_cells_with_foreground(&buffer, error), 1);
}

#[test]
fn changes_inspector_colors_markers_by_lifecycle() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("write".into()),
        name: "write".into(),
        arguments_summary: "src/lib.rs".into(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("write".into()),
        name: "write".into(),
        success: false,
        duration_ms: 3,
    });
    reduce(&mut state, ctrl('d'));

    let buffer = render_buffer(&state, 100, 24);
    let error = truecolor_error();
    assert_eq!(cell_at_token(&buffer, "×").fg, error);

    reduce(&mut state, ctrl('j'));
    let buffer = render_buffer(&state, 100, 24);
    assert_eq!(
        cell_at_token(&buffer, "✕ write").fg,
        error,
        "activity inspector must use lifecycle glyphs"
    );
}
