use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use slim_cli::{run_provider_headless_with_options, ProviderRequest, ProviderRunOptions};
use slim_core::provider::{ProviderContentBlock, ProviderError, ProviderKind};
use slim_core::OperatingMode;

fn request(endpoint: String, model: &str) -> ProviderRequest {
    ProviderRequest {
        prompt: "hello".into(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenCodeGo,
        endpoint,
        model: model.into(),
        api_key: "fixture-opencode-secret".into(),
        account_id: None,
        timeout: Duration::from_secs(2),
    }
}

fn fixture(
    expected_path: &'static str,
    events: Vec<serde_json::Value>,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = vec![0_u8; 64 * 1024];
        let size = stream.read(&mut request).expect("request");
        let request = String::from_utf8_lossy(&request[..size]);
        assert!(request.starts_with(&format!("POST {expected_path} ")));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-opencode-secret"));
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        for event in events {
            writeln!(stream, "data: {event}\n").expect("event");
        }
        stream.flush().expect("flush");
    });
    (format!("http://{address}/v1"), server)
}

#[test]
fn chat_completions_model_runs_through_logical_provider() {
    let (endpoint, server) = fixture(
        "/v1/chat/completions",
        vec![
            serde_json::json!({"choices":[{"delta":{"content":"chat-ok"}}]}),
            serde_json::json!({
                "choices":[{"delta":{},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1}
            }),
        ],
    );

    let result = run_provider_headless_with_options(
        request(endpoint, "deepseek-v4-flash"),
        ProviderRunOptions::default(),
    )
    .expect("headless result");
    server.join().expect("server");

    assert_eq!(result.text, "chat-ok");
    assert_eq!(result.provider, ProviderKind::OpenCodeGo);
}

#[test]
fn responses_model_runs_without_codex_subscription_headers() {
    let (endpoint, server) = fixture(
        "/v1/responses",
        vec![
            serde_json::json!({"type":"response.output_text.delta","delta":"responses-ok"}),
            serde_json::json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":3,"output_tokens":1}}
            }),
        ],
    );

    let result = run_provider_headless_with_options(
        request(endpoint, "gpt-5.6-luna"),
        ProviderRunOptions::default(),
    )
    .expect("headless result");
    server.join().expect("server");

    assert_eq!(result.text, "responses-ok");
}

#[test]
fn anthropic_messages_model_runs_with_bearer_auth() {
    let (endpoint, server) = fixture(
        "/v1/messages",
        vec![
            serde_json::json!({
                "type":"message_start",
                "message":{"usage":{"input_tokens":3}}
            }),
            serde_json::json!({
                "type":"content_block_delta",
                "index":0,
                "delta":{"type":"text_delta","text":"messages-ok"}
            }),
            serde_json::json!({
                "type":"message_delta",
                "delta":{"stop_reason":"end_turn"},
                "usage":{"output_tokens":1}
            }),
        ],
    );

    let result = run_provider_headless_with_options(
        request(endpoint, "minimax-m3"),
        ProviderRunOptions::default(),
    )
    .expect("headless result");
    server.join().expect("server");

    assert_eq!(result.text, "messages-ok");
}

#[test]
fn text_only_model_rejects_images_before_network() {
    let error = run_provider_headless_with_options(
        request("http://127.0.0.1:1/v1".into(), "deepseek-v4-flash"),
        ProviderRunOptions::default()
            .with_content_blocks(vec![ProviderContentBlock::image("image/png", "aGVsbG8=")]),
    )
    .expect_err("text-only model must reject image");

    assert!(matches!(
        error,
        ProviderError::InvalidResponse { message }
            if message == "OpenCode Go model deepseek-v4-flash does not accept images"
    ));
}
