use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use super::reduce;
use crate::api::UiEvent;
use crate::app::{AppState, FollowMode};
use crate::render::WrapCache;
use crate::runtime::terminal_action;

fn connected() -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state
}

/// Sends a key through the same path as the terminal: the runtime decides
/// whether it scrolls, recalls or edits, then the reducer applies it.
fn press(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) {
    let mut cache = WrapCache::default();
    if let Some(action) = terminal_action(
        Event::Key(KeyEvent::new(code, modifiers)),
        state,
        (100, 30),
        &mut cache,
    ) {
        reduce(state, action);
    }
}

fn key(state: &mut AppState, code: KeyCode) {
    press(state, code, KeyModifiers::NONE);
}

fn type_text(state: &mut AppState, text: &str) {
    for character in text.chars() {
        key(state, KeyCode::Char(character));
    }
}

/// Sends `prompt`. With the host absent the state keeps a preparation open,
/// so later prompts are queued, which remembers them just the same.
fn send(state: &mut AppState, prompt: &str) {
    type_text(state, prompt);
    key(state, KeyCode::Enter);
}

fn draft(state: &AppState) -> String {
    state.composer.payload()
}

#[test]
fn up_and_down_walk_the_sent_prompts_and_return_the_draft() {
    let mut state = connected();
    send(&mut state, "primeiro");
    send(&mut state, "segundo");
    send(&mut state, "terceiro");
    assert_eq!(draft(&state), "");

    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "terceiro");
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "segundo");
    key(&mut state, KeyCode::Up);
    key(&mut state, KeyCode::Up);
    assert_eq!(
        draft(&state),
        "primeiro",
        "the oldest one is the end of the list"
    );

    key(&mut state, KeyCode::Down);
    assert_eq!(draft(&state), "segundo");
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Down);
    assert_eq!(
        draft(&state),
        "",
        "past the newest, the empty draft is back"
    );
    // Nothing is being recalled any more: Down does nothing to the composer.
    key(&mut state, KeyCode::Down);
    assert_eq!(draft(&state), "");
}

#[test]
fn consecutive_repeats_collapse_and_blank_prompts_are_ignored() {
    let mut state = connected();
    send(&mut state, "mesmo");
    send(&mut state, "mesmo");
    // Whitespace alone is not sent: the draft stays, so clear it to recall.
    type_text(&mut state, "   ");
    key(&mut state, KeyCode::Enter);
    for _ in 0..3 {
        key(&mut state, KeyCode::Backspace);
    }
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "mesmo");
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "mesmo", "one entry only");
}

#[test]
fn editing_a_recalled_prompt_ends_recall_and_keeps_the_text() {
    let mut state = connected();
    send(&mut state, "ajuste o cache");
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "ajuste o cache");
    type_text(&mut state, "!");
    assert_eq!(draft(&state), "ajuste o cache!");
    // Recall is over: Up now belongs to the transcript again, not the composer.
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "ajuste o cache!");
}

#[test]
fn a_draft_typed_before_recall_comes_back_at_the_end() {
    let mut state = connected();
    send(&mut state, "antigo");
    // A non-empty composer never starts recall: Up keeps its old job there.
    type_text(&mut state, "rascunho");
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "rascunho");
    // Empty it and return to the live edge (Up above pinned the view), then
    // recall: the (empty) draft is what returns.
    for _ in 0.."rascunho".len() {
        key(&mut state, KeyCode::Backspace);
    }
    key(&mut state, KeyCode::End);
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "antigo");
    key(&mut state, KeyCode::Down);
    assert_eq!(draft(&state), "");
}

#[test]
fn recall_only_starts_at_the_live_edge_and_pinned_navigation_keeps_working() {
    let mut state = connected();
    send(&mut state, "prompt");
    // A pinned view is an explicit reading position; Up moves through blocks.
    state.scroll.mode = FollowMode::Top;
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "", "no recall while the view is pinned");
    // A view at the live edge recalls.
    state.scroll.mode = FollowMode::default();
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "prompt");
}

#[test]
fn without_history_up_still_scrolls() {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "linha\n".repeat(80),
    });
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "");
    assert!(state.scroll.is_pinned(), "Up scrolled the transcript");
}

#[test]
fn a_restored_session_seeds_the_history_with_its_own_prompts() {
    use crate::api::{SessionId, TranscriptMessage, TranscriptRole};
    let mut state = connected();
    send(&mut state, "de outra sessão");
    state.apply_event(UiEvent::SessionRestored {
        session_id: SessionId("s".into()),
        cwd: r"C:\projeto".into(),
        messages: vec![
            TranscriptMessage {
                role: TranscriptRole::User,
                text: "primeira da sessão".into(),
            },
            TranscriptMessage {
                role: TranscriptRole::Assistant,
                text: "resposta".into(),
            },
            TranscriptMessage {
                role: TranscriptRole::User,
                text: "segunda da sessão".into(),
            },
        ],
        skill_names: Vec::new(),
    });
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "segunda da sessão");
    key(&mut state, KeyCode::Up);
    assert_eq!(draft(&state), "primeira da sessão");
    key(&mut state, KeyCode::Up);
    assert_eq!(
        draft(&state),
        "primeira da sessão",
        "the other session's prompt is gone"
    );
}

#[test]
fn history_keeps_only_the_newest_hundred() {
    let mut state = connected();
    for index in 0..105 {
        state.remember_prompt(&format!("prompt {index}"));
    }
    for _ in 0..100 {
        key(&mut state, KeyCode::Up);
    }
    assert_eq!(draft(&state), "prompt 5", "the five oldest were dropped");
}
