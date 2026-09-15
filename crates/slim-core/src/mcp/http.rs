use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::mcp::spec::{McpConnection, McpError, MCP_PROTOCOL_VERSION};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const SESSION_HEADER: &str = "mcp-session-id";

/// Streamable HTTP transport (MCP 2025-11-25): every JSON-RPC message is a
/// POST to a single endpoint carrying `Accept: application/json,
/// text/event-stream`. Responses arrive either as a single JSON document or
/// as an SSE stream that ends once the matching response id is delivered.
/// `mcp-session-id`, when issued by the server, is replayed on later POSTs.
pub(crate) struct HttpConnection {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    tools_stale: AtomicBool,
    timeout: Duration,
}

impl HttpConnection {
    pub(crate) fn new(
        url: String,
        headers: BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let parsed = reqwest::Url::parse(&url)
            .map_err(|error| McpError::Protocol(format!("invalid MCP url {url}: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(McpError::Protocol(format!(
                "MCP url must be http(s): {url}"
            )));
        }
        let client = reqwest::Client::builder()
            // RPC endpoints never legitimately redirect; following one would
            // replay configured secret headers to another host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| McpError::Protocol(format!("http client: {error}")))?;
        Ok(Self {
            client,
            url,
            headers: headers.into_iter().collect(),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            tools_stale: AtomicBool::new(false),
            timeout,
        })
    }

    fn post(&self, body: Value) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .post(&self.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
            .json(&body)
            .timeout(self.timeout);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(session) = session {
            request = request.header(SESSION_HEADER, session);
        }
        request
    }

    fn remember_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            *self
                .session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(value.to_owned());
        }
    }

    /// Answers a server-to-client request received on an SSE stream with
    /// MethodNotFound on a fresh POST so the remote side never waits on us.
    async fn reject_server_request(&self, id: Value) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": JSONRPC_METHOD_NOT_FOUND, "message": "unsupported"},
        });
        let _ = self.post(response).send().await;
    }

    async fn send_request(&self, body: Value, expected_id: u64) -> Result<Value, McpError> {
        let response = self
            .post(body)
            .send()
            .await
            .map_err(|error| McpError::Protocol(format!("http request failed: {error}")))?;
        self.remember_session(&response);
        let status = response.status();
        if status.as_u16() == 404 {
            // Session expired or unknown endpoint: force a fresh handshake.
            self.closed.store(true, Ordering::Relaxed);
            return Err(McpError::Closed);
        }
        if !status.is_success() {
            return Err(McpError::Server {
                code: status.as_u16() as i64,
                message: status.canonical_reason().unwrap_or("http error").to_owned(),
            });
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if content_type.contains("text/event-stream") {
            self.read_sse_response(response, expected_id).await
        } else {
            if response
                .content_length()
                .is_some_and(|len| len > MAX_MESSAGE_BYTES as u64)
            {
                return Err(McpError::Protocol("response exceeds 16 MiB".into()));
            }
            // Bounded streamed read: the Content-Length pre-check does not
            // cover close-delimited or chunked bodies.
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|error| McpError::Protocol(format!("http body: {error}")))?;
                if bytes.len() + chunk.len() > MAX_MESSAGE_BYTES {
                    return Err(McpError::Protocol("response exceeds 16 MiB".into()));
                }
                bytes.extend_from_slice(&chunk);
            }
            let message: Value = serde_json::from_slice(&bytes)
                .map_err(|error| McpError::Protocol(format!("invalid JSON response: {error}")))?;
            if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
                return Err(McpError::Protocol("response id mismatch".into()));
            }
            extract_result(message)
        }
    }

    async fn read_sse_response(
        &self,
        response: reqwest::Response,
        expected_id: u64,
    ) -> Result<Value, McpError> {
        let mut stream = response.bytes_stream();
        // Byte-level line splitting: a multibyte UTF-8 char can straddle a
        // chunk boundary, so decode only complete lines (`\n` never appears
        // inside a codepoint's continuation bytes).
        let mut buffer: Vec<u8> = Vec::new();
        // One bounded payload per event: every `data:` line contributes at
        // least its terminating newline, so empty lines cannot bypass the cap.
        let mut payload = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| McpError::Protocol(format!("sse stream: {error}")))?;
            buffer.extend_from_slice(&chunk);
            if buffer.len() > MAX_MESSAGE_BYTES {
                return Err(McpError::Protocol("sse message exceeds 16 MiB".into()));
            }
            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let raw: Vec<u8> = buffer.drain(..=end).collect();
                let line = String::from_utf8_lossy(raw.strip_suffix(b"\n").unwrap_or(&raw))
                    .trim_end_matches('\r')
                    .to_owned();
                if line.is_empty() {
                    if payload.is_empty() {
                        continue;
                    }
                    let body = std::mem::take(&mut payload);
                    let message: Value =
                        serde_json::from_str(body.trim_end()).map_err(|error| {
                            McpError::Protocol(format!("invalid SSE message: {error}"))
                        })?;
                    match classify_inbound(&message, expected_id) {
                        Inbound::Response(result) => return result,
                        Inbound::ServerRequest(id) => self.reject_server_request(id).await,
                        Inbound::ToolsChanged => self.tools_stale.store(true, Ordering::Relaxed),
                        Inbound::Other => {}
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    payload.push_str(data.trim_start());
                    payload.push('\n');
                    if payload.len() > MAX_MESSAGE_BYTES {
                        return Err(McpError::Protocol("sse message exceeds 16 MiB".into()));
                    }
                }
            }
        }
        // Stream ended without our response: the session is dead, so flag it
        // and let the caller's reconnect logic take over.
        self.closed.store(true, Ordering::Relaxed);
        Err(McpError::Closed)
    }
}

#[async_trait::async_trait]
impl McpConnection for HttpConnection {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(McpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match tokio::time::timeout(self.timeout, self.send_request(body, id)).await {
            Ok(result) => result,
            Err(_) => Err(McpError::Timeout(self.timeout)),
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.post(body).send().await;
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn take_tools_stale(&self) -> bool {
        self.tools_stale.swap(false, Ordering::Relaxed)
    }

    fn mark_tools_stale(&self) {
        self.tools_stale.store(true, Ordering::Relaxed);
    }
}

enum Inbound {
    Response(Result<Value, McpError>),
    ServerRequest(Value),
    ToolsChanged,
    Other,
}

fn classify_inbound(message: &Value, expected_id: u64) -> Inbound {
    let id = message.get("id").cloned();
    if let Some(id) = id {
        if message.get("method").is_some() {
            return Inbound::ServerRequest(id);
        }
        if id.as_u64() == Some(expected_id) {
            return Inbound::Response(extract_result(message.clone()));
        }
        return Inbound::Other;
    }
    if message.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed") {
        return Inbound::ToolsChanged;
    }
    Inbound::Other
}

fn extract_result(message: Value) -> Result<Value, McpError> {
    if let Some(error) = message.get("error") {
        return Err(McpError::Server {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: crate::mcp::spec::bounded_server_text(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown server error"),
            ),
        });
    }
    if let Some(result) = message.get("result") {
        return Ok(result.clone());
    }
    Err(McpError::Protocol(
        "response has neither result nor error".into(),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use serde_json::json;

    use super::{HttpConnection, MAX_MESSAGE_BYTES};
    use crate::mcp::spec::{McpConnection, McpError};

    /// One-shot HTTP fixture: accepts a single connection, reads the request
    /// headers plus the declared Content-Length body, then replies with
    /// `response_headers` and a close-delimited body made of `pattern`
    /// repeated, written in 64 KiB chunks.
    fn serve_once(response_headers: &'static str, pattern: Vec<u8>, body_bytes: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut received: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            let content_length = loop {
                let read = socket.read(&mut chunk).expect("read request");
                assert!(read > 0, "client closed before sending headers");
                received.extend_from_slice(&chunk[..read]);
                if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&received[..end]).into_owned();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.trim().eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    received.drain(..end + 4);
                    break length;
                }
            };
            while received.len() < content_length {
                let read = socket.read(&mut chunk).expect("read body");
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(response_headers.as_bytes())
                .expect("write response headers");
            let mut block = Vec::new();
            while block.len() < 64 * 1024 {
                block.extend_from_slice(&pattern);
            }
            let mut remaining = body_bytes;
            while remaining > 0 {
                let n = remaining.min(block.len());
                if socket.write_all(&block[..n]).is_err() {
                    // Client disconnected once its bound tripped — expected.
                    break;
                }
                remaining -= n;
            }
        });
        url
    }

    /// Over-16-MiB close-delimited JSON body: no Content-Length, so the bound
    /// has to hold on the streamed read, not the header pre-check.
    #[test]
    fn json_body_without_content_length_is_bounded() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            b"x".to_vec(),
            MAX_MESSAGE_BYTES + 1024 * 1024,
        );
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(30)).expect("connection");
        let runtime = tokio::runtime::Runtime::new().expect("tokio");
        let error = runtime
            .block_on(connection.request("tools/list", json!({})))
            .expect_err("oversized body must error");
        match error {
            McpError::Protocol(message) => {
                assert!(message.contains("16 MiB"), "{message}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    /// Over-16-MiB of unterminated `data:` lines: the accumulated payload hits
    /// the per-message bound even though no blank-line event boundary ever comes.
    #[test]
    fn sse_data_lines_without_boundary_are_bounded() {
        let mut pattern = b"data: ".to_vec();
        pattern.extend(std::iter::repeat_n(b'x', 8000));
        pattern.push(b'\n');
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
            pattern,
            MAX_MESSAGE_BYTES * 2,
        );
        let connection =
            HttpConnection::new(url, BTreeMap::new(), Duration::from_secs(30)).expect("connection");
        let runtime = tokio::runtime::Runtime::new().expect("tokio");
        let error = runtime
            .block_on(connection.request("tools/list", json!({})))
            .expect_err("unterminated SSE flood must error");
        match error {
            McpError::Protocol(message) => {
                assert!(message.contains("16 MiB"), "{message}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }
}
