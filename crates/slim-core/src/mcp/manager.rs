use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use serde_json::{json, Value};

use crate::mcp::client::{ClientContext, McpLog, McpProgressSink, McpServerHandshake};
use crate::mcp::http::{is_transient_error, HttpConnection};
use crate::mcp::oauth::McpAuth;
use crate::mcp::spec::{
    McpCancellation, McpCleanupStatus, McpConnection, McpError, McpExposure, McpInterruption,
    McpRequestOutcome, McpServerBlock, McpServerInfo, McpServerSpec, McpServerStatus,
    McpToolSummary, McpTransport, DEFAULT_MCP_STARTUP_WAIT, MCP_PROTOCOL_VERSION,
};
use crate::mcp::stdio::StdioConnection;
use crate::process::ExecutableResolver;
use crate::sync::{lock_mutex, read_lock, write_lock};

mod discovery;
mod resources;
pub(crate) use resources::clean_text;
pub use resources::{
    is_mcp_app_resource, McpResourceCounts, McpResourceItem, McpResourceListing, McpResourcePage,
    McpResourceTargets,
};

const MAX_TOOLS_PER_SERVER: usize = 256;
const MAX_LIST_SERVERS: usize = 64;
const MAX_LIST_TOOLS_PER_SERVER: usize = 32;
const MAX_DESCRIBE_BYTES: usize = 16 * 1024;
const MAX_TOOLS_PAGES: usize = 8;
/// Pauses before the retries of an HTTP connect that failed transiently
/// (network error, 408, 429, 5xx except 501). stdio connects are not retried.
const CONNECT_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(250), Duration::from_millis(1000)];

/// Outcome of waiting for servers that connect in the background.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpStartupWait {
    /// Direct-exposure servers that had not finished connecting when the
    /// wait ended. Empty: nothing was left to wait for.
    pub still_connecting: Vec<String>,
}

/// Header/env values, OAuth tokens and client secrets of every server.
fn collect_sensitive_values(servers: &RwLock<BTreeMap<String, Arc<ServerEntry>>>) -> Vec<String> {
    read_lock(servers)
        .values()
        .flat_map(|entry| {
            let mut values: Vec<String> =
                entry.spec.transport.sensitive_values().cloned().collect();
            if let Some(secret) = entry
                .spec
                .options
                .oauth
                .as_ref()
                .and_then(|oauth| oauth.client_secret.as_ref())
            {
                values.push(secret.clone());
            }
            // OAuth tokens and the registered client secret.
            if let Some(handle) = &entry.spec.options.auth {
                values.extend(handle.0.sensitive_values());
            }
            values
        })
        .collect()
}

/// Moves the potential teardown (process kill + thread joins, ~0.5 s) off the
/// caller: the last `Arc` drop runs `StdioConnection::drop` synchronously and
/// must not stall an executor thread or a `servers` write-lock hold.
fn drop_detached<T: Send + 'static>(value: T) {
    std::thread::spawn(move || drop(value));
}

/// Owns every configured MCP server and its lazily established connection.
const MCP_READONLY_CACHE_MAX_ENTRIES: usize = 256;
const MCP_READONLY_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;
const MCP_READONLY_CACHE_TTL: Duration = Duration::from_secs(60);

static MCP_READONLY_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static MCP_READONLY_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Process-wide (hits, misses) for the shared MCP read-only call cache.
pub fn read_only_cache_stats() -> (u64, u64) {
    (
        MCP_READONLY_CACHE_HITS.load(Ordering::Relaxed),
        MCP_READONLY_CACHE_MISSES.load(Ordering::Relaxed),
    )
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct McpCallCacheKey {
    server: String,
    tool: String,
    args_hash: String,
    generation: u64,
}

struct CachedMcpCall {
    value: Value,
    bytes: usize,
    stored_at: std::time::Instant,
    last_used: u64,
}

#[derive(Default)]
struct McpToolCallCache {
    entries: std::collections::HashMap<McpCallCacheKey, CachedMcpCall>,
    total_bytes: usize,
    clock: u64,
}

impl McpToolCallCache {
    fn get(&mut self, key: &McpCallCacheKey) -> Option<Value> {
        let expired = self
            .entries
            .get(key)
            .is_some_and(|e| e.stored_at.elapsed() >= MCP_READONLY_CACHE_TTL);
        if expired {
            if let Some(removed) = self.entries.remove(key) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            }
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = clock;
            MCP_READONLY_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return Some(entry.value.clone());
        }
        None
    }

    fn insert(&mut self, key: McpCallCacheKey, value: Value) {
        let bytes = value.to_string().len();
        if bytes > MCP_READONLY_CACHE_MAX_BYTES / 4 {
            return;
        }
        if let Some(prev) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.bytes);
        }
        while (self.entries.len() >= MCP_READONLY_CACHE_MAX_ENTRIES
            || self.total_bytes.saturating_add(bytes) > MCP_READONLY_CACHE_MAX_BYTES)
            && !self.entries.is_empty()
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            }
        }
        self.clock = self.clock.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.entries.insert(
            key,
            CachedMcpCall {
                value,
                bytes,
                stored_at: std::time::Instant::now(),
                last_used: self.clock,
            },
        );
    }

    fn invalidate_server(&mut self, server: &str) {
        let mut removed_bytes = 0usize;
        self.entries.retain(|k, v| {
            if k.server == server {
                removed_bytes = removed_bytes.saturating_add(v.bytes);
                false
            } else {
                true
            }
        });
        self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }
}

/// Manages the lifecycle and RPC calls of configured MCP servers.
///
/// Nothing is spawned or connected at construction: the first `list_tools`,
/// `describe`, or `call` (or a `/mcp` test) performs the handshake. All state
/// transitions bump `revision` so UIs can poll cheaply.
pub struct McpManager {
    cwd: PathBuf,
    resolver: ExecutableResolver,
    /// Optional shared project-wide manager; when set, `self.servers` holds
    /// session-private overlays (e.g. per-member `slim_swarm`) that never
    /// leak to sibling sessions and never tear down `parent` on `disconnect_all`.
    parent: Option<Arc<McpManager>>,
    servers: Arc<RwLock<BTreeMap<String, Arc<ServerEntry>>>>,
    revision: AtomicU64,
    /// Where servers' `notifications/message` log entries go, if anywhere.
    log: Option<Arc<McpLog>>,
    /// Woken on every state change (`revision` bump) for waiters.
    changed: tokio::sync::Notify,
    /// Cancels the background connects of this session on shutdown.
    startup_cancel: Mutex<Option<McpCancellation>>,
    /// How long the first model request waits for direct-exposure servers.
    startup_wait_ms: AtomicU64,
    /// The first wait already happened; later runs do not wait again.
    startup_wait_spent: AtomicBool,
    /// Provider names of direct tools and the declarations built from them.
    direct: Mutex<crate::mcp::exposure::DirectState>,
    /// Complete resource listings per server (see `resources`).
    resource_cache: resources::ResourceCache,
    /// Shared LRU cache for read-only, non-destructive MCP tool calls.
    tool_call_cache: Mutex<McpToolCallCache>,
}

struct ServerEntry {
    spec: McpServerSpec,
    status: RwLock<McpServerStatus>,
    /// `initialize` result of the live connection; cleared with it.
    handshake: RwLock<Option<Arc<McpServerHandshake>>>,
    /// Catalog of the last time this entry was `Ready`. Direct tools stay
    /// declared from it while the server is reconnecting or was dropped by a
    /// cancellation, so the request's tool set does not change under the
    /// prompt cache.
    last_tools: RwLock<Option<Arc<Vec<McpToolSummary>>>>,
    connection: tokio::sync::Mutex<Option<Arc<dyn McpConnection>>>,
    generation: AtomicU64,
    /// A background connect for this entry was scheduled and has not ended.
    startup_pending: AtomicBool,
}

/// Clears an entry's background-connect flag when the task ends, however it
/// ends, and wakes whoever waits for it.
struct StartupGuard {
    entry: Arc<ServerEntry>,
    manager: Weak<McpManager>,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        self.entry.startup_pending.store(false, Ordering::Release);
        if let Some(manager) = self.manager.upgrade() {
            manager.bump();
        }
    }
}

impl McpManager {
    pub fn new(
        specs: BTreeMap<String, McpServerSpec>,
        cwd: PathBuf,
        resolver: ExecutableResolver,
    ) -> Self {
        let servers = specs
            .into_values()
            .map(|spec| (spec.name.clone(), Arc::new(ServerEntry::new(spec))))
            .collect();
        Self {
            cwd,
            resolver,
            parent: None,
            servers: Arc::new(RwLock::new(servers)),
            revision: AtomicU64::new(0),
            log: None,
            changed: tokio::sync::Notify::new(),
            startup_cancel: Mutex::new(None),
            startup_wait_ms: AtomicU64::new(DEFAULT_MCP_STARTUP_WAIT.as_millis() as u64),
            startup_wait_spent: AtomicBool::new(false),
            direct: Mutex::new(Default::default()),
            resource_cache: resources::ResourceCache::default(),
            tool_call_cache: Mutex::new(McpToolCallCache::default()),
        }
    }

    /// Creates a lightweight per-session overlay backed by a shared project-wide
    /// `parent` manager. Calls to [`Self::insert_connection`] stay private to
    /// this overlay (e.g. per-member `slim_swarm`), while all other servers are
    /// transparently served by `parent`. [`Self::disconnect_all`] tears down
    /// only local overlays and never disconnects `parent`.
    pub fn with_session_overlays(parent: Arc<McpManager>) -> Arc<Self> {
        Arc::new(Self {
            cwd: parent.cwd.clone(),
            resolver: parent.resolver.clone(),
            parent: Some(Arc::clone(&parent)),
            servers: Arc::new(RwLock::new(BTreeMap::new())),
            revision: AtomicU64::new(0),
            log: parent.log.clone(),
            changed: tokio::sync::Notify::new(),
            startup_cancel: Mutex::new(None),
            startup_wait_ms: AtomicU64::new(parent.startup_wait_ms.load(Ordering::Relaxed)),
            startup_wait_spent: AtomicBool::new(false),
            direct: Mutex::new(Default::default()),
            resource_cache: resources::ResourceCache::default(),
            tool_call_cache: Mutex::new(McpToolCallCache::default()),
        })
    }

    /// True if this manager is a session overlay wrapping a shared project manager.
    pub fn has_shared_parent(&self) -> bool {
        self.parent.is_some()
    }

    /// Appends server log messages (`notifications/message`) to `path`
    /// (`<config dir>/logs/mcp.log`). Entries are redacted with the secrets
    /// this manager holds when each entry is written, so tokens obtained or
    /// rotated after construction (sign-in, refresh) are covered too.
    pub fn with_log_path(mut self, path: PathBuf) -> Self {
        let log = Arc::new(McpLog::new(path));
        let servers = Arc::downgrade(&self.servers);
        log.set_secret_source(Arc::new(move || {
            servers
                .upgrade()
                .map(|servers| collect_sensitive_values(&servers))
                .unwrap_or_default()
        }));
        self.log = Some(log);
        self
    }

    /// Test hook: registers a server backed by a prebuilt connection so the
    /// agent loop can be exercised without a real process or socket.
    pub fn insert_connection(
        &self,
        spec: McpServerSpec,
        connection: Arc<dyn McpConnection>,
        tools: Vec<McpToolSummary>,
    ) {
        let tools = Arc::new(tools);
        let entry = Arc::new(ServerEntry {
            status: RwLock::new(McpServerStatus::Ready {
                tools: Arc::clone(&tools),
            }),
            handshake: RwLock::new(None),
            last_tools: RwLock::new(Some(tools)),
            connection: tokio::sync::Mutex::new(Some(connection)),
            generation: AtomicU64::new(0),
            startup_pending: AtomicBool::new(false),
            spec,
        });
        write_lock(&self.servers).insert(entry.spec.name.clone(), entry);
        self.bump();
    }

    pub fn is_empty(&self) -> bool {
        read_lock(&self.servers).is_empty()
            && self.parent.as_ref().is_none_or(|parent| parent.is_empty())
    }

    pub fn has_enabled_servers(&self) -> bool {
        read_lock(&self.servers)
            .values()
            .any(|entry| entry.spec.enabled)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.has_enabled_servers())
    }

    pub fn revision(&self) -> u64 {
        let local = self.revision.load(Ordering::Relaxed);
        let parent = self
            .parent
            .as_ref()
            .map_or(0, |parent| parent.revision());
        local.wrapping_add(parent)
    }

    /// Configured header/env values across all servers; callers register them
    /// for redaction so secrets never reach prompts, events, or logs.
    pub fn sensitive_values(&self) -> Vec<String> {
        let mut values = collect_sensitive_values(&self.servers);
        if let Some(parent) = &self.parent {
            values.extend(parent.sensitive_values());
        }
        values
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    /// How long the first model request waits for direct-exposure servers
    /// that are still connecting in the background (`[mcp] startup_wait_ms`).
    pub fn startup_wait(&self) -> Duration {
        Duration::from_millis(self.startup_wait_ms.load(Ordering::Relaxed))
    }

    pub fn set_startup_wait(&self, wait: Duration) {
        self.startup_wait_ms.store(
            u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Connects every enabled, trusted, non-lazy, non-hidden server that is
    /// not connected yet, in the background and without blocking the caller.
    /// Servers that fail stay listed as failed and are retried on use, like
    /// any lazy server. Safe to call again after a config reload: servers
    /// already connecting or connected are left alone. Returns how many
    /// connects were started; without a tokio runtime nothing is started.
    pub fn start_background_connect(self: &Arc<Self>) -> usize {
        let parent_started = self
            .parent
            .as_ref()
            .map_or(0, |parent| parent.start_background_connect());
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return parent_started;
        };
        let cancellation = {
            let mut slot = lock_mutex(&self.startup_cancel);
            match slot.as_ref() {
                Some(existing) if !existing.is_cancelled() => existing.clone(),
                _ => {
                    let fresh = McpCancellation::new();
                    *slot = Some(fresh.clone());
                    fresh
                }
            }
        };
        let entries: Vec<Arc<ServerEntry>> = read_lock(&self.servers).values().cloned().collect();
        let mut started = parent_started;
        for entry in entries {
            if !entry.wants_startup_connect() || entry.startup_pending.swap(true, Ordering::AcqRel)
            {
                continue;
            }
            if entry.spec.options.has_direct_tools() {
                self.startup_wait_spent.store(false, Ordering::Release);
            }
            started += 1;
            let manager = Arc::downgrade(self);
            let cancellation = cancellation.clone();
            drop(runtime.spawn(async move {
                let _pending = StartupGuard {
                    entry: Arc::clone(&entry),
                    manager: manager.clone(),
                };
                let Some(manager) = manager.upgrade() else {
                    return;
                };
                let _ = manager
                    .ensure_connected_cancellable(&entry, cancellation)
                    .await;
            }));
        }
        started
    }

    /// Whether any background connect started by
    /// [`Self::start_background_connect`] has not ended. An entry is pending
    /// from the moment its connect is scheduled, before the task has marked
    /// it `Connecting`, so this is the reliable "still settling" signal.
    pub fn startup_in_progress(&self) -> bool {
        read_lock(&self.servers)
            .values()
            .any(|entry| entry.startup_pending.load(Ordering::Acquire))
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.startup_in_progress())
    }

    /// Direct-exposure servers whose background connect has not ended.
    fn pending_direct_servers(&self) -> Vec<String> {
        let mut pending: Vec<String> = read_lock(&self.servers)
            .values()
            .filter(|entry| {
                entry.spec.options.has_direct_tools()
                    && entry.startup_pending.load(Ordering::Acquire)
            })
            .map(|entry| entry.spec.name.clone())
            .collect();
        if let Some(parent) = &self.parent {
            for name in parent.pending_direct_servers() {
                if !pending.contains(&name) {
                    pending.push(name);
                }
            }
        }
        pending
    }

    /// Waits until the direct-exposure servers started by
    /// [`Self::start_background_connect`] have connected or failed, at most
    /// `timeout`. Only the first call per session waits: after it (finished
    /// or timed out) later calls return at once, so a slow server delays one
    /// run, not every run. Gateway servers are never waited for here; a call
    /// that names one waits for that server alone.
    pub async fn wait_for_direct_servers(&self, timeout: Duration) -> McpStartupWait {
        if let Some(parent) = &self.parent {
            let report = Box::pin(parent.wait_for_direct_servers(timeout)).await;
            if read_lock(&self.servers).values().all(|e| {
                !e.spec.options.has_direct_tools() || !e.startup_pending.load(Ordering::Acquire)
            }) {
                self.startup_wait_spent.store(true, Ordering::Release);
                return report;
            }
        }
        if self.startup_wait_spent.load(Ordering::Acquire) {
            return McpStartupWait::default();
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let report = loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let pending = self.pending_direct_servers();
            if pending.is_empty() {
                break McpStartupWait::default();
            }
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep_until(deadline) => {
                    break McpStartupWait { still_connecting: pending };
                }
            }
        };
        self.startup_wait_spent.store(true, Ordering::Release);
        report
    }

    fn entry(&self, name: &str) -> Result<Arc<ServerEntry>, McpError> {
        if let Some(local) = read_lock(&self.servers).get(name).cloned() {
            return Ok(local);
        }
        if let Some(parent) = &self.parent {
            return parent.entry(name);
        }
        Err(McpError::UnknownServer(name.to_owned()))
    }

    fn set_status(entry: &ServerEntry, status: McpServerStatus) {
        if let McpServerStatus::Ready { tools } = &status {
            *write_lock(&entry.last_tools) = Some(Arc::clone(tools));
        }
        // What the server reported belongs to the live connection only.
        if !matches!(status, McpServerStatus::Ready { .. }) {
            *write_lock(&entry.handshake) = None;
        }
        *write_lock(&entry.status) = status;
    }

    /// What the connected server reported in `initialize` (protocol version,
    /// identity, capabilities, instructions); `None` while not connected.
    pub fn handshake(&self, server: &str) -> Option<Arc<McpServerHandshake>> {
        self.entry(server)
            .ok()
            .and_then(|entry| read_lock(&entry.handshake).clone())
    }

    /// True once after the server announced
    /// `notifications/resources/list_changed`; resource caches must refresh.
    /// A server that is mid-connect (or not connected) reports false: there
    /// is no cache to invalidate yet.
    pub fn take_resources_stale(&self, server: &str) -> bool {
        let Ok(entry) = self.entry(server) else {
            return false;
        };
        let Ok(slot) = entry.connection.try_lock() else {
            return false;
        };
        slot.as_ref()
            .is_some_and(|connection| connection.take_resources_stale())
    }

    /// Replaces the configured server set (config reload): new servers are
    /// added disconnected, changed specs drop their connection, removed
    /// servers disappear.
    pub fn reconcile(&self, specs: BTreeMap<String, McpServerSpec>) {
        if let Some(parent) = &self.parent {
            parent.reconcile(specs);
            self.bump();
            return;
        }
        let mut replaced: Vec<Arc<ServerEntry>> = Vec::new();
        {
            let mut servers = write_lock(&self.servers);
            let current: Vec<String> = servers.keys().cloned().collect();
            for name in &current {
                match specs.get(name) {
                    Some(spec) if servers[name].spec == *spec => {}
                    Some(spec) => {
                        if let Some(old) =
                            servers.insert(name.clone(), Arc::new(ServerEntry::new(spec.clone())))
                        {
                            replaced.push(old);
                        }
                    }
                    None => {
                        if let Some(old) = servers.remove(name) {
                            replaced.push(old);
                        }
                    }
                }
            }
            for (name, spec) in specs {
                servers
                    .entry(name)
                    .or_insert_with(|| Arc::new(ServerEntry::new(spec)));
            }
        }
        {
            let mut cache = lock_mutex(&self.tool_call_cache);
            for old in &replaced {
                cache.invalidate_server(&old.spec.name);
            }
        }
        for old in &replaced {
            old.retire();
        }
        drop_detached(replaced);
        self.bump();
    }

    pub fn statuses(&self) -> Vec<McpServerInfo> {
        self.entries_snapshot()
            .into_iter()
            .map(|entry| McpServerInfo {
                name: entry.spec.name.clone(),
                transport: entry.spec.transport.kind(),
                target: entry.spec.transport.target(),
                enabled: entry.spec.enabled,
                description: entry.spec.options.description.clone(),
                exposure: entry.spec.options.exposure,
                status: read_lock(&entry.status).clone(),
                handshake: read_lock(&entry.handshake).clone(),
            })
            .collect()
    }

    /// Server list for the model: names, transport and status only. Never
    /// connects — tool discovery happens per server via [`Self::list_tools`].
    pub fn list_servers(&self) -> String {
        self.render_server_list()
    }

    /// Connects if needed and returns this server's tool summaries.
    pub async fn list_tools(&self, server: &str) -> Result<Arc<Vec<McpToolSummary>>, McpError> {
        self.list_tools_cancellable(server, McpCancellation::new())
            .await
            .into_result()
    }

    pub async fn list_tools_cancellable(
        &self,
        server: &str,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Arc<Vec<McpToolSummary>>> {
        if cancellation.is_cancelled() {
            return McpRequestOutcome::cancelled_before_send();
        }
        let entry = match self.entry(server) {
            Ok(entry) => entry,
            Err(error) => return McpRequestOutcome::Completed(Err(error)),
        };
        let (connection, generation) = match self
            .ensure_connected_cancellable(&entry, cancellation.clone())
            .await
            .pass()
        {
            Ok(connected) => connected,
            Err(outcome) => return outcome,
        };
        let stale = connection.take_tools_stale();
        let cached = read_lock(&entry.status).clone();
        if let McpServerStatus::Ready { tools } = &cached {
            if !stale {
                return McpRequestOutcome::Completed(Ok(Arc::clone(tools)));
            }
        }
        let outcome =
            fetch_tools_pages_cancellable(connection.as_ref(), cancellation.clone()).await;
        match outcome {
            McpRequestOutcome::Completed(Ok(tools)) => {
                let slot = entry.connection.lock().await;
                if entry.generation.load(Ordering::Acquire) == generation
                    && slot
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &connection))
                {
                    Self::set_status(
                        &entry,
                        McpServerStatus::Ready {
                            tools: Arc::clone(&tools),
                        },
                    );
                    self.bump();
                }
                McpRequestOutcome::Completed(Ok(tools))
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
                self.disconnect_if_generation(&entry, generation, &connection)
                    .await;
                McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                }
            }
            McpRequestOutcome::Completed(Err(
                error @ (McpError::Closed | McpError::Protocol(_) | McpError::SessionExpired),
            )) if connection.is_closed() => {
                let _ = connection.close_for_cleanup().await;
                self.disconnect_if_generation(&entry, generation, &connection)
                    .await;
                if cancellation.is_cancelled() {
                    return McpRequestOutcome::Completed(Err(error));
                }
                let (reconnected, reconnected_generation) = match self
                    .ensure_connected_cancellable(&entry, cancellation)
                    .await
                    .pass()
                {
                    Ok(reconnected) => reconnected,
                    Err(outcome) => return outcome,
                };
                let slot = entry.connection.lock().await;
                if entry.generation.load(Ordering::Acquire) == reconnected_generation
                    && slot
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &reconnected))
                {
                    match &*read_lock(&entry.status) {
                        McpServerStatus::Ready { tools } => {
                            McpRequestOutcome::Completed(Ok(Arc::clone(tools)))
                        }
                        _ => McpRequestOutcome::Completed(Err(McpError::Closed)),
                    }
                } else {
                    McpRequestOutcome::Completed(Err(McpError::Closed))
                }
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } if connection.is_closed() => {
                let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
                self.disconnect_if_generation(&entry, generation, &connection)
                    .await;
                McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                }
            }
            outcome @ (McpRequestOutcome::InterruptedBeforeSend { .. }
            | McpRequestOutcome::Completed(Err(_))) => {
                if stale {
                    connection.mark_tools_stale();
                }
                outcome
            }
        }
    }

    /// Tool names (+short descriptions) for the model, bounded. Pages of
    /// `MAX_LIST_TOOLS_PER_SERVER` entries are selected with `offset`; the
    /// trailing "… N more tools" line never counts toward the page size.
    pub async fn list_tools_text(&self, server: &str, offset: usize) -> Result<String, McpError> {
        self.ensure_reachable(server)?;
        let tools = self.list_tools(server).await?;
        Ok(self.tools_page_text(server, &tools, offset))
    }

    /// A global search reads connected catalogs only. Naming a server permits
    /// its usual lazy connection, keeping discovery out of startup.
    pub async fn search_tools_cancellable(
        &self,
        server: Option<&str>,
        query: &str,
        offset: usize,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<String> {
        if query.trim().is_empty() || query.len() > 512 {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(
                "query must contain 1-512 bytes".into(),
            )));
        }
        if let Some(server) = server {
            if let Err(error) = self.ensure_reachable(server) {
                return McpRequestOutcome::Completed(Err(error));
            }
            return self
                .list_tools_cancellable(server, cancellation)
                .await
                .map(|tools| self.search_one_text(server, tools, query, offset));
        }
        McpRequestOutcome::Completed(Ok(self.search_all_text(query, offset)))
    }

    pub async fn list_tools_text_cancellable(
        &self,
        server: &str,
        offset: usize,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<String> {
        if let Err(error) = self.ensure_reachable(server) {
            return McpRequestOutcome::Completed(Err(error));
        }
        self.list_tools_cancellable(server, cancellation)
            .await
            .map(|tools| self.tools_page_text(server, &tools, offset))
    }

    pub async fn describe_cancellable(
        &self,
        server: &str,
        tool: &str,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<String> {
        if let Err(error) = self
            .ensure_reachable(server)
            .and_then(|()| self.ensure_tool_reachable(server, tool))
        {
            return McpRequestOutcome::Completed(Err(error));
        }
        let tools = match self
            .list_tools_cancellable(server, cancellation)
            .await
            .pass()
        {
            Ok(tools) => tools,
            Err(outcome) => return outcome,
        };
        let Some(tool) = tools.iter().find(|candidate| candidate.name == tool) else {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(format!(
                "unknown tool {tool} on server {server}"
            ))));
        };
        McpRequestOutcome::Completed(render_tool_schema(tool))
    }

    pub async fn describe(&self, server: &str, tool: &str) -> Result<String, McpError> {
        self.ensure_reachable(server)?;
        self.ensure_tool_reachable(server, tool)?;
        let tools = self.list_tools(server).await?;
        let tool = tools
            .iter()
            .find(|candidate| candidate.name == tool)
            .ok_or_else(|| McpError::Protocol(format!("unknown tool {tool} on server {server}")))?;
        render_tool_schema(tool)
    }

    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, McpError> {
        self.call_cancellable(server, tool, arguments, McpCancellation::new())
            .await
            .into_result()
    }

    /// Runs one non-replayable tools/call with cancellation classified at the
    /// transport's Queued -> Sending boundary.
    pub async fn call_cancellable(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        self.call_with_progress(server, tool, arguments, cancellation, None)
            .await
    }

    /// [`Self::call_cancellable`] that also reports the server's
    /// `notifications/progress` to `progress`. The call always carries a
    /// progress token: progress renews the request timeout whether or not
    /// anyone listens. `progress` runs on a transport thread and must not
    /// block.
    pub async fn call_with_progress(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
        cancellation: McpCancellation,
        progress: Option<McpProgressSink>,
    ) -> McpRequestOutcome<Value> {
        if cancellation.is_cancelled() {
            return McpRequestOutcome::cancelled_before_send();
        }
        if !arguments.is_object() {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(
                "MCP tool arguments must be an object".into(),
            )));
        }
        // Delegate non-overlay servers directly to the shared parent manager so
        // all sessions share both warm connections and the read-only call cache.
        if let Some(parent) = &self.parent {
            if !read_lock(&self.servers).contains_key(server) {
                return Box::pin(parent.call_with_progress(
                    server,
                    tool,
                    arguments,
                    cancellation,
                    progress,
                ))
                .await;
            }
        }
        let entry = match self.entry(server) {
            Ok(entry) => entry,
            Err(error) => return McpRequestOutcome::Completed(Err(error)),
        };
        // Hidden tools are unreachable for every caller, before any connect.
        if let Err(error) = self.ensure_tool_reachable(server, tool) {
            return McpRequestOutcome::Completed(Err(error));
        }
        let (connection, generation) = match self
            .ensure_connected_cancellable(&entry, cancellation.clone())
            .await
        {
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                // Lazy initialize/notifications/tools/list may have reached
                // the server, but the requested tool itself was not admitted.
                return McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                };
            }
            connected => match connected.pass() {
                Ok(connected) => connected,
                Err(outcome) => return outcome,
            },
        };

        if cancellation.is_cancelled() {
            return McpRequestOutcome::cancelled_before_send();
        }
        // Never cache session-private overlays (e.g. in-process `slim_swarm::board_read`
        // advances cursors even though it sets `read_only_hint = true`).
        let is_session_overlay = self.parent.is_some();
        let tool_cacheable = !is_session_overlay
            && progress.is_none()
            && self.is_tool_cacheable_read_only(&entry, tool);
        let cache_key = if tool_cacheable {
            let args_json = canonical_json_string(&arguments);
            let args_hash = crate::tools::digest_bytes(b"slim-mcp-ro-v1", args_json.as_bytes());
            let key = McpCallCacheKey {
                server: server.to_owned(),
                tool: tool.to_owned(),
                args_hash,
                generation,
            };
            if let Some(cached) = lock_mutex(&self.tool_call_cache).get(&key) {
                return McpRequestOutcome::Completed(Ok(cached));
            }
            MCP_READONLY_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
            Some(key)
        } else {
            if !is_session_overlay {
                lock_mutex(&self.tool_call_cache).invalidate_server(server);
            }
            None
        };
        let progress = progress.unwrap_or_else(|| Arc::new(|_| {}));
        let outcome = connection
            .request_with_progress(
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                cancellation,
                Some(progress),
            )
            .await;
        let settled = self
            .settle_request(&entry, generation, &connection, outcome)
            .await;
        if let (Some(key), McpRequestOutcome::Completed(Ok(value))) = (cache_key, &settled) {
            if value.get("isError").and_then(Value::as_bool) != Some(true) {
                lock_mutex(&self.tool_call_cache).insert(key, value.clone());
            }
        }
        settled
    }

    fn is_tool_cacheable_read_only(&self, entry: &ServerEntry, tool_name: &str) -> bool {
        let tools = match read_lock(&entry.status).clone() {
            McpServerStatus::Ready { tools } => Some(tools),
            McpServerStatus::Disconnected | McpServerStatus::Connecting => {
                read_lock(&entry.last_tools).clone()
            }
            _ => None,
        };
        tools.is_some_and(|list| {
            list.iter()
                .find(|summary| summary.name == tool_name)
                .is_some_and(|summary| summary.read_only_hint && !summary.destructive_hint)
        })
    }

    /// Transport clean-up every manager request performs once its outcome is
    /// known: a connection that was lost, or a request cancelled after it may
    /// have been sent, drops the connection so the next use starts a fresh one.
    async fn settle_request(
        &self,
        entry: &Arc<ServerEntry>,
        generation: u64,
        connection: &Arc<dyn McpConnection>,
        outcome: McpRequestOutcome<Value>,
    ) -> McpRequestOutcome<Value> {
        match outcome {
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
                self.disconnect_if_generation(entry, generation, connection)
                    .await;
                McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                }
            }
            McpRequestOutcome::Completed(Err(error)) if connection.is_closed() => {
                let _ = connection.close_for_cleanup().await;
                self.disconnect_if_generation(entry, generation, connection)
                    .await;
                McpRequestOutcome::Completed(Err(error))
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } if connection.is_closed() => {
                let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
                self.disconnect_if_generation(entry, generation, connection)
                    .await;
                McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                }
            }
            // The server rejected the call before running it and the user
            // has to sign in: the entry shows it and the next use starts a
            // fresh connection (which may find new credentials).
            McpRequestOutcome::Completed(Err(McpError::AuthRequired(reason))) => {
                self.disconnect_if_generation(entry, generation, connection)
                    .await;
                Self::set_status(
                    entry,
                    McpServerStatus::NeedsAuth {
                        reason: reason.clone(),
                    },
                );
                self.bump();
                McpRequestOutcome::Completed(Err(McpError::AuthRequired(reason)))
            }
            result => result,
        }
    }

    /// `/mcp` test action: connect and report the tool count.
    pub async fn test(&self, name: &str) -> Result<usize, McpError> {
        Ok(self.list_tools(name).await?.len())
    }

    pub async fn reconnect(&self, name: &str) -> Result<(), McpError> {
        let entry = self.entry(name)?;
        self.reconnect_entry(&entry).await.map(|_| ())
    }

    pub async fn disconnect(&self, name: &str) -> Result<(), McpError> {
        let entry = self.entry(name)?;
        entry.generation.fetch_add(1, Ordering::AcqRel);
        let taken = {
            let mut slot = entry.connection.lock().await;
            slot.take()
        };
        if let Some(connection) = &taken {
            connection.end_session().await;
        }
        drop_detached(taken);
        Self::set_status(&entry, entry.idle_status());
        self.bump();
        Ok(())
    }

    pub fn remove(&self, name: &str) -> bool {
        let removed = write_lock(&self.servers).remove(name);
        if let Some(entry) = removed {
            entry.retire();
            drop_detached(entry);
            self.bump();
            true
        } else {
            false
        }
    }

    /// Live-add or replace one server (from `/mcp add` or config edits). An
    /// identical spec keeps its warm connection.
    pub fn upsert(&self, spec: McpServerSpec) {
        let replaced;
        {
            let mut servers = write_lock(&self.servers);
            if servers
                .get(&spec.name)
                .is_some_and(|existing| existing.spec == spec)
            {
                return;
            }
            replaced = servers.insert(spec.name.clone(), Arc::new(ServerEntry::new(spec)));
        }
        if let Some(old) = replaced {
            old.retire();
            drop_detached(old);
        }
        self.bump();
    }

    /// Drops every connection; configured servers stay listed as
    /// `Disconnected`. Called on shutdown and usable to force laziness.
    /// Entries mid-handshake self-abort via the generation check in
    /// `ensure_connected` instead of publishing a connection.
    pub async fn disconnect_all(&self) {
        // Background connects of this session stop with it.
        if let Some(cancellation) = lock_mutex(&self.startup_cancel).take() {
            cancellation.cancel();
        }
        let entries: Vec<Arc<ServerEntry>> = read_lock(&self.servers).values().cloned().collect();
        let mut taken_connections = Vec::new();
        for entry in entries {
            entry.generation.fetch_add(1, Ordering::AcqRel);
            let taken = entry
                .connection
                .try_lock()
                .ok()
                .and_then(|mut slot| slot.take());
            taken_connections.extend(taken);
            Self::set_status(&entry, entry.idle_status());
        }
        lock_mutex(&self.tool_call_cache).clear();
        self.bump();
        // once: each is bounded to a second, so shutdown waits for the
        // slowest rather than the sum.
        futures_util::future::join_all(
            taken_connections
                .iter()
                .map(|connection| connection.end_session()),
        )
        .await;
        drop_detached(taken_connections);
    }

    async fn ensure_connected_cancellable(
        &self,
        entry: &Arc<ServerEntry>,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<(Arc<dyn McpConnection>, u64)> {
        if cancellation.is_cancelled() {
            return McpRequestOutcome::cancelled_before_send();
        }
        if !entry.spec.enabled {
            Self::set_status(entry, McpServerStatus::Disabled);
            self.bump();
            return McpRequestOutcome::Completed(Err(McpError::Disabled(entry.spec.name.clone())));
        }
        if let Some(block) = entry.spec.options.block.as_ref() {
            Self::set_status(entry, idle_status(&entry.spec));
            self.bump();
            return McpRequestOutcome::Completed(Err(McpError::Blocked(blocked_message(
                &entry.spec.name,
                block,
            ))));
        }
        let mut slot = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return McpRequestOutcome::cancelled_before_send();
            }
            slot = entry.connection.lock() => slot,
        };
        if cancellation.is_cancelled() {
            return McpRequestOutcome::cancelled_before_send();
        }
        let mut generation = entry.generation.load(Ordering::Acquire);
        if let Some(connection) = slot.as_ref() {
            if !connection.is_closed() {
                return McpRequestOutcome::Completed(Ok((Arc::clone(connection), generation)));
            }
            drop_detached(slot.take());
            entry.generation.fetch_add(1, Ordering::AcqRel);
            generation = entry.generation.load(Ordering::Acquire);
        }
        Self::set_status(entry, McpServerStatus::Connecting);
        self.bump();
        match connect_cancellable(
            &entry.spec,
            &self.cwd,
            &self.resolver,
            self.log.clone(),
            cancellation.clone(),
        )
        .await
        {
            McpRequestOutcome::Completed(Ok((connection, tools, handshake))) => {
                if entry.generation.load(Ordering::Acquire) != generation {
                    let cleanup = connection.close_for_cleanup().await;
                    return McpRequestOutcome::InterruptedBeforeSend {
                        interruption: McpInterruption::ConnectionClosed,
                        cleanup,
                    };
                }
                if cancellation.is_cancelled() {
                    let cleanup = connection.close_for_cleanup().await;
                    if entry.generation.load(Ordering::Acquire) == generation {
                        Self::set_status(entry, McpServerStatus::Disconnected);
                        self.bump();
                    }
                    return McpRequestOutcome::InterruptedBeforeSend {
                        interruption: McpInterruption::Cancelled,
                        cleanup,
                    };
                }
                Self::set_status(entry, McpServerStatus::Ready { tools });
                *write_lock(&entry.handshake) = Some(handshake);
                *slot = Some(Arc::clone(&connection));
                self.bump();
                McpRequestOutcome::Completed(Ok((connection, generation)))
            }
            McpRequestOutcome::Completed(Err(error)) => {
                if entry.generation.load(Ordering::Acquire) == generation {
                    Self::set_status(entry, failure_status(&error));
                    self.bump();
                }
                McpRequestOutcome::Completed(Err(error))
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                if entry.generation.load(Ordering::Acquire) == generation {
                    Self::set_status(entry, McpServerStatus::Disconnected);
                    self.bump();
                }
                McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                }
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                if entry.generation.load(Ordering::Acquire) == generation {
                    Self::set_status(entry, McpServerStatus::Disconnected);
                    self.bump();
                }
                // Discovery retains handshake uncertainty. The call path
                // separately records that tools/call has not been sent yet.
                McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                }
            }
        }
    }

    async fn disconnect_if_generation(
        &self,
        entry: &Arc<ServerEntry>,
        generation: u64,
        connection: &Arc<dyn McpConnection>,
    ) {
        let mut slot = entry.connection.lock().await;
        if entry.generation.load(Ordering::Acquire) != generation
            || !slot
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, connection))
        {
            return;
        }
        let taken = slot.take();
        entry.generation.fetch_add(1, Ordering::AcqRel);
        Self::set_status(entry, entry.idle_status());
        self.bump();
        drop(slot);
        drop_detached(taken);
    }

    async fn reconnect_entry(
        &self,
        entry: &Arc<ServerEntry>,
    ) -> Result<Arc<dyn McpConnection>, McpError> {
        {
            let mut slot = entry.connection.lock().await;
            drop_detached(slot.take());
            entry.generation.fetch_add(1, Ordering::AcqRel);
        }
        Self::set_status(entry, entry.idle_status());
        self.ensure_connected_cancellable(entry, McpCancellation::new())
            .await
            .map(|(connection, _)| connection)
            .into_result()
    }
}

impl McpManager {
    /// OAuth state machine of an HTTP server that signs in with OAuth; the
    /// host drives `/mcp login` and `/mcp logout` through it.
    pub fn auth_handle(&self, server: &str) -> Option<Arc<McpAuth>> {
        self.entry(server)
            .ok()?
            .spec
            .options
            .auth
            .as_ref()
            .map(|handle| Arc::clone(&handle.0))
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        // Dropping each entry drops its connection, whose Drop terminates the
        // child process tree; no async needed.
        write_lock(&self.servers).clear();
    }
}

impl ServerEntry {
    fn new(spec: McpServerSpec) -> Self {
        Self {
            status: RwLock::new(idle_status(&spec)),
            handshake: RwLock::new(None),
            last_tools: RwLock::new(None),
            connection: tokio::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
            startup_pending: AtomicBool::new(false),
            spec,
        }
    }

    /// Whether session start should connect this server in the background.
    fn wants_startup_connect(&self) -> bool {
        self.spec.enabled
            && self.spec.options.block.is_none()
            && !self.spec.options.lazy
            && !self.spec.options.fully_hidden()
            && matches!(*read_lock(&self.status), McpServerStatus::Disconnected)
    }

    /// An entry that left the server set: a connect still in flight for it
    /// must not publish its connection.
    fn retire(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Resting status of an entry without a live connection.
    fn idle_status(&self) -> McpServerStatus {
        idle_status(&self.spec)
    }
}

/// Status after a failed connect: a server that needs sign-in is not just
/// broken.
fn failure_status(error: &McpError) -> McpServerStatus {
    match error {
        McpError::AuthRequired(reason) => McpServerStatus::NeedsAuth {
            reason: reason.clone(),
        },
        other => McpServerStatus::Failed {
            error: other.to_string(),
        },
    }
}

fn idle_status(spec: &McpServerSpec) -> McpServerStatus {
    if !spec.enabled {
        return McpServerStatus::Disabled;
    }
    match &spec.options.block {
        Some(McpServerBlock::Untrusted) => McpServerStatus::Untrusted,
        Some(McpServerBlock::Invalid(error)) => McpServerStatus::Failed {
            error: error.clone(),
        },
        None => McpServerStatus::Disconnected,
    }
}

fn render_tools_page(tools: &[McpToolSummary], offset: usize) -> String {
    let mut lines = Vec::new();
    for tool in tools.iter().skip(offset).take(MAX_LIST_TOOLS_PER_SERVER) {
        match &tool.description {
            Some(description) => {
                let short = description.lines().next().unwrap_or("");
                let short = if short.chars().count() > 80 {
                    format!("{}…", short.chars().take(80).collect::<String>())
                } else {
                    short.to_owned()
                };
                lines.push(format!("{} — {}", tool.name, short));
            }
            None => lines.push(tool.name.clone()),
        }
    }
    let next_offset = offset.saturating_add(lines.len());
    let remaining = tools.len().saturating_sub(next_offset);
    if remaining > 0 {
        lines.push(format!(
            "… {remaining} more tools (call again with \"offset\": {next_offset})"
        ));
    }
    if lines.is_empty() {
        lines.push(if offset == 0 {
            "(no tools)".to_owned()
        } else {
            format!("(no tools at offset {offset}; {} total)", tools.len())
        });
    }
    lines.join("\n")
}

fn render_tool_schema(tool: &McpToolSummary) -> Result<String, McpError> {
    let rendered = match &tool.output_schema {
        Some(output) => serde_json::to_string_pretty(&json!({
            "inputSchema": tool.schema, "outputSchema": output,
        }))?,
        None => serde_json::to_string_pretty(&tool.schema)?,
    };
    Ok(if rendered.len() > MAX_DESCRIBE_BYTES {
        let mut end = MAX_DESCRIBE_BYTES;
        while !rendered.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…(truncated)", &rendered[..end])
    } else {
        rendered
    })
}

fn blocked_message(name: &str, block: &McpServerBlock) -> String {
    match block {
        McpServerBlock::Untrusted => format!(
            "MCP server {name} is defined by the project configuration, which is not trusted; \
             trust the project (/mcp trust, or --trust-project for one headless run) to start it"
        ),
        McpServerBlock::Invalid(error) => format!("MCP server {name} is misconfigured: {error}"),
    }
}

/// Working directory for a stdio server: the configured `cwd` (relative to
/// the workspace root) or the workspace root itself. Must be an existing
/// directory so a typo reports a clear error instead of an OS spawn failure.
fn server_cwd(spec: &McpServerSpec, workspace: &Path) -> Result<PathBuf, McpError> {
    let Some(configured) = spec.options.cwd.as_ref() else {
        return Ok(workspace.to_path_buf());
    };
    let resolved = if configured.is_absolute() {
        configured.clone()
    } else {
        workspace.join(configured)
    };
    if !resolved.is_dir() {
        return Err(McpError::Blocked(format!(
            "MCP server {} cwd is not a directory: {}",
            spec.name,
            resolved.display()
        )));
    }
    Ok(resolved)
}

type ConnectedServer = (
    Arc<dyn McpConnection>,
    Arc<Vec<McpToolSummary>>,
    Arc<McpServerHandshake>,
);

/// One connect with the retry policy: an HTTP server that fails transiently
/// (network error, 408, 429, 5xx except 501) is tried again after 250 ms and
/// after 1 s. Anything else, and every stdio failure, is final. The handshake
/// requests are all idempotent, so a retry cannot repeat a side effect.
async fn connect_cancellable(
    spec: &McpServerSpec,
    cwd: &Path,
    resolver: &ExecutableResolver,
    log: Option<Arc<McpLog>>,
    cancellation: McpCancellation,
) -> McpRequestOutcome<ConnectedServer> {
    let retries: &[Duration] = match spec.transport {
        McpTransport::Http { .. } => &CONNECT_RETRY_DELAYS,
        McpTransport::Stdio { .. } => &[],
    };
    let mut attempt = 0;
    loop {
        let outcome =
            connect_once_cancellable(spec, cwd, resolver, log.clone(), cancellation.clone()).await;
        let McpRequestOutcome::Completed(Err(error)) = &outcome else {
            return outcome;
        };
        let Some(delay) = retries.get(attempt).filter(|_| is_transient_error(error)) else {
            return outcome;
        };
        attempt += 1;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return McpRequestOutcome::cancelled_before_send();
            }
            () = tokio::time::sleep(*delay) => {}
        }
    }
}

async fn connect_once_cancellable(
    spec: &McpServerSpec,
    cwd: &Path,
    resolver: &ExecutableResolver,
    log: Option<Arc<McpLog>>,
    cancellation: McpCancellation,
) -> McpRequestOutcome<ConnectedServer> {
    // What the client offers back to the server: the workspace root for
    // `roots/list`, `ping`, and the log sink for `notifications/message`.
    let context = ClientContext::new(&spec.name, cwd, log);
    let connection: Arc<dyn McpConnection> = match &spec.transport {
        McpTransport::Stdio { command, args, env } => {
            let server_cwd = match server_cwd(spec, cwd) {
                Ok(path) => path,
                Err(error) => return McpRequestOutcome::Completed(Err(error)),
            };
            match StdioConnection::spawn(
                command,
                args,
                env,
                &server_cwd,
                spec.timeout,
                resolver,
                context,
            ) {
                Ok(connection) => connection,
                Err(error) => return McpRequestOutcome::Completed(Err(error)),
            }
        }
        McpTransport::Http { url, headers } => {
            match HttpConnection::new(url.clone(), headers.clone(), spec.timeout) {
                Ok(connection) => {
                    let connection = connection.with_context(context);
                    match &spec.options.auth {
                        Some(handle) => Arc::new(connection.with_auth(Arc::clone(&handle.0))),
                        None => Arc::new(connection),
                    }
                }
                Err(error) => return McpRequestOutcome::Completed(Err(error)),
            }
        }
    };
    let initialized = connection
        .request_cancellable(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {"roots": {}},
                "clientInfo": {"name": "slim", "version": env!("CARGO_PKG_VERSION")},
            }),
            cancellation.clone(),
        )
        .await;
    let result = match close_after_connect_step(
        connection.as_ref(),
        initialized,
        ConnectStep::Request,
    )
    .await
    {
        Ok(result) => result,
        Err(outcome) => return outcome,
    };
    let handshake = match McpServerHandshake::from_initialize(&result) {
        Ok(handshake) => Arc::new(handshake),
        Err(error) => {
            let _ = connection.close_for_cleanup().await;
            return McpRequestOutcome::Completed(Err(error));
        }
    };
    // The negotiated revision is what later HTTP requests announce.
    connection.set_protocol_version(&handshake.protocol_version);
    if cancellation.is_cancelled() {
        let cleanup = connection.close_for_cleanup().await;
        return McpRequestOutcome::InterruptedBeforeSend {
            interruption: McpInterruption::Cancelled,
            cleanup,
        };
    }
    let notified = connection
        .notify_cancellable("notifications/initialized", json!({}), cancellation.clone())
        .await;
    if let Err(outcome) =
        close_after_connect_step(connection.as_ref(), notified, ConnectStep::Notification).await
    {
        return outcome;
    }
    // A server that does not declare the tools capability does not answer
    // `tools/list`; skipping it keeps resource-only servers connectable.
    let tools = if handshake.has_tools() {
        fetch_tools_pages_cancellable(connection.as_ref(), cancellation.clone()).await
    } else {
        McpRequestOutcome::Completed(Ok(Arc::new(Vec::new())))
    };
    let tools =
        match close_after_connect_step(connection.as_ref(), tools, ConnectStep::Request).await {
            Ok(tools) => tools,
            Err(outcome) => return outcome,
        };
    if cancellation.is_cancelled() {
        let cleanup = connection.close_for_cleanup().await;
        return McpRequestOutcome::InterruptedBeforeSend {
            interruption: McpInterruption::Cancelled,
            cleanup,
        };
    }
    McpRequestOutcome::Completed(Ok((connection, tools, handshake)))
}

/// Where in the connect handshake an outcome came from; it decides how an
/// uncertain outcome is classified.
#[derive(Clone, Copy)]
enum ConnectStep {
    /// `initialize` and `tools/list`: a server closing the connection mid-request
    /// is a failed connect, and any other cut-short request stays uncertain.
    Request,
    /// `notifications/initialized`: the tool the user asked for was not
    /// admitted yet, so an uncertain outcome counts as interrupted before send.
    Notification,
}

/// The value of a handshake step, or the connect outcome after closing the
/// connection when the step failed or was interrupted.
async fn close_after_connect_step<T, U>(
    connection: &dyn McpConnection,
    outcome: McpRequestOutcome<T>,
    step: ConnectStep,
) -> Result<T, McpRequestOutcome<U>> {
    match outcome {
        McpRequestOutcome::Completed(Ok(value)) => Ok(value),
        McpRequestOutcome::Completed(Err(error)) => {
            let _ = connection.close_for_cleanup().await;
            Err(McpRequestOutcome::Completed(Err(error)))
        }
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => {
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            Err(McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            })
        }
        McpRequestOutcome::OutcomeUncertain {
            interruption,
            cleanup,
        } => {
            let closed = match step {
                ConnectStep::Request => handshake_closed(connection, &interruption).await,
                ConnectStep::Notification => None,
            };
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            if let Some(error) = closed {
                return Err(McpRequestOutcome::Completed(Err(error)));
            }
            Err(match step {
                ConnectStep::Request => McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                },
                ConnectStep::Notification => McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                },
            })
        }
    }
}

/// A handshake request cut short by the server closing the connection is a
/// failed connect, not an unknown outcome: nothing the user asked for was
/// sent. The error carries the transport's own diagnostics (the stderr tail
/// of a stdio server that crashed on startup). Cancellation and timeouts keep
/// their classification.
async fn handshake_closed(
    connection: &dyn McpConnection,
    interruption: &McpInterruption,
) -> Option<McpError> {
    match interruption {
        McpInterruption::ConnectionClosed => connection.closed_reason().await,
        _ => None,
    }
}

/// Paginated `tools/list`: pages stop at `nextCursor` exhaustion (absent, null
/// or empty), a cursor the server already handed out, `MAX_TOOLS_PAGES`, or
/// `MAX_TOOLS_PER_SERVER`. A repeated cursor keeps the tools collected so far.
async fn fetch_tools_pages_cancellable(
    connection: &dyn McpConnection,
    cancellation: McpCancellation,
) -> McpRequestOutcome<Arc<Vec<McpToolSummary>>> {
    let mut summaries = Vec::new();
    let mut cursor = Value::Null;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..MAX_TOOLS_PAGES {
        if cancellation.is_cancelled() {
            return McpRequestOutcome::cancelled_before_send();
        }
        let params = if cursor.is_null() {
            json!({})
        } else {
            json!({"cursor": cursor})
        };
        let response = match connection
            .request_cancellable("tools/list", params, cancellation.clone())
            .await
            .pass()
        {
            Ok(response) => response,
            Err(outcome) => return outcome,
        };
        let Some(tools) = response.get("tools").and_then(Value::as_array) else {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(
                "tools/list missing tools array".into(),
            )));
        };
        for tool in tools {
            if summaries.len() >= MAX_TOOLS_PER_SERVER {
                break;
            }
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            summaries.push(McpToolSummary {
                name: name.to_owned(),
                output_schema: tool.get("outputSchema").cloned(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
                read_only_hint: tool
                    .get("annotations")
                    .and_then(|annotations| annotations.get("readOnlyHint"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                destructive_hint: tool
                    .get("annotations")
                    .and_then(|annotations| annotations.get("destructiveHint"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                idempotent_hint: tool
                    .get("annotations")
                    .and_then(|annotations| annotations.get("idempotentHint"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
        // Some servers end pagination with `null` or `""`.
        match response
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|next| !next.is_empty())
        {
            Some(next)
                if summaries.len() < MAX_TOOLS_PER_SERVER
                    && seen_cursors.insert(next.to_owned()) =>
            {
                cursor = Value::String(next.to_owned());
            }
            _ => break,
        }
    }
    McpRequestOutcome::Completed(Ok(Arc::new(summaries)))
}

fn canonical_json_string(value: &Value) -> String {
    fn sort_value(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut sorted = serde_json::Map::with_capacity(map.len());
                for k in keys {
                    if let Some(val) = map.get(k) {
                        sorted.insert(k.clone(), sort_value(val));
                    }
                }
                Value::Object(sorted)
            }
            Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
            other => other.clone(),
        }
    }
    sort_value(value).to_string()
}

fn merge_cleanup(first: McpCleanupStatus, second: McpCleanupStatus) -> McpCleanupStatus {
    match (first, second) {
        (_, McpCleanupStatus::Confirmed) | (McpCleanupStatus::Confirmed, _) => {
            McpCleanupStatus::Confirmed
        }
        (McpCleanupStatus::Unconfirmed, _) | (_, McpCleanupStatus::Unconfirmed) => {
            McpCleanupStatus::Unconfirmed
        }
        _ => McpCleanupStatus::NotRequired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::oauth::{McpOAuthState, McpOAuthStore, MemoryOAuthStore, OAuthTokens};
    use crate::mcp::McpAuthHandle;
    use crate::mcp::McpOAuthSpec;

    #[tokio::test]
    async fn the_log_redacts_oauth_tokens_obtained_after_the_manager_was_built() {
        let url = "https://mcp.example.com/rpc";
        let store = Arc::new(MemoryOAuthStore::new());
        let auth = McpAuth::new("web", url, McpOAuthSpec::default(), store.clone()).unwrap();
        let mut spec = McpServerSpec::new(
            "web",
            McpTransport::Http {
                url: url.into(),
                headers: BTreeMap::new(),
            },
        );
        spec.options.auth = Some(McpAuthHandle(auth.clone()));
        let directory = std::env::temp_dir().join(format!(
            "slim-mcp-log-secrets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("mcp.log");
        let manager = McpManager::new(
            BTreeMap::from([("web".to_owned(), spec)]),
            PathBuf::from("."),
            ExecutableResolver::default(),
        )
        .with_log_path(path.clone());
        assert!(manager.sensitive_values().is_empty());

        // Sign-in happens after the manager exists (another process stored
        // the credentials; the next request adopts them).
        store
            .save(&McpOAuthState {
                server_url: auth.server_url().to_owned(),
                tokens: Some(OAuthTokens {
                    access_token: "late-access-token".into(),
                    refresh_token: Some("late-refresh-token".into()),
                    scope: None,
                    expires_at_ms: None,
                }),
                ..McpOAuthState::default()
            })
            .unwrap();
        auth.prepare().await;
        assert!(manager
            .sensitive_values()
            .contains(&"late-access-token".to_owned()));

        manager.log.as_ref().expect("log").write(
            "web",
            &json!({"data": "Authorization: Bearer late-access-token / late-refresh-token"}),
        );
        let written = std::fs::read_to_string(&path).expect("log written");
        let _ = std::fs::remove_dir_all(&directory);
        assert!(!written.contains("late-"), "{written}");
        assert!(written.contains("[REDACTED]"), "{written}");
    }
}
