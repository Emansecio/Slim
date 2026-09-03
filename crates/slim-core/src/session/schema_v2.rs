use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const DURABLE_SCHEMA_VERSION: u32 = 2;

/// Maximum number of calls represented by one durable tool batch.
pub const MAX_TOOL_BATCH_LIMIT: u32 = 32;

/// Maximum inline UTF-8 payload accepted for a durable tool input or output.
pub const MAX_TOOL_INLINE_BYTES: usize = 64 * 1024;

/// Maximum UTF-8 size for durable tool metadata and opaque artifact IDs.
pub const MAX_TOOL_METADATA_BYTES: usize = 4 * 1024;

/// Maximum on-disk size accepted when reopening a schema-v2 JSONL session.
///
/// The legacy schema-v1 writer is intentionally not governed by this limit.
pub const MAX_DURABLE_SESSION_BYTES: u64 = 64 * 1024 * 1024;

pub const MAX_COMPACTION_SUMMARY_BYTES: usize = 64 * 1024;
pub const MAX_COMPACTION_PATH_BYTES: usize = 4 * 1024;
pub const MAX_COMPACTION_FILES_PER_CLASS: usize = 256;
pub const MAX_COMPACTION_FILES_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableSessionHeader {
    schema_version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_id: Option<String>,
    pub cutoff_seq: Option<u64>,
}

impl DurableSessionHeader {
    pub fn new(
        id: impl Into<String>,
        timestamp: impl Into<String>,
        cwd: impl Into<String>,
        parent_id: Option<String>,
        cutoff_seq: Option<u64>,
    ) -> Self {
        Self {
            schema_version: DURABLE_SCHEMA_VERSION,
            id: id.into(),
            timestamp: timestamp.into(),
            cwd: cwd.into(),
            parent_id,
            cutoff_seq,
        }
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DurableSessionHeaderWire {
    #[serde(rename = "type")]
    record_type: String,
    schema_version: u32,
    id: String,
    timestamp: String,
    cwd: String,
    parent_id: Option<String>,
    cutoff_seq: Option<u64>,
}

impl Serialize for DurableSessionHeader {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        DurableSessionHeaderWire {
            record_type: "session".into(),
            schema_version: DURABLE_SCHEMA_VERSION,
            id: self.id.clone(),
            timestamp: self.timestamp.clone(),
            cwd: self.cwd.clone(),
            parent_id: self.parent_id.clone(),
            cutoff_seq: self.cutoff_seq,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DurableSessionHeader {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = DurableSessionHeaderWire::deserialize(deserializer)?;
        if wire.record_type != "session" {
            return Err(serde::de::Error::custom(
                "session header type must be session",
            ));
        }
        if wire.schema_version != DURABLE_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported durable session schema_version",
            ));
        }
        Ok(Self {
            schema_version: wire.schema_version,
            id: wire.id,
            timestamp: wire.timestamp,
            cwd: wire.cwd,
            parent_id: wire.parent_id,
            cutoff_seq: wire.cutoff_seq,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableEntryRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableEntry {
    pub entry_id: String,
    pub role: DurableEntryRole,
    pub content: String,
    pub parent_entry_id: Option<String>,
    pub operation_id: String,
    pub tool_call_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    Never,
    Safe,
}

/// Persisted provider retry policy. Missing configuration is interpreted by
/// the attempt ledger as `Never` plus non-repeatable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryPolicy {
    Never,
    SafeTransport,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableOutcome {
    Success,
    Failed,
    Cancelled,
    Unknown,
}

/// A provider failure category safe to persist without retaining provider
/// messages, headers, prompts, or other sensitive response data.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableErrorClass {
    Transport { safe_to_retry: bool },
    Remote,
    Invalid,
    Cancelled,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableOperationKind {
    /// Durable queue intent. The input entry is optional because queue
    /// ownership is independent from the entry writer; when present it is
    /// only a stable reference, never prompt content.
    QueueIntent {
        input_entry_id: Option<String>,
    },
    /// Durable claim of a previously queued operation.
    Claimed,
    Started {
        input_entry_id: String,
    },
    ProviderAttemptStarted {
        attempt_id: String,
        ordinal: u32,
    },
    RetryConfigured {
        repeatable: bool,
        policy: RetryPolicy,
    },
    ProviderAttemptFinished {
        attempt_id: String,
        outcome: DurableOutcome,
    },
    /// A failed attempt with a classified, persistible error. The operation
    /// remains open so a later attempt may reuse its logical operation ID.
    ProviderAttemptFailed {
        attempt_id: String,
        error: DurableErrorClass,
    },
    ToolIntent {
        tool_call_id: String,
        tool_name: String,
        replay_policy: ReplayPolicy,
    },
    ToolFinished {
        tool_call_id: String,
        outcome: DurableOutcome,
    },
    /// Canonical, unambiguous tool-call intent. `input_redacted` must already
    /// have secrets removed before this record is constructed.
    ToolPhaseIntent {
        batch_id: String,
        batch_index: u32,
        batch_limit: u32,
        tool_call_id: String,
        tool_name: String,
        replay_policy: ReplayPolicy,
        input_redacted: String,
    },
    ToolPhaseStarted {
        batch_id: String,
        batch_index: u32,
        batch_limit: u32,
        tool_call_id: String,
    },
    ToolPhaseOutput {
        batch_id: String,
        batch_index: u32,
        batch_limit: u32,
        tool_call_id: String,
        output: Option<String>,
        artifact_ref: Option<String>,
    },
    ToolPhaseFinished {
        batch_id: String,
        batch_index: u32,
        batch_limit: u32,
        tool_call_id: String,
        outcome: DurableOutcome,
    },
    Suspended {
        reason: String,
    },
    Finished {
        outcome: DurableOutcome,
    },
    Aborted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableOperation {
    pub operation_id: String,
    pub kind: DurableOperationKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableFact {
    pub namespace: String,
    pub key: String,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableUsage {
    pub operation_id: String,
    pub attempt_id: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompactionCheckpoint {
    pub checkpoint_id: String,
    pub summary: String,
    pub first_kept_entry_id: String,
    /// Lower-case SHA-256 prefix of the exact transcript preceding the anchor.
    pub prefix_fingerprint: String,
    pub previous_checkpoint_id: Option<String>,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: u64,
    pub reason: crate::context::CompactionReason,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DurableRecord {
    Entry {
        seq: u64,
        entry: DurableEntry,
    },
    Operation {
        seq: u64,
        operation: DurableOperation,
    },
    Fact {
        seq: u64,
        fact: DurableFact,
    },
    Usage {
        seq: u64,
        usage: DurableUsage,
    },
    Compaction {
        seq: u64,
        checkpoint: CompactionCheckpoint,
    },
}

impl DurableRecord {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Entry { seq, .. }
            | Self::Operation { seq, .. }
            | Self::Fact { seq, .. }
            | Self::Usage { seq, .. }
            | Self::Compaction { seq, .. } => *seq,
        }
    }
}
