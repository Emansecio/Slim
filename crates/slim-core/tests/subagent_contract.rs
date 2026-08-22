use slim_core::agents::{ChildStatus, MutationLease, Scheduler, SpawnRequest, SpawnResult};

#[test]
fn depth_one_scheduler_has_bounded_active_and_fifo_queue() {
    let mut scheduler = Scheduler::new(1, 2);
    assert!(matches!(
        scheduler.spawn(SpawnRequest::new("a", 1, true)),
        SpawnResult::Started
    ));
    assert!(matches!(
        scheduler.spawn(SpawnRequest::new("b", 1, true)),
        SpawnResult::Queued
    ));
    assert!(matches!(
        scheduler.spawn(SpawnRequest::new("c", 1, false)),
        SpawnResult::Queued
    ));
    assert!(matches!(
        scheduler.spawn(SpawnRequest::new("d", 1, true)),
        SpawnResult::QueueFull
    ));
    assert!(matches!(
        scheduler.spawn(SpawnRequest::new("nested", 2, true)),
        SpawnResult::DepthExceeded
    ));

    scheduler.finish("a", "done");
    assert_eq!(scheduler.status("b"), Some(ChildStatus::Active));
    assert_eq!(scheduler.list().len(), 3);
    scheduler.cancel("b");
    assert_eq!(scheduler.status("b"), Some(ChildStatus::Cancelled));
}

#[test]
fn mutation_lease_is_serial() {
    let mut lease = MutationLease::default();
    assert!(lease.acquire("a"));
    assert!(!lease.acquire("b"));
    lease.release("a");
    assert!(lease.acquire("b"));
}
