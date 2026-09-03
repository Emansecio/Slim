use slim_core::session::{
    drive_manual, planned_provider_effects, restore_manual_run, AttemptErrorClass, AttemptLedger,
    DurableEntryRole, DurableOperationKind, DurableRecord, DurableRepo, DurableSessionHeader,
    DurableUsage, Effect, JsonlRepo, ManualDriveError, ManualExecutor, ManualRunSpec, MemoryRepo,
    ProviderResponse,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
struct FixtureExecutor {
    calls: usize,
    effects: Vec<Effect>,
}

impl ManualExecutor for FixtureExecutor {
    type Error = &'static str;

    fn execute(&mut self, effect: &Effect) -> Result<ProviderResponse, Self::Error> {
        self.calls += 1;
        self.effects.push(effect.clone());
        Ok(ProviderResponse {
            content: "deterministic answer".into(),
            usage: None,
            outcome: slim_core::session::DurableOutcome::Success,
        })
    }
}

struct TransportFailExecutor {
    calls: usize,
}

struct DefaultClassifiedFailExecutor;

struct FailedResponseExecutor;

impl ManualExecutor for FailedResponseExecutor {
    type Error = &'static str;

    fn execute(&mut self, _effect: &Effect) -> Result<ProviderResponse, Self::Error> {
        Ok(ProviderResponse::with_outcome(
            "truncated output",
            None,
            slim_core::session::DurableOutcome::Failed,
        ))
    }
}

impl ManualExecutor for DefaultClassifiedFailExecutor {
    type Error = &'static str;

    fn execute(&mut self, _effect: &Effect) -> Result<ProviderResponse, Self::Error> {
        Err("unclassified fixture")
    }
}

impl ManualExecutor for TransportFailExecutor {
    type Error = &'static str;

    fn execute(&mut self, _effect: &Effect) -> Result<ProviderResponse, Self::Error> {
        self.calls += 1;
        Err("transport fixture")
    }

    fn classify_error(&self, _error: &Self::Error) -> AttemptErrorClass {
        AttemptErrorClass::Transport {
            safe_to_retry: true,
        }
    }
}

fn header(id: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "2026-08-22T00:00:00Z", "D:\\Slim", None, None)
}

fn spec() -> ManualRunSpec {
    ManualRunSpec::new(
        "op-1",
        "attempt-1",
        "entry-user",
        "entry-assistant",
        "hello",
        1,
    )
}

fn expected_effect() -> Effect {
    Effect::ProviderRequest {
        id: slim_core::session::EffectId::new("op-1", "attempt-1"),
        input_entry_id: "entry-user".into(),
    }
}

#[test]
fn memory_run_persists_before_and_after_one_provider_effect() {
    let mut repo = MemoryRepo::new(header("memory"));
    let mut executor = FixtureExecutor::default();

    drive_manual(&mut repo, &mut executor, spec()).expect("durable run");

    assert_eq!(executor.calls, 1);
    assert_eq!(executor.effects, vec![expected_effect()]);
    let before_restore = repo.records().to_vec();
    let restored = restore_manual_run(&repo).expect("restore completed memory run");
    assert_eq!(repo.records(), before_restore.as_slice());
    assert!(planned_provider_effects(repo.records())
        .expect("completed run has no pending effect")
        .is_empty());
    assert_eq!(restored.last_seq(), Some(6));
    assert_eq!(repo.records().len(), 6);
    assert!(matches!(
        &repo.records()[0],
        DurableRecord::Entry { seq: 1, entry }
            if entry.entry_id == "entry-user" && entry.role == DurableEntryRole::User
    ));
    assert!(matches!(
        &repo.records()[1],
        DurableRecord::Operation { seq: 2, operation }
            if operation.operation_id == "op-1"
                && matches!(operation.kind, DurableOperationKind::Started { ref input_entry_id } if input_entry_id == "entry-user")
    ));
    assert!(matches!(
        &repo.records()[2],
        DurableRecord::Operation { seq: 3, operation }
            if operation.operation_id == "op-1"
                && matches!(operation.kind, DurableOperationKind::ProviderAttemptStarted { ref attempt_id, .. } if attempt_id == "attempt-1")
    ));
    assert!(matches!(
        &repo.records()[3],
        DurableRecord::Entry { seq: 4, entry }
            if entry.entry_id == "entry-assistant"
                && entry.parent_entry_id.as_deref() == Some("entry-user")
                && entry.content == "deterministic answer"
    ));
    assert!(matches!(
        &repo.records()[4],
        DurableRecord::Operation { seq: 5, operation }
            if matches!(operation.kind, DurableOperationKind::ProviderAttemptFinished { ref attempt_id, .. } if attempt_id == "attempt-1")
    ));
    assert!(matches!(
        &repo.records()[5],
        DurableRecord::Operation { seq: 6, operation }
            if matches!(operation.kind, DurableOperationKind::Finished { .. })
    ));
}

struct BatchCountingRepo {
    inner: MemoryRepo,
    batch_sizes: Vec<usize>,
}

impl BatchCountingRepo {
    fn new() -> Self {
        Self {
            inner: MemoryRepo::new(header("batch-counting")),
            batch_sizes: Vec::new(),
        }
    }
}

impl DurableRepo for BatchCountingRepo {
    fn header(&self) -> &DurableSessionHeader {
        self.inner.header()
    }

    fn records(&self) -> &[DurableRecord] {
        self.inner.records()
    }

    fn append(&mut self, record: DurableRecord) -> std::io::Result<()> {
        self.inner.append(record)
    }

    fn append_batch(&mut self, records: Vec<DurableRecord>) -> std::io::Result<()> {
        self.batch_sizes.push(records.len());
        self.inner.append_batch(records)
    }
}

#[test]
fn manual_drive_persists_one_batch_on_each_side_of_the_provider_effect() {
    let mut repo = BatchCountingRepo::new();
    let mut executor = FixtureExecutor::default();

    drive_manual(&mut repo, &mut executor, spec()).expect("durable run");

    assert_eq!(repo.batch_sizes, vec![3, 3]);
    assert_eq!(repo.records().len(), 6);
    assert_eq!(executor.calls, 1);
}

#[test]
fn missing_parent_entry_is_rejected_before_persistence_or_execution() {
    let mut repo = MemoryRepo::new(header("missing-parent"));
    let mut executor = FixtureExecutor::default();

    let error = drive_manual(
        &mut repo,
        &mut executor,
        spec().with_parent_entry_id("missing-entry"),
    )
    .expect_err("missing parent must fail closed");

    assert!(matches!(
        error,
        ManualDriveError::InvalidInput("parent_entry_exists")
    ));
    assert!(repo.records().is_empty());
    assert_eq!(executor.calls, 0);
}

#[test]
fn executor_failure_is_classified_and_persisted_before_error_returns() {
    let mut repo = MemoryRepo::new(header("transport-failure"));
    let mut executor = TransportFailExecutor { calls: 0 };

    let error = drive_manual(&mut repo, &mut executor, spec()).expect_err("executor failure");

    assert!(matches!(
        error,
        ManualDriveError::Execute("transport fixture")
    ));
    assert_eq!(executor.calls, 1);
    assert!(matches!(
        &repo.records()[3],
        DurableRecord::Operation { seq: 4, operation }
            if matches!(
                operation.kind,
                DurableOperationKind::ProviderAttemptFailed {
                    ref attempt_id,
                    error: AttemptErrorClass::Transport { safe_to_retry: true }
                } if attempt_id == "attempt-1"
            )
    ));
    AttemptLedger::from_records(repo.records()).expect("failed attempt remains causal");
    assert!(planned_provider_effects(repo.records())
        .expect("failed attempt has no pending effect")
        .is_empty());
}

#[test]
fn executor_failure_without_classifier_persists_unknown_fail_closed() {
    let mut repo = MemoryRepo::new(header("unknown-failure"));
    let mut executor = DefaultClassifiedFailExecutor;

    let error = drive_manual(&mut repo, &mut executor, spec()).expect_err("executor failure");

    assert!(matches!(
        error,
        ManualDriveError::Execute("unclassified fixture")
    ));
    assert!(matches!(
        &repo.records()[3],
        DurableRecord::Operation { operation, .. }
            if matches!(
                operation.kind,
                DurableOperationKind::ProviderAttemptFailed {
                    error: AttemptErrorClass::Unknown,
                    ..
                }
            )
    ));
}

#[test]
fn non_success_provider_response_never_persists_success_terminal() {
    let mut repo = MemoryRepo::new(header("failed-response"));
    let mut executor = FailedResponseExecutor;

    drive_manual(&mut repo, &mut executor, spec()).expect("durable failed response");

    assert!(matches!(
        &repo.records()[4],
        DurableRecord::Operation { operation, .. }
            if matches!(
                operation.kind,
                DurableOperationKind::ProviderAttemptFinished {
                    outcome: slim_core::session::DurableOutcome::Failed,
                    ..
                }
            )
    ));
    assert!(matches!(
        &repo.records()[5],
        DurableRecord::Operation { operation, .. }
            if matches!(
                operation.kind,
                DurableOperationKind::Finished {
                    outcome: slim_core::session::DurableOutcome::Failed
                }
            )
    ));
}

#[derive(Default)]
struct UsageExecutor;

impl ManualExecutor for UsageExecutor {
    type Error = &'static str;

    fn execute(&mut self, _effect: &Effect) -> Result<ProviderResponse, Self::Error> {
        Ok(ProviderResponse::new(
            "answer with unknown usage",
            Some(DurableUsage {
                operation_id: "executor-value-is-rebound".into(),
                attempt_id: "executor-value-is-rebound".into(),
                input_tokens: None,
                output_tokens: None,
            }),
        ))
    }
}

#[test]
fn unknown_usage_is_optional_and_keeps_operation_attempt_correlation() {
    let mut repo = MemoryRepo::new(header("unknown-usage"));
    let mut executor = UsageExecutor;

    drive_manual(&mut repo, &mut executor, spec()).expect("durable run");

    assert_eq!(repo.records().len(), 7);
    assert!(matches!(
        &repo.records()[4],
        DurableRecord::Usage { seq: 5, usage }
            if usage.operation_id == "op-1"
                && usage.attempt_id == "attempt-1"
                && usage.input_tokens.is_none()
                && usage.output_tokens.is_none()
    ));
    assert!(matches!(
        &repo.records()[5],
        DurableRecord::Operation { seq: 6, operation }
            if matches!(operation.kind, DurableOperationKind::ProviderAttemptFinished { ref attempt_id, .. } if attempt_id == "attempt-1")
    ));
}

static PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

fn fixture_path(label: &str) -> PathBuf {
    let id = PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "slim-session-durable-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).expect("create fixture directory");
    directory.join("session.jsonl")
}

#[test]
fn jsonl_reopen_restore_is_equal_and_does_not_change_bytes() {
    let path = fixture_path("reopen");
    let mut repo = JsonlRepo::create(&path, header("jsonl")).expect("create repo");
    let mut executor = FixtureExecutor::default();
    drive_manual(&mut repo, &mut executor, spec()).expect("durable run");
    let expected_state = restore_manual_run(&repo).expect("restore before reopen");
    assert!(planned_provider_effects(repo.records())
        .expect("completed run has no pending effect")
        .is_empty());
    let before = fs::read(&path).expect("read before restore");
    let restored_state = restore_manual_run(&repo).expect("restore");
    let after = fs::read(&path).expect("read after restore");
    assert_eq!(expected_state, restored_state);
    assert_eq!(before, after);
    drop(repo);

    let reopened = JsonlRepo::open(&path).expect("reopen repo");
    assert_eq!(reopened.records().len(), 6);
    assert_eq!(
        restore_manual_run(&reopened).expect("restore reopened"),
        expected_state
    );
    assert!(planned_provider_effects(reopened.records())
        .expect("completed reopened run has no pending effect")
        .is_empty());
    drop(reopened);
    fs::remove_dir_all(path.parent().expect("fixture parent")).expect("cleanup");
}

#[test]
fn jsonl_preflight_rejects_repeated_spec_without_bytes_or_second_execution() {
    let path = fixture_path("repeat");
    let mut repo = JsonlRepo::create(&path, header("jsonl-repeat")).expect("create repo");
    let mut executor = FixtureExecutor::default();
    drive_manual(&mut repo, &mut executor, spec()).expect("first durable run");
    let before_bytes = fs::read(&path).expect("read before duplicate");
    let before_records = repo.records().to_vec();
    let before_calls = executor.calls;

    let error = drive_manual(&mut repo, &mut executor, spec()).expect_err("duplicate run");

    assert!(matches!(error, ManualDriveError::Conflict { .. }));
    assert_eq!(fs::read(&path).expect("read after duplicate"), before_bytes);
    assert_eq!(repo.records(), before_records.as_slice());
    assert_eq!(executor.calls, before_calls);
    drop(repo);
    fs::remove_dir_all(path.parent().expect("fixture parent")).expect("cleanup");
}

#[test]
fn jsonl_preflight_rejects_equal_entry_ids_without_bytes_or_execution() {
    let path = fixture_path("equal-entry");
    let mut repo = JsonlRepo::create(&path, header("jsonl-equal-entry")).expect("create repo");
    let before_bytes = fs::read(&path).expect("read before invalid run");
    let before_records = repo.records().to_vec();
    let mut executor = FixtureExecutor::default();
    let equal_entries = ManualRunSpec::new(
        "op-equal-jsonl",
        "attempt-equal-jsonl",
        "same-entry-jsonl",
        "same-entry-jsonl",
        "hello",
        1,
    );

    let error = drive_manual(&mut repo, &mut executor, equal_entries)
        .expect_err("equal entry ids must be invalid");

    assert!(matches!(error, ManualDriveError::InvalidInput(_)));
    assert_eq!(
        fs::read(&path).expect("read after invalid run"),
        before_bytes
    );
    assert_eq!(repo.records(), before_records.as_slice());
    assert_eq!(executor.calls, 0);
    drop(repo);
    fs::remove_dir_all(path.parent().expect("fixture parent")).expect("cleanup");
}

struct FailingRepo {
    inner: MemoryRepo,
    fail_on_append: usize,
    appends: usize,
}

impl FailingRepo {
    fn new(fail_on_append: usize) -> Self {
        Self {
            inner: MemoryRepo::new(header("failing")),
            fail_on_append,
            appends: 0,
        }
    }
}

impl DurableRepo for FailingRepo {
    fn header(&self) -> &DurableSessionHeader {
        self.inner.header()
    }

    fn records(&self) -> &[DurableRecord] {
        self.inner.records()
    }

    fn append(&mut self, record: DurableRecord) -> std::io::Result<()> {
        self.appends += 1;
        if self.appends == self.fail_on_append {
            return Err(std::io::Error::other("fixture persistence failure"));
        }
        self.inner.append(record)
    }
}

#[test]
fn persistence_failure_before_effect_does_not_call_executor() {
    let mut repo = FailingRepo::new(3);
    let mut executor = FixtureExecutor::default();

    let error = drive_manual(&mut repo, &mut executor, spec()).expect_err("persist must fail");

    assert!(matches!(error, ManualDriveError::Persist(_)));
    assert_eq!(executor.calls, 0);
    assert_eq!(repo.records().len(), 2);
}

#[test]
fn preflight_rejects_repeated_spec_without_mutating_memory_or_executing_again() {
    let mut repo = MemoryRepo::new(header("memory-repeat"));
    let mut executor = FixtureExecutor::default();
    drive_manual(&mut repo, &mut executor, spec()).expect("first durable run");
    let before_records = repo.records().to_vec();
    let before_calls = executor.calls;

    let error = drive_manual(&mut repo, &mut executor, spec()).expect_err("duplicate run");

    assert!(matches!(error, ManualDriveError::Conflict { .. }));
    assert_eq!(repo.records(), before_records.as_slice());
    assert_eq!(executor.calls, before_calls);
}

#[test]
fn preflight_rejects_reused_attempt_with_new_operation_without_mutation() {
    let mut repo = MemoryRepo::new(header("memory-attempt-repeat"));
    let mut executor = FixtureExecutor::default();
    drive_manual(&mut repo, &mut executor, spec()).expect("first durable run");
    let before_records = repo.records().to_vec();
    let before_calls = executor.calls;
    let repeated_attempt = ManualRunSpec::new(
        "op-2",
        "attempt-1",
        "entry-user-2",
        "entry-assistant-2",
        "second",
        7,
    );

    let error =
        drive_manual(&mut repo, &mut executor, repeated_attempt).expect_err("attempt id collision");

    assert!(matches!(error, ManualDriveError::Conflict { .. }));
    assert_eq!(repo.records(), before_records.as_slice());
    assert_eq!(executor.calls, before_calls);
}

#[test]
fn preflight_rejects_equal_entry_ids_without_mutating_memory_or_executing() {
    let mut repo = MemoryRepo::new(header("memory-equal-entry"));
    let mut executor = FixtureExecutor::default();
    let equal_entries = ManualRunSpec::new(
        "op-equal",
        "attempt-equal",
        "same-entry",
        "same-entry",
        "hello",
        1,
    );

    let error = drive_manual(&mut repo, &mut executor, equal_entries)
        .expect_err("equal entry ids must be invalid");

    assert!(matches!(error, ManualDriveError::InvalidInput(_)));
    assert!(repo.records().is_empty());
    assert_eq!(executor.calls, 0);
}

#[test]
fn preflight_rejects_sequence_window_overflow_without_mutation_or_execution() {
    let mut repo = MemoryRepo::new(header("memory-overflow"));
    let mut executor = FixtureExecutor::default();
    let near_end = ManualRunSpec::new(
        "op-overflow",
        "attempt-overflow",
        "entry-overflow",
        "assistant-overflow",
        "hello",
        u64::MAX - 5,
    );

    let error = drive_manual(&mut repo, &mut executor, near_end)
        .expect_err("worst-case sequence window must fit");

    assert!(matches!(error, ManualDriveError::InvalidInput(_)));
    assert!(repo.records().is_empty());
    assert_eq!(executor.calls, 0);
}

#[test]
fn sequence_window_reserves_through_max_for_a_known_usage_record() {
    let mut repo = MemoryRepo::new(header("memory-max-seq"));
    let mut executor = UsageExecutor;
    let max_window = ManualRunSpec::new(
        "op-max-seq",
        "attempt-max-seq",
        "entry-max-seq",
        "assistant-max-seq",
        "hello",
        u64::MAX - 6,
    );

    drive_manual(&mut repo, &mut executor, max_window).expect("max sequence window");

    assert_eq!(
        repo.records().last().expect("terminal record").seq(),
        u64::MAX
    );
}

#[test]
fn preflight_rejects_first_sequence_not_after_existing_prefix() {
    let mut repo = MemoryRepo::new(header("memory-sequence"));
    repo.append(DurableRecord::Fact {
        seq: 4,
        fact: slim_core::session::DurableFact {
            namespace: "fixture".into(),
            key: "existing".into(),
            value: serde_json::json!(true),
        },
    })
    .expect("existing prefix");
    let before_records = repo.records().to_vec();
    let mut executor = FixtureExecutor::default();
    let invalid_sequence = ManualRunSpec::new(
        "op-sequence",
        "attempt-sequence",
        "entry-sequence",
        "assistant-sequence",
        "hello",
        4,
    );

    let error = drive_manual(&mut repo, &mut executor, invalid_sequence)
        .expect_err("first sequence must follow prefix");

    assert!(matches!(error, ManualDriveError::InvalidInput(_)));
    assert_eq!(repo.records(), before_records.as_slice());
    assert_eq!(executor.calls, 0);
}

#[test]
fn post_effect_persistence_failure_leaves_restorable_inflight_prefix_without_reexecution() {
    let mut repo = FailingRepo::new(4);
    let mut executor = FixtureExecutor::default();

    let error = drive_manual(&mut repo, &mut executor, spec())
        .expect_err("assistant persistence must fail after effect");
    assert!(matches!(error, ManualDriveError::Persist(_)));
    assert_eq!(executor.calls, 1);
    assert_eq!(repo.records().len(), 3);
    let before_restore = repo.records().to_vec();

    let restored = restore_manual_run(&repo).expect("restore inflight prefix");

    assert_eq!(restored.last_seq(), Some(3));
    assert_eq!(
        planned_provider_effects(repo.records()).expect("planned effect"),
        vec![expected_effect()]
    );
    assert_eq!(repo.records(), before_restore.as_slice());
    assert_eq!(executor.calls, 1);
}

fn append_inflight_prefix(repo: &mut impl DurableRepo) {
    repo.append(DurableRecord::Entry {
        seq: 1,
        entry: slim_core::session::DurableEntry {
            entry_id: "entry-user".into(),
            role: DurableEntryRole::User,
            content: "hello".into(),
            parent_entry_id: None,
            operation_id: "op-1".into(),
            tool_call_id: None,
        },
    })
    .expect("input entry");
    repo.append(DurableRecord::Operation {
        seq: 2,
        operation: slim_core::session::DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "entry-user".into(),
            },
        },
    })
    .expect("operation start");
    repo.append(DurableRecord::Operation {
        seq: 3,
        operation: slim_core::session::DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-1".into(),
                ordinal: 1,
            },
        },
    })
    .expect("attempt start");
}

#[test]
fn restoring_inflight_memory_prefix_never_calls_an_executor() {
    let mut repo = MemoryRepo::new(header("memory-inflight"));
    append_inflight_prefix(&mut repo);
    let before_records = repo.records().to_vec();

    let state = restore_manual_run(&repo).expect("restore inflight prefix");

    assert_eq!(state.last_seq(), Some(3));
    assert_eq!(state.operation_history("op-1").expect("operation").len(), 2);
    assert_eq!(
        planned_provider_effects(repo.records()).expect("planned inflight effect"),
        vec![expected_effect()]
    );
    assert_eq!(repo.records(), before_records.as_slice());
}

#[test]
fn restoring_inflight_jsonl_prefix_never_calls_executor_or_changes_bytes() {
    let path = fixture_path("inflight");
    let mut repo = JsonlRepo::create(&path, header("jsonl-inflight")).expect("create repo");
    append_inflight_prefix(&mut repo);
    let before = fs::read(&path).expect("read before restore");
    let before_records = repo.records().to_vec();

    let state = restore_manual_run(&repo).expect("restore inflight prefix");

    let after = fs::read(&path).expect("read after restore");
    assert_eq!(state.last_seq(), Some(3));
    assert_eq!(
        planned_provider_effects(repo.records()).expect("planned inflight effect"),
        vec![expected_effect()]
    );
    assert_eq!(repo.records(), before_records.as_slice());
    assert_eq!(before, after);
    drop(repo);
    fs::remove_dir_all(path.parent().expect("fixture parent")).expect("cleanup");
}

#[test]
fn planned_effects_fail_closed_when_attempt_correlation_is_ambiguous() {
    let mut repo = MemoryRepo::new(header("ambiguous-effect"));
    append_inflight_prefix(&mut repo);
    let mut records = repo.records().to_vec();
    records.push(DurableRecord::Operation {
        seq: 4,
        operation: slim_core::session::DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "entry-user".into(),
            },
        },
    });

    assert!(planned_provider_effects(&records).is_err());
}

#[test]
fn planned_effects_fail_closed_when_attempt_finishes_under_another_operation() {
    let mut repo = MemoryRepo::new(header("mismatched-effect"));
    append_inflight_prefix(&mut repo);
    let mut records = repo.records().to_vec();
    records.push(DurableRecord::Operation {
        seq: 4,
        operation: slim_core::session::DurableOperation {
            operation_id: "op-other".into(),
            kind: DurableOperationKind::ProviderAttemptFinished {
                attempt_id: "attempt-1".into(),
                outcome: slim_core::session::DurableOutcome::Success,
            },
        },
    });

    assert!(planned_provider_effects(&records).is_err());
}
