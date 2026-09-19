use std::fmt;
use std::future::Future;
use std::io;

use serde_json::Value;

use super::effects::{Effect, EffectId};
use super::reducer::{restore_records, DurableState, ReduceError};
use super::repository::DurableRepo;
use super::schema_v2::{
    DurableEntry, DurableEntryRole, DurableErrorClass, DurableOperation, DurableOperationKind,
    DurableOutcome, DurableRecord, DurableUsage,
};

/// Deterministic provider output returned by a manual executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderResponse {
    pub content: String,
    pub usage: Option<DurableUsage>,
    pub outcome: DurableOutcome,
    pub transcript: Vec<crate::provider::ProviderMessage>,
    pub task_facts: Vec<super::schema_v2::DurableFact>,
}

impl ProviderResponse {
    pub fn new(content: impl Into<String>, usage: Option<DurableUsage>) -> Self {
        Self {
            content: content.into(),
            usage,
            outcome: DurableOutcome::Success,
            transcript: Vec::new(),
            task_facts: Vec::new(),
        }
    }

    pub fn with_outcome(
        content: impl Into<String>,
        usage: Option<DurableUsage>,
        outcome: DurableOutcome,
    ) -> Self {
        Self {
            content: content.into(),
            usage,
            outcome,
            transcript: Vec::new(),
            task_facts: Vec::new(),
        }
    }
}

/// Stable, non-sensitive metadata that identifies one durable provider run.
/// The start snapshot is written before the provider or any tool can run; a
/// terminal snapshot with the same key later replaces it in the reducer while
/// both records remain available in the append-only JSONL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunTelemetryContext {
    pub experiment_id: Option<String>,
    pub task_id: Option<String>,
    pub mode: crate::OperatingMode,
    pub provider: String,
    pub model: String,
    pub build_revision: String,
    /// Unix timestamp in milliseconds.
    pub started_at: u64,
    /// Configured limits known before execution. The terminal snapshot may
    /// replace this with the fully resolved limits used by the runtime.
    pub limits: Value,
}

/// Runtime-derived terminal fields for one durable run telemetry snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunTelemetryTerminal {
    pub stop: String,
    pub outcome: DurableOutcome,
    pub validated_completion: bool,
    pub validation_source: Option<String>,
    pub usage: Value,
    pub costs: Value,
    pub limits: Value,
}

/// Synchronous executor boundary used by the manual durable driver.
pub trait ManualExecutor {
    type Error;

    fn execute(&mut self, effect: &Effect) -> Result<ProviderResponse, Self::Error>;

    /// Classify only a bounded, non-sensitive failure category for durable
    /// retry policy. The default is fail-closed and never retryable.
    fn classify_error(&self, _error: &Self::Error) -> DurableErrorClass {
        DurableErrorClass::Unknown
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictKind {
    EntryId,
    OperationId,
    AttemptId,
}

/// Caller-owned identifiers and sequence seed for one provider operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManualRunSpec {
    pub operation_id: String,
    pub attempt_id: String,
    pub input_entry_id: String,
    pub assistant_entry_id: String,
    pub parent_entry_id: Option<String>,
    pub input: String,
    pub first_seq: u64,
    pub input_content_blocks: Vec<crate::provider::ProviderContentBlock>,
    pub run_telemetry: Option<RunTelemetryContext>,
}

impl ManualRunSpec {
    pub fn new(
        operation_id: impl Into<String>,
        attempt_id: impl Into<String>,
        input_entry_id: impl Into<String>,
        assistant_entry_id: impl Into<String>,
        input: impl Into<String>,
        first_seq: u64,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            attempt_id: attempt_id.into(),
            input_entry_id: input_entry_id.into(),
            assistant_entry_id: assistant_entry_id.into(),
            parent_entry_id: None,
            input: input.into(),
            first_seq,
            input_content_blocks: Vec::new(),
            run_telemetry: None,
        }
    }

    pub fn with_parent_entry_id(mut self, parent_entry_id: impl Into<String>) -> Self {
        self.parent_entry_id = Some(parent_entry_id.into());
        self
    }

    pub fn with_run_telemetry(mut self, telemetry: RunTelemetryContext) -> Self {
        self.run_telemetry = Some(telemetry);
        self
    }

    pub(crate) fn effect_id(&self) -> EffectId {
        EffectId::new(&self.operation_id, &self.attempt_id)
    }
}

#[derive(Debug)]
pub enum ManualDriveError<E> {
    Persist(io::Error),
    Execute(E),
    Conflict { kind: ConflictKind, id: String },
    InvalidInput(&'static str),
    SequenceOverflow,
}

impl<E: fmt::Display> fmt::Display for ManualDriveError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persist(error) => write!(formatter, "durable run persistence failed: {error}"),
            Self::Execute(error) => write!(formatter, "manual provider execution failed: {error}"),
            Self::Conflict { kind, id } => {
                write!(formatter, "durable run identity conflict: {kind:?}={id}")
            }
            Self::InvalidInput(field) => write!(formatter, "invalid durable run input: {field}"),
            Self::SequenceOverflow => formatter.write_str("durable run sequence overflowed"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ManualDriveError<E> {}

/// Synchronous driver that owns no clock, randomness, provider, or repository state.
pub struct ManualDrive<'a, R, E> {
    repo: &'a mut R,
    executor: &'a mut E,
}

impl<'a, R, E> ManualDrive<'a, R, E>
where
    R: DurableRepo,
    E: ManualExecutor,
{
    pub fn new(repo: &'a mut R, executor: &'a mut E) -> Self {
        Self { repo, executor }
    }

    /// Persist the input and attempt prefix, execute once, then persist its result.
    pub fn run(&mut self, spec: ManualRunSpec) -> Result<(), ManualDriveError<E::Error>> {
        let seq = persist_manual_prefix(self.repo, &spec)?;
        let effect = Effect::ProviderRequest {
            id: spec.effect_id(),
            input_entry_id: spec.input_entry_id.clone(),
        };
        match self.executor.execute(&effect) {
            Ok(response) => persist_manual_success(self.repo, spec, seq, response),
            Err(error) => {
                persist_manual_failure(
                    self.repo,
                    &spec,
                    seq,
                    self.executor.classify_error(&error),
                )?;
                Err(ManualDriveError::Execute(error))
            }
        }
    }
}

pub type ManualDriver<'a, R, E> = ManualDrive<'a, R, E>;

pub fn drive_manual<R, E>(
    repo: &mut R,
    executor: &mut E,
    spec: ManualRunSpec,
) -> Result<(), ManualDriveError<E::Error>>
where
    R: DurableRepo,
    E: ManualExecutor,
{
    ManualDrive::new(repo, executor).run(spec)
}

pub async fn drive_manual_async<R, E, F, Fut>(
    repo: &mut R,
    spec: ManualRunSpec,
    classify_error: impl Fn(&E) -> DurableErrorClass,
    execute: F,
) -> Result<(), ManualDriveError<E>>
where
    R: DurableRepo,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<ProviderResponse, E>>,
{
    let seq = persist_manual_prefix(repo, &spec)?;
    match execute().await {
        Ok(response) => persist_manual_success(repo, spec, seq, response),
        Err(error) => {
            persist_manual_failure(repo, &spec, seq, classify_error(&error))?;
            Err(ManualDriveError::Execute(error))
        }
    }
}

pub(super) fn persist_manual_prefix<R, E>(
    repo: &mut R,
    spec: &ManualRunSpec,
) -> Result<u64, ManualDriveError<E>>
where
    R: DurableRepo,
{
    preflight(repo, spec)?;
    let mut seq = spec.first_seq;
    let mut prefix = Vec::with_capacity(if spec.run_telemetry.is_some() { 4 } else { 3 });
    prefix.push(DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: spec.input_entry_id.clone(),
            role: DurableEntryRole::User,
            content: spec.input.clone(),
            parent_entry_id: spec.parent_entry_id.clone(),
            operation_id: spec.operation_id.clone(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: spec.input_content_blocks.clone(),
        },
    });
    seq = next_seq(seq)?;
    prefix.push(DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id: spec.operation_id.clone(),
            kind: DurableOperationKind::Started {
                input_entry_id: spec.input_entry_id.clone(),
            },
        },
    });
    seq = next_seq(seq)?;
    prefix.push(DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id: spec.operation_id.clone(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: spec.attempt_id.clone(),
                ordinal: 1,
            },
        },
    });
    if let Some(fact) = run_telemetry_fact(spec, None, None) {
        seq = next_seq(seq)?;
        prefix.push(DurableRecord::Fact { seq, fact });
    }
    repo.append_batch(prefix)
        .map_err(ManualDriveError::Persist)?;
    Ok(seq)
}

pub(super) fn persist_manual_failure<R, E>(
    repo: &mut R,
    spec: &ManualRunSpec,
    seq: u64,
    error: DurableErrorClass,
) -> Result<(), ManualDriveError<E>>
where
    R: DurableRepo,
{
    persist_manual_failure_with_facts(repo, spec, seq, error, Vec::new())
}

pub(super) fn persist_manual_failure_with_facts<R, E>(
    repo: &mut R,
    spec: &ManualRunSpec,
    mut seq: u64,
    error: DurableErrorClass,
    facts: Vec<super::schema_v2::DurableFact>,
) -> Result<(), ManualDriveError<E>>
where
    R: DurableRepo,
{
    let mut records = Vec::with_capacity(facts.len().saturating_add(1));
    seq = next_seq(seq)?;
    records.push(DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id: spec.operation_id.clone(),
            kind: DurableOperationKind::ProviderAttemptFailed {
                attempt_id: spec.attempt_id.clone(),
                error,
            },
        },
    });
    for fact in facts {
        seq = next_seq(seq)?;
        records.push(DurableRecord::Fact { seq, fact });
    }
    repo.append_batch(records)
        .map_err(ManualDriveError::Persist)
}

fn persist_manual_success<R, E>(
    repo: &mut R,
    spec: ManualRunSpec,
    mut seq: u64,
    mut response: ProviderResponse,
) -> Result<(), ManualDriveError<E>>
where
    R: DurableRepo,
{
    let mut suffix = Vec::with_capacity(if response.usage.is_some() { 4 } else { 3 });
    let transcript = if response.transcript.is_empty() {
        vec![crate::provider::ProviderMessage::assistant(
            std::mem::take(&mut response.content),
            Vec::new(),
        )]
    } else {
        std::mem::take(&mut response.transcript)
    };
    let mut parent = Some(spec.input_entry_id.clone());
    let last_index = transcript.len() - 1;
    let mut entries = Vec::with_capacity(transcript.len());
    for (index, message) in transcript.into_iter().enumerate() {
        let id = if index == last_index {
            spec.assistant_entry_id.clone()
        } else {
            format!("{}-message-{index}", spec.operation_id)
        };
        let entry = DurableEntry::from_provider_message(
            id.clone(),
            parent,
            spec.operation_id.clone(),
            message,
        )
        .map_err(ManualDriveError::InvalidInput)?;
        entries.push(entry);
        parent = Some(id);
    }
    super::provider_messages_from_entries(&entries).map_err(ManualDriveError::InvalidInput)?;
    for entry in entries {
        seq = next_seq(seq)?;
        suffix.push(DurableRecord::Entry { seq, entry });
    }

    persist_manual_terminal(repo, spec, seq, suffix, response)
}

pub(super) fn persist_manual_terminal<R: DurableRepo, E>(
    repo: &mut R,
    spec: ManualRunSpec,
    seq: u64,
    suffix: Vec<DurableRecord>,
    response: ProviderResponse,
) -> Result<(), ManualDriveError<E>> {
    persist_manual_terminal_with_facts(repo, spec, seq, suffix, response, Vec::new())
}

pub(super) fn persist_manual_terminal_with_facts<R: DurableRepo, E>(
    repo: &mut R,
    spec: ManualRunSpec,
    mut seq: u64,
    mut suffix: Vec<DurableRecord>,
    response: ProviderResponse,
    facts: Vec<super::schema_v2::DurableFact>,
) -> Result<(), ManualDriveError<E>> {
    if let Some(mut usage) = response.usage {
        usage.operation_id.clone_from(&spec.operation_id);
        usage.attempt_id.clone_from(&spec.attempt_id);
        seq = next_seq(seq)?;
        suffix.push(DurableRecord::Usage { seq, usage });
    }

    for fact in response.task_facts {
        if fact.namespace != "task.v1" {
            return Err(ManualDriveError::InvalidInput("task fact namespace"));
        }
        seq = next_seq(seq)?;
        suffix.push(DurableRecord::Fact { seq, fact });
    }

    seq = next_seq(seq)?;
    suffix.push(DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id: spec.operation_id.clone(),
            kind: DurableOperationKind::ProviderAttemptFinished {
                attempt_id: spec.attempt_id,
                outcome: response.outcome.clone(),
            },
        },
    });
    seq = next_seq(seq)?;
    let terminal = match response.outcome {
        DurableOutcome::Cancelled => DurableOperationKind::Aborted,
        DurableOutcome::Unknown => DurableOperationKind::Suspended {
            reason: "tool execution did not produce a known terminal result".into(),
        },
        outcome => DurableOperationKind::Finished { outcome },
    };
    suffix.push(DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id: spec.operation_id,
            kind: terminal,
        },
    });
    for fact in facts {
        seq = next_seq(seq)?;
        suffix.push(DurableRecord::Fact { seq, fact });
    }
    repo.append_batch(suffix).map_err(ManualDriveError::Persist)
}

pub(super) fn run_telemetry_terminal_fact(
    spec: &ManualRunSpec,
    terminal: &RunTelemetryTerminal,
    duration_ms: u64,
) -> Option<super::schema_v2::DurableFact> {
    run_telemetry_fact(spec, Some(terminal), Some(duration_ms))
}

fn run_telemetry_fact(
    spec: &ManualRunSpec,
    terminal: Option<&RunTelemetryTerminal>,
    duration_ms: Option<u64>,
) -> Option<super::schema_v2::DurableFact> {
    let context = spec.run_telemetry.as_ref()?;
    let (stop, outcome, validated_completion, validation_source, usage, costs, limits, phase) =
        match terminal {
            Some(terminal) => (
                Value::String(terminal.stop.clone()),
                serde_json::to_value(&terminal.outcome).unwrap_or(Value::Null),
                Value::Bool(terminal.validated_completion),
                serde_json::to_value(&terminal.validation_source).unwrap_or(Value::Null),
                terminal.usage.clone(),
                terminal.costs.clone(),
                terminal.limits.clone(),
                "terminal",
            ),
            None => (
                Value::Null,
                Value::String("running".into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                context.limits.clone(),
                "started",
            ),
        };
    Some(super::schema_v2::DurableFact {
        namespace: "run.telemetry.v1".into(),
        key: spec.operation_id.clone(),
        value: serde_json::json!({
            "operation_id": spec.operation_id,
            "attempt_id": spec.attempt_id,
            "experiment_id": context.experiment_id,
            "task_id": context.task_id,
            "mode": context.mode,
            "provider": context.provider,
            "model": context.model,
            "build_revision": context.build_revision,
            "started_at": context.started_at,
            "duration_ms": duration_ms,
            "stop": stop,
            "outcome": outcome,
            "validated_completion": validated_completion,
            "validation_source": validation_source,
            "usage": usage,
            "costs": costs,
            "limits": limits,
            "phase": phase,
        }),
    })
}

/// Restore is deliberately only a reducer operation: it never receives an executor.
pub fn restore_manual_run<R: DurableRepo>(repo: &R) -> Result<DurableState, ReduceError> {
    restore_records(repo.records())
}

fn validate_spec<E>(spec: &ManualRunSpec) -> Result<(), ManualDriveError<E>> {
    for (value, field) in [
        (&spec.operation_id, "operation_id"),
        (&spec.attempt_id, "attempt_id"),
        (&spec.input_entry_id, "input_entry_id"),
        (&spec.assistant_entry_id, "assistant_entry_id"),
    ] {
        if value.is_empty() {
            return Err(ManualDriveError::InvalidInput(field));
        }
    }
    if spec.input_entry_id == spec.assistant_entry_id {
        return Err(ManualDriveError::InvalidInput("entry_id_distinct"));
    }
    Ok(())
}

fn preflight<R, E>(repo: &R, spec: &ManualRunSpec) -> Result<(), ManualDriveError<E>>
where
    R: DurableRepo,
{
    validate_spec(spec)?;
    let last_seq = repo.records().last().map(DurableRecord::seq);
    let mut parent_found = spec.parent_entry_id.is_none();
    let mut input_exists = false;
    let mut assistant_exists = false;
    let mut operation_exists = false;
    let mut attempt_exists = false;
    for record in repo.records() {
        match record {
            DurableRecord::Entry { entry, .. } => {
                if spec.parent_entry_id.as_deref() == Some(entry.entry_id.as_str()) {
                    parent_found = true;
                }
                input_exists |= entry.entry_id == spec.input_entry_id;
                assistant_exists |= entry.entry_id == spec.assistant_entry_id;
                operation_exists |= entry.operation_id == spec.operation_id;
            }
            DurableRecord::Operation { operation, .. } => {
                operation_exists |= operation.operation_id == spec.operation_id;
                match &operation.kind {
                    DurableOperationKind::ProviderAttemptStarted { attempt_id, .. }
                    | DurableOperationKind::ProviderAttemptFinished { attempt_id, .. }
                    | DurableOperationKind::ProviderAttemptFailed { attempt_id, .. } => {
                        attempt_exists |= attempt_id == &spec.attempt_id;
                    }
                    _ => {}
                }
            }
            DurableRecord::Fact { .. }
            | DurableRecord::Usage { .. }
            | DurableRecord::Compaction { .. } => {}
        }
        if parent_found && input_exists && assistant_exists && operation_exists && attempt_exists {
            break;
        }
    }
    if !parent_found {
        return Err(ManualDriveError::InvalidInput("parent_entry_exists"));
    }

    if input_exists {
        return Err(ManualDriveError::Conflict {
            kind: ConflictKind::EntryId,
            id: spec.input_entry_id.clone(),
        });
    }
    if assistant_exists {
        return Err(ManualDriveError::Conflict {
            kind: ConflictKind::EntryId,
            id: spec.assistant_entry_id.clone(),
        });
    }
    if operation_exists {
        return Err(ManualDriveError::Conflict {
            kind: ConflictKind::OperationId,
            id: spec.operation_id.clone(),
        });
    }
    if attempt_exists {
        return Err(ManualDriveError::Conflict {
            kind: ConflictKind::AttemptId,
            id: spec.attempt_id.clone(),
        });
    }
    if last_seq.is_some_and(|last| spec.first_seq <= last) {
        return Err(ManualDriveError::InvalidInput("first_seq_after_last"));
    }
    if spec.first_seq.checked_add(6).is_none() {
        return Err(ManualDriveError::InvalidInput("sequence_window"));
    }
    Ok(())
}

fn next_seq<E>(seq: u64) -> Result<u64, ManualDriveError<E>> {
    seq.checked_add(1).ok_or(ManualDriveError::SequenceOverflow)
}
