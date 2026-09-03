//! Semantic code intelligence for Slim, backed by the Language Server
//! Protocol (LSP) over JSON-RPC 2.0 on stdio.
//!
//! This crate is deliberately *infrastructure for the agent*, not a generic
//! LSP client exposed to the model. It implements the CodeIntelligence
//! trait defined in slim-core using one or more shared language-server
//! processes, and every result is compact, bounded and annotated with
//! reliability metadata so callers can tell a warm complete answer from a
//! partial one produced while the server is still indexing.

pub mod diagnostics;
pub mod discovery;
pub mod document;
pub mod instance;
pub mod manager;
mod path_policy;
pub mod pool;
pub mod position;
pub mod process;
pub mod transport;

pub use diagnostics::{DiagnosticsStore, StoredDiagnostics};
pub use discovery::{discover_for_workspace, ServerSpec};
pub use document::{DocumentStore, OpenDocument};
pub use instance::{LspServerInstance, ServerInstanceConfig};
pub use manager::{LspCodeIntelligence, LspManagerConfig};
pub use pool::{Lease, LspProcessPool, PoolConfig, ProcessFactory, SpawnedServer};
pub use position::{PositionCodec, PositionEncoding};
pub use transport::{LspIo, LspTransport, TransportError, TransportOptions};
