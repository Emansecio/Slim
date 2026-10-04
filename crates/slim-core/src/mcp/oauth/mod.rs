//! OAuth for HTTP MCP servers that need authorization (MCP authorization
//! spec 2025-11-25): RFC 9728 protected resource metadata, RFC 8414 / OIDC
//! discovery, RFC 7591 dynamic client registration or a pre-registered
//! client, the authorization code flow with PKCE S256, RFC 8707 resource
//! indicators and RFC 9207 issuer checks.
//!
//! This module is protocol only. It never opens a browser, listens on a port
//! or touches the file system: persistence goes through [`McpOAuthStore`]
//! (the CLI implements it with its secure-file helpers) and sign-in is split
//! into [`McpAuth::begin_login`] and [`McpAuth::complete_login`] so the
//! application can run the browser and callback part.

mod auth;
mod discovery;
mod flow;
mod types;

use std::fmt;
use std::sync::Mutex;

pub(crate) use auth::AuthSendError;
pub use auth::{
    AuthorizationResponse, CallbackHint, LoginSummary, McpAuth, McpAuthHandle, PendingAuthorization,
};
pub use discovery::{is_loopback_host, parse_www_authenticate};
pub use flow::{merge_scopes, pkce_challenge, step_up_scope};
pub use types::{
    AuthServerMetadata, Challenge, DiscoveryState, McpOAuthState, OAuthClient, OAuthTokens,
    ProtectedResourceMetadata,
};

/// OAuth failures. The texts are safe to show: they never contain tokens,
/// secrets or URLs with credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpOAuthError {
    /// The user has to sign in (again).
    AuthRequired(String),
    /// The authorization server answered with an OAuth error.
    Server { code: String, description: String },
    /// Network, metadata, store and policy failures.
    Failed(String),
}

impl fmt::Display for McpOAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthRequired(message) | Self::Failed(message) => write!(formatter, "{message}"),
            Self::Server { code, description } if code == description => {
                write!(formatter, "authorization server error: {code}")
            }
            Self::Server { code, description } => {
                write!(
                    formatter,
                    "authorization server error {code}: {description}"
                )
            }
        }
    }
}

impl std::error::Error for McpOAuthError {}

/// Persistence of one server's OAuth state. An implementation is bound to
/// one server (name and URL); calls are blocking and short, except
/// [`Self::lock_refresh`], which may wait for another process.
pub trait McpOAuthStore: Send + Sync {
    /// The stored state, `None` when there is none. Failures are texts safe
    /// to show.
    fn load(&self) -> Result<Option<McpOAuthState>, String>;
    fn save(&self, state: &McpOAuthState) -> Result<(), String>;
    /// Deletes the stored state; `true` when there was some.
    fn clear(&self) -> Result<bool, String>;
    /// Takes the cross-process refresh lock, waiting a bounded time. The lock
    /// is released when the returned guard drops. A lock whose holder died is
    /// taken over after it went stale.
    fn lock_refresh(&self) -> Result<Box<dyn Send>, String>;
}

/// In-memory store: state lives as long as the value. For tests and for
/// hosts without a persistent store.
#[derive(Default)]
pub struct MemoryOAuthStore {
    state: Mutex<Option<McpOAuthState>>,
}

impl MemoryOAuthStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_state(state: McpOAuthState) -> Self {
        Self {
            state: Mutex::new(Some(state)),
        }
    }

    pub fn snapshot(&self) -> Option<McpOAuthState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl McpOAuthStore for MemoryOAuthStore {
    fn load(&self) -> Result<Option<McpOAuthState>, String> {
        Ok(self.snapshot())
    }

    fn save(&self, state: &McpOAuthState) -> Result<(), String> {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state.clone());
        Ok(())
    }

    fn clear(&self) -> Result<bool, String> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .is_some())
    }

    fn lock_refresh(&self) -> Result<Box<dyn Send>, String> {
        // `std::sync::Mutex` has no owned guard; a refresh in one process is
        // already serialized by `McpAuth`, so there is nothing more to hold.
        Ok(Box::new(()))
    }
}
