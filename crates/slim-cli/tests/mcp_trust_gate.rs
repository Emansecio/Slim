//! End to end: a project `slim.toml` MCP server must not be reachable by the
//! model until the workspace is trusted. A real headless run against a local
//! provider fixture makes the model call the server through the `mcp` tool
//! and the next request shows what the tool returned.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use slim_cli::{run_provider_headless_with_options, ProviderRequest, ProviderRunOptions};
use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct Env {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl Env {
    fn set(vars: &[(&'static str, &std::path::Path)]) -> Self {
        let previous = vars
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in vars {
            std::env::set_var(name, value);
        }
        Self { previous }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let read = stream.read(&mut chunk).expect("read request");
        data.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if data.len() >= header_end + 4 + length {
                return String::from_utf8_lossy(&data[header_end + 4..]).into_owned();
            }
        }
        assert!(Instant::now() < deadline, "request never completed");
    }
}

/// Serves two turns: the first asks for an `mcp` call, the second finishes.
/// Returns the endpoint and the request bodies (first, second).
fn provider_fixture(mcp_arguments: serde_json::Value) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let worker = thread::spawn(move || {
        let mut bodies = Vec::new();
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            bodies.push(read_request(&mut stream));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            let events = if turn == 0 {
                vec![
                    json!({"choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": "call-mcp-1",
                        "function": {"name": "mcp", "arguments": mcp_arguments.to_string()}
                    }]}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                ]
            } else {
                vec![
                    json!({"choices": [{"delta": {"content": "done"}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                ]
            };
            for event in events {
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("event");
            }
            stream.write_all(b"data: [DONE]\n\n").expect("done");
        }
        bodies
    });
    (format!("http://{address}"), worker)
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "slim-mcp-gate-{label}-{}-{:?}",
        std::process::id(),
        thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("workspace");
    root
}

fn run_with_project_server(label: &str, trust_project: bool) -> String {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = temp_root(label);
    // Hermetic: no real global config and no real trust store.
    let global = root.join("global-slim.toml");
    let trust_store = root.join("store").join("mcp-trust.json");
    let _env = Env::set(&[
        ("SLIM_CONFIG_FILE", &global),
        ("SLIM_MCP_TRUST_FILE", &trust_store),
    ]);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(
        workspace.join("slim.toml"),
        "[mcp.servers.proj]\ncommand = \"definitely-not-a-real-mcp-binary\"\n",
    )
    .expect("project config");

    let (endpoint, worker) = provider_fixture(json!({
        "server": "proj",
        "tool": "ping",
        "arguments": {}
    }));
    let options = ProviderRunOptions::default()
        .with_workspace_root(&workspace)
        .with_context_window_tokens(128_000)
        .with_trust_project(trust_project);
    let result = run_provider_headless_with_options(
        ProviderRequest {
            prompt: "call the project tool".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint,
            model: "fixture-model".into(),
            api_key: "fixture-key".into(),
            account_id: None,
            timeout: Duration::from_secs(10),
        },
        options,
    )
    .expect("run completes");
    assert_eq!(result.text, "done", "{result:?}");
    let bodies = worker.join().expect("fixture");
    let _ = std::fs::remove_dir_all(&root);
    bodies[1].clone()
}

#[test]
fn untrusted_project_server_is_refused_to_the_model_and_never_started() {
    let second_request = run_with_project_server("untrusted", false);
    assert!(
        second_request.contains("which is not trusted"),
        "the model must be told the server is untrusted: {second_request}"
    );
}

#[test]
fn trust_project_flag_lets_the_call_reach_the_server_start() {
    let second_request = run_with_project_server("trusted", true);
    assert!(
        !second_request.contains("which is not trusted"),
        "a trusted run must attempt the connection instead: {second_request}"
    );
    assert!(
        second_request.contains("mcp error"),
        "the (nonexistent) server fails to start, which the model sees: {second_request}"
    );
}
