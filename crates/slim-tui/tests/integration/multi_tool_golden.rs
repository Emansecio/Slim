use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use slim_tui::api::{
    ContentHandle, ContentRequestId, PageCursor, ToolBatchId, ToolCallId, UiCommand, UiEvent,
};
use slim_tui::app::{AppState, FollowMode, ScrollAnchor};
use slim_tui::block::{Block, BlockKind, BlockLifecycle, FoldState, ToolState};
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::render::{HeightIndex, WrapCache};
use slim_tui::runtime::terminal_action;
use slim_tui::testkit::render_terminal_text;

fn batch(value: &str) -> ToolBatchId {
    ToolBatchId(value.into())
}

fn call(value: &str) -> ToolCallId {
    ToolCallId(value.into())
}

fn complete(
    state: &mut AppState,
    batch_id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
    duration_ms: u64,
) {
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch(batch_id),
        call_id: call(call_id),
        name: name.into(),
        arguments_summary: arguments.into(),
    });
    // Grouped headers report lifecycle wall time when timestamps exist.
    state.clock.elapsed_ms = state.clock.elapsed_ms.saturating_add(duration_ms);
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch(batch_id),
        call_id: call(call_id),
        name: name.into(),
        success: true,
        duration_ms,
    });
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn settle(state: &mut AppState) {
    state.clock.elapsed_ms = state.clock.elapsed_ms.saturating_add(249);
}

#[test]
fn opening_group_keeps_materialized_member_details_closed() {
    for lifecycle in [BlockLifecycle::Complete, BlockLifecycle::Failed] {
        let mut state = AppState::new();
        for (id, output) in [
            ("first", "FIRST OUTPUT BODY"),
            ("second", "SECOND OUTPUT BODY"),
        ] {
            state.append_block(Block::new(
                id,
                BlockKind::Tool(ToolState {
                    name: "read".into(),
                    call_id: call(id),
                    materialized_output: output.into(),
                    ..ToolState::default()
                }),
                lifecycle,
            ));
        }
        let leader = state.blocks()[0].id.clone();
        reduce(&mut state, Action::ToggleBlock(leader));
        let expanded = render_terminal_text(&state, 80, 24);
        assert!(!expanded.contains("OUTPUT BODY"), "{expanded}");
        let mut cache = WrapCache::default();
        let index = HeightIndex::build(state.blocks(), 80, &mut cache);
        assert_eq!(index.total_rows, 3);
    }
}

#[test]
fn fitted_navigation_uses_the_rows_presented_during_the_tool_hold() {
    let mut state = AppState::new();
    state.clock.elapsed_ms = 500;
    for (id, ended_ms) in [("first", 0), ("second", 500)] {
        let mut block = Block::new(
            id,
            BlockKind::Tool(ToolState {
                name: "read".into(),
                content_handle: Some(ContentHandle(id.into())),
                ..ToolState::default()
            }),
            BlockLifecycle::Complete,
        );
        block.ended_ms = Some(ended_ms);
        state.append_block(block);
    }
    let ids: Vec<_> = state
        .blocks()
        .iter()
        .map(|block| block.id.clone())
        .collect();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: ids[0].clone(),
        row_offset: 0,
    });
    let mut cache = WrapCache::default();
    let down = terminal_action(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &state,
        (80, 24),
        &mut cache,
    )
    .unwrap();
    reduce(&mut state, down);
    assert_eq!(state.selected_block_id(), Some(&ids[1]));
    settle(&mut state);
    let up = terminal_action(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &state,
        (80, 24),
        &mut cache,
    )
    .unwrap();
    reduce(&mut state, up);
    assert_eq!(
        state.scroll.mode,
        FollowMode::Pinned(ScrollAnchor {
            block_id: ids[0].clone(),
            row_offset: 0
        })
    );
}

#[test]
fn live_enter_hint_targets_the_queued_texts_after_a_tool_group() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    complete(&mut state, "batch", "first", "read", "path=one", 1);
    complete(&mut state, "batch", "second", "read", "path=two", 1);
    settle(&mut state);
    state.apply_event(UiEvent::QueuedUserAdded {
        text: "queued preview\nQUEUED TEXT TAIL".into(),
        position: 1,
    });
    let collapsed = render_terminal_text(&state, 80, 24);
    assert!(collapsed.contains("Enter textos"), "{collapsed}");
    assert!(!collapsed.contains("Enter detalhes"), "{collapsed}");
    let mut cache = WrapCache::default();
    let action = terminal_action(Event::Key(enter()), &state, (80, 24), &mut cache).unwrap();
    reduce(&mut state, action);
    assert!(render_terminal_text(&state, 80, 24).contains("QUEUED TEXT TAIL"));
    assert_eq!(state.queue_len(), 1);
}

#[test]
fn grouped_members_navigate_and_page_individually_with_stable_anchors() {
    let mut state = AppState::new();
    for id in ["first", "second"] {
        state.append_block(Block::new(
            id,
            BlockKind::Tool(ToolState {
                name: "read".into(),
                call_id: call(id),
                content_handle: Some(ContentHandle(id.into())),
                ..ToolState::default()
            }),
            BlockLifecycle::Complete,
        ));
    }
    let leader = state.blocks()[0].id.clone();
    let second = state.blocks()[1].id.clone();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: leader.clone(),
        row_offset: 0,
    });
    reduce(&mut state, Action::Key(enter()));
    let mut cache = WrapCache::default();
    let down = terminal_action(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &state,
        (80, 24),
        &mut cache,
    )
    .unwrap();
    reduce(&mut state, down);
    assert_eq!(
        state.scroll.mode,
        FollowMode::Pinned(ScrollAnchor {
            block_id: leader.clone(),
            row_offset: 1
        })
    );
    assert_eq!(
        reduce(&mut state, Action::Key(enter())),
        vec![
            Effect::Send(UiCommand::RequestContentPage {
                handle: ContentHandle("first".into()),
                request_id: ContentRequestId(1),
                cursor: None
            }),
            Effect::RequestRender,
        ]
    );
    assert!(state.blocks()[0].group_expanded);
    state.apply_event(UiEvent::ContentPageLoaded {
        handle: ContentHandle("first".into()),
        request_id: ContentRequestId(1),
        cursor: None,
        text: "page one\n".into(),
        next_cursor: Some(PageCursor(9)),
    });
    assert_eq!(
        reduce(&mut state, Action::Key(enter())),
        vec![
            Effect::Send(UiCommand::RequestContentPage {
                handle: ContentHandle("first".into()),
                request_id: ContentRequestId(2),
                cursor: Some(PageCursor(9))
            }),
            Effect::RequestRender,
        ]
    );
    state.apply_event(UiEvent::ContentPageLoaded {
        handle: ContentHandle("first".into()),
        request_id: ContentRequestId(2),
        cursor: Some(PageCursor(9)),
        text: "page two".into(),
        next_cursor: None,
    });
    let down = terminal_action(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &state,
        (80, 24),
        &mut cache,
    )
    .unwrap();
    reduce(&mut state, down);
    assert_eq!(state.selected_block_id(), Some(&second));
    assert_eq!(
        reduce(&mut state, Action::Key(enter())),
        vec![
            Effect::Send(UiCommand::RequestContentPage {
                handle: ContentHandle("second".into()),
                request_id: ContentRequestId(3),
                cursor: None
            }),
            Effect::RequestRender,
        ]
    );
    state.apply_event(UiEvent::ContentPageLoaded {
        handle: ContentHandle("second".into()),
        request_id: ContentRequestId(3),
        cursor: None,
        text: "other result".into(),
        next_cursor: None,
    });
    let index = HeightIndex::build(state.blocks(), 80, &mut cache);
    assert_eq!(index.total_rows, 6);
    for row in 0..index.total_rows {
        assert_eq!(
            index.row_for_anchor(&index.anchor_for_row(row).unwrap()),
            Some(row)
        );
    }
    for expected in ["page one", "page two", "other result"] {
        assert!(render_terminal_text(&state, 80, 24).contains(expected));
    }
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: leader,
        row_offset: 0,
    });
    reduce(&mut state, Action::Key(enter()));
    assert_eq!(
        HeightIndex::build(state.blocks(), 80, &mut cache).total_rows,
        1
    );
    reduce(&mut state, Action::Key(enter()));
    assert_eq!(
        HeightIndex::build(state.blocks(), 80, &mut cache).total_rows,
        6
    );
}

#[test]
fn grouped_diffs_and_bridged_thinking_match_rendered_heights() {
    let mut state = AppState::new();
    state.append_block(Block::new(
        "first",
        BlockKind::Tool(ToolState {
            name: "read".into(),
            ..ToolState::default()
        }),
        BlockLifecycle::Complete,
    ));
    state.append_block(Block::new(
        "thought",
        BlockKind::Thinking("REASONING BODY\nsecond thought row".into()),
        BlockLifecycle::Complete,
    ));
    state.append_block(Block::new(
        "patch",
        BlockKind::Tool(ToolState {
            name: "patch".into(),
            edit_diff: Some(slim_core::ToolEditDiff {
                path: "src/lib.rs".into(),
                hunks: vec![slim_core::ToolEditHunk {
                    start_line: 1,
                    removed: vec!["old".into()],
                    added: vec!["new".into()],
                }],
                truncated: false,
            }),
            ..ToolState::default()
        }),
        BlockLifecycle::Complete,
    ));
    let ids: Vec<_> = state
        .blocks()
        .iter()
        .map(|block| block.id.clone())
        .collect();
    reduce(&mut state, Action::ToggleBlock(ids[0].clone()));
    assert!(!render_terminal_text(&state, 80, 24).contains("REASONING BODY"));
    for id in &ids[1..] {
        state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
            block_id: id.clone(),
            row_offset: 0,
        });
        reduce(&mut state, Action::Key(enter()));
    }
    assert!(state.blocks()[0].group_expanded);
    assert_eq!(state.blocks()[1].fold, FoldState::Expanded);
    assert_eq!(state.blocks()[2].fold, FoldState::Expanded);
    let mut cache = WrapCache::default();
    for width in [40, 80, 120] {
        let index = HeightIndex::build(state.blocks(), width, &mut cache);
        assert_eq!(index.len(), 1);
        assert_eq!(index.total_rows, 9);
        for row in 0..index.total_rows {
            assert_eq!(
                index.row_for_anchor(&index.anchor_for_row(row).unwrap()),
                Some(row)
            );
        }
        let text = render_terminal_text(&state, width, 30);
        for expected in [
            "REASONING BODY",
            "second thought row",
            "@@ src/lib.rs:1",
            "- old",
            "+ new",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
    }
}

#[test]
fn same_batch_groups_different_names_and_expands_in_provider_order() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    complete(&mut state, "batch-a", "call-1", "read", "path=one", 7);
    complete(&mut state, "batch-a", "call-2", "shell", "cmd=two", 5);
    settle(&mut state);

    let collapsed = render_terminal_text(&state, 120, 30);
    assert!(
        collapsed.contains("✓ 1 leitura, 1 comando · 12ms"),
        "{collapsed}"
    );
    assert!(
        collapsed.contains("Enter detalhes"),
        "collapsed group must hint expansion when it fits\n{collapsed}"
    );
    assert!(!collapsed.contains("call-1"));
    assert!(!collapsed.contains("call-2"));

    let leader = state.blocks()[0].id.clone();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: leader,
        row_offset: 0,
    });
    assert_eq!(
        reduce(&mut state, Action::Key(enter())),
        vec![Effect::RequestRender]
    );
    assert!(state.blocks()[0].group_expanded);
    let expanded = render_terminal_text(&state, 120, 30);
    assert!(
        expanded.contains("✓ 1 leitura, 1 comando · 12ms"),
        "{expanded}"
    );
    assert!(
        !expanded.contains("Enter detalhes"),
        "expanded header must not keep the collapse-competing hint\n{expanded}"
    );
    let first = expanded.find("Leu one").expect("first member");
    let second = expanded.find("Executou cmd=two").expect("second member");
    assert!(first < second, "provider order changed\n{expanded}");
    // Call ids are internal identity, not something the reader acts on.
    assert!(!expanded.contains("call-1"), "{expanded}");
    assert!(!expanded.contains("call-2"), "{expanded}");
    let first_row = expanded
        .lines()
        .find(|line| line.contains("Leu one"))
        .expect("first member row");
    assert!(
        !first_row.contains("cmd=two"),
        "member outputs must stay on separate rows\n{expanded}"
    );
    let mut cache = WrapCache::default();
    let index = HeightIndex::build(state.blocks(), 120, &mut cache);
    assert_eq!(index.total_rows, 3);
    let member_anchor = index.anchor_for_row(2).expect("member row anchor");
    assert_eq!(index.row_for_anchor(&member_anchor), Some(2));

    reduce(&mut state, Action::Key(enter()));
    assert!(!state.blocks()[0].group_expanded);
    let collapsed_again = render_terminal_text(&state, 120, 30);
    assert!(!collapsed_again.contains("call-1"));
}

#[test]
fn same_batch_groups_equal_names_by_batch_identity() {
    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "read", "one", 3);
    complete(&mut state, "batch-a", "call-2", "read", "two", 4);
    settle(&mut state);

    let frame = render_terminal_text(&state, 80, 24);
    assert!(frame.contains("✓ 2 leituras · 7ms"), "{frame}");
    assert!(!frame.contains("✓ read"), "{frame}");
}

#[test]
fn grouped_header_summarizes_names_counts_and_total_duration() {
    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "read", "one", 7);
    complete(&mut state, "batch-a", "call-2", "read", "two", 8);
    complete(&mut state, "batch-a", "call-3", "read", "three", 9);
    complete(&mut state, "batch-a", "call-4", "shell", "four", 18);
    settle(&mut state);

    let wide = render_terminal_text(&state, 120, 30);
    let expected = format!("3 leituras, 1 comando {} 42ms", '\u{00b7}');
    assert!(wide.contains(&expected), "{wide}");

    let narrow = render_terminal_text(&state, 24, 12);
    let compact = format!("4 ferramentas {} 42ms", '\u{00b7}');
    assert!(narrow.contains(&compact), "{narrow}");
    assert!(
        !narrow.contains("read"),
        "narrow header did not compact\n{narrow}"
    );
}

#[test]
fn grouped_header_omits_duration_when_any_member_has_no_duration() {
    let mut state = AppState::new();
    for (id, name, duration_ms) in [("one", "read", Some(3)), ("two", "shell", None)] {
        assert!(state.append_block(Block::new(
            id,
            BlockKind::Tool(ToolState {
                batch_id: batch("batch-a"),
                call_id: call(id),
                name: name.into(),
                duration_ms,
                ..ToolState::default()
            }),
            BlockLifecycle::Complete,
        )));
    }

    let frame = render_terminal_text(&state, 80, 24);
    assert!(frame.contains("1 leitura, 1 comando"), "{frame}");
    assert!(
        !frame.contains("3ms"),
        "partial duration must be omitted\n{frame}"
    );
}

#[test]
fn plain_enter_at_live_edge_activates_last_visible_tool_group() {
    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "read", "one", 3);
    complete(&mut state, "batch-a", "call-2", "shell", "two", 4);
    settle(&mut state);
    let leader = state.blocks()[0].id.clone();
    let mut cache = WrapCache::default();

    let action = terminal_action(Event::Key(enter()), &state, (80, 24), &mut cache);
    assert!(
        matches!(&action, Some(Action::ToggleBlock(id)) if id == &leader),
        "plain Enter must target the visible group, got {action:?}"
    );
    reduce(&mut state, action.expect("toggle action"));
    assert!(state.blocks()[0].group_expanded);
}

#[test]
fn collapsed_thinking_does_not_split_complete_tool_groups() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    complete(&mut state, "batch-a", "call-1", "read", "one", 4);
    complete(&mut state, "batch-a", "call-2", "read", "two", 1);
    let mut thinking = Block::new(
        "thought-mid",
        BlockKind::Thinking("internal scratch".into()),
        BlockLifecycle::Complete,
    );
    thinking.fold = FoldState::Collapsed;
    assert!(state.append_block(thinking));
    complete(&mut state, "batch-b", "call-3", "read", "three", 1);
    complete(&mut state, "batch-b", "call-4", "read", "four", 1);
    settle(&mut state);

    let frame = render_terminal_text(&state, 80, 24);
    assert!(
        frame.contains("✓ 4 leituras · 7ms"),
        "collapsed thought must not split adjacent tool groups\n{frame}"
    );
    assert_eq!(
        frame.matches("Pensou").count(),
        0,
        "sandwiched collapsed thought is chrome, not a row\n{frame}"
    );
    assert_eq!(
        frame.matches("✓ 2 tools").count(),
        0,
        "must merge both batches\n{frame}"
    );
}

#[test]
fn consecutive_complete_tools_group_across_batches() {
    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "shell", "one", 1489);
    complete(&mut state, "batch-b", "call-2", "shell", "two", 1561);
    complete(&mut state, "batch-c", "call-3", "shell", "three", 1548);
    complete(&mut state, "batch-d", "call-4", "shell", "four", 1559);
    settle(&mut state);

    let frame = render_terminal_text(&state, 80, 24);
    assert!(frame.contains("✓ 4 comandos · 6.2s"), "{frame}");
    assert_eq!(
        frame.matches("✓ shell").count(),
        0,
        "collapsed group must hide member rows\n{frame}"
    );
}

#[test]
fn enter_details_only_on_selected_or_last_live_collapsed_group() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    complete(&mut state, "batch-a", "call-1", "read", "one", 3);
    complete(&mut state, "batch-a", "call-2", "read", "two", 4);
    assert!(state.append_block(Block::new(
        "gap",
        BlockKind::Assistant("gap".into()),
        BlockLifecycle::Complete,
    )));
    complete(&mut state, "batch-b", "call-3", "shell", "three", 5);
    complete(&mut state, "batch-b", "call-4", "write", "four", 6);
    settle(&mut state);

    let live = render_terminal_text(&state, 120, 30);
    let hint_lines: Vec<_> = live
        .lines()
        .filter(|line| line.contains("Enter detalhes"))
        .collect();
    assert_eq!(
        hint_lines.len(),
        1,
        "only the last live collapsed group may hint\n{live}"
    );
    assert!(
        hint_lines[0].contains("1 edição, 1 comando"),
        "{hint_lines:?}"
    );

    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    let completed = render_terminal_text(&state, 120, 30);
    assert!(
        !completed.contains("Enter detalhes"),
        "completed run must not hint without selection\n{completed}"
    );

    let first_leader = state.blocks()[0].id.clone();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: first_leader,
        row_offset: 0,
    });
    let selected = render_terminal_text(&state, 120, 30);
    let hint_lines: Vec<_> = selected
        .lines()
        .filter(|line| line.contains("Enter detalhes"))
        .collect();
    assert_eq!(
        hint_lines.len(),
        1,
        "selected collapsed group must regain the hint\n{selected}"
    );
    assert!(hint_lines[0].contains("2 leituras"), "{hint_lines:?}");
}

#[test]
fn identical_consecutive_failures_collapse_to_one_row() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    let reason = "search requires exactly one of query or patterns";
    for index in 1..=4 {
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch("batch-a"),
            call_id: call(&format!("call-{index}")),
            name: "search".into(),
            arguments_summary: String::new(),
        });
        state.apply_event(UiEvent::ToolProgress {
            batch_id: batch("batch-a"),
            call_id: call(&format!("call-{index}")),
            name: "search".into(),
            preview: reason.into(),
            content_handle: None,
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id: batch("batch-a"),
            call_id: call(&format!("call-{index}")),
            name: "search".into(),
            success: false,
            duration_ms: 1,
        });
    }

    let frame = render_terminal_text(&state, 100, 24);
    assert!(
        frame.contains("✕ Buscou ×4 · search requires exactly one of query or patterns"),
        "{frame}"
    );
    assert_eq!(
        frame.matches("✕ Buscou").count(),
        1,
        "identical failures must occupy one row\n{frame}"
    );
}

#[test]
fn failed_and_cancelled_members_remain_individual_and_ordered() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    complete(&mut state, "batch-a", "call-1", "read", "ok", 2);
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-a"),
        call_id: call("call-2"),
        name: "shell".into(),
        arguments_summary: "bad".into(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch("batch-a"),
        call_id: call("call-2"),
        name: "shell".into(),
        success: false,
        duration_ms: 4,
    });
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch("batch-a"),
        call_id: call("call-3"),
        name: "write".into(),
        arguments_summary: "later".into(),
    });
    state.apply_event(UiEvent::RunCancelled { run_id: 1 });

    let blocks = state.blocks();
    // Three tool rows, then the receipt that closes an interrupted turn.
    assert_eq!(blocks.len(), 4);
    assert!(matches!(blocks[3].kind(), BlockKind::Receipt(receipt)
        if receipt.summary().starts_with("interrompido") && receipt.summary().contains("1 comando, 1 falhou")));
    assert_eq!(blocks[0].lifecycle, BlockLifecycle::Complete);
    assert_eq!(blocks[1].lifecycle, BlockLifecycle::Failed);
    assert_eq!(blocks[2].lifecycle, BlockLifecycle::Cancelled);
    let names = blocks[..3]
        .iter()
        .map(|block| match block.kind() {
            BlockKind::Tool(tool) => tool.name.as_str(),
            _ => panic!("tool"),
        })
        .collect::<Vec<_>>();
    assert_eq!(names, ["read", "shell", "write"]);
    let frame = render_terminal_text(&state, 80, 24);
    assert!(!frame.contains("tools ·"), "{frame}");
    assert!(frame.contains("✕ Executou · 4ms · falhou"), "{frame}");
    assert!(
        frame.contains("■ Escrevendo") && frame.contains("cancelada"),
        "{frame}"
    );
    assert!(!frame.contains("bad"), "{frame}");
    assert!(
        frame.contains("later"),
        "cancelled tools keep their target\n{frame}"
    );
}

#[test]
fn a_failure_joins_its_group_and_a_successful_retry_is_folded_into_its_row() {
    let fail = |state: &mut AppState, call_id: &str, arguments: &str| {
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch(call_id),
            call_id: call(call_id),
            name: "shell".into(),
            arguments_summary: arguments.into(),
        });
        state.apply_event(UiEvent::ToolProgress {
            batch_id: batch(call_id),
            call_id: call(call_id),
            name: "shell".into(),
            preview: "exit 101".into(),
            content_handle: None,
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id: batch(call_id),
            call_id: call(call_id),
            name: "shell".into(),
            success: false,
            duration_ms: 5,
        });
    };
    let mut state = AppState::new();
    complete(&mut state, "b-1", "edit", "patch", "path=src/lib.rs", 3);
    fail(&mut state, "test-1", "command=cargo test");
    fail(&mut state, "test-2", "command=cargo test");
    fail(&mut state, "lint", "command=cargo clippy");
    complete(
        &mut state,
        "b-4",
        "test-3",
        "shell",
        "command=cargo test",
        7,
    );
    settle(&mut state);

    let frame = render_terminal_text(&state, 100, 24);
    let rows = frame.lines().collect::<Vec<_>>();
    let header = rows
        .iter()
        .position(|row| row.contains("✓ 1 edição, 4 comandos"))
        .unwrap_or_else(|| panic!("one group for the whole streak\n{frame}"));
    // Identical failures share a row; the retried call says it later passed;
    // a failure nothing fixed stays plain.
    assert!(
        rows[header + 1].contains("✕ $ cargo test")
            && rows[header + 1].contains("×2")
            && rows[header + 1].contains("depois passou"),
        "{frame}"
    );
    assert!(
        rows[header + 2].contains("✕ $ cargo clippy")
            && !rows[header + 2].contains("depois passou"),
        "{frame}"
    );
    assert_eq!(frame.matches('✕').count(), 2, "{frame}");
    assert!(!frame.contains("Executou"), "{frame}");

    // Measured rows match what is drawn: header plus the two failure rows.
    let mut cache = WrapCache::default();
    let index = HeightIndex::build(state.blocks(), 99, &mut cache);
    assert_eq!(index.len(), 1);
    assert_eq!(index.total_rows, 3);

    // Enter on the group opens every member in provider order.
    let leader = state.blocks()[0].id.clone();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: leader,
        row_offset: 0,
    });
    reduce(&mut state, Action::Key(enter()));
    assert!(state.blocks()[0].group_expanded);
    let expanded = render_terminal_text(&state, 100, 24);
    assert_eq!(expanded.matches("$ cargo test").count(), 3, "{expanded}");
    assert!(!expanded.contains("depois passou"), "{expanded}");
}

#[test]
fn fresh_tool_keeps_its_row_until_the_emphasis_expires() {
    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "read", "one", 3);
    complete(&mut state, "batch-a", "call-2", "read", "two", 4);

    let fresh = render_terminal_text(&state, 80, 24);
    assert_eq!(
        fresh.matches("Leu ").count(),
        2,
        "each just-finished call keeps a row\n{fresh}"
    );
    assert!(
        !fresh.contains("2 leituras"),
        "the group waits out the emphasis\n{fresh}"
    );

    settle(&mut state);
    let grouped = render_terminal_text(&state, 80, 24);
    assert!(grouped.contains("2 leituras"), "{grouped}");
    assert!(
        !grouped.contains("✓ read"),
        "settled calls fold into the group\n{grouped}"
    );
}

#[test]
fn reduced_motion_folds_a_finished_tool_immediately() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use slim_tui::runtime::render_frame;
    use slim_tui::theme::{Capabilities, ColorDepth};

    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "read", "one", 3);
    complete(&mut state, "batch-a", "call-2", "read", "two", 4);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
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
                    reduced_motion: true,
                },
                &mut cache,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        text.push('\n');
    }
    assert!(
        text.contains("2 leituras"),
        "reduced motion shows the settled group\n{text}"
    );
}
