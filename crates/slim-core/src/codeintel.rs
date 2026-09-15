//! Contract for semantic code intelligence consumed by the agent loop.
//!
//! slim-lsp implements this trait; the runtime, the tool registry and the
//! CLI only ever see these types. The contract is deliberately small,
//! read-only in phase 1 and shaped around the agent: every outcome carries
//! reliability metadata (server, state, completeness, document version,
//! staleness) so readers can tell a warm complete answer from a partial one
//! produced while the server is still indexing.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::runtime::CancellationToken;

/// Default result limit for references / symbols / diagnostics.
pub const DEFAULT_CODE_INTEL_LIMIT: usize = 20;
/// Hard cap for any single code_intel result batch.
pub const MAX_CODE_INTEL_RESULTS: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelServerState {
    /// No server configured or binary unavailable for this workspace.
    Unavailable,
    /// Process spawned, handshake in progress.
    Starting,
    /// Server is alive but still building its index.
    Indexing,
    /// Server warm; answers may still be partial while indexing.
    Ready,
    /// Server experienced an error and is falling back to degraded answers.
    Degraded,
    /// Server shut down.
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelCompleteness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeIntelMeta {
    pub server: String,
    pub state: CodeIntelServerState,
    pub completeness: CodeIntelCompleteness,
    pub document_version: Option<i64>,
    pub stale: bool,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeIntelOutcome {
    pub meta: CodeIntelMeta,
    /// Compact, bounded result payload; shape depends on the requested action.
    pub payload: serde_json::Value,
}

impl CodeIntelOutcome {
    pub fn unavailable(server: &str, reason: &str) -> Self {
        Self {
            meta: CodeIntelMeta {
                server: server.to_owned(),
                state: CodeIntelServerState::Unavailable,
                completeness: CodeIntelCompleteness::Unknown,
                document_version: None,
                stale: false,
                elapsed_ms: 0,
            },
            payload: serde_json::json!({ "error": reason }),
        }
    }
}

/// Positional query used by definition / references / hover.
/// Lines and columns are human 1-based; conversion to the negotiated LSP
/// encoding happens inside the implementation.
#[derive(Clone, Debug, Default)]
pub struct CodeIntelPositionQuery {
    pub workspace: PathBuf,
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    /// Optional symbol name for error messages and result headers.
    pub symbol: Option<String>,
    pub max_results: usize,
    /// 0-based index of the first in-scope result to return (references
    /// paging); 0 is the first page.
    pub offset: usize,
    /// Continuation token emitted as `revision` by a previous page. When set,
    /// the call is rejected if the workspace revision or the server generation
    /// changed — pages never mix different states.
    pub revision: Option<u64>,
    /// Cooperative cancellation inherited from the active agent run.
    pub cancellation: Option<CancellationToken>,
}

/// Workspace/document symbol query.
#[derive(Clone, Debug, Default)]
pub struct CodeIntelSymbolQuery {
    pub workspace: PathBuf,
    /// When set, restricts to document symbols; otherwise workspace symbols.
    pub path: Option<PathBuf>,
    /// Workspace query sent to LSP; with path, ranks matching document
    /// symbols first before the page window.
    pub query: Option<String>,
    pub max_results: usize,
    /// 0-based index of the first result to return (paging); 0 is the
    /// first page.
    pub offset: usize,
    /// Continuation token emitted as `revision` by a previous page; rejected
    /// when the workspace revision or server generation changed.
    pub revision: Option<u64>,
    /// Cooperative cancellation inherited from the active agent run.
    pub cancellation: Option<CancellationToken>,
}

/// Diagnostics query. Info/hints are included only on request.
#[derive(Clone, Debug, Default)]
pub struct CodeIntelDiagnosticsQuery {
    pub workspace: PathBuf,
    pub path: Option<PathBuf>,
    pub include_info: bool,
    pub max_results: usize,
    /// Cooperative cancellation inherited from the active agent run.
    pub cancellation: Option<CancellationToken>,
}

/// A resolved patch position, before this individual edit is applied.
/// The LSP backend converts the physical-line prefix with its PositionCodec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeIntelEditPosition {
    pub line: u32,
    pub prefix: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeIntelTextEdit {
    pub start: CodeIntelEditPosition,
    pub end: CodeIntelEditPosition,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeIntelPatch {
    pub before_digest: String,
    /// Ordered edits: each range addresses the result of the preceding edit.
    pub edits: Vec<CodeIntelTextEdit>,
}

impl CodeIntelPatch {
    pub fn new(before: &str, edits: Vec<CodeIntelTextEdit>) -> Self {
        Self {
            before_digest: crate::tools::digest_bytes(
                b"slim-written-content-v1",
                before.as_bytes(),
            ),
            edits,
        }
    }

    pub fn matches_before(&self, text: &str) -> bool {
        self.before_digest
            == crate::tools::digest_bytes(b"slim-written-content-v1", text.as_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeIntelFileUpdate {
    pub text: String,
    pub patch: Option<CodeIntelPatch>,
}

/// Semantic code intelligence facade (phase 1: read-only).
#[async_trait]
pub trait CodeIntelligence: Send + Sync {
    /// Whether this backend can serve the workspace. Must not start a server.
    /// Backends without a discovery restriction remain advertised.
    fn supports_workspace(&self, _workspace: &Path) -> bool {
        true
    }

    /// Server availability and health for a workspace.
    async fn status(&self, workspace: &Path) -> CodeIntelOutcome;

    /// Locate the definition of the symbol at the given position.
    async fn definition(&self, query: &CodeIntelPositionQuery) -> CodeIntelOutcome;

    /// Find references to the symbol at the given position.
    async fn references(&self, query: &CodeIntelPositionQuery) -> CodeIntelOutcome;

    /// Hover documentation/signature for the symbol at the given position.
    async fn hover(&self, query: &CodeIntelPositionQuery) -> CodeIntelOutcome;

    /// Document or workspace symbol outline.
    async fn symbols(&self, query: &CodeIntelSymbolQuery) -> CodeIntelOutcome;

    /// Latest server-published diagnostics for a file (or all files).
    async fn diagnostics(&self, query: &CodeIntelDiagnosticsQuery) -> CodeIntelOutcome;

    /// Ordered best-effort sync after the agent mutated a file
    /// (didChange/didSave). Implementations remain fail-open, but completion
    /// must mean later semantic queries observe this notification sequence.
    async fn notify_file_changed(&self, workspace: &Path, path: &Path, text: Option<String>);

    async fn notify_file_updated(
        &self,
        workspace: &Path,
        path: &Path,
        update: CodeIntelFileUpdate,
    ) {
        self.notify_file_changed(workspace, path, Some(update.text))
            .await;
    }
}
