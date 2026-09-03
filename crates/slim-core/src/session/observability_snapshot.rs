use std::fmt;

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use super::reducer::{restore_records, DurableState, ReduceError};
use super::repository::DurableRepo;
use super::schema_v2::{DurableRecord, DurableSessionHeader};

/// An immutable, in-memory view of a durable prefix.
///
/// The snapshot owns both the records and the reduced state. It therefore
/// remains unchanged when the source repository receives later appends. Its
/// `Debug` and `Serialize` representations are deliberately metadata-only;
/// callers that are authorized to inspect durable payloads must use
/// `records()`/`state()` explicitly.
#[derive(Clone, Eq, PartialEq)]
pub struct DurableSnapshot {
    header: DurableSessionHeader,
    records: Vec<DurableRecord>,
    state: DurableState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DurableSnapshotSummary {
    pub session_id: String,
    pub schema_version: u32,
    pub record_count: usize,
    pub last_seq: Option<u64>,
    pub entry_count: usize,
    pub operation_count: usize,
    pub fact_count: usize,
    pub usage_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotError {
    InvalidPrefix(ReduceError),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrefix(error) => {
                write!(formatter, "invalid durable snapshot prefix: {error}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

impl DurableSnapshot {
    pub fn from_repo(repo: &impl DurableRepo) -> Result<Self, SnapshotError> {
        Self::from_repo_prefix(repo, u64::MAX)
    }

    pub fn from_repo_prefix(
        repo: &impl DurableRepo,
        through_seq: u64,
    ) -> Result<Self, SnapshotError> {
        Self::from_records(repo.header().clone(), repo.read_prefix(through_seq))
    }

    pub fn from_records(
        header: DurableSessionHeader,
        records: Vec<DurableRecord>,
    ) -> Result<Self, SnapshotError> {
        let state = restore_records(&records).map_err(SnapshotError::InvalidPrefix)?;
        Ok(Self {
            header,
            records,
            state,
        })
    }

    pub fn header(&self) -> &DurableSessionHeader {
        &self.header
    }

    pub fn records(&self) -> &[DurableRecord] {
        &self.records
    }

    pub fn state(&self) -> &DurableState {
        &self.state
    }

    pub fn last_seq(&self) -> Option<u64> {
        self.state.last_seq()
    }

    pub fn summary(&self) -> DurableSnapshotSummary {
        DurableSnapshotSummary {
            session_id: self.header.id.clone(),
            schema_version: self.header.schema_version(),
            record_count: self.records.len(),
            last_seq: self.last_seq(),
            entry_count: self.state.entries().len(),
            operation_count: self.state.operations().len(),
            fact_count: self.state.facts().len(),
            usage_count: self.state.usage().len(),
        }
    }
}

impl fmt::Debug for DurableSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableSnapshot")
            .field("summary", &self.summary())
            .finish()
    }
}

impl Serialize for DurableSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let summary = self.summary();
        let mut state = serializer.serialize_struct("DurableSnapshot", 1)?;
        state.serialize_field("summary", &summary)?;
        state.end()
    }
}
