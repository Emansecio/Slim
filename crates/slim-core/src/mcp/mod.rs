mod catalog;
mod http;
mod manager;
mod spec;
mod stdio;

pub use catalog::McpCatalog;
pub use manager::McpManager;
pub use spec::{
    MCP_PROTOCOL_VERSION, McpCancellation, McpCleanupStatus, McpConnection, McpError,
    McpInterruption, McpRequestOutcome, McpServerInfo, McpServerSpec, McpServerStatus,
    McpToolSummary, McpTransport,
};
pub use stdio::{FramedLine, JsonLineFramer};

pub fn canonical_name(server: &str, tool: &str) -> String {
    format!("mcp.{server}.{tool}")
}
