use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{reduce, Action, Effect};
use crate::api::{SessionListItem, TurnListItem, UiCommand, UiEvent};
use crate::app::AppState;
use crate::session_picker::PickerKind;

fn press(state: &mut AppState, code: KeyCode) -> Vec<Effect> {
    reduce(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn submit(state: &mut AppState, text: &str) -> Vec<Effect> {
    state.composer.clear();
    state.composer.insert_text(text);
    press(state, KeyCode::Enter)
}

fn session(id: &str, title: Option<&str>, prompt: &str, updated_ms: u64) -> SessionListItem {
    SessionListItem {
        id: id.into(),
        title: title.map(str::to_owned),
        first_prompt: prompt.into(),
        updated_ms,
        bytes: 4096,
        in_use: false,
        current: false,
    }
}

fn turns(count: usize) -> Vec<TurnListItem> {
    (0..count)
        .map(|index| TurnListItem {
            index,
            first_seq: index as u64 * 10 + 2,
            prompt: format!("prompt {index}"),
        })
        .collect()
}

fn list_request(effects: &[Effect]) -> Option<u64> {
    effects.iter().find_map(|effect| match effect {
        Effect::Send(UiCommand::ListSessions { request_id })
        | Effect::Send(UiCommand::ListTurns { request_id }) => Some(*request_id),
        _ => None,
    })
}

fn open_resume(items: Vec<SessionListItem>) -> AppState {
    let mut state = AppState::new();
    let effects = submit(&mut state, "/resume");
    let request_id = list_request(&effects).expect("list request");
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionsListed {
            request_id,
            now_ms: 1_000_000,
            items,
            error: None,
        }),
    );
    state
}

fn open_rewind(count: usize) -> AppState {
    let mut state = AppState::new();
    let effects = submit(&mut state, "/rewind");
    let request_id = list_request(&effects).expect("list request");
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::TurnsListed {
            request_id,
            items: turns(count),
            error: None,
        }),
    );
    state
}

#[test]
fn resume_opens_a_loading_list_and_asks_the_host() {
    let mut state = AppState::new();
    let effects = submit(&mut state, "/resume");
    assert!(list_request(&effects).is_some());
    let picker = state.session_picker.as_ref().expect("picker");
    assert_eq!(picker.kind, PickerKind::Resume);
    assert!(picker.loading);
    assert!(state.composer.is_empty(), "the command leaves the composer");
}

#[test]
fn list_arrival_fills_the_picker_and_stale_answers_are_ignored() {
    let mut state = AppState::new();
    let effects = submit(&mut state, "/resume");
    let request_id = list_request(&effects).unwrap();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionsListed {
            request_id: request_id + 5,
            now_ms: 1,
            items: vec![session("tui-x", None, "stale", 1)],
            error: None,
        }),
    );
    assert!(state.session_picker.as_ref().unwrap().loading);
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionsListed {
            request_id,
            now_ms: 10,
            items: vec![session("tui-a", None, "hello", 1)],
            error: None,
        }),
    );
    let picker = state.session_picker.as_ref().unwrap();
    assert!(!picker.loading);
    assert_eq!(picker.sessions.len(), 1);
}

#[test]
fn a_list_error_is_shown_in_place() {
    let mut state = AppState::new();
    let effects = submit(&mut state, "/resume");
    let request_id = list_request(&effects).unwrap();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionsListed {
            request_id,
            now_ms: 1,
            items: Vec::new(),
            error: Some("sem acesso".into()),
        }),
    );
    let picker = state.session_picker.as_ref().unwrap();
    assert_eq!(picker.error.as_deref(), Some("sem acesso"));
    assert!(
        press(&mut state, KeyCode::Enter).is_empty(),
        "nothing to act on"
    );
}

#[test]
fn enter_resumes_the_selected_session_by_id_and_closes() {
    let mut state = open_resume(vec![
        session("tui-old", Some("Antiga"), "primeira", 10),
        session("tui-new", None, "segunda", 900),
    ]);
    // Newest first: the initial selection is the newest resumable one.
    let effects = press(&mut state, KeyCode::Enter);
    assert!(effects.contains(&Effect::Send(UiCommand::ResumeSession {
        id: "tui-new".into()
    })));
    assert!(state.session_picker.is_none());

    let mut state = open_resume(vec![
        session("tui-old", Some("Antiga"), "primeira", 10),
        session("tui-new", None, "segunda", 900),
    ]);
    press(&mut state, KeyCode::Down);
    let effects = press(&mut state, KeyCode::Enter);
    assert!(effects.contains(&Effect::Send(UiCommand::ResumeSession {
        id: "tui-old".into()
    })));
}

#[test]
fn the_open_and_the_locked_sessions_are_refused_without_closing() {
    let mut open = session("tui-open", None, "atual", 900);
    open.current = true;
    let mut busy = session("tui-busy", None, "ocupada", 800);
    busy.in_use = true;
    let mut state = open_resume(vec![open, busy, session("tui-ok", None, "livre", 100)]);
    assert_eq!(state.session_picker.as_ref().unwrap().selected, 2);
    press(&mut state, KeyCode::Home);
    let effects = press(&mut state, KeyCode::Enter);
    assert!(state.session_picker.is_some());
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::ResumeSession { .. }))));
    press(&mut state, KeyCode::Down);
    let effects = press(&mut state, KeyCode::Enter);
    assert!(state.session_picker.is_some());
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::ResumeSession { .. }))));
}

#[test]
fn typing_filters_backspace_widens_and_esc_closes_without_a_command() {
    let mut state = open_resume(vec![
        session("tui-1", Some("Login"), "corrigir token", 3),
        session("tui-2", None, "escrever docs", 2),
    ]);
    for character in "docs".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
    assert_eq!(state.session_picker.as_ref().unwrap().rows().len(), 1);
    for _ in 0..4 {
        press(&mut state, KeyCode::Backspace);
    }
    assert_eq!(state.session_picker.as_ref().unwrap().rows().len(), 2);
    let effects = press(&mut state, KeyCode::Esc);
    assert!(state.session_picker.is_none());
    assert!(effects
        .iter()
        .all(|effect| !matches!(effect, Effect::Send(_))));
}

#[test]
fn paste_never_reaches_the_composer_under_the_picker() {
    let mut state = open_resume(vec![session("tui-1", None, "x", 1)]);
    reduce(&mut state, Action::Paste("segredo".into()));
    assert!(state.composer.is_empty());
}

#[test]
fn resume_and_rewind_wait_for_an_idle_worker() {
    for command in ["/resume", "/rewind", "/rename novo"] {
        let mut state = AppState::new();
        state.working = true;
        let effects = submit(&mut state, command);
        assert!(state.session_picker.is_none(), "{command}");
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                Effect::Send(
                    UiCommand::ListSessions { .. }
                        | UiCommand::ListTurns { .. }
                        | UiCommand::RenameSession { .. }
                )
            )),
            "{command}"
        );
        assert_eq!(state.composer.payload(), command, "the draft is kept");
    }
}

#[test]
fn rewind_lists_newest_first_and_enter_sends_the_turns_sequence() {
    let mut state = open_rewind(3);
    assert_eq!(
        state.session_picker.as_ref().unwrap().kind,
        PickerKind::Rewind
    );
    let effects = press(&mut state, KeyCode::Enter);
    assert!(effects.contains(&Effect::Send(UiCommand::RewindSession { first_seq: 22 })));
    assert!(state.session_picker.is_none());

    let mut state = open_rewind(3);
    press(&mut state, KeyCode::End);
    let effects = press(&mut state, KeyCode::Enter);
    assert!(effects.contains(&Effect::Send(UiCommand::RewindSession { first_seq: 2 })));
}

#[test]
fn rename_sends_the_title_and_clear_sends_an_empty_one() {
    let mut state = AppState::new();
    let effects = submit(&mut state, "/rename  Refatorar login ");
    assert!(effects.contains(&Effect::Send(UiCommand::RenameSession {
        title: "Refatorar login".into()
    })));
    assert!(state.composer.is_empty());
    let effects = submit(&mut state, "/rename --clear");
    assert!(effects.contains(&Effect::Send(UiCommand::RenameSession {
        title: String::new()
    })));
}

#[test]
fn bare_rename_shows_usage_and_the_current_name_without_asking_the_host() {
    let mut state = AppState::new();
    let effects = submit(&mut state, "/rename");
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::RenameSession { .. }))));
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionTitleChanged {
            title: Some("Meu trabalho".into()),
        }),
    );
    submit(&mut state, "/rename");
    let latest = state
        .visible_notifications()
        .last()
        .map(|notification| notification.message.clone())
        .unwrap_or_default();
    assert!(
        latest.contains("Meu trabalho") && latest.contains("/rename TÍTULO"),
        "{latest}"
    );
}

#[test]
fn title_event_updates_and_a_restored_session_forgets_the_old_name() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionTitleChanged {
            title: Some("Um".into()),
        }),
    );
    assert_eq!(state.session_title.as_deref(), Some("Um"));
    state.apply_event(UiEvent::SessionRestored {
        session_id: crate::api::SessionId("tui-2".into()),
        cwd: "C:\\proj".into(),
        messages: Vec::new(),
        skill_names: Vec::new(),
    });
    assert_eq!(state.session_title, None, "the host re-sends the new name");
    state.apply_event(UiEvent::SessionTitleChanged {
        title: Some("  ".into()),
    });
    assert_eq!(state.session_title, None, "blank names are no names");
}

#[test]
fn a_restored_session_closes_an_open_picker() {
    let mut state = open_resume(vec![session("tui-1", None, "x", 1)]);
    state.apply_event(UiEvent::SessionRestored {
        session_id: crate::api::SessionId("tui-1".into()),
        cwd: "C:\\proj".into(),
        messages: Vec::new(),
        skill_names: Vec::new(),
    });
    assert!(state.session_picker.is_none());
}
