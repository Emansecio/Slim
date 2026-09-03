use std::collections::VecDeque;

use serde::Serialize;

use super::observability_hooks::{
    safe_observability_id, DurableAppendProjection, DurableOperationProjection, FactValueKind,
    HookFailure,
};
use super::schema_v2::DurableErrorClass;

/// Hard upper bound for process-local telemetry entries.
pub const MAX_TELEMETRY_EVENTS: usize = 4096;

/// Stable category for one local telemetry item. No payload or provider text
/// is retained here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryKind {
    Entry,
    Fact {
        value_kind: FactValueKind,
    },
    Operation {
        class: OperationClass,
        error: Option<DurableErrorClass>,
    },
    Usage,
    Compaction {
        reason: crate::context::CompactionReason,
        tokens_before: u64,
        tokens_after: u64,
        duration_ms: u64,
    },
    HookFailure {
        failure: HookFailure,
    },
    WatchTornTail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationClass {
    Queue,
    Claim,
    ProviderAttempt,
    RetryConfiguration,
    Tool,
    Suspension,
    Terminal,
    Abort,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TelemetryEvent {
    id: u64,
    seq: Option<u64>,
    operation_id: Option<String>,
    kind: TelemetryKind,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl TelemetryEvent {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    pub fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }

    pub fn kind(&self) -> &TelemetryKind {
        &self.kind
    }

    pub fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    pub fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TelemetryCounters {
    events: u64,
    entries: u64,
    facts: u64,
    operations: u64,
    usage_records: u64,
    hook_failures: u64,
    watch_torn_tails: u64,
    input_tokens: u64,
    output_tokens: u64,
}

impl TelemetryCounters {
    pub fn events(&self) -> u64 {
        self.events
    }

    pub fn entries(&self) -> u64 {
        self.entries
    }

    pub fn facts(&self) -> u64 {
        self.facts
    }

    pub fn operations(&self) -> u64 {
        self.operations
    }

    pub fn usage_records(&self) -> u64 {
        self.usage_records
    }

    pub fn hook_failures(&self) -> u64 {
        self.hook_failures
    }

    pub fn watch_torn_tails(&self) -> u64 {
        self.watch_torn_tails
    }

    pub fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    pub fn output_tokens(&self) -> u64 {
        self.output_tokens
    }
}

/// Bounded process-local telemetry. It never implements or references
/// `DurableRepo`, so telemetry cannot become a durable record by accident.
pub struct TelemetryRing {
    capacity: usize,
    events: VecDeque<TelemetryEvent>,
    next_id: u64,
    id_exhausted: bool,
    overflow_count: u64,
    counters: TelemetryCounters,
}

impl std::fmt::Debug for TelemetryRing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TelemetryRing")
            .field("capacity", &self.capacity)
            .field("event_count", &self.events.len())
            .field("next_id", &self.next_id)
            .field("overflow_count", &self.overflow_count)
            .field("counters", &self.counters)
            .finish()
    }
}

impl TelemetryRing {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.min(MAX_TELEMETRY_EVENTS);
        Self {
            capacity,
            events: VecDeque::with_capacity(capacity),
            next_id: 0,
            id_exhausted: false,
            overflow_count: 0,
            counters: TelemetryCounters::default(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn overflow_count(&self) -> u64 {
        self.overflow_count
    }

    pub fn counters(&self) -> &TelemetryCounters {
        &self.counters
    }

    pub fn events(&self) -> Vec<TelemetryEvent> {
        self.events.iter().cloned().collect()
    }

    pub fn record_projection(&mut self, projection: &DurableAppendProjection) -> Option<u64> {
        let (kind, input_tokens, output_tokens) = match projection {
            DurableAppendProjection::Entry { .. } => {
                self.counters.entries = self.counters.entries.saturating_add(1);
                (TelemetryKind::Entry, None, None)
            }
            DurableAppendProjection::Fact { value_kind, .. } => {
                self.counters.facts = self.counters.facts.saturating_add(1);
                (
                    TelemetryKind::Fact {
                        value_kind: *value_kind,
                    },
                    None,
                    None,
                )
            }
            DurableAppendProjection::Operation { kind, .. } => {
                self.counters.operations = self.counters.operations.saturating_add(1);
                (
                    TelemetryKind::Operation {
                        class: OperationClass::from_projection(kind),
                        error: match kind {
                            DurableOperationProjection::ProviderAttemptFailed { error, .. } => {
                                Some(error.clone())
                            }
                            _ => None,
                        },
                    },
                    None,
                    None,
                )
            }
            DurableAppendProjection::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                self.counters.usage_records = self.counters.usage_records.saturating_add(1);
                self.counters.input_tokens = self
                    .counters
                    .input_tokens
                    .saturating_add(input_tokens.unwrap_or(0));
                self.counters.output_tokens = self
                    .counters
                    .output_tokens
                    .saturating_add(output_tokens.unwrap_or(0));
                (TelemetryKind::Usage, *input_tokens, *output_tokens)
            }
            DurableAppendProjection::Compaction {
                reason,
                tokens_before,
                tokens_after,
                duration_ms,
                ..
            } => (
                TelemetryKind::Compaction {
                    reason: *reason,
                    tokens_before: *tokens_before,
                    tokens_after: *tokens_after,
                    duration_ms: *duration_ms,
                },
                None,
                None,
            ),
        };
        self.counters.events = self.counters.events.saturating_add(1);
        let Some(id) = self.allocate_id() else {
            self.overflow_count = self.overflow_count.saturating_add(1);
            return None;
        };
        let event = TelemetryEvent {
            id,
            seq: Some(projection.seq()),
            operation_id: projection.operation_id().map(safe_observability_id),
            kind,
            input_tokens,
            output_tokens,
        };
        self.push(event);
        Some(id)
    }

    pub fn record_hook_failure(&mut self, failure: HookFailure) -> Option<u64> {
        self.counters.hook_failures = self.counters.hook_failures.saturating_add(1);
        self.counters.events = self.counters.events.saturating_add(1);
        let Some(id) = self.allocate_id() else {
            self.overflow_count = self.overflow_count.saturating_add(1);
            return None;
        };
        self.push(TelemetryEvent {
            id,
            seq: None,
            operation_id: None,
            kind: TelemetryKind::HookFailure { failure },
            input_tokens: None,
            output_tokens: None,
        });
        Some(id)
    }

    pub fn record_watch_torn_tail(&mut self) -> Option<u64> {
        self.counters.watch_torn_tails = self.counters.watch_torn_tails.saturating_add(1);
        self.counters.events = self.counters.events.saturating_add(1);
        let Some(id) = self.allocate_id() else {
            self.overflow_count = self.overflow_count.saturating_add(1);
            return None;
        };
        self.push(TelemetryEvent {
            id,
            seq: None,
            operation_id: None,
            kind: TelemetryKind::WatchTornTail,
            input_tokens: None,
            output_tokens: None,
        });
        Some(id)
    }

    fn push(&mut self, event: TelemetryEvent) {
        if self.capacity == 0 {
            self.overflow_count = self.overflow_count.saturating_add(1);
            return;
        }
        if self.events.len() == self.capacity {
            self.events.pop_front();
            self.overflow_count = self.overflow_count.saturating_add(1);
        }
        self.events.push_back(event);
    }

    fn allocate_id(&mut self) -> Option<u64> {
        if self.id_exhausted {
            return None;
        }
        let id = self.next_id;
        if id == u64::MAX {
            self.id_exhausted = true;
        } else {
            self.next_id += 1;
        }
        Some(id)
    }
}

impl OperationClass {
    fn from_projection(kind: &DurableOperationProjection) -> Self {
        match kind {
            DurableOperationProjection::QueueIntent { .. } => Self::Queue,
            DurableOperationProjection::Claimed => Self::Claim,
            DurableOperationProjection::ProviderAttemptStarted { .. }
            | DurableOperationProjection::ProviderAttemptFinished { .. }
            | DurableOperationProjection::ProviderAttemptFailed { .. } => Self::ProviderAttempt,
            DurableOperationProjection::RetryConfigured { .. } => Self::RetryConfiguration,
            DurableOperationProjection::ToolIntent { .. }
            | DurableOperationProjection::ToolFinished { .. }
            | DurableOperationProjection::ToolPhaseIntent { .. }
            | DurableOperationProjection::ToolPhaseStarted { .. }
            | DurableOperationProjection::ToolPhaseOutput { .. }
            | DurableOperationProjection::ToolPhaseFinished { .. } => Self::Tool,
            DurableOperationProjection::Suspended { .. } => Self::Suspension,
            DurableOperationProjection::Finished { .. } => Self::Terminal,
            DurableOperationProjection::Aborted => Self::Abort,
            DurableOperationProjection::Started { .. } => Self::ProviderAttempt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::schema_v2::DurableEntryRole;
    use super::*;

    fn projection() -> DurableAppendProjection {
        DurableAppendProjection::Entry {
            seq: 0,
            entry_id: "entry".into(),
            operation_id: "operation".into(),
            role: DurableEntryRole::User,
            content_bytes: 0,
        }
    }

    #[test]
    fn telemetry_ids_are_not_reused_after_u64_max() {
        let mut ring = TelemetryRing::new(4);
        ring.next_id = u64::MAX - 1;
        assert_eq!(ring.record_projection(&projection()), Some(u64::MAX - 1));
        assert_eq!(ring.record_projection(&projection()), Some(u64::MAX));
        assert_eq!(ring.record_projection(&projection()), None);
        let ids: Vec<_> = ring.events().into_iter().map(|event| event.id()).collect();
        assert_eq!(ids, vec![u64::MAX - 1, u64::MAX]);
        assert_eq!(ring.overflow_count(), 1);
    }
}
