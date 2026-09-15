//! JSON-RPC 2.0 transport over a framed byte stream (LSP style
//! Content-Length headers). Owns the read loop, the pending-request map,
//! request timeouts, /cancelRequest propagation, message-size limits and the
//! bounded notification stream consumed by the instance layer.
//!
//! The transport is generic over any tokio AsyncRead + AsyncWrite pair so the
//! exact same code paths run against a spawned language server (stdio) and
//! against an in-process mock server (duplex) in tests.
//!
//! Reader and writer halves are split at construction so the read loop never
//! blocks writes and vice versa — this eliminates a class of protocol deadlock
//! where the server is waiting for a request while the client holds the IO
//! mutex waiting for a response frame.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{json, Value};
use slim_core::runtime::CancellationToken;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tokio::sync::{oneshot, Mutex, Notify};

/// Anything that can carry an LSP byte stream. Boxed so the process-backed
/// and test-backed variants share one transport type.
pub trait LspIo: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> LspIo for T {}

pub type IoBox = Box<dyn LspIo>;

/// A notification pushed by the server (no response expected).
#[derive(Debug, Clone)]
pub struct ServerNotification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug)]
enum NotificationClass {
    Diagnostics(String),
    Progress { token: String, intermediate: bool },
    Other,
}

#[derive(Debug)]
struct QueuedNotification {
    class: NotificationClass,
    approx_bytes: usize,
    notification: ServerNotification,
}

/// Cheap size estimate for a queued notification — walks the JSON tree
/// counting string bytes plus a small per-node overhead, without
/// materializing a serialized copy of the payload.
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len().saturating_add(16),
        Value::Array(items) => items
            .iter()
            .map(value_bytes)
            .fold(16usize, usize::saturating_add),
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| key.len().saturating_add(value_bytes(item)))
            .fold(16usize, usize::saturating_add),
        _ => 24,
    }
}

impl QueuedNotification {
    fn new(notification: ServerNotification) -> Self {
        let approx_bytes = notification
            .method
            .len()
            .saturating_add(value_bytes(&notification.params));
        let class = match notification.method.as_str() {
            "textDocument/publishDiagnostics" => notification
                .params
                .get("uri")
                .and_then(Value::as_str)
                .map(|uri| NotificationClass::Diagnostics(uri.to_owned()))
                .unwrap_or(NotificationClass::Other),
            "$/progress" => {
                let token = notification
                    .params
                    .get("token")
                    .and_then(|token| serde_json::to_string(token).ok())
                    .unwrap_or_else(|| "null".to_owned());
                let intermediate = notification
                    .params
                    .pointer("/value/kind")
                    .and_then(Value::as_str)
                    == Some("report");
                NotificationClass::Progress {
                    token,
                    intermediate,
                }
            }
            _ => NotificationClass::Other,
        };
        Self {
            class,
            approx_bytes,
            notification,
        }
    }
}

#[derive(Debug, Default)]
struct NotificationMailboxState {
    queue: VecDeque<QueuedNotification>,
    queued_bytes: usize,
    sender_closed: bool,
    receiver_closed: bool,
}

#[derive(Debug)]
struct NotificationMailboxInner {
    capacity: usize,
    max_bytes: usize,
    state: StdMutex<NotificationMailboxState>,
    notify: Notify,
    dropped: AtomicU64,
}

impl NotificationMailboxInner {
    fn lock(&self) -> std::sync::MutexGuard<'_, NotificationMailboxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record_drop(&self, count: usize) {
        self.dropped.fetch_add(count as u64, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug)]
struct NotificationSender {
    inner: Arc<NotificationMailboxInner>,
}

/// Single-consumer notification stream backed by a bounded, non-blocking
/// mailbox. The transport's read loop never awaits notification capacity.
#[derive(Debug)]
pub struct NotificationReceiver {
    inner: Arc<NotificationMailboxInner>,
}

impl NotificationSender {
    fn channel(capacity: usize, max_bytes: usize) -> (Self, NotificationReceiver) {
        let inner = Arc::new(NotificationMailboxInner {
            capacity: capacity.max(1),
            max_bytes: max_bytes.max(1),
            state: StdMutex::new(NotificationMailboxState::default()),
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
        });
        (
            Self {
                inner: inner.clone(),
            },
            NotificationReceiver { inner },
        )
    }

    /// Eviction preference when the mailbox is full: diagnostics displace
    /// intermediate progress first, then unclassified traffic, then terminal
    /// progress, and only then the oldest diagnostics. Terminal progress
    /// displaces the same lower-value classes but never diagnostics; interim
    /// progress and unclassified notifications displace nothing.
    fn eviction_index(
        state: &NotificationMailboxState,
        class: &NotificationClass,
    ) -> Option<usize> {
        if state.queue.is_empty() {
            return None;
        }
        let intermediate_progress = |queued: &QueuedNotification| {
            matches!(
                queued.class,
                NotificationClass::Progress {
                    intermediate: true,
                    ..
                }
            )
        };
        let other = |queued: &QueuedNotification| matches!(queued.class, NotificationClass::Other);
        let any_progress = |queued: &QueuedNotification| {
            matches!(queued.class, NotificationClass::Progress { .. })
        };
        match class {
            NotificationClass::Diagnostics(_) => state
                .queue
                .iter()
                .position(intermediate_progress)
                .or_else(|| state.queue.iter().position(other))
                .or_else(|| state.queue.iter().position(any_progress))
                .or(Some(0)),
            NotificationClass::Progress {
                intermediate: true, ..
            }
            | NotificationClass::Other => None,
            NotificationClass::Progress {
                intermediate: false,
                ..
            } => state
                .queue
                .iter()
                .position(intermediate_progress)
                .or_else(|| state.queue.iter().position(other))
                .or_else(|| state.queue.iter().position(any_progress)),
        }
    }

    fn send(&self, notification: ServerNotification) {
        let incoming = QueuedNotification::new(notification);
        let mut state = self.inner.lock();
        if state.receiver_closed {
            drop(state);
            self.inner.record_drop(1);
            return;
        }

        match &incoming.class {
            NotificationClass::Diagnostics(uri) => {
                if let Some(index) = state.queue.iter().position(|queued| {
                    matches!(&queued.class, NotificationClass::Diagnostics(existing) if existing == uri)
                }) {
                    state.queued_bytes = state
                        .queued_bytes
                        .saturating_sub(state.queue[index].approx_bytes)
                        .saturating_add(incoming.approx_bytes);
                    state.queue[index] = incoming;
                    drop(state);
                    self.inner.record_drop(1);
                    self.inner.notify.notify_one();
                    return;
                }
            }
            NotificationClass::Progress {
                token,
                intermediate,
            } => {
                if let Some(index) = state.queue.iter().position(|queued| {
                    matches!(&queued.class, NotificationClass::Progress { token: existing, .. } if existing == token)
                }) {
                    let preserve_terminal = *intermediate
                        && matches!(
                            state.queue[index].class,
                            NotificationClass::Progress {
                                intermediate: false,
                                ..
                            }
                        );
                    if !preserve_terminal {
                        state.queued_bytes = state
                            .queued_bytes
                            .saturating_sub(state.queue[index].approx_bytes)
                            .saturating_add(incoming.approx_bytes);
                        state.queue[index] = incoming;
                    }
                    drop(state);
                    self.inner.record_drop(1);
                    self.inner.notify.notify_one();
                    return;
                }
            }
            NotificationClass::Other => {}
        }

        // Bounded in count and in bytes: evict by priority until the incoming
        // notification fits, or drop it when nothing may be displaced.
        while state.queue.len() >= self.inner.capacity
            || state.queued_bytes.saturating_add(incoming.approx_bytes) > self.inner.max_bytes
        {
            let Some(index) = Self::eviction_index(&state, &incoming.class) else {
                drop(state);
                self.inner.record_drop(1);
                return;
            };
            let removed = state.queue.remove(index).expect("index from live queue");
            state.queued_bytes = state.queued_bytes.saturating_sub(removed.approx_bytes);
            self.inner.record_drop(1);
        }
        state.queued_bytes = state.queued_bytes.saturating_add(incoming.approx_bytes);
        state.queue.push_back(incoming);
        drop(state);
        self.inner.notify.notify_one();
    }

    fn close(&self) {
        let mut state = self.inner.lock();
        state.sender_closed = true;
        drop(state);
        self.inner.notify.notify_waiters();
    }

    fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

impl NotificationReceiver {
    pub async fn recv(&mut self) -> Option<ServerNotification> {
        loop {
            let mut notified = Box::pin(self.inner.notify.notified());
            notified.as_mut().enable();
            {
                let mut state = self.inner.lock();
                if let Some(queued) = state.queue.pop_front() {
                    state.queued_bytes = state.queued_bytes.saturating_sub(queued.approx_bytes);
                    return Some(queued.notification);
                }
                if state.sender_closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl Drop for NotificationReceiver {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if state.receiver_closed {
            return;
        }
        state.receiver_closed = true;
        let queued = state.queue.len();
        state.queue.clear();
        state.queued_bytes = 0;
        drop(state);
        self.inner.record_drop(queued);
        self.inner.notify.notify_waiters();
    }
}

/// Handler for requests initiated by the *server*. Runs inside the read loop
/// and must be fast and pure. `Err` answers with JSON-RPC MethodNotFound so
/// the server never believes an unimplemented capability was registered.
pub type ServerRequestHandler =
    Box<dyn Fn(&str, Option<&Value>) -> Result<Value, String> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct TransportOptions {
    /// Hard cap on a single Content-Length body (protects memory).
    pub max_message_bytes: usize,
    /// Per-request timeout; on expiry a /cancelRequest is sent.
    pub request_timeout: Duration,
    /// Once a frame has started, every read must complete inside this
    /// deadline — a peer that stalls mid-header or mid-body would otherwise
    /// suspend the read loop forever with the transport reporting healthy.
    /// The first byte of a frame is exempt: an idle connection is legal.
    pub read_progress_timeout: Duration,
    /// Concurrent outstanding requests allowed per server.
    pub max_pending_requests: usize,
    /// Notifications buffered for the instance layer before coalescing/drops.
    pub notification_capacity: usize,
    /// Byte budget across buffered notifications. Count alone let a few
    /// max-size publishDiagnostics frames pin the whole queue; oversized
    /// traffic is evicted by the same priority policy once this is exceeded.
    pub notification_max_bytes: usize,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            max_message_bytes: 16 * 1024 * 1024,
            request_timeout: Duration::from_secs(30),
            read_progress_timeout: Duration::from_secs(30),
            max_pending_requests: 32,
            notification_capacity: 64,
            notification_max_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum TransportError {
    Io(String),
    Protocol(String),
    /// The peer returned a JSON-RPC error object for one of our requests.
    /// Code/message/data are preserved instead of flattening the object.
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// A started frame stopped making progress (partial header or body,
    /// then silence). Distinct from Io so a stalled peer is not mistaken
    /// for a healthy idle connection.
    Stalled,
    MessageTooLarge {
        bytes: usize,
        limit: usize,
    },
    RequestTimeout {
        method: String,
    },
    RequestCancelled {
        method: String,
    },
    PendingCapacity {
        method: String,
    },
    ServerClosed,
    Parse(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(message) => write!(f, "io error: {message}"),
            TransportError::Protocol(message) => write!(f, "protocol error: {message}"),
            TransportError::Remote {
                code,
                message,
                data,
            } => {
                write!(f, "server error {code}: {message}")?;
                if let Some(data) = data {
                    write!(f, " ({data})")?;
                }
                Ok(())
            }
            TransportError::Stalled => write!(f, "frame read progress stalled"),
            TransportError::MessageTooLarge { bytes, limit } => {
                write!(f, "message too large: {bytes} bytes (limit {limit})")
            }
            TransportError::RequestTimeout { method } => {
                write!(f, "request timed out: {method}")
            }
            TransportError::RequestCancelled { method } => {
                write!(f, "request cancelled: {method}")
            }
            TransportError::PendingCapacity { method } => {
                write!(f, "too many pending requests, dropped: {method}")
            }
            TransportError::ServerClosed => write!(f, "server closed the connection"),
            TransportError::Parse(message) => write!(f, "invalid JSON-RPC message: {message}"),
        }
    }
}

impl std::error::Error for TransportError {}

#[derive(Default)]
struct PendingEntry {
    sender: Option<oneshot::Sender<Result<Value, TransportError>>>,
    method: String,
}

struct TransportInner {
    next_id: AtomicI64,
    options: TransportOptions,
    pending: Mutex<HashMap<Value, PendingEntry>>,
}

/// Future-drop cancellation (task abort/select) needs the same cleanup as a
/// cooperative token. Cleanup owns only the transport pieces it needs.
struct PendingRequestGuard<'a> {
    transport: &'a LspTransport,
    id: Value,
    armed: bool,
}

impl Drop for PendingRequestGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let inner = self.transport.inner.clone();
        let writer = self.transport.writer.clone();
        let closed = self.transport.closed.clone();
        let notifications = self.transport.notifications.clone();
        let reader = self.transport.read_task.abort_handle();
        let id = self.id.clone();
        tokio::spawn(async move {
            let removed = inner.pending.lock().await.remove(&id).is_some();
            if removed && !closed.load(Ordering::Acquire) {
                let _ = write_bounded(
                    &writer,
                    &closed,
                    inner.options.max_message_bytes,
                    tokio::time::Instant::now() + Duration::from_millis(100),
                    "$/cancelRequest",
                    &json!({"jsonrpc":"2.0", "method":"$/cancelRequest", "params":{"id":id}}),
                    None,
                )
                .await;
            }
            if closed.load(Ordering::Acquire) {
                reader.abort();
                notifications.close();
                LspTransport::fail_all_pending(&inner, TransportError::ServerClosed).await;
            }
        });
    }
}

struct IncompleteFrame<'a> {
    closed: &'a AtomicBool,
    complete: bool,
}

impl Drop for IncompleteFrame<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.closed.store(true, Ordering::Release);
        }
    }
}

/// Reads a full framed message from a buffered reader. Returns None on EOF.
/// Headers arrive line by line through the buffer (one syscall per chunk,
/// not per byte); the buffer persists across frames so body bytes already
/// read are never lost.
///
/// `progress_timeout` bounds every read once the first byte of a frame has
/// arrived: a peer that stops mid-header or mid-body would otherwise suspend
/// this task forever while the transport still reports healthy. The first
/// header read has no deadline — an idle connection between frames is legal.
async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
    progress_timeout: Duration,
) -> Result<Option<Value>, TransportError> {
    let mut header = Vec::with_capacity(128);
    loop {
        let mut line = Vec::new();
        let limit = (4097 - header.len()) as u64;
        let read = if header.is_empty() {
            (&mut *reader)
                .take(limit)
                .read_until(b'\n', &mut line)
                .await
                .map_err(|e| TransportError::Io(e.to_string()))?
        } else {
            match tokio::time::timeout(
                progress_timeout,
                (&mut *reader).take(limit).read_until(b'\n', &mut line),
            )
            .await
            {
                Ok(result) => result.map_err(|e| TransportError::Io(e.to_string()))?,
                Err(_) => return Err(TransportError::Stalled),
            }
        };
        if read == 0 {
            return if header.is_empty() {
                Ok(None)
            } else {
                Err(TransportError::Protocol("EOF inside frame header".into()))
            };
        }
        header.extend_from_slice(&line);
        if header.len() > 4096 {
            return Err(TransportError::Protocol("header exceeds 4096 bytes".into()));
        }
        if line == b"\r\n" {
            break;
        }
    }
    let header = String::from_utf8_lossy(&header);
    let mut content_length = None;
    for line in header.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(TransportError::Protocol("duplicate Content-Length".into()));
            }
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| TransportError::Protocol(format!("bad Content-Length: {value:?}")))?;
            content_length = Some(parsed);
        }
    }
    let length = content_length
        .ok_or_else(|| TransportError::Protocol("missing Content-Length header".into()))?;
    if length > max_bytes {
        return Err(TransportError::MessageTooLarge {
            bytes: length,
            limit: max_bytes,
        });
    }
    let mut body = vec![0u8; length];
    match tokio::time::timeout(progress_timeout, reader.read_exact(&mut body)).await {
        Ok(result) => {
            result.map_err(|e| TransportError::Io(e.to_string()))?;
        }
        Err(_) => return Err(TransportError::Stalled),
    }
    let message =
        serde_json::from_slice::<Value>(&body).map_err(|e| TransportError::Parse(e.to_string()))?;
    Ok(Some(message))
}

/// Streaming JSON-RPC 2.0 connection to one language server.
///
/// Reader and writer are split so the read loop never blocks writes.
pub struct LspTransport {
    writer: Arc<Mutex<tokio::io::WriteHalf<IoBox>>>,
    inner: Arc<TransportInner>,
    read_task: tokio::task::JoinHandle<()>,
    closed: Arc<AtomicBool>,
    notifications: NotificationSender,
}

impl LspTransport {
    pub fn new(
        io: IoBox,
        options: TransportOptions,
        server_request_handler: ServerRequestHandler,
    ) -> (Self, NotificationReceiver) {
        let (reader, writer) = tokio::io::split(io);
        let writer = Arc::new(Mutex::new(writer));
        let inner = Arc::new(TransportInner {
            next_id: AtomicI64::new(1),
            options,
            pending: Mutex::new(HashMap::new()),
        });
        let (notify_tx, notifications) = NotificationSender::channel(
            inner.options.notification_capacity,
            inner.options.notification_max_bytes,
        );
        let closed = Arc::new(AtomicBool::new(false));
        let read_task = tokio::spawn(Self::read_loop(
            reader,
            writer.clone(),
            inner.clone(),
            notify_tx.clone(),
            server_request_handler,
            closed.clone(),
        ));
        let transport = Self {
            writer,
            inner,
            read_task,
            closed,
            notifications: notify_tx,
        };
        (transport, notifications)
    }

    async fn read_loop(
        reader: tokio::io::ReadHalf<IoBox>,
        writer: Arc<Mutex<tokio::io::WriteHalf<IoBox>>>,
        inner: Arc<TransportInner>,
        notify_tx: NotificationSender,
        server_request_handler: ServerRequestHandler,
        closed: Arc<std::sync::atomic::AtomicBool>,
    ) {
        // Buffered once for the connection lifetime: chunked header reads
        // must not consume body bytes past the blank line.
        let mut reader = tokio::io::BufReader::with_capacity(1024, reader);
        loop {
            let frame = match read_frame(
                &mut reader,
                inner.options.max_message_bytes,
                inner.options.read_progress_timeout,
            )
            .await
            {
                Ok(Some(message)) => Some(message),
                Ok(None) => None,
                Err(error) => {
                    closed.store(true, Ordering::Release);
                    Self::fail_all_pending(&inner, error).await;
                    break;
                }
            };
            let Some(message) = frame else {
                closed.store(true, Ordering::Release);
                Self::fail_all_pending(&inner, TransportError::ServerClosed).await;
                break;
            };
            let is_response = message.get("id").is_some() && message.get("method").is_none();
            if is_response {
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let outcome = if let Some(error) = message.get("error") {
                    Err(remote_error(error))
                } else {
                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                };
                let pending = {
                    let mut map = inner.pending.lock().await;
                    map.remove(&id)
                };
                if let Some(entry) = pending {
                    if let Some(sender) = entry.sender {
                        let _ = sender.send(outcome);
                    }
                }
                continue;
            }
            if let Some(_id) = message.get("id") {
                // Server-initiated request. The handler runs inline (it must
                // be fast and pure), but the response write is detached so a
                // busy writer never stalls the read loop — response frames
                // carry their own id, so write order between them is moot.
                let method = message
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let params = message.get("params");
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let response = match server_request_handler(&method, params) {
                    Ok(result) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    }),
                    Err(message) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": message }
                    }),
                };
                let writer = writer.clone();
                let inner = inner.clone();
                let closed = closed.clone();
                tokio::spawn(async move {
                    if let Err(error) = write_bounded(
                        &writer,
                        &closed,
                        inner.options.max_message_bytes,
                        tokio::time::Instant::now() + inner.options.request_timeout,
                        &method,
                        &response,
                        None,
                    )
                    .await
                    {
                        closed.store(true, Ordering::Release);
                        Self::fail_all_pending(&inner, error).await;
                    }
                });
                continue;
            }
            // Plain notification.
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            notify_tx.send(ServerNotification { method, params });
        }
        notify_tx.close();
        closed.store(true, Ordering::Release);
    }

    async fn fail_all_pending(inner: &Arc<TransportInner>, error: TransportError) {
        let mut map = inner.pending.lock().await;
        for (_id, mut entry) in map.drain() {
            if let Some(sender) = entry.sender.take() {
                let _ = sender.send(Err(clone_error(&error)));
            }
        }
    }

    /// Fire a request and await its response with the configured timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        self.request_cancellable(method, params, None).await
    }

    /// Fire a request while observing a cooperative run cancellation token.
    /// Cancelling removes the pending entry immediately and emits the standard
    /// `$/cancelRequest` notification; a late response is safely ignored.
    pub async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, TransportError> {
        let deadline = tokio::time::Instant::now() + self.inner.options.request_timeout;
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::ServerClosed);
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(TransportError::RequestCancelled {
                method: method.to_owned(),
            });
        }
        let id = Value::from(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.inner.pending.lock().await;
            if self.closed.load(Ordering::Acquire) {
                return Err(TransportError::ServerClosed);
            }
            if map.len() >= self.inner.options.max_pending_requests {
                return Err(TransportError::PendingCapacity {
                    method: method.to_owned(),
                });
            }
            map.insert(
                id.clone(),
                PendingEntry {
                    sender: Some(tx),
                    method: method.to_owned(),
                },
            );
        }
        let mut pending_guard = PendingRequestGuard {
            transport: self,
            id: id.clone(),
            armed: true,
        };
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self
            .write_payload(method, &payload, deadline, cancellation)
            .await
        {
            let _ = self.drop_pending(&id).await;
            pending_guard.armed = false;
            return Err(error);
        }
        let result = await_bounded(method, deadline, cancellation, async {
            Self::resolve_response(rx.await)
        })
        .await;
        if matches!(
            result,
            Err(TransportError::RequestCancelled { .. } | TransportError::RequestTimeout { .. })
        ) {
            let _ = self.drop_pending(&id).await;
            let _ = self.send_cancel_notification(&id).await;
        }
        pending_guard.armed = false;
        result
    }

    fn resolve_response(
        response: Result<Result<Value, TransportError>, oneshot::error::RecvError>,
    ) -> Result<Value, TransportError> {
        match response {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(TransportError::ServerClosed),
        }
    }

    /// Fire-and-forget notification (didOpen, didChange, exit, ...).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), TransportError> {
        self.notify_with_timeout(method, params, self.inner.options.request_timeout)
            .await
    }

    pub(crate) async fn notify_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<(), TransportError> {
        let payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_payload(
            method,
            &payload,
            tokio::time::Instant::now() + timeout,
            None,
        )
        .await
    }

    async fn write_payload(
        &self,
        method: &str,
        payload: &Value,
        deadline: tokio::time::Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(), TransportError> {
        if self.is_closed() {
            return Err(TransportError::ServerClosed);
        }
        let result = write_bounded(
            &self.writer,
            &self.closed,
            self.inner.options.max_message_bytes,
            deadline,
            method,
            payload,
            cancellation,
        )
        .await;
        if self.is_closed() {
            self.read_task.abort();
            self.notifications.close();
            Self::fail_all_pending(&self.inner, TransportError::ServerClosed).await;
        }
        result
    }

    async fn send_cancel_notification(&self, id: &Value) -> Result<(), TransportError> {
        // Cleanup cannot consume another full request timeout.
        self.notify_with_timeout(
            "$/cancelRequest",
            json!({ "id": id }),
            Duration::from_millis(100),
        )
        .await
    }

    async fn drop_pending(&self, id: &Value) -> Option<String> {
        let mut map = self.inner.pending.lock().await;
        map.remove(id).map(|entry| entry.method)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn invalidate(&self) {
        self.closed.store(true, Ordering::Release);
        self.read_task.abort();
        self.notifications.close();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            Self::fail_all_pending(&inner, TransportError::ServerClosed).await;
        });
    }

    /// Number of notifications coalesced, evicted or dropped because the
    /// bounded mailbox or its receiver could not accept them.
    pub fn notification_drop_count(&self) -> u64 {
        self.notifications.dropped()
    }
}

async fn await_bounded<T>(
    method: &str,
    deadline: tokio::time::Instant,
    cancellation: Option<&CancellationToken>,
    operation: impl std::future::Future<Output = Result<T, TransportError>>,
) -> Result<T, TransportError> {
    let cancelled = async {
        match cancellation {
            Some(token) => token.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        _ = cancelled => Err(TransportError::RequestCancelled { method: method.to_owned() }),
        _ = tokio::time::sleep_until(deadline) => Err(TransportError::RequestTimeout { method: method.to_owned() }),
        result = operation => result,
    }
}

async fn write_bounded(
    writer: &Mutex<tokio::io::WriteHalf<IoBox>>,
    closed: &AtomicBool,
    max_bytes: usize,
    deadline: tokio::time::Instant,
    method: &str,
    payload: &Value,
    cancellation: Option<&CancellationToken>,
) -> Result<(), TransportError> {
    let mut guard = await_bounded(method, deadline, cancellation, async {
        Ok(writer.lock().await)
    })
    .await?;
    if closed.load(Ordering::Acquire) {
        return Err(TransportError::ServerClosed);
    }
    let mut frame = IncompleteFrame {
        closed,
        complete: false,
    };
    let result = await_bounded(
        method,
        deadline,
        cancellation,
        write_message(&mut *guard, max_bytes, payload),
    )
    .await;
    frame.complete =
        result.is_ok() || matches!(result, Err(TransportError::MessageTooLarge { .. }));
    if result.is_err() && !matches!(result, Err(TransportError::MessageTooLarge { .. })) {
        // Publish closure while still holding the writer: no other writer may
        // append a new frame after an interrupted header/body/flush.
        closed.store(true, Ordering::Release);
    }
    result
}

/// Converts a JSON-RPC error object from a response frame into a structured
/// error. A non-object `error` field is malformed per spec and stays a
/// Protocol error with the raw payload.
fn remote_error(error: &Value) -> TransportError {
    if error.is_object() {
        TransportError::Remote {
            code: error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            data: error.get("data").cloned(),
        }
    } else {
        TransportError::Protocol(error.to_string())
    }
}

fn clone_error(error: &TransportError) -> TransportError {
    match error {
        TransportError::Io(message) => TransportError::Io(message.clone()),
        TransportError::Protocol(message) => TransportError::Protocol(message.clone()),
        TransportError::Remote {
            code,
            message,
            data,
        } => TransportError::Remote {
            code: *code,
            message: message.clone(),
            data: data.clone(),
        },
        TransportError::Stalled => TransportError::Stalled,
        TransportError::MessageTooLarge { bytes, limit } => TransportError::MessageTooLarge {
            bytes: *bytes,
            limit: *limit,
        },
        TransportError::RequestTimeout { method } => TransportError::RequestTimeout {
            method: method.clone(),
        },
        TransportError::RequestCancelled { method } => TransportError::RequestCancelled {
            method: method.clone(),
        },
        TransportError::PendingCapacity { method } => TransportError::PendingCapacity {
            method: method.clone(),
        },
        TransportError::ServerClosed => TransportError::ServerClosed,
        TransportError::Parse(message) => TransportError::Parse(message.clone()),
    }
}

impl Drop for LspTransport {
    fn drop(&mut self) {
        self.notifications.close();
        self.read_task.abort();
    }
}

/// Writes one framed JSON-RPC message (header + body atomically).
pub(crate) async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    max_bytes: usize,
    message: &Value,
) -> Result<(), TransportError> {
    let body = serde_json::to_vec(message).map_err(|e| TransportError::Protocol(e.to_string()))?;
    if body.len() > max_bytes {
        return Err(TransportError::MessageTooLarge {
            bytes: body.len(),
            limit: max_bytes,
        });
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|e| TransportError::Io(e.to_string()))?;
    writer
        .write_all(&body)
        .await
        .map_err(|e| TransportError::Io(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| TransportError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    const TEST_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

    fn test_transport(
        notification_capacity: usize,
    ) -> (
        LspTransport,
        NotificationReceiver,
        tokio::io::BufReader<DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(TEST_MAX_MESSAGE_BYTES);
        let options = TransportOptions {
            max_message_bytes: TEST_MAX_MESSAGE_BYTES,
            request_timeout: Duration::from_secs(2),
            read_progress_timeout: Duration::from_secs(2),
            max_pending_requests: 8,
            notification_capacity,
            notification_max_bytes: TEST_MAX_MESSAGE_BYTES,
        };
        let (transport, notifications) =
            LspTransport::new(Box::new(client), options, Box::new(|_, _| Ok(Value::Null)));
        (transport, notifications, tokio::io::BufReader::new(server))
    }

    async fn read_required(stream: &mut tokio::io::BufReader<DuplexStream>) -> Value {
        read_frame(stream, TEST_MAX_MESSAGE_BYTES, Duration::from_secs(5))
            .await
            .expect("valid frame")
            .expect("frame before EOF")
    }

    async fn send(stream: &mut (impl AsyncWrite + Unpin), message: Value) {
        write_message(stream, TEST_MAX_MESSAGE_BYTES, &message)
            .await
            .expect("write frame");
    }

    #[tokio::test]
    async fn dropped_request_cleans_pending_and_late_response_cannot_satisfy_next() {
        let (transport, _notifications, mut server) = test_transport(4);
        let mut request = Box::pin(transport.request("first", Value::Null));
        let first = tokio::select! {
            message = read_required(&mut server) => message,
            _ = &mut request => panic!("request completed without response"),
        };
        drop(request);
        let cancel = tokio::time::timeout(Duration::from_secs(1), read_required(&mut server))
            .await
            .unwrap();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], first["id"]);
        assert!(transport.inner.pending.lock().await.is_empty());
        assert!(!transport.is_closed());
        send(
            &mut server,
            json!({"jsonrpc":"2.0", "id":first["id"], "result":"late"}),
        )
        .await;
        let responder = async {
            let second = read_required(&mut server).await;
            assert_ne!(second["id"], first["id"]);
            send(
                &mut server,
                json!({"jsonrpc":"2.0", "id":second["id"], "result":"current"}),
            )
            .await;
        };
        let (result, ()) = tokio::join!(transport.request("second", Value::Null), responder);
        assert_eq!(result.unwrap(), "current");
    }

    #[tokio::test]
    async fn dropped_partial_frame_closes_transport() {
        let (transport, mut server) = stalled_transport(Duration::from_secs(10));
        let mut request = Box::pin(transport.request("blocked", json!({"text":"x".repeat(1024)})));
        let mut byte = [0];
        tokio::select! {
            result = server.read_exact(&mut byte) => { result.unwrap(); },
            _ = &mut request => panic!("write should block"),
        }
        drop(request);
        assert!(transport.is_closed());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !transport.inner.pending.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn header_limit_applies_before_newline_and_partial_eof_is_error() {
        let (client, mut peer) = tokio::io::duplex(8192);
        peer.write_all(&vec![b'x'; 4097]).await.unwrap();
        let mut reader = tokio::io::BufReader::new(client);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            read_frame(&mut reader, 8192, Duration::from_secs(5)),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(TransportError::Protocol(_))));
        let mut truncated = tokio::io::BufReader::new(&b"Content-Length: 1\r\n"[..]);
        assert!(matches!(
            read_frame(&mut truncated, 8192, Duration::from_secs(5)).await,
            Err(TransportError::Protocol(_))
        ));
        let mut duplicated =
            tokio::io::BufReader::new(&b"Content-Length: 1\r\nContent-Length: 2\r\n\r\n{}"[..]);
        assert!(matches!(
            read_frame(&mut duplicated, 8192, Duration::from_secs(5)).await,
            Err(TransportError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn independent_requests_are_sent_before_any_response_and_correlate_out_of_order() {
        let (transport, _notifications, mut server) = test_transport(4);
        let responder = async {
            let mut requests = Vec::new();
            for _ in 0..3 {
                requests.push(read_required(&mut server).await);
            }
            for request in requests.into_iter().rev() {
                send(
                    &mut server,
                    json!({"jsonrpc":"2.0", "id":request["id"], "result":request["method"]}),
                )
                .await;
            }
        };
        let (definition, references, hover, ()) = tokio::join!(
            transport.request("textDocument/definition", Value::Null),
            transport.request("textDocument/references", Value::Null),
            transport.request("textDocument/hover", Value::Null),
            responder,
        );
        assert_eq!(definition.unwrap(), "textDocument/definition");
        assert_eq!(references.unwrap(), "textDocument/references");
        assert_eq!(hover.unwrap(), "textDocument/hover");
    }

    #[tokio::test]
    async fn fragmented_unicode_frames_preserve_following_buffered_message() {
        let (reader, mut writer) = tokio::io::duplex(64);
        let message = json!({"result":"é🚀".repeat(1024)});
        let body = serde_json::to_vec(&message).unwrap();
        let frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        let mut bytes = frame;
        bytes.extend(body);
        bytes.extend_from_slice(b"Content-Length: 4\r\n\r\nnull");
        let sender = async {
            for fragment in bytes.chunks(7) {
                writer.write_all(fragment).await.unwrap();
            }
        };
        let receiver = async {
            let mut reader = tokio::io::BufReader::with_capacity(32, reader);
            assert_eq!(
                read_frame(&mut reader, 16384, Duration::from_secs(5))
                    .await
                    .unwrap(),
                Some(message)
            );
            assert_eq!(
                read_frame(&mut reader, 16384, Duration::from_secs(5))
                    .await
                    .unwrap(),
                Some(Value::Null)
            );
        };
        tokio::join!(sender, receiver);
    }

    #[tokio::test]
    async fn notification_is_delivered() {
        let (_transport, mut notifications, mut server) = test_transport(4);
        send(
            &mut server,
            json!({
                "jsonrpc": "2.0",
                "method": "window/logMessage",
                "params": { "message": "ready" }
            }),
        )
        .await;

        let notification = tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await
            .expect("notification must not block")
            .expect("notification stream open");
        assert_eq!(notification.method, "window/logMessage");
        assert_eq!(notification.params["message"], "ready");
    }

    #[tokio::test]
    async fn response_and_notification_interleave() {
        let (transport, mut notifications, mut server) = test_transport(4);
        let server_task = tokio::spawn(async move {
            let request = read_required(&mut server).await;
            let id = request["id"].clone();
            send(
                &mut server,
                json!({
                    "jsonrpc": "2.0",
                    "method": "window/logMessage",
                    "params": { "message": "between" }
                }),
            )
            .await;
            send(
                &mut server,
                json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } }),
            )
            .await;
        });

        let response = transport
            .request("example/interleaved", json!({}))
            .await
            .expect("response delivered");
        assert_eq!(response["ok"], true);
        let notification = notifications.recv().await.expect("notification delivered");
        assert_eq!(notification.params["message"], "between");
        server_task.await.expect("mock server task");
    }

    #[tokio::test]
    async fn flood_above_capacity_never_blocks_responses() {
        let (transport, _notifications, mut server) = test_transport(2);
        let server_task = tokio::spawn(async move {
            let request = read_required(&mut server).await;
            let id = request["id"].clone();
            for sequence in 0..256 {
                send(
                    &mut server,
                    json!({
                        "jsonrpc": "2.0",
                        "method": "window/logMessage",
                        "params": { "sequence": sequence }
                    }),
                )
                .await;
            }
            send(
                &mut server,
                json!({ "jsonrpc": "2.0", "id": id, "result": "responsive" }),
            )
            .await;
        });

        let response = tokio::time::timeout(
            Duration::from_secs(1),
            transport.request("example/barrier", json!({})),
        )
        .await
        .expect("notification flood must not block the read loop")
        .expect("response delivered");
        assert_eq!(response, "responsive");
        assert!(transport.notification_drop_count() > 0);
        server_task.await.expect("mock server task");
    }

    #[tokio::test]
    async fn final_diagnostics_update_is_preserved() {
        let (transport, mut notifications, mut server) = test_transport(2);
        let server_task = tokio::spawn(async move {
            let request = read_required(&mut server).await;
            let id = request["id"].clone();
            for version in 0..128 {
                send(
                    &mut server,
                    json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": "file:///workspace/main.rs",
                            "version": version,
                            "diagnostics": [{ "message": format!("v{version}") }]
                        }
                    }),
                )
                .await;
            }
            send(
                &mut server,
                json!({ "jsonrpc": "2.0", "id": id, "result": null }),
            )
            .await;
        });

        transport
            .request("example/barrier", json!({}))
            .await
            .expect("barrier response");
        let notification = notifications.recv().await.expect("latest diagnostics");
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        assert_eq!(notification.params["version"], 127);
        assert_eq!(notification.params["diagnostics"][0]["message"], "v127");
        server_task.await.expect("mock server task");
    }

    #[tokio::test]
    async fn unsupported_server_request_gets_method_not_found() {
        let (client, server) = tokio::io::duplex(TEST_MAX_MESSAGE_BYTES);
        let mut server = tokio::io::BufReader::new(server);
        let options = TransportOptions {
            max_message_bytes: TEST_MAX_MESSAGE_BYTES,
            request_timeout: Duration::from_secs(2),
            read_progress_timeout: Duration::from_secs(2),
            max_pending_requests: 8,
            notification_capacity: 4,
            notification_max_bytes: TEST_MAX_MESSAGE_BYTES,
        };
        let (transport, _notifications) = LspTransport::new(
            Box::new(client),
            options,
            Box::new(|method, _| {
                if method == "example/supported" {
                    Ok(json!({ "ok": true }))
                } else {
                    Err(format!("unsupported request: {method}"))
                }
            }),
        );
        send(
            &mut server,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "client/registerCapability",
                "params": {}
            }),
        )
        .await;
        let response = read_required(&mut server).await;
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32601);
        assert!(response.get("result").is_none());

        send(
            &mut server,
            json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "example/supported",
                "params": {}
            }),
        )
        .await;
        let response = read_required(&mut server).await;
        assert_eq!(response["result"], json!({ "ok": true }));
        drop(transport);
    }

    #[tokio::test]
    async fn cancellation_emits_cancel_request_and_returns_promptly() {
        let (transport, _notifications, mut server) = test_transport(4);
        let cancellation = CancellationToken::new();
        let server_task = tokio::spawn(async move {
            let request = read_required(&mut server).await;
            let request_id = request["id"].clone();
            let cancel = read_required(&mut server).await;
            assert_eq!(cancel["method"], "$/cancelRequest");
            assert_eq!(cancel["params"]["id"], request_id);
        });
        let cancel_task = {
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                cancellation.cancel();
            })
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            transport.request_cancellable("example/slow", json!({}), Some(&cancellation)),
        )
        .await
        .expect("cancellation must wake the request");
        assert!(matches!(
            result,
            Err(TransportError::RequestCancelled { ref method }) if method == "example/slow"
        ));
        cancel_task.await.expect("cancel task");
        server_task.await.expect("server observed cancellation");
    }

    #[tokio::test]
    async fn closing_notification_receiver_does_not_close_transport() {
        let (transport, notifications, mut server) = test_transport(2);
        drop(notifications);
        let server_task = tokio::spawn(async move {
            let request = read_required(&mut server).await;
            let id = request["id"].clone();
            send(
                &mut server,
                json!({
                    "jsonrpc": "2.0",
                    "method": "window/logMessage",
                    "params": { "message": "discarded" }
                }),
            )
            .await;
            send(
                &mut server,
                json!({ "jsonrpc": "2.0", "id": id, "result": 42 }),
            )
            .await;
        });

        let response = transport
            .request("example/afterReceiverClose", json!({}))
            .await
            .expect("response survives receiver closure");
        assert_eq!(response, 42);
        assert!(transport.notification_drop_count() >= 1);
        server_task.await.expect("mock server task");
    }

    #[tokio::test]
    async fn request_waiting_for_pending_lock_observes_concurrent_close() {
        let (transport, _notifications, _server) = test_transport(4);
        let pending = transport.inner.pending.lock().await;
        let mut request = Box::pin(transport.request("example/closingRace", json!({})));

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut request)
                .await
                .is_err(),
            "request must be waiting to acquire the pending lock"
        );
        transport.closed.store(true, Ordering::Release);
        drop(pending);

        let result = tokio::time::timeout(Duration::from_millis(100), request)
            .await
            .expect("closed transport must reject without waiting for request timeout");
        assert!(matches!(result, Err(TransportError::ServerClosed)));
        assert!(transport.inner.pending.lock().await.is_empty());
    }

    fn stalled_transport(timeout: Duration) -> (LspTransport, DuplexStream) {
        let (client, server) = tokio::io::duplex(32);
        let (transport, _) = LspTransport::new(
            Box::new(client),
            TransportOptions {
                request_timeout: timeout,
                ..Default::default()
            },
            Box::new(|_, _| Ok(Value::Null)),
        );
        (transport, server)
    }

    #[tokio::test]
    async fn blocked_request_write_times_out_and_closes_partial_frame() {
        let (transport, _server) = stalled_transport(Duration::from_millis(40));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            transport.request("example/blocked", json!({ "text": "x".repeat(1024) })),
        )
        .await
        .expect("write must respect request timeout");
        assert!(matches!(result, Err(TransportError::RequestTimeout { .. })));
        assert!(transport.is_closed());
        assert!(transport.inner.pending.lock().await.is_empty());
        assert!(matches!(
            transport.notify("exit", Value::Null).await,
            Err(TransportError::ServerClosed)
        ));
    }

    #[tokio::test]
    async fn blocked_request_write_observes_cancellation() {
        let (transport, _server) = stalled_transport(Duration::from_secs(10));
        let token = CancellationToken::new();
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        };
        let request = transport.request_cancellable("example/blocked", json!({}), Some(&token));
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(request, cancel)
        })
        .await
        .expect("cancel must interrupt a blocked write");
        assert!(matches!(
            result,
            Err(TransportError::RequestCancelled { .. })
        ));
        assert!(transport.is_closed());
        assert!(transport.inner.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn blocked_notification_write_is_bounded() {
        let (transport, _server) = stalled_transport(Duration::from_millis(40));
        let (sender, pending) = oneshot::channel();
        transport.inner.pending.lock().await.insert(
            json!(99),
            PendingEntry {
                sender: Some(sender),
                method: "example/other".into(),
            },
        );
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            transport.notify(
                "textDocument/didChange",
                json!({ "text": "x".repeat(1024) }),
            ),
        )
        .await
        .expect("notifications must not block forever");
        assert!(matches!(result, Err(TransportError::RequestTimeout { .. })));
        assert!(transport.is_closed());
        assert!(matches!(
            pending.await.expect("pending request failed"),
            Err(TransportError::ServerClosed)
        ));
    }

    #[tokio::test]
    async fn response_timeout_does_not_hang_sending_cancel() {
        let (transport, server) = stalled_transport(Duration::from_millis(40));
        let mut server = tokio::io::BufReader::new(server);
        let (result, request) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(
                transport.request("example/noResponse", json!({})),
                read_required(&mut server)
            )
        })
        .await
        .expect("cancel notification must be bounded too");
        assert_eq!(request["method"], "example/noResponse");
        assert!(matches!(result, Err(TransportError::RequestTimeout { .. })));
        assert!(transport.is_closed(), "cancel frame could not be completed");
        assert!(transport.inner.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn exit_notification_uses_shutdown_grace() {
        let (transport, _server) = stalled_transport(Duration::from_secs(30));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            transport.notify_with_timeout("exit", Value::Null, Duration::from_millis(40)),
        )
        .await
        .expect("exit must not consume the normal request timeout");
        assert!(matches!(result, Err(TransportError::RequestTimeout { .. })));
        assert!(transport.is_closed());
    }

    #[tokio::test]
    async fn writer_lock_wait_is_part_of_request_deadline() {
        let (transport, _server) = stalled_transport(Duration::from_millis(40));
        let _writer = transport.writer.lock().await;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            transport.request("example/queued", json!({})),
        )
        .await
        .expect("writer queue must respect deadline");
        assert!(matches!(result, Err(TransportError::RequestTimeout { .. })));
        assert!(!transport.is_closed(), "no frame was started");
        assert!(transport.inner.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn stalled_mid_body_frame_closes_transport_and_fails_pending() {
        let (client, server) = tokio::io::duplex(4096);
        let (transport, _notifications) = LspTransport::new(
            Box::new(client),
            TransportOptions {
                request_timeout: Duration::from_secs(5),
                read_progress_timeout: Duration::from_millis(60),
                ..Default::default()
            },
            Box::new(|_, _| Ok(Value::Null)),
        );
        let mut server = tokio::io::BufReader::new(server);
        let request = transport.request("example/pending", json!({}));
        tokio::pin!(request);
        tokio::select! {
            frame = read_required(&mut server) => {
                assert_eq!(frame["method"], "example/pending");
            }
            _ = &mut request => panic!("request resolved without a response"),
        }
        // Valid header, then a partial body and silence: the progress
        // deadline must trip instead of suspending the reader forever.
        server
            .write_all(b"Content-Length: 100\r\n\r\n{\"partial\":")
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), &mut request)
            .await
            .expect("stalled frame must fail the pending request");
        assert!(matches!(result, Err(TransportError::Stalled)), "{result:?}");
        assert!(transport.is_closed());
    }

    #[tokio::test]
    async fn idle_connection_never_trips_progress_deadline() {
        let (client, server) = tokio::io::duplex(4096);
        let (transport, _notifications) = LspTransport::new(
            Box::new(client),
            TransportOptions {
                request_timeout: Duration::from_secs(2),
                read_progress_timeout: Duration::from_millis(60),
                ..Default::default()
            },
            Box::new(|_, _| Ok(Value::Null)),
        );
        let mut server = tokio::io::BufReader::new(server);
        // Well past several progress deadlines with zero traffic: an idle
        // connection is legal and must stay open.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!transport.is_closed());
        let request = transport.request("example/ping", json!({}));
        tokio::pin!(request);
        let frame = tokio::select! {
            frame = read_required(&mut server) => frame,
            _ = &mut request => panic!("request resolved without a response"),
        };
        send(
            &mut server,
            json!({ "jsonrpc": "2.0", "id": frame["id"], "result": 1 }),
        )
        .await;
        assert_eq!(request.await.expect("idle connection still works"), 1);
    }

    #[tokio::test]
    async fn blocked_server_response_write_does_not_stall_the_reader() {
        // Pipe buffer of 32 bytes: the handler's 2 KiB response parks inside
        // write_all because the test never drains the client->server side.
        let (client, server) = tokio::io::duplex(32);
        let (transport, mut notifications) = LspTransport::new(
            Box::new(client),
            TransportOptions {
                request_timeout: Duration::from_secs(5),
                read_progress_timeout: Duration::from_secs(5),
                ..Default::default()
            },
            Box::new(|_, _| Ok(json!({ "reply": "x".repeat(2048) }))),
        );
        let mut server = tokio::io::BufReader::new(server);
        send(
            &mut server,
            json!({
                "jsonrpc": "2.0",
                "id": 41,
                "method": "workspace/configuration",
                "params": { "items": [{}] }
            }),
        )
        .await;
        // With the response write parked, the next incoming frame must still
        // be read and delivered — the read loop must not wait on the writer.
        // The send itself is bounded: a stalled reader would never drain the
        // 32-byte pipe and this write would hang instead of failing cleanly.
        tokio::time::timeout(
            Duration::from_secs(1),
            send(
                &mut server,
                json!({
                    "jsonrpc": "2.0",
                    "method": "window/logMessage",
                    "params": { "message": "still-reading" }
                }),
            ),
        )
        .await
        .expect("reader must keep draining incoming frames");
        let notification = tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await
            .expect("reader must not stall behind a blocked response write")
            .expect("notification stream open");
        assert_eq!(notification.params["message"], "still-reading");
        assert!(!transport.is_closed());
    }

    #[tokio::test]
    async fn mailbox_byte_budget_evicts_lower_value_traffic() {
        let (sender, mut receiver) = NotificationSender::channel(64, 1500);
        for index in 0..3 {
            sender.send(ServerNotification {
                method: "window/logMessage".into(),
                params: json!({ "message": "x".repeat(600), "index": index }),
            });
        }
        // ~700 bytes each: only two fit (~1400) and the third is dropped —
        // unclassified traffic may never displace queued items.
        sender.send(ServerNotification {
            method: "textDocument/publishDiagnostics".into(),
            params: json!({
                "uri": "file:///workspace/main.rs",
                "diagnostics": [{ "message": "y".repeat(900) }],
            }),
        });
        // ~1000 bytes: the diagnostics displaces both queued logMessages.
        let notification = receiver.recv().await.expect("diagnostics delivered");
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        assert!(
            sender.dropped() >= 3,
            "two evicted plus one undeliverable, got {}",
            sender.dropped()
        );
    }

    #[tokio::test]
    async fn error_response_preserves_code_message_and_data() {
        let (transport, _notifications, mut server) = test_transport(4);
        let server_task = tokio::spawn(async move {
            let request = read_required(&mut server).await;
            send(
                &mut server,
                json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {
                        "code": -32802,
                        "message": "server busy",
                        "data": { "retry": true }
                    }
                }),
            )
            .await;
        });
        let result = transport.request("example/fails", json!({})).await;
        match result {
            Err(TransportError::Remote {
                code,
                message,
                data,
            }) => {
                assert_eq!(code, -32802);
                assert_eq!(message, "server busy");
                assert_eq!(data, Some(json!({ "retry": true })));
            }
            other => panic!("expected a structured remote error: {other:?}"),
        }
        server_task.await.expect("mock server task");
    }
}
