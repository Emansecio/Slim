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

    /// Starts rust-analyzer in the background so the first code_intel call
    /// does not pay process spawn on the tool path.
    pub fn warm_workspace(self: &Arc<Self>, workspace: PathBuf) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let Ok((root, spec)) = this.resolve(&workspace) else {
                return;
            };
            let _ = this.acquire(root, spec).await;
        });
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
    ) -> Result<AcquiredServer, CodeIntelOutcome> {
        let lease = self
            .pool
            .acquire(
                root,
                spec,
                &self.config.server_config,
                self.transport_options(),
                self.config.max_open_documents,
            )
            .await
            .map_err(|error| CodeIntelOutcome::unavailable("rust-analyzer", &error.to_string()))?;
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
    fn indexing_state(
        snapshot: &crate::instance::InstanceSnapshot,
    ) -> (CodeIntelServerState, CodeIntelCompleteness) {
        if snapshot.indexing_active {
            (
                CodeIntelServerState::Indexing,
                CodeIntelCompleteness::Partial,
            )
        } else {
            (CodeIntelServerState::Ready, CodeIntelCompleteness::Complete)
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
            line.char_indices().nth(char_index).map(|(index, _)| index)?
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
    let mut truncated: String = text.chars().take(max_chars).collect();
    if truncated.chars().count() < text.chars().count() {
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
        let instance = match self.acquire(root.clone(), spec).await {
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
                    .document_meta(
                        &instance,
                        &document,
                        state,
                        completeness,
                        started,
                    )
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
        let mut file = String::new();
        let mut line = 1_u32;
        let mut column = 1_u32;
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
            if let Some((target, target_line, target_char)) = locations
                .iter()
                .find_map(|location| parse_location(&mut documents, location))
            {
                if let Some(relative) = documents.relative_path(&target) {
                    found = true;
                    file = relative;
                    if let Some((_path, content)) =
                        Self::operation_document(&instance, &mut documents, &target).await
                    {
                        (line, column) =
                            Self::human_position(encoding, &content, target_line, target_char);
                    }
                }
            }
        }
        CodeIntelOutcome {
            meta: self
                .document_meta(&instance, &document, state, completeness, started)
                .await,
            payload: json!({
                "found": found,
                "file": file,
                "line": line,
                "column": column,
                "symbol": query.symbol.clone(),
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
        let instance = match self.acquire(root.clone(), spec).await {
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
        let locations = serde_json::from_value::<Vec<Location>>(response_value).unwrap_or_default();
        let snapshot = instance.snapshot().await;
        let mut by_file: BTreeMap<String, Vec<(u32, u32, Option<String>)>> = BTreeMap::new();
        let mut total = 0_usize;
        let mut shown = 0_usize;
        for location in locations {
            let Some((path, target_line, target_char)) = parse_location(&mut documents, &location)
            else {
                continue;
            };
            let Some(relative) = documents.relative_path(&path) else {
                continue;
            };
            total = total.saturating_add(1);
            if shown >= max_results {
                continue;
            }
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
                "shown": shown,
                "has_more": total > shown,
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
        let instance = match self.acquire(root.clone(), spec).await {
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
        let payload = match serde_json::from_value::<lsp_types::Hover>(response_value) {
            Ok(hover) => {
                let raw = hover_text(&hover.contents);
                let text = truncate_text(&raw, MAX_HOVER_TEXT_CHARS);
                json!({
                    "found": !raw.is_empty(),
                    "text": text,
                    "truncated": text != raw,
                })
            }
            Err(_) => json!({ "found": false, "text": "", "truncated": false }),
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
        let instance = match self.acquire(root.clone(), spec).await {
            Ok(instance) => instance,
            Err(outcome) => return outcome,
        };
        let snapshot = instance.snapshot().await;
        let encoding = snapshot.encoding;
        let (state, completeness) = Self::indexing_state(&snapshot);
        let mut documents = OperationDocuments::new(&root, self.workspace_revision(&root));

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
            let response = match serde_json::from_value::<DocumentSymbolResponse>(response_value) {
                Ok(response) => response,
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
            let mut symbols = Vec::new();
            match response {
                DocumentSymbolResponse::Flat(flat) => {
                    for item in flat {
                        if symbols.len() >= max_results {
                            break;
                        }
                        let Some((target, target_line, target_char)) =
                            parse_location(&mut documents, &item.location)
                        else {
                            continue;
                        };
                        let Some(file) = documents.relative_path(&target) else {
                            continue;
                        };
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
                    while let Some((item, depth)) = stack.pop() {
                        if symbols.len() >= max_results {
                            break;
                        }
                        let indent = "  ".repeat(depth);
                        let (line, column) = Self::human_position(
                            encoding,
                            &document.content,
                            item.selection_range.start.line,
                            item.selection_range.start.character,
                        );
                        symbols.push(json!({
                            "name": format!("{indent}{}", item.name),
                            "kind": symbol_kind_name(item.kind),
                            "detail": item.detail,
                            "file": file,
                            "line": line,
                            "column": column,
                        }));
                        if let Some(children) = item.children {
                            for child in children.into_iter().rev() {
                                stack.push((child, depth.saturating_add(1)));
                            }
                        }
                    }
                }
            }
            return CodeIntelOutcome {
                meta: self
                    .document_meta(&instance, &document, state, completeness, started)
                    .await,
                payload: json!({
                    "kind": "document",
                    "shown": symbols.len(),
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
        let response = match serde_json::from_value::<WorkspaceSymbolResponse>(response_value) {
            Ok(response) => response,
            Err(error) => {
                return Self::degraded(&format!("workspace symbols response: {error}"), started);
            }
        };
        let mut symbols = Vec::new();
        match response {
            WorkspaceSymbolResponse::Flat(flat) => {
                for item in flat {
                    if symbols.len() >= max_results {
                        break;
                    }
                    let Some((target, target_line, target_char)) =
                        parse_location(&mut documents, &item.location)
                    else {
                        continue;
                    };
                    let Some(file) = documents.relative_path(&target) else {
                        continue;
                    };
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
                for item in nested {
                    if symbols.len() >= max_results {
                        break;
                    }
                    match item.location {
                        OneOf::Left(location) => {
                            let Some(target) = documents.resolve_uri(&location.uri) else {
                                continue;
                            };
                            let Some(file) = documents.relative_path(&target) else {
                                continue;
                            };
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
        CodeIntelOutcome {
            meta: Self::base_meta(state, completeness, None, false, started),
            payload: json!({
                "kind": "workspace",
                "query": query.query.clone(),
                "shown": symbols.len(),
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
        let instance = match self.acquire(root.clone(), spec).await {
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
            let mut rows = Vec::new();
            if let Some(diagnostics) = diagnostics {
                for item in diagnostics.items.into_iter().take(max_results) {
                    rows.push(Self::diagnostic_row(encoding, &document.content, item));
                }
            }
            let stale = self.document_is_stale(&instance, &document).await
                || diagnostic_version.is_some_and(|version| version != document.version);
            let file = documents.relative_path(&document.path).unwrap_or_default();
            return CodeIntelOutcome {
                meta: Self::base_meta(state, completeness, Some(document.version), stale, started),
                payload: json!({
                    "files": [{
                        "file": file,
                        "count": rows.len(),
                        "document_version": document.version,
                        "diagnostic_version": diagnostic_version,
                        "stale": stale,
                        "diagnostics": rows,
                    }]
                }),
            };
        }

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
            let stale = diagnostics
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
        CodeIntelOutcome {
            meta: Self::base_meta(state, completeness, None, any_stale, started),
            payload: json!({
                "shown": total,
                "files": files,
            }),
        }
    }

    async fn notify_file_changed(
        &self,
        workspace: &Path,
        path: &Path,
        text: Option<String>,
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
            crate::document::DocumentContent::from_text(text, crate::document::FileStamp::for_path(&path))
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
            .notify_file_changed_content(&path, content)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_columns_follow_chars_through_utf16() {
        let content =
            crate::document::DocumentContent::from_text("a\u{1F680}b\n".to_owned(), None);
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
}
