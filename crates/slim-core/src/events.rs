use crate::protocol::OperatingMode;
use crate::provider::{ProviderPhase, UsageBreakdown};
use crate::QuestionOption;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalProgressKind {
    WorkspaceChanged,
    ValidationGreen,
    NewEvidence,
    DiagnosticsChanged,
    DistinctFailure,
    ExternalInput,
    DependencyChanged,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalBoundaryKind {
    PotentiallyVolatile,
    Unclassifiable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalAnomalyKind {
    ReusableEvidence,
    RepeatedFailure,
    RedundantValidation,
    StagnantTurn,
    NoProgressCandidate,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalConfidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalShadowAction {
    Observe,
    WouldReuse,
    WouldReject,
    WouldWarn,
    WouldStop,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    #[default]
    ProviderTurn,
    Compaction,
}

/// Provider-declared meaning of the reasoning stream.  Adapters only emit a
/// classification when their wire protocol identifies the exposed content;
/// an absent event intentionally leaves the UI unclassified.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningClassification {
    Summary,
    Text,
}

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
    /// Adapter-known classification for the following reasoning stream.
    ReasoningClassification {
        classification: ReasoningClassification,
    },
    ThinkingStarted,
    ThinkingEnded,
    ProviderPhase {
        phase: ProviderPhase,
        #[serde(default)]
        elapsed_ms: u64,
        #[serde(default)]
        detail: Option<String>,
    },
    /// A retry has been scheduled; the wait is a delay, not work progress.
    RetryScheduled {
        attempt: u32,
        limit: u32,
        wait_ms: u64,
        #[serde(default)]
        reason: Option<String>,
    },
    AssistantEnded {
        reason: String,
    },
    UsagePartial {
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        input_known: bool,
        #[serde(default)]
        output_known: bool,
    },
    UsageBreakdown {
        usage: UsageBreakdown,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    ResponseCacheHit,
    GoalAssurance {
        verified: bool,
    },
    ToolStarted {
        #[serde(default)]
        batch_id: String,
        #[serde(default)]
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    /// Tool arguments and policy checks completed, before executor admission.
    ToolPrepared {
        #[serde(default)]
        batch_id: String,
        #[serde(default)]
        call_id: String,
        name: String,
    },
    /// The call was admitted to an execution segment/pool.
    ToolAdmitted {
        #[serde(default)]
        batch_id: String,
        #[serde(default)]
        call_id: String,
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
        #[serde(default)]
        batch_id: String,
        #[serde(default)]
        call_id: String,
        name: String,
        output: String,
    },
    ToolProgress {
        #[serde(default)]
        batch_id: String,
        #[serde(default)]
        call_id: String,
        name: String,
        preview: String,
    },
    /// Facts from the native process backing a tool call. These describe the
    /// process boundary only; they do not determine the surrounding task's
    /// semantic success.
    ToolProcessFinished {
        #[serde(default)]
        batch_id: String,
        #[serde(default)]
        call_id: String,
        name: String,
        process: crate::process::ProcessExecutionFacts,
    },
    ToolFinished {
        #[serde(default)]
        batch_id: String,
        #[serde(default)]
        call_id: String,
        name: String,
        success: bool,
        #[serde(default)]
        duration_ms: u64,
    },
    ToolEvidenceReused {
        #[serde(default)]
        original_bytes: u64,
        #[serde(default)]
        emitted_bytes: u64,
        #[serde(default)]
        post_compaction: bool,
    },
    ToolEvidenceElided {
        #[serde(default)]
        count: u64,
        #[serde(default)]
        original_bytes: u64,
        #[serde(default)]
        emitted_bytes: u64,
    },
    ToolCallsSuppressed {
        #[serde(default)]
        count: u64,
    },
    CausalProgressObserved {
        #[serde(default)]
        batch_id: Box<str>,
        #[serde(default)]
        call_id: Box<str>,
        kind: CausalProgressKind,
        tool_name: Box<str>,
        call_fingerprint: Box<str>,
        evidence_id: Box<str>,
        workspace_revision: u64,
    },
    CausalBoundaryObserved {
        #[serde(default)]
        batch_id: Box<str>,
        #[serde(default)]
        call_id: Box<str>,
        kind: CausalBoundaryKind,
        tool_name: Box<str>,
        call_fingerprint: Box<str>,
        uncertainty_epoch: u64,
    },
    CausalAnomalyDetected {
        #[serde(default)]
        batch_id: Box<str>,
        #[serde(default)]
        call_id: Box<str>,
        kind: CausalAnomalyKind,
        tool_name: Box<str>,
        call_fingerprint: Box<str>,
        evidence_id: Box<str>,
        workspace_revision: u64,
        occurrence: u32,
        confidence: CausalConfidence,
        action: CausalShadowAction,
    },
    ArtifactStored {
        id: String,
        size: u64,
    },
    CompactionCompleted,
    CompactionAttemptStarted {
        #[serde(default)]
        provider: String,
        #[serde(default)]
        model: String,
        #[serde(default)]
        system_bytes: u64,
        #[serde(default)]
        history_bytes: u64,
        #[serde(default)]
        serialized_chars: u64,
        #[serde(default)]
        request_bytes: u64,
        #[serde(default)]
        estimated_input_tokens: u64,
    },
    CompactionAttemptCompleted {
        #[serde(default, alias = "input_tokens")]
        uncached_input_tokens: u64,
        #[serde(default)]
        cache_write_tokens: u64,
        #[serde(default)]
        cache_read_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        time_to_first_byte_ms: u64,
        #[serde(default)]
        time_to_first_semantic_ms: u64,
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        usage_known: bool,
        #[serde(default)]
        system_bytes: Option<u64>,
        #[serde(default)]
        history_bytes: Option<u64>,
        #[serde(default)]
        estimated_input_tokens: Option<u64>,
    },
    CompactionAttemptCancelled {
        #[serde(default)]
        request_bytes: u64,
        #[serde(default)]
        estimated_input_tokens: u64,
        #[serde(default)]
        time_to_first_byte_ms: u64,
        #[serde(default)]
        time_to_first_semantic_ms: u64,
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        send_started: bool,
        #[serde(default)]
        headers_received: bool,
        #[serde(default)]
        first_byte_received: bool,
        #[serde(default)]
        first_token_received: bool,
    },
    CompactionUsageUnknown {
        #[serde(default)]
        estimated_input_tokens: u64,
        #[serde(default)]
        reason: String,
    },
    CompactionSkippedBelowBreakEven {
        #[serde(default)]
        projected_savings_tokens: u64,
        #[serde(default)]
        estimated_cost_tokens: u64,
        #[serde(default)]
        safety_margin_tokens: u64,
        #[serde(default)]
        future_turns: u8,
    },
    CompactionState {
        state: crate::context::CompactionStatus,
        reason: crate::context::CompactionReason,
        #[serde(default)]
        tokens_before: u64,
        #[serde(default)]
        tokens_after: u64,
        #[serde(default)]
        duration_ms: u64,
    },
    CompactionJevPruned {
        #[serde(default)]
        pairs_total: u64,
        #[serde(default)]
        pairs_dropped: u64,
        #[serde(default)]
        results_truncated: u64,
        #[serde(default)]
        batches: u64,
        /// Number of Jev requests that entered the bounded batch loop. Kept
        /// separate from the legacy `batches` field so partially completed
        /// or cancelled pruning remains observable.
        #[serde(default)]
        batches_started: u64,
        #[serde(default)]
        batches_completed: u64,
        /// True when at least one usage component was unavailable. Confirmed
        /// components are still retained by the usage ledger.
        #[serde(default)]
        usage_unknown: bool,
        #[serde(default)]
        estimated_saved_tokens: u64,
        #[serde(default)]
        input_tokens: Option<u64>,
        #[serde(default)]
        output_tokens: Option<u64>,
        #[serde(default)]
        model: Option<String>,
        /// Jev endpoint identity. `model` is the resolved model returned by
        /// the provider; this field records the requested model and backend
        /// for pricing/audit decisions.
        #[serde(default)]
        backend: Option<String>,
        #[serde(default)]
        requested_model: Option<String>,
        #[serde(default)]
        duration_ms: u64,
    },
    /// Jev pruning was selected but could not complete. The ordinary failure
    /// path uses the LLM summary; cancellation stops the compaction instead.
    /// `detail` carries a bounded, redacted reason.
    CompactionJevFallback {
        #[serde(default)]
        detail: String,
        #[serde(default)]
        batches: u64,
        #[serde(default)]
        batches_started: u64,
        #[serde(default)]
        batches_completed: u64,
        #[serde(default)]
        usage_unknown: bool,
        #[serde(default)]
        input_tokens: Option<u64>,
        #[serde(default)]
        output_tokens: Option<u64>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        backend: Option<String>,
        #[serde(default)]
        requested_model: Option<String>,
        #[serde(default)]
        duration_ms: u64,
    },
    /// Per-turn wire-size snapshot emitted before each provider request so
    /// token-economy regressions are observable in the session log.
    ContextSnapshot {
        #[serde(default)]
        request_kind: RequestKind,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        model: String,
        #[serde(default)]
        system_bytes: u64,
        #[serde(default, alias = "tools_bytes")]
        tool_schema_bytes: u64,
        history_bytes: u64,
        #[serde(default)]
        tool_result_bytes: u64,
        #[serde(default)]
        serialized_chars: u64,
        #[serde(default)]
        estimated_tokens: u64,
        #[serde(default)]
        context_window_tokens: u64,
    },
    RequestCompleted {
        #[serde(default)]
        provider_latency_ms: u64,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        failed: bool,
    },
    ApprovalRequired {
        #[serde(default)]
        request_id: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        persisted: bool,
    },
    InputRequired {
        #[serde(default)]
        request_id: String,
        #[serde(default)]
        prompt: String,
        #[serde(default)]
        options: Vec<String>,
        #[serde(default)]
        persisted: bool,
    },
    QuestionRequired {
        #[serde(default)]
        request_id: String,
        #[serde(default)]
        question: String,
        #[serde(default)]
        options: Vec<QuestionOption>,
        #[serde(default)]
        persisted: bool,
    },
    InteractionAcknowledged {
        #[serde(default)]
        request_id: String,
        accepted: bool,
        #[serde(default)]
        message: String,
    },
    SubagentActivity {
        message: String,
    },
    TodoChanged {
        items: Vec<TodoChangedItem>,
    },
    TerminalError {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TodoChangedItem {
    pub title: String,
    pub status: String,
}
