use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::codeintel::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelPositionQuery, CodeIntelServerState,
    CodeIntelSymbolQuery, CodeIntelligence,
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
        offset: 0,
        revision: None,
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
async fn aborting_startup_leader_does_not_strand_followers_or_shutdown() {
    let dir = TestDir::new("abort-startup");
    write_workspace(dir.path());
    let log = dir.path().join("startup.jsonl");
    let factory = Arc::new(CountingFactory::default());
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        factory: factory.clone(),
        ..PoolConfig::default()
    });
    let spec = mock_spec(vec![
        "--log".into(),
        log.to_string_lossy().into_owned(),
        "--initialize-delay-ms".into(),
        "250".into(),
    ]);
    let leader = {
        let pool = pool.clone();
        let spec = spec.clone();
        let root = dir.path().to_path_buf();
        tokio::spawn(async move {
            pool.acquire(root, spec, &json!({}), transport_options(), 8)
                .await
        })
    };
    wait_for_client_method(&log, "initialize").await;
    leader.abort();
    assert!(matches!(leader.await, Err(error) if error.is_cancelled()));
    let follower = tokio::time::timeout(
        Duration::from_secs(2),
        pool.acquire(
            dir.path().to_path_buf(),
            spec,
            &json!({}),
            transport_options(),
            8,
        ),
    )
    .await;
    assert!(
        follower.is_ok(),
        "aborted startup stranded the single-flight entry"
    );
    let lease = follower
        .unwrap()
        .expect("follower acquires shared initialization");
    assert_eq!(factory.spawns.load(Ordering::Acquire), 1);
    assert_eq!(pool.active_leases(), 1);
    drop(lease);
    tokio::time::timeout(Duration::from_secs(2), pool.close_all())
        .await
        .expect("shutdown must not wait forever for an abandoned leader");
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
        offset: 0,
        revision: None,
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
    let server_config = json!({ "mock": { "logPath": log.to_string_lossy(), "textDocumentSync": {"openClose":true,"change":1,"save":true} } });
    let (pool, manager) = manager_with_mock(workspace.path(), server_config.clone());

    manager
        .hover(&position_query(workspace.path(), source.clone()))
        .await;
    std::fs::write(&source, "fn version_two() {}\n").expect("write v2");
    manager
        .notify_file_changed(workspace.path(), &source, None)
        .await;
    std::fs::write(&source, "fn version_three() {}\n").expect("write v3");
    manager
        .notify_file_changed(workspace.path(), &source, None)
        .await;

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
            offset: 0,
            revision: None,
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

    // The mock reports a completed startup indexing cycle like real
    // rust-analyzer; wait until that observation lands before asserting
    // completeness. Until then, `unknown` is the honest answer.
    let canonical = std::fs::canonicalize(workspace.path()).expect("canonical root");
    let warm = pool
        .acquire_warm(&canonical, "rust-analyzer", &server_config)
        .await
        .expect("warm server lease");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snap = warm.instance().snapshot().await;
            if snap.indexing_observed && !snap.indexing_active {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup indexing cycle observed");
    let settled = manager
        .hover(&position_query(workspace.path(), source.clone()))
        .await;
    assert_eq!(settled.meta.completeness, CodeIntelCompleteness::Complete);

    // Simulate the server starting an indexing pass behind the manager.
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
    assert!(manager.supports_workspace(workspace.path()));
    manager.shutdown().await;
    assert!(!manager.supports_workspace(workspace.path()));
    let outcome = manager.status(workspace.path()).await;
    assert_eq!(outcome.meta.state, CodeIntelServerState::Stopped);
    assert!(outcome
        .payload
        .get("summary")
        .and_then(Value::as_str)
        .is_some_and(|summary| summary.contains("shut down")));
    pool.close_all().await;
}

#[test]
fn workspace_support_tracks_project_markers_without_starting_a_server() {
    let workspace = TestDir::new("catalog-availability");
    let factory = Arc::new(CountingFactory::default());
    let pool = LspProcessPool::new(PoolConfig {
        factory: factory.clone(),
        ..PoolConfig::default()
    });
    let manager = LspCodeIntelligence::new(
        pool,
        LspManagerConfig {
            server_path: Some(PathBuf::from(mock_binary())),
            ..LspManagerConfig::default()
        },
    );
    assert!(!manager.supports_workspace(workspace.path()));
    write_workspace(workspace.path());
    assert!(manager.supports_workspace(workspace.path()));
    std::fs::remove_file(workspace.path().join("Cargo.toml")).expect("remove own marker");
    assert!(!manager.supports_workspace(workspace.path()));
    assert_eq!(factory.spawns.load(Ordering::Acquire), 0);
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

fn diagnostics_query(workspace: &Path, path: Option<PathBuf>) -> CodeIntelDiagnosticsQuery {
    CodeIntelDiagnosticsQuery {
        workspace: workspace.to_path_buf(),
        path,
        include_info: false,
        max_results: 20,
        cancellation: None,
    }
}

#[tokio::test]
async fn cached_queries_refresh_document_lru_without_reopening() {
    let workspace = TestDir::new("query-lru");
    let (first, second) = write_workspace(workspace.path());
    let third = workspace.path().join("src/third.rs");
    std::fs::write(&third, "fn third() {}\n").unwrap();
    let log = workspace.path().join("protocol.jsonl");
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        ..Default::default()
    });
    let manager = LspCodeIntelligence::new(
        pool.clone(),
        LspManagerConfig {
            max_open_documents: 2,
            server_path: Some(PathBuf::from(mock_binary())),
            server_config: json!({ "mock": { "logPath": log } }),
            ..Default::default()
        },
    );
    for path in [&first, &second, &first, &first, &third, &first] {
        let result = manager
            .hover(&position_query(workspace.path(), path.clone()))
            .await;
        assert!(result.payload.get("error").is_none());
    }
    let first_uri = url::Url::from_file_path(std::fs::canonicalize(&first).unwrap()).unwrap();
    let second_uri = url::Url::from_file_path(std::fs::canonicalize(&second).unwrap()).unwrap();
    let messages = read_client_methods(&log);
    let opens = messages
        .iter()
        .filter(|m| {
            m["method"] == "textDocument/didOpen"
                && m["params"]["textDocument"]["uri"] == first_uri.as_str()
        })
        .count();
    assert_eq!(opens, 1, "cache hits must keep A warm when C evicts B");
    let closed: Vec<_> = messages
        .iter()
        .filter(|m| m["method"] == "textDocument/didClose")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0]["params"]["textDocument"]["uri"],
        second_uri.as_str()
    );
    pool.close_all().await;
}

#[tokio::test]
async fn symbol_result_limits_are_explicit() {
    let workspace = TestDir::new("symbols-limit");
    let (source, _) = write_workspace(workspace.path());
    let uri = url::Url::from_file_path(std::fs::canonicalize(&source).unwrap()).unwrap();
    let flat = json!([
        { "name": "one", "kind": 12, "location": { "uri": uri.as_str(), "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 }}}},
        { "name": "two", "kind": 12, "location": { "uri": uri.as_str(), "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 2 }}}},
    ]);
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({ "mock": {
            "documentSymbolNested": true, "responses": { "workspace/symbol": flat },
        }}),
    );
    for path in [Some(source), None] {
        let mut query = CodeIntelSymbolQuery {
            workspace: workspace.path().into(),
            path,
            query: Some("main".into()),
            max_results: 1,
            offset: 0,
            revision: None,
            cancellation: None,
        };
        let limited = manager.symbols(&query).await;
        assert_eq!(limited.payload["shown"], 1);
        assert_eq!(limited.payload["has_more"], true);
        assert!(slim_core::tools::render_code_intel("symbol", &limited).contains("more results"));
        query.max_results = 2;
        let complete = manager.symbols(&query).await;
        assert_eq!(complete.payload["shown"], 2);
        assert_eq!(complete.payload["has_more"], false);
    }
    pool.close_all().await;
}

#[tokio::test]
async fn symbol_annotations_reach_the_model_from_semantic_results() {
    let workspace = TestDir::new("symbol-annotations");
    let (source, _) = write_workspace(workspace.path());
    std::fs::write(&source, "struct Parser;\r\nimpl Parser {\r\n    fn parse(value: &str) -> usize { value.len() }\r\n}\r\n").unwrap();
    let uri = url::Url::from_file_path(std::fs::canonicalize(&source).unwrap()).unwrap();
    let range = json!({"start":{"line":2,"character":7},"end":{"line":2,"character":12}});
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({"mock":{"responses":{
            "textDocument/documentSymbol":[{"name":"impl Parser","kind":19,
                "range":{"start":{"line":1,"character":0},"end":{"line":3,"character":1}},
                "selectionRange":{"start":{"line":1,"character":5},"end":{"line":1,"character":11}},
                "children":[{"name":"parse","kind":12,"range":range,"selectionRange":range,"detail":"fn(value: &str) -> usize"}]}],
        "workspace/symbol":[
            {"name":"parse","kind":12,"containerName":"Parser","location":{"uri":uri.as_str(),"range":range}},
            {"name":"unresolved","kind":12,"containerName":"Other","location":{"uri":uri.as_str()}}
        ]
        }}}),
    );
    for (args, expected) in [
        (
            json!({"action":"symbol","path":"src/first.rs","query":"parse"}),
            "detail: fn(value: &str) -> usize",
        ),
        (json!({"action":"symbol","query":"parse"}), "in: Parser"),
    ] {
        let slim_core::tools::CodeIntelRequest::Symbols(query) =
            slim_core::tools::parse_code_intel_request(workspace.path(), &args).unwrap()
        else {
            panic!("symbol request")
        };
        let outcome = manager.symbols(&query).await;
        assert!(!outcome.meta.stale);
        if query.path.is_some() {
            assert_eq!(outcome.payload["query"], "parse");
        }
        let rendered = slim_core::tools::render_code_intel("symbol", &outcome);
        assert!(rendered.contains(expected), "{rendered}");
        assert!(rendered.contains("first.rs:3:8"));
        if query.path.is_some() {
            assert!(rendered.contains("  parse"));
            assert!(rendered.contains("document_version: 1"));
        } else {
            assert!(rendered.contains("first.rs (position unavailable) | in: Other"));
        }
    }
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual release measurement; no network"]
async fn measure_symbol_pipeline_sizes() {
    use slim_core::provider::{
        OpenAiCodexAdapter, ProviderAdapter, ProviderConfig, ProviderMessage, ProviderToolCall,
    };

    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "http://127.0.0.1:1",
        "fixture-model",
        "fixture-token",
        "fixture-account",
    ))
    .expect("fixture adapter");
    for count in [5_usize, 20, 100, 500] {
        let workspace = TestDir::new(&format!("symbol-pipeline-{count}"));
        let (source, _) = write_workspace(workspace.path());
        let source_uri = url::Url::from_file_path(std::fs::canonicalize(&source).unwrap()).unwrap();
        let source_text = (0..count)
            .map(|index| format!("fn symbol_{index}() {{}}\n"))
            .collect::<String>();
        std::fs::write(&source, source_text).unwrap();
        let target_index = count.saturating_sub(1).min(99);
        let response = (0..count)
            .map(|index| {
                json!({
                    "name": if index == target_index { "target".to_owned() } else { format!("noise_{index}") },
                    "kind": 12,
                    "containerName": if index == target_index {
                        "Parser".to_owned()
                    } else {
                        format!("Noise::{index}::{}", "x".repeat(100))
                    },
                    "location": {
                        "uri": source_uri.as_str(),
                        "range": {
                            "start": { "line": index, "character": 0 },
                            "end": { "line": index, "character": 1 }
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        let raw_fixture_bytes = serde_json::to_vec(&response).unwrap().len();
        let log = workspace.path().join("symbol-pipeline.jsonl");
        let (pool, manager) = manager_with_mock(
            workspace.path(),
            json!({"mock":{"logPath":log,"responses":{"workspace/symbol":response}}}),
        );
        let query = CodeIntelSymbolQuery {
            workspace: workspace.path().to_path_buf(),
            query: Some("target".into()),
            max_results: 100,
            ..Default::default()
        };
        let started = Instant::now();
        let outcome = manager.symbols(&query).await;
        let query_us = started.elapsed().as_micros();
        let transformed_bytes = serde_json::to_vec(&outcome.payload).unwrap().len();
        let started = Instant::now();
        let rendered = slim_core::tools::render_code_intel("symbol", &outcome);
        let render_ns = started.elapsed().as_nanos();
        let mut without_annotations = outcome.clone();
        for symbol in without_annotations.payload["symbols"]
            .as_array_mut()
            .unwrap()
        {
            symbol.as_object_mut().unwrap().remove("container");
            symbol.as_object_mut().unwrap().remove("detail");
        }
        let basic_rendered = slim_core::tools::render_code_intel("symbol", &without_annotations);
        let annotation_bytes = rendered.len().saturating_sub(basic_rendered.len());
        let mut without_containers = outcome.clone();
        for symbol in without_containers.payload["symbols"]
            .as_array_mut()
            .unwrap()
        {
            symbol.as_object_mut().unwrap().remove("container");
        }
        let no_container_rendered =
            slim_core::tools::render_code_intel("symbol", &without_containers);
        let container_bytes = rendered.len().saturating_sub(no_container_rendered.len());
        let target_line = rendered
            .lines()
            .find(|line| line.contains("target  ->"))
            .expect("target remains visible");
        assert!(target_line.contains("in: Parser"), "{target_line}");
        let messages = vec![
            ProviderMessage::user("Find target container."),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "symbols".into(),
                    name: "code_intel".into(),
                    arguments: r#"{"action":"symbol","query":"target","max_results":100}"#.into(),
                }],
            ),
            ProviderMessage::tool("code_intel", "symbols", &rendered),
        ];
        let started = Instant::now();
        let prepared = adapter
            .prepare_messages_request_with_tools_checked(&messages, &[])
            .expect("prepare next request");
        let prepare_ns = started.elapsed().as_nanos();
        let logs = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let request = logs
            .iter()
            .find(|row| {
                row["direction"] == "client_to_server"
                    && row["message"]["method"] == "workspace/symbol"
            })
            .expect("workspace request logged");
        let response_row = logs
            .iter()
            .find(|row| {
                row["direction"] == "server_to_client"
                    && row["message"]["id"] == request["message"]["id"]
            })
            .expect("workspace response logged");
        let raw_lsp_bytes = serde_json::to_vec(&response_row["message"]["result"])
            .unwrap()
            .len();
        assert_eq!(raw_lsp_bytes, raw_fixture_bytes);
        println!(
            "symbol_pipeline count={count} code_intel_invocations=1 raw_lsp_bytes={raw_lsp_bytes} transformed_payload_bytes={transformed_bytes} rendered_bytes={} annotation_bytes={annotation_bytes} container_annotation_bytes={container_bytes} next_request_bytes={} query_us={query_us} render_ns={render_ns} prepare_ns={prepare_ns} target_raw_position={} target_transformed_position={} target_container=true target_detail=false shown={} has_more={}",
            rendered.len(),
            prepared.body().len(),
            target_index + 1,
            usize::from(target_index < 100) * (target_index + 1),
            outcome.payload["shown"],
            outcome.payload["has_more"],
        );
        manager.shutdown().await;
        pool.close_all().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual release measurement; no network"]
async fn measure_document_symbol_pipeline_sizes() {
    use slim_core::provider::{
        OpenAiCodexAdapter, ProviderAdapter, ProviderConfig, ProviderMessage, ProviderToolCall,
    };

    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "http://127.0.0.1:1",
        "fixture-model",
        "fixture-token",
        "fixture-account",
    ))
    .expect("fixture adapter");
    for count in [5_usize, 20, 100, 500] {
        let workspace = TestDir::new(&format!("document-symbol-pipeline-{count}"));
        let (source, _) = write_workspace(workspace.path());
        let source_text = (0..count)
            .map(|index| format!("fn symbol_{index}() {{}}\n"))
            .collect::<String>();
        std::fs::write(&source, source_text).unwrap();
        let child_count = count.saturating_sub(1);
        let target_index = child_count.saturating_sub(1).min(98);
        let children = (0..child_count)
            .map(|index| {
                let target = index == target_index;
                let detail = if target {
                    "fn(value: &str) -> usize".to_owned()
                } else {
                    format!("fn(noise: &str) -> usize // {}", "x".repeat(100))
                };
                json!({
                    "name": if target { "target".to_owned() } else { format!("noise_{index}") },
                    "kind": 12,
                    "detail": detail,
                    "range": {
                        "start": { "line": index + 1, "character": 0 },
                        "end": { "line": index + 1, "character": 1 }
                    },
                    "selectionRange": {
                        "start": { "line": index + 1, "character": 3 },
                        "end": { "line": index + 1, "character": 9 }
                    }
                })
            })
            .collect::<Vec<_>>();
        let response = json!([{
            "name": "Root",
            "kind": 5,
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": count, "character": 0 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 4 }
            },
            "children": children
        }]);
        let raw_fixture_bytes = serde_json::to_vec(&response).unwrap().len();
        let log = workspace.path().join("document-symbol-pipeline.jsonl");
        let (pool, manager) = manager_with_mock(
            workspace.path(),
            json!({"mock":{"logPath":log,"responses":{"textDocument/documentSymbol":response}}}),
        );
        let query = CodeIntelSymbolQuery {
            workspace: workspace.path().to_path_buf(),
            path: Some(source),
            query: Some("target".into()),
            max_results: 100,
            ..Default::default()
        };
        let started = Instant::now();
        let outcome = manager.symbols(&query).await;
        let query_us = started.elapsed().as_micros();
        let transformed_bytes = serde_json::to_vec(&outcome.payload).unwrap().len();
        let started = Instant::now();
        let rendered = slim_core::tools::render_code_intel("symbol", &outcome);
        let render_ns = started.elapsed().as_nanos();
        let target_line = rendered
            .lines()
            .find(|line| line.contains("target  ->"))
            .expect("target remains visible");
        assert!(
            target_line.contains("detail: fn(value: &str) -> usize"),
            "{target_line}"
        );
        let messages = vec![
            ProviderMessage::user("Find target signature."),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "symbols".into(),
                    name: "code_intel".into(),
                    arguments: r#"{"action":"symbol","path":"src/first.rs","query":"target","max_results":100}"#.into(),
                }],
            ),
            ProviderMessage::tool("code_intel", "symbols", &rendered),
        ];
        let started = Instant::now();
        let prepared = adapter
            .prepare_messages_request_with_tools_checked(&messages, &[])
            .expect("prepare next request");
        let prepare_ns = started.elapsed().as_nanos();
        let logs = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let request = logs
            .iter()
            .find(|row| {
                row["direction"] == "client_to_server"
                    && row["message"]["method"] == "textDocument/documentSymbol"
            })
            .expect("document symbol request logged");
        let response_row = logs
            .iter()
            .find(|row| {
                row["direction"] == "server_to_client"
                    && row["message"]["id"] == request["message"]["id"]
            })
            .expect("document symbol response logged");
        let raw_lsp_bytes = serde_json::to_vec(&response_row["message"]["result"])
            .unwrap()
            .len();
        assert_eq!(raw_lsp_bytes, raw_fixture_bytes);
        println!(
            "document_symbol_pipeline count={count} code_intel_invocations=1 raw_lsp_bytes={raw_lsp_bytes} transformed_payload_bytes={transformed_bytes} rendered_bytes={} next_request_bytes={} query_us={query_us} render_ns={render_ns} prepare_ns={prepare_ns} target_raw_position={} target_transformed_position={} target_container=false target_detail=true shown={} has_more={}",
            rendered.len(),
            prepared.body().len(),
            target_index + 2,
            target_index + 2,
            outcome.payload["shown"],
            outcome.payload["has_more"],
        );
        manager.shutdown().await;
        pool.close_all().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symbol_detail_keeps_stale_metadata_after_external_write() {
    let workspace = TestDir::new("symbol-detail-stale");
    let (source, _) = write_workspace(workspace.path());
    let log = workspace.path().join("symbols.jsonl");
    let range = json!({"start":{"line":0,"character":3},"end":{"line":0,"character":8}});
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({"mock":{
            "logPath":log,"requestDelayMs":100,"responses":{"textDocument/documentSymbol":[{
                "name":"first","kind":12,"detail":"fn()","range":range,"selectionRange":range
            }]}
        }}),
    );
    let query = CodeIntelSymbolQuery {
        workspace: workspace.path().into(),
        path: Some(source.clone()),
        max_results: 20,
        ..Default::default()
    };
    let task = tokio::spawn({
        let manager = manager.clone();
        async move { manager.symbols(&query).await }
    });
    wait_for_client_method(&log, "textDocument/documentSymbol").await;
    std::fs::write(&source, "fn changed(value: &str) {}\n").unwrap();
    let outcome = task.await.unwrap();
    let rendered = slim_core::tools::render_code_intel("symbol", &outcome);
    assert!(rendered.contains("detail: fn()"));
    assert!(rendered.contains("document_version: 1 | stale: true"));
    assert!(!rendered.contains("value: &str"));
    pool.close_all().await;
}

#[tokio::test]
async fn diagnostic_result_limits_and_storage_cuts_are_explicit() {
    let workspace = TestDir::new("diagnostics-limit");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(workspace.path(), json!({}));
    let mut query = diagnostics_query(workspace.path(), Some(source.clone()));
    manager.diagnostics(&query).await;
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let lease = pool
        .acquire_warm(&root, "rust-analyzer", &json!({}))
        .await
        .unwrap();
    let uri = url::Url::from_file_path(std::fs::canonicalize(&source).unwrap()).unwrap();
    let items: Vec<_> = (0..201).map(|index| json!({
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
        "severity": if index == 200 { 3 } else { 1 }, "message": format!("error {index}"),
    })).collect();
    lease
        .instance()
        .request_value(
            "mock/publishDiagnostics",
            json!({ "uri": uri.as_str(), "version": 1, "diagnostics": items }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while lease
            .instance()
            .diagnostics_snapshot(&uri, false)
            .await
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    query.max_results = 1;
    let limited = manager.diagnostics(&query).await;
    assert_eq!(limited.payload["shown"], 1);
    assert_eq!(limited.payload["total"], 200);
    assert_eq!(limited.payload["has_more"], true);
    assert_eq!(limited.payload["storage_truncated"], true);
    assert!(slim_core::tools::render_code_intel("diagnostics", &limited).contains("1 shown of 200"));
    query.include_info = true;
    assert_eq!(manager.diagnostics(&query).await.payload["total"], 201);
    query.path = None;
    let workspace_view = manager.diagnostics(&query).await;
    assert_eq!(workspace_view.payload["shown"], 1);
    assert_eq!(workspace_view.payload["total"], 201);
    assert_eq!(workspace_view.payload["has_more"], true);
    assert_eq!(workspace_view.payload["storage_truncated"], true);
    drop(lease);
    pool.close_all().await;
}

#[tokio::test]
async fn definition_never_fabricates_position_for_unreadable_target() {
    let workspace = TestDir::new("definition-target");
    let (source, target) = write_workspace(workspace.path());
    let uri = url::Url::from_file_path(std::fs::canonicalize(&target).unwrap()).unwrap();
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({ "mock": { "responses": {
            "textDocument/definition": { "uri": uri.as_str(), "range": {
                "start": { "line": 1, "character": 5 }, "end": { "line": 1, "character": 6 },
            }},
        }}}),
    );
    let query = position_query(workspace.path(), source);
    std::fs::write(
        &target,
        vec![b'x'; slim_lsp::manager::MAX_CONTEXT_READ_BYTES + 1],
    )
    .unwrap();
    let oversized = manager.definition(&query).await;
    assert!(
        oversized.payload.get("error").is_some(),
        "must not invent 1:1: {:?}",
        oversized.payload
    );
    std::fs::write(&target, [0xff_u8]).unwrap();
    let invalid_utf8 = manager.definition(&query).await;
    assert!(invalid_utf8.payload.get("error").is_some());
    std::fs::write(&target, "fn target() {}\n// 🚀x\n").unwrap();
    let valid = manager.definition(&query).await;
    assert_eq!(valid.payload["found"], true);
    assert_eq!(valid.payload["line"], 2);
    assert_eq!(valid.payload["column"], 5);
    pool.close_all().await;
}

#[tokio::test]
async fn null_symbol_responses_are_successful_empty_results() {
    let workspace = TestDir::new("symbols-null");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({ "mock": { "responses": {
            "textDocument/documentSymbol": null, "workspace/symbol": null,
        }}}),
    );
    for path in [Some(source), None] {
        let result = manager
            .symbols(&CodeIntelSymbolQuery {
                workspace: workspace.path().to_path_buf(),
                path,
                query: Some("main".into()),
                max_results: 20,
                offset: 0,
                revision: None,
                cancellation: None,
            })
            .await;
        assert!(
            result.payload.get("error").is_none(),
            "null is valid: {:?}",
            result.payload
        );
        assert_eq!(result.payload["symbols"], json!([]));
    }
    pool.close_all().await;
}

#[tokio::test]
async fn malformed_reference_and_hover_responses_are_errors() {
    let workspace = TestDir::new("malformed-results");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({ "mock": { "responses": {
            "textDocument/references": {}, "textDocument/hover": {},
        }}}),
    );
    let query = position_query(workspace.path(), source);
    for result in [
        manager.references(&query).await,
        manager.hover(&query).await,
    ] {
        assert!(
            result.payload.get("error").is_some(),
            "malformed result must not be successful: {:?}",
            result.payload
        );
        assert_eq!(result.meta.state, CodeIntelServerState::Degraded);
    }
    pool.close_all().await;
}

#[tokio::test]
async fn null_reference_and_hover_responses_remain_successful() {
    let workspace = TestDir::new("empty-results");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({ "mock": { "responses": {
            "textDocument/references": null, "textDocument/hover": null,
        }}}),
    );
    let query = position_query(workspace.path(), source);
    let references = manager.references(&query).await;
    assert!(references.payload.get("error").is_none());
    assert_eq!(references.payload["total"], 0);
    let hover = manager.hover(&query).await;
    assert!(hover.payload.get("error").is_none());
    assert_eq!(hover.payload["found"], false);
    pool.close_all().await;
}

#[tokio::test]
async fn diagnostics_distinguish_missing_publication_from_empty_publication() {
    let workspace = TestDir::new("diagnostic-empty");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(workspace.path(), json!({}));
    let query = diagnostics_query(workspace.path(), Some(source.clone()));
    let missing = manager.diagnostics(&query).await;
    assert_eq!(missing.meta.completeness, CodeIntelCompleteness::Unknown);
    assert_eq!(missing.payload["files"][0]["received"], false);
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let lease = pool
        .acquire_warm(&root, "rust-analyzer", &json!({}))
        .await
        .unwrap();
    let uri = url::Url::from_file_path(std::fs::canonicalize(&source).unwrap()).unwrap();
    lease
        .instance()
        .request_value(
            "mock/publishDiagnostics",
            json!({
                "uri": uri.as_str(), "version": 1, "diagnostics": [],
            }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while lease
            .instance()
            .diagnostics_snapshot(&uri, false)
            .await
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("empty publication must be retained");
    // Wait until the startup indexing cycle is observed so completeness can
    // honestly report complete.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snap = lease.instance().snapshot().await;
            if snap.indexing_observed && !snap.indexing_active {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup indexing cycle observed");
    let empty = manager.diagnostics(&query).await;
    assert_eq!(empty.meta.completeness, CodeIntelCompleteness::Complete);
    assert_eq!(empty.payload["files"][0]["received"], true);
    assert_eq!(empty.payload["files"][0]["count"], 0);
    assert!(!empty.meta.stale);
    let workspace_view = manager
        .diagnostics(&diagnostics_query(workspace.path(), None))
        .await;
    assert_ne!(
        workspace_view.meta.completeness,
        CodeIntelCompleteness::Complete,
        "published cache is not a validation of every workspace file"
    );
    drop(lease);
    pool.close_all().await;
}

#[tokio::test]
async fn diagnostics_without_version_become_stale_after_write() {
    let workspace = TestDir::new("diagnostic-unversioned");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(workspace.path(), json!({}));
    let query = diagnostics_query(workspace.path(), Some(source.clone()));
    manager.diagnostics(&query).await;
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let lease = pool
        .acquire_warm(&root, "rust-analyzer", &json!({}))
        .await
        .unwrap();
    let uri = url::Url::from_file_path(std::fs::canonicalize(&source).unwrap()).unwrap();
    let publish = json!({ "uri": uri.as_str(), "diagnostics": [{
        "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        "severity": 1, "message": "unversioned",
    }]});
    lease
        .instance()
        .request_value("mock/publishDiagnostics", publish)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while lease
            .instance()
            .diagnostics_snapshot(&uri, false)
            .await
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!manager.diagnostics(&query).await.meta.stale);
    std::fs::write(&source, "fn changed_after_publication() {}\n").unwrap();
    manager
        .notify_file_changed(workspace.path(), &source, None)
        .await;
    let changed = manager.diagnostics(&query).await;
    assert!(
        changed.meta.stale,
        "old unversioned publication cannot validate new text"
    );
    assert_eq!(changed.meta.completeness, CodeIntelCompleteness::Unknown);
    assert_eq!(
        changed.payload["files"][0]["count"], 1,
        "retain useful stale diagnostics"
    );
    lease
        .instance()
        .request_value(
            "mock/publishDiagnostics",
            json!({
                "uri": uri.as_str(), "diagnostics": [],
            }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if lease
                .instance()
                .diagnostics_snapshot(&uri, false)
                .await
                .is_some_and(|s| s.items.is_empty())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let refreshed = manager.diagnostics(&query).await;
    assert!(!refreshed.meta.stale);
    assert_eq!(
        refreshed.meta.completeness,
        CodeIntelCompleteness::Unknown,
        "versionless publication cannot certify a specific version"
    );
    std::fs::write(&source, "fn external_change() {}\n").unwrap();
    assert!(
        manager.diagnostics(&query).await.meta.stale,
        "query-time disk sync also invalidates the publication"
    );
    drop(lease);
    pool.close_all().await;
}

#[tokio::test]
#[ignore = "release measurement with controlled stdio server; run explicitly"]
async fn measure_lsp_cold_warm_and_post_write() {
    fn stats(label: &str, mut samples: Vec<u128>) {
        samples.sort_unstable();
        println!(
            "{label}: n={} median_us={} min_us={} max_us={}",
            samples.len(),
            samples[samples.len() / 2],
            samples[0],
            samples[samples.len() - 1]
        );
    }
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    let mut edited = Vec::new();
    let mut wire = Vec::new();
    for sample in 0..11 {
        let dir = TestDir::new("measure-sync");
        let (source, _) = write_workspace(dir.path());
        let text = format!("fn first() {{}}\n//{}\n", "x".repeat(1024 * 1024));
        std::fs::write(&source, &text).unwrap();
        let log = dir.path().join("wire.jsonl");
        let (pool, manager) = manager_with_mock(
            dir.path(),
            json!({"mock":{
                "logPath":log.to_string_lossy(),
                "textDocumentSync":{"openClose":true,"change":1,"save":{"includeText":false}}
            }}),
        );
        let query = position_query(dir.path(), source.clone());
        let start = Instant::now();
        let result = manager.definition(&query).await;
        cold.push(start.elapsed().as_micros());
        assert!(result.payload.get("error").is_none());
        let start = Instant::now();
        let result = manager.definition(&query).await;
        warm.push(start.elapsed().as_micros());
        assert!(!result.meta.stale);
        let changed = text.replacen("first", "other", 1);
        std::fs::write(&source, &changed).unwrap();
        let start = Instant::now();
        manager.notify_file_changed(dir.path(), &source, None).await;
        let result = manager.definition(&query).await;
        edited.push(start.elapsed().as_micros());
        assert!(!result.meta.stale);
        assert_eq!(result.meta.document_version, Some(2));
        pool.close_all().await;
        let messages = read_client_methods(&log);
        let bytes: usize = messages
            .iter()
            .filter(|m| {
                matches!(
                    m["method"].as_str(),
                    Some("textDocument/didChange" | "textDocument/didSave")
                )
            })
            .map(|m| serde_json::to_vec(m).unwrap().len())
            .sum();
        wire.push(bytes);
        if sample == 0 {
            println!("result={}", result.payload);
        }
    }
    stats("cold_definition", cold);
    stats("warm_definition", warm);
    stats("post_write_definition", edited);
    println!("post_write_json_bytes={wire:?}");
}

#[tokio::test]
async fn save_notifications_follow_negotiation_and_definition_has_preview() {
    for (save, expected) in [
        (json!(false), None),
        (json!(true), Some(false)),
        (json!({"includeText":false}), Some(false)),
        (json!({"includeText":true}), Some(true)),
    ] {
        let dir = TestDir::new("save-capability");
        let (source, _) = write_workspace(dir.path());
        let log = dir.path().join("wire.jsonl");
        let (pool, manager) = manager_with_mock(
            dir.path(),
            json!({"mock":{
                "logPath":log.to_string_lossy(), "textDocumentSync":{"openClose":true,"change":1,"save":save}
            }}),
        );
        let query = position_query(dir.path(), source.clone());
        let outcome = manager.definition(&query).await;
        assert_eq!(outcome.payload["preview"], "fn first() {}");
        let changed = "fn updated() {}\n";
        std::fs::write(&source, changed).unwrap();
        manager.notify_file_changed(dir.path(), &source, None).await;
        let outcome = manager.definition(&query).await;
        assert_eq!(outcome.payload["preview"], "fn updated() {}");
        assert_eq!(outcome.meta.document_version, Some(2));
        assert!(!outcome.meta.stale);
        pool.close_all().await;
        let messages = read_client_methods(&log);
        let saves: Vec<_> = messages
            .iter()
            .filter(|m| m["method"] == "textDocument/didSave")
            .collect();
        match expected {
            None => assert!(saves.is_empty()),
            Some(include) => {
                assert_eq!(saves.len(), 1);
                assert_eq!(saves[0]["params"].get("text").is_some(), include);
                if include {
                    assert_eq!(saves[0]["params"]["text"], changed);
                }
            }
        }
        let changes: Vec<_> = messages
            .iter()
            .filter(|m| m["method"] == "textDocument/didChange")
            .collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["params"]["contentChanges"][0]["text"], changed);
    }
}

#[tokio::test]
async fn manager_cancellation_during_shared_initialize_is_prompt() {
    let dir = TestDir::new("cancel-acquisition");
    let (source, _) = write_workspace(dir.path());
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let log = dir.path().join("init.jsonl");
    let (pool, manager) = manager_with_mock(dir.path(), json!({}));
    let leader = {
        let pool = pool.clone();
        let spec = mock_spec(vec![
            "--log".into(),
            log.to_string_lossy().into_owned(),
            "--initialize-delay-ms".into(),
            "500".into(),
        ]);
        tokio::spawn(async move {
            pool.acquire(root, spec, &json!({}), transport_options(), 8)
                .await
        })
    };
    wait_for_client_method(&log, "initialize").await;
    let token = CancellationToken::new();
    let mut query = position_query(dir.path(), source);
    query.cancellation = Some(token.clone());
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_millis(200), async {
        tokio::join!(manager.definition(&query), cancel)
    })
    .await
    .expect("acquisition observes individual cancellation");
    assert!(outcome.payload["error"]
        .as_str()
        .unwrap()
        .contains("cancelled"));
    let lease = leader
        .await
        .unwrap()
        .expect("other consumer retains startup");
    assert!(!lease.instance().is_closed());
    drop(lease);
    pool.close_all().await;
}

#[derive(Default)]
struct ColdStartTimedFactory {
    spawns: AtomicUsize,
    spawn_durations: std::sync::Mutex<Vec<Duration>>,
}

impl ProcessFactory for ColdStartTimedFactory {
    fn spawn(&self, spec: &ServerSpec, root: &Path) -> Result<SpawnedServer, String> {
        let started = Instant::now();
        let spawned = StdioProcessFactory.spawn(spec, root);
        self.spawns.fetch_add(1, Ordering::AcqRel);
        self.spawn_durations
            .lock()
            .expect("spawn timing lock")
            .push(started.elapsed());
        spawned
    }
}

fn assert_cold_query_protocol(log: &Path) {
    let messages = read_client_methods(log);
    let did_open = messages
        .iter()
        .position(|message| {
            message.get("method").and_then(Value::as_str) == Some("textDocument/didOpen")
        })
        .expect("first query sends didOpen");
    let first_request = messages
        .iter()
        .position(|message| {
            message.get("method").and_then(Value::as_str) == Some("textDocument/definition")
        })
        .expect("first query sends definition request");
    assert!(
        did_open < first_request,
        "didOpen precedes first definition request"
    );
}

// Server-side phase durations, read only after the timed query. These include
// mock processing/logging and are not attributed to Slim CPU or RA indexing.
fn cold_server_phases(log: &Path) -> [u128; 3] {
    let rows: Vec<Value> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let request = |method: &str| {
        rows.iter()
            .find(|row| {
                row["direction"] == "client_to_server" && row["message"]["method"] == method
            })
            .unwrap()
    };
    let elapsed = |row: &Value| row["elapsed_us"].as_u64().unwrap() as u128;
    let response_time = |request: &Value| {
        let response = rows
            .iter()
            .find(|row| {
                row["direction"] == "server_to_client"
                    && row["message"]["id"] == request["message"]["id"]
            })
            .unwrap();
        elapsed(response) - elapsed(request)
    };
    let opened = rows
        .iter()
        .find(|row| row["event"] == "didOpen_applied")
        .unwrap();
    [
        response_time(request("initialize")),
        elapsed(opened) - elapsed(request("textDocument/didOpen")),
        response_time(request("textDocument/definition")),
    ]
}

fn cold_start_manager(
    root: &Path,
    server_config: Value,
    factory: Arc<ColdStartTimedFactory>,
) -> (Arc<LspProcessPool>, Arc<LspCodeIntelligence>) {
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory,
    });
    let manager = LspCodeIntelligence::new(
        pool.clone(),
        LspManagerConfig {
            idle_shutdown: None,
            max_servers: 1,
            request_timeout: Duration::from_secs(3),
            server_config,
            max_open_documents: 8,
            server_path: Some(PathBuf::from(mock_binary())),
        },
    );
    assert!(root.join("Cargo.toml").is_file());
    (pool, manager)
}

fn cold_start_median(label: &str, values: &mut [u128]) {
    if values.is_empty() {
        println!("{label}: n=0");
        return;
    }
    values.sort_unstable();
    println!(
        "{label}: n={} median_us={} min_us={} max_us={}",
        values.len(),
        values[values.len() / 2],
        values[0],
        values[values.len() - 1]
    );
}

struct ColdStartSample {
    runtime_ready_us: u128,
    first_us: u128,
    second_us: u128,
    aggregate_us: u128,
    first_need_us: u128,
    spawn_us: u128,
    initialize_us: u128,
    server_phases: [u128; 3],
}

async fn run_lazy_unused_sample(first_need_delay: Duration) -> u128 {
    let dir = TestDir::new("cold-lazy-unused");
    write_workspace(dir.path());
    let factory = Arc::new(ColdStartTimedFactory::default());
    let start = Instant::now();
    let (pool, manager) = cold_start_manager(dir.path(), json!({ "mock": {} }), factory.clone());
    let mut _runtime = slim_core::runtime::Runtime::new();
    _runtime.set_code_intelligence(manager);
    let runtime_ready_us = start.elapsed().as_micros();
    assert_eq!(factory.spawns.load(Ordering::Acquire), 0);
    if !first_need_delay.is_zero() {
        tokio::time::sleep(first_need_delay).await;
    }
    pool.close_all().await;
    runtime_ready_us
}

async fn run_prewarm_unused_sample(first_need_delay: Duration) -> u128 {
    let dir = TestDir::new("cold-prewarm-unused");
    write_workspace(dir.path());
    let log = dir.path().join("prewarm-unused.jsonl");
    let factory = Arc::new(ColdStartTimedFactory::default());
    let start = Instant::now();
    let (pool, manager) = cold_start_manager(
        dir.path(),
        json!({
            "mock": {
                "logPath": log.to_string_lossy(),
                "requestDelayMs": 2
            }
        }),
        factory.clone(),
    );
    let mut _runtime = slim_core::runtime::Runtime::new();
    _runtime.set_code_intelligence(manager);
    let root = std::fs::canonicalize(dir.path()).expect("prewarm unused root");
    let server_config = json!({
        "mock": {
            "logPath": log.to_string_lossy(),
            "requestDelayMs": 2
        }
    });
    let warm = {
        let pool = pool.clone();
        let root = root.clone();
        let server_config = server_config.clone();
        tokio::spawn(async move {
            pool.acquire(
                root,
                mock_spec(Vec::new()),
                &server_config,
                transport_options(),
                8,
            )
            .await
        })
    };
    let runtime_ready_us = start.elapsed().as_micros();
    if !first_need_delay.is_zero() {
        tokio::time::sleep(first_need_delay).await;
    }
    let methods = if log.is_file() {
        read_client_methods(&log)
    } else {
        Vec::new()
    };
    assert!(methods.iter().all(|message| {
        !matches!(
            message.get("method").and_then(Value::as_str),
            Some("textDocument/didOpen")
                | Some("textDocument/didChange")
                | Some("textDocument/definition")
        )
    }));
    let lease = warm
        .await
        .expect("prewarm unused task")
        .expect("prewarm unused server");
    assert_eq!(factory.spawns.load(Ordering::Acquire), 1);
    drop(lease);
    pool.close_all().await;
    runtime_ready_us
}

async fn run_lazy_cold_sample(first_need_delay: Duration) -> ColdStartSample {
    let dir = TestDir::new("cold-lazy");
    let (source, _) = write_workspace(dir.path());
    let log = dir.path().join("lazy.jsonl");
    let factory = Arc::new(ColdStartTimedFactory::default());
    let start = Instant::now();
    let (pool, manager) = cold_start_manager(
        dir.path(),
        json!({
            "mock": {
                "logPath": log.to_string_lossy(),
                "requestDelayMs": 2
            }
        }),
        factory.clone(),
    );
    let mut _runtime = slim_core::runtime::Runtime::new();
    _runtime.set_code_intelligence(manager.clone());
    let runtime_ready_us = start.elapsed().as_micros();
    assert_eq!(factory.spawns.load(Ordering::Acquire), 0);
    if !first_need_delay.is_zero() {
        tokio::time::sleep(first_need_delay).await;
    }
    let query = position_query(dir.path(), source);
    let first_need_us = start.elapsed().as_micros();
    let query_start = Instant::now();
    let outcome = manager.definition(&query).await;
    let first_us = query_start.elapsed().as_micros();
    let aggregate_us = start.elapsed().as_micros();
    assert!(outcome.payload.get("error").is_none());
    assert!(!outcome.meta.stale);
    assert_cold_query_protocol(&log);
    let second_start = Instant::now();
    let second = manager.definition(&query).await;
    assert!(second.payload.get("error").is_none());
    let second_us = second_start.elapsed().as_micros();
    let spawn_us = factory
        .spawn_durations
        .lock()
        .expect("lazy spawn timing lock")
        .first()
        .map(Duration::as_micros)
        .unwrap_or_default();
    assert_eq!(factory.spawns.load(Ordering::Acquire), 1);
    let server_phases = cold_server_phases(&log);
    pool.close_all().await;
    ColdStartSample {
        runtime_ready_us,
        first_us,
        second_us,
        aggregate_us,
        first_need_us,
        spawn_us,
        initialize_us: 0,
        server_phases,
    }
}

async fn run_prewarm_cold_sample(first_need_delay: Duration) -> ColdStartSample {
    let dir = TestDir::new("cold-prewarm");
    let (source, _) = write_workspace(dir.path());
    let log = dir.path().join("prewarm.jsonl");
    let factory = Arc::new(ColdStartTimedFactory::default());
    let start = Instant::now();
    let (pool, manager) = cold_start_manager(
        dir.path(),
        json!({
            "mock": {
                "logPath": log.to_string_lossy(),
                "requestDelayMs": 2
            }
        }),
        factory.clone(),
    );
    let mut _runtime = slim_core::runtime::Runtime::new();
    _runtime.set_code_intelligence(manager.clone());
    let root = std::fs::canonicalize(dir.path()).expect("prewarm root");
    let server_config = json!({
        "mock": {
            "logPath": log.to_string_lossy(),
            "requestDelayMs": 2
        }
    });
    let warm = {
        let pool = pool.clone();
        let root = root.clone();
        let server_config = server_config.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = pool
                .acquire(
                    root,
                    mock_spec(Vec::new()),
                    &server_config,
                    transport_options(),
                    8,
                )
                .await;
            (result, started.elapsed())
        })
    };
    let runtime_ready_us = start.elapsed().as_micros();
    if !first_need_delay.is_zero() {
        tokio::time::sleep(first_need_delay).await;
    }
    let query = position_query(dir.path(), source);
    let first_need_us = start.elapsed().as_micros();
    let query_start = Instant::now();
    let outcome = manager.definition(&query).await;
    let first_us = query_start.elapsed().as_micros();
    let aggregate_us = start.elapsed().as_micros();
    assert!(outcome.payload.get("error").is_none());
    assert!(!outcome.meta.stale);
    assert_cold_query_protocol(&log);
    let second_start = Instant::now();
    let second = manager.definition(&query).await;
    assert!(second.payload.get("error").is_none());
    let second_us = second_start.elapsed().as_micros();
    let (lease, warm_duration) = warm.await.expect("prewarm task");
    let lease = lease.expect("prewarm server");
    let spawn_us = factory
        .spawn_durations
        .lock()
        .expect("prewarm spawn timing lock")
        .first()
        .map(Duration::as_micros)
        .unwrap_or_default();
    let initialize_us = warm_duration.as_micros().saturating_sub(spawn_us);
    drop(lease);
    let server_phases = cold_server_phases(&log);
    pool.close_all().await;
    ColdStartSample {
        runtime_ready_us,
        first_us,
        second_us,
        aggregate_us,
        first_need_us,
        spawn_us,
        initialize_us,
        server_phases,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "release cold-start measurement with equivalent prewarm/lazy boundaries"]
async fn measure_cold_start_prewarm_vs_lazy_boundaries() {
    const SAMPLES: usize = 9;

    for delay_ms in [0_u64, 25] {
        let first_need_delay = Duration::from_millis(delay_ms);
        let mut lazy_unused = Vec::new();
        let mut prewarm_unused = Vec::new();
        let mut lazy_first = Vec::new();
        let mut prewarm_first = Vec::new();
        let mut lazy_second = Vec::new();
        let mut prewarm_second = Vec::new();
        let mut lazy_need = Vec::new();
        let mut prewarm_need = Vec::new();
        let mut lazy_aggregate = Vec::new();
        let mut prewarm_aggregate = Vec::new();
        let mut spawn = Vec::new();
        let mut prewarm_initialize = Vec::new();
        let mut first_response = Vec::new();
        let mut lazy_phases = Vec::new();
        let mut prewarm_phases = Vec::new();

        for sample in 0..SAMPLES {
            let (lazy_unused_us, prewarm_unused_us, lazy, prewarm) = if sample % 2 == 0 {
                (
                    run_lazy_unused_sample(first_need_delay).await,
                    run_prewarm_unused_sample(first_need_delay).await,
                    run_lazy_cold_sample(first_need_delay).await,
                    run_prewarm_cold_sample(first_need_delay).await,
                )
            } else {
                let prewarm_unused_us = run_prewarm_unused_sample(first_need_delay).await;
                let lazy_unused_us = run_lazy_unused_sample(first_need_delay).await;
                let prewarm = run_prewarm_cold_sample(first_need_delay).await;
                let lazy = run_lazy_cold_sample(first_need_delay).await;
                (lazy_unused_us, prewarm_unused_us, lazy, prewarm)
            };
            lazy_phases.push(lazy.server_phases);
            prewarm_phases.push(prewarm.server_phases);
            lazy_unused.push(lazy_unused_us);
            lazy_first.push(lazy.first_us);
            lazy_second.push(lazy.second_us);
            lazy_aggregate.push(lazy.aggregate_us);
            lazy_need.push(lazy.first_need_us);
            prewarm_need.push(prewarm.first_need_us);
            spawn.push(lazy.spawn_us);
            first_response.push(lazy.first_us);
            prewarm_unused.push(prewarm_unused_us);
            prewarm_first.push(prewarm.first_us);
            prewarm_second.push(prewarm.second_us);
            prewarm_aggregate.push(prewarm.aggregate_us);
            spawn.push(prewarm.spawn_us);
            prewarm_initialize.push(prewarm.initialize_us);
            first_response.push(prewarm.first_us);
            if sample == 0 {
                println!(
                "cold_sample: delay_ms={delay_ms} lazy_runtime_ready_us={} lazy_first_us={} lazy_second_us={} lazy_spawn_us={} lazy_protocol=didOpen->definition->response; prewarm_runtime_ready_us={} prewarm_first_us={} prewarm_second_us={} prewarm_spawn_us={} prewarm_initialize_after_spawn_us={} prewarm_protocol=didOpen->definition->response; lazy_processes=1 prewarm_processes=1",
                lazy.runtime_ready_us,
                lazy.first_us,
                lazy.second_us,
                lazy.spawn_us,
                prewarm.runtime_ready_us,
                prewarm.first_us,
                prewarm.second_us,
                prewarm.spawn_us,
                prewarm.initialize_us,
            );
            }
        }

        cold_start_median(&format!("delay{delay_ms}_lazy_first_need"), &mut lazy_need);
        cold_start_median(
            &format!("delay{delay_ms}_prewarm_first_need"),
            &mut prewarm_need,
        );
        cold_start_median(
            &format!("delay{delay_ms}_lazy_start_without_lsp"),
            &mut lazy_unused,
        );
        cold_start_median(
            &format!("delay{delay_ms}_prewarm_start_without_lsp"),
            &mut prewarm_unused,
        );
        cold_start_median(
            &format!("delay{delay_ms}_lazy_first_query"),
            &mut lazy_first,
        );
        cold_start_median(
            &format!("delay{delay_ms}_prewarm_first_query_after_warmup"),
            &mut prewarm_first,
        );
        cold_start_median(
            &format!("delay{delay_ms}_lazy_second_query"),
            &mut lazy_second,
        );
        cold_start_median(
            &format!("delay{delay_ms}_prewarm_second_query"),
            &mut prewarm_second,
        );
        cold_start_median(
            &format!("delay{delay_ms}_lazy_start_plus_first_query"),
            &mut lazy_aggregate,
        );
        cold_start_median(
            &format!("delay{delay_ms}_prewarm_start_plus_first_query"),
            &mut prewarm_aggregate,
        );
        cold_start_median(&format!("delay{delay_ms}_spawn"), &mut spawn);
        cold_start_median(
            &format!("delay{delay_ms}_prewarm_initialize_after_spawn"),
            &mut prewarm_initialize,
        );
        cold_start_median(
            &format!("delay{delay_ms}_first_response"),
            &mut first_response,
        );
        for (mode, phases) in [("lazy", lazy_phases), ("prewarm", prewarm_phases)] {
            for (index, label) in [
                "initialize_server",
                "didOpen_server",
                "definition_request_to_response_server",
            ]
            .iter()
            .enumerate()
            {
                let mut samples: Vec<_> = phases.iter().map(|row| row[index]).collect();
                cold_start_median(&format!("delay{delay_ms}_{mode}_{label}"), &mut samples);
            }
        }
        println!(
            "cold_process_contract delay_ms={delay_ms}: lazy_without_lsp=0; prewarm_without_lsp=1; lazy_first=1; prewarm_first=1"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn references_page_through_offset_bound_to_revision() {
    let workspace = TestDir::new("references-paging");
    let (first, second) = write_workspace(workspace.path());
    let first_uri = url::Url::from_file_path(std::fs::canonicalize(&first).unwrap()).unwrap();
    let second_uri = url::Url::from_file_path(std::fs::canonicalize(&second).unwrap()).unwrap();
    let point = |line: u32, character: u32| {
        json!({"start": {"line": line, "character": character},
               "end": {"line": line, "character": character + 1}})
    };
    let locations = json!([
        {"uri": first_uri.as_str(), "range": point(0, 0)},
        {"uri": second_uri.as_str(), "range": point(0, 0)},
        {"uri": first_uri.as_str(), "range": point(0, 1)},
        {"uri": second_uri.as_str(), "range": point(0, 1)},
    ]);
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({"mock": {"responses": {"textDocument/references": locations}}}),
    );
    let mut query = position_query(workspace.path(), first.clone());
    query.max_results = 2;

    let first_page = manager.references(&query).await;
    assert_eq!(first_page.payload["total"], 4);
    assert_eq!(first_page.payload["total_files"], 2);
    assert_eq!(first_page.payload["shown"], 2);
    assert_eq!(first_page.payload["has_more"], true);
    assert_eq!(first_page.payload["next_offset"], 2);
    let revision = first_page.payload["revision"]
        .as_u64()
        .expect("references pages carry the workspace revision");

    query.offset = 2;
    query.revision = Some(revision);
    let second_page = manager.references(&query).await;
    assert_eq!(second_page.payload["shown"], 2);
    assert_eq!(second_page.payload["offset"], 2);
    assert_eq!(second_page.payload["has_more"], false);
    let columns = |outcome: &slim_core::codeintel::CodeIntelOutcome| -> Vec<u64> {
        outcome.payload["files"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|file| file["results"].as_array().unwrap().iter())
            .filter_map(|row| row["column"].as_u64())
            .collect()
    };
    // First page shows the char-0 locations (human column 1); the second page
    // continues at the char-1 locations without repeating results.
    assert_eq!(columns(&first_page), vec![1, 1]);
    assert_eq!(columns(&second_page), vec![2, 2]);
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn references_continuation_rejects_stale_revision() {
    let workspace = TestDir::new("references-revision");
    let (source, _) = write_workspace(workspace.path());
    let (pool, manager) = manager_with_mock(workspace.path(), json!({}));
    let mut query = position_query(workspace.path(), source.clone());
    let first = manager.references(&query).await;
    let revision = first.payload["revision"]
        .as_u64()
        .expect("references carries the workspace revision");

    manager
        .notify_file_changed(workspace.path(), &source, None)
        .await;
    query.offset = 1;
    query.revision = Some(revision);
    let stale = manager.references(&query).await;
    let error = stale.payload["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("changed") && error.contains("offset"),
        "stale continuation must be rejected with an actionable message: {stale:?}"
    );

    // A server restart is a new generation even without any edit: the token
    // binds the instance id, so the same continuation is still rejected.
    let fresh = manager
        .references(&position_query(workspace.path(), source))
        .await;
    let restart_revision = fresh.payload["revision"]
        .as_u64()
        .expect("fresh page carries a revision token");
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    let lease = pool
        .acquire_warm(&root, "rust-analyzer", &json!({}))
        .await
        .expect("warm lease");
    assert!(
        lease
            .instance()
            .request_value("exit", json!(null))
            .await
            .is_err(),
        "mock exit must not be answered"
    );
    assert!(lease.instance().is_closed());
    drop(lease);
    wait_for_leases(&pool, 0).await;

    let mut query = position_query(workspace.path(), workspace.path().join("src/first.rs"));
    query.offset = 1;
    query.revision = Some(restart_revision);
    let after_restart = manager.references(&query).await;
    let error = after_restart.payload["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("changed"),
        "continuation across a restart must be rejected: {after_restart:?}"
    );
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_symbol_query_ranks_matches_before_the_window() {
    let workspace = TestDir::new("symbol-rank");
    let (source, _) = write_workspace(workspace.path());
    let uri = url::Url::from_file_path(std::fs::canonicalize(&source).unwrap()).unwrap();
    let point = |character: u32| {
        json!({"start": {"line": 0, "character": character},
               "end": {"line": 0, "character": character + 1}})
    };
    let flat = json!([
        {"name": "alpha", "kind": 12, "location": {"uri": uri.as_str(), "range": point(0)}},
        {"name": "beta", "kind": 12, "location": {"uri": uri.as_str(), "range": point(1)}},
        {"name": "target_late", "kind": 12, "location": {"uri": uri.as_str(), "range": point(2)}},
    ]);
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({"mock": {"responses": {"textDocument/documentSymbol": flat}}}),
    );
    let mut query = CodeIntelSymbolQuery {
        workspace: workspace.path().to_path_buf(),
        path: Some(source),
        query: Some("target_late".into()),
        max_results: 1,
        ..Default::default()
    };
    let ranked = manager.symbols(&query).await;
    assert_eq!(ranked.payload["total"], 3);
    assert_eq!(ranked.payload["received"], 3);
    assert_eq!(ranked.payload["shown"], 1);
    assert_eq!(ranked.payload["has_more"], true);
    let symbols = ranked.payload["symbols"].as_array().unwrap();
    assert_eq!(symbols[0]["name"], "target_late");

    query.offset = 1;
    query.revision = ranked.payload["revision"].as_u64();
    let next = manager.symbols(&query).await;
    let names: Vec<&str> = next.payload["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(names, vec!["alpha"]);
    assert_eq!(next.payload["has_more"], true);
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definition_reports_out_of_scope_locations() {
    let workspace = TestDir::new("definition-scope");
    let (source, _) = write_workspace(workspace.path());
    let outside =
        std::env::temp_dir().join(format!("slim-outside-scope-{}.rs", std::process::id()));
    std::fs::write(&outside, "fn outside() {}\n").expect("write outside file");
    let outside_uri = url::Url::from_file_path(std::fs::canonicalize(&outside).unwrap()).unwrap();
    let (pool, manager) = manager_with_mock(
        workspace.path(),
        json!({"mock": {"definitionUri": outside_uri.as_str()}}),
    );
    let outcome = manager
        .definition(&position_query(workspace.path(), source))
        .await;
    assert_eq!(outcome.payload["found"], false);
    assert_eq!(outcome.payload["locations_received"], 1);
    assert_eq!(outcome.payload["locations_out_of_scope"], 1);
    let rendered = slim_core::tools::render_code_intel("definition", &outcome);
    assert!(
        rendered.contains("outside the workspace scope"),
        "{rendered}"
    );
    let _ = std::fs::remove_file(&outside);
    pool.close_all().await;
}
