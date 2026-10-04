use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{
    ContentHandle, ContentRequestId, PageCursor, ToolBatchId, ToolCallId, UiCommand, UiEvent,
};
use slim_tui::app::{AppState, FollowMode, ScrollAnchor};
use slim_tui::block::{BlockKind, BlockLifecycle, FoldState};
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
fn shell_job_final_output_replaces_loaded_running_page_before_terminal() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch"),
        call_id: call("launch"),
        name: "shell".into(),
        arguments_summary: String::new(),
    });
    let progress = |preview: &str| UiEvent::ToolProgress {
        batch_id: batch("batch"),
        call_id: call("launch"),
        name: "shell".into(),
        preview: preview.into(),
        content_handle: Some(handle("shell-output")),
    };
    state.apply_event(progress("state=running"));
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: state.blocks()[0].id.clone(),
        row_offset: 0,
    });
    assert!(reduce(&mut state, Action::Key(enter()))
        .iter()
        .any(|effect| matches!(
            effect,
            Effect::Send(UiCommand::RequestContentPage { cursor: None, .. })
        )));
    state.apply_event(UiEvent::ContentPageLoaded {
        handle: handle("shell-output"),
        request_id: ContentRequestId(1),
        cursor: None,
        text: "state=running".into(),
        next_cursor: None,
    });
    assert!(render_at(&state).contains("state=running"));
    state.apply_event(progress("exit 1 · final marker"));
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Streaming);
    let BlockKind::Tool(tool) = state.blocks()[0].kind() else {
        panic!("tool block")
    };
    assert!(tool.materialized_output.is_empty());
    assert_eq!(tool.next_cursor, None);
    assert!(reduce(&mut state, Action::Key(enter()))
        .iter()
        .any(|effect| matches!(
            effect,
            Effect::Send(UiCommand::RequestContentPage { cursor: None, .. })
        )));
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch"),
        call_id: call("launch"),
        name: "shell".into(),
        success: false,
        duration_ms: 42,
    });
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Failed);
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
            .any(|message| message.contains("ciclo de vida da ferramenta")));
    }
}

#[test]
fn collapsed_single_tool_keeps_compact_target_and_result() {
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
    assert!(collapsed.contains("✓ $ Get-ChildItem"), "{collapsed}");
    assert!(!collapsed.contains("Executou"), "{collapsed}");
    assert!(collapsed.contains("854ms"), "{collapsed}");
    assert!(
        collapsed.contains("$ Get-ChildItem"),
        "collapsed row keeps the target summary\n{collapsed}"
    );
    assert!(
        !collapsed.contains("exit 0"),
        "a clean exit is already stated by the success glyph\n{collapsed}"
    );

    let tool_id = state.blocks()[0].id.clone();
    let (changed, _) = state.activate_block(&tool_id);
    assert!(changed);
    assert_eq!(state.blocks()[0].fold, FoldState::Expanded);
    let expanded = render_at(&state);
    assert!(expanded.contains("$ Get-ChildItem"), "{expanded}");
    assert!(
        expanded.contains("exit 0"),
        "details keep the full result\n{expanded}"
    );
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
    assert!(frame.contains(" $ cargo test"), "{frame}");
    assert!(!frame.contains("Executando $"), "{frame}");
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
    let transcript_rows = slim_tui::layout::plan(64, 24, 0).scrollback.height;
    let matching = frame
        .lines()
        .take(transcript_rows as usize)
        .filter(|line| {
            ["○ $", "limit 120s", "out 128 B", "err 0 B"]
                .iter()
                .any(|needle| line.contains(needle))
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "tool summary wrapped:\n{frame}");
    let summary = matching[0];
    assert!(summary.contains("○ $"), "{summary}");
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
        .find(|line| line.contains("✕ $"))
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

fn project(state: &mut AppState, kind: slim_core::EventKind) {
    let event = UiEvent::from_core(slim_core::SessionEvent::new(1, kind)).expect("projected");
    state.apply_event(event);
}

#[test]
fn projected_patch_and_shell_rows_read_as_verb_target_and_outcome() {
    use slim_core::EventKind as Kind;
    let mut state = AppState::new();
    let arguments = r#"{"path":"src/lib.rs","edits":[{"expected":"a\nold\nz","replacement":"a\nnew\nmore\nz"}]}"#;
    project(
        &mut state,
        Kind::ToolStarted {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            name: "patch".into(),
            arguments: arguments.into(),
        },
    );
    project(
        &mut state,
        Kind::ToolOutput {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            name: "patch".into(),
            output: "patched src/lib.rs:2; replaced 3 bytes with 8 bytes; bytes=9; sha256=ab; do not re-read".into(),
        },
    );
    project(
        &mut state,
        Kind::ToolFinished {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            name: "patch".into(),
            success: true,
            duration_ms: 8,
        },
    );
    project(
        &mut state,
        Kind::ToolStarted {
            batch_id: "b2".into(),
            call_id: "c2".into(),
            name: "shell".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        },
    );
    project(
        &mut state,
        Kind::ToolOutput {
            batch_id: "b2".into(),
            call_id: "c2".into(),
            name: "shell".into(),
            output: "exit 101\nstderr:\nerror: test failed".into(),
        },
    );
    project(
        &mut state,
        Kind::ToolFinished {
            batch_id: "b2".into(),
            call_id: "c2".into(),
            name: "shell".into(),
            success: false,
            duration_ms: 4_200,
        },
    );
    state.clock.elapsed_ms = 10_000;

    // Collapsed, the edit and the failed command are one group; the failure
    // keeps a row of its own under the header.
    let collapsed = render_at_width(&state, 100);
    let header = collapsed
        .lines()
        .find(|line| line.contains("1 edição, 1 comando"))
        .unwrap_or_else(|| panic!("group header\n{collapsed}"));
    assert!(header.contains("✓ "), "{header}");
    let failure = collapsed
        .lines()
        .find(|line| line.contains("✕ $"))
        .unwrap_or_else(|| panic!("failure row\n{collapsed}"));
    assert!(
        failure.starts_with("    ✕ $ cargo test · exit 101"),
        "the failure sits one step in from the header\n{collapsed}"
    );

    let leader = state.blocks()[0].id.clone();
    assert!(state.activate_block(&leader).0);
    let frame = render_at_width(&state, 100);
    let patch = frame
        .lines()
        .find(|line| line.contains("Editou"))
        .unwrap_or_else(|| panic!("patch row\n{frame}"));
    assert!(
        patch.contains("✓ Editou src/lib.rs · +2 -1 · 8ms"),
        "{patch}"
    );
    assert!(
        !patch.contains("patched"),
        "the receipt stays behind details\n{patch}"
    );
    assert!(failure.contains("4.2s"), "{failure}");
    // Expanded, the member row carries the full arguments and outcome.
    let shell = frame
        .lines()
        .find(|line| line.contains("✕ $"))
        .unwrap_or_else(|| panic!("shell row\n{frame}"));
    assert!(
        shell.contains("✕ $ cargo test") && shell.contains("exit 101"),
        "{shell}"
    );
    assert!(shell.contains("4.2s"), "{shell}");
}

#[test]
fn expanded_patch_shows_its_applied_diff_before_the_receipt() {
    use slim_core::EventKind as Kind;
    let mut state = AppState::new();
    for kind in [
        Kind::ToolStarted {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            name: "patch".into(),
            arguments: r#"{"path":"src/lib.rs","edits":[{"expected":"old","replacement":"new\nmore"}]}"#.into(),
        },
        Kind::ToolOutput {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            name: "patch".into(),
            output: "patched src/lib.rs:2; replaced 3 bytes with 8 bytes; bytes=9; sha256=ab; do not re-read".into(),
        },
        Kind::ToolEditApplied {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            name: "patch".into(),
            diff: slim_core::ToolEditDiff {
                path: "src/lib.rs".into(),
                hunks: vec![slim_core::ToolEditHunk {
                    start_line: 2,
                    removed: vec!["old".into()],
                    added: vec!["new".into(), "more".into()],
                }],
                truncated: false,
            },
        },
        Kind::ToolFinished {
            batch_id: "b1".into(),
            call_id: "c1".into(),
            name: "patch".into(),
            success: true,
            duration_ms: 8,
        },
    ] {
        project(&mut state, kind);
    }
    let collapsed = render_at_width(&state, 100);
    assert!(!collapsed.contains("@@ src/lib.rs:2"), "{collapsed}");

    let block_id = state.blocks()[0].id.clone();
    // No content handle: expanding needs no page request to show the diff.
    assert_eq!(
        reduce(&mut state, Action::ToggleBlock(block_id)),
        vec![Effect::RequestRender]
    );
    assert_eq!(state.blocks()[0].fold, FoldState::Expanded);

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut cache = WrapCache::default();
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                &state,
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
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let row_of = |needle: &str| {
        rows.iter()
            .position(|row| row.trim_end() == format!("    {needle}"))
            .unwrap_or_else(|| panic!("missing {needle:?}\n{}", rows.join("\n")))
    };
    let header = row_of("@@ src/lib.rs:2");
    let removed = row_of("- old");
    let added = row_of("+ new");
    assert_eq!(
        (removed, added, row_of("+ more")),
        (header + 1, header + 2, header + 3)
    );
    let color = |row: usize| buffer[(4, row as u16)].fg;
    assert_ne!(color(removed), color(added));
    assert_ne!(color(header), color(added));
    assert_ne!(color(header), color(removed));
    let background = |row: usize, x| buffer[(x, row as u16)].bg;
    assert_ne!(background(removed, 4), background(added, 4));
    assert_ne!(background(header, 4), background(added, 4));
    // The tint fills the code viewport, leaving its indentation neutral.
    assert_eq!(background(added, 4), background(added, 90));
    assert_ne!(background(added, 3), background(added, 4));
}
