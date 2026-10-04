//! Post-edit diagnostics against the real mock LSP subprocess: what a batch of
//! edits introduced, compared with the errors known before it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::codeintel::{
    CodeIntelDiagnosticsQuery, CodeIntelligence, EditDiagnosticsReport, EditFileDiagnostics,
    EditVerification,
};
use slim_core::runtime::CancellationToken;
use slim_lsp::discovery::{ServerOptions, RUST_ANALYZER};
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
            "slim-lsp-post-edit-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(path.join("src")).expect("create test directory");
        std::fs::write(
            path.join("Cargo.toml"),
            "[package]\nname='mock-workspace'\nversion='0.1.0'\nedition='2021'\n",
        )
        .expect("write Cargo.toml");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::write(&path, text).expect("write fixture file");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn manager(_root: &Path, mock: Value) -> (Arc<LspProcessPool>, Arc<LspCodeIntelligence>) {
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
            servers: [(
                RUST_ANALYZER.into(),
                ServerOptions {
                    path: Some(PathBuf::from(env!("CARGO_BIN_EXE_slim-lsp-mock"))),
                    initialization_options: Some(config.clone()),
                    settings: Some(json!({"rust-analyzer": config})),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            max_open_documents: 8,
        },
    );
    (pool, manager)
}

fn query(workspace: &Path, path: &Path) -> CodeIntelDiagnosticsQuery {
    CodeIntelDiagnosticsQuery {
        workspace: workspace.to_path_buf(),
        server: None,
        path: Some(path.to_path_buf()),
        include_info: false,
        max_results: 20,
        cancellation: None,
    }
}

/// Starts the server through the normal read path and waits until it has
/// published a current, non-stale answer for `path`, so the edit that follows
/// has a certified reference.
async fn warm(manager: &Arc<LspCodeIntelligence>, workspace: &Path, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let outcome = manager.diagnostics(&query(workspace, path)).await;
        let file = &outcome.payload["files"][0];
        if file["received"] == true && file["stale"] == false {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "server never published: {outcome:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn edit(
    manager: &Arc<LspCodeIntelligence>,
    workspace: &Path,
    path: &Path,
    text: &str,
) -> Option<EditDiagnosticsReport> {
    std::fs::write(path, text).expect("write edit");
    manager
        .notify_file_changed(workspace, path, Some(text.to_owned()))
        .await;
    manager
        .diagnostics_after_edits(
            workspace,
            &[path.to_path_buf()],
            Duration::from_secs(3),
            None,
        )
        .await
}

fn only_file(report: &EditDiagnosticsReport) -> &EditFileDiagnostics {
    assert_eq!(report.files.len(), 1, "{report:?}");
    &report.files[0]
}

#[tokio::test]
async fn edited_dependency_invalidates_other_files_without_invalidating_its_fresh_report() {
    let dir = TestDir::new("dependent-cache");
    let first = dir.write("src/first.rs", "fn first() {}\n");
    let second = dir.write("src/second.rs", "fn second() {}\n");
    let (pool, manager) = manager(
        dir.path(),
        json!({ "errorsFromDocument": true, "skipUnchanged": true }),
    );
    warm(&manager, dir.path(), &first).await;
    warm(&manager, dir.path(), &second).await;
    let report = edit(
        &manager,
        dir.path(),
        &second,
        "fn changed_dependency() {}\nlet BROKEN = 1;\n",
    )
    .await
    .unwrap();
    assert!(
        only_file(&report)
            .verification
            .unverified_reason()
            .is_none(),
        "{report:?}"
    );
    assert_eq!(only_file(&report).errors.len(), 1);
    let dependent = manager.diagnostics(&query(dir.path(), &first)).await;
    assert_eq!(
        dependent.payload["files"][0]["stale"], true,
        "{dependent:?}"
    );
    pool.close_all().await;
}

#[tokio::test]
async fn coverage_counts_unique_files_and_explains_the_batch_limit() {
    let dir = TestDir::new("coverage-limit");
    let paths = (0..14)
        .map(|index| dir.write(&format!("src/file{index}.rs"), "fn source() {}\n"))
        .collect::<Vec<_>>();
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));
    warm(&manager, dir.path(), &paths[0]).await;
    let mut inputs = vec![
        dir.write("NOTES.md", "notes"),
        dir.path().join("missing.rs"),
    ];
    inputs.extend(paths.clone());
    inputs.extend(paths);
    let report = manager
        .diagnostics_after_edits(dir.path(), &inputs, Duration::from_secs(5), None)
        .await
        .unwrap();
    assert_eq!(report.files.len(), 16);
    assert_eq!(
        report
            .files
            .iter()
            .filter(|file| file.verification == EditVerification::Unsupported)
            .count(),
        1
    );
    assert_eq!(
        report
            .files
            .iter()
            .filter(|file| file.verification == EditVerification::FileUnavailable)
            .count(),
        1
    );
    assert_eq!(
        report
            .files
            .iter()
            .filter(|file| file.verification == EditVerification::LimitExceeded)
            .count(),
        2
    );
    assert_eq!(
        report
            .files
            .iter()
            .filter(|file| file.verification.unverified_reason().is_none())
            .count(),
        12
    );
    let note = slim_core::codeintel::render_edit_diagnostics(&report, true).unwrap();
    assert!(
        note.contains("Coverage: 12/16 files verified; 4 not verified."),
        "{note}"
    );
    assert!(note.contains("batch limit exceeded"));
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn introduced_error_is_reported_against_the_clean_baseline() {
    let dir = TestDir::new("introduced");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));
    warm(&manager, dir.path(), &source).await;

    let report = edit(
        &manager,
        dir.path(),
        &source,
        "fn first() {}\nlet BROKEN = 1;\n",
    )
    .await
    .expect("warm server answers");

    let file = only_file(&report);
    assert_eq!(report.server, "rust-analyzer");
    assert_eq!(file.path.replace('\\', "/"), "src/first.rs");
    assert_eq!(file.verification, EditVerification::Verified);
    assert_eq!(file.errors.len(), 1, "{file:?}");
    assert_eq!(file.errors[0].line, 2);
    assert_eq!(file.errors[0].column, 5);
    assert_eq!(file.errors[0].code.as_deref(), Some("E0001"));
    assert!(file.errors[0].message.contains("BROKEN"));
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preexisting_error_is_not_a_regression_even_when_its_line_moves() {
    let dir = TestDir::new("preexisting");
    let source = dir.write("src/first.rs", "let BROKEN = 1;\n");
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));
    warm(&manager, dir.path(), &source).await;

    let report = edit(
        &manager,
        dir.path(),
        &source,
        "// header\nlet BROKEN = 1;\n",
    )
    .await
    .expect("warm server answers");

    let file = only_file(&report);
    assert_eq!(file.verification, EditVerification::Verified);
    assert!(file.errors.is_empty(), "{file:?}");
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_the_agent_keeps_is_reported_once() {
    let dir = TestDir::new("once");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));
    warm(&manager, dir.path(), &source).await;

    let first = edit(
        &manager,
        dir.path(),
        &source,
        "fn first() {}\nlet BROKEN = 1;\n",
    )
    .await
    .expect("first report");
    assert_eq!(only_file(&first).errors.len(), 1);

    let second = edit(
        &manager,
        dir.path(),
        &source,
        "fn first() {}\nlet BROKEN = 1;\nfn extra() {}\n",
    )
    .await
    .expect("second report");
    let file = only_file(&second);
    assert_eq!(file.verification, EditVerification::Verified);
    assert!(file.errors.is_empty(), "{file:?}");
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_edits_of_one_file_share_the_pre_batch_baseline() {
    let dir = TestDir::new("batch");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));
    warm(&manager, dir.path(), &source).await;

    std::fs::write(&source, "fn first() {}\nlet BROKEN = 1;\n").unwrap();
    manager
        .notify_file_changed(
            dir.path(),
            &source,
            Some("fn first() {}\nlet BROKEN = 1;\n".into()),
        )
        .await;
    let report = edit(
        &manager,
        dir.path(),
        &source,
        "fn first() {}\nlet BROKEN = 1;\nfn extra() {}\n",
    )
    .await
    .expect("batch report");

    let file = only_file(&report);
    assert_eq!(file.verification, EditVerification::Verified);
    assert_eq!(file.errors.len(), 1, "{file:?}");
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_analysis_is_awaited_within_the_deadline() {
    let dir = TestDir::new("slow");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(
        dir.path(),
        json!({ "errorsFromDocument": true, "publishDelayMs": 250 }),
    );
    warm(&manager, dir.path(), &source).await;

    let started = Instant::now();
    let report = edit(
        &manager,
        dir.path(),
        &source,
        "fn first() {}\nlet BROKEN = 1;\n",
    )
    .await
    .expect("warm server answers");

    let file = only_file(&report);
    assert_eq!(file.verification, EditVerification::Verified);
    assert_eq!(file.errors.len(), 1, "{file:?}");
    assert!(started.elapsed() >= Duration::from_millis(200));
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_server_is_unverified_never_clean() {
    let dir = TestDir::new("silent");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(
        dir.path(),
        json!({ "errorsFromDocument": true, "skipUnchanged": true }),
    );
    warm(&manager, dir.path(), &source).await;

    std::fs::write(&source, "fn first() {}\n// still clean\n").unwrap();
    manager
        .notify_file_changed(
            dir.path(),
            &source,
            Some("fn first() {}\n// still clean\n".into()),
        )
        .await;
    let started = Instant::now();
    let report = manager
        .diagnostics_after_edits(
            dir.path(),
            std::slice::from_ref(&source),
            Duration::from_millis(400),
            None,
        )
        .await
        .expect("warm server answers");

    let file = only_file(&report);
    assert_eq!(file.verification, EditVerification::Unverified);
    assert!(file.errors.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(350));
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn versionless_publications_return_unverified_without_spending_the_deadline() {
    let dir = TestDir::new("versionless");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(
        dir.path(),
        json!({ "errorsFromDocument": true, "omitDiagnosticVersion": true }),
    );
    warm(&manager, dir.path(), &source).await;

    std::fs::write(&source, "fn first() {}\nlet BROKEN = 1;\n").unwrap();
    manager
        .notify_file_changed(
            dir.path(),
            &source,
            Some("fn first() {}\nlet BROKEN = 1;\n".into()),
        )
        .await;
    let started = Instant::now();
    let report = manager
        .diagnostics_after_edits(
            dir.path(),
            std::slice::from_ref(&source),
            Duration::from_secs(3),
            None,
        )
        .await
        .expect("warm server answers");

    let file = only_file(&report);
    // A versionless publication cannot certify the edited version, so it
    // neither claims errors nor waits for a certification that cannot come.
    assert_eq!(file.verification, EditVerification::Unverified);
    assert!(file.errors.is_empty(), "{report:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "versionless check spent {:?}",
        started.elapsed()
    );
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_publication_for_an_older_version_is_discarded() {
    let dir = TestDir::new("stale");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(
        dir.path(),
        json!({ "errorsFromDocument": true, "diagnosticVersionDelta": -1 }),
    );
    // Every publication names a version older than the document, so none may
    // ever count as an answer for the edit.
    let _ = manager.diagnostics(&query(dir.path(), &source)).await;
    std::fs::write(&source, "let BROKEN = 1;\n").unwrap();
    manager
        .notify_file_changed(dir.path(), &source, Some("let BROKEN = 1;\n".into()))
        .await;
    let report = manager
        .diagnostics_after_edits(
            dir.path(),
            std::slice::from_ref(&source),
            Duration::from_millis(300),
            None,
        )
        .await
        .expect("warm server answers");

    let file = only_file(&report);
    assert_eq!(file.verification, EditVerification::Unverified);
    assert!(file.errors.is_empty());
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_the_edit_opened_has_no_baseline() {
    let dir = TestDir::new("no-baseline");
    let opened = dir.write("src/first.rs", "fn first() {}\n");
    let never_opened = dir.write("src/second.rs", "fn second() {}\n");
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));
    warm(&manager, dir.path(), &opened).await;

    let report = edit(
        &manager,
        dir.path(),
        &never_opened,
        "fn second() {}\nlet BROKEN = 2;\n",
    )
    .await
    .expect("warm server answers");

    let file = only_file(&report);
    assert_eq!(file.path.replace('\\', "/"), "src/second.rs");
    assert_eq!(file.verification, EditVerification::VerifiedWithoutBaseline);
    assert_eq!(file.errors.len(), 1, "{file:?}");
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_warm_server_reports_unverified_without_spawning() {
    let dir = TestDir::new("cold");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));

    let report = manager
        .diagnostics_after_edits(
            dir.path(),
            std::slice::from_ref(&source),
            Duration::from_secs(1),
            None,
        )
        .await;

    assert_eq!(
        only_file(&report.unwrap()).verification,
        EditVerification::ServerUnavailable
    );
    assert_eq!(pool.active_leases(), 0);
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_run_gets_no_report() {
    let dir = TestDir::new("cancelled");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let (pool, manager) = manager(
        dir.path(),
        json!({ "errorsFromDocument": true, "skipUnchanged": true }),
    );
    warm(&manager, dir.path(), &source).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let report = manager
        .diagnostics_after_edits(
            dir.path(),
            std::slice::from_ref(&source),
            Duration::from_secs(5),
            Some(cancellation),
        )
        .await;

    assert!(report.is_none());
    pool.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_the_server_does_not_serve_are_reported() {
    let dir = TestDir::new("unserved");
    let source = dir.write("src/first.rs", "fn first() {}\n");
    let notes = dir.write("NOTES.md", "BROKEN prose\n");
    let (pool, manager) = manager(dir.path(), json!({ "errorsFromDocument": true }));
    warm(&manager, dir.path(), &source).await;

    let report = manager
        .diagnostics_after_edits(dir.path(), &[notes], Duration::from_secs(1), None)
        .await;

    assert_eq!(
        only_file(&report.unwrap()).verification,
        EditVerification::Unsupported
    );
    pool.close_all().await;
}
