//! OAuth discovery for MCP servers: `WWW-Authenticate` challenges, RFC 9728
//! protected resource metadata, RFC 8414 / OpenID Connect authorization
//! server metadata with path-aware fallbacks, issuer validation and the
//! https-or-loopback endpoint rule.

use futures_util::StreamExt;
use reqwest::Url;
use serde_json::Value;

use super::types::{AuthServerMetadata, Challenge, DiscoveryState, ProtectedResourceMetadata};
use super::McpOAuthError;
use crate::mcp::MCP_PROTOCOL_VERSION;

/// Largest metadata or token document accepted from an authorization server.
pub(crate) const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// Which endpoints may be plain `http`. Every endpoint must be `https`; a
/// loopback `http` endpoint is also accepted when `allow_loopback_http`
/// (the MCP server itself is on loopback, or the endpoint was configured
/// explicitly by the user).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EndpointPolicy {
    pub allow_loopback_http: bool,
}

pub fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // IPv6 literals come with brackets.
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Parses `raw` and applies the endpoint rule. `what` names the endpoint in
/// the error. The error never contains the URL (it may carry credentials).
pub(crate) fn check_endpoint(
    raw: &str,
    what: &str,
    policy: EndpointPolicy,
) -> Result<Url, McpOAuthError> {
    let url =
        Url::parse(raw).map_err(|_| McpOAuthError::Failed(format!("invalid OAuth {what} URL")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(McpOAuthError::Failed(format!(
            "OAuth {what} URL must not contain credentials"
        )));
    }
    let secure = match url.scheme() {
        "https" => true,
        "http" => policy.allow_loopback_http && is_loopback_host(&url),
        _ => false,
    };
    if !secure {
        return Err(McpOAuthError::Failed(format!(
            "refusing the OAuth {what} {}: it must use https (or loopback http)",
            describe_origin(&url)
        )));
    }
    Ok(url)
}

/// `scheme://host[:port]` without path, query or credentials.
pub(crate) fn describe_origin(url: &Url) -> String {
    let mut origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?"));
    if let Some(port) = url.port() {
        origin.push_str(&format!(":{port}"));
    }
    origin
}

/// Strips control characters and bounds server-provided text.
pub(crate) fn clean_text(text: &str, max_chars: usize) -> String {
    let mut cleaned: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars + 1)
        .collect();
    if cleaned.chars().count() > max_chars {
        cleaned = cleaned.chars().take(max_chars).collect();
        cleaned.push('…');
    }
    cleaned
}

// ---------------------------------------------------------------------------
// WWW-Authenticate
// ---------------------------------------------------------------------------

/// Parses the `Bearer` challenge out of the `WWW-Authenticate` header values
/// of a response (several challenges and several headers are allowed).
pub fn parse_www_authenticate<'a>(values: impl IntoIterator<Item = &'a str>) -> Challenge {
    let joined = values.into_iter().collect::<Vec<_>>().join(", ");
    let bytes = joined.as_bytes();
    let mut position = 0;
    let mut in_bearer = false;
    let mut challenge = Challenge::default();
    let mut found = false;
    while position < bytes.len() {
        while position < bytes.len()
            && (bytes[position] == b',' || bytes[position].is_ascii_whitespace())
        {
            position += 1;
        }
        let start = position;
        while position < bytes.len()
            && !bytes[position].is_ascii_whitespace()
            && bytes[position] != b','
            && bytes[position] != b'='
        {
            position += 1;
        }
        if start == position {
            // Stray `=`: skip it.
            position += 1;
            continue;
        }
        let word = &joined[start..position];
        let mut lookahead = position;
        while lookahead < bytes.len() && bytes[lookahead] == b' ' {
            lookahead += 1;
        }
        if lookahead >= bytes.len() || bytes[lookahead] != b'=' {
            // A scheme name (no `=` after it) starts a new challenge. A
            // token68 value would also land here; it carries no parameters.
            in_bearer = word.eq_ignore_ascii_case("bearer");
            found |= in_bearer;
            continue;
        }
        position = lookahead + 1;
        while position < bytes.len() && bytes[position] == b' ' {
            position += 1;
        }
        let value = if position < bytes.len() && bytes[position] == b'"' {
            position += 1;
            let mut value = String::new();
            let mut escaped = false;
            while position < bytes.len() {
                let ch = joined[position..].chars().next().unwrap_or('"');
                position += ch.len_utf8();
                if escaped {
                    value.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    break;
                } else {
                    value.push(ch);
                }
            }
            value
        } else {
            let value_start = position;
            while position < bytes.len()
                && bytes[position] != b','
                && !bytes[position].is_ascii_whitespace()
            {
                position += 1;
            }
            joined[value_start..position].to_owned()
        };
        if !in_bearer || value.is_empty() {
            continue;
        }
        let slot = match word.to_ascii_lowercase().as_str() {
            "resource_metadata" => &mut challenge.resource_metadata,
            "scope" => &mut challenge.scope,
            "error" => &mut challenge.error,
            "error_description" => &mut challenge.error_description,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value);
        }
    }
    if found {
        challenge
    } else {
        Challenge::default()
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Reads a response body up to `limit` bytes.
pub(crate) async fn read_bounded(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, McpOAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(McpOAuthError::Failed(
            "OAuth server response is too large".into(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| network_error("reading the response", &error))?;
        if bytes.len() + chunk.len() > limit {
            return Err(McpOAuthError::Failed(
                "OAuth server response is too large".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn network_error(context: &str, error: &reqwest::Error) -> McpOAuthError {
    let detail = if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        let mut text = error.to_string();
        if let Some(url) = error.url() {
            text = text.replace(url.as_str(), "<url>");
        }
        clean_text(&text, 160)
    };
    McpOAuthError::Failed(format!("OAuth request failed while {context}: {detail}"))
}

/// GET of a metadata document. `Ok(Err(status))`: the server answered with a
/// non-success status; `Ok(Ok(value))`: a JSON document.
async fn fetch_document(
    http: &reqwest::Client,
    url: &Url,
) -> Result<Result<Value, u16>, McpOAuthError> {
    let response = http
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .send()
        .await
        .map_err(|error| network_error("loading metadata", &error))?;
    let status = response.status();
    if !status.is_success() {
        return Ok(Err(status.as_u16()));
    }
    let bytes = read_bounded(response, MAX_DOCUMENT_BYTES).await?;
    serde_json::from_slice(&bytes)
        .map(Ok)
        .map_err(|_| McpOAuthError::Failed("OAuth metadata is not valid JSON".into()))
}

/// 4xx and 502 mean "not here": discovery tries the next candidate.
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

fn path_suffix(url: &Url) -> String {
    let path = url.path();
    path.strip_suffix('/').unwrap_or(path).to_owned()
}

fn origin_url(url: &Url, path: &str) -> Result<Url, McpOAuthError> {
    let mut origin = url.clone();
    origin.set_path(path);
    origin.set_query(None);
    origin.set_fragment(None);
    Ok(origin)
}

// ---------------------------------------------------------------------------
// Protected resource metadata (RFC 9728)
// ---------------------------------------------------------------------------

/// Candidate URLs of the protected resource metadata, most specific first.
pub(crate) fn protected_resource_candidates(
    server: &Url,
    challenge_url: Option<&Url>,
) -> Result<Vec<Url>, McpOAuthError> {
    if let Some(url) = challenge_url {
        return Ok(vec![url.clone()]);
    }
    let mut candidates = vec![origin_url(
        server,
        &format!(
            "/.well-known/oauth-protected-resource{}",
            path_suffix(server)
        ),
    )?];
    if server.path() != "/" {
        candidates.push(origin_url(server, "/.well-known/oauth-protected-resource")?);
    }
    Ok(candidates)
}

async fn discover_protected_resource(
    http: &reqwest::Client,
    server: &Url,
    challenge_url: Option<&Url>,
) -> Result<Option<ProtectedResourceMetadata>, McpOAuthError> {
    for candidate in protected_resource_candidates(server, challenge_url)? {
        match fetch_document(http, &candidate).await {
            Ok(Ok(document)) => return Ok(Some(ProtectedResourceMetadata::parse(&document)?)),
            Ok(Err(status)) if is_discovery_miss(status) => continue,
            Ok(Err(_)) | Err(_) => return Ok(None),
        }
    }
    Ok(None)
}

/// RFC 8707 resource for the server: the metadata's `resource`, which must
/// cover the server URL (same origin, path prefix).
pub(crate) fn select_resource(
    server: &Url,
    metadata: &ProtectedResourceMetadata,
) -> Result<String, McpOAuthError> {
    let mismatch = || {
        McpOAuthError::Failed(
            "the protected resource metadata does not describe this MCP server".into(),
        )
    };
    let configured = Url::parse(&metadata.resource).map_err(|_| mismatch())?;
    if configured.origin() != server.origin() {
        return Err(mismatch());
    }
    let with_slash = |path: &str| {
        if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        }
    };
    if !with_slash(server.path()).starts_with(&with_slash(configured.path())) {
        return Err(mismatch());
    }
    Ok(metadata.resource.clone())
}

// ---------------------------------------------------------------------------
// Authorization server metadata (RFC 8414 / OIDC)
// ---------------------------------------------------------------------------

/// Metadata URLs to try for an issuer, in order (RFC 8414 §3.1 with the OIDC
/// path-append fallback).
pub(crate) fn authorization_server_candidates(issuer: &Url) -> Result<Vec<Url>, McpOAuthError> {
    let path = path_suffix(issuer);
    let mut candidates = vec![
        origin_url(
            issuer,
            &format!("/.well-known/oauth-authorization-server{path}"),
        )?,
        origin_url(issuer, &format!("/.well-known/openid-configuration{path}"))?,
    ];
    if !path.is_empty() {
        candidates.push(origin_url(
            issuer,
            &format!("{path}/.well-known/openid-configuration"),
        )?);
    }
    Ok(candidates)
}

fn same_issuer(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

async fn discover_authorization_server(
    http: &reqwest::Client,
    issuer: &Url,
    policy: EndpointPolicy,
) -> Result<Option<AuthServerMetadata>, McpOAuthError> {
    for candidate in authorization_server_candidates(issuer)? {
        match fetch_document(http, &candidate).await? {
            Ok(document) => {
                let metadata = AuthServerMetadata::parse(&document)?;
                if !same_issuer(&metadata.issuer, issuer.as_str()) {
                    return Err(McpOAuthError::Failed(format!(
                        "OAuth issuer mismatch: expected {}, the server reports {}",
                        describe_origin(issuer),
                        clean_text(&metadata.issuer, 120)
                    )));
                }
                check_metadata_endpoints(&metadata, policy)?;
                return Ok(Some(metadata));
            }
            Err(status) if is_discovery_miss(status) => continue,
            Err(status) => {
                return Err(McpOAuthError::Failed(format!(
                    "authorization server metadata request failed with HTTP {status}"
                )));
            }
        }
    }
    Ok(None)
}

/// Every endpoint the client will talk to must be acceptable.
pub(crate) fn check_metadata_endpoints(
    metadata: &AuthServerMetadata,
    policy: EndpointPolicy,
) -> Result<(), McpOAuthError> {
    check_endpoint(
        &metadata.authorization_endpoint,
        "authorization endpoint",
        policy,
    )?;
    check_endpoint(&metadata.token_endpoint, "token endpoint", policy)?;
    if let Some(registration) = &metadata.registration_endpoint {
        check_endpoint(registration, "registration endpoint", policy)?;
    }
    Ok(())
}

/// Metadata for an authorization server that publishes none: the default
/// endpoint paths of the original MCP authorization draft.
fn fallback_metadata(issuer: &Url) -> Result<AuthServerMetadata, McpOAuthError> {
    let endpoint = |path: &str| origin_url(issuer, path).map(String::from);
    Ok(AuthServerMetadata {
        issuer: issuer.as_str().trim_end_matches('/').to_owned(),
        authorization_endpoint: endpoint("/authorize")?,
        token_endpoint: endpoint("/token")?,
        registration_endpoint: Some(endpoint("/register")?),
        scopes_supported: Vec::new(),
        response_types_supported: Vec::new(),
        token_endpoint_auth_methods_supported: Vec::new(),
        code_challenge_methods_supported: None,
        authorization_response_iss_parameter_supported: false,
    })
}

/// What discovery needs to know about the server and its challenge.
pub(crate) struct DiscoveryRequest<'a> {
    pub server: &'a Url,
    /// `resource_metadata` of the last `WWW-Authenticate` challenge.
    pub resource_metadata: Option<&'a str>,
    /// `oauth.auth_server_metadata_url`: used instead of discovery. Trusted
    /// as configured, so its issuer is not compared.
    pub metadata_override: Option<&'a str>,
}

pub(crate) async fn discover(
    http: &reqwest::Client,
    request: &DiscoveryRequest<'_>,
    policy: EndpointPolicy,
) -> Result<DiscoveryState, McpOAuthError> {
    let challenge_url = request
        .resource_metadata
        .map(|raw| check_endpoint(raw, "protected resource metadata", policy))
        .transpose()?;
    let resource_metadata =
        discover_protected_resource(http, request.server, challenge_url.as_ref()).await?;
    let resource = match &resource_metadata {
        Some(metadata) => select_resource(request.server, metadata)?,
        None => {
            let mut canonical = request.server.clone();
            canonical.set_fragment(None);
            canonical.to_string()
        }
    };
    let resource_scopes = resource_metadata
        .as_ref()
        .map(|metadata| metadata.scopes_supported.clone())
        .unwrap_or_default();
    if let Some(raw) = request.metadata_override {
        // A configured URL is the user's own choice: loopback http is fine.
        let url = check_endpoint(
            raw,
            "authorization server metadata",
            EndpointPolicy {
                allow_loopback_http: true,
            },
        )?;
        let document = fetch_document(http, &url).await?.map_err(|status| {
            McpOAuthError::Failed(format!(
                "authorization server metadata request failed with HTTP {status}"
            ))
        })?;
        let metadata = AuthServerMetadata::parse(&document)?;
        check_metadata_endpoints(
            &metadata,
            EndpointPolicy {
                allow_loopback_http: true,
            },
        )?;
        return Ok(DiscoveryState {
            authorization_server_url: metadata.issuer.clone(),
            metadata,
            resource,
            resource_scopes,
        });
    }
    let issuer_text = match resource_metadata
        .as_ref()
        .and_then(|metadata| metadata.authorization_servers.first())
    {
        Some(url) => url.clone(),
        None => origin_url(request.server, "/")?.to_string(),
    };
    let issuer = check_endpoint(&issuer_text, "authorization server", policy)?;
    let metadata = match discover_authorization_server(http, &issuer, policy).await? {
        Some(metadata) => metadata,
        None => {
            let metadata = fallback_metadata(&issuer)?;
            check_metadata_endpoints(&metadata, policy)?;
            metadata
        }
    };
    Ok(DiscoveryState {
        authorization_server_url: issuer_text,
        metadata,
        resource,
        resource_scopes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict() -> EndpointPolicy {
        EndpointPolicy {
            allow_loopback_http: false,
        }
    }

    fn loopback_ok() -> EndpointPolicy {
        EndpointPolicy {
            allow_loopback_http: true,
        }
    }

    #[test]
    fn parses_bearer_challenges_with_quoted_and_bare_values() {
        let challenge = parse_www_authenticate([
            r#"Bearer realm="mcp", resource_metadata="https://x.example/.well-known/oauth-protected-resource/mcp", scope="read write", error=insufficient_scope, error_description="needs \"write\"""#,
        ]);
        assert_eq!(
            challenge.resource_metadata.as_deref(),
            Some("https://x.example/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(challenge.scope.as_deref(), Some("read write"));
        assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(
            challenge.error_description.as_deref(),
            Some("needs \"write\"")
        );
    }

    #[test]
    fn ignores_other_schemes_and_empty_values() {
        assert_eq!(
            parse_www_authenticate([r#"Basic realm="x", scope="nope""#]),
            Challenge::default()
        );
        assert_eq!(parse_www_authenticate([]), Challenge::default());
        let challenge = parse_www_authenticate([r#"Bearer scope="", error="invalid_token""#]);
        assert_eq!(challenge.scope, None);
        assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
    }

    #[test]
    fn picks_the_bearer_challenge_among_several() {
        let challenge = parse_www_authenticate([
            r#"Basic realm="legacy""#,
            r#"Bearer error="invalid_token", scope="a b""#,
        ]);
        assert_eq!(challenge.scope.as_deref(), Some("a b"));
        let single = parse_www_authenticate([
            r#"Basic realm="legacy", scope="wrong", Bearer scope="right""#,
        ]);
        assert_eq!(single.scope.as_deref(), Some("right"));
    }

    #[test]
    fn endpoints_must_be_https_or_loopback_http() {
        assert!(check_endpoint("https://auth.example/token", "token", strict()).is_ok());
        assert!(check_endpoint("http://auth.example/token", "token", loopback_ok()).is_err());
        assert!(check_endpoint("http://127.0.0.1:9/token", "token", strict()).is_err());
        assert!(check_endpoint("http://127.0.0.1:9/token", "token", loopback_ok()).is_ok());
        assert!(check_endpoint("http://localhost:9/token", "token", loopback_ok()).is_ok());
        assert!(check_endpoint("http://[::1]:9/token", "token", loopback_ok()).is_ok());
        assert!(check_endpoint("ftp://auth.example/token", "token", loopback_ok()).is_err());
        assert!(check_endpoint("javascript:alert(1)", "token", loopback_ok()).is_err());
        let credentials = check_endpoint("https://u:p@auth.example/", "token", strict())
            .expect_err("credentials refused");
        assert!(!credentials.to_string().contains("p@"), "{credentials}");
    }

    #[test]
    fn refusals_do_not_echo_credentials_or_paths() {
        let error = check_endpoint(
            "http://evil.example/secret/path?token=abc",
            "token",
            strict(),
        )
        .expect_err("refused");
        let text = error.to_string();
        assert!(text.contains("evil.example"), "{text}");
        assert!(!text.contains("abc") && !text.contains("secret"), "{text}");
    }

    #[test]
    fn protected_resource_candidates_are_path_aware() {
        let server = Url::parse("https://api.example/v1/mcp").unwrap();
        let urls: Vec<String> = protected_resource_candidates(&server, None)
            .unwrap()
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            urls,
            [
                "https://api.example/.well-known/oauth-protected-resource/v1/mcp",
                "https://api.example/.well-known/oauth-protected-resource",
            ]
        );
        let root = Url::parse("https://api.example/").unwrap();
        assert_eq!(protected_resource_candidates(&root, None).unwrap().len(), 1);
        let challenge = Url::parse("https://api.example/meta").unwrap();
        assert_eq!(
            protected_resource_candidates(&server, Some(&challenge)).unwrap(),
            vec![challenge]
        );
    }

    #[test]
    fn authorization_server_candidates_follow_rfc_8414_then_oidc() {
        let bare = Url::parse("https://auth.example").unwrap();
        let urls: Vec<String> = authorization_server_candidates(&bare)
            .unwrap()
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            urls,
            [
                "https://auth.example/.well-known/oauth-authorization-server",
                "https://auth.example/.well-known/openid-configuration",
            ]
        );
        let tenant = Url::parse("https://auth.example/tenant/1").unwrap();
        let urls: Vec<String> = authorization_server_candidates(&tenant)
            .unwrap()
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            urls,
            [
                "https://auth.example/.well-known/oauth-authorization-server/tenant/1",
                "https://auth.example/.well-known/openid-configuration/tenant/1",
                "https://auth.example/tenant/1/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn the_resource_must_cover_the_server_url() {
        let server = Url::parse("https://api.example/v1/mcp").unwrap();
        let metadata = |resource: &str| ProtectedResourceMetadata {
            resource: resource.to_owned(),
            ..ProtectedResourceMetadata::default()
        };
        assert_eq!(
            select_resource(&server, &metadata("https://api.example/v1")).unwrap(),
            "https://api.example/v1"
        );
        assert_eq!(
            select_resource(&server, &metadata("https://api.example/")).unwrap(),
            "https://api.example/"
        );
        assert!(select_resource(&server, &metadata("https://other.example/v1")).is_err());
        assert!(select_resource(&server, &metadata("https://api.example/v2")).is_err());
        assert!(select_resource(&server, &metadata("https://api.example/v1/mcp/deeper")).is_err());
    }

    #[test]
    fn issuers_compare_without_a_trailing_slash() {
        assert!(same_issuer("https://a.example", "https://a.example/"));
        assert!(same_issuer("https://a.example/t", "https://a.example/t/"));
        assert!(!same_issuer("https://a.example/t", "https://a.example/u"));
    }

    #[test]
    fn metadata_parsing_validates_urls_and_types() {
        let good = serde_json::json!({
            "issuer": "https://auth.example",
            "authorization_endpoint": "https://auth.example/authorize",
            "token_endpoint": "https://auth.example/token",
            "registration_endpoint": "https://auth.example/register",
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "authorization_response_iss_parameter_supported": true,
        });
        let metadata = AuthServerMetadata::parse(&good).unwrap();
        assert!(metadata.authorization_response_iss_parameter_supported);
        assert_eq!(
            metadata.code_challenge_methods_supported,
            Some(vec!["S256".to_owned()])
        );
        for broken in [
            serde_json::json!({"issuer": "javascript:x", "authorization_endpoint": "https://a/", "token_endpoint": "https://a/"}),
            serde_json::json!({"issuer": "https://a/", "authorization_endpoint": 5, "token_endpoint": "https://a/"}),
            serde_json::json!({"issuer": "https://a/", "authorization_endpoint": "https://a/", "token_endpoint": "https://a/", "response_types_supported": "code"}),
            serde_json::json!([]),
        ] {
            assert!(AuthServerMetadata::parse(&broken).is_err(), "{broken}");
        }
        // `null` and empty optionals count as absent.
        let sparse = serde_json::json!({
            "issuer": "https://a.example",
            "authorization_endpoint": "https://a.example/a",
            "token_endpoint": "https://a.example/t",
            "registration_endpoint": "",
            "scopes_supported": null,
        });
        let metadata = AuthServerMetadata::parse(&sparse).unwrap();
        assert_eq!(metadata.registration_endpoint, None);
        assert!(metadata.scopes_supported.is_empty());
        assert_eq!(metadata.code_challenge_methods_supported, None);
    }

    #[test]
    fn clean_text_strips_controls_and_bounds_length() {
        assert_eq!(clean_text("a\u{1b}[31mb\nc", 50), "a[31mbc");
        let long = "x".repeat(400);
        let cleaned = clean_text(&long, 100);
        assert_eq!(cleaned.chars().count(), 101);
        assert!(cleaned.ends_with('…'));
    }
}
