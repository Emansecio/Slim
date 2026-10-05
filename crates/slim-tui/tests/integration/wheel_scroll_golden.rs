//! The mouse wheel only scrolls (DESIGN-SLIM-TUI §1.2, revision of
//! 05/10/2026): it moves the viewport and never selects a block, draws the
//! `>` marker or the `Enter …` hint, or jumps between foldable rows. The
//! keyboard keeps its navigation and its selection.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{InteractionRequestId, UiEvent};
use slim_tui::app::{AppState, FollowMode};
use slim_tui::reducer::{reduce, Action};
use slim_tui::render::WrapCache;
use slim_tui::runtime::{render_frame, terminal_action};
use slim_tui::theme::{Capabilities, ColorDepth};

const SIZE: (u16, u16) = (80, 24);

fn render(state: &AppState) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(SIZE.0, SIZE.1)).expect("terminal");
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                state,
                Capabilities {
                    color_depth: ColorDepth::TrueColor,
                    mouse: true,
                    clipboard: false,
                    images: false,
                    reduced_motion: true,
                },
                &mut WrapCache::default(),
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect()
        })
        .collect()
}

/// No row of the frame carries the selection marker or an `Enter` hint.
fn assert_unmarked(rows: &[String]) {
    for row in rows {
        assert!(!row.contains("Enter expandir"), "{row:?}");
        assert!(!row.contains("Enter recolher"), "{row:?}");
        assert!(!row.starts_with("> "), "{row:?}");
    }
}

fn wheel(state: &mut AppState, cache: &mut WrapCache, up: bool) {
    let kind = if up {
        MouseEventKind::ScrollUp
    } else {
        MouseEventKind::ScrollDown
    };
    let event = Event::Mouse(MouseEvent {
        kind,
        column: 10,
        row: 5,
        modifiers: KeyModifiers::NONE,
    });
    if let Some(action) = terminal_action(event, state, SIZE, cache) {
        reduce(state, action);
    }
}

fn press(state: &mut AppState, cache: &mut WrapCache, code: KeyCode) {
    let event = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    if let Some(action) = terminal_action(event, state, SIZE, cache) {
        reduce(state, action);
    }
}

/// A run in progress with `pairs` finished thoughts (each one folded row)
/// separated by answers.
fn running(pairs: usize) -> AppState {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(slim_tui::api::LoginProvider::Anthropic),
        authenticated: true,
    });
    state.apply_event(UiEvent::UserMessageAdded {
        text: "investigue".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    for index in 0..pairs {
        state.apply_event(UiEvent::ThinkingStarted);
        state.apply_event(UiEvent::ThinkingDelta {
            text: format!("pensamento {index}"),
        });
        state.clock.elapsed_ms += 1_300;
        state.apply_event(UiEvent::ThinkingEnded);
        state.apply_event(UiEvent::AssistantDelta {
            text: format!("nota {index}: um texto curto."),
        });
        state.apply_event(UiEvent::AssistantEnded);
    }
    state
}

#[test]
fn the_wheel_over_a_transcript_that_fits_does_nothing() {
    let mut state = running(2);
    let mut cache = WrapCache::default();
    let before = render(&state);
    assert!(state.scroll.is_live_edge());
    for up in [true, true, false, true, false, false] {
        wheel(&mut state, &mut cache, up);
        assert!(state.scroll.is_live_edge(), "no pin");
        assert_eq!(state.scroll.unseen, 0);
        assert_eq!(state.selected_block_id(), None);
        assert_eq!(render(&state), before);
    }
    assert_unmarked(&before);
    assert!(!before.iter().any(|row| row.contains("End recentes")));
    // Content arriving afterwards is not counted as unseen either.
    state.apply_event(UiEvent::AssistantDelta {
        text: "mais uma nota.".into(),
    });
    assert_eq!(state.scroll.unseen, 0);
}

#[test]
fn the_wheel_over_a_long_transcript_scrolls_without_marker_or_hint_at_every_step() {
    let mut state = running(30);
    let mut cache = WrapCache::default();
    let bottom = render(&state);
    assert!(state.scroll.is_live_edge());
    assert_unmarked(&bottom);
    let mut seen = vec![bottom.clone()];
    // Up: the view moves a step at a time and rests on whatever block it
    // lands on, none of them selected.
    for _ in 0..60 {
        wheel(&mut state, &mut cache, true);
        let rows = render(&state);
        assert_unmarked(&rows);
        assert_eq!(state.selected_block_id(), None);
        if rows != *seen.last().unwrap() {
            seen.push(rows);
        }
    }
    assert!(seen.len() > 20, "the wheel moved the view: {}", seen.len());
    assert!(!state.scroll.is_live_edge());
    // The unseen counter and its hint are the existing ones for a pinned view.
    // Down returns to the live edge, as before, still unmarked.
    for _ in 0..200 {
        wheel(&mut state, &mut cache, false);
        assert_unmarked(&render(&state));
        if state.scroll.is_live_edge() {
            break;
        }
    }
    assert!(state.scroll.is_live_edge());
    assert_eq!(render(&state), bottom);
}

#[test]
fn the_keyboard_selects_again_from_where_the_wheel_left_the_view() {
    let mut state = running(30);
    let mut cache = WrapCache::default();
    for _ in 0..20 {
        wheel(&mut state, &mut cache, true);
    }
    assert_eq!(state.selected_block_id(), None);
    assert_unmarked(&render(&state));
    // Arrow navigation resumes: within a few presses the view rests on a
    // foldable row and shows its marker and hint.
    let mut selected = false;
    for _ in 0..6 {
        press(&mut state, &mut cache, KeyCode::Up);
        if state.selected_block_id().is_some() {
            selected = true;
            break;
        }
    }
    assert!(selected, "keyboard Up selects a foldable row");
    let rows = render(&state);
    assert!(
        rows.iter().any(|row| row.contains("Enter expandir")),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|row| row.starts_with("> ")), "{rows:#?}");
    // Enter opens the selected row, as before.
    press(&mut state, &mut cache, KeyCode::Enter);
    assert!(render(&state).iter().any(|row| row.contains('▾')));
}

#[test]
fn the_wheel_after_a_keyboard_selection_drops_it() {
    let mut state = running(30);
    let mut cache = WrapCache::default();
    press(&mut state, &mut cache, KeyCode::Up);
    for _ in 0..6 {
        if state.selected_block_id().is_some() {
            break;
        }
        press(&mut state, &mut cache, KeyCode::Up);
    }
    assert!(state.selected_block_id().is_some());
    assert!(render(&state)
        .iter()
        .any(|row| row.contains("Enter expandir")));
    wheel(&mut state, &mut cache, true);
    assert_eq!(state.selected_block_id(), None);
    assert_unmarked(&render(&state));
    // Enter no longer acts on the row the view happens to rest on.
    let folds = state.revisions.fold;
    press(&mut state, &mut cache, KeyCode::Enter);
    assert_eq!(state.revisions.fold, folds);
    // And on a transcript that fits, the wheel also drops a selection.
    let mut small = running(2);
    press(&mut small, &mut cache, KeyCode::Up);
    assert!(small.selected_block_id().is_some());
    wheel(&mut small, &mut cache, true);
    assert_eq!(small.selected_block_id(), None);
    assert_unmarked(&render(&small));
}

#[test]
fn a_click_that_pins_the_view_still_selects_as_before() {
    let mut state = running(30);
    let mut cache = WrapCache::default();
    for _ in 0..10 {
        wheel(&mut state, &mut cache, true);
    }
    assert!(state.scroll.pointer);
    let anchor = match state.scroll.mode.clone() {
        FollowMode::Pinned(anchor) => anchor,
        other => panic!("{other:?}"),
    };
    reduce(
        &mut state,
        Action::StartPinnedScreenSelection {
            x: 4,
            y: 3,
            area: None,
            anchor,
        },
    );
    assert!(!state.scroll.pointer, "a click is not wheel scrolling");
}

#[test]
fn an_approval_keeps_the_pointer_and_the_transcript_stays_put() {
    let mut state = running(30);
    let mut cache = WrapCache::default();
    state.apply_event(UiEvent::ApprovalRequired {
        request_id: InteractionRequestId("approval-1".into()),
        summary: (0..40)
            .map(|line| format!("linha {line} do plano"))
            .collect::<Vec<_>>()
            .join("\n"),
        persisted: false,
    });
    // Away from the card the wheel does nothing: the transcript never scrolls
    // behind an approval. (Over the card, and over an inspector panel, the
    // routing is pinned by `navigation_visibility` and the runtime tests.)
    let away = terminal_action(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
        &state,
        SIZE,
        &mut cache,
    );
    assert!(away.is_none(), "{away:?}");
}
