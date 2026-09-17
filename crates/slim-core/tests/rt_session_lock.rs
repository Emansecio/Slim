use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use slim_core::session::{DurableSessionHeader, JsonlRepo};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let candidate = root.join(format!(
        "slim-rt-lock-{}-{nanos}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&candidate).expect("create unique test directory");
    let directory = fs::canonicalize(&candidate).expect("canonical test directory");
    directory.join(format!("{label}.jsonl"))
}

fn header(id: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "2026-08-22T12:00:00Z", "D:\\Slim", None, None)
}

// A second writer on the same durable session must be refused while the
// first handle is alive. Windows enforces this through share modes; on Unix
// the lock file previously granted nothing, so a second repo could open and
// append at a stale offset, overwriting the first writer's records.
#[test]
fn second_repo_open_is_refused_while_first_holds_session() {
    let path = temp_path("locked");
    let first = JsonlRepo::create(&path, header("lock-session")).expect("create session");

    let second = JsonlRepo::open(&path);
    assert!(
        second.is_err(),
        "a second writer must not open an already-locked durable session"
    );

    drop(first);
    JsonlRepo::open(&path).expect("reopen after the writer is dropped");

    fs::remove_dir_all(path.parent().expect("test directory")).expect("remove test directory");
}
