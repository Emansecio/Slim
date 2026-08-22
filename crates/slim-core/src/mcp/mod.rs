mod catalog;
mod http;
mod lifecycle;
mod stdio;

pub use catalog::McpCatalog;
pub use http::authorize_http;
pub use lifecycle::McpLifecycle;
pub use stdio::JsonLineFramer;

pub fn canonical_name(server: &str, tool: &str) -> String {
    format!("mcp.{server}.{tool}")
}
