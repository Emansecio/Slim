use std::fmt;
use std::sync::{mpsc, Arc};

use slim_core::OperatingMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginProvider {
    Anthropic,
    OpenAiCodex,
    OpenCodeGo,
    OpenCodeZen,
    ClinePass,
    CommandCode,
    Xai,
}

impl LoginProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic — Claude Pro/Max",
            Self::OpenAiCodex => "OpenAI Codex — ChatGPT Plus/Pro",
            Self::OpenCodeGo => "OpenCode Go — API key",
            Self::OpenCodeZen => "OpenCode Zen — free tier / API key",
            Self::ClinePass => "ClinePass — API key",
            Self::CommandCode => "Command Code — API key",
            Self::Xai => "xAI — Grok/X subscription",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAlias {
    Sol,
    Terra,
    Luna,
    Astra,
}

impl ModelAlias {
    pub const ALL: [Self; 4] = [Self::Sol, Self::Terra, Self::Luna, Self::Astra];

    pub fn id(self) -> &'static str {
        match self {
            Self::Sol => "gpt-5.6-sol",
            Self::Terra => "gpt-5.6-terra",
            Self::Luna => "gpt-5.6-luna",
            Self::Astra => "gpt-6-astra",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sol => "GPT-5.6 Sol",
            Self::Terra => "GPT-5.6 Terra",
            Self::Luna => "GPT-5.6 Luna",
            Self::Astra => "GPT-6 Astra",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sol" | "gpt-5.6-sol" => Some(Self::Sol),
            "terra" | "gpt-5.6-terra" => Some(Self::Terra),
            "luna" | "gpt-5.6-luna" => Some(Self::Luna),
            "astra" | "gpt-6-astra" => Some(Self::Astra),
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
            Self::Astra => 3,
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

    /// Request-level efforts supported by each built-in Codex model.
    /// Astra Ultra requires Codex orchestration beyond this Responses client.
    pub fn supported(model: ModelAlias) -> &'static [Self] {
        match model {
            ModelAlias::Sol | ModelAlias::Terra => &Self::ALL,
            ModelAlias::Luna | ModelAlias::Astra => &Self::ALL[..Self::ALL.len() - 1],
        }
    }

    /// Session default, aligned with the user's Codex configuration.
    pub fn default_for(_model: ModelAlias) -> Self {
        Self::High
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(pub Arc<str>);

/// One row of the `/resume` list: a resumable durable session of this workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListItem {
    /// Session id, equal to the file stem (`tui-...`); what `ResumeSession` takes.
    pub id: String,
    /// Name set with `/rename`, when there is one.
    pub title: Option<String>,
    /// First user prompt, single line, possibly empty when it could not be read cheaply.
    pub first_prompt: String,
    /// Last modification, Unix epoch milliseconds.
    pub updated_ms: u64,
    pub bytes: u64,
    /// Another process holds the session's lock; resuming it would fail.
    pub in_use: bool,
    /// The session currently open in this TUI (listed, not resumable from itself).
    pub current: bool,
}

/// One row of the `/rewind` list: a finished turn the session can go back to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnListItem {
    /// Zero-based position among the session's turns (oldest first).
    pub index: usize,
    /// Sequence of the turn's user entry; what `RewindSession` takes.
    pub first_seq: u64,
    /// First line of the turn's prompt (already redacted), at most 200 chars.
    pub prompt: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BlockId(pub Arc<str>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MessageId(pub Arc<str>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RunId(pub Arc<str>);

/// Identity carried from prompt admission through OAuth preparation and the
/// resulting run. IDs are local to one TUI process; the journal stores text,
/// never these transient identities.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PromptId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PromptGeneration(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PromptOrigin {
    Direct,
    Queued,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PromptAdmission {
    pub id: PromptId,
    pub generation: PromptGeneration,
    pub origin: PromptOrigin,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct InteractionRequestId(pub Arc<str>);

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ContentHandle(pub Arc<str>);

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ToolCallId(pub Arc<str>);

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ToolBatchId(pub Arc<str>);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContentRequestId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PageCursor(pub u64);

#[derive(Clone, Default, Eq, PartialEq)]
pub struct SensitiveText(String);

impl SensitiveText {
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push(&mut self, character: char) {
        self.0.push(character);
    }

    pub fn push_str_bounded(&mut self, value: &str, max_chars: usize) -> bool {
        if self.0.chars().count().saturating_add(value.chars().count()) > max_chars {
            return false;
        }
        self.0.push_str(value);
        true
    }

    pub fn pop(&mut self) -> Option<char> {
        self.0.pop()
    }

    pub fn char_len(&self) -> usize {
        self.0.chars().count()
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
    pub reason: Option<String>,
    pub id: Option<u64>,
    pub title: String,
    pub status: TodoItemStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenCodeCatalogSource {
    Live,
    Cache,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClinePassCatalogSource {
    Live,
    Cache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandCodeCatalogSource {
    Live,
    Cache,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZenCatalogSource {
    Live,
    Cache,
    Fallback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenCodeModelView {
    pub id: String,
    pub name: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub reasoning_levels: Vec<ReasoningEffort>,
    pub accepts_images: bool,
}

/// One configured MCP server as shown by `/mcp`. Status is pushed by the
/// worker; `tools` is the cached tool count once a server is `Ready`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum McpStatusView {
    Disabled,
    #[default]
    Disconnected,
    Connecting,
    Ready,
    Failed,
    /// Defined by the project `slim.toml` of a workspace not yet trusted.
    Untrusted,
    /// The HTTP server needs OAuth sign-in (`/mcp login <server>`).
    NeedsAuth,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpServerView {
    pub name: String,
    pub transport: &'static str,
    /// Command line or endpoint URL; never carries header/env values.
    pub target: String,
    pub status: McpStatusView,
    pub tools: Option<usize>,
    /// Cached `resources/list` size; `None` until a listing was fetched.
    pub resources: Option<usize>,
    /// Cached `resources/templates/list` size; `None` until fetched.
    pub resource_templates: Option<usize>,
    /// `gateway`, `direct` or `hidden` (empty when the host does not say).
    pub exposure: &'static str,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptRole {
    User,
    Assistant,
    Tool {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        name: String,
        arguments: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiEvent {
    SessionSnapshot {
        session_id: SessionId,
        cwd: String,
        /// Workspace skills discovered off the UI thread; applied as-is.
        skill_names: Vec<String>,
    },
    SessionRestored {
        session_id: SessionId,
        cwd: String,
        messages: Vec<TranscriptMessage>,
        /// Workspace skills discovered off the UI thread; applied as-is.
        skill_names: Vec<String>,
    },
    WorkspaceChanged {
        cwd: String,
        /// Workspace skills discovered off the UI thread; applied as-is.
        skill_names: Vec<String>,
    },
    AttachmentsChanged {
        labels: Vec<String>,
    },
    RunStarted {
        run_id: u64,
        max_mutating_tool_calls: usize,
        max_read_tool_calls: usize,
        max_turns: usize,
    },
    /// A prompt admitted by the reducer has completed authentication and is
    /// now entering the normal run lifecycle.
    PromptRunStarted {
        admission: PromptAdmission,
        run_id: u64,
        max_mutating_tool_calls: usize,
        max_read_tool_calls: usize,
        max_turns: usize,
    },
    /// Preparation ended before a run was admitted. The reducer uses the
    /// matching admission to recover exactly one prompt.
    PromptPreparationCancelled {
        admission: PromptAdmission,
    },
    PromptPreparationFailed {
        admission: PromptAdmission,
        message: String,
    },
    PromptPreparationHandled {
        admission: PromptAdmission,
        restore_draft: Option<String>,
    },
    PromptRunCompleted {
        admission: PromptAdmission,
        run_id: u64,
    },
    PromptRunStopped {
        admission: PromptAdmission,
        run_id: u64,
        message: String,
    },
    PromptRunCancelled {
        admission: PromptAdmission,
        run_id: u64,
    },
    PromptRunFailed {
        admission: PromptAdmission,
        run_id: Option<u64>,
        message: String,
    },
    RunCompleted {
        run_id: u64,
    },
    RunStopped {
        run_id: u64,
        message: String,
    },
    RunCancelled {
        run_id: u64,
    },
    RunFailed {
        run_id: Option<u64>,
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
    /// Adapter-declared meaning of the reasoning stream.  The event is
    /// absent when the provider does not expose a trustworthy classification.
    ThinkingClassificationChanged {
        classification: slim_core::ReasoningClassification,
    },
    ThinkingStarted,
    ThinkingEnded,
    ActivityChanged {
        label: String,
    },
    ProviderPhaseChanged {
        phase: slim_core::ProviderPhase,
        label: String,
        elapsed_ms: u64,
    },
    RetryScheduled {
        attempt: u32,
        limit: u32,
        wait_ms: u64,
        reason: Option<String>,
    },
    CancellationRequested {
        run_id: u64,
    },
    CancellationStarted {
        run_id: u64,
    },
    ApprovalRequired {
        request_id: InteractionRequestId,
        summary: String,
        persisted: bool,
    },
    InputRequired {
        request_id: InteractionRequestId,
        prompt: String,
        options: Vec<String>,
        persisted: bool,
    },
    QuestionRequired {
        request_id: InteractionRequestId,
        question: String,
        options: Vec<slim_core::QuestionOption>,
        persisted: bool,
    },
    InteractionAcknowledged {
        request_id: InteractionRequestId,
        accepted: bool,
        message: String,
    },
    ToolStarted {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        name: String,
        arguments_summary: String,
    },
    ToolPrepared {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        name: String,
    },
    ToolAdmitted {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        name: String,
    },
    /// Final executor output. Unlike transient `ToolProgress`, this carries
    /// the result that must remain visible when cancellation closes a run.
    ToolOutput {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        name: String,
        output: String,
        content_handle: Option<ContentHandle>,
    },
    ToolProgress {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        name: String,
        preview: String,
        content_handle: Option<ContentHandle>,
    },
    /// Changed lines of a successful patch, shown in the expanded tool body.
    ToolDiff {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        diff: slim_core::ToolEditDiff,
    },
    ToolEnded {
        batch_id: ToolBatchId,
        call_id: ToolCallId,
        name: String,
        success: bool,
        duration_ms: u64,
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
        request_id: ContentRequestId,
        cursor: Option<PageCursor>,
        text: String,
        next_cursor: Option<PageCursor>,
    },
    ContentPageFailed {
        handle: ContentHandle,
        request_id: ContentRequestId,
        cursor: Option<PageCursor>,
        message: String,
    },
    /// The current session's name changed (`None` = unnamed). Sent after
    /// `/rename` and after every session switch or creation.
    SessionTitleChanged {
        title: Option<String>,
    },
    /// Answer to `ListSessions`. `error` set means the list could not be built.
    /// `now_ms` is the host clock (Unix epoch ms) when the list was built, so
    /// ages render without the TUI reading a wall clock.
    SessionsListed {
        request_id: u64,
        now_ms: u64,
        items: Vec<SessionListItem>,
        error: Option<String>,
    },
    /// Answer to `ListTurns`. `error` set means the list could not be built.
    TurnsListed {
        request_id: u64,
        items: Vec<TurnListItem>,
        error: Option<String>,
    },
    /// Last event of a `RunUserShell` request, sent whether the command ran,
    /// failed, was refused or was cancelled. Its output travelled as the usual
    /// `ToolStarted`/`ToolOutput`/`ToolEnded` events before it.
    UserShellFinished {
        request_id: u64,
    },
    /// Workspace-relative file paths for `@` completion (shallower first).
    /// `truncated` is set when the host stopped at its listing limit.
    WorkspaceFiles {
        request_id: u64,
        paths: Vec<String>,
        truncated: bool,
    },
    UsagePartial {
        input_tokens: u64,
        output_tokens: u64,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    UsageEstimate {
        request_id: u64,
        context_tokens: u64,
        context_window_tokens: u64,
    },
    UsageEstimateForRun {
        run_id: u64,
        request_id: u64,
        context_tokens: u64,
        context_window_tokens: u64,
    },
    RequestCompleted {
        provider_latency_ms: u64,
    },
    ModeChanged {
        mode: OperatingMode,
    },
    ModelChanged {
        model: String,
    },
    OpenCodeCatalogLoaded {
        models: Vec<OpenCodeModelView>,
        source: OpenCodeCatalogSource,
    },
    ClinePassCatalogLoaded {
        models: Vec<OpenCodeModelView>,
        source: ClinePassCatalogSource,
    },
    CommandCodeCatalogLoaded {
        models: Vec<OpenCodeModelView>,
        source: CommandCodeCatalogSource,
    },
    ZenCatalogLoaded {
        models: Vec<OpenCodeModelView>,
        source: ZenCatalogSource,
    },
    EffortChanged {
        effort: ReasoningEffort,
    },
    CodexSpeedChanged {
        fast: bool,
    },
    AuthStateChanged {
        provider: Option<LoginProvider>,
        authenticated: bool,
    },
    LoginProgress {
        message: String,
    },
    /// Terminal login failure: resets the overlay's `in_progress` so the
    /// user can retry instead of being stuck behind a spinner (G251).
    LoginFailed {
        message: String,
    },
    LoginUrl {
        url: SensitiveText,
        user_code: Option<SensitiveText>,
    },
    Notification {
        message: String,
    },
    JobsChanged {
        jobs: Vec<slim_core::runtime::ShellJobInfo>,
    },
    JobOutput {
        id: String,
        offset: Option<u64>,
        before: Option<u64>,
        output: Box<slim_core::runtime::ShellJobOutput>,
    },
    /// Fresh `/mcp` snapshot after any lifecycle change or poll tick.
    McpServersChanged {
        servers: Vec<McpServerView>,
    },
    /// A `/mcp login` sign-in is waiting for the user: show the full
    /// authorization URL and the pasted-redirect field.
    McpAuthorization {
        name: String,
        url: SensitiveText,
        browser_opened: bool,
    },
    /// The sign-in for `name` is over (success, failure, timeout or
    /// cancellation); the outcome arrives as a notification.
    McpLoginEnded {
        name: String,
    },
    /// Data-lane compaction checkpoint (§11.3): collapsed system block, not a toast.
    CompactionCompleted,
    CompactionState {
        state: slim_core::context::CompactionStatus,
        reason: slim_core::context::CompactionReason,
        tokens_before: u64,
        tokens_after: u64,
        duration_ms: u64,
    },
    FatalError {
        run_id: Option<u64>,
        message: String,
    },
    Shutdown,
}

impl UiEvent {
    pub const DEFAULT_MAX_MUTATING_TOOL_CALLS: usize = 32;
    pub const DEFAULT_MAX_READ_TOOL_CALLS: usize = 96;
    pub const DEFAULT_MAX_TURNS: usize = 128;

    pub fn run_started(run_id: u64) -> Self {
        Self::run_started_with_budget(
            run_id,
            Self::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            Self::DEFAULT_MAX_READ_TOOL_CALLS,
            Self::DEFAULT_MAX_TURNS,
        )
    }

    pub fn run_started_with_budget(
        run_id: u64,
        max_mutating_tool_calls: usize,
        max_read_tool_calls: usize,
        max_turns: usize,
    ) -> Self {
        Self::RunStarted {
            run_id,
            max_mutating_tool_calls,
            max_read_tool_calls,
            max_turns,
        }
    }

    /// Context and billing telemetry is durable causal state, not a visual
    /// delta. It stays ordered on the stream lane normally and migrates to a
    /// buffered control suffix only after cancellation.
    pub fn is_accounting_telemetry(&self) -> bool {
        matches!(
            self,
            Self::UsagePartial { .. }
                | Self::Usage { .. }
                | Self::UsageEstimate { .. }
                | Self::UsageEstimateForRun { .. }
                | Self::RequestCompleted { .. }
        )
    }

    pub fn is_causal_telemetry(&self) -> bool {
        self.is_accounting_telemetry() || matches!(self, Self::AssistantEnded)
    }

    /// Events that are part of an already-emitted causal suffix must not be
    /// discarded when cancellation interrupts projector backpressure.
    pub fn survives_cancellation(&self) -> bool {
        self.is_causal_telemetry()
            || matches!(
                self,
                Self::ToolStarted { .. }
                    | Self::ToolPrepared { .. }
                    | Self::ToolAdmitted { .. }
                    | Self::ToolOutput { .. }
                    | Self::ToolEnded { .. }
                    | Self::ApprovalRequired { .. }
                    | Self::InputRequired { .. }
                    | Self::QuestionRequired { .. }
                    | Self::InteractionAcknowledged { .. }
                    | Self::RequestCompleted { .. }
            )
    }

    pub fn is_run_terminal(&self) -> bool {
        matches!(
            self,
            Self::RunCompleted { .. }
                | Self::RunStopped { .. }
                | Self::RunCancelled { .. }
                | Self::RunFailed { .. }
        )
    }

    pub fn from_core(event: slim_core::SessionEvent) -> Option<Self> {
        let request_id = event.seq;
        match event.kind {
            slim_core::EventKind::ShellJobChanged { .. } => None,
            slim_core::EventKind::SessionStarted { session_id } => Some(Self::SessionSnapshot {
                session_id: SessionId(session_id.into()),
                cwd: String::new(),
                skill_names: Vec::new(),
            }),
            slim_core::EventKind::ModeChanged { mode } => Some(Self::ModeChanged { mode }),
            slim_core::EventKind::AssistantTextDelta { text } => {
                Some(Self::AssistantDelta { text })
            }
            slim_core::EventKind::ReasoningDelta { text } => Some(Self::ThinkingDelta { text }),
            slim_core::EventKind::ReasoningClassification { classification } => {
                Some(Self::ThinkingClassificationChanged { classification })
            }
            slim_core::EventKind::ThinkingStarted => Some(Self::ThinkingStarted),
            slim_core::EventKind::ThinkingEnded => Some(Self::ThinkingEnded),
            slim_core::EventKind::ProviderPhase {
                phase,
                elapsed_ms,
                detail,
            } => {
                let label = match phase {
                    slim_core::ProviderPhase::Compacting => detail
                        .filter(|text| !text.trim().is_empty())
                        .unwrap_or_else(|| "Compacting context".to_owned()),
                    slim_core::ProviderPhase::Connecting => detail
                        .filter(|text| !text.trim().is_empty())
                        .unwrap_or_else(|| "Connecting to provider".to_owned()),
                    slim_core::ProviderPhase::HeadersReceived => {
                        "Waiting for first byte".to_owned()
                    }
                    slim_core::ProviderPhase::FirstByte => {
                        "Stream open · waiting for content".to_owned()
                    }
                    slim_core::ProviderPhase::FirstSemantic => "Provider responding".to_owned(),
                    slim_core::ProviderPhase::PreparingTool => detail.map_or_else(
                        || "Preparing tool".to_owned(),
                        |name| format!("Preparing tool · {name}"),
                    ),
                };
                Some(Self::ProviderPhaseChanged {
                    phase,
                    label,
                    elapsed_ms,
                })
            }
            slim_core::EventKind::RetryScheduled {
                attempt,
                limit,
                wait_ms,
                reason,
            } => Some(Self::RetryScheduled {
                attempt,
                limit,
                wait_ms,
                reason: reason.map(|value| bounded_first_line(&value, 4 * 1024)),
            }),
            slim_core::EventKind::AssistantEnded { .. } => Some(Self::AssistantEnded),
            slim_core::EventKind::UsagePartial {
                input_tokens,
                output_tokens,
                ..
            } => Some(Self::UsagePartial {
                input_tokens,
                output_tokens,
            }),
            slim_core::EventKind::Usage {
                input_tokens,
                output_tokens,
            } => Some(Self::Usage {
                input_tokens,
                output_tokens,
            }),
            slim_core::EventKind::ToolStarted {
                batch_id,
                call_id,
                name,
                arguments,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 0, batch_id, call_id);
                let mut arguments_summary =
                    slim_core::tools::summarize_tool_arguments_for(&name, &arguments);
                // Edit size travels as one more summary segment; the tool
                // row styles it (see `edit_stats_segment`).
                if let Some((added, removed)) = slim_core::tools::edit_line_stats(&name, &arguments)
                {
                    arguments_summary.push_str(&format!(" · +{added} -{removed}"));
                }
                Some(Self::ToolStarted {
                    batch_id,
                    call_id,
                    name,
                    arguments_summary,
                })
            }
            slim_core::EventKind::ToolPrepared {
                batch_id,
                call_id,
                name,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 0, batch_id, call_id);
                Some(Self::ToolPrepared {
                    batch_id,
                    call_id,
                    name,
                })
            }
            slim_core::EventKind::ToolAdmitted {
                batch_id,
                call_id,
                name,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 1, batch_id, call_id);
                Some(Self::ToolAdmitted {
                    batch_id,
                    call_id,
                    name,
                })
            }
            // Provider discovery/call events precede executor ToolStarted for
            // the same call. Only the executor boundary owns the UI lifecycle.
            slim_core::EventKind::ToolCall { .. }
            | slim_core::EventKind::ProviderToolCall { .. } => None,
            slim_core::EventKind::ToolOutput {
                batch_id,
                call_id,
                name,
                output,
            }
            | slim_core::EventKind::ToolJobOutput {
                batch_id,
                call_id,
                name,
                output,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 1, batch_id, call_id);
                Some(Self::ToolOutput {
                    // A handle is attached only by a bridge that has actually
                    // registered this output in its bounded content store.
                    content_handle: None,
                    batch_id,
                    call_id,
                    output: tool_output_preview(&name, &output),
                    name,
                })
            }
            slim_core::EventKind::ToolProgress {
                batch_id,
                call_id,
                name,
                preview,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 1, batch_id, call_id);
                Some(Self::ToolProgress {
                    content_handle: None,
                    batch_id,
                    call_id,
                    name,
                    preview: bounded_first_line(&preview, 512),
                })
            }
            slim_core::EventKind::ToolProcessFinished {
                batch_id,
                call_id,
                name,
                process,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 1, batch_id, call_id);
                Some(Self::ToolProgress {
                    // Process facts arrive after ToolOutput. Keep any
                    // inspector handle already attached to that output while
                    // replacing the compact preview with typed status.
                    content_handle: None,
                    batch_id,
                    call_id,
                    name,
                    preview: process_preview(&process),
                })
            }
            slim_core::EventKind::ToolEditApplied {
                batch_id,
                call_id,
                name: _,
                diff,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 1, batch_id, call_id);
                Some(Self::ToolDiff {
                    batch_id,
                    call_id,
                    diff,
                })
            }
            slim_core::EventKind::ToolFinished {
                batch_id,
                call_id,
                name,
                success,
                duration_ms,
            } => {
                let (batch_id, call_id) = projected_tool_identity(request_id, 2, batch_id, call_id);
                Some(Self::ToolEnded {
                    batch_id,
                    call_id,
                    name,
                    success,
                    duration_ms,
                })
            }
            slim_core::EventKind::CausalProgressObserved { .. }
            | slim_core::EventKind::CausalBoundaryObserved { .. }
            | slim_core::EventKind::CausalAnomalyDetected { .. }
            | slim_core::EventKind::UsageBreakdown { .. }
            | slim_core::EventKind::ResponseCacheHit
            | slim_core::EventKind::GoalAssurance { .. }
            | slim_core::EventKind::ToolEvidenceReused { .. }
            | slim_core::EventKind::ToolEvidenceElided { .. }
            | slim_core::EventKind::ToolCallsSuppressed { .. }
            | slim_core::EventKind::ArtifactStored { .. } => None,
            slim_core::EventKind::RequestCompleted {
                provider_latency_ms,
                cancelled,
                failed,
            } => {
                if !cancelled && !failed && provider_latency_ms > 0 {
                    Some(Self::RequestCompleted {
                        provider_latency_ms,
                    })
                } else {
                    None
                }
            }
            slim_core::EventKind::CompactionCompleted => Some(Self::CompactionCompleted),
            slim_core::EventKind::CompactionUsageUnknown { .. } => None,
            slim_core::EventKind::CompactionState {
                state,
                reason,
                tokens_before,
                tokens_after,
                duration_ms,
            } => Some(Self::CompactionState {
                state,
                reason,
                tokens_before,
                tokens_after,
                duration_ms,
            }),
            slim_core::EventKind::ApprovalRequired {
                request_id,
                summary,
                persisted,
            } => Some(Self::ApprovalRequired {
                request_id: projected_interaction_id(&request_id)?,
                summary: bounded_first_line(&summary, 16 * 1024),
                persisted,
            }),
            slim_core::EventKind::InputRequired {
                request_id,
                prompt,
                options,
                persisted,
            } => Some(Self::InputRequired {
                request_id: projected_interaction_id(&request_id)?,
                prompt: bounded_first_line(&prompt, 16 * 1024),
                options: options
                    .into_iter()
                    .take(16)
                    .map(|option| bounded_first_line(&option, 1_024))
                    .collect(),
                persisted,
            }),
            slim_core::EventKind::QuestionRequired {
                request_id,
                question,
                options,
                persisted,
            } => Some(Self::QuestionRequired {
                request_id: projected_interaction_id(&request_id)?,
                question: bounded_first_line(&question, slim_core::MAX_QUESTION_CHARS),
                options: options
                    .into_iter()
                    .take(slim_core::MAX_QUESTION_OPTIONS)
                    .map(|option| slim_core::QuestionOption {
                        label: bounded_first_line(&option.label, slim_core::MAX_OPTION_LABEL_CHARS),
                        description: bounded_first_line(
                            &option.description,
                            slim_core::MAX_OPTION_DESCRIPTION_CHARS,
                        ),
                    })
                    .collect(),
                persisted,
            }),
            slim_core::EventKind::InteractionAcknowledged {
                request_id,
                accepted,
                message,
            } => Some(Self::InteractionAcknowledged {
                request_id: projected_interaction_id(&request_id)?,
                accepted,
                message: bounded_first_line(&message, 16 * 1024),
            }),
            slim_core::EventKind::SubagentActivity { message } => {
                Some(Self::ActivityChanged { label: message })
            }
            slim_core::EventKind::TodoChanged { items } => Some(Self::TodoChanged {
                items: items
                    .into_iter()
                    .map(|item| TodoItemView {
                        reason: item.reason,
                        id: item.id,
                        title: item.title,
                        status: match item.status.as_str() {
                            "in_progress" => TodoItemStatus::InProgress,
                            "completed" => TodoItemStatus::Completed,
                            "blocked" => TodoItemStatus::Blocked,
                            "cancelled" => TodoItemStatus::Cancelled,
                            _ => TodoItemStatus::Pending,
                        },
                    })
                    .collect(),
            }),
            // The host reports this bounded stop after finalization, with its
            // full blocker context. It is not an unexpected worker failure.
            slim_core::EventKind::TerminalError { message }
                if message == "repeated failed tool call blocked" =>
            {
                None
            }
            slim_core::EventKind::TerminalError { message } => Some(Self::FatalError {
                run_id: None,
                message,
            }),
            slim_core::EventKind::ContextSnapshot {
                estimated_tokens,
                context_window_tokens,
                ..
            } => Some(Self::UsageEstimate {
                request_id,
                context_tokens: estimated_tokens,
                context_window_tokens,
            }),
        }
    }
}

fn projected_tool_identity(
    event_seq: u64,
    phase_offset: u64,
    batch_id: String,
    call_id: String,
) -> (ToolBatchId, ToolCallId) {
    if !batch_id.is_empty() && !call_id.is_empty() {
        return (ToolBatchId(batch_id.into()), ToolCallId(call_id.into()));
    }
    // Legacy executor logs omitted both IDs but emitted the three lifecycle
    // events at consecutive sequence numbers. Anchor every phase to Started.
    let start_seq = event_seq.checked_sub(phase_offset).unwrap_or(event_seq);
    (
        ToolBatchId(format!("legacy-batch-{start_seq}").into()),
        ToolCallId(format!("legacy-call-{start_seq}").into()),
    )
}

fn process_preview(process: &slim_core::process::ProcessExecutionFacts) -> String {
    let mut parts = vec![format!(
        "exit {}",
        process
            .exit_code
            .map_or_else(|| "n/a".to_owned(), |code| code.to_string())
    )];
    if process.timed_out {
        parts.push("timed out".into());
    }
    if process.cancelled {
        parts.push("cancelled".into());
    }
    if process.capture_may_be_incomplete {
        parts.push("capture may be incomplete".into());
    }
    let discarded = process
        .stdout_discarded_bytes
        .saturating_add(process.stderr_discarded_bytes);
    if discarded > 0 {
        parts.push(format!("discarded {discarded} B"));
    }
    parts.join(" · ")
}

/// Bounded summary for the transcript; the complete output remains in the content store.
pub fn tool_output_preview(name: &str, output: &str) -> String {
    // Core prefixes admission notes as one `[admission: …]` line; the preview
    // describes the result that follows it.
    let output = output
        .strip_prefix("[admission: ")
        .and_then(|rest| rest.split_once("]\n"))
        .map(|(_, result)| result)
        .filter(|result| !result.trim().is_empty())
        .unwrap_or(output);
    if name == "todo" {
        if let Some(reason) = output
            .lines()
            .find(|line| line.starts_with("todo rejected ("))
        {
            return bounded_first_line(reason, 512);
        }
    }
    if name == "shell" {
        let status = output.lines().next().unwrap_or_default();
        if status.starts_with("exit ") && status != "exit 0" {
            let reason = output
                .split_once("\nstderr:\n")
                .and_then(|(_, stderr)| stderr.lines().find(|line| !line.trim().is_empty()))
                .or_else(|| {
                    output
                        .split_once("\nstdout:\n")
                        .and_then(|(_, rest)| rest.split("stderr:\n").next())
                        .and_then(|stdout| stdout.lines().find(|line| !line.trim().is_empty()))
                });
            if let Some(reason) = reason {
                return bounded_first_line(&format!("{status} · {}", reason.trim()), 512);
            }
        }
    }
    bounded_first_line(output, 512)
}

fn bounded_first_line(output: &str, limit: usize) -> String {
    let mut chars = output.lines().next().unwrap_or("").chars();
    let mut preview = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn projected_interaction_id(value: &str) -> Option<InteractionRequestId> {
    let trimmed = value.trim();
    (!trimmed.is_empty()
        && trimmed == value
        && value.chars().count() <= 256
        && !value.chars().any(char::is_control))
    .then(|| InteractionRequestId(Arc::from(value)))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiCommand {
    JobsRefresh,
    /// Host idle timer; the controller serializes the durable job writer.
    PersistJobs,
    JobOutput {
        id: String,
        offset: Option<u64>,
        before: Option<u64>,
    },
    JobControl {
        id: String,
        interrupt: bool,
    },
    RunBackgroundShell {
        command: String,
    },
    SendPrompt(String),
    /// Admission-aware prompt path used by the reducer. The legacy
    /// `SendPrompt` remains for existing programmatic bridge callers.
    PreparePrompt {
        prompt: String,
        admission: PromptAdmission,
    },
    CancelPromptPreparation {
        admission: PromptAdmission,
    },
    RetryProvider,
    SaveModelDefault,
    ResumePrevious,
    AttachImage(String),
    Compact {
        instructions: String,
    },
    AnswerInput {
        request_id: InteractionRequestId,
        answer: String,
    },
    AnswerQuestion {
        request_id: InteractionRequestId,
        answer: slim_core::QuestionAnswer,
    },
    Approve {
        request_id: InteractionRequestId,
    },
    Reject {
        request_id: InteractionRequestId,
    },
    StartLogin(LoginProvider),
    SaveApiKey {
        provider: LoginProvider,
        api_key: SensitiveText,
    },
    CancelLogin,
    Logout,
    SetMode(OperatingMode),
    SetModel {
        model: ModelAlias,
        effort: ReasoningEffort,
        fast: bool,
    },
    RefreshOpenCodeModels,
    RefreshZenModels,
    RefreshClinePassModels,
    RefreshCommandCodeModels,
    SetOpenCodeModel {
        model: String,
        effort: ReasoningEffort,
    },
    SetZenModel {
        model: String,
        effort: ReasoningEffort,
    },
    SetClinePassModel {
        model: String,
        effort: ReasoningEffort,
    },
    SetCommandCodeModel {
        model: String,
        effort: ReasoningEffort,
    },
    SetXaiModel {
        model: String,
        effort: ReasoningEffort,
    },
    CancelRun,
    Shutdown,
    /// Reload layered config into the MCP manager and push a fresh snapshot.
    McpRefresh,
    /// Connect (lazily) and count tools; the connection stays warm.
    McpTest {
        name: String,
    },
    McpReconnect {
        name: String,
    },
    McpDisconnect {
        name: String,
    },
    /// Remove from config file and drop the live entry.
    McpRemove {
        name: String,
    },
    /// Persist a new server into slim.toml (project, or global when
    /// `global`) and add it to the manager.
    McpAdd {
        name: String,
        command: Option<String>,
        args: Vec<String>,
        url: Option<String>,
        global: bool,
    },
    /// Record the trust decision for the project's MCP servers
    /// (`/mcp trust` = true, `/mcp untrust` = never) and reload them. The
    /// decision covers the whole project; `name` (optional) must be a
    /// configured server and is only echoed back.
    McpTrust {
        trust: bool,
        name: Option<String>,
    },
    /// Cheap status polling while the `/mcp` overlay is open.
    McpWatch {
        on: bool,
    },
    RequestContentPage {
        handle: ContentHandle,
        request_id: ContentRequestId,
        cursor: Option<PageCursor>,
    },
    /// `/rename TITLE`: names the current session (empty clears the name). The
    /// host persists it beside the session and answers `SessionTitleChanged`.
    RenameSession {
        title: String,
    },
    /// Lists the workspace's resumable sessions (`/resume` overlay); answered by
    /// `UiEvent::SessionsListed` with the same `request_id`.
    ListSessions {
        request_id: u64,
    },
    /// Resumes the session whose id is `id` (as listed). Replaces the current
    /// conversation exactly like `ResumePrevious`, which stays for startup.
    ResumeSession {
        id: String,
    },
    /// Lists the current session's finished turns (`/rewind` overlay); answered
    /// by `UiEvent::TurnsListed`.
    ListTurns {
        request_id: u64,
    },
    /// Goes back to just before the turn whose user entry has sequence
    /// `first_seq`: forks the session at that boundary (the original is left
    /// untouched), switches to the fork and hands that turn's prompt back as a
    /// draft. Conversation only; files are not restored.
    RewindSession {
        first_seq: u64,
    },
    /// Run `command` (a `!command` draft) in the workspace shell on the user's
    /// behalf. Auto mode only; the host always ends it with
    /// `UiEvent::UserShellFinished` carrying the same `request_id`.
    RunUserShell {
        request_id: u64,
        command: String,
    },
    /// Enumerate workspace files for `@` completion; answered by
    /// `UiEvent::WorkspaceFiles` carrying the same `request_id`.
    RequestWorkspaceFiles {
        request_id: u64,
    },
    /// `/mcp login <name>`: OAuth sign-in for an HTTP MCP server (opens the
    /// browser, waits for the loopback callback). With `redirect_url`, hands
    /// a pasted redirect URL to the sign-in already running for `name`.
    McpLogin {
        name: String,
        redirect_url: Option<String>,
    },
    /// `/mcp logout <name>`: deletes the stored OAuth credentials.
    McpLogout {
        name: String,
    },
    /// Stops the sign-in running for `name` (the login panel was dismissed).
    McpLoginCancel {
        name: String,
    },
    /// `/mcp enable|disable <name>`: writes `enabled` into the config file
    /// that defines the server and reloads it.
    McpEnable {
        name: String,
        enabled: bool,
    },
}

struct WakeInner {
    /// Set while the consumer sits in the drain→wait gap (armed). Producers
    /// use `notify_waiter` on hot paths so a flood of lane events does not
    /// pay a signal syscall per event while the consumer is busy.
    waiting: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    event: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(unix)]
    reader: std::os::unix::net::UnixStream,
    #[cfg(unix)]
    writer: std::os::unix::net::UnixStream,
}

#[cfg(windows)]
// SAFETY: Windows event handles may be signaled and waited from different
// threads; ownership remains uniquely managed by the Arc-backed WakeSignal.
unsafe impl Send for WakeInner {}
#[cfg(windows)]
unsafe impl Sync for WakeInner {}

#[cfg(windows)]
impl Drop for WakeInner {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

/// Cross-thread wake signal shared by event producers and the terminal wait
/// loop. It prevents quiescent polling without delaying bridge events.
#[derive(Clone)]
pub struct WakeSignal(Arc<WakeInner>);

#[cfg(windows)]
fn finite_wait_timeout_ms(timeout: std::time::Duration) -> u32 {
    use windows_sys::Win32::System::Threading::INFINITE;

    if timeout.is_zero() {
        0
    } else {
        timeout.as_millis().clamp(1, (INFINITE - 1) as u128) as u32
    }
}

#[cfg(unix)]
fn poll_timeout_ms(timeout: std::time::Duration) -> i32 {
    if timeout.is_zero() {
        0
    } else {
        i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX)
    }
}

impl WakeSignal {
    #[cfg(windows)]
    pub fn new() -> std::io::Result<Self> {
        let event = unsafe {
            windows_sys::Win32::System::Threading::CreateEventW(
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
            )
        };
        if event.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self(Arc::new(WakeInner {
                waiting: std::sync::atomic::AtomicBool::new(false),
                event,
            })))
        }
    }

    /// Unix wake: a non-blocking socketpair whose read end the wait loop
    /// `poll`s together with terminal input (fd 0).
    #[cfg(unix)]
    pub fn new() -> std::io::Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        Ok(Self(Arc::new(WakeInner {
            waiting: std::sync::atomic::AtomicBool::new(false),
            reader,
            writer,
        })))
    }

    #[cfg(not(any(windows, unix)))]
    pub fn new() -> std::io::Result<Self> {
        Ok(Self(Arc::new(WakeInner {
            waiting: std::sync::atomic::AtomicBool::new(false),
        })))
    }

    /// Unconditional signal. Used by one-shot producers (clipboard results,
    /// lane-space release, loop self-wake) where the auto-reset/pipe state
    /// must latch even when no waiter is armed yet.
    pub fn notify(&self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.0.event);
        }
        #[cfg(unix)]
        {
            use std::io::Write;
            // Non-blocking: a full pipe already means a pending signal.
            let _ = (&self.0.writer).write_all(&[1u8]);
        }
        #[cfg(not(any(windows, unix)))]
        {}
    }

    /// Hot-path signal: skips the platform call unless the consumer is armed
    /// for a wait. The consumer arms before its final lane probe, so a send
    /// racing the drain→wait gap still wakes it.
    pub fn notify_waiter(&self) {
        if self.0.waiting.load(std::sync::atomic::Ordering::Acquire) {
            self.notify();
        }
    }

    /// Marks the consumer as about to wait. Callers must arm, then probe the
    /// lanes/input once more, then wait — the probe is what closes the race
    /// against producers that arrived before the arm.
    pub(crate) fn arm(&self) {
        self.0
            .waiting
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn disarm(&self) {
        self.0
            .waiting
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Read end of the wake pipe, for `poll`-based terminal waits.
    #[cfg(unix)]
    pub(crate) fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.0.reader.as_raw_fd()
    }

    /// Empties the notification pipe after a wakeup.
    #[cfg(unix)]
    pub(crate) fn drain_pipe(&self) {
        use std::io::Read;
        let mut buf = [0u8; 64];
        loop {
            match (&self.0.reader).read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    }

    #[cfg(windows)]
    pub(crate) fn raw_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0.event
    }

    /// Waits for and consumes one notification from this signal.
    ///
    /// This single-handle form is primarily useful for deterministic bridge
    /// tests; the TUI runtime waits on this handle together with console input.
    #[cfg(windows)]
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> std::io::Result<bool> {
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let timeout_ms = finite_wait_timeout_ms(timeout);
        // SAFETY: WakeInner owns a valid event handle for this call's duration;
        // WaitForSingleObject does not transfer ownership.
        match unsafe { WaitForSingleObject(self.0.event, timeout_ms) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(std::io::Error::last_os_error()),
            outcome => Err(std::io::Error::other(format!(
                "unexpected wake wait outcome {outcome}"
            ))),
        }
    }

    #[cfg(unix)]
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> std::io::Result<bool> {
        let timeout_ms = poll_timeout_ms(timeout);
        let mut fd = libc::pollfd {
            fd: self.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `fd` points at a valid pollfd for the call's duration.
            let ready = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(false);
            }
            self.drain_pipe();
            return Ok(fd.revents & libc::POLLIN != 0);
        }
    }

    #[cfg(not(any(windows, unix)))]
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> std::io::Result<bool> {
        std::thread::sleep(timeout);
        Ok(false)
    }
}

impl Default for WakeSignal {
    fn default() -> Self {
        Self::new().expect("create TUI wake signal")
    }
}

/// Capacity of the ordered stream lane. Runtime barriers account for one
/// additional event held in their prefetch slot.
pub const STREAM_EVENT_CAPACITY: usize = 1_024;

pub struct UiChannels {
    pub commands: mpsc::Sender<UiCommand>,
    pub wake: WakeSignal,
    /// Projector backpressure: the UI notifies after draining a lane batch so
    /// a full stream/control send can resume without sleeping blindly.
    pub lane_space: WakeSignal,
    /// Immediate control lane (§10.1): cancellation, auth and global state —
    /// lossless, bounded, always drained first. Causal run errors stay ordered
    /// with their stream.
    pub events: mpsc::Receiver<UiEvent>,
    /// Ordered stream lane (§10.1): starts, deltas, tool lifecycle, usage and
    /// terminals share one bounded lossless order; only adjacent deltas may
    /// be coalesced.
    pub events_data: mpsc::Receiver<UiEvent>,
}

impl UiEvent {
    /// Immediate control events bypass stream backlogs. Ordered run lifecycle
    /// shares the bounded lossless stream lane with its deltas so starts and
    /// terminals cannot overtake their own content. Only adjacent delta kinds
    /// are coalesced; lifecycle and errors are never dropped.
    pub fn is_control(&self) -> bool {
        !matches!(
            self,
            Self::RunStarted { .. }
                | Self::PromptRunStarted { .. }
                | Self::SessionRestored { .. }
                | Self::JobsChanged { .. }
                | Self::JobOutput { .. }
                | Self::PromptRunCompleted { .. }
                | Self::PromptRunStopped { .. }
                | Self::PromptRunCancelled { .. }
                | Self::PromptRunFailed { .. }
                | Self::RunCompleted { .. }
                | Self::RunStopped { .. }
                | Self::RunFailed { .. }
                | Self::UserMessageAdded { .. }
                | Self::AssistantDelta { .. }
                | Self::AssistantEnded
                | Self::ThinkingStarted
                | Self::ThinkingDelta { .. }
                | Self::ThinkingClassificationChanged { .. }
                | Self::ThinkingEnded
                | Self::UsagePartial { .. }
                | Self::Usage { .. }
                | Self::UsageEstimate { .. }
                | Self::UsageEstimateForRun { .. }
                | Self::RequestCompleted { .. }
                | Self::ToolStarted { .. }
                | Self::ToolPrepared { .. }
                | Self::ToolAdmitted { .. }
                | Self::ToolOutput { .. }
                | Self::ToolProgress { .. }
                | Self::ToolDiff { .. }
                | Self::UserShellFinished { .. }
                // Both follow a `SessionRestored` (resume, rewind): on the control
                // lane they could be reduced before it and then wiped by it.
                | Self::SessionTitleChanged { .. }
                | Self::RestoreDraft { .. }
                | Self::ToolEnded { .. }
                | Self::ActivityChanged { .. }
                | Self::ProviderPhaseChanged { .. }
                | Self::RetryScheduled { .. }
                | Self::ApprovalRequired { .. }
                | Self::InputRequired { .. }
                | Self::QuestionRequired { .. }
                | Self::InteractionAcknowledged { .. }
                | Self::CancellationRequested { .. }
                | Self::CancellationStarted { .. }
                | Self::FatalError { .. }
                | Self::Notification { .. }
                | Self::CompactionCompleted
                | Self::CompactionState { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use slim_core::{EventKind, SessionEvent};

    use super::{
        InteractionRequestId, ModelAlias, ReasoningEffort, SessionId, TodoItemStatus, TodoItemView,
        ToolBatchId, ToolCallId, UiEvent,
    };

    #[test]
    fn only_executor_start_projects_a_tool_start() {
        let provider_call = SessionEvent::new(
            1,
            EventKind::ProviderToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        );
        let executor_start = SessionEvent::new(
            2,
            EventKind::ToolStarted {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        );
        assert_eq!(UiEvent::from_core(provider_call), None);
        assert_eq!(
            UiEvent::from_core(executor_start),
            Some(UiEvent::ToolStarted {
                batch_id: ToolBatchId("batch-1".into()),
                call_id: ToolCallId("call-1".into()),
                name: "read".into(),
                arguments_summary: String::new(),
            })
        );
    }

    #[test]
    fn typed_tool_lifecycle_and_retry_project_without_losing_identity() {
        let prepared = UiEvent::from_core(SessionEvent::new(
            2,
            EventKind::ToolPrepared {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "read".into(),
            },
        ));
        assert_eq!(
            prepared,
            Some(UiEvent::ToolPrepared {
                batch_id: ToolBatchId("batch-1".into()),
                call_id: ToolCallId("call-1".into()),
                name: "read".into(),
            })
        );
        let admitted = UiEvent::from_core(SessionEvent::new(
            3,
            EventKind::ToolAdmitted {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "read".into(),
            },
        ));
        assert_eq!(
            admitted,
            Some(UiEvent::ToolAdmitted {
                batch_id: ToolBatchId("batch-1".into()),
                call_id: ToolCallId("call-1".into()),
                name: "read".into(),
            })
        );
        assert_eq!(
            UiEvent::from_core(SessionEvent::new(
                4,
                EventKind::RetryScheduled {
                    attempt: 1,
                    limit: 2,
                    wait_ms: 30_000,
                    reason: Some("transient provider failure".into()),
                },
            )),
            Some(UiEvent::RetryScheduled {
                attempt: 1,
                limit: 2,
                wait_ms: 30_000,
                reason: Some("transient provider failure".into()),
            })
        );
    }

    #[test]
    fn reasoning_classification_projects_only_the_adapter_claim() {
        assert_eq!(
            UiEvent::from_core(SessionEvent::new(
                5,
                EventKind::ReasoningClassification {
                    classification: slim_core::ReasoningClassification::Summary,
                },
            )),
            Some(UiEvent::ThinkingClassificationChanged {
                classification: slim_core::ReasoningClassification::Summary,
            })
        );
    }

    #[test]
    fn artifact_stored_is_not_projected_as_a_toast() {
        let event = SessionEvent::new(
            3,
            EventKind::ArtifactStored {
                id: "art-1".into(),
                size: 12_345_678,
            },
        );
        assert_eq!(UiEvent::from_core(event), None);
    }

    #[test]
    fn process_finished_projects_typed_status_on_tool_progress_lane() {
        let event = SessionEvent::new(
            4,
            EventKind::ToolProcessFinished {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "shell".into(),
                process: slim_core::process::ProcessExecutionFacts {
                    exit_code: Some(7),
                    timed_out: true,
                    cancelled: false,
                    capture_may_be_incomplete: false,
                    stdout_bytes: 12,
                    stderr_bytes: 8,
                    stdout_discarded_bytes: 3,
                    stderr_discarded_bytes: 4,
                },
            },
        );
        assert_eq!(
            UiEvent::from_core(event),
            Some(UiEvent::ToolProgress {
                content_handle: None,
                batch_id: ToolBatchId("batch-1".into()),
                call_id: ToolCallId("call-1".into()),
                name: "shell".into(),
                preview: "exit 7 · timed out · discarded 7 B".into(),
            })
        );
    }

    #[test]
    fn terminal_tool_output_survives_cancellation_but_progress_does_not() {
        let output = UiEvent::ToolOutput {
            batch_id: ToolBatchId("batch-1".into()),
            call_id: ToolCallId("call-1".into()),
            name: "mcp".into(),
            output: "mcp operation outcome is uncertain".into(),
            content_handle: None,
        };
        let progress = UiEvent::ToolProgress {
            batch_id: ToolBatchId("batch-1".into()),
            call_id: ToolCallId("call-1".into()),
            name: "mcp".into(),
            preview: "connecting".into(),
            content_handle: None,
        };
        assert!(output.survives_cancellation());
        assert!(!progress.survives_cancellation());
    }

    #[test]
    fn compaction_completed_is_not_a_toast() {
        let projected = UiEvent::from_core(SessionEvent::new(4, EventKind::CompactionCompleted));
        assert_eq!(projected, Some(UiEvent::CompactionCompleted));
        assert!(!UiEvent::CompactionCompleted.is_control());
    }

    #[test]
    fn tool_arguments_summarize_path_not_raw_json() {
        let event = SessionEvent::new(
            5,
            EventKind::ToolStarted {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: r#"{"path":"src/lib.rs","unused":true}"#.into(),
            },
        );
        assert_eq!(
            UiEvent::from_core(event),
            Some(UiEvent::ToolStarted {
                batch_id: ToolBatchId("batch-1".into()),
                call_id: ToolCallId("call-1".into()),
                name: "read".into(),
                arguments_summary: "path=src/lib.rs".into(),
            })
        );
    }

    #[test]
    fn tool_arguments_summarize_in_progress_todo() {
        let event = SessionEvent::new(
            6,
            EventKind::ToolStarted {
                batch_id: "batch-1".into(),
                call_id: "call-2".into(),
                name: "todo".into(),
                arguments: r#"{"todos":[{"id":"investigate","content":"map the leak","status":"in_progress"}]}"#.into(),
            },
        );
        match UiEvent::from_core(event) {
            Some(UiEvent::ToolStarted {
                arguments_summary, ..
            }) => {
                assert_eq!(arguments_summary, "map the leak");
                assert!(!arguments_summary.contains('{'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn todo_changed_projects_dock_items() {
        let event = SessionEvent::new(
            9,
            EventKind::TodoChanged {
                items: vec![slim_core::TodoChangedItem {
                    reason: None,
                    id: None,
                    title: "ship n2".into(),
                    status: "in_progress".into(),
                }],
            },
        );
        assert_eq!(
            UiEvent::from_core(event),
            Some(UiEvent::TodoChanged {
                items: vec![TodoItemView {
                    reason: None,
                    id: None,
                    title: "ship n2".into(),
                    status: TodoItemStatus::InProgress,
                }],
            })
        );
    }

    #[test]
    fn legacy_tool_lifecycle_gets_one_deterministic_nonempty_identity() {
        let events = [
            SessionEvent::new(
                10,
                EventKind::ToolStarted {
                    batch_id: String::new(),
                    call_id: String::new(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            ),
            SessionEvent::new(
                11,
                EventKind::ToolOutput {
                    batch_id: String::new(),
                    call_id: String::new(),
                    name: "read".into(),
                    output: "ok".into(),
                },
            ),
            SessionEvent::new(
                12,
                EventKind::ToolFinished {
                    batch_id: String::new(),
                    call_id: String::new(),
                    name: "read".into(),
                    success: true,
                    duration_ms: 1,
                },
            ),
        ];
        let projected = events
            .into_iter()
            .map(UiEvent::from_core)
            .collect::<Option<Vec<_>>>()
            .expect("projected lifecycle");
        let identities = projected
            .iter()
            .map(|event| match event {
                UiEvent::ToolStarted {
                    batch_id, call_id, ..
                }
                | UiEvent::ToolOutput {
                    batch_id, call_id, ..
                }
                | UiEvent::ToolProgress {
                    batch_id, call_id, ..
                }
                | UiEvent::ToolEnded {
                    batch_id, call_id, ..
                } => (batch_id.clone(), call_id.clone()),
                _ => panic!("tool lifecycle"),
            })
            .collect::<Vec<_>>();
        assert!(!identities[0].0 .0.is_empty());
        assert!(!identities[0].1 .0.is_empty());
        assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn input_required_projects_typed_waiting_event() {
        let legacy = SessionEvent::new(
            1,
            EventKind::InputRequired {
                request_id: String::new(),
                prompt: String::new(),
                options: Vec::new(),
                persisted: false,
            },
        );
        assert_eq!(UiEvent::from_core(legacy), None);
        assert_eq!(
            UiEvent::from_core(SessionEvent::new(
                2,
                EventKind::InputRequired {
                    request_id: "input-1".into(),
                    prompt: "Choose".into(),
                    options: vec!["core".into(), "tui".into()],
                    persisted: true,
                },
            )),
            Some(UiEvent::InputRequired {
                request_id: InteractionRequestId("input-1".into()),
                prompt: "Choose".into(),
                options: vec!["core".into(), "tui".into()],
                persisted: true,
            })
        );
    }

    #[test]
    fn interaction_projection_rejects_ambiguous_or_oversized_identity() {
        for request_id in [" spaced".to_owned(), "x".repeat(257)] {
            let event = SessionEvent::new(
                1,
                EventKind::ApprovalRequired {
                    request_id,
                    summary: "approve".into(),
                    persisted: false,
                },
            );
            assert_eq!(UiEvent::from_core(event), None);
        }
    }

    #[test]
    fn thinking_boundaries_project_on_the_ordered_stream_lane() {
        for (kind, expected) in [
            (EventKind::ThinkingStarted, UiEvent::ThinkingStarted),
            (EventKind::ThinkingEnded, UiEvent::ThinkingEnded),
        ] {
            let projected = UiEvent::from_core(SessionEvent::new(1, kind));
            assert_eq!(projected, Some(expected.clone()));
            assert!(!expected.is_control());
        }
    }

    #[test]
    fn provider_phases_project_to_truthful_activity_labels() {
        let cases = [
            (
                slim_core::ProviderPhase::Compacting,
                None,
                "Compacting context",
            ),
            (
                slim_core::ProviderPhase::Connecting,
                None,
                "Connecting to provider",
            ),
            (
                slim_core::ProviderPhase::Connecting,
                Some("Retrying provider (1/2); waiting 30000 ms".to_owned()),
                "Retrying provider (1/2); waiting 30000 ms",
            ),
            (
                slim_core::ProviderPhase::Compacting,
                Some("Retrying foreground compaction (1/2); waiting 250 ms".to_owned()),
                "Retrying foreground compaction (1/2); waiting 250 ms",
            ),
            (
                slim_core::ProviderPhase::Connecting,
                Some(" ".to_owned()),
                "Connecting to provider",
            ),
            (
                slim_core::ProviderPhase::PreparingTool,
                Some("read".to_owned()),
                "Preparing tool · read",
            ),
            (
                slim_core::ProviderPhase::FirstByte,
                None,
                "Stream open · waiting for content",
            ),
        ];
        for (phase, detail, expected) in cases {
            assert_eq!(
                UiEvent::from_core(SessionEvent::new(
                    1,
                    EventKind::ProviderPhase {
                        phase,
                        elapsed_ms: 7,
                        detail,
                    },
                )),
                Some(UiEvent::ProviderPhaseChanged {
                    phase,
                    label: expected.into(),
                    elapsed_ms: 7,
                })
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn wake_signal_is_waitable_without_periodic_polling() {
        let wake = super::WakeSignal::new().expect("wake");
        wake.notify();

        assert!(!wake.raw_handle().is_null());
        assert!(wake
            .wait_timeout(std::time::Duration::ZERO)
            .expect("signaled wake"));
        assert!(!wake
            .wait_timeout(std::time::Duration::ZERO)
            .expect("auto-reset wake consumed"));
    }

    #[cfg(windows)]
    #[test]
    fn finite_wake_timeout_never_aliases_positive_duration_to_zero_or_infinite() {
        use windows_sys::Win32::System::Threading::INFINITE;

        assert_eq!(super::finite_wait_timeout_ms(std::time::Duration::ZERO), 0);
        assert_eq!(
            super::finite_wait_timeout_ms(std::time::Duration::from_nanos(1)),
            1
        );
        assert_eq!(
            super::finite_wait_timeout_ms(std::time::Duration::from_millis(INFINITE as u64)),
            INFINITE - 1
        );
    }

    #[test]
    fn run_lifecycle_uses_ordered_lossless_stream_lane() {
        for event in [
            UiEvent::run_started(1),
            UiEvent::UserMessageAdded {
                text: "prompt".into(),
            },
            UiEvent::SessionRestored {
                session_id: SessionId("restored".into()),
                cwd: "workspace".into(),
                messages: Vec::new(),
                skill_names: Vec::new(),
            },
            UiEvent::ToolStarted {
                batch_id: ToolBatchId("batch".into()),
                call_id: ToolCallId("call".into()),
                name: "read".into(),
                arguments_summary: String::new(),
            },
            UiEvent::ToolOutput {
                batch_id: ToolBatchId("batch".into()),
                call_id: ToolCallId("call".into()),
                name: "read".into(),
                output: "complete".into(),
                content_handle: None,
            },
            UiEvent::ToolProgress {
                batch_id: ToolBatchId("batch".into()),
                call_id: ToolCallId("call".into()),
                name: "read".into(),
                preview: "half".into(),
                content_handle: None,
            },
            UiEvent::ToolEnded {
                batch_id: ToolBatchId("batch".into()),
                call_id: ToolCallId("call".into()),
                name: "read".into(),
                success: true,
                duration_ms: 1,
            },
            UiEvent::InputRequired {
                request_id: InteractionRequestId("input-1".into()),
                prompt: "choose".into(),
                options: Vec::new(),
                persisted: false,
            },
            UiEvent::UsageEstimate {
                request_id: 1,
                context_tokens: 10,
                context_window_tokens: 100,
            },
            UiEvent::FatalError {
                run_id: Some(7),
                message: "fatal".into(),
            },
            // Must not overtake the ToolEnded of its own command.
            UiEvent::UserShellFinished { request_id: 1 },
            UiEvent::SessionTitleChanged { title: None },
            UiEvent::RestoreDraft { text: "x".into() },
            UiEvent::CompactionCompleted,
            UiEvent::RunCompleted { run_id: 1 },
        ] {
            assert!(!event.is_control());
        }
        assert!(UiEvent::RunCancelled { run_id: 1 }.is_control());
    }

    #[test]
    fn gpt_56_efforts_match_codex_model_catalog() {
        assert_eq!(
            ReasoningEffort::supported(ModelAlias::Astra),
            &ReasoningEffort::ALL[..5]
        );
        assert_eq!(ModelAlias::parse("astra"), Some(ModelAlias::Astra));
        assert_eq!(ModelAlias::parse("gpt-6-astra"), Some(ModelAlias::Astra));
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

#[cfg(test)]
mod output_preview_tests {
    use super::tool_output_preview;

    #[test]
    fn partial_todo_failure_previews_the_rejection_instead_of_prior_success() {
        let output = "todo 6 [completed]: first\ntodo rejected (second): only one todo may be in progress\nCurrent items:";
        assert_eq!(
            tool_output_preview("todo", output),
            "todo rejected (second): only one todo may be in progress"
        );
        assert_eq!(
            tool_output_preview("todo", "todo 0 [completed]: first"),
            "todo 0 [completed]: first"
        );
    }

    #[test]
    fn failed_shell_previews_stderr_then_stdout_without_changing_success_output() {
        assert_eq!(
            tool_output_preview(
                "shell",
                "exit 1\nstdout:\nnoise\nstderr:\nAccess denied\nmore"
            ),
            "exit 1 · Access denied"
        );
        assert_eq!(
            tool_output_preview("shell", "exit 2\nstdout:\nmissing file\nstderr:\n"),
            "exit 2 · missing file"
        );
        assert_eq!(
            tool_output_preview("shell", "exit 1\nstdout:\nstderr:\n"),
            "exit 1"
        );
        assert_eq!(
            tool_output_preview("shell", "exit 0\nstdout:\nok\nstderr:\nwarning"),
            "exit 0"
        );
    }

    #[test]
    fn admission_note_does_not_replace_the_result_preview() {
        assert_eq!(
            tool_output_preview(
                "shell",
                "[admission: possible bash syntax]\nexit 1\nstdout:\nstderr:\nParserError"
            ),
            "exit 1 · ParserError"
        );
        assert_eq!(
            tool_output_preview(
                "search",
                "[admission: context_lines 4 -> 3; maximum]\n[a.rs]\n1: x"
            ),
            "[a.rs]"
        );
        assert_eq!(
            tool_output_preview("read", "[admission: note]\n"),
            "[admission: note]"
        );
    }
}
