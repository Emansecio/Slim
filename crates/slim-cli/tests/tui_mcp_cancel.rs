//! End-to-end coverage for cancelling an admitted MCP call through the TUI
//! bridge, then reopening its durable session without replaying that call.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use slim_cli::{spawn_tui_runtime_with_resume, ProviderRequest, ProviderRunOptions};
use slim_core::provider::ProviderKind;
use slim_core::session::{
    preflight_session, DurableEntry, DurableEntryRole, DurableOperation, DurableOperationKind,
    DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo,
};
use slim_core::OperatingMode;
use slim_tui::api::{
    PromptAdmission, PromptGeneration, PromptId, PromptOrigin, UiCommand, UiEvent,
};

fn unique_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "slim-tui-mcp-cancel-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

fn provider_request(endpoint: String) -> ProviderRequest {
    ProviderRequest {
        prompt: String::new(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "tui-mcp-cancel-fixture".into(),
        api_key: "fixture-key".into(),
        account_id: None,
        timeout: Duration::from_secs(10),
    }
}

fn admission(sequence: u64) -> PromptAdmission {
    PromptAdmission {
        id: PromptId(sequence),
        generation: PromptGeneration(sequence),
        origin: PromptOrigin::Direct,
    }
}

fn seed_session(path: &Path) {
    let mut repo = JsonlRepo::create(
        path,
        DurableSessionHeader::new(
            "tui-mcp-cancel",
            "now",
            path.parent()
                .expect("workspace")
                .to_str()
                .expect("unicode path"),
            None,
            None,
        ),
    )
    .expect("create durable session");
    repo.append(DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "seed-input".into(),
            role: DurableEntryRole::User,
            content: "seed".into(),
            parent_entry_id: None,
            operation_id: "seed-op".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("seed entry");
    repo.append(DurableRecord::Operation {
        seq: 1,
        operation: DurableOperation {
            operation_id: "seed-op".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "seed-input".into(),
            },
        },
    })
    .expect("seed operation");
    repo.append(DurableRecord::Operation {
        seq: 2,
        operation: DurableOperation {
            operation_id: "seed-op".into(),
            kind: DurableOperationKind::Finished {
                outcome: slim_core::session::DurableOutcome::Success,
            },
        },
    })
    .expect("seed terminal");
}

fn read_http_json(stream: &mut TcpStream) -> Option<Value> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 64 * 1024 {
            return None;
        }
    }
    let headers = String::from_utf8_lossy(&headers);
    let length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_json(stream: &mut TcpStream, status: &str, body: &Value) {
    let body = body.to_string();
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(body.as_bytes()));
}

struct McpFixture {
    url: String,
    calls: Arc<Mutex<Vec<String>>>,
    admitted: mpsc::Receiver<String>,
    release_old_response: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl McpFixture {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.url.strip_prefix("http://").expect("fixture URL"));
        if let Some(worker) = self.worker.take() {
            worker.join().expect("MCP fixture worker");
        }
    }
}

fn spawn_mcp_fixture() -> McpFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("MCP bind");
    listener.set_nonblocking(true).expect("MCP nonblocking");
    let url = format!("http://{}", listener.local_addr().expect("MCP addr"));
    let (admitted_tx, admitted) = mpsc::channel();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed_calls = Arc::clone(&calls);
    let release_old_response = Arc::new((Mutex::new(false), Condvar::new()));
    let response_gate = Arc::clone(&release_old_response);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !stopping.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "MCP fixture did not stop");
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("MCP accept: {error}"),
            };
            // Windows accepted sockets inherit the listener's nonblocking mode;
            // an early WouldBlock read would silently drop the request.
            stream
                .set_nonblocking(false)
                .expect("blocking MCP connection");
            let Some(request) = read_http_json(&mut stream) else {
                continue;
            };
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            match request.get("method").and_then(Value::as_str) {
                Some("initialize") => write_json(
                    &mut stream,
                    "200 OK",
                    &json!({
                        "jsonrpc":"2.0", "id":id,
                        "result":{
                            "protocolVersion":"2025-11-25",
                            "capabilities":{},
                            "serverInfo":{"name":"fixture","version":"1"}
                        }
                    }),
                ),
                Some("notifications/initialized") => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Some("tools/list") => write_json(
                    &mut stream,
                    "200 OK",
                    &json!({
                        "jsonrpc":"2.0", "id":id,
                        "result":{"tools":[{
                            "name":"hold",
                            "description":"Wait for a controlled response",
                            "inputSchema":{"type":"object","properties":{"marker":{"type":"string"}},"required":["marker"]}
                        }]}
                    }),
                ),
                Some("tools/call") => {
                    let marker = request
                        .pointer("/params/arguments/marker")
                        .and_then(Value::as_str)
                        .unwrap_or("missing")
                        .to_owned();
                    observed_calls
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(marker.clone());
                    let _ = admitted_tx.send(marker.clone());
                    if marker == "old" {
                        let (gate, changed) = &*response_gate;
                        let mut released =
                            gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        while !*released {
                            released = changed
                                .wait(released)
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                        }
                    }
                    write_json(
                        &mut stream,
                        "200 OK",
                        &json!({
                            "jsonrpc":"2.0", "id":id,
                            "result":{"content":[{"type":"text","text":format!("fixture result {marker}")}],"isError":false}
                        }),
                    );
                }
                _ => write_json(
                    &mut stream,
                    "200 OK",
                    &json!({"jsonrpc":"2.0","id":id,"result":{}}),
                ),
            }
        }
    });
    McpFixture {
        url,
        calls,
        admitted,
        release_old_response,
        stop,
        worker: Some(worker),
    }
}

fn provider_tool_call(marker: &str, call_id: &str) -> String {
    let delta = json!({
        "choices":[{"delta":{"tool_calls":[{
            "index":0,
            "id":call_id,
            "type":"function",
            "function":{"name":"mcp","arguments":json!({"server":"fixture","tool":"hold","arguments":{"marker":marker}}).to_string()}
        }]}}]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn provider_response(body: &Value) -> String {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .expect("provider messages array");
    let last_user_message = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .expect("provider last user message");
    match last_user_message {
        prompt if prompt.starts_with("cancel the old call") => {
            provider_tool_call("old", "old-call")
        }
        prompt
            if prompt.starts_with("make a new call")
                && messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("tool")
                    && message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|content| content.contains("fixture result new"))
                }) =>
        {
            "data: {\"choices\":[{\"delta\":{\"content\":\"new connection ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()
        }
        prompt if prompt.starts_with("make a new call") => {
            provider_tool_call("new", "new-call")
        }
        unexpected => panic!("unexpected provider prompt in fixture: {unexpected:?}"),
    }
}

fn spawn_provider_fixture() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("provider bind");
    listener
        .set_nonblocking(true)
        .expect("provider nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("provider addr"));
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        for _ in 0..3 {
            let mut stream = loop {
                assert!(Instant::now() < deadline, "provider fixture timed out");
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("blocking provider connection");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .expect("provider read timeout");
                        break stream;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("provider accept: {error}"),
                }
            };
            let mut headers = Vec::new();
            let mut byte = [0_u8; 1];
            loop {
                if stream.read(&mut byte).expect("provider headers") == 0 {
                    panic!("provider closed before request headers");
                }
                headers.push(byte[0]);
                if headers.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&headers);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("provider content length");
            let mut request_body = vec![0_u8; length];
            stream
                .read_exact(&mut request_body)
                .expect("provider request body");
            let request_body: Value =
                serde_json::from_slice(&request_body).expect("provider request JSON");
            let response = provider_response(&request_body);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream
                .write_all(header.as_bytes())
                .and_then(|_| stream.write_all(response.as_bytes()))
                .expect("provider response");
        }
    });
    (endpoint, worker)
}

struct ConfigEnv(Option<std::ffi::OsString>);

impl ConfigEnv {
    fn set(path: &Path) -> Self {
        let old = std::env::var_os("SLIM_CONFIG_FILE");
        std::env::set_var("SLIM_CONFIG_FILE", path);
        Self(old)
    }
}

impl Drop for ConfigEnv {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            std::env::set_var("SLIM_CONFIG_FILE", value);
        } else {
            std::env::remove_var("SLIM_CONFIG_FILE");
        }
    }
}

fn start_runtime(
    endpoint: &str,
    session: &Path,
    workspace: &Path,
) -> (slim_cli::TuiRuntimeHandle, slim_tui::api::UiChannels) {
    spawn_tui_runtime_with_resume(
        provider_request(endpoint.to_owned()),
        session,
        ProviderRunOptions::default()
            .with_workspace_root(workspace)
            .with_context_window_tokens(32_000),
    )
    .expect("start resumable TUI runtime")
}

#[test]
fn cancelling_blocked_mcp_call_is_uncertain_and_not_replayed_after_resume() {
    let root = unique_root("restart");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let session = workspace.join("session.jsonl");
    seed_session(&session);

    let mcp = spawn_mcp_fixture();
    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            "[mcp.servers.fixture]\nurl = \"{}\"\ntimeout_ms = 30000\n",
            mcp.url
        ),
    )
    .expect("MCP config");
    let _config_env = ConfigEnv::set(&config);
    let (provider_endpoint, provider_worker) = spawn_provider_fixture();

    let (runtime, channels) = start_runtime(&provider_endpoint, &session, &workspace);
    let first_admission = admission(1);
    channels
        .commands
        .send(UiCommand::PreparePrompt {
            prompt: "cancel the old call".into(),
            admission: first_admission,
        })
        .expect("submit first prompt");
    assert_eq!(
        mcp.admitted
            .recv_timeout(Duration::from_secs(10))
            .expect("old tools/call reached fixture"),
        "old"
    );

    let start_deadline = Instant::now() + Duration::from_secs(10);
    let mut first_run_id = None;
    let mut first_tool = None;
    while Instant::now() < start_deadline && (first_run_id.is_none() || first_tool.is_none()) {
        let wait = start_deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        match channels.events_data.recv_timeout(wait) {
            Ok(UiEvent::PromptRunStarted {
                admission, run_id, ..
            }) if admission == first_admission => first_run_id = Some(run_id),
            Ok(UiEvent::ToolStarted {
                batch_id,
                call_id,
                name,
                ..
            }) if name == "mcp" => first_tool = Some((batch_id, call_id)),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("TUI data lane disconnected"),
        }
    }
    let first_run_id = first_run_id.expect("first prompt run started before cancellation");
    let (tool_batch_id, tool_call_id) = first_tool.expect("MCP ToolStarted before cancellation");
    let expected_call_id = format!("run-{first_run_id}:old-call");
    assert_eq!(tool_call_id.0.as_ref(), expected_call_id);

    let cancel_started = Instant::now();
    channels
        .commands
        .send(UiCommand::CancelRun)
        .expect("cancel active run");
    let deadline = cancel_started + Duration::from_secs(3);
    let mut phase = 0;
    while Instant::now() < deadline && phase < 3 {
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        match channels.events.recv_timeout(wait) {
            Ok(UiEvent::CancellationRequested { run_id })
            | Ok(UiEvent::CancellationStarted { run_id }) => assert_eq!(run_id, first_run_id),
            Ok(UiEvent::RequestCompleted { .. }) if phase == 0 => {}
            Ok(UiEvent::ToolOutput {
                batch_id,
                call_id,
                name,
                output,
                ..
            }) => {
                assert_eq!(phase, 0, "ToolOutput must be the first tool terminal event");
                assert_eq!(batch_id, tool_batch_id);
                assert_eq!(call_id, tool_call_id);
                assert_eq!(call_id.0.as_ref(), expected_call_id);
                assert_eq!(name, "mcp");
                assert!(
                    output.contains("outcome is uncertain"),
                    "unexpected MCP ToolOutput: {output}"
                );
                phase = 1;
            }
            Ok(UiEvent::ToolEnded {
                batch_id,
                call_id,
                name,
                success,
                ..
            }) => {
                assert_eq!(phase, 1, "ToolEnded must follow ToolOutput");
                assert_eq!(batch_id, tool_batch_id);
                assert_eq!(call_id, tool_call_id);
                assert_eq!(call_id.0.as_ref(), expected_call_id);
                assert_eq!(name, "mcp");
                assert!(!success, "cancelled MCP call must finish unsuccessful");
                phase = 2;
            }
            Ok(UiEvent::PromptRunCancelled { admission, run_id }) => {
                assert_eq!(phase, 2, "run terminal must follow ToolEnded");
                assert_eq!(admission, first_admission);
                assert_eq!(run_id, first_run_id);
                phase = 3;
            }
            Ok(UiEvent::WorkspaceChanged { .. }) => {}
            Ok(UiEvent::TodoChanged { .. }) => {}
            Ok(UiEvent::ModeChanged { .. }) => {}
            Ok(UiEvent::AuthStateChanged { .. }) => {}
            Ok(UiEvent::ModelChanged { .. }) => {}
            Ok(UiEvent::EffortChanged { .. }) => {}
            Ok(UiEvent::CodexSpeedChanged { .. }) => {}
            Ok(UiEvent::AttachmentsChanged { .. }) => {}
            Ok(event) => {
                panic!("unexpected control event during cancelled MCP lifecycle: {event:?}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("TUI control lane disconnected"),
        }
    }
    assert_eq!(
        phase, 3,
        "MCP output, ToolEnded, and run terminal must arrive within 3s"
    );
    assert!(cancel_started.elapsed() < Duration::from_secs(3));

    // The fixture has accepted the remote call, but its response is still
    // blocked. The observed output therefore represents an uncertain result.
    let (gate, changed) = &*mcp.release_old_response;
    *gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    changed.notify_all();
    preflight_session(&session).expect("cancelled session remains resumable");
    assert_eq!(
        mcp.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|marker| marker.as_str() == "old")
            .count(),
        1,
        "cancel must not replay the in-flight call"
    );

    let second_admission = admission(2);
    channels
        .commands
        .send(UiCommand::PreparePrompt {
            prompt: "make a new call".into(),
            admission: second_admission,
        })
        .expect("submit new prompt on the same worker");
    assert_eq!(
        mcp.admitted
            .recv_timeout(Duration::from_secs(10))
            .expect("new call reaches re-opened MCP session"),
        "new"
    );
    let mut completed = false;
    let mut second_run_id = None;
    let mut assistant_text = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !completed {
        match channels.events_data.recv_timeout(Duration::from_millis(50)) {
            Ok(UiEvent::AssistantDelta { text }) => assistant_text.push_str(&text),
            Ok(UiEvent::PromptRunStarted {
                admission, run_id, ..
            }) if admission == second_admission => second_run_id = Some(run_id),
            Ok(UiEvent::PromptRunCompleted { admission, run_id })
                if admission == second_admission =>
            {
                assert_eq!(second_run_id, Some(run_id));
                completed = true;
            }
            Ok(UiEvent::PromptRunFailed {
                admission, message, ..
            }) if admission == second_admission => {
                panic!("resumed run failed: {message}");
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("resumed data lane disconnected"),
        }
        if !completed {
            match channels.events.try_recv() {
                Ok(UiEvent::PromptRunCompleted { admission, run_id })
                    if admission == second_admission =>
                {
                    assert_eq!(second_run_id, Some(run_id));
                    completed = true;
                }
                Ok(UiEvent::PromptRunFailed {
                    admission, message, ..
                }) if admission == second_admission => {
                    panic!("resumed run failed: {message}");
                }
                Ok(_) | Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("resumed control lane disconnected")
                }
            }
        }
    }
    assert!(completed, "new MCP connection did not complete");
    assert!(
        second_run_id.is_some(),
        "new admission had no correlated start"
    );
    assert!(assistant_text.contains("new connection ok"));

    runtime
        .finish()
        .expect("finish same worker after reconnect");
    preflight_session(&session).expect("session remains resumable after reconnect");

    let (resumed, _resumed_channels) = start_runtime(&provider_endpoint, &session, &workspace);
    match mcp.admitted.recv_timeout(Duration::from_millis(250)) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Ok(marker) => panic!("resumed session unexpectedly replayed MCP call {marker}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("MCP fixture disconnected"),
    }
    let resumed_calls = mcp
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        resumed_calls
            .iter()
            .filter(|marker| marker.as_str() == "old")
            .count(),
        1,
        "resuming the session must not replay the cancelled remote call"
    );
    assert_eq!(
        resumed_calls
            .iter()
            .filter(|marker| marker.as_str() == "new")
            .count(),
        1,
        "the same worker must reconnect without repeating the previous call"
    );
    drop(resumed_calls);
    provider_worker.join().expect("provider fixture");
    resumed.finish().expect("finish resumed runtime");

    let calls = mcp
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        calls
            .iter()
            .filter(|marker| marker.as_str() == "old")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|marker| marker.as_str() == "new")
            .count(),
        1
    );
    drop(calls);
    mcp.stop();
    fs::remove_dir_all(root).expect("remove fixture files");
}
