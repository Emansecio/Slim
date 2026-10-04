//! OAuth for HTTP MCP servers, application side: where the credentials live,
//! the cross-process refresh lock, and the interactive sign-in (loopback
//! callback, browser, pasted redirect URL) and sign-out.
//!
//! The protocol itself is `slim_core::mcp::McpAuth`. Nothing here runs on
//! connect: the user starts a sign-in explicitly (`/mcp login <server>`; the
//! `slim mcp login` command will call [`login`] too).
//!
//! Credentials are stored in `mcp-auth.json` beside the global config, one
//! entry per server keyed by name and URL, through the same owner-only file
//! helpers as `auth.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slim_core::mcp::{
    AuthorizationResponse, CallbackHint, LoginSummary, McpAuth, McpAuthHandle, McpOAuthError,
    McpOAuthSpec, McpOAuthState, McpOAuthStore,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc as async_mpsc, watch};

use crate::auth::{read_secure_json, update_secure_json, AuthError};
use crate::oauth::pkce::{generate_pkce, generate_state};
use crate::oauth::BrowserLauncher;

const STORE_VERSION: u64 = 1;
#[cfg(not(test))]
const STORE_FILE_NAME: &str = "mcp-auth.json";
/// How long a refresh waits for another process's refresh: longer than one
/// refresh request may take (15 s), so a live holder is waited out.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);
const REFRESH_LOCK_RETRY: Duration = Duration::from_millis(100);
/// How long the sign-in waits for the browser by default.
pub(crate) const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const CALLBACK_PATH: &str = "/callback";
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Credential store
// ---------------------------------------------------------------------------

/// `SLIM_MCP_AUTH_FILE`, else `mcp-auth.json` beside the global config.
pub(crate) fn default_store_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SLIM_MCP_AUTH_FILE").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!("slim-test-mcp-auth-{}.json", std::process::id())))
    }
    #[cfg(not(test))]
    {
        let config = crate::config::global_config_path()
            .ok_or_else(|| "unable to resolve the Slim config directory".to_owned())?;
        let directory = config
            .parent()
            .ok_or_else(|| "unable to resolve the Slim config directory".to_owned())?;
        Ok(directory.join(STORE_FILE_NAME))
    }
}

/// Key of a server's entry: name and URL, so servers sharing a URL keep
/// separate accounts and a changed URL starts from scratch.
fn store_key(name: &str, url: &str) -> String {
    format!("{name}|{url}")
}

/// Normalizes a server URL the way `McpAuth` does (fragment removed).
fn canonical_url(url: &str) -> Option<String> {
    let mut parsed = reqwest::Url::parse(url).ok()?;
    parsed.set_fragment(None);
    Some(parsed.to_string())
}

/// One server's slice of `mcp-auth.json`.
pub(crate) struct FileOAuthStore {
    path: Result<PathBuf, String>,
    key: String,
}

impl FileOAuthStore {
    #[cfg(test)]
    pub(crate) fn at(path: impl Into<PathBuf>, name: &str, url: &str) -> Self {
        Self {
            path: Ok(path.into()),
            key: store_key(name, url),
        }
    }

    /// The store at the default location. A location that cannot be resolved
    /// makes every operation fail with that reason.
    pub(crate) fn for_server(name: &str, url: &str) -> Self {
        Self {
            path: default_store_path(),
            key: store_key(name, url),
        }
    }

    fn path(&self) -> Result<&Path, String> {
        self.path.as_deref().map_err(Clone::clone)
    }

    fn describe(path: &Path, error: impl std::fmt::Display) -> String {
        format!("MCP OAuth store {}: {error}", path.display())
    }

    fn servers(document: &Value) -> Result<&serde_json::Map<String, Value>, String> {
        if document.get("version").and_then(Value::as_u64) != Some(STORE_VERSION) {
            return Err("unsupported store version".to_owned());
        }
        document
            .get("servers")
            .and_then(Value::as_object)
            .ok_or_else(|| "store has no servers table".to_owned())
    }

    fn refresh_lock_path(&self, path: &Path) -> PathBuf {
        let digest = Sha256::digest(self.key.as_bytes());
        let name = format!(".mcp-auth-refresh-{}.lock", hex(&digest[..8]));
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join(name)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl McpOAuthStore for FileOAuthStore {
    fn load(&self) -> Result<Option<McpOAuthState>, String> {
        let path = self.path()?;
        let Some(document) = read_secure_json(path).map_err(|error| Self::describe(path, error))?
        else {
            return Ok(None);
        };
        let servers = Self::servers(&document).map_err(|error| Self::describe(path, error))?;
        match servers.get(&self.key) {
            None => Ok(None),
            Some(entry) => serde_json::from_value(entry.clone())
                .map(Some)
                .map_err(|_| Self::describe(path, "invalid entry")),
        }
    }

    fn save(&self, state: &McpOAuthState) -> Result<(), String> {
        let path = self.path()?;
        let entry = serde_json::to_value(state).map_err(|error| error.to_string())?;
        let mut failure: Option<String> = None;
        let result = update_secure_json(path, |current| {
            let mut document =
                current.unwrap_or_else(|| json!({ "version": STORE_VERSION, "servers": {} }));
            if let Err(error) = Self::servers(&document) {
                // A foreign or corrupt store is never overwritten.
                failure = Some(Self::describe(path, error));
                return Err(AuthError::InvalidSchema);
            }
            let servers = document
                .get_mut("servers")
                .and_then(Value::as_object_mut)
                .ok_or(AuthError::InvalidSchema)?;
            servers.insert(self.key.clone(), entry);
            Ok(document)
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(failure.unwrap_or_else(|| Self::describe(path, error))),
        }
    }

    fn clear(&self) -> Result<bool, String> {
        let path = self.path()?;
        // A corrupt entry is exactly what logout should be able to remove, so
        // only "nothing stored" ends early.
        if let Ok(None) = self.load() {
            return Ok(false);
        }
        let mut removed = false;
        let mut failure: Option<String> = None;
        let result = update_secure_json(path, |current| {
            let Some(mut document) = current else {
                return Ok(json!({ "version": STORE_VERSION, "servers": {} }));
            };
            if let Err(error) = Self::servers(&document) {
                failure = Some(Self::describe(path, error));
                return Err(AuthError::InvalidSchema);
            }
            if let Some(servers) = document.get_mut("servers").and_then(Value::as_object_mut) {
                removed = servers.remove(&self.key).is_some();
            }
            Ok(document)
        });
        match result {
            Ok(()) => Ok(removed),
            Err(error) => Err(failure.unwrap_or_else(|| Self::describe(path, error))),
        }
    }

    fn lock_refresh(&self) -> Result<Box<dyn Send>, String> {
        let path = self.path()?;
        let lock = self.refresh_lock_path(path);
        if let Some(parent) = lock.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Self::describe(path, error))?;
        }
        RefreshLock::acquire(&lock, REFRESH_LOCK_WAIT).map(|guard| Box::new(guard) as Box<dyn Send>)
    }
}

// ---------------------------------------------------------------------------
// Cross-process refresh lock
// ---------------------------------------------------------------------------

/// An OS advisory lock on a persistent lock file, held for as long as the
/// guard lives. The kernel releases it when the holder exits or crashes, so
/// there is no stale-lock takeover to race on, and dropping the guard only
/// closes its own handle: the file is never deleted, so it can never be
/// another holder's lock that goes away.
pub(crate) struct RefreshLock {
    _file: std::fs::File,
}

impl RefreshLock {
    pub(crate) fn acquire(path: &Path, wait: Duration) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| format!("MCP OAuth refresh lock: {error}"))?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(
                            "timed out waiting for another Slim process to refresh the MCP tokens"
                                .to_owned(),
                        );
                    }
                    std::thread::sleep(REFRESH_LOCK_RETRY);
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!("MCP OAuth refresh lock: {error}"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/// The OAuth handle of an HTTP server that has no `Authorization` header of
/// its own (`settings.client_secret` already resolved). `None` when the URL
/// is not a valid http(s) URL: the connect reports that.
pub(crate) fn build_auth(name: &str, url: &str, settings: McpOAuthSpec) -> Option<McpAuthHandle> {
    let canonical = canonical_url(url)?;
    let store = Arc::new(FileOAuthStore::for_server(name, &canonical));
    McpAuth::new(name, url, settings, store)
        .ok()
        .map(McpAuthHandle)
}

// ---------------------------------------------------------------------------
// Sign-in
// ---------------------------------------------------------------------------

/// What the sign-in tells its caller while it runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LoginNotice {
    /// The authorization URL to open. `browser_opened`: the system browser
    /// was asked to open it; otherwise the user must open it. A redirect that
    /// does not reach this machine can be completed by pasting the redirect
    /// URL.
    AuthorizationUrl {
        url: String,
        browser_opened: bool,
        redirect_uri: String,
    },
    /// Something the user should know (a pasted URL that did not fit).
    Warning(String),
}

pub(crate) struct LoginOptions {
    /// Give up after this long without a response (default [`LOGIN_TIMEOUT`]).
    pub timeout: Duration,
    pub browser: Arc<dyn BrowserLauncher>,
    pub notify: Arc<dyn Fn(LoginNotice) + Send + Sync>,
    /// Redirect URLs the user pasted (when the browser cannot reach the
    /// callback, for example over SSH).
    pub pasted: Option<async_mpsc::UnboundedReceiver<String>>,
    /// Becomes `true` when the user cancels.
    pub cancel: watch::Receiver<bool>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LoginError {
    Cancelled,
    Timeout,
    /// Text safe to show.
    Failed(String),
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("sign-in cancelled"),
            Self::Timeout => formatter.write_str("sign-in timed out"),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl From<McpOAuthError> for LoginError {
    fn from(error: McpOAuthError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// Binds the loopback callback listener: the configured port exactly, else
/// the port the stored client registered (so the registration stays valid),
/// else any free port.
async fn bind_callback(hint: CallbackHint) -> Result<(TcpListener, u16), LoginError> {
    if let Some(port) = hint.configured {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|error| {
            LoginError::Failed(format!(
                "cannot listen on 127.0.0.1:{port} for the OAuth callback (oauth.callback_port): {error}"
            ))
        })?;
        return Ok((listener, port));
    }
    if let Some(port) = hint.registered {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port));
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|error| {
        LoginError::Failed(format!("cannot listen for the OAuth callback: {error}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|error| {
            LoginError::Failed(format!("cannot listen for the OAuth callback: {error}"))
        })?
        .port();
    Ok((listener, port))
}

/// Signs in to the server: opens the authorization URL in the browser,
/// waits for the loopback callback (or a pasted redirect URL), exchanges the
/// code and stores the tokens. The listener lives only for this call.
pub(crate) async fn login(
    auth: &Arc<McpAuth>,
    options: LoginOptions,
) -> Result<LoginSummary, LoginError> {
    let LoginOptions {
        timeout,
        browser,
        notify,
        mut pasted,
        mut cancel,
    } = options;
    let hint = auth.callback_hint().await;
    let (listener, port) = bind_callback(hint).await?;
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let pkce = generate_pkce()
        .map_err(|_| LoginError::Failed("secure random generation failed".into()))?;
    let state = generate_state()
        .map_err(|_| LoginError::Failed("secure random generation failed".into()))?;
    let pending = auth
        .begin_login(&redirect_uri, &pkce.verifier, &state)
        .await?;
    let url = pending.authorization_url.clone();
    let browser_opened = {
        let browser = Arc::clone(&browser);
        let url = url.clone();
        tokio::task::spawn_blocking(move || browser.open(&url).is_ok())
            .await
            .unwrap_or(false)
    };
    notify(LoginNotice::AuthorizationUrl {
        url,
        browser_opened,
        redirect_uri: redirect_uri.clone(),
    });
    let response = tokio::time::timeout(timeout, async {
        loop {
            tokio::select! {
                biased;
                changed = cancel.changed() => {
                    // A dropped sender cancels as well.
                    let _ = changed;
                    return Err(LoginError::Cancelled);
                }
                callback = accept_callback(&listener, &state) => return callback,
                input = recv_pasted(&mut pasted) => match parse_redirect_input(&input, &state) {
                    Ok(response) => return Ok(response),
                    Err(message) => notify(LoginNotice::Warning(message)),
                },
            }
        }
    })
    .await
    .map_err(|_| LoginError::Timeout)??;
    drop(listener);
    Ok(auth.complete_login(pending, response).await?)
}

/// Next pasted line; never completes when there is no paste channel or it
/// closed.
async fn recv_pasted(pasted: &mut Option<async_mpsc::UnboundedReceiver<String>>) -> String {
    match pasted {
        Some(receiver) => match receiver.recv().await {
            Some(input) => input,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// The authorization response out of a pasted redirect URL.
pub(crate) fn parse_redirect_input(
    input: &str,
    expected_state: &str,
) -> Result<AuthorizationResponse, String> {
    let url = reqwest::Url::parse(input.trim())
        .map_err(|_| "Expected the full redirect URL from the browser address bar".to_owned())?;
    let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    if let Some(error) = params.get("error") {
        let description = params.get("error_description").unwrap_or(error);
        return Err(format!(
            "authorization failed: {}",
            sanitize(description, 200)
        ));
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err("The redirect URL belongs to a different sign-in".to_owned());
    }
    let code = params
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| "The redirect URL does not contain an authorization code".to_owned())?;
    Ok(AuthorizationResponse {
        code: code.clone(),
        state: Some(expected_state.to_owned()),
        iss: params.get("iss").cloned(),
    })
}

fn sanitize(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect()
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Waits on the loopback listener for the browser's redirect. Requests that
/// are not this sign-in's response (other paths, other peers, a wrong
/// `state`) are answered and ignored: a stray or forged request cannot abort
/// the sign-in.
async fn accept_callback(
    listener: &TcpListener,
    expected_state: &str,
) -> Result<AuthorizationResponse, LoginError> {
    loop {
        let (mut stream, peer) = listener
            .accept()
            .await
            .map_err(|_| LoginError::Failed("OAuth callback accept failed".into()))?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let Some(target) = read_request_target(&mut stream).await else {
            respond(&mut stream, 400, "Invalid OAuth callback").await;
            continue;
        };
        let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
            respond(&mut stream, 400, "Invalid OAuth callback").await;
            continue;
        };
        if url.path() != CALLBACK_PATH {
            respond(&mut stream, 404, "OAuth callback route not found").await;
            continue;
        }
        let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
        if params.get("state").map(String::as_str) != Some(expected_state) {
            respond(&mut stream, 400, "Invalid or expired OAuth state").await;
            continue;
        }
        if let Some(error) = params.get("error") {
            let description = sanitize(params.get("error_description").unwrap_or(error), 200);
            respond(
                &mut stream,
                200,
                &format!("Authorization failed. You may close this window. ({description})"),
            )
            .await;
            return Err(LoginError::Failed(format!(
                "authorization failed: {description}"
            )));
        }
        let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
            respond(&mut stream, 400, "Missing authorization code").await;
            return Err(LoginError::Failed(
                "the OAuth callback did not include an authorization code".into(),
            ));
        };
        respond(
            &mut stream,
            200,
            "Signed in to the MCP server. You may now close this page.",
        )
        .await;
        return Ok(AuthorizationResponse {
            code: code.clone(),
            state: Some(expected_state.to_owned()),
            iss: params.get("iss").cloned(),
        });
    }
}

/// Request target of a GET, bounded in size and time.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let read = async {
        let mut bytes = Vec::with_capacity(1024);
        loop {
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if bytes.len() >= MAX_CALLBACK_REQUEST_BYTES {
                return None;
            }
            let mut chunk = [0_u8; 1024];
            let limit = chunk.len().min(MAX_CALLBACK_REQUEST_BYTES - bytes.len());
            match stream.read(&mut chunk[..limit]).await {
                Ok(0) | Err(_) => return None,
                Ok(size) => bytes.extend_from_slice(&chunk[..size]),
            }
        }
        let request = std::str::from_utf8(&bytes).ok()?;
        let mut parts = request.lines().next()?.split_whitespace();
        (parts.next()? == "GET").then_some(())?;
        parts.next().map(str::to_owned)
    };
    tokio::time::timeout(CALLBACK_READ_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Slim</title><body>{}</body>",
        escape_html(message)
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ---------------------------------------------------------------------------
// Sign-out
// ---------------------------------------------------------------------------

/// Deletes the server's stored credentials. `true` when there were some.
pub(crate) async fn logout(auth: &Arc<McpAuth>) -> Result<bool, String> {
    auth.logout().await.map_err(|error| error.to_string())
}

/// The loopback MCP + authorization server the sign-in tests (here and in the
/// TUI) talk to. Included once: Rust forbids loading one file as two modules.
#[cfg(test)]
#[path = "../../../../tests/support/mcp_oauth_mock.rs"]
pub(crate) mod oauth_mock;

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
