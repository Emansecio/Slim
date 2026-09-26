use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_cli::opencode_go_catalog::{parse_catalog, CatalogSource, OpenCodeCatalog};

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
fn catalog_intersects_live_ids_with_documented_registry() {
    let body = br#"{"object":"list","data":[{"id":"deepseek-v4-flash"},{"id":"unknown-new-model"},{"id":"deepseek-flash"},{"id":"muse-spark-1.3-contributor"}]}"#;

    let models = parse_catalog(body).expect("catalog");

    assert_eq!(
        models,
        vec![
            "deepseek-flash",
            "deepseek-v4-flash",
            "muse-spark-1.3-contributor"
        ]
    );
}

#[test]
fn catalog_without_supported_models_fails_closed() {
    let body = br#"{"object":"list","data":[{"id":"future-unknown-model"}]}"#;

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
    let body = vec![b' '; 1024 * 1024 + 1];

    assert!(parse_catalog(&body).is_err());
}

#[tokio::test]
async fn refresh_fetches_public_catalog_and_publishes_cache() {
    let body = br#"{"object":"list","data":[{"id":"deepseek-flash"},{"id":"glm-5.3"}]}"#;
    let (url, server) = catalog_server(body);
    let root = temp_path("refresh");
    let cache = root.join("models.json");
    let catalog = OpenCodeCatalog::at(url, &cache).expect("catalog");

    let snapshot = catalog.refresh().await.expect("refresh");
    server.join().expect("server");

    assert_eq!(snapshot.source, CatalogSource::Live);
    assert_eq!(snapshot.model_ids, vec!["glm-5.3", "deepseek-flash"]);
    assert!(cache.is_file());
    let cached = catalog.load_or_fallback();
    assert_eq!(cached.source, CatalogSource::Cache);
    assert_eq!(cached.model_ids, snapshot.model_ids);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn redirect_is_rejected_and_last_valid_cache_survives() {
    let body = br#"{"object":"list","data":[{"id":"glm-5.3"}]}"#;
    let (url, server) = catalog_server(body);
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
    assert_eq!(snapshot.model_ids, vec!["glm-5.3"]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_cache_uses_documented_fallback() {
    let root = temp_path("fallback");
    let catalog = OpenCodeCatalog::at("http://127.0.0.1:1/models", root.join("missing.json"))
        .expect("catalog");

    let snapshot = catalog.load_or_fallback();

    assert_eq!(snapshot.source, CatalogSource::Fallback);
    assert_eq!(snapshot.model_ids.len(), 25);
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
