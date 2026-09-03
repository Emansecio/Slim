use slim_core::session::{
    DurableEntry, DurableEntryRole, DurableOutcome, DurableQueue, DurableQueueError, DurableRecord,
    DurableRepo, DurableSessionHeader, JsonlRepo, MemoryRepo, QueueItem, QueueStatus,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn header() -> DurableSessionHeader {
    DurableSessionHeader::new("queue-session", "now", "D:\\Slim", None, None)
}

#[test]
fn durable_queue_is_bounded_fifo_and_deduplicates_operation_ids() {
    let mut queue = DurableQueue::new(2);

    queue
        .enqueue(QueueItem::new("op-1", "entry-1"))
        .expect("first intent");
    queue
        .enqueue(QueueItem::new("op-2", "entry-2"))
        .expect("second intent");

    assert_eq!(
        queue
            .pending()
            .iter()
            .map(|item| item.operation_id.as_str())
            .collect::<Vec<_>>(),
        ["op-1", "op-2"]
    );
    assert!(matches!(
        queue.enqueue(QueueItem::new("op-3", "entry-3")),
        Err(DurableQueueError::Full { capacity: 2 })
    ));
    assert!(matches!(
        queue.enqueue(QueueItem::new("op-1", "other-entry")),
        Err(DurableQueueError::DuplicateOperation { ref operation_id }) if operation_id == "op-1"
    ));

    let claimed = queue.claim_next().expect("FIFO claim");
    assert_eq!(claimed.operation_id, "op-1");
    assert_eq!(queue.status("op-1"), Some(QueueStatus::Claimed));
    assert!(queue.enqueue(QueueItem::new("op-1", "entry-1")).is_err());
    assert_eq!(queue.pending()[0].operation_id, "op-2");
}

#[test]
fn finish_requires_claimed_and_rejects_queued_without_mutation() {
    let mut queue = DurableQueue::new(1);
    queue
        .enqueue(QueueItem::new("op-queued", "entry-q"))
        .expect("intent");
    let error = queue
        .finish("op-queued", DurableOutcome::Success)
        .expect_err("queued work cannot be terminal");
    assert!(matches!(error, DurableQueueError::InvalidTransition { .. }));
    assert_eq!(queue.status("op-queued"), Some(QueueStatus::Queued));
    assert_eq!(
        queue.pending(),
        vec![QueueItem::new("op-queued", "entry-q")]
    );

    let mut repo = MemoryRepo::new(header());
    let mut persisted = DurableQueue::new(1);
    persisted
        .enqueue_persisted(&mut repo, QueueItem::new("op-queued", "entry-q"))
        .expect("intent");
    let prefix = repo.records().to_vec();
    let error = persisted
        .finish_persisted(&mut repo, "op-queued", DurableOutcome::Success)
        .expect_err("queued work cannot be terminal");
    assert!(matches!(error, DurableQueueError::InvalidTransition { .. }));
    assert_eq!(repo.records(), prefix.as_slice());
    assert_eq!(persisted.status("op-queued"), Some(QueueStatus::Queued));
}

fn jsonl_path(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("slim-queue-{label}-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&directory).expect("test directory");
    directory.join("session.jsonl")
}

#[test]
fn finish_rejects_suspended_and_reopen_preserves_the_confirmed_prefix() {
    let path = jsonl_path("suspended");
    let mut repo = JsonlRepo::create(&path, header()).expect("create");
    let mut queue = DurableQueue::new(1);
    queue
        .enqueue_persisted(&mut repo, QueueItem::new("op-suspended", "entry-s"))
        .expect("intent");
    queue
        .suspend_persisted(&mut repo, "op-suspended", "explicit decision")
        .expect("suspend");
    let prefix = repo.records().to_vec();

    let error = queue
        .finish_persisted(&mut repo, "op-suspended", DurableOutcome::Success)
        .expect_err("suspended work cannot be terminal");
    assert!(matches!(error, DurableQueueError::InvalidTransition { .. }));
    assert_eq!(repo.records(), prefix.as_slice());
    assert_eq!(queue.status("op-suspended"), Some(QueueStatus::Suspended));

    drop(repo);
    let reopened = JsonlRepo::open(&path).expect("reopen");
    assert_eq!(reopened.records(), prefix.as_slice());
    let restored = DurableQueue::from_repo(1, &reopened).expect("restore queue");
    assert_eq!(
        restored.status("op-suspended"),
        Some(QueueStatus::Suspended)
    );
    assert!(restored.pending().is_empty());
    drop(reopened);
    std::fs::remove_dir_all(path.parent().expect("test directory")).expect("cleanup");
}

#[test]
fn persistence_restore_preserves_prefix_and_never_requeues_claimed_or_terminal_work() {
    let mut repo = MemoryRepo::new(header());
    let mut queue = DurableQueue::new(4);

    queue
        .enqueue_persisted(&mut repo, QueueItem::new("op-claimed", "entry-c"))
        .expect("claimed intent");
    queue
        .enqueue_persisted(&mut repo, QueueItem::new("op-queued", "entry-q"))
        .expect("queued intent");
    queue
        .enqueue_persisted(&mut repo, QueueItem::new("op-aborted", "entry-a"))
        .expect("aborted intent");
    queue
        .claim_next_persisted(&mut repo)
        .expect("claim queued item")
        .expect("claimed item");
    queue
        .abort_persisted(&mut repo, "op-aborted")
        .expect("abort item");
    queue
        .finish_persisted(&mut repo, "op-claimed", DurableOutcome::Success)
        .expect("terminal item");

    // A confirmed prefix is independent of queue reconstruction and remains byte-for-byte present.
    let prefix = repo.records().to_vec();
    let restored = DurableQueue::from_repo(4, &repo).expect("restore queue");

    assert_eq!(repo.records(), prefix.as_slice());
    assert_eq!(
        restored
            .pending()
            .iter()
            .map(|item| item.operation_id.as_str())
            .collect::<Vec<_>>(),
        ["op-queued"]
    );
    assert_eq!(
        restored.status("op-claimed"),
        Some(QueueStatus::Terminal(DurableOutcome::Success))
    );
    assert_eq!(restored.status("op-aborted"), Some(QueueStatus::Aborted));
    assert_eq!(restored.status("op-queued"), Some(QueueStatus::Queued));
}

#[test]
fn restore_reserves_entry_operation_ids_before_a_queue_intent_exists() {
    let records = vec![DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "entry-crash".into(),
            role: DurableEntryRole::User,
            content: "before queue intent".into(),
            parent_entry_id: None,
            operation_id: "op-crash".into(),
            tool_call_id: None,
        },
    }];

    let mut queue = DurableQueue::from_records(1, &records).expect("restore entry prefix");
    assert!(matches!(
        queue.enqueue(QueueItem::operation("op-crash")),
        Err(DurableQueueError::DuplicateOperation { ref operation_id })
            if operation_id == "op-crash"
    ));
    assert!(queue.pending().is_empty());
}

#[test]
fn enqueue_persisted_rejects_entry_operation_id_after_crash_before_queue_intent() {
    let mut repo = MemoryRepo::new(header());
    repo.append(DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "entry-crash-persisted".into(),
            role: DurableEntryRole::User,
            content: "before queue intent".into(),
            parent_entry_id: None,
            operation_id: "op-crash-persisted".into(),
            tool_call_id: None,
        },
    })
    .expect("persist entry prefix");
    let prefix = repo.records().to_vec();
    let mut queue = DurableQueue::new(1);

    let error = queue
        .enqueue_persisted(&mut repo, QueueItem::operation("op-crash-persisted"))
        .expect_err("entry operation id must reserve the queue identity");
    assert!(matches!(
        error,
        DurableQueueError::DuplicateOperation { ref operation_id }
            if operation_id == "op-crash-persisted"
    ));
    assert_eq!(repo.records(), prefix.as_slice());
    assert!(queue.pending().is_empty());
}

#[test]
fn enqueue_persisted_rehydrates_repo_occupancy_before_capacity_check() {
    let mut repo = MemoryRepo::new(header());
    let mut seeded = DurableQueue::new(2);
    seeded
        .enqueue_persisted(&mut repo, QueueItem::operation("op-existing"))
        .expect("seed persisted queue");
    let prefix = repo.records().to_vec();

    let mut fresh_queue = DurableQueue::new(1);
    let error = fresh_queue
        .enqueue_persisted(&mut repo, QueueItem::operation("op-new"))
        .expect_err("persisted occupancy must count against capacity");
    assert!(matches!(error, DurableQueueError::Full { capacity: 1 }));
    assert_eq!(repo.records(), prefix.as_slice());
    assert_eq!(
        fresh_queue.pending(),
        vec![QueueItem::operation("op-existing")]
    );
}

#[test]
fn jsonl_reopen_preserves_entry_prefix_and_rejects_queue_id_without_mutation() {
    let path = jsonl_path("entry-prefix");
    let entry = DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "entry-jsonl-crash".into(),
            role: DurableEntryRole::User,
            content: "before queue intent".into(),
            parent_entry_id: None,
            operation_id: "op-jsonl-crash".into(),
            tool_call_id: None,
        },
    };
    let mut repo = JsonlRepo::create(&path, header()).expect("create");
    repo.append(entry).expect("persist entry prefix");
    let prefix_records = repo.records().to_vec();
    let prefix_bytes = std::fs::read(&path).expect("read entry prefix");
    drop(repo);

    let mut reopened = JsonlRepo::open(&path).expect("reopen entry prefix");
    assert_eq!(reopened.records(), prefix_records.as_slice());
    let mut queue = DurableQueue::new(1);
    assert!(matches!(
        queue.enqueue_persisted(&mut reopened, QueueItem::operation("op-jsonl-crash")),
        Err(DurableQueueError::DuplicateOperation { ref operation_id })
            if operation_id == "op-jsonl-crash"
    ));
    assert_eq!(reopened.records(), prefix_records.as_slice());
    drop(reopened);
    assert_eq!(
        std::fs::read(&path).expect("read unchanged prefix"),
        prefix_bytes
    );

    let reopened_again = JsonlRepo::open(&path).expect("reopen unchanged prefix");
    assert_eq!(reopened_again.records(), prefix_records.as_slice());
    drop(reopened_again);
    std::fs::remove_dir_all(path.parent().expect("test directory")).expect("cleanup");
}

#[test]
fn suspended_work_is_visible_but_requires_an_explicit_replay_decision() {
    let mut repo = MemoryRepo::new(header());
    let mut queue = DurableQueue::new(2);
    queue
        .enqueue_persisted(&mut repo, QueueItem::new("op-suspended", "entry-s"))
        .expect("intent");
    queue
        .suspend_persisted(&mut repo, "op-suspended", "awaiting explicit decision")
        .expect("suspend");

    let restored = DurableQueue::from_repo(2, &repo).expect("restore");
    assert!(restored.pending().is_empty());
    assert_eq!(
        restored.status("op-suspended"),
        Some(QueueStatus::Suspended)
    );
    assert_eq!(
        restored.replay_candidates(),
        vec![QueueItem::new("op-suspended", "entry-s")]
    );
}
