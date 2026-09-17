//! gauge-prepare-cost — per-turn cost decomposition of the real Slim provider
//! pipeline against a loopback SSE server. No network, no provider account.
//!
//! Arms (all execute production slim-core code paths):
//!   loop          run_agent_loop_with_messages + real OpenAiCodexAdapter
//!                 (envelope bound Some => structural estimate preflight + prepare + send)
//!   loop-noenv    same loop, adapter delegates everything but reports
//!                 request_envelope_upper_bound_chars() == None, so preflight
//!                 prepares the request once and reuses it.
//!                 loop - loop-noenv  == P1 estimate_unprepared_request_chars walk.
//!   direct        run_provider_messages_turn with sensitive values registered
//!                 => redact_messages (clone+replace) + prepare + send.
//!   direct-nosens same direct path, no sensitive values => redact short-circuits.
//!                 direct - direct-nosens == P2 redaction cost.
//!   micro         on the actual captured request body: chars().count() vs len()
//!                 == P3 (provider.rs serialized_chars), plus body byte size.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slim_core::provider::{
    HttpProviderClient, HttpRequest, OpenAiCodexAdapter, PreparedProviderRequest,
    ProviderAdapter, ProviderCapabilities, ProviderConfig, ProviderError, ProviderEvent,
    ProviderKind, ProviderMessage, ProviderTimeouts, ProviderToolCall,
};
use slim_core::runtime::{AgentLoopConfig, Runtime};
use slim_core::session::{
    provider_messages_from_records, DurableEntry, DurableEntryRole, DurableOperation,
    DurableOperationKind, DurableRecord, DurableSessionHeader, DurableUsage, JsonlRepo,
};
use slim_core::session::DurableRepo;
use slim_core::{OperatingMode, ReasoningClassification};

/// Minimal Codex-wire SSE: one text delta, then completed with usage.
const SSE_BODY: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1234,\"output_tokens\":5}}}\n\n",
);

fn spawn_server() -> (u16, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    let last_body = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&last_body);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            handle_conn(stream, &sink);
        }
    });
    (port, last_body)
}

fn handle_conn(stream: std::net::TcpStream, sink: &Arc<Mutex<Vec<u8>>>) {
    stream.set_nodelay(true).ok();
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("expect:") && lower.contains("100-continue") {
            if writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                return;
            }
            writer.flush().ok();
        }
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    *sink.lock().expect("sink") = body;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
        SSE_BODY.len(),
        SSE_BODY
    );
    writer.write_all(response.as_bytes()).ok();
    writer.flush().ok();
}

/// History shaped like a real accumulated session: user turn, assistant text
/// plus a tool call, tool result ~12KiB (under max_result_bytes 16KiB).
fn make_history(target_bytes: usize, secrets: &[String]) -> Vec<ProviderMessage> {
    let mut messages = Vec::new();
    let mut total = 0usize;
    let mut index = 0usize;
    while total < target_bytes {
        let call_id = format!("call_{index}");
        let user = ProviderMessage::user(format!("turn {index}: {}", "u".repeat(2048)));
        let assistant = ProviderMessage::assistant(
            format!("ack {index} {}", "a".repeat(512)),
            vec![ProviderToolCall {
                id: call_id.clone(),
                name: "shell".into(),
                arguments: json!({ "command": format!("run --step {index}") }).to_string(),
            }],
        );
        let mut output = format!("tool output {index} {}", "t".repeat(11 * 1024));
        if index % 8 == 3 && !secrets.is_empty() {
            output.push_str(&format!(" leaked {}", secrets[index % secrets.len()]));
        }
        let tool = ProviderMessage::tool("shell", call_id, output);
        for message in [user, assistant, tool] {
            total += message.content.len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.id.len() + call.name.len() + call.arguments.len())
                    .sum::<usize>();
            messages.push(message);
        }
        index += 1;
    }
    messages
}

/// Delegating adapter: identical to OpenAiCodexAdapter except it reports no
/// envelope bound, forcing the eager-prepare preflight (runtime/mod.rs:1628).
struct NoEnvelope<A>(A);

impl<A: ProviderAdapter> ProviderAdapter for NoEnvelope<A> {
    fn kind(&self) -> ProviderKind {
        self.0.kind()
    }
    fn wire_kind(&self) -> ProviderKind {
        self.0.wire_kind()
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.0.capabilities()
    }
    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        self.0.materialize_prompt_cache_intent(body)
    }
    fn model(&self) -> &str {
        self.0.model()
    }
    fn reasoning_classification(&self) -> Option<ReasoningClassification> {
        self.0.reasoning_classification()
    }
    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.0.system_prompt_for_budget()
    }
    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        None
    }
    fn response_cache_scope_id(&self) -> Option<u64> {
        self.0.response_cache_scope_id()
    }
    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.0.build_request(prompt)
    }
    fn cache_namespace(&self) -> String {
        self.0.cache_namespace()
    }
    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        self.0.build_messages_request(messages)
    }
    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.0.build_messages_request_with_tools(messages, tools)
    }
    fn build_messages_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<HttpRequest, ProviderError> {
        self.0.build_messages_request_checked(messages)
    }
    fn build_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<HttpRequest, ProviderError> {
        self.0
            .build_messages_request_with_tools_checked(messages, tools)
    }
    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.0
            .prepare_messages_request_with_tools_checked(messages, tools)
    }
    fn prepare_messages_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.0.prepare_messages_request_checked(messages)
    }
    fn build_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<HttpRequest, ProviderError> {
        self.0.build_compaction_request_checked(messages)
    }
    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.0.prepare_compaction_request_checked(messages)
    }
    fn cache_key(&self, messages: &[ProviderMessage]) -> String {
        self.0.cache_key(messages)
    }
    fn cache_key_with_tools(&self, messages: &[ProviderMessage], tools: &[Value]) -> String {
        self.0.cache_key_with_tools(messages, tools)
    }
    fn cache_key_for_prepared(&self, request: &PreparedProviderRequest) -> String {
        self.0.cache_key_for_prepared(request)
    }
    fn sensitive_values(&self) -> Vec<String> {
        self.0.sensitive_values()
    }
    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.0.parse_event(value)
    }
}

fn codex_adapter(port: u16) -> OpenAiCodexAdapter {
    OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        format!("http://127.0.0.1:{port}"),
        "gpt-6-astra",
        "gauge-token",
        "gauge-account",
    ))
    .expect("codex adapter")
}

fn loop_config() -> AgentLoopConfig {
    AgentLoopConfig {
        max_turns: 8,
        context_window_tokens: 100_000_000,
        context_reserve_tokens: 0,
        context_compaction_enabled: false,
        ..AgentLoopConfig::default()
    }
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn fmt_ms(duration: Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1_000.0)
}

fn main() {
    let mut reps = 7usize;
    let mut sizes_kb = vec![64usize, 256, 1024];
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--reps" => {
                reps = args[i + 1].parse().expect("--reps");
                i += 2;
            }
            "--sizes" => {
                sizes_kb = args[i + 1]
                    .split(',')
                    .map(|v| v.trim().parse().expect("--sizes"))
                    .collect();
                i += 2;
            }
            other => panic!("unknown arg {other}"),
        }
    }

    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (port, last_body) = spawn_server();
    let cwd = std::env::current_dir().expect("cwd");
    let secrets: Vec<String> = (0..4)
        .map(|n| format!("sk-gauge-secret-{:016x}", 0xdead_beef_u64 + n as u64))
        .collect();

    println!("# gauge-prepare-cost");
    println!("# arms: loop(codex real) | loop-noenv(P1 off) | direct(P2 on, {} secrets) | direct-nosens", secrets.len());
    println!("size_kb,arm,body_bytes,median_ms,min_ms,max_ms");

    for &kb in &sizes_kb {
        let target = kb * 1024;
        // History identical across arms; secrets embedded so replace runs.
        let history = make_history(target, &secrets);

        // --- arm: loop (real codex adapter, envelope bound Some) ---
        let adapter = codex_adapter(port);
        let client =
            HttpProviderClient::with_shared_transport(adapter, ProviderTimeouts::uniform(Duration::from_secs(10)))
                .expect("client");
        let mut samples = Vec::with_capacity(reps);
        for _ in 0..reps {
            let mut runtime = Runtime::new();
            let started = Instant::now();
            tokio_rt
                .block_on(runtime.run_agent_loop_with_messages(
                    &client,
                    &history,
                    OperatingMode::Auto,
                    Path::new(&cwd),
                    1,
                    loop_config(),
                ))
                .expect("loop turn");
            samples.push(started.elapsed());
        }
        let body_len = last_body.lock().expect("body").len();
        let min = *samples.iter().min().expect("min");
        let max = *samples.iter().max().expect("max");
        let med = median(&mut samples);
        println!("{kb},loop,{body_len},{},{},{}", fmt_ms(med), fmt_ms(min), fmt_ms(max));

        // --- arm: loop-noenv (estimate path disabled) ---
        let adapter = NoEnvelope(codex_adapter(port));
        let client =
            HttpProviderClient::with_shared_transport(adapter, ProviderTimeouts::uniform(Duration::from_secs(10)))
                .expect("client");
        let mut samples = Vec::with_capacity(reps);
        for _ in 0..reps {
            let mut runtime = Runtime::new();
            let started = Instant::now();
            tokio_rt
                .block_on(runtime.run_agent_loop_with_messages(
                    &client,
                    &history,
                    OperatingMode::Auto,
                    Path::new(&cwd),
                    1,
                    loop_config(),
                ))
                .expect("loop-noenv turn");
            samples.push(started.elapsed());
        }
        let min = *samples.iter().min().expect("min");
        let max = *samples.iter().max().expect("max");
        let med = median(&mut samples);
        println!("{kb},loop-noenv,{body_len},{},{},{}", fmt_ms(med), fmt_ms(min), fmt_ms(max));

        // --- arm: direct (run_provider_messages_turn, secrets registered) ---
        let adapter = codex_adapter(port);
        let client =
            HttpProviderClient::with_shared_transport(adapter, ProviderTimeouts::uniform(Duration::from_secs(10)))
                .expect("client");
        let mut samples = Vec::with_capacity(reps);
        for _ in 0..reps {
            let mut runtime = Runtime::new();
            for secret in &secrets {
                runtime.register_sensitive_value(secret.clone());
            }
            let started = Instant::now();
            tokio_rt
                .block_on(runtime.run_provider_messages_turn(
                    &client,
                    &history,
                    OperatingMode::Auto,
                    Path::new(&cwd),
                    1,
                ))
                .expect("direct turn");
            samples.push(started.elapsed());
        }
        let min = *samples.iter().min().expect("min");
        let max = *samples.iter().max().expect("max");
        let med = median(&mut samples);
        println!("{kb},direct,{body_len},{},{},{}", fmt_ms(med), fmt_ms(min), fmt_ms(max));

        // --- arm: direct-nosens (redact short-circuits) ---
        let mut samples = Vec::with_capacity(reps);
        for _ in 0..reps {
            let mut runtime = Runtime::new();
            let started = Instant::now();
            tokio_rt
                .block_on(runtime.run_provider_messages_turn(
                    &client,
                    &history,
                    OperatingMode::Auto,
                    Path::new(&cwd),
                    1,
                ))
                .expect("direct-nosens turn");
            samples.push(started.elapsed());
        }
        let min = *samples.iter().min().expect("min");
        let max = *samples.iter().max().expect("max");
        let med = median(&mut samples);
        println!("{kb},direct-nosens,{body_len},{},{},{}", fmt_ms(med), fmt_ms(min), fmt_ms(max));
    }

    // --- P3 micro + prepare-pass proxies on the actual captured body ---
    // serde passes below use the same serde_json machinery on the real request
    // bytes; they bound what each internal pass costs (prepare does ~1 encode
    // for the body + 1 per-item to_writer pass for components + 1 recursive
    // fingerprint walk + chars().count()).
    let body = last_body.lock().expect("body").clone();
    if !body.is_empty() {
        let body_str = String::from_utf8_lossy(&body);
        let iterations = 20u32;
        time_op("chars_count", body.len(), iterations, || {
            std::hint::black_box(body_str.chars().count());
        });
        time_op("len_bytes", body.len(), iterations, || {
            std::hint::black_box(body_str.len());
        });
        time_op("json_parse_body", body.len(), iterations, || {
            std::hint::black_box(serde_json::from_slice::<Value>(&body).expect("parse"));
        });
        let parsed: Value = serde_json::from_slice(&body).expect("parse once");
        time_op("json_encode_body", body.len(), iterations, || {
            std::hint::black_box(serde_json::to_string(&parsed).expect("encode"));
        });
        // provider_request_components equivalent: to_writer per input item.
        if let Some(items) = parsed.get("input").and_then(Value::as_array) {
            time_op("components_items_writer", body.len(), iterations, || {
                for item in items {
                    serde_json::to_writer(std::io::sink(), item).expect("count");
                }
            });
        }
        // Pre-fix proxies: the fixers' uncommitted diff replaced a per-char
        // fold with a per-byte fold in estimate_json_string_chars and added the
        // empty-secrets short-circuit before redact_messages. Reproduce both
        // sides on the real payload so the before/after delta is measured, not
        // assumed.
        time_op("estimate_chars_fold_prefix", body.len(), iterations, || {
            // pre-fix semantics: iterate chars, match per char.
            std::hint::black_box(body_str.chars().fold(2_u64, |total, c| {
                total.saturating_add(match c {
                    '\u{0}'..='\u{1f}' => 6,
                    '"' | '\\' => 2,
                    _ => 1,
                })
            }));
        });
        time_op("estimate_bytes_fold_current", body.len(), iterations, || {
            // current semantics: bytes, continuation bytes cost 0.
            std::hint::black_box(body_str.bytes().fold(2_u64, |total, byte| {
                total.saturating_add(match byte {
                    0x00..=0x1f => 6,
                    b'"' | b'\\' => 2,
                    0x80..=0xbf => 0,
                    _ => 1,
                })
            }));
        });
        // serialized_chars: pre-fix chars().count() vs current bytes filter.
        time_op("serchars_bytes_filter_current", body.len(), iterations, || {
            std::hint::black_box(
                body_str
                    .bytes()
                    .filter(|byte| (byte & 0xC0) != 0x80)
                    .count(),
            );
        });
        // redact_values on one 12KiB field, 4 secrets registered, 1 present:
        // pre-fix folded replace per secret (N scans+allocs); current gates on
        // contains() first.
        let field = format!("{}{}", "t".repeat(11 * 1024), "sk-gauge-secret-00000000deadbeef");
        let secrets4: Vec<String> = (0..4)
            .map(|n| format!("sk-gauge-secret-{:016x}", 0xdead_beef_u64 + n as u64))
            .collect();
        time_op("redact_values_prefix", field.len(), iterations, || {
            let mut out = field.clone();
            for secret in &secrets4 {
                out = out.replace(secret.as_str(), "[REDACTED]");
            }
            std::hint::black_box(out);
        });
        time_op("redact_values_current", field.len(), iterations, || {
            if secrets4.iter().any(|s| field.contains(s.as_str())) {
                let mut out = field.clone();
                for secret in &secrets4 {
                    if out.contains(secret.as_str()) {
                        out = out.replace(secret.as_str(), "[REDACTED]");
                    }
                }
                std::hint::black_box(out);
            } else {
                std::hint::black_box(field.clone());
            }
        });
        // take_redacted_stream_chunk, no secrets: pre-fix allocated a copy of
        // the pending buffer per delta; current mem::take moves it.
        time_op("stream_chunk_prefix_alloc", 4096, iterations, || {
            let mut pending = String::new();
            for _ in 0..16 {
                pending.push_str(&"d".repeat(4096));
                std::hint::black_box(pending.clone());
            }
            std::hint::black_box(pending);
        });
        time_op("stream_chunk_current_take", 4096, iterations, || {
            let mut pending = String::new();
            for _ in 0..16 {
                pending.push_str(&"d".repeat(4096));
                std::hint::black_box(std::mem::take(&mut pending));
            }
        });
    }

    // Pre-fix P2 floor: run_provider_messages_with_tools used to call
    // redact_messages unconditionally — i.e. at least a full history clone
    // plus a to_owned per field on every provider request, secrets or not.
    // The floor is measurable: history.to_vec() on the fabricated history.
    {
        let history = make_history(1024 * 1024, &[]);
        let iterations = 20u32;
        let started = Instant::now();
        let mut bytes = 0usize;
        for _ in 0..iterations {
            let cloned = std::hint::black_box(history.to_vec());
            bytes += cloned.iter().map(|m| m.content.len()).sum::<usize>();
        }
        let elapsed = started.elapsed();
        println!(
            "micro,history_to_vec_1MB,{},{},({} iters)",
            bytes / iterations as usize,
            fmt_ms(elapsed / iterations),
            iterations
        );
    }

    // --- Session resume-load: fabricate a durable session with the REAL
    // JsonlRepo serializer, then measure the exact work `--resume` performs
    // before any provider call (headless.rs:432 preflight -> records ->
    // provider_messages_from_records).
    {
        let dir = std::env::temp_dir().join("gauge-session-bench");
        std::fs::create_dir_all(&dir).expect("bench dir");
        for &kb in &sizes_kb {
            let path = dir.join(format!("sess-{kb}k.jsonl"));
            if path.exists() {
                std::fs::remove_file(&path).expect("remove old session");
            }
            let lock_side = dir.join(format!("sess-{kb}k.jsonl.lock"));
            if lock_side.exists() {
                std::fs::remove_file(&lock_side).ok();
            }
            let file_bytes = fabricate_session(&path, kb * 1024);
            let iterations = 20u32;
            time_op("session_preflight", file_bytes, iterations, || {
                std::hint::black_box(
                    slim_core::session::preflight_session(&path).expect("preflight"),
                );
            });
            let preflight =
                slim_core::session::preflight_session(&path).expect("preflight once");
            let records = &preflight.records;
            time_op("session_rebuild_messages", file_bytes, iterations, || {
                std::hint::black_box(
                    provider_messages_from_records(records.iter()).expect("rebuild"),
                );
            });
            time_op("session_open_no_repair", file_bytes, iterations, || {
                std::hint::black_box(JsonlRepo::open_no_repair(&path).expect("open"));
            });
        }
    }
}

/// A durable transcript shaped like a real accumulated session: per turn a
/// user entry, operation start + provider attempt, assistant entry carrying a
/// tool call, and the tool result that closes it, plus a usage record.
fn fabricate_session(path: &Path, target_bytes: usize) -> usize {
    let header = DurableSessionHeader::new(
        "gauge-session",
        "2026-09-16T00:00:00Z",
        path.parent().unwrap_or(Path::new(".")).to_string_lossy().to_string(),
        None,
        None,
    );
    let mut repo = JsonlRepo::create(path, header).expect("create session");
    let mut seq = 0u64;
    let mut turn = 0usize;
    let mut written = 0usize;
    while written < target_bytes {
        let op = format!("op-{turn}");
        let attempt = format!("att-{turn}");
        let input_id = format!("in-{turn}");
        let call_id = format!("call_{turn}");
        let batch = vec![
            DurableRecord::Entry {
                seq,
                entry: DurableEntry {
                    entry_id: input_id.clone(),
                    role: DurableEntryRole::User,
                    content: format!("turn {turn} {}", "u".repeat(2048)),
                    parent_entry_id: (turn > 0).then(|| format!("tool-{prev}", prev = turn - 1)),
                    operation_id: op.clone(),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    content_blocks: Vec::new(),
                },
            },
            DurableRecord::Operation {
                seq: seq + 1,
                operation: DurableOperation {
                    operation_id: op.clone(),
                    kind: DurableOperationKind::Started {
                        input_entry_id: input_id,
                    },
                },
            },
            DurableRecord::Operation {
                seq: seq + 2,
                operation: DurableOperation {
                    operation_id: op.clone(),
                    kind: DurableOperationKind::ProviderAttemptStarted {
                        attempt_id: attempt.clone(),
                        ordinal: 1,
                    },
                },
            },
            DurableRecord::Entry {
                seq: seq + 3,
                entry: DurableEntry {
                    entry_id: format!("as-{turn}"),
                    role: DurableEntryRole::Assistant,
                    content: format!("ack {turn} {}", "a".repeat(512)),
                    parent_entry_id: Some(format!("in-{turn}")),
                    operation_id: op.clone(),
                    tool_call_id: None,
                    tool_calls: vec![ProviderToolCall {
                        id: call_id.clone(),
                        name: "shell".into(),
                        arguments: json!({ "command": format!("run --step {turn}") }).to_string(),
                    }],
                    content_blocks: Vec::new(),
                },
            },
            DurableRecord::Entry {
                seq: seq + 4,
                entry: DurableEntry {
                    entry_id: format!("tool-{turn}"),
                    role: DurableEntryRole::Tool,
                    content: format!("tool output {turn} {}", "t".repeat(11 * 1024)),
                    parent_entry_id: Some(format!("as-{turn}")),
                    operation_id: op.clone(),
                    tool_call_id: Some(call_id),
                    tool_calls: Vec::new(),
                    content_blocks: Vec::new(),
                },
            },
            DurableRecord::Usage {
                seq: seq + 5,
                usage: DurableUsage {
                    operation_id: op,
                    attempt_id: attempt,
                    input_tokens: Some(4096),
                    output_tokens: Some(64),
                },
            },
        ];
        repo.append_batch(batch).expect("append");
        seq += 6;
        turn += 1;
        written = std::fs::metadata(path).map(|m| m.len() as usize).unwrap_or(0);
    }
    written
}

fn time_op(name: &str, bytes: usize, iterations: u32, mut op: impl FnMut()) {
    let started = Instant::now();
    for _ in 0..iterations {
        op();
    }
    let elapsed = started.elapsed();
    println!(
        "micro,{name},{bytes},{} per op ({iterations} iters)",
        fmt_ms(elapsed / iterations),
    );
}
