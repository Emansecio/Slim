//! Data the OAuth flow persists and exchanges. Everything that can carry a
//! secret prints `[REDACTED]` in `Debug`.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::McpOAuthError;

fn redacted(value: &Option<String>) -> Option<&'static str> {
    value.as_ref().map(|_| "[REDACTED]")
}

/// Tokens of one server. `expires_at_ms` is milliseconds since the epoch,
/// computed from `expires_in` when the tokens were received; `None` means the
/// server reported no expiry.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthTokens")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &redacted(&self.refresh_token))
            .field("scope", &self.scope)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// OAuth client of one server: pre-registered (configured) or the result of
/// dynamic client registration. Only registered clients are persisted.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct OAuthClient {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Redirect URIs the client registered. A stored client is only reused
    /// for one of them.
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &redacted(&self.client_secret))
            .field("redirect_uris", &self.redirect_uris)
            .field(
                "token_endpoint_auth_method",
                &self.token_endpoint_auth_method,
            )
            .finish()
    }
}

/// RFC 8414 / OIDC authorization server metadata (the fields Slim uses).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuthServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    #[serde(default)]
    pub response_types_supported: Vec<String>,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// `None` when the document does not say.
    #[serde(default)]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    /// RFC 9207: authorization responses carry an `iss` parameter.
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
}

/// RFC 9728 protected resource metadata (the fields Slim uses).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// What discovery found; cached so a refresh does not discover again.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoveryState {
    pub authorization_server_url: String,
    pub metadata: AuthServerMetadata,
    /// RFC 8707 `resource` parameter value.
    pub resource: String,
    /// `scopes_supported` of the protected resource, the default scope.
    #[serde(default)]
    pub resource_scopes: Vec<String>,
}

/// Everything stored for one server. `server_url` binds it to the exact URL
/// it was obtained for: state for another URL is ignored.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpOAuthState {
    pub server_url: String,
    #[serde(default)]
    pub client: Option<OAuthClient>,
    #[serde(default)]
    pub tokens: Option<OAuthTokens>,
    #[serde(default)]
    pub discovery: Option<DiscoveryState>,
    /// Scopes a step-up (403 `insufficient_scope`) asked for, merged with
    /// the granted ones; the next sign-in requests them.
    #[serde(default)]
    pub pending_scope: Option<String>,
}

/// Parsed `WWW-Authenticate: Bearer ...` challenge.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn string_list(value: &Value, field: &str) -> Result<Vec<String>, McpOAuthError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid(field))
            })
            .collect(),
        Some(_) => Err(invalid(field)),
    }
}

fn invalid(what: &str) -> McpOAuthError {
    McpOAuthError::Failed(format!("invalid OAuth metadata: {what}"))
}

fn required_url(value: &Value, field: &str) -> Result<String, McpOAuthError> {
    let text = value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid(field))?;
    let url = reqwest::Url::parse(text).map_err(|_| invalid(field))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(field));
    }
    Ok(text.to_owned())
}

impl AuthServerMetadata {
    pub(crate) fn parse(value: &Value) -> Result<Self, McpOAuthError> {
        if !value.is_object() {
            return Err(invalid("authorization server metadata"));
        }
        let code_challenge_methods = match value.get("code_challenge_methods_supported") {
            None | Some(Value::Null) => None,
            Some(_) => Some(string_list(value, "code_challenge_methods_supported")?),
        };
        Ok(Self {
            issuer: required_url(value, "issuer")?,
            authorization_endpoint: required_url(value, "authorization_endpoint")?,
            token_endpoint: required_url(value, "token_endpoint")?,
            registration_endpoint: match value.get("registration_endpoint") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) if text.is_empty() => None,
                Some(_) => Some(required_url(value, "registration_endpoint")?),
            },
            scopes_supported: string_list(value, "scopes_supported")?,
            response_types_supported: string_list(value, "response_types_supported")?,
            token_endpoint_auth_methods_supported: string_list(
                value,
                "token_endpoint_auth_methods_supported",
            )?,
            code_challenge_methods_supported: code_challenge_methods,
            authorization_response_iss_parameter_supported: value
                .get("authorization_response_iss_parameter_supported")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

impl ProtectedResourceMetadata {
    pub(crate) fn parse(value: &Value) -> Result<Self, McpOAuthError> {
        if !value.is_object() {
            return Err(invalid("protected resource metadata"));
        }
        let servers = string_list(value, "authorization_servers")?;
        for server in &servers {
            let url = reqwest::Url::parse(server).map_err(|_| invalid("authorization_servers"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(invalid("authorization_servers"));
            }
        }
        Ok(Self {
            resource: required_url(value, "resource")?,
            authorization_servers: servers,
            scopes_supported: string_list(value, "scopes_supported")?,
        })
    }
}
