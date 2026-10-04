use slim_core::ProviderMessage;

use super::durable_checkpoint_anchor;

#[test]
fn checkpoint_anchor_requires_an_exact_durable_prefix() {
    let messages = vec![
        ProviderMessage::user("root"),
        ProviderMessage::assistant("answer", Vec::new()),
        ProviderMessage::user("next"),
    ];
    let entry_ids = vec![Some("u1".into()), Some("a1".into()), Some("u2".into())];
    let fingerprint = slim_core::context::canonical_prefix_fingerprint(&messages[..2]);

    assert_eq!(
        durable_checkpoint_anchor(&messages, &entry_ids, 2, &fingerprint).as_deref(),
        Some("u2")
    );
    let compacted_ids = [Some("u1".into()), None, Some("u2".into())];
    assert_eq!(
        durable_checkpoint_anchor(&messages, &compacted_ids, 2, &fingerprint).as_deref(),
        Some("u2")
    );
    assert!(durable_checkpoint_anchor(
        &messages,
        &compacted_ids,
        1,
        &slim_core::context::canonical_prefix_fingerprint(&messages[..1]),
    )
    .is_none());
    assert!(
        durable_checkpoint_anchor(&messages, &entry_ids, messages.len(), &fingerprint,).is_none()
    );
}

#[test]
fn persisted_commits_chain_on_the_journal_and_skip_an_unanchored_one() {
    use slim_core::context::{
        canonical_prefix_fingerprint, compaction_summary_message, CompactionCommit,
        CompactionReason,
    };
    use slim_core::session::{
        DurableEntry, DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo,
    };

    let path = std::env::temp_dir().join(format!(
        "slim-persist-commits-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
    )
    .expect("create");
    let messages = [
        ProviderMessage::user("one"),
        ProviderMessage::assistant("two", Vec::new()),
        ProviderMessage::user("three"),
        ProviderMessage::assistant("four", Vec::new()),
        ProviderMessage::user("five"),
    ];
    for (index, message) in messages.iter().cloned().enumerate() {
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
    let commit = |first_kept_index: usize, fingerprint: String, summary: &str| CompactionCommit {
        summary: summary.into(),
        canonical_prefix_fingerprint: fingerprint,
        first_kept_index,
        tokens_before: 100,
        tokens_after: 10,
        input_tokens: 5,
        output_tokens: 2,
        duration_ms: 1,
        reason: CompactionReason::Threshold,
        read_files: vec!["src/a.rs".into()],
        modified_files: vec!["src/b.rs".into()],
    };
    // The first compaction kept from `e2`. The second ran on the compacted
    // history `[summary, three, four, five]` and kept from `five`.
    let first = commit(
        2,
        canonical_prefix_fingerprint(&messages[..2]),
        "first summary",
    );
    let compacted = [
        compaction_summary_message("first summary"),
        messages[2].clone(),
        messages[3].clone(),
        messages[4].clone(),
    ];
    let second = commit(
        3,
        canonical_prefix_fingerprint(&compacted[..3]),
        "second summary",
    );
    let unanchored = commit(1, "0000000000000000".into(), "never persisted");

    let warnings = super::persist_compaction_commits(&mut repo, vec![first, unanchored, second])
        .expect("persist");
    assert_eq!(
        warnings.len(),
        1,
        "the skipped commit is reported: {warnings:?}"
    );
    assert!(warnings[0].contains("could not be saved"));

    let checkpoints: Vec<_> = repo
        .records()
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Compaction { checkpoint, .. } => Some(checkpoint.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints.len(), 2, "the unanchored commit is skipped");
    assert_eq!(checkpoints[0].first_kept_entry_id, "e2");
    assert_eq!(checkpoints[0].previous_checkpoint_id, None);
    assert_eq!(checkpoints[0].read_files, vec!["src/a.rs"]);
    assert_eq!(checkpoints[0].modified_files, vec!["src/b.rs"]);
    assert_eq!(checkpoints[1].first_kept_entry_id, "e4");
    assert_eq!(
        checkpoints[1].previous_checkpoint_id.as_deref(),
        Some(checkpoints[0].checkpoint_id.as_str())
    );
    let history = super::durable_provider_history(&super::SessionPreflight::from_open_repo(&repo))
        .expect("history");
    assert_eq!(history.messages.len(), 2);
    assert_eq!(
        slim_core::context::compaction_summary_text(&history.messages[0]),
        Some("second summary")
    );
    assert_eq!(history.messages[1].content, "five");
    drop(repo);
    std::fs::remove_file(&path).expect("cleanup");
    let _ = std::fs::remove_file(path.with_extension("jsonl.lock"));
}

/// A parallel batch's results are journaled as the calls complete, while the
/// live history appends them in call order: the checkpoint still anchors.
#[test]
fn a_checkpoint_survives_a_batch_journaled_out_of_call_order() {
    use slim_core::context::{
        canonical_prefix_fingerprint, compaction_prefix_fingerprint, CompactionCommit,
        CompactionReason,
    };
    use slim_core::session::{
        DurableEntry, DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo,
    };
    use slim_core::ProviderToolCall;

    let call = |id: &str, name: &str| ProviderToolCall {
        id: id.into(),
        name: name.into(),
        arguments: "{}".into(),
    };
    let assistant = ProviderMessage::assistant("", vec![call("a", "patch"), call("b", "read")]);
    let journaled = [
        ProviderMessage::user("go"),
        assistant.clone(),
        ProviderMessage::tool("read", "b", "read output"),
        ProviderMessage::tool("patch", "a", "patch output"),
        ProviderMessage::assistant("done", Vec::new()),
        ProviderMessage::user("next"),
    ];
    let live = [
        ProviderMessage::user("go"),
        assistant,
        ProviderMessage::tool("patch", "a", "patch output"),
        ProviderMessage::tool("read", "b", "read output"),
        ProviderMessage::assistant("done", Vec::new()),
        ProviderMessage::user("next"),
    ];
    assert_ne!(
        compaction_prefix_fingerprint(&journaled[..5]),
        compaction_prefix_fingerprint(&live[..5]),
        "the plain fingerprints are order sensitive"
    );

    let path = std::env::temp_dir().join(format!(
        "slim-persist-reordered-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("session", "now", r"D:\Slim", None, None),
    )
    .expect("create");
    for (index, message) in journaled.iter().cloned().enumerate() {
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
    let commit = CompactionCommit {
        summary: "summary".into(),
        canonical_prefix_fingerprint: canonical_prefix_fingerprint(&live[..5]),
        first_kept_index: 5,
        tokens_before: 100,
        tokens_after: 10,
        input_tokens: 5,
        output_tokens: 2,
        duration_ms: 1,
        reason: CompactionReason::Threshold,
        read_files: Vec::new(),
        modified_files: Vec::new(),
    };
    let warnings = super::persist_compaction_commits(&mut repo, vec![commit]).expect("persist");
    assert!(warnings.is_empty(), "{warnings:?}");

    let checkpoint = repo
        .records()
        .iter()
        .find_map(|record| match record {
            DurableRecord::Compaction { checkpoint, .. } => Some(checkpoint.clone()),
            _ => None,
        })
        .expect("checkpoint persisted");
    assert_eq!(checkpoint.first_kept_entry_id, "e5");
    assert_eq!(
        checkpoint.prefix_fingerprint,
        compaction_prefix_fingerprint(&journaled[..5]),
        "the checkpoint stores the journal's own order"
    );
    let history = super::durable_provider_history(&super::SessionPreflight::from_open_repo(&repo))
        .expect("history");
    assert!(history.skipped_checkpoints.is_empty());
    assert_eq!(history.messages.len(), 2);
    assert_eq!(
        slim_core::context::compaction_summary_text(&history.messages[0]),
        Some("summary")
    );
    assert_eq!(history.messages[1].content, "next");
    drop(repo);
    std::fs::remove_file(&path).expect("cleanup");
    let _ = std::fs::remove_file(path.with_extension("jsonl.lock"));
}
