use super::{
    control_requires_data_barrier, footer_line, measure_scrollback, menu_line, receive_batch,
    terminal_action, toast_row_count, update_visible_stream_state, LaneDrain, Palette,
    CONTROL_BATCH_LIMIT, STREAM_BATCH_LIMIT,
};
use crate::api::{
    McpServerView, McpStatusView, TodoItemStatus, TodoItemView, ToolBatchId, ToolCallId, UiEvent,
    STREAM_EVENT_CAPACITY,
};
use crate::app::{
    ActivityPhase, ActivityState, AppState, ConfirmedSetting, FrameClock, McpOverlay,
    SlashSuggestions,
};
use crate::reducer::{reduce, Action, ScrollIntent};
use crate::render::WrapCache;
use crate::runtime::render_frame;
use crate::selection::{ScreenPos, ScreenSelection};
use crate::theme::{Capabilities, ColorDepth};
use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use std::sync::mpsc;

fn caps() -> Capabilities {
    Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    }
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn state_with_transcript() -> AppState {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "hello from the transcript".into(),
    });
    state
}

#[test]
fn selected_menu_line_fills_the_row_and_keeps_no_color_fallbacks() {
    let palette = Palette::of(caps());
    assert_ne!(palette.menu_selected.bg, palette.surface_alt.bg);
    assert_eq!(palette.menu_selected.bg, Some(Color::Rgb(0x2A, 0x2A, 0x2A)));
    let ansi = Palette::of(Capabilities {
        color_depth: ColorDepth::Ansi16,
        ..caps()
    });
    assert_ne!(ansi.menu_selected.bg, ansi.surface_alt.bg);
    assert_eq!(ansi.menu_selected.bg, Some(Color::DarkGray));
    let ansi256 = Palette::of(Capabilities {
        color_depth: ColorDepth::Ansi256,
        ..caps()
    });
    assert_ne!(ansi256.menu_selected.bg, ansi256.surface_alt.bg);

    let mut terminal = Terminal::new(TestBackend::new(20, 1)).expect("terminal");
    terminal
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new(vec![menu_line(
                    vec![Span::styled("> Option", palette.accent_bold)],
                    true,
                    20,
                    &palette,
                )]),
                frame.area(),
            );
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    for x in 0..20 {
        assert_eq!(buffer[(x, 0)].bg, Color::Rgb(0x2A, 0x2A, 0x2A));
    }

    let no_color = Palette::of(Capabilities {
        color_depth: ColorDepth::None,
        ..caps()
    });
    assert_eq!(no_color.menu_selected.bg, Some(Color::Reset));
    let fallback = menu_line(
        vec![Span::styled("> Option", no_color.accent_bold)],
        true,
        20,
        &no_color,
    );
    assert_eq!(fallback.spans[0].content.chars().next(), Some('>'));
    assert!(fallback.spans[0]
        .style
        .add_modifier
        .contains(Modifier::BOLD));
}

#[test]
fn wide_terminal_does_not_open_the_run_inspector_by_default() {
    let state = state_with_transcript();
    let frame = render_to_string(&state, 160, 24);
    assert!(
        !frame.contains("Ctrl+J  activity"),
        "wide transcript must not dock the Run inspector:\n{frame}"
    );
    assert!(
        !frame.contains("Ctrl+D  changes"),
        "wide transcript must not dock inspector shortcuts:\n{frame}"
    );
}

#[test]
fn explicit_inspector_still_docks_on_a_wide_terminal() {
    let mut state = state_with_transcript();
    state.inspector.active = Some(crate::inspector::InspectorKind::Activity);
    let frame = render_to_string(&state, 160, 24);
    assert!(
        frame.contains("Atividade"),
        "Ctrl+J must still dock the activity inspector:\n{frame}"
    );
}

fn complete_tool(state: &mut AppState, batch: &str, call: &str, name: &str, duration_ms: u64) {
    state.apply_event(UiEvent::ToolStarted {
        batch_id: ToolBatchId(batch.into()),
        call_id: ToolCallId(call.into()),
        name: name.into(),
        arguments_summary: String::new(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: ToolBatchId(batch.into()),
        call_id: ToolCallId(call.into()),
        name: name.into(),
        success: true,
        duration_ms,
    });
}

#[test]
fn assistant_prose_breathes_before_tools_while_thinking_stays_flush() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "plan".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);
    complete_tool(&mut state, "b1", "c1", "list", 3);
    complete_tool(&mut state, "b1", "c2", "read", 4);
    state.apply_event(UiEvent::AssistantDelta {
        text: "Vou explorar o projeto.\n\n".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    complete_tool(&mut state, "b2", "c3", "list", 5);
    complete_tool(&mut state, "b2", "c4", "read", 7);
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "still thinking".into(),
    });
    state.clock.elapsed_ms = 249;

    let frame = render_to_string(&state, 80, 24);
    let rows: Vec<&str> = frame.lines().collect();
    let explore = rows
        .iter()
        .position(|row| row.contains("Vou explorar"))
        .unwrap_or_else(|| panic!("assistant body missing\n{frame}"));
    let second_tools = rows
        .iter()
        .rposition(|row| row.contains("Leu, list"))
        .unwrap_or_else(|| panic!("second tools missing\n{frame}"));
    let thinking = rows
        .iter()
        .position(|row| row.contains("Pensando"))
        .unwrap_or_else(|| panic!("thinking missing\n{frame}"));
    assert_eq!(
        second_tools,
        explore + 2,
        "one breathing row separates assistant prose from the next tool row\n{frame}"
    );
    assert!(rows[explore + 1].trim().is_empty(), "{frame}");
    assert_eq!(
        thinking,
        second_tools + 1,
        "thinking must sit flush against the tool row\n{frame}"
    );
}

#[test]
fn assistant_continuation_breathes_after_tools_and_scroll_up_never_sticks() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta {
        text: "Primeiro trecho.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    complete_tool(&mut state, "b1", "c1", "read", 3);
    state.apply_event(UiEvent::AssistantDelta {
        text: "Continuação.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.clock.elapsed_ms = 10_000;

    let frame = render_to_string(&state, 80, 24);
    let rows: Vec<&str> = frame.lines().collect();
    let tool = rows
        .iter()
        .position(|row| row.contains("read") || row.contains("Leu"))
        .unwrap_or_else(|| panic!("tool row missing\n{frame}"));
    let continuation = rows
        .iter()
        .position(|row| row.contains("Continuação"))
        .unwrap_or_else(|| panic!("continuation missing\n{frame}"));
    assert_eq!(continuation, tool + 2, "{frame}");
    assert!(rows[tool + 1].trim().is_empty(), "{frame}");

    // Every upward step must move: blank leading rows resolve to their block,
    // so the Up anchor steps past them instead of resolving to the start row.
    let mut cache = crate::render::WrapCache::default();
    let index = crate::render::HeightIndex::build(state.blocks(), 80, &mut cache);
    let mut mode = crate::app::FollowMode::Pinned(
        index
            .anchor_for_row(index.total_rows - 1)
            .expect("bottom anchor"),
    );
    let mut previous = u64::MAX;
    loop {
        let metrics = index.metrics(&mode, 1);
        if metrics.viewport_start == 0 {
            break;
        }
        assert!(
            metrics.viewport_start < previous,
            "scroll up stuck at {}",
            metrics.viewport_start
        );
        previous = metrics.viewport_start;
        mode = crate::app::FollowMode::Pinned(metrics.up_anchor.expect("up anchor"));
    }
}

#[test]
fn left_drag_selects_and_right_click_copies_or_pastes() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: "hello world".into(),
    });
    let mut cache = WrapCache::default();
    let size = (80, 24);
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
        .unwrap();
    let area = cache.selection_regions[0].unwrap();
    let row = area.bottom() - 1;
    let start = terminal_action(
        mouse_event(MouseEventKind::Down(MouseButton::Left), 2, row),
        &state,
        size,
        &mut cache,
    );
    reduce(&mut state, start.expect("start"));
    let drag = terminal_action(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), 6, row),
        &state,
        size,
        &mut cache,
    );
    reduce(&mut state, drag.expect("drag"));
    assert_eq!(
        state.selection,
        Some(ScreenSelection {
            anchor: ScreenPos::new(2, row),
            head: ScreenPos::new(6, row),
        })
    );
    let mut copied = String::new();
    terminal
        .draw(|frame| {
            render_frame(frame, &state, caps(), &mut cache);
            copied = super::extract_visible_selection(frame, &state, &cache);
        })
        .unwrap();
    assert_eq!(copied, "hello");
    state.selection_text = copied;
    let right = terminal_action(
        mouse_event(MouseEventKind::Down(MouseButton::Right), 6, 1),
        &state,
        size,
        &mut cache,
    );
    assert_eq!(right, Some(Action::MouseSecondary));
    assert!(reduce(&mut state, right.unwrap())
        .contains(&crate::reducer::Effect::CopyToClipboard("hello".into())));
    let middle = terminal_action(
        mouse_event(MouseEventKind::Down(MouseButton::Middle), 6, 1),
        &state,
        size,
        &mut cache,
    );
    assert_eq!(middle, Some(Action::RequestClipboardPaste));
}

#[test]
fn selection_stays_in_transcript_when_dragged_over_composer() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: "alpha  \nbeta".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.composer.insert_text("PRIVATE DRAFT");
    let mut cache = WrapCache::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let row = (0..24)
        .find(|y| {
            (0..80)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("alpha")
        })
        .unwrap();
    for event in [
        mouse_event(MouseEventKind::Down(MouseButton::Left), 2, row),
        mouse_event(MouseEventKind::Drag(MouseButton::Left), 70, 23),
        mouse_event(MouseEventKind::Up(MouseButton::Left), 70, 23),
    ] {
        let action = terminal_action(event, &state, (80, 24), &mut cache).unwrap();
        reduce(&mut state, action);
    }
    assert!(
        state.selection_text.is_empty(),
        "drag has not been painted yet"
    );
    let copy_event = mouse_event(MouseEventKind::Down(MouseButton::Right), 70, 23);
    assert!(super::is_selection_copy_event(&copy_event));
    let mut copied = String::new();
    terminal
        .draw(|frame| {
            render_frame(frame, &state, caps(), &mut cache);
            copied = super::extract_visible_selection(frame, &state, &cache);
        })
        .unwrap();
    assert!(
        copied.contains("alpha") && copied.contains("beta"),
        "{copied:?}"
    );
    assert!(
        !copied.contains("PRIVATE DRAFT") && !copied.contains("Ctrl+C"),
        "{copied:?}"
    );
    let selected_bg = Palette::of(caps()).selection;
    let area = state.selection_area.unwrap();
    let buffer = terminal.backend().buffer();
    for y in 0..24 {
        for x in 0..80 {
            if buffer[(x, y)].bg == selected_bg {
                assert!(super::area_contains(area, x, y));
                assert!(x < 10, "padding at {x},{y} was highlighted");
            }
        }
    }
    state.selection_text = copied.clone();
    let action = terminal_action(copy_event, &state, (80, 24), &mut cache).unwrap();
    assert!(reduce(&mut state, action).contains(&crate::reducer::Effect::CopyToClipboard(copied)));
    let action = terminal_action(
        mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 21),
        &state,
        (80, 24),
        &mut cache,
    )
    .unwrap();
    reduce(&mut state, action);
    assert!(
        state.selection.is_none(),
        "composer does not start transcript selection"
    );
}

#[test]
fn transcript_selection_survives_unrelated_growth_without_following_tail() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: "alpha".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let mut cache = WrapCache::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
        .unwrap();
    let area = cache.selection_regions[0].unwrap();
    let row = area.bottom() - 1;
    for event in [
        mouse_event(MouseEventKind::Down(MouseButton::Left), area.x, row),
        mouse_event(MouseEventKind::Drag(MouseButton::Left), area.x + 4, row),
    ] {
        let action = terminal_action(event, &state, (80, 24), &mut cache).unwrap();
        reduce(&mut state, action);
    }
    assert!(state.scroll.is_pinned());
    let mut selected = String::new();
    terminal
        .draw(|frame| {
            render_frame(frame, &state, caps(), &mut cache);
            selected = super::extract_visible_selection(frame, &state, &cache);
        })
        .unwrap();
    assert_eq!(selected, "alpha");
    state.selection_text = selected.clone();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "unrelated message".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "unrelated\n".repeat(30),
    });
    terminal
        .draw(|frame| {
            render_frame(frame, &state, caps(), &mut cache);
            selected = super::extract_visible_selection(frame, &state, &cache);
        })
        .unwrap();
    assert_eq!(selected, "alpha");
}

#[test]
fn changed_selected_cells_cannot_silently_retarget_copy() {
    let mut state = AppState::new();
    let area = ratatui::layout::Rect::new(0, 0, 5, 1);
    state.selection = Some(ScreenSelection {
        anchor: ScreenPos::new(0, 0),
        head: ScreenPos::new(4, 0),
    });
    state.selection_area = Some(area);
    let mut cache = WrapCache::default();
    cache.selection_regions[0] = Some(area);
    let mut terminal = Terminal::new(TestBackend::new(5, 1)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new("alpha"), area);
            assert!(super::validate_painted_selection(frame, &state, &mut cache));
        })
        .unwrap();
    state.selection_text = "alpha".into();
    for text in ["omega", "alpha"] {
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(text), area);
                assert!(!super::validate_painted_selection(
                    frame, &state, &mut cache
                ));
                assert!(super::extract_visible_selection(frame, &state, &cache).is_empty());
            })
            .unwrap();
    }
    reduce(&mut state, Action::ClearScreenSelection);
    for event in [
        mouse_event(MouseEventKind::Down(MouseButton::Right), 1, 0),
        Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )),
    ] {
        assert!(terminal_action(event, &state, (5, 1), &mut cache).is_none());
    }
    assert!(!state.shutdown);
}

#[test]
fn resize_then_copy_in_one_batch_does_not_cancel_or_paste() {
    let mut state = AppState::new();
    let area = ratatui::layout::Rect::new(0, 0, 5, 1);
    let selection = ScreenSelection {
        anchor: ScreenPos::new(0, 0),
        head: ScreenPos::new(4, 0),
    };
    state.selection = Some(selection);
    state.selection_area = Some(area);
    state.selection_text = "alpha".into();
    let mut cache = WrapCache::default();
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    buffer.set_string(0, 0, "alpha", ratatui::style::Style::default());
    cache.painted_selection = Some(crate::selection::capture_selection(
        &buffer, selection, area,
    ));
    let resize = terminal_action(Event::Resize(4, 1), &state, (4, 1), &mut cache).unwrap();
    reduce(&mut state, resize);
    assert!(state.selection.is_none());
    for event in [
        mouse_event(MouseEventKind::Down(MouseButton::Right), 1, 0),
        Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )),
    ] {
        assert!(terminal_action(event, &state, (4, 1), &mut cache).is_none());
    }
}

#[test]
fn selection_inside_inspector_excludes_border_and_transcript() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AssistantDelta {
        text: "TRANSCRIPT ONLY".into(),
    });
    state.activity = None;
    state.inspector.active = Some(crate::inspector::InspectorKind::Activity);
    let mut cache = WrapCache::default();
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
        .unwrap();
    let area = cache.selection_regions[1].unwrap();
    for event in [
        mouse_event(MouseEventKind::Down(MouseButton::Left), area.x, area.y),
        mouse_event(MouseEventKind::Drag(MouseButton::Left), 0, 23),
    ] {
        let action = terminal_action(event, &state, (120, 24), &mut cache).unwrap();
        reduce(&mut state, action);
    }
    let mut copied = String::new();
    terminal
        .draw(|frame| {
            render_frame(frame, &state, caps(), &mut cache);
            copied = super::extract_visible_selection(frame, &state, &cache);
        })
        .unwrap();
    assert!(copied.contains("Cronologia"), "{copied:?}");
    assert!(
        !copied.contains("TRANSCRIPT ONLY") && !copied.contains("scroll") && !copied.contains('│'),
        "{copied:?}"
    );
}

#[test]
fn wheel_still_scrolls_when_mouse_is_captured() {
    let state = AppState::new();
    let mut cache = WrapCache::default();
    let action = terminal_action(
        mouse_event(MouseEventKind::ScrollUp, 0, 0),
        &state,
        (80, 24),
        &mut cache,
    );
    assert!(matches!(
        action,
        Some(Action::Scroll {
            intent: ScrollIntent::Up,
            ..
        })
    ));
}

#[test]
fn slash_popup_keeps_a_late_selection_visible_in_a_small_viewport() {
    let mut state = AppState::new();
    state.set_skill_names_for_test((0..20).map(|index| format!("skill-{index:02}")).collect());
    state.slash_suggestions = Some(SlashSuggestions {
        query: "skill".into(),
        selected: 15,
    });

    let frame = render_to_string(&state, 32, 10);

    assert!(frame.contains("/skill-15"), "{frame}");
    assert!(!frame.contains("/skill-00"), "{frame}");
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

#[test]
fn migrated_causal_suffix_and_cancel_wait_for_queued_stream_data() {
    let tool_output = UiEvent::ToolOutput {
        batch_id: ToolBatchId("batch-1".into()),
        call_id: ToolCallId("call-1".into()),
        name: "mcp".into(),
        output: "mcp operation outcome is uncertain".into(),
        content_handle: None,
    };
    let tool_ended = UiEvent::ToolEnded {
        batch_id: ToolBatchId("batch-1".into()),
        call_id: ToolCallId("call-1".into()),
        name: "read".into(),
        success: false,
        duration_ms: 1,
    };
    assert!(control_requires_data_barrier(&[tool_output]));
    assert!(control_requires_data_barrier(&[tool_ended]));
    assert!(control_requires_data_barrier(&[UiEvent::RunCancelled {
        run_id: 1,
    }]));
    assert!(!control_requires_data_barrier(&[
        UiEvent::AuthStateChanged {
            provider: None,
            authenticated: false,
        },
    ]));
    let mut visible_stream_started = true;
    update_visible_stream_state(
        &UiEvent::RunStarted {
            run_id: 2,
            max_mutating_tool_calls: 1,
            max_read_tool_calls: 1,
            max_turns: 1,
        },
        &mut visible_stream_started,
    );
    assert!(!visible_stream_started);
}

#[test]
fn question_arrows_are_routed_to_reducer_instead_of_scrollback() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: crate::api::InteractionRequestId("question-1".into()),
        question: "Which crate?".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "core".into(),
                description: String::new(),
            },
            slim_core::QuestionOption {
                label: "tui".into(),
                description: String::new(),
            },
        ],
        persisted: false,
    });
    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::NONE,
    );
    let mut cache = WrapCache::default();
    let action = terminal_action(
        crossterm::event::Event::Key(key),
        &state,
        (80, 24),
        &mut cache,
    )
    .expect("action");
    assert!(
        matches!(
            action,
            Action::Key(event) if event.code == crossterm::event::KeyCode::Down
        ),
        "pending question must capture arrows: {action:?}"
    );
    reduce(&mut state, action);
    assert_eq!(
        state
            .pending_interaction()
            .map(|interaction| interaction.selected_question_option),
        Some(1)
    );
}

#[test]
fn approval_decision_is_blocked_when_resize_hides_the_card() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ApprovalRequired {
        request_id: crate::api::InteractionRequestId("approval-1".into()),
        summary: "uma solicitação que precisa ser lida".into(),
        persisted: false,
    });
    // Simulate a previously readable frame followed by a resize and Y in
    // the same input batch.  terminal_action must revalidate geometry
    // before allowing the reducer to see a decision key.
    state.approval_content_accessible = true;
    let mut cache = WrapCache::default();
    let action = terminal_action(
        Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('y'),
            crossterm::event::KeyModifiers::NONE,
        )),
        &state,
        (20, 3),
        &mut cache,
    )
    .expect("inaccessible approval still yields a reducer action");
    assert!(matches!(
        action,
        Action::SetApprovalContentAccessible(false)
    ));
    reduce(&mut state, action);
    assert!(!state.approval_content_accessible);
    assert_eq!(
        super::question_composer_hint(&state),
        Some("aprovação · amplie o terminal para ler")
    );
}

#[test]
fn toasts_keep_one_principal_row_and_expose_history() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "prompt".into(),
    });
    state.push_notification("first notice".into());
    state.push_notification("second notice".into());
    let frame = render_to_string(&state, 80, 24);
    assert!(
        frame.contains("second notice") && frame.contains("+1 histórico"),
        "latest toast should expose bounded history:\n{frame}"
    );
    assert!(
        !frame.contains("first notice"),
        "history is inspector-only:\n{frame}"
    );
    assert_eq!(toast_row_count(&state, 24), 1);
}

#[test]
fn toast_deadline_tracks_highlight_then_expiry_and_skips_hidden_rows() {
    let mut state = AppState::new();
    state.push_notification("notice".into());
    let count = toast_row_count(&state, 24);
    assert_eq!(count, 1);
    assert_eq!(
        super::next_toast_visual_deadline_ms(&state, 0, count, true),
        Some(super::INFO_TOAST_HIGHLIGHT_MS)
    );
    assert_eq!(
        super::next_toast_visual_deadline_ms(&state, super::INFO_TOAST_HIGHLIGHT_MS, count, true,),
        Some(crate::app::INFO_TOAST_TTL_MS)
    );
    assert_eq!(
        super::next_toast_visual_deadline_ms(&state, 0, count, false),
        Some(crate::app::INFO_TOAST_TTL_MS)
    );

    state.mcp_overlay = Some(McpOverlay::default());
    let hidden_count = toast_row_count(&state, 24);
    assert_eq!(hidden_count, 0);
    assert_eq!(
        super::next_toast_visual_deadline_ms(&state, 0, hidden_count, true),
        None
    );
}

#[test]
fn toast_is_bold_only_during_initial_highlight_and_reduced_motion_is_stable() {
    let mut state = AppState::new();
    state.push_notification("notice".into());
    let render = |state: &AppState, capabilities| {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                super::render_frame(frame, state, capabilities, &mut WrapCache::default())
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    };
    let notice_modifier = |buffer: &ratatui::buffer::Buffer| {
        for y in 0..buffer.area.height {
            let row = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            if let Some(x) = row.find("notice") {
                return buffer[(x as u16, y)].modifier;
            }
        }
        panic!("notice cell")
    };

    let highlighted = render(&state, caps());
    assert!(notice_modifier(&highlighted).contains(Modifier::BOLD));

    state.clock.elapsed_ms = super::INFO_TOAST_HIGHLIGHT_MS;
    let stable = render(&state, caps());
    assert!(!notice_modifier(&stable).contains(Modifier::BOLD));
    state.clock.elapsed_ms += 1_000;
    assert_eq!(stable, render(&state, caps()));

    let reduced = Capabilities {
        reduced_motion: true,
        ..caps()
    };
    state.clock.elapsed_ms = 0;
    let reduced_initial = render(&state, reduced);
    assert!(!notice_modifier(&reduced_initial).contains(Modifier::BOLD));
    state.clock.elapsed_ms = super::INFO_TOAST_HIGHLIGHT_MS - 1;
    assert_eq!(reduced_initial, render(&state, reduced));
}

fn mcp_server_view(
    name: &str,
    target: &str,
    status: McpStatusView,
    error: Option<&str>,
) -> McpServerView {
    McpServerView {
        name: name.into(),
        transport: "stdio",
        target: target.into(),
        status,
        tools: None,
        error: error.map(str::to_owned),
    }
}

#[test]
fn mcp_overlay_keeps_error_and_hints_on_their_own_rows() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.mcp_overlay = Some(McpOverlay {
        selected: 1,
        ..McpOverlay::default()
    });
    state.mcp_servers = vec![
        mcp_server_view(
            "burp-hunt",
            r"C:\Users\User\AppData\Local\Programs\Python\Python312\python.exe -B proxy.py",
            McpStatusView::Disconnected,
            None,
        ),
        mcp_server_view(
            "chrome-devtools",
            "npx -y chrome-devtools-mcp@latest",
            McpStatusView::Failed,
            Some("%1 não é um aplicativo Win32 válido. (os error 193)"),
        ),
    ];
    let frame = render_to_string(&state, 80, 24);
    assert!(
        frame.contains("chrome-devtools") && frame.contains("Enter test"),
        "{frame}"
    );
    assert!(
        frame.contains("os error 193") && frame.contains("python.exe"),
        "target and error must remain readable:\n{frame}"
    );
    assert!(
        !frame.contains("npx -y chrome-devtools-mcp@latest %1") && !frame.contains("Entertest"),
        "error and hints must not be jammed onto one truncated row:\n{frame}"
    );
}

#[test]
fn mcp_overlay_hides_status_toast() {
    let mut state = AppState::new();
    state.authenticated = true;
    state
        .notifications
        .push("mcp chrome-devtools: %1 não é um aplicativo Win32 válido. (os error 193)".into());
    state.mcp_overlay = Some(McpOverlay::default());
    state.mcp_servers = vec![mcp_server_view(
        "chrome-devtools",
        "npx -y chrome-devtools-mcp@latest",
        McpStatusView::Failed,
        Some("%1 não é um aplicativo Win32 válido. (os error 193)"),
    )];
    let frame = render_to_string(&state, 80, 24);
    assert!(
        !frame.contains("mcp chrome-devtools:"),
        "toast must not sit on the overlay:\n{frame}"
    );
    assert_eq!(toast_row_count(&state, 24), 0);
}

#[test]
fn wide_activity_rail_includes_turn_and_this_turn_tool_budgets() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started_with_budget(1, 32, 96, 128));
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 1,
        request_id: 1,
        context_tokens: 100,
        context_window_tokens: 128_000,
    });
    let wide = render_to_string(&state, 80, 24);
    assert!(!wide.contains("turnos 1/128"), "{wide}");
    assert!(!wide.contains("leituras 0/96"), "{wide}");
    assert!(!wide.contains("edições 0/32"), "{wide}");
    assert!(
        !wide.contains("Ctrl+C stop"),
        "cancel is already in the footer: {wide}"
    );
    assert!(wide.contains("Ctrl+C cancel"), "{wide}");
    state.turns_used = 103;
    state.tools_used_read = 77;
    state.tools_used_mutating = 2;
    let counters = render_to_string(&state, 80, 24);
    assert!(counters.contains("turnos 103/128"), "{counters}");
    assert!(counters.contains("leituras 77/96"), "{counters}");
    assert!(!counters.contains("edições 2/32"), "{counters}");
    let narrow = render_to_string(&state, 71, 24);
    assert!(!narrow.contains("turn 103/128"), "{narrow}");
}

#[test]
fn footer_model_keeps_identity_and_effort_with_cell_safe_elision() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.model = "model-宇宙-with-a-very-long-provider-qualified-name".into();
    for width in [38, 58, 78, 118] {
        let label = crate::view_model::model_metadata(&state, width as usize - 2);
        assert!(label.contains("model-"), "{width}: {label}");
        assert!(
            label.contains("(high)"),
            "effort stays beside the model: {width}: {label}"
        );
        assert!(
            !label.contains("Auto") || width < 78,
            "metadata leaves mode to its own footer row: {width}: {label}"
        );
        assert!(unicode_width::UnicodeWidthStr::width(label.as_str()) <= width as usize - 2);
        assert_eq!(label.contains('…'), width < 78, "{width}: {label}");
    }
    state.authenticated = false;
    assert!(super::composer_label(&state, 38, 1).is_empty());
    assert!(crate::view_model::model_metadata(&state, 38).is_empty());
}

#[test]
fn footer_values_are_accented_while_shortcut_descriptions_stay_muted() {
    let mut state = AppState::new();
    state.authenticated = true;
    let palette = super::Palette::of(caps());
    let line = footer_line(
        0,
        2,
        "Auto · GPT-5.6 Sol (high) · ctx ~7%",
        &state,
        true,
        caps(),
        &palette,
    );
    assert_eq!(
        line.spans[0].style,
        palette.secondary.add_modifier(Modifier::BOLD)
    );
    assert_eq!(line.spans[1].style, palette.muted);
    assert_eq!(
        line.spans[2].style,
        palette.secondary.add_modifier(Modifier::BOLD)
    );
    assert_eq!(line.spans[3].style, palette.muted);
    assert_eq!(line.spans[4].style, palette.muted);
}

#[test]
fn footer_highlights_hidden_phase_and_unread_value_without_highlighting_controls() {
    let mut state = AppState::new();
    state.working = true;
    state.scroll.mode = crate::app::FollowMode::Top;
    state.scroll.unseen = 4;
    let palette = super::Palette::of(caps());
    let line = footer_line(
        1,
        2,
        "Thinking · Esc stop · 4 new · End latest",
        &state,
        false,
        caps(),
        &palette,
    );
    assert_eq!(
        line.spans[0].style,
        palette.secondary.add_modifier(Modifier::BOLD)
    );
    assert_eq!(line.spans[2].style, palette.muted);
    assert_eq!(line.spans[4].style, palette.warning);
    assert_eq!(line.spans[6].style, palette.muted);

    let visible_controls = footer_line(
        0,
        1,
        "Esc stop · Ctrl+C cancel",
        &state,
        true,
        caps(),
        &palette,
    );
    assert!(visible_controls
        .spans
        .iter()
        .all(|span| span.style == palette.muted));
}

#[test]
fn activity_inspector_ages_keep_status_ticks_without_enabling_spinner() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta {
        text: "observed".into(),
    });
    state.inspector.active = Some(crate::inspector::InspectorKind::Activity);
    assert!(super::status_clock_visible(&state, 0));
    assert!(!super::motion_needed(
        &state,
        caps(),
        &mut WrapCache::default()
    ));
    let mut cache = WrapCache::default();
    let mut terminal = Terminal::new(TestBackend::new(144, 32)).unwrap();
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
        .unwrap();
    reduce(
        &mut state,
        Action::StatusTick(FrameClock {
            frame: 24,
            elapsed_ms: 2_000,
        }),
    );
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..32)
        .map(|y| {
            (0..144)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("há 2.0s"), "{text}");
    state.palette_query = Some(String::new());
    assert!(!super::status_clock_visible(&state, 0));
    state.palette_query = None;
    state.working = false;
    assert!(!super::status_clock_visible(&state, 0));
}

#[test]
fn microtransition_expires_at_249ms_and_reduced_motion_is_immediate() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.confirmed_setting = Some((ConfirmedSetting::Mode, 0));
    let palette = super::Palette::of(caps());
    let highlighted = super::footer_segment_style("Auto", false, false, &state, caps(), &palette);
    assert!(highlighted.add_modifier.contains(Modifier::BOLD));
    assert_eq!(
        super::next_transition_visual_deadline_ms(&state, 0, caps()),
        Some(super::INFO_TOAST_HIGHLIGHT_MS)
    );

    state.clock.elapsed_ms = super::INFO_TOAST_HIGHLIGHT_MS;
    let expired = super::footer_segment_style("Auto", false, false, &state, caps(), &palette);
    assert!(!expired.add_modifier.contains(Modifier::BOLD));
    assert_eq!(
        super::next_transition_visual_deadline_ms(&state, state.clock.elapsed_ms, caps()),
        None
    );

    state.clock.elapsed_ms = 0;
    let reduced = super::footer_segment_style(
        "Auto",
        false,
        false,
        &state,
        Capabilities {
            reduced_motion: true,
            ..caps()
        },
        &palette,
    );
    assert!(!reduced.add_modifier.contains(Modifier::BOLD));
    assert_eq!(
        super::next_transition_visual_deadline_ms(
            &state,
            state.clock.elapsed_ms,
            Capabilities {
                reduced_motion: true,
                ..caps()
            }
        ),
        None
    );
}

#[test]
fn idle_completed_block_keeps_frame_emphasis_when_spinner_is_suppressed() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::UserMessageAdded {
        text: "mensagem aceita".into(),
    });
    let user_marker_modifier = |state: &AppState| {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| super::render_frame(frame, state, caps(), &mut WrapCache::default()))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        for row in 0..buffer.area.height {
            for column in 0..buffer.area.width {
                if buffer[(column, row)].symbol() == "●" {
                    return buffer[(column, row)].modifier;
                }
            }
        }
        panic!("user marker");
    };

    let recent = user_marker_modifier(&state);
    assert!(recent.contains(Modifier::BOLD));
    state.clock.elapsed_ms = super::INFO_TOAST_HIGHLIGHT_MS;
    let settled = user_marker_modifier(&state);
    assert!(!settled.contains(Modifier::BOLD));
}

#[test]
fn todo_active_marker_is_stable_when_the_primary_indicator_animates() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.working = true;
    state.todo_dock_open = true;
    state.todo_items = vec![TodoItemView {
        reason: None,
        id: None,
        title: "inspect state".into(),
        status: TodoItemStatus::InProgress,
    }];

    let render = |state: &AppState| {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| super::render_frame(frame, state, caps(), &mut WrapCache::default()))
            .expect("draw");
        terminal.backend().buffer().clone()
    };
    let first = render(&state);
    state.clock.frame = 1;
    let second = render(&state);

    let todo_marker = |buffer: &ratatui::buffer::Buffer| {
        let row = (0..buffer.area.height)
            .find(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("inspect state")
            })
            .expect("TODO row");
        let marker_x = (0..buffer.area.width)
            .find(|x| matches!(buffer[(*x, row)].symbol(), "◌" | "~"))
            .expect("TODO active marker");
        buffer[(marker_x, row)].symbol().to_owned()
    };
    assert_eq!(todo_marker(&first), todo_marker(&second));
    assert_ne!(
        first, second,
        "the activity rail/thinking indicator should remain the animated owner"
    );
}

#[test]
fn todo_dock_prioritizes_actionable_items_and_neutralizes_cancelled() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.todo_dock_open = true;
    state.todo_items = vec![
        TodoItemView {
            reason: None,
            id: None,
            title: "old completed".into(),
            status: TodoItemStatus::Completed,
        },
        TodoItemView {
            reason: None,
            id: None,
            title: "still pending".into(),
            status: TodoItemStatus::Pending,
        },
        TodoItemView {
            reason: None,
            id: None,
            title: "blocked dependency".into(),
            status: TodoItemStatus::Blocked,
        },
        TodoItemView {
            reason: None,
            id: None,
            title: "cancelled branch".into(),
            status: TodoItemStatus::Cancelled,
        },
    ];
    let frame = render_to_string(&state, 100, 24);
    let blocked = frame.find("blocked dependency").expect("blocked row");
    let pending = frame.find("still pending").expect("pending row");
    let cancelled = frame.find("cancelled branch").expect("cancelled row");
    let completed = frame.find("old completed").expect("completed row");
    assert!(blocked < pending && pending < cancelled && cancelled < completed);
    let cancelled_row = frame
        .lines()
        .find(|line| line.contains("cancelled branch"))
        .expect("cancelled line");
    assert!(!cancelled_row.contains('x') && !cancelled_row.contains('✕'));
}

#[test]
fn thinking_pulse_moves_to_visible_header_and_freezes_with_motion_disabled() {
    use ratatui::backend::TestBackend;
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::UserMessageAdded {
        text: "history\n".repeat(80),
    });
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "Inspect the current state".into(),
    });
    let render = |state: &AppState, capabilities| {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                super::render_frame(frame, state, capabilities, &mut WrapCache::default())
            })
            .unwrap();
        terminal.backend().buffer().clone()
    };
    for at_top in [false, true] {
        if at_top {
            state.scroll.mode = crate::app::FollowMode::Top;
        }
        state.clock.frame = 0;
        let first = render(&state, caps());
        state.clock.frame = 6;
        let next = render(&state, caps());
        let changed = first
            .content
            .iter()
            .zip(&next.content)
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .collect::<Vec<_>>();
        assert_eq!(
            changed.len(),
            1,
            "only one pulse across transcript and rail"
        );
        let thinking_rows = (0..24)
            .filter(|y| {
                (0..80)
                    .map(|x| first[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("Pensando")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            thinking_rows.len(),
            1,
            "Thinking belongs to the visible header or the ActivityRail, never both"
        );
        assert_eq!(
            changed[0] / 80,
            thinking_rows[0] as usize,
            "pulse belongs to the visible reasoning header, or rail if offscreen"
        );
        for capabilities in [
            Capabilities {
                reduced_motion: true,
                ..caps()
            },
            Capabilities {
                color_depth: ColorDepth::None,
                ..caps()
            },
        ] {
            let still = render(&state, capabilities);
            state.clock.frame = 12;
            assert_eq!(still, render(&state, capabilities));
            state.clock.frame = 6;
        }
    }
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    let finished = render(&state, caps());
    state.clock.frame = 18;
    assert_eq!(finished, render(&state, caps()));
}

#[test]
fn thinking_header_patch_handles_boundary_anchor_offsets_without_body_work() {
    use crate::app::{FollowMode, ScrollAnchor};
    use crate::block::{Block, BlockKind, BlockLifecycle, FoldState};

    let mut state = AppState::new();
    state.authenticated = true;
    // Keep the pinned thinking block below the viewport so the anchor
    // produces a non-zero `skip_rows` value inside its boundary-prefixed
    // line block.
    for index in 0..24 {
        assert!(state.append_block(Block::new(
            format!("history-{index}"),
            BlockKind::Assistant("history row".into()),
            BlockLifecycle::Complete,
        )));
    }
    let mut thinking = Block::new(
        "streaming-thinking",
        BlockKind::Thinking("first body\nsecond body".into()),
        BlockLifecycle::Streaming,
    );
    thinking.fold = FoldState::Expanded;
    thinking.set_turn_boundary_before(true);
    assert!(thinking.turn_boundary_before());
    let thinking_id = thinking.id.clone();
    assert!(state.append_block(thinking));
    for index in 0..24 {
        assert!(state.append_block(Block::new(
            format!("tail-{index}"),
            BlockKind::Assistant("tail row".into()),
            BlockLifecycle::Complete,
        )));
    }
    state.working = true;

    let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
    let mut cache = WrapCache::default();
    for (row_offset, header_expected) in [(0, true), (1, false)] {
        state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
            block_id: thinking_id.clone(),
            row_offset,
        });
        state.clock.frame = 0;
        terminal
            .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
            .expect("initial draw");
        let first = terminal.backend().buffer().clone();
        let body_counters = (
            cache.body_hits(),
            cache.body_misses(),
            cache.body_bypasses(),
            cache.body_oversized_skips(),
        );

        state.clock.frame = 6;
        terminal
            .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
            .expect("animated draw");
        let second = terminal.backend().buffer().clone();
        let changed = first
            .content
            .iter()
            .zip(&second.content)
            .enumerate()
            .filter_map(|(index, (before, after))| (before != after).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(
            body_counters,
            (
                cache.body_hits(),
                cache.body_misses(),
                cache.body_bypasses(),
                cache.body_oversized_skips(),
            ),
            "clock-only redraw must not re-render the Thinking body"
        );
        let header_row = (0..12).find(|row| {
            (0..80)
                .map(|column| first[(column, *row)].symbol())
                .collect::<String>()
                .contains("Pensando")
        });
        if header_expected {
            let header_row = header_row.expect("visible Thinking header");
            assert_eq!(changed, vec![header_row as usize * 80 + 2]);
        } else {
            assert!(
                header_row.is_none(),
                "row_offset=1 should leave the boundary-prefixed header offscreen"
            );
            assert_eq!(changed.len(), 1, "offscreen Thinking pulses the rail only");
        }
    }
}

#[test]
fn residual_thinking_header_keeps_real_activity_label_without_second_spinner() {
    use crate::block::{Block, BlockKind, BlockLifecycle};

    let mut state = AppState::new();
    state.authenticated = true;
    state.working = true;
    let mut thinking = Block::new(
        "streaming-thinking",
        BlockKind::Thinking("plan".into()),
        BlockLifecycle::Streaming,
    );
    thinking.set_turn_boundary_before(true);
    assert!(state.append_block(thinking));
    state.activity = Some(ActivityState {
        phase: ActivityPhase::RunningTool("read".into()),
        started_ms: 0,
    });
    state.clock.elapsed_ms = 2_000;

    let render = |state: &AppState| {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
            .expect("draw");
        terminal.backend().buffer().clone()
    };
    let first = render(&state);
    let rail_row = (0..24)
        .find(|row| {
            (0..80)
                .map(|column| first[(column, *row)].symbol())
                .collect::<String>()
                .contains("Lendo")
        })
        .expect("real activity rail label");
    let rail_before = (0..80)
        .map(|column| first[(column, rail_row)].clone())
        .collect::<Vec<_>>();
    let header_rows = (0..24)
        .filter(|row| {
            (0..80)
                .map(|column| first[(column, *row)].symbol())
                .collect::<String>()
                .contains("Pensando")
        })
        .count();
    assert_eq!(
        header_rows, 1,
        "the residual header must not duplicate in rail"
    );

    state.clock.frame = 6;
    let second = render(&state);
    let rail_after = (0..80)
        .map(|column| second[(column, rail_row)].clone())
        .collect::<Vec<_>>();
    assert_eq!(rail_before, rail_after, "rail glyph must stay static");
}

#[test]
fn activity_pulse_changes_one_cell_and_stops_with_reduced_motion() {
    use ratatui::backend::TestBackend;
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ToolStarted {
        batch_id: ToolBatchId("pulse".into()),
        call_id: ToolCallId("pulse".into()),
        name: "read".into(),
        arguments_summary: "path=src/main.rs".into(),
    });
    let render = |state: &AppState, capabilities| {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                super::render_frame(frame, state, capabilities, &mut WrapCache::default())
            })
            .unwrap();
        terminal.backend().buffer().clone()
    };
    let first = render(&state, caps());
    state.clock.frame = 6;
    let next = render(&state, caps());
    let changed = first
        .content
        .iter()
        .zip(&next.content)
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(changed, 1, "only the activity glyph may animate");
    let reduced = crate::theme::Capabilities {
        reduced_motion: true,
        ..caps()
    };
    let still = render(&state, reduced);
    state.clock.frame = 12;
    assert_eq!(still, render(&state, reduced));
    let mut motion_cache = WrapCache::default();
    assert!(!super::motion_needed(&state, reduced, &mut motion_cache));
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    assert!(!super::motion_needed(&state, caps(), &mut motion_cache));
}

#[test]
fn idle_down_becomes_scroll_without_an_overlay() {
    let state = AppState::new();
    let mut cache = WrapCache::default();
    let action = terminal_action(
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        )),
        &state,
        (80, 24),
        &mut cache,
    )
    .expect("action");
    assert!(matches!(
        action,
        Action::Scroll {
            intent: ScrollIntent::Down,
            ..
        }
    ));
    let _ = measure_scrollback(&state, 80, 24, &mut cache);
    let _ = FrameClock::default();
}

fn pinned_state_with_draft() -> AppState {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "something to read back".into(),
    });
    let anchor = crate::app::ScrollAnchor {
        block_id: state.blocks().last().expect("block").id.clone(),
        row_offset: 0,
    };
    state.scroll.mode = crate::app::FollowMode::Pinned(anchor);
    state.composer.insert_text("half-typed draft");
    state
}

fn key_action(state: &AppState, code: crossterm::event::KeyCode) -> Action {
    let mut cache = WrapCache::default();
    terminal_action(
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        )),
        state,
        (80, 24),
        &mut cache,
    )
    .expect("action")
}

#[test]
fn end_returns_to_live_edge_while_pinned_with_nonempty_draft() {
    let state = pinned_state_with_draft();
    assert!(state.scroll.is_pinned());
    let action = key_action(&state, crossterm::event::KeyCode::End);
    assert!(
        matches!(
            action,
            Action::Scroll {
                intent: ScrollIntent::LiveEdge,
                ..
            }
        ),
        "pinned End must honor the footer promise: {action:?}"
    );
}

#[test]
fn home_returns_to_top_while_pinned_with_nonempty_draft() {
    let state = pinned_state_with_draft();
    let action = key_action(&state, crossterm::event::KeyCode::Home);
    assert!(
        matches!(
            action,
            Action::Scroll {
                intent: ScrollIntent::Top,
                ..
            }
        ),
        "pinned Home must navigate: {action:?}"
    );
}

#[test]
fn end_moves_cursor_while_typing_at_live_edge() {
    let mut state = AppState::new();
    state.composer.insert_text("half-typed draft");
    assert!(state.scroll.is_live_edge());
    let action = key_action(&state, crossterm::event::KeyCode::End);
    assert!(
        matches!(action, Action::Key(_)),
        "live-edge End must keep editing the cursor: {action:?}"
    );
}

#[test]
fn control_lane_yields_after_its_batch_budget() {
    let (sender, receiver) = mpsc::channel();
    for index in 0..=CONTROL_BATCH_LIMIT {
        sender
            .send(UiEvent::Notification {
                message: index.to_string(),
            })
            .expect("enqueue control event");
    }

    let (batch, state) = receive_batch(&receiver, CONTROL_BATCH_LIMIT, &mut Vec::new());

    assert_eq!(batch.len(), CONTROL_BATCH_LIMIT);
    assert_eq!(state, LaneDrain::Exhausted);
    assert!(receiver.try_recv().is_ok(), "next event must remain queued");
}

#[test]
fn stream_lane_yields_after_its_batch_budget() {
    let (sender, receiver) = mpsc::channel();
    for index in 0..=STREAM_BATCH_LIMIT {
        sender
            .send(UiEvent::AssistantDelta {
                text: index.to_string(),
            })
            .expect("enqueue stream event");
    }

    let (batch, state) = receive_batch(&receiver, STREAM_BATCH_LIMIT, &mut Vec::new());

    assert_eq!(batch.len(), STREAM_BATCH_LIMIT);
    assert_eq!(state, LaneDrain::Exhausted);
    assert!(receiver.try_recv().is_ok(), "next event must remain queued");
}

#[test]
fn data_barrier_drains_prefetched_event_and_full_bounded_stream_lane() {
    let (sender, receiver) = mpsc::sync_channel(STREAM_EVENT_CAPACITY);
    let mut prefetched = vec![UiEvent::AssistantDelta {
        text: "prefetched-before-control".into(),
    }];
    for index in 0..STREAM_EVENT_CAPACITY {
        sender
            .send(UiEvent::AssistantDelta {
                text: format!("queued-{index}"),
            })
            .expect("fill bounded stream lane");
    }

    let (batch, state) = receive_batch(&receiver, STREAM_BATCH_LIMIT, &mut prefetched);

    assert_eq!(STREAM_BATCH_LIMIT, STREAM_EVENT_CAPACITY + 1);
    assert_eq!(batch.len(), STREAM_EVENT_CAPACITY + 1);
    assert_eq!(state, LaneDrain::Exhausted);
    assert!(matches!(
        batch.first(),
        Some(UiEvent::AssistantDelta { text }) if text == "prefetched-before-control"
    ));
    assert!(matches!(
        batch.last(),
        Some(UiEvent::AssistantDelta { text }) if text == &format!("queued-{}", STREAM_EVENT_CAPACITY - 1)
    ));
    assert!(
        receiver.try_recv().is_err(),
        "all earlier stream events precede control"
    );
}

#[test]
fn spinner_glyph_cycles_braille_and_honors_fallbacks() {
    let full = caps();
    assert_eq!(super::spinner_glyph(0, full), '⠋');
    assert_eq!(super::spinner_glyph(1, full), '⠙');
    assert_eq!(super::spinner_glyph(9, full), '⠏');
    assert_eq!(super::spinner_glyph(10, full), '⠋');

    let reduced = Capabilities {
        reduced_motion: true,
        ..full
    };
    assert_eq!(super::spinner_glyph(0, reduced), '\u{25cb}');
    assert_eq!(super::spinner_glyph(1, reduced), '\u{25cb}');

    let no_color = Capabilities {
        color_depth: ColorDepth::None,
        ..full
    };
    assert_eq!(super::spinner_glyph(0, no_color), '~');
}
