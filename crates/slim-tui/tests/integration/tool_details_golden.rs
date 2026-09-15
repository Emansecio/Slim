use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{
    ContentHandle, ContentRequestId, PageCursor, ToolBatchId, ToolCallId, UiCommand, UiEvent,
};
use slim_tui::app::{AppState, FollowMode, ScrollAnchor};
use slim_tui::block::{BlockKind, FoldState};
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn batch(value: &str) -> ToolBatchId {
    ToolBatchId(value.into())
}

fn call(value: &str) -> ToolCallId {
    ToolCallId(value.into())
}

fn handle(value: &str) -> ContentHandle {
    ContentHandle(value.into())
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn render_at(state: &AppState) -> String {
    render_at_width(state, 80)
}

fn render_at_width(state: &AppState, width: u16) -> String {
    let backend = TestBackend::new(width, 24);
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
    let mut output = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            output.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        output.push('\n');
    }
    output
}

#[test]
fn homonymous_calls_correlate_by_batch_and_call_id() {
    let mut state = AppState::new();
    for (call_id, arguments) in [("call-a", "path=a.txt"), ("call-b", "path=b.txt")] {
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch("batch-1"),
            call_id: call(call_id),
            name: "read".into(),
            arguments_summary: arguments.into(),
        });
    }

    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "read".into(),
        preview: "alpha output".into(),
        content_handle: Some(handle("content-a")),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-b"),
        name: "read".into(),
        preview: "beta output".into(),
        content_handle: Some(handle("content-b")),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-b"),
        name: "read".into(),
        success: true,
        duration_ms: 22,
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "read".into(),
        success: false,
        duration_ms: 11,
    });

    let tools = state
        .blocks()
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::Tool(tool) => Some((block.lifecycle, tool)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].1.call_id, call("call-a"));
    assert_eq!(tools[0].1.arguments_summary, "path=a.txt");
    assert_eq!(tools[0].1.preview, "alpha output");
    assert_eq!(tools[0].1.duration_ms, Some(11));
    assert_eq!(tools[1].1.call_id, call("call-b"));
    assert_eq!(tools[1].1.arguments_summary, "path=b.txt");
    assert_eq!(tools[1].1.preview, "beta output");
    assert_eq!(tools[1].1.duration_ms, Some(22));
}

#[test]
fn enter_pages_inline_and_rejects_stale_duplicate_or_out_of_order_pages() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "read".into(),
        arguments_summary: "path=safe.txt".into(),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "read".into(),
        preview: "first line".into(),
        content_handle: Some(handle("content-a")),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "read".into(),
        success: true,
        duration_ms: 7,
    });
    let block_id = state.blocks()[0].id.clone();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id,
        row_offset: 0,
    });

    let first = reduce(&mut state, Action::Key(enter()));
    assert_eq!(
        first,
        vec![
            Effect::Send(UiCommand::RequestContentPage {
                handle: handle("content-a"),
                request_id: ContentRequestId(1),
                cursor: None,
            }),
            Effect::RequestRender,
        ]
    );
    assert_eq!(state.blocks()[0].fold, FoldState::Expanded);

    for stale in [
        UiEvent::ContentPageLoaded {
            handle: handle("wrong"),
            request_id: ContentRequestId(1),
            cursor: None,
            text: "wrong handle".into(),
            next_cursor: Some(PageCursor(16)),
        },
        UiEvent::ContentPageLoaded {
            handle: handle("content-a"),
            request_id: ContentRequestId(9),
            cursor: None,
            text: "wrong request".into(),
            next_cursor: Some(PageCursor(16)),
        },
        UiEvent::ContentPageLoaded {
            handle: handle("content-a"),
            request_id: ContentRequestId(1),
            cursor: Some(PageCursor(16)),
            text: "wrong cursor".into(),
            next_cursor: Some(PageCursor(32)),
        },
    ] {
        state.apply_event(stale);
    }
    assert!(!render_at(&state).contains("wrong"));

    let first_page = UiEvent::ContentPageLoaded {
        handle: handle("content-a"),
        request_id: ContentRequestId(1),
        cursor: None,
        text: "page one".into(),
        next_cursor: Some(PageCursor(8)),
    };
    state.apply_event(first_page.clone());
    state.apply_event(first_page);
    let frame = render_at(&state);
    assert_eq!(frame.matches("page one").count(), 1);

    let second = reduce(&mut state, Action::Key(enter()));
    assert_eq!(
        second,
        vec![
            Effect::Send(UiCommand::RequestContentPage {
                handle: handle("content-a"),
                request_id: ContentRequestId(2),
                cursor: Some(PageCursor(8)),
            }),
            Effect::RequestRender,
        ]
    );
    state.apply_event(UiEvent::ContentPageLoaded {
        handle: handle("content-a"),
        request_id: ContentRequestId(2),
        cursor: Some(PageCursor(8)),
        text: "page two".into(),
        next_cursor: None,
    });
    let frame = render_at(&state);
    assert!(frame.contains("page one"));
    assert!(frame.contains("page two"));
    assert!(!frame.contains('┌') && !frame.contains('┐'));
}

#[test]
fn arguments_and_output_are_sanitized_in_the_frame() {
    let secret = "super-secret-token";
    let projected = UiEvent::from_core(slim_core::SessionEvent::new(
        1,
        slim_core::EventKind::ToolStarted {
            batch_id: "batch-1".into(),
            call_id: "call-a".into(),
            name: "read".into(),
            arguments: format!(
                "{{\"token\":\"[REDACTED]\",\"note\":\"{}]8;;bad\"}}",
                '\u{1b}'
            ),
        },
    ))
    .expect("projected start");
    let mut state = AppState::new();
    state.apply_event(projected);
    let tool_id = state.blocks()[0].id.clone();
    let (changed, _) = state.activate_block(&tool_id);
    assert!(changed);
    let frame = render_at(&state);
    assert!(!frame.contains(secret));
    assert!(!frame.contains('\u{1b}'));
    assert!(frame.contains("[REDACTED]"));
}

#[test]
fn cancelled_tool_retains_correlated_output_without_reviving_lifecycle() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        arguments_summary: "command=safe".into(),
    });
    state.apply_event(UiEvent::RunCancelled { run_id: 1 });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        preview: "cancelled output".into(),
        content_handle: Some(handle("content-a")),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        success: false,
        duration_ms: 9,
    });

    let block = &state.blocks()[0];
    let BlockKind::Tool(tool) = block.kind() else {
        panic!("tool block");
    };
    assert_eq!(block.lifecycle, slim_tui::block::BlockLifecycle::Cancelled);
    assert_eq!(tool.preview, "cancelled output");
    assert_eq!(tool.duration_ms, Some(9));
    assert_eq!(tool.content_handle, Some(handle("content-a")));
}

#[test]
fn duplicate_tool_terminal_keeps_first_duration_and_requests_resync() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        arguments_summary: String::new(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        success: true,
        duration_ms: 7,
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        success: false,
        duration_ms: 99,
    });

    let BlockKind::Tool(tool) = state.blocks()[0].kind() else {
        panic!("tool block");
    };
    assert_eq!(tool.duration_ms, Some(7));
    assert!(state.snapshot_resync_needed());
}

#[test]
fn orphan_tool_suffix_requests_snapshot_resync() {
    for event in [
        UiEvent::ToolProgress {
            batch_id: batch("batch-1"),
            call_id: call("missing"),
            name: "shell".into(),
            preview: "orphan".into(),
            content_handle: None,
        },
        UiEvent::ToolEnded {
            batch_id: batch("batch-1"),
            call_id: call("missing"),
            name: "shell".into(),
            success: false,
            duration_ms: 1,
        },
    ] {
        let mut state = AppState::new();
        state.apply_event(event);
        assert!(state.blocks().is_empty());
        assert!(state.snapshot_resync_needed());
        assert!(state
            .notifications
            .iter()
            .any(|message| message.contains("tool lifecycle")));
    }
}

#[test]
fn collapsed_single_tool_omits_arguments_and_preview() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        arguments_summary: "command=Get-ChildItem".into(),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        preview: "exit 0\nstdout:\nok".into(),
        content_handle: None,
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        success: true,
        duration_ms: 854,
    });

    let collapsed = render_at(&state);
    assert!(collapsed.contains("✓ shell · 854ms"), "{collapsed}");
    assert!(!collapsed.contains("command="), "{collapsed}");
    assert!(!collapsed.contains("exit 0"), "{collapsed}");

    let tool_id = state.blocks()[0].id.clone();
    let (changed, _) = state.activate_block(&tool_id);
    assert!(changed);
    assert_eq!(state.blocks()[0].fold, FoldState::Expanded);
    let expanded = render_at(&state);
    assert!(expanded.contains("command=Get-ChildItem"), "{expanded}");
}

#[test]
fn running_shell_shows_command_limit_and_live_progress() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        arguments_summary: "command=cargo test · limit 120s".into(),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        preview: "Compiling slim-core · out 128 B · err 0 B".into(),
        content_handle: None,
    });

    let frame = render_at(&state);
    assert!(frame.contains("command="), "{frame}");
    assert!(frame.contains("limit 120s"), "{frame}");
    assert!(frame.contains("Compiling slim-core"), "{frame}");
}

#[test]
fn running_tool_summary_stays_on_one_row_and_preserves_progress_metadata() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        arguments_summary: "command=very-long-command-that-cannot-fit-in-the-terminal · limit 120s"
            .into(),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        preview: "Compiling slim-core · out 128 B · err 0 B".into(),
        content_handle: None,
    });

    let frame = render_at_width(&state, 64);
    let transcript_rows = slim_tui::layout::plan(64, 24, 0, true).scrollback.height;
    let matching = frame
        .lines()
        .take(transcript_rows as usize)
        .filter(|line| {
            ["shell", "limit 120s", "out 128 B", "err 0 B"]
                .iter()
                .any(|needle| line.contains(needle))
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "tool summary wrapped:\n{frame}");
    let summary = matching[0];
    assert!(summary.contains("shell"), "{summary}");
    assert!(summary.contains("limit 120s"), "{summary}");
    assert!(summary.contains("out 128 B"), "{summary}");
    assert!(summary.contains("err 0 B"), "{summary}");
    assert!(
        !summary.contains("very-long-command-that-cannot-fit-in-the-terminal"),
        "{summary}"
    );
}

#[test]
fn failed_tool_summary_includes_the_short_reason_and_sub_millisecond_duration() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "todo".into(),
        arguments_summary: "operation=write".into(),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "todo".into(),
        preview: "permission denied by workspace policy\nfull output hidden".into(),
        content_handle: None,
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "todo".into(),
        success: false,
        duration_ms: 0,
    });

    let frame = render_at(&state);
    assert!(
        frame.contains("permission denied by workspace policy"),
        "{frame}"
    );
    assert!(frame.contains("<1ms"), "{frame}");
    assert!(!frame.contains("operation=write"), "{frame}");
    assert!(!frame.contains("full output hidden"), "{frame}");
}

#[test]
fn collapsed_failed_tool_drops_telemetry_and_keeps_the_short_reason() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        arguments_summary: "command=git log".into(),
    });
    state.apply_event(UiEvent::ToolProgress {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        preview: "exit 1 · lastcommit=2026-09-03 04:57:42 -0300 15f4e9c · out 128 B\nfull log"
            .into(),
        content_handle: None,
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-1"),
        call_id: call("call-a"),
        name: "shell".into(),
        success: false,
        duration_ms: 733,
    });

    let frame = render_at(&state);
    let summary = frame
        .lines()
        .find(|line| line.contains("shell"))
        .expect("failed shell row");
    assert!(summary.contains("exit 1"), "{summary}");
    assert!(summary.contains("733ms"), "{summary}");
    assert!(
        !summary.contains("lastcommit="),
        "telemetry must not steal the row\n{summary}"
    );
    assert!(!summary.contains("out 128 B"), "{summary}");
    assert!(!summary.contains("command="), "{summary}");
}
