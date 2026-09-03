use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_cli::codex_catalog::{
    fetch_codex_catalog, should_fetch_live_codex_catalog, CodexCatalog, CodexCatalogError,
};

fn catalog_server(expected_path_prefix: &'static str, body: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 8192];
        let size = stream.read(&mut request).expect("request");
        let request = String::from_utf8_lossy(&request[..size]);
        assert!(
            request.starts_with(&format!("GET {expected_path_prefix}")),
            "request={request}"
        );
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer catalog-secret"));
        assert!(request
            .to_ascii_lowercase()
            .contains("chatgpt-account-id: acct-1"));
        assert!(request.to_ascii_lowercase().contains("originator: slim"));
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("headers");
        stream.write_all(body).expect("body");
    });
    format!("http://{address}/backend-api")
}

#[test]
fn live_fetch_reads_context_window_from_codex_models_endpoint() {
    let endpoint = catalog_server(
        "/backend-api/codex/models?",
        br#"{"models":[{"slug":"gpt-5.6-sol","context_window":258400,"max_context_window":272000}]}"#,
    );
    let cache = std::env::temp_dir().join(format!(
        "slim-codex-catalog-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));

    let snapshot = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(fetch_codex_catalog(
            &endpoint,
            "catalog-secret",
            "acct-1",
            cache.as_path(),
        ))
        .expect("live catalog");

    let sol = snapshot
        .entries
        .iter()
        .find(|entry| entry.slug == "gpt-5.6-sol")
        .expect("sol");
    assert_eq!(sol.context_window, 258_400);
    let _ = std::fs::remove_file(cache);
}

#[test]
fn malformed_live_catalog_fails_closed() {
    let endpoint = catalog_server("/backend-api/codex/models?", b"{\"models\":[]}");
    let cache = std::env::temp_dir().join(format!(
        "slim-codex-catalog-empty-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));

    let error = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(fetch_codex_catalog(
            &endpoint,
            "catalog-secret",
            "acct-1",
            cache.as_path(),
        ))
        .expect_err("empty catalog");
    assert_eq!(error, CodexCatalogError::Empty);
    let _ = std::fs::remove_file(cache);
}

#[test]
fn production_chatgpt_hosts_opt_into_live_catalog() {
    assert!(should_fetch_live_codex_catalog(
        "https://chatgpt.com/backend-api"
    ));
    assert!(should_fetch_live_codex_catalog(
        "https://chat.openai.com/backend-api"
    ));
    assert!(!should_fetch_live_codex_catalog(
        "http://127.0.0.1:9/backend-api"
    ));
    assert!(!should_fetch_live_codex_catalog(
        "https://example.invalid/backend-api"
    ));
}

#[test]
fn seeded_disk_cache_is_available_without_network() {
    let cache = std::env::temp_dir().join(format!(
        "slim-codex-catalog-seeded-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::write(
        &cache,
        br#"{"version":1,"entries":[{"slug":"gpt-5.6-sol","context_window":345678}]}"#,
    )
    .expect("seed cache");
    let catalog = CodexCatalog::at(&cache).expect("catalog");

    let snapshot = catalog
        .load_cached("http://127.0.0.1:9/backend-api", "acct-offline")
        .expect("cached catalog")
        .expect("seeded snapshot");

    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].slug, "gpt-5.6-sol");
    assert_eq!(snapshot.entries[0].context_window, 345_678);
    let _ = std::fs::remove_file(cache);
}
