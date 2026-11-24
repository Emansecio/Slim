use super::*;
use std::collections::VecDeque;

/// Mid-run user messages waiting for the next turn boundary. Never durable:
/// a run that ends without delivering them hands the leftovers back to the
/// host, which re-queues them as ordinary prompts.
#[derive(Clone, Debug, Default)]
pub struct SteerInbox(Arc<SteerShared>);

#[derive(Debug, Default)]
struct SteerShared {
    queue: Mutex<VecDeque<String>>,
    /// Wakes a run that is blocked waiting on something else (see `arrived`).
    arrived: tokio::sync::Notify,
}

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
        let mut inbox = lock_mutex(&self.0.queue);
        if inbox.len() >= STEER_INBOX_CAP {
            return false;
        }
        inbox.push_back(text);
        drop(inbox);
        self.0.arrived.notify_one();
        true
    }

    /// Takes every pending message in FIFO order.
    pub fn drain(&self) -> Vec<String> {
        lock_mutex(&self.0.queue).drain(..).collect()
    }

    pub fn is_empty(&self) -> bool {
        lock_mutex(&self.0.queue).is_empty()
    }

    pub fn len(&self) -> usize {
        lock_mutex(&self.0.queue).len()
    }

    /// Resolves once a message is waiting (right away when one already is). The message stays
    /// in the inbox: the loop still delivers it at the next turn boundary.
    pub async fn arrived(&self) {
        while self.is_empty() {
            self.0.arrived.notified().await;
        }
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

    #[tokio::test]
    async fn arrived_resolves_only_while_a_message_waits() {
        let inbox = SteerInbox::default();
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(20), inbox.arrived());
        assert!(quiet.await.is_err(), "empty inbox keeps waiting");
        let waiter = tokio::spawn({
            let inbox = inbox.clone();
            async move { inbox.arrived().await }
        });
        tokio::task::yield_now().await;
        assert!(inbox.push("go on".into()));
        waiter.await.unwrap();
        // Still there for the loop to deliver, and a second wait sees it at once.
        inbox.arrived().await;
        assert_eq!(inbox.drain(), vec!["go on".to_owned()]);
        // A drained inbox waits again, even with the stale wake-up from the push above.
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(20), inbox.arrived());
        assert!(quiet.await.is_err());
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
