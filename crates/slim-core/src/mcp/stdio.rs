use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::mcp::spec::{McpConnection, McpError};
use crate::process::ExecutableResolver;

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 16 * 1024;
const OUTBOUND_QUEUE_CAPACITY: usize = 256;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;

/// One framed stdout line: a protocol message or a non-JSON line. Real-world
/// servers sometimes pollute stdout with log lines; those become `Noise`
/// instead of tearing down the transport.
#[derive(Debug)]
pub enum FramedLine {
    Message(Value),
    Noise(String),
}

#[derive(Default)]
pub struct JsonLineFramer {
    buffer: Vec<u8>,
}

impl JsonLineFramer {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<FramedLine> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_slice(trimmed) {
                Ok(message) => lines.push(FramedLine::Message(message)),
                Err(_) => {
                    let noise = String::from_utf8_lossy(trimmed);
                    let noise = if noise.chars().count() > 160 {
                        format!("{}…", noise.chars().take(160).collect::<String>())
                    } else {
                        noise.into_owned()
                    };
                    lines.push(FramedLine::Noise(noise));
                }
            }
        }
        lines
    }

    /// Bytes buffered without a terminating newline yet; used to enforce the
    /// per-message size bound before more data is accepted.
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

/// Newline-delimited JSON-RPC over a child process' stdio pipes. One writer
/// thread owns stdin (FIFO ordering), one reader thread demultiplexes by id,
/// one thread keeps a bounded stderr tail for diagnostics. Dropping the
/// connection terminates the whole process tree (Job Object on Windows,
/// process group kill elsewhere).
pub(crate) struct StdioConnection {
    outbound: mpsc::SyncSender<Value>,
    pending: PendingMap,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    tools_stale: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    stdout_noise: Arc<Mutex<String>>,
    timeout: Duration,
    #[cfg(unix)]
    resolver: ExecutableResolver,
    child: Mutex<Option<Child>>,
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    job: Mutex<Option<crate::process::windows_job::Job>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl StdioConnection {
    pub(crate) fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
        timeout: Duration,
        resolver: &ExecutableResolver,
    ) -> Result<Arc<Self>, McpError> {
        let program = resolver.resolve(command)?.ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("executable not found: {command}"),
            ))
        })?;
        let mut process = Command::new(program);
        process
            .args(args)
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.process_group(0);
        }
        #[cfg(windows)]
        let (mut child, job) = crate::process::windows_job::Job::spawn(&mut process)?;
        #[cfg(not(windows))]
        let mut child = process.spawn()?;
        #[cfg(unix)]
        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "mcp stdio stdin unavailable",
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "mcp stdio stdout unavailable",
            ))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "mcp stdio stderr unavailable",
            ))
        })?;

        let (outbound_tx, outbound_rx) = mpsc::sync_channel::<Value>(OUTBOUND_QUEUE_CAPACITY);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let tools_stale = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stdout_noise = Arc::new(Mutex::new(String::new()));

        let threads = vec![
            spawn_writer(
                stdin,
                outbound_rx,
                Arc::clone(&pending),
                Arc::clone(&closed),
            ),
            spawn_reader(
                stdout,
                Arc::clone(&pending),
                Arc::clone(&closed),
                Arc::clone(&tools_stale),
                Arc::clone(&stdout_noise),
                outbound_tx.clone(),
            ),
            spawn_stderr_reader(stderr, Arc::clone(&stderr_tail)),
        ];

        Ok(Arc::new(Self {
            outbound: outbound_tx,
            pending,
            next_id: AtomicU64::new(1),
            closed,
            tools_stale,
            stderr_tail,
            stdout_noise,
            timeout,
            #[cfg(unix)]
            resolver: resolver.clone(),
            child: Mutex::new(Some(child)),
            #[cfg(unix)]
            pid,
            #[cfg(windows)]
            job: Mutex::new(Some(job)),
            threads: Mutex::new(threads),
        }))
    }

    /// Last bytes the child wrote to stderr; attached to transport errors so
    /// a server crash surfaces its own diagnostic instead of a bare "closed".
    fn stderr_tail_text(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>())
            .trim()
            .to_owned()
    }

    fn closed_error(&self) -> McpError {
        let tail = self.stderr_tail_text();
        if !tail.is_empty() {
            return McpError::Protocol(format!("connection closed; stderr: {tail}"));
        }
        let noise = self
            .stdout_noise
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if noise.is_empty() {
            McpError::Closed
        } else {
            McpError::Protocol(format!("connection closed; stdout noise: {noise}"))
        }
    }
}

#[async_trait::async_trait]
impl McpConnection for StdioConnection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(self.closed_error());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.outbound.try_send(message) {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return Err(match error {
                mpsc::TrySendError::Full(_) => McpError::Protocol("outbound queue full".into()),
                mpsc::TrySendError::Disconnected(_) => self.closed_error(),
            });
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.closed_error()),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                Err(McpError::Timeout(self.timeout))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let _ = self
            .outbound
            .try_send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn take_tools_stale(&self) -> bool {
        self.tools_stale.swap(false, Ordering::Relaxed)
    }

    fn mark_tools_stale(&self) {
        self.tools_stale.store(true, Ordering::Relaxed);
    }
}

impl Drop for StdioConnection {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        #[cfg(windows)]
        if let Some(job) = self
            .job
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = job.terminate();
        }
        #[cfg(unix)]
        {
            let _ = crate::process::terminate_process_tree(&self.resolver, self.pid);
        }
        if let Some(mut child) = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        fail_pending(&self.pending);
        for thread in self
            .threads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
        {
            let _ = thread.join();
        }
    }
}

fn spawn_writer(
    mut stdin: impl Write + Send + 'static,
    outbound_rx: mpsc::Receiver<Value>,
    pending: PendingMap,
    closed: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        // recv_timeout + `closed`: Drop sets the flag while the struct still
        // holds a sender, so channel disconnect alone cannot be the exit
        // signal — the join would deadlock.
        loop {
            let message = match outbound_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if closed.load(Ordering::Relaxed) {
                        break;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let Ok(bytes) = serde_json::to_vec(&message) else {
                continue;
            };
            let failed = stdin
                .write_all(&bytes)
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .is_err();
            if failed {
                break;
            }
        }
        closed.store(true, Ordering::Relaxed);
        fail_pending(&pending);
    })
}

fn spawn_reader(
    mut stdout: impl Read + Send + 'static,
    pending: PendingMap,
    closed: Arc<AtomicBool>,
    tools_stale: Arc<AtomicBool>,
    stdout_noise: Arc<Mutex<String>>,
    outbound: mpsc::SyncSender<Value>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut framer = JsonLineFramer::default();
        let mut chunk = [0u8; 8192];
        loop {
            let read = match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if framer.pending_bytes() + read > MAX_MESSAGE_BYTES {
                break;
            }
            for line in framer.push(&chunk[..read]) {
                match line {
                    FramedLine::Message(message) => {
                        dispatch_inbound(message, &pending, &tools_stale, &outbound);
                    }
                    FramedLine::Noise(noise) => {
                        *stdout_noise
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = noise;
                    }
                }
            }
        }
        closed.store(true, Ordering::Relaxed);
        fail_pending(&pending);
    })
}

fn dispatch_inbound(
    message: Value,
    pending: &PendingMap,
    tools_stale: &Arc<AtomicBool>,
    outbound: &mpsc::SyncSender<Value>,
) {
    if let Some(id) = message.get("id") {
        if message.get("method").is_some() {
            // Server-to-client request: sampling, elicitation, roots, etc.
            // v1 answers all of them with MethodNotFound so servers fail fast.
            // `id` is echoed verbatim — JSON-RPC allows string ids too.
            let _ = outbound.try_send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": JSONRPC_METHOD_NOT_FOUND, "message": "unsupported"},
            }));
            return;
        }
        let Some(id) = id.as_u64() else {
            return;
        };
        let sender = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        if let Some(sender) = sender {
            let result = if let Some(error) = message.get("error") {
                Err(McpError::Server {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: crate::mcp::spec::bounded_server_text(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown server error"),
                    ),
                })
            } else if message.get("result").is_some() {
                Ok(message["result"].clone())
            } else {
                Err(McpError::Protocol(
                    "response has neither result nor error".into(),
                ))
            };
            let _ = sender.send(result);
        }
        return;
    }
    if message.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed") {
        tools_stale.store(true, Ordering::Relaxed);
    }
}

fn spawn_stderr_reader(
    mut stderr: impl Read + Send + 'static,
    tail: Arc<Mutex<VecDeque<u8>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(read) = stderr.read(&mut chunk) {
            if read == 0 {
                break;
            }
            let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            tail.extend(&chunk[..read]);
            while tail.len() > STDERR_TAIL_BYTES {
                tail.pop_front();
            }
        }
    })
}

/// Drops every waiter without answering: the receiver's `Err(RecvError)` arm
/// rebuilds the rich closed error (stderr tail / stdout noise) at the call
/// site instead of a bare `Closed` here.
fn fail_pending(pending: &PendingMap) {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}
