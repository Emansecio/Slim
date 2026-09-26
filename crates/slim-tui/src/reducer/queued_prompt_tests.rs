use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{reduce, Action, Effect};
use crate::api::{PromptAdmission, PromptOrigin, UiCommand, UiEvent};
use crate::app::AppState;
use crate::block::BlockKind;

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn alt_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
}

fn run_started(run_id: u64) -> Action {
    Action::UiEventReceived(UiEvent::run_started(run_id))
}

fn run_completed(run_id: u64) -> Action {
    Action::UiEventReceived(UiEvent::RunCompleted { run_id })
}

fn prompt_run_started(admission: PromptAdmission, run_id: u64) -> Action {
    Action::UiEventReceived(UiEvent::PromptRunStarted {
        admission,
        run_id,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    })
}

fn queued_texts(state: &AppState) -> Vec<String> {
    state
        .blocks()
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::QueuedUser(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn assert_prepared_prompt(
    effects: &[Effect],
    expected_prompt: &str,
    expected_origin: PromptOrigin,
) -> PromptAdmission {
    let admissions = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(UiCommand::PreparePrompt { prompt, admission })
                if prompt == expected_prompt && admission.origin == expected_origin =>
            {
                Some(*admission)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(admissions.len(), 1, "one correlated prompt preparation");
    admissions[0]
}

fn type_text(state: &mut AppState, text: &str) {
    for character in text.chars() {
        reduce(
            state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
}

#[test]
fn submit_during_run_enqueues_visible_block_and_clears_composer() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    assert!(state.working, "RunStarted must flip the run active flag");

    type_text(&mut state, "second question");
    let effects = reduce(&mut state, Action::Key(enter()));

    assert_eq!(effects, vec![Effect::RequestRender]);
    assert!(state.composer.payload().is_empty());
    assert_eq!(queued_texts(&state), vec!["second question"]);
    assert_eq!(state.queued_prompts.len(), 1);
}

#[test]
fn second_enqueue_increments_fifo_position() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    type_text(&mut state, "first");
    reduce(&mut state, Action::Key(enter()));
    type_text(&mut state, "second");
    reduce(&mut state, Action::Key(enter()));

    assert_eq!(queued_texts(&state), vec!["first", "second"]);
}

#[test]
fn ninth_queued_prompt_is_rejected_without_losing_the_draft() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    for index in 0..8 {
        type_text(&mut state, &format!("queued-{index}"));
        reduce(&mut state, Action::Key(enter()));
    }
    type_text(&mut state, "keep ninth");

    let effects = reduce(&mut state, Action::Key(enter()));

    assert_eq!(state.queued_prompts.len(), 8);
    assert_eq!(state.composer.payload(), "keep ninth");
    assert_eq!(effects, vec![Effect::RequestRender]);
    assert!(state
        .notifications
        .iter()
        .any(|notification| notification.message == "Fila de prompts cheia (8)."));
}

#[test]
fn alt_enter_during_run_enqueues_too() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    type_text(&mut state, "steered");
    let effects = reduce(&mut state, Action::Key(alt_enter()));

    assert_eq!(effects, vec![Effect::RequestRender]);
    assert_eq!(queued_texts(&state), vec!["steered"]);
}

#[test]
fn empty_draft_never_enqueues() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.is_empty());
    assert_eq!(state.queued_prompts.len(), 0);
}

#[test]
fn run_terminal_drains_exactly_one_prompt() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    type_text(&mut state, "queued prompt");
    reduce(&mut state, Action::Key(enter()));

    let effects = reduce(&mut state, run_completed(1));

    assert_prepared_prompt(&effects, "queued prompt", PromptOrigin::Queued);
    assert_eq!(state.queued_prompts.len(), 0);
    assert!(queued_texts(&state).is_empty(), "drain removes the block");
}

#[test]
fn two_queued_two_terminals_fifo_order() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    type_text(&mut state, "alpha");
    reduce(&mut state, Action::Key(enter()));
    type_text(&mut state, "beta");
    reduce(&mut state, Action::Key(enter()));

    let first = reduce(&mut state, run_completed(1));
    let alpha = assert_prepared_prompt(&first, "alpha", PromptOrigin::Queued);
    assert_eq!(state.queued_prompts.len(), 1);
    assert_eq!(queued_texts(&state), vec!["beta"]);

    reduce(&mut state, prompt_run_started(alpha, 2));
    let second = reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::PromptRunCompleted {
            admission: alpha,
            run_id: 2,
        }),
    );
    let beta = assert_prepared_prompt(&second, "beta", PromptOrigin::Queued);
    assert_ne!(alpha.id, beta.id);
    assert_ne!(alpha.generation, beta.generation);
    assert_eq!(state.queued_prompts.len(), 0);
    assert!(queued_texts(&state).is_empty());
}

#[test]
fn locally_handled_skill_selection_drains_the_next_queued_prompt() {
    let mut state = AppState::new();
    state.set_skill_names_for_test(vec!["review-code".into()]);
    reduce(&mut state, run_started(1));
    type_text(&mut state, "/review-code");
    reduce(&mut state, Action::Key(enter()));
    type_text(&mut state, "after selection");
    reduce(&mut state, Action::Key(enter()));

    let first = reduce(&mut state, run_completed(1));
    let admission = assert_prepared_prompt(&first, "/review-code ", PromptOrigin::Queued);
    assert_eq!(state.queued_prompts.len(), 1);

    let second = reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::PromptPreparationHandled {
            admission,
            restore_draft: Some("/review-code ".into()),
        }),
    );
    assert_prepared_prompt(&second, "after selection", PromptOrigin::Queued);
    assert_eq!(state.composer.payload(), "/review-code ");
    assert!(state.queued_prompts.is_empty());
}

#[test]
fn non_terminal_event_does_not_drain() {
    let mut state = AppState::new();
    reduce(&mut state, run_started(1));
    type_text(&mut state, "kept while running");
    reduce(&mut state, Action::Key(enter()));
    assert_eq!(state.queued_prompts.len(), 1);

    // A non-terminal event (tick) must not drain the queue.
    reduce(&mut state, Action::Tick(Default::default()));
    assert_eq!(state.queued_prompts.len(), 1);
    assert_eq!(queued_texts(&state).len(), 1);
}
