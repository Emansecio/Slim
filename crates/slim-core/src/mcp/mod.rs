mod catalog;
mod client;
mod exposure;
mod http;
mod manager;
mod oauth;
mod search;
mod spec;
mod sse;
mod stdio;

pub use catalog::McpCatalog;
pub use client::{
    strip_terminal_controls, McpLog, McpProgress, McpProgressSink, McpServerHandshake,
    SUPPORTED_PROTOCOL_VERSIONS,
};
pub(crate) use client::skip_terminal_sequence;
pub use exposure::{
    exposure_glob_matches, is_direct_tool_name, provider_input_schema, sanitized_provider_name,
    AwarenessServer, DirectNames, McpDirectTool, DIRECT_TOOL_PREFIX, MAX_AWARENESS_BYTES,
    MAX_AWARENESS_LINE_CHARS, MAX_DIRECT_TOOLS, MAX_PROVIDER_TOOL_NAME,
};
pub use manager::{McpManager, McpStartupWait};
pub use oauth::{
    is_loopback_host, merge_scopes, parse_www_authenticate, pkce_challenge, step_up_scope,
    AuthServerMetadata, AuthorizationResponse, CallbackHint, Challenge, DiscoveryState,
    LoginSummary, McpAuth, McpAuthHandle, McpOAuthError, McpOAuthState, McpOAuthStore,
    MemoryOAuthStore, OAuthClient, OAuthTokens, PendingAuthorization, ProtectedResourceMetadata,
};
pub use search::{tokenize, tool_search_document, Bm25Match, Bm25Ranker, SearchServer};

pub(crate) use manager::clean_text;
pub use manager::{
    is_mcp_app_resource, McpResourceCounts, McpResourceItem, McpResourceListing, McpResourcePage,
    McpResourceTargets,
};
pub use spec::{
    McpCancellation, McpCleanupStatus, McpConnection, McpError, McpExposure, McpInterruption,
    McpOAuthSpec, McpRequestOutcome, McpServerBlock, McpServerInfo, McpServerOptions,
    McpServerSpec, McpServerStatus, McpToolSummary, McpTransport, DEFAULT_MCP_STARTUP_WAIT,
    DEFAULT_MCP_TIMEOUT, MCP_PROTOCOL_VERSION,
};
pub use stdio::{FramedLine, JsonLineFramer};

pub fn canonical_name(server: &str, tool: &str) -> String {
    format!("mcp.{server}.{tool}")
}
