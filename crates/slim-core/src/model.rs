#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub sequence: u64,
}

impl SessionSnapshot {
    pub fn new(session_id: impl Into<String>, sequence: u64) -> Self {
        Self {
            session_id: session_id.into(),
            sequence,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppHandle {
    snapshot: Option<SessionSnapshot>,
    events: Vec<SessionEvent>,
    last_event_seq: Option<u64>,
    event_sender: Option<std::sync::mpsc::Sender<SessionEvent>>,
}

impl PartialEq for AppHandle {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot == other.snapshot
            && self.events == other.events
            && self.last_event_seq == other.last_event_seq
    }
}

impl Eq for AppHandle {}

impl AppHandle {
    pub fn fake() -> Self {
        Self {
            snapshot: None,
            events: Vec::new(),
            last_event_seq: None,
            event_sender: None,
        }
    }

    pub fn apply_session_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.snapshot = Some(snapshot);
    }

    pub fn snapshot(&self) -> Option<&SessionSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn set_event_sender(&mut self, sender: std::sync::mpsc::Sender<SessionEvent>) {
        self.event_sender = Some(sender);
    }

    pub fn clear_event_sender(&mut self) {
        self.event_sender = None;
    }

    pub fn push_event(&mut self, event: SessionEvent) -> Result<(), &'static str> {
        if self
            .last_event_seq
            .is_some_and(|last_seq| event.seq <= last_seq)
        {
            return Err("event sequence must increase");
        }
        self.last_event_seq = Some(event.seq);
        self.events.push(event.clone());
        if self
            .event_sender
            .as_ref()
            .is_some_and(|sender| sender.send(event).is_err())
        {
            self.event_sender = None;
        }
        Ok(())
    }

    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }
}
use crate::events::SessionEvent;
