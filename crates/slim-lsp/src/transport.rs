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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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
    notification: ServerNotification,
}

impl QueuedNotification {
    fn new(notification: ServerNotification) -> Self {
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
            notification,
        }
    }
}

#[derive(Debug, Default)]
struct NotificationMailboxState {
    queue: VecDeque<QueuedNotification>,
    sender_closed: bool,
    receiver_closed: bool,
}

#[derive(Debug)]
struct NotificationMailboxInner {
    capacity: usize,
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
    fn channel(capacity: usize) -> (Self, NotificationReceiver) {
        let inner = Arc::new(NotificationMailboxInner {
            capacity: capacity.max(1),
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

        if state.queue.len() < self.inner.capacity {
            state.queue.push_back(incoming);
            drop(state);
            self.inner.notify.notify_one();
            return;
        }

        let eviction = match &incoming.class {
            NotificationClass::Diagnostics(_) => state
                .queue
                .iter()
                .position(|queued| {
                    matches!(
                        queued.class,
                        NotificationClass::Progress {
                            intermediate: true,
                            ..
                        }
                    )
                })
                .or_else(|| {
                    state
                        .queue
                        .iter()
                        .position(|queued| matches!(queued.class, NotificationClass::Other))
                })
                .or_else(|| {
                    state.queue.iter().position(|queued| {
                        matches!(queued.class, NotificationClass::Progress { .. })
                    })
                })
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
                .position(|queued| {
                    matches!(
                        queued.class,
                        NotificationClass::Progress {
                            intermediate: true,
                            ..
                        }
                    )
                })
                .or_else(|| {
                    state
                        .queue
                        .iter()
                        .position(|queued| matches!(queued.class, NotificationClass::Other))
                })
                .or_else(|| {
                    state.queue.iter().position(|queued| {
                        matches!(queued.class, NotificationClass::Progress { .. })
                    })
                }),
        };

        if let Some(index) = eviction {
            let _ = state.queue.remove(index);
            state.queue.push_back(incoming);
            drop(state);
            self.inner.record_drop(1);
            self.inner.notify.notify_one();
        } else {
            drop(state);
            self.inner.record_drop(1);
        }
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
    /// Concurrent outstanding requests allowed per server.
    pub max_pending_requests: usize,
    /// Notifications buffered for the instance layer before coalescing/drops.
    pub notification_capacity: usize,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            max_message_bytes: 16 * 1024 * 1024,
            request_timeout: Duration::from_secs(30),
            max_pending_requests: 32,
            notification_capacity: 64,
        }
    }
}

#[derive(Debug)]
pub enum TransportError {
    Io(String),
    Protocol(String),
    MessageTooLarge { bytes: usize, limit: usize },
    RequestTimeout { method: String },
    RequestCancelled { method: String },
    PendingCapacity { method: String },
    ServerClosed,
    Parse(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(message) => write!(f, "io error: {message}"),
            TransportError::Protocol(message) => write!(f, "protocol error: {message}"),
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

/// Reads a full framed message from a buffered reader. Returns None on EOF.
/// Headers arrive line by line through the buffer (one syscall per chunk,
/// not per byte); the buffer persists across frames so body bytes already
/// read are never lost.
async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Value>, TransportError> {
    let mut header = Vec::with_capacity(128);
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))?;
        if read == 0 {
            return Ok(None);
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
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| TransportError::Io(e.to_string()))?;
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
        let (notify_tx, notifications) =
            NotificationSender::channel(inner.options.notification_capacity);
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
            let frame = match read_frame(&mut reader, inner.options.max_message_bytes).await {
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
                    Err(TransportError::Protocol(error.to_string()))
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
                // Server-initiated request: answer from the handler.
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
                let mut guard = writer.lock().await;
                if let Err(error) =
                    write_message(&mut *guard, inner.options.max_message_bytes, &response).await
                {
                    drop(guard);
                    closed.store(true, Ordering::Release);
                    Self::fail_all_pending(&inner, error).await;
                    break;
                }
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
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let mut guard = self.writer.lock().await;
            if let Err(error) =
                write_message(&mut *guard, self.inner.options.max_message_bytes, &payload).await
            {
                drop(guard);
                let _ = self.drop_pending(&id).await;
                return Err(error);
            }
        }

        if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                response = rx => Self::resolve_response(response),
                _ = cancellation.cancelled() => {
                    let method = self
                        .drop_pending(&id)
                        .await
                        .unwrap_or_else(|| method.to_owned());
                    let _ = self.send_cancel_notification(&id).await;
                    Err(TransportError::RequestCancelled { method })
                }
                _ = tokio::time::sleep(self.inner.options.request_timeout) => {
                    let method = self
                        .drop_pending(&id)
                        .await
                        .unwrap_or_else(|| method.to_owned());
                    let _ = self.send_cancel_notification(&id).await;
                    Err(TransportError::RequestTimeout { method })
                }
            }
        } else {
            match tokio::time::timeout(self.inner.options.request_timeout, rx).await {
                Ok(response) => Self::resolve_response(response),
                Err(_elapsed) => {
                    let method = self
                        .drop_pending(&id)
                        .await
                        .unwrap_or_else(|| method.to_owned());
                    let _ = self.send_cancel_notification(&id).await;
                    Err(TransportError::RequestTimeout { method })
                }
            }
        }
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
        let payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut guard = self.writer.lock().await;
        write_message(&mut *guard, self.inner.options.max_message_bytes, &payload).await
    }

    async fn send_cancel_notification(&self, id: &Value) -> Result<(), TransportError> {
        self.notify("$/cancelRequest", json!({ "id": id })).await
    }

    async fn drop_pending(&self, id: &Value) -> Option<String> {
        let mut map = self.inner.pending.lock().await;
        map.remove(id).map(|entry| entry.method)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Number of notifications coalesced, evicted or dropped because the
    /// bounded mailbox or its receiver could not accept them.
    pub fn notification_drop_count(&self) -> u64 {
        self.notifications.dropped()
    }
}

fn clone_error(error: &TransportError) -> TransportError {
    match error {
        TransportError::Io(message) => TransportError::Io(message.clone()),
        TransportError::Protocol(message) => TransportError::Protocol(message.clone()),
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
            max_pending_requests: 8,
            notification_capacity,
        };
        let (transport, notifications) =
            LspTransport::new(Box::new(client), options, Box::new(|_, _| Ok(Value::Null)));
        (
            transport,
            notifications,
            tokio::io::BufReader::new(server),
        )
    }

    async fn read_required(stream: &mut tokio::io::BufReader<DuplexStream>) -> Value {
        read_frame(stream, TEST_MAX_MESSAGE_BYTES)
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
            max_pending_requests: 8,
            notification_capacity: 4,
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
}
