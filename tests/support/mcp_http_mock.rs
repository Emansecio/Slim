//! Scriptable Streamable HTTP MCP server for integration tests. Every
//! request (POST, GET or DELETE) is parsed, recorded and handed to a handler
//! that writes the whole response, so a test can serve JSON, hold an SSE
//! stream open, drop a connection, answer 404 or never answer at all. The
//! connection closes when the handler returns.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// One request as the server saw it.
#[derive(Clone, Debug)]
pub struct Req {
    /// `POST`, `GET` or `DELETE`.
    pub verb: String,
    /// Lower-cased header names.
    pub headers: BTreeMap<String, String>,
    /// JSON body; `Null` when there was none.
    pub body: Value,
    /// When the request was fully read.
    pub at: Instant,
}

impl Req {
    /// JSON-RPC method of a POST, `""` otherwise.
    pub fn method(&self) -> &str {
        self.body["method"].as_str().unwrap_or("")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn is(&self, verb: &str, method: &str) -> bool {
        self.verb == verb && self.method() == method
    }
}

/// State every handler can reach: what was received, and the open `GET`
/// streams (to push events from another request's handler).
pub struct Shared {
    received: Mutex<Vec<Req>>,
    streams: Mutex<Vec<TcpStream>>,
}

impl Shared {
    pub fn push_event(&self, data: &Value) {
        let mut streams = self
            .streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        streams.retain_mut(|stream| write!(stream, "data: {data}\n\n").is_ok());
        for stream in streams.iter_mut() {
            let _ = stream.flush();
        }
    }

    /// Keeps `stream` so later `push_event` calls reach it.
    pub fn register_stream(&self, stream: &TcpStream) {
        if let Ok(clone) = stream.try_clone() {
            self.streams
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(clone);
        }
    }
}

pub type Handler = dyn Fn(&Req, &mut TcpStream, &Shared) + Send + Sync;

/// Runs before the standard `initialize` reply of the n-th attempt; `false`
/// means it already answered and the standard reply is skipped.
pub type InitHandler = dyn Fn(usize, &Req, &mut TcpStream) -> bool + Send + Sync;

pub struct Mock {
    pub url: String,
    shared: Arc<Shared>,
}

impl Mock {
    pub fn spawn(handler: impl Fn(&Req, &mut TcpStream, &Shared) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let shared = Arc::new(Shared {
            received: Mutex::new(Vec::new()),
            streams: Mutex::new(Vec::new()),
        });
        let handler: Arc<Handler> = Arc::new(handler);
        let accept_shared = Arc::clone(&shared);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let shared = Arc::clone(&accept_shared);
                let handler = Arc::clone(&handler);
                thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                    let Some(request) = read_request(&mut stream) else {
                        return;
                    };
                    shared
                        .received
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(request.clone());
                    handler(&request, &mut stream, &shared);
                });
            }
        });
        Self { url, shared }
    }

    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    pub fn requests(&self) -> Vec<Req> {
        self.shared
            .received
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn count(&self, verb: &str, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.is(verb, method))
            .count()
    }

    pub fn gets(&self) -> Vec<Req> {
        self.requests()
            .into_iter()
            .filter(|request| request.verb == "GET")
            .collect()
    }

    pub fn deletes(&self) -> Vec<Req> {
        self.requests()
            .into_iter()
            .filter(|request| request.verb == "DELETE")
            .collect()
    }

    /// POST bodies' JSON-RPC methods in arrival order.
    pub fn methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter(|request| request.verb == "POST")
            .map(|request| request.method().to_owned())
            .collect()
    }

    /// Waits for `probe` over the recorded requests; panics on timeout.
    pub fn wait_for<T>(&self, what: &str, probe: impl Fn(&[Req]) -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(found) = probe(&self.requests()) {
                return found;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Req> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let verb = line.split_whitespace().next()?.to_owned();
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
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&body).ok()?
    };
    Some(Req {
        verb,
        headers,
        body,
        at: Instant::now(),
    })
}

/// `200` JSON-RPC reply; `session` adds `mcp-session-id`.
pub fn write_json(stream: &mut TcpStream, session: Option<&str>, body: &Value) {
    let body = body.to_string();
    let session = session
        .map(|id| format!("mcp-session-id: {id}\r\n"))
        .unwrap_or_default();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

pub fn write_status(stream: &mut TcpStream, status: u16, reason: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

pub fn write_accepted(stream: &mut TcpStream) {
    write_status(stream, 202, "Accepted");
}

/// Starts an SSE response (close-delimited).
pub fn sse_open(stream: &mut TcpStream) {
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

/// One SSE event; `id` and `retry_ms` are written when given.
pub fn sse_event(stream: &mut TcpStream, id: Option<&str>, retry_ms: Option<u64>, data: &Value) {
    if let Some(retry) = retry_ms {
        let _ = writeln!(stream, "retry: {retry}");
    }
    if let Some(id) = id {
        let _ = writeln!(stream, "id: {id}");
    }
    let _ = write!(stream, "data: {data}\n\n");
    let _ = stream.flush();
}

/// Result envelope answering `request`.
pub fn result_of(request: &Req, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request.body["id"], "result": result})
}

/// The standard `initialize` result (tools + a resources capability off).
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {"tools": {"listChanged": true}},
        "serverInfo": {"name": "mock", "version": "1"},
    })
}

pub fn tools_result(names: &[&str]) -> Value {
    let tools: Vec<Value> = names
        .iter()
        .map(|name| json!({"name": name, "inputSchema": {"type": "object"}}))
        .collect();
    json!({ "tools": tools })
}

pub fn text_result(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

/// How the scripted server answers each kind of request. Requests carrying
/// the first session id are answered 404 once `expired` is set.
pub struct Script {
    /// Issue `mcp-session-id: s<n>` on the n-th `initialize`.
    pub sessions: bool,
    pub expired: Arc<AtomicBool>,
    pub init: Box<InitHandler>,
    pub tools: Box<dyn Fn(usize) -> Vec<&'static str> + Send + Sync>,
    pub get: Box<Handler>,
    pub call: Box<Handler>,
    pub delete: Box<Handler>,
}

impl Script {
    pub fn new() -> Self {
        Self {
            sessions: true,
            expired: Arc::new(AtomicBool::new(false)),
            init: Box::new(|_, _, _| true),
            tools: Box::new(|_| vec!["echo"]),
            get: Box::new(|_, stream, _| write_status(stream, 405, "Method Not Allowed")),
            call: Box::new(|req, stream, _| {
                write_json(stream, None, &result_of(req, text_result("ok")));
            }),
            delete: Box::new(|_, stream, _| write_status(stream, 204, "No Content")),
        }
    }
}

impl Default for Script {
    fn default() -> Self {
        Self::new()
    }
}

/// A `GET` handler that opens an SSE stream, lets other handlers push events
/// into it and holds it open.
pub fn hold_stream() -> Box<Handler> {
    Box::new(|_, stream, shared| {
        sse_open(stream);
        shared.register_stream(stream);
        thread::sleep(Duration::from_secs(8));
    })
}

pub fn serve(script: Script) -> Mock {
    let initializes = AtomicUsize::new(0);
    let tool_lists = AtomicUsize::new(0);
    Mock::spawn(move |req, stream, shared| {
        if script.expired.load(Ordering::SeqCst) && req.header("mcp-session-id") == Some("s1") {
            write_status(stream, 404, "Not Found");
            return;
        }
        match (req.verb.as_str(), req.method()) {
            ("POST", "initialize") => {
                let attempt = initializes.fetch_add(1, Ordering::SeqCst);
                if !(script.init)(attempt, req, stream) {
                    return;
                }
                let session = script.sessions.then(|| format!("s{}", attempt + 1));
                write_json(
                    stream,
                    session.as_deref(),
                    &result_of(req, initialize_result()),
                );
            }
            ("POST", "tools/list") => {
                let index = tool_lists.fetch_add(1, Ordering::SeqCst);
                write_json(
                    stream,
                    None,
                    &result_of(req, tools_result(&(script.tools)(index))),
                );
            }
            ("POST", "tools/call") => (script.call)(req, stream, shared),
            ("POST", _) => write_accepted(stream),
            ("GET", _) => (script.get)(req, stream, shared),
            ("DELETE", _) => (script.delete)(req, stream, shared),
            _ => write_status(stream, 400, "Bad Request"),
        }
    })
}
