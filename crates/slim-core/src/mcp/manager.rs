use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};

use crate::mcp::http::HttpConnection;
use crate::mcp::spec::{
    McpConnection, McpError, McpServerInfo, McpServerSpec, McpServerStatus, McpToolSummary,
    McpTransport, MCP_PROTOCOL_VERSION,
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

    /// Tool names (+short descriptions) for the model, bounded. Pages of
    /// `MAX_LIST_TOOLS_PER_SERVER` entries are selected with `offset`; the
    /// trailing "… N more tools" line never counts toward the page size.
    pub async fn list_tools_text(&self, server: &str, offset: usize) -> Result<String, McpError> {
        let tools = self.list_tools(server).await?;
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
        Ok(lines.join("\n"))
    }

    pub async fn describe(&self, server: &str, tool: &str) -> Result<String, McpError> {
        let tools = self.list_tools(server).await?;
        let tool = tools
            .iter()
            .find(|candidate| candidate.name == tool)
            .ok_or_else(|| McpError::Protocol(format!("unknown tool {tool} on server {server}")))?;
        let rendered = serde_json::to_string_pretty(&tool.schema)?;
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

    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, McpError> {
        let entry = self.entry(server)?;
        let connection = self.ensure_connected(&entry).await?;
        let params = json!({"name": tool, "arguments": arguments});
        match connection.request("tools/call", params).await {
            Err(error) if connection.is_closed() => {
                // The tool may already have run server-side: never silently
                // retry a non-idempotent call. Drop the dead transport so the
                // next call reconnects, then surface the failure.
                {
                    let mut slot = entry.connection.lock().await;
                    if slot
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &connection))
                    {
                        drop_detached(slot.take());
                    }
                }
                Self::set_status(&entry, McpServerStatus::Disconnected);
                self.bump();
                Err(error)
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
        let generation = entry.generation.load(Ordering::Acquire);
        if let Some(connection) = slot.as_ref() {
            if !connection.is_closed() {
                return Ok(Arc::clone(connection));
            }
            drop_detached(slot.take());
        }
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

    async fn reconnect_entry(
        &self,
        entry: &Arc<ServerEntry>,
    ) -> Result<Arc<dyn McpConnection>, McpError> {
        {
            let mut slot = entry.connection.lock().await;
            drop_detached(slot.take());
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
