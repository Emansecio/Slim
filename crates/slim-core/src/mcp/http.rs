use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::mcp::client::{
    parse_progress, ClientContext, McpProgress, McpProgressSink, McpServerHandshake,
};
use crate::mcp::oauth::{AuthSendError, McpAuth};
use crate::mcp::spec::{
    McpCancellation, McpCleanupStatus, McpConnection, McpError, McpInterruption, McpRequestOutcome,
    MCP_PROTOCOL_VERSION,
};
use crate::mcp::sse::{SseDecoder, SseEvent};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const SESSION_HEADER: &str = "mcp-session-id";
/// Longest `mcp-session-id` accepted; it is echoed in every later request.
const MAX_SESSION_ID_BYTES: usize = 1024;
/// Bound on the best-effort `notifications/cancelled` POST: cancellation must
/// not wait on a server that has stopped answering.
const CANCEL_NOTIFY_WAIT: Duration = Duration::from_millis(500);
/// Bound on the `DELETE` that ends a session on close.
const DELETE_SESSION_WAIT: Duration = Duration::from_secs(1);

/// A server's `retry:` below this is raised to it: a hostile or broken
/// server must not turn reconnection into a hot loop.
const MIN_SERVER_RETRY: Duration = Duration::from_millis(100);

/// Backoff for a dropped SSE stream (the server-to-client `GET` stream and
/// resumable response streams): `initial` doubling up to `max`, giving up
/// after `retries` consecutive failures. A server `retry:` field replaces the
/// computed delay (bounded by `max`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReconnectPolicy {
    pub initial: Duration,
    pub max: Duration,
    pub retries: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            retries: 5,
        }
    }
}

impl ReconnectPolicy {
    pub(crate) fn delay(&self, attempt: u32, server_requested: Option<Duration>) -> Duration {
        match server_requested {
            Some(requested) => requested.max(MIN_SERVER_RETRY).min(self.max),
            None => self
                .initial
                .saturating_mul(1_u32 << attempt.min(16))
                .min(self.max),
        }
    }
}

/// Statuses worth another attempt: overloaded or restarting servers.
fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 429 || (status >= 500 && status != 501)
}

/// Network failures and transient HTTP statuses. Used for connect retries and
/// for deciding whether a dropped stream is reopened.
pub(crate) fn is_transient_error(error: &McpError) -> bool {
    match error {
        McpError::Io(_) => true,
        McpError::Server { code, .. } => u16::try_from(*code).is_ok_and(is_transient_status),
        _ => false,
    }
}

/// Requests that can run twice without harm; only these are retried on a new
/// session after the server forgot the old one. `tools/call` never is.
fn is_idempotent(method: &str) -> bool {
    matches!(
        method,
        "tools/list" | "resources/list" | "resources/read" | "resources/templates/list"
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Display-safe form of a configured URL: query, fragment and userinfo can
/// carry credentials, so they never appear in errors or status text.
pub(crate) fn sanitized_url(raw: &str) -> String {
    let base = raw.split(['?', '#']).next().unwrap_or(raw);
    match base.split_once("://") {
        Some((scheme, rest)) => {
            let path_start = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at(path_start);
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{scheme}://{host}{path}")
        }
        None => base
            .rsplit_once('@')
            .map_or_else(|| base.to_owned(), |(_, rest)| rest.to_owned()),
    }
}

/// Where progress for one in-flight request goes. Progress that arrives on
/// any stream (the request's own response stream or the standalone `GET`
/// stream) reaches the sink and renews the request timeout.
struct ProgressRoute {
    sink: McpProgressSink,
    renewed: Notify,
}

impl ProgressRoute {
    fn deliver(&self, update: McpProgress) {
        (self.sink)(update);
        self.renewed.notify_one();
    }
}

struct PendingGuard<'a> {
    shared: &'a HttpShared,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(&self.shared.pending).remove(&self.id);
    }
}

/// Lifecycle of the standalone server-to-client `GET` stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ListenerState {
    NotStarted = 0,
    Connecting = 1,
    Open = 2,
    /// The server answered 405: it offers no `GET` stream.
    Unsupported = 3,
    Reconnecting = 4,
    /// Gave up (retries exhausted, a non-retryable status, or closed).
    Stopped = 5,
}

#[cfg(test)]
impl ListenerState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Connecting,
            2 => Self::Open,
            3 => Self::Unsupported,
            4 => Self::Reconnecting,
            5 => Self::Stopped,
            _ => Self::NotStarted,
        }
    }
}

/// Resume position of an SSE stream across reconnects.
#[derive(Default)]
struct StreamCursor {
    last_event_id: Option<String>,
    retry: Option<Duration>,
    /// Some event arrived since the counter was last reset.
    received: bool,
}

impl StreamCursor {
    fn absorb(&mut self, decoder: &SseDecoder) {
        if let Some(id) = decoder.last_event_id() {
            self.last_event_id = Some(id.to_owned());
        }
        if let Some(retry) = decoder.retry() {
            self.retry = Some(retry);
        }
        self.received |= decoder.events_seen();
    }
}

/// How a response stream finished.
enum StreamEnd {
    /// The response to the awaited request.
    Response(Result<Value, McpError>),
    /// The stream ended without it.
    Ended,
}

/// State shared between the connection and its background `GET` listener.
struct HttpShared {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Option<String>>,
    /// The session was ended (DELETE sent or nothing to end); never repeat it.
    session_ended: AtomicBool,
    /// Revision negotiated in `initialize`, replayed in
    /// `MCP-Protocol-Version`; the requested revision until then.
    protocol_version: Mutex<String>,
    /// Ended for good (closed by the owner, or the session is unrecoverable).
    closed: AtomicBool,
    /// The server forgot the session. Recoverable: a renewal clears it, and
    /// until then the connection reports itself closed so its owner can
    /// reconnect instead.
    expired: AtomicBool,
    context: Arc<ClientContext>,
    timeout: Duration,
    pending: Mutex<HashMap<u64, Arc<ProgressRoute>>>,
    reconnect: ReconnectPolicy,
    listener_state: AtomicU8,
    /// OAuth for servers without a configured `Authorization` header.
    auth: Option<Arc<McpAuth>>,
}

impl HttpShared {
    fn session(&self) -> Option<String> {
        lock(&self.session).clone()
    }

    fn has_session(&self) -> bool {
        lock(&self.session).is_some()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.expired.load(Ordering::Acquire)
    }

    /// Flags the session as forgotten, unless it was replaced since `observed`
    /// (a stale stream must not condemn a freshly renewed session). The check
    /// and the flag happen under the session lock, which a renewal also takes.
    fn expire_session(&self, observed: &Option<String>) {
        let session = lock(&self.session);
        if *session == *observed {
            self.expired.store(true, Ordering::Release);
        }
    }

    fn set_listener_state(&self, state: ListenerState) {
        self.listener_state.store(state as u8, Ordering::Release);
    }

    /// Protocol version, configured headers and session on every request.
    fn with_common_headers(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let protocol_version = lock(&self.protocol_version).clone();
        request = request.header("MCP-Protocol-Version", protocol_version);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if let Some(token) = self.auth.as_ref().and_then(|auth| auth.bearer()) {
            if let Ok(mut value) =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            {
                value.set_sensitive(true);
                request = request.header(reqwest::header::AUTHORIZATION, value);
            }
        }
        if let Some(session) = self.session() {
            request = request.header(SESSION_HEADER, session);
        }
        request
    }

    /// POST builder without a total timeout: callers bound each phase
    /// themselves so progress notifications can extend a long-running call.
    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        let request = self
            .client
            .post(&self.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(body);
        self.with_common_headers(request)
    }

    fn remember_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= MAX_SESSION_ID_BYTES)
        {
            let mut session = lock(&self.session);
            if session.as_deref() != Some(value) {
                *session = Some(value.to_owned());
                self.session_ended.store(false, Ordering::Release);
            }
        }
    }

    /// `session_sent`: the request carried `mcp-session-id`. A 404 then means
    /// the server dropped the session; without one it is an ordinary error.
    fn check_status(
        &self,
        response: &reqwest::Response,
        session_sent: bool,
    ) -> Result<(), McpError> {
        let status = response.status();
        if status.as_u16() == 404 && session_sent {
            return Err(McpError::SessionExpired);
        }
        if !status.is_success() {
            return Err(McpError::Server {
                code: i64::from(status.as_u16()),
                message: status.canonical_reason().unwrap_or("http error").to_owned(),
            });
        }
        Ok(())
    }

    fn timed_out(&self) -> McpError {
        McpError::Timeout(self.timeout)
    }

    /// Network-level failure (refused, reset, TLS, ...). Reported as `Io` so
    /// callers can tell it from a protocol violation and retry it.
    fn transport_error(&self, context: &str, error: reqwest::Error) -> McpError {
        if error.is_timeout() {
            self.timed_out()
        } else {
            McpError::Io(std::io::Error::other(format!(
                "{context}: {}",
                error.without_url()
            )))
        }
    }

    /// Awaits `future`, failing with a timeout at `*deadline`. Progress that
    /// reaches `route` (from any stream) moves the deadline forward.
    async fn until_deadline<F: Future>(
        &self,
        future: F,
        deadline: &mut Instant,
        route: Option<&ProgressRoute>,
    ) -> Result<F::Output, McpError> {
        tokio::pin!(future);
        loop {
            let renewed = async {
                match route {
                    Some(route) => route.renewed.notified().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                output = &mut future => return Ok(output),
                () = renewed => *deadline = Instant::now() + self.timeout,
                () = tokio::time::sleep_until(*deadline) => return Err(self.timed_out()),
            }
        }
    }

    fn deliver_progress(&self, token: u64, update: McpProgress) {
        let route = lock(&self.pending).get(&token).cloned();
        if let Some(route) = route {
            route.deliver(update);
        }
    }

    /// Answers a server-to-client request received on an SSE stream with a
    /// fresh POST (`ping` and `roots/list` are served, everything else is
    /// MethodNotFound) so the remote side never waits on us.
    async fn answer_server_request(&self, id: Value, method: &str) {
        let response = self.context.answer_request(&id, method);
        let _ = tokio::time::timeout(self.timeout, self.post(&response).send()).await;
    }

    /// Routes one message that is not the awaited response.
    async fn dispatch_inbound(&self, inbound: Inbound) {
        match inbound {
            Inbound::ServerRequest { id, method } => {
                self.answer_server_request(id, &method).await;
            }
            Inbound::Progress { token, update } => self.deliver_progress(token, update),
            Inbound::Notification { method, params } => {
                self.context.on_notification(&method, &params);
            }
            Inbound::Response(_) | Inbound::Other => {}
        }
    }

    /// POSTs one request and reads its answer. `deadline` bounds every phase
    /// and moves forward on progress for this request.
    async fn send_request(
        &self,
        body: &Value,
        expected_id: u64,
        deadline: &mut Instant,
        route: Option<&ProgressRoute>,
    ) -> Result<Value, McpError> {
        let session_sent = self.has_session();
        let response = self
            .until_deadline(
                self.send_authed(|| self.post(body), "http request failed"),
                deadline,
                route,
            )
            .await??;
        self.check_status(&response, session_sent)?;
        self.remember_session(&response);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if content_type.contains("text/event-stream") {
            self.read_sse_response(response, expected_id, deadline, route)
                .await
        } else {
            if response
                .content_length()
                .is_some_and(|len| len > MAX_MESSAGE_BYTES as u64)
            {
                return Err(McpError::Protocol("response exceeds 16 MiB".into()));
            }
            // Bounded streamed read: the Content-Length pre-check does not
            // cover close-delimited or chunked bodies.
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = self.until_deadline(stream.next(), deadline, route).await? {
                let chunk = chunk.map_err(|error| self.transport_error("http body", error))?;
                if bytes.len() + chunk.len() > MAX_MESSAGE_BYTES {
                    return Err(McpError::Protocol("response exceeds 16 MiB".into()));
                }
                bytes.extend_from_slice(&chunk);
            }
            let message: Value = serde_json::from_slice(&bytes)
                .map_err(|error| McpError::Protocol(format!("invalid JSON response: {error}")))?;
            if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
                return Err(McpError::Protocol("response id mismatch".into()));
            }
            extract_result(message)
        }
    }

    /// Reads the SSE stream that answers one request. When the stream ends or
    /// breaks before the response and the server issued event ids, it is
    /// resumed with `GET` + `Last-Event-ID` (the server may close response
    /// streams at will); otherwise the session is considered dead.
    async fn read_sse_response(
        &self,
        response: reqwest::Response,
        expected_id: u64,
        deadline: &mut Instant,
        route: Option<&ProgressRoute>,
    ) -> Result<Value, McpError> {
        let mut cursor = StreamCursor::default();
        let mut attempt = 0_u32;
        let mut current = response;
        loop {
            let mut decoder = SseDecoder::new(MAX_MESSAGE_BYTES);
            let ended = self
                .read_response_events(current, &mut decoder, expected_id, deadline, route)
                .await;
            cursor.absorb(&decoder);
            let failure = match ended {
                Ok(StreamEnd::Response(result)) => return result,
                Ok(StreamEnd::Ended) => None,
                Err(error @ McpError::Io(_)) => Some(error),
                Err(error) => return Err(error),
            };
            if std::mem::take(&mut cursor.received) {
                attempt = 0;
            }
            if cursor.last_event_id.is_none() || attempt >= self.reconnect.retries {
                return match failure {
                    Some(error) => Err(error),
                    None => {
                        // Stream ended without our response: the session is
                        // dead, so flag it and let the reconnect logic of
                        // the caller take over.
                        self.closed.store(true, Ordering::Release);
                        Err(McpError::Closed)
                    }
                };
            }
            current = self
                .resume_response_stream(&mut cursor, &mut attempt, deadline, route)
                .await?;
        }
    }

    /// Reopens a dropped response stream, backing off between attempts. Any
    /// failure is reported as an interrupted stream: the request was already
    /// accepted, so the caller must not treat it as a clean failure.
    async fn resume_response_stream(
        &self,
        cursor: &mut StreamCursor,
        attempt: &mut u32,
        deadline: &mut Instant,
        route: Option<&ProgressRoute>,
    ) -> Result<reqwest::Response, McpError> {
        let interrupted = |detail: &str| {
            McpError::Io(std::io::Error::other(format!(
                "response stream interrupted and could not be resumed: {detail}"
            )))
        };
        loop {
            let delay = self.reconnect.delay(*attempt, cursor.retry);
            *attempt += 1;
            self.until_deadline(tokio::time::sleep(delay), deadline, route)
                .await?;
            let opened = self
                .until_deadline(
                    self.open_get(cursor.last_event_id.as_deref()),
                    deadline,
                    route,
                )
                .await?;
            match opened {
                Ok(Some(response)) => return Ok(response),
                // No GET stream means nothing to resume from.
                Ok(None) => {
                    self.closed.store(true, Ordering::Release);
                    return Err(McpError::Closed);
                }
                Err(error @ McpError::SessionExpired) => return Err(error),
                Err(error) if is_transient_error(&error) && *attempt < self.reconnect.retries => {}
                Err(error) => return Err(interrupted(&error.to_string())),
            }
        }
    }

    /// Reads events until the response to `expected_id` arrives or the stream
    /// ends. Server requests and notifications met on the way are served.
    async fn read_response_events(
        &self,
        response: reqwest::Response,
        decoder: &mut SseDecoder,
        expected_id: u64,
        deadline: &mut Instant,
        route: Option<&ProgressRoute>,
    ) -> Result<StreamEnd, McpError> {
        let mut stream = response.bytes_stream();
        loop {
            let Some(chunk) = self.until_deadline(stream.next(), deadline, route).await? else {
                return Ok(StreamEnd::Ended);
            };
            let chunk = chunk.map_err(|error| self.transport_error("sse stream", error))?;
            decoder.push(&chunk)?;
            while let Some(event) = decoder.next_event()? {
                let Some(message) = parse_message(&event)? else {
                    continue;
                };
                match classify_inbound(&message, Some(expected_id)) {
                    Inbound::Response(result) => return Ok(StreamEnd::Response(result)),
                    other => self.dispatch_inbound(other).await,
                }
            }
        }
    }

    /// Opens the standalone `GET` SSE stream. `Ok(None)`: the server answered
    /// 405 and offers none.
    async fn open_get(
        &self,
        last_event_id: Option<&str>,
    ) -> Result<Option<reqwest::Response>, McpError> {
        let session_sent = self.has_session();
        let build = || {
            let mut request = self.with_common_headers(
                self.client
                    .get(&self.url)
                    .header(reqwest::header::ACCEPT, "text/event-stream"),
            );
            if let Some(id) = last_event_id {
                request = request.header("Last-Event-ID", id);
            }
            request
        };
        let response = tokio::time::timeout(
            self.timeout,
            self.send_authed(build, "http stream request failed"),
        )
        .await
        .map_err(|_| self.timed_out())??;
        if response.status().as_u16() == 405 {
            return Ok(None);
        }
        self.check_status(&response, session_sent)?;
        self.remember_session(&response);
        let is_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if !is_stream {
            return Err(McpError::Protocol(
                "unsupported MCP GET response content type".into(),
            ));
        }
        Ok(Some(response))
    }

    /// Reads the standalone stream until it ends. Idle time is not bounded:
    /// the stream exists to wait for the server.
    async fn read_listener_stream(
        &self,
        response: reqwest::Response,
        decoder: &mut SseDecoder,
    ) -> Result<(), McpError> {
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| self.transport_error("http stream read failed", error))?;
            decoder.push(&chunk)?;
            while let Some(event) = decoder.next_event()? {
                // A malformed message cannot be attributed to any request.
                let Ok(Some(message)) = parse_message(&event) else {
                    continue;
                };
                let inbound = classify_inbound(&message, None);
                self.dispatch_inbound(inbound).await;
            }
        }
        Ok(())
    }

    /// `DELETE` ends the session on the server; bounded, best effort (a
    /// server may answer 405 or not at all).
    async fn send_delete(&self) {
        if !self.has_session() {
            return;
        }
        let request = self.with_common_headers(self.client.delete(&self.url));
        let _ = tokio::time::timeout(DELETE_SESSION_WAIT, request.send()).await;
    }

    /// A notification POST that waits for the status.
    async fn post_notification(&self, body: &Value) -> Result<(), McpError> {
        let session_sent = self.has_session();
        let response = tokio::time::timeout(
            self.timeout,
            self.send_authed(|| self.post(body), "http request failed"),
        )
        .await
        .map_err(|_| self.timed_out())??;
        self.check_status(&response, session_sent)
    }
}

impl HttpShared {
    /// Sends the request `build` makes. With OAuth the token is refreshed
    /// ahead of expiry and a 401 or a 403 `insufficient_scope` is handled by
    /// [`McpAuth::execute`]; without it this is a plain send.
    async fn send_authed(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
        context: &str,
    ) -> Result<reqwest::Response, McpError> {
        let Some(auth) = &self.auth else {
            return build()
                .send()
                .await
                .map_err(|error| self.transport_error(context, error));
        };
        auth.execute(build).await.map_err(|error| match error {
            AuthSendError::Transport(error) => self.transport_error(context, error),
            AuthSendError::AuthRequired(reason) => McpError::AuthRequired(reason),
            AuthSendError::Failed(message) => McpError::Io(std::io::Error::other(message)),
        })
    }
}

/// Keeps the standalone `GET` stream open for the life of the connection:
/// server notifications (list changes, logging, progress) and server requests
/// (`roots/list`, `ping`) arrive here. Reconnects with backoff, resuming from
/// the last event id.
async fn run_listener(shared: Arc<HttpShared>) {
    let policy = shared.reconnect;
    let mut cursor = StreamCursor::default();
    let mut attempt = 0_u32;
    shared.set_listener_state(ListenerState::Connecting);
    // The session this stream belongs to: a 404 only condemns the connection
    // while that session is still the current one (a renewal replaces it).
    let session = shared.session();
    loop {
        if shared.is_closed() {
            break;
        }
        let failure = match shared.open_get(cursor.last_event_id.as_deref()).await {
            Ok(Some(response)) => {
                shared.set_listener_state(ListenerState::Open);
                let opened_at = Instant::now();
                let mut decoder = SseDecoder::new(MAX_MESSAGE_BYTES);
                let ended = shared.read_listener_stream(response, &mut decoder).await;
                cursor.absorb(&decoder);
                // A stream that stayed up a while is healthy even if idle.
                if std::mem::take(&mut cursor.received) || opened_at.elapsed() > policy.max {
                    attempt = 0;
                }
                ended.err().unwrap_or(McpError::Closed)
            }
            Ok(None) => {
                shared.set_listener_state(ListenerState::Unsupported);
                return;
            }
            Err(error) => error,
        };
        let retryable = matches!(failure, McpError::Closed | McpError::Timeout(_))
            || is_transient_error(&failure);
        if matches!(failure, McpError::SessionExpired) {
            // The server forgot the session: the next use reconnects.
            shared.expire_session(&session);
        }
        if !retryable || attempt >= policy.retries || shared.is_closed() {
            break;
        }
        shared.set_listener_state(ListenerState::Reconnecting);
        tokio::time::sleep(policy.delay(attempt, cursor.retry)).await;
        attempt += 1;
    }
    shared.set_listener_state(ListenerState::Stopped);
}

pub(crate) struct HttpConnection {
    shared: Arc<HttpShared>,
    next_id: AtomicU64,
    /// Parameters of the `initialize` request, replayed when the server
    /// forgets the session and a new one has to be opened.
    init_params: Mutex<Option<Value>>,
    /// Counts session renewals so concurrent requests that all hit the
    /// expired session renew it once.
    epoch: AtomicU64,
    renewal: tokio::sync::Mutex<()>,
    listener: Mutex<Option<JoinHandle<()>>>,
    runtime: Mutex<Option<tokio::runtime::Handle>>,
}

impl HttpConnection {
    pub(crate) fn new(
        url: String,
        headers: BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let parsed = reqwest::Url::parse(&url).map_err(|error| {
            McpError::Protocol(format!("invalid MCP url {}: {error}", sanitized_url(&url)))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(McpError::Protocol(format!(
                "MCP url must be http(s): {}",
                sanitized_url(&url)
            )));
        }
        let client = reqwest::Client::builder()
            // RPC endpoints never legitimately redirect; following one would
            // replay configured secret headers to another host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| McpError::Protocol(format!("http client: {error}")))?;
        Ok(Self {
            shared: Arc::new(HttpShared {
                client,
                url,
                headers: headers.into_iter().collect(),
                session: Mutex::new(None),
                session_ended: AtomicBool::new(false),
                protocol_version: Mutex::new(MCP_PROTOCOL_VERSION.to_owned()),
                closed: AtomicBool::new(false),
                expired: AtomicBool::new(false),
                context: ClientContext::detached(),
                timeout,
                pending: Mutex::new(HashMap::new()),
                reconnect: ReconnectPolicy::default(),
                listener_state: AtomicU8::new(ListenerState::NotStarted as u8),
                auth: None,
            }),
            next_id: AtomicU64::new(1),
            init_params: Mutex::new(None),
            epoch: AtomicU64::new(0),
            renewal: tokio::sync::Mutex::new(()),
            listener: Mutex::new(None),
            runtime: Mutex::new(tokio::runtime::Handle::try_current().ok()),
        })
    }

    /// Serves the server's own requests (`roots/list`, `ping`) and records
    /// its notifications through `context`. Called before the connection is
    /// used, while it is still the only owner of its shared state.
    pub(crate) fn with_context(mut self, context: Arc<ClientContext>) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.context = context;
        }
        self
    }

    /// Authenticates with OAuth. Called before the connection is used.
    pub(crate) fn with_auth(mut self, auth: Arc<McpAuth>) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.auth = Some(auth);
        }
        self
    }

    #[cfg(test)]
    fn with_reconnect(mut self, policy: ReconnectPolicy) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.reconnect = policy;
        }
        self
    }

    #[cfg(test)]
    fn listener_state(&self) -> ListenerState {
        ListenerState::from_u8(self.shared.listener_state.load(Ordering::Acquire))
    }

    /// Starts the standalone `GET` stream. Called once the server has seen
    /// `notifications/initialized` (the stream may only open after that).
    fn start_listener(&self) {
        if self.shared.is_closed() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        *lock(&self.runtime) = Some(handle.clone());
        let task = handle.spawn(run_listener(Arc::clone(&self.shared)));
        if let Some(previous) = lock(&self.listener).replace(task) {
            previous.abort();
        }
    }

    fn stop_listener(&self) {
        if let Some(task) = lock(&self.listener).take() {
            task.abort();
        }
    }

    /// Fire-and-forget `DELETE` for a session nobody awaits closing.
    fn spawn_delete(&self) {
        if !self.shared.has_session() || self.shared.session_ended.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = lock(&self.runtime)
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        if let Some(handle) = handle {
            let shared = Arc::clone(&self.shared);
            drop(handle.spawn(async move { shared.send_delete().await }));
        }
    }

    /// The server no longer knows the session: opens a new one with the
    /// original `initialize` parameters. Concurrent callers renew once.
    async fn renew_session(&self, seen_epoch: u64) -> Result<(), McpError> {
        let _guard = self.renewal.lock().await;
        if self.epoch.load(Ordering::Acquire) != seen_epoch {
            return Ok(());
        }
        let renewed = self.open_new_session().await;
        match &renewed {
            Ok(()) => {
                self.epoch.fetch_add(1, Ordering::AcqRel);
                self.shared.expired.store(false, Ordering::Release);
                // A new session may serve different catalogs.
                self.shared.context.mark_tools_stale();
                self.shared.context.mark_resources_stale();
            }
            Err(_) => self.shared.closed.store(true, Ordering::Release),
        }
        renewed
    }

    async fn open_new_session(&self) -> Result<(), McpError> {
        let Some(params) = lock(&self.init_params).clone() else {
            return Err(McpError::SessionExpired);
        };
        self.stop_listener();
        *lock(&self.shared.session) = None;
        self.shared.session_ended.store(true, Ordering::Release);
        MCP_PROTOCOL_VERSION.clone_into(&mut lock(&self.shared.protocol_version));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params});
        let mut deadline = Instant::now() + self.shared.timeout;
        let result = self
            .shared
            .send_request(&body, id, &mut deadline, None)
            .await?;
        let handshake = McpServerHandshake::from_initialize(&result)?;
        handshake
            .protocol_version
            .clone_into(&mut lock(&self.shared.protocol_version));
        self.shared
            .post_notification(&json!({
                "jsonrpc": "2.0", "method": "notifications/initialized", "params": {},
            }))
            .await?;
        self.start_listener();
        Ok(())
    }

    /// One request with its timeout (renewed by progress when tracked). The
    /// cancellation notice is not sent here. An idempotent request that finds
    /// its session gone is retried once on a new session; anything else just
    /// reports the expiry and marks the connection closed so the next use
    /// reconnects.
    async fn request_core(
        &self,
        id: u64,
        method: &str,
        mut params: Value,
        progress: Option<&McpProgressSink>,
    ) -> Result<Value, McpError> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed);
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        if self.shared.expired.load(Ordering::Acquire) {
            // Known gone before even sending: a read request opens the new
            // session first; anything else leaves that to the owner.
            if !is_idempotent(method) {
                return Err(McpError::Closed);
            }
            self.renew_session(epoch).await?;
        }
        if method == "initialize" {
            *lock(&self.init_params) = Some(params.clone());
        }
        let route = progress.map(|sink| {
            Arc::new(ProgressRoute {
                sink: Arc::clone(sink),
                renewed: Notify::new(),
            })
        });
        if route.is_some() {
            // The request id doubles as the progress token.
            if params.is_null() {
                params = json!({});
            }
            if let Some(params) = params.as_object_mut() {
                let meta = params.entry("_meta").or_insert_with(|| json!({}));
                if let Some(meta) = meta.as_object_mut() {
                    meta.insert("progressToken".to_owned(), json!(id));
                }
            }
        }
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let _registration = route.as_ref().map(|route| {
            lock(&self.shared.pending).insert(id, Arc::clone(route));
            PendingGuard {
                shared: &self.shared,
                id,
            }
        });
        let mut deadline = Instant::now() + self.shared.timeout;
        let first = self
            .shared
            .send_request(&body, id, &mut deadline, route.as_deref())
            .await;
        if !matches!(first, Err(McpError::SessionExpired)) {
            return first;
        }
        if !is_idempotent(method) {
            self.shared.expired.store(true, Ordering::Release);
            return first;
        }
        self.renew_session(epoch).await?;
        let mut deadline = Instant::now() + self.shared.timeout;
        let retried = self
            .shared
            .send_request(&body, id, &mut deadline, route.as_deref())
            .await;
        if matches!(retried, Err(McpError::SessionExpired)) {
            self.shared.closed.store(true, Ordering::Release);
        }
        retried
    }

    /// Best-effort `notifications/cancelled` for a request the server may be
    /// executing. Never for `initialize` (the spec forbids it); bounded so a
    /// stalled server cannot hold up the cancellation.
    async fn send_cancelled(&self, id: u64, method: &str, reason: &str) {
        if method == "initialize" || self.shared.is_closed() {
            return;
        }
        let body = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": reason},
        });
        let _ = tokio::time::timeout(CANCEL_NOTIFY_WAIT, self.shared.post(&body).send()).await;
    }
}

impl Drop for HttpConnection {
    fn drop(&mut self) {
        self.stop_listener();
        self.shared.closed.store(true, Ordering::Release);
        self.spawn_delete();
    }
}

#[async_trait::async_trait]
impl McpConnection for HttpConnection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let result = self.request_core(id, method, params, None).await;
        if matches!(result, Err(McpError::Timeout(_))) {
            self.send_cancelled(id, method, "Request timed out").await;
        }
        result
    }

    async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        self.request_with_progress(method, params, cancellation, None)
            .await
    }

    /// Cancellation and failure classification: before the request is sent
    /// the interruption is safe; afterwards a `tools/call` is never reported
    /// as a clean failure because the server may have executed it.
    async fn request_with_progress(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
        progress: Option<McpProgressSink>,
    ) -> McpRequestOutcome<Value> {
        if cancellation.is_cancelled() {
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        tokio::select! {
            biased;
            result = self.request_core(id, method, params, progress.as_ref()) => match result {
                Err(McpError::Timeout(timeout)) => {
                    self.send_cancelled(id, method, "Request timed out").await;
                    if method == "tools/call" {
                        McpRequestOutcome::OutcomeUncertain {
                            interruption: McpInterruption::TimedOut(timeout),
                            cleanup: McpCleanupStatus::Unconfirmed,
                        }
                    } else {
                        McpRequestOutcome::Completed(Err(McpError::Timeout(timeout)))
                    }
                }
                Err(
                    McpError::Io(_)
                    | McpError::Protocol(_)
                    | McpError::Closed
                    | McpError::SessionExpired,
                ) if method == "tools/call" => McpRequestOutcome::OutcomeUncertain {
                    interruption: McpInterruption::ConnectionClosed,
                    cleanup: McpCleanupStatus::Unconfirmed,
                },
                result => McpRequestOutcome::Completed(result),
            },
            _ = cancellation.cancelled() => {
                self.send_cancelled(id, method, "Request cancelled by client").await;
                McpRequestOutcome::OutcomeUncertain {
                    interruption: McpInterruption::Cancelled,
                    cleanup: McpCleanupStatus::Unconfirmed,
                }
            }
        }
    }

    fn set_protocol_version(&self, version: &str) {
        version.clone_into(&mut lock(&self.shared.protocol_version));
    }

    async fn notify(&self, method: &str, params: Value) {
        if self.shared.is_closed() {
            return;
        }
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match self.shared.post_notification(&body).await {
            Ok(()) if method == "notifications/initialized" => self.start_listener(),
            Err(McpError::SessionExpired) => self.shared.expired.store(true, Ordering::Release),
            _ => {}
        }
    }

    async fn close_for_cleanup(&self) -> McpCleanupStatus {
        self.shared.closed.store(true, Ordering::Release);
        self.stop_listener();
        // The DELETE must not delay a cancellation, so it runs detached.
        self.spawn_delete();
        // HTTP requests have no child process to reap, but this transport does
        // not own cancellation handles for concurrent reqwest futures.
        McpCleanupStatus::Unconfirmed
    }

    async fn end_session(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.stop_listener();
        if self.shared.has_session() && !self.shared.session_ended.swap(true, Ordering::AcqRel) {
            self.shared.send_delete().await;
        }
    }

    async fn notify_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<()> {
        if self.shared.is_closed() {
            return McpRequestOutcome::Completed(Err(McpError::Closed));
        }
        if cancellation.is_cancelled() {
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let session_sent = self.shared.has_session();
        tokio::select! {
            biased;
            result = tokio::time::timeout(self.shared.timeout, self.shared.post(&body).send()) => match result {
                Ok(Ok(response)) => {
                    let status = self.shared.check_status(&response, session_sent);
                    match &status {
                        Ok(()) if method == "notifications/initialized" => self.start_listener(),
                        Err(McpError::SessionExpired) => {
                            self.shared.expired.store(true, Ordering::Release);
                        }
                        _ => {}
                    }
                    McpRequestOutcome::Completed(status)
                }
                // A notification has no effect to be unsure about: a network
                // failure is an ordinary (transient) error, so the connect
                // retry policy sees it like one on any handshake request.
                Ok(Err(error)) if !error.is_timeout() => McpRequestOutcome::Completed(Err(
                    self.shared.transport_error("notification", error),
                )),
                Ok(Err(_)) => McpRequestOutcome::OutcomeUncertain {
                    interruption: McpInterruption::TimedOut(self.shared.timeout),
                    cleanup: McpCleanupStatus::Unconfirmed,
                },
                Err(_) => McpRequestOutcome::OutcomeUncertain {
                    interruption: McpInterruption::TimedOut(self.shared.timeout),
                    cleanup: McpCleanupStatus::Unconfirmed,
                },
            },
            _ = cancellation.cancelled() => McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::Unconfirmed,
            },
        }
    }

    fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    fn take_tools_stale(&self) -> bool {
        self.shared.context.take_tools_stale()
    }

    fn mark_tools_stale(&self) {
        self.shared.context.mark_tools_stale();
    }

    fn take_resources_stale(&self) -> bool {
        self.shared.context.take_resources_stale()
    }
}

enum Inbound {
    Response(Result<Value, McpError>),
    ServerRequest {
        id: Value,
        method: String,
    },
    /// Progress for the request whose id is the token.
    Progress {
        token: u64,
        update: McpProgress,
    },
    Notification {
        method: String,
        params: Value,
    },
    Other,
}

/// `expected_id`: the request whose response is awaited. The standalone
/// stream awaits none, so responses there are ignored.
fn classify_inbound(message: &Value, expected_id: Option<u64>) -> Inbound {
    let method = message.get("method").and_then(Value::as_str);
    if let Some(id) = message.get("id").cloned() {
        if message.get("method").is_some() {
            return Inbound::ServerRequest {
                id,
                method: method.unwrap_or_default().to_owned(),
            };
        }
        if expected_id.is_some() && id.as_u64() == expected_id {
            return Inbound::Response(extract_result(message.clone()));
        }
        return Inbound::Other;
    }
    let Some(method) = method else {
        return Inbound::Other;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if method == "notifications/progress" {
        return match parse_progress(&params) {
            Some((token, update)) => Inbound::Progress { token, update },
            None => Inbound::Other,
        };
    }
    Inbound::Notification {
        method: method.to_owned(),
        params,
    }
}

/// The JSON-RPC message of one SSE event: `None` for events that carry none
/// (other event types, empty data).
fn parse_message(event: &SseEvent) -> Result<Option<Value>, McpError> {
    if !event.is_message() {
        return Ok(None);
    }
    let body = event.data.trim();
    if body.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(body)
        .map(Some)
        .map_err(|error| McpError::Protocol(format!("invalid SSE message: {error}")))
}

fn extract_result(message: Value) -> Result<Value, McpError> {
    if let Some(error) = message.get("error").filter(|error| !error.is_null()) {
        return Err(McpError::Server {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: crate::mcp::spec::bounded_server_text(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown server error"),
            ),
        });
    }
    if let Some(result) = message.get("result") {
        return Ok(result.clone());
    }
    Err(McpError::Protocol(
        "response has neither result nor error".into(),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::json;

    use super::{
        is_idempotent, is_transient_error, HttpConnection, ListenerState, ReconnectPolicy,
        MAX_MESSAGE_BYTES,
    };
    use crate::mcp::spec::{McpCancellation, McpConnection, McpError, McpRequestOutcome};

    /// One-shot HTTP fixture: accepts a single connection, reads the request
    /// headers plus the declared Content-Length body, then replies with
    /// `response_headers` and a close-delimited body made of `pattern`
    /// repeated, written in 64 KiB chunks.
    fn serve_once(response_headers: &'static str, pattern: Vec<u8>, body_bytes: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut received: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            let content_length = loop {
                let read = socket.read(&mut chunk).expect("read request");
                assert!(read > 0, "client closed before sending headers");
                received.extend_from_slice(&chunk[..read]);
                if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&received[..end]).into_owned();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.trim().eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    received.drain(..end + 4);
                    break length;
                }
            };
            while received.len() < content_length {
                let read = socket.read(&mut chunk).expect("read body");
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(response_headers.as_bytes())
                .expect("write response headers");
            let mut block = Vec::new();
            while block.len() < 64 * 1024 {
                block.extend_from_slice(&pattern);
            }
            let mut remaining = body_bytes;
            while remaining > 0 {
                let n = remaining.min(block.len());
                if socket.write_all(&block[..n]).is_err() {
                    // Client disconnected once its bound tripped — expected.
                    break;
                }
                remaining -= n;
            }
        });
        url
    }

    /// Over-16-MiB close-delimited JSON body: no Content-Length, so the bound
    /// has to hold on the streamed read, not the header pre-check.
    #[test]
    fn json_body_without_content_length_is_bounded() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            b"x".to_vec(),
            MAX_MESSAGE_BYTES + 1024 * 1024,
        );
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(30)).expect("connection");
        let runtime = tokio::runtime::Runtime::new().expect("tokio");
        let error = runtime
            .block_on(connection.request("tools/list", json!({})))
            .expect_err("oversized body must error");
        match error {
            McpError::Protocol(message) => {
                assert!(message.contains("16 MiB"), "{message}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    /// Over-16-MiB of unterminated `data:` lines: the accumulated payload hits
    /// the per-message bound even though no blank-line event boundary ever comes.
    #[test]
    fn sse_data_lines_without_boundary_are_bounded() {
        let mut pattern = b"data: ".to_vec();
        pattern.extend(std::iter::repeat_n(b'x', 8000));
        pattern.push(b'\n');
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
            pattern,
            MAX_MESSAGE_BYTES * 2,
        );
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(30)).expect("connection");
        let runtime = tokio::runtime::Runtime::new().expect("tokio");
        let error = runtime
            .block_on(connection.request("tools/list", json!({})))
            .expect_err("unterminated SSE flood must error");
        match error {
            McpError::Protocol(message) => {
                assert!(message.contains("16 MiB"), "{message}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Policy functions
    // -----------------------------------------------------------------------

    #[test]
    fn reconnect_delays_double_up_to_the_cap_and_a_server_retry_replaces_them() {
        let policy = ReconnectPolicy::default();
        let delays: Vec<u64> = (0..7).map(|n| policy.delay(n, None).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(policy.delay(40, None), Duration::from_secs(30));
        assert_eq!(
            policy.delay(3, Some(Duration::from_millis(100))),
            Duration::from_millis(100)
        );
        assert_eq!(
            policy.delay(0, Some(Duration::from_secs(86_400))),
            Duration::from_secs(30),
            "a hostile retry: value is bounded"
        );
        assert_eq!(
            policy.delay(0, Some(Duration::ZERO)),
            Duration::from_millis(100),
            "retry: 0 must not become a hot loop"
        );
        assert_eq!(policy.retries, 5);
    }

    #[test]
    fn only_network_failures_and_retryable_statuses_are_transient() {
        let status = |code: i64| McpError::Server {
            code,
            message: String::new(),
        };
        for code in [408, 429, 500, 502, 503, 504, 599] {
            assert!(is_transient_error(&status(code)), "{code}");
        }
        for code in [400, 401, 403, 404, 405, 501, -32601, 0] {
            assert!(!is_transient_error(&status(code)), "{code}");
        }
        assert!(is_transient_error(&McpError::Io(std::io::Error::other(
            "reset"
        ))));
        assert!(!is_transient_error(&McpError::Protocol("bad".into())));
        assert!(!is_transient_error(&McpError::Timeout(
            Duration::from_secs(1)
        )));
        assert!(!is_transient_error(&McpError::SessionExpired));
    }

    #[test]
    fn only_read_requests_are_idempotent() {
        for method in [
            "tools/list",
            "resources/list",
            "resources/read",
            "resources/templates/list",
        ] {
            assert!(is_idempotent(method), "{method}");
        }
        for method in ["tools/call", "initialize", "ping", "prompts/get"] {
            assert!(!is_idempotent(method), "{method}");
        }
    }

    // -----------------------------------------------------------------------
    // Scripted server for session and stream behavior
    // -----------------------------------------------------------------------

    /// `(verb, JSON-RPC method, mcp-session-id)` of each request, in order.
    type Seen = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

    fn scripted_server(
        handler: impl Fn(&str, &str, Option<&str>, u64, &mut TcpStream) + Send + Sync + 'static,
    ) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let log = Arc::clone(&log);
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let verb = line.split_whitespace().next().unwrap_or("").to_owned();
                    let mut length = 0;
                    let mut session = None;
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            match name.trim().to_ascii_lowercase().as_str() {
                                "content-length" => length = value.trim().parse().unwrap_or(0),
                                "mcp-session-id" => session = Some(value.trim().to_owned()),
                                _ => {}
                            }
                        }
                    }
                    let mut body = vec![0; length];
                    if reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    let parsed = serde_json::from_slice::<serde_json::Value>(&body)
                        .unwrap_or(serde_json::Value::Null);
                    let method = parsed["method"].as_str().unwrap_or_default().to_owned();
                    let id = parsed["id"].as_u64().unwrap_or(0);
                    log.lock()
                        .unwrap()
                        .push((verb.clone(), method.clone(), session.clone()));
                    handler(&verb, &method, session.as_deref(), id, &mut stream);
                });
            }
        });
        (url, seen)
    }

    fn reply_json(
        stream: &mut TcpStream,
        session: Option<&str>,
        id: u64,
        result: serde_json::Value,
    ) {
        let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        let session = session
            .map(|id| format!("mcp-session-id: {id}\r\n"))
            .unwrap_or_default();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn reply_status(stream: &mut TcpStream, code: u16) {
        let _ = write!(
            stream,
            "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }

    fn count(seen: &Seen, verb: &str, method: &str) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(v, m, _)| v == verb && m == method)
            .count()
    }

    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio")
    }

    /// Server that opens session `s<n>` on the n-th `initialize` and answers
    /// every other POST with `{"ok": true}`. Once `expired` is set, requests
    /// on session `s1` (on every session when `always`) get 404.
    fn session_server(expired: Arc<AtomicBool>, always: bool) -> (String, Seen) {
        let initializes = AtomicUsize::new(0);
        scripted_server(move |verb, method, session, id, stream| {
            let gone = expired.load(Ordering::SeqCst) && (always || session == Some("s1"));
            let handshake = matches!(method, "initialize" | "notifications/initialized");
            if gone && !handshake {
                return reply_status(stream, 404);
            }
            match (verb, method) {
                ("POST", "initialize") => {
                    let n = initializes.fetch_add(1, Ordering::SeqCst) + 1;
                    reply_json(
                        stream,
                        Some(&format!("s{n}")),
                        id,
                        json!({"protocolVersion": "2025-11-25", "capabilities": {}}),
                    );
                }
                ("POST", "notifications/initialized") => reply_status(stream, 202),
                ("POST", _) => reply_json(stream, None, id, json!({"ok": true})),
                _ => reply_status(stream, 405),
            }
        })
    }

    async fn handshake(connection: &HttpConnection) {
        connection
            .request("initialize", json!({"capabilities": {"roots": {}}}))
            .await
            .expect("initialize");
        connection
            .notify("notifications/initialized", json!({}))
            .await;
    }

    #[test]
    fn an_idempotent_request_renews_the_session_and_is_retried_once() {
        let expired = Arc::new(AtomicBool::new(false));
        let (url, seen) = session_server(Arc::clone(&expired), false);
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5)).expect("connection");
        let runtime = multi_thread_runtime();
        runtime.block_on(handshake(&connection));
        expired.store(true, Ordering::SeqCst);

        let result = runtime
            .block_on(connection.request("resources/list", json!({})))
            .expect("retried on a new session");
        assert_eq!(result["ok"], true);
        assert_eq!(count(&seen, "POST", "initialize"), 2);
        assert_eq!(
            count(&seen, "POST", "resources/list"),
            2,
            "one refusal, one retry"
        );
        let sessions: Vec<_> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, method, _)| method == "resources/list")
            .map(|(_, _, session)| session.clone())
            .collect();
        assert_eq!(sessions, [Some("s1".to_owned()), Some("s2".to_owned())]);
        assert!(!connection.is_closed());
        // A new session may serve different catalogs.
        assert!(connection.take_tools_stale());
        assert!(connection.take_resources_stale());
    }

    #[test]
    fn a_second_expiry_gives_up_instead_of_looping() {
        let expired = Arc::new(AtomicBool::new(false));
        let (url, seen) = session_server(Arc::clone(&expired), true);
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5)).expect("connection");
        let runtime = multi_thread_runtime();
        runtime.block_on(handshake(&connection));
        expired.store(true, Ordering::SeqCst);

        let error = runtime
            .block_on(connection.request("tools/list", json!({})))
            .expect_err("the new session expires too");
        assert!(matches!(error, McpError::SessionExpired), "{error:?}");
        assert_eq!(count(&seen, "POST", "tools/list"), 2, "a single retry");
        assert!(connection.is_closed());
    }

    #[test]
    fn tools_call_on_an_expired_session_is_never_replayed() {
        let expired = Arc::new(AtomicBool::new(false));
        let (url, seen) = session_server(Arc::clone(&expired), false);
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5)).expect("connection");
        let runtime = multi_thread_runtime();
        runtime.block_on(handshake(&connection));
        expired.store(true, Ordering::SeqCst);

        let outcome = runtime.block_on(connection.request_cancellable(
            "tools/call",
            json!({"name": "x", "arguments": {}}),
            McpCancellation::new(),
        ));
        assert!(
            matches!(outcome, McpRequestOutcome::OutcomeUncertain { .. }),
            "{outcome:?}"
        );
        assert_eq!(count(&seen, "POST", "tools/call"), 1);
        assert_eq!(
            count(&seen, "POST", "initialize"),
            1,
            "no silent new session"
        );
        assert!(connection.is_closed(), "the owner reconnects on next use");
    }

    #[test]
    fn a_stale_expiry_report_cannot_condemn_a_renewed_session() {
        let expired = Arc::new(AtomicBool::new(false));
        let (url, seen) = session_server(Arc::clone(&expired), false);
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5)).expect("connection");
        let runtime = multi_thread_runtime();
        runtime.block_on(handshake(&connection));
        let current = connection.shared.session();
        assert_eq!(current.as_deref(), Some("s1"));

        // A stream that belonged to some other session is ignored.
        connection.shared.expire_session(&Some("older".to_owned()));
        assert!(!connection.is_closed());

        // The current session reported gone: the connection says so (its
        // owner would reconnect) ...
        connection.shared.expire_session(&current);
        assert!(connection.is_closed());
        // ... a call is refused without being sent ...
        let outcome = runtime.block_on(connection.request_cancellable(
            "tools/call",
            json!({"name": "x", "arguments": {}}),
            McpCancellation::new(),
        ));
        assert!(matches!(
            outcome,
            McpRequestOutcome::OutcomeUncertain { .. }
        ));
        assert_eq!(count(&seen, "POST", "tools/call"), 0);
        // ... and a read request recovers on a new session by itself.
        runtime
            .block_on(connection.request("tools/list", json!({})))
            .expect("renewed");
        assert!(!connection.is_closed());
        assert_eq!(count(&seen, "POST", "initialize"), 2);
        assert_eq!(connection.shared.session().as_deref(), Some("s2"));
    }

    #[test]
    fn a_404_without_a_session_is_an_ordinary_error() {
        let (url, _) = scripted_server(|_, _, _, _, stream| reply_status(stream, 404));
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5)).expect("connection");
        let runtime = multi_thread_runtime();
        let error = runtime
            .block_on(connection.request("initialize", json!({})))
            .expect_err("404");
        assert!(
            matches!(error, McpError::Server { code: 404, .. }),
            "{error:?}"
        );
        assert!(!connection.is_closed());
    }

    fn listener_server(get_status: u16) -> (String, Seen) {
        scripted_server(move |verb, method, _, id, stream| match (verb, method) {
            ("POST", "initialize") => reply_json(stream, Some("s1"), id, json!({})),
            ("POST", _) => reply_status(stream, 202),
            ("GET", _) => reply_status(stream, get_status),
            _ => reply_status(stream, 405),
        })
    }

    fn wait_for_listener(connection: &HttpConnection, wanted: ListenerState) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while connection.listener_state() != wanted {
            assert!(
                std::time::Instant::now() < end,
                "listener stuck in {:?}, wanted {wanted:?}",
                connection.listener_state()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_listener_gives_up_after_the_configured_retries() {
        let (url, seen) = listener_server(503);
        let policy = ReconnectPolicy {
            initial: Duration::from_millis(5),
            max: Duration::from_millis(20),
            retries: 3,
        };
        let connection = HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5))
            .expect("connection")
            .with_reconnect(policy);
        let runtime = multi_thread_runtime();
        runtime.block_on(handshake(&connection));
        wait_for_listener(&connection, ListenerState::Stopped);
        assert_eq!(
            count(&seen, "GET", ""),
            4,
            "the first try plus three retries"
        );
        assert!(!connection.is_closed(), "POST traffic is unaffected");
    }

    #[test]
    fn a_405_marks_the_stream_unsupported_after_one_request() {
        let (url, seen) = listener_server(405);
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5)).expect("connection");
        let runtime = multi_thread_runtime();
        runtime.block_on(handshake(&connection));
        wait_for_listener(&connection, ListenerState::Unsupported);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count(&seen, "GET", ""), 1);
    }

    #[test]
    fn a_stream_that_delivers_events_resets_the_failure_count() {
        // Each GET delivers one event and closes: six such streams in a row
        // would exhaust a plain counter of three retries.
        let gets = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&gets);
        let (url, seen) = scripted_server(move |verb, method, _, id, stream| {
            match (verb, method) {
                ("POST", "initialize") => reply_json(stream, Some("s1"), id, json!({})),
                ("POST", _) => reply_status(stream, 202),
                ("GET", _) => {
                    let n = served.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                    let _ = write!(
                        stream,
                        "id: {n}\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}}\n\n"
                    );
                }
                _ => reply_status(stream, 405),
            }
        });
        let policy = ReconnectPolicy {
            initial: Duration::from_millis(5),
            max: Duration::from_millis(20),
            retries: 3,
        };
        let connection = HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(5))
            .expect("connection")
            .with_reconnect(policy);
        let runtime = multi_thread_runtime();
        runtime.block_on(handshake(&connection));
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while count(&seen, "GET", "") < 6 {
            assert!(
                std::time::Instant::now() < end,
                "stream stopped reconnecting"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(connection.listener_state(), ListenerState::Stopped);
    }
}
