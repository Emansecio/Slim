use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::OAuthError;

const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(2);

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
            let Some(bytes) = read_request(&mut stream, &mut cancel).await? else {
                respond(&mut stream, 400, "Invalid OAuth callback").await;
                continue;
            };
            let Ok(request) = std::str::from_utf8(&bytes) else {
                respond(&mut stream, 400, "Invalid OAuth callback").await;
                continue;
            };
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

async fn read_request(
    stream: &mut tokio::net::TcpStream,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Option<Vec<u8>>, OAuthError> {
    let read = async {
        let mut bytes = Vec::with_capacity(1024);
        loop {
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(Some(bytes));
            }
            if bytes.len() == MAX_CALLBACK_REQUEST_BYTES {
                return Ok(None);
            }
            let mut chunk = [0_u8; 1024];
            let limit = chunk.len().min(MAX_CALLBACK_REQUEST_BYTES - bytes.len());
            let size = tokio::select! {
                result = stream.read(&mut chunk[..limit]) => match result {
                    Ok(size) => size,
                    Err(_) => return Ok(None),
                },
                changed = cancel.changed() => {
                    let _ = changed;
                    return Err(OAuthError::Cancelled);
                }
            };
            if size == 0 {
                return Ok(None);
            }
            bytes.extend_from_slice(&chunk[..size]);
        }
    };
    tokio::time::timeout(CALLBACK_READ_TIMEOUT, read)
        .await
        .unwrap_or(Ok(None))
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
