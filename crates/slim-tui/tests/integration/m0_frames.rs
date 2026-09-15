use std::sync::Arc;

use slim_core::OperatingMode;
use slim_tui::api::{LoginProvider, SessionId, UiEvent};
use slim_tui::app::AppState;
use slim_tui::block::{BlockKind, BlockLifecycle, FoldState};
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
            skill_names: Vec::new(),
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
            "You",
            "> hello",
            "",
            "Slim",
            "world",
            "activity: Responding",
            "todo: 0/0 no active item",
            "composer: ",
            "Read-only",
            "signed out · /login",
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
            run_id: None,
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
    assert!(state.blocks().is_empty());

    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::Anthropic),
        authenticated: true,
    });
    assert!(state.login_overlay.is_none());
    let lines = ViewModel::derive(&state).lines;
    let footer = &lines[lines.len().saturating_sub(2)..];
    assert!(footer.iter().all(|line| !line.contains("signed out")));
    assert!(footer.first().is_some_and(|line| line.starts_with("Auto")));
    assert!(footer
        .iter()
        .any(|line| line.contains("GPT-5.6 Sol (high)")));
    assert!(footer.last().is_some_and(|line| line.contains("Ctrl+P")));
}

#[test]
fn login_success_dismisses_signed_out_toast_instead_of_joining_it() {
    let mut state = AppState::new();
    state.push_notification("No provider connected. Use /login.".into());
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::OpenAiCodex),
        authenticated: true,
    });
    state.apply_event(UiEvent::Notification {
        message: "Connected: OpenAI Codex — ChatGPT Plus/Pro".into(),
    });

    assert!(
        state
            .notifications
            .iter()
            .all(|message| !message.contains("No provider connected")),
        "{:?}",
        state.notifications
    );
    assert_eq!(
        state.notifications.last().map(|notice| notice.as_str()),
        Some("Connected: OpenAI Codex — ChatGPT Plus/Pro")
    );
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

#[test]
fn compaction_completed_becomes_collapsed_system_block() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::CompactionCompleted);
    assert!(state.notifications.is_empty());
    assert_eq!(state.blocks().len(), 1);
    assert!(matches!(
        state.blocks()[0].kind(),
        BlockKind::System(text) if text == "compaction completed"
    ));
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Complete);
    assert_eq!(state.blocks()[0].fold, FoldState::Collapsed);
}
