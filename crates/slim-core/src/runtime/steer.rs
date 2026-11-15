use super::*;
use std::collections::VecDeque;

/// Mid-run user messages waiting for the next turn boundary. Never durable:
/// a run that ends without delivering them hands the leftovers back to the
/// host, which re-queues them as ordinary prompts.
#[derive(Clone, Debug, Default)]
pub struct SteerInbox(Arc<Mutex<VecDeque<String>>>);

/// Same bound philosophy as the TUI's prompt queue: a runaway submit loop
/// cannot balloon the live conversation.
const STEER_INBOX_CAP: usize = 8;

impl PartialEq for SteerInbox {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for SteerInbox {}

impl Runtime {
    pub fn set_steer_inbox(&mut self, inbox: SteerInbox) {
        self.steer_inbox = Some(inbox);
    }
}

impl SteerInbox {
    /// Appends FIFO; `false` when the inbox is already at its cap.
    pub fn push(&self, text: String) -> bool {
        let mut inbox = lock_mutex(&self.0);
        if inbox.len() >= STEER_INBOX_CAP {
            return false;
        }
        inbox.push_back(text);
        true
    }

    /// Takes every pending message in FIFO order.
    pub fn drain(&self) -> Vec<String> {
        lock_mutex(&self.0).drain(..).collect()
    }

    pub fn is_empty(&self) -> bool {
        lock_mutex(&self.0).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_drains_in_fifo_order_and_empties() {
        let inbox = SteerInbox::default();
        assert!(inbox.is_empty());
        assert!(inbox.push("first".into()));
        assert!(inbox.push("second".into()));
        assert!(!inbox.is_empty());
        assert_eq!(inbox.drain(), vec!["first".to_owned(), "second".to_owned()]);
        assert!(inbox.is_empty());
        assert_eq!(inbox.drain(), Vec::<String>::new());
    }

    #[test]
    fn push_refuses_at_the_cap() {
        let inbox = SteerInbox::default();
        for index in 0..8 {
            assert!(inbox.push(format!("message-{index}")), "index {index}");
        }
        assert!(!inbox.push("ninth".into()));
        assert_eq!(inbox.drain().len(), 8);
        assert!(inbox.push("after drain".into()));
    }

    #[test]
    fn clones_share_the_same_inbox() {
        let inbox = SteerInbox::default();
        let clone = inbox.clone();
        assert_eq!(inbox, clone);
        assert!(inbox.push("shared".into()));
        assert_eq!(clone.drain(), vec!["shared".to_owned()]);
        let other = SteerInbox::default();
        assert_ne!(inbox, other);
    }
}
