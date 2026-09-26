#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TodoItem {
    pub id: u64,
    pub title: String,
    pub status: TodoStatus,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TodoTracker {
    next_id: u64,
    items: Vec<TodoItem>,
}

impl TodoTracker {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            items: Vec::new(),
        }
    }

    pub fn add(&mut self, title: impl Into<String>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(TodoItem {
            id,
            title: title.into(),
            status: TodoStatus::Pending,
            reason: None,
        });
        id
    }

    pub fn set_status(&mut self, id: u64, status: TodoStatus) -> Result<(), &'static str> {
        if status == TodoStatus::InProgress
            && self
                .items
                .iter()
                .any(|item| item.id != id && item.status == TodoStatus::InProgress)
        {
            return Err("only one todo may be in progress per agent");
        }
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or("todo not found")?;
        item.status = status;
        item.reason = None;
        Ok(())
    }

    pub fn set_status_with_reason(
        &mut self,
        id: u64,
        status: TodoStatus,
        reason: Option<String>,
    ) -> Result<(), &'static str> {
        self.set_status(id, status)?;
        self.items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or("todo not found")?
            .reason = reason;
        Ok(())
    }

    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }
}
