use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::OAuthError;

pub async fn await_callback(
    listener: TcpListener,
    path: &str,
    expected_state: &str,
    mut cancel: watch::Receiver<bool>,
    timeout: Duration,
) -> Result<String, OAuthError> {
    let wait = async {
        loop {
            let (mut stream, address) = tokio::select! {
                accepted = listener.accept() => accepted.map_err(|_| OAuthError::Callback("OAuth callback accept failed".into()))?,
                changed = cancel.changed() => {
                    let _ = changed;
                    return Err(OAuthError::Cancelled);
                }
            };
            if !address.ip().is_loopback() {
                continue;
            }
            let mut bytes = vec![0_u8; 16 * 1024];
            let size = stream
                .read(&mut bytes)
                .await
                .map_err(|_| OAuthError::Callback("OAuth callback read failed".into()))?;
            let request = String::from_utf8_lossy(&bytes[..size]);
            let target = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or_default();
            let parsed = reqwest::Url::parse(&format!("http://localhost{target}"));
            let Ok(url) = parsed else {
                respond(&mut stream, 400, "Invalid OAuth callback").await;
                continue;
            };
            if url.path() != path {
                respond(&mut stream, 404, "OAuth callback route not found").await;
                continue;
            }
            let state = url
                .query_pairs()
                .find(|(name, _)| name == "state")
                .map(|(_, value)| value.into_owned());
            let code = url
                .query_pairs()
                .find(|(name, _)| name == "code")
                .map(|(_, value)| value.into_owned());
            if state.as_deref() != Some(expected_state) {
                respond(&mut stream, 400, "OAuth state mismatch").await;
                continue;
            }
            let Some(code) = code.filter(|code| !code.is_empty()) else {
                respond(&mut stream, 400, "OAuth code missing").await;
                continue;
            };
            respond(
                &mut stream,
                200,
                "Authentication completed. Return to Slim.",
            )
            .await;
            return Ok(code);
        }
    };
    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| OAuthError::Timeout)?
}

async fn respond(stream: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let body = format!("<html><body>{message}</body></html>");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}
