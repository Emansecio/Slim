use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_cli::command_code_catalog::{CatalogSource, CommandCodeCatalog};
use slim_core::provider::parse_command_code_catalog;

fn temp_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "slim-command-code-catalog-{label}-{}-{}",
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
fn live_catalog_keeps_ids_absent_from_the_bundle() {
    let body = br#"{
        "object":"list",
        "data":[
            {"id":"deepseek/deepseek-v4-flash","name":"DeepSeek V4 Flash","context_length":1000000},
            {"id":"future/open-model","name":"Future Open","context_length":256000}
        ]
    }"#;

    let models = parse_command_code_catalog(body).expect("catalog");
    let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, vec!["deepseek/deepseek-v4-flash", "future/open-model"]);
}

#[test]
fn invalid_id_characters_fail_closed() {
    let body = br#"{"object":"list","data":[{"id":"bad id"}]}"#;
    assert!(parse_command_code_catalog(body).is_err());
}

#[tokio::test]
async fn refresh_fetches_public_catalog_and_publishes_cache() {
    // The live catalog includes :free IDs alongside DeepSeek. Rejecting one
    // unrelated entry must not make the supported V4.1 entry disappear.
    let body = br#"{"object":"list","data":[{"id":"deepseek/deepseek-v4.1-flash","name":"DeepSeek V4.1 Flash","context_length":1000000},{"id":"meituan/LongCat-2.0:free"}]}"#;
    let (url, server) = catalog_server(body);
    let root = temp_path("refresh");
    let cache = root.join("models.json");
    let catalog = CommandCodeCatalog::at(url, &cache).expect("catalog");

    let snapshot = catalog.refresh().await.expect("refresh");
    server.join().expect("server");

    assert_eq!(snapshot.source, CatalogSource::Live);
    assert_eq!(snapshot.models.len(), 2);
    assert_eq!(snapshot.models[0].id, "deepseek/deepseek-v4.1-flash");
    assert_eq!(snapshot.models[0].name, "DeepSeek V4.1 Flash");
    assert_eq!(snapshot.models[0].context_window, 1_000_000);
    assert!(cache.is_file());
    let cached = catalog.load_or_fallback();
    assert_eq!(cached.source, CatalogSource::Cache);
    assert_eq!(cached.models, snapshot.models);
    let _ = std::fs::remove_dir_all(root);
}
