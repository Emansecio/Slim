#[path = "../../../tests/support/budget_finalization.rs"]
mod budget_finalization;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use slim_cli::{
    load_local_images, render_provider_jsonl, render_provider_text, run_cli, run_provider_headless,
    run_provider_headless_with_options, run_provider_headless_with_session,
    run_provider_headless_with_session_and_options, ExitCode, ProviderRequest, ProviderRunOptions,
};
use slim_core::provider::ProviderKind;
use slim_core::session::{
    preflight_session, DurableEntryRole, DurableOperationKind, DurableOutcome, DurableRecord,
};
use slim_core::OperatingMode;
use std::sync::{Mutex, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn spawn_one_turn_fixture(
    events: Vec<serde_json::Value>,
    finalize: bool,
    attempts: usize,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for _ in 0..attempts {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture accept timed out");
                        thread::yield_now();
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            let mut request = [0_u8; 16 * 1024];
            let _ = stream.read(&mut request).expect("request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            for event in &events {
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("event");
            }
            stream.write_all(b"data: [DONE]\n\n").expect("done");
            drop(stream);
        }
        if finalize {
            budget_finalization::reject_budget_finalization(&listener);
        }
    });
    (format!("http://{address}"), server)
}

fn tool_event(path: &str, id: &str) -> serde_json::Value {
    json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "function": {"name": "read", "arguments": json!({"path": path, "max_lines": 2}).to_string()}
        }]}}]
    })
}

fn finish_tool_event() -> serde_json::Value {
    json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
}

#[test]
fn interrupted_stream_preserves_partial_text_and_separate_failure() {
    // A truncated stream is recoverable: the runtime retries it up to
    // MAX_PROVIDER_RECOVERIES, so the fixture serves the same partial stream.
    let (endpoint, server) = spawn_one_turn_fixture(
        vec![
            json!({"choices":[{"delta":{"content":"PARTIAL_USEFUL_EVIDENCE_42 fixture-secret "}}]}),
        ],
        false,
        3,
    );
    let result = run_provider_headless(ProviderRequest {
        prompt: "local fixture".into(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "fixture-model".into(),
        api_key: "fixture-secret".into(),
        account_id: None,
        timeout: Duration::from_secs(2),
    })
    .unwrap();
    server.join().unwrap();
    assert_eq!(result.code, ExitCode::Provider);
    assert_eq!(result.stop, "provider_error");
    assert!(result.text.contains("PARTIAL_USEFUL_EVIDENCE_42"));
    assert!(result
        .stop_message
        .as_ref()
        .unwrap()
        .contains("stream ended"));
    for rendered in [
        render_provider_text(&result),
        render_provider_jsonl(&result).unwrap(),
    ] {
        assert!(rendered.contains("PARTIAL_USEFUL_EVIDENCE_42"));
        assert!(rendered.contains("stream ended"));
        assert!(!rendered.contains("fixture-secret"));
    }
}

fn spawn_image_fixture(kind: ProviderKind, image_count: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture accept timed out");
                    thread::yield_now();
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        let mut request = [0_u8; 32 * 1024];
        let size = stream.read(&mut request).expect("request");
        let request = String::from_utf8_lossy(&request[..size]);
        let body = request.split("\r\n\r\n").nth(1).expect("body");
        let body: serde_json::Value = serde_json::from_str(body).expect("json body");
        // OpenAI-compatible: messages[0] is the native Slim system prompt and
        // the user turn follows. Anthropic carries cacheable system blocks in
        // the top-level `system` field, so its first message stays the user turn.
        match kind {
            ProviderKind::OpenAiCompatible => {
                assert_eq!(body["messages"][0]["role"], "system");
                assert!(body["system"].as_str().is_none());
            }
            ProviderKind::Anthropic => {
                assert_eq!(body["messages"][0]["role"], "user");
                assert_eq!(
                    body["system"][0]["text"],
                    slim_core::provider::NATIVE_SYSTEM_PROMPT
                );
                assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
                assert_eq!(body["cache_control"]["type"], "ephemeral");
            }
            ProviderKind::OpenAiCodex
            | ProviderKind::OpenCodeGo
            | ProviderKind::OpenCodeZen
            | ProviderKind::ClinePass
            | ProviderKind::CommandCode
            | ProviderKind::Xai => {
                unreachable!("image fixture does not use this provider")
            }
        }
        let user_index = usize::from(kind == ProviderKind::OpenAiCompatible);
        let content = &body["messages"][user_index]["content"];
        assert_eq!(
            content.as_array().expect("multimodal content").len(),
            image_count + 1
        );
        match kind {
            ProviderKind::OpenAiCompatible => {
                assert_eq!(content[1]["type"], "image_url");
                assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAEC");
            }
            ProviderKind::Anthropic => {
                assert_eq!(content[1]["type"], "image");
                assert_eq!(content[1]["source"]["media_type"], "image/png");
                assert_eq!(content[1]["source"]["data"], "AAEC");
            }
            ProviderKind::OpenAiCodex
            | ProviderKind::OpenCodeGo
            | ProviderKind::OpenCodeZen
            | ProviderKind::ClinePass
            | ProviderKind::CommandCode
            | ProviderKind::Xai => {
                unreachable!("image fixture does not use this provider")
            }
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let events = match kind {
            ProviderKind::OpenAiCompatible => b"data: {\"choices\":[{\"delta\":{\"content\":\"image ok\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".as_slice(),
            ProviderKind::Anthropic => b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"image ok\"}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: [DONE]\n\n".as_slice(),
            ProviderKind::OpenAiCodex | ProviderKind::OpenCodeGo | ProviderKind::OpenCodeZen | ProviderKind::ClinePass | ProviderKind::CommandCode | ProviderKind::Xai => {
                unreachable!("image fixture does not use this provider")
            }
        };
        stream.write_all(events).expect("events");
    });
    (format!("http://{address}"), server)
}

#[test]
fn configured_headless_provider_uses_local_sse_without_leaking_key() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        // Read until the declared Content-Length is satisfied: the native
        // system prompt makes the request body larger than a single read.
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 64 * 1024];
        let expected = loop {
            let size = stream.read(&mut chunk).expect("request");
            if size == 0 {
                panic!("fixture: connection closed before full request");
            }
            raw.extend_from_slice(&chunk[..size]);
            let text = String::from_utf8_lossy(&raw);
            if let Some(length) = text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .or_else(|| line.strip_prefix("content-length:"))
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
            {
                let header_end = text.find("\r\n\r\n").expect("header terminator") + 4;
                if raw.len() >= header_end + length {
                    break (header_end, length);
                }
            }
        };
        let body: serde_json::Value =
            serde_json::from_slice(&raw[expected.0..expected.0 + expected.1]).expect("body");
        assert_eq!(body["max_tokens"], 1234);
        let request = String::from_utf8_lossy(&raw).into_owned();
        assert!(request.contains("cline-pass/fixture-model"));
        assert!(request.contains("Bearer fixture-secret"));
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(
                br#"data: {"choices":[{"delta":{"content":"hello from fixture"}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#,
            )
            .expect("events");
    });

    let session_path = std::env::temp_dir().join(format!(
        "slim-provider-session-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let result = run_provider_headless_with_session_and_options(
        ProviderRequest {
            prompt: "hello".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::ClinePass,
            endpoint: format!("http://{address}"),
            model: "cline-pass/fixture-model".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        &session_path,
        ProviderRunOptions::default().with_max_output_tokens(1234),
    )
    .expect("provider");
    server.join().expect("server");

    assert_eq!(result.code, ExitCode::Success);
    assert_eq!(result.text, "hello from fixture");
    assert_eq!(render_provider_text(&result), "hello from fixture\n");
    let jsonl = render_provider_jsonl(&result).expect("jsonl");
    assert!(jsonl.contains("hello from fixture"));
    assert!(!jsonl.contains("fixture-secret"));
    let recovered = preflight_session(&session_path).expect("session");
    assert!(
        recovered
            .records
            .iter()
            .any(|record| matches!(record, DurableRecord::Entry { entry, .. }
            if entry.role == DurableEntryRole::Assistant && entry.content == "hello from fixture"))
    );
    assert!(!std::fs::read_to_string(&session_path)
        .expect("session text")
        .contains("fixture-secret"));
    let _ = std::fs::remove_file(session_path);
}

#[test]
fn invalid_session_destination_fails_before_provider_request() {
    let root = std::env::temp_dir().join(format!(
        "slim-session-preflight-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let session_path = root.join("session.jsonl");
    std::fs::create_dir_all(&session_path).expect("directory at session path");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let error = run_provider_headless_with_session(
        ProviderRequest {
            prompt: "hello".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(1),
        },
        &session_path,
    )
    .expect_err("invalid session destination");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "provider was contacted"
    );
    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { message }
            if message.starts_with("session:")
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_cli_astra_flags_reach_http_and_override_saved_speed() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use std::io::BufRead;
    let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let variables = ["SLIM_API_KEY", "SLIM_EFFORT", "SLIM_CONFIG_FILE"];
    let previous = variables.map(std::env::var_os);
    let config = std::env::temp_dir().join(format!("slim-astra-cli-{}.toml", std::process::id()));
    std::fs::write(&config, "codex_fast = true\neffort = \"low\"\n").expect("config");
    std::env::set_var("SLIM_CONFIG_FILE", &config);
    std::env::set_var("SLIM_EFFORT", "medium");
    let claims = URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}}"#);
    std::env::set_var("SLIM_API_KEY", format!("fixture.{claims}.fixture"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let server = thread::spawn(move || {
        for fast in [true, false] {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            assert!(line.starts_with("POST /codex/responses "));
            let mut length = None;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).expect("header") > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().expect("length"));
                }
            }
            let mut bytes = vec![0; length.filter(|n| *n <= 1024 * 1024).expect("bounded body")];
            reader.read_exact(&mut bytes).expect("body");
            let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
            assert_eq!(body["model"], "gpt-6-astra");
            assert_eq!(body["reasoning"]["effort"], "max");
            assert_eq!(body.get("service_tier"), fast.then_some(&json!("priority")));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ASTRA_OK\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n").expect("response");
        }
    });
    let results = ["--fast", "--normal"].map(|speed| {
        run_cli(
            [
                "--headless",
                "--provider",
                "openai-codex",
                "--model",
                "astra",
                "--effort",
                "max",
                speed,
                "--endpoint",
                &endpoint,
                "--prompt",
                "hello",
            ],
            "",
        )
    });
    server.join().expect("server");
    let rejected = run_cli(
        [
            "--headless",
            "--provider",
            "anthropic",
            "--fast",
            "--prompt",
            "hello",
        ],
        "",
    );
    for (name, value) in variables.into_iter().zip(previous) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    let _ = std::fs::remove_file(config);
    for result in results {
        assert_eq!(result.code, ExitCode::Success, "{}", result.stderr);
        assert!(result.stdout.contains("ASTRA_OK"));
    }
    assert_eq!(rejected.code, ExitCode::InputRequired);
    assert!(rejected
        .stderr
        .contains("require the openai-codex provider"));
}

#[test]
fn terminal_provider_failure_preserves_failed_request_ledger_and_session() {
    let session_path = std::env::temp_dir().join(format!(
        "slim-provider-failure-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let result = run_provider_headless_with_session(
        ProviderRequest {
            prompt: "offline failure fixture".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: "http://127.0.0.1:1".into(),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: Duration::from_millis(50),
        },
        &session_path,
    )
    .expect("terminal provider failure is a headless result");

    assert_eq!(result.code, ExitCode::Provider);
    assert_eq!(result.stop, "provider_error");
    assert!(!result.usage_complete);
    assert!(result.usage.requests[0].failed);
    let jsonl = render_provider_jsonl(&result).expect("provider JSONL");
    assert!(jsonl.contains("\"failed\":true"));
    let recovered = preflight_session(&session_path).expect("failed session");
    assert!(recovered.records.iter().any(|record| matches!(record,
        DurableRecord::Operation { operation, .. }
        if matches!(operation.kind, DurableOperationKind::ProviderAttemptFinished { outcome: DurableOutcome::Failed, .. })
    )));
    let _ = std::fs::remove_file(session_path);
}

#[test]
fn opencode_go_headless_uses_documented_chat_route() {
    let (endpoint, server) = spawn_one_turn_fixture(
        vec![
            json!({"choices": [{"delta": {"content": "hello from go"}}]}),
            json!({"usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ],
        false,
        1,
    );

    let result = run_provider_headless_with_options(
        ProviderRequest {
            prompt: "hello".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenCodeGo,
            endpoint,
            model: "deepseek-v4-flash".into(),
            api_key: "go-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        ProviderRunOptions::default(),
    )
    .expect("OpenCode Go provider");
    server.join().expect("server");

    assert_eq!(result.text, "hello from go");
}

#[test]
fn provider_plan_and_empty_input_do_not_make_network_requests() {
    for (prompt, mode, code, text) in [
        (
            "inspect",
            OperatingMode::Plan,
            ExitCode::ApprovalRequired,
            "approval_required",
        ),
        (
            "",
            OperatingMode::Auto,
            ExitCode::InputRequired,
            "input_required",
        ),
    ] {
        let result = run_provider_headless(ProviderRequest {
            prompt: prompt.into(),
            mode,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: "http://127.0.0.1:1".into(),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: Duration::from_millis(20),
        })
        .expect("short-circuit");
        assert_eq!(result.code, code);
        assert_eq!(result.text, text);
    }
}

#[test]
fn configured_headless_executes_a_read_tool_call_in_auto_mode() {
    use std::io::BufRead;
    let root = std::env::temp_dir().join(format!("slim-provider-tool-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("mkdir");
    let path = root.join("fixture.txt");
    std::fs::write(&path, "tool content\n").expect("fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let path_arg = "fixture.txt".to_owned();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut line = String::new();
            let mut length = None;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; length.filter(|n| *n <= 1024 * 1024).unwrap()];
            reader.read_exact(&mut body).unwrap();
            if turn == 1 {
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert!(request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool"
                        && message["tool_call_id"] == "single-read-call"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("tool content"))));
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            if turn == 0 {
                let event = format!(
                    "data: {}\n\n",
                    json!({
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "single-read-call",
                                    "function": {
                                        "name": "read",
                                        "arguments": json!({
                                            "path": path_arg,
                                            "max_lines": 10
                                        })
                                        .to_string()
                                    }
                                }]
                            }
                        }]
                    })
                );
                stream.write_all(event.as_bytes()).expect("tool event");
                stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n")
                    .expect("done");
            } else {
                stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"Read complete\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                    .expect("done");
            }
        }
    });

    let result = run_provider_headless_with_options(
        ProviderRequest {
            prompt: "read fixture".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        ProviderRunOptions::default()
            .with_workspace_root(&root)
            .with_artifact_root(root.join("artifacts")),
    )
    .expect("provider");
    server.join().expect("server");

    assert_eq!(result.code, ExitCode::Success);
    assert_eq!(result.text, "Read complete");
    assert_eq!(result.tool_summary_lines.len(), 1);
    assert!(result.tool_summary_lines[0].starts_with("✓ read · "));
    assert!(!result.tool_summary_lines[0].contains(path.to_string_lossy().as_ref()));
    assert!(!result.tool_summary_lines[0].contains("tool content"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn configured_headless_runs_a_second_provider_turn_after_tool_result() {
    let root = std::env::temp_dir().join(format!("slim-provider-loop-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("mkdir");
    let path = root.join("fixture.txt");
    std::fs::write(&path, "loop content fixture-secret\n").expect("fixture");
    let session_path = root.join("session.jsonl");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let path_arg = "fixture.txt".to_owned();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; 16 * 1024];
            let size = stream.read(&mut request).expect("request");
            let request = String::from_utf8_lossy(&request[..size]);
            if turn == 1 {
                assert!(request.contains("\"role\":\"tool\""));
                assert!(request.contains("loop content [REDACTED]"));
                let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
                assert!(!body.contains("fixture-secret"));
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            let payload = if turn == 0 {
                json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "multi-turn-read-call",
                                "function": {
                                    "name": "read",
                                    "arguments": json!({
                                        "path": path_arg,
                                        "max_lines": 10
                                    })
                                    .to_string()
                                }
                            }]
                        }
                    }]
                })
            } else {
                json!({"choices":[{"delta":{"content":"done after tool"}}]})
            };
            let first = format!("data: {payload}\n\n");
            stream.write_all(first.as_bytes()).expect("event");
            let finish = if turn == 0 {
                br#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

"#
                .to_vec()
            } else {
                br#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}

"#
                .to_vec()
            };
            stream.write_all(&finish).expect("finish");
            stream.write_all(b"data: [DONE]\n\n").expect("done");
        }
    });

    let result = run_provider_headless_with_session_and_options(
        ProviderRequest {
            prompt: "inspect and summarize".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        &session_path,
        ProviderRunOptions::default()
            .with_workspace_root(&root)
            .with_artifact_root(root.join("artifacts")),
    )
    .expect("provider");
    server.join().expect("server");

    assert_eq!(result.code, ExitCode::Success);
    assert_eq!(result.text, "done after tool");
    assert_eq!(result.stop_reason.as_deref(), Some("stop"));
    assert_eq!(result.stop, "provider_completed");
    assert!(!result.text.contains("fixture-secret"));
    let raw_session = std::fs::read_to_string(&session_path).expect("session");
    assert!(!raw_session.contains("fixture-secret"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bounded_stops_expose_stable_status_and_nonzero_codes() {
    let root = std::env::temp_dir().join(format!("slim-bounded-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    for (options, expected_code, expected_stop, events) in [
        (
            ProviderRunOptions::default().with_max_turns(1),
            ExitCode::Blocked,
            "turn_limit",
            vec![tool_event("missing.txt", "turn-1"), finish_tool_event()],
        ),
        (
            ProviderRunOptions::default()
                .with_max_tool_calls(0)
                .with_max_read_tool_calls(0),
            ExitCode::Tool,
            "tool_limit",
            vec![tool_event("missing.txt", "tool-1"), finish_tool_event()],
        ),
    ] {
        let (endpoint, server) = spawn_one_turn_fixture(events, true, 1);
        let result = run_provider_headless_with_options(
            ProviderRequest {
                prompt: "bounded".into(),
                mode: OperatingMode::Auto,
                kind: ProviderKind::OpenAiCompatible,
                endpoint,
                model: "fixture-model".into(),
                api_key: "fixture-secret".into(),
                account_id: None,
                timeout: Duration::from_secs(2),
            },
            options
                .with_workspace_root(&root)
                .with_artifact_root(root.join("artifacts")),
        )
        .expect("provider");
        server.join().expect("server");
        assert_eq!(result.code, expected_code);
        assert_eq!(result.stop, expected_stop);
        let jsonl = render_provider_jsonl(&result).expect("jsonl");
        assert!(jsonl.contains(&format!("\"stop\":\"{expected_stop}\"")));
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn repeated_failed_tool_stop_is_blocked_and_anti_loop_is_reported() {
    let root = std::env::temp_dir().join(format!("slim-antiloop-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        for turn in 0..2 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture accept timed out");
                        thread::yield_now();
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            let mut request = [0_u8; 16 * 1024];
            let _ = stream.read(&mut request).expect("request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            let event = tool_event("missing.txt", &format!("loop-{turn}"));
            stream
                .write_all(
                    format!(
                        "data: {event}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        finish_tool_event()
                    )
                    .as_bytes(),
                )
                .expect("events");
        }
    });
    let result = run_provider_headless_with_options(
        ProviderRequest {
            prompt: "repeat failed read".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        ProviderRunOptions::default()
            .with_workspace_root(&root)
            .with_artifact_root(root.join("artifacts")),
    )
    .expect("provider");
    server.join().expect("server");
    assert_eq!(result.code, ExitCode::Blocked);
    assert_eq!(result.stop, "repeated_failed_tool");
    assert!(result.text.contains("tool read:"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn local_images_are_repeatable_and_encoded_for_both_provider_shapes() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let root = std::env::temp_dir().join(format!("slim-images-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let image = root.join("fixture.png");
    std::fs::write(&image, [0_u8, 1, 2]).expect("image");
    let paths = vec![image.to_string_lossy().to_string()];
    let blocks = load_local_images(&paths).expect("image blocks");
    assert_eq!(blocks.len(), 1);

    for kind in [ProviderKind::OpenAiCompatible, ProviderKind::Anthropic] {
        let (endpoint, server) = spawn_image_fixture(kind, 1);
        let result = run_provider_headless_with_options(
            ProviderRequest {
                prompt: "inspect image".into(),
                mode: OperatingMode::Auto,
                kind,
                endpoint,
                model: "image-fixture".into(),
                api_key: "image-secret".into(),
                account_id: None,
                timeout: Duration::from_secs(2),
            },
            ProviderRunOptions::default()
                .with_content_blocks(blocks.clone())
                .with_workspace_root(&root)
                .with_artifact_root(root.join("artifacts")),
        )
        .expect("provider");
        server.join().expect("server");
        assert_eq!(result.code, ExitCode::Success);
        assert_eq!(result.text, "image ok");
    }

    let previous_key = std::env::var_os("SLIM_API_KEY");
    std::env::set_var("SLIM_API_KEY", "image-cli-secret");
    let (endpoint, server) = spawn_image_fixture(ProviderKind::OpenAiCompatible, 2);
    let output = run_cli(
        [
            "--provider".to_owned(),
            "openai-compatible".to_owned(),
            "--endpoint".to_owned(),
            endpoint,
            "--model".to_owned(),
            "image-fixture".to_owned(),
            "--image".to_owned(),
            image.to_string_lossy().to_string(),
            "--image".to_owned(),
            image.to_string_lossy().to_string(),
            "--prompt".to_owned(),
            "inspect repeatable image".to_owned(),
        ],
        "",
    );
    server.join().expect("server");
    if let Some(value) = previous_key {
        std::env::set_var("SLIM_API_KEY", value);
    } else {
        std::env::remove_var("SLIM_API_KEY");
    }
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(output.stdout, "image ok\n");
    let _ = std::fs::remove_dir_all(root);
}
