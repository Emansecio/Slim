use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};

use crate::mcp::http::HttpConnection;
use crate::mcp::spec::{
    McpCancellation, McpCleanupStatus, McpConnection, McpError, McpInterruption, McpRequestOutcome,
    McpServerInfo, McpServerSpec, McpServerStatus, McpToolSummary, McpTransport,
    MCP_PROTOCOL_VERSION,
};
use crate::mcp::stdio::StdioConnection;
use crate::process::ExecutableResolver;

const MAX_TOOLS_PER_SERVER: usize = 256;
const MAX_LIST_SERVERS: usize = 64;
const MAX_LIST_TOOLS_PER_SERVER: usize = 32;
const MAX_DESCRIBE_BYTES: usize = 16 * 1024;
const MAX_TOOLS_PAGES: usize = 8;

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
    servers: RwLock<BTreeMap<String, Arc<ServerEntry>>>,
    revision: AtomicU64,
}

struct ServerEntry {
    spec: McpServerSpec,
    status: RwLock<McpServerStatus>,
    connection: tokio::sync::Mutex<Option<Arc<dyn McpConnection>>>,
    generation: AtomicU64,
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
            servers: RwLock::new(servers),
            revision: AtomicU64::new(0),
        }
    }

    /// Test hook: registers a server backed by a prebuilt connection so the
    /// agent loop can be exercised without a real process or socket.
    pub fn insert_connection(
        &self,
        spec: McpServerSpec,
        connection: Arc<dyn McpConnection>,
        tools: Vec<McpToolSummary>,
    ) {
        let entry = Arc::new(ServerEntry {
            status: RwLock::new(McpServerStatus::Ready {
                tools: Arc::new(tools),
            }),
            connection: tokio::sync::Mutex::new(Some(connection)),
            generation: AtomicU64::new(0),
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
        self.servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .flat_map(|entry| {
                entry
                    .spec
                    .transport
                    .sensitive_values()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
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
        *entry
            .status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
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
                status: entry
                    .status
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            })
            .collect()
    }

    /// Server list for the model: names, transport and status only. Never
    /// connects — tool discovery happens per server via [`Self::list_tools`].
    pub fn list_servers(&self) -> String {
        let servers = self
            .servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if servers.is_empty() {
            return "no MCP servers configured".to_owned();
        }
        let mut lines = Vec::new();
        for (index, entry) in servers.values().enumerate() {
            if index >= MAX_LIST_SERVERS {
                lines.push(format!("… {} more servers", servers.len() - index));
                break;
            }
            let status = entry
                .status
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = match &*status {
                McpServerStatus::Disabled => "disabled".to_owned(),
                McpServerStatus::Disconnected => "disconnected".to_owned(),
                McpServerStatus::Connecting => "connecting".to_owned(),
                McpServerStatus::Ready { tools } => format!("ready, {} tools", tools.len()),
                McpServerStatus::Failed { error } => format!("failed: {error}"),
            };
            lines.push(format!(
                "{} [{}] {}",
                entry.spec.name,
                entry.spec.transport.kind(),
                state
            ));
        }
        lines.join("\n")
    }

    /// Connects if needed and returns this server's tool summaries.
    pub async fn list_tools(&self, server: &str) -> Result<Arc<Vec<McpToolSummary>>, McpError> {
        let entry = self.entry(server)?;
        let connection = self.ensure_connected(&entry).await?;
        let stale = connection.take_tools_stale();
        let cached = entry
            .status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let McpServerStatus::Ready { tools } = &cached {
            if !stale {
                return Ok(Arc::clone(tools));
            }
        }
        let tools = match self.fetch_tools(&entry).await {
            Ok(tools) => tools,
            Err(error) => {
                if stale {
                    connection.mark_tools_stale();
                }
                return Err(error);
            }
        };
        Self::set_status(
            &entry,
            McpServerStatus::Ready {
                tools: Arc::clone(&tools),
            },
        );
        self.bump();
        Ok(tools)
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
                error @ (McpError::Closed | McpError::Protocol(_)),
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
        let tools = self.list_tools(server).await?;
        Ok(render_tools_page(&tools, offset))
    }

    pub async fn list_tools_text_cancellable(
        &self,
        server: &str,
        offset: usize,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<String> {
        self.list_tools_cancellable(server, cancellation)
            .await
            .map(|tools| render_tools_page(&tools, offset))
    }

    pub async fn describe_cancellable(
        &self,
        server: &str,
        tool: &str,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<String> {
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
        McpRequestOutcome::Completed(render_tool_schema(&tool.schema))
    }

    pub async fn describe(&self, server: &str, tool: &str) -> Result<String, McpError> {
        let tools = self.list_tools(server).await?;
        let tool = tools
            .iter()
            .find(|candidate| candidate.name == tool)
            .ok_or_else(|| McpError::Protocol(format!("unknown tool {tool} on server {server}")))?;
        render_tool_schema(&tool.schema)
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
        let outcome = connection
            .request_cancellable(
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                cancellation,
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
            result => result,
        }
    }

    /// tools/list with reconnect-on-death and `nextCursor` pagination,
    /// bounded by `MAX_TOOLS_PAGES`/`MAX_TOOLS_PER_SERVER`.
    async fn fetch_tools(
        &self,
        entry: &Arc<ServerEntry>,
    ) -> Result<Arc<Vec<McpToolSummary>>, McpError> {
        self.request_with_reconnect(entry, |connection| async move {
            fetch_tools_pages(connection.as_ref()).await
        })
        .await
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
        drop_detached(taken);
        Self::set_status(
            &entry,
            if entry.spec.enabled {
                McpServerStatus::Disconnected
            } else {
                McpServerStatus::Disabled
            },
        );
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
            drop_detached(old);
        }
        self.bump();
    }

    /// Drops every connection; configured servers stay listed as
    /// `Disconnected`. Called on shutdown and usable to force laziness.
    /// Entries mid-handshake self-abort via the generation check in
    /// `ensure_connected` instead of publishing a connection.
    pub async fn disconnect_all(&self) {
        let entries: Vec<Arc<ServerEntry>> = self
            .servers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for entry in entries {
            entry.generation.fetch_add(1, Ordering::AcqRel);
            let taken = entry
                .connection
                .try_lock()
                .ok()
                .and_then(|mut slot| slot.take());
            drop_detached(taken);
            if entry.spec.enabled {
                Self::set_status(&entry, McpServerStatus::Disconnected);
            }
        }
        self.bump();
    }

    async fn ensure_connected(
        &self,
        entry: &Arc<ServerEntry>,
    ) -> Result<Arc<dyn McpConnection>, McpError> {
        if !entry.spec.enabled {
            Self::set_status(entry, McpServerStatus::Disabled);
            self.bump();
            return Err(McpError::Disabled(entry.spec.name.clone()));
        }
        let mut slot = entry.connection.lock().await;
        if let Some(connection) = slot.as_ref() {
            if !connection.is_closed() {
                return Ok(Arc::clone(connection));
            }
            drop_detached(slot.take());
            entry.generation.fetch_add(1, Ordering::AcqRel);
        }
        let generation = entry.generation.load(Ordering::Acquire);
        Self::set_status(entry, McpServerStatus::Connecting);
        self.bump();
        match connect(&entry.spec, &self.cwd, &self.resolver).await {
            Ok((connection, tools)) => {
                if entry.generation.load(Ordering::Acquire) != generation {
                    // disconnect()/disconnect_all() ran during the handshake:
                    // never publish a connection the caller already dropped.
                    drop_detached(connection);
                    return Err(McpError::Closed);
                }
                Self::set_status(entry, McpServerStatus::Ready { tools });
                *slot = Some(Arc::clone(&connection));
                self.bump();
                Ok(connection)
            }
            Err(error) => {
                if entry.generation.load(Ordering::Acquire) == generation {
                    Self::set_status(
                        entry,
                        McpServerStatus::Failed {
                            error: error.to_string(),
                        },
                    );
                    self.bump();
                }
                Err(error)
            }
        }
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
        match connect_cancellable(&entry.spec, &self.cwd, &self.resolver, cancellation.clone())
            .await
        {
            McpRequestOutcome::Completed(Ok((connection, tools))) => {
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
                *slot = Some(Arc::clone(&connection));
                self.bump();
                McpRequestOutcome::Completed(Ok((connection, generation)))
            }
            McpRequestOutcome::Completed(Err(error)) => {
                if entry.generation.load(Ordering::Acquire) == generation {
                    Self::set_status(
                        entry,
                        McpServerStatus::Failed {
                            error: error.to_string(),
                        },
                    );
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
                // This uncertainty belongs to initialize/tools/list; the
                // side-effecting tools/call has not been admitted yet.
                McpRequestOutcome::InterruptedBeforeSend {
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
        Self::set_status(
            entry,
            if entry.spec.enabled {
                McpServerStatus::Disconnected
            } else {
                McpServerStatus::Disabled
            },
        );
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
        Self::set_status(entry, McpServerStatus::Disconnected);
        self.ensure_connected(entry).await
    }

    /// One reconnect-and-retry for transport deaths observed mid-request; a
    /// successful request on a live connection never retries. Reserved for
    /// idempotent requests like `tools/list` — a side-effecting call may
    /// already have run server-side, so `call` must surface the failure
    /// instead of replaying it through here.
    async fn request_with_reconnect<T, F, Fut>(
        &self,
        entry: &Arc<ServerEntry>,
        operation: F,
    ) -> Result<T, McpError>
    where
        F: Fn(Arc<dyn McpConnection>) -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        let connection = self.ensure_connected(entry).await?;
        match operation(Arc::clone(&connection)).await {
            Err(McpError::Closed) | Err(McpError::Protocol(_)) if connection.is_closed() => {
                let connection = self.reconnect_entry(entry).await?;
                operation(connection).await
            }
            result => result,
        }
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
            status: RwLock::new(if spec.enabled {
                McpServerStatus::Disconnected
            } else {
                McpServerStatus::Disabled
            }),
            connection: tokio::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
            spec,
        }
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

fn render_tool_schema(schema: &Value) -> Result<String, McpError> {
    let rendered = serde_json::to_string_pretty(schema)?;
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

async fn connect(
    spec: &McpServerSpec,
    cwd: &Path,
    resolver: &ExecutableResolver,
) -> Result<(Arc<dyn McpConnection>, Arc<Vec<McpToolSummary>>), McpError> {
    let connection: Arc<dyn McpConnection> = match &spec.transport {
        McpTransport::Stdio { command, args, env } => {
            StdioConnection::spawn(command, args, env, cwd, spec.timeout, resolver)?
        }
        McpTransport::Http { url, headers } => Arc::new(HttpConnection::new(
            url.clone(),
            headers.clone(),
            spec.timeout,
        )?),
    };
    connection
        .request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "slim", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
    connection
        .notify("notifications/initialized", json!({}))
        .await;
    let tools = fetch_tools_pages(connection.as_ref()).await?;
    Ok((connection, tools))
}

async fn connect_cancellable(
    spec: &McpServerSpec,
    cwd: &Path,
    resolver: &ExecutableResolver,
    cancellation: McpCancellation,
) -> McpRequestOutcome<(Arc<dyn McpConnection>, Arc<Vec<McpToolSummary>>)> {
    let connection: Arc<dyn McpConnection> = match &spec.transport {
        McpTransport::Stdio { command, args, env } => {
            match StdioConnection::spawn(command, args, env, cwd, spec.timeout, resolver) {
                Ok(connection) => connection,
                Err(error) => return McpRequestOutcome::Completed(Err(error)),
            }
        }
        McpTransport::Http { url, headers } => {
            match HttpConnection::new(url.clone(), headers.clone(), spec.timeout) {
                Ok(connection) => Arc::new(connection),
                Err(error) => return McpRequestOutcome::Completed(Err(error)),
            }
        }
    };
    let initialized = connection
        .request_cancellable(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "slim", "version": env!("CARGO_PKG_VERSION")},
            }),
            cancellation.clone(),
        )
        .await;
    match initialized {
        McpRequestOutcome::Completed(Ok(_)) => {}
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
            return McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            };
        }
    }
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
    let tools = fetch_tools_pages_cancellable(connection.as_ref(), cancellation.clone()).await;
    match tools {
        McpRequestOutcome::Completed(Ok(_tools)) if cancellation.is_cancelled() => {
            let cleanup = connection.close_for_cleanup().await;
            McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup,
            }
        }
        McpRequestOutcome::Completed(Ok(tools)) => {
            McpRequestOutcome::Completed(Ok((connection, tools)))
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
            let cleanup = merge_cleanup(cleanup, connection.close_for_cleanup().await);
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            }
        }
    }
}

/// Paginated `tools/list`: pages stop at `nextCursor` exhaustion,
/// `MAX_TOOLS_PAGES`, or `MAX_TOOLS_PER_SERVER`.
async fn fetch_tools_pages(
    connection: &dyn McpConnection,
) -> Result<Arc<Vec<McpToolSummary>>, McpError> {
    let mut summaries = Vec::new();
    let mut cursor = Value::Null;
    for _ in 0..MAX_TOOLS_PAGES {
        let params = if cursor.is_null() {
            json!({})
        } else {
            json!({"cursor": cursor})
        };
        let response = connection.request("tools/list", params).await?;
        let tools = response
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Protocol("tools/list missing tools array".into()))?;
        for tool in tools {
            if summaries.len() >= MAX_TOOLS_PER_SERVER {
                break;
            }
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            summaries.push(McpToolSummary {
                name: name.to_owned(),
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
        match response.get("nextCursor").and_then(Value::as_str) {
            Some(next) if summaries.len() < MAX_TOOLS_PER_SERVER => {
                cursor = Value::String(next.to_owned());
            }
            _ => break,
        }
    }
    Ok(Arc::new(summaries))
}

async fn fetch_tools_pages_cancellable(
    connection: &dyn McpConnection,
    cancellation: McpCancellation,
) -> McpRequestOutcome<Arc<Vec<McpToolSummary>>> {
    let mut summaries = Vec::new();
    let mut cursor = Value::Null;
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
        match response.get("nextCursor").and_then(Value::as_str) {
            Some(next) if summaries.len() < MAX_TOOLS_PER_SERVER => {
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
