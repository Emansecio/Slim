//! MCP OAuth in the CLI: the credential file, the refresh lock and the
//! interactive sign-in against a loopback mock authorization server. The
//! user's browser is a fake that performs the redirect itself.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::oauth_mock as mock;
use mock::World;
use slim_core::mcp::{
    McpAuth, McpOAuthSpec, McpOAuthState, McpOAuthStore, OAuthClient, OAuthTokens,
};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::oauth::OAuthError;

fn temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "slim-mcp-oauth-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn state_for(url: &str, access: &str) -> McpOAuthState {
    McpOAuthState {
        server_url: url.to_owned(),
        client: Some(OAuthClient {
            client_id: "cid".into(),
            client_secret: Some("client-secret-value".into()),
            redirect_uris: vec!["http://127.0.0.1:1/callback".into()],
            token_endpoint_auth_method: None,
        }),
        tokens: Some(OAuthTokens {
            access_token: access.into(),
            refresh_token: Some(format!("{access}-refresh")),
            scope: Some("read".into()),
            expires_at_ms: Some(4_102_444_800_000),
        }),
        ..McpOAuthState::default()
    }
}

// ---------------------------------------------------------------------------
// Credential store
// ---------------------------------------------------------------------------

#[test]
fn credentials_round_trip_and_are_keyed_by_server_name_and_url() {
    let dir = temp_dir("store");
    let path = dir.join("mcp-auth.json");
    let a = FileOAuthStore::at(&path, "alpha", "https://one.example/mcp");
    let b = FileOAuthStore::at(&path, "beta", "https://one.example/mcp");
    let a_other_url = FileOAuthStore::at(&path, "alpha", "https://two.example/mcp");
    assert_eq!(a.load().unwrap(), None);

    a.save(&state_for("https://one.example/mcp", "tok-a"))
        .unwrap();
    b.save(&state_for("https://one.example/mcp", "tok-b"))
        .unwrap();
    // Same name, other URL: another account.
    assert_eq!(a_other_url.load().unwrap(), None);
    assert_eq!(
        a.load().unwrap().unwrap().tokens.unwrap().access_token,
        "tok-a"
    );
    assert_eq!(
        b.load().unwrap().unwrap().tokens.unwrap().access_token,
        "tok-b"
    );
    // The document is versioned and holds one entry per name|url key.
    let document = read_secure_json(&path).unwrap().unwrap();
    assert_eq!(document["version"], 1);
    let servers = document["servers"].as_object().unwrap();
    assert!(servers.contains_key("alpha|https://one.example/mcp"));
    assert!(servers.contains_key("beta|https://one.example/mcp"));
    assert_eq!(servers.len(), 2);

    // Updating keeps the other entries.
    a.save(&state_for("https://one.example/mcp", "tok-a2"))
        .unwrap();
    assert_eq!(
        b.load().unwrap().unwrap().tokens.unwrap().access_token,
        "tok-b"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn clearing_removes_only_that_servers_entry() {
    let dir = temp_dir("clear");
    let path = dir.join("mcp-auth.json");
    let a = FileOAuthStore::at(&path, "alpha", "https://one.example/mcp");
    let b = FileOAuthStore::at(&path, "beta", "https://one.example/mcp");
    assert!(!a.clear().unwrap(), "nothing stored yet");
    assert!(!path.exists(), "clearing does not create the file");
    a.save(&state_for("https://one.example/mcp", "tok-a"))
        .unwrap();
    b.save(&state_for("https://one.example/mcp", "tok-b"))
        .unwrap();
    assert!(a.clear().unwrap());
    assert!(!a.clear().unwrap());
    assert_eq!(a.load().unwrap(), None);
    assert!(b.load().unwrap().is_some());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_foreign_or_corrupt_store_is_reported_and_never_overwritten() {
    let dir = temp_dir("foreign");
    let path = dir.join("mcp-auth.json");
    let store = FileOAuthStore::at(&path, "alpha", "https://one.example/mcp");
    store
        .save(&state_for("https://one.example/mcp", "tok"))
        .unwrap();
    update_secure_json(&path, |_| Ok(json!({"version": 9, "servers": {}}))).unwrap();
    let error = store.load().unwrap_err();
    assert!(error.contains("unsupported store version"), "{error}");
    let error = store
        .save(&state_for("https://one.example/mcp", "tok2"))
        .unwrap_err();
    assert!(error.contains("unsupported store version"), "{error}");
    let error = store.clear().unwrap_err();
    assert!(error.contains("unsupported store version"), "{error}");
    let document = read_secure_json(&path).unwrap().unwrap();
    assert_eq!(document["version"], 9, "the store must not be rewritten");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_unresolvable_store_location_fails_every_operation() {
    let store = FileOAuthStore {
        path: Err("no config directory".into()),
        key: "x|y".into(),
    };
    assert_eq!(store.load().unwrap_err(), "no config directory");
    assert!(store.save(&McpOAuthState::default()).is_err());
    assert!(store.clear().is_err());
    assert!(store.lock_refresh().is_err());
}

#[test]
fn credentials_are_only_in_the_secure_file_never_in_debug_output() {
    let state = state_for("https://one.example/mcp", "tok-visible");
    let printed = format!("{state:?}");
    assert!(!printed.contains("tok-visible") && !printed.contains("client-secret-value"));
    let dir = temp_dir("secure");
    let path = dir.join("mcp-auth.json");
    let store = FileOAuthStore::at(&path, "alpha", "https://one.example/mcp");
    store.save(&state).unwrap();
    // `read_secure_json` verifies the owner-only ACL before reading.
    assert!(read_secure_json(&path).unwrap().is_some());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn canonical_urls_drop_the_fragment() {
    assert_eq!(
        canonical_url("https://a.example/mcp#frag").as_deref(),
        Some("https://a.example/mcp")
    );
    assert_eq!(canonical_url("not a url"), None);
}

#[test]
fn build_auth_needs_a_valid_http_url() {
    assert!(build_auth("s", "https://a.example/mcp", McpOAuthSpec::default()).is_some());
    assert!(build_auth("s", "ftp://a.example/mcp", McpOAuthSpec::default()).is_none());
    assert!(build_auth("s", "nonsense", McpOAuthSpec::default()).is_none());
}

// ---------------------------------------------------------------------------
// Refresh lock
// ---------------------------------------------------------------------------

#[test]
fn the_refresh_lock_excludes_other_holders_until_released() {
    let dir = temp_dir("lock");
    let path = dir.join("refresh.lock");
    let first = RefreshLock::acquire(&path, Duration::from_secs(1)).unwrap();
    // Held: a second acquirer times out.
    let error = RefreshLock::acquire(&path, Duration::from_millis(300))
        .err()
        .expect("held lock");
    assert!(error.contains("timed out"), "{error}");
    // Released while another waits: the waiter gets it.
    let (acquired, held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let waiter = {
        let path = path.clone();
        std::thread::spawn(move || {
            let lock = RefreshLock::acquire(&path, Duration::from_secs(5));
            let _ = acquired.send(lock.is_ok());
            let _ = released.recv();
            drop(lock);
        })
    };
    std::thread::sleep(Duration::from_millis(250));
    drop(first);
    assert!(
        held.recv_timeout(Duration::from_secs(5)).unwrap(),
        "the waiter acquired it after release"
    );
    // The first holder's drop did not release the waiter's lock.
    assert!(RefreshLock::acquire(&path, Duration::from_millis(200)).is_err());
    release.send(()).unwrap();
    waiter.join().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_lock_file_left_by_a_dead_holder_is_acquired_at_once() {
    let dir = temp_dir("leftover");
    let path = dir.join("refresh.lock");
    // What a killed process leaves behind: a file nobody holds the lock on.
    // There is no staleness window to wait out and nothing to take over.
    std::fs::write(&path, "999999\n").unwrap();
    let begun = Instant::now();
    let lock = RefreshLock::acquire(&path, Duration::from_millis(500)).expect("free lock");
    assert!(begun.elapsed() < Duration::from_millis(300));
    drop(lock);
    // The file stays: releasing never deletes a path another holder may own.
    assert!(path.exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_store_lock_is_per_server_and_blocks_a_second_refresh_of_the_same_server() {
    let dir = temp_dir("store-lock");
    let path = dir.join("mcp-auth.json");
    let a = FileOAuthStore::at(&path, "alpha", "https://one.example/mcp");
    let a2 = FileOAuthStore::at(&path, "alpha", "https://one.example/mcp");
    let b = FileOAuthStore::at(&path, "beta", "https://one.example/mcp");
    assert_ne!(a.refresh_lock_path(&path), b.refresh_lock_path(&path));
    assert_eq!(a.refresh_lock_path(&path), a2.refresh_lock_path(&path));
    let held = a.lock_refresh().unwrap();
    // Another server is not blocked; the same server is (checked without
    // waiting the full 25 s by using the lock primitive directly).
    let _other = b.lock_refresh().unwrap();
    assert!(
        RefreshLock::acquire(&a2.refresh_lock_path(&path), Duration::from_millis(200)).is_err()
    );
    drop(held);
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn pasted_redirect_urls_are_validated() {
    let ok = parse_redirect_input(
        "  http://127.0.0.1:5000/callback?code=abc&state=st&iss=https%3A%2F%2Fas.example  ",
        "st",
    )
    .unwrap();
    assert_eq!(ok.code, "abc");
    assert_eq!(ok.state.as_deref(), Some("st"));
    assert_eq!(ok.iss.as_deref(), Some("https://as.example"));
    assert!(parse_redirect_input("just-a-code", "st")
        .unwrap_err()
        .contains("full redirect URL"));
    assert!(
        parse_redirect_input("http://x/cb?code=abc&state=other", "st")
            .unwrap_err()
            .contains("different sign-in")
    );
    assert!(parse_redirect_input("http://x/cb?state=st", "st")
        .unwrap_err()
        .contains("authorization code"));
    let denied = parse_redirect_input(
        "http://x/cb?error=access_denied&error_description=No%20thanks&state=st",
        "st",
    )
    .unwrap_err();
    assert!(denied.contains("No thanks"), "{denied}");
}

#[test]
fn callback_pages_escape_server_provided_text() {
    assert_eq!(
        escape_html("<script>alert('x')</script>&\""),
        "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;&amp;&quot;"
    );
}

// ---------------------------------------------------------------------------
// Sign-in
// ---------------------------------------------------------------------------

/// Lines a fake browser records.
type Log = Arc<Mutex<Vec<String>>>;

/// What the fake browser does with the authorization URL.
#[derive(Clone)]
enum Visit {
    /// Follow the redirect with the code (after `stray` forged requests).
    Approve { stray: bool },
    /// Redirect back with an OAuth error.
    Deny(&'static str),
    /// Record the URL only: nothing reaches the callback.
    Ignore,
    /// Cannot open a browser.
    Unavailable,
}

struct FakeBrowser {
    visit: Visit,
    opened: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<Vec<String>>>,
}

impl FakeBrowser {
    fn new(visit: Visit) -> (Arc<Self>, Log, Log) {
        let opened: Arc<Mutex<Vec<String>>> = Arc::default();
        let responses: Arc<Mutex<Vec<String>>> = Arc::default();
        (
            Arc::new(Self {
                visit,
                opened: Arc::clone(&opened),
                responses: Arc::clone(&responses),
            }),
            opened,
            responses,
        )
    }
}

fn http_get(address: &str, target: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("callback listener");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .expect("request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

impl BrowserLauncher for FakeBrowser {
    fn open(&self, url: &str) -> Result<(), OAuthError> {
        self.opened.lock().unwrap().push(url.to_owned());
        if matches!(self.visit, Visit::Unavailable) {
            return Err(OAuthError::Browser);
        }
        let parsed = reqwest::Url::parse(url).expect("authorization URL");
        let query: std::collections::BTreeMap<String, String> =
            parsed.query_pairs().into_owned().collect();
        let redirect = reqwest::Url::parse(&query["redirect_uri"]).expect("redirect uri");
        let address = format!(
            "{}:{}",
            redirect.host_str().unwrap(),
            redirect.port().unwrap()
        );
        let state = query["state"].clone();
        let visit = self.visit.clone();
        let responses = Arc::clone(&self.responses);
        std::thread::spawn(move || {
            let record = |response: String| responses.lock().unwrap().push(response);
            match visit {
                Visit::Approve { stray } => {
                    if stray {
                        record(http_get(&address, "/callback?code=evil&state=forged"));
                        record(http_get(&address, "/elsewhere"));
                        record(http_get(&address, "/callback?code=evil"));
                    }
                    record(http_get(&address, &format!("/callback?code=code-1&state={state}")));
                }
                Visit::Deny(error) => record(http_get(
                    &address,
                    &format!(
                        "/callback?error={error}&error_description=%3Cb%3Edenied%3C%2Fb%3E&state={state}"
                    ),
                )),
                Visit::Ignore | Visit::Unavailable => {}
            }
        });
        Ok(())
    }
}

struct Session {
    world: World,
    dir: PathBuf,
    auth: Arc<McpAuth>,
    notices: Arc<Mutex<Vec<LoginNotice>>>,
}

impl Session {
    fn new(label: &str, settings: McpOAuthSpec) -> Self {
        let world = World::new();
        let dir = temp_dir(label);
        let url = canonical_url(&world.mcp_url()).unwrap();
        let store = Arc::new(FileOAuthStore::at(dir.join("mcp-auth.json"), "srv", &url));
        let auth = McpAuth::new("srv", &world.mcp_url(), settings, store).unwrap();
        Self {
            world,
            dir,
            auth,
            notices: Arc::default(),
        }
    }

    fn options(
        &self,
        browser: Arc<dyn BrowserLauncher>,
        timeout: Duration,
    ) -> (
        LoginOptions,
        mpsc::UnboundedSender<String>,
        watch::Sender<bool>,
    ) {
        let (paste_tx, paste_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let notices = Arc::clone(&self.notices);
        (
            LoginOptions {
                timeout,
                browser,
                notify: Arc::new(move |notice| notices.lock().unwrap().push(notice)),
                pasted: Some(paste_rx),
                cancel: cancel_rx,
            },
            paste_tx,
            cancel_tx,
        )
    }

    fn stored(&self) -> Option<McpOAuthState> {
        let url = canonical_url(&self.world.mcp_url()).unwrap();
        FileOAuthStore::at(self.dir.join("mcp-auth.json"), "srv", &url)
            .load()
            .unwrap()
    }

    fn url_notice(&self) -> (String, bool, String) {
        self.notices
            .lock()
            .unwrap()
            .iter()
            .find_map(|notice| match notice {
                LoginNotice::AuthorizationUrl {
                    url,
                    browser_opened,
                    redirect_uri,
                } => Some((url.clone(), *browser_opened, redirect_uri.clone())),
                LoginNotice::Warning(_) => None,
            })
            .expect("authorization URL notice")
    }

    fn warnings(&self) -> Vec<String> {
        self.notices
            .lock()
            .unwrap()
            .iter()
            .filter_map(|notice| match notice {
                LoginNotice::Warning(text) => Some(text.clone()),
                LoginNotice::AuthorizationUrl { .. } => None,
            })
            .collect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn login_through_the_browser_callback_stores_the_tokens() {
    let session = Session::new("login", McpOAuthSpec::default());
    let (browser, opened, responses) = FakeBrowser::new(Visit::Approve { stray: false });
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    let summary = login(&session.auth, options).await.expect("login");
    assert!(summary.has_refresh_token);

    // The browser was asked to open exactly the URL the user is shown.
    let (url, browser_opened, redirect_uri) = session.url_notice();
    assert!(browser_opened);
    assert_eq!(
        opened.lock().unwrap().as_slice(),
        std::slice::from_ref(&url)
    );
    assert!(
        redirect_uri.starts_with("http://127.0.0.1:"),
        "{redirect_uri}"
    );
    assert!(redirect_uri.ends_with("/callback"), "{redirect_uri}");
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    // The browser got a plain success page.
    for _ in 0..50 {
        if !responses.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let page = responses.lock().unwrap().join(
        "
",
    );
    assert!(page.contains("200 OK"), "{page}");
    assert!(page.contains("Signed in"), "{page}");

    let stored = session.stored().expect("credentials on disk");
    assert_eq!(stored.tokens.as_ref().unwrap().access_token, "at-1");
    assert_eq!(stored.client.as_ref().unwrap().client_id, "dcr-client");
    assert_eq!(session.auth.bearer().as_deref(), Some("at-1"));
    // Exactly one registration and one code exchange.
    assert_eq!(session.world.server.count("/as/register"), 1);
    assert_eq!(session.world.server.count("/as/token"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn forged_or_stray_callback_requests_do_not_abort_the_sign_in() {
    let session = Session::new("stray", McpOAuthSpec::default());
    let (browser, _opened, responses) = FakeBrowser::new(Visit::Approve { stray: true });
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    login(&session.auth, options).await.expect("login");
    for _ in 0..50 {
        if responses.lock().unwrap().len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let responses = responses.lock().unwrap().clone();
    assert!(responses[0].starts_with("HTTP/1.1 400"), "{}", responses[0]);
    assert!(responses[1].starts_with("HTTP/1.1 404"), "{}", responses[1]);
    assert!(responses[2].starts_with("HTTP/1.1 400"), "{}", responses[2]);
    assert!(responses[3].starts_with("HTTP/1.1 200"), "{}", responses[3]);
    // The forged code never reached the token endpoint.
    for request in session.world.server.hits("/as/token") {
        assert_eq!(request.form()["code"], "code-1");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pasted_redirect_url_completes_the_sign_in_when_the_browser_cannot() {
    let session = Session::new("paste", McpOAuthSpec::default());
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Unavailable);
    let (options, paste, _cancel) = session.options(browser, Duration::from_secs(20));
    let auth = Arc::clone(&session.auth);
    let task = tokio::spawn(async move { login(&auth, options).await });
    // Wait for the URL the user would copy.
    let (url, browser_opened, redirect_uri) = loop {
        let found = session
            .notices
            .lock()
            .unwrap()
            .iter()
            .find_map(|notice| match notice {
                LoginNotice::AuthorizationUrl {
                    url,
                    browser_opened,
                    redirect_uri,
                } => Some((url.clone(), *browser_opened, redirect_uri.clone())),
                LoginNotice::Warning(_) => None,
            });
        if let Some(found) = found {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(!browser_opened, "the browser could not be opened");
    let state = reqwest::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap();
    // A wrong paste only warns; the sign-in keeps waiting.
    paste.send("garbage".into()).unwrap();
    paste
        .send(format!("{redirect_uri}?code=code-1&state=wrong"))
        .unwrap();
    paste
        .send(format!("{redirect_uri}?code=code-1&state={state}"))
        .unwrap();
    task.await.unwrap().expect("login with a pasted URL");
    let warnings = session.warnings();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("full redirect URL"), "{warnings:?}");
    assert!(warnings[1].contains("different sign-in"), "{warnings:?}");
    assert_eq!(session.auth.bearer().as_deref(), Some("at-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oauth_error_in_the_callback_fails_the_sign_in_with_an_escaped_page() {
    let session = Session::new("denied", McpOAuthSpec::default());
    let (browser, _opened, responses) = FakeBrowser::new(Visit::Deny("access_denied"));
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    let error = login(&session.auth, options).await.unwrap_err();
    assert!(
        matches!(&error, LoginError::Failed(text) if text.contains("denied")),
        "{error:?}"
    );
    for _ in 0..50 {
        if !responses.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let page = responses.lock().unwrap().join("\n");
    assert!(
        !page.contains("<b>denied</b>"),
        "markup must be escaped: {page}"
    );
    assert_eq!(session.world.server.count("/as/token"), 0);
    assert!(session.stored().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sign_in_times_out_and_releases_the_port() {
    let session = Session::new("timeout", McpOAuthSpec::default());
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Ignore);
    let (options, _paste, _cancel) = session.options(browser, Duration::from_millis(300));
    let error = login(&session.auth, options).await.unwrap_err();
    assert_eq!(error, LoginError::Timeout);
    let (_, _, redirect_uri) = session.url_notice();
    let port: u16 = reqwest::Url::parse(&redirect_uri).unwrap().port().unwrap();
    // The listener is gone: the port can be bound again.
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("port released");
    assert!(session.stored().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_the_sign_in() {
    let session = Session::new("cancel", McpOAuthSpec::default());
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Ignore);
    let (options, _paste, cancel) = session.options(browser, Duration::from_secs(30));
    let auth = Arc::clone(&session.auth);
    let task = tokio::spawn(async move { login(&auth, options).await });
    while session.notices.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel.send(true).unwrap();
    assert_eq!(task.await.unwrap().unwrap_err(), LoginError::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_callback_port_is_used_exactly_and_must_be_free() {
    let free = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let session = Session::new(
        "port",
        McpOAuthSpec {
            callback_port: Some(free),
            ..McpOAuthSpec::default()
        },
    );
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Approve { stray: false });
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    login(&session.auth, options).await.expect("login");
    let (_, _, redirect_uri) = session.url_notice();
    assert_eq!(redirect_uri, format!("http://127.0.0.1:{free}/callback"));

    // A port someone else holds is an error that names the setting.
    let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    let session = Session::new(
        "port-busy",
        McpOAuthSpec {
            callback_port: Some(busy_port),
            ..McpOAuthSpec::default()
        },
    );
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Ignore);
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(5));
    let error = login(&session.auth, options).await.unwrap_err();
    assert!(
        matches!(&error, LoginError::Failed(text) if text.contains("oauth.callback_port")),
        "{error:?}"
    );
    drop(busy);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_sign_in_reuses_the_registered_port_and_client() {
    let session = Session::new("reuse", McpOAuthSpec::default());
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Approve { stray: false });
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    login(&session.auth, options).await.expect("first login");
    let (_, _, first_redirect) = session.url_notice();
    session.notices.lock().unwrap().clear();

    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Approve { stray: false });
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    login(&session.auth, options).await.expect("second login");
    let (_, _, second_redirect) = session.url_notice();
    assert_eq!(
        first_redirect, second_redirect,
        "same port, so the registration stays valid"
    );
    assert_eq!(session.world.server.count("/as/register"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_removes_the_credentials_from_disk_and_memory() {
    let session = Session::new("logout", McpOAuthSpec::default());
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Approve { stray: false });
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    login(&session.auth, options).await.expect("login");
    assert!(session.stored().is_some());
    assert!(logout(&session.auth).await.unwrap());
    assert!(session.stored().is_none());
    assert_eq!(session.auth.bearer(), None);
    assert!(!logout(&session.auth).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_process_sees_the_credentials_of_the_first() {
    // `McpAuth::new` seeds its cache from the file: a new process (or a new
    // `McpAuth` after `/mcp reload`) starts signed in.
    let session = Session::new("seed", McpOAuthSpec::default());
    let (browser, _opened, _responses) = FakeBrowser::new(Visit::Approve { stray: false });
    let (options, _paste, _cancel) = session.options(browser, Duration::from_secs(20));
    login(&session.auth, options).await.expect("login");
    let url = canonical_url(&session.world.mcp_url()).unwrap();
    let store = Arc::new(FileOAuthStore::at(
        session.dir.join("mcp-auth.json"),
        "srv",
        &url,
    ));
    let other = McpAuth::new(
        "srv",
        &session.world.mcp_url(),
        McpOAuthSpec::default(),
        store,
    )
    .unwrap();
    assert_eq!(other.bearer().as_deref(), Some("at-1"));
    // Another URL under the same name: not signed in.
    let store = Arc::new(FileOAuthStore::at(
        session.dir.join("mcp-auth.json"),
        "srv",
        "http://127.0.0.1:9/mcp",
    ));
    let moved = McpAuth::new(
        "srv",
        "http://127.0.0.1:9/mcp",
        McpOAuthSpec::default(),
        store,
    )
    .unwrap();
    assert_eq!(moved.bearer(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_processes_refreshing_a_rotating_token_spend_it_once() {
    use slim_core::mcp::{McpAuthHandle, McpManager, McpServerSpec, McpTransport};
    use std::collections::BTreeMap;

    let world = World::new();
    world.grant("current", Some("rt-spent-once"));
    let dir = temp_dir("race");
    let path = dir.join("mcp-auth.json");
    let url = canonical_url(&world.mcp_url()).unwrap();
    // Credentials whose access token expired a moment ago.
    let mut seeded = state_for(&url, "expired");
    seeded.tokens.as_mut().unwrap().refresh_token = Some("rt-spent-once".into());
    seeded.tokens.as_mut().unwrap().expires_at_ms = Some(1);
    seeded.client.as_mut().unwrap().client_secret = None;
    FileOAuthStore::at(&path, "srv", &url)
        .save(&seeded)
        .unwrap();

    // Two independent `McpAuth`s on one file stand in for two Slim processes:
    // they share no in-process state, only the file and its lock.
    let manager = || {
        let store = Arc::new(FileOAuthStore::at(&path, "srv", &url));
        let auth = McpAuth::new("srv", &world.mcp_url(), McpOAuthSpec::default(), store).unwrap();
        let mut spec = McpServerSpec::new(
            "srv",
            McpTransport::Http {
                url: world.mcp_url(),
                headers: BTreeMap::new(),
            },
        );
        spec.timeout = Duration::from_secs(10);
        spec.options.auth = Some(McpAuthHandle(auth));
        McpManager::new(
            BTreeMap::from([("srv".to_owned(), spec)]),
            PathBuf::from("."),
            Default::default(),
        )
    };
    let (first, second) = (manager(), manager());
    let (a, b) = tokio::join!(first.test("srv"), second.test("srv"));
    assert_eq!(a.unwrap(), 1);
    assert_eq!(b.unwrap(), 1);
    // The rotating refresh token was spent once; the loser of the race
    // adopted the winner's tokens from the file instead of presenting the
    // spent one (which the server would have rejected with invalid_grant).
    assert_eq!(world.server.count("/as/token"), 1);
    let stored = FileOAuthStore::at(&path, "srv", &url)
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(stored.tokens.unwrap().access_token, "at-1");
    let _ = std::fs::remove_dir_all(dir);
}
