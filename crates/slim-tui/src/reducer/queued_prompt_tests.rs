use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{reduce, Action, Effect};
use crate::api::{PromptAdmission, PromptOrigin, UiCommand, UiEvent};
use crate::app::{AppState, FollowMode, ScrollAnchor};
use crate::block::{consecutive_queued_user_span, BlockKind, FoldState};
use crate::render::{HeightIndex, WrapCache};
use crate::testkit::render_terminal_text;

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
fn queued_texts_expand_together_without_changing_fifo_or_physical_rows() {
    let mut state = AppState::new();
    state.enqueue_queued_prompt("alpha\nALPHA_TAIL".into());
    state.enqueue_queued_prompt("BETA_TAIL".into());
    let leader = state.blocks()[0].id.clone();
    let second = state.blocks()[1].id.clone();

    let collapsed = render_terminal_text(&state, 80, 24);
    assert!(collapsed.contains("2 na fila · alpha"), "{collapsed}");
    assert!(
        !collapsed.contains("pendentes"),
        "the count lives on the queue row only
{collapsed}"
    );
    assert!(!collapsed.contains("ALPHA_TAIL"), "{collapsed}");
    assert!(!collapsed.contains("BETA_TAIL"), "{collapsed}");
    assert_eq!(
        HeightIndex::build(state.blocks(), 79, &mut WrapCache::default()).total_rows,
        1,
    );
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: second,
        row_offset: 0,
    });
    assert_eq!(
        reduce(&mut state, Action::Key(enter())),
        vec![Effect::RequestRender],
        "expansion must not prepare or execute a prompt",
    );
    assert_eq!(state.blocks()[0].fold, FoldState::Expanded);
    assert_eq!(state.queue_len(), 2);
    assert_eq!(queued_texts(&state), vec!["alpha\nALPHA_TAIL", "BETA_TAIL"]);
    let expanded = render_terminal_text(&state, 80, 24);
    let first_tail = expanded.find("ALPHA_TAIL").expect("first message body");
    let second_tail = expanded.find("BETA_TAIL").expect("second message body");
    assert!(first_tail < second_tail, "{expanded}");
    let index = HeightIndex::build(state.blocks(), 79, &mut WrapCache::default());
    assert_eq!(
        index.total_rows, 4,
        "one header plus three physical body rows"
    );
    for row in 0..index.total_rows {
        let anchor = index.anchor_for_row(row).expect("visible queue row");
        assert_eq!(index.row_for_anchor(&anchor), Some(row));
    }
    let (changed, command) = state.activate_block(&leader);
    assert!(changed);
    assert!(command.is_none());
    assert_eq!(state.queue_len(), 2);
    assert_eq!(
        HeightIndex::build(state.blocks(), 79, &mut WrapCache::default()).total_rows,
        1,
    );
}

#[test]
fn queue_groups_preserve_output_between_pending_messages_and_editing_indices() {
    let mut state = AppState::new();
    state.enqueue_queued_prompt("first pending".into());
    state.apply_event(UiEvent::AssistantDelta {
        text: "progress in between".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.enqueue_queued_prompt("second pending".into());
    state.enqueue_queued_prompt("third pending".into());
    assert_eq!(
        consecutive_queued_user_span(state.blocks(), 0),
        Some((0, 1))
    );
    assert_eq!(consecutive_queued_user_span(state.blocks(), 1), None);
    assert_eq!(
        consecutive_queued_user_span(state.blocks(), 3),
        Some((2, 4))
    );
    let third = state.blocks()[3].id.clone();
    let (changed, command) = state.activate_block(&third);
    assert!(changed);
    assert!(command.is_none());
    let frame = render_terminal_text(&state, 80, 24);
    let first = frame.find("first pending").expect("first pending position");
    let progress = frame.find("progress in between").expect("output position");
    let second = frame
        .find("second pending")
        .expect("second pending position");
    let third = frame.find("third pending").expect("third pending position");
    assert!(
        first < progress && progress < second && second < third,
        "{frame}"
    );
    assert_eq!(
        state.take_queued_prompt_for_edit(1),
        Some("second pending".into())
    );
    assert_eq!(queued_texts(&state), vec!["first pending", "third pending"]);
    assert_eq!(state.pop_queued_prompt(), Some("first pending".into()));
    assert_eq!(state.queued_prompt(0), Some("third pending"));
    assert_eq!(queued_texts(&state), vec!["third pending"]);
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
