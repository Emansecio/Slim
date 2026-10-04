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
    servers
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
/// Nothing is spawned or connected at construction: the first `list_tools`,
/// `describe`, or `call` (or a `/mcp` test) performs the handshake. All state
/// transitions bump `revision` so UIs can poll cheaply.
pub struct McpManager {
    cwd: PathBuf,
    resolver: ExecutableResolver,
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
            servers: Arc::new(RwLock::new(servers)),
            revision: AtomicU64::new(0),
            log: None,
            changed: tokio::sync::Notify::new(),
            startup_cancel: Mutex::new(None),
            startup_wait_ms: AtomicU64::new(DEFAULT_MCP_STARTUP_WAIT.as_millis() as u64),
            startup_wait_spent: AtomicBool::new(false),
            direct: Mutex::new(Default::default()),
            resource_cache: resources::ResourceCache::default(),
        }
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
        self.servers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(entry.spec.name.clone(), entry);
        self.bump();
    }

    pub fn is_empty(&self) -> bool {
        self.servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }

    pub fn has_enabled_servers(&self) -> bool {
        self.servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|entry| entry.spec.enabled)
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    /// Configured header/env values across all servers; callers register them
    /// for redaction so secrets never reach prompts, events, or logs.
    pub fn sensitive_values(&self) -> Vec<String> {
        collect_sensitive_values(&self.servers)
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
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return 0;
        };
        let cancellation = {
            let mut slot = self
                .startup_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slot.as_ref() {
                Some(existing) if !existing.is_cancelled() => existing.clone(),
                _ => {
                    let fresh = McpCancellation::new();
                    *slot = Some(fresh.clone());
                    fresh
                }
            }
        };
        let entries: Vec<Arc<ServerEntry>> = self
            .servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        let mut started = 0;
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
        self.servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|entry| entry.startup_pending.load(Ordering::Acquire))
    }

    /// Direct-exposure servers whose background connect has not ended.
    fn pending_direct_servers(&self) -> Vec<String> {
        self.servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|entry| {
                entry.spec.options.has_direct_tools()
                    && entry.startup_pending.load(Ordering::Acquire)
            })
            .map(|entry| entry.spec.name.clone())
            .collect()
    }

    /// Waits until the direct-exposure servers started by
    /// [`Self::start_background_connect`] have connected or failed, at most
    /// `timeout`. Only the first call per session waits: after it (finished
    /// or timed out) later calls return at once, so a slow server delays one
    /// run, not every run. Gateway servers are never waited for here; a call
    /// that names one waits for that server alone.
    pub async fn wait_for_direct_servers(&self, timeout: Duration) -> McpStartupWait {
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
        self.servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(name.to_owned()))
    }

    fn set_status(entry: &ServerEntry, status: McpServerStatus) {
        if let McpServerStatus::Ready { tools } = &status {
            *entry
                .last_tools
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(tools));
        }
        // What the server reported belongs to the live connection only.
        if !matches!(status, McpServerStatus::Ready { .. }) {
            *entry
                .handshake
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
        *entry
            .status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
    }

    /// What the connected server reported in `initialize` (protocol version,
    /// identity, capabilities, instructions); `None` while not connected.
    pub fn handshake(&self, server: &str) -> Option<Arc<McpServerHandshake>> {
        self.entry(server).ok().and_then(|entry| {
            entry
                .handshake
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
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
        let mut replaced: Vec<Arc<ServerEntry>> = Vec::new();
        {
            let mut servers = self
                .servers
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        for old in &replaced {
            old.retire();
        }
        drop_detached(replaced);
        self.bump();
    }

    pub fn statuses(&self) -> Vec<McpServerInfo> {
        self.servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|entry| McpServerInfo {
                name: entry.spec.name.clone(),
                transport: entry.spec.transport.kind(),
                target: entry.spec.transport.target(),
                enabled: entry.spec.enabled,
                description: entry.spec.options.description.clone(),
                exposure: entry.spec.options.exposure,
                status: entry
                    .status
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
                handshake: entry
                    .handshake
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
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
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        let entry = match self.entry(server) {
            Ok(entry) => entry,
            Err(error) => return McpRequestOutcome::Completed(Err(error)),
        };
        let (connection, generation) = match self
            .ensure_connected_cancellable(&entry, cancellation.clone())
            .await
        {
            McpRequestOutcome::Completed(Ok(connection)) => connection,
            McpRequestOutcome::Completed(Err(error)) => {
                return McpRequestOutcome::Completed(Err(error));
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                return McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                };
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                return McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                };
            }
        };
        let stale = connection.take_tools_stale();
        let cached = entry
            .status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
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
                match self
                    .ensure_connected_cancellable(&entry, cancellation)
                    .await
                {
                    McpRequestOutcome::Completed(Ok((reconnected, reconnected_generation))) => {
                        let slot = entry.connection.lock().await;
                        if entry.generation.load(Ordering::Acquire) == reconnected_generation
                            && slot
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &reconnected))
                        {
                            match &*entry
                                .status
                                .read()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                            {
                                McpServerStatus::Ready { tools } => {
                                    McpRequestOutcome::Completed(Ok(Arc::clone(tools)))
                                }
                                _ => McpRequestOutcome::Completed(Err(McpError::Closed)),
                            }
                        } else {
                            McpRequestOutcome::Completed(Err(McpError::Closed))
                        }
                    }
                    McpRequestOutcome::Completed(Err(reconnect_error)) => {
                        McpRequestOutcome::Completed(Err(reconnect_error))
                    }
                    McpRequestOutcome::InterruptedBeforeSend {
                        interruption,
                        cleanup,
                    } => McpRequestOutcome::InterruptedBeforeSend {
                        interruption,
                        cleanup,
                    },
                    McpRequestOutcome::OutcomeUncertain {
                        interruption,
                        cleanup,
                    } => McpRequestOutcome::OutcomeUncertain {
                        interruption,
                        cleanup,
                    },
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
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                if stale {
                    connection.mark_tools_stale();
                }
                McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                }
            }
            McpRequestOutcome::Completed(Err(error)) => {
                if stale {
                    connection.mark_tools_stale();
                }
                McpRequestOutcome::Completed(Err(error))
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
        let tools = match self.list_tools_cancellable(server, cancellation).await {
            McpRequestOutcome::Completed(Ok(tools)) => tools,
            McpRequestOutcome::Completed(Err(error)) => {
                return McpRequestOutcome::Completed(Err(error));
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                return McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                };
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                return McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                };
            }
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
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        if !arguments.is_object() {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(
                "MCP tool arguments must be an object".into(),
            )));
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
            McpRequestOutcome::Completed(Ok(connection)) => connection,
            McpRequestOutcome::Completed(Err(error)) => {
                return McpRequestOutcome::Completed(Err(error));
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                return McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                };
            }
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
        };

        if cancellation.is_cancelled() {
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        let progress = progress.unwrap_or_else(|| Arc::new(|_| {}));
        let outcome = connection
            .request_with_progress(
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                cancellation,
                Some(progress),
            )
            .await;
        match outcome {
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
            McpRequestOutcome::Completed(Err(error)) if connection.is_closed() => {
                let _ = connection.close_for_cleanup().await;
                self.disconnect_if_generation(&entry, generation, &connection)
                    .await;
                McpRequestOutcome::Completed(Err(error))
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
            // The server rejected the call before running it and the user
            // has to sign in: the entry shows it and the next use starts a
            // fresh connection (which may find new credentials).
            McpRequestOutcome::Completed(Err(McpError::AuthRequired(reason))) => {
                self.disconnect_if_generation(&entry, generation, &connection)
                    .await;
                Self::set_status(
                    &entry,
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
        let removed = self
            .servers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(name);
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
            let mut servers = self
                .servers
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        if let Some(cancellation) = self
            .startup_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            cancellation.cancel();
        }
        let entries: Vec<Arc<ServerEntry>> = self
            .servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
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
        self.bump();
        // HTTP sessions are ended before the connections go away, all at
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
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
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
                return McpRequestOutcome::InterruptedBeforeSend {
                    interruption: McpInterruption::Cancelled,
                    cleanup: McpCleanupStatus::NotRequired,
                };
            }
            slot = entry.connection.lock() => slot,
        };
        if cancellation.is_cancelled() {
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
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
                *entry
                    .handshake
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handshake);
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
        self.servers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
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
            && matches!(
                *self
                    .status
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                McpServerStatus::Disconnected
            )
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
                return McpRequestOutcome::InterruptedBeforeSend {
                    interruption: McpInterruption::Cancelled,
                    cleanup: McpCleanupStatus::NotRequired,
                };
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
    let handshake = match initialized {
        McpRequestOutcome::Completed(Ok(result)) => {
            match McpServerHandshake::from_initialize(&result) {
                Ok(handshake) => Arc::new(handshake),
                Err(error) => {
                    let _ = connection.close_for_cleanup().await;
                    return McpRequestOutcome::Completed(Err(error));
                }
            }
        }
        McpRequestOutcome::Completed(Err(error)) => {
            let _ = connection.close_for_cleanup().await;
            return McpRequestOutcome::Completed(Err(error));
        }
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => {
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            };
        }
        McpRequestOutcome::OutcomeUncertain {
            interruption,
            cleanup,
        } => {
            let closed = handshake_closed(connection.as_ref(), &interruption).await;
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            if let Some(error) = closed {
                return McpRequestOutcome::Completed(Err(error));
            }
            return McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            };
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
    match connection
        .notify_cancellable("notifications/initialized", json!({}), cancellation.clone())
        .await
    {
        McpRequestOutcome::Completed(Ok(())) => {}
        McpRequestOutcome::Completed(Err(error)) => {
            let _ = connection.close_for_cleanup().await;
            return McpRequestOutcome::Completed(Err(error));
        }
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => {
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            };
        }
        McpRequestOutcome::OutcomeUncertain {
            interruption,
            cleanup,
        } => {
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            };
        }
    }
    // A server that does not declare the tools capability does not answer
    // `tools/list`; skipping it keeps resource-only servers connectable.
    let tools = if handshake.has_tools() {
        fetch_tools_pages_cancellable(connection.as_ref(), cancellation.clone()).await
    } else {
        McpRequestOutcome::Completed(Ok(Arc::new(Vec::new())))
    };
    match tools {
        McpRequestOutcome::Completed(Ok(_tools)) if cancellation.is_cancelled() => {
            let cleanup = connection.close_for_cleanup().await;
            McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup,
            }
        }
        McpRequestOutcome::Completed(Ok(tools)) => {
            McpRequestOutcome::Completed(Ok((connection, tools, handshake)))
        }
        McpRequestOutcome::Completed(Err(error)) => {
            let _ = connection.close_for_cleanup().await;
            McpRequestOutcome::Completed(Err(error))
        }
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => {
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            }
        }
        McpRequestOutcome::OutcomeUncertain {
            interruption,
            cleanup,
        } => {
            let closed = handshake_closed(connection.as_ref(), &interruption).await;
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            if let Some(error) = closed {
                return McpRequestOutcome::Completed(Err(error));
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            }
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
            return McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            };
        }
        let params = if cursor.is_null() {
            json!({})
        } else {
            json!({"cursor": cursor})
        };
        let response = match connection
            .request_cancellable("tools/list", params, cancellation.clone())
            .await
        {
            McpRequestOutcome::Completed(Ok(response)) => response,
            McpRequestOutcome::Completed(Err(error)) => {
                return McpRequestOutcome::Completed(Err(error));
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                return McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                };
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                return McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                };
            }
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
