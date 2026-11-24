use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_cli::opencode_go_catalog::{
    parse_catalog, parse_catalog_specs, CatalogSource, OpenCodeCatalog,
};
use slim_core::provider::OpenCodeApi;

fn temp_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "slim-opencode-catalog-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

fn catalog_server(
    expected_path: &'static str,
    expected_auth: Option<&'static str>,
    status: &'static str,
    body: &'static [u8],
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).expect("request");
        let request = String::from_utf8_lossy(&request[..size]);
        assert!(
            request.starts_with(&format!("GET {expected_path} ")),
            "unexpected request line: {request}"
        );
        match expected_auth {
            Some(token) => assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("authorization: bearer {}", token.to_ascii_lowercase())),
                "missing bearer auth in request: {request}"
            ),
            None => assert!(
                !request.to_ascii_lowercase().contains("authorization:"),
                "unexpected auth header in request: {request}"
            ),
        }
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("headers");
        stream.write_all(body).expect("body");
    });
    (format!("http://{address}{expected_path}"), server)
}

#[test]
fn parses_inference_list_including_brand_new_models_and_routes() {
    let body = br#"{
        "object": "list",
        "data": [
            {
                "id": "deepseek-v4.1-flash",
                "name": "DeepSeek V4.1 Flash 1017",
                "type": "chat",
                "modalities": {"input": ["text", "image/png"], "output": ["text"]},
                "capabilities": {"reasoning": {"supported": true, "effort": ["low", "high", "max"]}},
                "limit": {"context": 1000000, "output": 384000},
                "api": {"chat-completions": {"base_url": "https://opencode.ai/inference/go/openai/v1"}},
                "recommended_api": "chat-completions"
            },
            {
                "id": "future-model-9",
                "name": "Future Model 9",
                "type": "chat",
                "modalities": {"input": ["text"], "output": ["text"]},
                "capabilities": {"reasoning": {"supported": true, "effort": ["none", "low", "xhigh"]}},
                "limit": {"context": 2000000, "output": 256000},
                "api": {"responses": {"base_url": "https://opencode.ai/inference/go/openai/v1"}},
                "recommended_api": "responses"
            },
            {
                "id": "qwen4-max",
                "name": "Qwen4 Max",
                "type": "chat",
                "modalities": {"input": ["text", "image/jpeg"], "output": ["text"]},
                "capabilities": {"reasoning": {"supported": true, "effort": ["low", "high", "xhigh"]}},
                "limit": {"context": 1000000, "output": 262144},
                "api": {"messages": {"base_url": "https://opencode.ai/inference/go/anthropic/v1"}},
                "recommended_api": "messages"
            },
            {
                "id": "gemini-4-argon",
                "name": "Gemini 4 Argon",
                "type": "chat",
                "modalities": {"input": ["text", "image/png"], "output": ["text"]},
                "capabilities": {"reasoning": {"supported": true, "effort": ["low", "medium", "high"]}},
                "limit": {"context": 1048576, "output": 65536},
                "api": {"generate-content": {"base_url": "https://opencode.ai/inference/go/google/v1beta"}},
                "recommended_api": "interactions"
            }
        ]
    }"#;

    let specs = parse_catalog_specs(body).expect("catalog");

    assert_eq!(specs.len(), 4);
    assert_eq!(specs[0].id, "deepseek-v4.1-flash");
    assert_eq!(specs[0].api, OpenCodeApi::ChatCompletions);
    assert_eq!(
        specs[0].base_url.as_deref(),
        Some("https://opencode.ai/inference/go/openai/v1")
    );
    assert_eq!(specs[1].id, "future-model-9");
    assert_eq!(specs[1].name, "Future Model 9");
    assert_eq!(specs[1].api, OpenCodeApi::Responses);
    assert_eq!(specs[1].context_window, Some(2_000_000));
    assert_eq!(specs[1].max_output_tokens, Some(256_000));
    assert_eq!(specs[1].reasoning_levels, vec!["low", "xhigh"]);
    assert_eq!(specs[2].api, OpenCodeApi::AnthropicMessages);
    assert_eq!(
        specs[2].base_url.as_deref(),
        Some("https://opencode.ai/inference/go/anthropic/v1")
    );
    // Gemini models translate through OpenAI Chat Completions on Console.
    assert_eq!(specs[3].id, "gemini-4-argon");
    assert_eq!(specs[3].api, OpenCodeApi::ChatCompletions);
    assert_eq!(
        specs[3].base_url.as_deref(),
        Some("https://opencode.ai/inference/go/openai/v1")
    );
}

#[test]
fn parses_console_model_list_for_opencode_go_provider() {
    let body = br#"{
        "opencode": {
            "id": "opencode",
            "name": "Workspace / OpenCode",
            "models": {
                "claude-sonnet-5-5": {
                    "id": "claude-sonnet-5-5",
                    "name": "Claude Sonnet 5.5",
                    "type": "chat"
                }
            }
        },
        "opencode-go": {
            "id": "opencode-go",
            "name": "Workspace / OpenCode Go",
            "models": {
                "gpt-6-luna": {
                    "id": "gpt-6-luna",
                    "name": "GPT-6 Luna",
                    "type": "chat",
                    "modalities": {"input": ["text", "image/png"], "output": ["text"]},
                    "capabilities": {"reasoning": {"supported": true, "effort": ["low", "medium", "high", "xhigh", "max"]}},
                    "limit": {"context": 1050000, "output": 128000},
                    "api": {"responses": {"base_url": "https://opencode.ai/inference/go/openai/v1"}}
                },
                "future-go-model": {
                    "id": "future-go-model",
                    "name": "Future Go Model",
                    "type": "chat",
                    "limit": {"context": 512000, "output": 64000},
                    "api": {"chat-completions": {"base_url": "https://evil.example/v1"}}
                }
            }
        }
    }"#;

    let specs = parse_catalog_specs(body).expect("console catalog");
    assert_eq!(specs.len(), 2);
    let luna = specs.iter().find(|m| m.id == "gpt-6-luna").expect("gpt-6-luna");
    assert_eq!(luna.api, OpenCodeApi::Responses);
    assert_eq!(
        luna.base_url.as_deref(),
        Some("https://opencode.ai/inference/go/openai/v1")
    );
    assert!(luna.accepts_images);
    // Untrusted base_url from any response must be ignored.
    let future = specs
        .iter()
        .find(|m| m.id == "future-go-model")
        .expect("future-go-model");
    assert_eq!(future.base_url, None);
    assert_eq!(future.api, OpenCodeApi::ChatCompletions);
}

#[test]
fn empty_catalog_fails_closed() {
    let body = br#"{"object":"list","data":[]}"#;

    assert!(parse_catalog(body).is_err());
}

#[test]
fn duplicate_ids_fail_closed() {
    let body = br#"{"object":"list","data":[{"id":"glm-5.3"},{"id":"glm-5.3"}]}"#;

    assert!(parse_catalog(body).is_err());
}

#[test]
fn invalid_id_characters_fail_closed() {
    let body = br#"{"object":"list","data":[{"id":"glm 5.3"}]}"#;

    assert!(parse_catalog(body).is_err());
}

#[test]
fn more_than_256_entries_fail_closed() {
    let data = (0..257)
        .map(|index| serde_json::json!({"id": format!("model-{index}")}))
        .collect::<Vec<_>>();
    let body =
        serde_json::to_vec(&serde_json::json!({"object":"list","data":data})).expect("fixture");

    assert!(parse_catalog(&body).is_err());
}

#[test]
fn oversized_body_fails_closed() {
    let body = vec![b' '; 4 * 1024 * 1024 + 1];

    assert!(parse_catalog(&body).is_err());
}

#[tokio::test]
async fn refresh_prefers_console_models_endpoint_when_authenticated() {
    let console_body = br#"{
        "opencode-go": {
            "id": "opencode-go",
            "models": {
                "gpt-6-luna": {
                    "id": "gpt-6-luna",
                    "name": "GPT-6 Luna",
                    "type": "chat",
                    "limit": {"context": 1050000, "output": 128000},
                    "api": {"responses": {"base_url": "https://opencode.ai/inference/go/openai/v1"}}
                }
            }
        }
    }"#;
    let (console_url, console_server) = catalog_server(
        "/console/api/v1/models",
        Some("oc_sk_test"),
        "200 OK",
        console_body,
    );
    let root = temp_path("console-refresh");
    let cache = root.join("models.json");
    let catalog =
        OpenCodeCatalog::at_with_console(console_url, "http://127.0.0.1:1/models", &cache)
            .expect("catalog");

    let snapshot = catalog
        .refresh_with_key(Some("oc_sk_test"))
        .await
        .expect("refresh");
    console_server.join().expect("console server");

    assert_eq!(snapshot.source, CatalogSource::Live);
    assert_eq!(snapshot.model_ids(), vec!["gpt-6-luna"]);
    assert!(cache.is_file());
    let cached = catalog.load_or_fallback();
    assert_eq!(cached.source, CatalogSource::Cache);
    assert_eq!(cached.models, snapshot.models);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn refresh_falls_back_to_inference_catalog_when_console_rejects_key() {
    let (console_url, console_server) = catalog_server(
        "/console/api/v1/models",
        Some("oc_sk_restricted"),
        "403 Forbidden",
        br#"{"_tag":"Forbidden"}"#,
    );
    let fallback_body = br#"{"object":"list","data":[{"id":"deepseek-v4.1-flash"},{"id":"glm-5.3"}]}"#;
    let (inference_url, inference_server) = catalog_server(
        "/inference/go/v1/models",
        Some("oc_sk_restricted"),
        "200 OK",
        fallback_body,
    );
    let root = temp_path("console-fallback");
    let cache = root.join("models.json");
    let catalog =
        OpenCodeCatalog::at_with_console(console_url, inference_url, &cache).expect("catalog");

    let snapshot = catalog
        .refresh_with_key(Some("oc_sk_restricted"))
        .await
        .expect("refresh");
    console_server.join().expect("console server");
    inference_server.join().expect("inference server");

    assert_eq!(snapshot.source, CatalogSource::Live);
    assert_eq!(
        snapshot.model_ids(),
        vec!["deepseek-v4.1-flash", "glm-5.3"]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn refresh_fetches_public_catalog_and_publishes_cache() {
    let body = br#"{"object":"list","data":[{"id":"deepseek-v4.1-flash"},{"id":"glm-5.3"}]}"#;
    let (url, server) = catalog_server("/models", None, "200 OK", body);
    let root = temp_path("refresh");
    let cache = root.join("models.json");
    let catalog = OpenCodeCatalog::at(url, &cache).expect("catalog");

    let snapshot = catalog.refresh().await.expect("refresh");
    server.join().expect("server");

    assert_eq!(snapshot.source, CatalogSource::Live);
    assert_eq!(
        snapshot.model_ids(),
        vec!["deepseek-v4.1-flash", "glm-5.3"]
    );
    assert!(cache.is_file());
    let cached = catalog.load_or_fallback();
    assert_eq!(cached.source, CatalogSource::Cache);
    assert_eq!(cached.models, snapshot.models);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn redirect_is_rejected_and_last_valid_cache_survives() {
    let body = br#"{"object":"list","data":[{"id":"glm-5.3"}]}"#;
    let (url, server) = catalog_server("/models", None, "200 OK", body);
    let root = temp_path("redirect");
    let cache = root.join("models.json");
    let catalog = OpenCodeCatalog::at(url, &cache).expect("catalog");
    catalog.refresh().await.expect("initial refresh");
    server.join().expect("server");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind redirect");
    let address = listener.local_addr().expect("redirect address");
    let redirect = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("redirect accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("redirect request");
        stream
            .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/blocked\r\nContent-Length: 0\r\n\r\n")
            .expect("redirect response");
    });
    let catalog =
        OpenCodeCatalog::at(format!("http://{address}/models"), &cache).expect("redirect catalog");

    assert!(catalog.refresh().await.is_err());
    redirect.join().expect("redirect server");
    let snapshot = catalog.load_or_fallback();
    assert_eq!(snapshot.source, CatalogSource::Cache);
    assert_eq!(snapshot.model_ids(), vec!["glm-5.3"]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_cache_uses_documented_fallback() {
    let root = temp_path("fallback");
    let catalog = OpenCodeCatalog::at("http://127.0.0.1:1/models", root.join("missing.json"))
        .expect("catalog");

    let snapshot = catalog.load_or_fallback();

    assert_eq!(snapshot.source, CatalogSource::Fallback);
    assert_eq!(snapshot.models.len(), 31);
}

#[test]
fn directory_cache_path_is_ignored_without_destroying_it() {
    let root = temp_path("directory");
    std::fs::create_dir_all(&root).expect("root");
    let catalog = OpenCodeCatalog::at("http://127.0.0.1:1/models", &root).expect("catalog");

    let snapshot = catalog.load_or_fallback();

    assert_eq!(snapshot.source, CatalogSource::Fallback);
    assert!(root.is_dir());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn errors_name_the_go_provider() {
    let error = parse_catalog(b"{}").expect_err("schema");

    assert_eq!(error.to_string(), "OpenCode Go catalog schema is invalid");
}
