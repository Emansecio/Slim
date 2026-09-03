use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use slim_core::session::{
    AttemptErrorClass, AttemptLedger, DurableEntry, DurableEntryRole, DurableOperation,
    DurableOperationKind, DurableOutcome, DurableRecord, DurableRepo, DurableSessionHeader,
    DurableUsage, JsonlRepo, MemoryRepo, RetryPolicy,
};

static PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

fn header(id: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "2026-08-22T00:00:00Z", "D:\\Slim", None, None)
}

fn fixture_records() -> Vec<DurableRecord> {
    vec![
        DurableRecord::Entry {
            seq: 1,
            entry: DurableEntry {
                entry_id: "entry-user".into(),
                role: DurableEntryRole::User,
                content: "hello".into(),
                parent_entry_id: None,
                operation_id: "op-1".into(),
                tool_call_id: None,
            },
        },
        DurableRecord::Operation {
            seq: 2,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: "entry-user".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 3,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::RetryConfigured {
                    repeatable: true,
                    policy: RetryPolicy::SafeTransport,
                },
            },
        },
        DurableRecord::Operation {
            seq: 4,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptStarted {
                    attempt_id: "attempt-1".into(),
                    ordinal: 1,
                },
            },
        },
        DurableRecord::Usage {
            seq: 5,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: Some(3),
                output_tokens: None,
            },
        },
        DurableRecord::Usage {
            seq: 6,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: None,
                output_tokens: Some(5),
            },
        },
        DurableRecord::Operation {
            seq: 7,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptFailed {
                    attempt_id: "attempt-1".into(),
                    error: AttemptErrorClass::Transport {
                        safe_to_retry: true,
                    },
                },
            },
        },
    ]
}

fn append_all(repo: &mut impl DurableRepo, records: &[DurableRecord]) {
    for record in records {
        repo.append(record.clone()).expect("fixture record");
    }
}

fn temp_path(label: &str) -> PathBuf {
    let id = PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = fs::canonicalize(std::env::temp_dir()).expect("temp root");
    let directory = root.join(format!("slim-attempts-{label}-{}-{id}", std::process::id()));
    fs::create_dir(&directory).expect("fixture directory");
    directory.join("session.jsonl")
}

fn assert_ledger_matrix<R: DurableRepo>(mut repo: R) {
    let records = fixture_records();
    append_all(&mut repo, &records);
    let ledger = AttemptLedger::from_records(repo.records()).expect("valid attempt ledger");

    let attempts = ledger.attempts_for("op-1");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].attempt_id, "attempt-1");
    assert_eq!(attempts[0].ordinal, 1);
    assert_eq!(attempts[0].operation_id, "op-1");
    assert_eq!(
        ledger.usage("op-1", "attempt-1").unwrap().input_tokens,
        Some(3)
    );
    assert_eq!(
        ledger.usage("op-1", "attempt-1").unwrap().output_tokens,
        Some(5)
    );

    let retry = ledger
        .plan_retry("op-1", "attempt-2")
        .expect("safe transport retry");
    assert_eq!(retry.operation_id, "op-1");
    assert_eq!(retry.attempt_id, "attempt-2");
    assert_eq!(retry.ordinal, 2);
    let mut retry_records = records;
    retry_records.push(DurableRecord::Operation {
        seq: 8,
        operation: retry.operation(),
    });
    let retried = AttemptLedger::from_records(&retry_records).expect("retry prefix");
    assert_eq!(retried.attempts_for("op-1").len(), 2);
    assert!(retried
        .attempts_for("op-1")
        .iter()
        .all(|attempt| attempt.operation_id == "op-1"));
}

fn assert_rejected_in_memory_and_jsonl(records: &[DurableRecord]) {
    let mut memory = MemoryRepo::new(header("memory-invalid"));
    let memory_rejected = records
        .iter()
        .any(|record| memory.append(record.clone()).is_err());
    assert!(
        memory_rejected,
        "MemoryRepo accepted an invalid attempt lifecycle"
    );

    let path = temp_path("invalid");
    let mut jsonl = JsonlRepo::create(&path, header("jsonl-invalid")).expect("jsonl repo");
    let jsonl_rejected = records
        .iter()
        .any(|record| jsonl.append(record.clone()).is_err());
    assert!(
        jsonl_rejected,
        "JsonlRepo accepted an invalid attempt lifecycle"
    );
    drop(jsonl);
    fs::remove_dir_all(path.parent().unwrap()).expect("cleanup");
}

#[test]
fn memory_and_jsonl_share_attempt_correlation_usage_and_retry_contract() {
    assert_ledger_matrix(MemoryRepo::new(header("memory")));

    let path = temp_path("matrix");
    let repo = JsonlRepo::create(&path, header("jsonl")).expect("jsonl repo");
    assert_ledger_matrix(repo);
    fs::remove_dir_all(path.parent().unwrap()).expect("cleanup");
}

#[test]
fn retry_requires_safe_transport_repetability_and_non_terminal_operation() {
    let ledger = AttemptLedger::from_records(&fixture_records()).expect("valid ledger");
    assert!(ledger.plan_retry("op-1", "attempt-2").is_ok());

    let mut never_records = fixture_records();
    if let DurableRecord::Operation { operation, .. } = &mut never_records[2] {
        if let DurableOperationKind::RetryConfigured { policy, .. } = &mut operation.kind {
            *policy = RetryPolicy::Never;
        }
    }
    let never = AttemptLedger::from_records(&never_records).expect("never config");
    assert!(never.plan_retry("op-1", "attempt-2").is_err());

    let mut latest_remote_records = fixture_records();
    latest_remote_records.extend([
        DurableRecord::Operation {
            seq: 8,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptStarted {
                    attempt_id: "attempt-2".into(),
                    ordinal: 2,
                },
            },
        },
        DurableRecord::Operation {
            seq: 9,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptFailed {
                    attempt_id: "attempt-2".into(),
                    error: AttemptErrorClass::Remote,
                },
            },
        },
    ]);
    let latest_remote = AttemptLedger::from_records(&latest_remote_records).expect("latest remote");
    assert!(latest_remote.plan_retry("op-1", "attempt-3").is_err());
    let mut latest_remote_raw = latest_remote_records;
    latest_remote_raw.push(DurableRecord::Operation {
        seq: 10,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-3".into(),
                ordinal: 3,
            },
        },
    });
    assert_rejected_in_memory_and_jsonl(&latest_remote_raw);

    let mut terminal = fixture_records();
    terminal.push(DurableRecord::Operation {
        seq: 8,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Finished {
                outcome: DurableOutcome::Failed,
            },
        },
    });
    let terminal_ledger = AttemptLedger::from_records(&terminal).expect("terminal ledger");
    assert!(terminal_ledger.plan_retry("op-1", "attempt-2").is_err());
}

#[test]
fn causal_order_and_global_attempt_identity_fail_closed() {
    let mut orphan = fixture_records();
    orphan.remove(1);
    assert_rejected_in_memory_and_jsonl(&orphan);
    assert!(AttemptLedger::from_records(&orphan).is_err());

    let mut duplicate = fixture_records();
    duplicate.insert(4, duplicate[3].clone());
    if let DurableRecord::Operation { seq, .. } = &mut duplicate[4] {
        *seq = 5;
    }
    for record in duplicate.iter_mut().skip(5) {
        match record {
            DurableRecord::Entry { seq, .. }
            | DurableRecord::Operation { seq, .. }
            | DurableRecord::Fact { seq, .. }
            | DurableRecord::Usage { seq, .. }
            | DurableRecord::Compaction { seq, .. } => *seq += 1,
        }
    }
    assert_rejected_in_memory_and_jsonl(&duplicate);
    assert!(AttemptLedger::from_records(&duplicate).is_err());

    let mut terminal_attempt = fixture_records();
    terminal_attempt.push(DurableRecord::Operation {
        seq: 8,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-2".into(),
                ordinal: 2,
            },
        },
    });
    terminal_attempt.insert(
        7,
        DurableRecord::Operation {
            seq: 8,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Finished {
                    outcome: DurableOutcome::Failed,
                },
            },
        },
    );
    if let DurableRecord::Operation { seq, .. } = &mut terminal_attempt[8] {
        *seq = 9;
    }
    assert_rejected_in_memory_and_jsonl(&terminal_attempt);
    assert!(AttemptLedger::from_records(&terminal_attempt).is_err());
}

#[test]
fn planned_effects_fail_closed_before_replay_when_attempt_follows_terminal() {
    let mut records = fixture_records();
    records.push(DurableRecord::Operation {
        seq: 8,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Finished {
                outcome: DurableOutcome::Failed,
            },
        },
    });
    records.push(DurableRecord::Operation {
        seq: 9,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-2".into(),
                ordinal: 2,
            },
        },
    });
    assert_rejected_in_memory_and_jsonl(&records);
    assert!(slim_core::session::planned_provider_effects(&records).is_err());
}

#[test]
fn ordinals_and_finish_usage_correlation_are_closed_over() {
    let mut bad_ordinal = fixture_records();
    if let DurableRecord::Operation { operation, .. } = &mut bad_ordinal[3] {
        if let DurableOperationKind::ProviderAttemptStarted { ordinal, .. } = &mut operation.kind {
            *ordinal = 2;
        }
    }
    assert_rejected_in_memory_and_jsonl(&bad_ordinal);
    assert!(AttemptLedger::from_records(&bad_ordinal).is_err());

    let mut bad_finish = fixture_records();
    if let DurableRecord::Operation { operation, .. } = &mut bad_finish[6] {
        if let DurableOperationKind::ProviderAttemptFailed { attempt_id, .. } = &mut operation.kind
        {
            *attempt_id = "not-started".into();
        }
    }
    assert_rejected_in_memory_and_jsonl(&bad_finish);
    assert!(AttemptLedger::from_records(&bad_finish).is_err());

    let mut bad_usage = fixture_records();
    if let DurableRecord::Usage { usage, .. } = &mut bad_usage[4] {
        usage.attempt_id = "not-started".into();
    }
    assert_rejected_in_memory_and_jsonl(&bad_usage);
    assert!(AttemptLedger::from_records(&bad_usage).is_err());
}

#[test]
fn persisted_error_class_roundtrips_without_losing_safe_transport() {
    assert_eq!(
        serde_json::to_value(RetryPolicy::SafeTransport).unwrap(),
        serde_json::json!("safe_transport")
    );
    assert_eq!(
        serde_json::to_value(DurableOperationKind::RetryConfigured {
            repeatable: true,
            policy: RetryPolicy::SafeTransport,
        })
        .unwrap(),
        serde_json::json!({
            "kind": "retry_configured",
            "repeatable": true,
            "policy": "safe_transport"
        })
    );
    let error = AttemptErrorClass::Transport {
        safe_to_retry: true,
    };
    let encoded = serde_json::to_string(&error).expect("serialize error class");
    let decoded: AttemptErrorClass =
        serde_json::from_str(&encoded).expect("deserialize error class");
    assert_eq!(decoded, error);
    assert_eq!(
        serde_json::to_value(DurableOperationKind::ProviderAttemptFailed {
            attempt_id: "attempt-1".into(),
            error,
        })
        .expect("serialize failed attempt"),
        serde_json::json!({
            "kind": "provider_attempt_failed",
            "attempt_id": "attempt-1",
            "error": {"transport": {"safe_to_retry": true}}
        })
    );
}

#[test]
fn unknown_usage_stays_unknown_and_partial_deltas_never_zero_known_values() {
    let mut records = fixture_records();
    records[4] = DurableRecord::Usage {
        seq: 5,
        usage: DurableUsage {
            operation_id: "op-1".into(),
            attempt_id: "attempt-1".into(),
            input_tokens: None,
            output_tokens: None,
        },
    };
    let ledger = AttemptLedger::from_records(&records).expect("unknown usage is valid");
    assert_eq!(
        ledger.usage("op-1", "attempt-1").unwrap().input_tokens,
        None
    );
    assert_eq!(
        ledger.usage("op-1", "attempt-1").unwrap().output_tokens,
        Some(5)
    );
}

#[test]
fn raw_second_attempt_requires_persisted_safe_repeatable_config_and_safe_failure() {
    let mut no_config = fixture_records();
    no_config.remove(2);
    no_config.push(DurableRecord::Operation {
        seq: 8,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-2".into(),
                ordinal: 2,
            },
        },
    });
    assert_rejected_in_memory_and_jsonl(&no_config);

    let mut success = fixture_records();
    success[6] = DurableRecord::Operation {
        seq: 7,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptFinished {
                attempt_id: "attempt-1".into(),
                outcome: DurableOutcome::Success,
            },
        },
    };
    success.push(DurableRecord::Operation {
        seq: 8,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-2".into(),
                ordinal: 2,
            },
        },
    });
    assert_rejected_in_memory_and_jsonl(&success);
}

#[test]
fn terminal_requires_started_and_no_open_attempt() {
    let terminal_before_start = vec![DurableRecord::Operation {
        seq: 1,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Finished {
                outcome: DurableOutcome::Success,
            },
        },
    }];
    assert_rejected_in_memory_and_jsonl(&terminal_before_start);

    let started_after_terminal = vec![
        DurableRecord::Entry {
            seq: 1,
            entry: DurableEntry {
                entry_id: "entry-user".into(),
                role: DurableEntryRole::User,
                content: "hello".into(),
                parent_entry_id: None,
                operation_id: "op-1".into(),
                tool_call_id: None,
            },
        },
        DurableRecord::Operation {
            seq: 2,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Finished {
                    outcome: DurableOutcome::Success,
                },
            },
        },
        DurableRecord::Operation {
            seq: 3,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: "entry-user".into(),
                },
            },
        },
    ];
    assert_rejected_in_memory_and_jsonl(&started_after_terminal);

    let mut open_attempt = fixture_records();
    open_attempt.remove(6);
    open_attempt.push(DurableRecord::Operation {
        seq: 8,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::Finished {
                outcome: DurableOutcome::Success,
            },
        },
    });
    assert_rejected_in_memory_and_jsonl(&open_attempt);
}

#[test]
fn retry_configuration_is_started_once_and_before_attempts() {
    let before_started = vec![DurableRecord::Operation {
        seq: 1,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::RetryConfigured {
                repeatable: true,
                policy: RetryPolicy::SafeTransport,
            },
        },
    }];
    assert_rejected_in_memory_and_jsonl(&before_started);

    let mut duplicate = fixture_records();
    duplicate.push(DurableRecord::Operation {
        seq: 8,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::RetryConfigured {
                repeatable: true,
                policy: RetryPolicy::SafeTransport,
            },
        },
    });
    assert_rejected_in_memory_and_jsonl(&duplicate);

    let mut after_attempt = fixture_records();
    after_attempt.remove(2);
    after_attempt.insert(
        3,
        DurableRecord::Operation {
            seq: 5,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::RetryConfigured {
                    repeatable: true,
                    policy: RetryPolicy::SafeTransport,
                },
            },
        },
    );
    for (index, record) in after_attempt.iter_mut().enumerate() {
        if index >= 4 {
            match record {
                DurableRecord::Entry { seq, .. }
                | DurableRecord::Operation { seq, .. }
                | DurableRecord::Fact { seq, .. }
                | DurableRecord::Usage { seq, .. }
                | DurableRecord::Compaction { seq, .. } => *seq += 1,
            }
        }
    }
    assert_rejected_in_memory_and_jsonl(&after_attempt);
}

#[test]
fn empty_attempt_id_is_rejected_by_ledger_and_retry_planner() {
    let mut empty = fixture_records();
    if let DurableRecord::Operation { operation, .. } = &mut empty[3] {
        if let DurableOperationKind::ProviderAttemptStarted { attempt_id, .. } = &mut operation.kind
        {
            attempt_id.clear();
        }
    }
    assert_rejected_in_memory_and_jsonl(&empty);

    let ledger = AttemptLedger::from_records(&fixture_records()).expect("valid ledger");
    assert!(ledger.plan_retry("op-1", "").is_err());
}

#[test]
fn jsonl_open_rejects_orphan_attempt_before_torn_tail_repair() {
    let path = temp_path("orphan-open");
    let header = header("jsonl-orphan-open");
    let records = [
        fixture_records()[0].clone(),
        fixture_records()[1].clone(),
        DurableRecord::Operation {
            seq: 3,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptFinished {
                    attempt_id: "not-started".into(),
                    outcome: slim_core::session::DurableOutcome::Success,
                },
            },
        },
    ];
    let mut bytes = Vec::new();
    bytes.extend(serde_json::to_vec(&header).expect("encode header"));
    bytes.push(b'\n');
    for record in records {
        bytes.extend(serde_json::to_vec(&record).expect("encode record"));
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(br#"{"type":"operation""#);
    std::fs::write(&path, &bytes).expect("write invalid attempt prefix and torn tail");

    let before_open = std::fs::read(&path).expect("read invalid attempt file");
    assert!(JsonlRepo::open(&path).is_err());
    assert_eq!(
        std::fs::read(&path).expect("read unchanged invalid attempt file"),
        before_open
    );
    std::fs::remove_dir_all(path.parent().expect("fixture directory")).expect("cleanup");
}
