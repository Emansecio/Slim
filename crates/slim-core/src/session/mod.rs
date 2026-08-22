mod event_log;
mod index;
mod recovery;
mod snapshot;

use serde::{Deserialize, Serialize};

pub use event_log::SessionWriter;
pub use index::SessionIndex;
pub use recovery::{branch, recover, RecoveredSession};
pub use snapshot::SessionSnapshot;

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub record_type: String,
    pub schema_version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_id: Option<String>,
    pub cutoff_seq: Option<u64>,
}

impl SessionHeader {
    pub(crate) fn new(
        id: impl Into<String>,
        cwd: impl Into<String>,
        parent_id: Option<String>,
        cutoff_seq: Option<u64>,
    ) -> Self {
        Self {
            record_type: "session".into(),
            schema_version: CURRENT_SCHEMA_VERSION,
            id: id.into(),
            timestamp: "unknown".into(),
            cwd: cwd.into(),
            parent_id,
            cutoff_seq,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum SessionLine {
    #[serde(rename = "session")]
    Session {
        schema_version: u32,
        id: String,
        timestamp: String,
        cwd: String,
        parent_id: Option<String>,
        cutoff_seq: Option<u64>,
    },
    #[serde(rename = "event")]
    Event {
        seq: u64,
        event: crate::events::SessionEvent,
    },
}

impl From<SessionHeader> for SessionLine {
    fn from(header: SessionHeader) -> Self {
        Self::Session {
            schema_version: header.schema_version,
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            parent_id: header.parent_id,
            cutoff_seq: header.cutoff_seq,
        }
    }
}

impl SessionLine {
    pub(crate) fn header(self) -> Option<SessionHeader> {
        match self {
            Self::Session {
                schema_version,
                id,
                timestamp,
                cwd,
                parent_id,
                cutoff_seq,
            } => Some(SessionHeader {
                record_type: "session".into(),
                schema_version,
                id,
                timestamp,
                cwd,
                parent_id,
                cutoff_seq,
            }),
            Self::Event { .. } => None,
        }
    }
}
