use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

/// MCP protocol revision negotiated during `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

/// Resolved configuration for one MCP server (config file values already
/// merged and validated by the caller).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransport,
    pub enabled: bool,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl McpTransport {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }

    /// Human-target shown in `/mcp`; never includes header or env values.
    pub fn target(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } => {
                if args.is_empty() {
                    command.clone()
                } else {
                    format!("{command} {}", args.join(" "))
                }
            }
            Self::Http { url, .. } => url.clone(),
        }
    }

    /// Credential fields identified by explicit header/env naming conventions.
    /// Ordinary configuration values must not become global redaction tokens.
    pub fn sensitive_values(&self) -> impl Iterator<Item = &String> {
        match self {
            Self::Stdio { env, .. } => env.iter(),
            Self::Http { headers, .. } => headers.iter(),
        }
        .filter(|(key, _)| credential_key(key))
        .map(|(_, value)| value)
    }
}

fn credential_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    key == "authorization"
        || key == "proxy_authorization"
        || key == "cookie"
        || key == "set_cookie"
        || key.ends_with("api_key")
        || key.ends_with("apikey")
        || key.split('_').any(|part| {
            matches!(
                part,
                "token"
                    | "secret"
                    | "password"
                    | "passwd"
                    | "credential"
                    | "credentials"
                    | "signature"
            )
        })
}

#[derive(Clone, Debug)]
pub struct McpToolSummary {
    pub name: String,
    pub description: Option<String>,
    pub schema: Value,
}

#[derive(Clone, Debug)]
pub enum McpServerStatus {
    Disabled,
    Disconnected,
    Connecting,
    Ready { tools: Arc<Vec<McpToolSummary>> },
    Failed { error: String },
}

impl McpServerStatus {
    pub fn tool_count(&self) -> Option<usize> {
        match self {
            Self::Ready { tools } => Some(tools.len()),
            _ => None,
        }
    }
}

/// Snapshot of one server for status surfaces (`/mcp` overlay, `list`).
#[derive(Clone, Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub transport: &'static str,
    pub target: String,
    pub enabled: bool,
    pub status: McpServerStatus,
}

#[derive(Debug)]
pub enum McpError {
    Io(std::io::Error),
    Protocol(String),
    Timeout(Duration),
    Closed,
    Server { code: i64, message: String },
    UnknownServer(String),
    Disabled(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Protocol(message) => write!(formatter, "{message}"),
            Self::Timeout(timeout) => {
                write!(
                    formatter,
                    "request timed out after {}ms",
                    timeout.as_millis()
                )
            }
            Self::Closed => write!(formatter, "connection closed"),
            Self::Server { code, message } => write!(formatter, "server error {code}: {message}"),
            Self::UnknownServer(name) => write!(formatter, "unknown MCP server: {name}"),
            Self::Disabled(name) => write!(formatter, "MCP server is disabled: {name}"),
        }
    }
}

impl std::error::Error for McpError {}

impl From<std::io::Error> for McpError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for McpError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(error.to_string())
    }
}

/// Server-controlled strings (JSON-RPC error messages) get a hard cap so a
/// hostile peer cannot flood errors, statuses, or the event log.
pub(crate) const MAX_SERVER_MESSAGE_BYTES: usize = 2 * 1024;

pub(crate) fn bounded_server_text(text: &str) -> String {
    if text.len() <= MAX_SERVER_MESSAGE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_SERVER_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// One live transport to a server. Connections multiplex JSON-RPC requests
/// by id; `notify` is fire-and-forget.
#[async_trait::async_trait]
pub trait McpConnection: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>;
    /// Sends a notification. Awaited by callers (e.g. `notifications/
    /// initialized` must reach the server before the first real request on
    /// transports where separate POSTs have no ordering).
    async fn notify(&self, method: &str, params: Value);
    fn is_closed(&self) -> bool;
    /// Server signalled `notifications/tools/list_changed` since last check.
    fn take_tools_stale(&self) -> bool {
        false
    }
    /// Re-arms the stale flag after a failed refresh so the next `list_tools`
    /// retries instead of serving the cached list forever.
    fn mark_tools_stale(&self) {}
}
