//! Client side of an MCP session, shared by the stdio and HTTP transports:
//! protocol-version negotiation, what the client answers when the server
//! calls it (`roots/list`, `ping`), server notifications (list changes,
//! progress, logging) and the bounded `mcp.log` writer.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::mcp::spec::{McpError, MCP_PROTOCOL_VERSION};

/// Revisions this client can speak, newest first. The server's choice in
/// `initialize` must be one of them; it also becomes the
/// `MCP-Protocol-Version` header on HTTP.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = [
    MCP_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
/// Display bounds for text the server controls.
const MAX_IDENTITY_CHARS: usize = 128;
const MAX_INSTRUCTIONS_BYTES: usize = 4 * 1024;
const MAX_PROGRESS_MESSAGE_CHARS: usize = 512;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_LOG_ENTRY_BYTES: usize = 8 * 1024;
/// Input considered for one log entry before redaction; larger than the
/// stored entry so a secret cut by truncation cannot survive redaction.
const MAX_LOG_INPUT_BYTES: usize = 64 * 1024;

/// What the server told the client in its `initialize` result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerHandshake {
    /// Negotiated revision (one of [`SUPPORTED_PROTOCOL_VERSIONS`]).
    pub protocol_version: String,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub capabilities: Value,
    /// Server-provided usage notes; untrusted text, capped at 4 KiB.
    pub instructions: Option<String>,
}

impl McpServerHandshake {
    /// Validates the `initialize` result. A missing `protocolVersion` is
    /// tolerated (the requested revision is assumed); a version this client
    /// does not speak is an error.
    pub fn from_initialize(result: &Value) -> Result<Self, McpError> {
        let protocol_version = match result.get("protocolVersion").and_then(Value::as_str) {
            None => MCP_PROTOCOL_VERSION.to_owned(),
            Some(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => version.to_owned(),
            Some(version) => {
                return Err(McpError::Protocol(format!(
                    "MCP server selected unsupported protocol version {} (supported: {})",
                    display_text(version, 32),
                    SUPPORTED_PROTOCOL_VERSIONS.join(", ")
                )));
            }
        };
        let identity = |key: &str| {
            result
                .pointer(&format!("/serverInfo/{key}"))
                .and_then(Value::as_str)
                .map(|text| display_text(text, MAX_IDENTITY_CHARS))
                .filter(|text| !text.is_empty())
        };
        let instructions = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| truncate_bytes(text, MAX_INSTRUCTIONS_BYTES));
        Ok(Self {
            protocol_version,
            server_name: identity("name"),
            server_version: identity("version"),
            capabilities: result
                .get("capabilities")
                .filter(|capabilities| capabilities.is_object())
                .cloned()
                .unwrap_or_else(|| json!({})),
            instructions,
        })
    }

    /// `tools/list` is only valid when the server declares the capability.
    pub fn has_tools(&self) -> bool {
        self.capabilities.get("tools").is_some_and(Value::is_object)
    }

    pub fn has_resources(&self) -> bool {
        self.capabilities
            .get("resources")
            .is_some_and(Value::is_object)
    }
}

/// One `notifications/progress` for an in-flight request.
#[derive(Clone, Debug, PartialEq)]
pub struct McpProgress {
    pub progress: f64,
    pub total: Option<f64>,
    /// Server text, control characters removed and length capped.
    pub message: Option<String>,
}

/// Receives progress of one request. Called from a transport thread: it must
/// be cheap and must not block.
pub type McpProgressSink = Arc<dyn Fn(McpProgress) + Send + Sync>;

/// Extracts `(progressToken, progress)` from `notifications/progress`
/// params. Only numeric tokens are accepted: the transports issue the
/// request id as the token.
pub(crate) fn parse_progress(params: &Value) -> Option<(u64, McpProgress)> {
    let token = params.get("progressToken")?.as_u64()?;
    let progress = params.get("progress")?.as_f64()?;
    if !progress.is_finite() {
        return None;
    }
    Some((
        token,
        McpProgress {
            progress,
            total: params
                .get("total")
                .and_then(Value::as_f64)
                .filter(|total| total.is_finite()),
            message: params
                .get("message")
                .and_then(Value::as_str)
                .map(|text| display_text(text, MAX_PROGRESS_MESSAGE_CHARS))
                .filter(|text| !text.is_empty()),
        },
    ))
}

/// State one connection shares with its reader: what to answer when the
/// server calls the client, and the flags server notifications raise.
pub(crate) struct ClientContext {
    server: String,
    roots: Value,
    tools_stale: AtomicBool,
    resources_stale: AtomicBool,
    log: Option<Arc<McpLog>>,
}

impl ClientContext {
    pub(crate) fn new(server: &str, workspace: &Path, log: Option<Arc<McpLog>>) -> Arc<Self> {
        Arc::new(Self {
            server: display_text(server, MAX_IDENTITY_CHARS),
            roots: roots_result(workspace),
            tools_stale: AtomicBool::new(false),
            resources_stale: AtomicBool::new(false),
            log,
        })
    }

    /// Context for a connection that serves no roots and keeps no log.
    pub(crate) fn detached() -> Arc<Self> {
        Arc::new(Self {
            server: String::new(),
            roots: json!({"roots": []}),
            tools_stale: AtomicBool::new(false),
            resources_stale: AtomicBool::new(false),
            log: None,
        })
    }

    /// JSON-RPC response to a server-to-client request. Only `ping` and
    /// `roots/list` are served; everything else (sampling, elicitation, ...)
    /// is MethodNotFound so the server fails fast instead of waiting. The id
    /// is echoed verbatim: JSON-RPC allows string ids.
    pub(crate) fn answer_request(&self, id: &Value, method: &str) -> Value {
        match method {
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": self.roots}),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": JSONRPC_METHOD_NOT_FOUND, "message": "unsupported"},
            }),
        }
    }

    /// Handles the notifications that do not belong to one request:
    /// list-change flags and server logging. Progress is routed by the
    /// transport because it needs the pending-request table.
    pub(crate) fn on_notification(&self, method: &str, params: &Value) {
        match method {
            "notifications/tools/list_changed" => self.tools_stale.store(true, Ordering::Relaxed),
            "notifications/resources/list_changed" => {
                self.resources_stale.store(true, Ordering::Relaxed)
            }
            "notifications/message" => {
                if let Some(log) = &self.log {
                    log.write(&self.server, params);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn take_tools_stale(&self) -> bool {
        self.tools_stale.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn mark_tools_stale(&self) {
        self.tools_stale.store(true, Ordering::Relaxed);
    }

    pub(crate) fn take_resources_stale(&self) -> bool {
        self.resources_stale.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn mark_resources_stale(&self) {
        self.resources_stale.store(true, Ordering::Relaxed);
    }
}

/// `roots/list` result: the workspace root as a `file://` URI. An
/// unresolvable path yields an empty list rather than a malformed URI.
fn roots_result(workspace: &Path) -> Value {
    let absolute = std::fs::canonicalize(workspace)
        .or_else(|_| std::env::current_dir().map(|dir| dir.join(workspace)))
        .map(strip_verbatim_prefix);
    let Ok(absolute) = absolute else {
        return json!({"roots": []});
    };
    let Ok(uri) = reqwest::Url::from_file_path(&absolute) else {
        return json!({"roots": []});
    };
    let name = absolute
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let mut root = json!({"uri": uri.as_str()});
    if let Some(name) = name {
        root["name"] = Value::String(name);
    }
    json!({"roots": [root]})
}

/// `canonicalize` yields `\\?\C:\...` on Windows, which has no file URI form.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_owned());
        }
    }
    path
}

/// Appends server log messages (`notifications/message`) to one file:
/// `<ISO> [server] level logger: text`. Entries are redacted against the
/// configured secrets, capped, stripped of control characters, and the file
/// rotates to `<name>.1` once it passes 5 MB. Write failures are ignored:
/// logging must never break tool calls.
pub struct McpLog {
    path: PathBuf,
    secrets: RwLock<Vec<String>>,
    /// Secrets that change while the process runs (OAuth tokens), asked for
    /// on every write.
    secret_source: RwLock<Option<SecretSource>>,
    size: Mutex<Option<u64>>,
}

type SecretSource = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

impl McpLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            secrets: RwLock::new(Vec::new()),
            secret_source: RwLock::new(None),
            size: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replaces the exact values redacted from every entry.
    pub fn set_secrets(&self, mut secrets: Vec<String>) {
        secrets.retain(|secret| !secret.is_empty());
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        *self
            .secrets
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = secrets;
    }

    /// Redacts, besides [`Self::set_secrets`], whatever `source` returns at
    /// the moment each entry is written.
    pub fn set_secret_source(&self, source: SecretSource) {
        *self
            .secret_source
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
    }

    pub(crate) fn write(&self, server: &str, params: &Value) {
        let source = self
            .secret_source
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut secrets = self
            .secrets
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(source) = source {
            secrets.extend(source().into_iter().filter(|secret| !secret.is_empty()));
            secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        }
        let line = format_log_line(SystemTime::now(), server, params, &secrets);
        let _ = self.append(&line);
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        let mut size = self
            .size
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if size.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            *size = Some(self.current_size());
        }
        if size.unwrap_or(0) > MAX_LOG_BYTES {
            // Another process may have rotated it already: check the file.
            if self.current_size() > MAX_LOG_BYTES {
                let mut rotated = self.path.clone().into_os_string();
                rotated.push(".1");
                std::fs::rename(&self.path, PathBuf::from(rotated))?;
            }
            *size = Some(self.current_size());
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        *size = Some(size.unwrap_or(0) + line.len() as u64);
        Ok(())
    }

    fn current_size(&self) -> u64 {
        std::fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }
}

fn format_log_line(now: SystemTime, server: &str, params: &Value, secrets: &[String]) -> String {
    let level = params
        .get("level")
        .and_then(Value::as_str)
        .map(|level| display_text(level, 32))
        .filter(|level| !level.is_empty())
        .unwrap_or_else(|| "info".to_owned());
    let logger = params
        .get("logger")
        .and_then(Value::as_str)
        .map(|logger| display_text(logger, 64))
        .filter(|logger| !logger.is_empty())
        .map(|logger| format!(" {logger}:"))
        .unwrap_or_default();
    let data = match params.get("data") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let data = truncate_bytes(&data, MAX_LOG_INPUT_BYTES);
    let data = crate::runtime::redact_values(secrets, &data);
    let data = truncate_bytes(&data, MAX_LOG_ENTRY_BYTES);
    let mut text = String::with_capacity(data.len());
    for character in data.chars() {
        match character {
            '\r' => {}
            '\n' => text.push_str("\n    "),
            '\t' => text.push(' '),
            character if character.is_control() => text.push(' '),
            character => text.push(character),
        }
    }
    format!(
        "{} [{}] {level}{logger} {text}\n",
        iso_timestamp(now),
        display_text(server, MAX_IDENTITY_CHARS),
    )
}

/// `2026-10-02T12:34:56.789Z` without a calendar dependency.
fn iso_timestamp(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = elapsed.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    // Civil-from-days (proleptic Gregorian), H. Hinnant.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
        elapsed.subsec_millis(),
    )
}

/// Text safe to print on a terminal: escape sequences (CSI, OSC and the other
/// ESC-introduced forms, and their C1 spellings) and every control character
/// are removed, except `\n`; a tab becomes a space and a lone `\r` too (it
/// would overwrite the line). Bidirectional overrides, which reorder what
/// the reader sees, are removed as well. For text an MCP server or a
/// project file controls and a person reads before deciding anything.
pub fn strip_terminal_controls(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(line_break) = skip_terminal_sequence(character, &mut chars) {
            if line_break {
                cleaned.push('\n');
            }
            continue;
        }
        match character {
            '\n' => cleaned.push('\n'),
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    cleaned.push(' ');
                }
            }
            '\t' => cleaned.push(' '),
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' => {}
            character if character.is_control() => {}
            character => cleaned.push(character),
        }
    }
    cleaned
}

/// When `first` starts an escape sequence (`ESC` forms or a C1 introducer),
/// consumes the rest of it and returns `Some(line_break)`, where `line_break`
/// is true for `ESC` directly followed by `\n` (the line end is not part of
/// the sequence and must survive). Returns `None`, consuming nothing, for any
/// other character.
pub(crate) fn skip_terminal_sequence(
    first: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Option<bool> {
    match first {
        '\u{1b}' => match chars.next() {
            Some('[') => skip_control_sequence(chars),
            Some(']' | 'P' | 'X' | '^' | '_') => skip_control_string(chars),
            // Two-character and intermediate-byte forms (`ESC c`, `ESC ( B`).
            Some(' '..='/') => {
                while chars.next_if(|next| matches!(next, ' '..='/')).is_some() {}
                let _ = chars.next_if(|next| matches!(next, '0'..='~'));
            }
            Some('\n') => return Some(true),
            Some(_) | None => {}
        },
        '\u{9b}' => skip_control_sequence(chars),
        '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_control_string(chars),
        _ => return None,
    }
    Some(false)
}

/// Consumes the rest of a CSI sequence: parameter and intermediate bytes up
/// to and including the final byte.
fn skip_control_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while chars.next_if(|next| matches!(next, ' '..='?')).is_some() {}
    let _ = chars.next_if(|next| matches!(next, '@'..='~'));
}

/// Consumes an OSC/DCS/APC string: up to and including BEL or ST. A line end
/// stops it, so an unterminated string cannot hide the lines after it.
fn skip_control_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(next) = chars.peek().copied() {
        match next {
            '\n' => return,
            '\u{7}' | '\u{9c}' => {
                chars.next();
                return;
            }
            '\u{1b}' => {
                chars.next();
                if chars.next_if_eq(&'\\').is_some() {
                    return;
                }
            }
            _ => {
                chars.next();
            }
        }
    }
}

/// Single-line display form of server text: control characters become
/// spaces, whitespace is trimmed, length is capped in characters.
fn display_text(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() > max_chars {
        let mut shortened: String = cleaned.chars().take(max_chars).collect();
        shortened.push('…');
        shortened
    } else {
        cleaned.to_owned()
    }
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_accepts_every_supported_revision_and_rejects_others() {
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            let handshake = McpServerHandshake::from_initialize(&json!({
                "protocolVersion": version, "capabilities": {"tools": {}},
            }))
            .expect(version);
            assert_eq!(handshake.protocol_version, version);
            assert!(handshake.has_tools());
        }
        let error = McpServerHandshake::from_initialize(&json!({"protocolVersion": "2023-01-01"}))
            .expect_err("unsupported");
        let text = error.to_string();
        assert!(
            text.contains("unsupported protocol version 2023-01-01"),
            "{text}"
        );
        assert!(text.contains("2025-11-25"), "{text}");
    }

    #[test]
    fn handshake_keeps_identity_instructions_and_capabilities_bounded() {
        let handshake = McpServerHandshake::from_initialize(&json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"resources": {}},
            "serverInfo": {"name": "fs\u{1b}[31m\nserver", "version": "1.2"},
            "instructions": format!("  {}  ", "x".repeat(10_000)),
        }))
        .unwrap();
        assert_eq!(handshake.server_name.as_deref(), Some("fs [31m server"));
        assert_eq!(handshake.server_version.as_deref(), Some("1.2"));
        assert!(!handshake.has_tools());
        assert!(handshake.has_resources());
        let instructions = handshake.instructions.unwrap();
        assert!(instructions.len() <= MAX_INSTRUCTIONS_BYTES + '…'.len_utf8());
        assert!(instructions.ends_with('…'));
    }

    #[test]
    fn missing_version_and_non_object_capabilities_are_tolerated() {
        let handshake =
            McpServerHandshake::from_initialize(&json!({"capabilities": "nope"})).unwrap();
        assert_eq!(handshake.protocol_version, MCP_PROTOCOL_VERSION);
        assert_eq!(handshake.capabilities, json!({}));
        assert!(McpServerHandshake::from_initialize(&Value::Null).is_ok());
    }

    #[test]
    fn progress_parsing_requires_numeric_token_and_progress() {
        let (token, progress) = parse_progress(&json!({
            "progressToken": 7, "progress": 2.5, "total": 10, "message": "step\u{7}\n2",
        }))
        .unwrap();
        assert_eq!(token, 7);
        assert_eq!(progress.progress, 2.5);
        assert_eq!(progress.total, Some(10.0));
        assert_eq!(progress.message.as_deref(), Some("step  2"));
        assert!(parse_progress(&json!({"progressToken": "7", "progress": 1})).is_none());
        assert!(parse_progress(&json!({"progressToken": 7})).is_none());
        let long = parse_progress(&json!({
            "progressToken": 1, "progress": 1, "message": "m".repeat(5_000),
        }))
        .unwrap()
        .1;
        assert_eq!(
            long.message.unwrap().chars().count(),
            MAX_PROGRESS_MESSAGE_CHARS + 1
        );
    }

    #[test]
    fn client_answers_ping_and_roots_and_rejects_everything_else() {
        let directory = std::env::temp_dir();
        let context = ClientContext::new("srv", &directory, None);
        assert_eq!(
            context.answer_request(&json!("p-1"), "ping"),
            json!({"jsonrpc": "2.0", "id": "p-1", "result": {}})
        );
        let roots = context.answer_request(&json!(3), "roots/list");
        let uri = roots["result"]["roots"][0]["uri"].as_str().expect("uri");
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(!uri.contains('\\'), "{uri}");
        let other = context.answer_request(&json!(4), "sampling/createMessage");
        assert_eq!(other["error"]["code"], JSONRPC_METHOD_NOT_FOUND);
        assert_eq!(other["id"], 4);
    }

    #[test]
    fn list_change_notifications_raise_their_own_flag() {
        let context = ClientContext::detached();
        assert!(!context.take_tools_stale());
        context.on_notification("notifications/tools/list_changed", &json!({}));
        assert!(!context.take_resources_stale());
        assert!(context.take_tools_stale());
        assert!(!context.take_tools_stale());
        context.on_notification("notifications/resources/list_changed", &json!({}));
        assert!(context.take_resources_stale());
        assert!(!context.take_resources_stale());
    }

    #[test]
    fn timestamps_are_iso_8601_utc() {
        assert_eq!(iso_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso_timestamp(UNIX_EPOCH + Duration::from_millis(1_782_995_696_789)),
            "2026-07-02T12:34:56.789Z"
        );
        assert_eq!(
            iso_timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn log_lines_are_redacted_single_entry_and_control_free() {
        let line = format_log_line(
            UNIX_EPOCH,
            "srv",
            &json!({"level": "warning", "logger": "db", "data": "token=s3cr3t\nnext\u{1b}[0m"}),
            &["s3cr3t".to_owned()],
        );
        assert_eq!(
            line,
            "1970-01-01T00:00:00.000Z [srv] warning db: token=[REDACTED]\n    next [0m\n"
        );
        let structured = format_log_line(UNIX_EPOCH, "srv", &json!({"data": {"k": "v"}}), &[]);
        assert_eq!(
            structured,
            "1970-01-01T00:00:00.000Z [srv] info {\"k\":\"v\"}\n"
        );
    }

    #[test]
    fn oversized_log_data_is_truncated_after_redaction() {
        let secret = "S".repeat(40);
        let mut data = "a".repeat(MAX_LOG_ENTRY_BYTES - 20);
        data.push_str(&secret);
        data.push_str(&"b".repeat(100));
        let line = format_log_line(
            UNIX_EPOCH,
            "srv",
            &json!({"data": data}),
            std::slice::from_ref(&secret),
        );
        assert!(
            !line.contains("SSSS"),
            "secret fragment survived truncation"
        );
        assert!(line.len() < MAX_LOG_ENTRY_BYTES + 200);
    }
    #[test]
    fn terminal_controls_are_stripped_from_text_a_person_reads() {
        assert_eq!(
            strip_terminal_controls("node\u{1b}[1A\u{1b}[2K echo harmless"),
            "node echo harmless"
        );
        assert_eq!(
            strip_terminal_controls(
                "a\u{1b}]0;title\u{7}b\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\c"
            ),
            "ablinkc"
        );
        assert_eq!(strip_terminal_controls("x\u{9b}31my\u{9d}t\u{9c}z"), "xyz");
        assert_eq!(
            strip_terminal_controls("one\r\ntwo\rthree\tfour"),
            "one\ntwo three four"
        );
        assert_eq!(
            strip_terminal_controls("evil\u{202e}txt\u{7}\u{7f}"),
            "eviltxt"
        );
        // An unterminated string does not hide the following lines.
        assert_eq!(
            strip_terminal_controls("a\u{1b}]0;never ends\nnext"),
            "a\nnext"
        );
        assert_eq!(strip_terminal_controls("tail\u{1b}"), "tail");
        assert_eq!(strip_terminal_controls("naïve ✓ — ok"), "naïve ✓ — ok");
    }
}
