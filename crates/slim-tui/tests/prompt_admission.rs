use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use slim_tui::api::{PromptAdmission, PromptOrigin, UiCommand, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action, Effect};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn prepared_prompt(effects: &[Effect]) -> Option<(&str, PromptAdmission)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Send(UiCommand::PreparePrompt { prompt, admission }) => {
            Some((prompt.as_str(), *admission))
        }
        _ => None,
    })
}

#[test]
fn direct_submission_installs_matching_preparation_in_the_same_reduction() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("direct prompt");

    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));

    let preparation = state
        .prompt_preparation()
        .expect("admission is installed synchronously");
    let (prompt, admission) = prepared_prompt(&effects).expect("prepare command emitted");
    assert_eq!(prompt, "direct prompt");
    assert_eq!(preparation.prompt, "direct prompt");
    assert_eq!(preparation.admission, admission);
    assert_eq!(admission.origin, PromptOrigin::Direct);
}

#[test]
fn a_second_submission_while_preparing_is_queued_without_replacing_admission() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("first");
    let first_effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    let (_, first_admission) = prepared_prompt(&first_effects).expect("first prepare");

    state.composer.insert_text("second");
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));

    assert!(prepared_prompt(&effects).is_none());
    assert_eq!(
        state.prompt_preparation().unwrap().admission,
        first_admission
    );
    assert_eq!(state.prompt_preparation().unwrap().prompt, "first");
    assert_eq!(state.queue_len(), 1);
    assert_eq!(state.queued_prompt(0), Some("second"));
}

#[test]
fn escape_cancels_the_matching_preparation() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("cancel me");
    let first_effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    let (_, admission) = prepared_prompt(&first_effects).expect("prepare command");

    let effects = reduce(&mut state, Action::Key(key(KeyCode::Esc)));

    assert!(
        effects.contains(&Effect::Send(UiCommand::CancelPromptPreparation {
            admission,
        }))
    );
    assert!(state.prompt_preparation().is_none());
}

#[test]
fn equal_queued_texts_are_separate_admissions_with_distinct_ids() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));

    for _ in 0..2 {
        state.composer.insert_text("same text");
        let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
        assert!(prepared_prompt(&effects).is_none());
    }
    assert_eq!(state.queue_len(), 2);
    assert_eq!(state.queued_prompt(0), Some("same text"));
    assert_eq!(state.queued_prompt(1), Some("same text"));

    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::RunCompleted { run_id: 1 }),
    );
    let first = state
        .prompt_preparation()
        .expect("first queued prompt is admitted");
    assert_eq!(first.prompt, "same text");
    assert_eq!(first.admission.origin, PromptOrigin::Queued);

    let first_admission = first.admission;
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::PromptRunStarted {
            admission: first_admission,
            run_id: 2,
            max_mutating_tool_calls: 8,
            max_read_tool_calls: 8,
            max_turns: 8,
        }),
    );
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::PromptRunCompleted {
            admission: first_admission,
            run_id: 2,
        }),
    );

    let second = state
        .prompt_preparation()
        .expect("second queued prompt is admitted independently");
    assert_eq!(second.prompt, "same text");
    assert_eq!(second.admission.origin, PromptOrigin::Queued);
    assert_ne!(second.admission.id, first_admission.id);
}
