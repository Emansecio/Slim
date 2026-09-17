//! Planner-level regressions for tool-result presentation budgeting.
//!
//! * A duplicate inside the same tool batch must be priced as the omission
//!   notice, not as a second full copy: under a window that fits one copy
//!   plus the notice (R+P <= B < 2R) the first occurrence stays complete.
//! * The `fits` probe stays on the structural estimate when the adapter
//!   provides a request-envelope bound, and keeps the exact prepared-request
//!   path when it does not.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_core::context::AdaptiveTokenEstimator;
use slim_core::provider::{
    HttpProviderClient, HttpRequest, OpenAiCompatibleAdapter, PreparedProviderRequest,
    ProviderAdapter, ProviderCapabilities, ProviderConfig, ProviderContentBlock, ProviderError,
    ProviderEvent, ProviderKind, ProviderMessage,
};
use slim_core::runtime::{AgentLoopConfig, AgentLoopResult, AgentLoopStop};
use slim_core::{OperatingMode, ReasoningClassification, Runtime};

const DUPLICATE_NOTICE: &str =
    "[duplicate read result omitted; identical output already in context]";

fn request(listener: &TcpListener) -> (TcpStream, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "next provider request missing");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 16384];
    loop {
        let size = stream.read(&mut chunk).unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&chunk[..size]);
        assert!(bytes.len() < 4 * 1024 * 1024);
        let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        if bytes.len() >= end + 4 + length {
            return (
                stream,
                String::from_utf8(bytes[end + 4..end + 4 + length].to_vec()).unwrap(),
            );
        }
    }
}

fn respond(stream: &mut TcpStream, delta: Value, finish: &str) {
    let event = json!({"choices":[{"delta":delta,"finish_reason":finish}]});
    let body = format!("data: {event}\n\ndata: [DONE]\n\n");
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

/// One provider turn answering with two identical `read` calls, then a plain
/// completion once the batch comes back.
fn serve_identical_reads(listener: TcpListener) -> thread::JoinHandle<Vec<String>> {
    thread::spawn(move || {
        let mut bodies = Vec::new();
        let (mut stream, body) = request(&listener);
        bodies.push(body);
        let arguments = json!({"path": "dup.txt"}).to_string();
        respond(
            &mut stream,
            json!({"tool_calls": [
                {"index": 0, "id": "read-a", "function": {"name": "read", "arguments": arguments}},
                {"index": 1, "id": "read-b", "function": {"name": "read", "arguments": arguments}}
            ]}),
            "tool_calls",
        );
        let (mut stream, body) = request(&listener);
        bodies.push(body);
        respond(&mut stream, json!({"content": "done"}), "stop");
        bodies
    })
}

fn run_identical_reads<A, F>(
    root: &Path,
    seed: &[ProviderMessage],
    context_window_tokens: u64,
    build_adapter: F,
) -> (Vec<String>, AgentLoopResult)
where
    A: ProviderAdapter + Send + Sync + 'static,
    F: FnOnce(String) -> A,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = serve_identical_reads(listener);
    let client = HttpProviderClient::new(build_adapter(endpoint), Duration::from_secs(10)).unwrap();
    let mut runtime = Runtime::new();
    let outcome =
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime.run_agent_loop_with_messages(
                &client,
                seed,
                OperatingMode::Auto,
                root,
                1,
                AgentLoopConfig {
                    max_turns: 4,
                    context_window_tokens,
                    ..AgentLoopConfig::default()
                },
            ));
    let outcome = outcome.unwrap_or_else(|error| {
        panic!(
            "batch must reach the next request (window={context_window_tokens}): {error:?}; sizes={:?}",
            runtime
                .conversation()
                .iter()
                .map(|message| (message.role.clone(), message.content.len()))
                .collect::<Vec<_>>()
        )
    });
    let bodies = server.join().unwrap();
    (bodies, outcome)
}

fn tool_contents(body: &str) -> Vec<String> {
    serde_json::from_str::<Value>(body).unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap().to_owned())
        .collect()
}

fn fixture_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "slim-plan-budget-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Delegating adapter that counts full request preparations and can mask the
/// structural envelope bound to exercise the prepared-request fallback.
struct CountingAdapter {
    inner: OpenAiCompatibleAdapter,
    prepares: Arc<AtomicUsize>,
    envelope: Option<u64>,
}

impl ProviderAdapter for CountingAdapter {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    fn wire_kind(&self) -> ProviderKind {
        self.inner.wire_kind()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        self.inner.materialize_prompt_cache_intent(body)
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn reasoning_classification(&self) -> Option<ReasoningClassification> {
        self.inner.reasoning_classification()
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.inner.system_prompt_for_budget()
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        self.envelope
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        self.inner.response_cache_scope_id()
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.inner.build_request(prompt)
    }

    fn cache_namespace(&self) -> String {
        self.inner.cache_namespace()
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        self.inner.build_messages_request(messages)
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.inner
            .build_messages_request_with_tools(messages, tools)
    }

    fn build_messages_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<HttpRequest, ProviderError> {
        self.inner.build_messages_request_checked(messages)
    }

    fn build_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<HttpRequest, ProviderError> {
        self.inner
            .build_messages_request_with_tools_checked(messages, tools)
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        self.inner
            .prepare_messages_request_with_tools_checked(messages, tools)
    }

    fn prepare_messages_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.inner.prepare_messages_request_checked(messages)
    }

    fn build_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<HttpRequest, ProviderError> {
        self.inner.build_compaction_request_checked(messages)
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.inner.prepare_compaction_request_checked(messages)
    }

    fn cache_key(&self, messages: &[ProviderMessage]) -> String {
        self.inner.cache_key(messages)
    }

    fn cache_key_with_tools(&self, messages: &[ProviderMessage], tools: &[Value]) -> String {
        self.inner.cache_key_with_tools(messages, tools)
    }

    fn cache_key_for_prepared(&self, request: &PreparedProviderRequest) -> String {
        self.inner.cache_key_for_prepared(request)
    }

    fn sensitive_values(&self) -> Vec<String> {
        self.inner.sensitive_values()
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.inner.parse_event(value)
    }
}

#[test]
fn same_batch_duplicate_read_keeps_first_copy_complete_under_tight_window() {
    let root = fixture_root("dup");
    // 160 numbered rows of 150 bytes ≈ 24 KiB of identical read output.
    let source = (0..160)
        .map(|n| format!("row-{n:04}-{}", "y".repeat(139)))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("dup.txt"), &source).unwrap();
    let seed = [ProviderMessage::user("Read dup.txt twice")];

    // Calibration run under a generous window: captures the complete first
    // presentation and the serialized size of the [full, notice] request.
    let (bodies, outcome) = run_identical_reads(&root, &seed, 200_000, |endpoint| {
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .unwrap()
    });
    assert_eq!(outcome.stop, AgentLoopStop::ProviderCompleted);
    let contents = tool_contents(&bodies[1]);
    assert_eq!(contents.len(), 2);
    let full = contents[0].clone();
    assert_eq!(full, source, "calibration run must carry the complete read");
    assert_eq!(contents[1], DUPLICATE_NOTICE);

    // Pick the window inside R+P <= B < 2R: one copy plus the notice must
    // fit, two copies must not.
    let estimate =
        |chars: u64| AdaptiveTokenEstimator::default().estimate("fixture", "fixture", chars);
    let serialized = bodies[1].chars().count() as u64;
    let full_chars = full.chars().count() as u64;
    let window = 4_096 + estimate(serialized + full_chars * 6 / 10);
    assert!(
        estimate(serialized) + 4_096 <= window,
        "window must admit one copy plus the notice"
    );
    assert!(
        estimate(serialized + full_chars) + 4_096 > window,
        "window must reject two full copies for the regression to be decisive"
    );

    let prepares = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&prepares);
    let (bodies, outcome) = run_identical_reads(&root, &seed, window, |endpoint| CountingAdapter {
        inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            endpoint,
            "fixture-model",
            "fixture-key",
        ))
        .unwrap(),
        prepares: counter,
        envelope: Some(1_024),
    });
    assert_eq!(outcome.stop, AgentLoopStop::ProviderCompleted);
    let contents = tool_contents(&bodies[1]);
    assert_eq!(
        contents,
        vec![full, DUPLICATE_NOTICE.to_owned()],
        "the second copy must collapse to the notice without shrinking the first"
    );
    assert_eq!(
        prepares.load(Ordering::SeqCst),
        2,
        "fits must stay on the structural estimate; only the two real sends prepare"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn fits_falls_back_to_prepared_request_only_without_envelope_bound() {
    let root = fixture_root("fallback");
    // Unicode + JSON escapes + a seeded multimodal message keep the
    // structural estimate off a trivial path in both adapter modes.
    let source = (0..40)
        .map(|n| format!("row-{n:04}-界\"escaped\\\"{}é\n", "z".repeat(64)))
        .collect::<String>();
    std::fs::write(root.join("dup.txt"), &source).unwrap();
    let seed = [
        ProviderMessage::user("context image")
            .with_content_blocks(vec![ProviderContentBlock::image("image/png", "aGVsbG8=")]),
        ProviderMessage::user("Read dup.txt twice"),
    ];

    let run = |envelope: Option<u64>| {
        let prepares = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&prepares);
        let (bodies, outcome) =
            run_identical_reads(&root, &seed, 200_000, |endpoint| CountingAdapter {
                inner: OpenAiCompatibleAdapter::new(ProviderConfig::openai(
                    endpoint,
                    "fixture-model",
                    "fixture-key",
                ))
                .unwrap(),
                prepares: counter,
                envelope,
            });
        assert_eq!(outcome.stop, AgentLoopStop::ProviderCompleted);
        (bodies, prepares.load(Ordering::SeqCst))
    };

    let (structural_bodies, structural_prepares) = run(Some(1_024));
    let (fallback_bodies, fallback_prepares) = run(None);

    assert_eq!(
        structural_bodies, fallback_bodies,
        "the structural budget path must choose the same wire content"
    );
    assert_eq!(tool_contents(&structural_bodies[1])[0], source);
    assert_eq!(
        structural_prepares, 2,
        "envelope-bound adapter: only the two real sends prepare"
    );
    assert_eq!(
        fallback_prepares, 3,
        "no-bound adapter keeps the exact prepared path: one prepare per turn plus one fits probe"
    );
    let _ = std::fs::remove_dir_all(root);
}
