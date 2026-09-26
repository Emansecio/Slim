use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{reduce, slash_matches, slash_matches_with_skills, Action, Effect};
use crate::api::{PromptOrigin, UiCommand};
use crate::app::AppState;

fn type_text(state: &mut AppState, text: &str) {
    for character in text.chars() {
        reduce(
            state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
}

fn press_key(state: &mut AppState, code: KeyCode) {
    reduce(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

#[test]
fn slash_opens_at_start_and_mid_prompt() {
    let mut state = AppState::new();
    type_text(&mut state, "/");
    assert!(
        state.slash_suggestions.is_some(),
        "bare slash lists every command"
    );

    let mut state = AppState::new();
    type_text(&mut state, "explain this /mo");
    let suggestions = state
        .slash_suggestions
        .as_ref()
        .expect("mid-prompt slash opens");
    assert_eq!(suggestions.query, "mo");
}

#[test]
fn slash_opens_on_token_under_cursor_mid_draft() {
    let mut state = AppState::new();
    type_text(&mut state, "run /logi now");
    for _ in 0..5 {
        press_key(&mut state, KeyCode::Left);
    }
    let suggestions = state
        .slash_suggestions
        .as_ref()
        .expect("slash under cursor opens mid-draft");
    assert_eq!(suggestions.query, "logi");
}

#[test]
fn tab_completes_mid_draft_token_preserving_tail() {
    let mut state = AppState::new();
    type_text(&mut state, "run /logi now");
    for _ in 0..5 {
        press_key(&mut state, KeyCode::Left);
    }
    press_key(&mut state, KeyCode::Tab);
    assert_eq!(state.composer.payload(), "run /login now");
    assert!(state.slash_suggestions.is_none());
}

#[test]
fn moving_cursor_off_slash_token_closes_popup() {
    let mut state = AppState::new();
    type_text(&mut state, "run /lo");
    assert!(state.slash_suggestions.is_some());
    // Caret back onto "run": the token under the cursor no longer starts
    // with `/`, so the popup must close.
    for _ in 0..4 {
        press_key(&mut state, KeyCode::Left);
    }
    assert!(state.slash_suggestions.is_none());
}

#[test]
fn plain_text_never_opens() {
    let mut state = AppState::new();
    type_text(&mut state, "explain this mode");
    assert!(state.slash_suggestions.is_none());
}

#[test]
fn filter_narrows_to_prefix() {
    assert_eq!(slash_matches("lo"), vec!["/login", "/logout"]);
    assert_eq!(slash_matches("logi"), vec!["/login"]);
    assert_eq!(slash_matches("he"), vec!["/help"]);
    assert_eq!(slash_matches("").len(), 16);
    assert!(slash_matches("zzz").is_empty());
}

#[test]
fn discovered_skill_autocompletes_and_dispatches_as_a_prompt() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.set_skill_names_for_test(vec!["review-code".into()]);
    type_text(&mut state, "/rev");
    assert_eq!(
        slash_matches_with_skills(&state, "rev"),
        vec!["/review-code"]
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    );
    type_text(&mut state, "check this change");
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { prompt, admission })
            if prompt == "/review-code check this change"
                && admission.origin == PromptOrigin::Direct
    )));
}

#[test]
fn tab_completes_selected_command_into_draft() {
    let mut state = AppState::new();
    type_text(&mut state, "run /lo");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    );
    assert_eq!(state.composer.payload(), "run /logout ");
    assert!(state.slash_suggestions.is_none());
}

#[test]
fn backspace_after_tab_complete_removes_one_grapheme_not_the_draft() {
    let mut state = AppState::new();
    type_text(&mut state, "run /lo");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
    );
    assert_eq!(state.composer.payload(), "run /logout");
}

#[test]
fn enter_executes_highlighted_command() {
    let mut state = AppState::new();
    state.auth_provider = Some(crate::api::LoginProvider::OpenAiCodex);
    state.authenticated = true;
    type_text(&mut state, "/mo");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(state.model_overlay.is_some());
    assert!(state.composer.payload().is_empty());
    assert!(state.slash_suggestions.is_none());
}

#[test]
fn esc_dismisses_without_touching_draft() {
    let mut state = AppState::new();
    type_text(&mut state, "/mo");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.slash_suggestions.is_none());
    assert_eq!(state.composer.payload(), "/mo");
}

#[test]
fn editing_reopens_and_closes_the_popup() {
    let mut state = AppState::new();
    type_text(&mut state, "/mode");
    assert!(state.slash_suggestions.is_some());
    for _ in 0..5 {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
    }
    assert!(
        state.slash_suggestions.is_none(),
        "empty draft closes the popup"
    );
}

#[test]
fn paste_with_slash_token_opens() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Paste("run /log".into()));
    let suggestions = state
        .slash_suggestions
        .as_ref()
        .expect("paste triggers the popup too");
    assert_eq!(suggestions.query, "log");
}
