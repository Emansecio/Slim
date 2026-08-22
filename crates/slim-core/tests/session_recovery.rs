use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use slim_core::events::{EventKind, SessionEvent};
use slim_core::session::{recover, SessionWriter};

fn temp_path(name: &str) -> PathBuf {
    let unique = format!(
        "slim-session-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    std::env::temp_dir().join(unique).join(name)
}

#[test]
fn replay_preserves_valid_prefix_and_quarantines_partial_suffix() {
    let path = temp_path("session.jsonl");
    let mut writer = SessionWriter::create(&path, "session-1", "D:\\Slim").expect("create");
    writer
        .append(&SessionEvent::new(
            1,
            EventKind::SessionStarted {
                session_id: "session-1".into(),
            },
        ))
        .expect("append");
    drop(writer);

    let mut file = OpenOptions::new().append(true).open(&path).expect("open");
    file.write_all(br#"{"type":"event","seq":2"#)
        .expect("partial write");
    drop(file);

    let recovered = recover(&path).expect("recover");
    assert_eq!(recovered.events.len(), 1);
    let quarantine = recovered.quarantine_path.expect("quarantine");
    assert!(quarantine.exists());

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn reopening_writer_continues_sequence_and_rejects_duplicates() {
    let path = temp_path("reopen.jsonl");
    let mut writer = SessionWriter::create(&path, "session-2", "D:\\Slim").expect("create");
    writer
        .append(&SessionEvent::new(
            1,
            EventKind::AssistantTextDelta { text: "one".into() },
        ))
        .expect("append");
    drop(writer);

    let mut reopened = SessionWriter::create(&path, "session-2", "D:\\Slim").expect("reopen");
    assert!(reopened
        .append(&SessionEvent::new(
            1,
            EventKind::AssistantTextDelta {
                text: "duplicate".into()
            },
        ))
        .is_err());
    reopened
        .append(&SessionEvent::new(
            2,
            EventKind::AssistantTextDelta { text: "two".into() },
        ))
        .expect("continue");
    assert_eq!(recover(&path).expect("recover").events.len(), 2);

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}
