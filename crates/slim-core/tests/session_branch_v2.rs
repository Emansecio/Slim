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
