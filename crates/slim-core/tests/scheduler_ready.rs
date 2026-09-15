use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};
use slim_core::codeintel::CodeIntelFileUpdate;
use slim_core::provider::{HttpProviderClient, OpenAiCompatibleAdapter, ProviderConfig};
use slim_core::runtime::{AgentLoopConfig, AgentLoopStop};
use slim_core::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelMeta, CodeIntelOutcome,
    CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery, CodeIntelligence,
    EventKind, OperatingMode, Runtime,
};

struct SchedulerIntel {
    active: AtomicUsize,
    peak: AtomicUsize,
    sync_revision: AtomicU64,
    started_before_sync: AtomicUsize,
    all_started: Arc<tokio::sync::Barrier>,
}

impl SchedulerIntel {
    fn new(query_count: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            sync_revision: AtomicU64::new(0),
            started_before_sync: AtomicUsize::new(0),
            all_started: Arc::new(tokio::sync::Barrier::new(query_count)),
        }
    }

    fn outcome(&self, query: impl Into<String>) -> CodeIntelOutcome {
        CodeIntelOutcome {
            meta: CodeIntelMeta {
                server: "scheduler-fixture".into(),
                state: CodeIntelServerState::Ready,
                completeness: CodeIntelCompleteness::Complete,
                document_version: Some(
                    i64::try_from(self.sync_revision.load(Ordering::SeqCst)).unwrap_or(i64::MAX),
                ),
                stale: false,
                elapsed_ms: 1,
            },
            payload: json!({
                "kind": "workspace",
                "query": query.into(),
                "symbols": []
            }),
        }
    }
}

#[async_trait]
impl CodeIntelligence for SchedulerIntel {
    fn supports_workspace(&self, _workspace: &Path) -> bool {
        true
    }

    async fn status(&self, _workspace: &Path) -> CodeIntelOutcome {
        self.outcome("status")
    }

    async fn definition(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        self.outcome("definition")
    }

    async fn references(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        self.outcome("references")
    }

    async fn hover(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        self.outcome("hover")
    }

    async fn symbols(&self, query: &CodeIntelSymbolQuery) -> CodeIntelOutcome {
        if self.sync_revision.load(Ordering::SeqCst) == 0 {
            self.started_before_sync.fetch_add(1, Ordering::SeqCst);
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        // A bounded barrier makes the test fail promptly if the scheduler
        // serializes the wave instead of waiting forever on the third call.
        let _ = tokio::time::timeout(Duration::from_millis(500), self.all_started.wait()).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.outcome(query.query.clone().unwrap_or_default())
    }

    async fn diagnostics(&self, _query: &CodeIntelDiagnosticsQuery) -> CodeIntelOutcome {
        self.outcome("diagnostics")
    }

    async fn notify_file_changed(&self, _workspace: &Path, _path: &Path, _text: Option<String>) {}

    async fn notify_file_updated(
        &self,
        _workspace: &Path,
        _path: &Path,
        _update: CodeIntelFileUpdate,
    ) {
        self.sync_revision.fetch_add(1, Ordering::SeqCst);
    }
}

fn accept_with_deadline(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("blocking provider stream");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "provider request missing");
                thread::yield_now();
            }
            Err(error) => panic!("provider accept: {error}"),
        }
    }
}

fn read_http_body(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let size = stream.read(&mut chunk).expect("provider request");
        assert!(size > 0, "provider closed before request body");
        raw.extend_from_slice(&chunk[..size]);
        assert!(raw.len() <= 8 * 1024 * 1024, "provider request too large");
        let Some(header_end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&raw[..header_end]).expect("request headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("content length");
        let body_start = header_end + 4;
        if raw.len() >= body_start + content_length {
            return String::from_utf8(raw[body_start..body_start + content_length].to_vec())
                .expect("request body");
        }
    }
}

fn send_sse(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    stream
        .write_all(response.as_bytes())
        .expect("provider response");
}

fn tool_call(index: usize, id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "index": index,
        "id": id,
        "function": {"name": name, "arguments": arguments.to_string()}
    })
}

#[test]
fn mutation_releases_ready_queries_as_one_ordered_revisioned_wave() {
    let root = std::env::temp_dir().join(format!(
        "slim-scheduler-ready-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("workspace");
    std::fs::write(root.join("tracked.txt"), "old\n").expect("fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("provider bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("provider address");
    let server = thread::spawn(move || {
        let mut first = accept_with_deadline(&listener);
        let _ = read_http_body(&mut first);
        let calls = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        tool_call(0, "write-1", "write", json!({"path":"tracked.txt","content":"new\n","expected":"old\n"})),
                        tool_call(1, "query-1", "code_intel", json!({"action":"symbol","query":"symbol_1"})),
                        tool_call(2, "query-2", "code_intel", json!({"action":"symbol","query":"symbol_2"})),
                        tool_call(3, "query-3", "code_intel", json!({"action":"symbol","query":"symbol_3"}))
                    ]
                }
            }]
        });
        let body = format!(
            "data: {calls}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        send_sse(&mut first, &body);

        let mut second = accept_with_deadline(&listener);
        let request = read_http_body(&mut second);
        let done = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        send_sse(&mut second, done);
        request
    });

    let backend = Arc::new(SchedulerIntel::new(3));
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "scheduler-fixture",
        "scheduler-key",
    ))
    .expect("adapter");
    let client = HttpProviderClient::new(adapter, Duration::from_secs(5)).expect("client");
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(backend.clone());
    let result = tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(runtime.run_agent_loop(
            &client,
            "inspect the updated workspace",
            OperatingMode::Auto,
            &root,
            1,
            AgentLoopConfig {
                max_turns: 2,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("agent loop");
    let request = server.join().expect("provider server");

    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 4);
    assert_eq!(backend.peak.load(Ordering::SeqCst), 3);
    assert_eq!(backend.started_before_sync.load(Ordering::SeqCst), 0);
    assert_eq!(backend.sync_revision.load(Ordering::SeqCst), 1);
    assert!(result
        .tool_results
        .iter()
        .skip(1)
        .all(|result| result.output.contains("document_version: 1")));
    let positions = ["symbol_1", "symbol_2", "symbol_3"].map(|needle| {
        request
            .find(needle)
            .expect("query result in provider request")
    });
    assert!(positions[0] < positions[1] && positions[1] < positions[2]);

    let lifecycle = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolStarted { call_id, .. } => Some(("start", call_id.as_str())),
            EventKind::ToolFinished { call_id, .. } => Some(("finish", call_id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let write_finish = lifecycle
        .iter()
        .position(|event| *event == ("finish", "write-1"))
        .expect("write finish");
    for id in ["query-1", "query-2", "query-3"] {
        let start = lifecycle
            .iter()
            .position(|event| *event == ("start", id))
            .expect("query start");
        assert!(start > write_finish, "query {id} started before write sync");
    }

    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).expect("updated fixture"),
        "new\n"
    );
    let _ = std::fs::remove_dir_all(root);
}
