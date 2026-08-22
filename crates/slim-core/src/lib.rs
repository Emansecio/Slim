//! Shared runtime contracts for the Slim workspace.

pub mod agents;
pub mod context;
pub mod events;
pub mod mcp;
pub mod model;
pub mod profiles;
pub mod protocol;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod task;
pub mod tools;

pub use events::{EventKind, SessionEvent};
pub use model::{AppHandle, SessionSnapshot};
pub use profiles::{Profile, ProfileCatalog, ProfileId};
pub use protocol::OperatingMode;
pub use provider::{
    run_http_provider, run_http_provider_messages, AnthropicAdapter, FakeProvider,
    HttpProviderClient, HttpRequest, OpenAiCompatibleAdapter, ProviderAdapter, ProviderConfig,
    ProviderError, ProviderEvent, ProviderKind, ProviderMessage, ProviderPricing, ProviderTimeouts,
    ProviderToolCall,
};
pub use runtime::{AgentLoopConfig, AgentLoopResult, AgentLoopStop, Runtime, UsageTotals};
