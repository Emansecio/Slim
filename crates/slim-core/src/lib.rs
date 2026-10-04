//! Shared runtime contracts for the Slim workspace.

pub mod codeintel;
pub mod context;
pub mod events;
pub mod interaction;
pub mod mcp;
pub mod model;
pub mod process;
pub mod protocol;
pub mod provider;
pub mod redaction;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod task;
pub mod tools;
pub mod workspace_files;

pub use codeintel::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelMeta, CodeIntelOutcome,
    CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery, CodeIntelligence,
};
pub use events::{
    CausalAnomalyKind, CausalBoundaryKind, CausalConfidence, CausalProgressKind,
    CausalShadowAction, EventKind, ReasoningClassification, RequestKind, SessionEvent,
    TodoChangedItem, ToolEditDiff, ToolEditHunk,
};
pub use interaction::{
    ask_question_definition, interaction_route, AskQuestion, InteractionError,
    InteractionRequestId, InteractionResponder, InteractionRoute, PendingQuestion, QuestionAnswer,
    QuestionAnswerSource, QuestionOption, MAX_OPTION_DESCRIPTION_CHARS, MAX_OPTION_LABEL_CHARS,
    MAX_QUESTION_CHARS, MAX_QUESTION_OPTIONS,
};
pub use model::{AppHandle, EventQueueStats, SessionEventReceiver, SessionEventSender};
pub use protocol::OperatingMode;
pub use provider::{
    run_http_provider_messages, AnthropicAdapter, FakeProvider, HttpProviderClient, HttpRequest,
    OpenAiCompatibleAdapter, PreparedProviderRequest, ProviderAdapter, ProviderConfig,
    ProviderError, ProviderEvent, ProviderKind, ProviderMessage, ProviderPhase, ProviderPricing,
    ProviderRequestComponents, ProviderRequestFingerprints, ProviderTimeouts, ProviderToolCall,
    UsageBreakdown,
};
pub use redaction::redact_credentials;
pub use runtime::{
    tool_call_is_read_only, without_workspace_snapshot, AgentLoopConfig, AgentLoopResult,
    AgentLoopStop, RequestUsage, Runtime, RuntimeCapabilityBridge, UsageTotals,
};
pub use workspace_files::{
    is_sensitive_file_name, list_workspace_files, load_mention_file, mention_paths_in_prompt,
    MentionError, MentionFile,
};
