use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

use super::schema_v2::{
    CompactionCheckpoint, DurableEntry, DurableFact, DurableOperation, DurableRecord, DurableUsage,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DurableState {
    last_seq: Option<u64>,
    entries: BTreeMap<String, DurableEntry>,
    operations: BTreeMap<String, Vec<DurableOperation>>,
    facts: BTreeMap<(String, String), DurableFact>,
    usage: BTreeMap<(String, String), DurableUsage>,
    compaction_checkpoint: Option<CompactionCheckpoint>,
}

impl DurableState {
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    pub fn entries(&self) -> &BTreeMap<String, DurableEntry> {
        &self.entries
    }

    pub fn operations(&self) -> &BTreeMap<String, Vec<DurableOperation>> {
        &self.operations
    }

    pub fn facts(&self) -> &BTreeMap<(String, String), DurableFact> {
        &self.facts
    }

    pub fn usage(&self) -> &BTreeMap<(String, String), DurableUsage> {
        &self.usage
    }

    pub fn compaction_checkpoint(&self) -> Option<&CompactionCheckpoint> {
        self.compaction_checkpoint.as_ref()
    }

    pub fn fact_value(&self, namespace: &str, key: &str) -> Option<&Value> {
        self.facts
            .get(&(namespace.to_owned(), key.to_owned()))
            .map(|fact| &fact.value)
    }

    pub fn operation_history(&self, operation_id: &str) -> Option<&[DurableOperation]> {
        self.operations.get(operation_id).map(Vec::as_slice)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReduceError {
    SequenceNotIncreasing {
        previous: u64,
        next: u64,
    },
    EmptyRequiredId {
        field: &'static str,
    },
    DuplicateEntryId {
        entry_id: String,
    },
    MissingParentEntry {
        entry_id: String,
        parent_entry_id: String,
    },
    MissingInputEntry {
        operation_id: String,
        input_entry_id: String,
    },
    InputEntryOperationMismatch {
        operation_id: String,
        input_entry_id: String,
        entry_operation_id: String,
    },
    CompactionLimitExceeded {
        field: &'static str,
    },
}

impl fmt::Display for ReduceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceNotIncreasing { previous, next } => write!(
                formatter,
                "durable record sequence must increase: previous={previous}, next={next}"
            ),
            Self::EmptyRequiredId { field } => {
                write!(formatter, "required durable id is empty: {field}")
            }
            Self::DuplicateEntryId { entry_id } => {
                write!(formatter, "durable entry id is duplicated: {entry_id}")
            }
            Self::MissingParentEntry {
                entry_id,
                parent_entry_id,
            } => write!(
                formatter,
                "durable entry {entry_id} references missing parent {parent_entry_id}"
            ),
            Self::MissingInputEntry {
                operation_id,
                input_entry_id,
            } => write!(
                formatter,
                "durable operation {operation_id} references missing input entry {input_entry_id}"
            ),
            Self::InputEntryOperationMismatch {
                operation_id,
                input_entry_id,
                entry_operation_id,
            } => write!(
                formatter,
                "durable operation {operation_id} input entry {input_entry_id} belongs to {entry_operation_id}"
            ),
            Self::CompactionLimitExceeded { field } => {
                write!(formatter, "durable compaction checkpoint exceeds limit: {field}")
            }
        }
    }
}

impl std::error::Error for ReduceError {}

pub fn reduce<R>(state: &mut DurableState, record: R) -> Result<(), ReduceError>
where
    R: Borrow<DurableRecord>,
{
    let record = record.borrow();
    let next_seq = record.seq();
    if let Some(previous) = state.last_seq {
        if next_seq <= previous {
            return Err(ReduceError::SequenceNotIncreasing {
                previous,
                next: next_seq,
            });
        }
    }
    validate_record(state, record)?;

    match record {
        DurableRecord::Entry { entry, .. } => {
            state.entries.insert(entry.entry_id.clone(), entry.clone());
        }
        DurableRecord::Operation { operation, .. } => {
            state
                .operations
                .entry(operation.operation_id.clone())
                .or_default()
                .push(operation.clone());
        }
        DurableRecord::Fact { fact, .. } => {
            state
                .facts
                .insert((fact.namespace.clone(), fact.key.clone()), fact.clone());
        }
        DurableRecord::Usage { usage, .. } => {
            let key = (usage.operation_id.clone(), usage.attempt_id.clone());
            if let Some(existing) = state.usage.get_mut(&key) {
                if usage.input_tokens.is_some() {
                    existing.input_tokens = usage.input_tokens;
                }
                if usage.output_tokens.is_some() {
                    existing.output_tokens = usage.output_tokens;
                }
            } else {
                state.usage.insert(key, usage.clone());
            }
        }
        DurableRecord::Compaction { checkpoint, .. } => {
            state.compaction_checkpoint = Some(checkpoint.clone());
        }
    }
    state.last_seq = Some(next_seq);
    Ok(())
}

fn validate_record(state: &DurableState, record: &DurableRecord) -> Result<(), ReduceError> {
    // Identity/referential checks live here; attempt/tool/terminal lifecycle rules
    // intentionally remain in Etapas 5/6.
    match record {
        DurableRecord::Entry { entry, .. } => {
            require_id(&entry.entry_id, "entry.entry_id")?;
            require_id(&entry.operation_id, "entry.operation_id")?;
            if let Some(tool_call_id) = &entry.tool_call_id {
                require_id(tool_call_id, "entry.tool_call_id")?;
            }
            if state.entries.contains_key(&entry.entry_id) {
                return Err(ReduceError::DuplicateEntryId {
                    entry_id: entry.entry_id.clone(),
                });
            }
            if let Some(parent_entry_id) = &entry.parent_entry_id {
                require_id(parent_entry_id, "entry.parent_entry_id")?;
                if !state.entries.contains_key(parent_entry_id) {
                    return Err(ReduceError::MissingParentEntry {
                        entry_id: entry.entry_id.clone(),
                        parent_entry_id: parent_entry_id.clone(),
                    });
                }
            }
        }
        DurableRecord::Operation { operation, .. } => {
            require_id(&operation.operation_id, "operation.operation_id")?;
            match &operation.kind {
                super::schema_v2::DurableOperationKind::QueueIntent { input_entry_id } => {
                    if let Some(input_entry_id) = input_entry_id {
                        require_id(input_entry_id, "operation.input_entry_id")?;
                    }
                }
                super::schema_v2::DurableOperationKind::Claimed => {}
                super::schema_v2::DurableOperationKind::Started { input_entry_id } => {
                    require_id(input_entry_id, "operation.input_entry_id")?;
                    let input_entry = state.entries.get(input_entry_id).ok_or_else(|| {
                        ReduceError::MissingInputEntry {
                            operation_id: operation.operation_id.clone(),
                            input_entry_id: input_entry_id.clone(),
                        }
                    })?;
                    if input_entry.operation_id != operation.operation_id {
                        return Err(ReduceError::InputEntryOperationMismatch {
                            operation_id: operation.operation_id.clone(),
                            input_entry_id: input_entry_id.clone(),
                            entry_operation_id: input_entry.operation_id.clone(),
                        });
                    }
                }
                super::schema_v2::DurableOperationKind::ProviderAttemptStarted {
                    attempt_id,
                    ..
                }
                | super::schema_v2::DurableOperationKind::ProviderAttemptFinished {
                    attempt_id,
                    ..
                } => require_id(attempt_id, "operation.attempt_id")?,
                super::schema_v2::DurableOperationKind::ProviderAttemptFailed {
                    attempt_id,
                    ..
                } => require_id(attempt_id, "operation.attempt_id")?,
                super::schema_v2::DurableOperationKind::RetryConfigured { .. } => {}
                super::schema_v2::DurableOperationKind::ToolIntent { tool_call_id, .. }
                | super::schema_v2::DurableOperationKind::ToolFinished { tool_call_id, .. } => {
                    require_id(tool_call_id, "operation.tool_call_id")?;
                }
                super::schema_v2::DurableOperationKind::ToolPhaseIntent {
                    batch_id,
                    tool_call_id,
                    tool_name,
                    ..
                } => {
                    require_id(batch_id, "operation.batch_id")?;
                    require_id(tool_call_id, "operation.tool_call_id")?;
                    require_id(tool_name, "operation.tool_name")?;
                }
                super::schema_v2::DurableOperationKind::ToolPhaseStarted {
                    batch_id,
                    tool_call_id,
                    ..
                }
                | super::schema_v2::DurableOperationKind::ToolPhaseOutput {
                    batch_id,
                    tool_call_id,
                    ..
                }
                | super::schema_v2::DurableOperationKind::ToolPhaseFinished {
                    batch_id,
                    tool_call_id,
                    ..
                } => {
                    require_id(batch_id, "operation.batch_id")?;
                    require_id(tool_call_id, "operation.tool_call_id")?;
                }
                super::schema_v2::DurableOperationKind::Suspended { .. }
                | super::schema_v2::DurableOperationKind::Finished { .. }
                | super::schema_v2::DurableOperationKind::Aborted => {}
            }
        }
        DurableRecord::Fact { .. } => {}
        DurableRecord::Usage { usage, .. } => {
            require_id(&usage.operation_id, "usage.operation_id")?;
            require_id(&usage.attempt_id, "usage.attempt_id")?;
        }
        DurableRecord::Compaction { checkpoint, .. } => {
            require_id(&checkpoint.checkpoint_id, "compaction.checkpoint_id")?;
            require_id(
                &checkpoint.first_kept_entry_id,
                "compaction.first_kept_entry_id",
            )?;
            require_id(
                &checkpoint.prefix_fingerprint,
                "compaction.prefix_fingerprint",
            )?;
            if checkpoint.summary.len() > super::schema_v2::MAX_COMPACTION_SUMMARY_BYTES {
                return Err(ReduceError::CompactionLimitExceeded { field: "summary" });
            }
            if checkpoint.prefix_fingerprint.len() != 16
                || !checkpoint
                    .prefix_fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(ReduceError::CompactionLimitExceeded {
                    field: "prefix_fingerprint",
                });
            }
            if checkpoint.read_files.len() > super::schema_v2::MAX_COMPACTION_FILES_PER_CLASS
                || checkpoint.modified_files.len()
                    > super::schema_v2::MAX_COMPACTION_FILES_PER_CLASS
            {
                return Err(ReduceError::CompactionLimitExceeded {
                    field: "file_count",
                });
            }
            let mut total_path_bytes = 0usize;
            for path in checkpoint
                .read_files
                .iter()
                .chain(checkpoint.modified_files.iter())
            {
                if path.len() > super::schema_v2::MAX_COMPACTION_PATH_BYTES {
                    return Err(ReduceError::CompactionLimitExceeded { field: "path" });
                }
                total_path_bytes = total_path_bytes.saturating_add(path.len());
            }
            if total_path_bytes > super::schema_v2::MAX_COMPACTION_FILES_BYTES {
                return Err(ReduceError::CompactionLimitExceeded {
                    field: "file_bytes",
                });
            }
        }
    }
    Ok(())
}

fn require_id(value: &str, field: &'static str) -> Result<(), ReduceError> {
    if value.is_empty() {
        Err(ReduceError::EmptyRequiredId { field })
    } else {
        Ok(())
    }
}

pub fn restore_records(records: &[DurableRecord]) -> Result<DurableState, ReduceError> {
    let mut state = DurableState::default();
    for record in records {
        reduce(&mut state, record)?;
    }
    Ok(state)
}
