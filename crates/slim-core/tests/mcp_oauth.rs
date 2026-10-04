//! MCP OAuth (M5) against a loopback mock that plays the MCP server, the
//! protected resource metadata and the authorization server: discovery,
//! dynamic client registration, PKCE, `state`/`iss` checks, refresh (early,
//! on 401, single flight, rotation), URL binding, step-up and the
//! `needs-auth` status. Nothing here opens a browser: the tests act as the
//! user's browser by handing the authorization response to `complete_login`.

#[path = "../../../tests/support/mcp_oauth_mock.rs"]
mod mock;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mock::World;
use serde_json::{json, Value};
use slim_core::mcp::{
    pkce_challenge, AuthorizationResponse, McpAuth, McpAuthHandle, McpCancellation, McpError,
    McpManager, McpOAuthError, McpOAuthSpec, McpOAuthState, McpOAuthStore, McpRequestOutcome,
    McpServerSpec, McpServerStatus, McpTransport, MemoryOAuthStore, OAuthClient, OAuthTokens,
};
use slim_core::process::ExecutableResolver;

const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ-._~0123456789";
const REDIRECT: &str = "http://127.0.0.1:47000/callback";
/// BASE64URL(SHA256(VERIFIER)), computed independently of the code under test.
const CHALLENGE: &str = "OvysRRHRNEIHxb3yI1uMpZivqY-i4S-N7oZz0aUZLws";

fn settings() -> McpOAuthSpec {
    McpOAuthSpec::default()
}

fn auth_for(world: &World, store: Arc<dyn McpOAuthStore>, settings: McpOAuthSpec) -> Arc<McpAuth> {
    McpAuth::new("srv", &world.mcp_url(), settings, store).expect("auth")
}

fn bound_state(world: &World) -> McpOAuthState {
    McpOAuthState {
        server_url: world.mcp_url(),
        ..McpOAuthState::default()
    }
}

fn store_with_tokens(world: &World, tokens: OAuthTokens) -> Arc<MemoryOAuthStore> {
    let mut state = bound_state(world);
    state.client = Some(OAuthClient {
        client_id: "dcr-client".into(),
        client_secret: None,
        redirect_uris: vec![REDIRECT.into()],
        token_endpoint_auth_method: Some("none".into()),
    });
    state.tokens = Some(tokens);
    Arc::new(MemoryOAuthStore::with_state(state))
}

fn tokens(access: &str, refresh: Option<&str>, expires_at_ms: Option<u64>) -> OAuthTokens {
    OAuthTokens {
        access_token: access.into(),
        refresh_token: refresh.map(str::to_owned),
        scope: Some("read".into()),
        expires_at_ms,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn manager_with(world: &World, auth: Arc<McpAuth>) -> McpManager {
    let mut spec = McpServerSpec::new(
        "srv",
        McpTransport::Http {
            url: world.mcp_url(),
            headers: BTreeMap::new(),
        },
    );
    spec.timeout = Duration::from_secs(5);
    spec.options.auth = Some(McpAuthHandle(auth));
    McpManager::new(
        BTreeMap::from([(spec.name.clone(), spec)]),
        PathBuf::from("."),
        ExecutableResolver::default(),
    )
}

fn authorization_response(state: &str) -> AuthorizationResponse {
    AuthorizationResponse {
        code: "code-1".into(),
        state: Some(state.into()),
        iss: None,
    }
}

fn query_of(url: &str) -> BTreeMap<String, String> {
    let (_, query) = url.split_once('?').expect("query");
    mock::decode_pairs(query)
}

async fn call_echo(manager: &McpManager) -> Result<Value, McpError> {
    manager.call("srv", "echo", json!({})).await
}

// ---------------------------------------------------------------------------
// Discovery, registration and the authorization request
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn discovery_registration_and_the_authorization_url() {
    let world = World::with(|knobs| knobs.resource_scopes = Some(vec!["read", "write"]));
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let pending = auth
        .begin_login(REDIRECT, VERIFIER, "state-1")
        .await
        .expect("begin");

    // Protected resource metadata first (path-aware), then the authorization
    // server's, then dynamic client registration.
    assert_eq!(
        world
            .server
            .count("/.well-known/oauth-protected-resource/mcp"),
        1
    );
    assert_eq!(
        world
            .server
            .count("/.well-known/oauth-authorization-server/as"),
        1
    );
    let registration = &world.server.hits("/as/register")[0];
    let body = registration.json();
    assert_eq!(body["redirect_uris"], json!([REDIRECT]));
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert_eq!(body["client_name"], "Slim");
    assert_eq!(body["response_types"], json!(["code"]));
    assert_eq!(body["scope"], "read write");

    let url = &pending.authorization_url;
    assert!(
        url.starts_with(&format!("{}/as/authorize?", world.base())),
        "{url}"
    );
    let query = query_of(url);
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "dcr-client");
    assert_eq!(query["redirect_uri"], REDIRECT);
    assert_eq!(query["state"], "state-1");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["code_challenge"], CHALLENGE);
    assert_eq!(pkce_challenge(VERIFIER), CHALLENGE);
    assert_eq!(query["resource"], world.mcp_url());
    assert_eq!(query["scope"], "read write");
    // Nothing was opened or stored by `begin_login`.
}

#[tokio::test(flavor = "multi_thread")]
async fn login_exchanges_the_code_with_pkce_and_stores_the_tokens() {
    let world = World::new();
    let store = Arc::new(MemoryOAuthStore::new());
    let auth = auth_for(&world, store.clone(), settings());
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    let summary = auth
        .complete_login(pending, authorization_response("s"))
        .await
        .expect("complete");
    assert!(summary.has_refresh_token);

    let exchange = &world.server.hits("/as/token")[0];
    let form = exchange.form();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "code-1");
    assert_eq!(form["code_verifier"], VERIFIER);
    assert_eq!(form["redirect_uri"], REDIRECT);
    assert_eq!(form["resource"], world.mcp_url());
    assert_eq!(form["client_id"], "dcr-client");
    assert!(!form.contains_key("client_secret"));
    assert_eq!(exchange.header("authorization"), None);

    let stored = store.snapshot().expect("stored");
    assert_eq!(stored.server_url, world.mcp_url());
    assert_eq!(stored.tokens.as_ref().unwrap().access_token, "at-1");
    assert_eq!(
        stored.tokens.as_ref().unwrap().refresh_token.as_deref(),
        Some("rt-1")
    );
    assert_eq!(stored.client.as_ref().unwrap().client_id, "dcr-client");
    assert!(stored.tokens.as_ref().unwrap().expires_at_ms.unwrap() > now_ms());
    assert_eq!(auth.bearer().as_deref(), Some("at-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_lifetime_is_not_taken_as_an_expiry() {
    let world = World::with(|knobs| knobs.expires_in = 0);
    let store = Arc::new(MemoryOAuthStore::new());
    let auth = auth_for(&world, store.clone(), settings());
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    auth.complete_login(pending, authorization_response("s"))
        .await
        .unwrap();
    // Otherwise every request would refresh the token again.
    assert_eq!(
        store.snapshot().unwrap().tokens.unwrap().expires_at_ms,
        None
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_registered_client_skips_registration_and_uses_the_offered_method() {
    for (methods, expect_basic, expect_post) in [
        (
            vec!["client_secret_basic", "client_secret_post"],
            true,
            false,
        ),
        (vec!["client_secret_post", "none"], false, true),
    ] {
        let world = World::with(|knobs| knobs.token_auth_methods = methods.clone());
        let auth = auth_for(
            &world,
            Arc::new(MemoryOAuthStore::new()),
            McpOAuthSpec {
                client_id: Some("my client".into()),
                client_secret: Some("s3cr:et/".into()),
                ..McpOAuthSpec::default()
            },
        );
        let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
        assert_eq!(
            query_of(&pending.authorization_url)["client_id"],
            "my client"
        );
        auth.complete_login(pending, authorization_response("s"))
            .await
            .unwrap();
        assert_eq!(world.server.count("/as/register"), 0);
        let exchange = &world.server.hits("/as/token")[0];
        let form = exchange.form();
        if expect_basic {
            // RFC 6749 §2.3.1: id and secret are form-encoded, then Basic.
            let header = exchange.header("authorization").expect("basic header");
            assert!(header.starts_with("Basic "), "{header}");
            assert!(!form.contains_key("client_secret"));
        }
        if expect_post {
            assert_eq!(exchange.header("authorization"), None);
            assert_eq!(form["client_secret"], "s3cr:et/");
            assert_eq!(form["client_id"], "my client");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_pre_registered_client_authenticates_with_none() {
    let world = World::new();
    let auth = auth_for(
        &world,
        Arc::new(MemoryOAuthStore::new()),
        McpOAuthSpec {
            client_id: Some("public-client".into()),
            ..McpOAuthSpec::default()
        },
    );
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    auth.complete_login(pending, authorization_response("s"))
        .await
        .unwrap();
    let exchange = &world.server.hits("/as/token")[0];
    assert_eq!(exchange.header("authorization"), None);
    assert_eq!(exchange.form()["client_id"], "public-client");
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_scope_and_the_challenge_scope_are_merged() {
    let world = World::new();
    let store = Arc::new(MemoryOAuthStore::new());
    let auth = auth_for(
        &world,
        store,
        McpOAuthSpec {
            scope: Some("profile read".into()),
            ..McpOAuthSpec::default()
        },
    );
    // A rejected request teaches the auth its challenge (scope="read").
    let manager = manager_with(&world, auth.clone());
    assert!(manager.test("srv").await.is_err());
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    assert_eq!(
        query_of(&pending.authorization_url)["scope"],
        "profile read"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_client_is_reused_only_for_its_registered_redirect() {
    let world = World::new();
    let store = store_with_tokens(&world, tokens("old", None, None));
    let auth = auth_for(&world, store.clone(), settings());
    // Same redirect: no new registration.
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    assert_eq!(world.server.count("/as/register"), 0);
    assert_eq!(
        query_of(&pending.authorization_url)["client_id"],
        "dcr-client"
    );
    // Another port: the registration does not cover it, so register again,
    // and the new client replaces the old one only once the sign-in ends.
    let other = "http://127.0.0.1:47001/callback";
    let pending = auth.begin_login(other, VERIFIER, "s2").await.unwrap();
    assert_eq!(world.server.count("/as/register"), 1);
    auth.complete_login(pending, authorization_response("s2"))
        .await
        .unwrap();
    let stored = store.snapshot().unwrap();
    assert_eq!(stored.client.unwrap().redirect_uris, vec![other.to_owned()]);
}

// ---------------------------------------------------------------------------
// Checks the client must make
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn state_and_issuer_of_the_response_are_verified() {
    let world = World::with(|knobs| knobs.advertise_iss = true);
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());

    let pending = auth.begin_login(REDIRECT, VERIFIER, "right").await.unwrap();
    let wrong_state = auth
        .complete_login(pending, authorization_response("wrong"))
        .await
        .unwrap_err();
    assert!(wrong_state.to_string().contains("state"), "{wrong_state}");

    let pending = auth.begin_login(REDIRECT, VERIFIER, "right").await.unwrap();
    let no_state = auth
        .complete_login(
            pending,
            AuthorizationResponse {
                code: "code-1".into(),
                state: None,
                iss: Some(world.issuer()),
            },
        )
        .await
        .unwrap_err();
    assert!(no_state.to_string().contains("state"), "{no_state}");

    // The server promised `iss` (RFC 9207): a response without it is refused.
    let pending = auth.begin_login(REDIRECT, VERIFIER, "right").await.unwrap();
    let missing = auth
        .complete_login(pending, authorization_response("right"))
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("issuer"), "{missing}");

    let pending = auth.begin_login(REDIRECT, VERIFIER, "right").await.unwrap();
    let foreign = auth
        .complete_login(
            pending,
            AuthorizationResponse {
                code: "code-1".into(),
                state: Some("right".into()),
                iss: Some("https://evil.example".into()),
            },
        )
        .await
        .unwrap_err();
    assert!(foreign.to_string().contains("issuer"), "{foreign}");
    // None of the refused responses reached the token endpoint.
    assert_eq!(world.server.count("/as/token"), 0);

    let pending = auth.begin_login(REDIRECT, VERIFIER, "right").await.unwrap();
    auth.complete_login(
        pending,
        AuthorizationResponse {
            code: "code-1".into(),
            state: Some("right".into()),
            iss: Some(format!("{}/", world.issuer())),
        },
    )
    .await
    .expect("matching iss (trailing slash tolerated)");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_iss_parameter_is_checked_even_when_the_server_did_not_promise_it() {
    let world = World::new();
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    let error = auth
        .complete_login(
            pending,
            AuthorizationResponse {
                code: "code-1".into(),
                state: Some("s".into()),
                iss: Some("https://other.example".into()),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("issuer"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_metadata_issuer_that_differs_from_the_server_is_refused() {
    let world = World::with(|knobs| {
        knobs.metadata_issuer = Some("https://impostor.example".into());
    });
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let error = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap_err();
    assert!(error.to_string().contains("issuer mismatch"), "{error}");
    assert_eq!(world.server.count("/as/register"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn endpoints_that_are_neither_https_nor_loopback_are_refused() {
    let world = World::with(|knobs| {
        knobs.token_endpoint_override = Some("http://auth.example/token".into());
    });
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let error = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap_err();
    let text = error.to_string();
    assert!(text.contains("https"), "{text}");
    assert!(text.contains("auth.example"), "{text}");
    assert_eq!(world.server.count("/as/register"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorization_server_without_s256_is_refused() {
    let world = World::with(|knobs| knobs.advertise_s256 = false);
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let error = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap_err();
    assert!(error.to_string().contains("S256"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_registration_endpoint_the_user_is_told_to_configure_a_client() {
    let world = World::with(|knobs| knobs.registration = false);
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let error = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap_err();
    assert!(error.to_string().contains("oauth.client_id"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_server_metadata_url_replaces_discovery() {
    let world = World::new();
    let auth = auth_for(
        &world,
        Arc::new(MemoryOAuthStore::new()),
        McpOAuthSpec {
            auth_server_metadata_url: Some(format!(
                "{}/.well-known/oauth-authorization-server/as",
                world.base()
            )),
            ..McpOAuthSpec::default()
        },
    );
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    assert!(pending.authorization_url.starts_with(&world.issuer()));
    // The metadata document was fetched once, directly: no candidate probing.
    assert_eq!(
        world
            .server
            .count("/.well-known/oauth-authorization-server/as"),
        1
    );
    let broken = auth_for(
        &world,
        Arc::new(MemoryOAuthStore::new()),
        McpOAuthSpec {
            auth_server_metadata_url: Some("http://auth.example/meta".into()),
            ..McpOAuthSpec::default()
        },
    );
    assert!(broken.begin_login(REDIRECT, VERIFIER, "s").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_token_endpoints_are_bounded_and_never_followed() {
    let world = World::with(|knobs| knobs.token_response_padding = 400 * 1024);
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    let error = auth
        .complete_login(pending, authorization_response("s"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("too large"), "{error}");

    let world = World::with(|knobs| knobs.token_redirect = true);
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    let error = auth
        .complete_login(pending, authorization_response("s"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("302"), "{error}");
    assert_eq!(world.server.count("/elsewhere"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_needs_https_or_a_loopback_server() {
    let store = Arc::new(MemoryOAuthStore::with_state(McpOAuthState {
        server_url: "http://mcp.example.com/mcp".into(),
        tokens: Some(tokens("secret-token", Some("r"), None)),
        ..McpOAuthState::default()
    }));
    let auth = McpAuth::new("srv", "http://mcp.example.com/mcp", settings(), store).unwrap();
    // A token is never sent over cleartext http to a remote host.
    assert_eq!(auth.bearer(), None);
    let error = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap_err();
    assert!(matches!(error, McpOAuthError::AuthRequired(_)), "{error:?}");
    assert!(error.to_string().contains("https"), "{error}");
}

#[test]
fn state_of_another_server_url_is_ignored() {
    let world = World::new();
    let store = Arc::new(MemoryOAuthStore::with_state(McpOAuthState {
        server_url: "https://other.example/mcp".into(),
        tokens: Some(tokens("foreign-token", Some("r"), None)),
        ..McpOAuthState::default()
    }));
    let auth = auth_for(&world, store, settings());
    assert_eq!(auth.bearer(), None);
    assert!(auth.sensitive_values().is_empty());
}

// ---------------------------------------------------------------------------
// Connecting: needs-auth, bearer, refresh
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn without_credentials_the_server_is_needs_auth_and_nothing_is_opened() {
    let world = World::new();
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let manager = manager_with(&world, auth);
    let error = manager.test("srv").await.unwrap_err();
    assert!(matches!(error, McpError::AuthRequired(_)), "{error:?}");
    assert!(error.to_string().contains("/mcp login srv"), "{error}");
    let status = &manager.statuses()[0].status;
    assert!(
        matches!(status, McpServerStatus::NeedsAuth { reason } if reason.contains("/mcp login srv")),
        "{status:?}"
    );
    assert!(manager.list_servers().contains("needs-auth"));
    // The connect probed the server once and discovered nothing by itself:
    // no registration, no authorization request.
    assert_eq!(world.server.count("/as/register"), 0);
    assert_eq!(world.server.count("/as/authorize"), 0);
    assert_eq!(world.server.count("/as/token"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_token_connects_and_is_sent_as_a_bearer() {
    let world = World::new();
    world.grant("good", Some("r"));
    let store = store_with_tokens(
        &world,
        tokens("good", Some("r"), Some(now_ms() + 3_600_000)),
    );
    let auth = auth_for(&world, store, settings());
    let manager = manager_with(&world, auth.clone());
    assert_eq!(manager.test("srv").await.unwrap(), 1);
    assert!(matches!(
        manager.statuses()[0].status,
        McpServerStatus::Ready { .. }
    ));
    let posts: Vec<_> = world
        .server
        .hits("/mcp")
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert!(!posts.is_empty());
    assert!(posts.iter().all(|request| request.bearer() == Some("good")));
    assert_eq!(world.server.count("/as/token"), 0);
    // The token is a registered secret (redaction and the secret gate).
    assert!(manager.sensitive_values().contains(&"good".to_owned()));
    assert!(manager.sensitive_values().contains(&"r".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_is_refreshed_once_and_the_call_runs_once() {
    let world = World::new();
    // The stored token is not (or no longer) valid on the server.
    world.grant("unrelated", Some("rt-old"));
    let store = store_with_tokens(&world, tokens("expired", Some("rt-old"), None));
    let auth = auth_for(&world, store.clone(), settings());
    let manager = manager_with(&world, auth.clone());
    let reply = call_echo(&manager).await.expect("call after refresh");
    assert_eq!(reply["content"][0]["text"], "pong");

    let refreshes: Vec<_> = world
        .server
        .hits("/as/token")
        .into_iter()
        .filter(|request| request.form()["grant_type"] == "refresh_token")
        .collect();
    assert_eq!(refreshes.len(), 1, "one refresh");
    let form = refreshes[0].form();
    assert_eq!(form["refresh_token"], "rt-old");
    assert_eq!(form["resource"], world.mcp_url());
    // The rejected request is retried with the new token; the tool ran once.
    let calls: Vec<_> = world
        .server
        .hits("/mcp")
        .into_iter()
        .filter(|request| request.rpc_method() == "tools/call")
        .collect();
    let accepted = calls
        .iter()
        .filter(|request| request.bearer() == Some("at-1"))
        .count();
    assert_eq!(accepted, 1);
    // The rotated refresh token was saved.
    let stored = store.snapshot().unwrap().tokens.unwrap();
    assert_eq!(stored.access_token, "at-1");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-1"));
    assert_eq!(auth.bearer().as_deref(), Some("at-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_close_to_expiry_is_refreshed_before_it_is_sent() {
    let world = World::new();
    world.grant("soon", Some("rt-soon"));
    // Valid for 10 more seconds: inside the 30 s refresh window.
    let store = store_with_tokens(
        &world,
        tokens("soon", Some("rt-soon"), Some(now_ms() + 10_000)),
    );
    let auth = auth_for(&world, store, settings());
    let manager = manager_with(&world, auth);
    assert_eq!(manager.test("srv").await.unwrap(), 1);
    assert_eq!(world.server.count("/as/token"), 1);
    // The old token never reached the server: no 401 was needed.
    assert!(world
        .server
        .hits("/mcp")
        .iter()
        .all(|request| request.bearer() == Some("at-1")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_outside_the_window_is_not_refreshed_early() {
    let world = World::new();
    world.grant("fresh", Some("rt"));
    let store = store_with_tokens(
        &world,
        tokens("fresh", Some("rt"), Some(now_ms() + 120_000)),
    );
    let auth = auth_for(&world, store, settings());
    let manager = manager_with(&world, auth);
    assert_eq!(manager.test("srv").await.unwrap(), 1);
    assert_eq!(world.server.count("/as/token"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_share_one_refresh() {
    let world = World::new();
    world.grant("good", Some("rt-shared"));
    let store = store_with_tokens(&world, tokens("good", Some("rt-shared"), None));
    let auth = auth_for(&world, store, settings());
    let manager = Arc::new(manager_with(&world, auth));
    assert_eq!(manager.test("srv").await.unwrap(), 1);
    // The server stops accepting the token: every call below is rejected
    // at once, and all of them need a new one.
    world.knobs().valid_access.clear();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let manager = Arc::clone(&manager);
        tasks.push(tokio::spawn(async move { call_echo(&manager).await }));
    }
    for task in tasks {
        let reply = task.await.unwrap().expect("call after the shared refresh");
        assert_eq!(reply["content"][0]["text"], "pong");
    }
    // One refresh with the rotating refresh token. A second one would have
    // presented the spent token (invalid_grant) and discarded the grant.
    assert_eq!(world.server.count("/as/token"), 1);
}

/// A store whose lock simulates another process that refreshed while this
/// one waited for the lock.
struct RacingStore {
    inner: MemoryOAuthStore,
    replacement: OAuthTokens,
    locks: AtomicUsize,
}

impl McpOAuthStore for RacingStore {
    fn load(&self) -> Result<Option<McpOAuthState>, String> {
        self.inner.load()
    }

    fn save(&self, state: &McpOAuthState) -> Result<(), String> {
        self.inner.save(state)
    }

    fn clear(&self) -> Result<bool, String> {
        self.inner.clear()
    }

    fn lock_refresh(&self) -> Result<Box<dyn Send>, String> {
        self.locks.fetch_add(1, Ordering::SeqCst);
        let mut state = self.inner.snapshot().expect("state");
        state.tokens = Some(self.replacement.clone());
        self.inner.save(&state)?;
        Ok(Box::new(()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_refreshed_by_another_process_are_used_without_refreshing() {
    let world = World::new();
    world.grant("by-other-process", Some("rt-2"));
    let mut state = bound_state(&world);
    state.client = Some(OAuthClient {
        client_id: "dcr-client".into(),
        client_secret: None,
        redirect_uris: vec![REDIRECT.into()],
        token_endpoint_auth_method: None,
    });
    state.tokens = Some(tokens("expired", Some("rt-1"), None));
    let store = Arc::new(RacingStore {
        inner: MemoryOAuthStore::with_state(state),
        replacement: tokens("by-other-process", Some("rt-2"), Some(now_ms() + 3_600_000)),
        locks: AtomicUsize::new(0),
    });
    let auth = auth_for(&world, store.clone(), settings());
    let manager = manager_with(&world, auth.clone());
    assert_eq!(manager.test("srv").await.unwrap(), 1);
    // The refresh took the cross-process lock, found other tokens in the
    // store and adopted them: the token endpoint was never called, so the
    // other process's rotated refresh token was not spent.
    assert_eq!(store.locks.load(Ordering::SeqCst), 1);
    assert_eq!(world.server.count("/as/token"), 0);
    assert_eq!(auth.bearer().as_deref(), Some("by-other-process"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_refresh_token_clears_the_grant_and_needs_sign_in() {
    let world = World::with(|knobs| knobs.reject_refresh = Some("invalid_grant"));
    let store = store_with_tokens(&world, tokens("expired", Some("revoked"), None));
    let auth = auth_for(&world, store.clone(), settings());
    let manager = manager_with(&world, auth.clone());
    let error = manager.test("srv").await.unwrap_err();
    assert!(matches!(error, McpError::AuthRequired(_)), "{error:?}");
    assert!(matches!(
        manager.statuses()[0].status,
        McpServerStatus::NeedsAuth { .. }
    ));
    let stored = store.snapshot().unwrap();
    assert!(stored.tokens.is_none(), "tokens cleared");
    assert!(stored.client.is_some(), "the registration is kept");
    assert_eq!(auth.bearer(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_client_is_forgotten_too() {
    let world = World::with(|knobs| knobs.reject_refresh = Some("invalid_client"));
    let store = store_with_tokens(&world, tokens("expired", Some("rt"), None));
    let auth = auth_for(&world, store.clone(), settings());
    let manager = manager_with(&world, auth);
    assert!(manager.test("srv").await.is_err());
    let stored = store.snapshot().unwrap();
    assert!(stored.tokens.is_none());
    assert!(stored.client.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_refresh_token_means_sign_in_again() {
    let world = World::new();
    let store = store_with_tokens(&world, tokens("expired", None, None));
    let auth = auth_for(&world, store, settings());
    let manager = manager_with(&world, auth);
    let error = manager.test("srv").await.unwrap_err();
    assert!(error.to_string().contains("no refresh token"), "{error}");
    assert_eq!(world.server.count("/as/token"), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_needs_no_auth_connects_without_a_token() {
    let world = World::with(|knobs| knobs.public = true);
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let manager = manager_with(&world, auth);
    assert_eq!(manager.test("srv").await.unwrap(), 1);
    assert!(world
        .server
        .hits("/mcp")
        .iter()
        .all(|request| request.header("authorization").is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_stored_by_another_process_are_picked_up_on_connect() {
    let world = World::new();
    world.grant("late", Some("r"));
    let store = Arc::new(MemoryOAuthStore::new());
    let auth = auth_for(&world, store.clone(), settings());
    let manager = manager_with(&world, auth.clone());
    assert!(manager.test("srv").await.is_err());
    // Another process (`slim mcp login`) stores credentials meanwhile.
    let mut state = bound_state(&world);
    state.tokens = Some(tokens("late", Some("r"), Some(now_ms() + 3_600_000)));
    store.save(&state).unwrap();
    // The reload is throttled; the next connect after it sees the token.
    tokio::time::sleep(Duration::from_millis(5200)).await;
    manager.reconnect("srv").await.expect("reconnect");
    assert!(matches!(
        manager.statuses()[0].status,
        McpServerStatus::Ready { .. }
    ));
}

// ---------------------------------------------------------------------------
// Step-up and logout
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn insufficient_scope_asks_for_a_new_sign_in_with_merged_scopes() {
    let world = World::with(|knobs| knobs.call_needs_scope = Some("write"));
    world.grant("good", Some("r"));
    let store = store_with_tokens(
        &world,
        tokens("good", Some("r"), Some(now_ms() + 3_600_000)),
    );
    let auth = auth_for(&world, store.clone(), settings());
    let manager = manager_with(&world, auth.clone());
    let error = call_echo(&manager).await.unwrap_err();
    assert!(matches!(error, McpError::AuthRequired(_)), "{error:?}");
    assert!(error.to_string().contains("write"), "{error}");
    assert!(matches!(
        manager.statuses()[0].status,
        McpServerStatus::NeedsAuth { .. }
    ));
    // Not a replay of a call that may have run: the server refused it.
    assert_eq!(
        world
            .server
            .hits("/mcp")
            .iter()
            .filter(|request| request.rpc_method() == "tools/call")
            .count(),
        1
    );
    // The scopes asked for are remembered with the granted ones.
    assert_eq!(
        store.snapshot().unwrap().pending_scope.as_deref(),
        Some("read write")
    );
    // The next sign-in requests them, and refreshing is skipped (a refresh
    // would keep the old scope).
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    assert_eq!(query_of(&pending.authorization_url)["scope"], "read write");
    assert_eq!(world.server.count("/as/token"), 0);
    auth.complete_login(pending, authorization_response("s"))
        .await
        .unwrap();
    assert!(store.snapshot().unwrap().pending_scope.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_403_is_not_an_authorization_problem() {
    // Only `insufficient_scope` is a step-up; any other 403 is the server's
    // ordinary refusal and neither refreshes nor changes the status.
    let world = World::with(|knobs| knobs.plain_403 = true);
    world.grant("good", Some("r"));
    let store = store_with_tokens(
        &world,
        tokens("good", Some("r"), Some(now_ms() + 3_600_000)),
    );
    let auth = auth_for(&world, store, settings());
    let manager = manager_with(&world, auth);
    let error = call_echo(&manager).await.unwrap_err();
    assert!(
        matches!(error, McpError::Server { code: 403, .. }),
        "{error:?}"
    );
    assert_eq!(world.server.count("/as/token"), 0);
    assert!(matches!(
        manager.statuses()[0].status,
        McpServerStatus::Ready { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_deletes_the_credentials_and_forgets_the_secrets() {
    let world = World::new();
    let store = store_with_tokens(&world, tokens("good", Some("r"), None));
    let auth = auth_for(&world, store.clone(), settings());
    assert_eq!(auth.bearer().as_deref(), Some("good"));
    assert!(auth.logout().await.unwrap());
    assert!(store.snapshot().is_none());
    assert_eq!(auth.bearer(), None);
    assert!(auth.sensitive_values().is_empty());
    assert!(!auth.logout().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_are_registered_and_never_printed() {
    let world = World::new();
    let auth = auth_for(
        &world,
        Arc::new(MemoryOAuthStore::new()),
        McpOAuthSpec {
            client_id: Some("cid".into()),
            client_secret: Some("configured-secret".into()),
            ..McpOAuthSpec::default()
        },
    );
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    assert!(!format!("{pending:?}").contains(VERIFIER));
    auth.complete_login(pending, authorization_response("s"))
        .await
        .unwrap();
    let secrets = auth.sensitive_values();
    for expected in ["at-1", "rt-1", "configured-secret"] {
        assert!(secrets.contains(&expected.to_owned()), "{secrets:?}");
    }
    let debug = format!("{auth:?} {:?}", McpAuthHandle(auth.clone()));
    for secret in &secrets {
        assert!(!debug.contains(secret.as_str()), "{debug}");
    }
    let state = OAuthTokens {
        access_token: "tok-visible".into(),
        refresh_token: Some("ref-visible".into()),
        ..OAuthTokens::default()
    };
    let printed = format!("{state:?}");
    assert!(!printed.contains("tok-visible") && !printed.contains("ref-visible"));

    // Dynamically registered client secrets are registered too.
    let world = World::new();
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let pending = auth.begin_login(REDIRECT, VERIFIER, "s").await.unwrap();
    auth.complete_login(pending, authorization_response("s"))
        .await
        .unwrap();
    // `begin_login` registered the client; its secret is stored (and so
    // registered) once the sign-in completed.
    assert!(auth
        .sensitive_values()
        .contains(&"dcr-secret-value".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manager_exposes_the_auth_handle_of_oauth_servers() {
    let world = World::new();
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let manager = manager_with(&world, auth.clone());
    let handle = manager.auth_handle("srv").expect("handle");
    assert!(Arc::ptr_eq(&handle, &auth));
    assert!(manager.auth_handle("missing").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_connect_waiting_on_oauth_leaves_no_state_behind() {
    let world = World::new();
    let auth = auth_for(&world, Arc::new(MemoryOAuthStore::new()), settings());
    let manager = manager_with(&world, auth);
    let cancellation = McpCancellation::new();
    cancellation.cancel();
    let outcome = manager
        .call_cancellable("srv", "echo", json!({}), cancellation)
        .await;
    assert!(matches!(
        outcome,
        McpRequestOutcome::InterruptedBeforeSend { .. }
    ));
    assert!(world.server.requests().is_empty());
}
