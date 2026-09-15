//! Optional integration check against the rust-analyzer executable on PATH.
//!
//! This test is intentionally ignored: it requires a locally installed
//! rust-analyzer and is useful as an explicit release/fixture check only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use slim_core::codeintel::{
    CodeIntelEditPosition, CodeIntelFileUpdate, CodeIntelOutcome, CodeIntelPatch,
    CodeIntelPositionQuery, CodeIntelTextEdit, CodeIntelligence,
};
use slim_lsp::pool::{PoolConfig, StdioProcessFactory};
use slim_lsp::{LspCodeIntelligence, LspManagerConfig, LspProcessPool};

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slim-lsp-real-incremental-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).expect("create fixture");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='real-incremental-fixture'\nversion='0.1.0'\nedition='2021'\n",
        )
        .expect("write manifest");
        std::fs::write(
            root.join("src/main.rs"),
            "fn old() {}\nfn new() {}\nfn main() { old(); }\n",
        )
        .expect("write source");
        Self(root)
    }

    fn root(&self) -> &Path {
        &self.0
    }

    fn source(&self) -> PathBuf {
        self.0.join("src/main.rs")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn manager(root: &Path) -> (Arc<LspProcessPool>, Arc<LspCodeIntelligence>) {
    manager_with_factory(root, Arc::new(StdioProcessFactory))
}

fn manager_with_factory(
    root: &Path,
    factory: Arc<dyn slim_lsp::pool::ProcessFactory>,
) -> (Arc<LspProcessPool>, Arc<LspCodeIntelligence>) {
    let pool = LspProcessPool::new(PoolConfig {
        idle_shutdown: None,
        circuit_window: Duration::from_millis(10),
        max_servers: 1,
        factory,
    });
    let server_config = json!({
        "checkOnSave": false,
        "cargo": { "buildScripts": { "enable": false } },
        "procMacro": { "enable": false }
    });
    let manager = LspCodeIntelligence::new(
        pool.clone(),
        LspManagerConfig {
            idle_shutdown: None,
            max_servers: 1,
            request_timeout: Duration::from_secs(15),
            server_config,
            max_open_documents: 8,
            // Deliberately use rust-analyzer from PATH; this test must not
            // install or replace a server binary.
            server_path: None,
        },
    );
    assert!(root.join("Cargo.toml").is_file());
    (pool, manager)
}

fn definition_query(root: &Path, path: &Path) -> CodeIntelPositionQuery {
    CodeIntelPositionQuery {
        workspace: root.to_path_buf(),
        path: path.to_path_buf(),
        line: 3,
        // `old`/`new` starts at the 13th human column in `fn main() { ... }`.
        column: 13,
        symbol: Some("called function".into()),
        max_results: 20,
        offset: 0,
        revision: None,
        cancellation: None,
    }
}

async fn definition_until_found(
    manager: &Arc<LspCodeIntelligence>,
    root: &Path,
    path: &Path,
) -> CodeIntelOutcome {
    let mut last = None;
    for _ in 0..80 {
        let outcome = manager.definition(&definition_query(root, path)).await;
        if outcome.payload["found"] == true {
            return outcome;
        }
        last = Some(outcome);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    last.expect("definition attempt")
}

fn rename_patch(old: &str) -> CodeIntelPatch {
    CodeIntelPatch::new(
        old,
        vec![CodeIntelTextEdit {
            start: CodeIntelEditPosition {
                line: 2,
                prefix: "fn main() { ".into(),
            },
            end: CodeIntelEditPosition {
                line: 2,
                prefix: "fn main() { old".into(),
            },
            text: "new".into(),
        }],
    )
}

async fn run_case(label: &str, incremental: bool) -> serde_json::Value {
    let fixture = Fixture::new(label);
    let root = fixture.root().to_path_buf();
    let path = fixture.source();
    let (pool, manager) = manager(&root);
    let old = "fn old() {}\nfn new() {}\nfn main() { old(); }\n";
    let new = "fn old() {}\nfn new() {}\nfn main() { new(); }\n";
    let initial = definition_until_found(&manager, &root, &path).await;
    if initial.payload["found"] != true {
        pool.close_all().await;
    }
    assert_eq!(
        initial.payload["found"], true,
        "initial definition: {initial:?}"
    );
    assert_eq!(initial.payload["line"], 1);

    std::fs::write(&path, new).expect("write final fixture");
    if incremental {
        manager
            .notify_file_updated(
                &root,
                &path,
                CodeIntelFileUpdate {
                    text: new.into(),
                    patch: Some(rename_patch(old)),
                },
            )
            .await;
    } else {
        manager
            .notify_file_changed(&root, &path, Some(new.into()))
            .await;
    }
    let final_outcome = definition_until_found(&manager, &root, &path).await;
    pool.close_all().await;
    assert_eq!(
        final_outcome.payload["found"], true,
        "final definition: {final_outcome:?}"
    );
    assert_eq!(
        Path::new(final_outcome.payload["file"].as_str().unwrap()),
        Path::new("src/main.rs")
    );
    assert_eq!(final_outcome.payload["line"], 2);
    assert_eq!(final_outcome.payload["column"], 4);
    assert_eq!(final_outcome.payload["preview"], "fn new() {}");
    assert!(!final_outcome.meta.stale);
    assert_eq!(final_outcome.meta.document_version, Some(2));
    final_outcome.payload
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires rust-analyzer on PATH; run explicitly"]
async fn real_rust_analyzer_incremental_matches_full_definition() {
    let incremental = run_case("incremental", true).await;
    let full = run_case("full", false).await;
    assert_eq!(incremental, full);
    println!("real_rust_analyzer_equivalence={incremental}");
}

// A test-only wire observer around the real stdio connection. It records
// completed IO chunks and parses them after the timed operation, not on the
// request hot path. No production transport changes are needed for measurement.
mod semantic_utility {
    use super::*;
    use slim_lsp::pool::{ProcessFactory, SpawnedServer};
    use slim_lsp::transport::IoBox;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Instant;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    type Chunks = Arc<Mutex<Vec<(Instant, Vec<u8>)>>>;
    #[derive(Default)]
    struct TraceFactory {
        sent: Chunks,
        received: Chunks,
    }
    struct TracedIo {
        inner: IoBox,
        sent: Chunks,
        received: Chunks,
    }

    impl ProcessFactory for TraceFactory {
        fn spawn(
            &self,
            spec: &slim_lsp::discovery::ServerSpec,
            root: &Path,
        ) -> Result<SpawnedServer, String> {
            let SpawnedServer {
                io,
                stderr_tail,
                process,
            } = StdioProcessFactory.spawn(spec, root)?;
            Ok(SpawnedServer::new(
                Box::new(TracedIo {
                    inner: io,
                    sent: self.sent.clone(),
                    received: self.received.clone(),
                }),
                stderr_tail,
                process,
            ))
        }
    }
    impl AsyncRead for TracedIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let result = Pin::new(&mut self.inner).poll_read(cx, buf);
            if let Poll::Ready(Ok(())) = &result {
                if buf.filled().len() > before {
                    self.received
                        .lock()
                        .unwrap()
                        .push((Instant::now(), buf.filled()[before..].to_vec()));
                }
            }
            result
        }
    }
    impl AsyncWrite for TracedIo {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let result = Pin::new(&mut self.inner).poll_write(cx, buf);
            if let Poll::Ready(Ok(count)) = &result {
                if *count > 0 {
                    self.sent
                        .lock()
                        .unwrap()
                        .push((Instant::now(), buf[..*count].to_vec()));
                }
            }
            result
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }
    fn messages(chunks: &Chunks) -> Vec<(Instant, serde_json::Value)> {
        let mut pending = Vec::new();
        let mut messages = Vec::new();
        for (at, bytes) in chunks.lock().unwrap().iter() {
            pending.extend_from_slice(bytes);
            while let Some(header_end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = std::str::from_utf8(&pending[..header_end]).unwrap();
                let length = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                let end = header_end + 4 + length;
                if pending.len() < end {
                    break;
                }
                messages.push((
                    *at,
                    serde_json::from_slice(&pending[header_end + 4..end]).unwrap(),
                ));
                pending.drain(..end);
            }
        }
        messages
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires installed rust-analyzer; release observation without network calls"]
    async fn real_semantic_utility_and_independent_requests() {
        let fixture = Fixture::new("semantic-utility");
        let root = fixture.root();
        let source="fn old(value: &str) -> usize { value.len() }\nfn new() {}\nfn main() { let _ = old(\"ok\"); }\n";
        std::fs::write(fixture.source(), source).unwrap();
        let trace = Arc::new(TraceFactory::default());
        let (pool, manager) = manager_with_factory(root, trace.clone());
        let query = CodeIntelPositionQuery {
            workspace: root.into(),
            path: fixture.source(),
            line: 3,
            column: (source.lines().nth(2).unwrap().find("old").unwrap() + 1) as u32,
            max_results: 20,
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if manager.definition(&query).await.payload["found"] == true {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "real server did not resolve the fixture"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let symbols = manager
            .symbols(&slim_core::codeintel::CodeIntelSymbolQuery {
                workspace: root.into(),
                path: Some(fixture.source()),
                max_results: 20,
                ..Default::default()
            })
            .await;
        let rendered = slim_core::tools::render_code_intel("symbol", &symbols);
        assert!(
            rendered.contains("detail: fn(value: &str) -> usize"),
            "{rendered}"
        );
        assert!(!symbols.meta.stale);
        println!("real_symbol_result={rendered}");
        // Warm each operation once. Compilation/indexing setup is excluded.
        assert!(
            manager.references(&query).await.payload["total"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(manager.hover(&query).await.payload["found"], true);
        let mut serial = Vec::new();
        let mut parallel = Vec::new();
        let mut all_sent_first = 0;
        let mut reordered = 0;
        let mut offsets = std::collections::BTreeMap::<String, (Vec<u128>, Vec<u128>)>::new();
        for repetition in 0..7 {
            for concurrent in if repetition % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = Instant::now();
                let (definition, references, hover) = if concurrent {
                    tokio::join!(
                        manager.definition(&query),
                        manager.references(&query),
                        manager.hover(&query)
                    )
                } else {
                    (
                        manager.definition(&query).await,
                        manager.references(&query).await,
                        manager.hover(&query).await,
                    )
                };
                let elapsed = start.elapsed().as_micros();
                assert_eq!(definition.payload["found"], true);
                assert!(references.payload["total"].as_u64().unwrap() > 0);
                assert_eq!(hover.payload["found"], true);
                assert!(!definition.meta.stale && !references.meta.stale && !hover.meta.stale);
                if concurrent {
                    parallel.push(elapsed);
                } else {
                    serial.push(elapsed);
                }
                let sent = messages(&trace.sent)
                    .into_iter()
                    .filter(|(at, msg)| {
                        *at >= start
                            && matches!(
                                msg["method"].as_str(),
                                Some(
                                    "textDocument/definition"
                                        | "textDocument/references"
                                        | "textDocument/hover"
                                )
                            )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(sent.len(), 3);
                if concurrent {
                    let received = messages(&trace.received)
                        .into_iter()
                        .filter(|(_, msg)| {
                            msg.get("method").is_none()
                                && sent.iter().any(|(_, request)| request["id"] == msg["id"])
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(received.len(), 3);
                    all_sent_first += usize::from(
                        sent.iter().map(|(at, _)| *at).max().unwrap()
                            < received.iter().map(|(at, _)| *at).min().unwrap(),
                    );
                    reordered += usize::from(
                        sent.iter().map(|(_, msg)| &msg["id"]).collect::<Vec<_>>()
                            != received
                                .iter()
                                .map(|(_, msg)| &msg["id"])
                                .collect::<Vec<_>>(),
                    );
                    for (at, request) in sent {
                        let response = received
                            .iter()
                            .find(|(_, msg)| msg["id"] == request["id"])
                            .unwrap();
                        let row = offsets
                            .entry(request["method"].as_str().unwrap().into())
                            .or_default();
                        row.0.push(at.duration_since(start).as_micros());
                        row.1.push(response.0.duration_since(start).as_micros());
                    }
                }
            }
        }
        pool.close_all().await;
        let stats = |v: &mut Vec<u128>| {
            v.sort_unstable();
            format!("{}[{}..{}]", v[v.len() / 2], v[0], v[v.len() - 1])
        };
        println!("real_independent n=7 serial_us={} concurrent_us={} all_sent_before_first_response={all_sent_first}/7 response_order_different={reordered}/7 requests_per_round=3",stats(&mut serial),stats(&mut parallel));
        for (method, (mut sent, mut received)) in offsets {
            println!("real_transport method={method} frame_written_offset_us={} response_read_offset_us={}",stats(&mut sent),stats(&mut received));
        }
    }
}
