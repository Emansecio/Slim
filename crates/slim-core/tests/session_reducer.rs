use serde_json::json;
use slim_core::session::{
    reduce, restore_records, CompactionCheckpoint, CompactionReason, DurableEntry,
    DurableEntryRole, DurableFact, DurableOperation, DurableOperationKind, DurableRecord,
    DurableRepo, DurableState, DurableUsage, JsonlRepo, ReplayPolicy,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn records() -> Vec<DurableRecord> {
    vec![
        DurableRecord::Entry {
            seq: 10,
            entry: DurableEntry {
                entry_id: "entry-root".into(),
                role: DurableEntryRole::User,
                content: "hello".into(),
                parent_entry_id: None,
                operation_id: "op-1".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        },
        DurableRecord::Entry {
            seq: 20,
            entry: DurableEntry {
                entry_id: "entry-child".into(),
                role: DurableEntryRole::Assistant,
                content: "answer".into(),
                parent_entry_id: Some("entry-root".into()),
                operation_id: "op-1".into(),
                tool_call_id: Some("call-1".into()),
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        },
        DurableRecord::Operation {
            seq: 25,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: "entry-root".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 30,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Suspended {
                    reason: "tool fixture".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 40,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptStarted {
                    attempt_id: "attempt-1".into(),
                    ordinal: 1,
                },
            },
        },
        DurableRecord::Fact {
            seq: 45,
            fact: DurableFact {
                namespace: "session".into(),
                key: "mode".into(),
                value: json!("auto"),
            },
        },
        DurableRecord::Usage {
            seq: 50,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: None,
                output_tokens: Some(7),
            },
        },
    ]
}

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "slim-session-reducer-{label}-{}-{nanos}.jsonl",
        std::process::id()
    ));
    std::fs::create_dir(&directory).expect("create fixture directory");
    directory.join("session.jsonl")
}

#[test]
fn restores_four_real_record_kinds_into_deterministic_state() {
    let state: DurableState = restore_records(&records()).expect("restore");

    assert_eq!(state.last_seq(), Some(50));
    assert_eq!(state.entries()["entry-root"].parent_entry_id, None);
    assert_eq!(
        state.entries()["entry-child"].parent_entry_id.as_deref(),
        Some("entry-root")
    );
    assert_eq!(state.operations()["op-1"].len(), 3);
    assert_eq!(
        state.facts()[&(String::from("session"), String::from("mode"))].value,
        json!("auto")
    );
    assert_eq!(
        state.usage()[&(String::from("op-1"), String::from("attempt-1"))].input_tokens,
        None
    );
    assert_eq!(
        state.usage()[&(String::from("op-1"), String::from("attempt-1"))].output_tokens,
        Some(7)
    );
}

#[test]
fn repeated_restore_is_structurally_identical() {
    let records = records();

    assert_eq!(restore_records(&records), restore_records(&records));
}

#[test]
fn facts_last_write_by_namespace_and_key_and_operations_keep_history() {
    let mut records = records();
    records.push(DurableRecord::Operation {
        seq: 55,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptFinished {
                attempt_id: "attempt-1".into(),
                outcome: slim_core::session::DurableOutcome::Success,
            },
        },
    });
    records.push(DurableRecord::Operation {
        seq: 60,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Finished {
                outcome: slim_core::session::DurableOutcome::Success,
            },
        },
    });
    records.push(DurableRecord::Fact {
        seq: 70,
        fact: DurableFact {
            namespace: "session".into(),
            key: "mode".into(),
            value: json!("plan"),
        },
    });

    let state = restore_records(&records).expect("restore");
    assert_eq!(state.operations()["op-1"].len(), 5);
    assert_eq!(state.fact_value("session", "mode"), Some(&json!("plan")));
}

#[test]
fn unknown_usage_correlation_and_optional_token_counts_are_preserved() {
    let records = [DurableRecord::Usage {
        seq: 9,
        usage: DurableUsage {
            operation_id: "operation-not-restored".into(),
            attempt_id: "attempt-not-restored".into(),
            input_tokens: None,
            output_tokens: None,
        },
    }];

    let state = restore_records(&records).expect("restore");
    let usage = &state.usage()[&(
        String::from("operation-not-restored"),
        String::from("attempt-not-restored"),
    )];
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.output_tokens, None);
}

#[test]
fn restore_retains_latest_valid_compaction_checkpoint() {
    let checkpoint = CompactionCheckpoint {
        checkpoint_id: "compact-1".into(),
        summary: "## Goal\nContinue".into(),
        first_kept_entry_id: "entry-root".into(),
        prefix_fingerprint: "0123456789abcdef".into(),
        previous_checkpoint_id: None,
        tokens_before: 50_000,
        tokens_after: 20_000,
        input_tokens: Some(500),
        output_tokens: Some(100),
        duration_ms: 10,
        reason: CompactionReason::Manual,
        read_files: vec![],
        modified_files: vec![],
    };
    let state = restore_records(&[DurableRecord::Compaction {
        seq: 1,
        checkpoint: checkpoint.clone(),
    }])
    .expect("restore checkpoint");
    assert_eq!(state.compaction_checkpoint(), Some(&checkpoint));
}

#[test]
fn compaction_checkpoint_bounds_fail_before_state_mutation() {
    let mut state = DurableState::default();
    let oversized = CompactionCheckpoint {
        checkpoint_id: "compact-oversized".into(),
        summary: "x".repeat(64 * 1024 + 1),
        first_kept_entry_id: "entry-root".into(),
        prefix_fingerprint: "0123456789abcdef".into(),
        previous_checkpoint_id: None,
        tokens_before: 1,
        tokens_after: 1,
        input_tokens: None,
        output_tokens: None,
        duration_ms: 0,
        reason: CompactionReason::HardThreshold,
        read_files: vec![],
        modified_files: vec![],
    };
    let before = state.clone();
    assert!(reduce(
        &mut state,
        DurableRecord::Compaction {
            seq: 1,
            checkpoint: oversized,
        }
    )
    .is_err());
    assert_eq!(state, before);
}

#[test]
fn invalid_sequence_is_rejected_without_mutating_state() {
    let mut state = DurableState::default();
    let first = records()[0].clone();
    reduce(&mut state, first.clone()).expect("first record");
    let before = state.clone();
    let error = reduce(&mut state, &first).expect_err("duplicate seq");

    assert!(matches!(
        error,
        slim_core::session::ReduceError::SequenceNotIncreasing {
            previous: 10,
            next: 10
        }
    ));
    assert_eq!(state, before);
}

#[test]
fn restore_does_not_change_jsonl_bytes() {
    let path = temp_path("bytes");
    let header = slim_core::session::DurableSessionHeader::new(
        "reducer-bytes",
        "now",
        "D:\\Slim",
        None,
        None,
    );
    let mut repo = JsonlRepo::create(&path, header).expect("create repo");
    for record in records() {
        repo.append(record).expect("append record");
    }
    let before = std::fs::read(repo.path()).expect("read before restore");
    restore_records(repo.records()).expect("restore");
    let after = std::fs::read(repo.path()).expect("read after restore");
    assert_eq!(before, after);
    drop(repo);
    std::fs::remove_dir_all(path.parent().expect("fixture directory")).expect("remove fixture");
}

#[test]
fn duplicate_entry_id_is_rejected_without_mutating_state() {
    let mut state = DurableState::default();
    let first = records()[0].clone();
    reduce(&mut state, first).expect("first entry");
    let before = state.clone();
    let duplicate = DurableRecord::Entry {
        seq: 60,
        entry: DurableEntry {
            entry_id: "entry-root".into(),
            role: DurableEntryRole::Assistant,
            content: "duplicate".into(),
            parent_entry_id: None,
            operation_id: "op-1".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    };

    let error = reduce(&mut state, duplicate);
    assert!(matches!(
        error,
        Err(slim_core::session::ReduceError::DuplicateEntryId { ref entry_id })
            if entry_id == "entry-root"
    ));
    assert_eq!(state, before);
}

#[test]
fn usage_partial_updates_do_not_erase_known_tokens() {
    let mut state = DurableState::default();
    reduce(
        &mut state,
        DurableRecord::Usage {
            seq: 1,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: Some(11),
                output_tokens: None,
            },
        },
    )
    .expect("input usage");
    reduce(
        &mut state,
        DurableRecord::Usage {
            seq: 2,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: None,
                output_tokens: Some(7),
            },
        },
    )
    .expect("output usage");

    let usage = &state.usage()[&("op-1".into(), "attempt-1".into())];
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(7));
}

#[test]
fn empty_required_ids_are_rejected_without_mutation() {
    assert_empty_required_id(
        DurableRecord::Entry {
            seq: 1,
            entry: DurableEntry {
                entry_id: String::new(),
                role: DurableEntryRole::User,
                content: "input".into(),
                parent_entry_id: None,
                operation_id: "op-1".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        },
        "entry.entry_id",
    );
    assert_empty_required_id(
        DurableRecord::Operation {
            seq: 1,
            operation: DurableOperation {
                operation_id: String::new(),
                kind: DurableOperationKind::Aborted,
            },
        },
        "operation.operation_id",
    );
    assert_empty_required_id(
        DurableRecord::Usage {
            seq: 1,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: String::new(),
                input_tokens: None,
                output_tokens: None,
            },
        },
        "usage.attempt_id",
    );
    assert_empty_required_id(
        DurableRecord::Operation {
            seq: 1,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolIntent {
                    tool_call_id: String::new(),
                    tool_name: "read".into(),
                    replay_policy: ReplayPolicy::Safe,
                },
            },
        },
        "operation.tool_call_id",
    );
    assert_empty_required_id(
        DurableRecord::Entry {
            seq: 1,
            entry: DurableEntry {
                entry_id: "entry-child".into(),
                role: DurableEntryRole::Assistant,
                content: "child".into(),
                parent_entry_id: Some(String::new()),
                operation_id: "op-1".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        },
        "entry.parent_entry_id",
    );
    assert_empty_required_id(
        DurableRecord::Operation {
            seq: 1,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: String::new(),
                },
            },
        },
        "operation.input_entry_id",
    );
    assert_empty_required_id(
        DurableRecord::Operation {
            seq: 1,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptStarted {
                    attempt_id: String::new(),
                    ordinal: 1,
                },
            },
        },
        "operation.attempt_id",
    );
    assert_empty_required_id(
        DurableRecord::Usage {
            seq: 1,
            usage: DurableUsage {
                operation_id: String::new(),
                attempt_id: "attempt-1".into(),
                input_tokens: None,
                output_tokens: None,
            },
        },
        "usage.operation_id",
    );
}

fn assert_empty_required_id(record: DurableRecord, field: &'static str) {
    let mut state = DurableState::default();
    let error = reduce(&mut state, record).expect_err("empty required id");

    assert!(matches!(
        error,
        slim_core::session::ReduceError::EmptyRequiredId { field: actual } if actual == field
    ));
    assert_eq!(state, DurableState::default());
}

#[test]
fn missing_parent_is_rejected_without_mutation() {
    let mut state = DurableState::default();
    let record = DurableRecord::Entry {
        seq: 1,
        entry: DurableEntry {
            entry_id: "entry-child".into(),
            role: DurableEntryRole::Assistant,
            content: "child".into(),
            parent_entry_id: Some("entry-missing".into()),
            operation_id: "op-1".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    };
    let error = reduce(&mut state, record).expect_err("missing parent");

    assert!(matches!(
        error,
        slim_core::session::ReduceError::MissingParentEntry {
            ref entry_id,
            ref parent_entry_id
        } if entry_id == "entry-child" && parent_entry_id == "entry-missing"
    ));
    assert_eq!(state, DurableState::default());
}

#[test]
fn started_operation_requires_existing_matching_input_entry() {
    let mut state = DurableState::default();
    let missing = DurableRecord::Operation {
        seq: 1,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "entry-missing".into(),
            },
        },
    };
    let missing_error = reduce(&mut state, missing).expect_err("missing input entry");
    assert!(matches!(
        missing_error,
        slim_core::session::ReduceError::MissingInputEntry {
            ref operation_id,
            ref input_entry_id
        } if operation_id == "op-1" && input_entry_id == "entry-missing"
    ));
    assert_eq!(state, DurableState::default());

    let entry = DurableRecord::Entry {
        seq: 2,
        entry: DurableEntry {
            entry_id: "entry-1".into(),
            role: DurableEntryRole::User,
            content: "input".into(),
            parent_entry_id: None,
            operation_id: "op-entry".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    };
    reduce(&mut state, entry).expect("input entry");
    let mismatch = DurableRecord::Operation {
        seq: 3,
        operation: DurableOperation {
            operation_id: "op-other".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "entry-1".into(),
            },
        },
    };
    let before = state.clone();
    let mismatch_error = reduce(&mut state, mismatch).expect_err("operation mismatch");
    assert!(matches!(
        mismatch_error,
        slim_core::session::ReduceError::InputEntryOperationMismatch {
            ref operation_id,
            ref input_entry_id,
            ref entry_operation_id
        } if operation_id == "op-other"
            && input_entry_id == "entry-1"
            && entry_operation_id == "op-entry"
    ));
    assert_eq!(state, before);
}
