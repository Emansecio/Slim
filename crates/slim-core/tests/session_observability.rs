use slim_core::session::{
    append_with_hooks, DurableAppendProjection, DurableEntry, DurableEntryRole, DurableFact,
    DurableRecord, DurableRepo, DurableSessionHeader, DurableSnapshot, DurableWatch, HookFailure,
    JsonlRepo, MemoryRepo, PostAppendHooks, TelemetryRing, MAX_DURABLE_SESSION_BYTES,
    MAX_OBSERVABILITY_ID_BYTES, MAX_TELEMETRY_EVENTS,
};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn header() -> DurableSessionHeader {
    DurableSessionHeader::new("observability", "now", "D:\\Slim", None, None)
}

fn entry(seq: u64, content: &str) -> DurableRecord {
    DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: format!("entry-{seq}"),
            role: DurableEntryRole::User,
            content: content.into(),
            parent_entry_id: None,
            operation_id: format!("op-{seq}"),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    }
}

fn temp_path(label: &str) -> PathBuf {
    let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    root.join(format!(
        "slim-observability-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

#[test]
fn snapshot_captures_an_immutable_prefix_and_state() {
    let mut repo = MemoryRepo::new(header());
    repo.append(entry(0, "first")).expect("append first");
    repo.append(DurableRecord::Fact {
        seq: 1,
        fact: DurableFact {
            namespace: "test".into(),
            key: "answer".into(),
            value: serde_json::json!(42),
        },
    })
    .expect("append fact");

    let snapshot = DurableSnapshot::from_repo_prefix(&repo, 0).expect("snapshot");
    assert_eq!(snapshot.last_seq(), Some(0));
    assert_eq!(snapshot.records().len(), 1);
    assert_eq!(snapshot.state().entries().len(), 1);
    assert_eq!(snapshot.state().fact_value("test", "answer"), None);
    let debug = format!("{snapshot:?}");
    let encoded = serde_json::to_string(&snapshot).expect("serialize snapshot summary");
    assert!(!debug.contains("first"));
    assert!(!encoded.contains("first"));

    repo.append(entry(2, "later")).expect("append later");
    assert_eq!(snapshot.records().len(), 1);
    assert_eq!(snapshot.state().entries().len(), 1);
}

#[test]
fn watch_reads_only_newline_terminated_records_without_repair_or_duplicates() {
    let path = temp_path("watch");
    let mut repo = JsonlRepo::create(&path, header()).expect("create");
    repo.append(entry(0, "secret-entry")).expect("append first");
    let before = fs::read(&path).expect("read before watch");
    drop(repo);

    let mut watch = DurableWatch::open(&path).expect("open watch");
    let first = watch.poll().expect("first poll");
    assert_eq!(first.records().len(), 1);
    assert!(!first.torn_tail());
    let saved_cursor = first.cursor();
    assert!(watch.poll().expect("repeat poll").records().is_empty());

    let mut repo = JsonlRepo::open(&path).expect("reopen for append");
    repo.append(entry(1, "second")).expect("append second");
    let second = watch.poll().expect("second poll");
    assert_eq!(second.records().len(), 1);
    assert_eq!(second.records()[0].seq(), 1);
    assert!(watch
        .poll()
        .expect("no duplicate poll")
        .records()
        .is_empty());
    drop(repo);

    let mut resumed =
        DurableWatch::open_from_cursor(&path, saved_cursor).expect("reattach at saved cursor");
    let resumed_batch = resumed.poll().expect("resumed poll");
    assert_eq!(resumed_batch.records().len(), 1);
    assert_eq!(resumed_batch.records()[0].seq(), 1);
    drop(resumed);

    let mut raw = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open append handle");
    raw.write_all(br#"{"type":"entry""#)
        .expect("write torn tail");
    raw.sync_data().expect("sync torn tail");
    let torn = watch.poll().expect("observe torn tail");
    assert!(torn.records().is_empty());
    assert!(torn.torn_tail());
    assert_eq!(fs::read(&path).expect("read after watch"), {
        let mut expected = before;
        expected.extend_from_slice(&serde_json::to_vec(&entry(1, "second")).expect("record json"));
        expected.push(b'\n');
        expected.extend_from_slice(br#"{"type":"entry""#);
        expected
    });
    drop(raw);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(format!("{}.lock", path.display()));
}

#[test]
fn post_append_hooks_receive_only_a_safe_projection_and_cannot_rollback() {
    let secret = "hook-secret-content";
    let record = entry(0, secret);
    let projection = DurableAppendProjection::from_record(&record);
    let encoded = serde_json::to_string(&projection).expect("serialize projection");
    assert!(!encoded.contains(secret));
    assert!(encoded.contains("entry-0"));

    let mut hooks = PostAppendHooks::new();
    hooks.push(|_: &DurableAppendProjection| Err(HookFailure::Rejected));
    hooks.push(|event: &DurableAppendProjection| {
        assert_eq!(event.seq(), 0);
        assert_eq!(event.entry_id(), Some("entry-0"));
        Ok(())
    });
    let mut repo = MemoryRepo::new(header());
    let report = append_with_hooks(&mut repo, record, &mut hooks).expect("append succeeds");
    assert_eq!(repo.records().len(), 1);
    assert_eq!(report.invoked(), 2);
    assert_eq!(report.failures(), &[HookFailure::Rejected]);
}

#[test]
fn telemetry_is_bounded_saturating_and_content_free() {
    let secret = "telemetry-secret-content";
    let entry_projection = DurableAppendProjection::from_record(&entry(0, secret));
    let fact_projection = DurableAppendProjection::from_record(&DurableRecord::Fact {
        seq: 1,
        fact: DurableFact {
            namespace: "ns".into(),
            key: "key".into(),
            value: serde_json::json!(secret),
        },
    });
    let output_projection = DurableAppendProjection::from_record(&DurableRecord::Operation {
        seq: 2,
        operation: slim_core::session::DurableOperation {
            operation_id: "op-tool".into(),
            kind: slim_core::session::DurableOperationKind::ToolPhaseOutput {
                batch_id: "batch".into(),
                batch_index: 0,
                batch_limit: 1,
                tool_call_id: "call".into(),
                output: Some(secret.into()),
                artifact_ref: None,
            },
        },
    });
    let error_projection = DurableAppendProjection::from_record(&DurableRecord::Operation {
        seq: 3,
        operation: slim_core::session::DurableOperation {
            operation_id: "op-error".into(),
            kind: slim_core::session::DurableOperationKind::ProviderAttemptFailed {
                attempt_id: "attempt".into(),
                error: slim_core::session::DurableErrorClass::Unknown,
            },
        },
    });

    let mut telemetry = TelemetryRing::new(2);
    telemetry.record_projection(&entry_projection);
    telemetry.record_projection(&fact_projection);
    telemetry.record_projection(&output_projection);
    telemetry.record_projection(&error_projection);
    let encoded = serde_json::to_string(&telemetry.events()).expect("serialize telemetry");
    let debug = format!("{telemetry:?}");
    assert!(!encoded.contains(secret));
    assert!(!debug.contains(secret));
    assert_eq!(telemetry.events().len(), 2);
    assert_eq!(telemetry.overflow_count(), 2);

    let max_usage = DurableAppendProjection::Usage {
        seq: 4,
        operation_id: "op-usage".into(),
        attempt_id: "attempt".into(),
        input_tokens: Some(u64::MAX),
        output_tokens: Some(u64::MAX),
    };
    telemetry.record_projection(&max_usage);
    telemetry.record_projection(&max_usage);
    assert_eq!(telemetry.counters().input_tokens(), u64::MAX);
    assert_eq!(telemetry.counters().output_tokens(), u64::MAX);
}

#[test]
fn watch_rejects_an_oversized_source_without_touching_it() {
    let path = temp_path("watch-limit");
    let file = fs::File::create(&path).expect("create oversized source");
    file.set_len(MAX_DURABLE_SESSION_BYTES + 1)
        .expect("make sparse oversized source");
    drop(file);
    let before = fs::metadata(&path).expect("metadata before").len();
    assert!(DurableWatch::open(&path).is_err());
    assert_eq!(fs::metadata(&path).expect("metadata after").len(), before);
    let _ = fs::remove_file(&path);
}

#[test]
fn telemetry_capacity_is_clamped_and_zero_is_an_explicit_drop_mode() {
    let bounded = TelemetryRing::new(MAX_TELEMETRY_EVENTS.saturating_add(1));
    assert_eq!(bounded.capacity(), MAX_TELEMETRY_EVENTS);

    let mut zero = TelemetryRing::new(0);
    zero.record_projection(&DurableAppendProjection::from_record(&entry(0, "content")));
    assert!(zero.is_empty());
    assert_eq!(zero.overflow_count(), 1);
}

#[test]
fn caller_controlled_ids_are_replaced_when_oversized() {
    let secret_id = "secret-id-".to_owned() + &"x".repeat(MAX_OBSERVABILITY_ID_BYTES + 32);
    let record = DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: secret_id.clone(),
            role: DurableEntryRole::User,
            content: "content".into(),
            parent_entry_id: None,
            operation_id: secret_id.clone(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    };
    let projection = DurableAppendProjection::from_record(&record);
    let projection_json = serde_json::to_string(&projection).expect("serialize projection");
    assert!(!projection_json.contains(&secret_id));
    assert!(projection_json.contains("oversized-id"));

    let tool_record = DurableRecord::Operation {
        seq: 1,
        operation: slim_core::session::DurableOperation {
            operation_id: secret_id.clone(),
            kind: slim_core::session::DurableOperationKind::ToolPhaseOutput {
                batch_id: secret_id.clone(),
                batch_index: 0,
                batch_limit: 1,
                tool_call_id: secret_id.clone(),
                output: Some("tool-output-secret".into()),
                artifact_ref: None,
            },
        },
    };
    let tool_projection = DurableAppendProjection::from_record(&tool_record);
    let tool_json = serde_json::to_string(&tool_projection).expect("serialize tool projection");
    assert!(!tool_json.contains(&secret_id));
    assert!(!format!("{tool_projection:?}").contains(&secret_id));

    let mut telemetry = TelemetryRing::new(2);
    telemetry.record_projection(&projection);
    telemetry.record_projection(&tool_projection);
    let events_json = serde_json::to_string(&telemetry.events()).expect("serialize events");
    assert!(!events_json.contains(&secret_id));
    assert!(events_json.len() < MAX_OBSERVABILITY_ID_BYTES * 8);
}
