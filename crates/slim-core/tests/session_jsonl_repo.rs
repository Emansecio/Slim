use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::fs::symlink_file;

use serde_json::json;
use slim_core::session::{
    DurableEntry, DurableEntryRole, DurableFact, DurableOperation, DurableOperationKind,
    DurableRecord, DurableRepo, DurableSessionHeader, DurableUsage, JsonlRepo,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let candidate = root.join(format!(
        "slim-jsonl-repo-{}-{nanos}-{nonce}",
        std::process::id()
    ));
    assert_eq!(candidate.parent(), Some(root.as_path()));
    assert!(candidate
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("slim-jsonl-repo-")));
    fs::create_dir(&candidate).expect("create unique test directory");
    let directory = fs::canonicalize(&candidate).expect("canonical test directory");
    assert_eq!(directory.parent(), Some(root.as_path()));
    assert!(directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("slim-jsonl-repo-")));
    directory.join(format!("{label}.jsonl"))
}

fn header(id: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "2026-08-22T12:00:00Z", "D:\\Slim", None, None)
}

fn entry(seq: u64) -> DurableRecord {
    DurableRecord::Entry {
        seq,
        entry: DurableEntry {
            entry_id: format!("entry-{seq}"),
            role: DurableEntryRole::User,
            content: format!("content-{seq}"),
            parent_entry_id: None,
            operation_id: "op-1".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    }
}

fn four_kinds() -> Vec<DurableRecord> {
    vec![
        entry(0),
        DurableRecord::Operation {
            seq: 1,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::Started {
                    input_entry_id: "entry-0".into(),
                },
            },
        },
        DurableRecord::Operation {
            seq: 2,
            operation: DurableOperation {
                operation_id: "op-1".into(),
                kind: DurableOperationKind::ProviderAttemptStarted {
                    attempt_id: "attempt-1".into(),
                    ordinal: 1,
                },
            },
        },
        DurableRecord::Fact {
            seq: 3,
            fact: DurableFact {
                namespace: "session".into(),
                key: "mode".into(),
                value: json!("auto"),
            },
        },
        DurableRecord::Usage {
            seq: 4,
            usage: DurableUsage {
                operation_id: "op-1".into(),
                attempt_id: "attempt-1".into(),
                input_tokens: Some(3),
                output_tokens: Some(5),
            },
        },
    ]
}

fn cleanup(path: &Path) {
    let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let parent =
        fs::canonicalize(path.parent().expect("test directory")).expect("canonical test directory");
    assert_ne!(parent, root, "test cleanup must not remove %TEMP%");
    assert_eq!(parent.parent(), Some(root.as_path()));
    assert!(
        parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("slim-jsonl-repo-")),
        "unexpected test cleanup directory: {}",
        parent.display()
    );
    fs::remove_dir_all(parent).expect("remove test directory");
}

#[test]
fn golden_header_and_four_record_kinds_are_durable_jsonl() {
    let path = temp_path("golden");
    let header = header("golden");
    let mut repo = JsonlRepo::create(&path, header.clone()).expect("create");
    for record in four_kinds() {
        repo.append(record).expect("append");
    }
    drop(repo);

    let expected = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n",
        serde_json::to_string(&header).expect("header json"),
        serde_json::to_string(&entry(0)).expect("entry json"),
        serde_json::to_string(&four_kinds()[1]).expect("operation json"),
        serde_json::to_string(&four_kinds()[2]).expect("attempt json"),
        serde_json::to_string(&four_kinds()[3]).expect("fact json"),
        serde_json::to_string(&four_kinds()[4]).expect("usage json")
    );
    assert_eq!(fs::read_to_string(&path).expect("read jsonl"), expected);
    cleanup(&path);
}

#[test]
fn reopen_reads_records_and_append_continues_after_last_sequence() {
    let path = temp_path("reopen");
    let mut repo = JsonlRepo::create(&path, header("reopen")).expect("create");
    repo.append(entry(0)).expect("append first");
    drop(repo);

    let mut reopened = JsonlRepo::open(&path).expect("open");
    assert_eq!(reopened.records(), &[entry(0)]);
    reopened.append(entry(2)).expect("append gap");
    assert_eq!(reopened.records(), &[entry(0), entry(2)]);
    assert_eq!(reopened.read_prefix(0), vec![entry(0)]);
    drop(reopened);
    cleanup(&path);
}

#[test]
fn create_is_exclusive_even_when_data_file_is_empty() {
    let path = temp_path("create-exclusive");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(&path, []).expect("empty data file");
    let before = fs::read(&path).expect("read empty data");
    let error = JsonlRepo::create(&path, header("must-not-adopt"))
        .err()
        .expect("existing data file must not be adopted");
    assert!(matches!(error.kind(), std::io::ErrorKind::AlreadyExists));
    assert_eq!(fs::read(&path).expect("read unchanged data"), before);
    cleanup(&path);
}

#[test]
fn open_repairs_only_a_syntactically_incomplete_eof_tail() {
    let path = temp_path("torn-tail");
    let session_header = header("torn-tail");
    let mut repo = JsonlRepo::create(&path, session_header.clone()).expect("create");
    repo.append(entry(0)).expect("append");
    drop(repo);

    let tail = br#"{"type":"entry","seq":2"#;
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open raw");
    file.write_all(tail).expect("write torn tail");
    drop(file);

    let mut reopened = JsonlRepo::open(&path).expect("repair torn tail");
    assert_eq!(reopened.records(), &[entry(0)]);
    let quarantine = PathBuf::from(format!("{}.quarantine", path.display()));
    assert_eq!(fs::read(&quarantine).expect("read quarantine"), tail);
    reopened.append(entry(3)).expect("append after repair");
    drop(reopened);
    let expected = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&session_header).expect("header json"),
        serde_json::to_string(&entry(0)).expect("first record json"),
        serde_json::to_string(&entry(3)).expect("appended record json")
    );
    assert_eq!(
        fs::read(&path).expect("read repaired data"),
        expected.as_bytes()
    );
    let reopened = JsonlRepo::open(&path).expect("reopen repaired data");
    assert_eq!(reopened.records(), &[entry(0), entry(3)]);
    drop(reopened);
    cleanup(&path);
}

#[test]
fn rejected_duplicate_and_regressive_append_preserve_jsonl_bytes_and_state() {
    let path = temp_path("rejected-append");
    let mut repo = JsonlRepo::create(&path, header("rejected-append")).expect("create");
    repo.append(entry(0)).expect("append first");
    repo.append(entry(3)).expect("append second");
    drop(repo);

    for rejected in [entry(3), entry(2)] {
        let before_bytes = fs::read(&path).expect("read before rejected append");
        let mut repo = JsonlRepo::open(&path).expect("open");
        let before_records = repo.records().to_vec();
        let error = repo
            .append(rejected)
            .expect_err("non-increasing append must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(repo.records(), before_records.as_slice());
        drop(repo);
        assert_eq!(
            fs::read(&path).expect("read after rejected append"),
            before_bytes
        );
    }
    cleanup(&path);
}

#[test]
fn open_rejects_complete_or_interior_invalid_json_without_mutation() {
    for (label, suffix) in [
        ("complete", b"{not-json}\n".as_slice()),
        ("interior", b"{not-json}\n{\"type\":\"entry\"}\n".as_slice()),
        ("semantic-eof", b"{\"type\":\"entry\"}".as_slice()),
    ] {
        let path = temp_path(&format!("invalid-{label}"));
        let mut repo = JsonlRepo::create(&path, header(label)).expect("create");
        repo.append(entry(0)).expect("append");
        drop(repo);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open raw");
        file.write_all(suffix).expect("write invalid suffix");
        drop(file);
        let before = fs::read(&path).expect("read before");

        let error = JsonlRepo::open(&path)
            .err()
            .expect("invalid JSON must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{label}");
        assert_eq!(fs::read(&path).expect("read unchanged"), before, "{label}");
        assert!(
            !PathBuf::from(format!("{}.quarantine", path.display())).exists(),
            "{label}"
        );
        cleanup(&path);
    }
}

#[test]
fn open_inserts_missing_final_newline_before_append() {
    let path = temp_path("missing-newline");
    let mut repo = JsonlRepo::create(&path, header("missing-newline")).expect("create");
    repo.append(entry(0)).expect("append");
    drop(repo);
    let metadata = fs::metadata(&path).expect("metadata");
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open raw");
    file.set_len(metadata.len() - 1).expect("remove newline");
    drop(file);

    let mut reopened = JsonlRepo::open(&path).expect("repair separator");
    assert_eq!(reopened.records(), &[entry(0)]);
    reopened.append(entry(1)).expect("append after separator");
    drop(reopened);
    let bytes = fs::read(&path).expect("read repaired data");
    assert!(bytes.windows(2).any(|window| window == b"}\n"));
    let reopened = JsonlRepo::open(&path).expect("reopen repaired data");
    assert_eq!(reopened.records(), &[entry(0), entry(1)]);
    drop(reopened);
    cleanup(&path);
}

#[test]
fn quarantine_collision_preserves_existing_bytes_and_uses_next_slot() {
    let path = temp_path("quarantine-collision");
    let mut repo = JsonlRepo::create(&path, header("quarantine-collision")).expect("create");
    repo.append(entry(0)).expect("append");
    drop(repo);
    let tail = br#"{"type":"entry","seq":2"#;
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open raw");
    file.write_all(tail).expect("write torn tail");
    drop(file);

    let quarantine = PathBuf::from(format!("{}.quarantine", path.display()));
    let existing = b"previous evidence";
    fs::write(&quarantine, existing).expect("write collision");
    JsonlRepo::open(&path).expect("repair with collision");
    assert_eq!(fs::read(&quarantine).expect("read existing"), existing);
    assert_eq!(
        fs::read(format!("{}.1", quarantine.display())).expect("read next quarantine"),
        tail
    );
    cleanup(&path);
}

#[test]
fn open_rejects_schema_header_type_and_order_errors_without_mutation() {
    let valid_header = serde_json::to_string(&header("invalid")).expect("header json");
    let valid_record = serde_json::to_string(&entry(0)).expect("record json");
    let cases = [
        ("empty", String::new()),
        (
            "record-before-header",
            format!("{valid_record}\n{valid_header}\n"),
        ),
        (
            "duplicate-header",
            format!("{valid_header}\n{valid_header}\n"),
        ),
        (
            "unknown-type",
            format!("{valid_header}\n{{\"type\":\"unknown\"}}\n"),
        ),
        (
            "duplicate-sequence",
            format!("{valid_header}\n{valid_record}\n{valid_record}\n"),
        ),
        (
            "regressive-sequence",
            format!(
                "{valid_header}\n{}\n{}\n",
                serde_json::to_string(&entry(2)).expect("record json"),
                serde_json::to_string(&entry(1)).expect("record json")
            ),
        ),
    ];
    for version in [1, 99] {
        let invalid_header = json!({
            "type": "session",
            "schema_version": version,
            "id": "invalid",
            "timestamp": "now",
            "cwd": "D:\\Slim",
            "parent_id": null,
            "cutoff_seq": null,
        });
        let name = format!("schema-{version}");
        let path = temp_path(&name);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        let bytes = format!("{invalid_header}\n");
        fs::write(&path, &bytes).expect("write schema fixture");
        let error = JsonlRepo::open(&path).err().expect("schema must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(&path).expect("read schema"), bytes);
        assert!(!PathBuf::from(format!("{}.quarantine", path.display())).exists());
        cleanup(&path);
    }
    for (label, contents) in cases {
        let path = temp_path(label);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(&path, &contents).expect("write invalid fixture");
        let before = fs::read(&path).expect("read before");
        let error = JsonlRepo::open(&path)
            .err()
            .expect("invalid structure must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{label}");
        assert_eq!(fs::read(&path).expect("read unchanged"), before, "{label}");
        assert!(
            !PathBuf::from(format!("{}.quarantine", path.display())).exists(),
            "{label}"
        );
        cleanup(&path);
    }
}

#[cfg(windows)]
#[test]
fn lock_and_alias_cannot_open_same_repository_until_owner_drops() {
    let path = temp_path("lock");
    let mut owner = JsonlRepo::create(&path, header("lock")).expect("create");
    owner.append(entry(0)).expect("append");
    assert!(JsonlRepo::open(&path).is_err());
    drop(owner);

    let repo = JsonlRepo::open(&path).expect("open after drop");
    let target_path = fs::canonicalize(&path).expect("canonical target");
    let alias = path.parent().expect("parent").join(format!(
        "alias-{}.jsonl",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = fs::remove_file(&alias);
    match symlink_file(&path, &alias) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            drop(repo);
            cleanup(&path);
            return;
        }
        Err(error) => panic!("symlink setup failed: {error}"),
    }
    assert!(JsonlRepo::open(&alias).is_err());
    drop(repo);
    let aliased = JsonlRepo::open(&alias).expect("open alias after owner drop");
    assert_eq!(aliased.path(), target_path.as_path());
    assert_eq!(aliased.records(), &[entry(0)]);
    drop(aliased);
    cleanup(&path);
}

#[cfg(windows)]
#[test]
fn hardlink_cannot_open_while_owner_lives_and_reopens_same_records() {
    let path = temp_path("hardlink");
    let hardlink = path.parent().expect("parent").join("alias-hardlink.jsonl");
    let mut owner = JsonlRepo::create(&path, header("hardlink")).expect("create");
    owner.append(entry(0)).expect("append");
    fs::hard_link(&path, &hardlink).expect("create hardlink");
    assert!(JsonlRepo::open(&hardlink).is_err());
    drop(owner);

    let reopened = JsonlRepo::open(&hardlink).expect("open hardlink after drop");
    assert_eq!(reopened.records(), &[entry(0)]);
    drop(reopened);
    cleanup(&path);
}

#[cfg(windows)]
#[test]
fn data_handle_denies_raw_write_and_delete_while_repository_lives() {
    let path = temp_path("data-share");
    let repo = JsonlRepo::create(&path, header("data-share")).expect("create");
    drop(repo);
    let repo = JsonlRepo::open(&path).expect("open");
    assert!(OpenOptions::new().write(true).open(&path).is_err());
    assert!(fs::remove_file(&path).is_err());
    drop(repo);
    cleanup(&path);
}

#[test]
fn atomic_create_produces_reopenable_header_and_preserves_existing_final() {
    let path = temp_path("atomic-create");
    let session_header = header("atomic-create");
    let repo = JsonlRepo::create(&path, session_header.clone()).expect("create");
    drop(repo);
    let reopened = JsonlRepo::open(&path).expect("reopen created file");
    assert_eq!(reopened.header(), &session_header);
    assert!(reopened.records().is_empty());
    drop(reopened);
    let before = fs::read(&path).expect("read existing final");
    let error = JsonlRepo::create(&path, header("must-not-overwrite"))
        .err()
        .expect("existing final must reject create");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&path).expect("read preserved final"), before);
    cleanup(&path);
}

#[cfg(windows)]
#[test]
fn symlinked_lock_sentinel_is_rejected_without_redirecting() {
    let path = temp_path("lock-sentinel");
    let parent = path.parent().expect("parent");
    fs::create_dir_all(parent).expect("parent");
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let target = parent.join("redirect-target.lock");
    let original = b"unrelated sentinel";
    fs::write(&target, original).expect("target");
    match symlink_file(&target, &lock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            cleanup(&path);
            return;
        }
        Err(error) => panic!("symlink setup failed: {error}"),
    }
    assert!(JsonlRepo::create(&path, header("redirect")).is_err());
    assert_eq!(fs::read(&target).expect("target read"), original);
    cleanup(&path);
}
