use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OAuthProvider {
    Anthropic,
    OpenAiCodex,
}

impl OAuthProvider {
    pub fn key(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCodex => "openai-codex",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic — Claude Pro/Max",
            Self::OpenAiCodex => "OpenAI Codex — ChatGPT Plus/Pro",
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field("access", &"[REDACTED]")
            .field("refresh", &"[REDACTED]")
            .field("expires", &self.expires)
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum OAuthProgress {
    Message(String),
    AuthUrl {
        url: String,
        user_code: Option<String>,
    },
}

impl fmt::Debug for OAuthProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.debug_tuple("Message").field(message).finish(),
            Self::AuthUrl { .. } => formatter
                .debug_struct("AuthUrl")
                .field("url", &"[REDACTED]")
                .field("user_code", &"[REDACTED]")
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OAuthError {
    Cancelled,
    Timeout,
    Browser,
    Callback(String),
    Transport(String),
    InvalidResponse(String),
    Store(String),
}

impl fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("login cancelled"),
            Self::Timeout => formatter.write_str("login timed out"),
            Self::Browser => formatter.write_str("browser could not be opened"),
            Self::Callback(message)
            | Self::Transport(message)
            | Self::InvalidResponse(message)
            | Self::Store(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for OAuthError {}

#[derive(Clone)]
pub struct OAuthEndpoints {
    pub anthropic_authorize: String,
    pub anthropic_token: String,
    pub codex_authorize: String,
    pub codex_token: String,
    pub codex_device_code: String,
    pub codex_device_token: String,
    pub codex_device_verify: String,
}

impl Default for OAuthEndpoints {
    fn default() -> Self {
        Self {
            anthropic_authorize: "https://claude.ai/oauth/authorize".into(),
            anthropic_token: "https://api.anthropic.com/v1/oauth/token".into(),
            codex_authorize: "https://auth.openai.com/oauth/authorize".into(),
            codex_token: "https://auth.openai.com/oauth/token".into(),
            codex_device_code: "https://auth.openai.com/api/accounts/deviceauth/usercode".into(),
            codex_device_token: "https://auth.openai.com/api/accounts/deviceauth/token".into(),
            codex_device_verify: "https://auth.openai.com/codex/device".into(),
        }
    }
}
