//! A pending interaction owns the keyboard (§16.4, §17.2, G220): input
//! surfaces that would steal or shadow its keys must close on arrival and
//! must not open while it is unanswered.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use slim_tui::api::{InteractionRequestId, UiCommand, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::render::WrapCache;
use slim_tui::runtime::terminal_action;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn request_id(value: &str) -> InteractionRequestId {
    InteractionRequestId(value.into())
}

/// Applies the request and marks the card as painted with readable content,
/// mirroring the runtime `SetApprovalContentAccessible` sync.
fn pending_approval(state: &mut AppState, id: &str) {
    state.apply_event(UiEvent::ApprovalRequired {
        request_id: request_id(id),
        summary: "rm -rf target".into(),
        persisted: false,
    });
    reduce(state, Action::SetApprovalContentAccessible(true));
}

fn sent(effects: &[Effect], command: UiCommand) -> bool {
    effects.contains(&Effect::Send(command))
}

#[test]
fn approval_arrival_closes_search_so_y_is_captured() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Key(ctrl(KeyCode::Char('f'))));
    reduce(&mut state, Action::Key(key(KeyCode::Char('q'))));
    assert_eq!(
        state.search.as_ref().map(|search| search.query.as_str()),
        Some("q")
    );

    pending_approval(&mut state, "approval-over-search");

    assert!(
        state.search.is_none(),
        "search must release the keyboard when an interaction arrives"
    );
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Char('y'))));
    assert!(
        sent(
            &effects,
            UiCommand::Approve {
                request_id: request_id("approval-over-search"),
            },
        ),
        "Y must reach the pending approval, not the closed search: {effects:?}"
    );
}

#[test]
fn approval_arrival_closes_palette() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Key(ctrl(KeyCode::Char('p'))));
    assert!(state.palette_query.is_some());

    pending_approval(&mut state, "approval-over-palette");

    assert!(
        state.palette_query.is_none(),
        "palette must release the keyboard when an interaction arrives"
    );
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Char('n'))));
    assert!(
        sent(
            &effects,
            UiCommand::Reject {
                request_id: request_id("approval-over-palette"),
            },
        ),
        "N must reach the pending approval, not the closed palette: {effects:?}"
    );
}

#[test]
fn approval_arrival_closes_inspector_so_answers_are_not_invisible() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Key(ctrl(KeyCode::Char('j'))));
    assert!(state.inspector.active.is_some());

    pending_approval(&mut state, "approval-over-inspector");

    assert!(
        state.inspector.active.is_none(),
        "a hidden approval must never answer key presses"
    );
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Char('y'))));
    assert!(
        sent(
            &effects,
            UiCommand::Approve {
                request_id: request_id("approval-over-inspector"),
            },
        ),
        "Y must reach the now-visible approval: {effects:?}"
    );
}

#[test]
fn pending_approval_blocks_surface_toggles() {
    let mut state = AppState::new();
    pending_approval(&mut state, "approval-toggles");

    for toggle in [
        ctrl(KeyCode::Char('f')),
        ctrl(KeyCode::Char('p')),
        key(KeyCode::F(1)),
        ctrl(KeyCode::Char('l')),
        ctrl(KeyCode::Char('d')),
        ctrl(KeyCode::Char('j')),
        ctrl(KeyCode::Char('r')),
        ctrl(KeyCode::Char('g')),
    ] {
        reduce(&mut state, Action::Key(toggle));
    }

    assert!(state.search.is_none(), "search must not open");
    assert!(state.palette_query.is_none(), "palette must not open");
    assert!(state.model_overlay.is_none(), "model overlay must not open");
    assert!(state.inspector.active.is_none(), "inspector must not open");
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Char('y'))));
    assert!(
        sent(
            &effects,
            UiCommand::Approve {
                request_id: request_id("approval-toggles"),
            },
        ),
        "Y must still reach the pending approval: {effects:?}"
    );
}

#[test]
fn input_arrival_closes_search_so_the_answer_reaches_the_composer() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Key(ctrl(KeyCode::Char('f'))));
    assert!(state.search.is_some());

    state.apply_event(UiEvent::InputRequired {
        request_id: request_id("input-over-search"),
        prompt: "branch name?".into(),
        options: Vec::new(),
        persisted: false,
    });

    assert!(state.search.is_none());
    reduce(&mut state, Action::Key(key(KeyCode::Char('x'))));
    assert_eq!(state.composer.payload(), "x");
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    assert!(
        sent(
            &effects,
            UiCommand::AnswerInput {
                request_id: request_id("input-over-search"),
                answer: "x".into(),
            },
        ),
        "typed answer must reach the input request: {effects:?}"
    );
}

#[test]
fn paste_does_not_edit_the_composer_underneath_search_or_palette() {
    for opener in [ctrl(KeyCode::Char('f')), ctrl(KeyCode::Char('p'))] {
        let mut state = AppState::new();
        reduce(&mut state, Action::Key(opener));
        reduce(&mut state, Action::Paste("hidden draft".into()));
        assert!(
            state.composer.is_empty(),
            "paste must not edit the composer while a stacked surface owns the keyboard"
        );
    }
}

#[test]
fn question_arrows_reach_the_reducer_through_terminal_dispatch() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-arrows"),
        question: "pick one".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "alpha".into(),
                description: String::new(),
            },
            slim_core::QuestionOption {
                label: "beta".into(),
                description: String::new(),
            },
        ],
        persisted: false,
    });

    let mut cache = WrapCache::default();
    let action = terminal_action(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &state,
        (120, 24),
        &mut cache,
    );
    assert!(
        matches!(action, Some(Action::Key(_))),
        "question arrows must stay key actions, not scroll intents: {action:?}"
    );
    reduce(&mut state, action.expect("key action"));
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    assert!(
        sent(
            &effects,
            UiCommand::AnswerQuestion {
                request_id: request_id("question-arrows"),
                answer: slim_core::QuestionAnswer::option(1, "beta").expect("option"),
            },
        ),
        "Down must move the question selection to the second option: {effects:?}"
    );
}
