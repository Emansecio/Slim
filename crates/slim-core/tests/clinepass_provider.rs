use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use slim_core::provider::{
    fetch_clinepass_catalog, is_clinepass_model_id, parse_clinepass_catalog, ClinePassAdapter,
    ProviderAdapter, ProviderKind, CLINEPASS_BASE_URL,
};

#[test]
fn live_cline_pass_slug_is_accepted_even_if_absent_from_bundle() {
    assert!(is_clinepass_model_id("cline-pass/future-open-model"));
    let adapter = ClinePassAdapter::new(
        CLINEPASS_BASE_URL,
        "cline-pass/future-open-model",
        "cp-secret",
        None,
    )
    .expect("live slug")
    .with_system_prompt("skill-marker");
    assert_eq!(adapter.kind(), ProviderKind::ClinePass);
    assert_eq!(adapter.model(), "cline-pass/future-open-model");
    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");
    assert!(request.url.contains("/api/v1/chat/completions"));
    assert_eq!(body["messages"][0]["content"], "skill-marker");
}

#[test]
fn non_cline_pass_ids_are_rejected() {
    assert!(!is_clinepass_model_id("deepseek/deepseek-v4-flash"));
    assert!(ClinePassAdapter::new(CLINEPASS_BASE_URL, "luna", "k", None).is_err());
}

#[test]
fn parse_catalog_keeps_only_cline_pass_slugs() {
    let body = br#"{
        "object":"list",
        "data":[
            {"id":"anthropic/claude-sonnet-4-6","name":"Claude"},
            {"id":"cline-pass/qwen3.7-max","name":"Qwen3.7 Max","context_length":1000000},
            {"id":"cline-pass/new-live","name":"New Live","context_length":256000}
        ]
    }"#;

    let models = parse_clinepass_catalog(body).expect("catalog");

    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["cline-pass/qwen3.7-max", "cline-pass/new-live"]
    );
    assert_eq!(models[1].name, "New Live");
    assert_eq!(models[1].context_window, 256_000);
}

#[tokio::test]
async fn catalog_fetch_rejects_chunked_body_past_raw_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..17 {
            if write!(stream, "{:X}\r\n", chunk.len()).is_err()
                || stream.write_all(&chunk).is_err()
                || stream.write_all(b"\r\n").is_err()
            {
                break;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    let client = reqwest::Client::new();

    let error = fetch_clinepass_catalog(&client, &format!("http://{address}"), "fixture-key")
        .await
        .expect_err("oversized catalog must fail before parsing");
    server.join().expect("server");
    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { message }
            if message.contains("bound")
    ));
}
