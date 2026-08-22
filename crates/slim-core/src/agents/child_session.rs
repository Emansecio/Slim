#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStatus {
    Queued,
    Active,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnRequest {
    pub id: String,
    pub depth: u8,
    pub read_only: bool,
}

impl SpawnRequest {
    pub fn new(id: impl Into<String>, depth: u8, read_only: bool) -> Self {
        Self {
            id: id.into(),
            depth,
            read_only,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildResult {
    pub id: String,
    pub status: ChildStatus,
    pub session_id: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnResult {
    Started,
    Queued,
    QueueFull,
    DepthExceeded,
    Duplicate,
}
