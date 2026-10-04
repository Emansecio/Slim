//! `McpAuth`: the OAuth state machine of one HTTP MCP server.
//!
//! The connection asks it for the bearer token to send ([`McpAuth::bearer`],
//! [`McpAuth::prepare`]) and hands it every 401/403 ([`McpAuth::execute`]).
//! It never opens a browser: sign-in is [`McpAuth::begin_login`] +
//! [`McpAuth::complete_login`], driven by the application (loopback callback,
//! pasted redirect URL).
//!
//! Refresh rules: tokens are refreshed 30 s before they expire and after a
//! 401, by at most one request per process at a time and under the store's
//! cross-process lock, so a rotating refresh token is never spent twice.
//! Tokens that changed meanwhile (another process refreshed them, or the user
//! signed in) are adopted without refreshing.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Url;

use super::discovery::{
    clean_text, discover, is_loopback_host, parse_www_authenticate, DiscoveryRequest,
    EndpointPolicy,
};
use super::flow::{
    authorization_url, exchange_code, merge_scopes, refresh_grant, register_client, step_up_scope,
    valid_verifier, AuthorizationParams, TokenContext, TokenGrant,
};
use super::types::{Challenge, DiscoveryState, McpOAuthState, OAuthClient, OAuthTokens};
use super::{McpOAuthError, McpOAuthStore};
use crate::mcp::spec::McpOAuthSpec;

/// Access tokens this close to expiry are refreshed before they are sent.
const REFRESH_SKEW_MS: u64 = 30_000;
/// Bounds every request to the authorization server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Without a token, the store is looked at again (another process may have
/// signed in) at most this often.
const RELOAD_INTERVAL: Duration = Duration::from_secs(5);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Why a request could not be sent with authorization.
#[derive(Debug)]
pub(crate) enum AuthSendError {
    /// The request itself failed on the network.
    Transport(reqwest::Error),
    /// The user has to sign in (again). The text is safe to show.
    AuthRequired(String),
    /// The refresh failed for a reason that is not "sign in again".
    Failed(String),
}

#[derive(Default)]
struct Cache {
    token: Option<String>,
    expires_at_ms: Option<u64>,
    refreshable: bool,
    /// Every secret this server's OAuth state holds, for redaction.
    secrets: Vec<String>,
    reload_after: Option<Instant>,
}

/// Where a loopback callback should listen.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CallbackHint {
    /// `oauth.callback_port`: the redirect URI is fixed, the port is required.
    pub configured: Option<u16>,
    /// Port of the redirect URI the stored client registered: reuse it when
    /// it is free so the registration stays valid.
    pub registered: Option<u16>,
}

/// An authorization request waiting for the user's browser.
pub struct PendingAuthorization {
    /// Open this in the browser.
    pub authorization_url: String,
    /// The `state` the response must carry.
    pub state: String,
    pub redirect_uri: String,
    verifier: String,
    discovery: DiscoveryState,
    client: OAuthClient,
    /// The client came from dynamic registration and is persisted.
    registered: bool,
    scope: Option<String>,
}

impl fmt::Debug for PendingAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingAuthorization")
            .field("redirect_uri", &self.redirect_uri)
            .field("verifier", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// The authorization server's answer delivered to the redirect URI.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthorizationResponse {
    pub code: String,
    pub state: Option<String>,
    /// RFC 9207 `iss`.
    pub iss: Option<String>,
}

/// What a completed sign-in granted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoginSummary {
    pub scope: Option<String>,
    pub has_refresh_token: bool,
}

pub struct McpAuth {
    name: String,
    server_url: Url,
    /// Binds stored state to this exact URL.
    server_key: String,
    settings: McpOAuthSpec,
    store: Arc<dyn McpOAuthStore>,
    http: reqwest::Client,
    policy: EndpointPolicy,
    /// OAuth credentials only ever travel over https or to loopback.
    secure_server: bool,
    cache: Mutex<Cache>,
    /// One refresh at a time in this process.
    gate: tokio::sync::Mutex<()>,
    challenge: Mutex<Option<Challenge>>,
}

impl fmt::Debug for McpAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpAuth")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl McpAuth {
    /// `settings.client_secret` must already be resolved. Seeds the token
    /// cache from `store` (a blocking read of one small file).
    pub fn new(
        name: &str,
        server_url: &str,
        settings: McpOAuthSpec,
        store: Arc<dyn McpOAuthStore>,
    ) -> Result<Arc<Self>, McpOAuthError> {
        let mut url = Url::parse(server_url)
            .map_err(|_| McpOAuthError::Failed("invalid MCP server URL".into()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(McpOAuthError::Failed(
                "MCP server URL must be http(s)".into(),
            ));
        }
        url.set_fragment(None);
        let loopback = is_loopback_host(&url);
        let http = reqwest::Client::builder()
            // OAuth endpoints never legitimately redirect, and a redirect
            // would carry credentials to another host.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| McpOAuthError::Failed(format!("http client: {error}")))?;
        let auth = Arc::new(Self {
            name: name.to_owned(),
            server_key: url.to_string(),
            secure_server: url.scheme() == "https" || loopback,
            server_url: url,
            settings,
            store,
            http,
            policy: EndpointPolicy {
                allow_loopback_http: loopback,
            },
            cache: Mutex::new(Cache::default()),
            gate: tokio::sync::Mutex::new(()),
            challenge: Mutex::new(None),
        });
        // A store that cannot be read means "not signed in"; the failure
        // surfaces when the user signs in or a request is rejected.
        let state = auth.load_bound_state().ok().flatten();
        auth.adopt(state.as_ref());
        Ok(auth)
    }

    pub fn server_name(&self) -> &str {
        &self.name
    }

    pub fn server_url(&self) -> &str {
        &self.server_key
    }

    pub fn settings(&self) -> &McpOAuthSpec {
        &self.settings
    }

    /// Access token to send, if signed in. Never refreshes: see
    /// [`Self::prepare`].
    pub fn bearer(&self) -> Option<String> {
        if !self.secure_server {
            return None;
        }
        lock(&self.cache).token.clone()
    }

    /// Tokens and client secrets currently held, for redaction and the
    /// model-to-server secret gate.
    pub fn sensitive_values(&self) -> Vec<String> {
        lock(&self.cache).secrets.clone()
    }

    fn sign_in_hint(&self) -> String {
        format!("sign in with /mcp login {}", self.name)
    }

    fn insecure_error(&self) -> McpOAuthError {
        McpOAuthError::AuthRequired(format!(
            "MCP server {} needs OAuth sign-in, but OAuth only works with https (or loopback) servers",
            self.name
        ))
    }

    // -- store access -------------------------------------------------------

    fn load_bound_state(&self) -> Result<Option<McpOAuthState>, McpOAuthError> {
        let state = self.store.load().map_err(McpOAuthError::Failed)?;
        Ok(state.filter(|state| state.server_url == self.server_key))
    }

    fn save_bound_state(&self, state: &McpOAuthState) -> Result<(), McpOAuthError> {
        debug_assert_eq!(state.server_url, self.server_key);
        self.store.save(state).map_err(McpOAuthError::Failed)
    }

    /// Runs a blocking store operation off the async threads.
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl FnOnce(&Self) -> Result<T, McpOAuthError> + Send + 'static,
    ) -> Result<T, McpOAuthError> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || operation(&this))
            .await
            .map_err(|_| McpOAuthError::Failed("OAuth store task failed".into()))?
    }

    /// Replaces the cache with what `state` holds.
    fn adopt(&self, state: Option<&McpOAuthState>) {
        let mut cache = lock(&self.cache);
        let tokens = state.and_then(|state| state.tokens.as_ref());
        cache.token = tokens.map(|tokens| tokens.access_token.clone());
        cache.expires_at_ms = tokens.and_then(|tokens| tokens.expires_at_ms);
        cache.refreshable = tokens.is_some_and(|tokens| tokens.refresh_token.is_some());
        let mut secrets = Vec::new();
        if let Some(tokens) = tokens {
            secrets.push(tokens.access_token.clone());
            secrets.extend(tokens.refresh_token.clone());
        }
        if let Some(secret) = state
            .and_then(|state| state.client.as_ref())
            .and_then(|client| client.client_secret.clone())
        {
            secrets.push(secret);
        }
        secrets.extend(self.settings.client_secret.clone());
        secrets.retain(|secret| !secret.is_empty());
        cache.secrets = secrets;
    }

    /// The configured client, or the one registered earlier.
    fn client_of(&self, state: &McpOAuthState) -> Option<OAuthClient> {
        match &self.settings.client_id {
            Some(client_id) => Some(OAuthClient {
                client_id: client_id.clone(),
                client_secret: self.settings.client_secret.clone(),
                redirect_uris: Vec::new(),
                token_endpoint_auth_method: None,
            }),
            None => state.client.clone(),
        }
    }

    fn empty_state(&self) -> McpOAuthState {
        McpOAuthState {
            server_url: self.server_key.clone(),
            ..McpOAuthState::default()
        }
    }

    // -- requests -----------------------------------------------------------

    /// Before a request: refreshes a token that is about to expire, or looks
    /// for credentials another process stored. Failures are left to the
    /// request: a 401 decides what happens next.
    pub async fn prepare(self: &Arc<Self>) {
        if !self.secure_server {
            return;
        }
        let (token, expires_at_ms, refreshable, reload_due) = {
            let mut cache = lock(&self.cache);
            let reload_due = cache.token.is_none()
                && cache
                    .reload_after
                    .is_none_or(|after| Instant::now() >= after);
            if reload_due {
                cache.reload_after = Some(Instant::now() + RELOAD_INTERVAL);
            }
            (
                cache.token.clone(),
                cache.expires_at_ms,
                cache.refreshable,
                reload_due,
            )
        };
        match token {
            None if reload_due => {
                if let Ok(state) = self.blocking(Self::load_bound_state).await {
                    self.adopt(state.as_ref());
                }
            }
            Some(token) => {
                let expiring =
                    expires_at_ms.is_some_and(|at| at <= now_ms().saturating_add(REFRESH_SKEW_MS));
                if expiring && refreshable {
                    let _ = self.refresh(Some(token), None).await;
                }
            }
            None => {}
        }
    }

    /// Sends the request `build` makes (it must attach [`Self::bearer`]).
    /// After a 401 the token is refreshed and the request is sent once more
    /// (a 401 means the server did not run it, so this is not a replay); a
    /// 403 `insufficient_scope` asks for a new sign-in with merged scopes.
    pub(crate) async fn execute(
        self: &Arc<Self>,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, AuthSendError> {
        self.prepare().await;
        let used = self.bearer();
        let response = build().send().await.map_err(AuthSendError::Transport)?;
        let Some(challenge) = self.rejection(&response) else {
            return Ok(response);
        };
        if response.status().as_u16() == 403 {
            return Err(AuthSendError::AuthRequired(self.step_up(challenge).await));
        }
        drop(response);
        self.on_unauthorized(used, challenge)
            .await
            .map_err(|error| match error {
                McpOAuthError::AuthRequired(reason) => AuthSendError::AuthRequired(reason),
                other => AuthSendError::Failed(other.to_string()),
            })?;
        let retried = build().send().await.map_err(AuthSendError::Transport)?;
        match self.rejection(&retried) {
            None => Ok(retried),
            Some(challenge) if retried.status().as_u16() == 403 => {
                Err(AuthSendError::AuthRequired(self.step_up(challenge).await))
            }
            Some(_) => Err(AuthSendError::AuthRequired(format!(
                "MCP server {} rejected the access token; {}",
                self.name,
                self.sign_in_hint()
            ))),
        }
    }

    /// The challenge of a 401, or of a 403 that asks for more scope. `None`
    /// for any other response (including a 403 that is just "forbidden").
    fn rejection(&self, response: &reqwest::Response) -> Option<Challenge> {
        let status = response.status().as_u16();
        if status != 401 && status != 403 {
            return None;
        }
        let headers = response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE);
        let challenge = parse_www_authenticate(headers.iter().filter_map(|v| v.to_str().ok()));
        if status == 403 && challenge.error.as_deref() != Some("insufficient_scope") {
            return None;
        }
        Some(challenge)
    }

    /// 403 `insufficient_scope`: remember the scopes the next sign-in must
    /// ask for (the granted ones plus the challenged ones) and report that
    /// the user has to sign in again.
    async fn step_up(self: &Arc<Self>, challenge: Challenge) -> String {
        let wanted = challenge.scope.clone();
        *lock(&self.challenge) = Some(challenge);
        if wanted.is_some() {
            let wanted = wanted.clone();
            let _ = self
                .blocking(move |this| {
                    let Some(mut state) = this.load_bound_state()? else {
                        return Ok(());
                    };
                    let granted = state
                        .tokens
                        .as_ref()
                        .and_then(|tokens| tokens.scope.clone());
                    state.pending_scope = step_up_scope(granted.as_deref(), wanted.as_deref());
                    this.save_bound_state(&state)
                })
                .await;
        }
        format!(
            "MCP server {} asks for more access ({}); {} again",
            self.name,
            wanted.as_deref().map_or_else(
                || "a broader scope".to_owned(),
                |scope| format!("scope: {}", clean_text(scope, 160))
            ),
            self.sign_in_hint()
        )
    }

    /// A 401: replace the token the request carried (`used`), or report that
    /// the user has to sign in.
    async fn on_unauthorized(
        self: &Arc<Self>,
        used: Option<String>,
        challenge: Challenge,
    ) -> Result<(), McpOAuthError> {
        *lock(&self.challenge) = Some(challenge.clone());
        if !self.secure_server {
            return Err(self.insecure_error());
        }
        // `refresh` lets a request whose token was already replaced retry.
        self.refresh(used, Some(challenge)).await
    }

    /// Replaces `stale`, the access token that expired or was rejected.
    async fn refresh(
        self: &Arc<Self>,
        stale: Option<String>,
        challenge: Option<Challenge>,
    ) -> Result<(), McpOAuthError> {
        let _flight = self.gate.lock().await;
        let current = self.bearer();
        if current.is_some() && current != stale {
            return Ok(());
        }
        let guard = self
            .blocking(|this| this.store.lock_refresh().map_err(McpOAuthError::Failed))
            .await?;
        let result = self.refresh_locked(stale, challenge).await;
        drop(guard);
        result
    }

    async fn refresh_locked(
        self: &Arc<Self>,
        stale: Option<String>,
        challenge: Option<Challenge>,
    ) -> Result<(), McpOAuthError> {
        let state = self.blocking(Self::load_bound_state).await?;
        let not_signed_in = || {
            McpOAuthError::AuthRequired(format!(
                "MCP server {} requires authorization (HTTP 401). If it uses OAuth, {}; \
                 otherwise configure an Authorization header",
                self.name,
                self.sign_in_hint()
            ))
        };
        let Some(state) = state else {
            self.adopt(None);
            return Err(not_signed_in());
        };
        let Some(tokens) = state.tokens.clone() else {
            self.adopt(Some(&state));
            return Err(not_signed_in());
        };
        if stale.as_deref() != Some(tokens.access_token.as_str()) {
            // Another process refreshed (or the user signed in meanwhile).
            self.adopt(Some(&state));
            return Ok(());
        }
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            return Err(McpOAuthError::AuthRequired(format!(
                "the access token for MCP server {} expired and there is no refresh token; {}",
                self.name,
                self.sign_in_hint()
            )));
        };
        let Some(client) = self.client_of(&state) else {
            return Err(not_signed_in());
        };
        let discovery = match (&self.settings.auth_server_metadata_url, &state.discovery) {
            (None, Some(cached)) => cached.clone(),
            _ => {
                discover(
                    &self.http,
                    &DiscoveryRequest {
                        server: &self.server_url,
                        resource_metadata: challenge
                            .as_ref()
                            .and_then(|challenge| challenge.resource_metadata.as_deref()),
                        metadata_override: self.settings.auth_server_metadata_url.as_deref(),
                    },
                    self.policy,
                )
                .await?
            }
        };
        let grant = refresh_grant(
            TokenContext {
                http: &self.http,
                metadata: &discovery.metadata,
                policy: self.token_policy(),
                client: &client,
                resource: &discovery.resource,
            },
            &refresh_token,
        )
        .await;
        match grant {
            Ok(grant) => {
                let mut next = state.clone();
                next.tokens = Some(tokens_from_grant(grant, &tokens));
                next.discovery = Some(discovery);
                self.blocking_save(next.clone()).await?;
                self.adopt(Some(&next));
                Ok(())
            }
            Err(McpOAuthError::Server { code, .. }) if code == "invalid_grant" => {
                let mut next = state;
                next.tokens = None;
                let _ = self.blocking_save(next.clone()).await;
                self.adopt(Some(&next));
                Err(McpOAuthError::AuthRequired(format!(
                    "the authorization for MCP server {} is no longer valid; {}",
                    self.name,
                    self.sign_in_hint()
                )))
            }
            Err(McpOAuthError::Server { code, .. })
                if code == "invalid_client" || code == "unauthorized_client" =>
            {
                let mut next = state;
                next.tokens = None;
                if self.settings.client_id.is_none() {
                    next.client = None;
                }
                let _ = self.blocking_save(next.clone()).await;
                self.adopt(Some(&next));
                Err(McpOAuthError::AuthRequired(format!(
                    "the OAuth client of MCP server {} was rejected; {}",
                    self.name,
                    self.sign_in_hint()
                )))
            }
            Err(error) => Err(error),
        }
    }

    /// Endpoint policy for requests made with stored discovery data.
    fn token_policy(&self) -> EndpointPolicy {
        if self.settings.auth_server_metadata_url.is_some() {
            EndpointPolicy {
                allow_loopback_http: true,
            }
        } else {
            self.policy
        }
    }

    async fn blocking_save(self: &Arc<Self>, state: McpOAuthState) -> Result<(), McpOAuthError> {
        self.blocking(move |this| this.save_bound_state(&state))
            .await
    }

    // -- sign-in ------------------------------------------------------------

    /// Where a loopback callback should listen.
    pub async fn callback_hint(self: &Arc<Self>) -> CallbackHint {
        let registered = self
            .blocking(Self::load_bound_state)
            .await
            .ok()
            .flatten()
            .filter(|_| self.settings.client_id.is_none())
            .and_then(|state| state.client)
            .and_then(|client| client.redirect_uris.first().cloned())
            .and_then(|uri| Url::parse(&uri).ok())
            .and_then(|url| url.port());
        CallbackHint {
            configured: self.settings.callback_port,
            registered,
        }
    }

    /// Starts a sign-in: discovers the authorization server, finds or
    /// registers the client and builds the authorization URL. `verifier` is
    /// the PKCE code verifier (43-128 unreserved characters) and `state` the
    /// anti-forgery value, both freshly random. Nothing is opened or stored.
    pub async fn begin_login(
        self: &Arc<Self>,
        redirect_uri: &str,
        verifier: &str,
        state: &str,
    ) -> Result<PendingAuthorization, McpOAuthError> {
        if !self.secure_server {
            return Err(self.insecure_error());
        }
        if !valid_verifier(verifier) || state.is_empty() {
            return Err(McpOAuthError::Failed(
                "invalid PKCE verifier or state".into(),
            ));
        }
        let stored = self.blocking(Self::load_bound_state).await?;
        let challenge = lock(&self.challenge).clone().unwrap_or_default();
        let discovery = discover(
            &self.http,
            &DiscoveryRequest {
                server: &self.server_url,
                resource_metadata: challenge.resource_metadata.as_deref(),
                metadata_override: self.settings.auth_server_metadata_url.as_deref(),
            },
            self.policy,
        )
        .await?;
        let scope = merge_scopes([
            self.settings.scope.as_deref(),
            stored
                .as_ref()
                .and_then(|state| state.pending_scope.as_deref()),
            challenge.scope.as_deref(),
        ])
        .or_else(|| {
            let supported = discovery.resource_scopes.join(" ");
            (!supported.trim().is_empty()).then_some(supported)
        });
        let stored_state = stored.unwrap_or_else(|| self.empty_state());
        let (client, registered) = match self.client_of(&stored_state) {
            Some(client)
                if self.settings.client_id.is_some()
                    || client.redirect_uris.iter().any(|uri| uri == redirect_uri) =>
            {
                (client, false)
            }
            _ => {
                let name = self.settings.client_name.as_deref().unwrap_or("Slim");
                let client = register_client(
                    &self.http,
                    &discovery.metadata,
                    self.token_policy(),
                    name,
                    redirect_uri,
                    scope.as_deref(),
                )
                .await?;
                (client, true)
            }
        };
        let url = authorization_url(
            &discovery.metadata,
            &AuthorizationParams {
                client_id: &client.client_id,
                redirect_uri,
                scope: scope.as_deref(),
                state,
                code_challenge: &super::pkce_challenge(verifier),
                resource: &discovery.resource,
            },
        )?;
        Ok(PendingAuthorization {
            authorization_url: url,
            state: state.to_owned(),
            redirect_uri: redirect_uri.to_owned(),
            verifier: verifier.to_owned(),
            discovery,
            client,
            registered,
            scope,
        })
    }

    /// Finishes a sign-in with the authorization server's answer: checks
    /// `state` and `iss` (RFC 9207), exchanges the code (PKCE, RFC 8707
    /// resource) and stores the tokens.
    pub async fn complete_login(
        self: &Arc<Self>,
        pending: PendingAuthorization,
        response: AuthorizationResponse,
    ) -> Result<LoginSummary, McpOAuthError> {
        if response.state.as_deref() != Some(pending.state.as_str()) {
            return Err(McpOAuthError::Failed(
                "OAuth state mismatch: the response belongs to a different sign-in".into(),
            ));
        }
        let metadata = &pending.discovery.metadata;
        if response.iss.is_some() || metadata.authorization_response_iss_parameter_supported {
            let issuer = metadata.issuer.trim_end_matches('/');
            let received = response.iss.as_deref().map(|iss| iss.trim_end_matches('/'));
            if received != Some(issuer) {
                return Err(McpOAuthError::Failed(format!(
                    "OAuth issuer mismatch: expected {}, the response carries {}",
                    clean_text(issuer, 120),
                    received.map_or_else(|| "none".to_owned(), |iss| clean_text(iss, 120))
                )));
            }
        }
        let grant = exchange_code(
            TokenContext {
                http: &self.http,
                metadata,
                policy: self.token_policy(),
                client: &pending.client,
                resource: &pending.discovery.resource,
            },
            &response.code,
            &pending.verifier,
            &pending.redirect_uri,
        )
        .await?;
        // A response without `scope` grants what was requested (RFC 6749 §5.1).
        let previous = OAuthTokens {
            scope: pending.scope.clone(),
            ..OAuthTokens::default()
        };
        let tokens = tokens_from_grant(grant, &previous);
        let summary = LoginSummary {
            scope: tokens.scope.clone(),
            has_refresh_token: tokens.refresh_token.is_some(),
        };
        let mut next = self
            .blocking(Self::load_bound_state)
            .await?
            .unwrap_or_else(|| self.empty_state());
        next.tokens = Some(tokens);
        next.discovery = Some(pending.discovery.clone());
        next.pending_scope = None;
        if pending.registered {
            next.client = Some(pending.client.clone());
        }
        self.blocking_save(next.clone()).await?;
        self.adopt(Some(&next));
        *lock(&self.challenge) = None;
        Ok(summary)
    }

    /// Deletes the stored credentials. `true` when there were some.
    pub async fn logout(self: &Arc<Self>) -> Result<bool, McpOAuthError> {
        let removed = self
            .blocking(|this| this.store.clear().map_err(McpOAuthError::Failed))
            .await?;
        self.adopt(None);
        *lock(&self.challenge) = None;
        Ok(removed)
    }
}

/// Tokens after a grant: a refresh keeps the old refresh token and scope when
/// the server does not send new ones.
fn tokens_from_grant(grant: TokenGrant, previous: &OAuthTokens) -> OAuthTokens {
    OAuthTokens {
        access_token: grant.access_token,
        refresh_token: grant
            .refresh_token
            .or_else(|| previous.refresh_token.clone()),
        scope: grant.scope.or_else(|| previous.scope.clone()),
        // `expires_in: 0` carries no usable lifetime (and would make every
        // request refresh): it counts as "not reported".
        expires_at_ms: grant
            .expires_in
            .filter(|seconds| *seconds > 0)
            .map(|seconds| now_ms().saturating_add(seconds.saturating_mul(1000))),
    }
}

/// Shared handle to one server's [`McpAuth`], carried in the server's
/// options. Two handles always compare equal: the handle is derived from the
/// rest of the spec (server name, URL, `oauth` table), so it never decides
/// whether a spec changed.
#[derive(Clone)]
pub struct McpAuthHandle(pub Arc<McpAuth>);

impl PartialEq for McpAuthHandle {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for McpAuthHandle {}

impl fmt::Debug for McpAuthHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("McpAuthHandle")
            .field(&self.0.name)
            .finish()
    }
}
