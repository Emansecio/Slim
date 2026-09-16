use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for suffix in 0..u64::MAX {
            let path = std::env::temp_dir().join(format!(
                "slim-continuation-{}-{stamp}-{suffix}",
                std::process::id(),
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create exclusive test workspace: {error}"),
            }
        }
        panic!("test workspace suffix exhausted");
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "request ended before its body");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .expect("content-length");
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}

fn tool(id: &str, name: &str, arguments: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}})
}

fn spawn_turn(responses: Vec<Option<Value>>) -> (String, thread::JoinHandle<Vec<Value>>) {
    let responses = responses
        .into_iter()
        .map(|response| {
            if let Some(mut call) = response {
                call["index"] = json!(0);
                json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":"tool_calls"}]})
            } else {
                json!({"choices":[{"delta":{"content":"completed turn"},"finish_reason":"stop"}]})
            }
        })
        .collect();
    spawn_events(responses, |_| {})
}

fn spawn_events(
    responses: Vec<Value>,
    mut before_reply: impl FnMut(usize) + Send + 'static,
) -> (String, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (index, chunk) in responses.into_iter().enumerate() {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing provider request");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            requests.push(read_request(&mut stream));
            before_reply(index);
            let payload = format!(
                "data: {chunk}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}})
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).unwrap();
        }
        requests
    });
    (endpoint, handle)
}

#[test]
fn consecutive_compactions_restore_the_latest_checkpoint_and_pending_tasks() {
    use slim_core::context::{CompactionHandle, CompactionPolicy};
    use slim_core::provider::{ProviderKind, ProviderMessage};
    use slim_core::session::{preflight_session, DurableRecord};

    let root = Workspace::new();
    let session = root.0.join("session.jsonl");
    let handle = CompactionHandle::new(CompactionPolicy {
        background: false,
        ..CompactionPolicy::default()
    });
    handle.request_manual("").unwrap();
    let summary = |label| {
        json!({"choices":[{"delta":{"content":format!(
        "## Goal\nOriginal task\n## Constraints\nOffline\n## Progress\n{label}\n## Blocked\nNone\n## Decisions\nPreserve evidence\n## Next steps\nVerify changes\n## Critical context\nFixture"
    )},"finish_reason":"stop"}]})
    };
    let mut call = tool(
        "track",
        "todo",
        json!({"todos":[{"title":"verify changes"}]}),
    );
    call["index"] = json!(0);
    let next_compaction = handle.clone();
    let (endpoint, server) = spawn_events(
        vec![
            summary("first-checkpoint"),
            json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":"tool_calls"}]}),
            summary("second-checkpoint"),
            json!({"choices":[{"delta":{"content":"Tasks recorded"},"finish_reason":"stop"}]}),
        ],
        move |index| {
            if index == 1 {
                next_compaction.request_manual("").unwrap();
            }
        },
    );
    let request = |endpoint| slim_cli::ProviderRequest {
        prompt: "continue".into(),
        mode: slim_core::OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "fixture-model".into(),
        api_key: "fixture-key".into(),
        account_id: None,
        timeout: Duration::from_secs(5),
    };
    let options = || {
        slim_cli::ProviderRunOptions::default()
            .with_workspace_root(&root.0)
            .with_context_window_tokens(32_768)
            .with_max_output_tokens(1024)
    };
    let result = slim_cli::run_provider_headless_with_session_and_options(
        request(endpoint),
        &session,
        options()
            .with_compaction_handle(handle.clone())
            .with_history(vec![
                ProviderMessage::user("Original task"),
                ProviderMessage::assistant("old evidence ".repeat(4000), Vec::new()),
            ]),
    )
    .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(result.code, slim_cli::ExitCode::Success);
    assert!(result
        .stop_message
        .as_deref()
        .unwrap()
        .contains("verify changes"));
    let rendered = slim_cli::render_provider_text(&result);
    assert!(rendered.contains("[Run ended]"));
    assert!(!rendered.contains("[Run stopped]"));
    let preflight = preflight_session(&session).unwrap();
    let checkpoints: Vec<_> = preflight
        .records
        .iter()
        .filter_map(|record| {
            if let DurableRecord::Compaction { checkpoint, .. } = record {
                Some(checkpoint)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        checkpoints.len(),
        2,
        "every applied checkpoint must be persisted"
    );
    assert_eq!(
        checkpoints[1].previous_checkpoint_id.as_deref(),
        Some(checkpoints[0].checkpoint_id.as_str())
    );
    assert!(
        handle.take_commits().is_empty(),
        "persisted commits must be drained"
    );
    assert!(fs::read_to_string(&session)
        .unwrap()
        .contains("[Run ended]"));

    let (endpoint, server) = spawn_turn(vec![None]);
    let resumed = slim_cli::run_provider_headless_with_resume_and_options(
        request(endpoint),
        &session,
        options(),
    )
    .unwrap();
    assert_eq!(resumed.code, slim_cli::ExitCode::Success);
    assert!(resumed
        .stop_message
        .as_deref()
        .unwrap()
        .contains("verify changes"));
    let restored = server.join().unwrap();
    let messages = restored[0]["messages"].as_array().unwrap();
    assert!(messages.iter().any(
        |message| message["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("[Compacted context]")
                && text.contains("second-checkpoint"))
    ));
    assert!(!messages.iter().any(|message| message["content"]
        .as_str()
        .is_some_and(|text| text.contains("first-checkpoint") || text.contains("old evidence"))));
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["tool_call_id"] == "track")
            .count(),
        1
    );
}

#[test]
fn failed_validation_is_not_reported_as_validated_completion() {
    let root = Workspace::new();
    fs::write(
        root.0.join("Cargo.toml"),
        "[package]\nname = \"validation_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir(root.0.join("src")).unwrap();
    fs::write(
        root.0.join("src/lib.rs"),
        "#[test]\nfn required_check() { assert!(false, \"required check failed\"); }\n",
    )
    .unwrap();
    let session = root.0.join("session.jsonl");
    let (endpoint, server) = spawn_turn(vec![
        Some(tool(
            "green",
            "shell",
            json!({"command":"cargo", "args":["check", "--offline"]}),
        )),
        Some(tool(
            "red",
            "shell",
            json!({"command":"cargo", "args":["test", "--offline"]}),
        )),
        None,
    ]);
    let output = cli_command(&root.0, &session, &endpoint, false)
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    let green = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "green")
        .unwrap();
    assert!(
        green["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("exit 0"),
        "{green}"
    );
    assert!(requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["tool_call_id"] == "red"
            && message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("required check failed")));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["validation_source"].is_null(), "{result}");
    assert_eq!(result["usage"]["validated_completion"], false, "{result}");
    assert!(result["costs"]["cost_per_validated_completion_micros"].is_null());
}

fn cli_command(cwd: &Path, session: &Path, endpoint: &str, resume: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_slim"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SLIM_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(cwd)
        .env("SLIM_CONFIG_FILE", cwd.join("empty.toml"))
        .env("SLIM_API_KEY", "continuation-fixture-secret")
        .args([
            "--headless",
            "--provider",
            "openai-compatible",
            "--model",
            "fixture-model",
            "--endpoint",
            endpoint,
            "--jsonl",
        ])
        .arg(if resume { "--resume" } else { "--session" })
        .arg(session)
        .args([
            "--prompt",
            if resume {
                "make second change"
            } else {
                "make first change"
            },
        ]);
    command
}

fn run_process(cwd: &Path, session: &Path, endpoint: &str, resume: bool) -> std::process::Output {
    cli_command(cwd, session, endpoint, resume)
        .output()
        .unwrap()
}

#[test]
fn new_session_reopens_in_another_process_with_tools_and_fresh_file_guards() {
    let root = Workspace::new();
    let other = Workspace::new();
    fs::write(root.0.join("state.txt"), "initial\n").unwrap();
    let session = root.0.join("session.jsonl");
    let (endpoint, server) = spawn_turn(vec![
        Some(tool("first-read", "read", json!({"path":"state.txt"}))),
        Some(tool(
            "first-write",
            "write",
            json!({"path":"state.txt","content":"first\n"}),
        )),
        Some(tool(
            "first-check",
            "shell",
            json!({"command":"if ((Get-Content state.txt -Raw).Trim() -ne 'first') { throw 'bad state' }; Add-Content run-count.txt first"}),
        )),
        None,
    ]);
    let first = run_process(&root.0, &session, &endpoint, false);
    assert!(
        first.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(server.join().unwrap().len(), 4);
    let header: Value = serde_json::from_str(
        fs::read_to_string(&session)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        header["schema_version"], 2,
        "new coding sessions must be resumable"
    );
    fs::write(root.0.join("state.txt"), "external change\n").unwrap();
    let original_prefix = fs::read(&session).unwrap();
    let (endpoint, server) = spawn_turn(vec![
        Some(tool(
            "stale-write",
            "write",
            json!({"path":"state.txt","content":"must not overwrite\n"}),
        )),
        Some(tool("second-read", "read", json!({"path":"state.txt"}))),
        Some(tool(
            "second-write",
            "write",
            json!({"path":"state.txt","content":"second\n"}),
        )),
        Some(tool(
            "second-check",
            "shell",
            json!({"command":"if ((Get-Content state.txt -Raw).Trim() -ne 'second') { throw 'bad state' }"}),
        )),
        None,
    ]);
    let second = run_process(&other.0, &session, &endpoint, true);
    assert!(
        second.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let requests = server.join().unwrap();
    let messages = requests[0]["messages"].as_array().unwrap();
    for id in ["first-read", "first-write", "first-check"] {
        assert!(
            messages.iter().any(|m| m["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["id"] == id))),
            "missing call {id}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m["role"] == "tool" && m["tool_call_id"] == id),
            "missing result {id}"
        );
    }
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["tool_call_id"] == "stale-write"
            && m["content"].as_str().is_some_and(|s| s.contains("read"))));
    assert!(requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["tool_call_id"] == "second-read"
            && m["content"]
                .as_str()
                .is_some_and(|s| s.contains("external change"))));
    assert_eq!(
        fs::read_to_string(root.0.join("state.txt")).unwrap().trim(),
        "second"
    );
    assert_eq!(
        fs::read_to_string(root.0.join("run-count.txt"))
            .unwrap()
            .lines()
            .count(),
        1,
        "completed shell must not replay on reopen"
    );
    assert!(!other.0.join("state.txt").exists());
    let persisted = fs::read(&session).unwrap();
    assert!(persisted.starts_with(&original_prefix));
    assert!(!String::from_utf8_lossy(&persisted).contains("continuation-fixture-secret"));

    // Reopen the same completed coding session through the TUI runtime bridge.
    let (endpoint, server) = spawn_turn(vec![
        Some(tool("tui-read", "read", json!({"path":"state.txt"}))),
        Some(tool(
            "tui-write",
            "write",
            json!({"path":"state.txt","content":"from tui\n"}),
        )),
        None,
    ]);
    let request = slim_cli::ProviderRequest {
        prompt: String::new(),
        mode: slim_core::OperatingMode::Auto,
        kind: slim_core::provider::ProviderKind::OpenAiCompatible,
        endpoint,
        model: "fixture-model".into(),
        api_key: "continuation-fixture-secret".into(),
        account_id: None,
        timeout: Duration::from_secs(5),
    };
    let (runtime, channels) = slim_cli::spawn_tui_runtime_with_resume(
        request,
        &session,
        slim_cli::ProviderRunOptions::default().with_workspace_root(&root.0),
    )
    .unwrap();
    let restored = channels
        .events_data
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(matches!(
        restored,
        slim_tui::api::UiEvent::SessionRestored { .. }
    ));
    let mut app = slim_tui::app::AppState::new();
    let effects = slim_tui::reducer::reduce(
        &mut app,
        slim_tui::reducer::Action::UiEventReceived(restored),
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, slim_tui::reducer::Effect::Send(_))));
    let historical = app.blocks().iter().filter(|block| matches!(block.kind(), slim_tui::block::BlockKind::Tool(tool) if tool.historical)).collect::<Vec<_>>();
    assert_eq!(historical.len(), 7);
    assert!(historical
        .iter()
        .all(|block| block.fold == slim_tui::block::FoldState::Collapsed));
    let shell = historical.iter().find(|block| matches!(block.kind(), slim_tui::block::BlockKind::Tool(tool) if tool.call_id.0.ends_with(":first-check"))).unwrap();
    let shell_id = shell.id.clone();
    let frame = slim_tui::render::render(&app, 140, 100).lines.join("\n");
    assert!(frame.contains("shell · histórico"), "{frame}");
    assert_eq!(
        frame.matches("completed turn").count(),
        2,
        "both final answers must remain visible alongside the restored tools: {frame}"
    );
    assert!(!frame.contains("Add-Content"));
    let effects =
        slim_tui::reducer::reduce(&mut app, slim_tui::reducer::Action::ToggleBlock(shell_id));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, slim_tui::reducer::Effect::Send(_))));
    let frame = slim_tui::render::render(&app, 140, 100).lines.join("\n");
    assert!(
        frame.contains("Arguments:")
            && frame.contains("Add-Content")
            && frame.contains("Result (saved):"),
        "{frame}"
    );
    assert_eq!(
        fs::read(&session).unwrap(),
        persisted,
        "restoring and inspecting history must not append or replay work"
    );
    assert_eq!(
        fs::read_to_string(root.0.join("run-count.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    channels
        .commands
        .send(slim_tui::api::UiCommand::SendPrompt(
            "continue in tui".into(),
        ))
        .unwrap();
    loop {
        if channels
            .events_data
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            == (slim_tui::api::UiEvent::RunCompleted { run_id: 1 })
        {
            break;
        }
    }
    channels
        .commands
        .send(slim_tui::api::UiCommand::Shutdown)
        .unwrap();
    drop(runtime);
    let requests = server.join().unwrap();
    assert!(requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["tool_call_id"] == "second-check"));
    assert_eq!(
        fs::read_to_string(root.0.join("state.txt")).unwrap(),
        "from tui\n"
    );
    let report = slim_core::session::preflight_session(&session).unwrap();
    assert_eq!(report.summary.pending_count(), 0);
    assert!(fs::read(&session).unwrap().starts_with(&persisted));

    let (endpoint, server) = spawn_turn(vec![
        Some(tool(
            "readonly-write",
            "write",
            json!({"path":"forbidden.txt","content":"no"}),
        )),
        None,
    ]);
    let readonly = cli_command(&root.0, &session, &endpoint, true)
        .arg("--read-only")
        .output()
        .unwrap();
    assert!(
        readonly.status.success(),
        "{}",
        String::from_utf8_lossy(&readonly.stderr)
    );
    let requests = server.join().unwrap();
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["tool_call_id"] == "readonly-write"));
    assert!(
        !root.0.join("forbidden.txt").exists(),
        "resume must honor the current read-only mode"
    );
}

struct RunningCli(Child);

impl Drop for RunningCli {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn interrupted_process_after_side_effect_cannot_resume_or_replay_it() {
    let root = Workspace::new();
    let session = root.0.join("session.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            let request = read_request(&mut stream);
            if turn == 0 {
                let mut call = tool(
                    "side-effect",
                    "shell",
                    json!({"command":"Add-Content crash-marker.txt completed; Write-Output 'durable-receipt-42'"}),
                );
                call["index"] = json!(0);
                let chunk = json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":"tool_calls"}]});
                let payload = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).unwrap();
            } else {
                assert!(request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["tool_call_id"] == "side-effect"
                        && message["content"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("durable-receipt-42")));
                ready_tx.send(()).unwrap();
                let mut byte = [0];
                let _ = stream.read(&mut byte);
            }
        }
    });
    let mut child = RunningCli(
        cli_command(&root.0, &session, &endpoint, false)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let live_log = fs::read_to_string(&session).unwrap();
    assert!(
        live_log.contains("durable-receipt-42"),
        "completed output must be durable before the next provider request"
    );
    let live_records = slim_core::session::preflight_session(&session).unwrap();
    assert_eq!(
        live_records
            .records
            .iter()
            .filter(|record| matches!(record,
        slim_core::session::DurableRecord::Entry { entry, .. }
        if entry.role == slim_core::session::DurableEntryRole::Tool))
            .count(),
        1
    );
    assert_eq!(
        fs::read_to_string(root.0.join("crash-marker.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    server.join().unwrap();
    let prefix = fs::read(&session).unwrap();
    let preflight = slim_core::session::preflight_session(&session).unwrap();
    assert_eq!(preflight.summary.pending_count(), 1);
    let resumed = run_process(&root.0, &session, "http://127.0.0.1:1", true);
    assert!(!resumed.status.success());
    assert!(String::from_utf8_lossy(&resumed.stderr).contains("pending"));
    assert_eq!(fs::read(&session).unwrap(), prefix);
    let recovered = slim_cli::run_cli(
        [
            "--headless",
            "--recover",
            session.to_str().unwrap(),
            "--abandon-pending",
        ],
        "",
    );
    assert_eq!(
        recovered.code,
        slim_cli::ExitCode::Success,
        "{}",
        recovered.stderr
    );
    assert!(recovered.stdout.contains("effects remain unverified"));
    let recovered_bytes = fs::read(&session).unwrap();
    assert!(recovered_bytes.starts_with(&prefix));
    let report = slim_core::session::preflight_session(&session).unwrap();
    assert_eq!(report.summary.pending_count(), 0);
    assert_eq!(report.summary.terminal_count(), 1);
    let (endpoint, server) = spawn_turn(vec![None]);
    let resumed = run_process(&root.0, &session, &endpoint, true);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let requests = server.join().unwrap();
    assert_eq!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["tool_call_id"] == "side-effect"
                && message["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("durable-receipt-42"))
            .count(),
        1
    );
    assert!(requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["content"]
            .as_str()
            .is_some_and(|text| text.contains("[Recovery decision]")
                && text.contains("effects remain unverified"))));
    assert_eq!(
        fs::read_to_string(root.0.join("crash-marker.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[cfg(windows)]
#[test]
fn interrupted_batch_recovers_known_results_and_marks_missing_results_unknown() {
    let root = Workspace::new();
    let session = root.0.join("session.jsonl");
    let effect_barrier = TcpListener::bind("127.0.0.1:0").unwrap();
    effect_barrier.set_nonblocking(true).unwrap();
    let port = effect_barrier.local_addr().unwrap().port();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_request(&mut stream);
        let command = format!("Add-Content in-flight-count.txt once; $c = [Net.Sockets.TcpClient]::new('127.0.0.1', {port}); [void]$c.GetStream().ReadByte(); $c.Dispose()");
        let mut calls = vec![
            tool(
                "finished",
                "write",
                json!({"path":"done.txt","content":"once"}),
            ),
            tool(
                "in-flight",
                "shell",
                json!({"command":command,"timeout_ms":15000}),
            ),
            tool(
                "not-started",
                "write",
                json!({"path":"never.txt","content":"unexpected"}),
            ),
        ];
        for (index, call) in calls.iter_mut().enumerate() {
            call["index"] = json!(index);
        }
        let chunk =
            json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]});
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    });
    let mut child = RunningCli(
        cli_command(&root.0, &session, &endpoint, false)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let held_effect = loop {
        match effect_barrier.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "shell did not signal its effect");
                thread::yield_now();
            }
            Err(error) => panic!("effect barrier: {error}"),
        }
    };
    let report = slim_core::session::preflight_session(&session).unwrap();
    assert!(report.records.iter().any(|record| matches!(record,
        slim_core::session::DurableRecord::Entry { entry, .. }
        if entry.tool_call_id.as_deref() == Some("finished") && entry.content.starts_with("written") && entry.content.contains("done.txt"))));
    assert_eq!(fs::read_to_string(root.0.join("done.txt")).unwrap(), "once");
    assert!(!root.0.join("never.txt").exists());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    drop(held_effect);
    server.join().unwrap();
    let prefix = fs::read(&session).unwrap();
    let blocked = run_process(&root.0, &session, "http://127.0.0.1:1", true);
    assert!(!blocked.status.success());
    assert_eq!(fs::read(&session).unwrap(), prefix);
    let recovery = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args([
            "--headless",
            "--recover",
            session.to_str().unwrap(),
            "--abandon-pending",
        ])
        .output()
        .unwrap();
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(fs::read(&session).unwrap().starts_with(&prefix));
    let (endpoint, server) = spawn_turn(vec![None]);
    let resumed = run_process(&root.0, &session, &endpoint, true);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let requests = server.join().unwrap();
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["tool_call_id"] == "finished")
            .count(),
        1
    );
    assert!(messages.iter().any(|m| {
        m["tool_call_id"] == "finished"
            && m["content"].as_str().is_some_and(|content| {
                content.starts_with("written") && content.contains("done.txt")
            })
    }));
    for id in ["in-flight", "not-started"] {
        let result = messages.iter().find(|m| m["tool_call_id"] == id).unwrap();
        assert!(result["content"]
            .as_str()
            .unwrap()
            .contains("effects are unknown"));
    }
    assert_eq!(
        fs::read_to_string(root.0.join("in-flight-count.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!root.0.join("never.txt").exists());
}
