use std::sync::Arc;

use slim_core::OperatingMode;
use slim_tui::api::{LoginProvider, SessionId, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::testkit::MemorySurface;
use slim_tui::view_model::ViewModel;

#[test]
fn fake_session_materializes_deterministic_frame_without_io() {
    let mut state = AppState::new();
    state.todo_dock_open = true;
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionSnapshot {
            session_id: SessionId(Arc::from("s1")),
            cwd: "D:\\Slim".into(),
        }),
    );
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::UserMessageAdded {
            text: "hello".into(),
        }),
    );
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::AssistantDelta {
            text: "world".into(),
        }),
    );
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::ModeChanged {
            mode: OperatingMode::ReadOnly,
        }),
    );

    let mut surface = MemorySurface::default();
    surface.draw(ViewModel::derive(&state));
    assert_eq!(
        surface.frames[0].lines,
        vec![
            "you",
            "> hello",
            "Slim",
            "world",
            "todo: 0/0 no active item",
            "composer: ",
            "signed out · /login  ctx 0% · 0k/128k · ↑0 ↓0",
        ]
    );
}

#[test]
fn reducer_emits_commands_and_keeps_errors_visible() {
    let mut state = AppState::new();
    // Printable key press dirties the frame without emitting commands.
    let effects = reduce(
        &mut state,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('h'),
            crossterm::event::KeyModifiers::NONE,
        )),
    );
    assert_eq!(effects, vec![Effect::RequestRender]);
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::FatalError {
            message: "provider failed".into(),
        }),
    );
    assert_eq!(ViewModel::derive(&state).lines[0], "error: provider failed");
}

#[test]
fn signed_out_login_progress_and_success_are_projected_without_history_leaks() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AuthStateChanged {
        provider: None,
        authenticated: false,
    });
    assert!(ViewModel::derive(&state)
        .lines
        .last()
        .unwrap()
        .contains("signed out"));

    state.login_overlay = Some(Default::default());
    state.login_overlay.as_mut().unwrap().in_progress = true;
    let login_url = UiEvent::LoginUrl {
        url: "http://localhost/callback?state=secret-state"
            .to_owned()
            .into(),
        user_code: Some("secret-code".to_owned().into()),
    };
    let debug = format!("{login_url:?}");
    assert!(!debug.contains("secret-state"));
    assert!(!debug.contains("secret-code"));
    state.apply_event(login_url);
    state.apply_event(UiEvent::LoginProgress {
        message: "Waiting for browser authorization…".into(),
    });
    assert!(state
        .login_overlay
        .as_ref()
        .and_then(|overlay| overlay.auth_url.as_ref())
        .is_some());
    assert!(state.blocks.is_empty());

    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::Anthropic),
        authenticated: true,
    });
    assert!(state.login_overlay.is_none());
    let footer = ViewModel::derive(&state).lines.last().unwrap().clone();
    assert!(!footer.contains("signed out"));
    assert!(footer.contains("Shift+Tab:mode"));
}

#[test]
fn restore_draft_only_replaces_empty_composer() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RestoreDraft {
        text: "retry me".into(),
    });
    assert_eq!(state.composer.payload(), "retry me");
    state.apply_event(UiEvent::RestoreDraft {
        text: "do not overwrite".into(),
    });
    assert_eq!(state.composer.payload(), "retry me");
}
