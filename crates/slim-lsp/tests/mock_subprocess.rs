use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::codeintel::{
    CodeIntelCompleteness, CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery,
    CodeIntelligence,
};
use slim_core::runtime::CancellationToken;
use slim_lsp::discovery::ServerSpec;
use slim_lsp::pool::{PoolConfig, ProcessFactory, SpawnedServer, StdioProcessFactory};
use slim_lsp::{LspCodeIntelligence, LspManagerConfig, LspProcessPool, TransportOptions};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("slim-lsp-{label}-{}-{nonce}", std::process::id()));
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

#[derive(Default)]
struct CountingFactory {
    spawns: AtomicUsize,
}

impl ProcessFactory for CountingFactory {
    fn spawn(&self, spec: &ServerSpec, root: &std::path::Path) -> Result<SpawnedServer, String> {
        self.spawns.fetch_add(1, Ordering::AcqRel);
        StdioProcessFactory.spawn(spec, root)
    }
}

fn mock_binary() -> String {
    env!("CARGO_BIN_EXE_slim-lsp-mock").to_owned()
}

fn mock_spec(args: Vec<String>) -> ServerSpec {
    ServerSpec {
        id: "rust-analyzer".into(),
        label: "Slim test LSP".into(),
        command: mock_binary(),
        args,
        root_markers: vec!["Cargo.toml".into()],
        language_ids: vec![("rs".into(), "rust".into())],
        settings_section: "rust-analyzer".into(),
    }
}

fn transport_options() -> TransportOptions {
    TransportOptions {
        request_timeout: Duration::from_secs(3),
        ..TransportOptions::default()
    }
}

fn write_workspace(root: &Path) -> (PathBuf, PathBuf) {
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='mock-workspace'\nversion='0.1.0'\nedition='2021'\n",
    )
    .expect("write Cargo.toml");
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("create src");
    let first = src.join("first.rs");
    let second = src.join("second.rs");
    std::fs::write(&first, "fn first() {}\n").expect("write first");
    std::fs::write(&second, "fn second() {}\n").expect("write second");
    (first, second)
}

fn read_client_methods(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("read mock log");
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid JSONL"))
        .filter(|row| row.get("direction").and_then(Value::as_str) == Some("client_to_server"))
        .filter_map(|row| row.get("message").cloned())
        .collect()
}

async fn wait_for_client_method(path: &Path, method: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if path.is_file()
                && read_client_methods(path)
                    .iter()
                    .any(|message| message.get("method").and_then(Value::as_str) == Some(method))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("mock method appears in protocol log");
}

fn manager_with_mock(
    root: &Path,
    server_config: Value,
) -> (Arc<LspProcessPool>, Arc<LspCodeIntelligence>) {
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 2,
        factory: Arc::new(StdioProcessFactory),
    });
    let manager = LspCodeIntelligence::new(
        pool.clone(),
        LspManagerConfig {
            idle_shutdown: None,
            max_servers: 2,
            request_timeout: Duration::from_secs(3),
            server_config,
            max_open_documents: 8,
            server_path: Some(PathBuf::from(mock_binary())),
        },
    );
    assert!(root.join("Cargo.toml").is_file());
    (pool, manager)
}

fn position_query(workspace: &Path, path: PathBuf) -> CodeIntelPositionQuery {
    CodeIntelPositionQuery {
        workspace: workspace.to_path_buf(),
        path,
        line: 1,
        column: 1,
        symbol: Some("main".into()),
        max_results: 20,
        cancellation: None,
    }
}

async fn wait_for_leases(pool: &LspProcessPool, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while pool.active_leases() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lease count converges");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_subprocess_observes_dedup_and_lru_did_close() {
    let dir = TestDir::new("protocol");
    let (first, second) = write_workspace(dir.path());
    let log = dir.path().join("mock.jsonl");
    let spec = mock_spec(vec!["--log".into(), log.to_string_lossy().into_owned()]);
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 2,
        factory: Arc::new(StdioProcessFactory),
    });
    let lease = pool
        .acquire(
            dir.path().to_path_buf(),
            spec,
            &json!({}),
            transport_options(),
            1,
        )
        .await
        .expect("acquire mock LSP");
    let instance = lease.instance();

    assert_eq!(
        instance
            .sync_document(&first, "fn first() {}\n".into())
            .await,
        Some(1)
    );
    assert_eq!(
        instance
            .sync_document(&first, "fn first() {}\n".into())
            .await,
        Some(1),
        "identical content must not produce didChange"
    );
    assert_eq!(
        instance
            .sync_document(&second, "fn second() {}\n".into())
            .await,
        Some(1)
    );
    instance
        .request_value("mock/barrier", json!({}))
        .await
        .expect("protocol barrier");
    drop(lease);
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;

    let messages = read_client_methods(&log);
    let methods: Vec<&str> = messages
        .iter()
        .filter_map(|message| message.get("method").and_then(Value::as_str))
        .collect();
    assert_eq!(
        methods
            .iter()
            .filter(|method| **method == "textDocument/didOpen")
            .count(),
        2
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| **method == "textDocument/didChange")
            .count(),
        0
    );
    let close_index = methods
        .iter()
        .position(|method| *method == "textDocument/didClose")
        .expect("LRU eviction sends didClose");
    let second_open_index = methods
        .iter()
        .rposition(|method| *method == "textDocument/didOpen")
        .expect("second didOpen");
    assert!(
        close_index < second_open_index,
        "didClose precedes replacement didOpen"
    );
    assert!(methods.contains(&"shutdown"));
    assert!(methods.contains(&"exit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_pool_key_initializes_once_and_counts_every_lease() {
    let dir = TestDir::new("singleflight");
    write_workspace(dir.path());
    let factory = Arc::new(CountingFactory::default());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 2,
        factory: factory.clone(),
    });
    let spec = mock_spec(vec!["--initialize-delay-ms".into(), "150".into()]);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let root = dir.path().to_path_buf();
        let spec = spec.clone();
        tasks.push(tokio::spawn(async move {
            pool.acquire(root, spec, &json!({}), transport_options(), 8)
                .await
                .expect("singleflight acquire")
        }));
    }
    let mut leases = Vec::new();
    for task in tasks {
        leases.push(task.await.expect("acquire task"));
    }
    assert_eq!(factory.spawns.load(Ordering::Acquire), 1);
    assert_eq!(pool.running_servers().await, 1);
    assert_eq!(pool.active_leases(), 8);

    leases.pop();
    wait_for_leases(&pool, 7).await;
    drop(leases);
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_all_waits_for_inflight_initialize_and_prevents_reinsert() {
    let dir = TestDir::new("close-starting");
    write_workspace(dir.path());
    let factory = Arc::new(CountingFactory::default());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory: factory.clone(),
    });
    let log = dir.path().join("close-starting.jsonl");
    let spec = mock_spec(vec![
        "--log".into(),
        log.to_string_lossy().into_owned(),
        "--initialize-delay-ms".into(),
        "250".into(),
    ]);
    let acquire = {
        let pool = pool.clone();
        let root = dir.path().to_path_buf();
        let spec = spec.clone();
        tokio::spawn(async move {
            pool.acquire(root, spec, &json!({}), transport_options(), 8)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while factory.spawns.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup begins");

    pool.close_all().await;
    let methods = read_client_methods(&log);
    assert!(methods
        .iter()
        .any(|message| { message.get("method").and_then(Value::as_str) == Some("shutdown") }));
    assert!(methods
        .iter()
        .any(|message| message.get("method").and_then(Value::as_str) == Some("exit")));
    assert!(tokio::time::timeout(Duration::from_secs(1), acquire)
        .await
        .expect("initializing acquire completes with close_all")
        .expect("acquire task")
        .is_err());
    assert_eq!(pool.running_servers().await, 0);
    assert_eq!(pool.active_leases(), 0);
    assert!(
        pool.acquire(
            dir.path().to_path_buf(),
            spec,
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .is_err(),
        "closed pool must not start a replacement server"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_pool_keys_initialize_concurrently() {
    let first = TestDir::new("parallel-a");
    let second = TestDir::new("parallel-b");
    write_workspace(first.path());
    write_workspace(second.path());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 2,
        factory: Arc::new(StdioProcessFactory),
    });
    let spec = mock_spec(vec!["--initialize-delay-ms".into(), "500".into()]);
    let started = Instant::now();
    let left = {
        let pool = pool.clone();
        let spec = spec.clone();
        let root = first.path().to_path_buf();
        tokio::spawn(async move {
            pool.acquire(root, spec, &json!({}), transport_options(), 8)
                .await
        })
    };
    let right = {
        let pool = pool.clone();
        let root = second.path().to_path_buf();
        tokio::spawn(async move {
            pool.acquire(root, spec, &json!({}), transport_options(), 8)
                .await
        })
    };
    let left = left.await.expect("left task").expect("left acquire");
    let right = right.await.expect("right task").expect("right acquire");
    assert!(
        started.elapsed() < Duration::from_millis(850),
        "independent initialize calls were serialized: {:?}",
        started.elapsed()
    );
    drop((left, right));
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_keeps_lease_for_the_entire_slow_query() {
    let dir = TestDir::new("manager-lease");
    let (first, _) = write_workspace(dir.path());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: Some(Duration::ZERO),
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory: Arc::new(StdioProcessFactory),
    });
    let manager = LspCodeIntelligence::new(
        pool.clone(),
        LspManagerConfig {
            idle_shutdown: Some(Duration::ZERO),
            max_servers: 1,
            request_timeout: Duration::from_secs(3),
            server_config: json!({ "mock": { "requestDelayMs": 500 } }),
            max_open_documents: 8,
            server_path: Some(PathBuf::from(mock_binary())),
        },
    );
    let query = CodeIntelPositionQuery {
        workspace: dir.path().to_path_buf(),
        path: first,
        line: 1,
        column: 1,
        symbol: Some("main".into()),
        max_results: 20,
        cancellation: None,
    };
    let query_task = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.hover(&query).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while pool.running_servers().await == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server starts");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        pool.active_leases(),
        1,
        "query must retain its lease while awaiting the response"
    );
    let outcome = query_task.await.expect("query task");
    assert_eq!(outcome.payload.get("found"), Some(&Value::Bool(true)));
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_idle_shutdown_sends_exit_and_reaps_the_server() {
    let dir = TestDir::new("idle-shutdown");
    write_workspace(dir.path());
    let log = dir.path().join("idle.jsonl");
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: Some(Duration::ZERO),
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory: Arc::new(StdioProcessFactory),
    });
    let lease = pool
        .acquire(
            dir.path().to_path_buf(),
            mock_spec(vec!["--log".into(), log.to_string_lossy().into_owned()]),
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("server lease");
    drop(lease);
    wait_for_leases(&pool, 0).await;
    wait_for_client_method(&log, "shutdown").await;
    wait_for_client_method(&log, "exit").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while pool.running_servers().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle server removed and reaped");
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_result_outside_workspace_is_rejected() {
    let workspace = TestDir::new("path-output");
    let outside = TestDir::new("path-outside");
    let (source, _) = write_workspace(workspace.path());
    let secret = outside.path().join("secret.rs");
    std::fs::write(&secret, "fn secret() {}\n").expect("write outside file");
    let definition_uri = url::Url::from_file_path(&secret)
        .expect("outside file URI")
        .to_string();
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({ "mock": { "definitionUri": definition_uri } }),
    );

    let outcome = manager
        .definition(&position_query(workspace.path(), source))
        .await;
    assert_eq!(outcome.payload.get("found"), Some(&Value::Bool(false)));
    assert_eq!(outcome.meta.document_version, Some(1));
    assert!(!outcome.meta.stale);
    assert!(
        !serde_json::to_string(&outcome.payload)
            .expect("serialize payload")
            .contains("secret.rs"),
        "external server path must not leak into the result"
    );
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_query_reports_version_and_staleness_after_concurrent_write() {
    let workspace = TestDir::new("stale-query");
    let (source, _) = write_workspace(workspace.path());
    let log = workspace.path().join("stale.jsonl");
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({
            "mock": {
                "logPath": log.to_string_lossy(),
                "requestDelayMs": 500
            }
        }),
    );
    let query = position_query(workspace.path(), source.clone());
    let task = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.hover(&query).await })
    };
    wait_for_client_method(&log, "textDocument/hover").await;
    std::fs::write(&source, "fn changed_during_query() {}\n").expect("mutate source");

    let outcome = task.await.expect("hover task");
    assert_eq!(outcome.meta.document_version, Some(1));
    assert!(outcome.meta.stale);
    assert_eq!(outcome.payload.get("found"), Some(&Value::Bool(true)));
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_cancellation_returns_promptly_and_reaches_subprocess() {
    let workspace = TestDir::new("cancel-query");
    let (source, _) = write_workspace(workspace.path());
    let log = workspace.path().join("cancel.jsonl");
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({
            "mock": {
                "logPath": log.to_string_lossy(),
                "requestDelayMs": 500
            }
        }),
    );
    let cancellation = CancellationToken::new();
    let mut query = position_query(workspace.path(), source);
    query.cancellation = Some(cancellation.clone());
    let task = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.hover(&query).await })
    };
    wait_for_client_method(&log, "textDocument/hover").await;
    let cancelled_at = Instant::now();
    cancellation.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("query cancellation must be prompt")
        .expect("hover task");
    assert!(cancelled_at.elapsed() < Duration::from_millis(300));
    assert_eq!(outcome.meta.document_version, Some(1));
    assert!(outcome
        .payload
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|error| error.contains("cancelled")));
    wait_for_client_method(&log, "$/cancelRequest").await;
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_write_notifications_are_ordered_and_versions_are_monotonic() {
    let workspace = TestDir::new("post-write-order");
    let (source, _) = write_workspace(workspace.path());
    let log = workspace.path().join("writes.jsonl");
    let server_config = json!({ "mock": { "logPath": log.to_string_lossy() } });
    let (pool, manager) = manager_with_mock(workspace.path(), server_config.clone());

    manager
        .hover(&position_query(workspace.path(), source.clone()))
        .await;
    std::fs::write(&source, "fn version_two() {}\n").expect("write v2");
    manager.notify_file_changed(workspace.path(), &source, None).await;
    std::fs::write(&source, "fn version_three() {}\n").expect("write v3");
    manager.notify_file_changed(workspace.path(), &source, None).await;

    let canonical_root = std::fs::canonicalize(workspace.path()).expect("canonical root");
    let warm = pool
        .acquire_warm(&canonical_root, "rust-analyzer", &server_config)
        .await
        .expect("warm server lease");
    warm.instance()
        .request_value("mock/barrier", json!({}))
        .await
        .expect("protocol barrier");
    drop(warm);
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;

    let writes: Vec<Value> = read_client_methods(&log)
        .into_iter()
        .filter(|message| {
            matches!(
                message.get("method").and_then(Value::as_str),
                Some("textDocument/didChange" | "textDocument/didSave")
            )
        })
        .collect();
    let methods: Vec<&str> = writes
        .iter()
        .filter_map(|message| message.get("method").and_then(Value::as_str))
        .collect();
    assert_eq!(
        methods,
        vec![
            "textDocument/didChange",
            "textDocument/didSave",
            "textDocument/didChange",
            "textDocument/didSave"
        ]
    );
    let versions: Vec<i64> = writes
        .iter()
        .filter(|message| {
            message.get("method").and_then(Value::as_str) == Some("textDocument/didChange")
        })
        .filter_map(|message| message.pointer("/params/textDocument/version")?.as_i64())
        .collect();
    assert_eq!(versions, vec![2, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_server_is_evicted_and_restarted_on_next_acquire() {
    let dir = TestDir::new("dead-restart");
    write_workspace(dir.path());
    let factory = Arc::new(CountingFactory::default());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory: factory.clone(),
    });
    let spec = mock_spec(Vec::new());
    let lease = pool
        .acquire(
            dir.path().to_path_buf(),
            spec.clone(),
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("first acquire");
    // Kill the server behind the pool's back: the mock breaks its read loop
    // on "exit" without answering, so this request resolves once EOF fails
    // every pending entry.
    let exited = lease.instance().request_value("exit", json!(null)).await;
    assert!(exited.is_err(), "exit must not be answered: {exited:?}");
    assert!(
        lease.instance().is_closed(),
        "transport must observe the exit"
    );
    drop(lease);
    wait_for_leases(&pool, 0).await;
    let lease = pool
        .acquire(
            dir.path().to_path_buf(),
            spec,
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("reacquire after death starts a fresh server");
    assert_eq!(factory.spawns.load(Ordering::Acquire), 2);
    lease
        .instance()
        .request_value("mock/barrier", json!({}))
        .await
        .expect("fresh server answers");
    drop(lease);
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_document_symbols_carry_file_and_position() {
    let workspace = TestDir::new("nested-symbols");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({ "mock": { "documentSymbolNested": true } }),
    );
    let outcome = manager
        .symbols(&CodeIntelSymbolQuery {
            workspace: workspace.path().to_path_buf(),
            path: Some(source),
            query: None,
            max_results: 20,
            cancellation: None,
        })
        .await;
    let symbols = outcome.payload.get("symbols").and_then(Value::as_array);
    assert_eq!(
        outcome.payload.get("kind").and_then(Value::as_str),
        Some("document")
    );
    let symbols = symbols.expect("nested symbols payload");
    assert_eq!(symbols.len(), 2);
    assert!(symbols[0]
        .get("file")
        .and_then(Value::as_str)
        .is_some_and(|file| file.ends_with("first.rs")));
    assert_eq!(symbols[0].get("line").and_then(Value::as_u64), Some(1));
    assert_eq!(symbols[0].get("column").and_then(Value::as_u64), Some(4));
    assert!(symbols[1]
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name.contains("inner")));
    assert_eq!(symbols[1].get("line").and_then(Value::as_u64), Some(1));
    assert_eq!(symbols[1].get("column").and_then(Value::as_u64), Some(7));
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queries_during_indexing_report_partial_completeness() {
    let workspace = TestDir::new("indexing-meta");
    let (source, _) = write_workspace(workspace.path());
    let server_config = json!({});
    let (pool, manager) = manager_with_mock(workspace.path(), server_config.clone());
    let baseline = manager
        .hover(&position_query(workspace.path(), source.clone()))
        .await;
    assert_eq!(baseline.meta.state, CodeIntelServerState::Ready);
    assert_eq!(baseline.meta.completeness, CodeIntelCompleteness::Complete);

    // Simulate the server starting an indexing pass behind the manager.
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonical root");
    let warm = pool
        .acquire_warm(&canonical, "rust-analyzer", &server_config)
        .await
        .expect("warm server lease");
    warm.instance()
        .request_value("mock/beginIndexing", json!({}))
        .await
        .expect("begin indexing");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if warm.instance().snapshot().await.indexing_active {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("indexing flag observed");
    drop(warm);

    let outcome = manager
        .hover(&position_query(workspace.path(), source))
        .await;
    assert_eq!(outcome.meta.state, CodeIntelServerState::Indexing);
    assert_eq!(outcome.meta.completeness, CodeIntelCompleteness::Partial);
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn off_runtime_final_drop_arms_idle_on_next_acquire() {
    let first = TestDir::new("orphan-a");
    let second = TestDir::new("orphan-b");
    write_workspace(first.path());
    write_workspace(second.path());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: Some(Duration::from_millis(50)),
        circuit_window: Duration::from_millis(10),
        max_servers: 2,
        factory: Arc::new(StdioProcessFactory),
    });
    let spec = mock_spec(Vec::new());
    let lease = pool
        .acquire(
            first.path().to_path_buf(),
            spec.clone(),
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("first acquire");
    // Final drop outside any runtime: the idle arming cannot be spawned.
    std::thread::spawn(move || drop(lease))
        .join()
        .expect("drop thread");
    assert_eq!(pool.running_servers().await, 1);

    // Activity on another key consumes the orphaned release and arms its
    // timer; nothing touches the first server again, so idle shutdown must
    // fire for both without close_all.
    let lease = pool
        .acquire(
            second.path().to_path_buf(),
            spec,
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("second acquire");
    drop(lease);
    tokio::time::timeout(Duration::from_secs(3), async {
        while pool.running_servers().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle shutdown fires without further pool activity");
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_servers_do_not_consume_process_slots() {
    let first = TestDir::new("slot-a");
    let second = TestDir::new("slot-b");
    write_workspace(first.path());
    write_workspace(second.path());
    let factory = Arc::new(CountingFactory::default());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory: factory.clone(),
    });
    let spec = mock_spec(Vec::new());
    let lease = pool
        .acquire(
            first.path().to_path_buf(),
            spec.clone(),
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("first acquire");
    drop(lease);
    wait_for_leases(&pool, 0).await;
    // The second workspace must evict the idle first server, not fail.
    let lease = pool
        .acquire(
            second.path().to_path_buf(),
            spec,
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("idle slot reclaimed under pressure");
    assert_eq!(pool.running_servers().await, 1);
    assert_eq!(factory.spawns.load(Ordering::Acquire), 2);
    lease
        .instance()
        .request_value("mock/barrier", json!({}))
        .await
        .expect("second server answers");
    drop(lease);
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_server_starts_with_workspace_as_working_directory() {
    let dir = TestDir::new("server-cwd");
    write_workspace(dir.path());
    let log = dir.path().join("cwd.jsonl");
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory: Arc::new(StdioProcessFactory),
    });
    let lease = pool
        .acquire(
            dir.path().to_path_buf(),
            mock_spec(vec!["--log".into(), log.to_string_lossy().into_owned()]),
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("acquire mock LSP");
    drop(lease);
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;

    let text = std::fs::read_to_string(&log).expect("read mock log");
    let startup = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid JSONL"))
        .find(|row| row.get("event").and_then(Value::as_str) == Some("mock_startup"))
        .expect("startup row");
    let cwd = startup
        .get("cwd")
        .and_then(Value::as_str)
        .expect("cwd recorded");
    assert_eq!(
        std::fs::canonicalize(cwd).expect("canonical child cwd"),
        std::fs::canonicalize(dir.path()).expect("canonical root"),
        "server working directory must be the workspace root"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_after_shutdown_reports_stopped() {
    let workspace = TestDir::new("stopped-status");
    write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(workspace.path(), json!({}));
    manager.shutdown().await;
    let outcome = manager.status(workspace.path()).await;
    assert_eq!(outcome.meta.state, CodeIntelServerState::Stopped);
    assert!(outcome
        .payload
        .get("summary")
        .and_then(Value::as_str)
        .is_some_and(|summary| summary.contains("shut down")));
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn older_diagnostics_cannot_replace_current_document_version() {
    let workspace = TestDir::new("diagnostic-version");
    let (source, _) = write_workspace(workspace.path());
    let spec = mock_spec(Vec::new());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory: Arc::new(StdioProcessFactory),
    });
    let lease = pool
        .acquire(
            workspace.path().to_path_buf(),
            spec,
            &json!({}),
            transport_options(),
            8,
        )
        .await
        .expect("acquire mock server");
    let instance = lease.instance();
    assert_eq!(
        instance
            .sync_document(&source, "fn first() {}\n".into())
            .await,
        Some(1)
    );
    let uri = url::Url::from_file_path(std::fs::canonicalize(&source).expect("canonical source"))
        .expect("source URI");
    let diagnostic = |version: i64, message: &str| {
        json!({
            "uri": uri.to_string(),
            "version": version,
            "diagnostics": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "severity": 2,
                "source": "mock",
                "message": message
            }]
        })
    };
    instance
        .request_value("mock/publishDiagnostics", diagnostic(1, "current"))
        .await
        .expect("publish current diagnostics");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if instance
                .diagnostics_snapshot(&uri, false)
                .await
                .is_some_and(|snapshot| snapshot.version == Some(1))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("current diagnostics stored");
    instance
        .request_value("mock/publishDiagnostics", diagnostic(0, "old"))
        .await
        .expect("publish old diagnostics");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let stored = instance
        .diagnostics_snapshot(&uri, false)
        .await
        .expect("current diagnostics remain");
    assert_eq!(stored.version, Some(1));
    assert_eq!(stored.items[0].message, "current");
    drop(lease);
    wait_for_leases(&pool, 0).await;
    pool.close_all().await;
}
