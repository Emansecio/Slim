//! Adversarial coverage for the durable session JSONL repository (RODADA 2 —
//! Sifter). Files are crafted byte-for-byte to probe the parser's recovery
//! contract in `session/jsonl_repo.rs`: torn tails, missing separators,
//! duplicate headers, non-increasing sequences, and lifecycle violations
//! injected at the record level rather than through the writer API.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use slim_core::session::{
    DurableEntry, DurableEntryRole, DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo,
};

/// `JsonlRepo` has no `Debug` impl, so `Result::expect_err` cannot be used.
fn expect_io_error(result: std::io::Result<JsonlRepo>, context: &str) -> std::io::Error {
    match result {
        Ok(_) => panic!("{context}: expected error, got Ok"),
        Err(error) => error,
    }
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let dir = root.join(format!(
        "slim-adv-jsonl-{}-{nanos}-{nonce}-{label}",
        std::process::id()
    ));
    fs::create_dir(&dir).expect("create unique test directory");
    fs::canonicalize(dir).expect("canonical test directory")
}

fn header_line(id: &str) -> String {
    json!({
        "type": "session",
        "schema_version": 2,
        "id": id,
        "timestamp": "2026-01-01T00:00:00Z",
        "cwd": "D:\\Slim",
        "parent_id": null,
        "cutoff_seq": null,
    })
    .to_string()
}

fn entry_line(seq: u64, entry_id: &str) -> String {
    json!({
        "type": "entry",
        "seq": seq,
        "entry": {
            "entry_id": entry_id,
            "role": "user",
            "content": "payload",
            "parent_entry_id": null,
            "operation_id": "op-1",
            "tool_call_id": null,
        }
    })
    .to_string()
}

fn operation_line(seq: u64, operation_id: &str, kind: serde_json::Value) -> String {
    json!({
        "type": "operation",
        "seq": seq,
        "operation": {"operation_id": operation_id, "kind": kind}
    })
    .to_string()
}

fn write_raw(path: &PathBuf, body: &str) {
    fs::write(path, body.as_bytes()).expect("write crafted file");
}

fn header(id: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "2026-01-01T00:00:00Z", "D:\\Slim", None, None)
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

#[test]
fn torn_tail_is_quarantined_and_truncated_on_open() {
    let dir = temp_dir("torn");
    let path = dir.join("s.jsonl");
    let good = format!("{}\n{}\n", header_line("s"), entry_line(0, "e0"));
    write_raw(&path, &format!("{good}{{\"type\":\"entry\",\"seq\":1,\"en"));
    let torn_len = path.metadata().expect("meta").len();

    let repo = JsonlRepo::open(&path).expect("open repairs torn tail");
    assert_eq!(repo.records().len(), 1);
    assert!(repo.path().metadata().expect("meta").len() < torn_len);
    let quarantined = fs::read_to_string(path.with_file_name("s.jsonl.quarantine"))
        .expect("torn tail quarantined");
    assert_eq!(quarantined, r#"{"type":"entry","seq":1,"en"#);
}

#[test]
fn open_no_repair_rejects_torn_tail_and_leaves_bytes() {
    let dir = temp_dir("torn-norep");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\n{}\n{{\"type\":\"entry\",\"seq\":1",
            header_line("s"),
            entry_line(0, "e0")
        ),
    );
    let before = fs::read(&path).expect("read");

    let error = expect_io_error(JsonlRepo::open_no_repair(&path), "torn tail rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(error.to_string().contains("torn tail"));
    assert_eq!(fs::read(&path).expect("read"), before);
    assert!(!path.with_file_name("s.jsonl.quarantine").exists());
}

#[test]
fn missing_trailing_newline_is_repaired_on_open() {
    let dir = temp_dir("nosep");
    let path = dir.join("s.jsonl");
    // Complete JSON, just missing the final newline separator.
    write_raw(
        &path,
        &format!("{}\n{}", header_line("s"), entry_line(0, "e0")),
    );

    let mut repo = JsonlRepo::open(&path).expect("open inserts separator");
    repo.append(entry(1)).expect("append after repair");
    let bytes = fs::read(&path).expect("read");
    assert!(bytes.ends_with(b"\n"));
    drop(repo);
    let reopened = JsonlRepo::open(&path).expect("reopen");
    assert_eq!(reopened.records().len(), 2);
}

#[test]
fn open_no_repair_rejects_missing_separator() {
    let dir = temp_dir("nosep-norep");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!("{}\n{}", header_line("s"), entry_line(0, "e0")),
    );
    let error = expect_io_error(JsonlRepo::open_no_repair(&path), "separator required");
    assert!(error.to_string().contains("missing a separator"));
}

#[test]
fn invalid_complete_line_mid_file_fails_without_quarantine() {
    let dir = temp_dir("midgarbage");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\nnot json at all\n{}\n",
            header_line("s"),
            entry_line(1, "e1")
        ),
    );
    let before = fs::read(&path).expect("read");

    let error = expect_io_error(JsonlRepo::open(&path), "mid-file garbage");
    assert!(error.to_string().contains("invalid durable session JSON"));
    // No repair may run for a syntactically complete corrupt line.
    assert_eq!(fs::read(&path).expect("read"), before);
    assert!(!path.with_file_name("s.jsonl.quarantine").exists());
}

#[test]
fn duplicate_session_header_is_rejected() {
    let dir = temp_dir("duphead");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!("{}\n{}\n", header_line("s"), header_line("s2")),
    );
    let error = expect_io_error(JsonlRepo::open(&path), "duplicate header");
    assert!(error.to_string().contains("duplicate session header"));
}

#[test]
fn record_before_header_is_rejected() {
    let dir = temp_dir("nohead");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!("{}\n{}\n", entry_line(0, "e0"), header_line("s")),
    );
    let error = expect_io_error(JsonlRepo::open(&path), "record first");
    assert!(error.to_string().contains("record before session header"));
}

#[test]
fn non_increasing_sequences_are_rejected() {
    for (label, first, second) in [
        ("dup-seq", 0u64, 0u64),
        ("desc-seq", 7u64, 3u64),
        ("repeat-max", u64::MAX - 1, u64::MAX - 1),
    ] {
        let dir = temp_dir(label);
        let path = dir.join("s.jsonl");
        write_raw(
            &path,
            &format!(
                "{}\n{}\n{}\n",
                header_line("s"),
                entry_line(first, "a"),
                entry_line(second, "b")
            ),
        );
        let error = expect_io_error(JsonlRepo::open(&path), "non-increasing seq");
        assert!(
            error.to_string().contains("not increasing"),
            "{label}: {error}"
        );
    }
}

/// A seq gap is tolerated: `validate_next` only requires strictly increasing
/// sequences, so a jump forward is accepted and preserved.
#[test]
fn forward_sequence_gap_is_accepted() {
    let dir = temp_dir("seqgap");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\n{}\n{}\n",
            header_line("s"),
            entry_line(0, "a"),
            entry_line(42, "b")
        ),
    );
    let repo = JsonlRepo::open(&path).expect("gap tolerated");
    assert_eq!(repo.records()[1].seq(), 42);
    assert_eq!(repo.next_seq().expect("next"), 43);
}

/// The sequence ceiling fails closed: a prefix ending at `u64::MAX` has no
/// representable successor, so `next_seq` and appends must error instead of
/// wrapping to zero.
#[test]
fn sequence_at_u64_max_fails_closed() {
    let dir = temp_dir("seqmax");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!("{}\n{}\n", header_line("s"), entry_line(u64::MAX, "top")),
    );
    let mut repo = JsonlRepo::open(&path).expect("max seq record is loadable");
    let error = repo.next_seq().expect_err("no successor exists");
    assert!(error.to_string().contains("overflowed"));
    let error = repo.append(entry(0)).expect_err("append must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}

#[test]
fn empty_file_has_no_header() {
    let dir = temp_dir("empty");
    let path = dir.join("s.jsonl");
    write_raw(&path, "");
    let error = expect_io_error(JsonlRepo::open(&path), "empty file");
    assert!(error.to_string().contains("missing session header"));
}

/// Documents the current recovery boundary: a torn header cannot be
/// quarantined because there is no validated session identity to repair
/// against. `parse` flags a torn tail, but `open` checks the missing header
/// first — the file fails closed with no quarantine sidecar.
#[test]
fn torn_header_line_fails_without_quarantine() {
    let dir = temp_dir("tornhead");
    let path = dir.join("s.jsonl");
    write_raw(&path, r#"{"type":"session","schema_vers"#);
    let error = expect_io_error(JsonlRepo::open(&path), "torn header");
    assert!(error.to_string().contains("missing session header"));
    assert!(
        !path.with_file_name("s.jsonl.quarantine").exists(),
        "no quarantine when the header itself is torn"
    );
}

#[test]
fn whitespace_only_torn_tail_is_quarantined() {
    let dir = temp_dir("wstorn");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!("{}\n{}\n   ", header_line("s"), entry_line(0, "e0")),
    );
    let repo = JsonlRepo::open(&path).expect("whitespace tail quarantined");
    assert_eq!(repo.records().len(), 1);
    assert!(path.with_file_name("s.jsonl.quarantine").exists());
}

#[test]
fn unsupported_schema_version_is_rejected() {
    let dir = temp_dir("v3");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\n",
            json!({
                "type": "session",
                "schema_version": 3,
                "id": "s",
                "timestamp": "2026-01-01T00:00:00Z",
                "cwd": "D:\\Slim",
                "parent_id": null,
                "cutoff_seq": null,
            })
        ),
    );
    let error = expect_io_error(JsonlRepo::open(&path), "v3 rejected");
    assert!(error.to_string().contains("schema_version"));
}

#[test]
fn utf8_bom_prefixed_file_is_rejected() {
    let dir = temp_dir("bom");
    let path = dir.join("s.jsonl");
    fs::write(&path, format!("\u{FEFF}{}\n", header_line("s")).as_bytes()).expect("write");
    let error = expect_io_error(JsonlRepo::open(&path), "bom rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
}

#[test]
fn unknown_record_type_is_rejected() {
    let dir = temp_dir("unkrec");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\n{}\n",
            header_line("s"),
            json!({"type": "bogus", "seq": 0})
        ),
    );
    expect_io_error(JsonlRepo::open(&path), "unknown record type");
}

#[test]
fn non_numeric_and_out_of_range_seq_is_rejected() {
    for (label, seq_json) in [
        ("neg-seq", "-1"),
        ("float-seq", "0.5"),
        ("huge-seq", "18446744073709551616"),
        ("str-seq", "\"0\""),
    ] {
        let dir = temp_dir(label);
        let path = dir.join("s.jsonl");
        write_raw(
            &path,
            &format!(
                "{}\n{{\"type\":\"entry\",\"seq\":{seq_json},\"entry\":{{\"entry_id\":\"e\",\"role\":\"user\",\"content\":\"c\",\"parent_entry_id\":null,\"operation_id\":\"op-1\",\"tool_call_id\":null}}}}\n",
                header_line("s")
            ),
        );
        expect_io_error(JsonlRepo::open(&path), &format!("{label} must be rejected"));
    }
}

#[test]
fn operation_terminal_before_start_is_rejected() {
    let dir = temp_dir("termfirst");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\n{}\n{}\n",
            header_line("s"),
            entry_line(0, "e0"),
            operation_line(1, "op-9", json!({"kind": "finished", "outcome": "success"}))
        ),
    );
    let error = expect_io_error(JsonlRepo::open(&path), "terminal before start");
    assert!(error.to_string().contains("terminal before start"));
}

#[test]
fn operation_started_twice_is_rejected() {
    let dir = temp_dir("dblstart");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\n{}\n{}\n{}\n",
            header_line("s"),
            entry_line(0, "e0"),
            operation_line(
                1,
                "op-9",
                json!({"kind": "started", "input_entry_id": "e0"})
            ),
            operation_line(
                2,
                "op-9",
                json!({"kind": "queue_intent", "input_entry_id": null})
            )
        ),
    );
    let error = expect_io_error(JsonlRepo::open(&path), "double start");
    assert!(error.to_string().contains("started more than once"));
}

#[test]
fn operation_restarted_after_terminal_is_rejected() {
    let dir = temp_dir("reopenterm");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!(
            "{}\n{}\n{}\n{}\n{}\n",
            header_line("s"),
            entry_line(0, "e0"),
            operation_line(
                1,
                "op-9",
                json!({"kind": "started", "input_entry_id": "e0"})
            ),
            operation_line(2, "op-9", json!({"kind": "finished", "outcome": "success"})),
            operation_line(
                3,
                "op-9",
                json!({"kind": "started", "input_entry_id": "e0"})
            )
        ),
    );
    let error = expect_io_error(JsonlRepo::open(&path), "restart after terminal");
    assert!(error.to_string().contains("started after terminal"));
}

#[test]
fn quarantine_suffix_increments_on_collision() {
    let dir = temp_dir("quarcollide");
    let path = dir.join("s.jsonl");
    write_raw(
        &path,
        &format!("{}\n{{\"type\":\"entry\",", header_line("s")),
    );
    // Occupy the first quarantine slot; the repair must pick `.quarantine.1`.
    fs::write(dir.join("s.jsonl.quarantine"), b"preexisting").expect("seed");
    JsonlRepo::open(&path).expect("open repairs");
    assert!(dir.join("s.jsonl.quarantine.1").exists());
    assert_eq!(
        fs::read(dir.join("s.jsonl.quarantine")).expect("read"),
        b"preexisting"
    );
}

/// Mutual exclusion holds on every platform, but the surfaced error differs:
/// Unix reaches `try_lock` and reports the curated `ResourceBusy` ("already in
/// use"); on Windows the share mode fails the lock-file open first, so callers
/// see a raw `Uncategorized` sharing violation (os error 32) instead.
/// Spec gap: Windows users get an opaque OS error rather than the curated
/// "session is already in use" message.
#[test]
fn second_writer_is_rejected_while_lock_is_held() {
    let dir = temp_dir("lock");
    let path = dir.join("s.jsonl");
    let first = JsonlRepo::create(&path, header("s")).expect("create");
    let error = expect_io_error(JsonlRepo::open(&path), "lock held");
    if cfg!(windows) {
        assert_eq!(error.raw_os_error(), Some(32));
    } else {
        assert_eq!(error.kind(), ErrorKind::ResourceBusy);
        assert!(error.to_string().contains("already in use"));
    }
    drop(first);
    JsonlRepo::open(&path).expect("lock released");
}

#[test]
fn open_no_repair_expected_detects_post_preflight_mutation() {
    let dir = temp_dir("expected");
    let path = dir.join("s.jsonl");
    let mut repo = JsonlRepo::create(&path, header("s")).expect("create");
    repo.append(entry(0)).expect("append");
    let expected_header = repo.header().clone();
    let expected_records = repo.records().to_vec();
    repo.append(entry(1)).expect("mutate after preflight");
    drop(repo);

    let error = expect_io_error(
        JsonlRepo::open_no_repair_expected(&path, &expected_header, &expected_records),
        "stale preflight",
    );
    assert!(error
        .to_string()
        .contains("changed after read-only preflight"));

    let snapshot = JsonlRepo::open(&path).expect("snapshot");
    let live_records = snapshot.records().to_vec();
    drop(snapshot);
    JsonlRepo::open_no_repair_expected(&path, &header("s"), &live_records)
        .expect("matching preflight opens");
}
