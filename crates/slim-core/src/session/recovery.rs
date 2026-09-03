use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::events::SessionEvent;

use super::{SessionHeader, SessionLine, SessionWriter, MAX_DURABLE_SESSION_BYTES};

#[derive(Debug)]
pub struct RecoveredSession {
    pub header: SessionHeader,
    pub events: Vec<SessionEvent>,
    pub quarantine_path: Option<PathBuf>,
}

pub(crate) struct ParsedSession {
    pub(crate) header: SessionHeader,
    pub(crate) events: Vec<SessionEvent>,
    pub(crate) valid_offset: usize,
    pub(crate) torn_tail: Option<Vec<u8>>,
    pub(crate) needs_separator: bool,
}

pub fn recover(path: impl AsRef<Path>) -> io::Result<RecoveredSession> {
    let path = super::event_log::resolve_session_path(path.as_ref())?;
    let _lock = super::event_log::acquire_lock(&path)?;
    let parsed = parse(&path)?;
    let quarantine_path = if let Some(tail) = parsed.torn_tail.as_ref() {
        Some(quarantine_torn_tail(&path, tail)?)
    } else {
        None
    };

    Ok(RecoveredSession {
        header: parsed.header,
        events: parsed.events,
        quarantine_path,
    })
}

pub(crate) fn parse(path: impl AsRef<Path>) -> io::Result<ParsedSession> {
    let path = path.as_ref();
    let file = OpenOptions::new().read(true).open(path)?;
    let length = file.metadata()?.len();
    if length > MAX_DURABLE_SESSION_BYTES {
        return Err(invalid_data("session exceeds durable size limit"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(MAX_DURABLE_SESSION_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DURABLE_SESSION_BYTES {
        return Err(invalid_data("session exceeds durable size limit"));
    }
    let mut offset = 0usize;
    let mut header = None;
    let mut events = Vec::new();
    let mut torn_tail = None;
    let mut needs_separator = false;

    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.ends_with(b"\n");
        let is_final_line = offset + line.len() == bytes.len();
        let parsed = serde_json::from_slice::<SessionLine>(line);
        match parsed {
            Ok(SessionLine::Session { schema_version, .. }) if header.is_none() => {
                if schema_version != super::CURRENT_SCHEMA_VERSION {
                    return Err(invalid_data("unsupported session schema version"));
                }
                header =
                    SessionLine::header(serde_json::from_slice(line).map_err(io::Error::other)?);
            }
            Ok(SessionLine::Event { seq, event }) => {
                if header.is_none() {
                    return Err(invalid_data("event before session header"));
                }
                if events
                    .last()
                    .is_some_and(|last: &SessionEvent| seq <= last.seq)
                {
                    return Err(invalid_data("session event sequence is not increasing"));
                }
                if event.seq != seq {
                    return Err(invalid_data("event envelope and payload sequence differ"));
                }
                events.push(event);
            }
            Ok(SessionLine::Session { schema_version, .. }) => {
                if schema_version != super::CURRENT_SCHEMA_VERSION {
                    return Err(invalid_data("unsupported session schema version"));
                }
                return Err(invalid_data("duplicate session header"));
            }
            Err(error) => {
                if error.is_eof() && !has_newline && is_final_line {
                    torn_tail = Some(bytes[offset..].to_vec());
                    break;
                }
                return Err(invalid_data("invalid session line"));
            }
        }
        if is_final_line && !has_newline {
            needs_separator = true;
        }
        offset += line.len();
    }

    let header = header.ok_or_else(|| invalid_data("missing session header"))?;
    Ok(ParsedSession {
        header,
        events,
        valid_offset: offset,
        torn_tail,
        needs_separator,
    })
}

pub(crate) fn quarantine_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.quarantine", path.display()))
}

pub(crate) fn quarantine_torn_tail(path: &Path, tail: &[u8]) -> io::Result<PathBuf> {
    let base = quarantine_path(path);
    for index in 0.. {
        let candidate = if index == 0 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{}", base.display(), index))
        };
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        file.write_all(tail)?;
        file.sync_data()?;
        return Ok(candidate);
    }
    unreachable!("quarantine suffix space exhausted")
}

pub fn branch(path: impl AsRef<Path>, child_id: &str, cutoff_seq: u64) -> io::Result<PathBuf> {
    super::branch_v2::validate_child_id(child_id)?;
    let source = super::event_log::resolve_session_path(path.as_ref())?;
    let parent = recover(&source)?;
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("session");
    let child_path = source.with_file_name(format!("{stem}-{child_id}.jsonl"));
    let mut writer = SessionWriter::create_with_header(
        &child_path,
        SessionHeader::new(
            child_id,
            parent.header.cwd,
            Some(parent.header.id),
            Some(cutoff_seq),
        ),
    )?;
    for event in parent
        .events
        .into_iter()
        .filter(|event| event.seq <= cutoff_seq)
    {
        writer.append(&event)?;
    }
    Ok(child_path)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
