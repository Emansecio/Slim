use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use serde_json::json;
use slim_cli::{run_provider_tui_turn, ProviderRequest, ProviderRunOptions};
use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;
use slim_tui::api::UiEvent;

#[test]
fn tui_projection_redacts_the_configured_key_from_user_prompt() {
    let events = run_provider_tui_turn(
        ProviderRequest {
            prompt: "do not show tui-secret".into(),
            mode: OperatingMode::Plan,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: "http://127.0.0.1:1".into(),
            model: "unused".into(),
            api_key: "tui-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(1),
        },
        ProviderRunOptions::default(),
    )
    .expect("plan projection");
    assert_eq!(
        events.first(),
        Some(&UiEvent::UserMessageAdded {
            text: "do not show [REDACTED]".into()
        })
    );
}

#[test]
fn tui_turn_projects_prompt_tool_stream_final_text_and_usage() {
    let root = std::env::temp_dir().join(format!("slim-tui-runtime-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("workspace");
    let source = root.join("fixture.txt");
    std::fs::write(&source, "tui tool fixture\n").expect("fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let source_arg = source.to_string_lossy().to_string();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; 16 * 1024];
            let size = stream.read(&mut request).expect("request");
            let request = String::from_utf8_lossy(&request[..size]);
            if turn == 1 {
                assert!(request.contains("tui tool fixture"));
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("headers");
            if turn == 0 {
                let event = json!({
                    "choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": "tui-call-1",
                        "function": {
                            "name": "read",
                            "arguments": json!({"path": source_arg, "max_lines": 10}).to_string()
                        }
                    }]}}]
                });
                stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .expect("tool event");
                stream
                    .write_all(b"data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n")
                    .expect("tool completion");
            } else {
                stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"final from tui\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":4}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                    .expect("final completion");
            }
        }
    });

    let events = run_provider_tui_turn(
        ProviderRequest {
            prompt: "inspect fixture".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: format!("http://{address}"),
            model: "tui-fixture".into(),
            api_key: "tui-secret".into(),
            account_id: None,
            timeout: Duration::from_secs(2),
        },
        ProviderRunOptions::default()
            .with_workspace_root(&root)
            .with_artifact_root(root.join("artifacts")),
    )
    .expect("tui turn");
    server.join().expect("server");

    assert_eq!(
        events.first(),
        Some(&UiEvent::UserMessageAdded {
            text: "inspect fixture".into()
        })
    );
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolStarted { name } if name == "read"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolProgress { preview, .. } if preview.contains("tui tool fixture")
    )));
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::AssistantDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        "final from tui"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::Usage {
            input_tokens: 5,
            output_tokens: 4
        }
    )));

    let _ = std::fs::remove_dir_all(root);
}
