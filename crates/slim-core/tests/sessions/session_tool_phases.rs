use serde_json::json;
use slim_core::session::{
    DurableEntry, DurableEntryRole, DurableOperation, DurableOperationKind, DurableOutcome,
    DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo, MemoryRepo, ReplayPlan,
    ReplayPolicy, ToolPhaseLedger, MAX_DURABLE_SESSION_BYTES,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn records() -> Vec<DurableRecord> {
    vec![
        DurableRecord::Entry {
            seq: 1,
            entry: DurableEntry {
                entry_id: "entry-1".into(),
                role: DurableEntryRole::User,
                content: "run tool".into(),
                parent_entry_id: None,
                operation_id: "op-1".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        },
        DurableRecord::Operation {
            seq: 2,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: "entry-1".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 3,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseIntent {
                    batch_id: "batch-1".into(),
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    replay_policy: ReplayPolicy::Safe,
                    input_redacted: "{\"path\":\"README.md\"}".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 4,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseStarted {
                    batch_id: "batch-1".into(),
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id: "call-1".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 5,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseOutput {
                    batch_id: "batch-1".into(),
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id: "call-1".into(),
                    output: Some("ok".into()),
                    artifact_ref: None,
                },
            },
        },
        DurableRecord::Operation {
            seq: 6,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseFinished {
                    batch_id: "batch-1".into(),
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id: "call-1".into(),
                    outcome: DurableOutcome::Success,
                },
            },
        },
        DurableRecord::Operation {
            seq: 7,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Finished {
                    outcome: DurableOutcome::Success,
                },
            },
        },
    ]
}

#[test]
fn reconstructs_a_completed_batch_without_a_replay_item() {
    let ledger = ToolPhaseLedger::from_records(&records()).expect("valid phases");
    let batches = ledger.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].batch_id, "batch-1");
    assert_eq!(batches[0].calls.len(), 1);
    assert_eq!(batches[0].calls[0].tool_call_id, "call-1");
    assert_eq!(batches[0].calls[0].outcome, Some(DurableOutcome::Success));

    let plan = ReplayPlan::from_records(&records()).expect("replay plan");
    assert!(plan.pending().is_empty());
}

#[test]
fn many_operations_can_complete_without_leaking_open_tool_calls() {
    const OPERATION_COUNT: u32 = 128;
    let mut durable_records = Vec::with_capacity((OPERATION_COUNT as usize) * 7);
    let mut seq = 1u64;

    for index in 0..OPERATION_COUNT {
        let operation_id = format!("op-scale-{index}");
        let entry_id = format!("entry-scale-{index}");
        let batch_id = format!("batch-scale-{index}");
        let tool_call_id = format!("call-scale-{index}");

        durable_records.push(DurableRecord::Entry {
            seq,
            entry: DurableEntry {
                entry_id: entry_id.clone(),
                role: DurableEntryRole::User,
                content: "scale".into(),
                parent_entry_id: None,
                operation_id: operation_id.clone(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        });
        seq += 1;
        durable_records.push(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.clone(),
                kind: DurableOperationKind::Started {
                    input_entry_id: entry_id,
                },
            },
        });
        seq += 1;
        durable_records.push(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.clone(),
                kind: DurableOperationKind::ToolPhaseIntent {
                    batch_id: batch_id.clone(),
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id: tool_call_id.clone(),
                    tool_name: "read".into(),
                    replay_policy: ReplayPolicy::Safe,
                    input_redacted: "{}".into(),
                },
            },
        });
        seq += 1;
        durable_records.push(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.clone(),
                kind: DurableOperationKind::ToolPhaseStarted {
                    batch_id: batch_id.clone(),
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id: tool_call_id.clone(),
                },
            },
        });
        seq += 1;
        durable_records.push(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.clone(),
                kind: DurableOperationKind::ToolPhaseOutput {
                    batch_id: batch_id.clone(),
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id: tool_call_id.clone(),
                    output: Some("ok".into()),
                    artifact_ref: None,
                },
            },
        });
        seq += 1;
        durable_records.push(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.clone(),
                kind: DurableOperationKind::ToolPhaseFinished {
                    batch_id,
                    batch_index: 0,
                    batch_limit: 1,
                    tool_call_id,
                    outcome: DurableOutcome::Success,
                },
            },
        });
        seq += 1;
        durable_records.push(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id,
                kind: DurableOperationKind::Finished {
                    outcome: DurableOutcome::Success,
                },
            },
        });
        seq += 1;
    }

    let ledger = ToolPhaseLedger::from_records(&durable_records).expect("scale ledger");
    assert_eq!(ledger.batches().len(), OPERATION_COUNT as usize);
    assert!(ledger.incomplete().is_empty());
}

#[test]
fn replay_never_is_excluded_and_safe_pending_is_listed_once() {
    let mut pending = records();
    pending.truncate(4);
    let plan = ReplayPlan::from_records(&pending).expect("pending replay plan");
    assert_eq!(plan.pending().len(), 1);
    assert_eq!(
        plan.items()[0].disposition,
        slim_core::session::ReplayDisposition::SafePending
    );

    if let DurableRecord::Operation { operation, .. } = &mut pending[2] {
        if let DurableOperationKind::ToolPhaseIntent { replay_policy, .. } = &mut operation.kind {
            *replay_policy = ReplayPolicy::Never;
        }
    }
    let never = ReplayPlan::from_records(&pending).expect("never replay plan");
    assert!(never.pending().is_empty());
    assert_eq!(
        never.items()[0].disposition,
        slim_core::session::ReplayDisposition::Never
    );
}

#[test]
fn batch_reconstruction_keeps_index_order_and_stable_call_ids() {
    let mut two_calls = records();
    two_calls.pop();
    for record in &mut two_calls {
        if let DurableRecord::Operation { operation, .. } = record {
            match &mut operation.kind {
                DurableOperationKind::ToolPhaseIntent { batch_limit, .. }
                | DurableOperationKind::ToolPhaseStarted { batch_limit, .. }
                | DurableOperationKind::ToolPhaseOutput { batch_limit, .. }
                | DurableOperationKind::ToolPhaseFinished { batch_limit, .. } => {
                    *batch_limit = 2;
                }
                _ => {}
            }
        }
    }
    two_calls.extend([
        DurableRecord::Operation {
            seq: 7,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseIntent {
                    batch_id: "batch-1".into(),
                    batch_index: 1,
                    batch_limit: 2,
                    tool_call_id: "call-2".into(),
                    tool_name: "read".into(),
                    replay_policy: ReplayPolicy::Never,
                    input_redacted: "{}".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 8,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseStarted {
                    batch_id: "batch-1".into(),
                    batch_index: 1,
                    batch_limit: 2,
                    tool_call_id: "call-2".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 9,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseOutput {
                    batch_id: "batch-1".into(),
                    batch_index: 1,
                    batch_limit: 2,
                    tool_call_id: "call-2".into(),
                    output: Some("failed output".into()),
                    artifact_ref: None,
                },
            },
        },
        DurableRecord::Operation {
            seq: 10,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ToolPhaseFinished {
                    batch_id: "batch-1".into(),
                    batch_index: 1,
                    batch_limit: 2,
                    tool_call_id: "call-2".into(),
                    outcome: DurableOutcome::Failed,
                },
            },
        },
    ]);
    let ledger = ToolPhaseLedger::from_records(&two_calls).expect("two-call batch");
    let calls = &ledger.batches()[0].calls;
    assert_eq!(
        calls
            .iter()
            .map(|call| call.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call-1", "call-2"]
    );
    assert_eq!(calls[1].outcome, Some(DurableOutcome::Failed));
}

#[test]
fn bounds_and_output_shape_are_checked_before_ledger_mutation() {
    let mut pending = records();
    pending.truncate(4);
    let mut ledger = ToolPhaseLedger::from_records(&pending).expect("pending ledger");
    let before = ledger.clone();

    let mut too_large = records()[4].clone();
    if let DurableRecord::Operation { operation, .. } = &mut too_large {
        if let DurableOperationKind::ToolPhaseOutput { output, .. } = &mut operation.kind {
            *output = Some("x".repeat(slim_core::session::MAX_TOOL_INLINE_BYTES + 1));
        }
    }
    assert!(ledger.append(&too_large).is_err());
    assert_eq!(ledger, before);

    let mut bad_limit = pending[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut bad_limit {
        if let DurableOperationKind::ToolPhaseIntent { batch_limit, .. } = &mut operation.kind {
            *batch_limit = 33;
        }
    }
    assert!(
        ToolPhaseLedger::from_records(&[pending[0].clone(), pending[1].clone(), bad_limit,])
            .is_err()
    );

    let mut bad_output = records()[4].clone();
    if let DurableRecord::Operation { operation, .. } = &mut bad_output {
        if let DurableOperationKind::ToolPhaseOutput {
            output,
            artifact_ref,
            ..
        } = &mut operation.kind
        {
            *output = None;
            *artifact_ref = None;
        }
    }
    assert!(ToolPhaseLedger::from_records(&[
        pending[0].clone(),
        pending[1].clone(),
        pending[2].clone(),
        pending[3].clone(),
        bad_output,
    ])
    .is_err());

    let mut exact_input = pending;
    if let DurableRecord::Operation { operation, .. } = &mut exact_input[2] {
        if let DurableOperationKind::ToolPhaseIntent { input_redacted, .. } = &mut operation.kind {
            *input_redacted = "x".repeat(slim_core::session::MAX_TOOL_INLINE_BYTES);
        }
    }
    assert!(ToolPhaseLedger::from_records(&exact_input).is_ok());
}

#[test]
fn phase_order_and_correlation_fail_closed() {
    let mut output_before_started = records();
    let output = output_before_started.remove(4);
    let started = output_before_started.remove(3);
    let finished = output_before_started.remove(3);
    let terminal = output_before_started.remove(3);
    let mut output = output;
    let mut started = started;
    let mut finished = finished;
    let mut terminal = terminal;
    for (record, seq) in [
        (&mut output, 4),
        (&mut started, 5),
        (&mut finished, 6),
        (&mut terminal, 7),
    ] {
        match record {
            DurableRecord::Operation {
                seq: record_seq, ..
            } => *record_seq = seq,
            _ => unreachable!(),
        }
        output_before_started.push(record.clone());
    }
    assert!(ToolPhaseLedger::from_records(&output_before_started).is_err());

    let mut missing_start = records();
    missing_start.remove(1);
    assert!(ToolPhaseLedger::from_records(&missing_start).is_err());

    let mut duplicate_finish = records();
    let terminal = duplicate_finish.pop().expect("terminal");
    let mut duplicate = duplicate_finish[5].clone();
    if let DurableRecord::Operation { seq, .. } = &mut duplicate {
        *seq = 7;
    }
    duplicate_finish.push(duplicate);
    let mut terminal = terminal;
    if let DurableRecord::Operation { seq, .. } = &mut terminal {
        *seq = 8;
    }
    duplicate_finish.push(terminal);
    assert!(ToolPhaseLedger::from_records(&duplicate_finish).is_err());

    let mut terminal_with_incomplete_tool = records();
    terminal_with_incomplete_tool.remove(5);
    assert!(ToolPhaseLedger::from_records(&terminal_with_incomplete_tool).is_err());
}

#[test]
fn canonical_tool_phase_json_is_additive_and_explicit() {
    let kind = DurableOperationKind::ToolPhaseIntent {
        batch_id: "batch-1".into(),
        batch_index: 0,
        batch_limit: 1,
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        replay_policy: ReplayPolicy::Safe,
        input_redacted: "{}".into(),
    };
    assert_eq!(
        serde_json::to_value(kind).expect("tool phase json"),
        json!({
            "kind": "tool_phase_intent",
            "batch_id": "batch-1",
            "batch_index": 0,
            "batch_limit": 1,
            "tool_call_id": "call-1",
            "tool_name": "read",
            "replay_policy": "safe",
            "input_redacted": "{}"
        })
    );
}

#[test]
fn canonical_started_output_finished_json_has_stable_goldens() {
    let goldens = [
        (
            DurableOperationKind::ToolPhaseStarted {
                batch_id: "batch-1".into(),
                batch_index: 0,
                batch_limit: 1,
                tool_call_id: "call-1".into(),
            },
            json!({
                "kind": "tool_phase_started",
                "batch_id": "batch-1",
                "batch_index": 0,
                "batch_limit": 1,
                "tool_call_id": "call-1"
            }),
        ),
        (
            DurableOperationKind::ToolPhaseOutput {
                batch_id: "batch-1".into(),
                batch_index: 0,
                batch_limit: 1,
                tool_call_id: "call-1".into(),
                output: Some("ok".into()),
                artifact_ref: None,
            },
            json!({
                "kind": "tool_phase_output",
                "batch_id": "batch-1",
                "batch_index": 0,
                "batch_limit": 1,
                "tool_call_id": "call-1",
                "output": "ok",
                "artifact_ref": null
            }),
        ),
        (
            DurableOperationKind::ToolPhaseFinished {
                batch_id: "batch-1".into(),
                batch_index: 0,
                batch_limit: 1,
                tool_call_id: "call-1".into(),
                outcome: DurableOutcome::Success,
            },
            json!({
                "kind": "tool_phase_finished",
                "batch_id": "batch-1",
                "batch_index": 0,
                "batch_limit": 1,
                "tool_call_id": "call-1",
                "outcome": "success"
            }),
        ),
    ];
    for (kind, golden) in goldens {
        assert_eq!(serde_json::to_value(kind).expect("phase json"), golden);
    }
}

#[test]
fn completed_batch_restore_in_memory_and_jsonl_has_no_pending_replay() {
    let durable_records = records();
    let header = DurableSessionHeader::new("session-tool", "now", "D:\\Slim", None, None);

    let mut memory = MemoryRepo::new(header.clone());
    for record in &durable_records {
        memory.append(record.clone()).expect("memory append");
    }
    assert!(ReplayPlan::from_records(memory.records())
        .expect("memory restore")
        .pending()
        .is_empty());

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path: PathBuf = std::env::temp_dir().join(format!(
        "slim-tool-phases-{}-{nanos}.jsonl",
        std::process::id()
    ));
    let mut jsonl = JsonlRepo::create(&path, header).expect("jsonl create");
    for record in &durable_records {
        jsonl.append(record.clone()).expect("jsonl append");
    }
    drop(jsonl);
    let reopened = JsonlRepo::open(&path).expect("jsonl reopen");
    assert!(ReplayPlan::from_records(reopened.records())
        .expect("jsonl restore")
        .pending()
        .is_empty());
    drop(reopened);
    std::fs::remove_file(path).expect("remove test file");
}

#[test]
fn repositories_reject_invalid_tool_phase_before_state_or_bytes_change() {
    let prefix = records()[..2].to_vec();
    let mut invalid_candidates = Vec::new();

    let mut limit_zero = records()[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut limit_zero {
        if let DurableOperationKind::ToolPhaseIntent { batch_limit, .. } = &mut operation.kind {
            *batch_limit = 0;
        }
    }
    invalid_candidates.push(limit_zero);

    let mut limit_too_large = records()[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut limit_too_large {
        if let DurableOperationKind::ToolPhaseIntent { batch_limit, .. } = &mut operation.kind {
            *batch_limit = 33;
        }
    }
    invalid_candidates.push(limit_too_large);

    let mut oversized = records()[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut oversized {
        if let DurableOperationKind::ToolPhaseIntent { input_redacted, .. } = &mut operation.kind {
            *input_redacted = "x".repeat(slim_core::session::MAX_TOOL_INLINE_BYTES + 1);
        }
    }
    invalid_candidates.push(oversized);

    let mut phase_invalid = records()[4].clone();
    if let DurableRecord::Operation { seq, .. } = &mut phase_invalid {
        *seq = 3;
    }
    invalid_candidates.push(phase_invalid);

    invalid_candidates.push(DurableRecord::Operation {
        seq: 3,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ToolIntent {
                tool_call_id: "legacy-call".into(),
                tool_name: "read".into(),
                replay_policy: ReplayPolicy::Safe,
            },
        },
    });

    let mut oversized_artifact = records()[4].clone();
    if let DurableRecord::Operation { operation, .. } = &mut oversized_artifact {
        if let DurableOperationKind::ToolPhaseOutput {
            output,
            artifact_ref,
            ..
        } = &mut operation.kind
        {
            *output = None;
            *artifact_ref = Some("a".repeat(slim_core::session::MAX_TOOL_METADATA_BYTES + 1));
        }
    }
    invalid_candidates.push(oversized_artifact);

    let mut oversized_tool_call_id = records()[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut oversized_tool_call_id {
        if let DurableOperationKind::ToolPhaseIntent { tool_call_id, .. } = &mut operation.kind {
            *tool_call_id = "c".repeat(slim_core::session::MAX_TOOL_METADATA_BYTES + 1);
        }
    }
    invalid_candidates.push(oversized_tool_call_id);

    let mut oversized_tool_name = records()[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut oversized_tool_name {
        if let DurableOperationKind::ToolPhaseIntent { tool_name, .. } = &mut operation.kind {
            *tool_name = "t".repeat(slim_core::session::MAX_TOOL_METADATA_BYTES + 1);
        }
    }
    invalid_candidates.push(oversized_tool_name);

    for (index, candidate) in invalid_candidates.into_iter().enumerate() {
        let header = DurableSessionHeader::new(
            format!("session-tool-invalid-{index}"),
            "now",
            "D:\\Slim",
            None,
            None,
        );
        let mut memory = MemoryRepo::new(header.clone());
        for record in &prefix {
            memory.append(record.clone()).expect("memory prefix append");
        }
        let before_state = memory.records().to_vec();
        assert!(memory.append(candidate.clone()).is_err());
        assert_eq!(memory.records(), before_state.as_slice());

        let path = std::env::temp_dir().join(format!(
            "slim-tool-phases-invalid-{}-{index}.jsonl",
            std::process::id()
        ));
        let mut jsonl = JsonlRepo::create(&path, header).expect("jsonl create");
        for record in &prefix {
            jsonl.append(record.clone()).expect("jsonl prefix append");
        }
        let before_bytes = std::fs::read(&path).expect("read before invalid append");
        let before_records = jsonl.records().to_vec();
        assert!(jsonl.append(candidate).is_err());
        assert_eq!(jsonl.records(), before_records.as_slice());
        drop(jsonl);
        assert_eq!(
            std::fs::read(&path).expect("read after invalid append"),
            before_bytes
        );
        std::fs::remove_file(path).expect("remove invalid test file");
    }

    let mut no_output = records()[5].clone();
    if let DurableRecord::Operation { seq, .. } = &mut no_output {
        *seq = 5;
    }
    let mut memory = MemoryRepo::new(DurableSessionHeader::new(
        "session-tool-no-output",
        "now",
        "D:\\Slim",
        None,
        None,
    ));
    for record in &records()[..4] {
        memory.append(record.clone()).expect("no-output prefix");
    }
    let before_state = memory.records().to_vec();
    assert!(memory.append(no_output.clone()).is_err());
    assert_eq!(memory.records(), before_state.as_slice());

    let path = std::env::temp_dir().join(format!(
        "slim-tool-phases-no-output-{}-{}.jsonl",
        std::process::id(),
        "append"
    ));
    let mut jsonl = JsonlRepo::create(
        &path,
        DurableSessionHeader::new(
            "session-tool-no-output-jsonl",
            "now",
            "D:\\Slim",
            None,
            None,
        ),
    )
    .expect("no-output jsonl create");
    for record in &records()[..4] {
        jsonl
            .append(record.clone())
            .expect("no-output jsonl prefix");
    }
    let before_bytes = std::fs::read(&path).expect("no-output bytes before");
    let before_records = jsonl.records().to_vec();
    assert!(jsonl.append(no_output).is_err());
    assert_eq!(jsonl.records(), before_records.as_slice());
    drop(jsonl);
    assert_eq!(
        std::fs::read(&path).expect("no-output bytes after"),
        before_bytes
    );
    std::fs::remove_file(path).expect("remove no-output test file");

    let mut duplicate = records()[2].clone();
    if let DurableRecord::Operation { seq, .. } = &mut duplicate {
        *seq = 4;
    }
    let mut memory = MemoryRepo::new(DurableSessionHeader::new(
        "session-tool-duplicate",
        "now",
        "D:\\Slim",
        None,
        None,
    ));
    for record in &records()[..3] {
        memory.append(record.clone()).expect("duplicate prefix");
    }
    let before_state = memory.records().to_vec();
    assert!(memory.append(duplicate.clone()).is_err());
    assert_eq!(memory.records(), before_state.as_slice());

    let path = std::env::temp_dir().join(format!(
        "slim-tool-phases-duplicate-{}-{}.jsonl",
        std::process::id(),
        "append"
    ));
    let mut jsonl = JsonlRepo::create(
        &path,
        DurableSessionHeader::new(
            "session-tool-duplicate-jsonl",
            "now",
            "D:\\Slim",
            None,
            None,
        ),
    )
    .expect("duplicate jsonl create");
    for record in &records()[..3] {
        jsonl
            .append(record.clone())
            .expect("duplicate jsonl prefix");
    }
    let before_bytes = std::fs::read(&path).expect("duplicate bytes before");
    let before_records = jsonl.records().to_vec();
    assert!(jsonl.append(duplicate).is_err());
    assert_eq!(jsonl.records(), before_records.as_slice());
    drop(jsonl);
    assert_eq!(
        std::fs::read(&path).expect("duplicate bytes after"),
        before_bytes
    );
    std::fs::remove_file(path).expect("remove duplicate test file");
}

#[test]
fn tool_phase_finished_requires_a_validated_output() {
    let mut no_output = records()[..4].to_vec();
    let mut finished = records()[5].clone();
    if let DurableRecord::Operation { seq, .. } = &mut finished {
        *seq = 5;
    }
    no_output.push(finished);
    assert!(ToolPhaseLedger::from_records(&no_output).is_err());
}

#[test]
fn tool_metadata_and_artifact_references_are_bounded() {
    let too_large = slim_core::session::MAX_TOOL_METADATA_BYTES + 1;

    let mut batch_id = records()[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut batch_id {
        if let DurableOperationKind::ToolPhaseIntent { batch_id, .. } = &mut operation.kind {
            *batch_id = "b".repeat(too_large);
        }
    }
    assert!(ToolPhaseLedger::from_records(
        &[records()[0].clone(), records()[1].clone(), batch_id,]
    )
    .is_err());

    let mut tool_name = records()[2].clone();
    if let DurableRecord::Operation { operation, .. } = &mut tool_name {
        if let DurableOperationKind::ToolPhaseIntent { tool_name, .. } = &mut operation.kind {
            *tool_name = "t".repeat(too_large);
        }
    }
    assert!(ToolPhaseLedger::from_records(&[
        records()[0].clone(),
        records()[1].clone(),
        tool_name,
    ])
    .is_err());

    let mut artifact = records()[4].clone();
    if let DurableRecord::Operation { operation, .. } = &mut artifact {
        if let DurableOperationKind::ToolPhaseOutput {
            artifact_ref,
            output,
            ..
        } = &mut operation.kind
        {
            *output = None;
            *artifact_ref = Some("a".repeat(too_large));
        }
    }
    assert!(ToolPhaseLedger::from_records(&[
        records()[0].clone(),
        records()[1].clone(),
        records()[2].clone(),
        records()[3].clone(),
        artifact,
    ])
    .is_err());
}

#[test]
fn legacy_tool_variants_fail_closed_in_the_stage6_ledger() {
    let mut legacy_only = records()[..2].to_vec();
    legacy_only.push(DurableRecord::Operation {
        seq: 3,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ToolIntent {
                tool_call_id: "legacy-call".into(),
                tool_name: "read".into(),
                replay_policy: ReplayPolicy::Safe,
            },
        },
    });
    assert!(ToolPhaseLedger::from_records(&legacy_only).is_err());

    let mut mixed = records();
    let terminal = mixed.pop().expect("terminal");
    mixed.push(DurableRecord::Operation {
        seq: 7,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ToolFinished {
                tool_call_id: "legacy-call".into(),
                outcome: DurableOutcome::Success,
            },
        },
    });
    let mut terminal = terminal;
    if let DurableRecord::Operation { seq, .. } = &mut terminal {
        *seq = 8;
    }
    mixed.push(terminal);
    assert!(ToolPhaseLedger::from_records(&mixed).is_err());
}

#[test]
fn provider_attempt_effect_is_not_replayed_after_tool_intent() {
    let mut provider_then_tool = records()[..2].to_vec();
    provider_then_tool.push(DurableRecord::Operation {
        seq: 3,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-1".into(),
                ordinal: 1,
            },
        },
    });
    let mut intent = records()[2].clone();
    if let DurableRecord::Operation { seq, .. } = &mut intent {
        *seq = 4;
    }
    provider_then_tool.push(intent);

    assert!(ToolPhaseLedger::from_records(&provider_then_tool).is_err());
    assert!(
        slim_core::session::planned_provider_effects(&provider_then_tool)
            .expect("effect plan")
            .is_empty()
    );

    let mut legacy_provider = provider_then_tool;
    if let DurableRecord::Operation { operation, .. } = &mut legacy_provider[3] {
        operation.kind = DurableOperationKind::ToolIntent {
            tool_call_id: "legacy-call".into(),
            tool_name: "read".into(),
            replay_policy: ReplayPolicy::Safe,
        };
    }
    assert!(
        slim_core::session::planned_provider_effects(&legacy_provider)
            .expect("legacy effect plan")
            .is_empty()
    );

    let mut provider_after_tool = records()[..3].to_vec();
    provider_after_tool.push(DurableRecord::Operation {
        seq: 4,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ProviderAttemptStarted {
                attempt_id: "attempt-after-tool".into(),
                ordinal: 1,
            },
        },
    });
    assert!(ToolPhaseLedger::from_records(&provider_after_tool).is_err());
}

#[test]
fn jsonl_open_rejects_tampered_tool_prefix_before_repair_or_mutation() {
    let mut no_output = records()[..4].to_vec();
    let mut finished = records()[5].clone();
    if let DurableRecord::Operation { seq, .. } = &mut finished {
        *seq = 5;
    }
    no_output.push(finished);

    let mut oversized = records()[..3].to_vec();
    if let DurableRecord::Operation { operation, .. } = &mut oversized[2] {
        if let DurableOperationKind::ToolPhaseIntent { input_redacted, .. } = &mut operation.kind {
            *input_redacted = "x".repeat(slim_core::session::MAX_TOOL_INLINE_BYTES + 1);
        }
    }

    let mut legacy = records()[..2].to_vec();
    legacy.push(DurableRecord::Operation {
        seq: 3,
        operation: DurableOperation {
            operation_id: "op-1".into(),
            kind: DurableOperationKind::ToolIntent {
                tool_call_id: "legacy-call".into(),
                tool_name: "read".into(),
                replay_policy: ReplayPolicy::Safe,
            },
        },
    });

    for (index, durable_records) in [no_output, oversized, legacy].into_iter().enumerate() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slim-tool-phases-tampered-{}-{index}-{nanos}.jsonl",
            std::process::id()
        ));
        let header = DurableSessionHeader::new(
            format!("session-tool-tampered-{index}"),
            "now",
            "D:\\Slim",
            None,
            None,
        );
        let mut bytes = format!("{}\n", serde_json::to_string(&header).expect("header json"));
        for record in durable_records {
            bytes.push_str(&serde_json::to_string(&record).expect("record json"));
            bytes.push('\n');
        }
        std::fs::write(&path, bytes).expect("write tampered fixture");
        let before = std::fs::read(&path).expect("read tampered fixture");
        let error = match JsonlRepo::open(&path) {
            Ok(_) => panic!("tampered tool prefix must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read(&path).expect("read unchanged fixture"),
            before
        );
        std::fs::remove_file(path).expect("remove tampered fixture");
    }
}

#[test]
fn jsonl_open_rejects_oversized_sparse_file_before_read_or_mutation() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "slim-tool-phases-oversized-{}-{nanos}.jsonl",
        std::process::id()
    ));
    let oversized_len = MAX_DURABLE_SESSION_BYTES + 1;
    let file = std::fs::File::create(&path).expect("create sparse fixture");
    file.set_len(oversized_len).expect("sparsify fixture");
    drop(file);
    let before_len = std::fs::metadata(&path).expect("metadata before").len();

    let error = match JsonlRepo::open(&path) {
        Ok(_) => panic!("oversized session must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::metadata(&path).expect("metadata after").len(),
        before_len
    );
    assert!(!path.with_extension("jsonl.quarantine").exists());

    let _ = std::fs::remove_file(path.with_extension("jsonl.lock"));
    std::fs::remove_file(path).expect("remove sparse fixture");
}
