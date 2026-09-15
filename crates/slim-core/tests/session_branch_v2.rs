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
            |prompt| async move {
                assert!(prompt.contains("Summarize the prior agent transcript"));
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
fn branch_compaction_prompt_excludes_the_pinned_latest_instruction() {
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

    let seen_prompt = std::sync::Arc::new(std::sync::Mutex::new(None));
    let prompt_capture = std::sync::Arc::clone(&seen_prompt);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime
        .block_on(create_durable_branch_compacted(
            &path,
            "child-pinned",
            9,
            move |prompt| {
                *prompt_capture.lock().expect("capture") = Some(prompt);
                async { Ok("## Goal\nContinue with the exact constraint".to_owned()) }
            },
        ))
        .expect("branch");

    let prompt = seen_prompt
        .lock()
        .expect("capture")
        .clone()
        .expect("summary prompt");
    assert!(prompt.contains("closed work that must remain summarized"));
    assert!(!prompt.contains("latest instruction: preserve this exact constraint"));
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
            move |prompt| {
                *capture.lock().expect("capture") = Some(prompt);
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
