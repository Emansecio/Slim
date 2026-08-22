use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::json;
use slim_cli::{spawn_tui_runtime, ProviderRequest, ProviderRunOptions};
use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;
use slim_tui::api::{ModelAlias, ReasoningEffort, UiCommand, UiEvent};

fn request(endpoint: String) -> ProviderRequest {
    ProviderRequest {
        prompt: String::new(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "tui-bridge-fixture".into(),
        api_key: "tui-bridge-secret".into(),
        account_id: None,
        timeout: Duration::from_secs(2),
    }
}

#[test]
fn bridge_streams_first_delta_before_provider_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let delta = json!({"choices":[{"delta":{"content":"live"}}]});
        stream
            .write_all(format!("data: {delta}\n\n").as_bytes())
            .expect("delta");
        stream.flush().expect("flush");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release completion");
        stream
            .write_all(
                b"data: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("completion");
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default(),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("hello".into()))
        .expect("prompt");

    let mut saw_live_delta = false;
    while !saw_live_delta {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(1))
            .expect("live event before completion");
        saw_live_delta = event
            == UiEvent::AssistantDelta {
                text: "live".into(),
            };
    }
    release_tx.send(()).expect("release server");

    let mut saw_usage = false;
    while !saw_usage {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(1))
            .expect("completion event");
        saw_usage = matches!(
            event,
            UiEvent::Usage {
                input_tokens: 2,
                output_tokens: 1
            }
        );
    }
    let mut completed = false;
    while !completed {
        completed = channels
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("run completion")
            == UiEvent::RunCompleted;
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn model_alias_updates_next_codex_request_without_restarting_tui() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let size = stream.read(&mut request).expect("request");
        let request = String::from_utf8_lossy(&request[..size]);
        assert!(request.contains("gpt-5.6-terra"));
        assert!(request.contains("\"effort\":\"max\""));
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("response");
    });
    let mut model_request = request(format!("http://{address}"));
    model_request.kind = ProviderKind::OpenAiCodex;
    model_request.model = "gpt-5.6-sol".into();
    model_request.account_id = Some("account-1".into());
    let (runtime, channels) =
        spawn_tui_runtime(model_request, ProviderRunOptions::default()).expect("bridge");
    channels
        .commands
        .send(UiCommand::SetModel {
            model: ModelAlias::Terra,
            effort: ReasoningEffort::Max,
        })
        .expect("model");
    loop {
        if channels
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("model event")
            == (UiEvent::ModelChanged {
                model: "gpt-5.6-terra".into(),
            })
        {
            break;
        }
    }
    channels
        .commands
        .send(UiCommand::SendPrompt("hello".into()))
        .expect("prompt");
    loop {
        if channels
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("completion")
            == UiEvent::RunCompleted
        {
            break;
        }
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn bounded_agent_stop_is_not_reported_as_completed() {
    let (runtime, channels) = spawn_tui_runtime(
        request("http://127.0.0.1:1".into()),
        ProviderRunOptions::default().with_max_turns(0),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("bounded".into()))
        .expect("prompt");

    let mut stopped = false;
    while !stopped {
        let event = channels
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("bounded stop");
        assert_ne!(event, UiEvent::RunCompleted);
        stopped = matches!(event, UiEvent::RunStopped { .. });
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
}

#[test]
fn cancel_run_kills_active_shell_before_late_workspace_mutation() {
    let root = std::env::temp_dir().join(format!("slim-tui-shell-cancel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let marker = root.join("late.txt");
    let command = format!(
        "Start-Sleep -Seconds 30; Set-Content -LiteralPath '{}' -Value late",
        marker.display()
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request);
        let arguments = serde_json::json!({"command": command, "timeout_ms": 60_000}).to_string();
        let event = json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "shell-1",
                "function": {"name": "shell", "arguments": arguments}
            }]}}]
        });
        let body = format!(
            "data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("response");
    });
    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default().with_workspace_root(&root),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("run shell".into()))
        .expect("prompt");
    loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("tool start");
        if event == (UiEvent::ToolStarted { name: "shell".into() }) {
            break;
        }
    }
    channels
        .commands
        .send(UiCommand::CancelRun)
        .expect("cancel");
    loop {
        if channels
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel event")
            == UiEvent::RunCancelled
        {
            break;
        }
    }
    thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists());
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cancel_run_closes_provider_connection_and_returns_idle() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n")
            .expect("partial delta");
        stream.flush().expect("flush");
        accepted_tx.send(()).expect("accepted");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut byte = [0_u8; 1];
        assert_eq!(stream.read(&mut byte).expect("connection close"), 0);
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default(),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("cancel me".into()))
        .expect("prompt");
    accepted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("request accepted");
    channels
        .commands
        .send(UiCommand::CancelRun)
        .expect("cancel");

    let mut cancelled = false;
    while !cancelled {
        let event = channels
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel event");
        cancelled = event == UiEvent::RunCancelled;
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    assert_eq!(
        channels
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown event"),
        UiEvent::Shutdown
    );
    drop(runtime);
    server.join().expect("server");
}
