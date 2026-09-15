//! LspCodeIntelligence: the slim-lsp implementation of the
//! CodeIntelligence trait. It owns one shared process pool, resolves servers
//! per workspace, keeps documents synced, translates positions through the
//! negotiated encoding and produces compact, bounded, reliability-annotated
//! payloads for the agent-facing code_intel tool.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use lsp_types::{
    Diagnostic, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionResponse, HoverContents, Location, LocationLink, MarkedString, MarkupContent,
    OneOf, SymbolKind, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde_json::{json, Value};

use slim_core::codeintel::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelMeta, CodeIntelOutcome,
    CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery, CodeIntelligence,
    MAX_CODE_INTEL_RESULTS,
};

use crate::pool::{Lease, LspProcessPool, PoolConfig};
use crate::position::{PositionCodec, PositionEncoding};

/// Cap on a single file read for position math / context lines.
pub const MAX_CONTEXT_READ_BYTES: usize = 4 * 1024 * 1024;
/// Cap on hover text returned to the agent.
pub const MAX_HOVER_TEXT_CHARS: usize = 600;
/// Cap on an individual diagnostics message.
pub const MAX_DIAGNOSTIC_MESSAGE_CHARS: usize = 300;
/// Cap on one-line reference/symbol context.
pub const MAX_CONTEXT_LINE_CHARS: usize = 120;
/// Hard bound on locations scanned for one references call. The frame cap
/// already bounds the input, but a degenerate response could still hold
/// ~100k entries; scanning is cheap per item yet unbounded work is unbounded.
const MAX_REFERENCE_SCAN: usize = 65_536;
const MAX_DISCOVERY_CACHE_ENTRIES: usize = 32;

#[derive(Clone, Debug)]
pub struct LspManagerConfig {
    pub idle_shutdown: Option<Duration>,
    pub max_servers: usize,
    pub request_timeout: Duration,
    /// initializationOptions + workspace/configuration section value.
    pub server_config: Value,
    pub max_open_documents: usize,
    /// Optional explicit binary path for the configured server.
    pub server_path: Option<PathBuf>,
}

impl Default for LspManagerConfig {
    fn default() -> Self {
        Self {
            idle_shutdown: Some(Duration::from_secs(15 * 60)),
            max_servers: 4,
            request_timeout: Duration::from_secs(30),
            server_config: json!({ "checkOnSave": false }),
            max_open_documents: crate::document::DEFAULT_MAX_OPEN_DOCUMENTS,
            server_path: None,
        }
    }
}

pub struct LspCodeIntelligence {
    pool: Arc<LspProcessPool>,
    config: LspManagerConfig,
    discovery_cache: Mutex<HashMap<DiscoveryCacheKey, crate::discovery::DiscoveryResult>>,
    workspace_revisions: Mutex<HashMap<PathBuf, u64>>,
    stopped: AtomicBool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DiscoveryCacheKey {
    workspace: PathBuf,
    configured_server: Option<PathBuf>,
    config_hash: u64,
    path_generation: u64,
}

/// A live server plus the pool lease that keeps it alive for the complete
/// operation. Cloning only the Arc would release the lease too early and allow
/// idle shutdown to race an in-flight JSON-RPC request.
pub struct AcquiredServer {
    lease: Lease,
}

impl AcquiredServer {
    fn new(lease: Lease) -> Self {
        Self { lease }
    }
}

impl std::ops::Deref for AcquiredServer {
    type Target = crate::instance::LspServerInstance;

    fn deref(&self) -> &Self::Target {
        self.lease.instance().as_ref()
    }
}

#[derive(Clone, Debug)]
struct SyncedDocument {
    path: PathBuf,
    version: i64,
    workspace_revision: u64,
    content: Arc<crate::document::DocumentContent>,
}

#[derive(Debug)]
struct OperationDocuments {
    root: PathBuf,
    workspace_revision: u64,
    loaded: HashMap<PathBuf, Arc<crate::document::DocumentContent>>,
    resolved_uris: HashMap<String, Option<PathBuf>>,
}

impl OperationDocuments {
    fn new(root: &Path, workspace_revision: u64) -> Self {
        Self {
            root: root.to_path_buf(),
            workspace_revision,
            loaded: HashMap::new(),
            resolved_uris: HashMap::new(),
        }
    }

    fn load(&mut self, path: &Path) -> Option<(PathBuf, Arc<crate::document::DocumentContent>)> {
        if let Some(content) = self.loaded.get(path) {
            return Some((path.to_path_buf(), Arc::clone(content)));
        }
        let path = crate::path_policy::existing_workspace_path(&self.root, path)?;
        if let Some(content) = self.loaded.get(&path) {
            return Some((path, Arc::clone(content)));
        }
        let content = crate::document::DocumentContent::read_capped(&path, MAX_CONTEXT_READ_BYTES)?;
        self.loaded.insert(path.clone(), Arc::clone(&content));
        Some((path, content))
    }

    fn resolve_path(&self, path: &Path) -> Option<PathBuf> {
        if self.loaded.contains_key(path) {
            return Some(path.to_path_buf());
        }
        crate::path_policy::existing_workspace_path(&self.root, path)
    }

    fn remember(&mut self, path: PathBuf, content: Arc<crate::document::DocumentContent>) {
        self.loaded.insert(path, content);
    }

    fn resolve_url(&mut self, url: &url::Url) -> Option<PathBuf> {
        let key = url.as_str().to_owned();
        if let Some(path) = self.resolved_uris.get(&key) {
            return path.clone();
        }
        let path = crate::path_policy::url_workspace_path(&self.root, url);
        self.resolved_uris.insert(key, path.clone());
        path
    }

    fn resolve_uri(&mut self, uri: &lsp_types::Uri) -> Option<PathBuf> {
        self.resolve_url(&crate::instance::uri_to_url(uri)?)
    }

    fn relative_path(&self, path: &Path) -> Option<String> {
        path.strip_prefix(&self.root)
            .ok()
            .map(|relative| relative.to_string_lossy().into_owned())
    }
}

impl LspCodeIntelligence {
    pub fn new(pool: Arc<LspProcessPool>, config: LspManagerConfig) -> Arc<Self> {
        Arc::new(Self {
            pool,
            config,
            discovery_cache: Mutex::new(HashMap::new()),
            workspace_revisions: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        })
    }

    pub fn from_config(config: LspManagerConfig) -> Arc<Self> {
        let pool_config = PoolConfig {
            idle_shutdown: config.idle_shutdown,
            max_servers: config.max_servers,
            ..Default::default()
        };
        let pool = LspProcessPool::new(pool_config);
        Self::new(pool, config)
    }

    pub fn pool(&self) -> &Arc<LspProcessPool> {
        &self.pool
    }

    /// Gracefully closes every server owned by this application-scoped manager.
    pub async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        self.pool.close_all().await;
    }

    fn discovery_cache_key(&self, workspace: &Path) -> DiscoveryCacheKey {
        let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.into());
        let mut path_hasher = std::collections::hash_map::DefaultHasher::new();
        std::env::var_os("PATH").hash(&mut path_hasher);
        DiscoveryCacheKey {
            workspace,
            configured_server: self.config.server_path.clone(),
            config_hash: crate::discovery::config_hash(&self.config.server_config),
            path_generation: path_hasher.finish(),
        }
    }

    fn discover(&self, workspace: &Path) -> crate::discovery::DiscoveryResult {
        let key = self.discovery_cache_key(workspace);
        let cached = self
            .discovery_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned();
        if let Some(cached) = cached {
            // Cheap marker/binary stamps are checked outside the lock. This
            // keeps successful entries current without repeating discovery.
            let root_exists = cached
                .root
                .as_ref()
                .is_some_and(|root| root.join("Cargo.toml").is_file());
            let binary_exists = cached
                .spec
                .as_ref()
                .is_some_and(|spec| Path::new(&spec.command).is_file());
            if root_exists && binary_exists && !cached.binary_missing {
                return cached;
            }
            self.discovery_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&key);
        }

        // Directory walking and PATH probes deliberately happen outside the
        // cache lock so unrelated workspaces cannot block each other.
        let discovery =
            crate::discovery::discover_for_workspace(workspace, self.config.server_path.as_deref());
        let mut cache = self
            .discovery_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= MAX_DISCOVERY_CACHE_ENTRIES {
            cache.clear();
        }
        if discovery.root.is_some() && !discovery.binary_missing {
            cache.insert(key, discovery.clone());
        }
        discovery
    }

    fn resolve_discovery(
        &self,
        discovery: crate::discovery::DiscoveryResult,
    ) -> Result<(PathBuf, crate::discovery::ServerSpec), CodeIntelOutcome> {
        let Some(root) = discovery.root else {
            return Err(CodeIntelOutcome::unavailable(
                "rust-analyzer",
                "no Cargo.toml found in the workspace (rust-analyzer serves Cargo projects only)",
            ));
        };
        let Some(root) = crate::path_policy::canonical_root(&root) else {
            return Err(CodeIntelOutcome::unavailable(
                "rust-analyzer",
                "workspace root is missing or cannot be canonicalized",
            ));
        };
        let Some(spec) = discovery.spec else {
            return Err(CodeIntelOutcome::unavailable(
                "rust-analyzer",
                "no language server configured for this workspace",
            ));
        };
        if discovery.binary_missing {
            return Err(CodeIntelOutcome::unavailable(
                "rust-analyzer",
                "rust-analyzer binary not found on PATH (install it or set lsp.servers.rust-analyzer.path in slim.toml)",
            ));
        }
        Ok((root, spec))
    }

    fn resolve(
        &self,
        workspace: &Path,
    ) -> Result<(PathBuf, crate::discovery::ServerSpec), CodeIntelOutcome> {
        self.resolve_discovery(self.discover(workspace))
    }

    /// Continuation token binding a result page to both the workspace
    /// revision and the exact server generation that produced it, so a page
    /// can never silently continue after an edit or a server restart.
    fn page_revision(workspace_revision: u64, instance_id: u64) -> u64 {
        (instance_id << 32) ^ workspace_revision
    }

    fn workspace_revision(&self, root: &Path) -> u64 {
        self.workspace_revisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(root)
            .copied()
            .unwrap_or(0)
    }

    fn bump_workspace_revision(&self, root: &Path) {
        let mut revisions = self
            .workspace_revisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let revision = revisions.entry(root.to_path_buf()).or_insert(0);
        *revision = revision.saturating_add(1);
    }

    fn transport_options(&self) -> crate::transport::TransportOptions {
        crate::transport::TransportOptions {
            request_timeout: self.config.request_timeout,
            ..Default::default()
        }
    }

    async fn acquire(
        &self,
        root: PathBuf,
        spec: crate::discovery::ServerSpec,
        cancellation: Option<&slim_core::runtime::CancellationToken>,
    ) -> Result<AcquiredServer, CodeIntelOutcome> {
        let acquire = self.pool.acquire(
            root,
            spec,
            &self.config.server_config,
            self.transport_options(),
            self.config.max_open_documents,
        );
        let cancelled = async {
            match cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let lease = tokio::select! {
            biased;
            _ = cancelled => return Err(CodeIntelOutcome::unavailable("rust-analyzer", "request cancelled during server acquisition")),
            result = acquire => result,
        }.map_err(|error| CodeIntelOutcome::unavailable("rust-analyzer", &error.to_string()))?;
        Ok(AcquiredServer::new(lease))
    }

    fn base_meta(
        state: CodeIntelServerState,
        completeness: CodeIntelCompleteness,
        document_version: Option<i64>,
        stale: bool,
        started: Instant,
    ) -> CodeIntelMeta {
        CodeIntelMeta {
            server: "rust-analyzer".into(),
            state,
            completeness,
            document_version,
            stale,
            elapsed_ms: elapsed_ms(started),
        }
    }

    /// Server state for a successful query outcome: indexing servers report
    /// partial completeness instead of masquerading as warm and complete.
    /// A server that never reported indexing progress is ready to answer but
    /// cannot certify the workspace was fully indexed.
    fn indexing_state(
        snapshot: &crate::instance::InstanceSnapshot,
    ) -> (CodeIntelServerState, CodeIntelCompleteness) {
        if snapshot.indexing_active {
            (
                CodeIntelServerState::Indexing,
                CodeIntelCompleteness::Partial,
            )
        } else if snapshot.indexing_observed {
            (CodeIntelServerState::Ready, CodeIntelCompleteness::Complete)
        } else {
            (CodeIntelServerState::Ready, CodeIntelCompleteness::Unknown)
        }
    }

    fn degraded(error: &str, started: Instant) -> CodeIntelOutcome {
        CodeIntelOutcome {
            meta: Self::base_meta(
                CodeIntelServerState::Degraded,
                CodeIntelCompleteness::Unknown,
                None,
                false,
                started,
            ),
            payload: json!({ "error": error }),
        }
    }

    async fn degraded_document(
        &self,
        instance: &crate::instance::LspServerInstance,
        document: &SyncedDocument,
        error: &str,
        started: Instant,
    ) -> CodeIntelOutcome {
        CodeIntelOutcome {
            meta: self
                .document_meta(
                    instance,
                    document,
                    CodeIntelServerState::Degraded,
                    CodeIntelCompleteness::Unknown,
                    started,
                )
                .await,
            payload: json!({ "error": error }),
        }
    }

    async fn ensure_document(
        instance: &crate::instance::LspServerInstance,
        documents: &mut OperationDocuments,
        path: &Path,
    ) -> Option<SyncedDocument> {
        let path = documents.resolve_path(path)?;
        if let Some((version, content)) = instance.document_content_snapshot_async(&path).await {
            if content.stamp_matches_path(&path) {
                #[cfg(test)]
                crate::document::read_metrics::store_hit();
                documents.remember(path.clone(), Arc::clone(&content));
                return Some(SyncedDocument {
                    path,
                    version,
                    workspace_revision: documents.workspace_revision,
                    content,
                });
            }
        }
        let (path, content) = documents.load(&path)?;
        let version = instance
            .sync_document_content(&path, Arc::clone(&content))
            .await?;
        Some(SyncedDocument {
            path,
            version,
            workspace_revision: documents.workspace_revision,
            content,
        })
    }

    async fn operation_document(
        instance: &crate::instance::LspServerInstance,
        documents: &mut OperationDocuments,
        path: &Path,
    ) -> Option<(PathBuf, Arc<crate::document::DocumentContent>)> {
        let path = documents.resolve_path(path)?;
        if let Some(content) = documents.loaded.get(&path) {
            return Some((path, Arc::clone(content)));
        }
        if let Some((_version, content)) = instance.document_content_snapshot_async(&path).await {
            if content.stamp_matches_path(&path) {
                #[cfg(test)]
                crate::document::read_metrics::store_hit();
                documents.remember(path.clone(), Arc::clone(&content));
                return Some((path, content));
            }
        }
        documents.load(&path)
    }

    async fn document_is_stale(
        &self,
        instance: &crate::instance::LspServerInstance,
        document: &SyncedDocument,
    ) -> bool {
        if self.workspace_revision(instance.root()) != document.workspace_revision {
            return true;
        }
        let Some((version, _content)) = instance
            .document_content_snapshot_async(&document.path)
            .await
        else {
            return true;
        };
        if version != document.version {
            return true;
        }
        !document.content.stamp_matches_path(&document.path)
    }

    async fn document_meta(
        &self,
        instance: &crate::instance::LspServerInstance,
        document: &SyncedDocument,
        state: CodeIntelServerState,
        completeness: CodeIntelCompleteness,
        started: Instant,
    ) -> CodeIntelMeta {
        Self::base_meta(
            state,
            completeness,
            Some(document.version),
            self.document_is_stale(instance, document).await,
            started,
        )
    }

    fn human_position(
        encoding: PositionEncoding,
        content: &crate::document::DocumentContent,
        lsp_line: u32,
        lsp_character: u32,
    ) -> (u32, u32) {
        let human_line = lsp_line.saturating_add(1);
        let Some(line) = content.line(lsp_line) else {
            return (human_line, lsp_character.saturating_add(1));
        };
        // Human columns count Unicode scalar values (1-based), not encoding
        // units: translate the LSP character to a byte offset, then count
        // chars. An out-of-range character clamps to the end of the line so
        // the diagnostic/result stays visible instead of shifting.
        let byte =
            PositionCodec::character_to_byte(encoding, line, lsp_character).unwrap_or(line.len());
        let column = line
            .get(..byte)
            .map(|prefix| prefix.chars().count())
            .unwrap_or_default();
        (
            human_line,
            u32::try_from(column.saturating_add(1)).unwrap_or(u32::MAX),
        )
    }

    fn human_to_lsp(
        encoding: PositionEncoding,
        content: &crate::document::DocumentContent,
        human_line: u32,
        human_column: u32,
    ) -> Option<(u32, u32)> {
        let line_index = human_line.checked_sub(1)?;
        let line = content.line(line_index)?;
        let char_index = usize::try_from(human_column.checked_sub(1)?).ok()?;
        let char_count = line.chars().count();
        if char_index > char_count {
            return None;
        }
        let byte = if char_index == char_count {
            line.len()
        } else {
            line.char_indices()
                .nth(char_index)
                .map(|(index, _)| index)?
        };
        let lsp_column = PositionCodec::byte_to_character(encoding, line, byte)?;
        Some((line_index, lsp_column))
    }

    fn diagnostic_row(
        encoding: PositionEncoding,
        content: &crate::document::DocumentContent,
        item: Diagnostic,
    ) -> Value {
        let (line, column) = Self::human_position(
            encoding,
            content,
            item.range.start.line,
            item.range.start.character,
        );
        let message = truncate_text(&item.message, MAX_DIAGNOSTIC_MESSAGE_CHARS);
        json!({
            "severity": severity_name(item.severity),
            "code": item.code.as_ref().map(diagnostic_code_string),
            "source": item.source,
            "line": line,
            "column": column,
            "message": message,
        })
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn line_context(content: &crate::document::DocumentContent, line_index: u32) -> Option<String> {
    let raw = content.line(line_index)?;
    Some(truncate_text(raw, MAX_CONTEXT_LINE_CHARS))
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        truncated.push_str("...");
    }
    truncated
}

fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type parameter",
        _ => "symbol",
    }
}

/// Lower is better; without a query every symbol ranks equally, so the stable
/// sort keeps document order. A hint only reorders — it never drops results.
fn symbol_query_rank(name: &str, query: Option<&str>) -> u8 {
    let Some(query) = query else {
        return 0;
    };
    let lower = name.to_lowercase();
    let needle = query.to_lowercase();
    if lower == needle {
        0
    } else if lower.starts_with(&needle) {
        1
    } else if lower.contains(&needle) {
        2
    } else {
        3
    }
}

fn severity_name(severity: Option<lsp_types::DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
        Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
        Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
        _ => "diagnostic",
    }
}

fn diagnostic_code_string(code: &lsp_types::NumberOrString) -> String {
    match code {
        lsp_types::NumberOrString::Number(number) => number.to_string(),
        lsp_types::NumberOrString::String(text) => text.clone(),
    }
}

fn file_uri_string(path: &Path) -> Option<String> {
    crate::instance::file_uri(path).map(|url| url.to_string())
}

fn hover_text(contents: &HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked) => marked_string_text(marked),
        HoverContents::Array(items) => items
            .iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n"),
        HoverContents::Markup(markup) => markup_text(markup),
    }
}

fn marked_string_text(marked: &MarkedString) -> String {
    match marked {
        MarkedString::String(text) => text.clone(),
        MarkedString::LanguageString(language) => {
            format!("{}: {}", language.language, language.value)
        }
    }
}

fn markup_text(markup: &MarkupContent) -> String {
    markup.value.clone()
}

fn parse_location(
    documents: &mut OperationDocuments,
    location: &Location,
) -> Option<(PathBuf, u32, u32)> {
    let path = documents.resolve_uri(&location.uri)?;
    Some((
        path,
        location.range.start.line,
        location.range.start.character,
    ))
}

#[async_trait::async_trait]
impl CodeIntelligence for LspCodeIntelligence {
    fn supports_workspace(&self, workspace: &Path) -> bool {
        !self.stopped.load(Ordering::Acquire) && self.resolve(workspace).is_ok()
    }

    async fn status(&self, workspace: &Path) -> CodeIntelOutcome {
        let started = Instant::now();
        let discovery = self.discover(workspace);
        let binary_missing = discovery.binary_missing;
        let no_root = discovery.root.is_none();
        let mut servers: Vec<Value> = Vec::new();
        let baseline_summary;
        if let Some(discovered_root) = discovery.root.as_ref() {
            let root = crate::path_policy::canonical_root(discovered_root)
                .unwrap_or_else(|| discovered_root.clone());
            let binary = discovery
                .spec
                .as_ref()
                .map(|spec| spec.command.clone())
                .unwrap_or_else(|| "rust-analyzer".into());
            servers.push(json!({
                "server": "rust-analyzer",
                "root": root.to_string_lossy(),
                "binary": binary,
                "binary_missing": discovery.binary_missing,
                "state": if discovery.binary_missing { "unavailable" } else { "configured" },
                "language": "rust",
            }));
            baseline_summary = if discovery.binary_missing {
                "rust-analyzer: configured for this workspace but the binary is not available"
                    .into()
            } else {
                "rust-analyzer: configured for this workspace".into()
            };
        } else {
            baseline_summary =
                "no Cargo.toml found for this workspace; rust-analyzer stays inactive".into();
        }
        let mut summary = baseline_summary;
        if let Ok((root, spec)) = self.resolve_discovery(discovery) {
            if let Some(instance) = self
                .pool
                .acquire_warm(&root, &spec.id, &self.config.server_config)
                .await
            {
                let snapshot = instance.instance().snapshot().await;
                if let Some(server) = servers.iter_mut().find(|server| {
                    server.get("server").and_then(Value::as_str) == Some("rust-analyzer")
                }) {
                    let state = if snapshot.indexing_active {
                        "indexing"
                    } else {
                        "ready"
                    };
                    server["state"] = json!(state);
                    server["indexing"] = json!(snapshot.indexing_active);
                    server["indexing_observed"] = json!(snapshot.indexing_observed);
                    server["open_documents"] = json!(snapshot.open_documents);
                    server["diagnostic_uris"] = json!(snapshot.diagnostic_uris);
                    summary = format!(
                        "rust-analyzer {state} ({} open document(s))",
                        snapshot.open_documents
                    );
                }
            }
        }
        let stopped = self.stopped.load(Ordering::Acquire);
        let state = if servers
            .iter()
            .any(|server| server.get("state").and_then(Value::as_str) == Some("ready"))
        {
            CodeIntelServerState::Ready
        } else if stopped {
            CodeIntelServerState::Stopped
        } else if binary_missing || no_root {
            CodeIntelServerState::Unavailable
        } else {
            CodeIntelServerState::Starting
        };
        if stopped {
            summary = "rust-analyzer shut down".into();
        }
        CodeIntelOutcome {
            meta: Self::base_meta(state, CodeIntelCompleteness::Complete, None, false, started),
            payload: json!({ "servers": servers, "summary": summary }),
        }
    }

    async fn definition(&self, query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        let started = Instant::now();
        let (root, spec) = match self.resolve(&query.workspace) {
            Ok(pair) => pair,
            Err(outcome) => return outcome,
        };
        let instance = match self
            .acquire(root.clone(), spec, query.cancellation.as_ref())
            .await
        {
            Ok(instance) => instance,
            Err(outcome) => return outcome,
        };
        let mut documents = OperationDocuments::new(&root, self.workspace_revision(&root));
        let Some(document) = Self::ensure_document(&instance, &mut documents, &query.path).await
        else {
            return CodeIntelOutcome::unavailable(
                "rust-analyzer",
                "file is outside the workspace, too large, unreadable, or not served",
            );
        };
        let snapshot = instance.snapshot().await;
        let encoding = snapshot.encoding;
        let (state, completeness) = Self::indexing_state(&snapshot);
        let Some((lsp_line, lsp_char)) =
            Self::human_to_lsp(encoding, &document.content, query.line, query.column)
        else {
            return CodeIntelOutcome {
                meta: self
                    .document_meta(&instance, &document, state, completeness, started)
                    .await,
                payload: json!({
                    "error": format!(
                        "position {}:{} is outside {}",
                        query.line,
                        query.column,
                        document.path.display()
                    )
                }),
            };
        };
        let Some(uri) = file_uri_string(&document.path) else {
            return self
                .degraded_document(
                    &instance,
                    &document,
                    "failed to convert document path to URI",
                    started,
                )
                .await;
        };
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": lsp_line, "character": lsp_char },
        });
        let response_value = match instance
            .request_value_cancellable(
                "textDocument/definition",
                params,
                query.cancellation.as_ref(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return self
                    .degraded_document(&instance, &document, &error.to_string(), started)
                    .await;
            }
        };

        let mut found = false;
        let mut preview = None;
        let mut file = String::new();
        let mut line = 1_u32;
        let mut column = 1_u32;
        let mut locations_received = 0usize;
        let mut locations_out_of_scope = 0usize;
        if !response_value.is_null() {
            let response: GotoDefinitionResponse = match serde_json::from_value(response_value) {
                Ok(response) => response,
                Err(error) => {
                    return self
                        .degraded_document(
                            &instance,
                            &document,
                            &format!("definition response: {error}"),
                            started,
                        )
                        .await;
                }
            };
            let locations: Vec<Location> = match response {
                GotoDefinitionResponse::Scalar(location) => vec![location],
                GotoDefinitionResponse::Array(locations) => locations,
                GotoDefinitionResponse::Link(links) => links
                    .into_iter()
                    .map(|link: LocationLink| Location {
                        uri: link.target_uri,
                        range: link.target_selection_range,
                    })
                    .collect(),
            };
            locations_received = locations.len();
            // Scan every location so the out-of-scope count is exact — URI
            // resolution is cached per file, so the extra passes are cheap.
            let mut located = None;
            for location in &locations {
                match parse_location(&mut documents, location) {
                    Some(parsed) => {
                        if located.is_none() {
                            located = Some(parsed);
                        }
                    }
                    None => locations_out_of_scope += 1,
                }
            }
            if let Some((target, target_line, target_char)) = located {
                if let Some(relative) = documents.relative_path(&target) {
                    let Some((_path, content)) =
                        Self::operation_document(&instance, &mut documents, &target).await
                    else {
                        return self.degraded_document(&instance, &document,
                            &format!("definition target {relative} is too large, unreadable, or not UTF-8; position unavailable"), started).await;
                    };
                    (line, column) =
                        Self::human_position(encoding, &content, target_line, target_char);
                    preview = line_context(&content, target_line);
                    found = true;
                    file = relative;
                }
            }
        }
        // `found` alone cannot distinguish "no definition" from "the
        // definition lives outside the allowed workspace scope" — the
        // counts make that difference visible to the renderer.
        CodeIntelOutcome {
            meta: self
                .document_meta(&instance, &document, state, completeness, started)
                .await,
            payload: json!({
                "found": found,
                "preview": preview,
                "file": file,
                "line": line,
                "column": column,
                "symbol": query.symbol.clone(),
                "locations_received": locations_received,
                "locations_out_of_scope": locations_out_of_scope,
            }),
        }
    }

    async fn references(&self, query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        let started = Instant::now();
        let max_results = query.max_results.clamp(1, MAX_CODE_INTEL_RESULTS);
        let (root, spec) = match self.resolve(&query.workspace) {
            Ok(pair) => pair,
            Err(outcome) => return outcome,
        };
        let instance = match self
            .acquire(root.clone(), spec, query.cancellation.as_ref())
            .await
        {
            Ok(instance) => instance,
            Err(outcome) => return outcome,
        };
        let mut documents = OperationDocuments::new(&root, self.workspace_revision(&root));
        let revision = Self::page_revision(documents.workspace_revision, instance.id());
        if query.revision.is_some_and(|expected| expected != revision) {
            return Self::degraded(
                "workspace or server state changed since the previous page; re-run with \"offset\": 0",
                started,
            );
        }
        let Some(document) = Self::ensure_document(&instance, &mut documents, &query.path).await
        else {
            return CodeIntelOutcome::unavailable(
                "rust-analyzer",
                "file is outside the workspace, too large, unreadable, or not served",
            );
        };
        let snapshot = instance.snapshot().await;
        let encoding = snapshot.encoding;
        let (state, completeness) = Self::indexing_state(&snapshot);
        let Some((lsp_line, lsp_char)) =
            Self::human_to_lsp(encoding, &document.content, query.line, query.column)
        else {
            return CodeIntelOutcome {
                meta: self
                    .document_meta(&instance, &document, state, completeness, started)
                    .await,
                payload: json!({ "error": "position is outside the file" }),
            };
        };
        let Some(uri) = file_uri_string(&document.path) else {
            return self
                .degraded_document(
                    &instance,
                    &document,
                    "failed to convert document path to URI",
                    started,
                )
                .await;
        };
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": lsp_line, "character": lsp_char },
            "context": { "includeDeclaration": false },
        });
        let response_value = match instance
            .request_value_cancellable(
                "textDocument/references",
                params,
                query.cancellation.as_ref(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return self
                    .degraded_document(&instance, &document, &error.to_string(), started)
                    .await;
            }
        };
        let locations = match serde_json::from_value::<Option<Vec<Location>>>(response_value) {
            Ok(locations) => locations.unwrap_or_default(),
            Err(error) => {
                return self
                    .degraded_document(
                        &instance,
                        &document,
                        &format!("references response: {error}"),
                        started,
                    )
                    .await
            }
        };
        let snapshot = instance.snapshot().await;
        let mut by_file: BTreeMap<String, Vec<(u32, u32, Option<String>)>> = BTreeMap::new();
        let received = locations.len();
        let scan_truncated = received > MAX_REFERENCE_SCAN;
        let offset = query.offset;
        let mut total = 0_usize;
        let mut shown = 0_usize;
        // Cursor of the next page: one past the last in-window index. Items
        // that fail resolution still advance the cursor — they can never be
        // displayed, so later pages skip them instead of repeating siblings.
        let mut next_offset = offset;
        let mut total_files = std::collections::BTreeSet::new();
        for location in locations.into_iter().take(MAX_REFERENCE_SCAN) {
            let Some((path, target_line, target_char)) = parse_location(&mut documents, &location)
            else {
                continue;
            };
            let Some(relative) = documents.relative_path(&path) else {
                continue;
            };
            let index = total;
            total = total.saturating_add(1);
            total_files.insert(relative.clone());
            // Only the page window pays for document opens and context lines;
            // everything outside [offset, offset + max_results) is counted
            // but never resolved.
            if index < offset || shown >= max_results {
                continue;
            }
            next_offset = index.saturating_add(1);
            let Some((_path, content)) =
                Self::operation_document(&instance, &mut documents, &path).await
            else {
                continue;
            };
            let (line, column) = Self::human_position(encoding, &content, target_line, target_char);
            by_file.entry(relative).or_default().push((
                line,
                column,
                line_context(&content, target_line),
            ));
            shown = shown.saturating_add(1);
        }
        let files: Vec<Value> = by_file
            .into_iter()
            .map(|(file, results)| {
                let count = results.len();
                let results: Vec<Value> = results
                    .into_iter()
                    .map(|(line, column, context)| {
                        json!({ "line": line, "column": column, "context": context })
                    })
                    .collect();
                json!({ "file": file, "count": count, "results": results })
            })
            .collect();
        let (state, completeness) = Self::indexing_state(&snapshot);
        CodeIntelOutcome {
            meta: self
                .document_meta(&instance, &document, state, completeness, started)
                .await,
            payload: json!({
                "total": total,
                "total_files": total_files.len(),
                "shown": shown,
                "offset": offset,
                "has_more": next_offset < total || scan_truncated,
                "next_offset": next_offset,
                "received": received,
                "scanned": received.min(MAX_REFERENCE_SCAN),
                "scan_truncated": scan_truncated,
                "revision": revision,
                "files": files,
                "symbol": query.symbol.clone(),
            }),
        }
    }

    async fn hover(&self, query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        let started = Instant::now();
        let (root, spec) = match self.resolve(&query.workspace) {
            Ok(pair) => pair,
            Err(outcome) => return outcome,
        };
        let instance = match self
            .acquire(root.clone(), spec, query.cancellation.as_ref())
            .await
        {
            Ok(instance) => instance,
            Err(outcome) => return outcome,
        };
        let mut documents = OperationDocuments::new(&root, self.workspace_revision(&root));
        let Some(document) = Self::ensure_document(&instance, &mut documents, &query.path).await
        else {
            return CodeIntelOutcome::unavailable(
                "rust-analyzer",
                "file is outside the workspace, too large, unreadable, or not served",
            );
        };
        let snapshot = instance.snapshot().await;
        let encoding = snapshot.encoding;
        let (state, completeness) = Self::indexing_state(&snapshot);
        let Some((lsp_line, lsp_char)) =
            Self::human_to_lsp(encoding, &document.content, query.line, query.column)
        else {
            return CodeIntelOutcome {
                meta: self
                    .document_meta(&instance, &document, state, completeness, started)
                    .await,
                payload: json!({ "error": "position is outside the file" }),
            };
        };
        let Some(uri) = file_uri_string(&document.path) else {
            return self
                .degraded_document(
                    &instance,
                    &document,
                    "failed to convert document path to URI",
                    started,
                )
                .await;
        };
        let response_value = match instance
            .request_value_cancellable(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": lsp_line, "character": lsp_char },
                }),
                query.cancellation.as_ref(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return self
                    .degraded_document(&instance, &document, &error.to_string(), started)
                    .await;
            }
        };
        let payload = match serde_json::from_value::<Option<lsp_types::Hover>>(response_value) {
            Ok(Some(hover)) => {
                let raw = hover_text(&hover.contents);
                let text = truncate_text(&raw, MAX_HOVER_TEXT_CHARS);
                json!({
                    "found": !raw.is_empty(),
                    "text": text,
                    "truncated": text != raw,
                })
            }
            Ok(None) => json!({ "found": false, "text": "", "truncated": false }),
            Err(error) => {
                return self
                    .degraded_document(
                        &instance,
                        &document,
                        &format!("hover response: {error}"),
                        started,
                    )
                    .await
            }
        };
        CodeIntelOutcome {
            meta: self
                .document_meta(&instance, &document, state, completeness, started)
                .await,
            payload,
        }
    }

    async fn symbols(&self, query: &CodeIntelSymbolQuery) -> CodeIntelOutcome {
        let started = Instant::now();
        let max_results = query.max_results.clamp(1, MAX_CODE_INTEL_RESULTS);
        let (root, spec) = match self.resolve(&query.workspace) {
            Ok(pair) => pair,
            Err(outcome) => return outcome,
        };
        let instance = match self
            .acquire(root.clone(), spec, query.cancellation.as_ref())
            .await
        {
            Ok(instance) => instance,
            Err(outcome) => return outcome,
        };
        let snapshot = instance.snapshot().await;
        let encoding = snapshot.encoding;
        let (state, completeness) = Self::indexing_state(&snapshot);
        let mut documents = OperationDocuments::new(&root, self.workspace_revision(&root));
        let revision = Self::page_revision(documents.workspace_revision, instance.id());
        if query.revision.is_some_and(|expected| expected != revision) {
            return Self::degraded(
                "workspace or server state changed since the previous page; re-run with \"offset\": 0",
                started,
            );
        }
        let offset = query.offset;

        if let Some(path) = &query.path {
            let Some(document) = Self::ensure_document(&instance, &mut documents, path).await
            else {
                return CodeIntelOutcome::unavailable(
                    "rust-analyzer",
                    "file is outside the workspace, too large, unreadable, or not served",
                );
            };
            let Some(uri) = crate::instance::file_uri(&document.path) else {
                return self
                    .degraded_document(
                        &instance,
                        &document,
                        "failed to convert document path to URI",
                        started,
                    )
                    .await;
            };
            let params = DocumentSymbolParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: crate::instance::to_lsp_uri(&uri),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let response_value = match instance
                .request_value_cancellable(
                    "textDocument/documentSymbol",
                    serde_json::to_value(&params).unwrap_or(Value::Null),
                    query.cancellation.as_ref(),
                )
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    return self
                        .degraded_document(&instance, &document, &error.to_string(), started)
                        .await;
                }
            };
            let response =
                match serde_json::from_value::<Option<DocumentSymbolResponse>>(response_value) {
                    Ok(response) => {
                        response.unwrap_or_else(|| DocumentSymbolResponse::Flat(Vec::new()))
                    }
                    Err(error) => {
                        return self
                            .degraded_document(
                                &instance,
                                &document,
                                &format!("document symbols response: {error}"),
                                started,
                            )
                            .await;
                    }
                };
            // Ranking happens before the page window, so a `query` match pulls
            // the target into view even when it sits past the display limit;
            // equal ranks keep document order. Items outside the window are
            // counted but never resolved.
            let name_rank = |name: &str| symbol_query_rank(name, query.query.as_deref());
            let mut symbols = Vec::new();
            let mut next_offset = offset;
            let total: usize;
            let received;
            match response {
                DocumentSymbolResponse::Flat(flat) => {
                    received = flat.len();
                    let mut candidates = Vec::new();
                    for item in flat {
                        let Some((target, target_line, target_char)) =
                            parse_location(&mut documents, &item.location)
                        else {
                            continue;
                        };
                        let Some(file) = documents.relative_path(&target) else {
                            continue;
                        };
                        candidates.push((
                            name_rank(&item.name),
                            item.name,
                            symbol_kind_name(item.kind),
                            item.container_name,
                            target,
                            target_line,
                            target_char,
                            file,
                        ));
                    }
                    candidates.sort_by_key(|candidate| candidate.0);
                    total = candidates.len();
                    for (
                        index,
                        (_rank, name, kind, container, target, target_line, target_char, file),
                    ) in candidates.into_iter().enumerate()
                    {
                        if index < offset || symbols.len() >= max_results {
                            continue;
                        }
                        next_offset = index.saturating_add(1);
                        let Some((_path, content)) =
                            Self::operation_document(&instance, &mut documents, &target).await
                        else {
                            continue;
                        };
                        let (line, column) =
                            Self::human_position(encoding, &content, target_line, target_char);
                        symbols.push(json!({
                            "name": name,
                            "kind": kind,
                            "file": file,
                            "line": line,
                            "column": column,
                            "container": container,
                        }));
                    }
                }
                DocumentSymbolResponse::Nested(nested) => {
                    let mut stack: Vec<(DocumentSymbol, usize)> = nested
                        .into_iter()
                        .rev()
                        .map(|item| (item, 0_usize))
                        .collect();
                    // Nested symbols live in the queried document itself, so
                    // positions convert against its content (no per-item
                    // lookup). selectionRange is the navigation target.
                    let file = documents.relative_path(&document.path).unwrap_or_default();
                    let mut candidates = Vec::new();
                    while let Some((item, depth)) = stack.pop() {
                        let (line, column) = Self::human_position(
                            encoding,
                            &document.content,
                            item.selection_range.start.line,
                            item.selection_range.start.character,
                        );
                        candidates.push((
                            name_rank(&item.name),
                            format!("{}{}", "  ".repeat(depth), item.name),
                            symbol_kind_name(item.kind),
                            item.detail,
                            line,
                            column,
                        ));
                        if let Some(children) = item.children {
                            for child in children.into_iter().rev() {
                                stack.push((child, depth.saturating_add(1)));
                            }
                        }
                    }
                    received = candidates.len();
                    candidates.sort_by_key(|candidate| candidate.0);
                    total = candidates.len();
                    for (index, (_rank, name, kind, detail, line, column)) in
                        candidates.into_iter().enumerate()
                    {
                        if index < offset || symbols.len() >= max_results {
                            continue;
                        }
                        next_offset = index.saturating_add(1);
                        symbols.push(json!({
                            "name": name,
                            "kind": kind,
                            "detail": detail,
                            "file": file,
                            "line": line,
                            "column": column,
                        }));
                    }
                }
            }
            let shown = symbols.len();
            return CodeIntelOutcome {
                meta: self
                    .document_meta(&instance, &document, state, completeness, started)
                    .await,
                payload: json!({
                    "kind": "document",
                    "query": query.query.clone(),
                    "received": received,
                    "total": total,
                    "shown": shown,
                    "offset": offset,
                    "has_more": next_offset < total,
                    "next_offset": next_offset,
                    "revision": revision,
                    "symbols": symbols,
                }),
            };
        }

        let params = WorkspaceSymbolParams {
            query: query.query.clone().unwrap_or_default(),
            ..Default::default()
        };
        let response_value = match instance
            .request_value_cancellable(
                "workspace/symbol",
                serde_json::to_value(&params).unwrap_or(Value::Null),
                query.cancellation.as_ref(),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => return Self::degraded(&error.to_string(), started),
        };
        let response =
            match serde_json::from_value::<Option<WorkspaceSymbolResponse>>(response_value) {
                Ok(response) => {
                    response.unwrap_or_else(|| WorkspaceSymbolResponse::Flat(Vec::new()))
                }
                Err(error) => {
                    return Self::degraded(
                        &format!("workspace symbols response: {error}"),
                        started,
                    );
                }
            };
        // The server already searched by `query`, so workspace results keep
        // server order; only the page window pays for document opens.
        let mut symbols = Vec::new();
        let mut next_offset = offset;
        let mut total = 0_usize;
        let received;
        match response {
            WorkspaceSymbolResponse::Flat(flat) => {
                received = flat.len();
                for item in flat {
                    let Some((target, target_line, target_char)) =
                        parse_location(&mut documents, &item.location)
                    else {
                        continue;
                    };
                    let Some(file) = documents.relative_path(&target) else {
                        continue;
                    };
                    let index = total;
                    total = total.saturating_add(1);
                    if index < offset || symbols.len() >= max_results {
                        continue;
                    }
                    next_offset = index.saturating_add(1);
                    let Some((_path, content)) =
                        Self::operation_document(&instance, &mut documents, &target).await
                    else {
                        continue;
                    };
                    let (line, column) =
                        Self::human_position(encoding, &content, target_line, target_char);
                    symbols.push(json!({
                        "name": item.name,
                        "kind": symbol_kind_name(item.kind),
                        "file": file,
                        "line": line,
                        "column": column,
                        "container": item.container_name,
                    }));
                }
            }
            WorkspaceSymbolResponse::Nested(nested) => {
                received = nested.len();
                for item in nested {
                    match item.location {
                        OneOf::Left(location) => {
                            let Some(target) = documents.resolve_uri(&location.uri) else {
                                continue;
                            };
                            let Some(file) = documents.relative_path(&target) else {
                                continue;
                            };
                            let index = total;
                            total = total.saturating_add(1);
                            if index < offset || symbols.len() >= max_results {
                                continue;
                            }
                            next_offset = index.saturating_add(1);
                            let Some((_path, content)) =
                                Self::operation_document(&instance, &mut documents, &target).await
                            else {
                                continue;
                            };
                            let (line, column) = Self::human_position(
                                encoding,
                                &content,
                                location.range.start.line,
                                location.range.start.character,
                            );
                            symbols.push(json!({
                                "name": item.name,
                                "kind": symbol_kind_name(item.kind),
                                "file": file,
                                "line": line,
                                "column": column,
                                "container": item.container_name,
                            }));
                        }
                        OneOf::Right(symbol_location) => {
                            let Some(target) = documents.resolve_uri(&symbol_location.uri) else {
                                continue;
                            };
                            let Some(file) = documents.relative_path(&target) else {
                                continue;
                            };
                            let index = total;
                            total = total.saturating_add(1);
                            if index < offset || symbols.len() >= max_results {
                                continue;
                            }
                            next_offset = index.saturating_add(1);
                            symbols.push(json!({
                                "name": item.name,
                                "kind": symbol_kind_name(item.kind),
                                "file": file,
                                "line": null,
                                "column": null,
                                "container": item.container_name,
                            }));
                        }
                    }
                }
            }
        }
        let shown = symbols.len();
        CodeIntelOutcome {
            meta: Self::base_meta(state, completeness, None, false, started),
            payload: json!({
                "kind": "workspace",
                "query": query.query.clone(),
                "received": received,
                "total": total,
                "shown": shown,
                "offset": offset,
                "has_more": next_offset < total,
                "next_offset": next_offset,
                "revision": revision,
                "symbols": symbols,
            }),
        }
    }

    async fn diagnostics(&self, query: &CodeIntelDiagnosticsQuery) -> CodeIntelOutcome {
        let started = Instant::now();
        let max_results = query.max_results.clamp(1, MAX_CODE_INTEL_RESULTS);
        let (root, spec) = match self.resolve(&query.workspace) {
            Ok(pair) => pair,
            Err(outcome) => return outcome,
        };
        let instance = match self
            .acquire(root.clone(), spec, query.cancellation.as_ref())
            .await
        {
            Ok(instance) => instance,
            Err(outcome) => return outcome,
        };
        if query
            .cancellation
            .as_ref()
            .is_some_and(slim_core::runtime::CancellationToken::is_cancelled)
        {
            return Self::degraded("diagnostics query cancelled", started);
        }
        let snapshot = instance.snapshot().await;
        let encoding = snapshot.encoding;
        let mut documents = OperationDocuments::new(&root, self.workspace_revision(&root));
        let (state, completeness) = Self::indexing_state(&snapshot);

        if let Some(path) = &query.path {
            let Some(document) = Self::ensure_document(&instance, &mut documents, path).await
            else {
                return CodeIntelOutcome::unavailable(
                    "rust-analyzer",
                    "file is outside the workspace, too large, unreadable, or not served",
                );
            };
            if query
                .cancellation
                .as_ref()
                .is_some_and(slim_core::runtime::CancellationToken::is_cancelled)
            {
                return self
                    .degraded_document(&instance, &document, "diagnostics query cancelled", started)
                    .await;
            }
            let Some(uri) = crate::instance::file_uri(&document.path) else {
                return self
                    .degraded_document(
                        &instance,
                        &document,
                        "failed to convert document path to URI",
                        started,
                    )
                    .await;
            };
            let diagnostics = instance
                .diagnostics_snapshot(&uri, query.include_info)
                .await;
            let diagnostic_version = diagnostics.as_ref().and_then(|stored| stored.version);
            let received = diagnostics.is_some();
            let publication_stale = diagnostics.as_ref().is_some_and(|stored| stored.stale);
            let total = diagnostics.as_ref().map(|stored| stored.total);
            let storage_truncated = diagnostics.as_ref().is_some_and(|stored| stored.truncated);
            let mut rows = Vec::new();
            if let Some(diagnostics) = diagnostics {
                for item in diagnostics.items.into_iter().take(max_results) {
                    rows.push(Self::diagnostic_row(encoding, &document.content, item));
                }
            }
            let stale = self.document_is_stale(&instance, &document).await
                || publication_stale
                || diagnostic_version.is_some_and(|version| version != document.version);
            let has_more = total.is_some_and(|total| total > rows.len());
            // A versionless publication can be useful, but cannot certify
            // which document version the server actually validated.
            let completeness = if !received || stale || diagnostic_version.is_none() {
                CodeIntelCompleteness::Unknown
            } else if has_more || storage_truncated {
                CodeIntelCompleteness::Partial
            } else {
                completeness
            };
            let file = documents.relative_path(&document.path).unwrap_or_default();
            return CodeIntelOutcome {
                meta: Self::base_meta(state, completeness, Some(document.version), stale, started),
                payload: json!({
                    "shown": rows.len(),
                    "total": total,
                    "has_more": has_more,
                    "storage_truncated": storage_truncated,
                    "files": [{
                        "file": file,
                        "count": rows.len(),
                        "document_version": document.version,
                        "diagnostic_version": diagnostic_version,
                        "received": received,
                        "stale": stale,
                        "diagnostics": rows,
                    }]
                }),
            };
        }

        let (mut published_total, mut storage_truncated, publication_revision) =
            instance.diagnostic_totals(query.include_info).await;
        let mut total = 0_usize;
        let mut any_stale = false;
        let mut files = Vec::new();
        for uri in instance.diagnostic_uris().await {
            if total >= max_results {
                break;
            }
            if query
                .cancellation
                .as_ref()
                .is_some_and(slim_core::runtime::CancellationToken::is_cancelled)
            {
                return Self::degraded("diagnostics query cancelled", started);
            }
            let Some(path) = documents.resolve_url(&uri) else {
                continue;
            };
            let Some(file) = documents.relative_path(&path) else {
                continue;
            };
            let Some(diagnostics) = instance
                .diagnostics_snapshot(&uri, query.include_info)
                .await
            else {
                continue;
            };
            let document = instance.document_content_snapshot_async(&path).await;
            let document_version = document.as_ref().map(|(version, _)| *version);
            let stamp_stale = document
                .as_ref()
                .is_none_or(|(_, content)| !content.stamp_matches_path(&path));
            let stale = diagnostics.stale
                || diagnostics
                    .version
                    .zip(document_version)
                    .is_some_and(|(published, version)| published != version)
                || stamp_stale;
            let content = match document.as_ref() {
                Some((_, content)) if !stamp_stale => Arc::clone(content),
                _ => match documents.load(&path) {
                    Some((_path, content)) => content,
                    None => match document.as_ref() {
                        Some((_, content)) => Arc::clone(content),
                        None => continue,
                    },
                },
            };
            any_stale |= stale;
            let mut rows = Vec::new();
            for item in diagnostics.items {
                if total >= max_results {
                    break;
                }
                rows.push(Self::diagnostic_row(encoding, &content, item));
                total = total.saturating_add(1);
            }
            if !rows.is_empty() {
                files.push(json!({
                    "file": file,
                    "count": rows.len(),
                    "document_version": document_version,
                    "diagnostic_version": diagnostics.version,
                    "stale": stale,
                    "diagnostics": rows,
                }));
            }
        }
        any_stale |= self.workspace_revision(&root) != documents.workspace_revision;
        let (_, latest_truncated, latest_revision) =
            instance.diagnostic_totals(query.include_info).await;
        if latest_revision != publication_revision {
            // Do not combine counts from one publication with rows from another.
            published_total = None;
        }
        storage_truncated |= latest_truncated;
        CodeIntelOutcome {
            // Push diagnostics cover only publications received so far. Even
            // an idle server does not certify coverage of every workspace file.
            meta: Self::base_meta(
                state,
                CodeIntelCompleteness::Unknown,
                None,
                any_stale,
                started,
            ),
            payload: json!({
                "scope": "published",
                "shown": total,
                "total": published_total,
                "has_more": published_total.map(|available| available > total),
                "storage_truncated": storage_truncated,
                "files": files,
            }),
        }
    }

    async fn notify_file_changed(&self, workspace: &Path, path: &Path, text: Option<String>) {
        self.notify_update(workspace, path, text, None).await;
    }

    async fn notify_file_updated(
        &self,
        workspace: &Path,
        path: &Path,
        update: slim_core::codeintel::CodeIntelFileUpdate,
    ) {
        self.notify_update(workspace, path, Some(update.text), update.patch.as_ref())
            .await;
    }
}

impl LspCodeIntelligence {
    async fn notify_update(
        &self,
        workspace: &Path,
        path: &Path,
        text: Option<String>,
        patch: Option<&slim_core::codeintel::CodeIntelPatch>,
    ) {
        let Ok((root, spec)) = self.resolve(workspace) else {
            return;
        };
        let Some(path) = crate::path_policy::existing_workspace_path(&root, path) else {
            return;
        };
        self.bump_workspace_revision(&root);
        let Some(instance) = self
            .pool
            .acquire_warm(&root, &spec.id, &self.config.server_config)
            .await
        else {
            return;
        };
        if !instance.instance().document_is_open(&path).await {
            return;
        }
        let content = if let Some(text) = text {
            crate::document::DocumentContent::from_text(
                text,
                crate::document::FileStamp::for_path(&path),
            )
        } else {
            let Some(content) =
                crate::document::DocumentContent::read_capped(&path, MAX_CONTEXT_READ_BYTES)
            else {
                return;
            };
            content
        };
        instance
            .instance()
            .notify_file_updated_content(&path, content, patch)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_boundaries_and_unicode() {
        for (text, limit, expected) in [
            ("", 0, ""),
            ("a", 0, "..."),
            ("ab", 3, "ab"),
            ("ab", 2, "ab"),
            ("abc", 2, "ab..."),
            ("é🚀中", 2, "é🚀..."),
            ("é🚀中", 3, "é🚀中"),
            ("e\u{301}x", 2, "e\u{301}..."),
        ] {
            assert_eq!(truncate_text(text, limit), expected);
        }
    }

    #[test]
    fn human_columns_follow_chars_through_utf16() {
        let content = crate::document::DocumentContent::from_text("a\u{1F680}b\n".to_owned(), None);
        let encoding = PositionEncoding::Utf16;
        assert_eq!(
            LspCodeIntelligence::human_to_lsp(encoding, &content, 1, 2),
            Some((0, 1))
        );
        assert_eq!(
            LspCodeIntelligence::human_to_lsp(encoding, &content, 1, 3),
            Some((0, 3))
        );
        assert_eq!(
            LspCodeIntelligence::human_to_lsp(encoding, &content, 1, 5),
            None
        );
        assert_eq!(
            LspCodeIntelligence::human_position(encoding, &content, 0, 3),
            (1, 3)
        );
        // Out-of-range LSP characters clamp to the end of the line.
        assert_eq!(
            LspCodeIntelligence::human_position(encoding, &content, 0, 99),
            (1, 4)
        );
    }

    #[test]
    fn operation_uses_one_immutable_snapshot_per_file() {
        let root = std::env::temp_dir().join(format!(
            "slim-lsp-operation-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create operation cache root");
        let path = root.join("sample.rs");
        std::fs::write(&path, "fn first() {}\n").expect("write first content");

        let mut documents = OperationDocuments::new(&root, 0);
        let (_, first) = documents.load(&path).expect("first snapshot");
        std::fs::write(&path, "fn second() {}\n").expect("write changed content");
        let (_, second) = documents.load(&path).expect("cached snapshot");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.text(), "fn first() {}\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[ignore = "manual release measurement; build slim-lsp-mock first; no network"]
    async fn measure_existing_references_utility() {
        use slim_core::provider::{
            OpenAiCodexAdapter, ProviderAdapter, ProviderConfig, ProviderMessage,
        };
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("slim-lsp-mock{}", std::env::consts::EXE_SUFFIX));
        assert!(
            binary.is_file(),
            "build the slim-lsp-mock binary in the same profile first"
        );
        let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
            "http://127.0.0.1:1",
            "fixture-model",
            "fixture-token",
            "fixture-account",
        ))
        .unwrap();
        for count in [3_usize, 50, 300] {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "slim-reference-utility-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(root.join("src")).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            std::fs::write(
                root.join("Cargo.toml"),
                "[package]\nname='ref-utility'\nversion='0.1.0'\nedition='2021'\n",
            )
            .unwrap();
            let source = root.join("src/lib.rs");
            std::fs::write(&source, "pub fn target(value: usize) {}\n").unwrap();
            let file_count = count.min(5);
            let mut texts = vec![String::new(); file_count];
            let paths = (0..file_count)
                .map(|i| root.join(format!("src/use_{i}.rs")))
                .collect::<Vec<_>>();
            let mut locations = Vec::new();
            for i in 0..count {
                let file = i % file_count;
                let line = (i / file_count) as u32;
                let text = format!("fn caller_{i}() {{ target({i}); }}\r\n");
                let column = text.find("target").unwrap();
                texts[file].push_str(&text);
                locations.push(json!({"uri":url::Url::from_file_path(&paths[file]).unwrap().as_str(),
                    "range":{"start":{"line":line,"character":column},"end":{"line":line,"character":column+6}}}));
            }
            for (path, text) in paths.iter().zip(&texts) {
                std::fs::write(path, text).unwrap();
            }
            let log = root.join("wire.jsonl");
            let manager = LspCodeIntelligence::from_config(LspManagerConfig {
                idle_shutdown: None,
                server_path: Some(binary.clone()),
                server_config: json!({"mock":{"logPath":log,"responses":{"textDocument/references":locations}}}),
                ..Default::default()
            });
            let (resolved, spec) = manager.resolve(&root).unwrap();
            drop(manager.acquire(resolved, spec, None).await.unwrap());
            let query = CodeIntelPositionQuery {
                workspace: root.clone(),
                path: source,
                line: 1,
                column: 8,
                max_results: 100,
                offset: 0,
                revision: None,
                symbol: Some("target".into()),
                cancellation: None,
            };
            crate::document::read_metrics::take();
            let start = Instant::now();
            let cold = manager.references(&query).await;
            let cold_us = start.elapsed().as_micros();
            let (opens, bytes, hits) = crate::document::read_metrics::take();
            assert_eq!(opens, file_count + 1);
            assert_eq!(cold.payload["shown"], count.min(100));
            assert_eq!(cold.payload["has_more"], count > 100);
            println!("references_existing count={count} phase=cold_documents source_opens={opens} source_bytes={bytes} store_hits={hits} local_us={cold_us}");
            // Open the target files through the existing document-symbol path.
            // This setup is outside both the timing and IO measurement window.
            for path in &paths {
                manager
                    .symbols(&CodeIntelSymbolQuery {
                        workspace: root.clone(),
                        path: Some(path.clone()),
                        max_results: 1,
                        ..Default::default()
                    })
                    .await;
            }
            let mut elapsed = Vec::new();
            let mut render_times = Vec::new();
            let mut prepare_times = Vec::new();
            let mut result_bytes = 0;
            let mut next_bytes = 0;
            for _ in 0..7 {
                crate::document::read_metrics::take();
                let start = Instant::now();
                let outcome = manager.references(&query).await;
                elapsed.push(start.elapsed().as_micros());
                let (opens, bytes, hits) = crate::document::read_metrics::take();
                assert_eq!((opens, bytes, hits), (0, 0, file_count + 1));
                assert!(!outcome.meta.stale);
                let start = Instant::now();
                let rendered = slim_core::tools::render_code_intel("references", &outcome);
                render_times.push(start.elapsed().as_nanos());
                for i in 0..count.min(100) {
                    assert!(
                        rendered.contains(&format!("target({i});")),
                        "context must contain the observed call argument"
                    );
                }
                result_bytes = rendered.len();
                let messages = vec![ProviderMessage::user("Inspect the call arguments at the returned references."),
                    ProviderMessage::assistant("",vec![slim_core::provider::ProviderToolCall {
                        id:"references".into(),name:"code_intel".into(),arguments:r#"{"action":"references","path":"src/lib.rs","line":1,"column":8,"max_results":100}"#.into()
                    }]),ProviderMessage::tool("code_intel","references",rendered)];
                let start = Instant::now();
                let prepared = adapter
                    .prepare_messages_request_with_tools_checked(&messages, &[])
                    .unwrap();
                prepare_times.push(start.elapsed().as_nanos());
                next_bytes = prepared.body().len();
            }
            manager.shutdown().await;
            let logs = std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            let mut server = Vec::new();
            for request in logs
                .iter()
                .filter(|row| {
                    row["direction"] == "client_to_server"
                        && row["message"]["method"] == "textDocument/references"
                })
                .skip(1)
            {
                let response = logs
                    .iter()
                    .find(|row| {
                        row["direction"] == "server_to_client"
                            && row["message"]["id"] == request["message"]["id"]
                    })
                    .unwrap();
                server.push(u128::from(
                    response["elapsed_us"].as_u64().unwrap()
                        - request["elapsed_us"].as_u64().unwrap(),
                ));
            }
            let stats = |values: &mut Vec<u128>| {
                values.sort_unstable();
                format!(
                    "{}[{}..{}]",
                    values[values.len() / 2],
                    values[0],
                    values[values.len() - 1]
                )
            };
            println!("references_existing count={count} phase=warm n=7 invocations=1 lsp_requests=1 shown={} source_opens=0 source_bytes=0 store_hits={} result_bytes={result_bytes} next_request_bytes={next_bytes} local_us={} mock_request_to_response_us={} render_ns={} prepare_ns={}",count.min(100),file_count+1,stats(&mut elapsed),stats(&mut server),stats(&mut render_times),stats(&mut prepare_times));
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
}
