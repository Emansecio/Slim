use super::{durable_provider_history, DurableProviderHistory};
use slim_core::context::{compaction_prefix_fingerprint, CompactionReason};
use slim_core::session::{
    preflight_session, CompactionCheckpoint, DurableEntry, DurableEntryRole, DurableRecord,
    DurableRepo, DurableSessionHeader, JsonlRepo,
};
use slim_core::ProviderMessage;

#[test]
fn checkpoint_cannot_separate_a_tool_result_from_its_call() {
    let path = fixture_path("tool-boundary");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
    )
    .unwrap();
    let messages = vec![
        ProviderMessage::user("inspect"),
        ProviderMessage::assistant(
            "",
            vec![slim_core::provider::ProviderToolCall {
                id: "read-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
        ),
        ProviderMessage::tool("read", "read-1", "contents"),
        ProviderMessage::assistant("done", vec![]),
    ];
    for (index, message) in messages.iter().cloned().enumerate() {
        repo.append(DurableRecord::Entry {
            seq: index as u64,
            entry: DurableEntry::from_provider_message(
                index.to_string(),
                index.checked_sub(1).map(|i| i.to_string()),
                "op".into(),
                message,
            )
            .unwrap(),
        })
        .unwrap();
    }
    repo.append(DurableRecord::Compaction {
        seq: 4,
        checkpoint: CompactionCheckpoint {
            checkpoint_id: "unsafe".into(),
            summary: "summary".into(),
            first_kept_entry_id: "2".into(),
            prefix_fingerprint: compaction_prefix_fingerprint(&messages[..2]),
            previous_checkpoint_id: None,
            tokens_before: 100,
            tokens_after: 10,
            input_tokens: None,
            output_tokens: None,
            duration_ms: 0,
            reason: CompactionReason::Threshold,
            read_files: vec![],
            modified_files: vec![],
        },
    })
    .unwrap();
    let restored =
        durable_provider_history(&super::SessionPreflight::from_open_repo(&repo)).unwrap();
    assert_eq!(restored.messages, messages);
    assert_eq!(restored.applied_checkpoint_id, None);
    let ids = (0..4).map(|i| Some(i.to_string())).collect::<Vec<_>>();
    assert_eq!(
        super::durable_checkpoint_anchor(
            &messages,
            &ids,
            2,
            &compaction_prefix_fingerprint(&messages[..2])
        ),
        None
    );
    assert_eq!(
        super::durable_checkpoint_anchor(
            &messages,
            &ids,
            3,
            &compaction_prefix_fingerprint(&messages[..3])
        ),
        Some("3".into())
    );
    drop(repo);
    std::fs::remove_file(&path).unwrap();
    let _ = std::fs::remove_file(path.with_extension("jsonl.lock"));
}

fn fixture_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "slim-compaction-resume-{label}-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

#[test]
fn durable_history_uses_only_a_matching_checkpoint_prefix() {
    for (label, fingerprint_matches) in [("valid", true), ("stale", false)] {
        let path = fixture_path(label);
        let mut repo = JsonlRepo::create(
            &path,
            DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
        )
        .expect("create");
        let entries = [
            ("root", DurableEntryRole::User, "root instruction", None),
            (
                "old",
                DurableEntryRole::Assistant,
                "old answer",
                Some("root"),
            ),
            (
                "kept",
                DurableEntryRole::User,
                "recent question",
                Some("old"),
            ),
        ];
        for (seq, (id, role, content, parent)) in entries.into_iter().enumerate() {
            repo.append(DurableRecord::Entry {
                seq: seq as u64,
                entry: DurableEntry {
                    entry_id: id.into(),
                    role,
                    content: content.into(),
                    parent_entry_id: parent.map(str::to_owned),
                    operation_id: format!("op-{id}"),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    content_blocks: Vec::new(),
                },
            })
            .expect("append entry");
        }
        let prefix = [
            ProviderMessage::user("root instruction"),
            ProviderMessage::assistant("old answer", Vec::new()),
        ];
        repo.append(DurableRecord::Compaction {
            seq: 3,
            checkpoint: CompactionCheckpoint {
                checkpoint_id: "compact-1".into(),
                summary: "## Goal\nContinue".into(),
                first_kept_entry_id: "kept".into(),
                prefix_fingerprint: if fingerprint_matches {
                    compaction_prefix_fingerprint(&prefix)
                } else {
                    "0000000000000000".into()
                },
                previous_checkpoint_id: None,
                tokens_before: 100,
                tokens_after: 20,
                input_tokens: Some(5),
                output_tokens: Some(2),
                duration_ms: 1,
                reason: CompactionReason::Threshold,
                read_files: vec![],
                modified_files: vec![],
            },
        })
        .expect("append checkpoint");
        repo.append(DurableRecord::Entry {
            seq: 4,
            entry: DurableEntry {
                entry_id: "kept-2".into(),
                role: DurableEntryRole::Assistant,
                content: "recent answer".into(),
                parent_entry_id: Some("kept".into()),
                operation_id: "op-kept-2".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        })
        .expect("append second kept entry");
        if fingerprint_matches {
            // What the model saw after the first checkpoint: its summary, then
            // the messages from the first kept entry on.
            let chained_prefix = [
                slim_core::context::compaction_summary_message("## Goal\nContinue"),
                ProviderMessage::user("recent question"),
            ];
            repo.append(DurableRecord::Compaction {
                seq: 5,
                checkpoint: CompactionCheckpoint {
                    checkpoint_id: "compact-2".into(),
                    summary: "## Goal\nSecond".into(),
                    first_kept_entry_id: "kept-2".into(),
                    prefix_fingerprint: compaction_prefix_fingerprint(&chained_prefix),
                    previous_checkpoint_id: Some("compact-1".into()),
                    tokens_before: 80,
                    tokens_after: 15,
                    input_tokens: Some(4),
                    output_tokens: Some(2),
                    duration_ms: 1,
                    reason: CompactionReason::Threshold,
                    read_files: vec![],
                    modified_files: vec![],
                },
            })
            .expect("append chained checkpoint");
            repo.append(DurableRecord::Compaction {
                seq: 6,
                checkpoint: CompactionCheckpoint {
                    checkpoint_id: "compact-invalid".into(),
                    summary: "ignored".into(),
                    first_kept_entry_id: "kept-2".into(),
                    prefix_fingerprint: "0000000000000000".into(),
                    previous_checkpoint_id: Some("compact-2".into()),
                    tokens_before: 15,
                    tokens_after: 10,
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    duration_ms: 1,
                    reason: CompactionReason::Threshold,
                    read_files: vec![],
                    modified_files: vec![],
                },
            })
            .expect("append invalid checkpoint");
        }
        drop(repo);

        let preflight = preflight_session(&path).expect("preflight");
        let DurableProviderHistory {
            messages: history,
            parent_entry_id: parent,
            entry_ids: ids,
            applied_checkpoint_id,
            ..
        } = durable_provider_history(&preflight).expect("history");
        assert_eq!(parent.as_deref(), Some("kept-2"));
        if fingerprint_matches {
            // The latest checkpoint's summary stands in for everything before its
            // first kept entry: no root instruction, no pinned message.
            assert_eq!(history.len(), 2);
            assert_eq!(
                slim_core::context::compaction_summary_text(&history[0]),
                Some("## Goal\nSecond")
            );
            assert_eq!(history[1].content, "recent answer");
            assert_eq!(ids, vec![None, Some("kept-2".into())]);
            assert_eq!(applied_checkpoint_id.as_deref(), Some("compact-2"));
        } else {
            assert_eq!(history.len(), 4);
            assert_eq!(history[1].content, "old answer");
            assert_eq!(applied_checkpoint_id, None);
        }
        std::fs::remove_file(&path).expect("cleanup data");
        let lock = path.with_extension("jsonl.lock");
        if lock.exists() {
            std::fs::remove_file(lock).expect("cleanup lock");
        }
    }
}

/// A checkpoint exactly as the writer before the Pi port stored it: a
/// seven-heading summary with runtime facts, and the `soft_threshold` reason.
#[test]
fn a_checkpoint_from_the_old_writer_loads_and_seeds_the_next_compaction() {
    let path = fixture_path("old-writer");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    let entries = [
        ("root", DurableEntryRole::User, "root instruction", None),
        (
            "old",
            DurableEntryRole::Assistant,
            "old answer",
            Some("root"),
        ),
        (
            "kept",
            DurableEntryRole::User,
            "recent question",
            Some("old"),
        ),
        (
            "kept-answer",
            DurableEntryRole::Assistant,
            "recent answer",
            Some("kept"),
        ),
        (
            "next",
            DurableEntryRole::User,
            "newer question",
            Some("kept-answer"),
        ),
        (
            "next-answer",
            DurableEntryRole::Assistant,
            "newer answer",
            Some("next"),
        ),
    ];
    for (seq, (id, role, content, parent)) in entries.into_iter().enumerate() {
        let entry = DurableEntry {
            entry_id: id.into(),
            role,
            content: content.into(),
            parent_entry_id: parent.map(str::to_owned),
            operation_id: format!("op-{id}"),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        };
        // The checkpoint record sits between the second and third entries.
        let seq = if seq < 2 { seq } else { seq + 1 };
        repo.append(DurableRecord::Entry {
            seq: seq as u64,
            entry,
        })
        .expect("append entry");
        if seq == 1 {
            let old_summary = "## Goal\\nShip the port\\n## Constraints\\nOffline\\n## Progress\\nDone\\n## Blocked\\nNone\\n## Decisions\\nKeep it small\\n## Next steps\\nVerify\\n## Critical context\\nSee runtime facts\\n\\n## Runtime facts\\nvalidation: passed";
            let fingerprint = compaction_prefix_fingerprint(&[
                ProviderMessage::user("root instruction"),
                ProviderMessage::assistant("old answer", Vec::new()),
            ]);
            let line = format!(
                "{{\"type\":\"compaction\",\"seq\":2,\"checkpoint\":{{\"checkpoint_id\":\"compact-old-2\",\"summary\":\"{old_summary}\",\"first_kept_entry_id\":\"kept\",\"prefix_fingerprint\":\"{fingerprint}\",\"previous_checkpoint_id\":null,\"tokens_before\":9000,\"tokens_after\":900,\"input_tokens\":5,\"output_tokens\":2,\"duration_ms\":1,\"reason\":\"soft_threshold\",\"read_files\":[],\"modified_files\":[]}}}}"
            );
            let record: DurableRecord = serde_json::from_str(&line).expect("old-writer record");
            repo.append(record).expect("append old-writer checkpoint");
        }
    }
    drop(repo);

    let preflight = preflight_session(&path).expect("preflight");
    let restored = durable_provider_history(&preflight).expect("history");
    assert_eq!(
        restored.applied_checkpoint_id.as_deref(),
        Some("compact-old-2")
    );
    assert_eq!(restored.messages.len(), 5);
    let summary = slim_core::context::compaction_summary_text(&restored.messages[0])
        .expect("the old summary is wrapped as a Pi summary message");
    assert!(summary.starts_with("## Goal\nShip the port"));
    assert!(summary.contains("## Runtime facts"));
    assert_eq!(restored.messages[1].content, "recent question");
    assert_eq!(restored.entry_ids[0], None);

    // The next compaction starts from the old summary (update prompt), with
    // the kept messages inside its span.
    let preparation = slim_core::context::prepare_compaction(
        &restored.messages,
        &slim_core::context::CompactionSettings {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 1,
        },
        slim_core::context::ContextUsage::default(),
    )
    .expect("something to compact");
    assert_eq!(preparation.previous_summary.as_deref(), Some(summary));
    assert!(preparation.first_kept_index > 1);

    std::fs::remove_file(&path).expect("cleanup data");
    let lock = path.with_extension("jsonl.lock");
    if lock.exists() {
        std::fs::remove_file(lock).expect("cleanup lock");
    }
}

/// The file lists a checkpoint carries survive a resume through the summary
/// text and seed the next compaction's cumulative lists.
#[test]
fn file_lists_in_a_checkpoint_summary_seed_the_next_compaction() {
    let summary = slim_core::context::fit_summary_for_persistence(
        "## Goal\nEdit files",
        &{
            let mut ops = slim_core::context::FileOperations::new();
            ops.extract_from_message(&ProviderMessage::assistant(
                "",
                vec![
                    slim_core::provider::ProviderToolCall {
                        id: "r".into(),
                        name: "read".into(),
                        arguments: "{\"path\":\"src/a.rs\"}".into(),
                    },
                    slim_core::provider::ProviderToolCall {
                        id: "w".into(),
                        name: "write".into(),
                        arguments: "{\"path\":\"src/b.rs\"}".into(),
                    },
                ],
            ));
            ops
        },
        64 * 1024,
    )
    .expect("fits");
    assert_eq!(summary.files.read_files, vec!["src/a.rs"]);
    assert_eq!(summary.files.modified_files, vec!["src/b.rs"]);

    let history = vec![
        slim_core::context::compaction_summary_message(&summary.summary),
        ProviderMessage::user("next question"),
        ProviderMessage::assistant("next answer", Vec::new()),
    ];
    let preparation = slim_core::context::prepare_compaction(
        &history,
        &slim_core::context::CompactionSettings {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 1,
        },
        slim_core::context::ContextUsage::default(),
    )
    .expect("something to compact");
    let lists = slim_core::context::compute_file_lists(&preparation.file_ops);
    assert_eq!(lists.read_files, vec!["src/a.rs"]);
    assert_eq!(lists.modified_files, vec!["src/b.rs"]);
}

/// Two checkpoints from the writer before the Pi port. The second one was
/// fingerprinted over that writer's live view (the first user message, a
/// `[Compacted context]` message, the pinned instruction, then what the first
/// checkpoint kept), which the Pi-layout rebuild does not reproduce. A
/// checkpoint written after the upgrade chains onto them, and one that matches
/// nothing is reported instead of skipped silently.
#[test]
fn a_chain_of_old_writer_checkpoints_loads_and_later_checkpoints_chain_onto_it() {
    let path = fixture_path("old-writer-chain");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    let raw = [
        ProviderMessage::user("root instruction"),
        ProviderMessage::assistant("first answer", Vec::new()),
        ProviderMessage::user("second question"),
        ProviderMessage::assistant("second answer", Vec::new()),
        ProviderMessage::user("third question"),
        ProviderMessage::assistant("third answer", Vec::new()),
        ProviderMessage::user("latest question"),
        ProviderMessage::assistant("latest answer", Vec::new()),
    ];
    for (index, message) in raw.iter().cloned().enumerate() {
        repo.append(DurableRecord::Entry {
            seq: index as u64,
            entry: DurableEntry::from_provider_message(
                format!("e{index}"),
                index.checked_sub(1).map(|i| format!("e{i}")),
                "op".into(),
                message,
            )
            .expect("entry"),
        })
        .expect("append entry");
    }
    let old_line = |seq: u64,
                    id: &str,
                    summary: &str,
                    anchor: &str,
                    fingerprint: &str,
                    previous: &str| {
        format!(
            "{{\"type\":\"compaction\",\"seq\":{seq},\"checkpoint\":{{\"checkpoint_id\":\"{id}\",\"summary\":\"{summary}\",\"first_kept_entry_id\":\"{anchor}\",\"prefix_fingerprint\":\"{fingerprint}\",\"previous_checkpoint_id\":{previous},\"tokens_before\":9000,\"tokens_after\":900,\"input_tokens\":5,\"output_tokens\":2,\"duration_ms\":1,\"reason\":\"hard_threshold\",\"read_files\":[],\"modified_files\":[]}}}}"
        )
    };
    let append = |repo: &mut JsonlRepo, line: String| {
        let record: DurableRecord = serde_json::from_str(&line).expect("record");
        repo.append(record).expect("append checkpoint");
    };
    // The first checkpoint fingerprints the plain entries before its anchor.
    append(
        &mut repo,
        old_line(
            8,
            "old-1",
            "first old summary",
            "e4",
            &compaction_prefix_fingerprint(&raw[..4]),
            "null",
        ),
    );
    // What the old writer's live history held after it: the root, the
    // `[Compacted context]` message, the later user instruction pinned in
    // front of the kept messages, then everything from the anchor on.
    let old_view = [
        raw[0].clone(),
        ProviderMessage::user(
            "[Compacted context]
first old summary",
        ),
        raw[2].clone(),
        raw[4].clone(),
        raw[5].clone(),
        raw[6].clone(),
        raw[7].clone(),
    ];
    append(
        &mut repo,
        old_line(
            9,
            "old-2",
            "second old summary",
            "e6",
            &compaction_prefix_fingerprint(&old_view[..5]),
            "\"old-1\"",
        ),
    );
    // Written after the upgrade: fingerprinted over the Pi layout.
    let pi_view = [
        slim_core::context::compaction_summary_message("second old summary"),
        raw[6].clone(),
        raw[7].clone(),
    ];
    append(
        &mut repo,
        old_line(
            10,
            "new-3",
            "third summary",
            "e7",
            &compaction_prefix_fingerprint(&pi_view[..2]),
            "\"old-2\"",
        ),
    );
    // Matches no layout.
    append(
        &mut repo,
        old_line(
            11,
            "stale-4",
            "ignored",
            "e7",
            "0000000000000000",
            "\"new-3\"",
        ),
    );
    drop(repo);

    let preflight = preflight_session(&path).expect("preflight");
    let restored = durable_provider_history(&preflight).expect("history");
    assert_eq!(restored.applied_checkpoint_id.as_deref(), Some("new-3"));
    assert_eq!(restored.messages.len(), 2);
    assert_eq!(
        slim_core::context::compaction_summary_text(&restored.messages[0]),
        Some("third summary")
    );
    assert_eq!(restored.messages[1].content, "latest answer");
    assert_eq!(restored.entry_ids, vec![None, Some("e7".into())]);
    assert_eq!(restored.skipped_checkpoints.len(), 1);
    assert!(restored.skipped_checkpoints[0].contains("stale-4"));

    // Without the later checkpoints the old chain alone applies as well.
    let two = durable_provider_history(&{
        let mut cut = preflight;
        cut.records.truncate(raw.len() + 2);
        cut
    })
    .expect("history");
    assert_eq!(two.applied_checkpoint_id.as_deref(), Some("old-2"));
    assert_eq!(
        slim_core::context::compaction_summary_text(&two.messages[0]),
        Some("second old summary")
    );
    assert_eq!(two.messages[1].content, "latest question");
    assert!(two.skipped_checkpoints.is_empty());

    std::fs::remove_file(&path).expect("cleanup data");
    let lock = path.with_extension("jsonl.lock");
    if lock.exists() {
        std::fs::remove_file(lock).expect("cleanup lock");
    }
}

// The writer before the Pi port stored a fingerprint over the raw journal
// bytes. Tool-call arguments as the provider streamed them (spaces, unsorted
// keys) and JSON tool output hash differently once redacted and re-serialized,
// so such a checkpoint must still verify against the raw form.
#[test]
fn a_checkpoint_fingerprinted_over_raw_bytes_is_still_applied() {
    let path = fixture_path("raw-fingerprint");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
    )
    .unwrap();
    let messages = [
        ProviderMessage::user("inspect"),
        ProviderMessage::assistant(
            "",
            vec![slim_core::provider::ProviderToolCall {
                id: "read-1".into(),
                name: "read".into(),
                arguments: r#"{"start_line": 1, "path": "src/a.rs"}"#.into(),
            }],
        ),
        ProviderMessage::tool("read", "read-1", r#"{"b": 1, "a": 2}"#),
        ProviderMessage::assistant("done", vec![]),
        ProviderMessage::user("recent question"),
    ];
    for (index, message) in messages.iter().cloned().enumerate() {
        repo.append(DurableRecord::Entry {
            seq: index as u64,
            entry: DurableEntry::from_provider_message(
                index.to_string(),
                index.checked_sub(1).map(|i| i.to_string()),
                "op".into(),
                message,
            )
            .unwrap(),
        })
        .unwrap();
    }
    let raw = slim_core::context::legacy_prefix_fingerprint(&messages[..4]);
    assert_ne!(
        raw,
        compaction_prefix_fingerprint(&messages[..4]),
        "the fixture must hash differently once redacted"
    );
    repo.append(DurableRecord::Compaction {
        seq: 5,
        checkpoint: CompactionCheckpoint {
            checkpoint_id: "old-writer".into(),
            summary: "summary".into(),
            first_kept_entry_id: "4".into(),
            prefix_fingerprint: raw,
            previous_checkpoint_id: None,
            tokens_before: 100,
            tokens_after: 10,
            input_tokens: None,
            output_tokens: None,
            duration_ms: 0,
            reason: CompactionReason::Threshold,
            read_files: vec![],
            modified_files: vec![],
        },
    })
    .unwrap();
    let restored =
        durable_provider_history(&super::SessionPreflight::from_open_repo(&repo)).unwrap();
    assert_eq!(
        restored.applied_checkpoint_id.as_deref(),
        Some("old-writer")
    );
    assert!(restored.skipped_checkpoints.is_empty());
    assert_eq!(restored.messages.len(), 2);
    assert_eq!(restored.messages[1], messages[4]);
    drop(repo);
    std::fs::remove_file(&path).unwrap();
    let _ = std::fs::remove_file(path.with_extension("jsonl.lock"));
}

// The journal keeps a duplicate-result pointer as it was written. When the
// checkpoint summarizes the original output away, the rebuilt history must not
// still claim the identical output is in context.
#[test]
fn a_resumed_duplicate_pointer_whose_original_was_compacted_away_is_rewritten() {
    let path = fixture_path("stale-pointer");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
    )
    .unwrap();
    let call = |id: &str| {
        ProviderMessage::assistant(
            "",
            vec![slim_core::provider::ProviderToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: r#"{"path":"src/a.rs"}"#.into(),
            }],
        )
    };
    let pointer = "[duplicate read result omitted; identical output already in context]";
    let messages = [
        ProviderMessage::user("inspect"),
        call("read-1"),
        ProviderMessage::tool("read", "read-1", "full file output"),
        ProviderMessage::assistant("ok", vec![]),
        ProviderMessage::user("again"),
        call("read-2"),
        ProviderMessage::tool("read", "read-2", pointer),
        ProviderMessage::assistant("done", vec![]),
    ];
    for (index, message) in messages.iter().cloned().enumerate() {
        repo.append(DurableRecord::Entry {
            seq: index as u64,
            entry: DurableEntry::from_provider_message(
                index.to_string(),
                index.checked_sub(1).map(|i| i.to_string()),
                "op".into(),
                message,
            )
            .unwrap(),
        })
        .unwrap();
    }
    repo.append(DurableRecord::Compaction {
        seq: 8,
        checkpoint: CompactionCheckpoint {
            checkpoint_id: "cut".into(),
            summary: "summary".into(),
            first_kept_entry_id: "4".into(),
            prefix_fingerprint: compaction_prefix_fingerprint(&messages[..4]),
            previous_checkpoint_id: None,
            tokens_before: 100,
            tokens_after: 10,
            input_tokens: None,
            output_tokens: None,
            duration_ms: 0,
            reason: CompactionReason::Threshold,
            read_files: vec![],
            modified_files: vec![],
        },
    })
    .unwrap();
    let restored =
        durable_provider_history(&super::SessionPreflight::from_open_repo(&repo)).unwrap();
    assert_eq!(restored.applied_checkpoint_id.as_deref(), Some("cut"));
    let result = &restored.messages[3];
    assert_eq!(result.role, "tool");
    assert_ne!(result.content, pointer);
    assert!(result.content.contains("compacted away"));
    drop(repo);
    std::fs::remove_file(&path).unwrap();
    let _ = std::fs::remove_file(path.with_extension("jsonl.lock"));
}
