use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use slim_core::OperatingMode;

use super::{reduce, Action, Effect};
use crate::api::{UiCommand, UiEvent};
use crate::app::AppState;

fn ready(mode: OperatingMode) -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state.mode = mode;
    state
}

fn submit(state: &mut AppState, text: &str) -> Vec<Effect> {
    state.composer.clear();
    state.composer.insert_text(text);
    reduce(
        state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    )
}

fn shell_commands(effects: &[Effect]) -> Vec<(u64, &str)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(UiCommand::RunUserShell {
                request_id,
                command,
            }) => Some((*request_id, command.as_str())),
            _ => None,
        })
        .collect()
}

fn sends_prompt(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::PreparePrompt { .. })))
}

#[test]
fn bang_command_runs_in_auto_mode_and_clears_the_draft() {
    let mut state = ready(OperatingMode::Auto);
    let effects = submit(&mut state, "!  cargo test -q ");
    assert_eq!(shell_commands(&effects), [(1, "cargo test -q")]);
    assert_eq!(state.user_shell, Some(1));
    assert!(state.composer.is_empty());
    assert!(!sends_prompt(&effects), "a ! command is never a model turn");
}

#[test]
fn bang_command_does_not_need_a_connected_provider() {
    let mut state = ready(OperatingMode::Auto);
    state.authenticated = false;
    let effects = submit(&mut state, "!dir");
    assert_eq!(shell_commands(&effects).len(), 1);
    assert!(state.composer.is_empty());
}

#[test]
fn other_modes_refuse_locally_and_keep_the_draft() {
    for mode in [OperatingMode::ReadOnly, OperatingMode::Plan] {
        let mut state = ready(mode);
        let effects = submit(&mut state, "!rm -rf build");
        assert!(shell_commands(&effects).is_empty(), "{mode:?}");
        assert_eq!(state.composer.payload(), "!rm -rf build");
        assert!(state.user_shell.is_none());
    }
}

#[test]
fn double_bang_is_a_literal_prompt_and_bare_bang_shows_usage() {
    let mut state = ready(OperatingMode::Auto);
    let effects = submit(&mut state, "!!importante");
    assert!(shell_commands(&effects).is_empty());
    assert!(sends_prompt(&effects));

    let mut state = ready(OperatingMode::Auto);
    let effects = submit(&mut state, "!   ");
    assert!(shell_commands(&effects).is_empty());
    assert_eq!(state.composer.payload(), "!   ", "usage keeps the draft");
}

#[test]
fn only_one_command_at_a_time_and_prompts_wait_for_it() {
    let mut state = ready(OperatingMode::Auto);
    submit(&mut state, "!sleep 5");
    let second = submit(&mut state, "!dir");
    assert!(shell_commands(&second).is_empty());
    assert_eq!(state.composer.payload(), "!dir");
    let prompt = submit(&mut state, "explique o erro");
    assert!(!sends_prompt(&prompt));
    assert_eq!(state.composer.payload(), "explique o erro");
}

#[test]
fn esc_cancels_the_running_command_and_finish_releases_the_input() {
    let mut state = ready(OperatingMode::Auto);
    submit(&mut state, "!sleep 5");
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::CancelRun))));
    assert_eq!(
        state.user_shell,
        Some(1),
        "stays busy until the host confirms"
    );
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::UserShellFinished { request_id: 99 }),
    );
    assert_eq!(state.user_shell, Some(1), "a stale request id is ignored");
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::UserShellFinished { request_id: 1 }),
    );
    assert!(state.user_shell.is_none());
    let effects = submit(&mut state, "!dir");
    assert_eq!(shell_commands(&effects), [(2, "dir")], "ids keep counting");
}

#[test]
fn bang_command_is_never_queued_behind_a_running_turn() {
    let mut state = ready(OperatingMode::Auto);
    state.working = true;
    let effects = submit(&mut state, "!dir");
    assert!(shell_commands(&effects).is_empty());
    assert_eq!(state.queue_len(), 0);
    assert_eq!(state.composer.payload(), "!dir");
}
