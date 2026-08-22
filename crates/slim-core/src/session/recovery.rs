use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::events::SessionEvent;

use super::{SessionHeader, SessionLine, SessionWriter};

#[derive(Debug)]
pub struct RecoveredSession {
    pub header: SessionHeader,
    pub events: Vec<SessionEvent>,
    pub quarantine_path: Option<PathBuf>,
}

pub fn recover(path: impl AsRef<Path>) -> io::Result<RecoveredSession> {
    let path = path.as_ref();
    let bytes = fs::read(path)?;
    let mut offset = 0usize;
    let mut header = None;
    let mut events = Vec::new();
    let mut quarantine_path = None;

    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let parsed = serde_json::from_slice::<SessionLine>(line);
        match parsed {
            Ok(SessionLine::Session { .. }) if header.is_none() => {
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
            Ok(SessionLine::Session { .. }) => {
                return Err(invalid_data("duplicate session header"));
            }
            Err(_) => {
                let suffix = &bytes[offset..];
                let quarantine = PathBuf::from(format!("{}.quarantine", path.display()));
                fs::write(&quarantine, suffix)?;
                quarantine_path = Some(quarantine);
                break;
            }
        }
        offset += line.len();
    }

    let header = header.ok_or_else(|| invalid_data("missing session header"))?;
    Ok(RecoveredSession {
        header,
        events,
        quarantine_path,
    })
}

pub fn branch(path: impl AsRef<Path>, child_id: &str, cutoff_seq: u64) -> io::Result<PathBuf> {
    let parent = recover(&path)?;
    let source = path.as_ref();
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
