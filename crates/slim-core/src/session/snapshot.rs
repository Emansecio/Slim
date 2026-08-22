use crate::events::SessionEvent;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub event_count: usize,
    pub last_seq: Option<u64>,
}

impl SessionSnapshot {
    pub fn rebuild(events: &[SessionEvent]) -> Self {
        Self {
            event_count: events.len(),
            last_seq: events.last().map(|event| event.seq),
        }
    }
}
