use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::browser::BrowserLauncher;
use super::callback::await_callback;
use super::pkce::{generate_pkce, generate_state};
use super::{OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress};

const CLIENT_ID: &str = "9d1c250a-e6ae-44d9-88ed-5944d1962f5e";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CALLBACK_PORT: u16 = 54545;
const CALLBACK_PATH: &str = "/callback";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

pub async fn login(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    browser: Arc<dyn BrowserLauncher>,
    progress: &mpsc::UnboundedSender<OAuthProgress>,
    cancel: watch::Receiver<bool>,
) -> Result<OAuthCredential, OAuthError> {
    let pkce = generate_pkce()?;
    let state = generate_state()?;
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|_| {
            OAuthError::Callback(format!(
                "OAuth callback port {CALLBACK_PORT} is unavailable"
            ))
        })?;
    let redirect = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
    let mut url = reqwest::Url::parse(&endpoints.anthropic_authorize).map_err(|_| {
        OAuthError::InvalidResponse("Anthropic authorization URL is invalid".into())
    })?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    let url = url.to_string();
    let _ = progress.send(OAuthProgress::AuthUrl {
        url: url.clone(),
        user_code: None,
    });
    let _ = browser.open(&url);
    let code = await_callback(
        listener,
        CALLBACK_PATH,
        &state,
        cancel,
        Duration::from_secs(10 * 60),
    )
    .await?;
    let _ = progress.send(OAuthProgress::Message(
        "Exchanging Anthropic authorization code…".into(),
    ));
    let response = client
        .post(&endpoints.anthropic_token)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": redirect,
            "code_verifier": pkce.verifier,
        }))
        .send()
        .await
        .map_err(|_| OAuthError::Transport("Anthropic token exchange failed".into()))?;
    parse_token(response, None).await
}

pub async fn refresh(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    credential: &OAuthCredential,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(&endpoints.anthropic_token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "anthropic-sdk-rust/0.1 userOAuthProvider")
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": credential.refresh,
        }))
        .send()
        .await
        .map_err(|_| OAuthError::Transport("Anthropic token refresh failed".into()))?;
    parse_token(response, Some(&credential.refresh)).await
}

async fn parse_token(
    response: reqwest::Response,
    prior_refresh: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    if !response.status().is_success() {
        return Err(OAuthError::InvalidResponse(format!(
            "Anthropic OAuth failed ({})",
            response.status().as_u16()
        )));
    }
    let token: TokenResponse = response
        .json()
        .await
        .map_err(|_| OAuthError::InvalidResponse("Anthropic token response is invalid".into()))?;
    if token.access_token.is_empty() {
        return Err(OAuthError::InvalidResponse(
            "Anthropic token response is incomplete".into(),
        ));
    }
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token
            .refresh_token
            .or_else(|| prior_refresh.map(str::to_owned))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                OAuthError::InvalidResponse("Anthropic refresh token is missing".into())
            })?,
        expires: now_ms()
            .saturating_add(token.expires_in.saturating_mul(1000))
            .saturating_sub(5 * 60 * 1000),
        account_id: None,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
