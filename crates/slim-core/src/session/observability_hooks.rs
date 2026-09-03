use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde::Serialize;

use super::repository::DurableRepo;
use super::schema_v2::{
    DurableEntryRole, DurableErrorClass, DurableOperationKind, DurableOutcome, DurableRecord,
    ReplayPolicy, RetryPolicy,
};

/// Maximum bytes retained for caller-controlled IDs in observational data.
/// Oversized values become a fixed marker so bounded telemetry also has a
/// bounded byte footprint and never preserves arbitrary content.
pub const MAX_OBSERVABILITY_ID_BYTES: usize = 256;
pub(crate) const OVERSIZED_ID_MARKER: &str = "<oversized-id>";

pub(crate) fn safe_observability_id(value: &str) -> String {
    if value.len() <= MAX_OBSERVABILITY_ID_BYTES {
        value.to_owned()
    } else {
        OVERSIZED_ID_MARKER.to_owned()
    }
}

/// Content-free projection sent to post-append observers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DurableAppendProjection {
    Entry {
        seq: u64,
        entry_id: String,
        operation_id: String,
        role: DurableEntryRole,
        content_bytes: usize,
    },
    Operation {
        seq: u64,
        operation_id: String,
        kind: DurableOperationProjection,
    },
    Fact {
        seq: u64,
        namespace: String,
        key: String,
        value_kind: FactValueKind,
    },
    Usage {
        seq: u64,
        operation_id: String,
        attempt_id: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    Compaction {
        seq: u64,
        checkpoint_id: String,
        reason: crate::context::CompactionReason,
        tokens_before: u64,
        tokens_after: u64,
        duration_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableOperationProjection {
    QueueIntent {
        input_entry_id_present: bool,
    },
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
    ToolPhaseIntent {
        batch_id: String,
        batch_index: u32,
        batch_limit: u32,
        tool_call_id: String,
        tool_name: String,
        replay_policy: ReplayPolicy,
        input_bytes: usize,
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
        output_bytes: Option<usize>,
        artifact_ref_present: bool,
    },
    ToolPhaseFinished {
        batch_id: String,
        batch_index: u32,
        batch_limit: u32,
        tool_call_id: String,
        outcome: DurableOutcome,
    },
    Suspended {
        reason_bytes: usize,
    },
    Finished {
        outcome: DurableOutcome,
    },
    Aborted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactValueKind {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

impl DurableAppendProjection {
    pub fn from_record(record: &DurableRecord) -> Self {
        match record {
            DurableRecord::Entry { seq, entry } => Self::Entry {
                seq: *seq,
                entry_id: safe_observability_id(&entry.entry_id),
                operation_id: safe_observability_id(&entry.operation_id),
                role: entry.role.clone(),
                content_bytes: entry.content.len(),
            },
            DurableRecord::Operation { seq, operation } => Self::Operation {
                seq: *seq,
                operation_id: safe_observability_id(&operation.operation_id),
                kind: DurableOperationProjection::from_kind(&operation.kind),
            },
            DurableRecord::Fact { seq, fact } => Self::Fact {
                seq: *seq,
                namespace: safe_observability_id(&fact.namespace),
                key: safe_observability_id(&fact.key),
                value_kind: FactValueKind::from_json(&fact.value),
            },
            DurableRecord::Usage { seq, usage } => Self::Usage {
                seq: *seq,
                operation_id: safe_observability_id(&usage.operation_id),
                attempt_id: safe_observability_id(&usage.attempt_id),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            },
            DurableRecord::Compaction { seq, checkpoint } => Self::Compaction {
                seq: *seq,
                checkpoint_id: safe_observability_id(&checkpoint.checkpoint_id),
                reason: checkpoint.reason,
                tokens_before: checkpoint.tokens_before,
                tokens_after: checkpoint.tokens_after,
                duration_ms: checkpoint.duration_ms,
            },
        }
    }

    pub fn seq(&self) -> u64 {
        match self {
            Self::Entry { seq, .. }
            | Self::Operation { seq, .. }
            | Self::Fact { seq, .. }
            | Self::Usage { seq, .. }
            | Self::Compaction { seq, .. } => *seq,
        }
    }

    pub fn entry_id(&self) -> Option<&str> {
        match self {
            Self::Entry { entry_id, .. } => Some(entry_id),
            _ => None,
        }
    }

    pub fn operation_id(&self) -> Option<&str> {
        match self {
            Self::Entry { operation_id, .. }
            | Self::Operation { operation_id, .. }
            | Self::Usage { operation_id, .. } => Some(operation_id),
            Self::Fact { .. } | Self::Compaction { .. } => None,
        }
    }
}

impl DurableOperationProjection {
    fn from_kind(kind: &DurableOperationKind) -> Self {
        match kind {
            DurableOperationKind::QueueIntent { input_entry_id } => Self::QueueIntent {
                input_entry_id_present: input_entry_id.is_some(),
            },
            DurableOperationKind::Claimed => Self::Claimed,
            DurableOperationKind::Started { input_entry_id } => Self::Started {
                input_entry_id: safe_observability_id(input_entry_id),
            },
            DurableOperationKind::ProviderAttemptStarted {
                attempt_id,
                ordinal,
            } => Self::ProviderAttemptStarted {
                attempt_id: safe_observability_id(attempt_id),
                ordinal: *ordinal,
            },
            DurableOperationKind::RetryConfigured { repeatable, policy } => Self::RetryConfigured {
                repeatable: *repeatable,
                policy: *policy,
            },
            DurableOperationKind::ProviderAttemptFinished {
                attempt_id,
                outcome,
            } => Self::ProviderAttemptFinished {
                attempt_id: safe_observability_id(attempt_id),
                outcome: outcome.clone(),
            },
            DurableOperationKind::ProviderAttemptFailed { attempt_id, error } => {
                Self::ProviderAttemptFailed {
                    attempt_id: safe_observability_id(attempt_id),
                    error: error.clone(),
                }
            }
            DurableOperationKind::ToolIntent {
                tool_call_id,
                tool_name,
                replay_policy,
            } => Self::ToolIntent {
                tool_call_id: safe_observability_id(tool_call_id),
                tool_name: safe_observability_id(tool_name),
                replay_policy: replay_policy.clone(),
            },
            DurableOperationKind::ToolFinished {
                tool_call_id,
                outcome,
            } => Self::ToolFinished {
                tool_call_id: safe_observability_id(tool_call_id),
                outcome: outcome.clone(),
            },
            DurableOperationKind::ToolPhaseIntent {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
                tool_name,
                replay_policy,
                input_redacted,
            } => Self::ToolPhaseIntent {
                batch_id: safe_observability_id(batch_id),
                batch_index: *batch_index,
                batch_limit: *batch_limit,
                tool_call_id: safe_observability_id(tool_call_id),
                tool_name: safe_observability_id(tool_name),
                replay_policy: replay_policy.clone(),
                input_bytes: input_redacted.len(),
            },
            DurableOperationKind::ToolPhaseStarted {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
            } => Self::ToolPhaseStarted {
                batch_id: safe_observability_id(batch_id),
                batch_index: *batch_index,
                batch_limit: *batch_limit,
                tool_call_id: safe_observability_id(tool_call_id),
            },
            DurableOperationKind::ToolPhaseOutput {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
                output,
                artifact_ref,
            } => Self::ToolPhaseOutput {
                batch_id: safe_observability_id(batch_id),
                batch_index: *batch_index,
                batch_limit: *batch_limit,
                tool_call_id: safe_observability_id(tool_call_id),
                output_bytes: output.as_ref().map(String::len),
                artifact_ref_present: artifact_ref.is_some(),
            },
            DurableOperationKind::ToolPhaseFinished {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
                outcome,
            } => Self::ToolPhaseFinished {
                batch_id: safe_observability_id(batch_id),
                batch_index: *batch_index,
                batch_limit: *batch_limit,
                tool_call_id: safe_observability_id(tool_call_id),
                outcome: outcome.clone(),
            },
            DurableOperationKind::Suspended { reason } => Self::Suspended {
                reason_bytes: reason.len(),
            },
            DurableOperationKind::Finished { outcome } => Self::Finished {
                outcome: outcome.clone(),
            },
            DurableOperationKind::Aborted => Self::Aborted,
        }
    }
}

impl FactValueKind {
    fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(_) => Self::Boolean,
            serde_json::Value::Number(_) => Self::Number,
            serde_json::Value::String(_) => Self::String,
            serde_json::Value::Array(_) => Self::Array,
            serde_json::Value::Object(_) => Self::Object,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailure {
    Rejected,
    Panicked,
}

pub trait PostAppendHook {
    fn on_append(&mut self, event: &DurableAppendProjection) -> Result<(), HookFailure>;
}

impl<F> PostAppendHook for F
where
    F: FnMut(&DurableAppendProjection) -> Result<(), HookFailure>,
{
    fn on_append(&mut self, event: &DurableAppendProjection) -> Result<(), HookFailure> {
        self(event)
    }
}

#[derive(Default)]
pub struct PostAppendHooks {
    hooks: Vec<Box<dyn PostAppendHook>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct HookReport {
    invoked: u64,
    failures: Vec<HookFailure>,
}

impl HookReport {
    pub fn invoked(&self) -> u64 {
        self.invoked
    }

    pub fn failures(&self) -> &[HookFailure] {
        &self.failures
    }
}

impl PostAppendHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push<H>(&mut self, hook: H)
    where
        H: PostAppendHook + 'static,
    {
        self.hooks.push(Box::new(hook));
    }

    pub fn push_fn<F>(&mut self, hook: F)
    where
        F: FnMut(&DurableAppendProjection) -> Result<(), HookFailure> + 'static,
    {
        self.hooks.push(Box::new(hook));
    }

    pub fn dispatch(&mut self, record: &DurableRecord) -> HookReport {
        let projection = DurableAppendProjection::from_record(record);
        let mut report = HookReport::default();
        for hook in &mut self.hooks {
            report.invoked = report.invoked.saturating_add(1);
            let result = catch_unwind(AssertUnwindSafe(|| hook.on_append(&projection)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => report.failures.push(failure),
                Err(_) => report.failures.push(HookFailure::Panicked),
            }
        }
        report
    }
}

pub fn append_with_hooks<R: DurableRepo>(
    repo: &mut R,
    record: DurableRecord,
    hooks: &mut PostAppendHooks,
) -> io::Result<HookReport> {
    repo.append(record.clone())?;
    Ok(hooks.dispatch(&record))
}
