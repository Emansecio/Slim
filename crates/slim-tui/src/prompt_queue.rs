use crate::{
    api::{PromptAdmission, UiCommand, UiEvent},
    app::AppState,
    reducer::{reduce, Action, Effect},
};
use slim_core::session::PromptQueueJournal;
use std::io;

#[derive(Default)]
pub(crate) struct PersistentQueue {
    identity: Option<(String, std::sync::Arc<str>)>,
    journal: Option<PromptQueueJournal>,
    active: Option<ActivePrompt>,
    initial_prompt: Option<String>,
}

#[derive(Clone, Debug)]
struct ActivePrompt {
    admission: Option<PromptAdmission>,
    text: String,
    run_id: Option<u64>,
}

impl PersistentQueue {
    pub fn with_initial_prompt(prompt: Option<String>) -> Self {
        Self {
            initial_prompt: prompt,
            ..Self::default()
        }
    }

    pub fn reduce(&mut self, state: &mut AppState, action: Action) -> io::Result<Vec<Effect>> {
        let session_restored = matches!(
            &action,
            Action::UiEventReceived(UiEvent::SessionRestored { .. })
        );
        let explicit_edit = matches!(
            &action,
            Action::Key(_)
                | Action::Paste(_)
                | Action::ClipboardPull { .. }
                | Action::SubmitInitialPrompt(_)
        );
        let composer_before = state.composer.payload();
        let prompt_event = match &action {
            Action::UiEventReceived(event) => Some(event.clone()),
            _ => None,
        };
        let was_working = state.working;
        let mut effects = reduce(state, action);
        let Some(session) = &state.session_id else {
            return Ok(effects);
        };
        let identity = (state.cwd.clone(), session.0.clone());
        let restored = self.identity.as_ref() != Some(&identity) || session_restored;
        if restored {
            self.journal = None;
            self.active = None;
            let journal = PromptQueueJournal::open(std::path::Path::new(&identity.0), &identity.1)?;
            let snapshot = journal.snapshot().clone();
            let active_preparation = state.current_prompt_preparation().cloned();
            let mut recovery_draft = snapshot.recovery_draft.clone();
            let mut recovered_queue = Vec::new();
            if let Some(prompt) = snapshot.in_flight.clone() {
                recovered_queue.push(prompt);
                state.apply_event(UiEvent::Notification { message:
                    "Fila recuperada: uma execução ficou sem confirmação. Confira os efeitos antes de repetir o primeiro item.".into() });
            }
            if let Some(draft) = recovery_draft.take() {
                if state.composer.is_empty() {
                    state.composer.insert_text(draft.clone());
                    recovery_draft = Some(draft);
                } else {
                    recovered_queue.push(draft);
                }
            }
            recovered_queue.extend(snapshot.pending.iter().cloned());
            for prompt in recovered_queue.into_iter().rev() {
                state.enqueue_queued_prompt_front(prompt);
            }
            if !snapshot.pending.is_empty()
                || snapshot.in_flight.is_some()
                || snapshot.recovery_draft.is_some() && recovery_draft.is_none()
            {
                state.queue_paused = true;
            }
            if let Some(preparation) = active_preparation {
                self.active = Some(ActivePrompt {
                    admission: Some(preparation.admission),
                    text: preparation.prompt.clone(),
                    run_id: None,
                });
            }
            self.journal = Some(journal);
            self.identity = Some(identity);
        }

        if self.initial_prompt.is_some()
            && matches!(
                prompt_event.as_ref(),
                Some(UiEvent::SessionSnapshot { .. } | UiEvent::SessionRestored { .. })
            )
        {
            let prompt = self.initial_prompt.take().expect("initial prompt present");
            effects.extend(reduce(state, Action::SubmitInitialPrompt(prompt)));
        }

        let journal = self.journal.as_mut().expect("queue opened");

        let mut snapshot = journal.snapshot().clone();
        if restored {
            // Ambiguous work is paused and surfaced as queued; only a currently
            // admitted in-process prompt remains in-flight.
            snapshot.in_flight = self.active.as_ref().map(|active| active.text.clone());
            snapshot.pending = state.queued_prompts.iter().cloned().collect();
            snapshot.recovery_draft = snapshot
                .recovery_draft
                .take()
                .filter(|draft| state.composer.payload() == *draft);
        }

        if explicit_edit
            && snapshot.recovery_draft.as_ref().is_some_and(|draft| {
                composer_before == *draft && state.composer.payload() != *draft
            })
        {
            snapshot.recovery_draft = None;
        }

        for effect in &effects {
            match effect {
                Effect::Send(UiCommand::PreparePrompt { prompt, admission }) => {
                    self.active = Some(ActivePrompt {
                        admission: Some(*admission),
                        text: prompt.clone(),
                        run_id: None,
                    });
                    snapshot.in_flight = Some(prompt.clone());
                    snapshot.recovery_draft = None;
                }
                Effect::Send(UiCommand::SendPrompt(prompt)) => {
                    self.active = Some(ActivePrompt {
                        admission: None,
                        text: prompt.clone(),
                        run_id: None,
                    });
                    snapshot.in_flight = Some(prompt.clone());
                    snapshot.recovery_draft = None;
                }
                _ => {}
            }
        }

        match prompt_event.as_ref() {
            Some(UiEvent::PromptPreparationCancelled { admission })
            | Some(UiEvent::PromptPreparationFailed { admission, .. }) => {
                if let Some(active) = self
                    .active
                    .as_ref()
                    .filter(|active| active.admission == Some(*admission))
                    .cloned()
                {
                    snapshot.in_flight = None;
                    self.active = None;
                    if admission.origin == crate::api::PromptOrigin::Direct
                        && composer_before.is_empty()
                        && state.composer.payload() == active.text
                    {
                        snapshot.recovery_draft = Some(active.text);
                    } else {
                        snapshot.recovery_draft = None;
                    }
                }
            }
            Some(UiEvent::PromptPreparationHandled { admission, .. }) => {
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.admission == Some(*admission))
                {
                    self.active = None;
                    snapshot.in_flight = None;
                    snapshot.recovery_draft = None;
                }
            }
            Some(UiEvent::PromptRunStarted {
                admission, run_id, ..
            }) => {
                if let Some(active) = self
                    .active
                    .as_mut()
                    .filter(|active| active.admission == Some(*admission))
                {
                    active.run_id = Some(*run_id);
                }
            }
            Some(
                UiEvent::PromptRunCompleted { admission, run_id }
                | UiEvent::PromptRunStopped {
                    admission, run_id, ..
                }
                | UiEvent::PromptRunCancelled { admission, run_id },
            ) => {
                if self.active.as_ref().is_some_and(|active| {
                    active.admission == Some(*admission)
                        && active
                            .run_id
                            .is_none_or(|active_run_id| active_run_id == *run_id)
                }) {
                    self.active = None;
                    snapshot.in_flight = None;
                    snapshot.recovery_draft = None;
                }
            }
            Some(UiEvent::PromptRunFailed {
                admission,
                run_id: Some(run_id),
                ..
            }) => {
                if self.active.as_ref().is_some_and(|active| {
                    active.admission == Some(*admission)
                        && active
                            .run_id
                            .is_none_or(|active_run_id| active_run_id == *run_id)
                }) {
                    self.active = None;
                    snapshot.in_flight = None;
                    snapshot.recovery_draft = None;
                }
            }
            Some(UiEvent::RunStarted { run_id, .. }) => {
                if let Some(active) = self
                    .active
                    .as_mut()
                    .filter(|active| active.admission.is_none())
                {
                    active.run_id = Some(*run_id);
                }
            }
            Some(
                UiEvent::RunCompleted { run_id }
                | UiEvent::RunStopped { run_id, .. }
                | UiEvent::RunCancelled { run_id },
            ) => {
                if self.active.as_ref().is_some_and(|active| {
                    active.admission.is_none() && active.run_id == Some(*run_id)
                }) {
                    self.active = None;
                    snapshot.in_flight = None;
                    snapshot.recovery_draft = None;
                }
            }
            Some(UiEvent::RunFailed {
                run_id: Some(run_id),
                ..
            }) => {
                if self.active.as_ref().is_some_and(|active| {
                    active.admission.is_none() && active.run_id == Some(*run_id)
                }) {
                    self.active = None;
                    snapshot.in_flight = None;
                    snapshot.recovery_draft = None;
                }
            }
            _ => {}
        }

        // A legacy RunStarted can arrive on a later event after the command was
        // accepted. Its matching terminal is the only event that clears it.
        if was_working && !state.working && self.active.is_some() && prompt_event.is_none() {
            snapshot.in_flight = journal.snapshot().in_flight.clone();
        }
        snapshot.pending = state.queued_prompts.iter().cloned().collect();
        journal.save(snapshot)?;
        Ok(effects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PromptAdmission, SessionId};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use slim_core::session::PromptQueueSnapshot;

    fn temp_workspace(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "slim-queue-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn session_snapshot(root: &std::path::Path, session: &str) -> Action {
        Action::UiEventReceived(UiEvent::SessionSnapshot {
            session_id: SessionId(session.into()),
            cwd: root.to_string_lossy().into(),
            skill_names: vec![],
        })
    }

    fn key(code: KeyCode) -> Action {
        Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn submit_direct(
        queue: &mut PersistentQueue,
        state: &mut AppState,
        prompt: &str,
    ) -> PromptAdmission {
        state.authenticated = true;
        state.composer.insert_text(prompt);
        queue
            .reduce(state, key(KeyCode::Enter))
            .unwrap()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Send(UiCommand::PreparePrompt {
                    prompt: sent,
                    admission,
                }) if sent == prompt => Some(admission),
                _ => None,
            })
            .expect("direct prompt admitted")
    }

    fn cancel_direct(
        queue: &mut PersistentQueue,
        state: &mut AppState,
        admission: PromptAdmission,
    ) {
        let effects = queue.reduce(state, key(KeyCode::Esc)).unwrap();
        assert!(
            effects.contains(&Effect::Send(UiCommand::CancelPromptPreparation {
                admission
            }))
        );
        queue
            .reduce(
                state,
                Action::UiEventReceived(UiEvent::PromptPreparationCancelled { admission }),
            )
            .unwrap();
    }

    fn seed_cancelled_direct(root: &std::path::Path, session: &str, prompt: &str) {
        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(root, session))
            .unwrap();
        let admission = submit_direct(&mut queue, &mut state, prompt);
        cancel_direct(&mut queue, &mut state, admission);
        drop(queue);
    }

    #[test]
    fn queue_restart_preserves_pending_and_uncertain_prompt_without_replay_or_duplicates() {
        let root = temp_workspace("restart");
        let mut journal = PromptQueueJournal::open(&root, "session").unwrap();
        journal
            .save(PromptQueueSnapshot {
                pending: vec!["next".into()],
                in_flight: Some("uncertain".into()),
                recovery_draft: None,
            })
            .unwrap();
        drop(journal);
        for _ in 0..2 {
            let mut queue = PersistentQueue::default();
            let mut state = AppState::new();
            let effects = queue
                .reduce(
                    &mut state,
                    Action::UiEventReceived(UiEvent::SessionSnapshot {
                        session_id: SessionId("session".into()),
                        cwd: root.to_string_lossy().into(),
                        skill_names: vec![],
                    }),
                )
                .unwrap();
            assert_eq!(
                state
                    .queued_prompts
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["uncertain", "next"]
            );
            assert!(state.queue_paused);
            assert!(!effects.iter().any(|e| matches!(
                e,
                Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
            )));
            assert!(queue
                .journal
                .as_ref()
                .unwrap()
                .snapshot()
                .in_flight
                .is_none());
        }
        {
            let mut queue = PersistentQueue::default();
            let mut state = AppState::new();
            queue
                .reduce(
                    &mut state,
                    Action::UiEventReceived(UiEvent::SessionSnapshot {
                        session_id: SessionId("completed".into()),
                        cwd: root.to_string_lossy().into(),
                        skill_names: vec![],
                    }),
                )
                .unwrap();
            queue
                .reduce(
                    &mut state,
                    Action::UiEventReceived(UiEvent::RunStarted {
                        run_id: 1,
                        max_mutating_tool_calls: 8,
                        max_read_tool_calls: 8,
                        max_turns: 8,
                    }),
                )
                .unwrap();
            queue
                .reduce(
                    &mut state,
                    Action::UiEventReceived(UiEvent::QueuedUserAdded {
                        text: "queued".into(),
                        position: 0,
                    }),
                )
                .unwrap();
            let effects = queue
                .reduce(
                    &mut state,
                    Action::UiEventReceived(UiEvent::RunCompleted { run_id: 1 }),
                )
                .unwrap();
            let admission = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::Send(UiCommand::PreparePrompt { prompt, admission })
                        if prompt == "queued" =>
                    {
                        Some(*admission)
                    }
                    _ => None,
                })
                .expect("queued prompt admitted");
            assert_eq!(
                queue
                    .journal
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .in_flight
                    .as_deref(),
                Some("queued")
            );
            assert!(queue
                .journal
                .as_ref()
                .unwrap()
                .snapshot()
                .pending
                .is_empty());
            queue
                .reduce(
                    &mut state,
                    Action::UiEventReceived(UiEvent::PromptRunStarted {
                        admission,
                        run_id: 2,
                        max_mutating_tool_calls: 8,
                        max_read_tool_calls: 8,
                        max_turns: 8,
                    }),
                )
                .unwrap();
            queue
                .reduce(
                    &mut state,
                    Action::UiEventReceived(UiEvent::PromptRunCompleted {
                        admission,
                        run_id: 2,
                    }),
                )
                .unwrap();
            assert_eq!(
                queue.journal.as_ref().unwrap().snapshot(),
                &PromptQueueSnapshot::default()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_direct_prompt_is_recovered_once_and_resubmission_consumes_backup() {
        let root = temp_workspace("direct-recovery");
        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(&root, "direct"))
            .unwrap();

        let admission = submit_direct(&mut queue, &mut state, "direct prompt");
        cancel_direct(&mut queue, &mut state, admission);
        assert_eq!(state.composer.payload(), "direct prompt");
        let snapshot = queue.journal.as_ref().unwrap().snapshot();
        assert_eq!(snapshot.in_flight, None);
        assert_eq!(snapshot.recovery_draft.as_deref(), Some("direct prompt"));
        drop(queue);

        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        state.authenticated = true;
        let effects = queue
            .reduce(&mut state, session_snapshot(&root, "direct"))
            .unwrap();
        assert_eq!(state.composer.payload(), "direct prompt");
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::PreparePrompt { .. }))));
        assert_eq!(
            queue
                .journal
                .as_ref()
                .unwrap()
                .snapshot()
                .recovery_draft
                .as_deref(),
            Some("direct prompt")
        );

        let admission = queue
            .reduce(&mut state, key(KeyCode::Enter))
            .unwrap()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Send(UiCommand::PreparePrompt { admission, .. }) => Some(admission),
                _ => None,
            })
            .expect("recovered draft is submitted through admission");
        let snapshot = queue.journal.as_ref().unwrap().snapshot();
        assert_eq!(snapshot.recovery_draft, None);
        assert_eq!(snapshot.in_flight.as_deref(), Some("direct prompt"));
        assert_eq!(admission.origin, crate::api::PromptOrigin::Direct);
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_prompt_cancelled_before_session_snapshot_is_recovered_after_restart() {
        let root = temp_workspace("cancel-before-first-snapshot");
        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        state.authenticated = true;
        state.composer.insert_text("first direct prompt");

        let admission = queue
            .reduce(&mut state, key(KeyCode::Enter))
            .unwrap()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Send(UiCommand::PreparePrompt { prompt, admission })
                    if prompt == "first direct prompt" =>
                {
                    Some(admission)
                }
                _ => None,
            })
            .expect("first prompt admitted before session exists");
        assert!(queue.journal.is_none());

        let cancel_effects = queue.reduce(&mut state, key(KeyCode::Esc)).unwrap();
        assert!(
            cancel_effects.contains(&Effect::Send(UiCommand::CancelPromptPreparation {
                admission,
            }))
        );
        assert_eq!(
            state.current_prompt_preparation().unwrap().admission,
            admission
        );

        queue
            .reduce(&mut state, session_snapshot(&root, "first-session"))
            .unwrap();
        assert_eq!(
            queue
                .journal
                .as_ref()
                .unwrap()
                .snapshot()
                .in_flight
                .as_deref(),
            Some("first direct prompt")
        );

        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::PromptPreparationCancelled { admission }),
            )
            .unwrap();
        assert_eq!(state.composer.payload(), "first direct prompt");
        assert_eq!(queue.journal.as_ref().unwrap().snapshot().in_flight, None);
        assert_eq!(
            queue
                .journal
                .as_ref()
                .unwrap()
                .snapshot()
                .recovery_draft
                .as_deref(),
            Some("first direct prompt")
        );
        drop(queue);

        let mut queue = PersistentQueue::default();
        let mut restarted = AppState::new();
        let effects = queue
            .reduce(&mut restarted, session_snapshot(&root, "first-session"))
            .unwrap();
        assert_eq!(restarted.composer.payload(), "first direct prompt");
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::PreparePrompt { .. }))));
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovered_direct_prompt_moves_once_to_paused_queue_if_new_draft_exists() {
        let root = temp_workspace("direct-new-draft");
        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(&root, "direct"))
            .unwrap();
        let admission = submit_direct(&mut queue, &mut state, "cancelled prompt");
        cancel_direct(&mut queue, &mut state, admission);
        drop(queue);

        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        state.composer.insert_text("new draft");
        let effects = queue
            .reduce(&mut state, session_snapshot(&root, "direct"))
            .unwrap();
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::PreparePrompt { .. }))));
        assert_eq!(state.composer.payload(), "new draft");
        assert_eq!(state.queued_prompt(0), Some("cancelled prompt"));
        assert!(state.queue_paused);
        let snapshot = queue.journal.as_ref().unwrap().snapshot();
        assert_eq!(snapshot.recovery_draft, None);
        assert_eq!(snapshot.pending, vec!["cancelled prompt".to_string()]);
        drop(queue);

        let mut queue = PersistentQueue::default();
        let mut restarted = AppState::new();
        restarted.composer.insert_text("another new draft");
        queue
            .reduce(&mut restarted, session_snapshot(&root, "direct"))
            .unwrap();
        assert_eq!(restarted.composer.payload(), "another new draft");
        assert_eq!(restarted.queue_len(), 1);
        assert_eq!(restarted.queued_prompt(0), Some("cancelled prompt"));
        assert!(restarted.queue_paused);
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_prompt_restart_preserves_paused_item_without_auto_admission() {
        let root = temp_workspace("queued-restart");
        let mut journal = PromptQueueJournal::open(&root, "queued").unwrap();
        journal
            .save(PromptQueueSnapshot {
                pending: vec!["queued prompt".into()],
                in_flight: None,
                recovery_draft: None,
            })
            .unwrap();
        drop(journal);

        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        let effects = queue
            .reduce(&mut state, session_snapshot(&root, "queued"))
            .unwrap();
        assert_eq!(state.queued_prompt(0), Some("queued prompt"));
        assert!(state.queue_paused);
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::PreparePrompt { .. }))));
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_edit_or_discard_clears_recovered_draft_backup() {
        let root = temp_workspace("recovery-edit");
        seed_cancelled_direct(&root, "edited", "recover me");
        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(&root, "edited"))
            .unwrap();
        queue
            .reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)),
            )
            .unwrap();
        assert_eq!(state.composer.payload(), "recover me!");
        assert_eq!(
            queue.journal.as_ref().unwrap().snapshot().recovery_draft,
            None
        );
        drop(queue);

        seed_cancelled_direct(&root, "discarded", "discard me");
        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(&root, "discarded"))
            .unwrap();
        queue
            .reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            )
            .unwrap();
        assert!(state.composer.is_empty());
        assert_eq!(
            queue.journal.as_ref().unwrap().snapshot().recovery_draft,
            None
        );
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_and_stale_terminals_cannot_finish_a_new_same_text_admission() {
        let root = temp_workspace("stale-terminal");
        let mut queue = PersistentQueue::default();
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(&root, "same-text"))
            .unwrap();

        let first = submit_direct(&mut queue, &mut state, "same text");
        queue.reduce(&mut state, key(KeyCode::Esc)).unwrap();
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::PromptRunCancelled {
                    admission: first,
                    run_id: 1,
                }),
            )
            .unwrap();
        assert!(state.queue_paused);
        assert!(queue.active.is_none());
        assert_eq!(queue.journal.as_ref().unwrap().snapshot().in_flight, None);
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::PromptRunStarted {
                    admission: first,
                    run_id: 1,
                    max_mutating_tool_calls: 8,
                    max_read_tool_calls: 8,
                    max_turns: 8,
                }),
            )
            .unwrap();

        let second = submit_direct(&mut queue, &mut state, "same text");
        assert_ne!(first.id, second.id);
        assert_ne!(first.generation, second.generation);
        for event in [
            UiEvent::PromptRunCompleted {
                admission: first,
                run_id: 1,
            },
            UiEvent::PromptRunStarted {
                admission: first,
                run_id: 99,
                max_mutating_tool_calls: 8,
                max_read_tool_calls: 8,
                max_turns: 8,
            },
            UiEvent::PromptRunFailed {
                admission: first,
                run_id: Some(99),
                message: "late duplicate".into(),
            },
        ] {
            queue
                .reduce(&mut state, Action::UiEventReceived(event))
                .unwrap();
        }
        assert_eq!(state.prompt_preparation().unwrap().admission, second);
        assert_eq!(
            queue.active.as_ref().and_then(|active| active.admission),
            Some(second)
        );
        assert_eq!(
            queue
                .journal
                .as_ref()
                .unwrap()
                .snapshot()
                .in_flight
                .as_deref(),
            Some("same text")
        );
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }
}
