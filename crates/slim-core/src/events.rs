use crate::protocol::OperatingMode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub kind: EventKind,
}

impl SessionEvent {
    pub fn new(seq: u64, kind: EventKind) -> Self {
        Self { seq, kind }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum EventKind {
    SessionStarted {
        session_id: String,
    },
    ModeChanged {
        mode: OperatingMode,
    },
    AssistantTextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    AssistantEnded {
        reason: String,
    },
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
    ToolStarted {
        name: String,
    },
    ToolCall {
        name: String,
        arguments: String,
    },
    ProviderToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolOutput {
        name: String,
        output: String,
    },
    ToolFinished {
        name: String,
        success: bool,
    },
    ArtifactStored {
        id: String,
        size: u64,
    },
    CompactionCompleted,
    /// Per-turn wire-size snapshot emitted before each provider request so
    /// token-economy regressions are observable in the session log.
    ContextSnapshot {
        tools_bytes: u64,
        history_bytes: u64,
    },
    ApprovalRequired,
    InputRequired,
    SubagentActivity {
        message: String,
    },
    TerminalError {
        message: String,
    },
}
