//! Loopback HTTP server for OAuth tests: one origin that plays the MCP
//! resource server, the protected resource metadata, the authorization
//! server (metadata, dynamic client registration, authorization and token
//! endpoints) in whatever combination a test scripts. Every request is
//! recorded; a handler maps a request to a response.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct HttpReq {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Lower-cased header names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpReq {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// `application/x-www-form-urlencoded` body.
    pub fn form(&self) -> BTreeMap<String, String> {
        decode_pairs(&String::from_utf8_lossy(&self.body))
    }

    pub fn query_pairs(&self) -> BTreeMap<String, String> {
        decode_pairs(&self.query)
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn rpc_method(&self) -> String {
        self.json()
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    /// Bearer token of the `Authorization` header.
    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")?.strip_prefix("Bearer ")
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn decode_pairs(text: &str) -> BTreeMap<String, String> {
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (percent_decode(name), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: value.to_string().into_bytes(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn raw(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body,
        }
    }
}

pub struct Server {
    /// `http://127.0.0.1:<port>`
    pub url: String,
    requests: Arc<Mutex<Vec<HttpReq>>>,
}

impl Server {
    /// The handler receives the server's own URL, so a response can point
    /// back to it. Binding happens first, then `make` builds the handler.
    pub fn spawn<H>(make: impl FnOnce(&str) -> H) -> Self
    where
        H: Fn(&HttpReq) -> Resp + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let handler = Arc::new(make(&url));
        let requests: Arc<Mutex<Vec<HttpReq>>> = Arc::default();
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log);
                thread::spawn(move || serve_one(stream, &*handler, &log));
            }
        });
        Self { url, requests }
    }

    pub fn requests(&self) -> Vec<HttpReq> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Requests whose path is `path`.
    pub fn hits(&self, path: &str) -> Vec<HttpReq> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }

    pub fn count(&self, path: &str) -> usize {
        self.hits(path).len()
    }

    /// Waits until `probe` holds over the recorded requests.
    pub fn wait_for(&self, what: &str, probe: impl Fn(&[HttpReq]) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !probe(&self.requests()) {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn read_request(stream: &TcpStream) -> Option<HttpReq> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), query.to_owned()),
        None => (target, String::new()),
    };
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(HttpReq {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn serve_one(
    mut stream: TcpStream,
    handler: &(dyn Fn(&HttpReq) -> Resp + Send + Sync),
    log: &Mutex<Vec<HttpReq>>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let Some(request) = read_request(&stream) else {
        return;
    };
    log.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(request.clone());
    let response = handler(&request);
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

/// JSON-RPC answer of a minimal MCP server: `initialize`, `tools/list` (one
/// tool, `echo`) and `tools/call`. `None` for notifications (answer 202).
pub fn mcp_reply(request: &HttpReq) -> Resp {
    let body = request.json();
    let Some(id) = body.get("id").cloned() else {
        return Resp::status(202);
    };
    let result = match body["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "oauth-mock", "version": "1"},
        }),
        "tools/list" => json!({"tools": [{
            "name": "echo",
            "description": "echo",
            "inputSchema": {"type": "object", "properties": {}},
        }]}),
        "tools/call" => json!({"content": [{"type": "text", "text": "pong"}]}),
        _ => json!({}),
    };
    Resp::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

// ---------------------------------------------------------------------------
// A scripted world: MCP server + protected resource metadata + authorization server
// ---------------------------------------------------------------------------

/// What the mock authorization server does; tests flip these.
pub struct Knobs {
    pub valid_access: HashSet<String>,
    pub valid_refresh: HashSet<String>,
    pub counter: u32,
    pub expires_in: u64,
    pub rotate: bool,
    /// OAuth error code the token endpoint answers to a refresh.
    pub reject_refresh: Option<&'static str>,
    /// Make the MCP endpoint answer without authentication.
    pub public: bool,
    pub advertise_s256: bool,
    pub advertise_iss: bool,
    pub metadata_issuer: Option<String>,
    pub token_endpoint_override: Option<String>,
    pub token_auth_methods: Vec<&'static str>,
    pub resource_scopes: Option<Vec<&'static str>>,
    pub registration: bool,
    /// `tools/call` answers 403 `insufficient_scope` with this scope.
    pub call_needs_scope: Option<&'static str>,
    pub token_response_padding: usize,
    pub token_redirect: bool,
    /// `tools/call` answers a plain 403.
    pub plain_403: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            valid_access: HashSet::new(),
            valid_refresh: HashSet::new(),
            counter: 0,
            expires_in: 3600,
            rotate: true,
            reject_refresh: None,
            public: false,
            advertise_s256: true,
            advertise_iss: false,
            metadata_issuer: None,
            token_endpoint_override: None,
            token_auth_methods: vec!["none"],
            resource_scopes: None,
            registration: true,
            call_needs_scope: None,
            token_response_padding: 0,
            token_redirect: false,
            plain_403: false,
        }
    }
}

pub struct World {
    pub server: Server,
    knobs: Arc<Mutex<Knobs>>,
}

impl World {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(configure: impl FnOnce(&mut Knobs)) -> Self {
        let mut initial = Knobs::default();
        configure(&mut initial);
        let knobs = Arc::new(Mutex::new(initial));
        let shared = Arc::clone(&knobs);
        let server = Server::spawn(move |base| {
            let base = base.to_owned();
            move |request: &HttpReq| respond(&base, &shared, request)
        });
        Self { server, knobs }
    }

    pub fn knobs(&self) -> std::sync::MutexGuard<'_, Knobs> {
        self.knobs.lock().unwrap()
    }

    pub fn base(&self) -> &str {
        &self.server.url
    }

    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.server.url)
    }

    pub fn issuer(&self) -> String {
        format!("{}/as", self.server.url)
    }

    pub fn grant(&self, access: &str, refresh: Option<&str>) {
        let mut knobs = self.knobs();
        knobs.valid_access.insert(access.to_owned());
        if let Some(refresh) = refresh {
            knobs.valid_refresh.insert(refresh.to_owned());
        }
    }
}

pub fn respond(base: &str, knobs: &Arc<Mutex<Knobs>>, request: &HttpReq) -> Resp {
    let mut knobs = knobs.lock().unwrap();
    let issuer = format!("{base}/as");
    match (request.method.as_str(), request.path.as_str()) {
        (_, "/mcp") => {
            let authorized = knobs.public
                || request
                    .bearer()
                    .is_some_and(|token| knobs.valid_access.contains(token));
            if !authorized {
                return Resp::status(401).header(
                    "WWW-Authenticate",
                    &format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\", scope=\"read\""
                    ),
                );
            }
            match request.method.as_str() {
                "POST" => {
                    if request.rpc_method() == "tools/call" {
                        if knobs.plain_403 {
                            return Resp::status(403);
                        }
                        if let Some(scope) = knobs.call_needs_scope {
                            return Resp::status(403).header(
                                "WWW-Authenticate",
                                &format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\""),
                            );
                        }
                    }
                    mcp_reply(request)
                }
                "DELETE" => Resp::status(200),
                _ => Resp::status(405),
            }
        }
        ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            let mut document = json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [issuer],
            });
            if let Some(scopes) = &knobs.resource_scopes {
                document["scopes_supported"] = json!(scopes);
            }
            Resp::json(200, &document)
        }
        ("GET", "/.well-known/oauth-authorization-server/as") => {
            let mut document = json!({
                "issuer": knobs.metadata_issuer.clone().unwrap_or_else(|| issuer.clone()),
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": knobs
                    .token_endpoint_override
                    .clone()
                    .unwrap_or_else(|| format!("{issuer}/token")),
                "response_types_supported": ["code"],
                "token_endpoint_auth_methods_supported": knobs.token_auth_methods,
                "authorization_response_iss_parameter_supported": knobs.advertise_iss,
            });
            if knobs.registration {
                document["registration_endpoint"] = json!(format!("{issuer}/register"));
            }
            if knobs.advertise_s256 {
                document["code_challenge_methods_supported"] = json!(["S256"]);
            } else {
                document["code_challenge_methods_supported"] = json!(["plain"]);
            }
            Resp::json(200, &document)
        }
        ("POST", "/as/register") => Resp::json(
            201,
            &json!({"client_id": "dcr-client", "client_secret": "dcr-secret-value"}),
        ),
        ("POST", "/as/token") => {
            if knobs.token_redirect {
                return Resp::status(302).header("Location", &format!("{base}/elsewhere"));
            }
            let form = request.form();
            match form.get("grant_type").map(String::as_str) {
                Some("authorization_code") => {
                    if form.get("code").map(String::as_str) != Some("code-1") {
                        return Resp::json(400, &json!({"error": "invalid_grant"}));
                    }
                }
                Some("refresh_token") => {
                    if let Some(code) = knobs.reject_refresh {
                        return Resp::json(
                            400,
                            &json!({"error": code, "error_description": "nope"}),
                        );
                    }
                    let presented = form.get("refresh_token").cloned().unwrap_or_default();
                    if !knobs.valid_refresh.contains(&presented) {
                        return Resp::json(400, &json!({"error": "invalid_grant"}));
                    }
                    if knobs.rotate {
                        knobs.valid_refresh.remove(&presented);
                    }
                }
                _ => return Resp::json(400, &json!({"error": "unsupported_grant_type"})),
            }
            knobs.counter += 1;
            let access = format!("at-{}", knobs.counter);
            let refresh = format!("rt-{}", knobs.counter);
            knobs.valid_access.insert(access.clone());
            knobs.valid_refresh.insert(refresh.clone());
            let mut body = json!({
                "access_token": access,
                "token_type": "Bearer",
                "expires_in": knobs.expires_in,
                "refresh_token": refresh,
            });
            if knobs.token_response_padding > 0 {
                body["padding"] = json!("x".repeat(knobs.token_response_padding));
            }
            Resp::json(200, &body)
        }
        _ => Resp::status(404),
    }
}
