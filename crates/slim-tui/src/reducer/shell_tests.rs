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

#[test]
fn background_bang_keeps_composer_available_even_during_a_run() {
    for busy in [false, true] {
        let mut state = ready(OperatingMode::Auto);
        state.working = busy;
        let effects = submit(&mut state, "!& echo background");
        assert!(effects.iter().any(|e| matches!(e, Effect::Send(UiCommand::RunBackgroundShell { command }) if command == "echo background")));
        assert!(state.user_shell.is_none());
        assert!(state.composer.is_empty());
        assert_eq!(state.queue_len(), 0);
    }
}

#[test]
fn jobs_overlay_captures_keys_and_preserves_draft_and_exit_warning() {
    let mut state = ready(OperatingMode::Auto);
    state.working = true;
    let effects = submit(&mut state, "/jobs");
    assert!(effects.contains(&Effect::Send(UiCommand::JobsRefresh)));
    state.composer.insert_text("draft");
    reduce(&mut state, Action::Paste("must not enter".into()));
    assert_eq!(state.composer.payload(), "draft");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.jobs_overlay.is_none());
    assert!(state.working, "Esc closes overlay before cancelling run");
    state.working = false;
    state.jobs.push(slim_core::runtime::ShellJobInfo {
        id: "shell-1".into(),
        command: "echo".into(),
        origin: "user".into(),
        state: "running".into(),
        elapsed_ms: 0,
        exit_code: None,
        output_bytes: 0,
    });
    let effects = reduce(&mut state, Action::RequestShutdown);
    assert!(!state.shutdown);
    assert!(!effects.contains(&Effect::Send(UiCommand::Shutdown)));
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.job_exit_confirm.is_none());
}

#[test]
fn jobs_detail_discards_previous_output_pages_and_palette_resume_requires_confirmation() {
    let mut state = ready(OperatingMode::Auto);
    for (id, phase) in [("shell-1", "completed"), ("shell-2", "running")] {
        state.jobs.push(slim_core::runtime::ShellJobInfo {
            id: id.into(),
            command: "echo".into(),
            origin: "user".into(),
            state: phase.into(),
            elapsed_ms: 0,
            exit_code: None,
            output_bytes: 100000,
        });
    }
    submit(&mut state, "/jobs");
    let key = |state: &mut AppState, code| {
        reduce(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    };
    key(&mut state, KeyCode::Enter);
    state.apply_event(UiEvent::JobOutput {
        id: "shell-1".into(),
        offset: None,
        before: None,
        output: Box::new(slim_core::runtime::ShellJobOutput {
            text: "OLD".into(),
            start_offset: 90000,
            next_offset: 100000,
            output_bytes: 100000,
            truncated: true,
            log_path: None,
        }),
    });
    let effects = key(&mut state, KeyCode::PageUp);
    assert!(effects.contains(&Effect::Send(UiCommand::JobOutput {
        id: "shell-1".into(),
        offset: None,
        before: Some(90000)
    })));
    key(&mut state, KeyCode::Esc);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);
    assert!(state.jobs_overlay.as_ref().unwrap().output.is_empty());
    key(&mut state, KeyCode::Esc);
    key(&mut state, KeyCode::Esc);
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
    );
    state.palette_query = Some("/resume".into());
    let effects = key(&mut state, KeyCode::Enter);
    assert!(state.job_exit_confirm.is_some());
    assert!(!effects.contains(&Effect::Send(UiCommand::ResumePrevious)));
    let effects = key(&mut state, KeyCode::Enter);
    assert!(effects.contains(&Effect::Send(UiCommand::ResumePrevious)));
}

#[test]
fn jobs_eviction_clears_the_detail_identity_and_live_elapsed_requests_are_bounded() {
    let mut state = ready(OperatingMode::Auto);
    state.jobs.push(slim_core::runtime::ShellJobInfo {
        id: "shell-1".into(),
        command: "echo".into(),
        origin: "user".into(),
        state: "completed".into(),
        elapsed_ms: 0,
        exit_code: Some(0),
        output_bytes: 12,
    });
    submit(&mut state, "/jobs");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    state.jobs_overlay.as_mut().unwrap().output = "OLD".into();
    state.jobs_overlay.as_mut().unwrap().scroll.top();
    let mut new = state.jobs[0].clone();
    new.id = "shell-2".into();
    new.state = "running".into();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::JobsChanged { jobs: vec![new] }),
    );
    assert!(!state.jobs_overlay.as_ref().unwrap().detail);
    assert!(state.jobs_overlay.as_ref().unwrap().output.is_empty());
    let clock = state.clock;
    assert!(reduce(&mut state, Action::StatusTick(clock))
        .contains(&Effect::Send(UiCommand::JobsRefresh)));
    assert!(
        !reduce(&mut state, Action::Tick(clock)).contains(&Effect::Send(UiCommand::JobsRefresh))
    );
}

#[test]
fn restoring_another_session_clears_the_previous_job_output_and_focus() {
    let mut state = ready(OperatingMode::Auto);
    submit(&mut state, "/jobs");
    state.jobs_overlay.as_mut().unwrap().output = "previous session output".into();
    state.apply_event(UiEvent::SessionRestored {
        session_id: crate::api::SessionId("new".into()),
        cwd: "workspace".into(),
        messages: vec![],
        skill_names: vec![],
    });
    assert!(state.jobs_overlay.is_none());
    assert!(state.jobs.is_empty());
}
