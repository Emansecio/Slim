use crate::{
    api::{PromptAdmission, QueueJournalBridge, UiCommand, UiEvent},
    app::AppState,
    reducer::{reduce, Action, Effect},
};
use slim_core::session::{PromptQueueJournal, PromptQueueSnapshot};
use std::{io, path::PathBuf, sync::mpsc};

enum JournalWorkerCmd {
    Attach {
        journal: PromptQueueJournal,
        seq: u64,
    },
    Open {
        workspace: PathBuf,
        session: String,
        reply: mpsc::SyncSender<io::Result<PromptQueueSnapshot>>,
    },
    Detach {
        seq: u64,
    },
    Save {
        seq: u64,
        snapshot: PromptQueueSnapshot,
    },
    Shutdown,
}

struct PendingSave {
    min_seq: u64,
    max_seq: u64,
    snapshot: PromptQueueSnapshot,
}

struct WorkerAliveGuard(QueueJournalBridge);

impl Drop for WorkerAliveGuard {
    fn drop(&mut self) {
        self.0.set_worker_alive(false);
    }
}

struct JournalWorker {
    tx: mpsc::Sender<JournalWorkerCmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl JournalWorker {
    fn spawn(bridge: QueueJournalBridge) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<JournalWorkerCmd>();
        bridge.set_worker_alive(true);
        let thread_bridge = bridge.clone();
        let handle = std::thread::Builder::new()
            .name("slim-tui-queue".into())
            .spawn(move || {
                let _alive_guard = WorkerAliveGuard(thread_bridge.clone());
                run_journal_worker(rx, &thread_bridge);
            })
            .inspect_err(|_| {
                bridge.set_worker_alive(false);
            })?;
        Ok(Self {
            tx,
            handle: Some(handle),
        })
    }
}

impl Drop for JournalWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(JournalWorkerCmd::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn flush_pending_save(
    journal: &mut Option<PromptQueueJournal>,
    pending: &mut Option<PendingSave>,
    bridge: &QueueJournalBridge,
) {
    let Some(PendingSave {
        min_seq,
        max_seq,
        snapshot,
    }) = pending.take()
    else {
        return;
    };
    let (wrote, error) = match journal.as_mut() {
        Some(journal) => {
            let needs_write = journal.snapshot() != &snapshot;
            match journal.save(snapshot) {
                Ok(()) => (needs_write, None),
                Err(error) => (false, Some(error.to_string())),
            }
        }
        None => (false, Some("prompt queue journal not attached".to_owned())),
    };
    if error.is_some() {
        bridge.mark_retry_needed();
    }
    bridge.mark_durable(min_seq, max_seq, error, wrote);
}

fn run_journal_worker(rx: mpsc::Receiver<JournalWorkerCmd>, bridge: &QueueJournalBridge) {
    let mut journal: Option<PromptQueueJournal> = None;
    let mut pending_save: Option<PendingSave> = None;
    while let Ok(first) = rx.recv() {
        let mut shutdown = false;
        let mut next = Some(first);
        while let Some(cmd) = next {
            match cmd {
                JournalWorkerCmd::Attach {
                    journal: new_journal,
                    seq,
                } => {
                    flush_pending_save(&mut journal, &mut pending_save, bridge);
                    journal = Some(new_journal);
                    bridge.mark_attached(seq);
                }
                JournalWorkerCmd::Open {
                    workspace,
                    session,
                    reply,
                } => {
                    flush_pending_save(&mut journal, &mut pending_save, bridge);
                    match PromptQueueJournal::open(&workspace, &session) {
                        Ok(opened) => {
                            let snapshot = opened.snapshot().clone();
                            journal = Some(opened);
                            bridge.clear_session_errors();
                            let _ = reply.send(Ok(snapshot));
                        }
                        Err(error) => {
                            journal = None;
                            bridge.clear_session_errors();
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                JournalWorkerCmd::Detach { seq } => {
                    flush_pending_save(&mut journal, &mut pending_save, bridge);
                    journal = None;
                    bridge.mark_detached(seq);
                }
                JournalWorkerCmd::Save { seq, snapshot } => {
                    // Coalesce consecutive snapshots queued while the worker was
                    // busy syncing an earlier barrier, preserving the full seq range.
                    match &mut pending_save {
                        Some(existing) => {
                            existing.max_seq = existing.max_seq.max(seq);
                            existing.snapshot = snapshot;
                        }
                        None => {
                            pending_save = Some(PendingSave {
                                min_seq: seq,
                                max_seq: seq,
                                snapshot,
                            });
                        }
                    }
                }
                JournalWorkerCmd::Shutdown => {
                    shutdown = true;
                    break;
                }
            }
            next = rx.try_recv().ok();
        }
        flush_pending_save(&mut journal, &mut pending_save, bridge);
        if shutdown {
            break;
        }
    }
    flush_pending_save(&mut journal, &mut pending_save, bridge);
}

pub(crate) struct PersistentQueue {
    identity: Option<(String, std::sync::Arc<str>)>,
    snapshot: PromptQueueSnapshot,
    last_saved_snapshot: Option<PromptQueueSnapshot>,
    active: Option<ActivePrompt>,
    initial_prompt: Option<String>,
    bridge: QueueJournalBridge,
    worker: Option<JournalWorker>,
}

impl Default for PersistentQueue {
    fn default() -> Self {
        Self::with_initial_prompt_and_bridge(None, QueueJournalBridge::new())
    }
}

impl Drop for PersistentQueue {
    fn drop(&mut self) {
        self.worker.take();
        self.bridge.set_ui_attached(false);
    }
}

#[derive(Clone, Debug)]
struct ActivePrompt {
    admission: Option<PromptAdmission>,
    text: String,
    run_id: Option<u64>,
}

impl PersistentQueue {
    pub fn with_initial_prompt_and_bridge(
        prompt: Option<String>,
        bridge: QueueJournalBridge,
    ) -> Self {
        bridge.set_ui_attached(true);
        Self {
            identity: None,
            snapshot: PromptQueueSnapshot::default(),
            last_saved_snapshot: None,
            active: None,
            initial_prompt: prompt,
            bridge,
            worker: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Option<&PromptQueueSnapshot> {
        self.identity.as_ref().map(|_| &self.snapshot)
    }

    fn ensure_worker(&mut self) -> io::Result<&JournalWorker> {
        if self.worker.is_none() {
            self.worker = Some(JournalWorker::spawn(self.bridge.clone())?);
        }
        Ok(self.worker.as_ref().expect("worker initialized"))
    }

    pub fn reduce(&mut self, state: &mut AppState, action: Action) -> io::Result<Vec<Effect>> {
        // Clock actions arrive on every wake of the loop and never touch the
        // queue: they skip the draft snapshot and the journal comparison.
        // So do streamed output and usage events: they change neither the
        // queue, the draft, nor the session identity, and emit no prompt
        // command, so there is nothing to journal.
        if matches!(
            action,
            Action::SyncClock(_)
                | Action::Tick(_)
                | Action::StatusTick(_)
                | Action::UiEventReceived(
                    UiEvent::AssistantDelta { .. }
                        | UiEvent::ThinkingDelta { .. }
                        | UiEvent::ToolProgress { .. }
                        | UiEvent::ToolOutput { .. }
                        | UiEvent::ToolDiff { .. }
                        | UiEvent::ToolSource { .. }
                        | UiEvent::UsagePartial { .. }
                        | UiEvent::Usage { .. }
                        | UiEvent::UsageBreakdown { .. }
                        | UiEvent::UsageEstimate { .. }
                        | UiEvent::UsageEstimateForRun { .. }
                        | UiEvent::ContextTelemetry { .. }
                        | UiEvent::CacheDriftDetected { .. }
                )
        ) {
            return Ok(reduce(state, action));
        }
        let session_restored = matches!(
            &action,
            Action::UiEventReceived(UiEvent::SessionRestored { .. })
        );
        let session_cleared = matches!(
            &action,
            Action::UiEventReceived(UiEvent::SessionCleared { .. })
        );
        let explicit_edit = matches!(
            &action,
            Action::Key(_)
                | Action::Paste(_)
                | Action::ClipboardPull { .. }
                | Action::SubmitInitialPrompt(_)
        );
        // Compare without allocating `composer.payload()`.
        let had_matching_recovery_draft = explicit_edit
            && self
                .snapshot
                .recovery_draft
                .as_deref()
                .is_some_and(|draft| state.composer.payload_equals(draft));
        let composer_was_empty = matches!(
            &action,
            Action::UiEventReceived(
                UiEvent::PromptPreparationCancelled { .. }
                    | UiEvent::PromptPreparationFailed { .. }
            )
        ) && state.composer.is_empty();
        let ui_event = matches!(&action, Action::UiEventReceived(_));
        let session_event = matches!(
            &action,
            Action::UiEventReceived(
                UiEvent::SessionSnapshot { .. } | UiEvent::SessionRestored { .. }
            )
        );
        // Only the events the queue reads below are kept; a streamed delta or
        // a restored session is not copied.
        let prompt_event = match &action {
            Action::UiEventReceived(event) if queue_reads(event) => Some(event.clone()),
            _ => None,
        };
        let was_working = state.working;
        let mut effects = reduce(state, action);
        if session_cleared {
            self.identity = None;
            self.snapshot = PromptQueueSnapshot::default();
            self.last_saved_snapshot = None;
            self.active = None;
            let _ = self.bridge.take_retry_needed();
            if let Some(worker) = self.worker.as_ref() {
                let seq = self.bridge.next_enqueue_seq();
                if worker.tx.send(JournalWorkerCmd::Detach { seq }).is_err() {
                    self.bridge.mark_clear_settled();
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "prompt queue persistence worker stopped",
                    ));
                }
            } else {
                self.bridge.mark_clear_settled();
            }
        }
        let Some(session) = &state.session_id else {
            return Ok(effects);
        };
        let identity = (state.cwd.clone(), session.0.clone());
        let restored = self.identity.as_ref() != Some(&identity) || session_restored;
        if restored {
            let previous_active = self.active.take();
            self.identity = None;
            self.snapshot = PromptQueueSnapshot::default();
            self.last_saved_snapshot = None;
            let _ = self.bridge.take_retry_needed();
            let preopened = self.bridge.take_preopened(&identity.1);
            let snapshot = if let Some(journal) = preopened {
                let snapshot = journal.snapshot().clone();
                self.ensure_worker()?;
                let seq = self.bridge.begin_attach();
                let worker = self.worker.as_ref().expect("worker initialized");
                worker
                    .tx
                    .send(JournalWorkerCmd::Attach { journal, seq })
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "prompt queue persistence worker stopped",
                        )
                    })?;
                snapshot
            } else {
                let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                let worker = self.ensure_worker()?;
                worker
                    .tx
                    .send(JournalWorkerCmd::Open {
                        workspace: PathBuf::from(&identity.0),
                        session: identity.1.to_string(),
                        reply: reply_tx,
                    })
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "prompt queue persistence worker stopped",
                        )
                    })?;
                reply_rx.recv().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "prompt queue persistence worker stopped",
                    )
                })??
            };
            let active_preparation = state.current_prompt_preparation().cloned();
            let matches_live_in_flight = !session_restored
                && snapshot.in_flight.as_deref().is_some_and(|in_flight| {
                    active_preparation
                        .as_ref()
                        .map(|prep| prep.prompt.as_str())
                        .or_else(|| previous_active.as_ref().map(|active| active.text.as_str()))
                        == Some(in_flight)
                });
            let mut recovery_draft = snapshot.recovery_draft.clone();
            let mut recovered_queue = Vec::new();
            if let Some(prompt) = snapshot.in_flight.clone() {
                if !matches_live_in_flight {
                    recovered_queue.push(prompt);
                    state.apply_event(UiEvent::Notification { message:
                        "Fila recuperada: uma execução ficou sem confirmação. Confira os efeitos antes de repetir o primeiro item.".into() });
                }
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
                || (snapshot.in_flight.is_some() && !matches_live_in_flight)
                || (snapshot.recovery_draft.is_some() && recovery_draft.is_none())
            {
                state.queue_paused = true;
            }
            if let Some(preparation) = active_preparation {
                self.active = Some(ActivePrompt {
                    admission: Some(preparation.admission),
                    text: preparation.prompt,
                    run_id: None,
                });
            } else if matches_live_in_flight {
                self.active = previous_active;
            }
            self.last_saved_snapshot = Some(snapshot.clone());
            self.snapshot = snapshot;
            self.identity = Some(identity);
        }

        if self.initial_prompt.is_some() && session_event {
            let prompt = self.initial_prompt.take().expect("initial prompt present");
            effects.extend(reduce(state, Action::SubmitInitialPrompt(prompt)));
        }

        let mut snapshot = self.snapshot.clone();
        if restored {
            // Ambiguous work is paused and surfaced as queued; only a currently
            // admitted in-process prompt remains in-flight.
            snapshot.in_flight = self.active.as_ref().map(|active| active.text.clone());
            snapshot.pending = state.queued_prompts.iter().cloned().collect();
            snapshot.recovery_draft = snapshot
                .recovery_draft
                .take()
                .filter(|draft| state.composer.payload_equals(draft));
        }

        if had_matching_recovery_draft
            && snapshot
                .recovery_draft
                .as_deref()
                .is_some_and(|draft| !state.composer.payload_equals(draft))
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
                        && composer_was_empty
                        && state.composer.payload_equals(&active.text)
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
            }) if self.active.as_ref().is_some_and(|active| {
                active.admission.is_none() && active.run_id == Some(*run_id)
            }) =>
            {
                self.active = None;
                snapshot.in_flight = None;
                snapshot.recovery_draft = None;
            }
            _ => {}
        }

        // A legacy RunStarted can arrive on a later event after the command was
        // accepted. Its matching terminal is the only event that clears it.
        if was_working && !state.working && self.active.is_some() && !ui_event {
            snapshot.in_flight.clone_from(&self.snapshot.in_flight);
        }
        snapshot.pending = state.queued_prompts.iter().cloned().collect();
        self.snapshot = snapshot;
        if self.bridge.take_retry_needed() {
            self.last_saved_snapshot = None;
        }
        if self.last_saved_snapshot.as_ref() != Some(&self.snapshot) {
            let to_save = self.snapshot.clone();
            self.last_saved_snapshot = Some(to_save.clone());
            self.ensure_worker()?;
            let seq = self.bridge.next_enqueue_seq();
            let worker = self.worker.as_ref().expect("worker initialized");
            worker
                .tx
                .send(JournalWorkerCmd::Save {
                    seq,
                    snapshot: to_save,
                })
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "prompt queue persistence worker stopped",
                    )
                })?;
        }
        Ok(effects)
    }
}

/// The events `PersistentQueue::reduce` inspects after the reducer ran.
fn queue_reads(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::PromptPreparationCancelled { .. }
            | UiEvent::PromptPreparationFailed { .. }
            | UiEvent::PromptPreparationHandled { .. }
            | UiEvent::PromptRunStarted { .. }
            | UiEvent::PromptRunCompleted { .. }
            | UiEvent::PromptRunStopped { .. }
            | UiEvent::PromptRunCancelled { .. }
            | UiEvent::PromptRunFailed { .. }
            | UiEvent::RunStarted { .. }
            | UiEvent::RunCompleted { .. }
            | UiEvent::RunStopped { .. }
            | UiEvent::RunCancelled { .. }
            | UiEvent::RunFailed { .. }
    )
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
            assert!(queue.snapshot().unwrap().in_flight.is_none());
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
                queue.snapshot().unwrap().in_flight.as_deref(),
                Some("queued")
            );
            assert!(queue.snapshot().unwrap().pending.is_empty());
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
            assert_eq!(queue.snapshot().unwrap(), &PromptQueueSnapshot::default());
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
        let snapshot = queue.snapshot().unwrap();
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
            queue.snapshot().unwrap().recovery_draft.as_deref(),
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
        let snapshot = queue.snapshot().unwrap();
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
        assert!(queue.snapshot().is_none());

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
            queue.snapshot().unwrap().in_flight.as_deref(),
            Some("first direct prompt")
        );

        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::PromptPreparationCancelled { admission }),
            )
            .unwrap();
        assert_eq!(state.composer.payload(), "first direct prompt");
        assert_eq!(queue.snapshot().unwrap().in_flight, None);
        assert_eq!(
            queue.snapshot().unwrap().recovery_draft.as_deref(),
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
        let snapshot = queue.snapshot().unwrap();
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
        assert_eq!(queue.snapshot().unwrap().recovery_draft, None);
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
        assert_eq!(queue.snapshot().unwrap().recovery_draft, None);
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
        assert_eq!(queue.snapshot().unwrap().in_flight, None);
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
            queue.snapshot().unwrap().in_flight.as_deref(),
            Some("same text")
        );
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bridge_preopened_session_and_wait_durable_persist_across_display_cwd() {
        let root = temp_workspace("bridge-preopened");
        let journal = PromptQueueJournal::open(&root, "bridge-session").unwrap();
        let bridge = QueueJournalBridge::new();
        bridge.seed_preopened("bridge-session".into(), journal);

        let mut queue = PersistentQueue::with_initial_prompt_and_bridge(None, bridge.clone());
        let mut state = AppState::new();
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::SessionSnapshot {
                    session_id: SessionId("bridge-session".into()),
                    cwd: "~/abbreviated-display-path".into(),
                    skill_names: vec![],
                }),
            )
            .unwrap();

        let _admission = submit_direct(&mut queue, &mut state, "durable prompt");
        bridge.wait_durable().unwrap();

        let queue_files: Vec<_> = std::fs::read_dir(root.join(".slim/queues"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        assert_eq!(queue_files.len(), 1);
        let raw = std::fs::read_to_string(queue_files[0].path()).unwrap();
        assert!(raw.contains("durable prompt"));

        drop(queue);
        let disk = PromptQueueJournal::open(&root, "bridge-session").unwrap();
        assert_eq!(disk.snapshot().in_flight.as_deref(), Some("durable prompt"));
        drop(disk);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_cleared_detaches_worker_and_releases_queue_lock_for_reopen() {
        let root = temp_workspace("session-cleared-detach");
        let bridge = QueueJournalBridge::new();
        let journal = PromptQueueJournal::open(&root, "same-session").unwrap();
        bridge.seed_preopened("same-session".into(), journal);

        let mut queue = PersistentQueue::with_initial_prompt_and_bridge(None, bridge.clone());
        let mut state = AppState::new();
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::SessionSnapshot {
                    session_id: SessionId("same-session".into()),
                    cwd: root.to_string_lossy().into(),
                    skill_names: vec![],
                }),
            )
            .unwrap();
        let _admission = submit_direct(&mut queue, &mut state, "persisted before new");
        bridge.wait_durable().unwrap();

        // While the worker owns the journal, opening the same session must fail with ResourceBusy.
        let busy = PromptQueueJournal::open(&root, "same-session");
        assert_eq!(
            busy.err().map(|e| e.kind()),
            Some(io::ErrorKind::ResourceBusy)
        );

        // `/new` emits SessionCleared; waiting for durability must finish the detach and drop the lock.
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::SessionCleared {
                    cwd: root.to_string_lossy().into(),
                    skill_names: vec![],
                }),
            )
            .unwrap();
        assert!(state.session_id.is_none());
        assert!(queue.snapshot().is_none());
        bridge.wait_durable().unwrap();

        // Re-opening the same session immediately succeeds without ResourceBusy.
        let reopened = PromptQueueJournal::open(&root, "same-session").unwrap();
        assert_eq!(
            reopened.snapshot().in_flight.as_deref(),
            Some("persisted before new")
        );
        bridge.seed_preopened("same-session".into(), reopened);

        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::SessionRestored {
                    session_id: SessionId("same-session".into()),
                    cwd: root.to_string_lossy().into(),
                    messages: vec![],
                    skill_names: vec![],
                }),
            )
            .unwrap();
        assert_eq!(
            state.queued_prompt(0),
            Some("persisted before new"),
            "resuming the same session restores its queue cleanly"
        );
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mark_durable_preserves_error_across_subsequent_noop_ok_save() {
        let bridge = QueueJournalBridge::new();
        bridge.set_worker_alive(true);

        // Simulate seq=1 failing (e.g., >64 MiB snapshot) followed by seq=2 no-op `Ok`
        // (e.g., cancelled back to `default()` without writing to disk).
        let seq1 = bridge.next_enqueue_seq();
        bridge.mark_durable(seq1, seq1, Some("64 MiB limit exceeded".into()), false);

        let seq2 = bridge.next_enqueue_seq();
        bridge.mark_durable(seq2, seq2, None, false);

        let err = bridge
            .wait_durable()
            .expect_err("no-op Ok save must not swallow earlier io::Error");
        assert!(
            err.to_string().contains("64 MiB limit exceeded"),
            "unexpected error: {err}"
        );

        // Even after a later real disk write succeeds at seq=3, a batch covering seq=1
        // must still report failure for callers waiting on seq=1, while seq=3 succeeds.
        let seq3 = bridge.next_enqueue_seq();
        bridge.mark_durable(seq3, seq3, None, true);
        bridge.wait_durable().expect("real write heals future targets");

        let root = temp_workspace("retry-after-save-error");
        let journal = PromptQueueJournal::open(&root, "retry-session").unwrap();
        let bridge = QueueJournalBridge::new();
        bridge.seed_preopened("retry-session".into(), journal);

        let mut queue = PersistentQueue::with_initial_prompt_and_bridge(None, bridge.clone());
        let mut state = AppState::new();
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::SessionSnapshot {
                    session_id: SessionId("retry-session".into()),
                    cwd: root.to_string_lossy().into(),
                    skill_names: vec![],
                }),
            )
            .unwrap();

        // Keep a run active so queued items stay in `pending` rather than admitting to `in_flight`.
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

        // Trigger a >64 MiB `PromptQueueJournal::save` failure without redundant clones.
        let chunk = "x".repeat(17 * 1_048_576);
        for _ in 0..3 {
            state.queued_prompts.push_back(chunk.clone());
        }
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::QueuedUserAdded {
                    text: chunk,
                    position: 3,
                }),
            )
            .unwrap();
        let save_err = bridge
            .wait_durable()
            .expect_err("oversized snapshot should fail journal save");
        assert!(save_err.to_string().contains("64 MiB"), "{save_err}");

        // While `self.snapshot` is still the failed snapshot, a no-op state event
        // must trigger a retry (because `last_saved_snapshot` is not frozen on error).
        let seq_before_retry = bridge.next_enqueue_seq();
        bridge.mark_durable(seq_before_retry, seq_before_retry, None, false);
        queue.reduce(&mut state, key(KeyCode::Left)).unwrap();
        let retry_err = bridge
            .wait_durable()
            .expect_err("retry after failed save must not be suppressed");
        assert!(retry_err.to_string().contains("64 MiB"), "{retry_err}");

        // Reverting back to `default()` causes a no-op save; `wait_durable` must STILL error
        // because no real disk write succeeded after the failure.
        state.queued_prompts.clear();
        queue.reduce(&mut state, key(KeyCode::Left)).unwrap();
        let noop_err = bridge
            .wait_durable()
            .expect_err("reverting to no-op default must not erase save failure");
        assert!(noop_err.to_string().contains("64 MiB"), "{noop_err}");

        // Submitting a valid prompt triggers a real write (`wrote == true`) and succeeds.
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::RunCompleted { run_id: 1 }),
            )
            .unwrap();
        state.queue_paused = false;
        let _ok_admission = submit_direct(&mut queue, &mut state, "small valid prompt");
        bridge.wait_durable().unwrap();

        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_session_attach_matches_prepersisted_in_flight_without_false_recovery() {
        let root = temp_workspace("fresh-prepersisted-in-flight");
        let mut queue = PersistentQueue::with_initial_prompt_and_bridge(
            None,
            QueueJournalBridge::new(),
        );
        let bridge = queue.bridge.clone();
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
        assert!(state.session_id.is_none());

        // `slim-cli` pre-persists `in_flight` on disk before seeding the preopened journal
        // and sending `UiEvent::SessionSnapshot`.
        let mut journal = PromptQueueJournal::open(&root, "tui-fresh").unwrap();
        journal
            .save(PromptQueueSnapshot {
                pending: Vec::new(),
                in_flight: Some("first direct prompt".into()),
                recovery_draft: None,
            })
            .unwrap();
        bridge.seed_preopened("tui-fresh".into(), journal);

        queue
            .reduce(&mut state, session_snapshot(&root, "tui-fresh"))
            .unwrap();

        assert!(state.queued_prompts.is_empty());
        assert!(!state.queue_paused);
        assert!(state.notifications.is_empty());
        assert_eq!(
            queue.active.as_ref().and_then(|active| active.admission),
            Some(admission)
        );
        assert_eq!(
            queue.snapshot().unwrap().in_flight.as_deref(),
            Some("first direct prompt")
        );

        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::PromptRunStarted {
                    admission,
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
                Action::UiEventReceived(UiEvent::PromptRunCompleted {
                    admission,
                    run_id: 1,
                }),
            )
            .unwrap();
        assert_eq!(queue.snapshot().unwrap(), &PromptQueueSnapshot::default());
        bridge.wait_durable().unwrap();

        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn begin_session_clear_blocks_wait_durable_until_detach_and_skips_unattached_ui() {
        // Without a PersistentQueue attached (`ui_attached == false`), begin_session_clear
        // must not stall wait_durable().
        let unattached = QueueJournalBridge::new();
        unattached.begin_session_clear();
        unattached.wait_durable().unwrap();

        let root = temp_workspace("clear-detach-barrier");
        let bridge = QueueJournalBridge::new();
        let journal = PromptQueueJournal::open(&root, "clear-s1").unwrap();
        bridge.seed_preopened("clear-s1".into(), journal);

        let mut queue = PersistentQueue::with_initial_prompt_and_bridge(None, bridge.clone());
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(&root, "clear-s1"))
            .unwrap();
        let _admission = submit_direct(&mut queue, &mut state, "hold lock");
        bridge.wait_durable().unwrap();

        // Simulate `start_new_session`: begin_session_clear() before PersistentQueue reduces
        // UiEvent::SessionCleared.
        bridge.begin_session_clear();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter_bridge = bridge.clone();
        let waiter = std::thread::spawn(move || {
            let res = waiter_bridge.wait_durable();
            let _ = done_tx.send(res);
        });

        assert!(
            matches!(
                done_rx.recv_timeout(std::time::Duration::from_millis(30)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "wait_durable must block until SessionCleared detaches the journal"
        );

        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::SessionCleared {
                    cwd: root.to_string_lossy().into(),
                    skill_names: vec![],
                }),
            )
            .unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("wait_durable wakes after SessionCleared detach")
            .unwrap();
        waiter.join().unwrap();

        // The file lock must already be released when wait_durable returns.
        let reopened = PromptQueueJournal::open(&root, "clear-s1").unwrap();
        drop(reopened);

        // No-worker SessionCleared also settles pending_clears cleanly.
        let bridge_no_worker = QueueJournalBridge::new();
        let mut queue_no_worker =
            PersistentQueue::with_initial_prompt_and_bridge(None, bridge_no_worker.clone());
        let mut state_no_worker = AppState::new();
        bridge_no_worker.begin_session_clear();
        queue_no_worker
            .reduce(
                &mut state_no_worker,
                Action::UiEventReceived(UiEvent::SessionCleared {
                    cwd: root.to_string_lossy().into(),
                    skill_names: vec![],
                }),
            )
            .unwrap();
        bridge_no_worker.wait_durable().unwrap();

        // Dropping PersistentQueue before SessionCleared is reduced must not hang wait_durable().
        bridge_no_worker.begin_session_clear();
        drop(queue_no_worker);
        bridge_no_worker.wait_durable().unwrap();

        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn direct_session_restored_clears_stale_save_error_from_previous_session() {
        let root = temp_workspace("stale-error-across-restore");
        let bridge = QueueJournalBridge::new();
        let journal_s1 = PromptQueueJournal::open(&root, "s1").unwrap();
        bridge.seed_preopened("s1".into(), journal_s1);

        let mut queue = PersistentQueue::with_initial_prompt_and_bridge(None, bridge.clone());
        let mut state = AppState::new();
        queue
            .reduce(&mut state, session_snapshot(&root, "s1"))
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

        // Trigger a >64 MiB save failure in Session 1.
        let chunk = "x".repeat(17 * 1_048_576);
        for _ in 0..3 {
            state.queued_prompts.push_back(chunk.clone());
        }
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::QueuedUserAdded {
                    text: chunk,
                    position: 3,
                }),
            )
            .unwrap();
        let s1_err = bridge
            .wait_durable()
            .expect_err("Session 1 oversized snapshot must fail");
        assert!(s1_err.to_string().contains("64 MiB"), "{s1_err}");

        // Switch directly to empty Session 2 via SessionRestored (without SessionCleared),
        // so Session 2 performs no Save on attach.
        state.queued_prompts.clear();
        let journal_s2 = PromptQueueJournal::open(&root, "s2").unwrap();
        bridge.seed_preopened("s2".into(), journal_s2);
        queue
            .reduce(
                &mut state,
                Action::UiEventReceived(UiEvent::SessionRestored {
                    session_id: SessionId("s2".into()),
                    cwd: root.to_string_lossy().into(),
                    messages: vec![],
                    skill_names: vec![],
                }),
            )
            .unwrap();

        bridge
            .wait_durable()
            .expect("Session 1 save error must not leak into Session 2");

        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }
}
