use std::fs;
use std::path::PathBuf;

use slim_core::events::{EventKind, SessionEvent};
use slim_core::session::{branch, recover, SessionIndex, SessionWriter};

fn temp_path(name: &str) -> PathBuf {
    let unique = format!(
        "slim-branch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    std::env::temp_dir().join(unique).join(name)
}

#[test]
fn branch_keeps_parent_identity_and_rebuildable_index() {
    let path = temp_path("parent.jsonl");
    let mut writer = SessionWriter::create(&path, "parent", "D:\\Slim").expect("create");
    for seq in 1..=2 {
        writer
            .append(&SessionEvent::new(
                seq,
                EventKind::AssistantTextDelta {
                    text: format!("{seq}"),
                },
            ))
            .expect("append");
    }
    drop(writer);

    let child_path = branch(&path, "child", 1).expect("branch");
    let child = recover(&child_path).expect("recover child");
    assert_eq!(child.header.id, "child");
    assert_eq!(child.header.parent_id.as_deref(), Some("parent"));
    assert_eq!(child.header.cutoff_seq, Some(1));
    assert_eq!(child.events.len(), 1);

    let index = SessionIndex::rebuild(&child.events);
    assert_eq!(index.last_seq, Some(1));

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn legacy_branch_rejects_traversal_child_ids_before_creating_a_path() {
    let path = temp_path("parent-traversal.jsonl");
    let mut writer = SessionWriter::create(&path, "parent", "D:\\Slim").expect("create");
    writer
        .append(&SessionEvent::new(
            1,
            EventKind::AssistantTextDelta { text: "one".into() },
        ))
        .expect("append");
    drop(writer);

    assert!(branch(&path, "../escape", 1).is_err());
    assert!(branch(&path, "", 1).is_err());
    let escaped = path
        .parent()
        .expect("parent")
        .parent()
        .expect("temp root")
        .join("escape.jsonl");
    assert!(!escaped.exists());
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}
