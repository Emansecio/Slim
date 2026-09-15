//! LspProcessPool: deduplicates language-server processes across sessions and
//! queries. Keyed by (workspace root, server id, config hash), so different
//! worktrees or different configurations naturally get different processes
//! while identical requests share one warm server.
//!
//! A session or query holds a Lease. The pool only tears a server down when
//! the last lease for that key is released and the idle window elapses.
//! Boot failures are remembered with exponential backoff so a broken server is
//! not respawned in a hot loop. Startup is singleflight per PoolKey and never
//! holds the pool-state mutex while a process initializes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::discovery::{config_hash, ServerSpec};
use crate::instance::{LspServerInstance, ServerInstanceConfig};
use crate::transport::{IoBox, TransportError};

const PROCESS_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Owns the direct child until it has been reaped. Every asynchronous exit
/// path takes the Child out of the Option before waiting, so Drop cannot kill a
/// PID that has already exited and potentially been reused by the OS.
struct ServerProcess {
    child: Option<tokio::process::Child>,
    pid: Option<u32>,
}

impl ServerProcess {
    fn new(child: tokio::process::Child) -> Self {
        let pid = child.id();
        Self {
            child: Some(child),
            pid,
        }
    }

    async fn wait_or_force_kill(mut self, grace: Duration) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => {
                terminate_process_tree(self.pid).await;
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }

    async fn force_kill_and_wait(mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        terminate_process_tree(self.pid).await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        if let Some(pid) = self.pid.filter(|pid| *pid != 0) {
            crate::process::kill_process_tree(pid);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

async fn terminate_process_tree(pid: Option<u32>) {
    let Some(pid) = pid.filter(|pid| *pid != 0) else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || crate::process::kill_process_tree(pid)).await;
}

/// Result of a process spawn: the io pair, a shared bounded stderr tail and
/// the direct child handle used for graceful shutdown and final reaping.
pub struct SpawnedServer {
    pub io: IoBox,
    pub stderr_tail: Arc<Mutex<String>>,
    pub process: Option<tokio::process::Child>,
}

impl SpawnedServer {
    pub fn new(
        io: IoBox,
        stderr_tail: Arc<Mutex<String>>,
        process: Option<tokio::process::Child>,
    ) -> Self {
        Self {
            io,
            stderr_tail,
            process,
        }
    }
}

/// Abstraction over process spawning so focused tests can still inject a
/// duplex transport while integration tests and production use real stdio.
/// The workspace root is passed through so the server starts with its
/// working directory inside the project it serves.
pub trait ProcessFactory: Send + Sync {
    fn spawn(&self, spec: &ServerSpec, root: &Path) -> Result<SpawnedServer, String>;
}

/// Spawns real language-server binaries with stdio piped.
pub struct StdioProcessFactory;

fn reap_failed_spawn(child: tokio::process::Child) {
    drop(tokio::spawn(async move {
        ServerProcess::new(child).force_kill_and_wait().await;
    }));
}

impl ProcessFactory for StdioProcessFactory {
    fn spawn(&self, spec: &ServerSpec, root: &Path) -> Result<SpawnedServer, String> {
        let mut command = tokio::process::Command::new(&spec.command);
        // Detach the server into its own process group so the Unix kill path
        // below reaches cargo/build-script grandchildren too (and a terminal
        // SIGINT no longer takes the server down with the parent).
        #[cfg(unix)]
        command.process_group(0);
        command
            .args(&spec.args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to spawn {}: {error}", spec.command))?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                reap_failed_spawn(child);
                return Err("child stdin unavailable".to_owned());
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                reap_failed_spawn(child);
                return Err("child stdout unavailable".to_owned());
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                reap_failed_spawn(child);
                return Err("child stderr unavailable".to_owned());
            }
        };
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            crate::process::capture_stderr_tail(stderr, tail).await;
        });
        let io: IoBox = Box::new(crate::process::StdioPair { stdin, stdout });
        Ok(SpawnedServer::new(io, stderr_tail, Some(child)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PoolKey {
    pub root: PathBuf,
    pub server_id: String,
    pub config_hash: u64,
}

#[derive(Clone)]
pub struct PoolConfig {
    /// Time a zero-lease server stays alive before shutdown. None keeps it
    /// alive until close_all or pool drop.
    pub idle_shutdown: Option<Duration>,
    /// Base backoff window for consecutive boot failures.
    pub circuit_window: Duration,
    /// Maximum simultaneous running or initializing language servers.
    pub max_servers: usize,
    /// Injectable spawner.
    pub factory: Arc<dyn ProcessFactory>,
}

impl std::fmt::Debug for PoolConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PoolConfig")
            .field("idle_shutdown", &self.idle_shutdown)
            .field("circuit_window", &self.circuit_window)
            .field("max_servers", &self.max_servers)
            .field("factory", &"boxed")
            .finish()
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            idle_shutdown: Some(Duration::from_secs(15 * 60)),
            circuit_window: Duration::from_secs(10),
            max_servers: 4,
            factory: Arc::new(StdioProcessFactory),
        }
    }
}

struct PoolEntry {
    instance: Arc<LspServerInstance>,
    process: Option<ServerProcess>,
    leases: Arc<AtomicUsize>,
    idle_task: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct PoolState {
    entries: HashMap<PoolKey, PoolEntry>,
    starting: HashMap<PoolKey, Arc<Notify>>,
    shutdowns_in_flight: usize,
    closed: bool,
}

struct FailureRecord {
    failures: u32,
    failed_until: Instant,
}

/// A final-lease release that happened outside any Tokio runtime, where the
/// idle-shutdown arming could not be spawned. Consumed by the next acquire.
struct OrphanedRelease {
    key: PoolKey,
    instance: Arc<LspServerInstance>,
    leases: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub enum PoolError {
    Unavailable { server: String, reason: String },
    ServerLimit { limit: usize },
    Transport(TransportError),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::Unavailable { server, reason } => {
                write!(formatter, "server {server} unavailable: {reason}")
            }
            PoolError::ServerLimit { limit } => {
                write!(formatter, "language-server process limit reached ({limit})")
            }
            PoolError::Transport(error) => write!(formatter, "transport: {error}"),
        }
    }
}

impl From<TransportError> for PoolError {
    fn from(error: TransportError) -> Self {
        PoolError::Transport(error)
    }
}

/// Lease token. Its instance is valid for the complete lifetime of the token;
/// releasing the final lease arms the idle-shutdown timer.
pub struct Lease {
    pool: Arc<LspProcessPool>,
    key: PoolKey,
    instance: Arc<LspServerInstance>,
    leases: Arc<AtomicUsize>,
}

impl Lease {
    pub fn instance(&self) -> &Arc<LspServerInstance> {
        &self.instance
    }

    pub fn key(&self) -> &PoolKey {
        &self.key
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if decrement_atomic_once(&self.leases).is_none() {
            return;
        }
        decrement_atomic(&self.pool.leases_total, 1);
        if self.leases.load(Ordering::Acquire) != 0 {
            return;
        }
        let pool = self.pool.clone();
        let key = self.key.clone();
        let leases = self.leases.clone();
        let expected = self.instance.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                pool.release_if_zero(&key, &expected, &leases).await;
            });
        } else {
            // No runtime to spawn the idle arming on: register the release
            // so the next acquire (or close_all via the entries map)
            // consumes it instead of leaking the server until shutdown.
            pool.orphaned_releases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(OrphanedRelease {
                    key,
                    instance: expected,
                    leases,
                });
        }
    }
}

/// Shared pool of language-server processes (see the module docs).
///
/// Ownership contract: the pool has no graceful `Drop` — async shutdown
/// cannot run in a destructor — so every owner must call [`close_all`](Self::close_all)
/// before dropping the last `Arc`. An un-shut pool falls back to
/// `kill_on_drop` plus the `ServerProcess` tree-kill on drop, which skips
/// the `shutdown`/`exit` handshake and abandons in-flight requests.
pub struct LspProcessPool {
    state: Mutex<PoolState>,
    failures: Mutex<HashMap<PoolKey, FailureRecord>>,
    orphaned_releases: std::sync::Mutex<Vec<OrphanedRelease>>,
    leases_total: AtomicUsize,
    lifecycle: Notify,
    config: PoolConfig,
}

impl LspProcessPool {
    pub fn new(config: PoolConfig) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PoolState::default()),
            failures: Mutex::new(HashMap::new()),
            orphaned_releases: std::sync::Mutex::new(Vec::new()),
            leases_total: AtomicUsize::new(0),
            lifecycle: Notify::new(),
            config,
        })
    }

    pub fn active_leases(&self) -> usize {
        self.leases_total.load(Ordering::Acquire)
    }

    /// Arms idle shutdown for final-lease releases that happened outside any
    /// runtime. Runs at most once per pending release; stale entries (already
    /// reacquired or removed) are harmless no-ops inside release_if_zero.
    async fn drain_orphaned_releases(self: &Arc<Self>) {
        let pending = std::mem::take(
            &mut *self
                .orphaned_releases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for release in pending {
            self.release_if_zero(&release.key, &release.instance, &release.leases)
                .await;
        }
    }

    /// Acquires or singleflight-starts the server for one PoolKey.
    pub async fn acquire(
        self: &Arc<Self>,
        root: PathBuf,
        spec: ServerSpec,
        config_payload: &Value,
        transport_options: crate::transport::TransportOptions,
        max_open_documents: usize,
    ) -> Result<Lease, PoolError> {
        let key = PoolKey {
            root: root.clone(),
            server_id: spec.id.clone(),
            config_hash: config_hash(config_payload),
        };

        self.drain_orphaned_releases().await;
        loop {
            self.check_circuit(&key, &spec.id).await?;

            let mut leader_signal = None;
            let waiter = {
                let mut state = self.state.lock().await;
                if state.closed {
                    return Err(PoolError::Unavailable {
                        server: spec.id.clone(),
                        reason: "process pool is closed".into(),
                    });
                }
                let dead = state
                    .entries
                    .get(&key)
                    .is_some_and(|entry| entry.instance.is_closed());
                if dead {
                    // Dead server: evict and start fresh below instead of
                    // handing a closed transport to a new caller. Lease
                    // accounting needs no fixup: outstanding leases keep
                    // their Arc and decrement normally on drop, while the
                    // missing entry makes release_if_zero a no-op.
                    let mut dead = state.entries.remove(&key).expect("entry checked above");
                    drop(dead.idle_task.take());
                    state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_add(1);
                    drop(state);
                    let this = self.clone();
                    tokio::spawn(async move {
                        dead.instance.shutdown().await;
                        if let Some(process) = dead.process.take() {
                            process.wait_or_force_kill(PROCESS_EXIT_GRACE).await;
                        }
                        let mut state = this.state.lock().await;
                        state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_sub(1);
                        drop(state);
                        this.lifecycle.notify_waiters();
                    });
                    continue;
                }
                if let Some(entry) = state.entries.get_mut(&key) {
                    if let Some(task) = entry.idle_task.take() {
                        task.abort();
                    }
                    entry.leases.fetch_add(1, Ordering::AcqRel);
                    self.leases_total.fetch_add(1, Ordering::AcqRel);
                    return Ok(Lease {
                        pool: self.clone(),
                        key,
                        instance: entry.instance.clone(),
                        leases: entry.leases.clone(),
                    });
                }

                if let Some(signal) = state.starting.get(&key) {
                    let mut waiter = Box::pin(signal.clone().notified_owned());
                    waiter.as_mut().enable();
                    Some(waiter)
                } else {
                    // A live process occupies a slot whether leased or not,
                    // but a zero-lease (idle or orphaned) entry is evicted
                    // under pressure instead of failing the acquire.
                    let occupied = state.entries.len().saturating_add(state.starting.len());
                    if occupied >= self.config.max_servers.max(1) {
                        let idle_key = state.entries.iter().find_map(|(key, entry)| {
                            (entry.leases.load(Ordering::Acquire) == 0).then(|| key.clone())
                        });
                        let Some(idle_key) = idle_key else {
                            return Err(PoolError::ServerLimit {
                                limit: self.config.max_servers.max(1),
                            });
                        };
                        let mut idle = state
                            .entries
                            .remove(&idle_key)
                            .expect("idle key checked above");
                        if let Some(task) = idle.idle_task.take() {
                            task.abort();
                        }
                        state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_add(1);
                        drop(state);
                        let this = self.clone();
                        tokio::spawn(async move {
                            idle.instance.shutdown().await;
                            if let Some(process) = idle.process.take() {
                                process.wait_or_force_kill(PROCESS_EXIT_GRACE).await;
                            }
                            let mut state = this.state.lock().await;
                            state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_sub(1);
                            drop(state);
                            this.lifecycle.notify_waiters();
                        });
                        continue;
                    }
                    let signal = Arc::new(Notify::new());
                    state.starting.insert(key.clone(), signal.clone());
                    leader_signal = Some(signal);
                    None
                }
            };

            if let Some(waiter) = waiter {
                waiter.await;
                continue;
            }

            let signal = leader_signal.expect("leader registers a singleflight signal");
            // The pool owns initialization, not the first consumer. Dropping
            // its JoinHandle detaches startup; its eventual Lease is dropped
            // normally, while other consumers still observe the shared flight.
            let this = self.clone();
            let config_payload = config_payload.clone();
            return tokio::spawn(async move {
                let started = this
                    .start_server(
                        root.clone(),
                        spec.clone(),
                        &config_payload,
                        transport_options.clone(),
                        max_open_documents,
                    )
                    .await;

                match started {
                    Ok((instance, process)) => {
                        this.failures.lock().await.remove(&key);
                        let mut state = this.state.lock().await;
                        let owns_flight = state
                            .starting
                            .get(&key)
                            .is_some_and(|current| Arc::ptr_eq(current, &signal));
                        if !owns_flight || state.closed {
                            let reason = if state.closed {
                                "process pool closed during server startup"
                            } else {
                                "server startup was superseded"
                            };
                            drop(state);
                            instance.shutdown().await;
                            if let Some(process) = process {
                                process.wait_or_force_kill(PROCESS_EXIT_GRACE).await;
                            }
                            if owns_flight {
                                let mut state = this.state.lock().await;
                                if state
                                    .starting
                                    .get(&key)
                                    .is_some_and(|current| Arc::ptr_eq(current, &signal))
                                {
                                    state.starting.remove(&key);
                                }
                            }
                            signal.notify_waiters();
                            this.lifecycle.notify_waiters();
                            return Err(PoolError::Unavailable {
                                server: spec.id.clone(),
                                reason: reason.into(),
                            });
                        }
                        state.starting.remove(&key);
                        let leases = Arc::new(AtomicUsize::new(1));
                        state.entries.insert(
                            key.clone(),
                            PoolEntry {
                                instance: instance.clone(),
                                process,
                                leases: leases.clone(),
                                idle_task: None,
                            },
                        );
                        this.leases_total.fetch_add(1, Ordering::AcqRel);
                        drop(state);
                        signal.notify_waiters();
                        Ok(Lease {
                            pool: this.clone(),
                            key,
                            instance,
                            leases,
                        })
                    }
                    Err(error) => {
                        this.record_failure(&key).await;
                        let mut state = this.state.lock().await;
                        if state
                            .starting
                            .get(&key)
                            .is_some_and(|current| Arc::ptr_eq(current, &signal))
                        {
                            state.starting.remove(&key);
                        }
                        drop(state);
                        signal.notify_waiters();
                        this.lifecycle.notify_waiters();
                        Err(PoolError::Unavailable {
                            server: spec.id.clone(),
                            reason: error,
                        })
                    }
                }
            })
            .await
            .map_err(|error| PoolError::Unavailable {
                server: "language server".into(),
                reason: format!("startup task failed: {error}"),
            })?;
        }
    }

    async fn start_server(
        &self,
        root: PathBuf,
        spec: ServerSpec,
        config_payload: &Value,
        transport_options: crate::transport::TransportOptions,
        max_open_documents: usize,
    ) -> Result<(Arc<LspServerInstance>, Option<ServerProcess>), String> {
        let spawned = self.config.factory.spawn(&spec, &root)?;
        let SpawnedServer {
            io,
            stderr_tail,
            process,
        } = spawned;
        let mut process = process.map(ServerProcess::new);
        let instance_config = ServerInstanceConfig {
            root,
            spec,
            transport_options,
            initialization_options: config_payload.clone(),
            settings: config_payload.clone(),
            max_open_documents,
        };
        match LspServerInstance::open(io, stderr_tail, instance_config).await {
            Ok(instance) => Ok((Arc::new(instance), process)),
            Err(error) => {
                if let Some(process) = process.take() {
                    process.force_kill_and_wait().await;
                }
                Err(format!("initialization failed: {error}"))
            }
        }
    }

    async fn check_circuit(&self, key: &PoolKey, server: &str) -> Result<(), PoolError> {
        let failures = self.failures.lock().await;
        if let Some(record) = failures.get(key) {
            if record.failed_until > Instant::now() {
                return Err(PoolError::Unavailable {
                    server: server.to_owned(),
                    reason: "server recently failed to start; waiting for backoff".into(),
                });
            }
        }
        Ok(())
    }

    async fn record_failure(&self, key: &PoolKey) {
        let mut failures = self.failures.lock().await;
        let record = failures.entry(key.clone()).or_insert(FailureRecord {
            failures: 0,
            failed_until: Instant::now(),
        });
        record.failures = record.failures.saturating_add(1);
        record.failed_until = Instant::now() + backoff(self.config.circuit_window, record.failures);
    }

    /// Acquires a lease only when the keyed server is already warm. This is
    /// used by status and post-write synchronization so they never start a
    /// process yet cannot race the idle-shutdown path.
    pub async fn acquire_warm(
        self: &Arc<Self>,
        root: &std::path::Path,
        server_id: &str,
        config_payload: &Value,
    ) -> Option<Lease> {
        let key = PoolKey {
            root: root.to_path_buf(),
            server_id: server_id.to_owned(),
            config_hash: config_hash(config_payload),
        };
        self.drain_orphaned_releases().await;
        let mut state = self.state.lock().await;
        if state.closed {
            return None;
        }
        let dead = state
            .entries
            .get(&key)
            .is_some_and(|entry| entry.instance.is_closed());
        if dead {
            // Warm-only callers never start a replacement; evict the dead
            // entry so the next full acquire starts fresh.
            let mut dead = state.entries.remove(&key).expect("entry checked above");
            drop(dead.idle_task.take());
            state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_add(1);
            drop(state);
            let this = self.clone();
            tokio::spawn(async move {
                dead.instance.shutdown().await;
                if let Some(process) = dead.process.take() {
                    process.wait_or_force_kill(PROCESS_EXIT_GRACE).await;
                }
                let mut state = this.state.lock().await;
                state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_sub(1);
                drop(state);
                this.lifecycle.notify_waiters();
            });
            return None;
        }
        let entry = state.entries.get_mut(&key)?;
        if let Some(task) = entry.idle_task.take() {
            task.abort();
        }
        entry.leases.fetch_add(1, Ordering::AcqRel);
        self.leases_total.fetch_add(1, Ordering::AcqRel);
        Some(Lease {
            pool: self.clone(),
            key,
            instance: entry.instance.clone(),
            leases: entry.leases.clone(),
        })
    }

    async fn release_if_zero(
        self: &Arc<Self>,
        key: &PoolKey,
        expected: &Arc<LspServerInstance>,
        leases: &Arc<AtomicUsize>,
    ) {
        let mut state = self.state.lock().await;
        let Some(entry) = state.entries.get_mut(key) else {
            return;
        };
        if !Arc::ptr_eq(&entry.instance, expected)
            || !Arc::ptr_eq(&entry.leases, leases)
            || entry.leases.load(Ordering::Acquire) != 0
        {
            return;
        }
        let Some(idle) = self.config.idle_shutdown else {
            return;
        };
        if let Some(task) = entry.idle_task.take() {
            task.abort();
        }
        let expected = entry.instance.clone();
        let pool = self.clone();
        let key = key.clone();
        entry.idle_task = Some(tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            pool.shutdown_if_idle(&key, &expected).await;
        }));
    }

    async fn shutdown_if_idle(&self, key: &PoolKey, expected: &Arc<LspServerInstance>) {
        let mut state = self.state.lock().await;
        let should_remove = state.entries.get(key).is_some_and(|entry| {
            entry.leases.load(Ordering::Acquire) == 0 && Arc::ptr_eq(&entry.instance, expected)
        });
        if !should_remove {
            return;
        }
        let Some(mut entry) = state.entries.remove(key) else {
            return;
        };
        drop(entry.idle_task.take());
        state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_add(1);
        drop(state);
        entry.instance.shutdown().await;
        if let Some(process) = entry.process.take() {
            process.wait_or_force_kill(PROCESS_EXIT_GRACE).await;
        }
        let mut state = self.state.lock().await;
        state.shutdowns_in_flight = state.shutdowns_in_flight.saturating_sub(1);
        drop(state);
        self.lifecycle.notify_waiters();
    }

    /// Shuts down every running server and waits for in-flight startup leaders,
    /// which self-terminate instead of inserting stale entries. This is the
    /// only graceful teardown: call it before dropping the last `Arc` to this
    /// pool (see the ownership contract on [`LspProcessPool`]).
    pub async fn close_all(&self) {
        let entries = {
            let mut state = self.state.lock().await;
            state.closed = true;
            let entries: Vec<PoolEntry> = state.entries.drain().map(|(_, entry)| entry).collect();
            entries
        };
        let released_leases = entries
            .iter()
            .map(|entry| entry.leases.swap(0, Ordering::AcqRel))
            .sum();
        decrement_atomic(&self.leases_total, released_leases);
        self.failures.lock().await.clear();
        let mut join_set = tokio::task::JoinSet::new();
        for mut entry in entries {
            join_set.spawn(async move {
                if let Some(task) = entry.idle_task.take() {
                    task.abort();
                }
                entry.instance.shutdown().await;
                if let Some(process) = entry.process.take() {
                    process.wait_or_force_kill(PROCESS_EXIT_GRACE).await;
                }
            });
        }
        while join_set.join_next().await.is_some() {}
        loop {
            let mut lifecycle = Box::pin(self.lifecycle.notified());
            lifecycle.as_mut().enable();
            let complete = {
                let state = self.state.lock().await;
                state.starting.is_empty() && state.shutdowns_in_flight == 0
            };
            if complete {
                break;
            }
            lifecycle.await;
        }
    }

    pub async fn running_servers(&self) -> usize {
        self.state.lock().await.entries.len()
    }
}

fn decrement_atomic(counter: &AtomicUsize, amount: usize) {
    if amount == 0 {
        return;
    }
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(amount))
    });
}

fn decrement_atomic_once(counter: &AtomicUsize) -> Option<usize> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_sub(1)
        })
        .ok()
}

fn backoff(window: Duration, failures: u32) -> Duration {
    let multiplier = 2u32.saturating_pow(failures.saturating_sub(1).min(5));
    window.saturating_mul(multiplier)
}
