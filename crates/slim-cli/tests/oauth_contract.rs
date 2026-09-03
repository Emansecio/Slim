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
    assert!(
        !saved.contains("existing-key"),
        "OAuth save must drop the leftover api_key: {saved}"
    );
    store.remove(OAuthProvider::OpenAiCodex).expect("remove");
    assert_eq!(store.active().expect("active"), None);
    assert!(!std::fs::read_to_string(auth_path)
        .expect("auth text")
        .contains("existing-key"));
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
    let server = thread::spawn(move || {
        listener.set_nonblocking(false).expect("blocking accept");
        let (mut stream, _) = listener.accept().expect("refresh accept");
        server_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).expect("refresh request");
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
        listener
            .set_nonblocking(true)
            .expect("nonblocking extra accept");
        thread::sleep(Duration::from_millis(150));
        if listener.accept().is_ok() {
            server_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
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
    let _ = server.join();
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(root);
}
