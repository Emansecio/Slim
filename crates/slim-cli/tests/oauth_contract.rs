use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use slim_cli::oauth::callback::await_callback;
use slim_cli::oauth::pkce::{challenge_for_verifier, generate_pkce, generate_state};
use slim_cli::oauth::{
    codex_account_id, BrowserLauncher, OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress,
    OAuthProvider, OAuthService, OAuthStore,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

#[test]
fn production_anthropic_endpoint_matches_direct_inference_oauth_contract() {
    assert_eq!(
        OAuthEndpoints::default().anthropic_token,
        "https://api.anthropic.com/v1/oauth/token"
    );
}

#[test]
fn pkce_matches_rfc_vector_and_generated_values_are_url_safe() {
    assert_eq!(
        challenge_for_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let pkce = generate_pkce().expect("pkce");
    assert!(pkce.verifier.len() >= 43);
    assert!(!pkce.verifier.contains('='));
    assert!(!pkce.challenge.contains('='));
    assert_ne!(
        generate_state().expect("state"),
        generate_state().expect("state")
    );
}

#[test]
fn oauth_credentials_and_errors_never_debug_tokens() {
    let credential = OAuthCredential {
        access: "access-secret".into(),
        refresh: "refresh-secret".into(),
        expires: 42,
        account_id: Some("account-1".into()),
    };
    let debug = format!("{credential:?}");
    assert!(!debug.contains("access-secret"));
    assert!(!debug.contains("refresh-secret"));

    let pkce = generate_pkce().expect("pkce");
    let verifier = pkce.verifier.clone();
    assert!(!format!("{pkce:?}").contains(&verifier));
    let progress = OAuthProgress::AuthUrl {
        url: "https://example.test/authorize?state=secret-state".into(),
        user_code: Some("secret-code".into()),
    };
    let debug = format!("{progress:?}");
    assert!(!debug.contains("secret-state"));
    assert!(!debug.contains("secret-code"));
}

#[test]
fn codex_account_id_is_read_from_expected_jwt_claim() {
    let payload = URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-1"}}"#);
    assert_eq!(
        codex_account_id(&format!("header.{payload}.signature")).expect("account"),
        "account-1"
    );
}

#[test]
fn oauth_store_round_trips_active_credential_without_debug_leak() {
    let root = std::env::temp_dir().join(format!(
        "slim-oauth-store-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("root");
    let auth_path = root.join("auth.json");
    std::fs::write(
        &auth_path,
        r#"{"version":1,"providers":{"openai-codex":{"api_key":"existing-key"}}}"#,
    )
    .expect("existing auth");
    let store = OAuthStore::at(&auth_path);
    let credential = OAuthCredential {
        access: "access-secret".into(),
        refresh: "refresh-secret".into(),
        expires: 42,
        account_id: Some("account-1".into()),
    };
    store
        .save(OAuthProvider::OpenAiCodex, &credential)
        .expect("save");
    assert_eq!(
        store.active().expect("active"),
        Some((OAuthProvider::OpenAiCodex, credential))
    );
    let saved = std::fs::read_to_string(&auth_path).expect("auth text");
    assert!(saved.contains("access-secret") || saved.contains("oauth"));
    let saved_document: serde_json::Value =
        serde_json::from_str(&saved).expect("saved auth document");
    assert_eq!(
        saved_document["providers"]["openai-codex"]["api_key"], "existing-key",
        "OAuth save must preserve the sibling api_key"
    );
    assert_eq!(
        saved_document["providers"]["openai-codex"]["preferred_method"], "oauth",
        "OAuth save must select OAuth"
    );
    store.remove(OAuthProvider::OpenAiCodex).expect("remove");
    assert_eq!(store.active().expect("active"), None);
    let removed_document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(auth_path).expect("auth text after remove"))
            .expect("auth document after remove");
    assert_eq!(
        removed_document["providers"]["openai-codex"]["api_key"], "existing-key",
        "OAuth removal must preserve the sibling api_key"
    );
    assert_eq!(
        removed_document["providers"]["openai-codex"]["preferred_method"], "oauth",
        "OAuth removal must retain the explicit selection and avoid fallback"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn callback_rejects_wrong_state_then_accepts_valid_code() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(await_callback(
        listener,
        "/callback",
        "expected",
        cancel_rx,
        Duration::from_secs(2),
    ));
    let wrong = request(address, "/callback?code=secret-code&state=wrong").await;
    assert!(wrong.contains("400"));
    assert!(!wrong.contains("secret-code"));
    let valid = request(address, "/callback?code=good-code&state=expected").await;
    assert!(valid.contains("200"));
    assert_eq!(task.await.expect("task").expect("callback"), "good-code");
}

#[tokio::test]
async fn callback_accepts_http_request_fragmented_across_reads() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(await_callback(
        listener,
        "/callback",
        "expected",
        cancel_rx,
        Duration::from_millis(500),
    ));
    let mut stream = TcpStream::connect(address).await.expect("connect");
    stream
        .write_all(b"GET /callback?code=fragmented")
        .await
        .expect("first fragment");
    tokio::time::sleep(Duration::from_millis(20)).await;
    stream
        .write_all(b"-code&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("second fragment");

    assert_eq!(
        task.await.expect("task").expect("callback"),
        "fragmented-code"
    );
}

#[tokio::test]
async fn callback_cancellation_interrupts_an_idle_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(await_callback(
        listener,
        "/callback",
        "expected",
        cancel_rx,
        Duration::from_secs(2),
    ));
    let _idle = TcpStream::connect(address).await.expect("connect");
    cancel_tx.send(true).expect("cancel");

    let result = tokio::time::timeout(Duration::from_millis(200), task)
        .await
        .expect("cancellation must not wait for callback read timeout")
        .expect("task");
    assert!(matches!(result, Err(OAuthError::Cancelled)));
}

#[derive(Clone, Copy)]
struct CallbackBrowser;

impl BrowserLauncher for CallbackBrowser {
    fn open(&self, url: &str) -> Result<(), OAuthError> {
        let url = reqwest::Url::parse(url).expect("auth url");
        let redirect = url
            .query_pairs()
            .find(|(name, _)| name == "redirect_uri")
            .map(|(_, value)| value.into_owned())
            .expect("redirect");
        let state = url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .map(|(_, value)| value.into_owned())
            .expect("state");
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let redirect = reqwest::Url::parse(&redirect).expect("redirect url");
            let port = redirect.port().expect("port");
            let mut stream = StdTcpStream::connect(("127.0.0.1", port)).expect("callback");
            let target = format!("{}?code=fixture-code&state={state}", redirect.path());
            stream
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                .expect("callback write");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
        });
        Ok(())
    }
}

#[test]
fn api_key_save_and_remove_preserve_sibling_oauth_entries() {
    let root = std::env::temp_dir().join(format!(
        "slim-api-key-store-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let path = root.join("auth.json");
    let store = OAuthStore::at(&path);
    let credential = OAuthCredential {
        access: "oauth-access".into(),
        refresh: "oauth-refresh".into(),
        expires: u64::MAX,
        account_id: None,
    };
    store
        .save(OAuthProvider::Anthropic, &credential)
        .expect("save sibling OAuth");

    store
        .save_api_key("opencode-go", "go-secret")
        .expect("save API key");
    assert_eq!(
        store.api_key("opencode-go").expect("read key").as_deref(),
        Some("go-secret")
    );
    store.remove_api_key("opencode-go").expect("remove API key");
    assert!(store
        .credential(OAuthProvider::Anthropic)
        .expect("read sibling")
        .is_some());
    assert!(store.api_key("opencode-go").expect("removed key").is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn empty_api_key_is_rejected_without_publishing_auth_file() {
    let root = std::env::temp_dir().join(format!(
        "slim-empty-api-key-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let path = root.join("auth.json");
    let store = OAuthStore::at(&path);

    let error = store
        .save_api_key("opencode-go", "  ")
        .expect_err("empty key must fail");

    assert!(!path.exists());
    assert!(!error.to_string().contains("opencode-go"));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn anthropic_native_login_exchanges_callback_and_persists_credential() {
    let access = "anthropic-access";
    let (token_url, server) = token_server(format!(
        "{{\"access_token\":\"{access}\",\"refresh_token\":\"anthropic-refresh\",\"expires_in\":3600}}"
    ));
    let root = std::env::temp_dir().join(format!("slim-anthropic-oauth-{}", std::process::id()));
    let store = OAuthStore::at(root.join("auth.json"));
    let endpoints = OAuthEndpoints {
        anthropic_token: token_url,
        ..OAuthEndpoints::default()
    };
    let service =
        OAuthService::new(endpoints, Arc::new(CallbackBrowser), store.clone()).expect("service");
    let (progress, _events) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel, cancel_rx) = watch::channel(false);
    let credential = service
        .login(OAuthProvider::Anthropic, progress, cancel_rx)
        .await
        .expect("login");
    server.join().expect("token server");
    assert_eq!(credential.access, access);
    assert_eq!(
        store
            .active()
            .expect("active")
            .map(|(provider, _)| provider),
        Some(OAuthProvider::Anthropic)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn codex_native_login_extracts_account_and_persists_credential() {
    let payload = URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-1"}}"#);
    let access = format!("header.{payload}.signature");
    let (token_url, server) = token_server(format!(
        "{{\"access_token\":\"{access}\",\"refresh_token\":\"codex-refresh\",\"expires_in\":3600}}"
    ));
    let root = std::env::temp_dir().join(format!("slim-codex-oauth-{}", std::process::id()));
    let store = OAuthStore::at(root.join("auth.json"));
    let endpoints = OAuthEndpoints {
        codex_token: token_url,
        ..OAuthEndpoints::default()
    };
    let service =
        OAuthService::new(endpoints, Arc::new(CallbackBrowser), store.clone()).expect("service");
    let (progress, _events) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel, cancel_rx) = watch::channel(false);
    let credential = service
        .login(OAuthProvider::OpenAiCodex, progress, cancel_rx)
        .await
        .expect("login");
    server.join().expect("token server");
    assert_eq!(credential.account_id.as_deref(), Some("account-1"));
    assert_eq!(
        store
            .active()
            .expect("active")
            .map(|(provider, _)| provider),
        Some(OAuthProvider::OpenAiCodex)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_refresh_uses_latest_rotated_credential_once() {
    let root = std::env::temp_dir().join(format!("slim-refresh-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = OAuthCredential {
        access: "expired-access".into(),
        refresh: "expired-refresh".into(),
        expires: 1,
        account_id: None,
    };
    store
        .save(OAuthProvider::Anthropic, &expired)
        .expect("expired store");
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("refresh bind");
    let address = listener.local_addr().expect("refresh address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("refresh accept");
        let mut request = [0_u8; 16 * 1024];
        let size = stream.read(&mut request).expect("refresh request");
        assert!(String::from_utf8_lossy(&request[..size]).contains("expired-refresh"));
        thread::sleep(Duration::from_millis(50));
        let body = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("refresh response");
    });
    let endpoints = OAuthEndpoints {
        anthropic_token: format!("http://{address}"),
        ..OAuthEndpoints::default()
    };
    let service =
        OAuthService::new(endpoints, Arc::new(CallbackBrowser), store.clone()).expect("service");
    let (first, second) = tokio::join!(
        service.fresh_credential(OAuthProvider::Anthropic, expired.clone()),
        service.fresh_credential(OAuthProvider::Anthropic, expired)
    );
    server.join().expect("refresh server");
    assert_eq!(first.expect("first").credential.access, "rotated-access");
    assert_eq!(second.expect("second").credential.access, "rotated-access");
    assert_eq!(
        store
            .credential(OAuthProvider::Anthropic)
            .expect("stored")
            .expect("credential")
            .refresh,
        "rotated-refresh"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn refreshed_credential_survives_persistence_failure_in_memory() {
    let root = std::env::temp_dir().join(format!("slim-refresh-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let auth_path = root.join("auth.json");
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("refresh bind");
    let address = listener.local_addr().expect("refresh address");
    let blocked_path = auth_path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("refresh accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("refresh request");
        std::fs::create_dir(&blocked_path).expect("block auth path");
        let body = r#"{"access_token":"memory-access","refresh_token":"memory-refresh","expires_in":3600}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("refresh response");
    });
    let endpoints = OAuthEndpoints {
        anthropic_token: format!("http://{address}"),
        ..OAuthEndpoints::default()
    };
    let service = OAuthService::new(
        endpoints,
        Arc::new(CallbackBrowser),
        OAuthStore::at(auth_path),
    )
    .expect("service");
    let fresh = service
        .fresh_credential(
            OAuthProvider::Anthropic,
            OAuthCredential {
                access: "expired-access".into(),
                refresh: "expired-refresh".into(),
                expires: 1,
                account_id: None,
            },
        )
        .await
        .expect("in-memory refresh");
    server.join().expect("refresh server");
    assert_eq!(fresh.credential.refresh, "memory-refresh");
    assert!(fresh.persistence_warning.is_some());
    let next = service
        .fresh_credential(OAuthProvider::Anthropic, fresh.credential.clone())
        .await
        .expect("fresh in-memory credential bypasses unreadable store");
    assert_eq!(next.credential.refresh, "memory-refresh");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn anthropic_refresh_rejects_oversized_token_response() {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("refresh bind");
    let address = listener.local_addr().expect("refresh address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("refresh accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("refresh request");
        let body = format!(
            "{{\"access_token\":\"{}\",\"refresh_token\":\"rotated\",\"expires_in\":3600}}",
            "x".repeat(70 * 1024)
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("refresh response");
    });
    let root = std::env::temp_dir().join(format!("slim-oauth-limit-{}", std::process::id()));
    let service = OAuthService::new(
        OAuthEndpoints {
            anthropic_token: format!("http://{address}"),
            ..OAuthEndpoints::default()
        },
        Arc::new(CallbackBrowser),
        OAuthStore::at(root.join("auth.json")),
    )
    .expect("service");

    let error = service
        .fresh_credential(
            OAuthProvider::Anthropic,
            OAuthCredential {
                access: "expired".into(),
                refresh: "refresh".into(),
                expires: 1,
                account_id: None,
            },
        )
        .await
        .expect_err("oversized OAuth response must be rejected");
    server.join().expect("server");
    assert!(matches!(error, OAuthError::InvalidResponse(_)));
    let _ = std::fs::remove_dir_all(root);
}

fn token_server(body: String) -> (String, thread::JoinHandle<()>) {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("token bind");
    let address = listener.local_addr().expect("token address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("token accept");
        let mut request = [0_u8; 16 * 1024];
        let size = stream.read(&mut request).expect("token request");
        let request = String::from_utf8_lossy(&request[..size]);
        assert!(request.contains("fixture-code"));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        stream
            .write_all(response.as_bytes())
            .expect("token response");
    });
    (format!("http://{address}"), server)
}

async fn request(address: SocketAddr, target: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
}

#[tokio::test]
async fn unexpired_credential_returns_without_waiting_for_refresh() {
    let root = std::env::temp_dir().join(format!(
        "slim-refresh-fast-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let store = OAuthStore::at(root.join("auth.json"));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let live = OAuthCredential {
        access: "live-access".into(),
        refresh: "live-refresh".into(),
        expires: now_ms + 2 * 60 * 1000,
        account_id: None,
    };
    store
        .save(OAuthProvider::Anthropic, &live)
        .expect("live store");
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("refresh bind");
    let address = listener.local_addr().expect("refresh address");
    let _server = thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            thread::sleep(Duration::from_secs(5));
            drop(stream);
        }
    });
    let endpoints = OAuthEndpoints {
        anthropic_token: format!("http://{address}"),
        ..OAuthEndpoints::default()
    };
    let service = OAuthService::new(endpoints, Arc::new(CallbackBrowser), store).expect("service");
    let started = std::time::Instant::now();
    let fresh = service
        .fresh_credential(OAuthProvider::Anthropic, live.clone())
        .await
        .expect("fresh");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "unexpired credential must not wait for refresh HTTP"
    );
    assert_eq!(fresh.credential.access, "live-access");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_refresh_within_ten_minutes_persists_rotated_token() {
    background_refresh_is_reused(3600).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_lived_background_refresh_is_reused() {
    background_refresh_is_reused(240).await;
}

async fn background_refresh_is_reused(expires_in: u32) {
    let root = std::env::temp_dir().join(format!(
        "slim-refresh-background-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let store = OAuthStore::at(root.join("auth.json"));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let live = OAuthCredential {
        access: "live-access".into(),
        refresh: "live-refresh".into(),
        expires: now_ms + 2 * 60 * 1000,
        account_id: None,
    };
    store
        .save(OAuthProvider::Anthropic, &live)
        .expect("live store");
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("refresh bind");
    let address = listener.local_addr().expect("refresh address");
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let server_hits = hits.clone();
    let extra_watch = ExtraConnectionWatch::new();
    let stop_flag = extra_watch.flag();
    let server = thread::spawn(move || {
        listener.set_nonblocking(false).expect("blocking accept");
        let (mut stream, _) = listener.accept().expect("refresh accept");
        server_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("refresh request");
        let body = format!(
            r#"{{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":{expires_in}}}"#
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("refresh response");
        let extra = count_connections_until(&listener, &stop_flag);
        server_hits.fetch_add(extra as u32, std::sync::atomic::Ordering::SeqCst);
    });
    let endpoints = OAuthEndpoints {
        anthropic_token: format!("http://{address}"),
        ..OAuthEndpoints::default()
    };
    let service =
        OAuthService::new(endpoints, Arc::new(CallbackBrowser), store.clone()).expect("service");
    let (first, second) = tokio::join!(
        service.fresh_credential(OAuthProvider::Anthropic, live.clone()),
        service.fresh_credential(OAuthProvider::Anthropic, live.clone())
    );
    assert_eq!(first.expect("first").credential.access, "live-access");
    assert_eq!(second.expect("second").credential.access, "live-access");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let stored = store
            .credential(OAuthProvider::Anthropic)
            .expect("stored")
            .expect("credential");
        if stored.access == "rotated-access" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background refresh must persist the rotated token"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let next = service
        .fresh_credential(OAuthProvider::Anthropic, live)
        .await
        .expect("reread store");
    assert_eq!(next.credential.access, "rotated-access");
    // Everything the test needed has been observed: end the extra-request watch.
    extra_watch.stop();
    let _ = server.join();
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[derive(Clone, Copy)]
struct NoopBrowser;

#[tokio::test]
async fn short_lived_refresh_is_reused_but_expiry_still_forces_refresh() {
    let root = std::env::temp_dir().join(format!(
        "slim-short-refresh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = OAuthCredential {
        access: "old".into(),
        refresh: "refresh".into(),
        expires: 1,
        account_id: None,
    };
    store.save(OAuthProvider::Anthropic, &expired).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let server_hits = hits.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            server_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = r#"{"access_token":"short-access","refresh_token":"short-refresh","expires_in":240}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let service = OAuthService::new(
        OAuthEndpoints {
            anthropic_token: format!("http://{address}"),
            ..OAuthEndpoints::default()
        },
        Arc::new(NoopBrowser),
        store.clone(),
    )
    .unwrap();
    let (first, second) = tokio::join!(
        service.fresh_credential(OAuthProvider::Anthropic, expired.clone()),
        service.fresh_credential(OAuthProvider::Anthropic, expired.clone())
    );
    let first = first.unwrap().credential;
    assert_eq!(first, second.unwrap().credential);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        (230_000..=240_000).contains(&first.expires.saturating_sub(now)),
        "server validity must not lose five minutes"
    );
    for _ in 0..3 {
        assert_eq!(
            service
                .fresh_credential(OAuthProvider::Anthropic, expired.clone())
                .await
                .unwrap()
                .credential,
            first
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    // A newly loaded expired credential must override the recent-refresh window.
    store.save(OAuthProvider::Anthropic, &expired).unwrap();
    assert_eq!(
        service
            .fresh_credential(OAuthProvider::Anthropic, expired)
            .await
            .unwrap()
            .credential
            .access,
        "short-access"
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_services_sharing_a_store_refresh_one_base_once() {
    let root = temp_oauth_root("distinct-services");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = expired_fixture_credential("shared-expired");
    store.save(OAuthProvider::Anthropic, &expired).unwrap();

    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let extra_watch = ExtraConnectionWatch::new();
    let stop_flag = extra_watch.flag();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        let request = read_http_request(&mut stream);
        assert!(request.contains("shared-expired"));
        // Keep the first POST in flight so the second service really contends.
        thread::sleep(Duration::from_millis(100));
        write_refresh_response(&mut stream, "shared-access", "shared-refresh", 3600);
        count_connections_until(&listener, &stop_flag)
    });

    let endpoint = format!("http://{address}");
    let first = oauth_service(endpoint.clone(), store.clone());
    let second = oauth_service(endpoint, store.clone());
    let (one, two) = tokio::join!(
        first.fresh_credential(OAuthProvider::Anthropic, expired.clone()),
        second.fresh_credential(OAuthProvider::Anthropic, expired),
    );
    extra_watch.stop();
    let extra_requests = server.join().unwrap();
    assert_eq!(one.unwrap().credential.access, "shared-access");
    assert_eq!(two.unwrap().credential.access, "shared-access");
    assert_eq!(
        extra_requests, 0,
        "independent services must share refresh ownership"
    );
    assert_eq!(
        store
            .credential(OAuthProvider::Anthropic)
            .unwrap()
            .unwrap()
            .refresh,
        "shared-refresh"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_ownership_isolated_by_store_and_provider() {
    let root = temp_oauth_root("isolation");
    std::fs::create_dir_all(&root).unwrap();
    let store_one = OAuthStore::at(root.join("one.json"));
    let store_two = OAuthStore::at(root.join("two.json"));
    let anthropic = expired_fixture_credential("anthropic-base");
    let codex = expired_fixture_credential("codex-base");
    store_one
        .save(OAuthProvider::Anthropic, &anthropic)
        .unwrap();
    store_one.save(OAuthProvider::OpenAiCodex, &codex).unwrap();
    store_two
        .save(
            OAuthProvider::Anthropic,
            &expired_fixture_credential("other-store-base"),
        )
        .unwrap();

    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let codex_access = codex_fixture_access();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..3 {
            let (mut stream, _) = accept_fixture(&listener);
            let request = read_http_request(&mut stream);
            let path = request.lines().next().unwrap().to_owned();
            requests.push((stream, request, path));
        }
        let mut outcomes = Vec::new();
        for (mut stream, request, path) in requests {
            let (access, refresh) = if path.contains("/codex") {
                (codex_access.as_str(), "codex-next")
            } else if request.contains("other-store-base") {
                ("other-store-access", "other-store-next")
            } else {
                ("anthropic-access", "anthropic-next")
            };
            outcomes.push((path, access.to_owned()));
            write_refresh_response(&mut stream, access, refresh, 3600);
        }
        outcomes
    });

    let origin = format!("http://{address}");
    let endpoints = OAuthEndpoints {
        anthropic_token: format!("{origin}/anthropic"),
        codex_token: format!("{origin}/codex"),
        ..OAuthEndpoints::default()
    };
    let first =
        OAuthService::new(endpoints.clone(), Arc::new(NoopBrowser), store_one.clone()).unwrap();
    let second = oauth_service(format!("{origin}/anthropic"), store_two.clone());
    let (anthropic_result, codex_result, other_store_result) = tokio::join!(
        first.fresh_credential(OAuthProvider::Anthropic, anthropic),
        first.fresh_credential(OAuthProvider::OpenAiCodex, codex),
        second.fresh_credential(
            OAuthProvider::Anthropic,
            expired_fixture_credential("other-store-base")
        ),
    );
    let requests = server.join().unwrap();
    assert_eq!(
        anthropic_result.unwrap().credential.access,
        "anthropic-access"
    );
    assert_eq!(
        codex_result.unwrap().credential.access,
        codex_fixture_access()
    );
    assert_eq!(
        other_store_result.unwrap().credential.access,
        "other-store-access"
    );
    assert!(requests.iter().any(|(path, _)| path.contains("/codex")));
    assert_eq!(
        store_two
            .credential(OAuthProvider::Anthropic)
            .unwrap()
            .unwrap()
            .access,
        "other-store-access"
    );
    assert_eq!(
        store_one
            .credential(OAuthProvider::Anthropic)
            .unwrap()
            .unwrap()
            .access,
        "anthropic-access"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_change_supersedes_an_in_flight_oauth_refresh() {
    let root = temp_oauth_root("api-key-cas");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = expired_fixture_credential("api-key-base");
    store.save(OAuthProvider::Xai, &expired).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("loopback OAuth refresh request")
            .unwrap();
        let request = read_http_request_async(&mut stream).await;
        assert!(request.contains("api-key-base"));
        received_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let body = r#"{"access_token":"late-xai-access","refresh_token":"late-xai-refresh","expires_in":3600}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let endpoints = OAuthEndpoints {
        xai_token: format!("http://{address}"),
        ..OAuthEndpoints::default()
    };
    let service = OAuthService::new(endpoints, Arc::new(NoopBrowser), store.clone()).unwrap();
    let request = service
        .request_fresh_credential(OAuthProvider::Xai, expired)
        .unwrap();
    let waiter = tokio::spawn(request.wait());
    received_rx.await.unwrap();
    OAuthStore::at(store.path().to_path_buf())
        .save_api_key("xai", "chosen-api-key-fixture")
        .unwrap();
    release_tx.send(()).unwrap();
    let _ = waiter.await;
    server.await.unwrap();

    let document = std::fs::read_to_string(store.path()).unwrap();
    assert!(document.contains("chosen-api-key-fixture"));
    assert!(!document.contains("late-xai-access"));
    assert_eq!(store.active_provider_key().unwrap().as_deref(), Some("xai"));
    assert!(service.shutdown().await.is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_oauth_login_supersedes_a_late_refresh_response() {
    let root = temp_oauth_root("external-login-cas");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = expired_fixture_credential("external-login-base");
    store.save(OAuthProvider::Anthropic, &expired).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("loopback OAuth refresh request")
            .unwrap();
        let request = read_http_request_async(&mut stream).await;
        assert!(request.contains("external-login-base"));
        received_tx.send(()).unwrap();
        release_rx.await.unwrap();
        write_refresh_response_async(&mut stream, "stale-access", "stale-refresh", 3600).await;
    });
    let service = oauth_service(format!("http://{address}"), store.clone());
    let request = service
        .request_fresh_credential(OAuthProvider::Anthropic, expired)
        .unwrap();
    let waiter = tokio::spawn(request.wait());
    received_rx.await.unwrap();
    let external = OAuthCredential {
        access: "external-access-fixture".into(),
        refresh: "external-refresh-fixture".into(),
        expires: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600,
        account_id: Some("external-account-fixture".into()),
    };
    OAuthStore::at(store.path().to_path_buf())
        .save(OAuthProvider::Anthropic, &external)
        .unwrap();
    release_tx.send(()).unwrap();
    match waiter.await.unwrap() {
        Ok(result) => assert_eq!(
            result.credential, external,
            "late refresh must not supersede external login"
        ),
        Err(error) => assert!(matches!(
            error,
            OAuthError::CredentialsChanged | OAuthError::Store(_)
        )),
    }
    server.await.unwrap();
    assert_eq!(
        store.credential(OAuthProvider::Anthropic).unwrap(),
        Some(external)
    );
    assert_eq!(service.shutdown().await.len(), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn refresh_http_error_releases_ownership_for_a_later_attempt() {
    let root = temp_oauth_root("http-error-release");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = expired_fixture_credential("retry-after-error");
    store.save(OAuthProvider::Anthropic, &expired).unwrap();
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for attempt in 0..2 {
            let (mut stream, _) = accept_fixture(&listener);
            assert!(read_http_request(&mut stream).contains("retry-after-error"));
            if attempt == 0 {
                let body = r#"{"error":"invalid_grant"}"#;
                stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
            } else {
                write_refresh_response(&mut stream, "retry-access", "retry-next", 3600);
            }
        }
    });
    let service = oauth_service(format!("http://{address}"), store.clone());
    assert!(service
        .fresh_credential(OAuthProvider::Anthropic, expired.clone())
        .await
        .is_err());
    let recovered = service
        .fresh_credential(OAuthProvider::Anthropic, expired)
        .await
        .expect("a failed request must release ownership");
    server.join().unwrap();
    assert_eq!(recovered.credential.access, "retry-access");
    assert_eq!(
        store
            .credential(OAuthProvider::Anthropic)
            .unwrap()
            .unwrap()
            .refresh,
        "retry-next"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn refresh_preserves_account_identity_and_keeps_secrets_out_of_diagnostics() {
    let root = temp_oauth_root("account-preservation");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = OAuthCredential {
        access: "old-access-secret-fixture".into(),
        refresh: "old-refresh-secret-fixture".into(),
        expires: 1,
        account_id: Some("account-preserved-fixture".into()),
    };
    store.save(OAuthProvider::Anthropic, &expired).unwrap();
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        assert!(read_http_request(&mut stream).contains("old-refresh-secret-fixture"));
        write_refresh_response(
            &mut stream,
            "new-access-secret-fixture",
            "new-refresh-secret-fixture",
            240,
        );
    });
    let service = oauth_service(format!("http://{address}"), store.clone());
    let fresh = service
        .request_fresh_credential(OAuthProvider::Anthropic, expired)
        .unwrap()
        .wait()
        .await
        .unwrap();
    server.join().unwrap();
    assert_eq!(
        fresh.credential.account_id.as_deref(),
        Some("account-preserved-fixture")
    );
    assert_eq!(fresh.credential.access, "new-access-secret-fixture");
    let warnings = service.shutdown().await;
    let diagnostics = format!("{warnings:?} {fresh:?}");
    assert!(!diagnostics.contains("new-access-secret-fixture"));
    assert!(!diagnostics.contains("new-refresh-secret-fixture"));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn unreadable_store_and_expired_fallback_fail_closed_without_refreshing() {
    let root = temp_oauth_root("unreadable-expired");
    std::fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    std::fs::write(&auth_path, b"not-json").unwrap();
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let extra_watch = ExtraConnectionWatch::new();
    let stop_flag = extra_watch.flag();
    let server = thread::spawn(move || {
        loop {
            let stopping = stop_flag.load(std::sync::atomic::Ordering::SeqCst);
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = read_http_request(&mut stream);
                    let body = r#"{"error":"fixture-reject"}"#;
                    let _ = stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // Raised only after the service has already failed and shut down.
                    if stopping {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("loopback listener failed: {error}"),
            }
        }
    });
    let service = oauth_service(format!("http://{address}"), OAuthStore::at(&auth_path));
    let error = service
        .fresh_credential(
            OAuthProvider::Anthropic,
            expired_fixture_credential("synthetic-expired-fallback"),
        )
        .await
        .expect_err("unreadable store must not downgrade to the supplied expired token");
    assert!(matches!(
        error,
        OAuthError::Store(_) | OAuthError::CredentialsChanged
    ));
    // Drain any supervised task, then end the watch: a refresh POST issued at
    // any point before this would already sit in the listener backlog.
    let _ = service.shutdown().await;
    extra_watch.stop();
    assert!(
        !server.join().unwrap(),
        "malformed auth must fail before any refresh POST"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_windows_processes_share_refresh_ownership_for_one_store() {
    const CHILD: &str = "SLIM_OAUTH_R2_PROCESS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let root = std::path::PathBuf::from(std::env::var_os("SLIM_OAUTH_R2_STORE").unwrap());
        let endpoint = std::env::var("SLIM_OAUTH_R2_ENDPOINT").unwrap();
        let service = oauth_service(endpoint, OAuthStore::at(root.join("auth.json")));
        let result = service
            .fresh_credential(
                OAuthProvider::Anthropic,
                expired_fixture_credential("process-shared-base"),
            )
            .await
            .unwrap();
        assert_eq!(result.credential.access, "process-shared-access");
        return;
    }

    let root = temp_oauth_root("two-processes");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    store
        .save(
            OAuthProvider::Anthropic,
            &expired_fixture_credential("process-shared-base"),
        )
        .unwrap();
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let extra_watch = ExtraConnectionWatch::new();
    let stop_flag = extra_watch.flag();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        assert!(read_http_request(&mut stream).contains("process-shared-base"));
        // Keep the first POST in flight so the second process really contends.
        thread::sleep(Duration::from_millis(150));
        write_refresh_response(
            &mut stream,
            "process-shared-access",
            "process-shared-next",
            3600,
        );
        count_connections_until(&listener, &stop_flag)
    });

    let current_exe = std::env::current_exe().unwrap();
    let endpoint = format!("http://{address}");
    let start_child = || {
        std::process::Command::new(&current_exe)
            .arg("--exact")
            .arg("two_windows_processes_share_refresh_ownership_for_one_store")
            .arg("--nocapture")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env(CHILD, "1")
            .env("SLIM_OAUTH_R2_STORE", &root)
            .env("SLIM_OAUTH_R2_ENDPOINT", &endpoint)
            .spawn()
            .unwrap()
    };
    let first = start_child();
    let second = start_child();
    let first_output = first.wait_with_output().unwrap();
    let second_output = second.wait_with_output().unwrap();
    // Both processes have exited: any second POST would already be queued.
    extra_watch.stop();
    assert!(
        first_output.status.success(),
        "{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert!(
        second_output.status.success(),
        "{}",
        String::from_utf8_lossy(&second_output.stderr)
    );
    assert_eq!(
        server.join().unwrap(),
        0,
        "only one process may POST this refresh base"
    );
    assert_eq!(
        store
            .credential(OAuthProvider::Anthropic)
            .unwrap()
            .unwrap()
            .refresh,
        "process-shared-next"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_process_restart_after_refresh_post_reuses_consumed_base() {
    const MODE: &str = "SLIM_OAUTH_R2_CRASH_CHILD";
    if let Ok(mode) = std::env::var(MODE) {
        let root = std::path::PathBuf::from(std::env::var_os("SLIM_OAUTH_R2_CRASH_STORE").unwrap());
        let endpoint = std::env::var("SLIM_OAUTH_R2_CRASH_ENDPOINT").unwrap();
        let service = oauth_service(endpoint, OAuthStore::at(root.join("auth.json")));
        let result = service
            .fresh_credential(
                OAuthProvider::Anthropic,
                expired_fixture_credential("crash-consumed-base"),
            )
            .await;
        if mode == "successor" {
            match result {
                Err(OAuthError::InvalidResponse(message)) => {
                    assert_eq!(message, "Anthropic OAuth failed (400)");
                }
                other => panic!("successor must report an explicit OAuth rejection: {other:?}"),
            }
        } else {
            panic!("crash-owner must remain blocked until the harness terminates it: {result:?}");
        }
        return;
    }

    let root = temp_oauth_root("post-crash");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    store
        .save(
            OAuthProvider::Anthropic,
            &expired_fixture_credential("crash-consumed-base"),
        )
        .unwrap();

    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (first_post_tx, first_post_rx) = std::sync::mpsc::channel::<()>();
    let (owner_killed_tx, owner_killed_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut first_stream, _) = accept_fixture(&listener);
        let first_request = read_http_request(&mut first_stream);
        // Accepting this request consumes and rotates the synthetic base; the
        // fixture intentionally withholds the response until the harness kills A.
        first_post_tx.send(()).unwrap();
        owner_killed_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("harness terminates the owner after its POST is observed");
        drop(first_stream);

        let (mut second_stream, _) = accept_fixture(&listener);
        let second_request = read_http_request(&mut second_stream);
        let body = r#"{"error":"invalid_grant"}"#;
        second_stream
            .write_all(
                format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        (first_request, second_request)
    });

    let current_exe = std::env::current_exe().unwrap();
    let endpoint = format!("http://{address}");
    let start_child = |mode: &str| {
        std::process::Command::new(&current_exe)
            .arg("--exact")
            .arg("windows_process_restart_after_refresh_post_reuses_consumed_base")
            .arg("--nocapture")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env(MODE, mode)
            .env("SLIM_OAUTH_R2_CRASH_STORE", &root)
            .env("SLIM_OAUTH_R2_CRASH_ENDPOINT", &endpoint)
            .spawn()
    };

    let mut owner = start_child("owner").expect("spawn crash-owner child");
    match first_post_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(()) => {}
        Err(error) => {
            let _ = owner.kill();
            let _ = owner.wait();
            let _ = owner_killed_tx.send(());
            let _ = server.join();
            let _ = std::fs::remove_dir_all(&root);
            panic!("crash-owner did not POST before the deadline: {error}");
        }
    }
    let owner_kill_result = owner.kill();
    let owner_status = owner.wait();
    let _ = owner_killed_tx.send(());

    let mut successor = match start_child("successor") {
        Ok(child) => child,
        Err(error) => {
            let _ = server.join();
            let _ = std::fs::remove_dir_all(&root);
            panic!("spawn successor child: {error}");
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut successor_status = None;
    let mut successor_poll_error = None;
    while std::time::Instant::now() < deadline {
        match successor.try_wait() {
            Ok(Some(status)) => {
                successor_status = Some(status);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                successor_poll_error = Some(error.to_string());
                break;
            }
        }
    }
    let mut successor_cleanup_errors = Vec::new();
    if successor_status.is_none() {
        if let Err(error) = successor.kill() {
            successor_cleanup_errors.push(format!("kill successor: {error}"));
        }
        match successor.wait() {
            Ok(status) => successor_status = Some(status),
            Err(error) => successor_cleanup_errors.push(format!("wait for successor: {error}")),
        }
    }
    let successor_output = successor.wait_with_output();
    let requests = server.join();
    let _ = std::fs::remove_dir_all(&root);

    let (first_request, second_request) = requests.expect("loopback fixture must finish");
    assert!(
        successor_poll_error.is_none(),
        "polling successor must succeed: {successor_poll_error:?}"
    );
    assert!(
        successor_cleanup_errors.is_empty(),
        "successor cleanup must succeed: {successor_cleanup_errors:?}"
    );
    let first_body = first_request.split_once("\r\n\r\n").unwrap().1;
    let second_body = second_request.split_once("\r\n\r\n").unwrap().1;
    assert!(
        owner_kill_result.is_ok(),
        "owner process must be terminated"
    );
    assert!(
        !owner_status.unwrap().success(),
        "owner must be killed after POST"
    );
    assert!(
        first_body.contains("crash-consumed-base"),
        "the blocked owner POST must use the fixture base"
    );
    assert!(
        second_body.contains("crash-consumed-base"),
        "the successor must explicitly retry the consumed base"
    );
    let successor_output = successor_output.expect("collect successor output");
    assert!(
        successor_status.is_some_and(|status| status.success()),
        "successor must exit after recognizing invalid_grant: {}",
        String::from_utf8_lossy(&successor_output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_wins_against_a_refresh_response_already_in_flight() {
    let root = temp_oauth_root("logout-cas");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = expired_fixture_credential("logout-base");
    store.save(OAuthProvider::Anthropic, &expired).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("loopback OAuth refresh request")
            .unwrap();
        let request = read_http_request_async(&mut stream).await;
        assert!(request.contains("logout-base"));
        received_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let body =
            r#"{"access_token":"late-access","refresh_token":"late-refresh","expires_in":3600}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let service = oauth_service(format!("http://{address}"), store.clone());
    let request = service
        .request_fresh_credential(OAuthProvider::Anthropic, expired)
        .unwrap();
    let waiter = tokio::spawn(request.wait());
    received_rx.await.unwrap();
    service.logout(OAuthProvider::Anthropic).unwrap();
    release_tx.send(()).unwrap();
    let _ = waiter.await;
    server.await.unwrap();
    assert_eq!(store.credential(OAuthProvider::Anthropic).unwrap(), None);
    assert_eq!(store.active().unwrap(), None);
    assert!(service.shutdown().await.is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_waiter_after_send_does_not_cancel_the_shared_owner() {
    let root = temp_oauth_root("owner-after-waiter");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = expired_fixture_credential("owner-base");
    store.save(OAuthProvider::Anthropic, &expired).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("loopback OAuth refresh request")
            .unwrap();
        let mut request = vec![0; 16 * 1024];
        let size = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..size]).contains("owner-base"));
        received_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let body =
            r#"{"access_token":"owner-access","refresh_token":"owner-refresh","expires_in":3600}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let service = oauth_service(format!("http://{address}"), store.clone());
    let request = service
        .request_fresh_credential(OAuthProvider::Anthropic, expired)
        .unwrap();
    let waiter = tokio::spawn(request.wait());
    received_rx.await.unwrap();
    waiter.abort();
    release_tx.send(()).unwrap();
    server.await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if store
            .credential(OAuthProvider::Anthropic)
            .unwrap()
            .is_some_and(|credential| credential.access == "owner-access")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owner must persist after waiter drop"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(service.shutdown().await.is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn dropping_an_unpolled_request_prevents_refresh_send() {
    let root = temp_oauth_root("cancel-before-send");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let expired = expired_fixture_credential("cancel-base");
    store.save(OAuthProvider::Anthropic, &expired).unwrap();
    let before = std::fs::read(store.path()).unwrap();

    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let service = oauth_service(format!("http://{address}"), store.clone());
    let request = service
        .request_fresh_credential(OAuthProvider::Anthropic, expired)
        .unwrap();
    drop(request);
    // Shutdown joins every supervised task, so a dispatched POST would already
    // be queued in the loopback listener; no grace period is needed.
    assert!(service.shutdown().await.is_empty());
    assert!(
        matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn auth_file_legacy_preference_defaults_to_oauth_and_rejects_unknown_method() {
    let root = temp_oauth_root("preferred-method-legacy");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("auth.json");
    std::fs::write(
        &path,
        r#"{"version":1,"providers":{"anthropic":{"oauth":{"access":"legacy-oauth-fixture","refresh":"legacy-refresh-fixture","expires":4102444800,"account_id":"legacy-account-fixture"},"api_key":"legacy-api-key-fixture"}}}"#,
    )
    .unwrap();

    let legacy =
        slim_cli::load_auth_credential(&path, slim_core::provider::ProviderKind::Anthropic)
            .unwrap()
            .expect("legacy provider credential");
    assert!(
        legacy.oauth,
        "legacy files prefer OAuth when the method is absent"
    );
    assert_eq!(legacy.access, "legacy-oauth-fixture");
    assert_eq!(legacy.account_id.as_deref(), Some("legacy-account-fixture"));

    std::fs::write(
        &path,
        r#"{"version":1,"providers":{"anthropic":{"preferred_method":"future_method","oauth":{"access":"legacy-oauth-fixture","refresh":"legacy-refresh-fixture","expires":4102444800},"api_key":"legacy-api-key-fixture"}}}"#,
    )
    .unwrap();
    assert!(matches!(
        slim_cli::load_auth_credential(&path, slim_core::provider::ProviderKind::Anthropic),
        Err(slim_cli::AuthError::InvalidSchema)
    ));

    std::fs::write(
        &path,
        r#"{"version":1,"providers":{"anthropic":{"preferred_method":null,"oauth":{"access":"legacy-oauth-fixture","refresh":"legacy-refresh-fixture","expires":4102444800},"api_key":"legacy-api-key-fixture"}}}"#,
    )
    .unwrap();
    assert!(matches!(
        slim_cli::load_auth_credential(&path, slim_core::provider::ProviderKind::Anthropic),
        Err(slim_cli::AuthError::InvalidSchema)
    ));
    assert!(
        OAuthStore::at(&path).active().is_err(),
        "OAuthStore must reject null preferred_method too"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn auth_api_key_save_and_delete_preserve_oauth_credentials() {
    let root = temp_oauth_root("auth-key-preserves-oauth");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("auth.json");
    let provider = slim_core::provider::ProviderKind::Anthropic;
    let store = OAuthStore::at(&path);
    let oauth = OAuthCredential {
        access: "auth-preserved-oauth-access-fixture".into(),
        refresh: "auth-preserved-oauth-refresh-fixture".into(),
        expires: 4102444800,
        account_id: Some("auth-preserved-account-fixture".into()),
    };
    store.save(OAuthProvider::Anthropic, &oauth).unwrap();

    slim_cli::save_api_key_file(&path, provider, "saved-api-key-fixture").unwrap();
    let after_save: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        after_save["providers"]["anthropic"]["oauth"]["access"],
        "auth-preserved-oauth-access-fixture"
    );
    assert_eq!(
        after_save["providers"]["anthropic"]["api_key"],
        "saved-api-key-fixture"
    );

    store.save(OAuthProvider::Anthropic, &oauth).unwrap();
    slim_cli::delete_api_key_file(&path, provider).unwrap();
    let after_delete: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        after_delete["providers"]["anthropic"]["oauth"]["access"],
        "auth-preserved-oauth-access-fixture"
    );
    assert!(after_delete["providers"]["anthropic"]["api_key"].is_null());
    let selected = slim_cli::load_auth_credential(&path, provider)
        .unwrap()
        .expect("OAuth remains selected after deleting the API key");
    assert!(selected.oauth);
    assert_eq!(selected.access, "auth-preserved-oauth-access-fixture");
    let _ = std::fs::remove_dir_all(root);
}

fn temp_oauth_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "slim-oauth-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn expired_fixture_credential(refresh: &str) -> OAuthCredential {
    OAuthCredential {
        access: "expired-access-fixture".into(),
        refresh: refresh.into(),
        expires: 1,
        account_id: Some("account-fixture".into()),
    }
}

fn codex_fixture_access() -> String {
    format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"codex-account-fixture"}}"#
        )
    )
}

fn oauth_service(endpoint: String, store: OAuthStore) -> OAuthService {
    OAuthService::new(
        OAuthEndpoints {
            anthropic_token: endpoint,
            ..OAuthEndpoints::default()
        },
        Arc::new(NoopBrowser),
        store,
    )
    .unwrap()
}

/// Ends a fixture's "no extra request" watch. It replaces a fixed grace sleep:
/// the test raises it once it has observed everything it needs, and dropping it
/// (for example on a failed assertion) also releases the fixture thread.
struct ExtraConnectionWatch(Arc<std::sync::atomic::AtomicBool>);

impl ExtraConnectionWatch {
    fn new() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }

    fn flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.0)
    }

    fn stop(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for ExtraConnectionWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Counts connections accepted until `stop` is raised. A connection made before
/// the flag is raised is counted even if it is still queued in the loopback
/// backlog, because the flag is read before each accept attempt.
fn count_connections_until(
    listener: &StdTcpListener,
    stop: &std::sync::atomic::AtomicBool,
) -> usize {
    listener
        .set_nonblocking(true)
        .expect("nonblocking extra accept");
    let mut count = 0;
    loop {
        let stopping = stop.load(std::sync::atomic::Ordering::SeqCst);
        match listener.accept() {
            Ok(_) => count += 1,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stopping {
                    return count;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("loopback listener failed: {error}"),
        }
    }
}

fn accept_fixture(listener: &StdTcpListener) -> (StdTcpStream, SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                stream
                    .set_nonblocking(false)
                    .expect("fixture stream blocking mode");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("fixture stream read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("fixture stream write timeout");
                return (stream, address);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "loopback OAuth request timed out"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("loopback listener accept failed: {error}"),
        }
    }
}

fn read_http_request(stream: &mut StdTcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "fixture request ended before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..end]);
            let body_len = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + body_len {
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}

async fn read_http_request_async(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "fixture request ended before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..end]);
            let body_len = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + body_len {
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn write_refresh_response(stream: &mut StdTcpStream, access: &str, refresh: &str, expires: u32) {
    let body = format!(
        r#"{{"access_token":"{access}","refresh_token":"{refresh}","expires_in":{expires}}}"#
    );
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
}

async fn write_refresh_response_async(
    stream: &mut TcpStream,
    access: &str,
    refresh: &str,
    expires: u32,
) {
    let body = format!(
        r#"{{"access_token":"{access}","refresh_token":"{refresh}","expires_in":{expires}}}"#
    );
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

impl BrowserLauncher for NoopBrowser {
    fn open(&self, _url: &str) -> Result<(), OAuthError> {
        Ok(())
    }
}

fn xai_service(
    auth_path: &std::path::Path,
    device_code_url: String,
    token_url: String,
) -> OAuthService {
    OAuthService::new(
        OAuthEndpoints {
            xai_device_code: device_code_url,
            xai_token: token_url,
            ..OAuthEndpoints::default()
        },
        Arc::new(NoopBrowser),
        OAuthStore::at(auth_path),
    )
    .expect("xai service")
}

async fn serve_scripted(
    listener: TcpListener,
    bodies: Vec<(u16, String)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for (status, body) in bodies {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = vec![0_u8; 8192];
            let _ = stream.read(&mut request).await;
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    })
}

fn xai_temp_auth(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "slim-xai-oauth-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("root");
    let auth_path = root.join("auth.json");
    (root, auth_path)
}

#[tokio::test]
async fn xai_device_login_polls_then_stores_oauth_credential() {
    let (root, auth_path) = xai_temp_auth("login");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let base = format!("http://{address}");
    let server = serve_scripted(
        listener,
        vec![
            (200, r#"{"device_code":"device-1","user_code":"ABCD-1234","verification_uri":"https://auth.x.ai/device","verification_uri_complete":"https://auth.x.ai/device?code=ABCD-1234","interval":1,"expires_in":600}"#.into()),
            (400, r#"{"error":"authorization_pending"}"#.into()),
            (200, r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600}"#.into()),
        ],
    )
    .await;
    let service = xai_service(
        &auth_path,
        format!("{base}/device/code"),
        format!("{base}/token"),
    );
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let credential = service
        .login(OAuthProvider::Xai, progress_tx, cancel_rx)
        .await
        .expect("xai login");
    assert_eq!(credential.access, "access-1");
    assert_eq!(credential.refresh, "refresh-1");
    assert_eq!(credential.account_id, None);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let skew = now + 3_300_000 - credential.expires;
    assert!(skew < 120_000, "5-minute refresh skew is applied: {skew}");
    match progress_rx.recv().await.expect("progress") {
        OAuthProgress::AuthUrl { user_code, .. } => {
            assert_eq!(user_code.as_deref(), Some("ABCD-1234"))
        }
        other => panic!("device code progress expected, got {other:?}"),
    }
    let stored = OAuthStore::at(&auth_path)
        .credential(OAuthProvider::Xai)
        .expect("stored")
        .expect("credential");
    assert_eq!(stored.access, "access-1");
    let _ = server.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn xai_device_login_denied_is_a_clean_error() {
    let (root, auth_path) = xai_temp_auth("denied");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let base = format!("http://{address}");
    let server = serve_scripted(
        listener,
        vec![
            (200, r#"{"device_code":"device-1","user_code":"ABCD-1234","verification_uri":"https://auth.x.ai/device","interval":1,"expires_in":600}"#.into()),
            (400, r#"{"error":"access_denied"}"#.into()),
        ],
    )
    .await;
    let service = xai_service(
        &auth_path,
        format!("{base}/device/code"),
        format!("{base}/token"),
    );
    let (progress_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let error = service
        .login(OAuthProvider::Xai, progress_tx, cancel_rx)
        .await
        .expect_err("denied login fails");
    assert!(error.to_string().contains("denied"), "{error}");
    let _ = server.await;
    let _ = std::fs::remove_dir_all(root);
}
