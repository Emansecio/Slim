use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::codeintel::{
    CodeIntelEditPosition, CodeIntelFileUpdate, CodeIntelPatch, CodeIntelPositionQuery,
    CodeIntelServerState, CodeIntelTextEdit, CodeIntelligence,
};
use slim_lsp::pool::{PoolConfig, StdioProcessFactory};
use slim_lsp::{LspCodeIntelligence, LspManagerConfig, LspProcessPool};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slim-lsp-incremental-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mock_binary() -> String {
    env!("CARGO_BIN_EXE_slim-lsp-mock").to_owned()
}

fn write_workspace(root: &Path, text: &str) -> PathBuf {
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='incremental-test'\nversion='0.1.0'\nedition='2021'\n",
    )
    .expect("write Cargo.toml");
    let source = root.join("src");
    std::fs::create_dir_all(&source).expect("create src");
    let path = source.join("main.rs");
    std::fs::write(&path, text).expect("write source");
    path
}

fn manager_with_mock(
    root: &Path,
    mock: Value,
) -> (Arc<LspProcessPool>, Arc<LspCodeIntelligence>, Value) {
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 2,
        factory: Arc::new(StdioProcessFactory),
    });
    let config = json!({ "mock": mock });
    let manager = LspCodeIntelligence::new(
        pool.clone(),
        LspManagerConfig {
            idle_shutdown: None,
            max_servers: 2,
            request_timeout: Duration::from_secs(3),
            server_config: config.clone(),
            max_open_documents: 8,
            server_path: Some(PathBuf::from(mock_binary())),
        },
    );
    assert!(root.join("Cargo.toml").is_file());
    (pool, manager, config)
}

fn query(root: &Path, path: &Path) -> CodeIntelPositionQuery {
    CodeIntelPositionQuery {
        workspace: root.to_path_buf(),
        path: path.to_path_buf(),
        line: 1,
        column: 1,
        symbol: Some("main".into()),
        max_results: 20,
        offset: 0,
        revision: None,
        cancellation: None,
    }
}

fn edit(line: u32, start: &str, end: &str, text: &str) -> CodeIntelTextEdit {
    edit_between(line, start, line, end, text)
}

fn edit_between(
    start_line: u32,
    start: &str,
    end_line: u32,
    end: &str,
    text: &str,
) -> CodeIntelTextEdit {
    CodeIntelTextEdit {
        start: CodeIntelEditPosition {
            line: start_line,
            prefix: start.into(),
        },
        end: CodeIntelEditPosition {
            line: end_line,
            prefix: end.into(),
        },
        text: text.into(),
    }
}

fn client_messages(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .expect("read mock log")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["direction"] == "client_to_server")
        .filter_map(|row| row.get("message").cloned())
        .collect()
}

async fn server_state(
    pool: &Arc<LspProcessPool>,
    root: &Path,
    config: &Value,
    path: &Path,
) -> Value {
    let root = std::fs::canonicalize(root).expect("canonical test root");
    let lease = pool
        .acquire_warm(&root, "rust-analyzer", config)
        .await
        .expect("warm mock server");
    let uri = slim_lsp::instance::file_uri(path)
        .expect("file URI")
        .to_string();
    lease
        .instance()
        .request_value("mock/documentState", json!({ "uri": uri }))
        .await
        .expect("mock document state")
}

async fn open_document(manager: &Arc<LspCodeIntelligence>, root: &Path, path: &Path) {
    let outcome = manager.definition(&query(root, path)).await;
    assert!(outcome.payload.get("error").is_none(), "{outcome:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incremental_stdio_applies_ordered_utf16_unicode_and_crlf_edits() {
    let dir = TestDir::new("unicode");
    let old = "😀 e\u{301}\r\nsecond\r\n";
    let path = write_workspace(dir.path(), old);
    let log = dir.path().join("mock.jsonl");
    let (pool, manager, config) = manager_with_mock(
        dir.path(),
        json!({
            "textDocumentSync": 2,
            "positionEncoding": "utf-16",
            "semanticFromDocument": true,
            "logPath": log,
        }),
    );
    open_document(&manager, dir.path(), &path).await;

    let new = "😀 ok\r\nsecond!\r\n";
    std::fs::write(&path, new).expect("write changed source");
    let patch = CodeIntelPatch::new(
        old,
        vec![
            // 😀 (2 UTF-16 units), space, e, combining mark (1 each).
            edit(0, "😀 ", "😀 e\u{301}", "ok"),
            edit(1, "second", "second", "!"),
        ],
    );
    manager
        .notify_file_updated(
            dir.path(),
            &path,
            CodeIntelFileUpdate {
                text: new.into(),
                patch: Some(patch),
            },
        )
        .await;

    let with_inserted_line = "😀 ok\r\nsecond!\r\nthird!\r\n";
    std::fs::write(&path, with_inserted_line).expect("insert line");
    manager
        .notify_file_updated(
            dir.path(),
            &path,
            CodeIntelFileUpdate {
                text: with_inserted_line.into(),
                patch: Some(CodeIntelPatch::new(
                    new,
                    vec![edit(1, "second!", "second!", "\r\nthird!")],
                )),
            },
        )
        .await;

    let without_inserted_line = "😀 ok\r\nsecond!third!\r\n";
    std::fs::write(&path, without_inserted_line).expect("remove line");
    manager
        .notify_file_updated(
            dir.path(),
            &path,
            CodeIntelFileUpdate {
                text: without_inserted_line.into(),
                patch: Some(CodeIntelPatch::new(
                    with_inserted_line,
                    vec![edit_between(1, "second!", 2, "third!", "third!")],
                )),
            },
        )
        .await;

    let state = server_state(&pool, dir.path(), &config, &path).await;
    assert_eq!(state["text"], without_inserted_line);
    assert_eq!(state["version"], 4);
    let semantic_query = query(dir.path(), &path);
    let (definition, hover) = tokio::join!(
        manager.definition(&semantic_query),
        manager.hover(&semantic_query),
    );
    assert!(definition.payload.get("error").is_none(), "{definition:?}");
    assert_eq!(hover.payload["text"], without_inserted_line);
    let messages = client_messages(&log);
    let changes: Vec<_> = messages
        .iter()
        .filter(|message| message["method"] == "textDocument/didChange")
        .collect();
    assert_eq!(changes.len(), 3);
    assert_eq!(changes[0]["params"]["textDocument"]["version"], 2);
    assert_eq!(changes[1]["params"]["textDocument"]["version"], 3);
    assert_eq!(changes[2]["params"]["textDocument"]["version"], 4);
    assert_eq!(
        changes[0]["params"]["contentChanges"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incremental_stdio_honors_utf8_positions_for_multibyte_text() {
    let dir = TestDir::new("utf8");
    let old = "café\n";
    let path = write_workspace(dir.path(), old);
    let log = dir.path().join("mock.jsonl");
    let (pool, manager, config) = manager_with_mock(
        dir.path(),
        json!({
            "textDocumentSync": 2,
            "positionEncoding": "utf-8",
            "logPath": log,
        }),
    );
    open_document(&manager, dir.path(), &path).await;
    let new = "café!\n";
    std::fs::write(&path, new).expect("write changed source");
    manager
        .notify_file_updated(
            dir.path(),
            &path,
            CodeIntelFileUpdate {
                text: new.into(),
                patch: Some(CodeIntelPatch::new(old, vec![edit(0, "café", "café", "!")])),
            },
        )
        .await;
    let state = server_state(&pool, dir.path(), &config, &path).await;
    assert_eq!(state["text"], new);
    assert_eq!(state["version"], 2);
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_unknown_and_none_capabilities_follow_safe_contracts() {
    for (label, capability, sends_change) in [
        ("full", json!(1), true),
        ("unknown", json!(99), true),
        ("none", json!(0), false),
    ] {
        let dir = TestDir::new(label);
        let old = "fn main() {\n    let value = 1;\n}\n";
        let path = write_workspace(dir.path(), old);
        let log = dir.path().join("mock.jsonl");
        let (pool, manager, config) = manager_with_mock(
            dir.path(),
            json!({ "textDocumentSync": capability, "logPath": log }),
        );
        open_document(&manager, dir.path(), &path).await;
        let new = "fn main() {\n    let value = 42;\n}\n";
        std::fs::write(&path, new).expect("write changed source");
        manager
            .notify_file_updated(
                dir.path(),
                &path,
                CodeIntelFileUpdate {
                    text: new.into(),
                    patch: Some(CodeIntelPatch::new(
                        old,
                        vec![edit(1, "    let value = ", "    let value = 1", "42")],
                    )),
                },
            )
            .await;
        let state = server_state(&pool, dir.path(), &config, &path).await;
        let messages = client_messages(&log);
        let changes: Vec<_> = messages
            .iter()
            .filter(|message| message["method"] == "textDocument/didChange")
            .collect();
        assert_eq!(changes.len(), usize::from(sends_change), "{label}");
        if sends_change {
            assert!(changes[0]["params"]["contentChanges"][0]["range"].is_null());
            assert_eq!(changes[0]["params"]["contentChanges"][0]["text"], new);
            assert_eq!(changes[0]["params"]["textDocument"]["version"], 2);
        }
        if sends_change {
            assert_eq!(state["text"], new);
            assert_eq!(state["version"], 2);
        } else {
            assert_eq!(state["text"], old);
            assert_eq!(state["version"], 1);
            let outcome = manager.hover(&query(dir.path(), &path)).await;
            assert!(matches!(
                outcome.meta.state,
                CodeIntelServerState::Unavailable | CodeIntelServerState::Degraded
            ));
            assert!(outcome.payload.get("error").is_some());
        }
        pool.close_all().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incremental_save_omits_full_text_and_falls_back_on_digest_mismatch() {
    let dir = TestDir::new("save-fallback");
    let old = "fn main() {\n    let value = 1;\n}\n";
    let path = write_workspace(dir.path(), old);
    let log = dir.path().join("mock.jsonl");
    let (pool, manager, config) = manager_with_mock(
        dir.path(),
        json!({
            "textDocumentSync": { "change": 2, "save": true },
            "logPath": log,
        }),
    );
    open_document(&manager, dir.path(), &path).await;

    let new = "fn main() {\n    let value = 42;\n}\n";
    std::fs::write(&path, new).expect("write changed source");
    // A wrong digest deliberately exercises the complete-change fallback.
    manager
        .notify_file_updated(
            dir.path(),
            &path,
            CodeIntelFileUpdate {
                text: new.into(),
                patch: Some(CodeIntelPatch::new(
                    "different snapshot",
                    vec![edit(1, "    let value = ", "    let value = 1", "42")],
                )),
            },
        )
        .await;
    let state = server_state(&pool, dir.path(), &config, &path).await;
    assert_eq!(state["text"], new);
    let messages = client_messages(&log);
    let save = messages
        .iter()
        .find(|message| message["method"] == "textDocument/didSave")
        .expect("didSave");
    assert!(save["params"].get("text").is_none());
    let change = messages
        .iter()
        .find(|message| message["method"] == "textDocument/didChange")
        .expect("didChange");
    assert!(change["params"]["contentChanges"][0]["range"].is_null());
    assert_eq!(change["params"]["textDocument"]["version"], 2);
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_reopens_document_with_full_did_open() {
    let dir = TestDir::new("restart");
    let old = "fn old() {}\nfn main() { old(); }\n";
    let path = write_workspace(dir.path(), old);
    let log = dir.path().join("mock.jsonl");
    let (pool, manager, config) =
        manager_with_mock(dir.path(), json!({ "textDocumentSync": 2, "logPath": log }));
    open_document(&manager, dir.path(), &path).await;

    let lease = pool
        .acquire_warm(
            &std::fs::canonicalize(dir.path()).expect("canonical root"),
            "rust-analyzer",
            &config,
        )
        .await
        .expect("warm mock server");
    assert!(lease
        .instance()
        .request_value("mock/crash", Value::Null)
        .await
        .is_err());
    drop(lease);

    let new = "fn new() {}\nfn main() { new(); }\n";
    std::fs::write(&path, new).expect("write after restart");
    let mut restarted = false;
    for _attempt in 0..20 {
        let outcome = manager.definition(&query(dir.path(), &path)).await;
        if outcome.payload.get("error").is_none() {
            restarted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(restarted, "replacement mock server did not become ready");
    let state = server_state(&pool, dir.path(), &config, &path).await;
    assert_eq!(state["text"], new);
    assert_eq!(state["version"], 1);
    let messages = client_messages(&log);
    assert!(messages.iter().any(|message| {
        message["method"] == "textDocument/didOpen"
            && message["params"]["textDocument"]["version"] == 1
    }));
    pool.close_all().await;
}

#[derive(Debug)]
struct BenchmarkSample {
    change_json: usize,
    save_json: usize,
    change_frame: usize,
    save_frame: usize,
    sync_elapsed: Duration,
    definition_elapsed: Duration,
    total_elapsed: Duration,
    semantic_text: String,
}

fn frame_bytes(message: &Value) -> (usize, usize) {
    let body = serde_json::to_vec(message).expect("serialize JSON-RPC message");
    (
        body.len(),
        body.len() + format!("Content-Length: {}\r\n\r\n", body.len()).len(),
    )
}

fn benchmark_document(
    size: usize,
    replacement_bytes: usize,
) -> (String, String, Vec<CodeIntelTextEdit>) {
    let mut old = format!(
        "fn old() {{}}\n{}",
        "// unchanged line\n".repeat(size / 18 + 1)
    );
    old.truncate(size);
    let replacement = if replacement_bytes == 8 {
        "new_func".to_owned()
    } else {
        format!("new_func/*{}*/", "x".repeat(replacement_bytes - 12))
    };
    assert_eq!(replacement.len(), replacement_bytes);
    let new = old.replacen("old", &replacement, 1);
    let edits = vec![edit(0, "fn ", "fn old", &replacement)];
    (old, new, edits)
}

async fn run_payload_case(
    size: usize,
    replacement_bytes: usize,
    incremental: bool,
) -> BenchmarkSample {
    let dir = TestDir::new("bench-wire");
    let log = dir.path().join("mock.jsonl");
    let (old, new, edits) = benchmark_document(size, replacement_bytes);
    let path = write_workspace(dir.path(), &old);
    let (pool, manager, config) = manager_with_mock(
        dir.path(),
        json!({
            "textDocumentSync": {"change": if incremental {2} else {1}, "save":{"includeText":false}},
            "semanticFromDocument":true, "logPath":log,
        }),
    );
    open_document(&manager, dir.path(), &path).await;
    std::fs::write(&path, &new).expect("write benchmark source");
    let update = CodeIntelFileUpdate {
        text: new.clone(),
        patch: Some(CodeIntelPatch::new(&old, edits)),
    };
    let semantic_query = query(dir.path(), &path);
    let start = Instant::now();
    manager.notify_file_updated(dir.path(), &path, update).await;
    let sync_elapsed = start.elapsed();
    let definition_start = Instant::now();
    let definition = manager.definition(&semantic_query).await;
    let definition_elapsed = definition_start.elapsed();
    let total_elapsed = start.elapsed();
    assert!(definition.payload.get("error").is_none(), "{definition:?}");
    assert!(!definition.meta.stale);
    assert_eq!(definition.meta.document_version, Some(2));
    assert!(definition.payload["preview"]
        .as_str()
        .unwrap()
        .contains("new_func"));
    // Independent server-owned text proves the remote document changed;
    // local preview alone is not evidence of remote semantic equivalence.
    let hover = manager.hover(&semantic_query).await;
    assert!(hover.payload.get("error").is_none(), "{hover:?}");
    let semantic_text = hover.payload["text"].as_str().unwrap().to_owned();
    assert!(semantic_text.starts_with("fn new_func"));
    let state = server_state(&pool, dir.path(), &config, &path).await;
    assert_eq!(state["text"], new);
    assert_eq!(state["version"], 2);
    let messages = client_messages(&log);
    let change = messages
        .iter()
        .find(|m| m["method"] == "textDocument/didChange")
        .unwrap();
    let save = messages
        .iter()
        .find(|m| m["method"] == "textDocument/didSave")
        .unwrap();
    assert!(save["params"].get("text").is_none());
    let (change_json, change_frame) = frame_bytes(change);
    let (save_json, save_frame) = frame_bytes(save);
    pool.close_all().await;
    BenchmarkSample {
        change_json,
        save_json,
        change_frame,
        save_frame,
        sync_elapsed,
        definition_elapsed,
        total_elapsed,
        semantic_text,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "release benchmark; run explicitly without concurrent Cargo work"]
async fn release_payload_benchmark_shows_incremental_wire_scaling() {
    fn stats(samples: &[BenchmarkSample], get: impl Fn(&BenchmarkSample) -> Duration) -> String {
        let mut values: Vec<_> = samples
            .iter()
            .map(|sample| get(sample).as_micros())
            .collect();
        values.sort_unstable();
        format!(
            "{}[{}..{}]",
            values[values.len() / 2],
            values[0],
            values[values.len() - 1]
        )
    }
    for size in [1024, 64 * 1024, 1024 * 1024] {
        for replacement_bytes in [8, 4096] {
            let mut full = Vec::new();
            let mut incremental = Vec::new();
            for sample in 0..7 {
                for mode in [sample % 2 == 0, sample % 2 != 0] {
                    let result = run_payload_case(size, replacement_bytes, mode).await;
                    if mode {
                        incremental.push(result);
                    } else {
                        full.push(result);
                    }
                }
            }
            let expected = &full[0].semantic_text;
            assert!(full
                .iter()
                .chain(&incremental)
                .all(|sample| &sample.semantic_text == expected));
            for (mode, samples) in [("full", &full), ("incremental", &incremental)] {
                let first = &samples[0];
                println!("sync_bench size={size} replacement_bytes={replacement_bytes} mode={mode} n=7 change_json={} save_json={} total_json={} change_frame={} save_frame={} total_frame={} sync_us={} definition_us={} sync_plus_definition_us={} semantic_equivalent=true",
                    first.change_json,first.save_json,first.change_json+first.save_json,
                    first.change_frame,first.save_frame,first.change_frame+first.save_frame,
                    stats(samples,|s|s.sync_elapsed),stats(samples,|s|s.definition_elapsed),stats(samples,|s|s.total_elapsed));
            }
            assert!(incremental[0].change_frame < full[0].change_frame);
        }
    }
}
