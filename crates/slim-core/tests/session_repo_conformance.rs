use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use slim_core::session::{
    DurableEntry, DurableEntryRole, DurableFact, DurableOperation, DurableOperationKind,
    DurableOutcome, DurableRecord, DurableRepo, DurableSessionHeader, DurableUsage, JsonlRepo,
    MemoryRepo,
};

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let candidate = root.join(format!(
        "slim-jsonl-repo-{}-{nanos}-{nonce}",
        std::process::id()
    ));
    assert_eq!(candidate.parent(), Some(root.as_path()));
    assert!(candidate
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("slim-jsonl-repo-")));
    std::fs::create_dir(&candidate).expect("create unique test directory");
    let directory = std::fs::canonicalize(&candidate).expect("canonical test directory");
    assert_eq!(directory.parent(), Some(root.as_path()));
    assert!(directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("slim-jsonl-repo-")));
    directory.join(format!("{label}.jsonl"))
}

fn cleanup(path: &Path) {
    let root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let parent = std::fs::canonicalize(path.parent().expect("test directory"))
        .expect("canonical test directory");
    assert_ne!(parent, root, "test cleanup must not remove %TEMP%");
    assert_eq!(parent.parent(), Some(root.as_path()));
    assert!(
        parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("slim-jsonl-repo-")),
        "unexpected test cleanup directory: {}",
        parent.display()
    );
    std::fs::remove_dir_all(parent).expect("remove test directory");
}

#[test]
fn memory_repo_appends_and_reads() {
    let header = DurableSessionHeader::new("session-1", "now", "D:\\Slim", None, None);
    let mut repo = MemoryRepo::new(header.clone());
    assert!(repo.records().is_empty());
    let record = DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "entry-1".into(),
            role: DurableEntryRole::User,
            content: "hello".into(),
            parent_entry_id: None,
            operation_id: "op-1".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    };

    repo.append(record.clone()).expect("append");

    assert_eq!(repo.header(), &header);
    assert_eq!(repo.records(), std::slice::from_ref(&record));
    assert_eq!(repo.read_prefix(0), vec![record]);
}

fn assert_common_contract<R: DurableRepo>(mut repo: R) {
    assert!(repo.records().is_empty());
    assert!(repo.read_prefix(0).is_empty());
    let records = vec![
        DurableRecord::Entry {
            seq: 0,
            entry: DurableEntry {
                entry_id: "entry-1".into(),
                role: DurableEntryRole::User,
                content: "hello".into(),
                parent_entry_id: None,
                operation_id: "op-1".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        },
        DurableRecord::Operation {
            seq: 1,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: "entry-1".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 2,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptStarted {
                    attempt_id: "attempt-1".into(),
                    ordinal: 1,
                },
            },
        },
        DurableRecord::Fact {
            seq: 3,
            fact: DurableFact {
                namespace: "session".into(),
                key: "mode".into(),
                value: json!("auto"),
            },
        },
        DurableRecord::Usage {
            seq: 4,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: Some(3),
                output_tokens: Some(5),
            },
        },
    ];
    let expected_header = repo.header().clone();

    for record in &records {
        repo.append(record.clone()).expect("append valid record");
    }
    assert_eq!(repo.header(), &expected_header);
    assert_eq!(repo.records(), records.as_slice());
    assert_eq!(
        repo.records()
            .iter()
            .map(DurableRecord::seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert_eq!(repo.read_prefix(1), records[..2]);
    assert_eq!(repo.read_prefix(u64::MAX), records);

    let before_rejected_append = repo.records().to_vec();
    let duplicate = DurableRecord::Usage {
        seq: 5,
        usage: DurableUsage {
            operation_id: "op-duplicate".into(),
            attempt_id: "attempt-duplicate".into(),
            input_tokens: None,
            output_tokens: None,
        },
    };
    let error = repo
        .append(duplicate)
        .expect_err("duplicate sequence rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(repo.records(), before_rejected_append.as_slice());

    let regressive = DurableRecord::Fact {
        seq: 3,
        fact: DurableFact {
            namespace: "session".into(),
            key: "regressive".into(),
            value: json!(true),
        },
    };
    let error = repo
        .append(regressive)
        .expect_err("regressive sequence rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(repo.records(), before_rejected_append.as_slice());

    let gap = DurableRecord::Operation {
        seq: 10,
        operation: DurableOperation {
            operation_id: "op-gap".into(),
            kind: DurableOperationKind::Suspended {
                reason: "gap".into(),
            },
        },
    };
    repo.append(gap.clone()).expect("increasing gap accepted");
    assert_eq!(repo.records().last(), Some(&gap));

    let first_read = repo.read_prefix(1);
    let snapshot = repo.records().to_vec();
    let second_read = repo.read_prefix(1);
    assert_eq!(first_read, second_read);
    assert_eq!(repo.records(), snapshot.as_slice());
}

#[test]
fn memory_repo_conformance() {
    let header = DurableSessionHeader::new("session-conformance", "now", "D:\\Slim", None, None);
    assert_common_contract(MemoryRepo::new(header));
}

#[test]
fn jsonl_repo_conformance() {
    let path = temp_path("conformance");
    let header = DurableSessionHeader::new("session-conformance", "now", "D:\\Slim", None, None);
    let repo = JsonlRepo::create(&path, header).expect("create jsonl repo");
    assert_common_contract(repo);
    cleanup(&path);
}

fn invalid_lifecycle_records() -> [(&'static str, Vec<DurableRecord>); 3] {
    [
        (
            "finished-before-started",
            vec![DurableRecord::Operation {
                seq: 0,
                operation: DurableOperation {
                    operation_id: "op-before-start".into(),
                    kind: DurableOperationKind::Finished {
                        outcome: DurableOutcome::Success,
                    },
                },
            }],
        ),
        (
            "duplicate-terminal",
            vec![
                DurableRecord::Entry {
                    seq: 0,
                    entry: DurableEntry {
                        entry_id: "entry-terminal".into(),
                        role: DurableEntryRole::User,
                        content: "hello".into(),
                        parent_entry_id: None,
                        operation_id: "op-terminal".into(),
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                        content_blocks: Vec::new(),
                    },
                },
                DurableRecord::Operation {
                    seq: 1,
                    operation: DurableOperation {
                        operation_id: "op-terminal".into(),
                        kind: DurableOperationKind::Started {
                            input_entry_id: "entry-terminal".into(),
                        },
                    },
                },
                DurableRecord::Operation {
                    seq: 2,
                    operation: DurableOperation {
                        operation_id: "op-terminal".into(),
                        kind: DurableOperationKind::Finished {
                            outcome: DurableOutcome::Success,
                        },
                    },
                },
                DurableRecord::Operation {
                    seq: 3,
                    operation: DurableOperation {
                        operation_id: "op-terminal".into(),
                        kind: DurableOperationKind::Finished {
                            outcome: DurableOutcome::Success,
                        },
                    },
                },
            ],
        ),
        (
            "queue-finished-before-claim",
            vec![
                DurableRecord::Operation {
                    seq: 0,
                    operation: DurableOperation {
                        operation_id: "op-queue-invalid".into(),
                        kind: DurableOperationKind::QueueIntent {
                            input_entry_id: None,
                        },
                    },
                },
                DurableRecord::Operation {
                    seq: 1,
                    operation: DurableOperation {
                        operation_id: "op-queue-invalid".into(),
                        kind: DurableOperationKind::Finished {
                            outcome: DurableOutcome::Success,
                        },
                    },
                },
            ],
        ),
    ]
}

#[test]
fn repositories_reject_invalid_lifecycle_before_mutating_memory_or_jsonl() {
    for (label, records) in invalid_lifecycle_records() {
        let mut memory = MemoryRepo::new(DurableSessionHeader::new(
            format!("memory-{label}"),
            "now",
            "D:\\Slim",
            None,
            None,
        ));
        let mut accepted = Vec::new();
        for record in &records {
            let result = memory.append(record.clone());
            if result.is_err() {
                break;
            }
            accepted.push(record.clone());
        }
        assert!(
            accepted.len() < records.len(),
            "memory repo accepted invalid lifecycle {label}"
        );
        assert_eq!(memory.records(), accepted.as_slice());

        let path = temp_path(&format!("invalid-{label}"));
        let mut jsonl = JsonlRepo::create(
            &path,
            DurableSessionHeader::new(format!("jsonl-{label}"), "now", "D:\\Slim", None, None),
        )
        .expect("create jsonl repo");
        for record in &records {
            if jsonl.append(record.clone()).is_err() {
                break;
            }
        }
        assert_eq!(jsonl.records(), accepted.as_slice());
        let bytes_before_drop = std::fs::read(&path).expect("read jsonl prefix");
        drop(jsonl);
        let reopened = JsonlRepo::open(&path).expect("reopen valid prefix");
        assert_eq!(reopened.records(), accepted.as_slice());
        drop(reopened);
        assert_eq!(
            std::fs::read(&path).expect("read reopened jsonl prefix"),
            bytes_before_drop
        );
        cleanup(&path);
    }
}

#[test]
fn jsonl_open_rejects_invalid_lifecycle_before_torn_tail_repair() {
    let path = temp_path("invalid-open");
    let header = DurableSessionHeader::new("invalid-open", "now", "D:\\Slim", None, None);
    let invalid_terminal = DurableRecord::Operation {
        seq: 0,
        operation: DurableOperation {
            operation_id: "op-before-start-open".into(),
            kind: DurableOperationKind::Finished {
                outcome: DurableOutcome::Success,
            },
        },
    };
    let mut bytes = Vec::new();
    bytes.extend(serde_json::to_vec(&header).expect("encode header"));
    bytes.push(b'\n');
    bytes.extend(serde_json::to_vec(&invalid_terminal).expect("encode invalid terminal"));
    bytes.push(b'\n');
    bytes.extend_from_slice(br#"{"type":"operation""#);
    std::fs::write(&path, &bytes).expect("write invalid prefix and torn tail");

    let before_open = std::fs::read(&path).expect("read invalid file");
    assert!(JsonlRepo::open(&path).is_err());
    assert_eq!(
        std::fs::read(&path).expect("read unchanged invalid file"),
        before_open
    );
    cleanup(&path);
}
