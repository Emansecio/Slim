use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::mcp::spec::{
    McpCancellation, McpCleanupStatus, McpConnection, McpError, McpInterruption, McpRequestOutcome,
};
use crate::process::ExecutableResolver;

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 16 * 1024;
const OUTBOUND_QUEUE_CAPACITY: usize = 256;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const CLOSE_WAIT_LIMIT: Duration = Duration::from_millis(900);
const REAPER_WAIT_LIMIT: Duration = Duration::from_millis(800);

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
    /// Bytes at the front of `buffer` already confirmed newline-free; the
    /// scan resumes here so a long unterminated line is not re-scanned on
    /// every chunk.
    scanned: usize,
}

impl JsonLineFramer {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<FramedLine> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut consumed = 0;
        while let Some(relative) = self.buffer[self.scanned..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let newline = self.scanned + relative;
            let line = &self.buffer[consumed..newline];
            consumed = newline + 1;
            self.scanned = consumed;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice(line) {
                Ok(message) => lines.push(FramedLine::Message(message)),
                Err(_) => {
                    let noise = String::from_utf8_lossy(line);
                    let noise = if noise.chars().count() > 160 {
                        format!("{}…", noise.chars().take(160).collect::<String>())
                    } else {
                        noise.into_owned()
                    };
                    lines.push(FramedLine::Noise(noise));
                }
            }
        }
        self.buffer.drain(..consumed);
        self.scanned = self.buffer.len();
        lines
    }

    /// Bytes buffered without a terminating newline yet; used to enforce the
    /// per-message size bound before more data is accepted.
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }
}

type PendingMap = Arc<Mutex<HashMap<u64, Arc<RequestState>>>>;

enum OutboundMessage {
    Request(RequestEnvelope),
    Notification(Value),
}

struct RequestEnvelope {
    id: u64,
    deadline: Instant,
    timeout: Duration,
    message: Value,
    state: Arc<RequestState>,
    cancellation: McpCancellation,
}

struct RequestState {
    phase: Mutex<RequestPhase>,
    notify: Notify,
    deadline: Instant,
    timeout: Duration,
    cancellation: McpCancellation,
}

enum RequestPhase {
    Queued,
    Sending,
    Awaiting,
    Completed(Option<Result<Value, McpError>>),
    InterruptedBeforeSend(McpInterruption),
    OutcomeUncertain(McpInterruption),
    Taken,
}

enum InterruptedRequest {
    Completed(Result<Value, McpError>),
    BeforeSend(McpInterruption),
    Uncertain(McpInterruption),
    AlreadyTerminal,
}

impl RequestState {
    fn new(deadline: Instant, timeout: Duration, cancellation: McpCancellation) -> Self {
        Self {
            phase: Mutex::new(RequestPhase::Queued),
            notify: Notify::new(),
            deadline,
            timeout,
            cancellation,
        }
    }

    fn admit_send(
        &self,
        deadline: Instant,
        timeout: Duration,
        cancellation: &McpCancellation,
        closed: &AtomicBool,
    ) -> bool {
        let _admission = cancellation.lock_admission();
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(*phase, RequestPhase::Queued) {
            return false;
        }
        if cancellation.is_cancelled() {
            *phase = RequestPhase::InterruptedBeforeSend(McpInterruption::Cancelled);
            drop(phase);
            self.notify.notify_one();
            return false;
        }
        if closed.load(Ordering::Acquire) {
            *phase = RequestPhase::InterruptedBeforeSend(McpInterruption::ConnectionClosed);
            drop(phase);
            self.notify.notify_one();
            return false;
        }
        if Instant::now() >= deadline {
            *phase = RequestPhase::InterruptedBeforeSend(McpInterruption::TimedOut(timeout));
            drop(phase);
            self.notify.notify_one();
            return false;
        }
        *phase = RequestPhase::Sending;
        true
    }

    fn mark_awaiting(&self) {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(*phase, RequestPhase::Sending) {
            *phase = RequestPhase::Awaiting;
        }
    }

    fn complete(&self, result: Result<Value, McpError>) -> bool {
        let _admission = self.cancellation.lock_admission();
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(*phase, RequestPhase::Sending | RequestPhase::Awaiting) {
            return false;
        }
        *phase = if self.cancellation.is_cancelled() {
            RequestPhase::OutcomeUncertain(McpInterruption::Cancelled)
        } else if Instant::now() >= self.deadline {
            RequestPhase::OutcomeUncertain(McpInterruption::TimedOut(self.timeout))
        } else {
            RequestPhase::Completed(Some(result))
        };
        drop(phase);
        self.notify.notify_one();
        true
    }

    fn interrupt(&self, interruption: McpInterruption) -> InterruptedRequest {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut *phase {
            RequestPhase::Queued => {
                *phase = RequestPhase::InterruptedBeforeSend(interruption.clone());
                drop(phase);
                self.notify.notify_one();
                InterruptedRequest::BeforeSend(interruption)
            }
            RequestPhase::Sending | RequestPhase::Awaiting => {
                *phase = RequestPhase::OutcomeUncertain(interruption.clone());
                drop(phase);
                self.notify.notify_one();
                InterruptedRequest::Uncertain(interruption)
            }
            RequestPhase::Completed(result) => {
                let result = result.take().unwrap_or(Err(McpError::Closed));
                *phase = RequestPhase::Taken;
                InterruptedRequest::Completed(result)
            }
            RequestPhase::InterruptedBeforeSend(reason) => {
                InterruptedRequest::BeforeSend(reason.clone())
            }
            RequestPhase::OutcomeUncertain(reason) => InterruptedRequest::Uncertain(reason.clone()),
            RequestPhase::Taken => InterruptedRequest::AlreadyTerminal,
        }
    }

    fn abandon(&self) -> bool {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let close_transport = match *phase {
            RequestPhase::Queued => {
                *phase = RequestPhase::InterruptedBeforeSend(McpInterruption::Cancelled);
                false
            }
            RequestPhase::Sending | RequestPhase::Awaiting => {
                *phase = RequestPhase::OutcomeUncertain(McpInterruption::Cancelled);
                true
            }
            RequestPhase::Completed(_)
            | RequestPhase::InterruptedBeforeSend(_)
            | RequestPhase::OutcomeUncertain(_)
            | RequestPhase::Taken => false,
        };
        drop(phase);
        self.notify.notify_one();
        close_transport
    }

    fn take_terminal(&self) -> Option<McpRequestOutcome<Value>> {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut *phase {
            RequestPhase::Completed(result) => {
                let result = result.take().unwrap_or(Err(McpError::Closed));
                *phase = RequestPhase::Taken;
                Some(McpRequestOutcome::Completed(result))
            }
            RequestPhase::InterruptedBeforeSend(interruption) => {
                let interruption = interruption.clone();
                *phase = RequestPhase::Taken;
                Some(McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup: McpCleanupStatus::NotRequired,
                })
            }
            RequestPhase::OutcomeUncertain(interruption) => {
                Some(McpRequestOutcome::OutcomeUncertain {
                    interruption: interruption.clone(),
                    cleanup: McpCleanupStatus::Unconfirmed,
                })
            }
            RequestPhase::Queued
            | RequestPhase::Sending
            | RequestPhase::Awaiting
            | RequestPhase::Taken => None,
        }
    }
}

struct ProcessResources {
    child: Child,
    #[cfg(unix)]
    resolver: ExecutableResolver,
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    job: crate::process::windows_job::Job,
}

struct CloseControl {
    closed: AtomicBool,
    admission_gate: Mutex<()>,
    pending: PendingMap,
    resources: Mutex<Option<ProcessResources>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    cleanup: Mutex<Option<McpCleanupStatus>>,
    cleanup_started_at: Mutex<Option<Instant>>,
    cleanup_notify: Notify,
}

struct RequestGuard {
    id: u64,
    state: Arc<RequestState>,
    close: Arc<CloseControl>,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let close_transport = self.state.abandon();
        remove_pending(&self.close.pending, self.id, &self.state);
        if close_transport {
            self.close.close_once();
        }
    }
}

impl CloseControl {
    fn close_once(self: &Arc<Self>) {
        let changed = {
            let _admission = self
                .admission_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.closed.swap(true, Ordering::AcqRel) {
                false
            } else {
                *self
                    .cleanup
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                *self
                    .cleanup_started_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
                true
            }
        };
        if !changed {
            return;
        }
        fail_pending(&self.pending);
        let resources = self
            .resources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let threads = std::mem::take(
            &mut *self
                .threads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let close = Arc::clone(self);
        std::thread::spawn(move || {
            let confirmed = reap_transport(resources, threads);
            *close
                .cleanup
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(if confirmed {
                McpCleanupStatus::Confirmed
            } else {
                McpCleanupStatus::Unconfirmed
            });
            close.cleanup_notify.notify_waiters();
        });
    }

    async fn wait_cleanup(&self) -> McpCleanupStatus {
        if let Some(status) = *self
            .cleanup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            return status;
        }
        let remaining = self
            .cleanup_started_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .map(|started| CLOSE_WAIT_LIMIT.saturating_sub(started.elapsed()));
        let Some(remaining) = remaining else {
            return McpCleanupStatus::NotRequired;
        };
        if remaining.is_zero() {
            return McpCleanupStatus::Unconfirmed;
        }
        let completed = tokio::time::timeout(remaining, async {
            loop {
                let notified = self.cleanup_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(status) = *self
                    .cleanup
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                {
                    return status;
                }
                notified.await;
            }
        })
        .await;
        completed.unwrap_or(McpCleanupStatus::Unconfirmed)
    }
}

fn reap_transport(resources: Option<ProcessResources>, mut threads: Vec<JoinHandle<()>>) -> bool {
    let mut confirmed = true;
    if let Some(mut resources) = resources {
        #[cfg(windows)]
        if resources.job.terminate().is_err() {
            confirmed = false;
        }
        #[cfg(unix)]
        if crate::process::terminate_process_tree(&resources.resolver, resources.pid).is_err() {
            confirmed = false;
        }

        let deadline = Instant::now() + REAPER_WAIT_LIMIT;
        let mut exited = false;
        while Instant::now() < deadline {
            match resources.child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => {
                    confirmed = false;
                    break;
                }
            }
        }
        if !exited {
            let _ = resources.child.kill();
            let kill_deadline = Instant::now() + REAPER_WAIT_LIMIT;
            while Instant::now() < kill_deadline {
                match resources.child.try_wait() {
                    Ok(Some(_)) => {
                        exited = true;
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            if !exited {
                confirmed = false;
            }
        }
    } else {
        confirmed = false;
    }

    let deadline = Instant::now() + REAPER_WAIT_LIMIT;
    while threads.iter().any(|thread| !thread.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    for thread in threads.drain(..) {
        if thread.is_finished() {
            if thread.join().is_err() {
                confirmed = false;
            }
        } else {
            confirmed = false;
        }
    }
    confirmed
}

/// Newline-delimited JSON-RPC over a child process' stdio pipes. One writer
/// thread owns stdin (FIFO ordering), one reader thread demultiplexes by id,
/// one thread keeps a bounded stderr tail for diagnostics. Dropping the
/// connection terminates the whole process tree (Job Object on Windows,
/// process group kill elsewhere).
pub(crate) struct StdioConnection {
    outbound: mpsc::SyncSender<OutboundMessage>,
    close: Arc<CloseControl>,
    next_id: AtomicU64,
    tools_stale: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    stdout_noise: Arc<Mutex<String>>,
    timeout: Duration,
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

        let (outbound_tx, outbound_rx) =
            mpsc::sync_channel::<OutboundMessage>(OUTBOUND_QUEUE_CAPACITY);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let close = Arc::new(CloseControl {
            closed: AtomicBool::new(false),
            admission_gate: Mutex::new(()),
            pending,
            resources: Mutex::new(Some(ProcessResources {
                child,
                #[cfg(unix)]
                resolver: resolver.clone(),
                #[cfg(unix)]
                pid,
                #[cfg(windows)]
                job,
            })),
            threads: Mutex::new(Vec::new()),
            cleanup: Mutex::new(Some(McpCleanupStatus::NotRequired)),
            cleanup_started_at: Mutex::new(None),
            cleanup_notify: Notify::new(),
        });
        let tools_stale = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stdout_noise = Arc::new(Mutex::new(String::new()));
        let start = Arc::new(Barrier::new(4));

        let threads = vec![
            spawn_writer(stdin, outbound_rx, Arc::clone(&close), Arc::clone(&start)),
            spawn_reader(
                stdout,
                Arc::clone(&close),
                Arc::clone(&tools_stale),
                Arc::clone(&stdout_noise),
                outbound_tx.clone(),
                Arc::clone(&start),
            ),
            spawn_stderr_reader(stderr, Arc::clone(&stderr_tail), Arc::clone(&start)),
        ];
        *close
            .threads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = threads;
        start.wait();

        Ok(Arc::new(Self {
            outbound: outbound_tx,
            close,
            next_id: AtomicU64::new(1),
            tools_stale,
            stderr_tail,
            stdout_noise,
            timeout,
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

    async fn request_inner(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + self.timeout;
        let state = Arc::new(RequestState::new(
            deadline,
            self.timeout,
            cancellation.clone(),
        ));
        {
            let mut pending = self
                .close
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.close.closed.load(Ordering::Acquire) {
                return McpRequestOutcome::Completed(Err(self.closed_error()));
            }
            pending.insert(id, Arc::clone(&state));
        }
        let guard = RequestGuard {
            id,
            state: Arc::clone(&state),
            close: Arc::clone(&self.close),
        };
        let envelope = RequestEnvelope {
            id,
            deadline,
            timeout: self.timeout,
            message: json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            state: Arc::clone(&state),
            cancellation: cancellation.clone(),
        };
        if let Err(error) = self.outbound.try_send(OutboundMessage::Request(envelope)) {
            let interruption = state.interrupt(McpInterruption::ConnectionClosed);
            remove_pending(&self.close.pending, id, &state);
            drop(guard);
            return match error {
                mpsc::TrySendError::Full(_) => McpRequestOutcome::Completed(Err(
                    McpError::Protocol("outbound queue full".into()),
                )),
                mpsc::TrySendError::Disconnected(_) => match interruption {
                    InterruptedRequest::BeforeSend(_) | InterruptedRequest::AlreadyTerminal => {
                        McpRequestOutcome::Completed(Err(self.closed_error()))
                    }
                    InterruptedRequest::Completed(result) => McpRequestOutcome::Completed(result),
                    InterruptedRequest::Uncertain(reason) => McpRequestOutcome::OutcomeUncertain {
                        interruption: reason,
                        cleanup: McpCleanupStatus::Unconfirmed,
                    },
                },
            };
        }

        loop {
            let notified = state.notify.notified();
            if let Some(outcome) = state.take_terminal() {
                let outcome = match outcome {
                    McpRequestOutcome::OutcomeUncertain { interruption, .. } => {
                        self.close.close_once();
                        McpRequestOutcome::OutcomeUncertain {
                            interruption,
                            cleanup: self.close.wait_cleanup().await,
                        }
                    }
                    outcome => outcome,
                };
                drop(guard);
                return outcome;
            }
            if cancellation.is_cancelled() {
                let outcome = self
                    .interrupt_request(&state, id, McpInterruption::Cancelled)
                    .await;
                drop(guard);
                return outcome;
            }
            if Instant::now() >= deadline {
                let outcome = self
                    .interrupt_request(&state, id, McpInterruption::TimedOut(self.timeout))
                    .await;
                drop(guard);
                return outcome;
            }
            tokio::select! {
                _ = cancellation.cancelled() => {},
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {},
                _ = notified => {},
            }
        }
    }

    async fn interrupt_request(
        &self,
        state: &Arc<RequestState>,
        id: u64,
        interruption: McpInterruption,
    ) -> McpRequestOutcome<Value> {
        match state.interrupt(interruption) {
            InterruptedRequest::Completed(result) => McpRequestOutcome::Completed(result),
            InterruptedRequest::BeforeSend(interruption) => {
                remove_pending(&self.close.pending, id, state);
                McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup: McpCleanupStatus::NotRequired,
                }
            }
            InterruptedRequest::Uncertain(interruption) => {
                remove_pending(&self.close.pending, id, state);
                self.close.close_once();
                McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup: self.close.wait_cleanup().await,
                }
            }
            InterruptedRequest::AlreadyTerminal => {
                McpRequestOutcome::Completed(Err(McpError::Closed))
            }
        }
    }
}

#[async_trait::async_trait]
impl McpConnection for StdioConnection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.request_inner(method, params, McpCancellation::new())
            .await
            .into_result()
    }

    async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        self.request_inner(method, params, cancellation).await
    }

    async fn notify(&self, method: &str, params: Value) {
        if self.close.closed.load(Ordering::Acquire) {
            return;
        }
        let _ = self.outbound.try_send(OutboundMessage::Notification(
            json!({"jsonrpc": "2.0", "method": method, "params": params}),
        ));
    }

    fn is_closed(&self) -> bool {
        self.close.closed.load(Ordering::Acquire)
    }

    async fn close_for_cleanup(&self) -> McpCleanupStatus {
        self.close.close_once();
        self.close.wait_cleanup().await
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
        self.close.close_once();
    }
}

fn spawn_writer(
    mut stdin: impl Write + Send + 'static,
    outbound_rx: mpsc::Receiver<OutboundMessage>,
    close: Arc<CloseControl>,
    start: Arc<Barrier>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        start.wait();
        loop {
            let message = match outbound_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if close.closed.load(Ordering::Acquire) {
                        break;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if close.closed.load(Ordering::Acquire) {
                break;
            }
            let (message, request) = match message {
                OutboundMessage::Request(envelope) => {
                    let admitted = {
                        let _admission = close
                            .admission_gate
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        envelope.state.admit_send(
                            envelope.deadline,
                            envelope.timeout,
                            &envelope.cancellation,
                            &close.closed,
                        )
                    };
                    if !admitted {
                        remove_pending(&close.pending, envelope.id, &envelope.state);
                        continue;
                    }
                    (envelope.message, Some(envelope.state))
                }
                OutboundMessage::Notification(message) => (message, None),
            };
            let Ok(bytes) = serde_json::to_vec(&message) else {
                close.close_once();
                break;
            };
            let failed = stdin
                .write_all(&bytes)
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .is_err();
            if failed {
                close.close_once();
                break;
            }
            if let Some(request) = request {
                request.mark_awaiting();
            }
        }
        close.close_once();
    })
}

fn spawn_reader(
    mut stdout: impl Read + Send + 'static,
    close: Arc<CloseControl>,
    tools_stale: Arc<AtomicBool>,
    stdout_noise: Arc<Mutex<String>>,
    outbound: mpsc::SyncSender<OutboundMessage>,
    start: Arc<Barrier>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        start.wait();
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
                        dispatch_inbound(message, &close.pending, &tools_stale, &outbound);
                    }
                    FramedLine::Noise(noise) => {
                        *stdout_noise
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = noise;
                    }
                }
            }
        }
        close.close_once();
    })
}

fn dispatch_inbound(
    message: Value,
    pending: &PendingMap,
    tools_stale: &Arc<AtomicBool>,
    outbound: &mpsc::SyncSender<OutboundMessage>,
) {
    if let Some(id) = message.get("id") {
        if message.get("method").is_some() {
            // Server-to-client request: sampling, elicitation, roots, etc.
            // v1 answers all of them with MethodNotFound so servers fail fast.
            // `id` is echoed verbatim — JSON-RPC allows string ids too.
            let _ = outbound.try_send(OutboundMessage::Notification(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": JSONRPC_METHOD_NOT_FOUND, "message": "unsupported"},
            })));
            return;
        }
        let Some(id) = id.as_u64() else {
            return;
        };
        let state = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        if let Some(state) = state {
            let result = if let Some(error) = message.get("error").filter(|error| !error.is_null())
            {
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
            state.complete(result);
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
    start: Arc<Barrier>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        start.wait();
        let mut chunk = [0u8; 4096];
        while let Ok(read) = stderr.read(&mut chunk) {
            if read == 0 {
                break;
            }
            let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            tail.extend(&chunk[..read]);
            let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
            if excess > 0 {
                tail.drain(..excess);
            }
        }
    })
}

/// Invalidates every waiter without retaining the pending-map lock while
/// notifying request states. Sent requests become explicitly uncertain.
fn fail_pending(pending: &PendingMap) {
    let requests = std::mem::take(
        &mut *pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for request in requests.into_values() {
        request.interrupt(McpInterruption::ConnectionClosed);
    }
}

fn remove_pending(pending: &PendingMap, id: u64, expected: &Arc<RequestState>) {
    let mut pending = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending
        .get(&id)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        pending.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_state(timeout: Duration, cancellation: McpCancellation) -> Arc<RequestState> {
        Arc::new(RequestState::new(
            Instant::now() + timeout,
            timeout,
            cancellation,
        ))
    }

    fn close_control(pending: PendingMap) -> Arc<CloseControl> {
        Arc::new(CloseControl {
            closed: AtomicBool::new(false),
            admission_gate: Mutex::new(()),
            pending,
            resources: Mutex::new(None),
            threads: Mutex::new(Vec::new()),
            cleanup: Mutex::new(Some(McpCleanupStatus::NotRequired)),
            cleanup_started_at: Mutex::new(None),
            cleanup_notify: Notify::new(),
        })
    }

    fn connection(
        outbound: mpsc::SyncSender<OutboundMessage>,
        close: Arc<CloseControl>,
        timeout: Duration,
    ) -> StdioConnection {
        StdioConnection {
            outbound,
            close,
            next_id: AtomicU64::new(1),
            tools_stale: Arc::new(AtomicBool::new(false)),
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            stdout_noise: Arc::new(Mutex::new(String::new())),
            timeout,
        }
    }

    #[derive(Clone, Default)]
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct BlockingWriter {
        entered: Option<mpsc::SyncSender<()>>,
        release: mpsc::Receiver<()>,
        blocked: bool,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                self.entered
                    .take()
                    .expect("single write entry")
                    .send(())
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "write-entry receiver dropped",
                        )
                    })?;
                self.release.recv().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "write-release sender dropped",
                    )
                })?;
            }
            self.written
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cancellation_while_writer_is_held_prevents_admission_and_write() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let cancellation = McpCancellation::new();
        let timeout = Duration::from_secs(10);
        let state = request_state(timeout, cancellation.clone());
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(1, Arc::clone(&state));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (outbound, receiver) = mpsc::sync_channel(1);
        let barrier = Arc::new(Barrier::new(2));
        let writer = spawn_writer(
            RecordingWriter(Arc::clone(&bytes)),
            receiver,
            Arc::clone(&close),
            Arc::clone(&barrier),
        );
        outbound
            .send(OutboundMessage::Request(RequestEnvelope {
                id: 1,
                deadline: Instant::now() + Duration::from_secs(10),
                timeout: Duration::from_secs(10),
                message: json!({"jsonrpc":"2.0","id":1,"method":"tools/call"}),
                state: Arc::clone(&state),
                cancellation: cancellation.clone(),
            }))
            .expect("queue request while writer is barrier-held");

        cancellation.cancel();
        barrier.wait();
        drop(outbound);
        writer.join().expect("writer exits after queue disconnect");

        assert!(bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            })
        ));
    }

    #[test]
    fn queued_request_expiring_while_notification_write_is_blocked_is_discarded() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let timeout = Duration::from_secs(7);
        let cancellation = McpCancellation::new();
        let deadline = Instant::now() + Duration::from_millis(200);
        let state = Arc::new(RequestState::new(deadline, timeout, cancellation.clone()));
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(2, Arc::clone(&state));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (outbound, receiver) = mpsc::sync_channel(2);
        let barrier = Arc::new(Barrier::new(2));
        let writer = spawn_writer(
            BlockingWriter {
                entered: Some(entered_tx),
                release: release_rx,
                blocked: false,
                written: Arc::clone(&bytes),
            },
            receiver,
            Arc::clone(&close),
            Arc::clone(&barrier),
        );
        let notification = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        outbound
            .send(OutboundMessage::Notification(notification.clone()))
            .expect("queue notification before the request");
        barrier.wait();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer is blocked while writing the notification");
        outbound
            .send(OutboundMessage::Request(RequestEnvelope {
                id: 2,
                deadline,
                timeout,
                message: json!({"jsonrpc":"2.0","id":2,"method":"delayed"}),
                state: Arc::clone(&state),
                cancellation,
            }))
            .expect("queue request with a future absolute deadline");

        let until_deadline = deadline.saturating_duration_since(Instant::now());
        let (deadline_tx, deadline_rx) = mpsc::sync_channel::<()>(1);
        assert_eq!(
            deadline_rx.recv_timeout(until_deadline),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the request deadline should expire while the notification write is blocked"
        );
        drop(deadline_tx);
        release_tx.send(()).expect("release notification write");
        drop(outbound);
        writer
            .join()
            .expect("writer exits after draining the expired request");

        let expected = serde_json::to_vec(&notification).expect("serialize notification");
        let mut expected = expected;
        expected.push(b'\n');
        assert_eq!(
            *bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            expected,
            "only the notification should reach the writer"
        );
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::TimedOut(timeout),
                cleanup: McpCleanupStatus::NotRequired,
            }) if timeout == Duration::from_secs(7)
        ));
    }

    #[test]
    fn queued_cancellation_batch_returns_pending_and_writer_queue_to_baseline() {
        const CANCELLED_REQUESTS: usize = 8;

        let pending = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (outbound, receiver) = mpsc::sync_channel(CANCELLED_REQUESTS + 1);
        let barrier = Arc::new(Barrier::new(2));
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = spawn_writer(
            BlockingWriter {
                entered: Some(entered_tx),
                release: release_rx,
                blocked: false,
                written: Arc::clone(&written),
            },
            receiver,
            Arc::clone(&close),
            Arc::clone(&barrier),
        );
        let notification = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        outbound
            .send(OutboundMessage::Notification(notification.clone()))
            .expect("queue notification to hold writer");
        barrier.wait();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer is blocked in notification write");

        let mut requests = Vec::with_capacity(CANCELLED_REQUESTS);
        for offset in 0..CANCELLED_REQUESTS {
            let id = 100 + offset as u64;
            let cancellation = McpCancellation::new();
            let timeout = Duration::from_secs(10);
            let state = request_state(timeout, cancellation.clone());
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id, Arc::clone(&state));
            outbound
                .send(OutboundMessage::Request(RequestEnvelope {
                    id,
                    deadline: Instant::now() + timeout,
                    timeout,
                    message: json!({"jsonrpc":"2.0","id":id,"method":"tools/call"}),
                    state: Arc::clone(&state),
                    cancellation: cancellation.clone(),
                }))
                .expect("bounded queue has room for the fixed batch");
            requests.push((id, state, cancellation));
        }
        assert_eq!(
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            CANCELLED_REQUESTS
        );

        for (id, state, cancellation) in &requests {
            cancellation.cancel();
            assert!(matches!(
                state.interrupt(McpInterruption::Cancelled),
                InterruptedRequest::BeforeSend(McpInterruption::Cancelled)
            ));
            remove_pending(&pending, *id, state);
        }
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());

        release_tx.send(()).expect("release notification write");
        drop(outbound);
        writer
            .join()
            .expect("writer thread is joined after queue drains");

        let mut expected = serde_json::to_vec(&notification).expect("serialize notification");
        expected.push(b'\n');
        assert_eq!(
            *written
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            expected,
            "cancelled requests must not be written"
        );
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        for (_, state, _) in &requests {
            assert!(matches!(
                state.take_terminal(),
                Some(McpRequestOutcome::InterruptedBeforeSend {
                    interruption: McpInterruption::Cancelled,
                    cleanup: McpCleanupStatus::NotRequired,
                })
            ));
        }
    }

    #[test]
    fn cancellation_after_sending_admission_is_outcome_uncertain() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let cancellation = McpCancellation::new();
        let timeout = Duration::from_secs(10);
        let state = request_state(timeout, cancellation.clone());
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(3, Arc::clone(&state));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (outbound, receiver) = mpsc::sync_channel(1);
        let barrier = Arc::new(Barrier::new(2));
        let writer = spawn_writer(
            BlockingWriter {
                entered: Some(entered_tx),
                release: release_rx,
                blocked: false,
                written: Arc::new(Mutex::new(Vec::new())),
            },
            receiver,
            Arc::clone(&close),
            Arc::clone(&barrier),
        );
        outbound
            .send(OutboundMessage::Request(RequestEnvelope {
                id: 3,
                deadline: Instant::now() + Duration::from_secs(10),
                timeout: Duration::from_secs(10),
                message: json!({"jsonrpc":"2.0","id":3,"method":"tools/call"}),
                state: Arc::clone(&state),
                cancellation: cancellation.clone(),
            }))
            .expect("queue request before releasing writer");
        barrier.wait();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer crossed admission and entered write");

        let phase = state
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(matches!(&*phase, RequestPhase::Sending));
        drop(phase);
        cancellation.cancel();
        assert!(matches!(
            state.interrupt(McpInterruption::Cancelled),
            InterruptedRequest::Uncertain(McpInterruption::Cancelled)
        ));

        release_tx.send(()).expect("release blocked writer");
        drop(outbound);
        writer.join().expect("writer exits after queue disconnect");
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::Unconfirmed,
            })
        ));
    }

    #[test]
    fn cancellation_after_response_preserves_completed_result() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cancellation = McpCancellation::new();
        let timeout = Duration::from_secs(10);
        let state = request_state(timeout, cancellation.clone());
        assert!(state.admit_send(
            Instant::now() + timeout,
            timeout,
            &cancellation,
            &AtomicBool::new(false),
        ));
        state.mark_awaiting();
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(7, Arc::clone(&state));

        dispatch_inbound(
            json!({"jsonrpc":"2.0","id":7,"result":{"ok":true}}),
            &pending,
            &Arc::new(AtomicBool::new(false)),
            &mpsc::sync_channel(1).0,
        );
        cancellation.cancel();

        assert!(matches!(
            state.interrupt(McpInterruption::Cancelled),
            InterruptedRequest::Completed(Ok(value)) if value == json!({"ok":true})
        ));
    }

    #[test]
    fn cancellation_before_response_wins_over_late_inbound_result() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cancellation = McpCancellation::new();
        let timeout = Duration::from_secs(10);
        let state = request_state(timeout, cancellation.clone());
        assert!(state.admit_send(
            Instant::now() + timeout,
            timeout,
            &cancellation,
            &AtomicBool::new(false),
        ));
        state.mark_awaiting();
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(8, Arc::clone(&state));

        cancellation.cancel();
        dispatch_inbound(
            json!({"jsonrpc":"2.0","id":8,"result":{"late":true}}),
            &pending,
            &Arc::new(AtomicBool::new(false)),
            &mpsc::sync_channel(1).0,
        );

        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::Unconfirmed,
            })
        ));
    }

    #[test]
    fn response_for_old_id_does_not_complete_new_pending_request() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let timeout = Duration::from_secs(10);
        let new_state = request_state(timeout, McpCancellation::new());
        assert!(new_state.admit_send(
            Instant::now() + timeout,
            timeout,
            &new_state.cancellation,
            &AtomicBool::new(false),
        ));
        new_state.mark_awaiting();
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(12, Arc::clone(&new_state));
        let tools_stale = Arc::new(AtomicBool::new(false));
        let (outbound, _receiver) = mpsc::sync_channel(1);

        dispatch_inbound(
            json!({"jsonrpc":"2.0","id":11,"result":{"old":true}}),
            &pending,
            &tools_stale,
            &outbound,
        );

        let phase = new_state
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(matches!(&*phase, RequestPhase::Awaiting));
        drop(phase);
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&12)
            .is_some_and(|state| Arc::ptr_eq(state, &new_state)));
        assert!(new_state.take_terminal().is_none());

        dispatch_inbound(
            json!({"jsonrpc":"2.0","id":12,"result":{"new":true}}),
            &pending,
            &tools_stale,
            &outbound,
        );
        assert!(matches!(
            new_state.take_terminal(),
            Some(McpRequestOutcome::Completed(Ok(value))) if value == json!({"new":true})
        ));
    }

    #[tokio::test]
    async fn close_once_invalidates_pending_request_and_cleanup_is_stable() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let timeout = Duration::from_secs(10);
        let state = request_state(timeout, McpCancellation::new());
        assert!(state.admit_send(
            Instant::now() + timeout,
            timeout,
            &state.cancellation,
            &AtomicBool::new(false),
        ));
        state.mark_awaiting();
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(9, Arc::clone(&state));
        let close = close_control(Arc::clone(&pending));

        close.close_once();
        close.close_once();

        assert!(close.closed.load(Ordering::Acquire));
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::ConnectionClosed,
                cleanup: McpCleanupStatus::Unconfirmed,
            })
        ));
        let (first_waiter, second_waiter) =
            tokio::join!(close.wait_cleanup(), close.wait_cleanup());
        assert_eq!(first_waiter, McpCleanupStatus::Unconfirmed);
        assert_eq!(second_waiter, McpCleanupStatus::Unconfirmed);
    }

    #[tokio::test]
    async fn close_once_is_bounded_and_reaper_joins_blocked_writer_after_release() {
        use std::future::Future;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let cancellation = McpCancellation::new();
        let timeout = Duration::from_secs(10);
        let state = request_state(timeout, cancellation.clone());
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(21, Arc::clone(&state));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (outbound, receiver) = mpsc::sync_channel(1);
        let barrier = Arc::new(Barrier::new(2));
        let writer = spawn_writer(
            BlockingWriter {
                entered: Some(entered_tx),
                release: release_rx,
                blocked: false,
                written: Arc::new(Mutex::new(Vec::new())),
            },
            receiver,
            Arc::clone(&close),
            Arc::clone(&barrier),
        );
        outbound
            .send(OutboundMessage::Request(RequestEnvelope {
                id: 21,
                deadline: Instant::now() + timeout,
                timeout,
                message: json!({"jsonrpc":"2.0","id":21,"method":"tools/call"}),
                state: Arc::clone(&state),
                cancellation,
            }))
            .expect("queue request before writer admission");
        barrier.wait();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer is blocked after admitting the request");
        assert!(matches!(
            &*state
                .phase
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            RequestPhase::Sending
        ));

        let (writer_joined_tx, writer_joined_rx) = mpsc::sync_channel(1);
        let joiner = std::thread::spawn(move || {
            let joined = writer.join().is_ok();
            let _ = writer_joined_tx.send(joined);
        });
        close
            .threads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(joiner);

        let (close_returned_tx, close_returned_rx) = mpsc::sync_channel(1);
        let close_for_thread = Arc::clone(&close);
        let close_thread = std::thread::spawn(move || {
            close_for_thread.close_once();
            let _ = close_returned_tx.send(());
        });
        let (second_close_tx, second_close_rx) = mpsc::sync_channel(1);
        let close_for_second_thread = Arc::clone(&close);
        let second_close_thread = std::thread::spawn(move || {
            close_for_second_thread.close_once();
            let _ = second_close_tx.send(());
        });
        let bounded_close_deadline = Instant::now() + Duration::from_millis(500);
        let close_returned_before_release = close_returned_rx
            .recv_timeout(bounded_close_deadline.saturating_duration_since(Instant::now()))
            .is_ok();
        let second_close_returned_before_release = second_close_rx
            .recv_timeout(bounded_close_deadline.saturating_duration_since(Instant::now()))
            .is_ok();

        let closed_while_writer_held = close.closed.load(Ordering::Acquire);
        let pending_invalidated_while_writer_held =
            pending.try_lock().is_ok_and(|pending| pending.is_empty());
        let uncertain_while_writer_held = matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::ConnectionClosed,
                cleanup: McpCleanupStatus::Unconfirmed,
            })
        );
        let not_confirmed_while_writer_held = close
            .cleanup
            .try_lock()
            .is_ok_and(|cleanup| !matches!(*cleanup, Some(McpCleanupStatus::Confirmed)));
        let mut cleanup = Box::pin(close.wait_cleanup());
        let cleanup_pending_while_writer_held =
            std::future::poll_fn(|context| std::task::Poll::Ready(cleanup.as_mut().poll(context)))
                .await
                .is_pending();

        release_tx.send(()).expect("release the admitted writer");
        drop(outbound);
        assert!(writer_joined_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("joiner observed writer termination"));
        close_thread.join().expect("close_once caller exits");
        second_close_thread
            .join()
            .expect("repeated close_once caller exits");
        close.close_once();

        assert!(close_returned_before_release);
        assert!(second_close_returned_before_release);
        assert!(closed_while_writer_held);
        assert!(pending_invalidated_while_writer_held);
        assert!(uncertain_while_writer_held);
        assert!(not_confirmed_while_writer_held);
        assert!(cleanup_pending_while_writer_held);
        // close_control has no child resource, so status stays Unconfirmed
        // even though the writer JoinHandle was collected successfully.
        assert_eq!(cleanup.await, McpCleanupStatus::Unconfirmed);
        assert!(close
            .threads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
    }

    #[tokio::test]
    async fn aborting_enqueued_request_future_drops_guard_and_reaps_sending_writer() {
        use std::future::Future;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (outbound, receiver) = mpsc::sync_channel(1);
        let barrier = Arc::new(Barrier::new(2));
        let writer = spawn_writer(
            BlockingWriter {
                entered: Some(entered_tx),
                release: release_rx,
                blocked: false,
                written: Arc::new(Mutex::new(Vec::new())),
            },
            receiver,
            Arc::clone(&close),
            Arc::clone(&barrier),
        );
        let (writer_joined_tx, writer_joined_rx) = mpsc::sync_channel(1);
        let joiner = std::thread::spawn(move || {
            let joined = writer.join().is_ok();
            let _ = writer_joined_tx.send(joined);
        });
        close
            .threads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(joiner);

        let connection = Arc::new(connection(
            outbound,
            Arc::clone(&close),
            Duration::from_secs(5),
        ));
        let request_connection = Arc::clone(&connection);
        let request = tokio::spawn(async move {
            request_connection
                .request_inner("tools/call", json!({"name":"ping"}), McpCancellation::new())
                .await
        });
        barrier.wait();
        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request crossed Sending and entered the blocked write");
        })
        .await
        .expect("writer-entry waiter exits");

        let state = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&1)
            .cloned()
            .expect("request_inner registered its pending state");
        assert!(matches!(
            &*state
                .phase
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            RequestPhase::Sending
        ));

        request.abort();
        assert!(request
            .await
            .expect_err("aborted request future")
            .is_cancelled());
        drop(connection);

        assert!(close.closed.load(Ordering::Acquire));
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::Unconfirmed,
            })
        ));
        let mut cleanup = Box::pin(close.wait_cleanup());
        let cleanup_pending =
            std::future::poll_fn(|context| std::task::Poll::Ready(cleanup.as_mut().poll(context)))
                .await
                .is_pending();

        release_tx.send(()).expect("release the in-flight writer");
        assert!(writer_joined_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("RequestGuard cleanup joined the writer"));
        assert_eq!(cleanup.await, McpCleanupStatus::Unconfirmed);
        assert!(cleanup_pending);
    }

    #[tokio::test]
    async fn full_outbound_queue_returns_explicit_error_without_leaking_waiter() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let (outbound, receiver) = mpsc::sync_channel(1);
        let retained = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        outbound
            .try_send(OutboundMessage::Notification(retained.clone()))
            .expect("fill outbound queue");
        let connection = connection(outbound, Arc::clone(&close), Duration::from_secs(5));

        let outcome = connection
            .request_inner("tools/list", json!({}), McpCancellation::new())
            .await;

        assert!(matches!(
            outcome,
            McpRequestOutcome::Completed(Err(McpError::Protocol(message)))
                if message == "outbound queue full"
        ));
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(!connection.is_closed());
        assert!(matches!(
            receiver.try_recv(),
            Ok(OutboundMessage::Notification(message)) if message == retained
        ));
    }

    #[test]
    fn dropping_queued_request_guard_removes_pending_without_closing_transport() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let state = request_state(Duration::from_secs(10), McpCancellation::new());
        pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(14, Arc::clone(&state));

        drop(RequestGuard {
            id: 14,
            state: Arc::clone(&state),
            close: Arc::clone(&close),
        });

        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(!close.closed.load(Ordering::Acquire));
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            })
        ));
    }

    #[tokio::test]
    async fn aborting_queued_request_inner_future_removes_pending_without_closing_transport() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let close = close_control(Arc::clone(&pending));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (outbound, receiver) = mpsc::sync_channel(2);
        let barrier = Arc::new(Barrier::new(2));
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = spawn_writer(
            BlockingWriter {
                entered: Some(entered_tx),
                release: release_rx,
                blocked: false,
                written: Arc::clone(&written),
            },
            receiver,
            Arc::clone(&close),
            Arc::clone(&barrier),
        );
        let connection = Arc::new(connection(
            outbound.clone(),
            Arc::clone(&close),
            Duration::from_secs(5),
        ));
        let notification = json!({
            "jsonrpc":"2.0",
            "method":"notifications/initialized",
            "params":{},
        });
        connection
            .notify("notifications/initialized", json!({}))
            .await;
        barrier.wait();
        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("writer entered the notification write");
        })
        .await
        .expect("writer-entry waiter exits");

        let request_connection = Arc::clone(&connection);
        let request = tokio::spawn(async move {
            request_connection
                .request_inner("tools/call", json!({"name":"ping"}), McpCancellation::new())
                .await
        });
        let state = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(state) = pending
                    .try_lock()
                    .ok()
                    .and_then(|pending| pending.get(&1).cloned())
                {
                    let queued = state
                        .phase
                        .try_lock()
                        .is_ok_and(|phase| matches!(&*phase, RequestPhase::Queued));
                    if queued {
                        break state;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request_inner enqueued an envelope that remains Queued");

        request.abort();
        assert!(request
            .await
            .expect_err("aborted queued request future")
            .is_cancelled());
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(!connection.is_closed());
        assert!(!close.closed.load(Ordering::Acquire));
        assert!(matches!(
            state.take_terminal(),
            Some(McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            })
        ));

        let fence = json!({
            "jsonrpc":"2.0",
            "method":"notifications/test-fence",
            "params":{"marker":"after-cancelled-request"},
        });
        outbound
            .try_send(OutboundMessage::Notification(fence.clone()))
            .expect("fence fits behind the cancelled request in the outbound queue");
        assert!(!connection.is_closed());
        release_tx.send(()).expect("release the notification write");

        let mut expected = serde_json::to_vec(&notification).expect("serialize notification");
        expected.push(b'\n');
        expected.extend(serde_json::to_vec(&fence).expect("serialize fence"));
        expected.push(b'\n');
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let written_fence = written
                    .try_lock()
                    .ok()
                    .is_some_and(|bytes| bytes.len() >= expected.len());
                if written_fence {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer crossed the cancelled request and wrote the FIFO fence");
        assert!(!connection.is_closed());
        assert!(!close.closed.load(Ordering::Acquire));
        assert_eq!(
            *written
                .try_lock()
                .expect("writer released byte buffer after the fence"),
            expected,
            "the cancelled request must be skipped between the initial notification and fence"
        );

        drop(connection);
        drop(outbound);
        tokio::task::spawn_blocking(move || writer.join().expect("writer exits after close"))
            .await
            .expect("writer-join waiter exits");

        assert_eq!(
            *written.try_lock().expect("writer thread has been joined"),
            expected,
            "aborted Queued request must not write tools/call"
        );
        assert!(pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(close.closed.load(Ordering::Acquire));
    }
}
