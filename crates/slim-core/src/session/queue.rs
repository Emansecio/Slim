use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io;

use super::repository::DurableRepo;
use super::schema_v2::{DurableOperation, DurableOperationKind, DurableOutcome, DurableRecord};

/// Stable identity carried by a durable queue entry.
///
/// The queue stores references only. Prompt text and provider output remain in
/// their own durable entry/operation records and are never copied here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueItem {
    pub operation_id: String,
    pub input_entry_id: Option<String>,
}

impl QueueItem {
    pub fn new(operation_id: impl Into<String>, input_entry_id: impl Into<String>) -> Self {
        Self {
            operation_id: operation_id.into(),
            input_entry_id: Some(input_entry_id.into()),
        }
    }

    pub fn operation(operation_id: impl Into<String>) -> Self {
        Self {
            operation_id: operation_id.into(),
            input_entry_id: None,
        }
    }
}

/// Observable queue state. Suspension details are intentionally queried
/// separately so the state remains a small stable discriminator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueStatus {
    Queued,
    Claimed,
    Suspended,
    Aborted,
    Terminal(DurableOutcome),
}

impl QueueStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Aborted | Self::Terminal(_))
    }
}

#[derive(Debug)]
pub enum DurableQueueError {
    Full {
        capacity: usize,
    },
    DuplicateOperation {
        operation_id: String,
    },
    InvalidInput(&'static str),
    UnknownOperation {
        operation_id: String,
    },
    InvalidTransition {
        operation_id: String,
        status: QueueStatus,
    },
    InvalidRecords(String),
    Persist(io::Error),
    SequenceOverflow,
}

impl fmt::Display for DurableQueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { capacity } => {
                write!(formatter, "durable queue is full (capacity={capacity})")
            }
            Self::DuplicateOperation { operation_id } => {
                write!(
                    formatter,
                    "operation is already present in durable queue: {operation_id}"
                )
            }
            Self::InvalidInput(field) => write!(formatter, "invalid durable queue input: {field}"),
            Self::UnknownOperation { operation_id } => {
                write!(formatter, "unknown durable queue operation: {operation_id}")
            }
            Self::InvalidTransition {
                operation_id,
                status,
            } => {
                write!(
                    formatter,
                    "invalid durable queue transition for {operation_id}: {status:?}"
                )
            }
            Self::InvalidRecords(message) => {
                write!(formatter, "invalid durable queue records: {message}")
            }
            Self::Persist(error) => write!(formatter, "durable queue persistence failed: {error}"),
            Self::SequenceOverflow => formatter.write_str("durable queue sequence overflowed"),
        }
    }
}

impl std::error::Error for DurableQueueError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueueEntry {
    item: QueueItem,
    status: QueueStatus,
    suspension_reason: Option<String>,
}

/// Pure bounded FIFO state machine with an optional durable-repository
/// boundary. Restore only makes work whose last durable state is `Queued`
/// visible to the FIFO; claimed, suspended, aborted, and terminal work needs
/// an explicit caller decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableQueue {
    capacity: usize,
    pending: VecDeque<String>,
    entries: BTreeMap<String, QueueEntry>,
    operation_ids: BTreeSet<String>,
    next_seq: u64,
}

impl DurableQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            pending: VecDeque::new(),
            entries: BTreeMap::new(),
            operation_ids: BTreeSet::new(),
            next_seq: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn pending(&self) -> Vec<QueueItem> {
        self.pending
            .iter()
            .filter_map(|operation_id| self.entries.get(operation_id))
            .map(|entry| entry.item.clone())
            .collect()
    }

    pub fn status(&self, operation_id: &str) -> Option<QueueStatus> {
        self.entries
            .get(operation_id)
            .map(|entry| entry.status.clone())
    }

    pub fn suspension_reason(&self, operation_id: &str) -> Option<&str> {
        self.entries
            .get(operation_id)
            .and_then(|entry| entry.suspension_reason.as_deref())
    }

    /// Returns suspended work as a decision list. Calling restore does not
    /// place these items back in the FIFO.
    pub fn replay_candidates(&self) -> Vec<QueueItem> {
        self.entries
            .values()
            .filter(|entry| entry.status == QueueStatus::Suspended)
            .map(|entry| entry.item.clone())
            .collect()
    }

    pub fn enqueue(&mut self, item: QueueItem) -> Result<(), DurableQueueError> {
        self.validate_item(&item)?;
        self.ensure_not_seen(&item.operation_id)?;
        self.ensure_capacity()?;
        self.insert_queued(item);
        Ok(())
    }

    pub fn claim_next(&mut self) -> Option<QueueItem> {
        let operation_id = self.pending.pop_front()?;
        let entry = self.entries.get_mut(&operation_id)?;
        debug_assert_eq!(entry.status, QueueStatus::Queued);
        entry.status = QueueStatus::Claimed;
        Some(entry.item.clone())
    }

    pub fn claim(&mut self, operation_id: &str) -> Result<QueueItem, DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if entry.status != QueueStatus::Queued {
            return Err(self.transition_error(operation_id));
        }
        self.remove_pending(operation_id);
        let entry = self
            .entries
            .get_mut(operation_id)
            .expect("queue entry checked before mutation");
        entry.status = QueueStatus::Claimed;
        Ok(entry.item.clone())
    }

    pub fn suspend(
        &mut self,
        operation_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if !matches!(entry.status, QueueStatus::Queued | QueueStatus::Claimed) {
            return Err(self.transition_error(operation_id));
        }
        self.remove_pending(operation_id);
        let entry = self
            .entries
            .get_mut(operation_id)
            .expect("queue entry checked before mutation");
        entry.status = QueueStatus::Suspended;
        entry.suspension_reason = Some(reason.into());
        Ok(())
    }

    pub fn abort(&mut self, operation_id: &str) -> Result<(), DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if entry.status.is_terminal() {
            return Err(self.transition_error(operation_id));
        }
        self.remove_pending(operation_id);
        let entry = self
            .entries
            .get_mut(operation_id)
            .expect("queue entry checked before mutation");
        entry.status = QueueStatus::Aborted;
        Ok(())
    }

    pub fn finish(
        &mut self,
        operation_id: &str,
        outcome: DurableOutcome,
    ) -> Result<(), DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if entry.status != QueueStatus::Claimed {
            return Err(self.transition_error(operation_id));
        }
        self.remove_pending(operation_id);
        let entry = self
            .entries
            .get_mut(operation_id)
            .expect("queue entry checked before mutation");
        entry.status = QueueStatus::Terminal(outcome);
        Ok(())
    }

    /// Explicitly put suspended work back in the queue. Restore never calls
    /// this method; callers must make the safe-replay decision themselves.
    pub fn requeue_suspended(&mut self, operation_id: &str) -> Result<(), DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if entry.status != QueueStatus::Suspended {
            return Err(self.transition_error(operation_id));
        }
        self.ensure_capacity()?;
        let entry = self
            .entries
            .get_mut(operation_id)
            .expect("queue entry checked before mutation");
        entry.status = QueueStatus::Queued;
        entry.suspension_reason = None;
        self.pending.push_back(operation_id.to_owned());
        Ok(())
    }

    pub(crate) fn grow_capacity_to(&mut self, capacity: usize) {
        self.capacity = self.capacity.max(capacity.max(1));
    }

    pub(crate) fn apply_validation_record(
        &mut self,
        record: &DurableRecord,
    ) -> Result<(), DurableQueueError> {
        self.apply_record(record)?;
        self.next_seq = record
            .seq()
            .checked_add(1)
            .ok_or(DurableQueueError::SequenceOverflow)?;
        Ok(())
    }

    pub fn from_records(
        capacity: usize,
        records: &[DurableRecord],
    ) -> Result<Self, DurableQueueError> {
        let mut queue = Self::new(capacity);
        let mut previous_seq = None;
        for record in records {
            if previous_seq.is_some_and(|previous| record.seq() <= previous) {
                return Err(DurableQueueError::InvalidRecords(
                    "durable record sequence is not increasing".into(),
                ));
            }
            queue.apply_record(record)?;
            queue.next_seq = record
                .seq()
                .checked_add(1)
                .ok_or(DurableQueueError::SequenceOverflow)?;
            previous_seq = Some(record.seq());
        }
        Ok(queue)
    }

    pub fn restore(capacity: usize, records: &[DurableRecord]) -> Result<Self, DurableQueueError> {
        Self::from_records(capacity, records)
    }

    pub fn from_repo<R: DurableRepo>(capacity: usize, repo: &R) -> Result<Self, DurableQueueError> {
        Self::from_records(capacity, repo.records())
    }

    pub fn restore_repo<R: DurableRepo>(
        capacity: usize,
        repo: &R,
    ) -> Result<Self, DurableQueueError> {
        Self::from_repo(capacity, repo)
    }

    pub fn enqueue_persisted<R: DurableRepo>(
        &mut self,
        repo: &mut R,
        item: QueueItem,
    ) -> Result<(), DurableQueueError> {
        let restored = Self::from_repo(self.capacity, repo)?;
        if self.is_pristine() {
            *self = restored;
        } else if *self != restored {
            return Err(DurableQueueError::InvalidRecords(
                "in-memory queue state does not match durable repository prefix".into(),
            ));
        }
        self.validate_item(&item)?;
        self.ensure_not_seen(&item.operation_id)?;
        if repo
            .records()
            .iter()
            .any(|record| record_operation_id(record) == Some(item.operation_id.as_str()))
        {
            return Err(DurableQueueError::DuplicateOperation {
                operation_id: item.operation_id,
            });
        }
        self.ensure_capacity()?;
        self.append_operation(
            repo,
            &item.operation_id,
            DurableOperationKind::QueueIntent {
                input_entry_id: item.input_entry_id.clone(),
            },
        )?;
        self.insert_queued(item);
        Ok(())
    }

    pub fn claim_next_persisted<R: DurableRepo>(
        &mut self,
        repo: &mut R,
    ) -> Result<Option<QueueItem>, DurableQueueError> {
        let Some(operation_id) = self.pending.front().cloned() else {
            return Ok(None);
        };
        let item = self
            .entries
            .get(&operation_id)
            .map(|entry| entry.item.clone())
            .expect("pending queue entry");
        self.append_operation(repo, &operation_id, DurableOperationKind::Claimed)?;
        self.pending.pop_front();
        self.entries
            .get_mut(&operation_id)
            .expect("pending queue entry")
            .status = QueueStatus::Claimed;
        Ok(Some(item))
    }

    pub fn suspend_persisted<R: DurableRepo>(
        &mut self,
        repo: &mut R,
        operation_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), DurableQueueError> {
        let reason = reason.into();
        let entry = self.entry(operation_id)?;
        if !matches!(entry.status, QueueStatus::Queued | QueueStatus::Claimed) {
            return Err(self.transition_error(operation_id));
        }
        self.append_operation(
            repo,
            operation_id,
            DurableOperationKind::Suspended {
                reason: reason.clone(),
            },
        )?;
        self.remove_pending(operation_id);
        let entry = self.entries.get_mut(operation_id).expect("queue entry");
        entry.status = QueueStatus::Suspended;
        entry.suspension_reason = Some(reason);
        Ok(())
    }

    pub fn abort_persisted<R: DurableRepo>(
        &mut self,
        repo: &mut R,
        operation_id: &str,
    ) -> Result<(), DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if entry.status.is_terminal() {
            return Err(self.transition_error(operation_id));
        }
        self.append_operation(repo, operation_id, DurableOperationKind::Aborted)?;
        self.remove_pending(operation_id);
        self.entries
            .get_mut(operation_id)
            .expect("queue entry")
            .status = QueueStatus::Aborted;
        Ok(())
    }

    pub fn finish_persisted<R: DurableRepo>(
        &mut self,
        repo: &mut R,
        operation_id: &str,
        outcome: DurableOutcome,
    ) -> Result<(), DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if entry.status != QueueStatus::Claimed {
            return Err(self.transition_error(operation_id));
        }
        self.append_operation(
            repo,
            operation_id,
            DurableOperationKind::Finished {
                outcome: outcome.clone(),
            },
        )?;
        self.remove_pending(operation_id);
        self.entries
            .get_mut(operation_id)
            .expect("queue entry")
            .status = QueueStatus::Terminal(outcome);
        Ok(())
    }

    fn validate_item(&self, item: &QueueItem) -> Result<(), DurableQueueError> {
        if item.operation_id.is_empty() {
            return Err(DurableQueueError::InvalidInput("operation_id"));
        }
        if item.input_entry_id.as_deref().is_some_and(str::is_empty) {
            return Err(DurableQueueError::InvalidInput("input_entry_id"));
        }
        Ok(())
    }

    fn is_pristine(&self) -> bool {
        self.pending.is_empty()
            && self.entries.is_empty()
            && self.operation_ids.is_empty()
            && self.next_seq == 0
    }

    fn ensure_not_seen(&self, operation_id: &str) -> Result<(), DurableQueueError> {
        if self.operation_ids.contains(operation_id) {
            Err(DurableQueueError::DuplicateOperation {
                operation_id: operation_id.into(),
            })
        } else {
            Ok(())
        }
    }

    fn ensure_capacity(&self) -> Result<(), DurableQueueError> {
        if self.pending.len() >= self.capacity {
            Err(DurableQueueError::Full {
                capacity: self.capacity,
            })
        } else {
            Ok(())
        }
    }

    fn insert_queued(&mut self, item: QueueItem) {
        let operation_id = item.operation_id.clone();
        self.operation_ids.insert(operation_id.clone());
        self.entries.insert(
            operation_id.clone(),
            QueueEntry {
                item,
                status: QueueStatus::Queued,
                suspension_reason: None,
            },
        );
        self.pending.push_back(operation_id);
    }

    fn entry(&self, operation_id: &str) -> Result<&QueueEntry, DurableQueueError> {
        self.entries
            .get(operation_id)
            .ok_or_else(|| DurableQueueError::UnknownOperation {
                operation_id: operation_id.into(),
            })
    }

    fn transition_error(&self, operation_id: &str) -> DurableQueueError {
        DurableQueueError::InvalidTransition {
            operation_id: operation_id.into(),
            status: self.status(operation_id).unwrap_or(QueueStatus::Aborted),
        }
    }

    fn remove_pending(&mut self, operation_id: &str) {
        self.pending.retain(|queued| queued != operation_id);
    }

    fn append_operation<R: DurableRepo>(
        &mut self,
        repo: &mut R,
        operation_id: &str,
        kind: DurableOperationKind,
    ) -> Result<(), DurableQueueError> {
        let repo_next = repo
            .records()
            .last()
            .map(|record| record.seq().checked_add(1))
            .unwrap_or(Some(0))
            .ok_or(DurableQueueError::SequenceOverflow)?;
        let seq = self.next_seq.max(repo_next);
        let following_seq = seq
            .checked_add(1)
            .ok_or(DurableQueueError::SequenceOverflow)?;
        let record = DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.into(),
                kind,
            },
        };
        repo.append(record).map_err(DurableQueueError::Persist)?;
        self.next_seq = following_seq;
        Ok(())
    }

    fn apply_record(&mut self, record: &DurableRecord) -> Result<(), DurableQueueError> {
        let DurableRecord::Entry { entry, .. } = record else {
            let DurableRecord::Operation { operation, .. } = record else {
                return Ok(());
            };
            return self.apply_operation(operation);
        };
        if entry.operation_id.is_empty() {
            return Err(DurableQueueError::InvalidRecords(
                "entry operation identity is empty".into(),
            ));
        }
        self.operation_ids.insert(entry.operation_id.clone());
        Ok(())
    }

    fn apply_operation(&mut self, operation: &DurableOperation) -> Result<(), DurableQueueError> {
        let operation_id = operation.operation_id.as_str();
        match &operation.kind {
            DurableOperationKind::QueueIntent { input_entry_id } => {
                if operation_id.is_empty()
                    || input_entry_id.as_deref().is_some_and(str::is_empty)
                    || self.operation_ids.contains(operation_id)
                {
                    return Err(DurableQueueError::InvalidRecords(
                        "queue intent identity is empty or duplicated".into(),
                    ));
                }
                self.ensure_capacity()?;
                self.insert_queued(QueueItem {
                    operation_id: operation_id.into(),
                    input_entry_id: input_entry_id.clone(),
                });
            }
            DurableOperationKind::Claimed => {
                self.claim(operation_id).map_err(|error| match error {
                    DurableQueueError::UnknownOperation { .. }
                    | DurableQueueError::InvalidTransition { .. } => {
                        DurableQueueError::InvalidRecords(error.to_string())
                    }
                    other => other,
                })?;
            }
            DurableOperationKind::Suspended { reason } => {
                self.suspend(operation_id, reason.clone())
                    .map_err(|error| match error {
                        DurableQueueError::UnknownOperation { .. }
                        | DurableQueueError::InvalidTransition { .. } => {
                            DurableQueueError::InvalidRecords(error.to_string())
                        }
                        other => other,
                    })?;
            }
            // ManualDrive also ends non-queued turns with Aborted. Their known
            // identity stays reserved, but there is no FIFO item to abort/replay.
            DurableOperationKind::Aborted
                if !self.entries.contains_key(operation_id)
                    && self.operation_ids.contains(operation_id) => {}
            DurableOperationKind::Aborted => {
                self.abort(operation_id).map_err(|error| match error {
                    DurableQueueError::UnknownOperation { .. }
                    | DurableQueueError::InvalidTransition { .. } => {
                        DurableQueueError::InvalidRecords(error.to_string())
                    }
                    other => other,
                })?;
            }
            DurableOperationKind::Finished { outcome } => {
                if let Some(entry) = self.entries.get(operation_id) {
                    if entry.status.is_terminal() {
                        return Err(DurableQueueError::InvalidRecords(
                            "queue terminal state is duplicated".into(),
                        ));
                    }
                    self.finish(operation_id, outcome.clone())?;
                } else {
                    // A non-queue operation may still have a normal Finished
                    // marker. It remains globally deduplicated but is not a
                    // queue entry.
                    self.operation_ids.insert(operation_id.into());
                }
            }
            _ => {
                self.operation_ids.insert(operation_id.into());
            }
        }
        Ok(())
    }
}

fn record_operation_id(record: &DurableRecord) -> Option<&str> {
    match record {
        DurableRecord::Entry { entry, .. } => Some(entry.operation_id.as_str()),
        DurableRecord::Operation { operation, .. } => Some(operation.operation_id.as_str()),
        DurableRecord::Fact { .. }
        | DurableRecord::Usage { .. }
        | DurableRecord::Compaction { .. } => None,
    }
}
