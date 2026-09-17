//! Adversarial coverage for `session/queue.rs` (RODADA 2 — Sifter). The queue
//! is a bounded FIFO with durable replay; these tests feed it record
//! sequences no honest writer produces — out-of-order seqs, lifecycle steps
//! for unknown operations, terminal-before-claim — plus direct API abuse at
//! the capacity and identity boundaries.

use slim_core::session::{
    DurableEntry, DurableEntryRole, DurableOperation, DurableOperationKind, DurableOutcome,
    DurableQueue, DurableQueueError, DurableRecord, DurableRepo, DurableSessionHeader, MemoryRepo,
    QueueItem, QueueStatus,
};

fn intent(seq: u64, operation_id: &str) -> DurableRecord {
    DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id: operation_id.into(),
            kind: DurableOperationKind::QueueIntent {
                input_entry_id: None,
            },
        },
    }
}

fn operation(seq: u64, operation_id: &str, kind: DurableOperationKind) -> DurableRecord {
    DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id: operation_id.into(),
            kind,
        },
    }
}

fn entry(seq: u64, operation_id: &str) -> DurableRecord {
    DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: format!("entry-{seq}"),
            role: DurableEntryRole::User,
            content: "payload".into(),
            parent_entry_id: None,
            operation_id: operation_id.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    }
}

fn header() -> DurableSessionHeader {
    DurableSessionHeader::new("adv", "2026-01-01T00:00:00Z", "D:\\Slim", None, None)
}

// ---------------------------------------------------------------------------
// from_records / restore: sequences a real writer never produces
// ---------------------------------------------------------------------------

#[test]
fn restore_rejects_non_increasing_record_sequences() {
    let records = [intent(5, "op-a"), intent(5, "op-b")];
    let error = DurableQueue::from_records(4, &records).expect_err("duplicate seq");
    assert!(error.to_string().contains("not increasing"));

    let records = [intent(5, "op-a"), intent(2, "op-b")];
    assert!(DurableQueue::from_records(4, &records).is_err());
}

#[test]
fn restore_at_sequence_ceiling_overflows() {
    let records = [intent(u64::MAX, "op-top")];
    let error = DurableQueue::from_records(4, &records).expect_err("seq overflow");
    assert!(matches!(error, DurableQueueError::SequenceOverflow));
}

#[test]
fn restore_rejects_duplicate_queue_intent() {
    let records = [intent(0, "op-a"), intent(1, "op-a")];
    let error = DurableQueue::from_records(4, &records).expect_err("duplicate intent");
    assert!(error.to_string().contains("empty or duplicated"));
}

#[test]
fn restore_rejects_claim_for_unknown_operation() {
    let records = [operation(0, "ghost", DurableOperationKind::Claimed)];
    let error = DurableQueue::from_records(4, &records).expect_err("unknown claim");
    assert!(error.to_string().contains("invalid durable queue records"));
}

#[test]
fn restore_rejects_double_claim() {
    let records = [
        intent(0, "op-a"),
        operation(1, "op-a", DurableOperationKind::Claimed),
        operation(2, "op-a", DurableOperationKind::Claimed),
    ];
    assert!(DurableQueue::from_records(4, &records).is_err());
}

#[test]
fn restore_rejects_suspend_of_terminal_operation() {
    let records = [
        intent(0, "op-a"),
        operation(1, "op-a", DurableOperationKind::Claimed),
        operation(
            2,
            "op-a",
            DurableOperationKind::Finished {
                outcome: DurableOutcome::Success,
            },
        ),
        operation(
            3,
            "op-a",
            DurableOperationKind::Suspended {
                reason: "late".into(),
            },
        ),
    ];
    assert!(DurableQueue::from_records(4, &records).is_err());
}

#[test]
fn restore_rejects_second_terminal_record() {
    let records = [
        intent(0, "op-a"),
        operation(1, "op-a", DurableOperationKind::Claimed),
        operation(
            2,
            "op-a",
            DurableOperationKind::Finished {
                outcome: DurableOutcome::Success,
            },
        ),
        operation(
            3,
            "op-a",
            DurableOperationKind::Finished {
                outcome: DurableOutcome::Failed,
            },
        ),
    ];
    let error = DurableQueue::from_records(4, &records).expect_err("double terminal");
    assert!(error.to_string().contains("terminal state is duplicated"));
}

/// Spec gap worth noting: `Finished` on a queued-but-never-claimed operation
/// surfaces the raw `InvalidTransition` error while sibling transitions
/// (`Claimed`, `Suspended`, `Aborted`) on unknown/invalid states are
/// re-wrapped as `InvalidRecords` (queue.rs:566-597 vs :605). The behavior is
/// correct — the classification is just inconsistent for callers matching on
/// variants.
#[test]
fn finish_on_queued_operation_leaks_raw_transition_error() {
    let records = [
        intent(0, "op-a"),
        operation(
            1,
            "op-a",
            DurableOperationKind::Finished {
                outcome: DurableOutcome::Success,
            },
        ),
    ];
    let error = DurableQueue::from_records(4, &records).expect_err("unclaimed finish");
    assert!(
        matches!(error, DurableQueueError::InvalidTransition { .. }),
        "expected raw InvalidTransition, got: {error}"
    );
}

/// Same inconsistency family: capacity overflow during restore surfaces the
/// raw `Full` variant instead of `InvalidRecords`.
#[test]
fn restore_capacity_overflow_leaks_full_error() {
    let records = [intent(0, "op-a"), intent(1, "op-b"), intent(2, "op-c")];
    let error = DurableQueue::from_records(1, &records).expect_err("over capacity");
    assert!(
        matches!(error, DurableQueueError::Full { capacity: 1 }),
        "expected raw Full, got: {error}"
    );
}

/// An `Aborted` record for an operation only known via an entry is a silent
/// no-op (manual-drive convention, queue.rs:584-588): the id stays reserved
/// but nothing is queued.
#[test]
fn abort_of_entry_known_operation_is_a_silent_noop() {
    let records = [
        entry(0, "op-manual"),
        operation(1, "op-manual", DurableOperationKind::Aborted),
    ];
    let queue = DurableQueue::from_records(4, &records).expect("restore");
    assert!(queue.pending().is_empty());
    assert_eq!(queue.status("op-manual"), None);
    // The identity is still reserved: a later intent for the same op fails.
    let more = [
        entry(0, "op-manual"),
        operation(1, "op-manual", DurableOperationKind::Aborted),
        intent(2, "op-manual"),
    ];
    assert!(DurableQueue::from_records(4, &more).is_err());
}

/// A `Finished` record for an operation never seen at all is accepted as a
/// non-queue terminal marker and reserves the id — but a `Suspended` for an
/// unknown operation is an error. The asymmetry is deliberate (Finished is a
/// valid marker for non-queue work; Suspended is queue-only), pinned here so
/// a future refactor can't silently change it.
#[test]
fn finish_of_unknown_operation_reserves_id_but_suspend_rejects() {
    let records = [operation(
        0,
        "op-elsewhere",
        DurableOperationKind::Finished {
            outcome: DurableOutcome::Success,
        },
    )];
    let mut queue = DurableQueue::from_records(4, &records).expect("non-queue finish");
    assert!(queue.pending().is_empty());
    let error = queue
        .enqueue(QueueItem::operation("op-elsewhere"))
        .expect_err("id reserved");
    assert!(matches!(
        error,
        DurableQueueError::DuplicateOperation { .. }
    ));

    let bad = [operation(
        0,
        "op-elsewhere",
        DurableOperationKind::Suspended {
            reason: "why".into(),
        },
    )];
    assert!(DurableQueue::from_records(4, &bad).is_err());
}

#[test]
fn restore_rejects_entry_with_empty_operation_id() {
    let records = [entry(0, "")];
    let error = DurableQueue::from_records(4, &records).expect_err("empty op id");
    assert!(error.to_string().contains("operation identity is empty"));
}

#[test]
fn restore_rejects_intent_with_empty_input_entry_id() {
    let records = [DurableRecord::Operation {
        seq: 0,
        operation: DurableOperation {
            operation_id: "op-a".into(),
            kind: DurableOperationKind::QueueIntent {
                input_entry_id: Some(String::new()),
            },
        },
    }];
    assert!(DurableQueue::from_records(4, &records).is_err());
}

#[test]
fn restore_rejects_intent_with_empty_operation_id() {
    let records = [DurableRecord::Operation {
        seq: 0,
        operation: DurableOperation {
            operation_id: String::new(),
            kind: DurableOperationKind::QueueIntent {
                input_entry_id: None,
            },
        },
    }];
    assert!(DurableQueue::from_records(4, &records).is_err());
}

// ---------------------------------------------------------------------------
// Direct API boundaries
// ---------------------------------------------------------------------------

#[test]
fn zero_capacity_queue_rejects_first_enqueue() {
    let mut queue = DurableQueue::new(0);
    let error = queue
        .enqueue(QueueItem::operation("op-a"))
        .expect_err("capacity 0");
    assert!(matches!(error, DurableQueueError::Full { capacity: 0 }));
}

#[test]
fn enqueue_rejects_empty_and_empty_input_identities() {
    let mut queue = DurableQueue::new(4);
    let error = queue
        .enqueue(QueueItem::operation(""))
        .expect_err("empty id");
    assert!(matches!(
        error,
        DurableQueueError::InvalidInput("operation_id")
    ));

    let error = queue
        .enqueue(QueueItem::new("op-a", ""))
        .expect_err("empty input entry");
    assert!(matches!(
        error,
        DurableQueueError::InvalidInput("input_entry_id")
    ));
}

#[test]
fn lifecycle_transitions_on_unknown_operations_are_rejected() {
    let mut queue = DurableQueue::new(4);
    for result in [
        queue.claim("ghost").map(|_| ()).err(),
        queue.suspend("ghost", "r").err(),
        queue.abort("ghost").err(),
        queue.finish("ghost", DurableOutcome::Success).err(),
    ] {
        assert!(matches!(
            result,
            Some(DurableQueueError::UnknownOperation { .. })
        ));
    }
}

#[test]
fn finish_requires_claim_and_abort_rejects_terminal() {
    let mut queue = DurableQueue::new(4);
    queue
        .enqueue(QueueItem::operation("op-a"))
        .expect("enqueue");
    let error = queue
        .finish("op-a", DurableOutcome::Success)
        .expect_err("finish needs claim");
    assert!(matches!(error, DurableQueueError::InvalidTransition { .. }));

    queue.claim("op-a").expect("claim");
    queue
        .finish("op-a", DurableOutcome::Success)
        .expect("finish");
    let error = queue.abort("op-a").expect_err("terminal abort");
    assert!(matches!(error, DurableQueueError::InvalidTransition { .. }));
}

#[test]
fn suspended_items_are_replay_candidates_not_fifo() {
    let mut queue = DurableQueue::new(4);
    queue
        .enqueue(QueueItem::operation("op-a"))
        .expect("enqueue");
    queue.suspend("op-a", "waiting").expect("suspend");
    assert!(queue.claim_next().is_none());
    assert_eq!(queue.replay_candidates()[0].operation_id, "op-a");
    assert_eq!(queue.suspension_reason("op-a"), Some("waiting"));

    // An empty suspension reason is accepted today — spec gap to note.
    queue
        .enqueue(QueueItem::operation("op-b"))
        .expect("enqueue");
    queue.suspend("op-b", "").expect("empty reason accepted");
    assert_eq!(queue.suspension_reason("op-b"), Some(""));
}

// ---------------------------------------------------------------------------
// Persisted boundary (MemoryRepo runs the same validator as JsonlRepo)
// ---------------------------------------------------------------------------

#[test]
fn enqueue_persisted_rejects_diverged_in_memory_state() {
    let mut repo = MemoryRepo::new(header());
    let mut queue = DurableQueue::new(4);
    // In-memory mutation without persistence: the next persisted call must
    // detect the divergence instead of silently re-basing.
    queue
        .enqueue(QueueItem::operation("op-ghost"))
        .expect("enqueue");
    let error = queue
        .enqueue_persisted(&mut repo, QueueItem::operation("op-real"))
        .expect_err("divergence");
    assert!(error
        .to_string()
        .contains("does not match durable repository"));
    assert!(repo.records().is_empty(), "no record may be written");
}

#[test]
fn enqueue_persisted_rejects_operation_known_to_repo() {
    let mut repo = MemoryRepo::new(header());
    repo.append(intent(0, "op-seen")).expect("seed");
    let mut queue = DurableQueue::from_repo(4, &repo).expect("restore");
    let error = queue
        .enqueue_persisted(&mut repo, QueueItem::operation("op-seen"))
        .expect_err("duplicate");
    assert!(matches!(
        error,
        DurableQueueError::DuplicateOperation { .. }
    ));
}

#[test]
fn persisted_claim_suspend_finish_roundtrip_restores() {
    let mut repo = MemoryRepo::new(header());
    let mut queue = DurableQueue::new(4);
    queue
        .enqueue_persisted(&mut repo, QueueItem::operation("op-a"))
        .expect("enqueue");
    queue
        .enqueue_persisted(&mut repo, QueueItem::new("op-b", "entry-x"))
        .expect("enqueue b");
    assert_eq!(
        queue
            .claim_next_persisted(&mut repo)
            .expect("claim")
            .expect("item")
            .operation_id,
        "op-a"
    );
    queue
        .suspend_persisted(&mut repo, "op-b", "blocked")
        .expect("suspend");
    queue
        .finish_persisted(&mut repo, "op-a", DurableOutcome::Success)
        .expect("finish");

    let restored = DurableQueue::from_repo(4, &repo).expect("restore");
    assert!(restored.pending().is_empty());
    assert_eq!(
        restored.status("op-a"),
        Some(QueueStatus::Terminal(DurableOutcome::Success))
    );
    assert_eq!(restored.status("op-b"), Some(QueueStatus::Suspended));
    assert_eq!(restored.replay_candidates()[0].operation_id, "op-b");
}

#[test]
fn claim_next_persisted_on_empty_queue_writes_nothing() {
    let mut repo = MemoryRepo::new(header());
    let mut queue = DurableQueue::new(4);
    assert!(queue
        .claim_next_persisted(&mut repo)
        .expect("claim")
        .is_none());
    assert!(repo.records().is_empty());
}
