use slim_core::session::{
    branch_v2, open_resume_v2, preflight_session, recover_durable_v2, resume_plan_from_path,
    DurableEntry, DurableEntryRole, DurableOperation, DurableOperationKind, DurableRecord,
    DurableRepo, DurableSessionHeader, JsonlRepo, PreflightStatus, ReplayPolicy, ResumePlanError,
    SessionFormat, MAX_DURABLE_SESSION_BYTES,
};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_path(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("slim-stage8-resume-{}-{stamp}", std::process::id()));
    fs::create_dir(&directory).expect("test directory");
    directory.join(label)
}

fn header(id: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "now", "D:\\Slim", None, None)
}

fn entry(seq: u64, operation_id: &str, entry_id: &str) -> DurableRecord {
    DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: entry_id.into(),
            role: DurableEntryRole::User,
            content: "content".into(),
            parent_entry_id: None,
            operation_id: operation_id.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    }
}

fn cleanup(path: &Path) {
    fs::remove_dir_all(path.parent().expect("test directory")).expect("cleanup");
}

#[test]
fn preflight_is_read_only_and_reports_torn_tail_until_explicit_recovery() {
    let path = temp_path("torn.jsonl");
    let mut repo = JsonlRepo::create(&path, header("torn")).expect("create");
    repo.append(entry(0, "op-1", "entry-1")).expect("append");
    drop(repo);

    let tail = br#"{"type":"entry","seq":1"#;
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open raw");
    file.write_all(tail).expect("write torn tail");
    drop(file);
    let before = fs::read(&path).expect("read before preflight");

    let report = preflight_session(&path).expect("preflight");
    assert_eq!(report.format, Some(SessionFormat::DurableV2));
    assert!(matches!(report.status, PreflightStatus::TornTail { .. }));
    assert_eq!(report.last_seq, Some(0));
    assert_eq!(report.next_seq, Some(1));
    assert_eq!(fs::read(&path).expect("read after preflight"), before);
    assert!(!PathBuf::from(format!("{}.quarantine", path.display())).exists());

    let reopened = recover_durable_v2(&path).expect("explicit recovery");
    assert_eq!(reopened.records(), &[entry(0, "op-1", "entry-1")]);
    drop(reopened);
    assert!(PathBuf::from(format!("{}.quarantine", path.display())).exists());
    cleanup(&path);
}

#[test]
fn open_no_repair_rejects_changed_incomplete_bytes_without_mutation() {
    let path = temp_path("no-repair.jsonl");
    let mut repo = JsonlRepo::create(&path, header("no-repair")).expect("create");
    repo.append(entry(0, "op-1", "entry-1")).expect("append");
    drop(repo);
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open raw");
    file.write_all(br#"{"type":"entry","seq":1"#)
        .expect("write torn tail");
    drop(file);
    let before = fs::read(&path).expect("bytes before");

    assert!(JsonlRepo::open_no_repair(&path).is_err());
    assert_eq!(fs::read(&path).expect("bytes after"), before);
    assert!(!PathBuf::from(format!("{}.quarantine", path.display())).exists());
    cleanup(&path);
}

#[test]
fn resume_handoff_rejects_different_healthy_replacement_after_preflight() {
    let path = temp_path("replacement.jsonl");
    let replacement = temp_path("replacement-new.jsonl");
    let backup = PathBuf::from(format!("{}.old", path.display()));
    let mut repo = JsonlRepo::create(&path, header("original")).expect("create original");
    repo.append(entry(0, "op-original", "entry-original"))
        .expect("append original");
    drop(repo);
    let report = preflight_session(&path).expect("preflight original");
    let mut new_repo =
        JsonlRepo::create(&replacement, header("replacement")).expect("create replacement");
    new_repo
        .append(entry(0, "op-replacement", "entry-replacement"))
        .expect("append replacement");
    drop(new_repo);

    fs::rename(&path, &backup).expect("move original");
    fs::rename(&replacement, &path).expect("install replacement");
    let before = fs::read(&path).expect("replacement bytes");
    let error = match open_resume_v2(&report) {
        Ok(_) => panic!("replacement must fail closed"),
        Err(error) => error,
    };
    assert!(format!("{error}").contains("resume repository handoff failed"));
    assert_eq!(fs::read(&path).expect("replacement unchanged"), before);

    let _ = fs::remove_file(&backup);
    cleanup(&path);
}

#[test]
fn v1_is_reported_but_cannot_be_resumed_as_v2() {
    let path = temp_path("legacy.jsonl");
    let header = serde_json::json!({
        "type": "session",
        "schema_version": 1,
        "id": "legacy",
        "timestamp": "now",
        "cwd": "D:\\Slim",
        "parent_id": null,
        "cutoff_seq": null
    });
    fs::write(&path, format!("{header}\n")).expect("legacy fixture");

    let report = preflight_session(&path).expect("preflight legacy");
    assert_eq!(report.format, Some(SessionFormat::LegacyV1));
    assert!(!report.can_resume_v2());
    assert!(matches!(
        resume_plan_from_path(&path),
        Err(ResumePlanError::UnsupportedSchema { .. })
    ));
    cleanup(&path);
}

#[test]
fn resume_plan_is_pure_and_only_exposes_safe_pending_tools() {
    let records = vec![
        entry(0, "op-1", "entry-1"),
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
                kind: DurableOperationKind::ToolPhaseIntent {
                    batch_id: "batch-1".into(),
                    batch_index: 0,
                    batch_limit: 2,
                    tool_call_id: "never-call".into(),
                    tool_name: "read".into(),
                    replay_policy: ReplayPolicy::Never,
                    input_redacted: "{}".into(),
                },
            },
        },
        entry(3, "op-2", "entry-2"),
        DurableRecord::Operation {
            seq: 4,
            operation: DurableOperation {
                operation_id: "op-2".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: "entry-2".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 5,
            operation: DurableOperation {
                operation_id: "op-2".into(),
                kind: DurableOperationKind::ToolPhaseIntent {
                    batch_id: "batch-2".into(),
                    batch_index: 0,
                    batch_limit: 2,
                    tool_call_id: "safe-call".into(),
                    tool_name: "read".into(),
                    replay_policy: ReplayPolicy::Safe,
                    input_redacted: "{}".into(),
                },
            },
        },
    ];
    let plan = slim_core::session::ResumePlan::from_records(&records).expect("resume plan");
    assert_eq!(plan.tool_replays().len(), 1);
    assert_eq!(plan.tool_replays()[0].call.tool_call_id, "safe-call");
    assert_eq!(plan.pending_queue().len(), 0);
    assert_eq!(plan.state().last_seq(), Some(5));
}

#[test]
fn preflight_marks_sequence_overflow_without_wrapping() {
    let path = temp_path("overflow.jsonl");
    let header = header("overflow");
    let record = entry(u64::MAX, "op-1", "entry-max");
    let bytes = format!(
        "{}\n{}\n",
        serde_json::to_string(&header).expect("header"),
        serde_json::to_string(&record).expect("record")
    );
    fs::write(&path, bytes).expect("overflow fixture");
    let report = preflight_session(&path).expect("preflight");
    assert_eq!(report.last_seq, Some(u64::MAX));
    assert_eq!(report.next_seq, None);
    assert!(report.sequence_overflow);
    cleanup(&path);
}

#[test]
fn semantic_prefix_is_invalid_and_recovery_keeps_bytes_unchanged() {
    let path = temp_path("semantic-invalid.jsonl");
    let header = header("semantic-invalid");
    let invalid = DurableRecord::Operation {
        seq: 0,
        operation: DurableOperation {
            operation_id: "op-invalid".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "missing-entry".into(),
            },
        },
    };
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header).expect("header"),
            serde_json::to_string(&invalid).expect("record")
        ),
    )
    .expect("fixture");
    let before = fs::read(&path).expect("bytes before");

    let report = preflight_session(&path).expect("preflight report");
    assert!(matches!(report.status, PreflightStatus::Invalid { .. }));
    assert!(recover_durable_v2(&path).is_err());
    assert_eq!(fs::read(&path).expect("bytes after"), before);
    cleanup(&path);
}

#[test]
fn preflight_checks_the_same_handle_metadata_before_reading_an_oversized_file() {
    let path = temp_path("oversized.jsonl");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("create sparse fixture");
    file.set_len(MAX_DURABLE_SESSION_BYTES + 1)
        .expect("grow sparse fixture");
    drop(file);

    let report = preflight_session(&path).expect("preflight report");
    assert!(matches!(report.status, PreflightStatus::Invalid { .. }));
    cleanup(&path);
}

#[test]
fn branch_v2_is_explicit_and_preserves_parent_bytes() {
    let path = temp_path("parent.jsonl");
    let mut repo = JsonlRepo::create(&path, header("parent")).expect("create");
    repo.append(entry(0, "op-1", "entry-1"))
        .expect("append one");
    repo.append(entry(1, "op-2", "entry-2"))
        .expect("append two");
    drop(repo);
    let before = fs::read(&path).expect("parent bytes");

    let child_path = branch_v2(&path, "child", 0).expect("branch");
    assert_eq!(fs::read(&path).expect("parent bytes after"), before);
    let child = JsonlRepo::open(&child_path).expect("open child");
    assert_eq!(child.header().parent_id.as_deref(), Some("parent"));
    assert_eq!(child.header().cutoff_seq, Some(0));
    assert_eq!(child.records(), &[entry(0, "op-1", "entry-1")]);
    assert_eq!(child.next_seq().expect("child next seq"), 1);
    drop(child);
    cleanup(&path);
}
