use super::child_session::{ChildResult, ChildStatus, SpawnRequest, SpawnResult};

#[derive(Clone, Debug)]
struct ChildRecord {
    request: SpawnRequest,
    status: ChildStatus,
    result: Option<ChildResult>,
}

#[derive(Debug)]
pub struct Scheduler {
    max_active: usize,
    max_queue: usize,
    children: Vec<ChildRecord>,
}

impl Scheduler {
    pub fn new(max_active: usize, max_queue: usize) -> Self {
        Self {
            max_active,
            max_queue,
            children: Vec::new(),
        }
    }

    pub fn spawn(&mut self, request: SpawnRequest) -> SpawnResult {
        if request.depth > 1 {
            return SpawnResult::DepthExceeded;
        }
        if self
            .children
            .iter()
            .any(|child| child.request.id == request.id)
        {
            return SpawnResult::Duplicate;
        }
        let active = self
            .children
            .iter()
            .filter(|child| child.status == ChildStatus::Active)
            .count();
        let queued = self
            .children
            .iter()
            .filter(|child| child.status == ChildStatus::Queued)
            .count();
        if active < self.max_active {
            self.children.push(ChildRecord {
                request,
                status: ChildStatus::Active,
                result: None,
            });
            SpawnResult::Started
        } else if queued < self.max_queue {
            self.children.push(ChildRecord {
                request,
                status: ChildStatus::Queued,
                result: None,
            });
            SpawnResult::Queued
        } else {
            SpawnResult::QueueFull
        }
    }

    pub fn finish(&mut self, id: &str, message: &str) {
        if let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.request.id == id)
        {
            child.status = ChildStatus::Completed;
            child.result = Some(ChildResult {
                id: id.into(),
                status: ChildStatus::Completed,
                session_id: format!("session-{id}"),
                message: message.into(),
            });
        }
        self.promote_next();
    }

    pub fn cancel(&mut self, id: &str) {
        if let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.request.id == id)
        {
            child.status = ChildStatus::Cancelled;
            child.result = Some(ChildResult {
                id: id.into(),
                status: ChildStatus::Cancelled,
                session_id: format!("session-{id}"),
                message: "cancelled".into(),
            });
        }
        self.promote_next();
    }

    pub fn status(&self, id: &str) -> Option<ChildStatus> {
        self.children
            .iter()
            .find(|child| child.request.id == id)
            .map(|child| child.status)
    }

    pub fn list(&self) -> Vec<ChildResult> {
        self.children
            .iter()
            .map(|child| {
                child.result.clone().unwrap_or_else(|| ChildResult {
                    id: child.request.id.clone(),
                    status: child.status,
                    session_id: format!("session-{}", child.request.id),
                    message: String::new(),
                })
            })
            .collect()
    }

    fn promote_next(&mut self) {
        let active = self
            .children
            .iter()
            .filter(|child| child.status == ChildStatus::Active)
            .count();
        if active >= self.max_active {
            return;
        }
        if let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.status == ChildStatus::Queued)
        {
            child.status = ChildStatus::Active;
        }
    }
}

#[derive(Debug, Default)]
pub struct MutationLease {
    owner: Option<String>,
}

impl MutationLease {
    pub fn acquire(&mut self, owner: &str) -> bool {
        if self.owner.is_some() {
            return false;
        }
        self.owner = Some(owner.into());
        true
    }

    pub fn release(&mut self, owner: &str) {
        if self.owner.as_deref() == Some(owner) {
            self.owner = None;
        }
    }
}
