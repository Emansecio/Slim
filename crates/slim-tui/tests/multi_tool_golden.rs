use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use slim_tui::api::{ToolBatchId, ToolCallId, UiEvent};
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

#[test]
fn same_batch_groups_different_names_and_expands_in_provider_order() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    complete(&mut state, "batch-a", "call-1", "read", "path=one", 7);
    complete(&mut state, "batch-a", "call-2", "shell", "cmd=two", 5);

    let collapsed = render_terminal_text(&state, 120, 30);
    assert!(
        collapsed.contains("✓ 2 tools · read, shell · 12ms"),
        "{collapsed}"
    );
    assert!(
        collapsed.contains("Enter details"),
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
    assert_eq!(state.blocks()[0].fold, FoldState::Expanded);
    let expanded = render_terminal_text(&state, 120, 30);
    assert!(
        expanded.contains("✓ 2 tools · read, shell · 12ms"),
        "{expanded}"
    );
    assert!(
        !expanded.contains("Enter details"),
        "expanded header must not keep the collapse-competing hint\n{expanded}"
    );
    let first = expanded.find("call-1").expect("first member");
    let second = expanded.find("call-2").expect("second member");
    assert!(first < second, "provider order changed\n{expanded}");
    for expected in ["read", "path=one", "shell", "cmd=two"] {
        assert!(
            expanded.contains(expected),
            "missing {expected}\n{expanded}"
        );
    }
    let first_row = expanded
        .lines()
        .find(|line| line.contains("path=one"))
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
    assert_ne!(state.blocks()[0].fold, FoldState::Expanded);
    let collapsed_again = render_terminal_text(&state, 120, 30);
    assert!(!collapsed_again.contains("call-1"));
}

#[test]
fn same_batch_groups_equal_names_by_batch_identity() {
    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "read", "one", 3);
    complete(&mut state, "batch-a", "call-2", "read", "two", 4);

    let frame = render_terminal_text(&state, 80, 24);
    assert!(frame.contains("✓ 2 tools · read ×2 · 7ms"), "{frame}");
    assert!(!frame.contains("✓ read"), "{frame}");
}

#[test]
fn grouped_header_summarizes_names_counts_and_total_duration() {
    let mut state = AppState::new();
    complete(&mut state, "batch-a", "call-1", "read", "one", 7);
    complete(&mut state, "batch-a", "call-2", "read", "two", 8);
    complete(&mut state, "batch-a", "call-3", "read", "three", 9);
    complete(&mut state, "batch-a", "call-4", "shell", "four", 18);

    let wide = render_terminal_text(&state, 120, 30);
    let expected = format!(
        "4 tools {} read {}3, shell {} 42ms",
        '\u{00b7}', '\u{00d7}', '\u{00b7}'
    );
    assert!(wide.contains(&expected), "{wide}");

    let narrow = render_terminal_text(&state, 24, 12);
    let compact = format!("4 tools {} 42ms", '\u{00b7}');
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
    assert!(frame.contains("2 tools · read, shell"), "{frame}");
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
    let leader = state.blocks()[0].id.clone();
    let mut cache = WrapCache::default();

    let action = terminal_action(Event::Key(enter()), &state, (80, 24), &mut cache);
    assert!(
        matches!(&action, Some(Action::ToggleBlock(id)) if id == &leader),
        "plain Enter must target the visible group, got {action:?}"
    );
    reduce(&mut state, action.expect("toggle action"));
    assert_eq!(state.blocks()[0].fold, FoldState::Expanded);
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

    let frame = render_terminal_text(&state, 80, 24);
    assert!(
        frame.contains("✓ 4 tools · read ×4 · 7ms"),
        "collapsed thought must not split adjacent tool groups\n{frame}"
    );
    assert_eq!(
        frame.matches("Thought").count(),
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

    let frame = render_terminal_text(&state, 80, 24);
    assert!(frame.contains("✓ 4 tools · shell ×4 · 6.2s"), "{frame}");
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

    let live = render_terminal_text(&state, 120, 30);
    let hint_lines: Vec<_> = live
        .lines()
        .filter(|line| line.contains("Enter details"))
        .collect();
    assert_eq!(
        hint_lines.len(),
        1,
        "only the last live collapsed group may hint\n{live}"
    );
    assert!(hint_lines[0].contains("shell, write"), "{hint_lines:?}");

    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    let completed = render_terminal_text(&state, 120, 30);
    assert!(
        !completed.contains("Enter details"),
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
        .filter(|line| line.contains("Enter details"))
        .collect();
    assert_eq!(
        hint_lines.len(),
        1,
        "selected collapsed group must regain the hint\n{selected}"
    );
    assert!(hint_lines[0].contains("read ×2"), "{hint_lines:?}");
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
        frame.contains("✕ search ×4 · search requires exactly one of query or patterns"),
        "{frame}"
    );
    assert_eq!(
        frame.matches("✕ search").count(),
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
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].lifecycle, BlockLifecycle::Complete);
    assert_eq!(blocks[1].lifecycle, BlockLifecycle::Failed);
    assert_eq!(blocks[2].lifecycle, BlockLifecycle::Cancelled);
    let names = blocks
        .iter()
        .map(|block| match block.kind() {
            BlockKind::Tool(tool) => tool.name.as_str(),
            _ => panic!("tool"),
        })
        .collect::<Vec<_>>();
    assert_eq!(names, ["read", "shell", "write"]);
    let frame = render_terminal_text(&state, 80, 24);
    assert!(!frame.contains("tools ·"), "{frame}");
    assert!(frame.contains("✕ shell · 4ms · failed"), "{frame}");
    assert!(frame.contains("■ write · cancelled"), "{frame}");
    assert!(!frame.contains("bad"), "{frame}");
    assert!(!frame.contains("later"), "{frame}");
}
