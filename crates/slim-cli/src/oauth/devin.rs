use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::browser::BrowserLauncher;
use super::callback::await_callback;
use super::pkce::generate_pkce;
use super::{
    parse_json_response_bounded, OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress,
};

const CALLBACK_PORT: u16 = 59653;
const CALLBACK_PATH: &str = "/callback";
/// A token without a JWT `exp` claim is treated as valid for one year.
const FALLBACK_LIFETIME_MS: u64 = 365 * 24 * 60 * 60 * 1000;
const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

/// Devin's CLI browser login: PKCE without a client id, redirected to loopback.
/// The session token never refreshes, so `refresh` repeats it and an expired
/// session signs in again.
pub async fn login(
    client: &reqwest::Client,
    endpoints: &OAuthEndpoints,
    browser: Arc<dyn BrowserLauncher>,
    progress: &mpsc::UnboundedSender<OAuthProgress>,
    cancel: watch::Receiver<bool>,
) -> Result<OAuthCredential, OAuthError> {
    let pkce = generate_pkce()?;
    let state = generate_uuid()?;
    let listener = match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await {
        Ok(listener) => listener,
        Err(_) => TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| OAuthError::Callback("OAuth callback port is unavailable".into()))?,
    };
    let port = listener
        .local_addr()
        .map_err(|_| OAuthError::Callback("OAuth callback port is unavailable".into()))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let mut url = reqwest::Url::parse(&endpoints.devin_authorize)
        .map_err(|_| OAuthError::InvalidResponse("Devin authorization URL is invalid".into()))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("prompt", "select_account");
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
        "Exchanging Devin authorization code…".into(),
    ));
    let response = client
        .post(&endpoints.devin_token)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": pkce.verifier,
        }))
        .send()
        .await
        .map_err(|_| OAuthError::Transport("Devin token exchange failed".into()))?;
    if !response.status().is_success() {
        return Err(OAuthError::InvalidResponse(format!(
            "Devin OAuth failed ({})",
            response.status().as_u16()
        )));
    }
    let token: TokenResponse =
        parse_json_response_bounded(response, "Devin token response is invalid").await?;
    if token.token.is_empty() {
        return Err(OAuthError::InvalidResponse(
            "Devin token response is incomplete".into(),
        ));
    }
    Ok(OAuthCredential {
        expires: token_expiry_ms(&token.token, now_ms()),
        refresh: token.token.clone(),
        access: token.token,
        account_id: None,
    })
}

fn generate_uuid() -> Result<String, OAuthError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| OAuthError::InvalidResponse("secure random generation failed".into()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn token_expiry_ms(token: &str, now_ms: u64) -> u64 {
    jwt_exp_ms(token).map_or(now_ms.saturating_add(FALLBACK_LIFETIME_MS), |expires| {
        expires.saturating_sub(EXPIRY_SKEW_MS)
    })
}

fn jwt_exp_ms(token: &str) -> Option<u64> {
    let payload = token.rsplit('$').next()?.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_u64()?.checked_mul(1000)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::OAuthError;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct CapturedBrowser(Mutex<Option<String>>);

    impl BrowserLauncher for CapturedBrowser {
        fn open(&self, url: &str) -> Result<(), OAuthError> {
            *self.0.lock().unwrap() = Some(url.to_owned());
            Ok(())
        }
    }

    fn query(url: &reqwest::Url, name: &str) -> Option<String> {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    async fn serve_once(listener: TcpListener, status: &str, body: &str) -> String {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let size = stream.read(&mut chunk).await.unwrap();
            request.extend_from_slice(&chunk[..size]);
            let text = String::from_utf8_lossy(&request);
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        let lower = line.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length: ")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&request).into_owned()
    }

    #[tokio::test]
    async fn login_follows_the_native_authorize_and_token_shape() {
        let token_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoints = OAuthEndpoints {
            devin_authorize: "https://devin.example/auth/cli/continue".into(),
            devin_token: format!(
                "http://{}/auth/cli/token",
                token_listener.local_addr().unwrap()
            ),
            ..OAuthEndpoints::default()
        };
        let exchange = tokio::spawn(serve_once(
            token_listener,
            "200 OK",
            r#"{"token":"session-token"}"#,
        ));
        let browser = Arc::new(CapturedBrowser::default());
        let (progress, mut progress_rx) = mpsc::unbounded_channel();
        let (_cancel, cancel) = watch::channel(false);
        let client = reqwest::Client::new();
        let login_browser = browser.clone();
        let login = tokio::spawn(async move {
            login(&client, &endpoints, login_browser, &progress, cancel).await
        });
        let Some(OAuthProgress::AuthUrl { url, .. }) = progress_rx.recv().await else {
            panic!("authorization URL expected");
        };
        let url = reqwest::Url::parse(&url).unwrap();
        assert_eq!(url.path(), "/auth/cli/continue");
        assert_eq!(query(&url, "client_id"), None);
        assert_eq!(query(&url, "response_type").as_deref(), Some("code"));
        assert_eq!(query(&url, "prompt").as_deref(), Some("select_account"));
        assert_eq!(query(&url, "code_challenge_method").as_deref(), Some("S256"));
        let redirect = query(&url, "redirect_uri").unwrap();
        assert!(redirect.starts_with("http://127.0.0.1:"));
        assert!(redirect.ends_with("/callback"));
        let state = query(&url, "state").unwrap();
        assert_eq!(state.len(), 36);

        let callback = format!("{redirect}?code=auth-code&state={state}");
        let response = reqwest::get(&callback).await.unwrap();
        assert!(response.status().is_success());

        let credential = login.await.unwrap().unwrap();
        assert_eq!(credential.access, "session-token");
        assert_eq!(credential.refresh, "session-token");
        let request = exchange.await.unwrap();
        assert!(request.starts_with("POST /auth/cli/token "));
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["code"], "auth-code");
        assert!(body["code_verifier"].is_string());
        assert_eq!(body.as_object().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn token_exchange_failure_reports_status_only() {
        let token_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoints = OAuthEndpoints {
            devin_token: format!("http://{}/token", token_listener.local_addr().unwrap()),
            ..OAuthEndpoints::default()
        };
        tokio::spawn(serve_once(token_listener, "401 Unauthorized", "secret"));
        let browser = Arc::new(CapturedBrowser::default());
        let (progress, mut progress_rx) = mpsc::unbounded_channel();
        let (_cancel, cancel) = watch::channel(false);
        let client = reqwest::Client::new();
        let login = tokio::spawn(async move {
            login(&client, &endpoints, browser, &progress, cancel).await
        });
        let Some(OAuthProgress::AuthUrl { url, .. }) = progress_rx.recv().await else {
            panic!("authorization URL expected");
        };
        let url = reqwest::Url::parse(&url).unwrap();
        let callback = format!(
            "{}?code=c&state={}",
            query(&url, "redirect_uri").unwrap(),
            query(&url, "state").unwrap()
        );
        reqwest::get(&callback).await.unwrap();
        assert_eq!(
            login.await.unwrap().unwrap_err(),
            OAuthError::InvalidResponse("Devin OAuth failed (401)".into())
        );
    }

    #[test]
    fn expiry_comes_from_the_jwt_or_the_one_year_fallback() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"exp":2000}"#);
        let jwt = format!("h.{payload}.s");
        assert_eq!(token_expiry_ms(&jwt, 0), 2_000_000 - EXPIRY_SKEW_MS);
        assert_eq!(
            token_expiry_ms(&format!("devin-session-token${jwt}"), 0),
            2_000_000 - EXPIRY_SKEW_MS
        );
        assert_eq!(token_expiry_ms("opaque", 7), 7 + FALLBACK_LIFETIME_MS);
    }

    #[test]
    fn state_is_a_version_four_uuid() {
        let uuid = generate_uuid().unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
    }
}
