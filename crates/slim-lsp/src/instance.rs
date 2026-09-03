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
    ClientCapabilities, ClientInfo, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, GeneralClientCapabilities,
    GotoCapability, HoverClientCapabilities, InitializeParams, InitializeResult, InitializedParams,
    MarkupKind, ProgressParams, ProgressParamsValue, ProgressToken,
    PublishDiagnosticsClientCapabilities, ServerCapabilities, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentSyncClientCapabilities, VersionedTextDocumentIdentifier, WindowClientCapabilities,
    WorkDoneProgress, WorkspaceClientCapabilities, WorkspaceFolder,
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

/// Latest negotiated LSP protocol version advertised in initialize.
pub const PROTOCOL_VERSION: &str = "3.17";
const SHUTDOWN_REQUEST_GRACE: Duration = Duration::from_millis(500);

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
    pub items: Vec<lsp_types::Diagnostic>,
}

struct InstanceState {
    ready: bool,
    caps: Option<ServerCapabilities>,
    encoding: PositionEncoding,
    indexing_observed: bool,
    indexing_active: bool,
    documents: DocumentStore,
    diagnostics: DiagnosticsStore,
}

impl InstanceState {
    fn new(max_open_documents: usize) -> Self {
        Self {
            ready: false,
            caps: None,
            encoding: PositionEncoding::Utf16,
            indexing_observed: false,
            indexing_active: false,
            documents: DocumentStore::new(max_open_documents),
            diagnostics: DiagnosticsStore::new(
                crate::diagnostics::DEFAULT_MAX_DIAGNOSTICS_PER_URI,
                crate::diagnostics::DEFAULT_MAX_DIAGNOSTIC_URIS,
            ),
        }
    }
}

/// Live server connection.
pub struct LspServerInstance {
    state: Arc<Mutex<InstanceState>>,
    /// Serializes didOpen/didChange/didSave/didClose sequences for this server.
    document_sync: Mutex<()>,
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
                did_save: None,
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
            state: Arc::new(Mutex::new(InstanceState::new(config.max_open_documents))),
            document_sync: Mutex::new(()),
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
                    // drives the indexing flag; begin/end pairs toggle it.
                    let tracked = matches!(params.token, ProgressToken::String(ref token) if token.starts_with("rustAnalyzer/"))
                        || matches!(params.token, ProgressToken::Number(_));
                    if tracked {
                        let ProgressParamsValue::WorkDone(progress) = params.value;
                        let mut guard = state.lock().await;
                        guard.indexing_observed = true;
                        match progress {
                            WorkDoneProgress::Begin(_) => guard.indexing_active = true,
                            WorkDoneProgress::End(_) => guard.indexing_active = false,
                            WorkDoneProgress::Report(_) => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Full-text sync. First open sends didOpen; later changes send didChange.
    /// Returns the new document version when the path is served by this
    /// server's language, otherwise None (not served: no sync).
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
        let _document_sync = self.document_sync.lock().await;
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let language_id = self.config.spec.language_id_for(&path)?;
        let uri = file_uri(&path)?;
        let update = {
            let mut state = self.state.lock().await;
            state
                .documents
                .upsert_content(path, uri.clone(), language_id, content)
        };

        match update {
            DocumentUpdate::Opened { document, evicted } => {
                Self::send_evicted_did_closes(&self.transport, &evicted).await;
                let params = DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: to_lsp_uri(&uri),
                        language_id: language_id.to_owned(),
                        version: wire_version(document.version),
                        text: document.content.text().to_owned(),
                    },
                };
                let _ = self
                    .transport
                    .notify(
                        "textDocument/didOpen",
                        serde_json::to_value(&params).unwrap_or(Value::Null),
                    )
                    .await;
                Some(document.version)
            }
            DocumentUpdate::Changed { document, evicted } => {
                Self::send_evicted_did_closes(&self.transport, &evicted).await;
                let params = DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier {
                        uri: to_lsp_uri(&uri),
                        version: wire_version(document.version),
                    },
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: document.content.text().to_owned(),
                    }],
                };
                let _ = self
                    .transport
                    .notify(
                        "textDocument/didChange",
                        serde_json::to_value(&params).unwrap_or(Value::Null),
                    )
                    .await;
                Some(document.version)
            }
            DocumentUpdate::Unchanged { version } => Some(version),
        }
    }

    /// didChange + didSave for a file just written by the agent.
    pub async fn notify_file_changed(&self, path: &Path, text: String) {
        let Some(path) = crate::path_policy::existing_workspace_path(&self.config.root, path)
        else {
            return;
        };
        let content = DocumentContent::from_text(text, FileStamp::for_path(&path));
        self.notify_file_changed_content(&path, content).await;
    }

    pub(crate) async fn notify_file_changed_content(
        &self,
        path: &Path,
        content: Arc<DocumentContent>,
    ) {
        let _document_sync = self.document_sync.lock().await;
        let Some(path) = crate::path_policy::existing_workspace_path(&self.config.root, path)
        else {
            return;
        };
        let Some(language_id) = self.config.spec.language_id_for(&path) else {
            return;
        };
        let Some(uri) = file_uri(&path) else {
            return;
        };
        let update = {
            let mut state = self.state.lock().await;
            if !state.documents.is_open(&path) {
                return;
            }
            state
                .documents
                .upsert_content(path, uri.clone(), language_id, content)
        };
        let (document, evicted) = match update {
            DocumentUpdate::Opened { document, evicted }
            | DocumentUpdate::Changed { document, evicted } => (document, evicted),
            DocumentUpdate::Unchanged { .. } => return,
        };
        Self::send_evicted_did_closes(&self.transport, &evicted).await;

        let did_change_params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: to_lsp_uri(&uri),
                version: wire_version(document.version),
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: document.content.text().to_owned(),
            }],
        };
        let _ = self
            .transport
            .notify(
                "textDocument/didChange",
                serde_json::to_value(&did_change_params).unwrap_or(Value::Null),
            )
            .await;
        let save_params = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: to_lsp_uri(&uri),
            },
            text: Some(document.content.text().to_owned()),
        };
        let _ = self
            .transport
            .notify(
                "textDocument/didSave",
                serde_json::to_value(&save_params).unwrap_or(Value::Null),
            )
            .await;
    }

    /// Fire-and-forget didClose for a list of evicted documents (LRU eviction).
    async fn send_evicted_did_closes(
        transport: &LspTransport,
        evicted: &[crate::document::OpenDocument],
    ) {
        for doc in evicted {
            let params = lsp_types::DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: to_lsp_uri(&doc.uri),
                },
            };
            let _ = transport
                .notify(
                    "textDocument/didClose",
                    serde_json::to_value(&params).unwrap_or(Value::Null),
                )
                .await;
        }
    }

    /// didClose for a document (LRU eviction or shutdown path).
    pub async fn close_document(&self, path: &Path) {
        let _document_sync = self.document_sync.lock().await;
        let Some(path) = crate::path_policy::existing_workspace_path(&self.config.root, path)
        else {
            return;
        };
        let Some(uri) = file_uri(&path) else {
            return;
        };
        let removed = {
            let mut state = self.state.lock().await;
            state.documents.remove(&path).is_some()
        };
        if !removed {
            return;
        }
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: to_lsp_uri(&uri),
            },
        };
        let _ = self
            .transport
            .notify(
                "textDocument/didClose",
                serde_json::to_value(&params).unwrap_or(Value::Null),
            )
            .await;
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
        let state = self.state.lock().await;
        state.documents.get(&path).map(|doc| doc.version)
    }

    pub async fn document_snapshot_async(&self, path: &Path) -> Option<(i64, String)> {
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let state = self.state.lock().await;
        state
            .documents
            .get(&path)
            .map(|document| (document.version, document.text.to_string()))
    }

    pub(crate) async fn document_content_snapshot_async(
        &self,
        path: &Path,
    ) -> Option<(i64, Arc<DocumentContent>)> {
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let state = self.state.lock().await;
        state
            .documents
            .get(&path)
            .map(|document| (document.version, Arc::clone(&document.content)))
    }

    pub async fn diagnostics_snapshot(
        &self,
        uri: &url::Url,
        include_info: bool,
    ) -> Option<DiagnosticsSnapshot> {
        let path = crate::path_policy::url_workspace_path(&self.config.root, uri)?;
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
            items,
        })
    }

    pub async fn diagnostics_for(
        &self,
        uri: &url::Url,
        include_info: bool,
    ) -> Vec<lsp_types::Diagnostic> {
        self.diagnostics_snapshot(uri, include_info)
            .await
            .map(|snapshot| snapshot.items)
            .unwrap_or_default()
    }

    /// URIs that currently have stored diagnostics from this server.
    pub async fn diagnostic_uris(&self) -> Vec<url::Url> {
        let state = self.state.lock().await;
        state.diagnostics.uris().cloned().collect()
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
                state.indexing_active,
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
        let _ = self.transport.notify("exit", json!(null)).await;
        self.drain_task.abort();
    }

    pub fn root(&self) -> &Path {
        &self.config.root
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
            // One answer per requested item, positions aligned: only our
            // section carries settings, anything else is explicitly null.
            Ok(Value::Array(
                items
                    .iter()
                    .map(|item| {
                        let section = item.get("section").and_then(Value::as_str);
                        if section.is_none_or(|section| section == settings_section) {
                            let mut row = serde_json::Map::new();
                            row.insert(settings_section.clone(), settings.clone());
                            Value::Object(row)
                        } else {
                            Value::Null
                        }
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
        let handler =
            server_request_handler(json!({ "checkOnSave": false }), "rust-analyzer".into());
        let response = handler(
            "workspace/configuration",
            Some(&json!({ "items": [{ "section": "rust-analyzer" }, { "section": "other" }] })),
        )
        .expect("configuration answered");
        assert_eq!(
            response,
            json!([{ "rust-analyzer": { "checkOnSave": false } }, null])
        );
        let empty = handler(
            "workspace/configuration",
            Some(&json!({ "items": [] })),
        )
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
            assert!(guard.indexing_active);
        }
        LspServerInstance::handle_notification(
            &state,
            root,
            progress_notification(serde_json::json!("rustAnalyzer/Roots Scanned"), "report"),
        )
        .await;
        assert!(state.lock().await.indexing_active);
        LspServerInstance::handle_notification(
            &state,
            root,
            progress_notification(serde_json::json!("rustAnalyzer/Roots Scanned"), "end"),
        )
        .await;
        assert!(!state.lock().await.indexing_active);
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
        assert!(!guard.indexing_active);
    }
}
