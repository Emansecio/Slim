//! Shared runtime contracts for the Slim workspace.

pub mod agents;
pub mod codeintel;
pub mod context;
pub mod events;
pub mod interaction;
pub mod mcp;
pub mod model;
pub mod process;
pub mod profiles;
pub mod protocol;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod task;
pub mod tools;

pub use codeintel::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelMeta, CodeIntelOutcome,
    CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery, CodeIntelligence,
};
pub use events::{
    CausalAnomalyKind, CausalBoundaryKind, CausalConfidence, CausalProgressKind,
    CausalShadowAction, EventKind, RequestKind, SessionEvent, TodoChangedItem,
};
pub use interaction::{
    ask_question_definition, interaction_route, AskQuestion, InteractionError,
    InteractionRequestId, InteractionResponder, InteractionRoute, PendingQuestion, QuestionAnswer,
    QuestionAnswerSource, QuestionOption, MAX_OPTION_DESCRIPTION_CHARS, MAX_OPTION_LABEL_CHARS,
    MAX_QUESTION_CHARS, MAX_QUESTION_OPTIONS,
};
pub use model::{
    AppHandle, EventQueueStats, SessionEventReceiver, SessionEventSender, SessionSnapshot,
};
pub use profiles::{Profile, ProfileCatalog, ProfileId};
pub use protocol::OperatingMode;
pub use provider::{
    run_http_provider, run_http_provider_messages, AnthropicAdapter, FakeProvider,
    HttpProviderClient, HttpRequest, OpenAiCompatibleAdapter, PreparedProviderRequest,
    ProviderAdapter, ProviderConfig, ProviderError, ProviderEvent, ProviderKind, ProviderMessage,
    ProviderPhase, ProviderPricing, ProviderRequestComponents, ProviderRequestFingerprints,
    ProviderTimeouts, ProviderToolCall, UsageBreakdown,
};
pub use runtime::{
    tool_call_is_read_only, without_workspace_snapshot, AgentLoopConfig, AgentLoopResult,
    AgentLoopStop, InProcessCapabilityAdapter, RequestUsage, Runtime, RuntimeCapabilityAdapter,
    RuntimeCapabilityBridge, RuntimeCapabilityTarget, UsageTotals,
};
