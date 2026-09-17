use std::collections::BTreeMap;
use std::fmt;

use super::reducer::restore_records;
use super::schema_v2::{
    DurableErrorClass, DurableOperation, DurableOperationKind, DurableOutcome, DurableRecord,
    DurableUsage, RetryPolicy,
};

pub use super::schema_v2::DurableErrorClass as AttemptErrorClass;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attempt {
    pub operation_id: String,
    pub attempt_id: String,
    pub ordinal: u32,
    pub outcome: Option<DurableOutcome>,
    pub error: Option<AttemptErrorClass>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryAttempt {
    pub operation_id: String,
    pub attempt_id: String,
    pub ordinal: u32,
}

impl RetryAttempt {
    pub fn operation(&self) -> DurableOperation {
        DurableOperation {
            operation_id: self.operation_id.clone(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: self.attempt_id.clone(),
                ordinal: self.ordinal,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttemptLedgerError {
    InvalidRecords,
    OperationAlreadyStarted {
        operation_id: String,
    },
    StartedAfterOperationTerminal {
        operation_id: String,
    },
    RetryConfigurationBeforeOperationStart {
        operation_id: String,
    },
    RetryConfigurationAfterAttempt {
        operation_id: String,
    },
    DuplicateRetryConfiguration {
        operation_id: String,
    },
    RetryConfigurationAfterOperationTerminal {
        operation_id: String,
    },
    AttemptBeforeOperationStart {
        operation_id: String,
    },
    AttemptAfterOperationTerminal {
        operation_id: String,
    },
    DuplicateAttemptId {
        attempt_id: String,
    },
    InvalidOrdinal {
        operation_id: String,
        expected: u32,
        actual: u32,
    },
    AttemptNotStarted {
        operation_id: String,
        attempt_id: String,
    },
    AttemptOperationMismatch {
        attempt_id: String,
        operation_id: String,
    },
    AttemptAlreadyFinished {
        attempt_id: String,
    },
    UsageAfterOperationTerminal {
        operation_id: String,
    },
    UsageAttemptMismatch {
        attempt_id: String,
        operation_id: String,
    },
    DuplicateOperationTerminal {
        operation_id: String,
    },
    OperationTerminalBeforeStart {
        operation_id: String,
    },
    OpenAttemptAtOperationTerminal {
        operation_id: String,
    },
    AttemptIdEmpty,
    RetryNotConfigured {
        operation_id: String,
    },
    AttemptNotRetryable {
        operation_id: String,
    },
}

impl fmt::Display for AttemptLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecords => formatter.write_str("invalid durable records"),
            Self::OperationAlreadyStarted { operation_id } => {
                write!(formatter, "operation already started: {operation_id}")
            }
            Self::StartedAfterOperationTerminal { operation_id } => {
                write!(
                    formatter,
                    "operation started after terminal: {operation_id}"
                )
            }
            Self::RetryConfigurationBeforeOperationStart { operation_id } => {
                write!(
                    formatter,
                    "retry configuration before operation start: {operation_id}"
                )
            }
            Self::RetryConfigurationAfterAttempt { operation_id } => {
                write!(
                    formatter,
                    "retry configuration after attempt: {operation_id}"
                )
            }
            Self::DuplicateRetryConfiguration { operation_id } => {
                write!(
                    formatter,
                    "retry configuration is duplicated: {operation_id}"
                )
            }
            Self::RetryConfigurationAfterOperationTerminal { operation_id } => write!(
                formatter,
                "retry configuration after operation terminal: {operation_id}"
            ),
            Self::AttemptBeforeOperationStart { operation_id } => {
                write!(formatter, "attempt before operation start: {operation_id}")
            }
            Self::AttemptAfterOperationTerminal { operation_id } => {
                write!(
                    formatter,
                    "attempt after operation terminal: {operation_id}"
                )
            }
            Self::DuplicateAttemptId { attempt_id } => {
                write!(formatter, "attempt id is globally duplicated: {attempt_id}")
            }
            Self::InvalidOrdinal {
                operation_id,
                expected,
                actual,
            } => write!(
                formatter,
                "attempt ordinal for {operation_id} must be {expected}, got {actual}"
            ),
            Self::AttemptNotStarted {
                operation_id,
                attempt_id,
            } => write!(
                formatter,
                "attempt {attempt_id} for {operation_id} was not started"
            ),
            Self::AttemptOperationMismatch {
                attempt_id,
                operation_id,
            } => write!(
                formatter,
                "attempt {attempt_id} does not belong to operation {operation_id}"
            ),
            Self::AttemptAlreadyFinished { attempt_id } => {
                write!(formatter, "attempt already finished: {attempt_id}")
            }
            Self::UsageAfterOperationTerminal { operation_id } => {
                write!(formatter, "usage after operation terminal: {operation_id}")
            }
            Self::UsageAttemptMismatch {
                attempt_id,
                operation_id,
            } => write!(
                formatter,
                "usage attempt {attempt_id} does not belong to operation {operation_id}"
            ),
            Self::DuplicateOperationTerminal { operation_id } => {
                write!(
                    formatter,
                    "operation terminal is duplicated: {operation_id}"
                )
            }
            Self::OperationTerminalBeforeStart { operation_id } => {
                write!(formatter, "operation terminal before start: {operation_id}")
            }
            Self::OpenAttemptAtOperationTerminal { operation_id } => {
                write!(
                    formatter,
                    "operation terminal has open attempt: {operation_id}"
                )
            }
            Self::AttemptIdEmpty => formatter.write_str("attempt id is empty"),
            Self::RetryNotConfigured { operation_id } => {
                write!(formatter, "retry is not configured: {operation_id}")
            }
            Self::AttemptNotRetryable { operation_id } => {
                write!(
                    formatter,
                    "previous attempt is not safely retryable: {operation_id}"
                )
            }
        }
    }
}

impl std::error::Error for AttemptLedgerError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetryPlanError {
    UnknownOperation { operation_id: String },
    OperationNotStarted { operation_id: String },
    OperationTerminal { operation_id: String },
    RetryNotConfigured { operation_id: String },
    PolicyDisallowsRetry { operation_id: String },
    NoRetryableFailure { operation_id: String },
    DuplicateAttemptId { attempt_id: String },
    AttemptIdEmpty,
    AttemptOrdinalOverflow { operation_id: String },
}

impl fmt::Display for RetryPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOperation { operation_id } => {
                write!(formatter, "unknown operation: {operation_id}")
            }
            Self::OperationNotStarted { operation_id } => {
                write!(formatter, "operation was not started: {operation_id}")
            }
            Self::OperationTerminal { operation_id } => {
                write!(formatter, "operation is terminal: {operation_id}")
            }
            Self::RetryNotConfigured { operation_id } => {
                write!(formatter, "retry is not configured: {operation_id}")
            }
            Self::PolicyDisallowsRetry { operation_id } => {
                write!(formatter, "retry policy disallows retry: {operation_id}")
            }
            Self::NoRetryableFailure { operation_id } => {
                write!(
                    formatter,
                    "operation has no safe transport failure: {operation_id}"
                )
            }
            Self::DuplicateAttemptId { attempt_id } => {
                write!(formatter, "attempt id is already used: {attempt_id}")
            }
            Self::AttemptIdEmpty => formatter.write_str("attempt id is empty"),
            Self::AttemptOrdinalOverflow { operation_id } => {
                write!(formatter, "attempt ordinal overflowed: {operation_id}")
            }
        }
    }
}

impl std::error::Error for RetryPlanError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct OperationState {
    started: bool,
    terminal: bool,
    retry: Option<RetryConfiguration>,
    attempts: Vec<Attempt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetryConfiguration {
    repeatable: bool,
    policy: RetryPolicy,
}

/// Pure reconstruction and validation of provider attempt lifecycle state.
/// No repository, provider, clock, or executor is retained by this type.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttemptLedger {
    operations: BTreeMap<String, OperationState>,
    attempt_operations: BTreeMap<String, String>,
    attempt_positions: BTreeMap<String, usize>,
    usage: BTreeMap<(String, String), DurableUsage>,
}

impl AttemptLedger {
    pub fn from_records(records: &[DurableRecord]) -> Result<Self, AttemptLedgerError> {
        restore_records(records).map_err(|_| AttemptLedgerError::InvalidRecords)?;

        let mut ledger = Self::default();
        for record in records {
            ledger.ingest(record)?;
        }
        Ok(ledger)
    }

    pub fn validate(records: &[DurableRecord]) -> Result<(), AttemptLedgerError> {
        Self::from_records(records).map(|_| ())
    }

    pub(crate) fn apply_record(
        &mut self,
        record: &DurableRecord,
    ) -> Result<(), AttemptLedgerError> {
        self.ingest(record)
    }

    pub fn attempts_for(&self, operation_id: &str) -> &[Attempt] {
        self.operations
            .get(operation_id)
            .map_or(&[], |operation| operation.attempts.as_slice())
    }

    pub fn operation_is_terminal(&self, operation_id: &str) -> bool {
        self.operations
            .get(operation_id)
            .is_some_and(|operation| operation.terminal)
    }

    pub fn usage(&self, operation_id: &str, attempt_id: &str) -> Option<&DurableUsage> {
        self.usage
            .get(&(operation_id.to_owned(), attempt_id.to_owned()))
    }

    /// Produce the next attempt record without mutating or executing anything.
    /// The caller must append the returned operation to the durable prefix.
    pub fn plan_retry(
        &self,
        operation_id: &str,
        attempt_id: &str,
    ) -> Result<RetryAttempt, RetryPlanError> {
        if attempt_id.is_empty() {
            return Err(RetryPlanError::AttemptIdEmpty);
        }
        let operation =
            self.operations
                .get(operation_id)
                .ok_or_else(|| RetryPlanError::UnknownOperation {
                    operation_id: operation_id.into(),
                })?;
        if !operation.started {
            return Err(RetryPlanError::OperationNotStarted {
                operation_id: operation_id.into(),
            });
        }
        if operation.terminal {
            return Err(RetryPlanError::OperationTerminal {
                operation_id: operation_id.into(),
            });
        }
        let retry = operation
            .retry
            .as_ref()
            .ok_or_else(|| RetryPlanError::RetryNotConfigured {
                operation_id: operation_id.into(),
            })?;
        if !retry.repeatable || retry.policy != RetryPolicy::SafeTransport {
            return Err(RetryPlanError::PolicyDisallowsRetry {
                operation_id: operation_id.into(),
            });
        }

        let retryable_failure = operation.attempts.last().and_then(|attempt| {
            attempt.error.as_ref().filter(|error| {
                matches!(
                    error,
                    DurableErrorClass::Transport {
                        safe_to_retry: true
                    }
                )
            })
        });
        if retryable_failure.is_none() {
            return Err(RetryPlanError::NoRetryableFailure {
                operation_id: operation_id.into(),
            });
        }
        if self.attempt_operations.contains_key(attempt_id) {
            return Err(RetryPlanError::DuplicateAttemptId {
                attempt_id: attempt_id.into(),
            });
        }
        let ordinal = operation
            .attempts
            .len()
            .checked_add(1)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| RetryPlanError::AttemptOrdinalOverflow {
                operation_id: operation_id.into(),
            })?;
        Ok(RetryAttempt {
            operation_id: operation_id.into(),
            attempt_id: attempt_id.into(),
            ordinal,
        })
    }

    fn ingest(&mut self, record: &DurableRecord) -> Result<(), AttemptLedgerError> {
        let DurableRecord::Operation { operation, .. } = record else {
            if let DurableRecord::Usage { usage, .. } = record {
                return self.ingest_usage(usage);
            }
            return Ok(());
        };

        match &operation.kind {
            DurableOperationKind::QueueIntent { .. } => {
                let state = self
                    .operations
                    .entry(operation.operation_id.clone())
                    .or_default();
                if state.terminal {
                    return Err(AttemptLedgerError::StartedAfterOperationTerminal {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if state.started {
                    return Err(AttemptLedgerError::OperationAlreadyStarted {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                state.started = true;
            }
            DurableOperationKind::Claimed => {}
            DurableOperationKind::Started { .. } => {
                let state = self
                    .operations
                    .entry(operation.operation_id.clone())
                    .or_default();
                if state.terminal {
                    return Err(AttemptLedgerError::StartedAfterOperationTerminal {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if state.started {
                    return Err(AttemptLedgerError::OperationAlreadyStarted {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                state.started = true;
            }
            DurableOperationKind::RetryConfigured { repeatable, policy } => {
                let state = self
                    .operations
                    .get_mut(&operation.operation_id)
                    .ok_or_else(
                        || AttemptLedgerError::RetryConfigurationBeforeOperationStart {
                            operation_id: operation.operation_id.clone(),
                        },
                    )?;
                if !state.started {
                    return Err(AttemptLedgerError::RetryConfigurationBeforeOperationStart {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if state.terminal {
                    return Err(
                        AttemptLedgerError::RetryConfigurationAfterOperationTerminal {
                            operation_id: operation.operation_id.clone(),
                        },
                    );
                }
                if state.retry.is_some() {
                    return Err(AttemptLedgerError::DuplicateRetryConfiguration {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if !state.attempts.is_empty() {
                    return Err(AttemptLedgerError::RetryConfigurationAfterAttempt {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                state.retry = Some(RetryConfiguration {
                    repeatable: *repeatable,
                    policy: *policy,
                });
            }
            DurableOperationKind::ProviderAttemptStarted {
                attempt_id,
                ordinal,
            } => {
                if attempt_id.is_empty() {
                    return Err(AttemptLedgerError::AttemptIdEmpty);
                }
                let state = self
                    .operations
                    .get_mut(&operation.operation_id)
                    .ok_or_else(|| AttemptLedgerError::AttemptBeforeOperationStart {
                        operation_id: operation.operation_id.clone(),
                    })?;
                if !state.started {
                    return Err(AttemptLedgerError::AttemptBeforeOperationStart {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if state.terminal {
                    return Err(AttemptLedgerError::AttemptAfterOperationTerminal {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if self.attempt_operations.contains_key(attempt_id) {
                    return Err(AttemptLedgerError::DuplicateAttemptId {
                        attempt_id: attempt_id.clone(),
                    });
                }
                let expected = state
                    .attempts
                    .len()
                    .checked_add(1)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(u32::MAX);
                if *ordinal == 0 || *ordinal != expected {
                    return Err(AttemptLedgerError::InvalidOrdinal {
                        operation_id: operation.operation_id.clone(),
                        expected,
                        actual: *ordinal,
                    });
                }
                if *ordinal > 1 {
                    let retry = state.retry.as_ref().ok_or_else(|| {
                        AttemptLedgerError::RetryNotConfigured {
                            operation_id: operation.operation_id.clone(),
                        }
                    })?;
                    if !retry.repeatable || retry.policy != RetryPolicy::SafeTransport {
                        return Err(AttemptLedgerError::AttemptNotRetryable {
                            operation_id: operation.operation_id.clone(),
                        });
                    }
                    let previous = state
                        .attempts
                        .last()
                        .expect("ordinal greater than one has a previous attempt");
                    if !matches!(
                        previous.error,
                        Some(DurableErrorClass::Transport {
                            safe_to_retry: true
                        })
                    ) || previous.outcome.is_none()
                    {
                        return Err(AttemptLedgerError::AttemptNotRetryable {
                            operation_id: operation.operation_id.clone(),
                        });
                    }
                }
                let position = state.attempts.len();
                state.attempts.push(Attempt {
                    operation_id: operation.operation_id.clone(),
                    attempt_id: attempt_id.clone(),
                    ordinal: *ordinal,
                    outcome: None,
                    error: None,
                });
                self.attempt_operations
                    .insert(attempt_id.clone(), operation.operation_id.clone());
                self.attempt_positions.insert(attempt_id.clone(), position);
            }
            DurableOperationKind::ProviderAttemptFinished {
                attempt_id,
                outcome,
            } => {
                self.finish_attempt(
                    &operation.operation_id,
                    attempt_id,
                    Some(outcome.clone()),
                    None,
                )?;
            }
            DurableOperationKind::ProviderAttemptFailed { attempt_id, error } => {
                let outcome = if matches!(error, DurableErrorClass::Cancelled) {
                    DurableOutcome::Cancelled
                } else {
                    DurableOutcome::Failed
                };
                self.finish_attempt(
                    &operation.operation_id,
                    attempt_id,
                    Some(outcome),
                    Some(error.clone()),
                )?;
            }
            DurableOperationKind::Finished { .. } | DurableOperationKind::Aborted => {
                let state = self
                    .operations
                    .get_mut(&operation.operation_id)
                    .ok_or_else(|| AttemptLedgerError::OperationTerminalBeforeStart {
                        operation_id: operation.operation_id.clone(),
                    })?;
                if !state.started {
                    return Err(AttemptLedgerError::OperationTerminalBeforeStart {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if state.terminal {
                    return Err(AttemptLedgerError::DuplicateOperationTerminal {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                if state
                    .attempts
                    .iter()
                    .any(|attempt| attempt.outcome.is_none())
                {
                    return Err(AttemptLedgerError::OpenAttemptAtOperationTerminal {
                        operation_id: operation.operation_id.clone(),
                    });
                }
                state.terminal = true;
            }
            DurableOperationKind::ToolIntent { .. }
            | DurableOperationKind::ToolFinished { .. }
            | DurableOperationKind::ToolPhaseIntent { .. }
            | DurableOperationKind::ToolPhaseStarted { .. }
            | DurableOperationKind::ToolPhaseOutput { .. }
            | DurableOperationKind::ToolPhaseFinished { .. }
            | DurableOperationKind::Suspended { .. } => {
                self.operations
                    .entry(operation.operation_id.clone())
                    .or_default();
            }
        }
        Ok(())
    }

    fn finish_attempt(
        &mut self,
        operation_id: &str,
        attempt_id: &str,
        outcome: Option<DurableOutcome>,
        error: Option<AttemptErrorClass>,
    ) -> Result<(), AttemptLedgerError> {
        if attempt_id.is_empty() {
            return Err(AttemptLedgerError::AttemptIdEmpty);
        }
        let operation = self.operations.get(operation_id).ok_or_else(|| {
            AttemptLedgerError::AttemptNotStarted {
                operation_id: operation_id.into(),
                attempt_id: attempt_id.into(),
            }
        })?;
        if operation.terminal {
            return Err(AttemptLedgerError::AttemptAfterOperationTerminal {
                operation_id: operation_id.into(),
            });
        }
        match self.attempt_operations.get(attempt_id) {
            None => {
                return Err(AttemptLedgerError::AttemptNotStarted {
                    operation_id: operation_id.into(),
                    attempt_id: attempt_id.into(),
                });
            }
            Some(owner) if owner != operation_id => {
                return Err(AttemptLedgerError::AttemptOperationMismatch {
                    attempt_id: attempt_id.into(),
                    operation_id: operation_id.into(),
                });
            }
            Some(_) => {}
        }
        let position = self.attempt_positions[attempt_id];
        let attempt = &mut self
            .operations
            .get_mut(operation_id)
            .expect("owner was just checked")
            .attempts[position];
        if attempt.outcome.is_some() || attempt.error.is_some() {
            return Err(AttemptLedgerError::AttemptAlreadyFinished {
                attempt_id: attempt_id.into(),
            });
        }
        attempt.outcome = outcome;
        attempt.error = error;
        Ok(())
    }

    fn ingest_usage(&mut self, usage: &DurableUsage) -> Result<(), AttemptLedgerError> {
        if usage.attempt_id.is_empty() {
            return Err(AttemptLedgerError::AttemptIdEmpty);
        }
        let operation = self.operations.get(&usage.operation_id).ok_or_else(|| {
            AttemptLedgerError::UsageAttemptMismatch {
                attempt_id: usage.attempt_id.clone(),
                operation_id: usage.operation_id.clone(),
            }
        })?;
        if operation.terminal {
            return Err(AttemptLedgerError::UsageAfterOperationTerminal {
                operation_id: usage.operation_id.clone(),
            });
        }
        match self.attempt_operations.get(&usage.attempt_id) {
            Some(owner) if owner == &usage.operation_id => {}
            _ => {
                return Err(AttemptLedgerError::UsageAttemptMismatch {
                    attempt_id: usage.attempt_id.clone(),
                    operation_id: usage.operation_id.clone(),
                });
            }
        }
        let key = (usage.operation_id.clone(), usage.attempt_id.clone());
        if let Some(existing) = self.usage.get_mut(&key) {
            if usage.input_tokens.is_some() {
                existing.input_tokens = usage.input_tokens;
            }
            if usage.output_tokens.is_some() {
                existing.output_tokens = usage.output_tokens;
            }
        } else {
            self.usage.insert(key, usage.clone());
        }
        Ok(())
    }
}
