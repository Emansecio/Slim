use std::collections::BTreeMap;
use std::io;

use super::attempts::{AttemptLedger, AttemptLedgerError};
use super::queue::{DurableQueue, DurableQueueError};
use super::reducer::{reduce, restore_records, DurableState, ReduceError};
use super::schema_v2::{DurableOperationKind, DurableRecord, DurableSessionHeader};
use super::tool_phases::{is_legacy_tool_phase, ToolPhaseError, ToolPhaseLedger};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static FULL_PREFIX_VALIDATIONS: Cell<u64> = const { Cell::new(0) };
}

pub trait DurableRepo {
    fn header(&self) -> &DurableSessionHeader;

    fn records(&self) -> &[DurableRecord];

    fn append(&mut self, record: DurableRecord) -> io::Result<()>;

    fn append_batch(&mut self, records: Vec<DurableRecord>) -> io::Result<()> {
        for record in records {
            self.append(record)?;
        }
        Ok(())
    }

    fn read_prefix(&self, through_seq: u64) -> Vec<DurableRecord> {
        self.records()
            .iter()
            .take_while(|record| record.seq() <= through_seq)
            .cloned()
            .collect()
    }
}

pub(crate) fn validate_next(records: &[DurableRecord], next_seq: u64) -> io::Result<()> {
    if records.last().is_some_and(|last| next_seq <= last.seq()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "record sequence must increase",
        ));
    }

    Ok(())
}

/// Validate lifecycle and payload constraints for a parsed durable prefix.
/// JSONL parsing remains responsible only for syntax/schema/sequence; this
/// hook is intentionally called before any torn-tail repair or separator write.
pub(crate) fn validate_durable_records(records: &[DurableRecord]) -> io::Result<()> {
    validate_durable_records_with_kind(records, io::ErrorKind::InvalidData)
}

fn validate_durable_records_with_kind(
    records: &[DurableRecord],
    error_kind: io::ErrorKind,
) -> io::Result<()> {
    note_full_prefix_validation();
    let _ = operation_lifecycle_from_records(records, error_kind)?;

    AttemptLedger::validate(records).map_err(|error| attempt_error(error_kind, error))?;

    if has_queue_lifecycle(records) {
        DurableQueue::from_records(records.len().max(1), records)
            .map_err(|error| queue_error(error_kind, error))?;
    }

    if has_tool_phase(records) {
        ToolPhaseLedger::validate(records).map_err(|error| tool_error(error_kind, error))?;
    }
    Ok(())
}

fn note_full_prefix_validation() {
    #[cfg(test)]
    FULL_PREFIX_VALIDATIONS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn take_full_prefix_validations() -> u64 {
    FULL_PREFIX_VALIDATIONS.with(|count| {
        let value = count.get();
        count.set(0);
        value
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct OperationLifecycle {
    started: bool,
    terminal: bool,
}

/// Incremental validator reconstructed once after a full prefix check.
/// `prepare` clones and applies one record without mutating `self`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DurableAppendValidator {
    reducer: DurableState,
    lifecycle: BTreeMap<String, OperationLifecycle>,
    attempts: AttemptLedger,
    queue: DurableQueue,
    saw_queue: bool,
    tools: ToolPhaseLedger,
    saw_tools: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedDurableAppend {
    validator: DurableAppendValidator,
}

impl DurableAppendValidator {
    pub(crate) fn empty() -> Self {
        Self {
            reducer: DurableState::default(),
            lifecycle: BTreeMap::new(),
            attempts: AttemptLedger::default(),
            queue: DurableQueue::new(1),
            saw_queue: false,
            tools: ToolPhaseLedger::default(),
            saw_tools: false,
        }
    }

    pub(crate) fn from_records(records: &[DurableRecord]) -> io::Result<Self> {
        validate_durable_records(records)?;
        let mut attempts = AttemptLedger::default();
        for record in records {
            attempts
                .apply_record(record)
                .map_err(|error| attempt_error(io::ErrorKind::InvalidData, error))?;
        }
        let saw_queue = has_queue_lifecycle(records);
        let queue = if saw_queue {
            DurableQueue::from_records(records.len().max(1), records)
                .map_err(|error| queue_error(io::ErrorKind::InvalidData, error))?
        } else {
            DurableQueue::new(records.len().max(1))
        };
        let saw_tools = has_tool_phase(records);
        let tools = if saw_tools {
            replay_tools(records, &[], None)
                .map_err(|error| tool_error(io::ErrorKind::InvalidData, error))?
        } else {
            ToolPhaseLedger::default()
        };
        Ok(Self {
            reducer: restore_records(records)
                .map_err(|error| reduce_error(io::ErrorKind::InvalidData, error))?,
            lifecycle: operation_lifecycle_from_records(records, io::ErrorKind::InvalidData)?,
            attempts,
            queue,
            saw_queue,
            tools,
            saw_tools,
        })
    }

    pub(crate) fn prepare(
        &self,
        prefix: &[DurableRecord],
        record: &DurableRecord,
    ) -> io::Result<PreparedDurableAppend> {
        self.prepare_batch(prefix, std::slice::from_ref(record))
    }

    pub(crate) fn prepare_batch(
        &self,
        prefix: &[DurableRecord],
        records: &[DurableRecord],
    ) -> io::Result<PreparedDurableAppend> {
        let mut next = self.clone();
        for (index, record) in records.iter().enumerate() {
            if index == 0 {
                validate_next(prefix, record.seq())?;
            } else {
                validate_next(&records[..index], record.seq())?;
            }
            next.apply_one(
                prefix,
                &records[..index],
                record,
                io::ErrorKind::InvalidInput,
            )?;
        }
        Ok(PreparedDurableAppend { validator: next })
    }

    pub(crate) fn commit(&mut self, prepared: PreparedDurableAppend) {
        *self = prepared.validator;
    }

    fn apply_one(
        &mut self,
        prefix: &[DurableRecord],
        batch_prefix: &[DurableRecord],
        record: &DurableRecord,
        error_kind: io::ErrorKind,
    ) -> io::Result<()> {
        apply_lifecycle(&mut self.lifecycle, record, error_kind)?;
        reduce(&mut self.reducer, record).map_err(|error| reduce_error(error_kind, error))?;
        self.attempts
            .apply_record(record)
            .map_err(|error| attempt_error(error_kind, error))?;

        self.queue.grow_capacity_to(
            prefix
                .len()
                .saturating_add(batch_prefix.len())
                .saturating_add(1),
        );
        if self.saw_queue || is_queue_intent(record) {
            if self.saw_queue {
                self.queue
                    .apply_validation_record(record)
                    .map_err(|error| queue_error(error_kind, error))?;
            } else {
                let mut queue = DurableQueue::new(
                    prefix
                        .len()
                        .saturating_add(batch_prefix.len())
                        .saturating_add(1)
                        .max(1),
                );
                for existing in prefix.iter().chain(batch_prefix) {
                    queue
                        .apply_validation_record(existing)
                        .map_err(|error| queue_error(error_kind, error))?;
                }
                queue
                    .apply_validation_record(record)
                    .map_err(|error| queue_error(error_kind, error))?;
                self.queue = queue;
                self.saw_queue = true;
            }
        }

        if self.saw_tools || is_tool_phase(record) {
            if self.saw_tools {
                self.tools
                    .apply_record(record)
                    .map_err(|error| tool_error(error_kind, error))?;
            } else {
                self.tools = replay_tools(prefix, batch_prefix, Some(record))
                    .map_err(|error| tool_error(error_kind, error))?;
                self.saw_tools = true;
            }
        }
        Ok(())
    }
}

fn replay_tools(
    prefix: &[DurableRecord],
    batch_prefix: &[DurableRecord],
    record: Option<&DurableRecord>,
) -> Result<ToolPhaseLedger, ToolPhaseError> {
    let mut ledger = ToolPhaseLedger::default();
    for existing in prefix.iter().chain(batch_prefix) {
        ledger.apply_record(existing)?;
    }
    if let Some(record) = record {
        ledger.apply_record(record)?;
    } else if prefix.iter().chain(batch_prefix).any(is_legacy_tool_phase) {
        return Err(ToolPhaseError::LegacyToolPhase);
    }
    Ok(ledger)
}

fn reduce_error(error_kind: io::ErrorKind, _error: ReduceError) -> io::Error {
    io::Error::new(
        error_kind,
        "invalid durable attempt lifecycle: invalid durable records",
    )
}

fn attempt_error(error_kind: io::ErrorKind, error: AttemptLedgerError) -> io::Error {
    io::Error::new(
        error_kind,
        format!("invalid durable attempt lifecycle: {error}"),
    )
}

fn queue_error(error_kind: io::ErrorKind, error: DurableQueueError) -> io::Error {
    io::Error::new(
        error_kind,
        format!("invalid durable queue lifecycle: {error}"),
    )
}

fn tool_error(error_kind: io::ErrorKind, error: ToolPhaseError) -> io::Error {
    io::Error::new(error_kind, error.to_string())
}

fn operation_lifecycle_from_records(
    records: &[DurableRecord],
    error_kind: io::ErrorKind,
) -> io::Result<BTreeMap<String, OperationLifecycle>> {
    let mut operations = BTreeMap::new();
    for record in records {
        apply_lifecycle(&mut operations, record, error_kind)?;
    }
    Ok(operations)
}

/// Enforce the small operation-level contract shared by all durable writers.
/// Attempt and tool correlation remain delegated to their specialized ledgers;
/// this guard only prevents terminal records from inventing or reopening an
/// operation before the repository mutates.
fn apply_lifecycle(
    operations: &mut BTreeMap<String, OperationLifecycle>,
    record: &DurableRecord,
    error_kind: io::ErrorKind,
) -> io::Result<()> {
    let DurableRecord::Operation { operation, .. } = record else {
        return Ok(());
    };
    let state = operations
        .entry(operation.operation_id.clone())
        .or_default();
    match &operation.kind {
        DurableOperationKind::QueueIntent { .. } | DurableOperationKind::Started { .. } => {
            if state.terminal {
                return Err(io::Error::new(
                    error_kind,
                    format!(
                        "operation started after terminal: {}",
                        operation.operation_id
                    ),
                ));
            }
            if state.started {
                return Err(io::Error::new(
                    error_kind,
                    format!(
                        "operation started more than once: {}",
                        operation.operation_id
                    ),
                ));
            }
            state.started = true;
        }
        DurableOperationKind::Finished { .. } | DurableOperationKind::Aborted => {
            if !state.started {
                return Err(io::Error::new(
                    error_kind,
                    format!(
                        "operation terminal before start: {}",
                        operation.operation_id
                    ),
                ));
            }
            if state.terminal {
                return Err(io::Error::new(
                    error_kind,
                    format!(
                        "operation terminal is duplicated: {}",
                        operation.operation_id
                    ),
                ));
            }
            state.terminal = true;
        }
        _ => {}
    }
    Ok(())
}

fn has_queue_lifecycle(records: &[DurableRecord]) -> bool {
    records.iter().any(is_queue_intent)
}

fn is_queue_intent(record: &DurableRecord) -> bool {
    matches!(
        record,
        DurableRecord::Operation {
            operation: super::schema_v2::DurableOperation {
                kind: DurableOperationKind::QueueIntent { .. },
                ..
            },
            ..
        }
    )
}

fn has_tool_phase(records: &[DurableRecord]) -> bool {
    records.iter().any(is_tool_phase)
}

fn is_tool_phase(record: &DurableRecord) -> bool {
    matches!(
        record,
        DurableRecord::Operation {
            operation: super::schema_v2::DurableOperation {
                kind: DurableOperationKind::ToolPhaseIntent { .. }
                    | DurableOperationKind::ToolPhaseStarted { .. }
                    | DurableOperationKind::ToolPhaseOutput { .. }
                    | DurableOperationKind::ToolPhaseFinished { .. }
                    | DurableOperationKind::ToolIntent { .. }
                    | DurableOperationKind::ToolFinished { .. },
                ..
            },
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema_v2::{
        DurableEntry, DurableEntryRole, DurableFact, DurableOperation, DurableOutcome,
    };
    use serde_json::json;

    fn entry(seq: u64, operation_id: &str) -> DurableRecord {
        DurableRecord::Entry {
            seq,
            entry: DurableEntry {
                entry_id: format!("entry-{seq}"),
                role: DurableEntryRole::User,
                content: format!("content-{seq}"),
                parent_entry_id: None,
                operation_id: operation_id.into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        }
    }

    fn started(seq: u64, operation_id: &str, input_entry_id: &str) -> DurableRecord {
        DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: input_entry_id.into(),
                },
            },
        }
    }

    fn fact(seq: u64) -> DurableRecord {
        DurableRecord::Fact {
            seq,
            fact: DurableFact {
                namespace: "session".into(),
                key: format!("k-{seq}"),
                value: json!(seq),
            },
        }
    }

    fn queue_intent(seq: u64, operation_id: &str) -> DurableRecord {
        DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.into(),
                kind: DurableOperationKind::QueueIntent {
                    input_entry_id: None,
                },
            },
        }
    }

    fn claimed(seq: u64, operation_id: &str) -> DurableRecord {
        DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.into(),
                kind: DurableOperationKind::Claimed,
            },
        }
    }

    fn finished(seq: u64, operation_id: &str) -> DurableRecord {
        DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.into(),
                kind: DurableOperationKind::Finished {
                    outcome: DurableOutcome::Success,
                },
            },
        }
    }

    fn tool_intent(
        seq: u64,
        operation_id: &str,
        tool_call_id: &str,
        batch_index: u32,
    ) -> DurableRecord {
        DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.into(),
                kind: DurableOperationKind::ToolPhaseIntent {
                    batch_id: "batch-1".into(),
                    batch_index,
                    batch_limit: 2,
                    tool_call_id: tool_call_id.into(),
                    tool_name: "read".into(),
                    replay_policy: crate::session::schema_v2::ReplayPolicy::Never,
                    input_redacted: "{}".into(),
                },
            },
        }
    }

    #[test]
    fn append_prepare_does_not_rescan_or_mutate_the_prefix() {
        let _ = take_full_prefix_validations();
        let mut records = Vec::new();
        let mut validator = DurableAppendValidator::empty();
        assert_eq!(take_full_prefix_validations(), 0);

        for seq in 0..64 {
            let record = fact(seq);
            let prepared = validator
                .prepare(&records, &record)
                .expect("valid fact append");
            let snapshot = validator.clone();
            assert_eq!(validator, snapshot, "prepare must not mutate the validator");
            records.push(record);
            validator.commit(prepared);
        }

        assert_eq!(
            take_full_prefix_validations(),
            0,
            "append must not revalidate the whole prefix"
        );
        let rebuilt = DurableAppendValidator::from_records(&records).expect("rebuild");
        assert_eq!(take_full_prefix_validations(), 1);
        assert_eq!(validator, rebuilt);
    }

    #[test]
    fn open_runs_full_prefix_validation_once() {
        let records = vec![entry(0, "op-1"), started(1, "op-1", "entry-0"), fact(2)];
        let _ = take_full_prefix_validations();
        DurableAppendValidator::from_records(&records).expect("open prefix");
        assert_eq!(take_full_prefix_validations(), 1);
    }

    #[test]
    fn rejected_prepare_leaves_incremental_state_unchanged() {
        let records = vec![entry(0, "op-1"), started(1, "op-1", "entry-0")];
        let validator = DurableAppendValidator::from_records(&records).expect("open");
        let before = validator.clone();
        let error = validator
            .prepare(&records, &started(2, "op-1", "entry-0"))
            .expect_err("duplicate start");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(validator, before);
        let error = validator
            .prepare(&records, &finished(1, "op-1"))
            .expect_err("non-increasing sequence");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "record sequence must increase");
        assert_eq!(validator, before);
    }

    #[test]
    fn incremental_queue_and_tool_state_matches_full_rebuild() {
        let records = vec![
            entry(0, "op-1"),
            started(1, "op-1", "entry-0"),
            tool_intent(2, "op-1", "call-1", 0),
            queue_intent(3, "op-2"),
            claimed(4, "op-2"),
            finished(5, "op-2"),
        ];
        let mut validator = DurableAppendValidator::empty();
        let mut prefix = Vec::new();
        for record in &records {
            let prepared = validator.prepare(&prefix, record).expect("append");
            prefix.push(record.clone());
            validator.commit(prepared);
        }
        assert_eq!(
            validator,
            DurableAppendValidator::from_records(&records).expect("rebuild")
        );
    }
}
