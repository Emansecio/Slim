use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{reduce, Action, Effect};
use crate::api::{UiCommand, UiEvent};
use crate::app::AppState;

fn type_text(state: &mut AppState, text: &str) -> Vec<Effect> {
    let mut effects = Vec::new();
    for character in text.chars() {
        effects.extend(reduce(
            state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        ));
    }
    effects
}

fn press(state: &mut AppState, code: KeyCode) -> Vec<Effect> {
    reduce(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn requests(effects: &[Effect]) -> Vec<u64> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(UiCommand::RequestWorkspaceFiles { request_id }) => Some(*request_id),
            _ => None,
        })
        .collect()
}

fn deliver(state: &mut AppState, request_id: u64, paths: &[&str]) {
    reduce(
        state,
        Action::UiEventReceived(UiEvent::WorkspaceFiles {
            request_id,
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            truncated: false,
        }),
    );
}

/// A state whose `@` popup already has its file list.
fn loaded(paths: &[&str]) -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    let effects = type_text(&mut state, "@");
    let id = *requests(&effects).first().expect("first @ requests files");
    deliver(&mut state, id, paths);
    state
}

#[test]
fn at_requests_files_once_and_shows_loading_row() {
    let mut state = AppState::new();
    let effects = type_text(&mut state, "@s");
    assert_eq!(requests(&effects).len(), 1, "one fetch for the whole token");
    let popup = state.mention_suggestions.as_ref().expect("popup opens");
    assert!(popup.matches.is_empty() && !state.workspace_files_loaded);
    assert_eq!(popup.query, "s");
}

#[test]
fn list_arrival_ranks_the_current_query_and_stale_ids_are_ignored() {
    let mut state = AppState::new();
    let effects = type_text(&mut state, "@lib");
    let id = requests(&effects)[0];
    deliver(&mut state, id + 7, &["nope.rs"]);
    assert!(!state.workspace_files_loaded, "unknown request id dropped");
    deliver(&mut state, id, &["README.md", "src/lib.rs", "src/main.rs"]);
    let popup = state.mention_suggestions.as_ref().expect("still open");
    let hits: Vec<&str> = popup
        .matches
        .iter()
        .map(|index| state.workspace_files[*index].as_str())
        .collect();
    assert_eq!(hits, ["src/lib.rs"]);
}

#[test]
fn no_candidates_after_load_closes_the_popup() {
    let mut state = loaded(&["a.rs"]);
    type_text(&mut state, "zzz");
    assert!(state.mention_suggestions.is_none());
}

#[test]
fn tab_completes_mid_draft_preserving_tail_and_paste_chip() {
    let mut state = loaded(&["src/lib.rs", "src/main.rs"]);
    press(&mut state, KeyCode::Backspace);
    type_text(&mut state, "veja @lib agora");
    for _ in 0.." agora".chars().count() {
        press(&mut state, KeyCode::Left);
    }
    assert_eq!(state.mention_suggestions.as_ref().unwrap().query, "lib");
    press(&mut state, KeyCode::Tab);
    assert_eq!(state.composer.payload(), "veja @src/lib.rs agora");
    assert!(state.mention_suggestions.is_none());
    assert_eq!(
        state.composer.cursor(),
        "veja @src/lib.rs ".chars().count(),
        "cursor sits after the separating space"
    );
}

#[test]
fn enter_completes_and_never_submits() {
    let mut state = loaded(&["docs/plan.md"]);
    type_text(&mut state, "plan");
    let effects = press(&mut state, KeyCode::Enter);
    assert_eq!(state.composer.payload(), "@docs/plan.md ");
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::PreparePrompt { .. }))),
        "Enter on a completion must not send the prompt"
    );
}

#[test]
fn arrows_move_selection_and_esc_keeps_the_draft() {
    let mut state = loaded(&["a/one.rs", "a/two.rs", "a/three.rs"]);
    press(&mut state, KeyCode::Down);
    press(&mut state, KeyCode::Down);
    assert_eq!(state.mention_suggestions.as_ref().unwrap().selected, 2);
    press(&mut state, KeyCode::Up);
    assert_eq!(state.mention_suggestions.as_ref().unwrap().selected, 1);
    press(&mut state, KeyCode::Esc);
    assert!(state.mention_suggestions.is_none());
    assert_eq!(state.composer.payload(), "@");
}

#[test]
fn email_like_text_and_pasted_at_signs_do_not_open_the_popup() {
    let mut state = AppState::new();
    type_text(&mut state, "mail a@b.com");
    assert!(state.mention_suggestions.is_none());
    reduce(&mut state, Action::Paste("x\n@secret\ny".into()));
    assert!(state.mention_suggestions.is_none());
}

#[test]
fn leaving_the_token_closes_the_popup() {
    let mut state = loaded(&["src/lib.rs"]);
    type_text(&mut state, "li");
    assert!(state.mention_suggestions.is_some());
    type_text(&mut state, " ");
    assert!(state.mention_suggestions.is_none());
}

#[test]
fn enter_while_list_is_loading_still_submits() {
    let mut state = AppState::new();
    state.authenticated = true;
    type_text(&mut state, "@wip");
    assert!(state.mention_suggestions.is_some());
    let effects = press(&mut state, KeyCode::Enter);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::PreparePrompt { .. }))),
        "Enter is not swallowed by an empty popup"
    );
}

#[test]
fn finished_tool_marks_list_stale_and_next_open_refetches() {
    let mut state = loaded(&["a.rs"]);
    press(&mut state, KeyCode::Esc);
    state.composer.clear();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::ToolEnded {
            batch_id: crate::api::ToolBatchId(String::from("b").into()),
            call_id: crate::api::ToolCallId(String::from("c").into()),
            name: "write".into(),
            success: true,
            duration_ms: 1,
        }),
    );
    let effects = type_text(&mut state, "@");
    assert_eq!(requests(&effects).len(), 1, "stale list is refetched");
    assert!(
        state.mention_suggestions.is_some(),
        "old list still offered meanwhile"
    );
}

#[test]
fn recalled_prompt_with_a_mention_does_not_open_the_popup() {
    let mut state = AppState::new();
    state.remember_prompt("olhe @src/lib.rs");
    reduce(&mut state, Action::HistoryRecall { older: true });
    assert_eq!(state.composer.payload(), "olhe @src/lib.rs");
    assert!(state.mention_suggestions.is_none());
}
