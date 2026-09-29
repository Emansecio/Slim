//! User-prompt turns of a durable v2 session: listing, rewinding into a new
//! child session, and a cheap head-only peek for session pickers.
//!
//! A turn is one user `Entry` that a `Started` operation names as its input
//! (the shape written by `persist_manual_prefix`: one TUI prompt is one
//! operation). The source file is only ever read.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::branch_v2::validate_child_id;
use super::jsonl_repo::JsonlRepo;
use super::resume::{preflight_session, PreflightStatus, SessionPreflight};
use super::schema_v2::{DurableEntryRole, DurableOperationKind, DurableRecord};
use super::{DurableSessionHeader, SessionFormat};

const PROMPT_PREVIEW_CHARS: usize = 200;

/// One user prompt of a durable session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnInfo {
    /// Zero-based position among the session's turns.
    pub index: usize,
    /// Sequence of the turn's user entry (its first record).
    pub first_seq: u64,
    /// First non-empty line of the prompt as stored (already redacted),
    /// at most 200 characters.
    pub prompt: String,
    /// The turn's operation reached `Finished` or `Aborted`. Only terminal
    /// turns are valid rewind targets.
    pub terminal: bool,
}

#[derive(Debug)]
pub enum TurnError {
    Io(io::Error),
    /// The source is not a durable schema-v2 session (legacy v1 is not migrated).
    UnsupportedSchema,
    /// Torn tail, missing separator, sequence overflow or invalid content;
    /// explicit recovery is required before the session can be used.
    NotHealthy(String),
    InvalidChildId(String),
    TurnOutOfRange {
        index: usize,
        turns: usize,
    },
    /// The requested turn has not reached a terminal record.
    TurnNotTerminal {
        index: usize,
    },
    /// An operation before the requested turn is not terminal, so the cutoff
    /// would not land on a turn boundary.
    OpenWorkBeforeTurn {
        index: usize,
    },
}

impl fmt::Display for TurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "session io error: {error}"),
            Self::UnsupportedSchema => {
                formatter.write_str("turns require a durable schema-v2 session")
            }
            Self::NotHealthy(reason) => {
                write!(formatter, "session needs explicit recovery: {reason}")
            }
            Self::InvalidChildId(reason) => write!(formatter, "invalid child id: {reason}"),
            Self::TurnOutOfRange { index, turns } => {
                write!(formatter, "turn {index} is out of range ({turns} turns)")
            }
            Self::TurnNotTerminal { index } => {
                write!(formatter, "turn {index} has not finished")
            }
            Self::OpenWorkBeforeTurn { index } => write!(
                formatter,
                "an operation before turn {index} is not terminal; the cutoff is not a turn boundary"
            ),
        }
    }
}

impl std::error::Error for TurnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TurnError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Enumerate the user-prompt turns of a healthy durable v2 session in order.
/// Reads the file through `preflight_session`, so the 64 MiB cap applies.
pub fn list_turns(path: &Path) -> Result<Vec<TurnInfo>, TurnError> {
    Ok(scan_turns(&healthy_v2(path)?.records))
}

/// Create a new session whose history is everything before turn `turn_index`
/// and return its path (`<sessions dir>/<child_id>.jsonl`, so the file stem
/// equals the header id as `tui-` session discovery requires).
///
/// The child header records the source as `parent_id` and the last copied
/// sequence as `cutoff_seq`. The source is not modified. Refused when the
/// source is not healthy v2, the turn does not exist or has not finished, or
/// an earlier operation is still open. Turn 0 yields a header-only child
/// (`cutoff_seq` is `None`, or the last pre-prompt record when facts precede
/// the first prompt); that is a valid resumable empty session.
pub fn fork_session_before_turn(
    path: &Path,
    turn_index: usize,
    child_id: &str,
) -> Result<PathBuf, TurnError> {
    validate_child_id(child_id).map_err(|error| TurnError::InvalidChildId(error.to_string()))?;
    let report = healthy_v2(path)?;
    let turns = scan_turns(&report.records);
    let turn = turns.get(turn_index).ok_or(TurnError::TurnOutOfRange {
        index: turn_index,
        turns: turns.len(),
    })?;
    if !turn.terminal {
        return Err(TurnError::TurnNotTerminal { index: turn_index });
    }
    let prefix: Vec<DurableRecord> = report
        .records
        .iter()
        .filter(|record| record.seq() < turn.first_seq)
        .cloned()
        .collect();
    if has_open_operation(&prefix) {
        return Err(TurnError::OpenWorkBeforeTurn { index: turn_index });
    }

    let header = report
        .header
        .as_ref()
        .ok_or_else(|| TurnError::NotHealthy("durable session header is missing".into()))?;
    let parent_id = report
        .session_id
        .clone()
        .ok_or_else(|| TurnError::NotHealthy("durable session id is missing".into()))?;
    let directory = report
        .path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "session has no directory"))?;
    let child_path = directory.join(format!("{child_id}.jsonl"));
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let child_header = DurableSessionHeader::new(
        child_id,
        created.to_string(),
        header.cwd.clone(),
        Some(parent_id),
        prefix.last().map(DurableRecord::seq),
    );
    drop(JsonlRepo::create_with_records(
        &child_path,
        child_header,
        prefix,
    )?);
    Ok(child_path)
}

/// What a session picker needs without opening the whole file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionPeek {
    /// First non-empty line of the first user prompt found in the head,
    /// at most 200 characters.
    pub first_prompt: Option<String>,
    /// Reserved: never computed, since counting turns needs the whole file.
    pub turns_hint: Option<usize>,
}

/// Read only the first `max_head_bytes` of a session file, parse the complete
/// JSON lines in that window and return the first user prompt. A line cut by
/// the window is ignored; the rest of the file is never read.
pub fn peek_session(path: &Path, max_head_bytes: usize) -> io::Result<SessionPeek> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut head = Vec::new();
    file.take(max_head_bytes as u64).read_to_end(&mut head)?;
    let window_cut = (head.len() as u64) < file_len;
    let complete_end = if window_cut || head.ends_with(b"\n") {
        head.iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    } else {
        head.len()
    };
    let first_prompt = head[..complete_end]
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|value| value.get("type").and_then(serde_json::Value::as_str) == Some("entry"))
        .filter_map(|value| {
            let entry = value.get("entry")?;
            if entry.get("role")?.as_str()? != "user" {
                return None;
            }
            prompt_preview(entry.get("content")?.as_str()?)
        })
        .next();
    Ok(SessionPeek {
        first_prompt,
        turns_hint: None,
    })
}

fn healthy_v2(path: &Path) -> Result<SessionPreflight, TurnError> {
    let report = preflight_session(path)?;
    if report.format != Some(SessionFormat::DurableV2) {
        return Err(match report.status {
            PreflightStatus::Invalid { message } => TurnError::NotHealthy(message),
            _ => TurnError::UnsupportedSchema,
        });
    }
    if report.status != PreflightStatus::Healthy
        || report.needs_separator
        || report.sequence_overflow
    {
        return Err(TurnError::NotHealthy(format!("{:?}", report.status)));
    }
    Ok(report)
}

fn scan_turns(records: &[DurableRecord]) -> Vec<TurnInfo> {
    let mut input_of = BTreeMap::<&str, &str>::new();
    let mut terminal = BTreeSet::<&str>::new();
    for record in records {
        let DurableRecord::Operation { operation, .. } = record else {
            continue;
        };
        match &operation.kind {
            DurableOperationKind::Started { input_entry_id } => {
                input_of.insert(&operation.operation_id, input_entry_id);
            }
            DurableOperationKind::Finished { .. } | DurableOperationKind::Aborted => {
                terminal.insert(&operation.operation_id);
            }
            _ => {}
        }
    }
    let mut turns = Vec::new();
    for record in records {
        let DurableRecord::Entry { seq, entry } = record else {
            continue;
        };
        if entry.role != DurableEntryRole::User
            || input_of.get(entry.operation_id.as_str()) != Some(&entry.entry_id.as_str())
        {
            continue;
        }
        turns.push(TurnInfo {
            index: turns.len(),
            first_seq: *seq,
            prompt: prompt_preview(&entry.content).unwrap_or_default(),
            terminal: terminal.contains(entry.operation_id.as_str()),
        });
    }
    turns
}

/// True when some operation seen in `records` has no terminal record in them.
fn has_open_operation(records: &[DurableRecord]) -> bool {
    let mut open = BTreeSet::<&str>::new();
    for record in records {
        let DurableRecord::Operation { operation, .. } = record else {
            continue;
        };
        match operation.kind {
            DurableOperationKind::Finished { .. } | DurableOperationKind::Aborted => {
                open.remove(operation.operation_id.as_str());
            }
            _ => {
                open.insert(&operation.operation_id);
            }
        }
    }
    !open.is_empty()
}

fn prompt_preview(content: &str) -> Option<String> {
    let line = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if line.chars().count() <= PROMPT_PREVIEW_CHARS {
        return Some(line.to_owned());
    }
    let mut preview: String = line.chars().take(PROMPT_PREVIEW_CHARS - 1).collect();
    preview.push('…');
    Some(preview)
}
