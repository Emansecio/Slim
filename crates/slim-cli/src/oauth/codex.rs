use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::browser::BrowserLauncher;
use super::callback::await_callback;
use super::pkce::{generate_pkce, generate_state};
use super::{
    parse_json_response_bounded, OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";

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
    match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await {
        Ok(listener) => browser_login(client, endpoints, browser, progress, cancel, listener).await,
        Err(_) => device_login(client, endpoints, browser, progress, cancel).await,
    }
}

async fn browser_login(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    browser: Arc<dyn BrowserLauncher>,
    progress: &mpsc::UnboundedSender<OAuthProgress>,
    cancel: watch::Receiver<bool>,
    listener: TcpListener,
) -> Result<OAuthCredential, OAuthError> {
    let pkce = generate_pkce()?;
    let state = generate_state()?;
    let redirect = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
    let mut url = reqwest::Url::parse(&endpoints.codex_authorize)
        .map_err(|_| OAuthError::InvalidResponse("Codex authorization URL is invalid".into()))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "slim");
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
    exchange(
        client,
        &endpoints.codex_token,
        &code,
        &pkce.verifier,
        &redirect,
        None,
    )
    .await
}

async fn device_login(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    browser: Arc<dyn BrowserLauncher>,
    progress: &mpsc::UnboundedSender<OAuthProgress>,
    mut cancel: watch::Receiver<bool>,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(&endpoints.codex_device_code)
        .json(&serde_json::json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .map_err(|_| OAuthError::Transport("Codex device authorization failed".into()))?;
    if !response.status().is_success() {
        return Err(OAuthError::InvalidResponse(format!(
            "Codex device authorization failed ({})",
            response.status().as_u16()
        )));
    }
    let value: Value =
        parse_json_response_bounded(response, "Codex device response is invalid").await?;
    let device_id = value
        .get("device_auth_id")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::InvalidResponse("Codex device id is missing".into()))?;
    let user_code = value
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::InvalidResponse("Codex user code is missing".into()))?;
    let interval = value
        .get("interval")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(5)
        .max(1);
    let _ = progress.send(OAuthProgress::AuthUrl {
        url: endpoints.codex_device_verify.clone(),
        user_code: Some(user_code.into()),
    });
    let _ = browser.open(&endpoints.codex_device_verify);
    for _ in 0..120 {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            changed = cancel.changed() => {
                let _ = changed;
                return Err(OAuthError::Cancelled);
            }
        }
        let response = client
            .post(&endpoints.codex_device_token)
            .json(&serde_json::json!({
                "device_auth_id": device_id,
                "user_code": user_code,
            }))
            .send()
            .await
            .map_err(|_| OAuthError::Transport("Codex device polling failed".into()))?;
        if matches!(response.status().as_u16(), 403 | 404) {
            continue;
        }
        if !response.status().is_success() {
            return Err(OAuthError::InvalidResponse(format!(
                "Codex device polling failed ({})",
                response.status().as_u16()
            )));
        }
        let value: Value =
            parse_json_response_bounded(response, "Codex device token is invalid").await?;
        let code = value
            .get("authorization_code")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                OAuthError::InvalidResponse("Codex authorization code is missing".into())
            })?;
        let verifier = value
            .get("code_verifier")
            .and_then(Value::as_str)
            .ok_or_else(|| OAuthError::InvalidResponse("Codex verifier is missing".into()))?;
        return exchange(
            client,
            &endpoints.codex_token,
            code,
            verifier,
            DEVICE_REDIRECT,
            None,
        )
        .await;
    }
    Err(OAuthError::Timeout)
}

pub async fn refresh(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    credential: &OAuthCredential,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(&endpoints.codex_token)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", credential.refresh.as_str()),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|_| OAuthError::Transport("Codex token refresh failed".into()))?;
    parse_token(response, Some(&credential.refresh)).await
}

async fn exchange(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect: &str,
    prior_refresh: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
        ])
        .send()
        .await
        .map_err(|_| OAuthError::Transport("Codex token exchange failed".into()))?;
    parse_token(response, prior_refresh).await
}

async fn parse_token(
    response: reqwest::Response,
    prior_refresh: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    if !response.status().is_success() {
        return Err(OAuthError::InvalidResponse(format!(
            "Codex OAuth failed ({})",
            response.status().as_u16()
        )));
    }
    let token: TokenResponse =
        parse_json_response_bounded(response, "Codex token response is invalid").await?;
    let account_id = account_id(&token.access_token)?;
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token
            .refresh_token
            .or_else(|| prior_refresh.map(str::to_owned))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| OAuthError::InvalidResponse("Codex refresh token is missing".into()))?,
        expires: now_ms().saturating_add(token.expires_in.saturating_mul(1000)),
        account_id: Some(account_id),
    })
}

pub fn account_id(access_token: &str) -> Result<String, OAuthError> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or_else(|| OAuthError::InvalidResponse("Codex access token is not a JWT".into()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| OAuthError::InvalidResponse("Codex access token payload is invalid".into()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| OAuthError::InvalidResponse("Codex access token claims are invalid".into()))?;
    value
        .get(JWT_AUTH_CLAIM)
        .and_then(|claims| claims.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| OAuthError::InvalidResponse("Codex account id is missing".into()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
