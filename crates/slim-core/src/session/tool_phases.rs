use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::attempts::AttemptLedger;
use super::schema_v2::{
    DurableOperationKind, DurableOutcome, DurableRecord, ReplayPolicy, MAX_TOOL_BATCH_LIMIT,
    MAX_TOOL_INLINE_BYTES, MAX_TOOL_METADATA_BYTES,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolOutput {
    Inline(String),
    Artifact(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallState {
    pub operation_id: String,
    pub batch_id: String,
    pub batch_index: u32,
    pub batch_limit: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    pub replay_policy: ReplayPolicy,
    pub input_redacted: String,
    pub started: bool,
    pub output: Option<ToolOutput>,
    pub outcome: Option<DurableOutcome>,
}

impl ToolCallState {
    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn is_incomplete(&self) -> bool {
        !self.is_finished()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolBatch {
    pub operation_id: String,
    pub batch_id: String,
    pub batch_limit: u32,
    pub calls: Vec<ToolCallState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolPhaseError {
    InvalidRecords,
    LegacyToolPhase,
    OperationNotStarted {
        operation_id: String,
    },
    OperationTerminal {
        operation_id: String,
    },
    OperationTerminalWithIncompleteTools {
        operation_id: String,
    },
    OpenProviderAttempt {
        operation_id: String,
        attempt_id: String,
    },
    ProviderAttemptAfterToolIntent {
        operation_id: String,
        attempt_id: String,
    },
    EmptyId {
        field: &'static str,
    },
    MetadataTooLarge {
        field: &'static str,
        bytes: usize,
    },
    BatchLimitOutOfRange {
        batch_id: String,
        batch_limit: u32,
    },
    BatchIndexOutOfRange {
        batch_id: String,
        batch_index: u32,
        batch_limit: u32,
    },
    BatchIdentityMismatch {
        batch_id: String,
    },
    BatchIndexOutOfOrder {
        batch_id: String,
        expected: u32,
        actual: u32,
    },
    DuplicateToolCallId {
        tool_call_id: String,
    },
    UnknownToolCall {
        tool_call_id: String,
    },
    PhaseOutOfOrder {
        tool_call_id: String,
    },
    DuplicateOutput {
        tool_call_id: String,
    },
    FinishedWithoutOutput {
        tool_call_id: String,
    },
    DuplicateFinished {
        tool_call_id: String,
    },
    OutputMissing,
    OutputAmbiguous,
    InlinePayloadTooLarge {
        field: &'static str,
        bytes: usize,
    },
    UnknownOutcome {
        tool_call_id: String,
    },
}

impl fmt::Display for ToolPhaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecords => formatter.write_str("invalid durable records"),
            Self::LegacyToolPhase => {
                formatter.write_str("legacy tool phase cannot enter the Stage 6 ledger")
            }
            Self::OperationNotStarted { operation_id } => {
                write!(formatter, "tool operation was not started: {operation_id}")
            }
            Self::OperationTerminal { operation_id } => {
                write!(formatter, "tool operation is terminal: {operation_id}")
            }
            Self::OperationTerminalWithIncompleteTools { operation_id } => write!(
                formatter,
                "tool operation is terminal with incomplete phases: {operation_id}"
            ),
            Self::OpenProviderAttempt {
                operation_id,
                attempt_id,
            } => write!(
                formatter,
                "tool intent follows an open provider attempt {attempt_id} in {operation_id}"
            ),
            Self::ProviderAttemptAfterToolIntent {
                operation_id,
                attempt_id,
            } => write!(
                formatter,
                "provider attempt {attempt_id} follows a tool intent in {operation_id}"
            ),
            Self::EmptyId { field } => write!(formatter, "empty tool phase id: {field}"),
            Self::MetadataTooLarge { field, bytes } => write!(
                formatter,
                "{field} exceeds the {MAX_TOOL_METADATA_BYTES}-byte metadata limit: {bytes}"
            ),
            Self::BatchLimitOutOfRange {
                batch_id,
                batch_limit,
            } => write!(
                formatter,
                "batch {batch_id} limit must be 1..={MAX_TOOL_BATCH_LIMIT}, got {batch_limit}"
            ),
            Self::BatchIndexOutOfRange {
                batch_id,
                batch_index,
                batch_limit,
            } => write!(
                formatter,
                "batch {batch_id} index {batch_index} is outside 0..{batch_limit}"
            ),
            Self::BatchIdentityMismatch { batch_id } => {
                write!(formatter, "batch identity changed: {batch_id}")
            }
            Self::BatchIndexOutOfOrder {
                batch_id,
                expected,
                actual,
            } => write!(
                formatter,
                "batch {batch_id} expected index {expected}, got {actual}"
            ),
            Self::DuplicateToolCallId { tool_call_id } => {
                write!(
                    formatter,
                    "tool call id is globally duplicated: {tool_call_id}"
                )
            }
            Self::UnknownToolCall { tool_call_id } => {
                write!(formatter, "unknown tool call: {tool_call_id}")
            }
            Self::PhaseOutOfOrder { tool_call_id } => {
                write!(formatter, "tool phase is out of order: {tool_call_id}")
            }
            Self::DuplicateOutput { tool_call_id } => {
                write!(formatter, "tool output is duplicated: {tool_call_id}")
            }
            Self::FinishedWithoutOutput { tool_call_id } => {
                write!(
                    formatter,
                    "tool completion has no valid output: {tool_call_id}"
                )
            }
            Self::DuplicateFinished { tool_call_id } => {
                write!(formatter, "tool completion is duplicated: {tool_call_id}")
            }
            Self::OutputMissing => {
                formatter.write_str("tool output must contain inline data or an artifact reference")
            }
            Self::OutputAmbiguous => formatter
                .write_str("tool output cannot contain inline data and an artifact reference"),
            Self::InlinePayloadTooLarge { field, bytes } => write!(
                formatter,
                "{field} exceeds the {MAX_TOOL_INLINE_BYTES}-byte inline limit: {bytes}"
            ),
            Self::UnknownOutcome { tool_call_id } => {
                write!(
                    formatter,
                    "tool completion outcome is unknown: {tool_call_id}"
                )
            }
        }
    }
}

impl std::error::Error for ToolPhaseError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolPhaseLedger {
    records: Vec<DurableRecord>,
    batches: Vec<ToolBatch>,
    batch_positions: BTreeMap<String, usize>,
    call_positions: BTreeMap<String, (usize, usize)>,
    started_operations: BTreeSet<String>,
    terminal_operations: BTreeSet<String>,
    open_attempts: BTreeMap<String, BTreeSet<String>>,
    tool_intent_operations: BTreeSet<String>,
    open_tool_calls_by_operation: BTreeMap<String, usize>,
}

impl ToolPhaseLedger {
    pub fn from_records(records: &[DurableRecord]) -> Result<Self, ToolPhaseError> {
        if records.iter().any(is_legacy_tool_phase) {
            return Err(ToolPhaseError::LegacyToolPhase);
        }
        AttemptLedger::validate(records).map_err(|_| ToolPhaseError::InvalidRecords)?;
        let mut ledger = Self::default();
        for record in records {
            ledger.apply(record)?;
        }
        ledger.records = records.to_vec();
        Ok(ledger)
    }

    pub fn validate(records: &[DurableRecord]) -> Result<(), ToolPhaseError> {
        Self::from_records(records).map(|_| ())
    }

    pub(crate) fn apply_record(&mut self, record: &DurableRecord) -> Result<(), ToolPhaseError> {
        if is_legacy_tool_phase(record) {
            return Err(ToolPhaseError::LegacyToolPhase);
        }
        self.apply(record)
    }

    /// Append one durable record to a reconstructed prefix. Validation is
    /// complete before this ledger is changed.
    pub fn append<R>(&mut self, record: R) -> Result<(), ToolPhaseError>
    where
        R: Borrow<DurableRecord>,
    {
        let mut records = self.records.clone();
        records.push(record.borrow().clone());
        let next = Self::from_records(&records)?;
        *self = next;
        Ok(())
    }

    pub fn ingest(&mut self, record: DurableRecord) -> Result<(), ToolPhaseError> {
        self.append(&record)
    }

    pub fn batches(&self) -> &[ToolBatch] {
        &self.batches
    }

    pub fn batch(&self, batch_id: &str) -> Option<&ToolBatch> {
        self.batch_positions
            .get(batch_id)
            .and_then(|position| self.batches.get(*position))
    }

    pub fn incomplete(&self) -> Vec<&ToolCallState> {
        self.batches
            .iter()
            .flat_map(|batch| batch.calls.iter())
            .filter(|call| call.is_incomplete())
            .collect()
    }

    fn apply(&mut self, record: &DurableRecord) -> Result<(), ToolPhaseError> {
        let DurableRecord::Operation { operation, .. } = record else {
            return Ok(());
        };
        let operation_id = operation.operation_id.as_str();
        match &operation.kind {
            DurableOperationKind::ToolPhaseIntent {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
                tool_name,
                replay_policy,
                input_redacted,
            } => {
                self.validate_operation(operation_id)?;
                if let Some(attempt_id) = self
                    .open_attempts
                    .get(operation_id)
                    .and_then(|attempts| attempts.iter().next())
                {
                    return Err(ToolPhaseError::OpenProviderAttempt {
                        operation_id: operation_id.into(),
                        attempt_id: attempt_id.clone(),
                    });
                }
                validate_metadata(batch_id, "batch_id")?;
                validate_metadata(tool_call_id, "tool_call_id")?;
                validate_metadata(tool_name, "tool_name")?;
                validate_batch_key(batch_id, *batch_index, *batch_limit)?;
                validate_inline(input_redacted, "input_redacted")?;
                if self.call_positions.contains_key(tool_call_id) {
                    return Err(ToolPhaseError::DuplicateToolCallId {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                let batch_position =
                    self.ensure_batch(operation_id, batch_id, *batch_index, *batch_limit)?;
                let call_position = self.batches[batch_position].calls.len();
                self.tool_intent_operations.insert(operation_id.to_owned());
                self.batches[batch_position].calls.push(ToolCallState {
                    operation_id: operation_id.into(),
                    batch_id: batch_id.clone(),
                    batch_index: *batch_index,
                    batch_limit: *batch_limit,
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    replay_policy: replay_policy.clone(),
                    input_redacted: input_redacted.clone(),
                    started: false,
                    output: None,
                    outcome: None,
                });
                self.call_positions
                    .insert(tool_call_id.clone(), (batch_position, call_position));
                let open_calls = self
                    .open_tool_calls_by_operation
                    .entry(operation_id.to_owned())
                    .or_default();
                *open_calls = open_calls
                    .checked_add(1)
                    .ok_or(ToolPhaseError::InvalidRecords)?;
            }
            DurableOperationKind::ToolPhaseStarted {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
            } => {
                self.validate_operation(operation_id)?;
                let position = self.find_call(
                    operation_id,
                    batch_id,
                    *batch_index,
                    *batch_limit,
                    tool_call_id,
                )?;
                let call = &mut self.batches[position.0].calls[position.1];
                if call.started || call.output.is_some() || call.outcome.is_some() {
                    return Err(ToolPhaseError::PhaseOutOfOrder {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                call.started = true;
            }
            DurableOperationKind::ToolPhaseOutput {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
                output,
                artifact_ref,
            } => {
                self.validate_operation(operation_id)?;
                validate_output(output.as_deref(), artifact_ref.as_deref())?;
                let position = self.find_call(
                    operation_id,
                    batch_id,
                    *batch_index,
                    *batch_limit,
                    tool_call_id,
                )?;
                let call = &mut self.batches[position.0].calls[position.1];
                if !call.started || call.outcome.is_some() {
                    return Err(ToolPhaseError::PhaseOutOfOrder {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                if call.output.is_some() {
                    return Err(ToolPhaseError::DuplicateOutput {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                call.output = Some(match (output, artifact_ref) {
                    (Some(value), None) => ToolOutput::Inline(value.clone()),
                    (None, Some(reference)) => ToolOutput::Artifact(reference.clone()),
                    _ => unreachable!("validate_output checked the output shape"),
                });
            }
            DurableOperationKind::ToolPhaseFinished {
                batch_id,
                batch_index,
                batch_limit,
                tool_call_id,
                outcome,
            } => {
                self.validate_operation(operation_id)?;
                if *outcome == DurableOutcome::Unknown {
                    return Err(ToolPhaseError::UnknownOutcome {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                let position = self.find_call(
                    operation_id,
                    batch_id,
                    *batch_index,
                    *batch_limit,
                    tool_call_id,
                )?;
                let call = &mut self.batches[position.0].calls[position.1];
                if !call.started {
                    return Err(ToolPhaseError::PhaseOutOfOrder {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                if call.output.is_none() {
                    return Err(ToolPhaseError::FinishedWithoutOutput {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                if call.outcome.is_some() {
                    return Err(ToolPhaseError::DuplicateFinished {
                        tool_call_id: tool_call_id.clone(),
                    });
                }
                let open_calls = self
                    .open_tool_calls_by_operation
                    .get(operation_id)
                    .copied()
                    .ok_or(ToolPhaseError::InvalidRecords)?;
                if open_calls == 0 {
                    return Err(ToolPhaseError::InvalidRecords);
                }
                call.outcome = Some(outcome.clone());
                self.close_tool_call(operation_id)?;
            }
            DurableOperationKind::Started { .. } => {
                self.started_operations.insert(operation_id.to_owned());
            }
            DurableOperationKind::QueueIntent { .. } => {
                self.started_operations.insert(operation_id.to_owned());
            }
            DurableOperationKind::Claimed => {}
            DurableOperationKind::ProviderAttemptStarted { attempt_id, .. } => {
                if self.tool_intent_operations.contains(operation_id) {
                    return Err(ToolPhaseError::ProviderAttemptAfterToolIntent {
                        operation_id: operation_id.into(),
                        attempt_id: attempt_id.clone(),
                    });
                }
                self.open_attempts
                    .entry(operation_id.to_owned())
                    .or_default()
                    .insert(attempt_id.clone());
            }
            DurableOperationKind::ProviderAttemptFinished { attempt_id, .. }
            | DurableOperationKind::ProviderAttemptFailed { attempt_id, .. } => {
                self.close_attempt(operation_id, attempt_id);
            }
            DurableOperationKind::Finished { .. } | DurableOperationKind::Aborted => {
                if self
                    .open_tool_calls_by_operation
                    .get(operation_id)
                    .copied()
                    .unwrap_or(0)
                    != 0
                {
                    return Err(ToolPhaseError::OperationTerminalWithIncompleteTools {
                        operation_id: operation_id.into(),
                    });
                }
                self.terminal_operations.insert(operation_id.to_owned());
            }
            // The legacy pair deliberately remains outside the Stage 6 ledger.
            DurableOperationKind::ToolIntent { .. }
            | DurableOperationKind::ToolFinished { .. }
            | DurableOperationKind::RetryConfigured { .. }
            | DurableOperationKind::Suspended { .. } => {}
        }
        Ok(())
    }

    fn validate_operation(&self, operation_id: &str) -> Result<(), ToolPhaseError> {
        validate_metadata(operation_id, "operation_id")?;
        if !self.started_operations.contains(operation_id) {
            return Err(ToolPhaseError::OperationNotStarted {
                operation_id: operation_id.into(),
            });
        }
        if self.terminal_operations.contains(operation_id) {
            return Err(ToolPhaseError::OperationTerminal {
                operation_id: operation_id.into(),
            });
        }
        Ok(())
    }

    fn close_attempt(&mut self, operation_id: &str, attempt_id: &str) {
        let remove_operation = self
            .open_attempts
            .get_mut(operation_id)
            .is_some_and(|attempts| {
                attempts.remove(attempt_id);
                attempts.is_empty()
            });
        if remove_operation {
            self.open_attempts.remove(operation_id);
        }
    }

    fn close_tool_call(&mut self, operation_id: &str) -> Result<(), ToolPhaseError> {
        let Some(open_calls) = self.open_tool_calls_by_operation.get_mut(operation_id) else {
            return Err(ToolPhaseError::InvalidRecords);
        };
        if *open_calls == 0 {
            return Err(ToolPhaseError::InvalidRecords);
        }
        *open_calls -= 1;
        if *open_calls == 0 {
            self.open_tool_calls_by_operation.remove(operation_id);
        }
        Ok(())
    }

    fn ensure_batch(
        &mut self,
        operation_id: &str,
        batch_id: &str,
        batch_index: u32,
        batch_limit: u32,
    ) -> Result<usize, ToolPhaseError> {
        if let Some(position) = self.batch_positions.get(batch_id).copied() {
            let batch = &self.batches[position];
            if batch.operation_id != operation_id || batch.batch_limit != batch_limit {
                return Err(ToolPhaseError::BatchIdentityMismatch {
                    batch_id: batch_id.into(),
                });
            }
            let expected = batch.calls.len() as u32;
            if expected != batch_index {
                return Err(ToolPhaseError::BatchIndexOutOfOrder {
                    batch_id: batch_id.into(),
                    expected,
                    actual: batch_index,
                });
            }
            return Ok(position);
        }
        if batch_index != 0 {
            return Err(ToolPhaseError::BatchIndexOutOfOrder {
                batch_id: batch_id.into(),
                expected: 0,
                actual: batch_index,
            });
        }
        let position = self.batches.len();
        self.batches.push(ToolBatch {
            operation_id: operation_id.into(),
            batch_id: batch_id.into(),
            batch_limit,
            calls: Vec::new(),
        });
        self.batch_positions.insert(batch_id.into(), position);
        Ok(position)
    }

    fn find_call(
        &self,
        operation_id: &str,
        batch_id: &str,
        batch_index: u32,
        batch_limit: u32,
        tool_call_id: &str,
    ) -> Result<(usize, usize), ToolPhaseError> {
        validate_metadata(batch_id, "batch_id")?;
        validate_metadata(tool_call_id, "tool_call_id")?;
        validate_batch_key(batch_id, batch_index, batch_limit)?;
        let position = self
            .call_positions
            .get(tool_call_id)
            .copied()
            .ok_or_else(|| ToolPhaseError::UnknownToolCall {
                tool_call_id: tool_call_id.into(),
            })?;
        let call = &self.batches[position.0].calls[position.1];
        if call.operation_id != operation_id
            || call.batch_id != batch_id
            || call.batch_index != batch_index
            || call.batch_limit != batch_limit
        {
            return Err(ToolPhaseError::BatchIdentityMismatch {
                batch_id: batch_id.into(),
            });
        }
        Ok(position)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayDisposition {
    Never,
    SafePending,
    AlreadyFinished,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayItem {
    pub call: ToolCallState,
    pub disposition: ReplayDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayPlan {
    items: Vec<ReplayItem>,
}

impl ReplayPlan {
    pub fn from_records(records: &[DurableRecord]) -> Result<Self, ToolPhaseError> {
        let ledger = ToolPhaseLedger::from_records(records)?;
        Ok(Self::from_ledger(&ledger))
    }

    pub fn from_ledger(ledger: &ToolPhaseLedger) -> Self {
        let items = ledger
            .batches
            .iter()
            .flat_map(|batch| batch.calls.iter())
            .map(|call| ReplayItem {
                call: call.clone(),
                disposition: if call.is_finished() {
                    ReplayDisposition::AlreadyFinished
                } else {
                    match call.replay_policy {
                        ReplayPolicy::Never => ReplayDisposition::Never,
                        ReplayPolicy::Safe => ReplayDisposition::SafePending,
                    }
                },
            })
            .collect();
        Self { items }
    }

    pub fn items(&self) -> &[ReplayItem] {
        &self.items
    }

    pub fn pending(&self) -> Vec<&ReplayItem> {
        self.items
            .iter()
            .filter(|item| item.disposition == ReplayDisposition::SafePending)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn validate_key(value: &str, field: &'static str) -> Result<(), ToolPhaseError> {
    if value.is_empty() {
        Err(ToolPhaseError::EmptyId { field })
    } else {
        Ok(())
    }
}

pub(crate) fn is_legacy_tool_phase(record: &DurableRecord) -> bool {
    matches!(
        record,
        DurableRecord::Operation {
            operation: super::schema_v2::DurableOperation {
                kind: DurableOperationKind::ToolIntent { .. }
                    | DurableOperationKind::ToolFinished { .. },
                ..
            },
            ..
        }
    )
}

fn validate_metadata(value: &str, field: &'static str) -> Result<(), ToolPhaseError> {
    validate_key(value, field)?;
    if value.len() > MAX_TOOL_METADATA_BYTES {
        return Err(ToolPhaseError::MetadataTooLarge {
            field,
            bytes: value.len(),
        });
    }
    Ok(())
}

fn validate_batch_key(
    batch_id: &str,
    batch_index: u32,
    batch_limit: u32,
) -> Result<(), ToolPhaseError> {
    if !(1..=MAX_TOOL_BATCH_LIMIT).contains(&batch_limit) {
        return Err(ToolPhaseError::BatchLimitOutOfRange {
            batch_id: batch_id.into(),
            batch_limit,
        });
    }
    if batch_index >= batch_limit {
        return Err(ToolPhaseError::BatchIndexOutOfRange {
            batch_id: batch_id.into(),
            batch_index,
            batch_limit,
        });
    }
    Ok(())
}

fn validate_inline(value: &str, field: &'static str) -> Result<(), ToolPhaseError> {
    let bytes = value.len();
    if bytes > MAX_TOOL_INLINE_BYTES {
        Err(ToolPhaseError::InlinePayloadTooLarge { field, bytes })
    } else {
        Ok(())
    }
}

fn validate_output(output: Option<&str>, artifact_ref: Option<&str>) -> Result<(), ToolPhaseError> {
    match (output, artifact_ref) {
        (Some(value), None) => validate_inline(value, "output"),
        (None, Some(reference)) => validate_metadata(reference, "artifact_ref"),
        (Some(_), Some(_)) => Err(ToolPhaseError::OutputAmbiguous),
        (None, None) => Err(ToolPhaseError::OutputMissing),
    }
}
