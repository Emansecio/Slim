use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::attempts::AttemptLedger;
use super::schema_v2::{DurableOperationKind, DurableRecord};

/// Stable identity for an executable provider effect.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct EffectId {
    pub operation_id: String,
    pub attempt_id: String,
}

impl EffectId {
    pub fn new(operation_id: impl Into<String>, attempt_id: impl Into<String>) -> Self {
        Self {
            operation_id: operation_id.into(),
            attempt_id: attempt_id.into(),
        }
    }
}

/// The only executable effect in the no-tools durable-run slice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Effect {
    ProviderRequest {
        id: EffectId,
        input_entry_id: String,
    },
}

impl Effect {
    pub fn id(&self) -> &EffectId {
        match self {
            Self::ProviderRequest { id, .. } => id,
        }
    }

    pub fn input_entry_id(&self) -> &str {
        match self {
            Self::ProviderRequest { input_entry_id, .. } => input_entry_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectPlanError {
    InvalidRecords,
    AmbiguousOperation { operation_id: String },
    AmbiguousAttempt { attempt_id: String },
    MissingOperationStart { operation_id: String },
    MissingAttemptStart { attempt_id: String },
}

/// Reconstruct provider effects from the durable operation prefix.
///
/// The effect itself is intentionally not persisted as a second schema record:
/// its stable ID and input reference are derived from the operation start and
/// provider-attempt start records. Ambiguous or incomplete correlation fails
/// closed instead of guessing.
pub fn planned_provider_effects(records: &[DurableRecord]) -> Result<Vec<Effect>, EffectPlanError> {
    // Validate causal attempt order before deriving any replayable effect.
    // A structurally reducible prefix is not sufficient: replay must not guess
    // across an orphan attempt, duplicate attempt ID, or terminal operation.
    let ledger =
        AttemptLedger::from_records(records).map_err(|_| EffectPlanError::InvalidRecords)?;

    let mut operation_inputs = BTreeMap::<String, String>::new();
    let mut attempts = Vec::<(String, String)>::new();
    let mut attempt_operations = BTreeMap::<String, String>::new();
    let mut tool_intent_operations = BTreeSet::<String>::new();

    for record in records {
        let DurableRecord::Operation { operation, .. } = record else {
            continue;
        };
        match &operation.kind {
            DurableOperationKind::Started { input_entry_id } => {
                if operation_inputs
                    .insert(operation.operation_id.clone(), input_entry_id.clone())
                    .is_some()
                {
                    return Err(EffectPlanError::AmbiguousOperation {
                        operation_id: operation.operation_id.clone(),
                    });
                }
            }
            DurableOperationKind::ProviderAttemptStarted { attempt_id, .. } => {
                if attempt_operations
                    .insert(attempt_id.clone(), operation.operation_id.clone())
                    .is_some()
                {
                    return Err(EffectPlanError::AmbiguousAttempt {
                        attempt_id: attempt_id.clone(),
                    });
                }
                attempts.push((operation.operation_id.clone(), attempt_id.clone()));
            }
            DurableOperationKind::ProviderAttemptFinished { attempt_id, .. }
            | DurableOperationKind::ProviderAttemptFailed { attempt_id, .. } => {
                match attempt_operations.get(attempt_id) {
                    None => {
                        return Err(EffectPlanError::MissingAttemptStart {
                            attempt_id: attempt_id.clone(),
                        });
                    }
                    Some(started_operation_id)
                        if started_operation_id != &operation.operation_id =>
                    {
                        return Err(EffectPlanError::AmbiguousAttempt {
                            attempt_id: attempt_id.clone(),
                        });
                    }
                    Some(_) => {}
                }
            }
            DurableOperationKind::ToolIntent { .. }
            | DurableOperationKind::ToolPhaseIntent { .. } => {
                tool_intent_operations.insert(operation.operation_id.clone());
            }
            _ => {}
        }
    }

    attempts
        .into_iter()
        .filter(|(operation_id, attempt_id)| {
            !tool_intent_operations.contains(operation_id)
                && !ledger.operation_is_terminal(operation_id)
                && ledger
                    .attempts_for(operation_id)
                    .iter()
                    .any(|attempt| attempt.attempt_id == *attempt_id && attempt.outcome.is_none())
        })
        .map(|(operation_id, attempt_id)| {
            let input_entry_id = operation_inputs.get(&operation_id).ok_or_else(|| {
                EffectPlanError::MissingOperationStart {
                    operation_id: operation_id.clone(),
                }
            })?;
            Ok(Effect::ProviderRequest {
                id: EffectId::new(operation_id, attempt_id),
                input_entry_id: input_entry_id.clone(),
            })
        })
        .collect()
}
