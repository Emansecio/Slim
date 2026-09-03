use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::attempts::{AttemptLedger, AttemptLedgerError};
use super::jsonl_repo::parse;
use super::queue::{DurableQueue, DurableQueueError, QueueItem};
use super::reducer::{restore_records, DurableState, ReduceError};
use super::repository::{validate_durable_records, DurableRepo};
use super::schema_v2::{
    DurableOperationKind, DurableRecord, DurableSessionHeader, MAX_DURABLE_SESSION_BYTES,
};
use super::tool_phases::{ReplayItem, ToolPhaseError, ToolPhaseLedger};
use super::{JsonlRepo, SessionFormat, SessionHeader};

/// Read-only result of inspecting one durable session file.
///
/// A preflight never opens a writable handle, quarantines a tail, inserts a
/// separator, or truncates bytes. Callers must choose `recover_durable_v2`
/// explicitly before asking a repository to repair a torn tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPreflight {
    pub path: PathBuf,
    pub format: Option<SessionFormat>,
    pub session_id: Option<String>,
    pub header: Option<DurableSessionHeader>,
    pub status: PreflightStatus,
    pub last_seq: Option<u64>,
    pub next_seq: Option<u64>,
    pub sequence_overflow: bool,
    pub needs_separator: bool,
    pub summary: SessionSummary,
    pub records: Vec<DurableRecord>,
}

impl SessionPreflight {
    /// Snapshot a repo that is already open and healthy. Callers that just
    /// appended can hand this back instead of re-reading the file.
    pub fn from_open_repo(repo: &JsonlRepo) -> Self {
        let last_seq = repo.records().last().map(DurableRecord::seq);
        let sequence_overflow = last_seq == Some(u64::MAX);
        Self {
            path: repo.path().to_path_buf(),
            format: Some(SessionFormat::DurableV2),
            session_id: Some(repo.header().id.clone()),
            header: Some(repo.header().clone()),
            status: PreflightStatus::Healthy,
            last_seq,
            next_seq: last_seq
                .and_then(|seq| seq.checked_add(1))
                .or_else(|| last_seq.is_none().then_some(0)),
            sequence_overflow,
            needs_separator: false,
            summary: summarize(repo.records()),
            records: repo.records().to_vec(),
        }
    }

    pub fn can_resume_v2(&self) -> bool {
        self.format == Some(SessionFormat::DurableV2)
            && self.status == PreflightStatus::Healthy
            && !self.needs_separator
            && !self.sequence_overflow
    }

    pub fn is_healthy(&self) -> bool {
        self.status == PreflightStatus::Healthy
    }
}

/// The file has either a confirmed healthy prefix or an explicit condition
/// that requires a caller decision. Invalid content is never silently treated
/// as a recoverable tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreflightStatus {
    Healthy,
    TornTail {
        valid_offset: u64,
        tail_bytes: usize,
    },
    Invalid {
        message: String,
    },
    UnsupportedSchema {
        schema_version: u32,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionSummary {
    pub terminal_operation_ids: Vec<String>,
    pub claimed_operation_ids: Vec<String>,
    pub suspended_operation_ids: Vec<String>,
    pub pending_operation_ids: Vec<String>,
}

impl SessionSummary {
    pub fn terminal_count(&self) -> usize {
        self.terminal_operation_ids.len()
    }

    pub fn claimed_count(&self) -> usize {
        self.claimed_operation_ids.len()
    }

    pub fn suspended_count(&self) -> usize {
        self.suspended_operation_ids.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_operation_ids.len()
    }
}

/// Pure preflight for a session path. Filesystem failures are returned as IO
/// errors; malformed or unsupported session content is represented in the
/// returned status so the caller can report it without mutating the file.
pub fn preflight_session(path: impl AsRef<Path>) -> io::Result<SessionPreflight> {
    let path = fs::canonicalize(path.as_ref())?;
    let mut file = File::open(&path)?;
    let file_len = file.metadata()?.len();
    if file_len > MAX_DURABLE_SESSION_BYTES {
        return Ok(invalid_report(
            path,
            PreflightStatus::Invalid {
                message: "durable session file exceeds the 64 MiB limit".into(),
            },
        ));
    }
    let mut first_line = Vec::new();
    BufReader::new(&mut file).read_until(b'\n', &mut first_line)?;
    if first_line.is_empty() {
        return Ok(invalid_report(
            path,
            PreflightStatus::Invalid {
                message: "session file is empty".into(),
            },
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let first_value: Value = match serde_json::from_slice(&first_line) {
        Ok(value) => value,
        Err(error) => {
            return Ok(invalid_report(
                path,
                PreflightStatus::Invalid {
                    message: error.to_string(),
                },
            ));
        }
    };
    let object = match first_value.as_object() {
        Some(object) => object,
        None => {
            return Ok(invalid_report(
                path,
                PreflightStatus::Invalid {
                    message: "session header must be an object".into(),
                },
            ));
        }
    };
    if object.get("type").and_then(Value::as_str) != Some("session") {
        return Ok(invalid_report(
            path,
            PreflightStatus::Invalid {
                message: "session header type must be session".into(),
            },
        ));
    }
    let Some(schema_version) = object.get("schema_version").and_then(Value::as_u64) else {
        return Ok(invalid_report(
            path,
            PreflightStatus::Invalid {
                message: "session header schema_version is invalid".into(),
            },
        ));
    };
    let schema_version = match u32::try_from(schema_version) {
        Ok(version) => version,
        Err(_) => {
            return Ok(invalid_report(
                path,
                PreflightStatus::UnsupportedSchema {
                    schema_version: u32::MAX,
                },
            ));
        }
    };

    if schema_version == super::CURRENT_SCHEMA_VERSION {
        let header = match serde_json::from_value::<SessionHeader>(first_value) {
            Ok(header) => header,
            Err(error) => {
                return Ok(invalid_report(
                    path,
                    PreflightStatus::Invalid {
                        message: error.to_string(),
                    },
                ));
            }
        };
        return Ok(SessionPreflight {
            path,
            format: Some(SessionFormat::LegacyV1),
            session_id: Some(header.id),
            header: None,
            status: PreflightStatus::UnsupportedSchema { schema_version },
            last_seq: None,
            next_seq: None,
            sequence_overflow: false,
            needs_separator: false,
            summary: SessionSummary::default(),
            records: Vec::new(),
        });
    }
    if schema_version != super::schema_v2::DURABLE_SCHEMA_VERSION {
        return Ok(invalid_report(
            path,
            PreflightStatus::UnsupportedSchema { schema_version },
        ));
    }

    let parsed = match parse(&mut file) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(invalid_report(
                path,
                PreflightStatus::Invalid {
                    message: error.to_string(),
                },
            ));
        }
    };
    let header = match parsed.header.clone() {
        Some(header) => header,
        None => {
            return Ok(invalid_report(
                path,
                PreflightStatus::Invalid {
                    message: "missing durable session header".into(),
                },
            ));
        }
    };
    if let Err(error) = restore_records(&parsed.records) {
        return Ok(SessionPreflight {
            path,
            format: Some(SessionFormat::DurableV2),
            session_id: Some(header.id.clone()),
            header: Some(header),
            status: PreflightStatus::Invalid {
                message: error.to_string(),
            },
            last_seq: parsed.records.last().map(DurableRecord::seq),
            next_seq: parsed
                .records
                .last()
                .and_then(|record| record.seq().checked_add(1)),
            sequence_overflow: parsed
                .records
                .last()
                .is_some_and(|record| record.seq() == u64::MAX),
            needs_separator: parsed.needs_separator,
            summary: summarize(&parsed.records),
            records: parsed.records,
        });
    }
    if let Err(error) = validate_durable_records(&parsed.records) {
        return Ok(SessionPreflight {
            path,
            format: Some(SessionFormat::DurableV2),
            session_id: Some(header.id.clone()),
            header: Some(header),
            status: PreflightStatus::Invalid {
                message: error.to_string(),
            },
            last_seq: parsed.records.last().map(DurableRecord::seq),
            next_seq: parsed
                .records
                .last()
                .and_then(|record| record.seq().checked_add(1)),
            sequence_overflow: parsed
                .records
                .last()
                .is_some_and(|record| record.seq() == u64::MAX),
            needs_separator: parsed.needs_separator,
            summary: summarize(&parsed.records),
            records: parsed.records,
        });
    }

    let status = parsed
        .torn_tail
        .as_ref()
        .map(|tail| PreflightStatus::TornTail {
            valid_offset: parsed.valid_offset as u64,
            tail_bytes: tail.len(),
        })
        .unwrap_or(PreflightStatus::Healthy);
    let last_seq = parsed.records.last().map(DurableRecord::seq);
    let sequence_overflow = last_seq == Some(u64::MAX);
    Ok(SessionPreflight {
        path,
        format: Some(SessionFormat::DurableV2),
        session_id: Some(header.id.clone()),
        header: Some(header),
        status,
        last_seq,
        next_seq: last_seq.and_then(|seq| seq.checked_add(1)).or_else(|| {
            if last_seq.is_none() {
                Some(0)
            } else {
                None
            }
        }),
        sequence_overflow,
        needs_separator: parsed.needs_separator,
        summary: summarize(&parsed.records),
        records: parsed.records,
    })
}

pub fn preflight(path: impl AsRef<Path>) -> io::Result<SessionPreflight> {
    preflight_session(path)
}

/// Perform the only mutating recovery action for v2 JSONL. This delegates to
/// the existing repository repair after a read-only preflight has established
/// that the path is v2 and not semantically invalid.
pub fn recover_durable_v2(path: impl AsRef<Path>) -> io::Result<JsonlRepo> {
    // JsonlRepo::open pins the data handle, derives the canonical path from
    // that handle, acquires the lock, validates the same parsed prefix, and
    // only then performs the explicit repair. Avoiding a separate preflight
    // here closes the preflight->open TOCTOU window.
    JsonlRepo::open(path)
}

pub fn recover_v2(path: impl AsRef<Path>) -> io::Result<JsonlRepo> {
    recover_durable_v2(path)
}

#[derive(Debug)]
pub enum ResumePlanError {
    UnsupportedSchema { schema_version: Option<u32> },
    PreflightNotHealthy { status: PreflightStatus },
    SequenceOverflow,
    Repository(io::Error),
    Reducer(ReduceError),
    Attempts(AttemptLedgerError),
    Tools(ToolPhaseError),
    Queue(DurableQueueError),
}

impl fmt::Display for ResumePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { schema_version } => {
                write!(
                    formatter,
                    "resume requires durable schema v2, got {schema_version:?}"
                )
            }
            Self::PreflightNotHealthy { status } => {
                write!(formatter, "resume requires explicit recovery: {status:?}")
            }
            Self::SequenceOverflow => formatter.write_str("durable session sequence overflowed"),
            Self::Repository(error) => {
                write!(formatter, "resume repository handoff failed: {error}")
            }
            Self::Reducer(error) => write!(formatter, "resume reducer failed: {error}"),
            Self::Attempts(error) => write!(formatter, "resume attempt ledger failed: {error}"),
            Self::Tools(error) => write!(formatter, "resume tool ledger failed: {error}"),
            Self::Queue(error) => write!(formatter, "resume queue failed: {error}"),
        }
    }
}

impl std::error::Error for ResumePlanError {}

/// Pure reconstruction of all durable state needed by a headless resume.
/// The plan contains data only; no executor, provider, tool, or queue worker
/// is retained and no effect is invoked while constructing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumePlan {
    state: DurableState,
    attempts: AttemptLedger,
    tools: ToolPhaseLedger,
    queue: DurableQueue,
    tool_replays: Vec<ReplayItem>,
    pending_queue: Vec<QueueItem>,
}

impl ResumePlan {
    pub fn from_records(records: &[DurableRecord]) -> Result<Self, ResumePlanError> {
        let state = restore_records(records).map_err(ResumePlanError::Reducer)?;
        let attempts = AttemptLedger::from_records(records).map_err(ResumePlanError::Attempts)?;
        let tools = ToolPhaseLedger::from_records(records).map_err(ResumePlanError::Tools)?;
        let queue = DurableQueue::from_records(records.len().max(1), records)
            .map_err(ResumePlanError::Queue)?;
        let tool_replays = super::tool_phases::ReplayPlan::from_ledger(&tools)
            .pending()
            .into_iter()
            .cloned()
            .collect();
        let pending_queue = queue.pending();
        Ok(Self {
            state,
            attempts,
            tools,
            queue,
            tool_replays,
            pending_queue,
        })
    }

    pub fn state(&self) -> &DurableState {
        &self.state
    }

    pub fn attempts(&self) -> &AttemptLedger {
        &self.attempts
    }

    pub fn tools(&self) -> &ToolPhaseLedger {
        &self.tools
    }

    pub fn queue(&self) -> &DurableQueue {
        &self.queue
    }

    /// Only explicitly safe, unfinished tool calls are exposed. Never-policy
    /// and already-finished calls are intentionally absent.
    pub fn tool_replays(&self) -> &[ReplayItem] {
        &self.tool_replays
    }

    pub fn pending_queue(&self) -> &[QueueItem] {
        &self.pending_queue
    }
}

pub fn resume_plan_from_preflight(
    preflight: &SessionPreflight,
) -> Result<ResumePlan, ResumePlanError> {
    if preflight.format != Some(SessionFormat::DurableV2) {
        return Err(ResumePlanError::UnsupportedSchema {
            schema_version: preflight
                .format
                .and_then(|format| (format == SessionFormat::LegacyV1).then_some(1)),
        });
    }
    if preflight.status != PreflightStatus::Healthy || preflight.needs_separator {
        return Err(ResumePlanError::PreflightNotHealthy {
            status: preflight.status.clone(),
        });
    }
    if preflight.sequence_overflow {
        return Err(ResumePlanError::SequenceOverflow);
    }
    ResumePlan::from_records(&preflight.records)
}

/// Pin and reopen the exact healthy prefix that was observed by a read-only
/// preflight, then build the plan from that same repository handle. A healthy
/// replacement at the path is rejected before any append can occur.
pub fn open_resume_v2(
    preflight: &SessionPreflight,
) -> Result<(JsonlRepo, ResumePlan), ResumePlanError> {
    if preflight.format != Some(SessionFormat::DurableV2) {
        return Err(ResumePlanError::UnsupportedSchema {
            schema_version: preflight
                .format
                .and_then(|format| (format == SessionFormat::LegacyV1).then_some(1)),
        });
    }
    if preflight.status != PreflightStatus::Healthy || preflight.needs_separator {
        return Err(ResumePlanError::PreflightNotHealthy {
            status: preflight.status.clone(),
        });
    }
    if preflight.sequence_overflow {
        return Err(ResumePlanError::SequenceOverflow);
    }
    let header = preflight.header.as_ref().ok_or_else(|| {
        ResumePlanError::Repository(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing durable session header",
        ))
    })?;
    let repo = JsonlRepo::open_no_repair_expected(&preflight.path, header, &preflight.records)
        .map_err(ResumePlanError::Repository)?;
    let plan = ResumePlan::from_records(repo.records())?;
    Ok((repo, plan))
}

pub fn resume_plan_from_path(path: impl AsRef<Path>) -> Result<ResumePlan, ResumePlanError> {
    let report = preflight_session(path).map_err(|error| ResumePlanError::PreflightNotHealthy {
        status: PreflightStatus::Invalid {
            message: error.to_string(),
        },
    })?;
    resume_plan_from_preflight(&report)
}

pub fn resume_plan(records: &[DurableRecord]) -> Result<ResumePlan, ResumePlanError> {
    ResumePlan::from_records(records)
}

fn summarize(records: &[DurableRecord]) -> SessionSummary {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum State {
        Pending,
        Claimed,
        Suspended,
        Terminal,
    }

    let mut states = BTreeMap::<String, State>::new();
    for record in records {
        let DurableRecord::Operation { operation, .. } = record else {
            continue;
        };
        let state = states
            .entry(operation.operation_id.clone())
            .or_insert(State::Pending);
        *state = match operation.kind {
            DurableOperationKind::QueueIntent { .. }
            | DurableOperationKind::Started { .. }
            | DurableOperationKind::ProviderAttemptStarted { .. }
            | DurableOperationKind::ProviderAttemptFinished { .. }
            | DurableOperationKind::ProviderAttemptFailed { .. }
            | DurableOperationKind::RetryConfigured { .. }
            | DurableOperationKind::ToolIntent { .. }
            | DurableOperationKind::ToolFinished { .. }
            | DurableOperationKind::ToolPhaseIntent { .. }
            | DurableOperationKind::ToolPhaseStarted { .. }
            | DurableOperationKind::ToolPhaseOutput { .. }
            | DurableOperationKind::ToolPhaseFinished { .. } => *state,
            DurableOperationKind::Claimed => State::Claimed,
            DurableOperationKind::Suspended { .. } => State::Suspended,
            DurableOperationKind::Finished { .. } | DurableOperationKind::Aborted => {
                State::Terminal
            }
        };
    }

    let mut summary = SessionSummary::default();
    for (operation_id, state) in states {
        match state {
            State::Terminal => summary.terminal_operation_ids.push(operation_id),
            State::Claimed => summary.claimed_operation_ids.push(operation_id),
            State::Suspended => summary.suspended_operation_ids.push(operation_id),
            State::Pending => summary.pending_operation_ids.push(operation_id),
        }
    }
    summary
}

fn invalid_report(path: PathBuf, status: PreflightStatus) -> SessionPreflight {
    SessionPreflight {
        path,
        format: None,
        session_id: None,
        header: None,
        status,
        last_seq: None,
        next_seq: None,
        sequence_overflow: false,
        needs_separator: false,
        summary: SessionSummary::default(),
        records: Vec::new(),
    }
}
