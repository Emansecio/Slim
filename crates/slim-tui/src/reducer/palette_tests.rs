use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{reduce, Action};
use crate::app::AppState;

fn ctrl_p() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
}

#[test]
fn palette_filters_and_submits_top_match() {
    let mut state = AppState::new();
    state.auth_provider = Some(crate::api::LoginProvider::OpenAiCodex);
    state.authenticated = true;
    reduce(&mut state, Action::Key(ctrl_p()));
    assert!(state.palette_query.is_some());

    for character in "mod".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
    assert_eq!(state.palette_query.as_deref(), Some("mod"));

    // Top match is "/model": submitted as a slash command and executed.
    let _ = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(state.palette_query.is_none());
    assert!(
        state.model_overlay.is_some(),
        "palette submit must execute /model (G243)"
    );
}

#[test]
fn palette_searches_description_without_case_and_keeps_the_exact_draft() {
    let mut state = AppState::new();
    state.auth_provider = Some(crate::api::LoginProvider::OpenAiCodex);
    state.authenticated = true;
    state.composer.insert_text("rascunho de trabalho");
    state.composer.move_left();
    let draft = state.composer.clone();

    reduce(&mut state, Action::Key(ctrl_p()));
    for character in "MoDeLo".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
    assert_eq!(super::palette_matches("MoDeLo")[0], "/model");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );

    assert!(state.model_overlay.is_some());
    assert_eq!(state.composer, draft, "cursor and undo history survive");
}

#[test]
fn palette_query_without_slash_matches_and_executes_login() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Key(ctrl_p()));
    for character in "log".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
    let _ = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        state.login_overlay.is_some(),
        "bare 'log' must run /login without the leading slash"
    );
}

#[test]
fn esc_dismisses_palette_without_submitting() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Key(ctrl_p()));
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.palette_query.is_none());
}

#[test]
fn palette_matches_by_substring_not_only_prefix() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Key(ctrl_p()));
    for character in "agn".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
    let _ = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        state.palette_query.is_none(),
        "enter must submit the substring match"
    );
    assert_eq!(
        state.inspector.active,
        Some(crate::inspector::InspectorKind::Diagnostics),
        "bare 'agn' must run /diagnostics"
    );
}

#[test]
fn f1_toggles_palette_open_and_closed() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
    );
    assert!(state.palette_query.is_some(), "F1 must open palette");

    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
    );
    assert!(state.palette_query.is_none(), "F1 must toggle palette off");
}

#[test]
fn help_slash_command_shows_shortcuts_toast() {
    let mut state = AppState::new();
    state.composer.insert_text("/help");
    let _ = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        state
            .notifications
            .iter()
            .any(|n| n.message.contains("F1 / Ctrl+P")),
        "help command must display shortcuts notification"
    );
}

#[test]
fn palette_selection_clamps_when_narrowed() {
    let mut state = AppState::new();
    state.palette_query = Some(String::new());
    state.palette_selected = 100;
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    let total = super::palette_matches("").len();
    assert!(state.palette_selected < total, "must clamp selection");
}

#[test]
fn search_selection_clamps_when_matches_are_fewer() {
    let mut state = AppState::new();
    state.search = Some(crate::inspector::SearchState {
        query: "test".into(),
        selected: 99,
        filter: crate::inspector::SearchFilter::All,
    });
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    assert_eq!(state.search.as_ref().unwrap().selected, 0);
}
