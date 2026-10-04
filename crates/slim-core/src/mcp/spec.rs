use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;

use crate::mcp::client::{McpProgressSink, McpServerHandshake};
use crate::mcp::oauth::McpAuthHandle;

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
    pub options: McpServerOptions,
}

/// Default per-request timeout for a server that does not configure one.
pub const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(60);

/// Default time the first model request waits for direct-exposure servers
/// that are still connecting (`[mcp] startup_wait_ms`).
pub const DEFAULT_MCP_STARTUP_WAIT: Duration = Duration::from_secs(10);

impl McpServerSpec {
    /// Enabled server with the default timeout and no optional settings.
    pub fn new(name: impl Into<String>, transport: McpTransport) -> Self {
        Self {
            name: name.into(),
            transport,
            enabled: true,
            timeout: DEFAULT_MCP_TIMEOUT,
            options: McpServerOptions::default(),
        }
    }
}

/// How a server's tools reach the model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpExposure {
    /// Reachable through the `mcp` gateway tool and codemode (default).
    #[default]
    Gateway,
    /// Declared to the model as `mcp__<server>__<tool>` provider tools.
    Direct,
    /// Registered but unreachable.
    Hidden,
}

impl McpExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Direct => "direct",
            Self::Hidden => "hidden",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "gateway" => Some(Self::Gateway),
            "direct" => Some(Self::Direct),
            "hidden" => Some(Self::Hidden),
            _ => None,
        }
    }
}

/// OAuth settings for an HTTP server (`client_secret` is already resolved).
#[derive(Clone, Default, Eq, PartialEq)]
pub struct McpOAuthSpec {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub callback_port: Option<u16>,
    pub scope: Option<String>,
    pub client_name: Option<String>,
    pub auth_server_metadata_url: Option<String>,
}

impl std::fmt::Debug for McpOAuthSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthSpec")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("callback_port", &self.callback_port)
            .field("scope", &self.scope)
            .field("client_name", &self.client_name)
            .field("auth_server_metadata_url", &self.auth_server_metadata_url)
            .finish()
    }
}

/// Why a configured server must not start. The entry stays listed so the
/// reason is visible in `/mcp` and to the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpServerBlock {
    /// Defined (or overridden) by a project `slim.toml` whose workspace is
    /// not trusted yet.
    Untrusted,
    /// A configuration value could not be resolved (missing environment
    /// variable, failing `!command`, ...). The text never contains values.
    Invalid(String),
}

/// Optional per-server settings. All default to "not configured".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpServerOptions {
    /// Working directory for stdio servers; relative paths resolve against
    /// the workspace root. `None` uses the workspace root.
    pub cwd: Option<PathBuf>,
    /// One-sentence description shown to the model and in `/mcp`.
    pub description: Option<String>,
    pub exposure: McpExposure,
    /// Per-tool exposure overrides: exact tool name or `*` glob.
    pub tool_exposure: BTreeMap<String, McpExposure>,
    /// Connect only on first use instead of at session start.
    pub lazy: bool,
    pub oauth: Option<McpOAuthSpec>,
    pub block: Option<McpServerBlock>,
    /// OAuth state machine of an HTTP server without a configured
    /// `Authorization` header. Built by the host (it owns the token store);
    /// does not take part in spec equality.
    pub auth: Option<McpAuthHandle>,
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

    /// Human-target shown in `/mcp`; HTTP credentials are omitted.
    pub fn target(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } => {
                if args.is_empty() {
                    command.clone()
                } else {
                    format!("{command} {}", args.join(" "))
                }
            }
            Self::Http { url, .. } => super::http::sanitized_url(url),
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
                    | "pass"
                    | "passphrase"
                    | "credential"
                    | "credentials"
                    | "creds"
                    | "signature"
                    | "key"
            )
        })
}

#[derive(Clone, Debug)]
pub struct McpToolSummary {
    pub name: String,
    pub description: Option<String>,
    pub schema: Value,
    pub output_schema: Option<Value>,
}

#[derive(Clone, Debug)]
pub enum McpServerStatus {
    Disabled,
    Disconnected,
    Connecting,
    Ready {
        tools: Arc<Vec<McpToolSummary>>,
    },
    Failed {
        error: String,
    },
    /// Project-defined server waiting for the workspace to be trusted.
    Untrusted,
    /// The server needs OAuth sign-in (or a new one: expired grant, more
    /// scope asked). Nothing opens a browser by itself: the user signs in
    /// with `/mcp login`.
    NeedsAuth {
        reason: String,
    },
}

/// Snapshot of one server for status surfaces (`/mcp` overlay, `list`).
#[derive(Clone, Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub transport: &'static str,
    pub target: String,
    pub enabled: bool,
    pub description: Option<String>,
    pub exposure: McpExposure,
    pub status: McpServerStatus,
    /// What the server reported in `initialize`; present while connected.
    pub handshake: Option<Arc<McpServerHandshake>>,
}

#[derive(Debug)]
pub enum McpError {
    Io(std::io::Error),
    Protocol(String),
    Timeout(Duration),
    Closed,
    /// The HTTP server no longer knows the session (404 on a request that
    /// carried `mcp-session-id`): it restarted or expired the session. The
    /// request may or may not have run; only idempotent requests are retried
    /// on a new session.
    SessionExpired,
    Server {
        code: i64,
        message: String,
    },
    UnknownServer(String),
    Disabled(String),
    /// Server cannot start: untrusted project configuration or an
    /// unresolvable configuration value. The text is safe to show.
    Blocked(String),
    CancelledBeforeSend,
    OutcomeUncertain {
        interruption: McpInterruption,
        cleanup: McpCleanupStatus,
    },
    /// The HTTP server needs OAuth sign-in. The request was rejected before
    /// it ran (HTTP 401/403), so nothing executed. The text is safe to show.
    AuthRequired(String),
}

/// Why an in-flight MCP request stopped. This is separate from a normal
/// protocol error so callers cannot mistake a possibly executed tool call
/// for a safe retry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpInterruption {
    Cancelled,
    TimedOut(Duration),
    ConnectionClosed,
}

/// Whether transport cleanup was confirmed after an interrupted request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpCleanupStatus {
    NotRequired,
    Confirmed,
    Unconfirmed,
}

/// Result of a cancel-aware request. `InterruptedBeforeSend` means the
/// writer did not admit the request; `OutcomeUncertain` means it had entered
/// the send transition and may have affected the server.
#[derive(Debug)]
pub enum McpRequestOutcome<T> {
    Completed(Result<T, McpError>),
    InterruptedBeforeSend {
        interruption: McpInterruption,
        cleanup: McpCleanupStatus,
    },
    OutcomeUncertain {
        interruption: McpInterruption,
        cleanup: McpCleanupStatus,
    },
}

impl<T> McpRequestOutcome<T> {
    pub fn map<U>(self, map: impl FnOnce(T) -> U) -> McpRequestOutcome<U> {
        match self {
            Self::Completed(Ok(value)) => McpRequestOutcome::Completed(Ok(map(value))),
            Self::Completed(Err(error)) => McpRequestOutcome::Completed(Err(error)),
            Self::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            },
            Self::OutcomeUncertain {
                interruption,
                cleanup,
            } => McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            },
        }
    }

    pub fn into_result(self) -> Result<T, McpError> {
        match self {
            Self::Completed(result) => result,
            Self::InterruptedBeforeSend { interruption, .. } => Err(match interruption {
                McpInterruption::Cancelled => McpError::CancelledBeforeSend,
                McpInterruption::TimedOut(timeout) => McpError::Timeout(timeout),
                McpInterruption::ConnectionClosed => McpError::Closed,
            }),
            Self::OutcomeUncertain {
                interruption,
                cleanup,
            } => Err(McpError::OutcomeUncertain {
                interruption,
                cleanup,
            }),
        }
    }
}

#[derive(Default)]
struct McpCancellationState {
    cancelled: AtomicBool,
    notify: Notify,
    gate: Mutex<()>,
}

/// Cancellation signal shared by runtime, manager, and transports.
#[derive(Clone, Default)]
pub struct McpCancellation(Arc<McpCancellationState>);

impl McpCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        let changed = {
            let _gate = self
                .0
                .gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            !self.0.cancelled.swap(true, Ordering::AcqRel)
        };
        if changed {
            self.0.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn lock_admission(&self) -> MutexGuard<'_, ()> {
        self.0
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
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
            Self::SessionExpired => write!(formatter, "MCP session expired"),
            Self::Server { code, message } => write!(formatter, "server error {code}: {message}"),
            Self::UnknownServer(name) => write!(formatter, "unknown MCP server: {name}"),
            Self::Disabled(name) => write!(formatter, "MCP server is disabled: {name}"),
            Self::Blocked(message) | Self::AuthRequired(message) => {
                write!(formatter, "{message}")
            }
            Self::CancelledBeforeSend => write!(formatter, "MCP request cancelled before send"),
            Self::OutcomeUncertain {
                interruption,
                cleanup,
            } => write!(
                formatter,
                "MCP request outcome is uncertain ({interruption:?}; cleanup {cleanup:?})"
            ),
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
    /// Additive cancel-aware path. Transports without request admission
    /// tracking classify cancellation conservatively because the request may
    /// already have reached the remote server.
    async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        if cancellation.is_cancelled() {
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        tokio::select! {
            biased;
            result = self.request(method, params) => match result {
                Err(McpError::Timeout(timeout)) if method == "tools/call" => {
                    McpRequestOutcome::OutcomeUncertain {
                        interruption: McpInterruption::TimedOut(timeout),
                        cleanup: McpCleanupStatus::Unconfirmed,
                    }
                }
                Err(
                    McpError::Io(_)
                    | McpError::Protocol(_)
                    | McpError::Closed
                    | McpError::SessionExpired,
                ) if method == "tools/call" =>
                {
                    McpRequestOutcome::OutcomeUncertain {
                        interruption: McpInterruption::ConnectionClosed,
                        cleanup: McpCleanupStatus::Unconfirmed,
                    }
                }
                result => McpRequestOutcome::Completed(result),
            },
            _ = cancellation.cancelled() => McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::Unconfirmed,
            },
        }
    }

    /// Cancel-aware request that also tracks progress. With `Some(sink)` the
    /// request carries a `progressToken`; the connection renews the request
    /// timeout on every matching `notifications/progress` and forwards it to
    /// the sink. Transports that cannot track progress ignore the sink.
    async fn request_with_progress(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
        progress: Option<McpProgressSink>,
    ) -> McpRequestOutcome<Value> {
        let _ = progress;
        self.request_cancellable(method, params, cancellation).await
    }

    /// Records the revision negotiated in `initialize`; HTTP replays it in
    /// the `MCP-Protocol-Version` header.
    fn set_protocol_version(&self, _version: &str) {}

    /// Requests transport shutdown and waits only for bounded cleanup.
    async fn close_for_cleanup(&self) -> McpCleanupStatus {
        McpCleanupStatus::NotRequired
    }
    /// Ends the server-side session in an orderly way (HTTP `DELETE`, bounded
    /// to one second) and returns once that finished or timed out. Callers
    /// that are about to drop the connection await this so the notice is not
    /// lost to process exit; transports with nothing to end do nothing.
    async fn end_session(&self) {}
    /// Sends a notification with cancellation observation. Transports without
    /// write admission tracking conservatively report uncertainty once the
    /// notification future may have started.
    async fn notify_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<()> {
        if cancellation.is_cancelled() {
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        tokio::select! {
            biased;
            _ = self.notify(method, params) => McpRequestOutcome::Completed(Ok(())),
            _ = cancellation.cancelled() => McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::Unconfirmed,
            },
        }
    }
    /// Sends a notification. Awaited by callers (e.g. `notifications/
    /// initialized` must reach the server before the first real request on
    /// transports where separate POSTs have no ordering).
    async fn notify(&self, method: &str, params: Value);
    fn is_closed(&self) -> bool;
    /// Server signalled `notifications/tools/list_changed` since last check.
    fn take_tools_stale(&self) -> bool {
        false
    }
    /// Server signalled `notifications/resources/list_changed` since last
    /// check; resource caches must be refreshed.
    fn take_resources_stale(&self) -> bool {
        false
    }
    /// Re-arms the stale flag after a failed refresh so the next `list_tools`
    /// retries instead of serving the cached list forever.
    fn mark_tools_stale(&self) {}
    /// Why a connection that just closed under a pending handshake request
    /// went away, with the diagnostics the transport kept (a stdio server's
    /// stderr tail). `None` for transports that keep none.
    async fn closed_reason(&self) -> Option<McpError> {
        None
    }
}
