mod catalog;
mod http;
mod manager;
mod spec;
mod stdio;

pub use catalog::McpCatalog;
pub use manager::McpManager;
pub use spec::{
    McpConnection, McpError, McpServerInfo, McpServerSpec, McpServerStatus, McpToolSummary,
    McpTransport, MCP_PROTOCOL_VERSION,
};
pub use stdio::{FramedLine, JsonLineFramer};

pub fn canonical_name(server: &str, tool: &str) -> String {
    format!("mcp.{server}.{tool}")
}
