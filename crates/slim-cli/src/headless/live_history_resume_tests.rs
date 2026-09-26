use super::{
    keep_live_history, run_provider_resume_with_preflight_events, ProviderRequest,
    ProviderRunOptions,
};
use slim_core::provider::ProviderKind;
use slim_core::session::{
    preflight_session, DurableEntry, DurableEntryRole, DurableRecord, DurableRepo,
    DurableSessionHeader, JsonlRepo,
};
use slim_core::{OperatingMode, ProviderMessage};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn temp_session(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "slim-live-history-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("workspace");
    dir.join("session.jsonl")
}

fn create_empty_v2(path: &Path) {
    let cwd = path.parent().expect("parent").to_str().expect("unicode");
    drop(
        JsonlRepo::create(
            path,
            DurableSessionHeader::new("live-history", "now", cwd, None, None),
        )
        .expect("empty v2"),
    );
}

fn create_v2_with_visible_history(path: &Path) {
    let cwd = path.parent().expect("parent").to_str().expect("unicode");
    let mut repo = JsonlRepo::create(
        path,
        DurableSessionHeader::new("visible", "now", cwd, None, None),
    )
    .expect("v2");
    repo.append(DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "user-1".into(),
            role: DurableEntryRole::User,
            content: "seed question".into(),
            parent_entry_id: None,
            operation_id: "seed".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("user");
    repo.append(DurableRecord::Entry {
        seq: 1,
        entry: DurableEntry {
            entry_id: "asst-1".into(),
            role: DurableEntryRole::Assistant,
            content: "seed answer".into(),
            parent_entry_id: Some("user-1".into()),
            operation_id: "seed".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("assistant");
}

fn request(endpoint: String, prompt: &str) -> ProviderRequest {
    ProviderRequest {
        prompt: prompt.into(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "deepseek-v4-flash".into(),
        api_key: "fixture-secret".into(),
        account_id: None,
        timeout: Duration::from_secs(5),
    }
}

fn read_http_body(stream: &mut std::net::TcpStream) -> String {
    stream.set_nonblocking(false).expect("blocking");
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let size = stream.read(&mut chunk).expect("request");
        assert!(size > 0, "request closed before body");
        raw.extend_from_slice(&chunk[..size]);
        let text = String::from_utf8_lossy(&raw);
        let Some(header_end) = text.find("\r\n\r\n").map(|index| index + 4) else {
            continue;
        };
        let Some(content_length) = text.lines().find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
        }) else {
            continue;
        };
        if raw.len() >= header_end + content_length {
            return String::from_utf8_lossy(&raw[header_end..header_end + content_length])
                .into_owned();
        }
    }
}

fn write_sse(stream: &mut std::net::TcpStream, payload: &[u8]) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("headers");
    stream.write_all(payload).expect("events");
}

fn spawn_turns(
    payloads: Vec<&'static [u8]>,
) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let captured = Arc::clone(&bodies);
    let server = thread::spawn(move || {
        for payload in payloads {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture accept timed out");
                        thread::yield_now();
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            let body = read_http_body(&mut stream);
            captured.lock().expect("bodies").push(body);
            write_sse(&mut stream, payload);
        }
    });
    (format!("http://{address}"), bodies, server)
}

#[test]
fn keep_live_history_requires_visible_identity() {
    let durable = vec![
        ProviderMessage::user("a"),
        ProviderMessage::assistant("b", vec![]),
    ];
    assert!(keep_live_history(&durable, &durable));
    assert!(!keep_live_history(&[], &durable));
    assert!(!keep_live_history(
        &[
            ProviderMessage::user("z"),
            ProviderMessage::assistant("b", vec![])
        ],
        &durable
    ));
    assert!(!keep_live_history(&[ProviderMessage::user("a")], &durable));
    let snapshot = format!(
        "a{} (partial):\nfile.txt\n",
        "\n\nWorkspace paths observed before this turn"
    );
    assert!(keep_live_history(
        &[
            ProviderMessage::user(snapshot),
            ProviderMessage::assistant("b", vec![]),
        ],
        &durable
    ));
    let channel = "a\n\nHarness channel: Auto, unattended.".to_string();
    assert!(keep_live_history(
        &[
            ProviderMessage::user(channel),
            ProviderMessage::assistant("b", vec![]),
        ],
        &durable
    ));
}

#[test]
fn mismatched_live_history_is_replaced_by_durable_prefix() {
    let path = temp_session("mismatch");
    create_v2_with_visible_history(&path);
    let (endpoint, bodies, server) = spawn_turns(vec![
        b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
    ]);
    let preflight = preflight_session(&path).expect("preflight");
    let options = ProviderRunOptions::default()
        .with_context_window_tokens(32_000)
        .with_workspace_root(path.parent().expect("parent"))
        .with_history(vec![
            ProviderMessage::user("stale live question"),
            ProviderMessage::assistant("stale live answer", vec![]),
        ]);
    let execution = run_provider_resume_with_preflight_events(
        request(endpoint, "continue"),
        preflight,
        options,
        None,
    )
    .expect("resume");
    server.join().expect("server");
    assert_eq!(execution.result.code, super::ExitCode::Success);
    let body = bodies.lock().expect("bodies")[0].clone();
    assert!(body.contains("seed question"), "{body}");
    assert!(body.contains("seed answer"), "{body}");
    assert!(!body.contains("stale live question"), "{body}");
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn same_process_resume_resends_chat_reasoning() {
    let path = temp_session("thought");
    create_empty_v2(&path);
    let first = b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hold this thought\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"first answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let second = b"data: {\"choices\":[{\"delta\":{\"content\":\"second answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (endpoint, bodies, server) = spawn_turns(vec![first, second]);
    let workspace = path.parent().expect("parent").to_path_buf();
    let first_options = ProviderRunOptions::default()
        .with_context_window_tokens(32_000)
        .with_workspace_root(&workspace)
        .with_reasoning_effort("high");
    let first_execution = run_provider_resume_with_preflight_events(
        request(endpoint.clone(), "first prompt"),
        preflight_session(&path).expect("first preflight"),
        first_options,
        None,
    )
    .expect("first resume");
    assert_eq!(first_execution.result.code, super::ExitCode::Success);
    assert_eq!(first_execution.result.text, "first answer");
    let history = first_execution.history.expect("live history");
    assert!(
        history.iter().any(|message| {
            message.response_cache_scope_id().is_some()
                && message.role == "assistant"
                && message.content == "first answer"
        }),
        "first turn must retain chat reasoning: {history:?}"
    );

    let second_options = ProviderRunOptions::default()
        .with_context_window_tokens(32_000)
        .with_workspace_root(&workspace)
        .with_reasoning_effort("high")
        .with_history(history);
    let second_execution = run_provider_resume_with_preflight_events(
        request(endpoint, "second prompt"),
        first_execution.resume_preflight.expect("updated preflight"),
        second_options,
        None,
    )
    .expect("second resume");
    server.join().expect("server");
    assert_eq!(second_execution.result.code, super::ExitCode::Success);
    assert_eq!(second_execution.result.text, "second answer");
    let captured = bodies.lock().expect("bodies");
    assert_eq!(captured.len(), 2, "{captured:?}");
    assert!(
        captured[1].contains("hold this thought"),
        "second request must resend live thought: {}",
        captured[1]
    );
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn cold_resume_does_not_invent_reasoning() {
    let path = temp_session("cold");
    create_v2_with_visible_history(&path);
    let (endpoint, bodies, server) = spawn_turns(vec![
        b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
    ]);
    let options = ProviderRunOptions::default()
        .with_context_window_tokens(32_000)
        .with_workspace_root(path.parent().expect("parent"))
        .with_reasoning_effort("high");
    let execution = run_provider_resume_with_preflight_events(
        request(endpoint, "continue"),
        preflight_session(&path).expect("preflight"),
        options,
        None,
    )
    .expect("cold resume");
    server.join().expect("server");
    assert_eq!(execution.result.code, super::ExitCode::Success);
    let body = bodies.lock().expect("bodies")[0].clone();
    assert!(body.contains("seed question"), "{body}");
    assert!(!body.contains("hold this thought"), "{body}");
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}
