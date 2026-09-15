use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_cli::opencode_zen_catalog::{parse_catalog, OpenCodeZenCatalog};

fn temp_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "slim-zen-catalog-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

fn catalog_server(body: &'static [u8]) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).expect("request");
        let request = String::from_utf8_lossy(&request[..size]);
        assert!(request.starts_with("GET /models "));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("headers");
        stream.write_all(body).expect("body");
    });
    (format!("http://{address}/models"), server)
}

#[test]
fn catalog_keeps_only_free_ids_with_local_metadata() {
    let body = br#"{"object":"list","data":[
        {"id":"claude-sonnet-4-6"},
        {"id":"big-pickle"},
        {"id":"gpt-5.6-luna"},
        {"id":"mimo-v2.5-free"},
        {"id":"ring-2.6-1t-free"}
    ]}"#;

    let models = parse_catalog(body).expect("catalog");

    assert_eq!(models, vec!["big-pickle", "mimo-v2.5-free"]);
}

#[test]
fn catalog_without_free_models_fails_closed() {
    let body = br#"{"object":"list","data":[{"id":"claude-sonnet-4-6"}]}"#;

    assert!(parse_catalog(body).is_err());
}

#[test]
fn duplicate_ids_fail_closed() {
    let body = br#"{"object":"list","data":[{"id":"big-pickle"},{"id":"big-pickle"}]}"#;

    assert!(parse_catalog(body).is_err());
}

#[test]
fn oversized_body_fails_closed() {
    let body = vec![b' '; 1024 * 1024 + 1];

    assert!(parse_catalog(&body).is_err());
}

#[tokio::test]
async fn refresh_fetches_catalog_and_publishes_cache() {
    let body = br#"{"object":"list","data":[{"id":"big-pickle"},{"id":"claude-sonnet-4-6"},{"id":"nemotron-3-ultra-free"}]}"#;
    let (url, server) = catalog_server(body);
    let root = temp_path("refresh");
    let cache = root.join("models.json");
    let catalog = OpenCodeZenCatalog::at(url, &cache).expect("catalog");

    let snapshot = catalog.refresh().await.expect("refresh");
    server.join().expect("server");

    assert_eq!(
        snapshot.source,
        slim_cli::opencode_zen_catalog::CatalogSource::Live
    );
    assert_eq!(
        snapshot.model_ids,
        vec!["big-pickle", "nemotron-3-ultra-free"]
    );
    assert!(cache.is_file());
    let cached = catalog.load_or_fallback();
    assert_eq!(
        cached.source,
        slim_cli::opencode_zen_catalog::CatalogSource::Cache
    );
    assert_eq!(cached.model_ids, snapshot.model_ids);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_cache_uses_free_fallback() {
    let root = temp_path("fallback");
    let catalog = OpenCodeZenCatalog::at("http://127.0.0.1:1/models", root.join("missing.json"))
        .expect("catalog");

    let snapshot = catalog.load_or_fallback();

    assert_eq!(
        snapshot.source,
        slim_cli::opencode_zen_catalog::CatalogSource::Fallback
    );
    assert_eq!(
        snapshot.model_ids.len(),
        slim_core::provider::zen_models().len()
    );
    assert!(snapshot.model_ids.contains(&"big-pickle".to_owned()));
}
