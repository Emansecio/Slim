use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::sync::{mpsc, watch};

use super::browser::BrowserLauncher;
use super::{
    parse_json_response_bounded, OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress,
};

/// Public device-flow client, same value as the opencode and pi references.
/// xAI has no browser-redirect login for API access; the device flow shows a
/// user code the human approves at the verification URI.
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Refresh 5 minutes before real expiry, like the pi reference.
const EXPIRY_SKEW_MS: u64 = 300_000;

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

pub async fn login(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    browser: Arc<dyn BrowserLauncher>,
    progress: &mpsc::UnboundedSender<OAuthProgress>,
    mut cancel: watch::Receiver<bool>,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(&endpoints.xai_device_code)
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await
        .map_err(|_| OAuthError::Transport("xAI device authorization failed".into()))?;
    if !response.status().is_success() {
        return Err(OAuthError::InvalidResponse(format!(
            "xAI device authorization failed ({})",
            response.status().as_u16()
        )));
    }
    let device: DeviceCode =
        parse_json_response_bounded(response, "xAI device response is invalid").await?;
    let _ = progress.send(OAuthProgress::AuthUrl {
        url: device
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| device.verification_uri.clone()),
        user_code: Some(device.user_code.clone()),
    });
    let _ = browser.open(&device.verification_uri);
    let deadline = now_ms().saturating_add(device.expires_in.saturating_mul(1000));
    let interval = device.interval.max(1);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            changed = cancel.changed() => {
                let _ = changed;
                return Err(OAuthError::Cancelled);
            }
        }
        if now_ms() >= deadline {
            return Err(OAuthError::Timeout);
        }
        let response = client
            .post(&endpoints.xai_token)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
            ])
            .send()
            .await
            .map_err(|_| OAuthError::Transport("xAI device polling failed".into()))?;
        if response.status().is_success() {
            return parse_token(response, None).await;
        }
        let value: serde_json::Value =
            parse_json_response_bounded(response, "xAI device token is invalid").await?;
        match value.get("error").and_then(|value| value.as_str()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Some("access_denied" | "authorization_denied") => {
                return Err(OAuthError::InvalidResponse(
                    "xAI device authorization was denied".into(),
                ));
            }
            Some("expired_token") => {
                return Err(OAuthError::InvalidResponse(
                    "xAI device code expired - please re-run login".into(),
                ));
            }
            _ => {
                return Err(OAuthError::InvalidResponse(
                    "xAI device token exchange failed".into(),
                ));
            }
        }
    }
}

pub async fn refresh(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    credential: &OAuthCredential,
) -> Result<OAuthCredential, OAuthError> {
    let response = client
        .post(&endpoints.xai_token)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", credential.refresh.as_str()),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|_| OAuthError::Transport("xAI token refresh failed".into()))?;
    parse_token(response, Some(&credential.refresh)).await
}

async fn parse_token(
    response: reqwest::Response,
    prior_refresh: Option<&str>,
) -> Result<OAuthCredential, OAuthError> {
    if !response.status().is_success() {
        return Err(OAuthError::InvalidResponse(format!(
            "xAI OAuth failed ({})",
            response.status().as_u16()
        )));
    }
    let token: TokenResponse =
        parse_json_response_bounded(response, "xAI token response is invalid").await?;
    if token.access_token.trim().is_empty() {
        return Err(OAuthError::InvalidResponse(
            "xAI access token is missing".into(),
        ));
    }
    Ok(OAuthCredential {
        access: token.access_token,
        refresh: token
            .refresh_token
            .or_else(|| prior_refresh.map(str::to_owned))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| OAuthError::InvalidResponse("xAI refresh token is missing".into()))?,
        expires: now_ms()
            .saturating_add(token.expires_in.unwrap_or(3600).saturating_mul(1000))
            .saturating_sub(EXPIRY_SKEW_MS),
        account_id: None,
    })
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    #[serde(default = "default_interval")]
    interval: u64,
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
