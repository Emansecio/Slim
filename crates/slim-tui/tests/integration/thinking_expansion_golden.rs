use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{UiCommand, UiEvent};
use slim_tui::app::{AppState, FollowMode};
use slim_tui::block::{Block, BlockKind, BlockLifecycle, FoldState};
use slim_tui::reducer::{reduce, Action, Effect, ScrollIntent};
use slim_tui::render::{HeightIndex, WrapCache};
use slim_tui::runtime::{measure_scrollback, render_frame};
use slim_tui::theme::{Capabilities, ColorDepth};

fn fixture() -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "alpha\n界 beta\n👩‍💻 gamma\ndelta".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);
    state.apply_event(UiEvent::AssistantDelta {
        text: "final".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    state
}

fn render_at(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
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

fn select_thinking(state: &mut AppState, width: u16, height: u16) {
    let mut cache = WrapCache::default();
    let metrics = measure_scrollback(state, width, height, &mut cache);
    reduce(
        state,
        Action::Scroll {
            intent: ScrollIntent::Up,
            metrics,
        },
    );
    assert!(matches!(
        state
            .selected_block_id()
            .and_then(|id| state.blocks().iter().find(|block| &block.id == id))
            .map(|block| block.kind()),
        Some(BlockKind::Thinking(_))
    ));
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

#[test]
fn consecutive_collapsed_thoughts_render_as_one_row() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    for index in 0..5 {
        state.apply_event(UiEvent::ThinkingStarted);
        state.apply_event(UiEvent::ThinkingDelta {
            text: format!("scratch {index}"),
        });
        state.apply_event(UiEvent::ThinkingEnded);
    }
    let frame = render_at(&state, 80, 24);
    assert!(
        frame.contains("Pensamento ×4"),
        "prior completed thoughts must collapse\n{frame}"
    );
    assert_eq!(
        frame.matches("Pensamento").count(),
        2,
        "the last close frame keeps a preview row\n{frame}"
    );
    assert!(
        frame.contains("scratch 4"),
        "latest preview stays visible\n{frame}"
    );
    assert!(
        !frame.contains("scratch 0"),
        "released previews stay hidden\n{frame}"
    );
}

#[test]
fn keyboard_expands_and_collapses_thinking_inline_at_normative_sizes() {
    for (width, height) in [(120, 30), (80, 24), (60, 16), (40, 10)] {
        let mut state = fixture();
        select_thinking(&mut state, width, height);
        let thinking_id = state
            .selected_block_id()
            .expect("thinking selected")
            .clone();

        let collapsed = render_at(&state, width, height);
        assert!(
            collapsed.contains("> ▸ Pensamento"),
            "{width}x{height}\n{collapsed}"
        );
        assert!(collapsed.contains("Enter expandir"), "{collapsed}");
        for hidden in ["alpha", "beta", "gamma", "delta"] {
            assert!(
                !collapsed.contains(hidden),
                "collapsed thinking leaked {hidden} at {width}x{height}\n{collapsed}"
            );
        }

        reduce(&mut state, Action::Key(enter()));
        assert!(matches!(
            state
                .blocks()
                .iter()
                .find(|block| block.id == thinking_id)
                .map(|block| block.fold),
            Some(FoldState::Expanded)
        ));
        let expanded = render_at(&state, width, height);
        assert!(expanded.contains("> ▾ Pensamento"), "{expanded}");
        assert!(expanded.contains("Enter recolher"), "{expanded}");
        assert!(expanded.contains("delta"), "{width}x{height}\n{expanded}");
        assert!(
            !expanded.contains('┌') && !expanded.contains('┐'),
            "thinking expansion must stay inline at {width}x{height}\n{expanded}"
        );

        let anchor = match &state.scroll.mode {
            FollowMode::Pinned(anchor) => anchor.clone(),
            other => panic!("expected pinned selection, got {other:?}"),
        };
        assert_eq!(anchor.block_id, thinking_id);
        assert_eq!(anchor.row_offset, 0);
        let mut cache = WrapCache::default();
        assert!(HeightIndex::build(state.blocks(), width, &mut cache)
            .row_for_anchor(&anchor)
            .is_some());

        reduce(&mut state, Action::Key(enter()));
        assert!(matches!(
            state
                .blocks()
                .iter()
                .find(|block| block.id == thinking_id)
                .map(|block| block.fold),
            Some(FoldState::Collapsed)
        ));
    }
}

#[test]
fn completed_thinking_retains_preview_until_the_next_content_boundary() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "first hidden line\nsecond live line\nfinal live line".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);

    let thinking = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Thinking(_)))
        .expect("completed thinking block");
    assert_eq!(thinking.lifecycle, BlockLifecycle::Complete);
    assert!(
        thinking.preview_retained,
        "the close frame keeps the latest preview"
    );
    let retained = render_at(&state, 48, 16);
    assert!(retained.contains("… second live line"), "{retained}");
    assert!(retained.contains("final live line"), "{retained}");

    state.apply_event(UiEvent::AssistantDelta {
        text: "final answer".into(),
    });
    let thinking = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Thinking(_)))
        .expect("thinking block after boundary");
    assert!(
        !thinking.preview_retained,
        "next semantic content releases the preview"
    );
    let released = render_at(&state, 48, 16);
    assert!(!released.contains("second live line"), "{released}");
    assert!(!released.contains("final live line"), "{released}");
}

#[test]
fn nonempty_composer_keeps_enter_submit_priority() {
    let mut state = fixture();
    select_thinking(&mut state, 80, 24);
    let thinking_id = state
        .selected_block_id()
        .expect("thinking selected")
        .clone();
    reduce(&mut state, Action::Paste("send this".into()));

    let effects = reduce(&mut state, Action::Key(enter()));

    assert!(effects.contains(&Effect::Send(UiCommand::SendPrompt("send this".into()))));
    assert!(matches!(
        state
            .blocks()
            .iter()
            .find(|block| block.id == thinking_id)
            .map(|block| block.fold),
        Some(FoldState::Collapsed)
    ));
}

#[test]
fn first_up_never_jumps_to_an_offscreen_thinking_block() {
    let mut state = AppState::new();
    let mut thinking = Block::new(
        "old-thinking",
        BlockKind::Thinking("old reasoning".into()),
        BlockLifecycle::Complete,
    );
    thinking.fold = FoldState::Collapsed;
    assert!(state.append_block(thinking));
    for index in 0..30 {
        assert!(state.append_block(Block::new(
            format!("user-{index}"),
            BlockKind::User(format!("recent question {index}")),
            BlockLifecycle::Complete,
        )));
    }

    let mut cache = WrapCache::default();
    let metrics = measure_scrollback(&state, 40, 10, &mut cache);
    assert!(metrics.viewport_start > 0);
    reduce(
        &mut state,
        Action::Scroll {
            intent: ScrollIntent::Up,
            metrics,
        },
    );

    assert!(state.selected_block_id().is_none());
    assert!(matches!(
        &state.scroll.mode,
        FollowMode::Pinned(anchor) if anchor.block_id.0.as_ref() != "old-thinking"
    ));
}

#[test]
fn hidden_scrollback_rows_never_create_a_thinking_selection() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    let mut thinking = Block::new(
        "hidden-thinking",
        BlockKind::Thinking("hidden".into()),
        BlockLifecycle::Complete,
    );
    thinking.fold = FoldState::Collapsed;
    assert!(state.append_block(thinking));
    for index in 0..4 {
        assert!(state.append_block(Block::new(
            format!("filler-{index}"),
            BlockKind::User(format!("filler {index}")),
            BlockLifecycle::Complete,
        )));
    }
    state.notifications = vec!["one".into(), "two".into(), "three".into()];
    let scrollback_height = slim_tui::layout::plan(40, 8, 0, true).scrollback.height;
    assert!(
        scrollback_height <= 3,
        "fixture must leave zero transcript rows"
    );

    let mut cache = WrapCache::default();
    let metrics = measure_scrollback(&state, 40, 8, &mut cache);
    assert!(metrics.last_visible_foldable_anchor.is_none());
    reduce(
        &mut state,
        Action::Scroll {
            intent: ScrollIntent::Up,
            metrics,
        },
    );
    assert!(state.selected_block_id().is_none());
}

#[test]
fn collapsed_preview_caps_wrapped_unicode_rows_without_losing_expandability() {
    let mut state = AppState::new();
    let mut thinking = Block::new(
        "wrapped-thinking",
        BlockKind::Thinking(format!("{}TAIL", "界".repeat(80))),
        BlockLifecycle::Complete,
    );
    thinking.fold = FoldState::Collapsed;
    assert!(state.append_block(thinking));
    select_thinking(&mut state, 40, 10);

    let collapsed = render_at(&state, 40, 10);
    assert!(!collapsed.contains("TAIL"), "{collapsed}");
    reduce(&mut state, Action::Key(enter()));
    let expanded = render_at(&state, 40, 10);
    assert!(expanded.contains("TAIL"), "{expanded}");
}

#[test]
fn modified_enter_keeps_composer_semantics_when_thinking_is_selected() {
    let mut state = fixture();
    select_thinking(&mut state, 80, 24);
    let thinking_id = state
        .selected_block_id()
        .expect("thinking selected")
        .clone();

    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
    );

    assert_eq!(state.composer.payload(), "\n");
    assert!(matches!(
        state
            .blocks()
            .iter()
            .find(|block| block.id == thinking_id)
            .map(|block| block.fold),
        Some(FoldState::Collapsed)
    ));
}

#[test]
fn repeat_and_release_enter_never_toggle_the_selected_block() {
    let mut state = fixture();
    select_thinking(&mut state, 80, 24);
    let fold_revision = state.revisions.fold;

    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                kind,
            )),
        );
    }

    assert_eq!(state.revisions.fold, fold_revision);
    assert!(matches!(
        state
            .selected_block_id()
            .and_then(|id| state.blocks().iter().find(|block| &block.id == id))
            .map(|block| block.fold),
        Some(FoldState::Collapsed)
    ));
}

#[test]
fn auto_fold_thinking_hides_body_until_expanded() {
    let mut state = AppState::new();
    let block = Block::new(
        "auto-thinking",
        BlockKind::Thinking("secret body".into()),
        BlockLifecycle::Complete,
    );
    assert_eq!(block.fold, FoldState::Auto);
    assert!(state.append_block(block));

    let collapsed = render_at(&state, 80, 24);
    assert!(collapsed.contains("Pensamento"), "{collapsed}");
    assert!(!collapsed.contains("secret body"), "{collapsed}");
}
