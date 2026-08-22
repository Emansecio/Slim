use super::api::BlockId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolState {
    pub name: String,
    pub preview: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockKind {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(ToolState),
    System(String),
    Error(String),
    Activity(String),
    QueuedUser(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockLifecycle {
    Pending,
    Streaming,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FoldState {
    Auto,
    Collapsed,
    Expanded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    pub lifecycle: BlockLifecycle,
    pub fold: FoldState,
}

impl Block {
    pub fn new(id: impl Into<String>, kind: BlockKind, lifecycle: BlockLifecycle) -> Self {
        Self {
            id: BlockId(id.into().into()),
            kind,
            lifecycle,
            fold: FoldState::Auto,
        }
    }
}
