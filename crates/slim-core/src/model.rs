use crate::events::{EventKind, SessionEvent};
use crate::runtime::CancellationToken;
use std::collections::VecDeque;
use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct EventQueueState {
    events: VecDeque<SessionEvent>,
    capacity: usize,
    sender_count: usize,
    receiver_alive: bool,
    high_watermark: usize,
    send_wait_count: u64,
    send_wait_duration: Duration,
    coalesced_events: u64,
}

#[derive(Debug)]
struct EventQueue {
    state: Mutex<EventQueueState>,
    not_empty: Condvar,
    not_full: Condvar,
}

fn lock_event_queue(queue: &EventQueue) -> MutexGuard<'_, EventQueueState> {
    queue
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
pub struct SessionEventSender {
    queue: Arc<EventQueue>,
    cancellation: CancellationToken,
}

#[derive(Debug)]
pub struct SessionEventReceiver {
    queue: Arc<EventQueue>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventQueueStats {
    pub capacity: usize,
    pub queued: usize,
    pub high_watermark: usize,
    pub send_wait_count: u64,
    pub send_wait_duration: Duration,
    pub coalesced_events: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendOutcome {
    Sent,
    DroppedAfterCancellation,
    Disconnected,
}

impl SessionEventSender {
    pub fn bounded(
        capacity: usize,
        cancellation: CancellationToken,
    ) -> (Self, SessionEventReceiver) {
        let queue = Arc::new(EventQueue {
            state: Mutex::new(EventQueueState {
                events: VecDeque::with_capacity(capacity.max(1)),
                capacity: capacity.max(1),
                sender_count: 1,
                receiver_alive: true,
                high_watermark: 0,
                send_wait_count: 0,
                send_wait_duration: Duration::ZERO,
                coalesced_events: 0,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
        });
        (
            Self {
                queue: queue.clone(),
                cancellation,
            },
            SessionEventReceiver { queue },
        )
    }

    // Backpressure callers must recover the exact unsent event for retry/coalescing.
    #[allow(clippy::result_large_err)]
    pub fn try_send(&self, event: SessionEvent) -> Result<(), TrySendError<SessionEvent>> {
        let mut state = lock_event_queue(&self.queue);
        if !state.receiver_alive {
            return Err(TrySendError::Disconnected(event));
        }
        if try_coalesce_tail(&mut state.events, &event) {
            state.coalesced_events = state.coalesced_events.saturating_add(1);
            return Ok(());
        }
        if state.events.len() >= state.capacity {
            return Err(TrySendError::Full(event));
        }
        state.events.push_back(event);
        state.high_watermark = state.high_watermark.max(state.events.len());
        self.queue.not_empty.notify_one();
        Ok(())
    }

    fn send_interruptible(&self, event: SessionEvent) -> SendOutcome {
        let mut event = Some(event);
        loop {
            let mut state = lock_event_queue(&self.queue);
            let pending = event.as_ref().expect("pending event");
            if !state.receiver_alive {
                return SendOutcome::Disconnected;
            }
            if try_coalesce_tail(&mut state.events, pending) {
                state.coalesced_events = state.coalesced_events.saturating_add(1);
                return SendOutcome::Sent;
            }
            if state.events.len() < state.capacity {
                state.events.push_back(event.take().expect("pending event"));
                state.high_watermark = state.high_watermark.max(state.events.len());
                self.queue.not_empty.notify_one();
                return SendOutcome::Sent;
            }
            if self.cancellation.is_cancelled() {
                if is_cancel_droppable(pending) {
                    return SendOutcome::DroppedAfterCancellation;
                }
                if let Some(index) = state.events.iter().position(is_cancel_droppable) {
                    state.events.remove(index);
                    state.events.push_back(event.take().expect("pending event"));
                    self.queue.not_empty.notify_one();
                    return SendOutcome::Sent;
                }
            }
            let wait_started = Instant::now();
            let (mut next, _) = self
                .queue
                .not_full
                .wait_timeout(state, Duration::from_millis(1))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            next.send_wait_count = next.send_wait_count.saturating_add(1);
            next.send_wait_duration = next
                .send_wait_duration
                .saturating_add(wait_started.elapsed());
            drop(next);
        }
    }

    pub fn stats(&self) -> EventQueueStats {
        queue_stats(&self.queue)
    }
}

impl Clone for SessionEventSender {
    fn clone(&self) -> Self {
        lock_event_queue(&self.queue).sender_count += 1;
        Self {
            queue: self.queue.clone(),
            cancellation: self.cancellation.clone(),
        }
    }
}

impl Drop for SessionEventSender {
    fn drop(&mut self) {
        let mut state = lock_event_queue(&self.queue);
        state.sender_count = state.sender_count.saturating_sub(1);
        if state.sender_count == 0 {
            self.queue.not_empty.notify_all();
        }
    }
}

impl SessionEventReceiver {
    pub fn stats(&self) -> EventQueueStats {
        queue_stats(&self.queue)
    }

    pub fn recv(&self) -> Result<SessionEvent, RecvError> {
        let mut state = lock_event_queue(&self.queue);
        loop {
            if let Some(event) = state.events.pop_front() {
                self.queue.not_full.notify_one();
                return Ok(event);
            }
            if state.sender_count == 0 {
                return Err(RecvError);
            }
            state = self
                .queue
                .not_empty
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub fn try_recv(&self) -> Result<SessionEvent, TryRecvError> {
        let mut state = lock_event_queue(&self.queue);
        if let Some(event) = state.events.pop_front() {
            self.queue.not_full.notify_one();
            return Ok(event);
        }
        if state.sender_count == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<SessionEvent, RecvTimeoutError> {
        let started = Instant::now();
        let mut state = lock_event_queue(&self.queue);
        loop {
            if let Some(event) = state.events.pop_front() {
                self.queue.not_full.notify_one();
                return Ok(event);
            }
            if state.sender_count == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            let (next, wait) = self
                .queue
                .not_empty
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if wait.timed_out() && state.events.is_empty() {
                return if state.sender_count == 0 {
                    Err(RecvTimeoutError::Disconnected)
                } else {
                    Err(RecvTimeoutError::Timeout)
                };
            }
        }
    }

    pub fn try_iter(&self) -> impl Iterator<Item = SessionEvent> + '_ {
        std::iter::from_fn(move || self.try_recv().ok())
    }
}

const MAX_COALESCED_DELTA_BYTES: usize = 64 * 1024;

fn try_coalesce_tail(events: &mut VecDeque<SessionEvent>, pending: &SessionEvent) -> bool {
    let Some(tail) = events.back_mut() else {
        return false;
    };
    try_coalesce_event(tail, pending)
}

fn try_coalesce_event_vec(events: &mut [SessionEvent], pending: &SessionEvent) -> bool {
    let Some(tail) = events.last_mut() else {
        return false;
    };
    try_coalesce_event(tail, pending)
}

fn try_coalesce_event(tail: &mut SessionEvent, pending: &SessionEvent) -> bool {
    let merged = match (&mut tail.kind, &pending.kind) {
        (
            crate::EventKind::AssistantTextDelta { text: current },
            crate::EventKind::AssistantTextDelta { text },
        )
        | (
            crate::EventKind::ReasoningDelta { text: current },
            crate::EventKind::ReasoningDelta { text },
        ) if current.len().saturating_add(text.len()) <= MAX_COALESCED_DELTA_BYTES => {
            current.push_str(text);
            true
        }
        (
            crate::EventKind::ToolProgress {
                batch_id: current_batch,
                call_id: current_call,
                name: current_name,
                preview: current_preview,
            },
            crate::EventKind::ToolProgress {
                batch_id,
                call_id,
                name,
                preview,
            },
        ) if current_batch == batch_id && current_call == call_id => {
            current_name.clone_from(name);
            current_preview.clone_from(preview);
            true
        }
        _ => false,
    };
    if merged {
        tail.seq = pending.seq;
    }
    merged
}

fn queue_stats(queue: &EventQueue) -> EventQueueStats {
    let state = lock_event_queue(queue);
    EventQueueStats {
        capacity: state.capacity,
        queued: state.events.len(),
        high_watermark: state.high_watermark,
        send_wait_count: state.send_wait_count,
        send_wait_duration: state.send_wait_duration,
        coalesced_events: state.coalesced_events,
    }
}

impl Iterator for SessionEventReceiver {
    type Item = SessionEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv().ok()
    }
}

impl Drop for SessionEventReceiver {
    fn drop(&mut self) {
        let mut state = lock_event_queue(&self.queue);
        state.receiver_alive = false;
        self.queue.not_full.notify_all();
    }
}

fn is_cancel_droppable(event: &SessionEvent) -> bool {
    matches!(
        &event.kind,
        crate::EventKind::AssistantTextDelta { .. }
            | crate::EventKind::ReasoningDelta { .. }
            | crate::EventKind::ToolOutput { .. }
            | crate::EventKind::ToolProgress { .. }
    )
}

#[derive(Clone, Debug)]
pub struct AppHandle {
    events: Vec<SessionEvent>,
    last_event_seq: Option<u64>,
    event_sender: Option<SessionEventSender>,
    pub(crate) run_journal: Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
}

impl PartialEq for AppHandle {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events && self.last_event_seq == other.last_event_seq
    }
}

impl Eq for AppHandle {}

impl AppHandle {
    pub fn fake() -> Self {
        Self {
            events: Vec::new(),
            last_event_seq: None,
            event_sender: None,
            run_journal: None,
        }
    }

    pub fn set_event_sender(&mut self, sender: SessionEventSender) {
        self.event_sender = Some(sender);
    }

    pub fn set_run_journal(&mut self, journal: Arc<Mutex<crate::session::ManualRunJournal>>) {
        self.run_journal = Some(journal);
    }

    pub fn push_event(&mut self, event: SessionEvent) -> Result<(), &'static str> {
        if self
            .last_event_seq
            .is_some_and(|last_seq| event.seq <= last_seq)
        {
            return Err("event sequence must increase");
        }
        self.last_event_seq = Some(event.seq);
        if !try_coalesce_event_vec(&mut self.events, &event) {
            self.events.push(event.clone());
        }
        // ToolOutput is the only carrier of a tool's full result: it must wait
        // for queue space like every other event instead of being dropped on a
        // full queue. It stays cancel-droppable inside `send_interruptible`.
        if self
            .event_sender
            .as_ref()
            .is_some_and(|sender| sender.send_interruptible(event) == SendOutcome::Disconnected)
        {
            self.event_sender = None;
        }
        Ok(())
    }

    pub(crate) fn push_transient_event(&mut self, event: SessionEvent) -> Result<(), &'static str> {
        if self
            .last_event_seq
            .is_some_and(|last_seq| event.seq <= last_seq)
        {
            return Err("event sequence must increase");
        }
        self.last_event_seq = Some(event.seq);
        if !try_coalesce_event_vec(&mut self.events, &event) {
            self.events.push(event.clone());
        }
        if let Some(sender) = &self.event_sender {
            match sender.try_send(event) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => self.event_sender = None,
            }
        }
        Ok(())
    }

    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn discard_projected_payloads(&mut self) {
        if self.event_sender.is_none() {
            return;
        }
        // Release the buffers, not just the contents: `clear()` would retain
        // up to `max_result_bytes` of capacity per old output event.
        for event in &mut self.events {
            match &mut event.kind {
                EventKind::AssistantTextDelta { text } | EventKind::ReasoningDelta { text } => {
                    *text = String::new()
                }
                EventKind::ToolStarted { arguments, .. }
                | EventKind::ToolCall { arguments, .. }
                | EventKind::ProviderToolCall { arguments, .. } => *arguments = String::new(),
                EventKind::ToolOutput { output, .. } => *output = String::new(),
                EventKind::ToolProgress { preview, .. } => *preview = String::new(),
                EventKind::QuestionRequired { question, .. } => *question = String::new(),
                _ => {}
            }
        }
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }
}

#[cfg(test)]
mod tests {
    use super::{AppHandle, SessionEventSender};
    use crate::runtime::CancellationToken;
    use crate::{EventKind, SessionEvent};
    use std::sync::mpsc::TrySendError;

    #[test]
    fn bounded_sender_reports_full_at_exact_capacity() {
        let (sender, receiver) = SessionEventSender::bounded(2, CancellationToken::new());
        sender
            .try_send(SessionEvent::new(
                1,
                EventKind::Usage {
                    input_tokens: 1,
                    output_tokens: 0,
                },
            ))
            .expect("first");
        sender
            .try_send(SessionEvent::new(
                2,
                EventKind::Usage {
                    input_tokens: 2,
                    output_tokens: 0,
                },
            ))
            .expect("second");
        assert!(matches!(
            sender.try_send(SessionEvent::new(
                3,
                EventKind::Usage {
                    input_tokens: 3,
                    output_tokens: 0,
                },
            )),
            Err(TrySendError::Full(_))
        ));
        assert_eq!(receiver.try_iter().count(), 2);
    }

    #[test]
    fn event_queue_recovers_after_a_poisoned_lock() {
        let (sender, receiver) = SessionEventSender::bounded(1, CancellationToken::new());
        let queue = sender.queue.clone();
        let poisoned = std::thread::spawn(move || {
            let _guard = queue.state.lock().expect("initial lock");
            panic!("poison event queue");
        });
        assert!(poisoned.join().is_err());

        sender
            .try_send(SessionEvent::new(
                1,
                EventKind::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ))
            .expect("queue recovers");
        assert!(matches!(
            receiver.try_recv().expect("event after poison").kind,
            EventKind::Usage { .. }
        ));
    }

    #[test]
    fn adjacent_stream_deltas_coalesce_before_queue_backpressure() {
        let (sender, receiver) = SessionEventSender::bounded(2, CancellationToken::new());
        for seq in 1..=1_000 {
            sender
                .try_send(SessionEvent::new(
                    seq,
                    EventKind::AssistantTextDelta { text: "x".into() },
                ))
                .expect("coalesced delta");
        }
        let event = receiver.try_recv().expect("merged event");
        assert!(matches!(
            event.kind,
            EventKind::AssistantTextDelta { text } if text.len() == 1_000
        ));
        let stats = sender.stats();
        assert_eq!(stats.high_watermark, 1);
        assert_eq!(stats.coalesced_events, 999);
        assert_eq!(stats.send_wait_count, 0);
    }

    #[test]
    fn cancellation_drops_only_visual_backlog_and_keeps_causal_sender() {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = SessionEventSender::bounded(1, cancellation.clone());
        let mut app = AppHandle::fake();
        app.set_event_sender(sender);
        app.push_event(SessionEvent::new(
            1,
            EventKind::AssistantTextDelta {
                text: "sent".into(),
            },
        ))
        .expect("first visual event");

        cancellation.cancel();
        app.push_event(SessionEvent::new(
            2,
            EventKind::ReasoningDelta {
                text: "dropped".into(),
            },
        ))
        .expect("cancelled visual event remains in ledger");
        app.push_event(SessionEvent::new(
            3,
            EventKind::ToolOutput {
                batch_id: "batch".into(),
                call_id: "call".into(),
                name: "read".into(),
                output: "dropped preview".into(),
            },
        ))
        .expect("cancelled tool progress remains in ledger");
        assert_eq!(receiver.try_iter().count(), 1);

        app.push_event(SessionEvent::new(
            4,
            EventKind::ToolFinished {
                batch_id: "batch".into(),
                call_id: "call".into(),
                name: "read".into(),
                success: false,
                duration_ms: 1,
            },
        ))
        .expect("causal tool terminal");
        assert!(matches!(
            receiver.try_recv().expect("causal event").kind,
            EventKind::ToolFinished { .. }
        ));
        assert_eq!(app.events().len(), 4);
    }

    #[test]
    fn cancellation_evicts_queued_visual_event_for_causal_suffix() {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = SessionEventSender::bounded(1, cancellation.clone());
        let mut app = AppHandle::fake();
        app.set_event_sender(sender);
        app.push_event(SessionEvent::new(
            1,
            EventKind::AssistantTextDelta {
                text: "discardable".into(),
            },
        ))
        .expect("queued visual event");

        cancellation.cancel();
        app.push_event(SessionEvent::new(
            2,
            EventKind::Usage {
                input_tokens: 3,
                output_tokens: 2,
            },
        ))
        .expect("causal suffix must not block behind visual backlog");

        assert!(matches!(
            receiver.try_recv().expect("causal suffix").kind,
            EventKind::Usage {
                input_tokens: 3,
                output_tokens: 2
            }
        ));
        assert_eq!(app.events().len(), 2, "ledger remains append-only");
    }

    #[test]
    fn cancellation_keeps_process_facts_before_tool_terminal() {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = SessionEventSender::bounded(1, cancellation.clone());
        let mut app = AppHandle::fake();
        app.set_event_sender(sender);
        app.push_event(SessionEvent::new(
            1,
            EventKind::ToolProgress {
                batch_id: "batch".into(),
                call_id: "call".into(),
                name: "shell".into(),
                preview: "running".into(),
            },
        ))
        .expect("visual progress");

        cancellation.cancel();
        app.push_event(SessionEvent::new(
            2,
            EventKind::ToolProcessFinished {
                batch_id: "batch".into(),
                call_id: "call".into(),
                name: "shell".into(),
                process: crate::process::ProcessExecutionFacts {
                    exit_code: Some(1),
                    timed_out: false,
                    cancelled: true,
                    stdout_bytes: 0,
                    stderr_bytes: 0,
                    stdout_discarded_bytes: 0,
                    stderr_discarded_bytes: 0,
                },
            },
        ))
        .expect("causal process fact");
        assert!(matches!(
            receiver
                .try_recv()
                .expect("process fact survives cancellation")
                .kind,
            EventKind::ToolProcessFinished { .. }
        ));

        app.push_event(SessionEvent::new(
            3,
            EventKind::ToolFinished {
                batch_id: "batch".into(),
                call_id: "call".into(),
                name: "shell".into(),
                success: false,
                duration_ms: 1,
            },
        ))
        .expect("terminal event follows process fact");
        assert!(matches!(
            receiver.try_recv().expect("terminal event").kind,
            EventKind::ToolFinished { .. }
        ));
    }

    #[test]
    fn tool_output_waits_for_space_instead_of_dropping() {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = SessionEventSender::bounded(1, cancellation);
        let mut app = AppHandle::fake();
        app.set_event_sender(sender);
        app.push_event(SessionEvent::new(
            1,
            EventKind::Usage {
                input_tokens: 1,
                output_tokens: 0,
            },
        ))
        .expect("fill the single queue slot");
        let producer = std::thread::spawn(move || {
            app.push_event(SessionEvent::new(
                2,
                EventKind::ToolOutput {
                    batch_id: "batch".into(),
                    call_id: "call".into(),
                    name: "read".into(),
                    output: "complete result".into(),
                },
            ))
            .expect("tool output delivery");
        });
        assert!(matches!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("queued event")
                .kind,
            EventKind::Usage { .. }
        ));
        producer.join().expect("producer finishes once space frees");
        assert!(matches!(
            receiver.try_recv().expect("tool output kept").kind,
            EventKind::ToolOutput { output, .. } if output == "complete result"
        ));
    }

    #[test]
    fn discard_projected_payloads_releases_retained_buffers() {
        let (sender, _receiver) = SessionEventSender::bounded(16, CancellationToken::new());
        let mut app = AppHandle::fake();
        app.set_event_sender(sender);
        app.push_event(SessionEvent::new(
            1,
            EventKind::ToolOutput {
                batch_id: "batch".into(),
                call_id: "call".into(),
                name: "read".into(),
                output: "x".repeat(16_384),
            },
        ))
        .expect("push output");
        app.discard_projected_payloads();
        let output = app
            .events()
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ToolOutput { output, .. } => Some(output),
                _ => None,
            })
            .expect("tool output");
        assert!(output.is_empty());
        assert!(
            output.capacity() < 1024,
            "cleared payload must not retain a 16 KiB buffer"
        );
    }

    #[test]
    fn ledger_coalesces_adjacent_stream_deltas() {
        let mut app = AppHandle::fake();
        for seq in 1..=1_000 {
            app.push_event(SessionEvent::new(
                seq,
                EventKind::AssistantTextDelta { text: "x".into() },
            ))
            .expect("delta");
        }
        assert_eq!(app.events().len(), 1);
        let event = &app.events()[0];
        assert_eq!(event.seq, 1_000);
        assert!(matches!(
            &event.kind,
            EventKind::AssistantTextDelta { text } if text == &"x".repeat(1_000)
        ));
    }

    #[test]
    fn ledger_coalescing_stops_at_any_boundary_event() {
        let mut app = AppHandle::fake();
        app.push_event(SessionEvent::new(
            1,
            EventKind::AssistantTextDelta { text: "a".into() },
        ))
        .expect("first delta");
        app.push_event(SessionEvent::new(
            2,
            EventKind::Usage {
                input_tokens: 1,
                output_tokens: 0,
            },
        ))
        .expect("boundary");
        app.push_event(SessionEvent::new(
            3,
            EventKind::AssistantTextDelta { text: "b".into() },
        ))
        .expect("second delta");
        app.push_event(SessionEvent::new(
            4,
            EventKind::ReasoningDelta { text: "r".into() },
        ))
        .expect("other delta kind");
        app.push_event(SessionEvent::new(
            5,
            EventKind::ReasoningDelta { text: "2".into() },
        ))
        .expect("same-kind delta");
        assert_eq!(app.events().len(), 4);
        assert!(matches!(
            &app.events()[0].kind,
            EventKind::AssistantTextDelta { text } if text == "a"
        ));
        assert!(matches!(
            &app.events()[2].kind,
            EventKind::AssistantTextDelta { text } if text == "b"
        ));
        assert!(matches!(
            &app.events()[3].kind,
            EventKind::ReasoningDelta { text } if text == "r2"
        ));
        assert_eq!(app.events()[3].seq, 5);
    }

    #[test]
    fn ledger_tool_progress_coalesces_only_matching_identity() {
        let mut app = AppHandle::fake();
        let progress = |seq: u64, call_id: &str, name: &str, preview: &str| {
            SessionEvent::new(
                seq,
                EventKind::ToolProgress {
                    batch_id: "batch".into(),
                    call_id: call_id.into(),
                    name: name.into(),
                    preview: preview.into(),
                },
            )
        };
        app.push_event(progress(1, "call", "read", "first"))
            .expect("p1");
        app.push_event(progress(2, "call", "write", "latest"))
            .expect("p2");
        app.push_event(progress(3, "other", "list", "other first"))
            .expect("different call");
        app.push_event(progress(4, "other", "list", "other latest"))
            .expect("same call");
        assert_eq!(app.events().len(), 2);
        assert!(matches!(
            &app.events()[0].kind,
            EventKind::ToolProgress { preview, name, .. }
                if preview == "latest" && name == "write"
        ));
        assert_eq!(app.events()[0].seq, 2);
        assert!(matches!(
            &app.events()[1].kind,
            EventKind::ToolProgress { preview, .. } if preview == "other latest"
        ));
        assert_eq!(app.events()[1].seq, 4);
    }
}
