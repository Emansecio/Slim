//! Authorization code flow pieces: PKCE S256, dynamic client registration,
//! the authorization URL, client authentication and the token endpoint.

use reqwest::Url;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::discovery::{
    check_endpoint, clean_text, network_error, read_bounded, EndpointPolicy, MAX_DOCUMENT_BYTES,
};
use super::types::{AuthServerMetadata, OAuthClient};
use super::McpOAuthError;

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url (RFC 4648 §5).
pub(crate) fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(BASE64URL[(group >> 18) as usize & 63] as char);
        out.push(BASE64URL[(group >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(BASE64URL[(group >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64URL[group as usize & 63] as char);
        }
    }
    out
}

/// PKCE S256 challenge (RFC 7636): `BASE64URL(SHA256(verifier))`.
pub fn pkce_challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// Verifiers are 43 to 128 characters of the unreserved set (RFC 7636 §4.1).
pub(crate) fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

/// `application/x-www-form-urlencoded` encoding of one value (RFC 6749
/// appendix B), used for the Basic credentials.
pub(crate) fn form_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Space-separated scopes of every list, each once, in order.
pub fn merge_scopes<'a>(scopes: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    let mut merged: Vec<&str> = Vec::new();
    for scope in scopes.into_iter().flatten() {
        for item in scope.split_whitespace() {
            if !merged.contains(&item) {
                merged.push(item);
            }
        }
    }
    (!merged.is_empty()).then(|| merged.join(" "))
}

/// Scopes for a step-up authorization: the challenged scopes plus the ones
/// granted so far (a challenge may list only the missing ones, and a token
/// with just those would lose access the old one had). `None` without
/// challenged scopes.
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    challenged.filter(|scope| !scope.trim().is_empty())?;
    merge_scopes([granted, challenged])
}

// ---------------------------------------------------------------------------
// Authorization request
// ---------------------------------------------------------------------------

pub(crate) struct AuthorizationParams<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub scope: Option<&'a str>,
    pub state: &'a str,
    pub code_challenge: &'a str,
    pub resource: &'a str,
}

/// Checks the server can run the code flow with PKCE S256 and builds the
/// authorization URL.
pub(crate) fn authorization_url(
    metadata: &AuthServerMetadata,
    params: &AuthorizationParams<'_>,
) -> Result<String, McpOAuthError> {
    if !metadata.response_types_supported.is_empty()
        && !metadata
            .response_types_supported
            .iter()
            .any(|kind| kind == "code")
    {
        return Err(McpOAuthError::Failed(
            "the authorization server does not support the authorization code flow".into(),
        ));
    }
    if let Some(methods) = &metadata.code_challenge_methods_supported {
        if !methods.iter().any(|method| method == "S256") {
            return Err(McpOAuthError::Failed(
                "the authorization server does not support PKCE S256".into(),
            ));
        }
    }
    let mut url = Url::parse(&metadata.authorization_endpoint)
        .map_err(|_| McpOAuthError::Failed("invalid OAuth authorization endpoint URL".into()))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", params.client_id)
            .append_pair("code_challenge", params.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", params.redirect_uri)
            .append_pair("state", params.state)
            .append_pair("resource", params.resource);
        if let Some(scope) = params.scope {
            query.append_pair("scope", scope);
            if scope
                .split_whitespace()
                .any(|item| item == "offline_access")
            {
                query.append_pair("prompt", "consent");
            }
        }
    }
    Ok(url.into())
}

// ---------------------------------------------------------------------------
// Client authentication
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientAuth {
    Basic,
    Post,
    None,
}

/// The token endpoint authentication method for `client`.
pub(crate) fn select_client_auth(client: &OAuthClient, supported: &[String]) -> ClientAuth {
    let offered = |method: &str| supported.iter().any(|item| item == method);
    let has_secret = client.client_secret.is_some();
    if let Some(hinted) = client.token_endpoint_auth_method.as_deref() {
        let method = match hinted {
            "client_secret_basic" => Some(ClientAuth::Basic),
            "client_secret_post" => Some(ClientAuth::Post),
            "none" => Some(ClientAuth::None),
            _ => None,
        };
        if let Some(method) = method {
            let usable = supported.is_empty() || offered(hinted);
            let needs_secret = method != ClientAuth::None;
            if usable && (!needs_secret || has_secret) {
                return method;
            }
        }
    }
    if supported.is_empty() {
        return if has_secret {
            ClientAuth::Basic
        } else {
            ClientAuth::None
        };
    }
    if has_secret && offered("client_secret_basic") {
        ClientAuth::Basic
    } else if has_secret && offered("client_secret_post") {
        ClientAuth::Post
    } else if offered("none") {
        ClientAuth::None
    } else if has_secret {
        ClientAuth::Post
    } else {
        ClientAuth::None
    }
}

// ---------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------

/// A successful token response.
#[derive(Clone)]
pub(crate) struct TokenGrant {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
    pub expires_in: Option<u64>,
}

fn oauth_error_of(value: &Value) -> Option<McpOAuthError> {
    let code = value.get("error")?.as_str()?;
    let description = value
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or(code);
    Some(McpOAuthError::Server {
        code: clean_text(code, 64),
        description: clean_text(description, 300),
    })
}

fn parse_grant(value: &Value) -> Result<TokenGrant, McpOAuthError> {
    let invalid =
        |what: &str| McpOAuthError::Failed(format!("invalid OAuth token response: {what}"));
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| invalid("access_token"))?;
    let token_type = value
        .get("token_type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("token_type"))?;
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(invalid("token_type is not Bearer"));
    }
    let expires_in = match value.get("expires_in") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => Some(
            number
                .as_u64()
                .or_else(|| number.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
                .ok_or_else(|| invalid("expires_in"))?,
        ),
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.parse().map_err(|_| invalid("expires_in"))?),
        Some(_) => return Err(invalid("expires_in")),
    };
    let optional = |field: &str| -> Option<String> {
        value
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Ok(TokenGrant {
        access_token: access_token.to_owned(),
        refresh_token: optional("refresh_token"),
        scope: optional("scope"),
        expires_in,
    })
}

/// What every token request needs.
#[derive(Clone, Copy)]
pub(crate) struct TokenContext<'a> {
    pub http: &'a reqwest::Client,
    pub metadata: &'a AuthServerMetadata,
    pub policy: EndpointPolicy,
    pub client: &'a OAuthClient,
    /// RFC 8707 resource.
    pub resource: &'a str,
}

/// POSTs a token request. OAuth errors are reported from the body whatever
/// the status (servers use several), other failures by status only.
async fn token_request(
    context: TokenContext<'_>,
    mut params: Vec<(&'static str, String)>,
) -> Result<TokenGrant, McpOAuthError> {
    let TokenContext {
        http,
        metadata,
        policy,
        client,
        resource,
    } = context;
    let endpoint = check_endpoint(&metadata.token_endpoint, "token endpoint", policy)?;
    params.push(("resource", resource.to_owned()));
    let mut request = http
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json");
    match select_client_auth(client, &metadata.token_endpoint_auth_methods_supported) {
        ClientAuth::Basic => {
            let secret = client.client_secret.as_deref().unwrap_or_default();
            request = request.basic_auth(form_encode(&client.client_id), Some(form_encode(secret)));
        }
        ClientAuth::Post => {
            params.push(("client_id", client.client_id.clone()));
            if let Some(secret) = &client.client_secret {
                params.push(("client_secret", secret.clone()));
            }
        }
        ClientAuth::None => params.push(("client_id", client.client_id.clone())),
    }
    let response = request
        .form(&params)
        .send()
        .await
        .map_err(|error| network_error("contacting the token endpoint", &error))?;
    let status = response.status();
    let bytes = read_bounded(response, MAX_DOCUMENT_BYTES).await?;
    let value: Option<Value> = serde_json::from_slice(&bytes).ok();
    if let Some(error) = value.as_ref().and_then(oauth_error_of) {
        return Err(error);
    }
    if !status.is_success() {
        return Err(McpOAuthError::Failed(format!(
            "token endpoint answered HTTP {}",
            status.as_u16()
        )));
    }
    let value = value
        .ok_or_else(|| McpOAuthError::Failed("token endpoint answered with invalid JSON".into()))?;
    parse_grant(&value)
}

pub(crate) async fn exchange_code(
    context: TokenContext<'_>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenGrant, McpOAuthError> {
    token_request(
        context,
        vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code.to_owned()),
            ("code_verifier", verifier.to_owned()),
            ("redirect_uri", redirect_uri.to_owned()),
        ],
    )
    .await
}

pub(crate) async fn refresh_grant(
    context: TokenContext<'_>,
    refresh_token: &str,
) -> Result<TokenGrant, McpOAuthError> {
    token_request(
        context,
        vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.to_owned()),
        ],
    )
    .await
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

pub(crate) async fn register_client(
    http: &reqwest::Client,
    metadata: &AuthServerMetadata,
    policy: EndpointPolicy,
    client_name: &str,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<OAuthClient, McpOAuthError> {
    let Some(endpoint) = metadata.registration_endpoint.as_deref() else {
        return Err(McpOAuthError::Failed(
            "the authorization server does not support dynamic client registration; \
             set oauth.client_id for this server"
                .into(),
        ));
    };
    let endpoint = check_endpoint(endpoint, "registration endpoint", policy)?;
    let mut body = json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(scope) = scope {
        body["scope"] = json!(scope);
    }
    let response = http
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| network_error("registering the client", &error))?;
    let status = response.status();
    let bytes = read_bounded(response, MAX_DOCUMENT_BYTES).await?;
    let value: Option<Value> = serde_json::from_slice(&bytes).ok();
    if !status.is_success() {
        let detail = value
            .as_ref()
            .and_then(oauth_error_of)
            .map(|error| format!(": {error}"))
            .unwrap_or_default();
        return Err(McpOAuthError::Failed(format!(
            "dynamic client registration failed with HTTP {}{detail}",
            status.as_u16()
        )));
    }
    let value = value.ok_or_else(|| {
        McpOAuthError::Failed("client registration answered with invalid JSON".into())
    })?;
    let client_id = value
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            McpOAuthError::Failed("client registration did not return a client_id".into())
        })?;
    Ok(OAuthClient {
        client_id: client_id.to_owned(),
        client_secret: value
            .get("client_secret")
            .and_then(Value::as_str)
            .filter(|secret| !secret.is_empty())
            .map(str::to_owned),
        redirect_uris: vec![redirect_uri.to_owned()],
        token_endpoint_auth_method: value
            .get("token_endpoint_auth_method")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some("none".to_owned())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> AuthServerMetadata {
        AuthServerMetadata {
            issuer: "https://auth.example".into(),
            authorization_endpoint: "https://auth.example/authorize?tenant=1".into(),
            token_endpoint: "https://auth.example/token".into(),
            registration_endpoint: None,
            scopes_supported: vec![],
            response_types_supported: vec!["code".into()],
            token_endpoint_auth_methods_supported: vec![],
            code_challenge_methods_supported: Some(vec!["S256".into()]),
            authorization_response_iss_parameter_supported: false,
        }
    }

    #[test]
    fn pkce_challenge_matches_the_rfc_7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn base64url_handles_every_remainder() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn verifiers_follow_rfc_7636() {
        assert!(valid_verifier(&"a".repeat(43)));
        assert!(valid_verifier(&"a~._-".repeat(20)));
        assert!(!valid_verifier(&"a".repeat(42)));
        assert!(!valid_verifier(&"a".repeat(129)));
        assert!(!valid_verifier(&format!("{}!", "a".repeat(50))));
    }

    #[test]
    fn form_encoding_follows_rfc_6749_appendix_b() {
        assert_eq!(form_encode("abc-._*09"), "abc-._*09");
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(form_encode("p@ss:w/rd%"), "p%40ss%3Aw%2Frd%25");
        assert_eq!(form_encode("é"), "%C3%A9");
    }

    #[test]
    fn scopes_merge_without_duplicates_and_step_up_keeps_granted_scopes() {
        assert_eq!(
            merge_scopes([Some("a b"), None, Some("b c")]).as_deref(),
            Some("a b c")
        );
        assert_eq!(merge_scopes([None, Some("  ")]), None);
        assert_eq!(
            step_up_scope(Some("read"), Some("write")).as_deref(),
            Some("read write")
        );
        assert_eq!(step_up_scope(Some("read"), None), None);
        assert_eq!(step_up_scope(Some("read"), Some(" ")), None);
    }

    #[test]
    fn the_authorization_url_carries_pkce_state_resource_and_scope() {
        let url = authorization_url(
            &metadata(),
            &AuthorizationParams {
                client_id: "cid",
                redirect_uri: "http://127.0.0.1:5000/callback",
                scope: Some("read offline_access"),
                state: "st",
                code_challenge: "chal",
                resource: "https://api.example/mcp",
            },
        )
        .unwrap();
        let url = Url::parse(&url).unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["tenant"], "1");
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "cid");
        assert_eq!(query["code_challenge"], "chal");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["redirect_uri"], "http://127.0.0.1:5000/callback");
        assert_eq!(query["state"], "st");
        assert_eq!(query["resource"], "https://api.example/mcp");
        assert_eq!(query["scope"], "read offline_access");
        assert_eq!(query["prompt"], "consent");
    }

    #[test]
    fn a_server_without_s256_or_the_code_flow_is_refused() {
        let params = AuthorizationParams {
            client_id: "c",
            redirect_uri: "http://127.0.0.1:1/callback",
            scope: None,
            state: "s",
            code_challenge: "x",
            resource: "https://r",
        };
        let mut plain_only = metadata();
        plain_only.code_challenge_methods_supported = Some(vec!["plain".into()]);
        let error = authorization_url(&plain_only, &params).unwrap_err();
        assert!(error.to_string().contains("S256"), "{error}");
        let mut implicit = metadata();
        implicit.response_types_supported = vec!["token".into()];
        assert!(authorization_url(&implicit, &params).is_err());
        // A document that does not mention PKCE support is accepted (the
        // challenge is always sent); one that lists S256 among others too.
        let mut unspecified = metadata();
        unspecified.code_challenge_methods_supported = None;
        assert!(authorization_url(&unspecified, &params).is_ok());
        let mut both = metadata();
        both.code_challenge_methods_supported = Some(vec!["plain".into(), "S256".into()]);
        assert!(authorization_url(&both, &params).is_ok());
    }

    #[test]
    fn client_auth_method_follows_the_server_and_the_secret() {
        let public = OAuthClient {
            client_id: "c".into(),
            client_secret: None,
            redirect_uris: vec![],
            token_endpoint_auth_method: None,
        };
        let confidential = OAuthClient {
            client_secret: Some("s".into()),
            ..public.clone()
        };
        let list = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(select_client_auth(&public, &[]), ClientAuth::None);
        assert_eq!(select_client_auth(&confidential, &[]), ClientAuth::Basic);
        assert_eq!(
            select_client_auth(&confidential, &list(&["client_secret_post", "none"])),
            ClientAuth::Post
        );
        assert_eq!(
            select_client_auth(
                &confidential,
                &list(&["client_secret_basic", "client_secret_post"])
            ),
            ClientAuth::Basic
        );
        assert_eq!(
            select_client_auth(&public, &list(&["client_secret_basic", "none"])),
            ClientAuth::None
        );
        let hinted = OAuthClient {
            token_endpoint_auth_method: Some("client_secret_post".into()),
            ..confidential
        };
        assert_eq!(
            select_client_auth(
                &hinted,
                &list(&["client_secret_basic", "client_secret_post"])
            ),
            ClientAuth::Post
        );
        // A hint the server does not offer is ignored.
        assert_eq!(
            select_client_auth(&hinted, &list(&["client_secret_basic"])),
            ClientAuth::Basic
        );
        // A hint needing a secret the client lacks is ignored.
        let no_secret = OAuthClient {
            token_endpoint_auth_method: Some("client_secret_basic".into()),
            ..public
        };
        assert_eq!(select_client_auth(&no_secret, &[]), ClientAuth::None);
    }

    #[test]
    fn token_responses_are_validated() {
        let ok = parse_grant(&json!({
            "access_token": "a", "token_type": "bearer", "expires_in": "3600",
            "refresh_token": "r", "scope": "x y"
        }))
        .unwrap();
        assert_eq!(ok.expires_in, Some(3600));
        assert_eq!(ok.refresh_token.as_deref(), Some("r"));
        assert_eq!(ok.scope.as_deref(), Some("x y"));
        let null_expiry =
            parse_grant(&json!({"access_token": "a", "token_type": "Bearer", "expires_in": null}))
                .unwrap();
        assert_eq!(null_expiry.expires_in, None);
        for broken in [
            json!({"token_type": "bearer"}),
            json!({"access_token": "", "token_type": "bearer"}),
            json!({"access_token": "a"}),
            json!({"access_token": "a", "token_type": "DPoP"}),
            json!({"access_token": "a", "token_type": "bearer", "expires_in": "soon"}),
            json!({"access_token": "a", "token_type": "bearer", "expires_in": []}),
        ] {
            assert!(parse_grant(&broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn oauth_errors_come_from_the_body_and_are_bounded() {
        let error = oauth_error_of(
            &json!({"error": "invalid_grant", "error_description": "x".repeat(900)}),
        )
        .expect("error");
        let McpOAuthError::Server { code, description } = error else {
            panic!("server error");
        };
        assert_eq!(code, "invalid_grant");
        assert!(description.chars().count() <= 301, "{}", description.len());
        assert!(oauth_error_of(&json!({"access_token": "a"})).is_none());
    }
}
