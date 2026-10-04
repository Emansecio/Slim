//! JS/TS and mixed-workspace routing against the deterministic stdio fixture.
//! These tests validate the host contract; they do not claim compatibility with
//! the external TypeScript Language Server or its TypeScript engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::codeintel::{
    CodeIntelDiagnosticsQuery, CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery,
    CodeIntelligence, EditVerification,
};
use slim_core::runtime::CancellationToken;
use slim_lsp::discovery::{
    discover_server, ServerOptions, RUST_ANALYZER, TYPESCRIPT_LANGUAGE_SERVER,
};
use slim_lsp::{LspCodeIntelligence, LspManagerConfig, TransportOptions};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slim-lsp-typescript-integration-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn mixed(&self) {
        self.write(
            "Cargo.toml",
            "[package]\nname='mixed-fixture'\nversion='0.1.0'\nedition='2021'\n",
        );
        self.write("package.json", r#"{"name":"mixed-fixture","private":true}"#);
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mock_manager(rust: Value, typescript: Value) -> Arc<LspCodeIntelligence> {
    mock_manager_with_timeout(rust, typescript, Duration::from_secs(3))
}

fn mock_manager_with_timeout(
    rust: Value,
    typescript: Value,
    timeout: Duration,
) -> Arc<LspCodeIntelligence> {
    let options = |mock| ServerOptions {
        path: Some(PathBuf::from(env!("CARGO_BIN_EXE_slim-lsp-mock"))),
        args: Some(vec![]),
        initialization_options: Some(json!({ "mock": mock })),
        settings: Some(json!({})),
        ..Default::default()
    };
    LspCodeIntelligence::from_config(LspManagerConfig {
        idle_shutdown: None,
        max_servers: 2,
        request_timeout: timeout,
        max_open_documents: 32,
        servers: [
            (RUST_ANALYZER.to_owned(), options(rust)),
            (TYPESCRIPT_LANGUAGE_SERVER.to_owned(), options(typescript)),
        ]
        .into_iter()
        .collect(),
    })
}

async fn wait_for_leases(manager: &LspCodeIntelligence, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.pool().active_leases() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mock reaches expected lease count");
}

async fn wait_for_wire_method(log: &Path, method: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if std::fs::read_to_string(log).is_ok_and(|text| {
                text.lines().any(|row| {
                    serde_json::from_str::<Value>(row).is_ok_and(|row| {
                        row["direction"] == "client_to_server" && row["message"]["method"] == method
                    })
                })
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mock reads the request before blocking on its gate");
}

fn position(workspace: &Workspace, path: &Path) -> CodeIntelPositionQuery {
    CodeIntelPositionQuery {
        workspace: workspace.0.clone(),
        path: path.to_path_buf(),
        line: 1,
        column: 1,
        max_results: 20,
        ..Default::default()
    }
}

fn diagnostics(workspace: &Workspace, path: &Path) -> CodeIntelDiagnosticsQuery {
    CodeIntelDiagnosticsQuery {
        workspace: workspace.0.clone(),
        path: Some(path.to_path_buf()),
        max_results: 20,
        ..Default::default()
    }
}

async fn warm(manager: &LspCodeIntelligence, workspace: &Workspace, path: &Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let outcome = manager.diagnostics(&diagnostics(workspace, path)).await;
            let file = &outcome.payload["files"][0];
            if file["received"] == true && file["stale"] == false {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("mock publishes a current diagnostic baseline");
}

#[tokio::test]
async fn automatic_defaults_advertise_the_tool_without_a_marker_or_spawn() {
    let workspace = Workspace::new();
    let manager = LspCodeIntelligence::from_config(LspManagerConfig::default());
    assert!(manager.supports_workspace(&workspace.0));
    let _ = manager.status(&workspace.0).await;
    assert_eq!(manager.pool().running_servers().await, 0);
    let source = workspace.write("source.ts", "const value = 1;\n");
    let unavailable = LspCodeIntelligence::from_config(LspManagerConfig {
        servers: [(
            TYPESCRIPT_LANGUAGE_SERVER.into(),
            ServerOptions {
                path: Some(workspace.0.join("missing-server.exe")),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    assert!(unavailable.supports_workspace(&workspace.0));
    let outcome = unavailable.hover(&position(&workspace, &source)).await;
    assert_eq!(outcome.meta.server, TYPESCRIPT_LANGUAGE_SERVER);
    assert_eq!(outcome.meta.state, CodeIntelServerState::Unavailable);
    assert!(outcome.payload["error"]
        .as_str()
        .unwrap()
        .contains("configured"));
    assert_eq!(unavailable.pool().running_servers().await, 0);
    unavailable.shutdown().await;
    manager.shutdown().await;
}

#[tokio::test]
async fn file_extensions_route_to_the_correct_profile_and_language_id() {
    let workspace = Workspace::new();
    workspace.mixed();
    let rust = workspace.write("src/main.rs", "fn main() {}\n");
    let log = workspace.0.join("typescript-wire.jsonl");
    let manager = mock_manager(
        json!({}),
        json!({"logPath": log, "publishDiagnostics": false}),
    );
    let rust_outcome = manager.hover(&position(&workspace, &rust)).await;
    assert_eq!(rust_outcome.meta.server, RUST_ANALYZER);
    for (extension, language) in [
        ("js", "javascript"),
        ("jsx", "javascriptreact"),
        ("mjs", "javascript"),
        ("cjs", "javascript"),
        ("ts", "typescript"),
        ("tsx", "typescriptreact"),
        ("mts", "typescript"),
        ("cts", "typescript"),
    ] {
        let source = workspace.write(&format!("src/file.{extension}"), "const value = 1;\n");
        let outcome = manager.hover(&position(&workspace, &source)).await;
        assert_eq!(
            outcome.meta.server, TYPESCRIPT_LANGUAGE_SERVER,
            "{outcome:?}"
        );
        assert!(outcome.payload.get("error").is_none(), "{outcome:?}");
        // Match this file's own didOpen: the log is cumulative, so a language
        // shared with an earlier extension must not satisfy the check.
        let file_suffix = format!("/file.{extension}");
        let messages = std::fs::read_to_string(&log).unwrap();
        assert!(
            messages
                .lines()
                .filter_map(|row| serde_json::from_str::<Value>(row).ok())
                .any(|row| {
                    let document = &row["message"]["params"]["textDocument"];
                    row["direction"] == "client_to_server"
                        && row["message"]["method"] == "textDocument/didOpen"
                        && document["uri"]
                            .as_str()
                            .is_some_and(|uri| uri.ends_with(&file_suffix))
                        && document["languageId"] == language
                }),
            "no didOpen for file.{extension} as {language}"
        );
    }
    assert_eq!(manager.pool().running_servers().await, 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn mixed_workspace_queries_require_an_explicit_profile_before_any_spawn() {
    let workspace = Workspace::new();
    workspace.mixed();
    let manager = mock_manager(json!({}), json!({}));
    let symbols = CodeIntelSymbolQuery {
        workspace: workspace.0.clone(),
        query: Some("value".into()),
        max_results: 20,
        ..Default::default()
    };
    let ambiguous = manager.symbols(&symbols).await;
    assert!(ambiguous.payload["error"].is_string(), "{ambiguous:?}");
    assert_eq!(
        ambiguous.payload["servers"],
        json!([RUST_ANALYZER, TYPESCRIPT_LANGUAGE_SERVER])
    );
    let ambiguous = manager
        .diagnostics(&CodeIntelDiagnosticsQuery {
            workspace: workspace.0.clone(),
            max_results: 20,
            ..Default::default()
        })
        .await;
    assert!(ambiguous.payload["error"].is_string(), "{ambiguous:?}");
    assert_eq!(manager.pool().running_servers().await, 0);
    let selected = manager
        .symbols(&CodeIntelSymbolQuery {
            server: Some(TYPESCRIPT_LANGUAGE_SERVER.into()),
            ..symbols
        })
        .await;
    assert_eq!(selected.meta.server, TYPESCRIPT_LANGUAGE_SERVER);
    assert!(selected.payload.get("error").is_none(), "{selected:?}");
    assert_eq!(manager.pool().running_servers().await, 1);
    let selected = manager
        .diagnostics(&CodeIntelDiagnosticsQuery {
            workspace: workspace.0.clone(),
            server: Some(RUST_ANALYZER.into()),
            max_results: 20,
            ..Default::default()
        })
        .await;
    assert_eq!(selected.meta.server, RUST_ANALYZER);
    assert_eq!(manager.pool().running_servers().await, 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn first_workspace_symbol_query_opens_one_nearest_source_from_the_existing_snapshot() {
    let workspace = Workspace::new();
    workspace.write("package.json", "{}\n");
    workspace.write(
        "packages/nested/src/earlier.ts",
        "export const nested = 1;\n",
    );
    workspace.write("src/module.ts", "export const module = 1;\n");
    let anchor = workspace.write("alpha.ts", "export const rootValue = 1;\n");
    workspace.write("omega.ts", "export const otherRootValue = 1;\n");
    let log = workspace.0.join("typescript-wire.jsonl");
    let manager = mock_manager(
        json!({}),
        json!({"logPath": log, "publishDiagnostics": false}),
    );
    let query = CodeIntelSymbolQuery {
        workspace: workspace.0.clone(),
        query: Some("rootValue".into()),
        max_results: 20,
        ..Default::default()
    };
    for _ in 0..2 {
        let outcome = manager.symbols(&query).await;
        assert_eq!(outcome.meta.server, TYPESCRIPT_LANGUAGE_SERVER);
        assert_eq!(
            outcome.meta.completeness,
            slim_core::codeintel::CodeIntelCompleteness::Unknown
        );
        assert!(outcome.payload.get("error").is_none(), "{outcome:?}");
    }
    let messages = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["direction"] == "client_to_server")
        .map(|row| row["message"].clone())
        .collect::<Vec<_>>();
    let opens = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message["method"] == "textDocument/didOpen")
        .collect::<Vec<_>>();
    assert_eq!(
        opens.len(),
        1,
        "a repeat query must reuse the single anchor"
    );
    assert_eq!(
        opens[0].1["params"]["textDocument"]["uri"],
        url::Url::from_file_path(&anchor).unwrap().as_str()
    );
    let first_symbols = messages
        .iter()
        .position(|message| message["method"] == "workspace/symbol")
        .unwrap();
    assert!(
        opens[0].0 < first_symbols,
        "project loading must precede workspace/symbol"
    );
    assert_eq!(manager.pool().running_servers().await, 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn incompatible_and_unknown_server_filters_fail_without_starting_a_process() {
    let workspace = Workspace::new();
    workspace.mixed();
    let source = workspace.write("src/source.ts", "const value = 1;\n");
    let manager = mock_manager(json!({}), json!({}));
    for profile in [RUST_ANALYZER, "unknown-server"] {
        let outcome = manager
            .diagnostics(&CodeIntelDiagnosticsQuery {
                server: Some(profile.into()),
                ..diagnostics(&workspace, &source)
            })
            .await;
        assert!(outcome.payload["error"].is_string(), "{outcome:?}");
        assert_eq!(manager.pool().running_servers().await, 0);
    }
    let outcome = manager
        .symbols(&CodeIntelSymbolQuery {
            workspace: workspace.0.clone(),
            path: Some(source),
            server: Some(RUST_ANALYZER.into()),
            max_results: 20,
            ..Default::default()
        })
        .await;
    assert!(outcome.payload["error"].is_string(), "{outcome:?}");
    assert_eq!(manager.pool().running_servers().await, 0);
    manager.shutdown().await;
}

#[tokio::test]
async fn nested_tsconfig_keeps_sibling_definitions_inside_the_authorized_workspace() {
    let workspace = Workspace::new();
    workspace.write("package.json", "{}\n");
    workspace.write("packages/a/tsconfig.json", "{\"compilerOptions\":{}}\n");
    let source = workspace.write("packages/a/src/source.ts", "export const value = 1;\n");
    let sibling = workspace.write("packages/b/src/target.ts", "export const target = 2;\n");
    let manager = mock_manager(
        json!({}),
        json!({
            "definitionUri": url::Url::from_file_path(&sibling).unwrap().to_string()
        }),
    );
    let outcome = manager.definition(&position(&workspace, &source)).await;
    assert_eq!(outcome.meta.server, TYPESCRIPT_LANGUAGE_SERVER);
    assert_eq!(
        outcome.payload["file"].as_str().unwrap().replace('\\', "/"),
        "packages/b/src/target.ts"
    );
    let status = manager.status(&workspace.0).await;
    let server = status.payload["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["server"] == TYPESCRIPT_LANGUAGE_SERVER)
        .unwrap();
    assert_eq!(server["root"], workspace.0.to_string_lossy().as_ref());
    assert_eq!(manager.pool().running_servers().await, 1);
    manager.shutdown().await;

    let outside = Workspace::new();
    let external = outside.write("outside.ts", "PRIVATE_OUTSIDE_VALUE\n");
    let manager = mock_manager(
        json!({}),
        json!({
            "definitionUri": url::Url::from_file_path(&external).unwrap().to_string()
        }),
    );
    let outcome = manager.definition(&position(&workspace, &source)).await;
    assert_eq!(outcome.payload["found"], false);
    assert_eq!(outcome.payload["locations_out_of_scope"], 1);
    assert!(!outcome
        .payload
        .to_string()
        .contains("PRIVATE_OUTSIDE_VALUE"));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_typescript_does_not_starve_ready_rust_in_the_global_post_edit_budget() {
    let workspace = Workspace::new();
    workspace.mixed();
    let rust = workspace.write("src/main.rs", "fn main() {}\n");
    let typescript = workspace.write("src/main.ts", "const value = 1;\n");
    let manager = mock_manager(
        json!({"errorsFromDocument": true}),
        json!({"publishDiagnostics": false}),
    );
    warm(&manager, &workspace, &rust).await;
    let _ = manager.hover(&position(&workspace, &typescript)).await;
    for (path, text) in [
        (&typescript, "const BROKEN = 1;\n"),
        (&rust, "let BROKEN = 1;\n"),
    ] {
        std::fs::write(path, text).unwrap();
        manager
            .notify_file_changed(&workspace.0, path, Some(text.into()))
            .await;
    }
    let started = Instant::now();
    let report = tokio::time::timeout(
        Duration::from_secs(2),
        manager.diagnostics_after_edits(
            &workspace.0,
            &[typescript, rust],
            Duration::from_millis(350),
            None,
        ),
    )
    .await
    .expect("one bounded global budget")
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "elapsed: {:?}",
        started.elapsed()
    );
    assert_eq!(report.files.len(), 2);
    assert_eq!(
        report.files[0].server.as_deref(),
        Some(TYPESCRIPT_LANGUAGE_SERVER)
    );
    assert_eq!(report.files[0].verification, EditVerification::Unverified);
    assert_eq!(report.files[1].server.as_deref(), Some(RUST_ANALYZER));
    assert!(
        report.files[1].verification.unverified_reason().is_none(),
        "{report:?}"
    );
    assert_eq!(report.files[1].errors.len(), 1, "{report:?}");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_batch_limit_preserves_original_order_and_ignores_ineligible_files() {
    let workspace = Workspace::new();
    workspace.mixed();
    let paths = (0..14)
        .map(|index| {
            workspace.write(
                &format!(
                    "src/file{index}.{}",
                    if index % 2 == 0 { "ts" } else { "rs" }
                ),
                if index % 2 == 0 {
                    "const value = 1;\n"
                } else {
                    "fn source() {}\n"
                },
            )
        })
        .collect::<Vec<_>>();
    let manager = mock_manager(
        json!({"errorsFromDocument": true}),
        json!({"errorsFromDocument": true}),
    );
    warm(&manager, &workspace, &paths[0]).await;
    warm(&manager, &workspace, &paths[1]).await;
    let mut inputs = vec![
        workspace.write("NOTES.md", "notes\n"),
        workspace.0.join("missing.ts"),
    ];
    inputs.extend(paths.clone());
    inputs.extend(paths.clone());
    let report = manager
        .diagnostics_after_edits(&workspace.0, &inputs, Duration::from_secs(4), None)
        .await
        .unwrap();
    assert_eq!(report.files.len(), 16, "{report:?}");
    assert_eq!(report.files[0].verification, EditVerification::Unsupported);
    assert_eq!(
        report.files[1].verification,
        EditVerification::FileUnavailable
    );
    for (index, file) in report.files[2..].iter().enumerate() {
        assert_eq!(
            file.path.replace('\\', "/"),
            paths[index]
                .strip_prefix(&workspace.0)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        );
        assert_eq!(
            file.server.as_deref(),
            Some(if index % 2 == 0 {
                TYPESCRIPT_LANGUAGE_SERVER
            } else {
                RUST_ANALYZER
            })
        );
        if index >= 12 {
            assert_eq!(
                file.verification,
                EditVerification::LimitExceeded,
                "{report:?}"
            );
        } else {
            assert!(
                file.verification.unverified_reason().is_none(),
                "{report:?}"
            );
        }
    }
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_first_typescript_file_does_not_hide_a_ready_second_file() {
    let workspace = Workspace::new();
    let silent = workspace.write("src/silent.ts", "const value = 1;\n");
    let ready = workspace.write("src/ready.ts", "const other = 1;\n");
    let manager = mock_manager(
        json!({}),
        json!({"errorsFromDocument": true, "skipUnchanged": true}),
    );
    warm(&manager, &workspace, &silent).await;
    warm(&manager, &workspace, &ready).await;
    for (path, text) in [
        (&silent, "const value = 2;\n"),
        (&ready, "const BROKEN = 2;\n"),
    ] {
        std::fs::write(path, text).unwrap();
        manager
            .notify_file_changed(&workspace.0, path, Some(text.into()))
            .await;
    }
    let report = manager
        .diagnostics_after_edits(
            &workspace.0,
            &[silent, ready],
            Duration::from_millis(450),
            None,
        )
        .await
        .unwrap();
    assert_eq!(report.files.len(), 2);
    assert_eq!(report.files[0].path.replace('\\', "/"), "src/silent.ts");
    assert_eq!(report.files[0].verification, EditVerification::Unverified);
    assert_eq!(report.files[1].path.replace('\\', "/"), "src/ready.ts");
    assert!(
        report.files[1].verification.unverified_reason().is_none(),
        "{report:?}"
    );
    assert_eq!(report.files[1].errors.len(), 1, "{report:?}");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_diagnostic_wait_or_settle_does_not_return_ready() {
    for publishes in [false, true] {
        let workspace = Workspace::new();
        let source = workspace.write("src/source.ts", "const value = 1;\n");
        let manager = mock_manager(
            json!({}),
            if publishes {
                json!({"errorsFromDocument": true})
            } else {
                json!({"publishDiagnostics": false})
            },
        );
        if publishes {
            warm(&manager, &workspace, &source).await;
        } else {
            let _ = manager.hover(&position(&workspace, &source)).await;
        }
        wait_for_leases(&manager, 0).await;
        let cancellation = CancellationToken::new();
        let query = CodeIntelDiagnosticsQuery {
            cancellation: Some(cancellation.clone()),
            ..diagnostics(&workspace, &source)
        };
        let task = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.diagnostics(&query).await })
        };
        wait_for_leases(&manager, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "wait/settle must still be pending before cancellation"
        );
        cancellation.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            outcome.meta.state,
            CodeIntelServerState::Ready,
            "{outcome:?}"
        );
        assert!(
            outcome.payload["error"]
                .as_str()
                .is_some_and(|reason| reason.contains("cancel")),
            "{outcome:?}"
        );
        manager.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_symbol_timeout_keeps_the_automatically_selected_profile_id() {
    for profile in [RUST_ANALYZER, TYPESCRIPT_LANGUAGE_SERVER] {
        let workspace = Workspace::new();
        if profile == RUST_ANALYZER {
            workspace.write(
                "Cargo.toml",
                "[package]\nname='timeout-fixture'\nversion='0.1.0'\n",
            );
        } else {
            workspace.write("package.json", "{}\n");
        }
        let mock = json!({"requestDelayMs": 5_000});
        let manager =
            mock_manager_with_timeout(mock.clone(), mock.clone(), Duration::from_millis(50));
        let discovered = discover_server(
            &workspace.0,
            profile,
            Some(Path::new(env!("CARGO_BIN_EXE_slim-lsp-mock"))),
            Some(&[]),
        );
        // Establish initialization independently; the deadline being tested is
        // the global query, rather than platform-dependent process startup.
        let lease = manager
            .pool()
            .acquire(
                discovered.root.unwrap(),
                discovered.spec.unwrap(),
                &json!({"mock": mock}),
                &json!({}),
                TransportOptions {
                    request_timeout: Duration::from_millis(500),
                    ..Default::default()
                },
                32,
            )
            .await
            .unwrap();
        drop(lease);
        let outcome = manager
            .symbols(&CodeIntelSymbolQuery {
                workspace: workspace.0.clone(),
                query: Some("value".into()),
                max_results: 20,
                ..Default::default()
            })
            .await;
        assert_eq!(outcome.meta.server, profile, "{outcome:?}");
        assert_eq!(
            outcome.meta.state,
            CodeIntelServerState::Degraded,
            "{outcome:?}"
        );
        assert!(
            outcome.payload["error"]
                .as_str()
                .is_some_and(|reason| reason.contains("deadline")),
            "{outcome:?}"
        );
        manager.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_diagnostics_startup_timeout_keeps_the_selected_profile_id() {
    for profile in [RUST_ANALYZER, TYPESCRIPT_LANGUAGE_SERVER] {
        let workspace = Workspace::new();
        if profile == RUST_ANALYZER {
            workspace.write(
                "Cargo.toml",
                "[package]\nname='timeout-fixture'\nversion='0.1.0'\n",
            );
        } else {
            workspace.write("package.json", "{}\n");
        }
        let manager = LspCodeIntelligence::from_config(LspManagerConfig {
            idle_shutdown: None,
            request_timeout: Duration::from_millis(50),
            servers: [(
                profile.into(),
                ServerOptions {
                    path: Some(PathBuf::from(env!("CARGO_BIN_EXE_slim-lsp-mock"))),
                    args: Some(vec!["--initialize-delay-ms".into(), "5000".into()]),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        });
        let outcome = manager
            .diagnostics(&CodeIntelDiagnosticsQuery {
                workspace: workspace.0.clone(),
                max_results: 20,
                ..Default::default()
            })
            .await;
        assert_eq!(outcome.meta.server, profile, "{outcome:?}");
        assert_eq!(
            outcome.meta.state,
            CodeIntelServerState::Degraded,
            "{outcome:?}"
        );
        assert!(
            outcome.payload["error"]
                .as_str()
                .is_some_and(|reason| reason.contains("deadline")),
            "{outcome:?}"
        );
        manager.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_typescript_refresh_does_not_starve_ready_rust() {
    let workspace = Workspace::new();
    workspace.mixed();
    let rust = workspace.write("src/main.rs", "fn main() {}\n");
    let typescript = workspace.write("src/main.ts", "const value = 1;\n");
    let changed = (0..600)
        .map(|index| {
            workspace.write(
                &format!("src/watch_{index}_long_module_name_for_pipe_backpressure.ts"),
                "const value = 1;\n",
            )
        })
        .collect::<Vec<_>>();
    let gate = workspace.write("request.gate", "");
    let log = workspace.0.join("typescript-wire.jsonl");
    let manager = mock_manager(
        json!({"errorsFromDocument": true}),
        json!({
            "errorsFromDocument": true, "requestGate": gate, "logPath": log
        }),
    );
    warm(&manager, &workspace, &rust).await;
    warm(&manager, &workspace, &typescript).await;
    std::fs::remove_file(&gate).unwrap();
    let blocked = {
        let manager = manager.clone();
        let query = position(&workspace, &typescript);
        tokio::spawn(async move { manager.hover(&query).await })
    };
    wait_for_wire_method(&log, "textDocument/hover").await;
    assert!(
        !blocked.is_finished(),
        "subprocess is held before reading further input"
    );
    for path in changed {
        std::fs::write(path, "const value = 200;\n").unwrap();
    }
    std::fs::write(&rust, "let BROKEN = 1;\n").unwrap();
    manager
        .notify_file_changed(&workspace.0, &rust, Some("let BROKEN = 1;\n".into()))
        .await;
    let report = manager
        .diagnostics_after_edits(
            &workspace.0,
            &[typescript, rust],
            Duration::from_millis(750),
            None,
        )
        .await
        .unwrap();
    // Always release the real subprocess before assertions and shutdown.
    std::fs::write(&gate, "").unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), blocked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        report.files[0].verification,
        EditVerification::RefreshFailed,
        "pipe backpressure must exercise the refresh phase: {report:?}"
    );
    assert!(
        report.files[1].verification.unverified_reason().is_none(),
        "{report:?}"
    );
    assert_eq!(report.files[1].errors.len(), 1, "{report:?}");
    manager.shutdown().await;
}
