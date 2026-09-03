use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use slim_core::session::{
    inspect_session, CompactionCheckpoint, CompactionReason, DurableEntry, DurableEntryRole,
    DurableFact, DurableOperation, DurableOperationKind, DurableRecord, DurableSessionHeader,
    DurableUsage, ReplayPolicy, SessionFormat, DURABLE_SCHEMA_VERSION,
};

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "slim-schema-v2-{label}-{}-{nanos}.jsonl",
        std::process::id()
    ))
}

#[test]
fn durable_header_and_record_kinds_have_stable_json_envelopes() {
    assert_eq!(DURABLE_SCHEMA_VERSION, 2);

    let header = DurableSessionHeader::new(
        "session-2",
        "2026-08-22T12:00:00Z",
        "D:\\Slim",
        Some("parent-1".into()),
        Some(3),
    );
    assert_eq!(header.schema_version(), DURABLE_SCHEMA_VERSION);
    let header_json = serde_json::to_value(&header).expect("header json");
    assert_eq!(
        header_json,
        json!({
            "type": "session",
            "schema_version": 2,
            "id": "session-2",
            "timestamp": "2026-08-22T12:00:00Z",
            "cwd": "D:\\Slim",
            "parent_id": "parent-1",
            "cutoff_seq": 3,
        })
    );
    for schema_version in [1, 99] {
        let invalid = json!({
            "type": "session",
            "schema_version": schema_version,
            "id": "invalid",
            "timestamp": "now",
            "cwd": "D:\\Slim",
            "parent_id": null,
            "cutoff_seq": null,
        });
        assert!(serde_json::from_value::<DurableSessionHeader>(invalid).is_err());
    }

    let records = [
        DurableRecord::Entry {
            seq: 1,
            entry: DurableEntry {
                entry_id: "entry-1".into(),
                role: DurableEntryRole::User,
                content: "hello".into(),
                parent_entry_id: None,
                operation_id: "op-1".into(),
                tool_call_id: None,
            },
        },
        DurableRecord::Entry {
            seq: 2,
            entry: DurableEntry {
                entry_id: "entry-2".into(),
                role: DurableEntryRole::Assistant,
                content: "tool request".into(),
                parent_entry_id: Some("entry-1".into()),
                operation_id: "op-1".into(),
                tool_call_id: Some("call-1".into()),
            },
        },
        DurableRecord::Operation {
            seq: 3,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolIntent {
                    tool_call_id: "call-1".into(),
                    tool_name: "write".into(),
                    replay_policy: ReplayPolicy::Never,
                },
            },
        },
        DurableRecord::Fact {
            seq: 4,
            fact: DurableFact {
                namespace: "session".into(),
                key: "mode".into(),
                value: json!("auto"),
            },
        },
        DurableRecord::Usage {
            seq: 5,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: Some(17),
                output_tokens: Some(42),
            },
        },
    ];

    let encoded: Vec<_> = records
        .iter()
        .map(|record| serde_json::to_value(record).expect("record json"))
        .collect();
    assert_eq!(
        encoded,
        vec![
            json!({
                "type": "entry",
                "seq": 1,
                "entry": {
                    "entry_id": "entry-1",
                    "role": "user",
                    "content": "hello",
                    "parent_entry_id": null,
                    "operation_id": "op-1",
                    "tool_call_id": null
                }
            }),
            json!({
                "type": "entry",
                "seq": 2,
                "entry": {
                    "entry_id": "entry-2",
                    "role": "assistant",
                    "content": "tool request",
                    "parent_entry_id": "entry-1",
                    "operation_id": "op-1",
                    "tool_call_id": "call-1"
                }
            }),
            json!({
                "type": "operation",
                "seq": 3,
                "operation": {
                    "operation_id": "op-1",
                    "kind": {
                        "kind": "tool_intent",
                        "tool_call_id": "call-1",
                        "tool_name": "write",
                        "replay_policy": "never"
                    }
                }
            }),
            json!({
                "type": "fact",
                "seq": 4,
                "fact": {
                    "namespace": "session",
                    "key": "mode",
                    "value": "auto"
                }
            }),
            json!({
                "type": "usage",
                "seq": 5,
                "usage": {
                    "operation_id": "op-1",
                    "attempt_id": "attempt-1",
                    "input_tokens": 17,
                    "output_tokens": 42
                }
            })
        ]
    );
    assert_eq!(
        records.iter().map(DurableRecord::seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        [
            DurableEntryRole::User,
            DurableEntryRole::Assistant,
            DurableEntryRole::Tool,
        ]
        .iter()
        .map(|role| serde_json::to_value(role).unwrap())
        .collect::<Vec<_>>(),
        vec![json!("user"), json!("assistant"), json!("tool")]
    );

    let operation_goldens = vec![
        (
            DurableOperationKind::Started {
                input_entry_id: "entry-1".into(),
            },
            json!({"kind": "started", "input_entry_id": "entry-1"}),
        ),
        (
            DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-1".into(),
                ordinal: 1,
            },
            json!({"kind": "provider_attempt_started", "attempt_id": "attempt-1", "ordinal": 1}),
        ),
        (
            DurableOperationKind::ProviderAttemptFinished {
                attempt_id: "attempt-1".into(),
                outcome: slim_core::session::DurableOutcome::Success,
            },
            json!({"kind": "provider_attempt_finished", "attempt_id": "attempt-1", "outcome": "success"}),
        ),
        (
            DurableOperationKind::ToolIntent {
                tool_call_id: "call-1".into(),
                tool_name: "write".into(),
                replay_policy: ReplayPolicy::Never,
            },
            json!({"kind": "tool_intent", "tool_call_id": "call-1", "tool_name": "write", "replay_policy": "never"}),
        ),
        (
            DurableOperationKind::ToolIntent {
                tool_call_id: "call-2".into(),
                tool_name: "read".into(),
                replay_policy: ReplayPolicy::Safe,
            },
            json!({"kind": "tool_intent", "tool_call_id": "call-2", "tool_name": "read", "replay_policy": "safe"}),
        ),
        (
            DurableOperationKind::ToolFinished {
                tool_call_id: "call-1".into(),
                outcome: slim_core::session::DurableOutcome::Failed,
            },
            json!({"kind": "tool_finished", "tool_call_id": "call-1", "outcome": "failed"}),
        ),
        (
            DurableOperationKind::Suspended {
                reason: "paused".into(),
            },
            json!({"kind": "suspended", "reason": "paused"}),
        ),
        (
            DurableOperationKind::Finished {
                outcome: slim_core::session::DurableOutcome::Success,
            },
            json!({"kind": "finished", "outcome": "success"}),
        ),
        (DurableOperationKind::Aborted, json!({"kind": "aborted"})),
    ];
    for (kind, golden) in operation_goldens {
        assert_eq!(serde_json::to_value(kind).unwrap(), golden);
    }
    assert_eq!(
        serde_json::to_value(DurableUsage {
            operation_id: "op-unknown".into(),
            attempt_id: "attempt-unknown".into(),
            input_tokens: None,
            output_tokens: None,
        })
        .unwrap(),
        json!({
            "operation_id": "op-unknown",
            "attempt_id": "attempt-unknown",
            "input_tokens": null,
            "output_tokens": null
        })
    );
}

#[test]
fn compaction_checkpoint_is_additive_to_schema_v2_and_round_trips() {
    let record = DurableRecord::Compaction {
        seq: 6,
        checkpoint: CompactionCheckpoint {
            checkpoint_id: "compact-1".into(),
            summary: "## Goal\nShip".into(),
            first_kept_entry_id: "entry-9".into(),
            prefix_fingerprint: "0123456789abcdef".into(),
            previous_checkpoint_id: None,
            tokens_before: 100_000,
            tokens_after: 19_000,
            input_tokens: Some(1_000),
            output_tokens: Some(300),
            duration_ms: 25,
            reason: CompactionReason::HardThreshold,
            read_files: vec!["D:\\Slim\\README.md".into()],
            modified_files: vec!["D:\\Slim\\crates\\slim-core\\src\\lib.rs".into()],
        },
    };
    let value = serde_json::to_value(&record).expect("serialize checkpoint");
    assert_eq!(value["type"], "compaction");
    assert_eq!(value["checkpoint"]["reason"], "hard_threshold");
    assert_eq!(
        serde_json::from_value::<DurableRecord>(value).expect("deserialize checkpoint"),
        record
    );

    let legacy = serde_json::json!({
        "type": "fact",
        "seq": 1,
        "fact": {"namespace": "session", "key": "mode", "value": "auto"}
    });
    assert!(serde_json::from_value::<DurableRecord>(legacy).is_ok());
}

#[test]
fn inspect_session_reads_only_header_and_never_quarantines() {
    let path = temp_path("legacy");
    let legacy = b"{\"type\":\"session\",\"schema_version\":1,\"id\":\"legacy-1\",\"timestamp\":\"unknown\",\"cwd\":\"D:\\\\Slim\",\"parent_id\":null,\"cutoff_seq\":null}\n{\"type\":\"event\",\"seq\":1,\"event\":{}}\n";
    fs::write(&path, legacy).expect("legacy fixture");
    let before = fs::read(&path).expect("read legacy");
    let inspection = inspect_session(&path).expect("inspect legacy");
    assert_eq!(inspection.format, SessionFormat::LegacyV1);
    assert_eq!(inspection.session_id, "legacy-1");
    assert_eq!(fs::read(&path).expect("read unchanged legacy"), before);
    assert!(!path.with_extension("jsonl.quarantine").exists());
    assert!(!PathBuf::from(format!("{}.quarantine", path.display())).exists());
    let _ = fs::remove_file(path);
}

#[test]
fn inspect_session_classifies_v2_and_rejects_empty_unknown_or_invalid_headers() {
    let v2_path = temp_path("v2");
    let header = DurableSessionHeader::new("durable-2", "now", "D:\\Slim", None, None);
    fs::write(
        &v2_path,
        format!(
            "{}\n{{\"type\":\"not-read\"}}",
            serde_json::to_string(&header).unwrap()
        ),
    )
    .expect("v2 fixture");
    let before = fs::read(&v2_path).expect("read v2");
    let inspection = inspect_session(&v2_path).expect("inspect v2");
    assert_eq!(inspection.format, SessionFormat::DurableV2);
    assert_eq!(inspection.session_id, "durable-2");
    assert_eq!(fs::read(&v2_path).expect("read unchanged v2"), before);
    assert!(!v2_path.with_extension("jsonl.quarantine").exists());
    assert!(!PathBuf::from(format!("{}.quarantine", v2_path.display())).exists());

    for (label, contents) in [
        ("empty", ""),
        (
            "unknown",
            r#"{"type":"session","schema_version":99,"id":"unknown"}"#,
        ),
        (
            "bad-type",
            r#"{"type":"event","schema_version":2,"id":"bad"}"#,
        ),
        ("malformed", "not json"),
    ] {
        let path = temp_path(label);
        fs::write(&path, contents).expect("invalid fixture");
        let error = inspect_session(&path).expect_err("invalid header should fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{label}");
        let _ = fs::remove_file(path);
    }
    let _ = fs::remove_file(v2_path);
}
