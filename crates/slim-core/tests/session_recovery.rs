use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

#[cfg(windows)]
use std::os::windows::fs::symlink_file;

use slim_core::events::{EventKind, SessionEvent};
use slim_core::session::{recover, SessionWriter, MAX_DURABLE_SESSION_BYTES};

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
fn legacy_recovery_rejects_file_past_durable_size_limit() {
    let path = temp_path("oversized.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .expect("create");
    file.set_len(MAX_DURABLE_SESSION_BYTES + 1)
        .expect("sparse fixture");
    drop(file);

    let error = recover(&path).expect_err("oversized legacy session must be rejected");
    assert!(error.to_string().contains("size limit"));
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
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
    drop(reopened);
    assert_eq!(recover(&path).expect("recover").events.len(), 2);

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn batched_append_validates_the_full_sequence_before_one_durable_write() {
    let path = temp_path("batch.jsonl");
    let mut writer = SessionWriter::create(&path, "session-batch", "D:\\Slim").expect("create");
    let events = (1..=3)
        .map(|seq| {
            SessionEvent::new(
                seq,
                EventKind::AssistantTextDelta {
                    text: format!("delta-{seq}"),
                },
            )
        })
        .collect::<Vec<_>>();
    writer.append_batch(&events).expect("batch append");
    assert!(writer
        .append_batch(&[
            SessionEvent::new(
                5,
                EventKind::AssistantTextDelta {
                    text: "five".into()
                },
            ),
            SessionEvent::new(
                4,
                EventKind::AssistantTextDelta {
                    text: "four".into()
                },
            ),
        ])
        .is_err());
    writer
        .append(&SessionEvent::new(
            4,
            EventKind::AssistantTextDelta {
                text: "four".into(),
            },
        ))
        .expect("sequence remains unchanged after rejected batch");
    drop(writer);

    let recovered = recover(&path).expect("recover");
    assert_eq!(recovered.events.len(), 4);
    assert_eq!(recovered.events.last().map(|event| event.seq), Some(4));
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[cfg(windows)]
#[test]
fn competing_writer_is_rejected_until_owner_drops() {
    let path = temp_path("locked.jsonl");
    let writer = SessionWriter::create(&path, "session-lock", "D:\\Slim").expect("create");
    assert!(SessionWriter::create(&path, "session-lock", "D:\\Slim").is_err());
    drop(writer);
    SessionWriter::create(&path, "session-lock", "D:\\Slim").expect("reopen");

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn reopening_after_torn_tail_repairs_before_append() {
    let path = temp_path("torn-tail.jsonl");
    let mut writer = SessionWriter::create(&path, "session-torn", "D:\\Slim").expect("create");
    writer
        .append(&SessionEvent::new(
            1,
            EventKind::AssistantTextDelta { text: "one".into() },
        ))
        .expect("append");
    drop(writer);

    let tail = br#"{"type":"event","seq":2"#;
    let mut file = OpenOptions::new().append(true).open(&path).expect("open");
    file.write_all(tail).expect("partial write");
    drop(file);

    let mut reopened = SessionWriter::create(&path, "session-torn", "D:\\Slim").expect("reopen");
    reopened
        .append(&SessionEvent::new(
            2,
            EventKind::AssistantTextDelta { text: "two".into() },
        ))
        .expect("append after repair");
    drop(reopened);

    let recovered = recover(&path).expect("recover");
    assert_eq!(recovered.events.len(), 2);
    let quarantine = PathBuf::from(format!("{}.quarantine", path.display()));
    assert_eq!(fs::read(quarantine).expect("read quarantine"), tail);

    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn reopening_after_valid_line_without_newline_inserts_separator() {
    let path = temp_path("missing-newline.jsonl");
    let mut writer = SessionWriter::create(&path, "session-newline", "D:\\Slim").expect("create");
    writer
        .append(&SessionEvent::new(
            1,
            EventKind::AssistantTextDelta { text: "one".into() },
        ))
        .expect("append");
    drop(writer);

    let metadata = fs::metadata(&path).expect("metadata");
    let file = OpenOptions::new().write(true).open(&path).expect("open");
    file.set_len(metadata.len() - 1).expect("remove newline");
    drop(file);

    let mut reopened = SessionWriter::create(&path, "session-newline", "D:\\Slim").expect("reopen");
    reopened
        .append(&SessionEvent::new(
            2,
            EventKind::AssistantTextDelta { text: "two".into() },
        ))
        .expect("append after separator repair");
    drop(reopened);

    assert_eq!(recover(&path).expect("recover").events.len(), 2);
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn reopening_torn_tail_preserves_existing_quarantine_and_uses_next_slot() {
    let path = temp_path("quarantine-collision.jsonl");
    let mut writer =
        SessionWriter::create(&path, "session-quarantine", "D:\\Slim").expect("create");
    writer
        .append(&SessionEvent::new(
            1,
            EventKind::AssistantTextDelta { text: "one".into() },
        ))
        .expect("append");
    drop(writer);

    let tail = br#"{"type":"event","seq":2"#;
    let mut file = OpenOptions::new().append(true).open(&path).expect("open");
    file.write_all(tail).expect("partial write");
    drop(file);

    let quarantine = PathBuf::from(format!("{}.quarantine", path.display()));
    let existing = b"previous evidence";
    fs::write(&quarantine, existing).expect("existing quarantine");

    let mut reopened =
        SessionWriter::create(&path, "session-quarantine", "D:\\Slim").expect("reopen");
    reopened
        .append(&SessionEvent::new(
            2,
            EventKind::AssistantTextDelta { text: "two".into() },
        ))
        .expect("append after repair");
    drop(reopened);

    assert_eq!(
        fs::read(&quarantine).expect("read existing quarantine"),
        existing
    );
    assert_eq!(
        fs::read(format!("{}.1", quarantine.display())).expect("read new quarantine"),
        tail
    );
    assert_eq!(recover(&path).expect("recover").events.len(), 2);
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn invalid_complete_or_interior_json_fails_without_quarantine() {
    for (name, suffix) in [
        ("invalid-complete.jsonl", b"{not-json}\n".as_slice()),
        (
            "invalid-interior.jsonl",
            b"{not-json}\n{\"type\":\"event\"}\n".as_slice(),
        ),
        (
            "invalid-semantic-eof.jsonl",
            b"{\"type\":\"event\"}".as_slice(),
        ),
    ] {
        let path = temp_path(name);
        let mut writer =
            SessionWriter::create(&path, "session-invalid", "D:\\Slim").expect("create");
        writer
            .append(&SessionEvent::new(
                1,
                EventKind::AssistantTextDelta { text: "one".into() },
            ))
            .expect("append");
        drop(writer);

        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        file.write_all(suffix).expect("invalid write");
        drop(file);

        let error = recover(&path).expect_err("invalid JSON must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!PathBuf::from(format!("{}.quarantine", path.display())).exists());
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }
}

#[cfg(windows)]
#[test]
fn recover_is_rejected_while_writer_is_alive() {
    let path = temp_path("recover-locked.jsonl");
    let writer = SessionWriter::create(&path, "session-recover-lock", "D:\\Slim").expect("create");
    assert!(recover(&path).is_err());
    drop(writer);
    recover(&path).expect("recover after drop");
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn unsupported_schema_versions_are_rejected_without_quarantine() {
    for version in [2, 99] {
        let path = temp_path(&format!("schema-{version}.jsonl"));
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        let header = serde_json::json!({
            "type": "session",
            "schema_version": version,
            "id": "session-schema",
            "timestamp": "unknown",
            "cwd": "D:\\Slim",
            "parent_id": null,
            "cutoff_seq": null
        });
        fs::write(&path, format!("{header}\n")).expect("write header");

        let error = recover(&path).expect_err("unsupported schema must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!PathBuf::from(format!("{}.quarantine", path.display())).exists());
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }
}

#[cfg(windows)]
#[test]
fn symlinked_session_alias_cannot_bypass_writer_lock() {
    let path = temp_path("real-session.jsonl");
    let alias = path.parent().expect("parent").join("alias-session.jsonl");
    let writer = SessionWriter::create(&path, "session-alias", "D:\\Slim").expect("create");

    match symlink_file(&path, &alias) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            drop(writer);
            let _ = fs::remove_dir_all(path.parent().expect("parent"));
            return;
        }
        Err(error) => panic!("symlink setup failed: {error}"),
    }

    assert!(SessionWriter::create(&alias, "session-alias", "D:\\Slim").is_err());
    drop(writer);
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[cfg(windows)]
#[test]
fn symlinked_lock_sentinel_is_rejected_without_redirecting() {
    let path = temp_path("redirected-lock.jsonl");
    let parent = path.parent().expect("parent");
    fs::create_dir_all(parent).expect("create parent");
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let target = parent.join("redirect-target.lock");
    let original = b"unrelated sentinel";
    fs::write(&target, original).expect("target");

    match symlink_file(&target, &lock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let _ = fs::remove_dir_all(parent);
            return;
        }
        Err(error) => panic!("symlink setup failed: {error}"),
    }

    assert!(SessionWriter::create(&path, "session-redirect", "D:\\Slim").is_err());
    assert_eq!(fs::read(&target).expect("target read"), original);
    let _ = fs::remove_dir_all(parent);
}
