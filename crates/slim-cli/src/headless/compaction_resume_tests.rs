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
            reason: CompactionReason::HardThreshold,
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
                reason: CompactionReason::HardThreshold,
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
            let chained_prefix = [
                ProviderMessage::user("root instruction"),
                ProviderMessage::user("[Compacted context]\n## Goal\nContinue"),
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
                    reason: CompactionReason::HardThreshold,
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
                    reason: CompactionReason::HardThreshold,
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
        } = durable_provider_history(&preflight).expect("history");
        assert_eq!(parent.as_deref(), Some("kept-2"));
        if fingerprint_matches {
            assert_eq!(history.len(), 4);
            assert_eq!(history[0].content, "root instruction");
            assert!(history[1].content.contains("Second"));
            assert_eq!(history[2].content, "recent question");
            assert_eq!(history[3].content, "recent answer");
            assert_eq!(
                ids,
                vec![
                    Some("root".into()),
                    None,
                    Some("kept".into()),
                    Some("kept-2".into())
                ]
            );
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
