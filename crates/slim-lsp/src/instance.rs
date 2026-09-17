//! LspServerInstance: one live, initialized connection to a language server
//! (or an in-process mock in tests). Owns the transport, the negotiated
//! capabilities and position encoding, the open-document mirror, the
//! diagnostics drain and the server-request handler that keeps the server
//! from ever mutating the workspace on its own (workspace/applyEdit is
//! answered with "not applied").
//!
//! URIs are handled internally with url::Url (stable file-path semantics) and
//! converted to lsp_types::Uri only at the wire boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lsp_types::{
    ClientCapabilities, ClientInfo, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, GeneralClientCapabilities, GotoCapability, HoverClientCapabilities,
    InitializeParams, InitializeResult, InitializedParams, MarkupKind, ProgressParams,
    ProgressParamsValue, ProgressToken, PublishDiagnosticsClientCapabilities, ServerCapabilities,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentSyncClientCapabilities, VersionedTextDocumentIdentifier,
    WindowClientCapabilities, WorkDoneProgress, WorkspaceClientCapabilities, WorkspaceFolder,
};
use serde_json::{json, Value};
use slim_core::runtime::CancellationToken;
use tokio::sync::Mutex;

use crate::diagnostics::DiagnosticsStore;
use crate::discovery::ServerSpec;
use crate::document::{DocumentContent, DocumentStore, DocumentUpdate, FileStamp};
use crate::position::{encoding_from_lsp_name, PositionEncoding};
use crate::transport::{
    LspTransport, NotificationReceiver, ServerNotification, TransportError, TransportOptions,
};

const SHUTDOWN_REQUEST_GRACE: Duration = Duration::from_millis(500);

struct SyncTransaction<'a> {
    transport: &'a LspTransport,
    committed: bool,
}

impl Drop for SyncTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.transport.invalidate();
        }
    }
}

fn sync_contract(
    sync: Option<&lsp_types::TextDocumentSyncCapability>,
) -> (lsp_types::TextDocumentSyncKind, Option<bool>) {
    use lsp_types::{
        TextDocumentSyncCapability as Capability, TextDocumentSyncKind as Kind,
        TextDocumentSyncSaveOptions as Save,
    };
    let (mode, save) = match sync {
        Some(Capability::Kind(kind)) => (*kind, None),
        Some(Capability::Options(options)) => (
            options.change.unwrap_or(Kind::NONE),
            match &options.save {
                Some(Save::Supported(true)) => Some(false),
                Some(Save::SaveOptions(options)) => Some(options.include_text.unwrap_or(false)),
                _ => None,
            },
        ),
        None => (Kind::NONE, None),
    };
    // Unknown numeric kinds cannot justify incrementals; use a complete change.
    let mode = if [Kind::NONE, Kind::FULL, Kind::INCREMENTAL].contains(&mode) {
        mode
    } else {
        Kind::FULL
    };
    (mode, save)
}

fn incremental_changes(
    patch: &slim_core::codeintel::CodeIntelPatch,
    encoding: PositionEncoding,
) -> Option<Vec<TextDocumentContentChangeEvent>> {
    if patch.edits.is_empty() {
        return None;
    }
    patch
        .edits
        .iter()
        .map(|edit| {
            let position = |p: &slim_core::codeintel::CodeIntelEditPosition| {
                // An offset inside CRLF has no equivalent LSP position. Full resync
                // preserves the exact patch semantics in this unusual case.
                if p.prefix.ends_with('\r') {
                    return None;
                }
                Some(lsp_types::Position::new(
                    p.line,
                    crate::position::PositionCodec::byte_to_character(
                        encoding,
                        &p.prefix,
                        p.prefix.len(),
                    )?,
                ))
            };
            Some(TextDocumentContentChangeEvent {
                range: Some(lsp_types::Range::new(
                    position(&edit.start)?,
                    position(&edit.end)?,
                )),
                range_length: None,
                text: edit.text.clone(),
            })
        })
        .collect()
}

/// Converts an internal file URL to an lsp-types Uri (wire format).
pub fn to_lsp_uri(url: &url::Url) -> lsp_types::Uri {
    url.to_string().parse().expect("file url parses as lsp Uri")
}

/// Converts an lsp-types Uri back to an internal file URL.
pub fn uri_to_url(uri: &lsp_types::Uri) -> Option<url::Url> {
    uri.as_str().parse().ok()
}

/// File URI for an absolute path (internal url::Url).
pub fn file_uri(path: &Path) -> Option<url::Url> {
    url::Url::from_file_path(path).ok()
}

#[derive(Clone, Debug)]
pub struct ServerInstanceConfig {
    pub root: PathBuf,
    pub spec: ServerSpec,
    pub transport_options: TransportOptions,
    /// Sent as initializationOptions (server-specific).
    pub initialization_options: Value,
    /// Value answered for workspace/configuration under spec.settings_section.
    pub settings: Value,
    pub max_open_documents: usize,
}

impl ServerInstanceConfig {
    pub fn for_rust_analyzer(root: PathBuf, spec: ServerSpec) -> Self {
        Self {
            root,
            spec,
            transport_options: TransportOptions::default(),
            initialization_options: json!({ "checkOnSave": false }),
            settings: json!({ "checkOnSave": false }),
            max_open_documents: crate::document::DEFAULT_MAX_OPEN_DOCUMENTS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstanceSnapshot {
    pub ready: bool,
    pub encoding: PositionEncoding,
    pub indexing_observed: bool,
    pub indexing_active: bool,
    pub open_documents: usize,
    pub diagnostic_uris: usize,
    pub stderr_tail: String,
}

#[derive(Clone, Debug)]
pub struct DiagnosticsSnapshot {
    pub version: Option<i64>,
    pub stale: bool,
    pub total: usize,
    pub truncated: bool,
    pub items: Vec<lsp_types::Diagnostic>,
}

struct InstanceState {
    ready: bool,
    caps: Option<ServerCapabilities>,
    encoding: PositionEncoding,
    indexing_observed: bool,
    /// Tokens with an open begin and no matching end. Progress is per-token
    /// in the protocol; a single bool would let one token's end hide another
    /// token's ongoing work.
    active_progress: std::collections::HashSet<ProgressToken>,
    /// Begins that arrived while the tracked set was full. Counted as active
    /// so a dropped begin can only over-report indexing, never under-report.
    progress_overflow: usize,
    documents: DocumentStore,
    diagnostics: DiagnosticsStore,
}

/// Distinct in-flight progress tokens tracked per server. WorkDone cycles
/// number in the handful; the cap only bounds a misbehaving flood.
const MAX_TRACKED_PROGRESS_TOKENS: usize = 256;

static NEXT_INSTANCE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl InstanceState {
    fn new(max_open_documents: usize) -> Self {
        Self {
            ready: false,
            caps: None,
            encoding: PositionEncoding::Utf16,
            indexing_observed: false,
            active_progress: std::collections::HashSet::new(),
            progress_overflow: 0,
            documents: DocumentStore::new(max_open_documents),
            diagnostics: DiagnosticsStore::new(
                crate::diagnostics::DEFAULT_MAX_DIAGNOSTICS_PER_URI,
                crate::diagnostics::DEFAULT_MAX_DIAGNOSTIC_URIS,
            ),
        }
    }

    fn indexing_active(&self) -> bool {
        !self.active_progress.is_empty() || self.progress_overflow > 0
    }
}

/// Live server connection.
pub struct LspServerInstance {
    /// Process-unique identifier; a respawned server always gets a fresh id,
    /// so continuations can reject pages that would mix server generations.
    id: u64,
    state: Arc<Mutex<InstanceState>>,
    /// Ordering is per document; unrelated semantic queries remain concurrent.
    document_sync: std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<Mutex<()>>>>,
    /// Only opening/closing and LRU eviction share this lifecycle barrier.
    document_lifecycle: Mutex<()>,
    transport: LspTransport,
    config: ServerInstanceConfig,
    stderr_tail: Arc<Mutex<String>>,
    drain_task: tokio::task::JoinHandle<()>,
}

fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            workspace_folders: Some(true),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                dynamic_registration: None,
                will_save: None,
                will_save_wait_until: None,
                did_save: Some(true),
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                related_information: Some(false),
                tag_support: None,
                version_support: Some(true),
                code_description_support: None,
                data_support: None,
            }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: None,
                content_format: Some(vec![MarkupKind::PlainText, MarkupKind::Markdown]),
            }),
            definition: Some(GotoCapability {
                dynamic_registration: None,
                link_support: Some(true),
            }),
            references: Some(lsp_types::ReferenceClientCapabilities {
                dynamic_registration: None,
            }),
            document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                dynamic_registration: None,
                symbol_kind: None,
                hierarchical_document_symbol_support: Some(true),
                tag_support: None,
            }),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                lsp_types::PositionEncodingKind::UTF8,
                lsp_types::PositionEncodingKind::UTF16,
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl LspServerInstance {
    /// Opens and initializes a connection over the given io pair.
    pub async fn open(
        io: crate::transport::IoBox,
        stderr_tail: Arc<Mutex<String>>,
        config: ServerInstanceConfig,
    ) -> Result<Self, TransportError> {
        let handler = server_request_handler(
            config.settings.clone(),
            config.spec.settings_section.clone(),
        );
        let options = config.transport_options.clone();
        let (transport, notifications) = LspTransport::new(io, options, handler);
        let mut instance = Self {
            id: NEXT_INSTANCE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state: Arc::new(Mutex::new(InstanceState::new(config.max_open_documents))),
            document_sync: std::sync::Mutex::new(std::collections::HashMap::new()),
            document_lifecycle: Mutex::new(()),
            transport,
            config,
            stderr_tail,
            drain_task: tokio::spawn(async {}),
        };
        instance.initialize().await?;
        instance.drain_task = instance.spawn_drain(notifications);
        Ok(instance)
    }

    /// Main client-side entry: open + initialize + initialized.
    #[allow(deprecated)]
    async fn initialize(&self) -> Result<(), TransportError> {
        let root_str = self.config.root.to_string_lossy().into_owned();
        let root_url = file_uri(&self.config.root)
            .ok_or_else(|| TransportError::Protocol(format!("invalid root path: {root_str}")))?;
        let root_uri = to_lsp_uri(&root_url);
        let params = InitializeParams {
            process_id: None,
            root_path: Some(root_str),
            root_uri: Some(root_uri.clone()),
            initialization_options: Some(self.config.initialization_options.clone()),
            capabilities: client_capabilities(),
            trace: None,
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: self
                    .config
                    .root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "workspace".into()),
            }]),
            client_info: Some(ClientInfo {
                name: "slim".into(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            locale: None,
            work_done_progress_params: Default::default(),
        };
        let response = self
            .transport
            .request(
                "initialize",
                serde_json::to_value(&params).map_err(protocol)?,
            )
            .await?;
        let result: InitializeResult = serde_json::from_value(response)
            .map_err(|e| TransportError::Parse(format!("initialize result: {e}")))?;
        let encoding = result
            .capabilities
            .position_encoding
            .as_ref()
            .map(|kind| encoding_from_lsp_name(Some(kind.as_str())))
            .unwrap_or(PositionEncoding::Utf16);
        let mut state = self.state.lock().await;
        state.ready = true;
        state.caps = Some(result.capabilities);
        state.encoding = encoding;
        drop(state);

        self.transport
            .notify(
                "initialized",
                serde_json::to_value(InitializedParams {}).map_err(protocol)?,
            )
            .await
    }

    fn spawn_drain(&self, mut notifications: NotificationReceiver) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        let root = self.config.root.clone();
        tokio::spawn(async move {
            while let Some(notification) = notifications.recv().await {
                Self::handle_notification(&state, &root, notification).await;
            }
        })
    }

    async fn handle_notification(
        state: &Arc<Mutex<InstanceState>>,
        root: &Path,
        notification: ServerNotification,
    ) {
        match notification.method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Ok(params) = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(
                    notification.params,
                ) {
                    let Some(uri) = uri_to_url(&params.uri) else {
                        return;
                    };
                    let Some(path) = crate::path_policy::url_workspace_path(root, &uri) else {
                        return;
                    };
                    let canonical_uri = file_uri(&path).unwrap_or(uri);
                    let version = params.version.map(i64::from);
                    let mut guard = state.lock().await;
                    if version.is_some_and(|published| {
                        guard
                            .documents
                            .get(&path)
                            .is_some_and(|document| published < document.version)
                    }) {
                        return;
                    }
                    guard
                        .diagnostics
                        .set(canonical_uri, version, params.diagnostics);
                }
            }
            "$/progress" => {
                if let Ok(params) = serde_json::from_value::<ProgressParams>(notification.params) {
                    // Real rust-analyzer reports work under string tokens such as
                    // "rustAnalyzer/Roots Scanned" (never the bare
                    // "rustAnalyzer/indexing"), so every rustAnalyzer/* token
                    // is tracked; begin/end open/close that token only.
                    let tracked = matches!(params.token, ProgressToken::String(ref token) if token.starts_with("rustAnalyzer/"))
                        || matches!(params.token, ProgressToken::Number(_));
                    if tracked {
                        let ProgressParamsValue::WorkDone(progress) = params.value;
                        let mut guard = state.lock().await;
                        guard.indexing_observed = true;
                        match progress {
                            WorkDoneProgress::Begin(_) => {
                                if guard.active_progress.contains(&params.token) {
                                    // Duplicate begin: nothing changes.
                                } else if guard.active_progress.len() < MAX_TRACKED_PROGRESS_TOKENS
                                {
                                    guard.active_progress.insert(params.token.clone());
                                } else {
                                    guard.progress_overflow =
                                        guard.progress_overflow.saturating_add(1);
                                }
                            }
                            WorkDoneProgress::End(_) => {
                                if !guard.active_progress.remove(&params.token) {
                                    // Probably the end of a begin dropped
                                    // for capacity; an unmatched end still
                                    // cannot push the count below zero.
                                    guard.progress_overflow =
                                        guard.progress_overflow.saturating_sub(1);
                                }
                            }
                            WorkDoneProgress::Report(_) => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// First open sends the complete document. Later changes follow negotiation.
    pub async fn sync_document(&self, path: &Path, text: String) -> Option<i64> {
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let content = DocumentContent::from_text(text, FileStamp::for_path(&path));
        self.sync_document_content(&path, content).await
    }

    pub(crate) async fn sync_document_content(
        &self,
        path: &Path,
        content: Arc<DocumentContent>,
    ) -> Option<i64> {
        self.synchronize(path, content, None, false).await
    }

    pub async fn notify_file_changed(&self, path: &Path, text: String) {
        let content = DocumentContent::from_text(text, FileStamp::for_path(path));
        self.notify_file_changed_content(path, content).await;
    }

    pub(crate) async fn notify_file_changed_content(
        &self,
        path: &Path,
        content: Arc<DocumentContent>,
    ) {
        self.synchronize(path, content, None, true).await;
    }

    pub(crate) async fn notify_file_updated_content(
        &self,
        path: &Path,
        content: Arc<DocumentContent>,
        patch: Option<&slim_core::codeintel::CodeIntelPatch>,
    ) {
        self.synchronize(path, content, patch, true).await;
    }

    async fn synchronize(
        &self,
        path: &Path,
        content: Arc<DocumentContent>,
        patch: Option<&slim_core::codeintel::CodeIntelPatch>,
        save: bool,
    ) -> Option<i64> {
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let _document_sync = self.document_lock(&path).lock_owned().await;
        let lifecycle = self.document_lifecycle.lock().await;
        if self.transport.is_closed() {
            return None;
        }
        let language_id = self.config.spec.language_id_for(&path)?;
        let uri = file_uri(&path)?;
        let (previous, mode, save_text, encoding) = {
            let state = self.state.lock().await;
            let sync = state
                .caps
                .as_ref()
                .and_then(|caps| caps.text_document_sync.as_ref());
            let (mode, save_text) = sync_contract(sync);
            (
                state.documents.get(&path).cloned(),
                mode,
                save_text,
                state.encoding,
            )
        };
        let _lifecycle = if previous.is_none() {
            Some(lifecycle)
        } else {
            drop(lifecycle);
            None
        };
        if save && previous.is_none() {
            return None;
        }
        if let Some(previous) = &previous {
            if previous.content.text() == content.text() {
                self.state
                    .lock()
                    .await
                    .documents
                    .upsert_content(path, uri, language_id, content);
                return Some(previous.version);
            }
        }
        let version = previous
            .as_ref()
            .map_or(Some(1), |doc| doc.version.checked_add(1))?;
        if version > i32::MAX as i64 {
            self.transport.invalidate();
            return None;
        }
        // If this future is dropped after a frame, discard the connection: its
        // peer may already have advanced even though our local commit did not.
        let mut transaction = SyncTransaction {
            transport: &self.transport,
            committed: false,
        };
        self.state.lock().await.diagnostics.invalidate(&uri);
        if previous.is_none() {
            let evicted = self
                .state
                .lock()
                .await
                .documents
                .reserve_open(content.text().len());
            Self::send_evicted_did_closes(&self.transport, &evicted)
                .await
                .ok()?;
        }
        let (method, params) = if let Some(previous) = &previous {
            if mode == lsp_types::TextDocumentSyncKind::NONE {
                // No change channel exists. Do not claim the new text is mirrored.
                if save {
                    if let Some(include_text) = save_text {
                        self.send_save(&uri, &content, include_text).await.ok()?;
                    }
                }
                transaction.committed = true;
                return None;
            }
            let changes = if mode == lsp_types::TextDocumentSyncKind::INCREMENTAL {
                patch
                    .filter(|patch| patch.matches_before(previous.content.text()))
                    .and_then(|patch| incremental_changes(patch, encoding))
            } else {
                None
            };
            (
                "textDocument/didChange",
                serde_json::to_value(DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier {
                        uri: to_lsp_uri(&uri),
                        version: wire_version(version),
                    },
                    content_changes: changes.unwrap_or_else(|| {
                        vec![TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: content.text().to_owned(),
                        }]
                    }),
                })
                .ok()?,
            )
        } else {
            (
                "textDocument/didOpen",
                serde_json::to_value(DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: to_lsp_uri(&uri),
                        language_id: language_id.to_owned(),
                        version: wire_version(version),
                        text: content.text().to_owned(),
                    },
                })
                .ok()?,
            )
        };
        self.transport.notify(method, params).await.ok()?;
        if save {
            if let Some(include_text) = save_text {
                self.send_save(&uri, &content, include_text).await.ok()?;
            }
        }
        let update = {
            let mut state = self.state.lock().await;
            if self.transport.is_closed() {
                return None;
            }
            if state.documents.get(&path).map(|doc| doc.version)
                != previous.as_ref().map(|doc| doc.version)
            {
                // Eviction raced an in-flight update. Its increment cannot be
                // replayed as a new open; rebuild on the next connection.
                return None;
            }
            state
                .documents
                .upsert_content(path, uri, language_id, content)
        };
        let evicted = match update {
            DocumentUpdate::Opened { evicted, .. } | DocumentUpdate::Changed { evicted, .. } => {
                evicted
            }
            DocumentUpdate::Unchanged { .. } => Vec::new(),
        };
        Self::send_evicted_did_closes(&self.transport, &evicted)
            .await
            .ok()?;
        transaction.committed = true;
        Some(version)
    }

    async fn send_save(
        &self,
        uri: &url::Url,
        content: &DocumentContent,
        include_text: bool,
    ) -> Result<(), TransportError> {
        self.transport
            .notify(
                "textDocument/didSave",
                serde_json::to_value(DidSaveTextDocumentParams {
                    text_document: TextDocumentIdentifier {
                        uri: to_lsp_uri(uri),
                    },
                    text: include_text.then(|| content.text().to_owned()),
                })
                .map_err(|error| TransportError::Protocol(error.to_string()))?,
            )
            .await
    }

    fn document_lock(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.document_sync.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(path).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(path.to_owned(), Arc::downgrade(&lock));
        lock
    }

    /// Fire-and-forget didClose for a list of evicted documents (LRU eviction).
    async fn send_evicted_did_closes(
        transport: &LspTransport,
        evicted: &[crate::document::OpenDocument],
    ) -> Result<(), TransportError> {
        for doc in evicted {
            let params = lsp_types::DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: to_lsp_uri(&doc.uri),
                },
            };
            transport
                .notify(
                    "textDocument/didClose",
                    serde_json::to_value(&params).unwrap_or(Value::Null),
                )
                .await?;
        }
        Ok(())
    }

    /// Low-level typed request used by the manager for LSP queries.
    pub async fn request_value(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, TransportError> {
        self.transport.request(method, params).await
    }

    pub async fn request_value_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, TransportError> {
        self.transport
            .request_cancellable(method, params, cancellation)
            .await
    }

    pub async fn document_version_async(&self, path: &Path) -> Option<i64> {
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let _document_sync = self.document_lock(&path).lock_owned().await;
        if self.transport.is_closed() {
            return None;
        }
        let state = self.state.lock().await;
        state.documents.get(&path).map(|doc| doc.version)
    }

    pub(crate) async fn document_content_snapshot_async(
        &self,
        path: &Path,
    ) -> Option<(i64, Arc<DocumentContent>)> {
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let _document_sync = self.document_lock(&path).lock_owned().await;
        if self.transport.is_closed() {
            return None;
        }
        let mut state = self.state.lock().await;
        state
            .documents
            .get_and_touch(&path)
            .map(|document| (document.version, Arc::clone(&document.content)))
    }

    pub async fn diagnostics_snapshot(
        &self,
        uri: &url::Url,
        include_info: bool,
    ) -> Option<DiagnosticsSnapshot> {
        let path = crate::path_policy::url_workspace_path(&self.config.root, uri)?;
        let _document_sync = self.document_lock(&path).lock_owned().await;
        if self.transport.is_closed() {
            return None;
        }
        let uri = file_uri(&path)?;
        let state = self.state.lock().await;
        let stored = state.diagnostics.get(&uri)?;
        let items = state
            .diagnostics
            .agent_view(&uri, include_info)
            .into_iter()
            .cloned()
            .collect();
        Some(DiagnosticsSnapshot {
            version: stored.version,
            stale: stored.stale,
            total: if include_info {
                stored.total
            } else {
                stored.total_without_info
            },
            truncated: stored.truncated,
            items,
        })
    }

    /// URIs that currently have stored diagnostics from this server.
    pub async fn diagnostic_uris(&self) -> Vec<url::Url> {
        let state = self.state.lock().await;
        state.diagnostics.uris().cloned().collect()
    }

    pub async fn diagnostic_totals(&self, include_info: bool) -> (Option<usize>, bool, u64) {
        let state = self.state.lock().await;
        let (total, truncated) = state.diagnostics.totals(include_info);
        (total, truncated, state.diagnostics.revision())
    }

    pub fn language_id_for(&self, path: &Path) -> Option<&str> {
        self.config.spec.language_id_for(path)
    }

    pub async fn document_is_open(&self, path: &Path) -> bool {
        let state = self.state.lock().await;
        state.documents.is_open(path)
    }

    pub async fn snapshot(&self) -> InstanceSnapshot {
        let (ready, encoding, indexing_observed, indexing_active, open_documents, diagnostic_uris) = {
            let state = self.state.lock().await;
            (
                state.ready,
                state.encoding,
                state.indexing_observed,
                state.indexing_active(),
                state.documents.open_documents().len(),
                state.diagnostics.len(),
            )
        };
        let stderr_tail = self.stderr_tail.lock().await.clone();
        InstanceSnapshot {
            ready,
            encoding,
            indexing_observed,
            indexing_active,
            open_documents,
            diagnostic_uris,
            stderr_tail,
        }
    }

    /// Bounded graceful shutdown. The pool owns process reaping and escalates
    /// to a process-tree kill when the child ignores this protocol request.
    pub async fn shutdown(&self) {
        let cancellation = CancellationToken::new();
        let timeout_cancellation = cancellation.clone();
        let request =
            self.transport
                .request_cancellable("shutdown", json!(null), Some(&cancellation));
        tokio::pin!(request);
        tokio::select! {
            _ = &mut request => {}
            _ = tokio::time::sleep(SHUTDOWN_REQUEST_GRACE) => {
                timeout_cancellation.cancel();
                let _ = request.await;
            }
        }
        let _ = self
            .transport
            .notify_with_timeout("exit", json!(null), SHUTDOWN_REQUEST_GRACE)
            .await;
        self.drain_task.abort();
    }

    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// Unique per process and never reused; a respawn always differs.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// True once the transport observed EOF or a framing error. The pool
    /// uses this as its liveness signal so a dead server is evicted instead
    /// of being handed to the next caller.
    pub fn is_closed(&self) -> bool {
        self.transport.is_closed()
    }

    pub fn spec(&self) -> &ServerSpec {
        &self.config.spec
    }
}

/// Answers server-initiated requests. Never lets the server write files, and
/// never acknowledges a capability the client does not implement: unsupported
/// requests fail with MethodNotFound so the server falls back instead of
/// assuming silent support (e.g. file-watching registrations).
fn server_request_handler(
    settings: Value,
    settings_section: String,
) -> crate::transport::ServerRequestHandler {
    Box::new(move |method, params| match method {
        "workspace/configuration" => {
            let items = params
                .and_then(|value| value.get("items"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // One answer per requested item, positions aligned. A section
            // asks for the *content* under that key: wrapping the settings
            // in `{ settings_section: ... }` again would double-nest them and
            // the server would silently drop the real values. Subsections of
            // ours resolve to their subtree; unrelated sections get null.
            Ok(Value::Array(
                items
                    .iter()
                    .map(|item| match item.get("section").and_then(Value::as_str) {
                        None | Some("") => {
                            let mut row = serde_json::Map::new();
                            row.insert(settings_section.clone(), settings.clone());
                            Value::Object(row)
                        }
                        Some(section) if section == settings_section => settings.clone(),
                        Some(section) => section
                            .strip_prefix(settings_section.as_str())
                            .and_then(|rest| rest.strip_prefix('.'))
                            .and_then(|rest| {
                                rest.split('.')
                                    .try_fold(&settings, |node, key| node.get(key))
                                    .cloned()
                            })
                            .unwrap_or(Value::Null),
                    })
                    .collect(),
            ))
        }
        "workspace/applyEdit" => Ok(json!({ "applied": false })),
        "window/workDoneProgress/create" => Ok(json!(null)),
        "window/showMessageRequest" => Ok(json!(null)),
        _ => Err(format!("unsupported request: {method}")),
    })
}

fn protocol(error: serde_json::Error) -> TransportError {
    TransportError::Protocol(error.to_string())
}

fn wire_version(version: i64) -> i32 {
    version.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_fixture(capacity: usize) -> (LspServerInstance, tokio::io::DuplexStream, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "slim-sync-fault-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("main.rs");
        std::fs::write(&path, "old").unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let (client, server) = tokio::io::duplex(capacity);
        let (transport, notifications) = LspTransport::new(
            Box::new(client),
            TransportOptions::default(),
            server_request_handler(Value::Null, "test".into()),
        );
        let mut state = InstanceState::new(4);
        state.ready = true;
        state.caps = Some(serde_json::from_value(json!({"textDocumentSync": 2})).unwrap());
        state
            .documents
            .upsert(path.clone(), file_uri(&path).unwrap(), "rust", "old".into());
        let mut instance = LspServerInstance {
            id: NEXT_INSTANCE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state: Arc::new(Mutex::new(state)),
            document_sync: std::sync::Mutex::new(std::collections::HashMap::new()),
            document_lifecycle: Mutex::new(()),
            transport,
            config: ServerInstanceConfig::for_rust_analyzer(
                root,
                crate::discovery::rust_analyzer_spec("unused".into()),
            ),
            stderr_tail: Arc::new(Mutex::new(String::new())),
            drain_task: tokio::spawn(async {}),
        };
        instance.drain_task = instance.spawn_drain(notifications);
        (instance, server, path)
    }

    #[tokio::test]
    async fn failed_or_aborted_sync_never_commits_the_new_version() {
        use tokio::io::AsyncReadExt;
        for phase in ["before", "partial"] {
            let (instance, mut server, path) = sync_fixture(32);
            let future = instance.sync_document(&path, "new".repeat(200));
            tokio::pin!(future);
            if phase == "before" {
                drop(server);
                assert_eq!(future.as_mut().await, None);
            } else {
                // A nonempty prefix proves the frame actually started.
                let mut prefix = [0u8; 8];
                tokio::select! {
                    result = &mut future => panic!("sync completed before frame drained: {result:?}"),
                    result = server.read_exact(&mut prefix) => { result.unwrap(); },
                }
                drop(server);
                assert_eq!(future.as_mut().await, None);
            }
            assert!(instance.is_closed());
            assert_eq!(
                instance
                    .state
                    .lock()
                    .await
                    .documents
                    .get(&path)
                    .unwrap()
                    .version,
                1
            );
            let _ = std::fs::remove_dir_all(&instance.config.root);
        }
    }

    #[tokio::test]
    async fn cancelled_sync_closes_generation_and_document_waiters_do_not_see_partial_state() {
        use tokio::io::AsyncReadExt;
        let (instance, mut server, path) = sync_fixture(32);
        {
            let future = instance.sync_document(&path, "new".repeat(200));
            tokio::pin!(future);
            let mut prefix = [0u8; 8];
            tokio::select! {
                result = &mut future => panic!("unexpected completion: {result:?}"),
                result = server.read_exact(&mut prefix) => { result.unwrap(); },
            }
            let first = instance.document_content_snapshot_async(&path);
            let second = instance.document_content_snapshot_async(&path);
            tokio::pin!(first, second);
            tokio::select! {
                _ = &mut first => panic!("first query observed uncommitted sync"),
                _ = &mut second => panic!("second query observed uncommitted sync"),
                _ = tokio::task::yield_now() => {},
            }
            let other = instance.config.root.join("other.rs");
            std::fs::write(&other, "other").unwrap();
            assert_eq!(instance.document_version_async(&other).await, None);
        }
        assert!(instance.is_closed());
        assert_eq!(
            instance
                .state
                .lock()
                .await
                .documents
                .get(&path)
                .unwrap()
                .version,
            1
        );
        assert!(instance
            .document_content_snapshot_async(&path)
            .await
            .is_none());
        let _ = std::fs::remove_dir_all(&instance.config.root);
    }

    #[test]
    #[ignore = "release CPU measurement of actual incremental range conversion and serialization"]
    fn measure_incremental_range_serialization() {
        use slim_core::codeintel::{CodeIntelEditPosition, CodeIntelPatch, CodeIntelTextEdit};
        use std::time::Instant;
        fn stats(label: &str, samples: &mut [u128]) {
            samples.sort_unstable();
            println!(
                "{label} n={} median_ns={} min_ns={} max_ns={}",
                samples.len(),
                samples[samples.len() / 2],
                samples[0],
                samples[samples.len() - 1]
            );
        }
        for size in [1024usize, 64 * 1024, 1024 * 1024] {
            for replacement_bytes in [8usize, 4096] {
                let before = format!("fn old() {{}}\n//{}", "x".repeat(size));
                let replacement = "z".repeat(replacement_bytes);
                let after = before.replacen("old", &replacement, 1);
                let patch = CodeIntelPatch::new(
                    &before,
                    vec![CodeIntelTextEdit {
                        start: CodeIntelEditPosition {
                            line: 0,
                            prefix: "fn ".into(),
                        },
                        end: CodeIntelEditPosition {
                            line: 0,
                            prefix: "fn old".into(),
                        },
                        text: replacement,
                    }],
                );
                let mut ranges = Vec::new();
                let mut guards = Vec::new();
                let mut full_json = Vec::new();
                let mut incremental_json = Vec::new();
                for _ in 0..101 {
                    let start = Instant::now();
                    let changes = std::hint::black_box(
                        incremental_changes(std::hint::black_box(&patch), PositionEncoding::Utf16)
                            .unwrap(),
                    );
                    ranges.push(start.elapsed().as_nanos());
                    let start = Instant::now();
                    assert!(std::hint::black_box(
                        patch.matches_before(std::hint::black_box(&before))
                    ));
                    guards.push(start.elapsed().as_nanos());
                    for (events, times) in [
                        (
                            vec![TextDocumentContentChangeEvent {
                                range: None,
                                range_length: None,
                                text: after.clone(),
                            }],
                            &mut full_json,
                        ),
                        (changes, &mut incremental_json),
                    ] {
                        let params = DidChangeTextDocumentParams {
                            text_document: VersionedTextDocumentIdentifier {
                                uri: "file:///fixture.rs".parse().unwrap(),
                                version: 2,
                            },
                            content_changes: events,
                        };
                        let start = Instant::now();
                        let value = serde_json::to_value(&params).unwrap();
                        let bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":value})).unwrap();
                        std::hint::black_box(bytes);
                        times.push(start.elapsed().as_nanos());
                    }
                }
                let label = format!("size={size} replacement={replacement_bytes}");
                stats(&format!("{label} range_codec"), &mut ranges);
                stats(&format!("{label} before_digest_guard"), &mut guards);
                stats(&format!("{label} full_json"), &mut full_json);
                stats(&format!("{label} incremental_json"), &mut incremental_json);
            }
        }
    }

    fn progress_notification(token: serde_json::Value, kind: &str) -> ServerNotification {
        ServerNotification {
            method: "$/progress".to_owned(),
            params: serde_json::json!({
                "token": token,
                "value": { "kind": kind, "title": "Roots Scanned" }
            }),
        }
    }

    #[test]
    fn configuration_answers_per_requested_section() {
        let handler = server_request_handler(
            json!({ "checkOnSave": false, "cargo": { "allTargets": true } }),
            "rust-analyzer".into(),
        );
        let response = handler(
            "workspace/configuration",
            Some(&json!({
                "items": [
                    { "section": "rust-analyzer" },
                    { "section": "rust-analyzer.cargo" },
                    { "section": "rust-analyzer.check" },
                    { "section": "rust-analyzerX" },
                    { "section": "other" },
                    {},
                ]
            })),
        )
        .expect("configuration answered");
        assert_eq!(
            response,
            json!([
                { "checkOnSave": false, "cargo": { "allTargets": true } },
                { "allTargets": true },
                null,
                null,
                null,
                { "rust-analyzer": { "checkOnSave": false, "cargo": { "allTargets": true } } },
            ]),
            "section content must be answered verbatim, never re-wrapped"
        );
        let empty = handler("workspace/configuration", Some(&json!({ "items": [] })))
            .expect("empty configuration answered");
        assert_eq!(empty, json!([]));
        assert!(handler("client/registerCapability", None).is_err());
        assert_eq!(
            handler("workspace/applyEdit", None).expect("applyEdit answered"),
            json!({ "applied": false })
        );
    }

    #[tokio::test]
    async fn rust_analyzer_work_tokens_drive_indexing_flag() {
        let state = Arc::new(Mutex::new(InstanceState::new(8)));
        let root = Path::new("D:/demo");
        LspServerInstance::handle_notification(
            &state,
            root,
            progress_notification(serde_json::json!("rustAnalyzer/Roots Scanned"), "begin"),
        )
        .await;
        {
            let guard = state.lock().await;
            assert!(guard.indexing_observed);
            assert!(guard.indexing_active());
        }
        LspServerInstance::handle_notification(
            &state,
            root,
            progress_notification(serde_json::json!("rustAnalyzer/Roots Scanned"), "report"),
        )
        .await;
        assert!(state.lock().await.indexing_active());
        LspServerInstance::handle_notification(
            &state,
            root,
            progress_notification(serde_json::json!("rustAnalyzer/Roots Scanned"), "end"),
        )
        .await;
        assert!(!state.lock().await.indexing_active());
    }

    #[tokio::test]
    async fn one_tokens_end_does_not_close_another_tokens_begin() {
        let state = Arc::new(Mutex::new(InstanceState::new(8)));
        let root = Path::new("D:/demo");
        for (token, kind) in [
            ("rustAnalyzer/Roots Scanned", "begin"),
            ("rustAnalyzer/Indexing", "begin"),
            ("rustAnalyzer/Roots Scanned", "end"),
        ] {
            LspServerInstance::handle_notification(
                &state,
                root,
                progress_notification(serde_json::json!(token), kind),
            )
            .await;
        }
        let guard = state.lock().await;
        assert!(guard.indexing_active(), "the Indexing token is still open");
        assert_eq!(guard.active_progress.len(), 1);
    }

    #[tokio::test]
    async fn unrelated_progress_token_leaves_indexing_flag_clear() {
        let state = Arc::new(Mutex::new(InstanceState::new(8)));
        let root = Path::new("D:/demo");
        LspServerInstance::handle_notification(
            &state,
            root,
            progress_notification(serde_json::json!("otherServer/work"), "begin"),
        )
        .await;
        let guard = state.lock().await;
        assert!(!guard.indexing_observed);
        assert!(!guard.indexing_active());
    }
}
