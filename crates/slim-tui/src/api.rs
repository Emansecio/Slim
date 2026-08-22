use std::fmt;
use std::sync::{mpsc, Arc};

use slim_core::OperatingMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginProvider {
    Anthropic,
    OpenAiCodex,
}

impl LoginProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic — Claude Pro/Max",
            Self::OpenAiCodex => "OpenAI Codex — ChatGPT Plus/Pro",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAlias {
    Sol,
    Terra,
    Luna,
}

impl ModelAlias {
    pub const ALL: [Self; 3] = [Self::Sol, Self::Terra, Self::Luna];

    pub fn id(self) -> &'static str {
        match self {
            Self::Sol => "gpt-5.6-sol",
            Self::Terra => "gpt-5.6-terra",
            Self::Luna => "gpt-5.6-luna",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sol => "GPT-5.6 Sol",
            Self::Terra => "GPT-5.6 Terra",
            Self::Luna => "GPT-5.6 Luna",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sol" | "gpt-5.6-sol" => Some(Self::Sol),
            "terra" | "gpt-5.6-terra" => Some(Self::Terra),
            "luna" | "gpt-5.6-luna" => Some(Self::Luna),
            _ => None,
        }
    }

    pub fn from_index(index: usize) -> Self {
        Self::ALL[index.min(Self::ALL.len() - 1)]
    }

    pub fn index(self) -> usize {
        match self {
            Self::Sol => 0,
            Self::Terra => 1,
            Self::Luna => 2,
        }
    }
}

/// Reasoning effort levels supported by the GPT-5.6 family, mirroring the
/// Codex model catalog (`supported_reasoning_levels`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

impl ReasoningEffort {
    pub const ALL: [Self; 6] = [
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "XHigh",
            Self::Max => "Max",
            Self::Ultra => "Ultra",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Low => "Fast responses with lighter reasoning",
            Self::Medium => "Balances speed and reasoning depth for everyday tasks",
            Self::High => "Greater reasoning depth for complex problems",
            Self::XHigh => "Extra high reasoning depth for complex problems",
            Self::Max => "Maximum reasoning depth for the hardest problems",
            Self::Ultra => "Maximum reasoning with automatic task delegation",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|level| *level == self)
            .unwrap_or(0)
    }

    pub fn from_index(index: usize) -> Self {
        Self::ALL[index.min(Self::ALL.len() - 1)]
    }

    /// Levels actually supported by each model in the 5.6 family.
    pub fn supported(model: ModelAlias) -> &'static [Self] {
        match model {
            ModelAlias::Sol | ModelAlias::Terra => &Self::ALL,
            ModelAlias::Luna => &Self::ALL[..Self::ALL.len() - 1],
        }
    }

    /// Session default, aligned with the user's Codex configuration.
    pub fn default_for(_model: ModelAlias) -> Self {
        Self::High
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(pub Arc<str>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BlockId(pub Arc<str>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MessageId(pub Arc<str>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RunId(pub Arc<str>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ContentHandle(pub Arc<str>);

#[derive(Clone, Eq, PartialEq)]
pub struct SensitiveText(String);

impl SensitiveText {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SensitiveText {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for SensitiveText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TodoItemStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Cancelled,
}

impl TodoItemStatus {
    pub fn glyph(self) -> char {
        match self {
            Self::Completed => '\u{2713}',
            Self::InProgress => '\u{25CC}',
            Self::Pending => '\u{25CB}',
            Self::Blocked | Self::Cancelled => '\u{2715}',
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TodoItemView {
    pub title: String,
    pub status: TodoItemStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiEvent {
    SessionSnapshot {
        session_id: SessionId,
        cwd: String,
    },
    RunStarted,
    RunCompleted,
    RunStopped {
        message: String,
    },
    RunCancelled,
    RunFailed {
        message: String,
    },
    UserMessageAdded {
        text: String,
    },
    RestoreDraft {
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    AssistantEnded,
    ThinkingDelta {
        text: String,
    },
    ActivityChanged {
        label: String,
    },
    ToolStarted {
        name: String,
    },
    ToolProgress {
        name: String,
        preview: String,
    },
    ToolEnded {
        name: String,
        success: bool,
    },
    QueuedUserAdded {
        text: String,
        position: usize,
    },
    TodoChanged {
        items: Vec<TodoItemView>,
    },
    ContentPageLoaded {
        handle: ContentHandle,
        text: String,
    },
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
    ModeChanged {
        mode: OperatingMode,
    },
    ModelChanged {
        model: String,
    },
    EffortChanged {
        effort: ReasoningEffort,
    },
    AuthStateChanged {
        provider: Option<LoginProvider>,
        authenticated: bool,
    },
    LoginProgress {
        message: String,
    },
    LoginUrl {
        url: SensitiveText,
        user_code: Option<SensitiveText>,
    },
    Notification {
        message: String,
    },
    FatalError {
        message: String,
    },
    Shutdown,
}

impl UiEvent {
    pub fn from_core(event: slim_core::SessionEvent) -> Option<Self> {
        match event.kind {
            slim_core::EventKind::SessionStarted { session_id } => Some(Self::SessionSnapshot {
                session_id: SessionId(session_id.into()),
                cwd: String::new(),
            }),
            slim_core::EventKind::ModeChanged { mode } => Some(Self::ModeChanged { mode }),
            slim_core::EventKind::AssistantTextDelta { text } => {
                Some(Self::AssistantDelta { text })
            }
            slim_core::EventKind::ReasoningDelta { text } => Some(Self::ThinkingDelta { text }),
            slim_core::EventKind::AssistantEnded { .. } => Some(Self::AssistantEnded),
            slim_core::EventKind::Usage {
                input_tokens,
                output_tokens,
            } => Some(Self::Usage {
                input_tokens,
                output_tokens,
            }),
            slim_core::EventKind::ToolStarted { name } => Some(Self::ToolStarted { name }),
            slim_core::EventKind::ToolCall { name, .. }
                | slim_core::EventKind::ProviderToolCall { name, .. }
            => Some(Self::ToolStarted { name }),
            slim_core::EventKind::ToolOutput { name, output } => Some(Self::ToolProgress {
                name,
                preview: bounded_first_line(&output, 512),
            }),
            slim_core::EventKind::ToolFinished { name, success } => {
                Some(Self::ToolEnded { name, success })
            }
            slim_core::EventKind::ArtifactStored { id, size } => Some(Self::Notification {
                message: format!("artifact stored: {id} ({size} bytes)"),
            }),
            slim_core::EventKind::CompactionCompleted => Some(Self::Notification {
                message: "compaction completed".into(),
            }),
            slim_core::EventKind::ApprovalRequired => Some(Self::Notification {
                message: "approval_required".into(),
            }),
            slim_core::EventKind::InputRequired => Some(Self::Notification {
                message: "input_required".into(),
            }),
            slim_core::EventKind::SubagentActivity { message } => {
                Some(Self::ActivityChanged { label: message })
            }
            slim_core::EventKind::TerminalError { message } => Some(Self::FatalError { message }),
            slim_core::EventKind::ContextSnapshot { .. } => None,
        }
    }
}

fn bounded_first_line(output: &str, limit: usize) -> String {
    let mut chars = output.lines().next().unwrap_or("").chars();
    let mut preview = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiCommand {
    SendPrompt(String),
    StartLogin(LoginProvider),
    CancelLogin,
    Logout,
    SetMode(OperatingMode),
    SetModel {
        model: ModelAlias,
        effort: ReasoningEffort,
    },
    CancelRun,
    Shutdown,
    RequestContentPage {
        handle: ContentHandle,
    },
}

pub struct UiChannels {
    pub commands: mpsc::Sender<UiCommand>,
    /// Control lane (§10.1): input-adjacent lifecycle, errors, auth — lossless,
    /// bounded, always drained first.
    pub events: mpsc::Receiver<UiEvent>,
    /// Data lane (§10.1): deltas, usage, activity — bounded, coalescible.
    pub events_data: mpsc::Receiver<UiEvent>,
}

impl UiEvent {
    /// Control events are lifecycle/errors/state transitions that must never
    /// be coalesced or dropped (spec §10.2 "lifecycle start/end nunca
    /// coalescer; errors nunca coalescer").
    pub fn is_control(&self) -> bool {
        !matches!(
            self,
            Self::AssistantDelta { .. }
                | Self::ThinkingDelta { .. }
                | Self::Usage { .. }
                | Self::ActivityChanged { .. }
                | Self::ToolStarted { .. }
                | Self::ToolProgress { .. }
                | Self::ToolEnded { .. }
                | Self::Notification { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelAlias, ReasoningEffort};

    #[test]
    fn gpt_56_efforts_match_codex_model_catalog() {
        assert_eq!(
            ReasoningEffort::supported(ModelAlias::Sol),
            &ReasoningEffort::ALL
        );
        assert_eq!(
            ReasoningEffort::supported(ModelAlias::Terra),
            &ReasoningEffort::ALL
        );
        assert_eq!(
            ReasoningEffort::supported(ModelAlias::Luna),
            &ReasoningEffort::ALL[..5]
        );
        assert!(!ReasoningEffort::supported(ModelAlias::Luna).contains(&ReasoningEffort::Ultra));
    }
}
