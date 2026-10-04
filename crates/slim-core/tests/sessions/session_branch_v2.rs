use slim_core::session::{
    branch_v2, create_durable_branch_compacted, DurableEntry, DurableEntryRole, DurableRecord,
    DurableRepo, DurableSessionHeader, JsonlRepo,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn path(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let nonce = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "slim-stage8-branch-{}-{stamp}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&directory).expect("test directory");
    directory.join(label)
}

fn cleanup(path: &Path) {
    fs::remove_dir_all(path.parent().expect("test directory")).expect("cleanup");
}

fn record(seq: u64) -> DurableRecord {
    DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: format!("entry-{seq}"),
            role: DurableEntryRole::User,
            content: format!("content-{seq}"),
            parent_entry_id: None,
            operation_id: format!("operation-{seq}"),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    }
}

fn parent(label: &str) -> PathBuf {
    let path = path(label);
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("parent", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    repo.append(record(10)).expect("append");
    repo.append(record(11)).expect("append");
    drop(repo);
    path
}

#[test]
fn branch_rejects_traversal_empty_and_out_of_range_cutoffs() {
    let path = parent("parent.jsonl");
    for child in ["", "..", ".", "../escape", "nested/child", "nested\\child"] {
        assert!(branch_v2(&path, child, 10).is_err(), "child={child:?}");
    }
    assert!(branch_v2(&path, "too-late", 12).is_err());
    assert!(branch_v2(&path, "too-early", 9).is_err());
    cleanup(&path);
}

#[test]
fn branch_rejects_cutoff_successor_overflow() {
    let path = path("max.jsonl");
    let header = DurableSessionHeader::new("max", "now", "D:\\Slim", None, None);
    let bytes = format!(
        "{}\n{}\n",
        serde_json::to_string(&header).expect("header"),
        serde_json::to_string(&record(u64::MAX)).expect("record")
    );
    fs::write(&path, bytes).expect("fixture");
    assert!(branch_v2(&path, "child", u64::MAX).is_err());
    cleanup(&path);
}

#[test]
fn branch_rejects_a_cutoff_that_is_only_a_sequence_gap() {
    let path = path("gap.jsonl");
    let header = DurableSessionHeader::new("gap", "now", "D:\\Slim", None, None);
    let bytes = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&header).expect("header"),
        serde_json::to_string(&record(0)).expect("record"),
        serde_json::to_string(&record(2)).expect("record")
    );
    fs::write(&path, bytes).expect("fixture");
    assert!(branch_v2(&path, "child", 1).is_err());
    cleanup(&path);
}

#[test]
fn async_branch_wrapper_appends_a_branch_compaction_checkpoint() {
    let path = path("compact-parent.jsonl");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("parent", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    for (seq, role, content) in [
        (0, DurableEntryRole::User, "root".to_owned()),
        (1, DurableEntryRole::Assistant, "x".repeat(100_000)),
        (2, DurableEntryRole::User, "recent".to_owned()),
    ] {
        repo.append(DurableRecord::Entry {
            seq,
            entry: DurableEntry {
                entry_id: format!("entry-{seq}"),
                role,
                content,
                parent_entry_id: None,
                operation_id: format!("operation-{seq}"),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        })
        .expect("append");
    }
    drop(repo);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let branch = runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child",
            2,
            |request| async move {
                // The cut splits the only turn: its prefix is what gets summarized.
                assert!(request.prompt.contains("# Conversation\n[User]: root"));
                Ok("## Goal\nContinue branch".to_owned())
            },
        ))
        .expect("branch");
    let child = JsonlRepo::open(&branch.path).expect("open child");
    assert!(matches!(
        child.records().last(),
        Some(DurableRecord::Compaction { checkpoint, .. })
            if checkpoint.reason == slim_core::context::CompactionReason::Branch
    ));
    drop(child);
    cleanup(&path);
}

#[test]
fn failed_compaction_does_not_publish_child_or_consume_its_id() {
    let path = path("compact-retry.jsonl");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("parent-retry", "now", "D:\\Slim", None, None),
    )
    .unwrap();
    for (seq, content) in [
        (0, "root".into()),
        (1, "x".repeat(100_000)),
        (2, "recent".into()),
    ] {
        repo.append(DurableRecord::Entry {
            seq,
            entry: DurableEntry {
                entry_id: format!("entry-{seq}"),
                role: DurableEntryRole::User,
                content,
                parent_entry_id: None,
                operation_id: format!("operation-{seq}"),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        })
        .unwrap();
    }
    drop(repo);
    let child = path.with_file_name("compact-retry-child.jsonl");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    assert!(runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child",
            2,
            |_| async { Err(std::io::Error::other("summary failed")) }
        ))
        .is_err());
    assert!(!child.exists());
    let branch = runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child",
            2,
            |_| async { Ok("## Goal\nRetry".into()) },
        ))
        .unwrap();
    assert!(child.exists());
    assert_eq!(branch.path.file_name(), child.file_name());
    assert_eq!(branch.next_seq, 4);
    cleanup(&path);
}

#[test]
fn branch_compaction_summarizes_the_latest_instruction_before_the_cut() {
    let path = path("compact-pinned.jsonl");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("parent-pinned", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    let mut entries = vec![
        (0, DurableEntryRole::User, "root instruction".to_owned()),
        (
            1,
            DurableEntryRole::Assistant,
            "closed work that must remain summarized".to_owned(),
        ),
        (
            2,
            DurableEntryRole::User,
            "latest instruction: preserve this exact constraint".to_owned(),
        ),
    ];
    for seq in 3..=9 {
        entries.push((
            seq,
            DurableEntryRole::Assistant,
            format!("recent work {seq} {}", "x".repeat(12_000)),
        ));
    }
    for (seq, role, content) in entries {
        repo.append(DurableRecord::Entry {
            seq,
            entry: DurableEntry {
                entry_id: format!("entry-{seq}"),
                role,
                content,
                parent_entry_id: None,
                operation_id: format!("operation-{seq}"),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        })
        .expect("append");
    }
    drop(repo);

    let seen_prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let prompt_capture = std::sync::Arc::clone(&seen_prompts);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child-pinned",
            9,
            move |request| {
                prompt_capture.lock().expect("capture").push(request.prompt);
                async { Ok("## Goal\nContinue with the exact constraint".to_owned()) }
            },
        ))
        .expect("branch");

    // The cut splits the turn that began at the latest instruction: one request
    // for the history before it, one for that turn's prefix.
    let prompts = seen_prompts.lock().expect("capture").clone();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("closed work that must remain summarized"));
    // Pi keeps nothing but the recent tail verbatim: an instruction before the
    // cut is summarized with the rest of the turn, not pinned.
    assert!(prompts[1].contains("latest instruction: preserve this exact constraint"));
    assert!(prompts
        .iter()
        .all(|prompt| !prompt.contains("recent work 9")));
    cleanup(&path);
}

#[test]
fn branch_compaction_uses_tool_presentation_fact_before_checkpoint_fingerprint() {
    let path = path("compact-projection.jsonl");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("parent-projection", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    let tool_call = slim_core::provider::ProviderToolCall {
        id: "same".into(),
        name: "read".into(),
        arguments: "{}".into(),
    };
    let messages = [
        (0, slim_core::provider::ProviderMessage::user("root")),
        (
            1,
            slim_core::provider::ProviderMessage::assistant("", vec![tool_call]),
        ),
        (
            2,
            slim_core::provider::ProviderMessage::tool("read", "same", "raw-capture"),
        ),
        (
            3,
            slim_core::provider::ProviderMessage::assistant(
                "recent-a ".to_owned() + &"x".repeat(40_000),
                Vec::new(),
            ),
        ),
        (
            4,
            slim_core::provider::ProviderMessage::assistant(
                "recent-b ".to_owned() + &"x".repeat(40_000),
                Vec::new(),
            ),
        ),
    ];
    for (seq, message) in messages {
        let entry = DurableEntry::from_provider_message(
            format!("entry-{seq}"),
            None,
            "operation".into(),
            message,
        )
        .expect("entry");
        repo.append(DurableRecord::Entry { seq, entry })
            .expect("append");
    }
    repo.append(DurableRecord::Fact {
        seq: 5,
        fact: slim_core::session::DurableFact {
            namespace: "tool.presentation.v1".into(),
            key: "entry-2".into(),
            value: serde_json::json!({
                "name": "read",
                "call_id": "same",
                "output": "shown-projection"
            }),
        },
    })
    .expect("projection fact");
    drop(repo);

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let seen_prompt = std::sync::Arc::new(std::sync::Mutex::new(None));
    let capture = std::sync::Arc::clone(&seen_prompt);
    runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child-projection",
            5,
            move |request| {
                *capture.lock().expect("capture") = Some(request.prompt);
                async { Ok("## Goal\nContinue".to_owned()) }
            },
        ))
        .expect("branch");
    let prompt = seen_prompt
        .lock()
        .expect("capture")
        .clone()
        .expect("summary prompt");
    assert!(prompt.contains("shown-projection"), "{prompt}");
    assert!(
        !prompt.contains("raw-capture"),
        "raw entry leaked: {prompt}"
    );
    cleanup(&path);
}

#[test]
fn branch_compaction_continues_from_the_parents_checkpoint_and_its_file_lists() {
    use slim_core::context::{compaction_prefix_fingerprint, CompactionReason};
    use slim_core::session::CompactionCheckpoint;
    use slim_core::ProviderMessage;

    let path = path("compact-continue.jsonl");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("parent-continue", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    let entry = |seq: u64, role, content: String| DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: format!("entry-{seq}"),
            role,
            content,
            parent_entry_id: None,
            operation_id: format!("operation-{seq}"),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    };
    repo.append(entry(0, DurableEntryRole::User, "root".into()))
        .expect("root");
    repo.append(entry(1, DurableEntryRole::Assistant, "first answer".into()))
        .expect("answer");
    repo.append(entry(2, DurableEntryRole::User, "recent".into()))
        .expect("recent");
    // The parent was compacted once: its summary carries a file list.
    let old_summary = "## Goal\nOld goal\n\n<read-files>\nsrc/a.rs\n</read-files>";
    repo.append(DurableRecord::Compaction {
        seq: 3,
        checkpoint: CompactionCheckpoint {
            checkpoint_id: "compact-parent-3".into(),
            summary: old_summary.into(),
            first_kept_entry_id: "entry-2".into(),
            prefix_fingerprint: compaction_prefix_fingerprint(&[
                ProviderMessage::user("root"),
                ProviderMessage::assistant("first answer", Vec::new()),
            ]),
            previous_checkpoint_id: None,
            tokens_before: 1_000,
            tokens_after: 100,
            input_tokens: None,
            output_tokens: None,
            duration_ms: 0,
            reason: CompactionReason::Threshold,
            read_files: vec!["src/a.rs".into()],
            modified_files: Vec::new(),
        },
    })
    .expect("checkpoint");
    repo.append(entry(4, DurableEntryRole::Assistant, "ok".into()))
        .expect("answer");
    repo.append(entry(5, DurableEntryRole::User, "next".into()))
        .expect("next");
    repo.append(entry(6, DurableEntryRole::Assistant, "x".repeat(100_000)))
        .expect("long answer");
    repo.append(entry(7, DurableEntryRole::User, "latest".into()))
        .expect("latest");
    drop(repo);

    let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = prompts.clone();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let branch = runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child",
            7,
            move |request| {
                seen.lock().unwrap().push(request.prompt);
                async move { Ok("## Goal\nNew goal".to_owned()) }
            },
        ))
        .expect("branch");

    let prompts = prompts.lock().unwrap();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt.contains("<previous-summary>") && prompt.contains("Old goal")),
        "the branch summary updates the parent's checkpoint summary: {prompts:?}"
    );
    let child = JsonlRepo::open(&branch.path).expect("open child");
    let Some(DurableRecord::Compaction { checkpoint, .. }) = child.records().last() else {
        panic!("the child ends with its branch checkpoint");
    };
    assert_eq!(checkpoint.reason, CompactionReason::Branch);
    assert_eq!(
        checkpoint.previous_checkpoint_id.as_deref(),
        Some("compact-parent-3")
    );
    assert_eq!(checkpoint.first_kept_entry_id, "entry-6");
    // The old list is carried over and appended to the new summary.
    assert_eq!(checkpoint.read_files, vec!["src/a.rs".to_owned()]);
    assert!(checkpoint
        .summary
        .ends_with("<read-files>\nsrc/a.rs\n</read-files>"));
    drop(child);
    cleanup(&path);
}

#[test]
fn branch_compaction_chains_onto_the_last_applied_checkpoint_not_the_last_record() {
    use slim_core::context::{compaction_prefix_fingerprint, CompactionReason};
    use slim_core::session::CompactionCheckpoint;
    use slim_core::ProviderMessage;

    let path = path("compact-applied-chain.jsonl");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("parent-applied", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    let entry = |seq: u64, role, content: String| DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: format!("entry-{seq}"),
            role,
            content,
            parent_entry_id: None,
            operation_id: format!("operation-{seq}"),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    };
    repo.append(entry(0, DurableEntryRole::User, "root".into()))
        .expect("root");
    repo.append(entry(1, DurableEntryRole::Assistant, "first answer".into()))
        .expect("answer");
    repo.append(entry(2, DurableEntryRole::User, "recent".into()))
        .expect("recent");
    let checkpoint =
        |id: &str, anchor: &str, fingerprint: String, previous: Option<&str>, summary: &str| {
            DurableRecord::Compaction {
                seq: 0,
                checkpoint: CompactionCheckpoint {
                    checkpoint_id: id.into(),
                    summary: summary.into(),
                    first_kept_entry_id: anchor.into(),
                    prefix_fingerprint: fingerprint,
                    previous_checkpoint_id: previous.map(str::to_owned),
                    tokens_before: 1_000,
                    tokens_after: 100,
                    input_tokens: None,
                    output_tokens: None,
                    duration_ms: 0,
                    reason: CompactionReason::Threshold,
                    read_files: Vec::new(),
                    modified_files: Vec::new(),
                },
            }
        };
    let with_seq = |mut record: DurableRecord, new_seq: u64| {
        if let DurableRecord::Compaction { seq, .. } = &mut record {
            *seq = new_seq;
        }
        record
    };
    repo.append(with_seq(
        checkpoint(
            "applied",
            "entry-2",
            compaction_prefix_fingerprint(&[
                ProviderMessage::user("root"),
                ProviderMessage::assistant("first answer", Vec::new()),
            ]),
            None,
            "## Goal
Applied goal",
        ),
        3,
    ))
    .expect("applied checkpoint");
    repo.append(entry(4, DurableEntryRole::Assistant, "ok".into()))
        .expect("answer");
    repo.append(entry(5, DurableEntryRole::User, "next".into()))
        .expect("next");
    // The latest record chains onto the applied one but its fingerprint is
    // wrong: a resume skips it, so a branch must not build on it.
    repo.append(with_seq(
        checkpoint(
            "never-applied",
            "entry-5",
            "0000000000000000".into(),
            Some("applied"),
            "## Goal
Ignored goal",
        ),
        6,
    ))
    .expect("unapplied checkpoint");
    repo.append(entry(7, DurableEntryRole::Assistant, "x".repeat(100_000)))
        .expect("long answer");
    repo.append(entry(8, DurableEntryRole::User, "latest".into()))
        .expect("latest");
    drop(repo);

    let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = prompts.clone();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let branch = runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child",
            8,
            move |request| {
                seen.lock().unwrap().push(request.prompt);
                async move {
                    Ok("## Goal
New goal"
                        .to_owned())
                }
            },
        ))
        .expect("branch");

    let prompts = prompts.lock().unwrap();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt.contains("Applied goal") && !prompt.contains("Ignored goal")),
        "{prompts:?}"
    );
    let child = JsonlRepo::open(&branch.path).expect("open child");
    let Some(DurableRecord::Compaction { checkpoint, .. }) = child.records().last() else {
        panic!("the child ends with its branch checkpoint");
    };
    assert_eq!(
        checkpoint.previous_checkpoint_id.as_deref(),
        Some("applied")
    );
    // The branch checkpoint applies on rebuild: the chain is intact.
    let rebuilt = slim_core::session::rebuild_provider_history(child.records()).expect("rebuild");
    assert_eq!(
        rebuilt.applied_checkpoint_id.as_deref(),
        Some(checkpoint.checkpoint_id.as_str())
    );
    assert_eq!(rebuilt.skipped_checkpoints.len(), 1);
    assert_eq!(
        rebuilt.skipped_checkpoints[0].checkpoint_id,
        "never-applied"
    );
    drop(child);
    cleanup(&path);
}
