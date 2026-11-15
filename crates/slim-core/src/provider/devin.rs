//! Devin (Cognition) provider over the Codeium/Windsurf "Cascade" API.
//!
//! The wire is the Connect protocol on top of protobuf: unary `application/proto`
//! calls (`GetUserJwt`, `AssignModel`, `GetCliModelConfigs`) plus a server stream
//! of Connect envelopes (`application/connect+proto`, 1 flag byte + u32be length)
//! for `GetChatMessage`. This mirrors the oh-my-pi Devin provider exactly — same
//! client identity tuple (`ide_type: "chisel"` unlocks the CLI model surface and
//! the `AssignModel` router), same field layout, same auth-in-protobuf-metadata.
//! Auth never travels in headers: `Metadata.api_key` carries the credential.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::catalog_fetch::MAX_CATALOG_BYTES;
use super::{
    normalize_messages, redact_values, HttpRequest, PreparedProviderRequest, ProviderAdapter,
    ProviderCapabilities, ProviderError, ProviderEvent, ProviderKind, ProviderMessage,
    StreamWire, UsageBreakdown,
};

/// Base host for Devin's Cascade API (Connect protocol over HTTP/1.1).
pub const DEVIN_DEFAULT_BASE_URL: &str = "https://server.codeium.com";
/// Boot-time default; `GetCliModelConfigs` discovery replaces it with the
/// account's `default_override_model_config` once a catalog lands.
pub const DEVIN_DEFAULT_MODEL: &str = "swe-1-6";
pub const DEVIN_CHAT_MESSAGE_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const DEVIN_AUTH_PATH: &str = "/exa.auth_pb.AuthService/GetUserJwt";
const DEVIN_ASSIGN_MODEL_PATH: &str = "/exa.api_server_pb.ApiServerService/AssignModel";
const DEVIN_MODEL_CONFIGS_PATH: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";

/// Requested `Metadata.supported_model_displays` values: MODEL_ROUTER,
/// QUICK_REVIEW plus slots 6-8 the vendored proto predates (INTERNAL_DEFAULT,
/// UNCLASSIFIED, NORMAL) — same set the omp discovery request sends.
const DEVIN_SUPPORTED_MODEL_DISPLAYS: [u64; 5] = [3, 4, 6, 7, 8];
/// Display slots the server reserves for internal/config-only entries
/// (QUICK_REVIEW, INTERNAL_DEFAULT) — filtered out of the surfaced catalog.
fn is_internal_display(option: u64) -> bool {
    matches!(option, 4 | 6)
}
/// `DisplayOption.MODEL_ROUTER`.
const DISPLAY_OPTION_MODEL_ROUTER: u64 = 3;

const MAX_MODEL_ID_BYTES: usize = 128;
const MAX_CATALOG_ENTRIES: usize = 1024;
/// `GetCliModelConfigs` returns the whole account catalog (600+ configs).
const MAX_MODEL_CONFIGS_BYTES: usize = 16 * 1024 * 1024;
/// OpenAI-routed models reject replayed tool-call ids past 40 chars.
const TOOL_CALL_ID_MAX_LENGTH: usize = 40;
/// Compactable history this large turns an opaque `invalid_argument` into
/// context overflow, so maintenance prunes instead of failing the turn.
const LARGE_HISTORY_RECOVERY_BYTES: usize = 512 * 1024;

// ChatMessageSource.
const CHAT_SOURCE_USER: u64 = 1;
const CHAT_SOURCE_SYSTEM: u64 = 2;
const CHAT_SOURCE_TOOL: u64 = 4;
// ChatMessageRequestType::CASCADE.
const REQUEST_TYPE_CASCADE: u64 = 5;
// ConversationalPlannerMode::DEFAULT.
const PLANNER_MODE_DEFAULT: u64 = 1;
// CacheControlType::EPHEMERAL.
const CACHE_CONTROL_EPHEMERAL: u64 = 1;
// StopReason::MAX_TOKENS (all others just stop the turn).
const STOP_REASON_MAX_TOKENS: u64 = 3;

/// Prefix for bare JWT-shaped session tokens; other keys (`sk-ws-…` legacy
/// included) go out as stored.
const DEVIN_SESSION_TOKEN_PREFIX: &str = "devin-session-token$";

fn encode_devin_credential(api_key: &str) -> String {
    let key = api_key.trim();
    if key.is_empty() || key.starts_with(DEVIN_SESSION_TOKEN_PREFIX) {
        return key.to_owned();
    }
    if key.split('.').count() == 3 {
        format!("{DEVIN_SESSION_TOKEN_PREFIX}{key}")
    } else {
        key.to_owned()
    }
}

fn devin_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

// ---------- minimal protobuf codec (only what the Cascade surface needs) ----

const WIRE_VARINT: u8 = 0;
const WIRE_64BIT: u8 = 1;
const WIRE_LENGTH: u8 = 2;
const WIRE_32BIT: u8 = 5;

fn proto_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn proto_key(out: &mut Vec<u8>, field: u32, wire: u8) {
    proto_varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn proto_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    if value == 0 {
        return;
    }
    proto_key(out, field, WIRE_VARINT);
    proto_varint(out, value);
}

fn proto_bool_field(out: &mut Vec<u8>, field: u32, value: bool) {
    if !value {
        return;
    }
    proto_key(out, field, WIRE_VARINT);
    out.push(1);
}

fn proto_double_field(out: &mut Vec<u8>, field: u32, value: f64) {
    proto_key(out, field, WIRE_64BIT);
    out.extend_from_slice(&value.to_le_bytes());
}

fn proto_len_field(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    proto_key(out, field, WIRE_LENGTH);
    proto_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn proto_string_field(out: &mut Vec<u8>, field: u32, value: &str) {
    proto_len_field(out, field, value.as_bytes());
}

fn proto_message_field(out: &mut Vec<u8>, field: u32, value: Vec<u8>) {
    proto_len_field(out, field, &value);
}

fn proto_packed_varints(out: &mut Vec<u8>, field: u32, values: &[u64]) {
    if values.is_empty() {
        return;
    }
    let mut packed = Vec::with_capacity(values.len() * 2);
    for &value in values {
        proto_varint(&mut packed, value);
    }
    proto_len_field(out, field, &packed);
}

/// One decoded protobuf field: number, wire type, and where its payload sits.
#[derive(Clone, Copy)]
struct ProtoField<'a> {
    number: u32,
    #[allow(dead_code)]
    wire: u8,
    varint: u64,
    /// Length-delimited payload (field data for `WIRE_LENGTH`, all-zero for
    /// scalar wires which store the value in `varint`/fixed slices).
    bytes: &'a [u8],
}

struct ProtoCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoCursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut value = 0_u64;
        let mut shift = 0_u32;
        while self.pos < self.buf.len() {
            let byte = self.buf[self.pos];
            self.pos += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
        None
    }

    fn next(&mut self) -> Option<Option<ProtoField<'a>>> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let key = self.read_varint()?;
        let number = (key >> 3) as u32;
        let wire = (key & 0x07) as u8;
        let field = match wire {
            WIRE_VARINT => ProtoField {
                number,
                wire,
                varint: self.read_varint()?,
                bytes: &[],
            },
            WIRE_64BIT => {
                let end = self.pos.checked_add(8)?;
                if end > self.buf.len() {
                    return Some(None);
                }
                let mut fixed = [0_u8; 8];
                fixed.copy_from_slice(&self.buf[self.pos..end]);
                self.pos = end;
                ProtoField {
                    number,
                    wire,
                    varint: u64::from_le_bytes(fixed),
                    bytes: &[],
                }
            }
            WIRE_LENGTH => {
                let length = self.read_varint()? as usize;
                let end = self.pos.checked_add(length)?;
                if end > self.buf.len() {
                    return Some(None);
                }
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                ProtoField {
                    number,
                    wire,
                    varint: 0,
                    bytes,
                }
            }
            WIRE_32BIT => {
                let end = self.pos.checked_add(4)?;
                if end > self.buf.len() {
                    return Some(None);
                }
                let mut fixed = [0_u8; 4];
                fixed.copy_from_slice(&self.buf[self.pos..end]);
                self.pos = end;
                ProtoField {
                    number,
                    wire,
                    varint: u64::from(u32::from_le_bytes(fixed)),
                    bytes: &[],
                }
            }
            _ => return Some(None),
        };
        Some(Some(field))
    }

    /// Iterate all fields; a malformed tail truncates the iteration rather
    /// than failing the whole message (protobuf decoders tolerate junk tags
    /// poorly, but a truncated frame shouldn't discard parsed fields).
    fn fields(&mut self) -> Vec<ProtoField<'a>> {
        let mut out = Vec::new();
        while let Some(field) = self.next() {
            match field {
                Some(field) => out.push(field),
                None => break,
            }
        }
        out
    }
}

impl ProtoField<'_> {
    fn string(&self) -> &str {
        std::str::from_utf8(self.bytes).unwrap_or("")
    }
    #[allow(dead_code)]
    fn double(&self) -> f64 {
        match self.wire {
            WIRE_64BIT => f64::from_le_bytes(self.varint.to_le_bytes()),
            WIRE_32BIT => f64::from(f32::from_bits(self.varint as u32)),
            WIRE_VARINT => self.varint as f64,
            _ => 0.0,
        }
    }
}

// ---------- Cascade message encoders ----------

/// `Metadata` for released-CLI calls (GetUserJwt, AssignModel, GetChatMessage).
fn encode_cli_metadata(api_key: &str, user_jwt: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    proto_string_field(&mut out, 1, "devin-cli");
    proto_string_field(&mut out, 2, "3000.11.205"); // extension_version
    proto_string_field(&mut out, 3, &encode_devin_credential(api_key));
    proto_string_field(&mut out, 4, "en");
    proto_string_field(&mut out, 5, devin_os());
    proto_string_field(&mut out, 7, "3000.11.205"); // ide_version
    proto_string_field(&mut out, 12, "chisel"); // extension_name
    proto_string_field(&mut out, 21, user_jwt);
    proto_string_field(&mut out, 28, "chisel"); // ide_type
    out
}

/// `Metadata` for the dev-channel `GetCliModelConfigs` call.
fn encode_discovery_metadata(api_key: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    proto_string_field(&mut out, 1, "chisel");
    proto_string_field(&mut out, 2, "0.0.0-dev");
    proto_string_field(&mut out, 3, &encode_devin_credential(api_key));
    proto_string_field(&mut out, 4, "en");
    proto_string_field(&mut out, 5, devin_os());
    proto_string_field(&mut out, 7, "0.0.0-dev");
    proto_string_field(&mut out, 12, "chisel");
    proto_packed_varints(&mut out, 30, &DEVIN_SUPPORTED_MODEL_DISPLAYS);
    out
}


/// Message ids are `cascadeId\0index\0role` hashed, like the native client:
/// ids stay stable across content edits/history rebuilds.
fn deterministic_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 4122 variant + version nibble so the ids parse as UUIDs.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}
fn random_uuid() -> String {
    // Entropy source: a monotonic counter salted with pid + clock. Cascade ids
    // only need per-process uniqueness (thread/thread-of-execution markers).
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seed = format!(
        "devin\0{}\0{}\0{:?}\0{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::Instant::now(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    deterministic_uuid_from_bytes(&bytes)
}

fn deterministic_uuid_from_bytes(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

fn encode_image_data(media_type: &str, data: &str) -> Vec<u8> {
    let mut out = Vec::new();
    proto_string_field(&mut out, 1, data);
    proto_string_field(&mut out, 2, media_type);
    out
}

fn encode_tool_call(id: &str, name: &str, arguments_json: &str) -> Vec<u8> {
    let mut out = Vec::new();
    proto_string_field(&mut out, 1, id);
    proto_string_field(&mut out, 2, name);
    proto_string_field(&mut out, 3, arguments_json);
    out
}

/// Replayed ids may come from another provider; Cascade forwards them to the
/// backend serving the chat model, which rejects them opaquely when invalid.
fn wire_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(TOOL_CALL_ID_MAX_LENGTH)
        .collect()
}

/// Cascade's Gemini backend rejects JSON Schema type arrays such as
/// `["number", "null"]` with an opaque `invalid_argument`.
pub(super) fn scalarize_schema_types(schema: &mut Value) {
    match schema {
        Value::Object(fields) => {
            if let Some(Value::Array(types)) = fields.get("type") {
                let nullable = types.iter().any(|kind| kind == "null");
                let first = types.iter().find(|kind| *kind != "null").cloned();
                if let Some(first) = first {
                    fields.insert("type".into(), first);
                    if nullable {
                        fields.insert("nullable".into(), Value::Bool(true));
                    }
                }
            }
            fields.values_mut().for_each(scalarize_schema_types);
        }
        Value::Array(items) => items.iter_mut().for_each(scalarize_schema_types),
        _ => {}
    }
}

fn is_gemini_uid(uid: &str) -> bool {
    uid.starts_with("MODEL_GOOGLE_GEMINI_") || uid.to_ascii_lowercase().contains("gemini")
}

fn encode_prompt_cache_options() -> Vec<u8> {
    let mut out = Vec::new();
    proto_varint_field(&mut out, 1, CACHE_CONTROL_EPHEMERAL);
    out
}

struct ChatPromptParts {
    message_id: String,
    source: u64,
    prompt: String,
    thinking: String,
    signature: String,
    signature_type: String,
    tool_calls: Vec<Vec<u8>>,
    tool_call_id: String,
    tool_result_is_error: bool,
    images: Vec<Vec<u8>>,
}

fn encode_chat_message_prompt(parts: &ChatPromptParts) -> Vec<u8> {
    let mut out = Vec::new();
    proto_string_field(&mut out, 1, &parts.message_id);
    proto_varint_field(&mut out, 2, parts.source);
    proto_string_field(&mut out, 3, &parts.prompt);
    for call in &parts.tool_calls {
        proto_message_field(&mut out, 6, call.clone());
    }
    proto_string_field(&mut out, 7, &parts.tool_call_id);
    proto_bool_field(&mut out, 9, parts.tool_result_is_error);
    for image in &parts.images {
        proto_message_field(&mut out, 10, image.clone());
    }
    proto_string_field(&mut out, 11, &parts.thinking);
    proto_string_field(&mut out, 12, &parts.signature);
    proto_string_field(&mut out, 18, &parts.signature_type);
    out
}

fn encode_tool_definition(name: &str, description: &str, schema: &Value, strict: bool) -> Vec<u8> {
    let mut out = Vec::new();
    proto_string_field(&mut out, 1, name);
    proto_string_field(&mut out, 2, description);
    proto_string_field(&mut out, 3, &serde_json::to_string(schema).unwrap_or_default());
    proto_bool_field(&mut out, 12, strict);
    out
}

fn encode_completion_configuration(max_tokens: u64) -> Vec<u8> {
    let mut out = Vec::new();
    proto_varint_field(&mut out, 1, 1); // num_completions
    proto_varint_field(&mut out, 2, max_tokens);
    proto_varint_field(&mut out, 3, 400); // max_newlines, native client default
    proto_double_field(&mut out, 5, 1.0); // temperature
    proto_varint_field(&mut out, 7, 40); // top_k
    proto_double_field(&mut out, 8, f64::from(f32::from_bits(0x3f733333))); // fround(0.95)
    out
}

fn encode_tool_choice_auto() -> Vec<u8> {
    let mut out = Vec::new();
    proto_string_field(&mut out, 1, "auto"); // oneof choice { option_name = 1 }
    out
}

/// Fields of `GetChatMessageRequest` that precede auth: everything except
/// `metadata` (1), `chat_model_uid` (21) and `model_assignment_jwt` (26), which
/// the pre-request splices in once the fresh JWT/assignment exists.
struct PendingChatBody {
    /// Already-framed Connect envelope of the full request minus auth fields.
    body: Vec<u8>,
    /// chat_model_uid that goes out once auth lands (assigned uid for routers).
    chat_model_uid: String,
    /// Router prompt for AssignModel; only Some for `model_router` models.
    router_prompt: Vec<u8>,
    /// Cascade thread id shared by AssignModel and GetChatMessage.
    cascade_id: String,
    /// Encoded once the chat uid is final: Gemini needs scalar schema types.
    tools: Vec<Value>,
}

/// Text and images of a user/tool turn: blocks replace `content` when present.
fn message_text_and_images(message: &ProviderMessage) -> (String, Vec<Vec<u8>>) {
    let mut prompt = String::new();
    let mut images = Vec::new();
    for block in &message.content_blocks {
        match block {
            super::ProviderContentBlock::Text(text) => prompt.push_str(text),
            super::ProviderContentBlock::Image { media_type, data } => {
                images.push(encode_image_data(media_type, data));
            }
            _ => {}
        }
    }
    if message.content_blocks.is_empty() {
        prompt.clone_from(&message.content);
    }
    (prompt, images)
}

fn build_chat_prompts(adapter: &DevinAdapter, messages: &[ProviderMessage]) -> Vec<ChatPromptParts> {
    let cascade_id = adapter.cascade_id.as_str();
    let mut prompts = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "user" | "system" | "developer" => {
                let (prompt, images) = message_text_and_images(message);
                prompts.push(ChatPromptParts {
                    message_id: deterministic_uuid(&format!(
                        "{cascade_id}\0{index}\0{}",
                        message.role
                    )),
                    source: CHAT_SOURCE_USER,
                    prompt,
                    thinking: String::new(),
                    signature: String::new(),
                    signature_type: String::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: String::new(),
                    tool_result_is_error: false,
                    images,
                });
            }
            "assistant" => {
                let tool_calls = message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        encode_tool_call(&wire_tool_call_id(&call.id), &call.name, &call.arguments)
                    })
                    .collect::<Vec<_>>();
                let reasoning = message
                    .chat_reasoning
                    .as_ref()
                    .filter(|state| state.belongs_to(adapter));
                let thinking = reasoning.map(|state| state.content.clone()).unwrap_or_default();
                let signature = reasoning
                    .and_then(|state| state.details.first())
                    .and_then(|detail| detail.get("signature"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if message.content.is_empty()
                    && thinking.is_empty()
                    && signature.is_empty()
                    && tool_calls.is_empty()
                {
                    continue;
                }
                prompts.push(ChatPromptParts {
                    message_id: deterministic_uuid(&format!(
                        "{cascade_id}\0{index}\0assistant"
                    )),
                    source: CHAT_SOURCE_SYSTEM,
                    prompt: message.content.clone(),
                    thinking,
                    signature,
                    signature_type: String::new(),
                    tool_calls,
                    tool_call_id: String::new(),
                    tool_result_is_error: false,
                    images: Vec::new(),
                });
            }
            "tool" => {
                let tool_call_id =
                    wire_tool_call_id(message.tool_call_id.as_deref().unwrap_or_default());
                let (prompt, images) = message_text_and_images(message);
                prompts.push(ChatPromptParts {
                    message_id: deterministic_uuid(&format!(
                        "{cascade_id}\0{index}\0tool\0{tool_call_id}"
                    )),
                    source: CHAT_SOURCE_TOOL,
                    prompt,
                    thinking: String::new(),
                    signature: String::new(),
                    signature_type: String::new(),
                    tool_calls: Vec::new(),
                    tool_call_id,
                    tool_result_is_error: false,
                    images,
                });
            }
            _ => {}
        }
    }
    prompts
}

/// Encoded size of the history compaction can shrink: everything before the
/// active user turn, because the system prompt and tools never get smaller.
fn compactable_history_bytes(messages: &[ProviderMessage], prompts: &[ChatPromptParts]) -> usize {
    let active_tail = match messages.last().map(|message| message.role.as_str()) {
        Some("user") => 1,
        Some("developer") => messages
            .iter()
            .rev()
            .take_while(|message| matches!(message.role.as_str(), "user" | "developer"))
            .count(),
        _ => 0,
    };
    prompts[..prompts.len().saturating_sub(active_tail)]
        .iter()
        .map(|parts| encode_chat_message_prompt(parts).len())
        .sum()
}

// ---------- catalog ----------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevinModel {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub accepts_images: bool,
    /// Uids the server routes through `AssignModel` rather than serving.
    pub model_router: bool,
}

/// Bundled seed: the two SWE-1.6 lanes must resolve at boot; discovery then
/// replaces them. Cascade's catalog is account-scoped, so nothing else is baked
/// in. Both lanes ignore inline images despite advertising `supports_images`.
const MODELS: &[DevinModel] = &[
    DevinModel {
        id: "swe-1-6-fast",
        name: "SWE-1.6 Fast",
        context_window: 200_000,
        max_output_tokens: 128_000,
        accepts_images: false,
        model_router: false,
    },
    DevinModel {
        id: "swe-1-6",
        name: "SWE-1.6",
        context_window: 200_000,
        max_output_tokens: 128_000,
        accepts_images: false,
        model_router: false,
    },
];

pub fn devin_models() -> &'static [DevinModel] {
    MODELS
}

pub fn devin_model(id: &str) -> Option<&'static DevinModel> {
    MODELS.iter().find(|model| model.id == id)
}

/// The Cascade catalog is account-scoped; any well-formed uid may be served
/// (upstream `GetCliModelConfigs` decides entitlement), so the check is shape
/// only — like the OpenCode gateways' bare-id rule.
pub fn is_devin_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

/// Whether `model` is a server-side router uid that must go through
/// `AssignModel`. Known routers (`adaptive`, `subagent-*`) are handled without
/// a catalog; harness-backed composites (`fusion-*`) are valid chat uids and
/// stay direct.
pub fn is_devin_router_model(model: &str) -> bool {
    model == "adaptive" || model.starts_with("subagent-") || model == "auto"
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DevinCatalogEntry {
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub accepts_images: bool,
    /// Server router uid (AssignModel before GetChatMessage).
    pub model_router: bool,
    /// Account default (`default_override_model_config.model_uid`).
    pub default: bool,
    /// `chat_model_uid` when it differs from `id`: a family's default lane or a
    /// Fusion pairing's lead (the composite uid itself is never servable).
    pub request_model: Option<String>,
    /// Family lanes as `(effort, wire uid)`, lowest effort first.
    pub effort_routes: Vec<(String, String)>,
    pub parallel_tool_calls: bool,
}

impl DevinCatalogEntry {
    /// The wire uid serving this model at `effort`.
    pub fn chat_model_uid(&self, effort: Option<&str>) -> &str {
        effort
            .and_then(|effort| {
                self.effort_routes
                    .iter()
                    .find(|(route, _)| route == effort)
                    .map(|(_, uid)| uid.as_str())
            })
            .or(self.request_model.as_deref())
            .unwrap_or(&self.id)
    }
}

#[derive(Deserialize)]
struct DevinModelsDocument {
    #[serde(default)]
    models: Vec<DevinCatalogEntrySerde>,
}

#[derive(Deserialize)]
struct DevinCatalogEntrySerde {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    accepts_images: bool,
    #[serde(default)]
    model_router: bool,
    #[serde(default)]
    default: bool,
}

/// Parse a cached `~/.slim/devin-models.json` snapshot.
pub fn parse_devin_models_json(bytes: &[u8]) -> Result<Vec<DevinCatalogEntry>, ProviderError> {
    let invalid = |message: &str| ProviderError::InvalidResponse {
        message: format!("Devin catalog {message}"),
    };
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(invalid("exceeds its bound"));
    }
    let document: DevinModelsDocument =
        serde_json::from_slice(bytes).map_err(|_| invalid("schema is invalid"))?;
    if document.models.len() > MAX_CATALOG_ENTRIES {
        return Err(invalid("exceeds its model bound"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for entry in document.models {
        if !is_devin_model_id(&entry.id) {
            continue;
        }
        if !seen.insert(entry.id.clone()) {
            return Err(invalid("contains a duplicate model id"));
        }
        let known = devin_model(&entry.id);
        entries.push(DevinCatalogEntry {
            name: entry
                .name
                .filter(|name| !name.is_empty())
                .or_else(|| known.map(|model| model.name.to_owned()))
                .unwrap_or_else(|| entry.id.clone()),
            context_window: entry
                .context_window
                .or_else(|| known.map(|model| model.context_window))
                .unwrap_or(200_000)
                .max(1),
            max_output_tokens: entry
                .max_output_tokens
                .or_else(|| known.map(|model| model.max_output_tokens))
                .unwrap_or(super::DEFAULT_MAX_OUTPUT_TOKENS)
                .max(1),
            accepts_images: entry.accepts_images,
            model_router: entry.model_router || is_devin_router_model(&entry.id),
            default: entry.default,
            id: entry.id,
            ..DevinCatalogEntry::default()
        });
    }
    Ok(entries)
}

/// Serialize entries for `~/.slim/devin-models.json` (`{version, models:[…]}`).
pub fn encode_devin_models_json(entries: &[DevinCatalogEntry]) -> Vec<u8> {
    let models = entries
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "name": entry.name,
                "context_window": entry.context_window,
                "max_output_tokens": entry.max_output_tokens,
                "accepts_images": entry.accepts_images,
                "model_router": entry.model_router,
                "default": entry.default,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_vec_pretty(&json!({ "version": 1, "models": models }))
        .unwrap_or_else(|_| b"{}".to_vec())
}

#[derive(Default)]
struct ModelFamily {
    label: String,
    /// `(key, value.order, value.name)`.
    entries: Vec<(String, u64, String)>,
    is_default: bool,
}

/// Normalized view of one `ClientModelConfig` (fields Slim uses).
#[derive(Default)]
struct ClientModelConfig {
    label: String,
    disabled: bool,
    supports_images: bool,
    model_uid: String,
    max_tokens: i64,
    info_max_output_tokens: i64,
    info_max_tokens: i64,
    info_display_option: u64,
    info_is_model_router: bool,
    /// ModelInfo.harness_uids = 20 (repeated): harness-backed composites
    /// (`fusion-*`) are valid chat uids themselves — only routers WITHOUT
    /// harnesses take the AssignModel path (omp: composites 404 there).
    info_has_harness_uids: bool,
    /// ModelFeatures.supports_images = 11 when `model_features` is present;
    /// it outranks the config-level flag.
    feature_images: Option<bool>,
    feature_parallel_tools: bool,
    family: Option<ModelFamily>,
    is_default_in_family: bool,
}

fn decode_model_features(bytes: &[u8], config: &mut ClientModelConfig) {
    let mut images = false;
    for feature in ProtoCursor::new(bytes).fields() {
        match feature.number {
            11 => images = feature.varint != 0,
            21 => config.feature_parallel_tools = feature.varint != 0,
            _ => {}
        }
    }
    config.feature_images = Some(images);
}

fn decode_model_family(bytes: &[u8]) -> ModelFamily {
    let mut family = ModelFamily::default();
    for field in ProtoCursor::new(bytes).fields() {
        match field.number {
            1 => field.string().clone_into(&mut family.label),
            2 => {
                let mut key = String::new();
                let mut value = None;
                for entry in ProtoCursor::new(field.bytes).fields() {
                    match entry.number {
                        1 => entry.string().clone_into(&mut key),
                        2 => {
                            let (mut order, mut name) = (0, String::new());
                            for part in ProtoCursor::new(entry.bytes).fields() {
                                match part.number {
                                    1 => order = part.varint,
                                    2 => part.string().clone_into(&mut name),
                                    _ => {}
                                }
                            }
                            value = Some((order, name));
                        }
                        _ => {}
                    }
                }
                if let Some((order, name)) = value {
                    family.entries.push((key, order, name));
                }
            }
            3 => family.is_default = field.varint != 0,
            _ => {}
        }
    }
    family
}

fn decode_client_model_config(bytes: &[u8]) -> ClientModelConfig {
    let mut config = ClientModelConfig::default();
    for field in ProtoCursor::new(bytes).fields() {
        match field.number {
            1 => field.string().clone_into(&mut config.label),
            4 => config.disabled = field.varint != 0,
            5 => config.supports_images = field.varint != 0,
            18 => config.max_tokens = field.varint as i64,
            22 => field.string().clone_into(&mut config.model_uid),
            23 => {
                for info in ProtoCursor::new(field.bytes).fields() {
                    match info.number {
                        4 => config.info_max_tokens = info.varint as i64,
                        6 => decode_model_features(info.bytes, &mut config),
                        13 => config.info_max_output_tokens = info.varint as i64,
                        20 => config.info_has_harness_uids = true,
                        22 => config.info_display_option = info.varint,
                        25 => config.info_is_model_router = info.varint != 0,
                        _ => {}
                    }
                }
            }
            30 => config.family = Some(decode_model_family(field.bytes)),
            31 => config.is_default_in_family = field.varint != 0,
            _ => {}
        }
    }
    config
}

/// Effort ladder, lowest first; `off` routes a no-thinking lane.
const FAMILY_EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

fn family_effort(name: &str) -> Option<&'static str> {
    let name: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match name.as_str() {
        "none" | "nothinking" => Some("off"),
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

/// Lowercase, with every non-alphanumeric run collapsed to `separator`.
fn slug(input: &str, separator: char) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with(separator) {
            out.push(separator);
        }
    }
    out.truncate(out.trim_end_matches(separator).len());
    out
}

/// One server-declared family lane: fast service and 1M context are separate
/// logical models, and reasoning effort is the lane's only selectable axis.
struct FamilyLane {
    id: String,
    name: String,
    members: Vec<String>,
    default_member: Option<String>,
    routing: BTreeMap<&'static str, String>,
}

fn collect_family_lane(lanes: &mut Vec<FamilyLane>, config: &ClientModelConfig, uid: &str) {
    let Some(family) = &config.family else {
        return;
    };
    let label = family.label.trim();
    if label.is_empty() {
        return;
    }
    let (mut effort, mut thinking, mut fast, mut one_million) = (None, None, false, false);
    for (key, order, name) in &family.entries {
        match slug(key, ' ').as_str() {
            "fast mode" => fast = *order == 1,
            "thinking" => thinking = Some(*order == 1),
            "1m context" => one_million = *order == 1,
            "effort" | "reasoning effort" => effort = family_effort(name),
            _ => {}
        }
    }
    // Claude pairs share one "High" label; the Thinking axis decides the route.
    if thinking == Some(false) {
        effort = Some("off");
    }
    let base = slug(label, '-');
    if base.is_empty() {
        return;
    }
    let (context_suffix, fast_suffix) = (
        if one_million { "1m" } else { "" },
        if fast { "fast" } else { "" },
    );
    let id = [base.as_str(), context_suffix, fast_suffix]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let position = lanes.iter().position(|lane| lane.id == id).unwrap_or_else(|| {
        lanes.push(FamilyLane {
            name: format!(
                "{label}{}{}",
                if one_million { " 1M" } else { "" },
                if fast { " Fast" } else { "" }
            ),
            id,
            members: Vec::new(),
            default_member: None,
            routing: BTreeMap::new(),
        });
        lanes.len() - 1
    });
    let lane = &mut lanes[position];
    lane.members.push(uid.to_owned());
    if lane.default_member.is_none() && (config.is_default_in_family || family.is_default) {
        lane.default_member = Some(uid.to_owned());
    }
    if let Some(effort) = effort {
        lane.routing.entry(effort).or_insert_with(|| uid.to_owned());
    }
}

/// Collapse the lanes that declare an effort ladder into one logical model; a
/// lane with nothing to route leaves its members standalone.
fn collapse_family_lanes(
    entries: &mut Vec<DevinCatalogEntry>,
    lanes: Vec<FamilyLane>,
    account_default: &str,
) {
    for lane in lanes {
        let mut effort_routes = FAMILY_EFFORTS
            .iter()
            .filter_map(|effort| {
                lane.routing
                    .get(effort)
                    .map(|uid| ((*effort).to_owned(), uid.clone()))
            })
            .collect::<Vec<_>>();
        if effort_routes.is_empty() {
            continue;
        }
        if let Some(off) = lane.routing.get("off") {
            effort_routes.insert(0, ("off".to_owned(), off.clone()));
        }
        let head = lane
            .default_member
            .as_ref()
            .or_else(|| lane.members.first())
            .cloned()
            .unwrap_or_default();
        let Some(mut family) = entries.iter().find(|entry| entry.id == head).cloned() else {
            continue;
        };
        family.default = entries
            .iter()
            .any(|entry| entry.default && lane.members.contains(&entry.id));
        // The account default outranks the family's own lane when an effort
        // routes to it; a no-thinking default keeps the family's lane.
        let account_routed = effort_routes
            .iter()
            .any(|(effort, uid)| effort != "off" && uid == account_default);
        family.request_model = Some(if account_routed {
            account_default.to_owned()
        } else {
            head
        });
        family.id = lane.id;
        family.name = lane.name;
        family.effort_routes = effort_routes;
        entries.retain(|entry| entry.id != family.id && !lane.members.contains(&entry.id));
        entries.push(family);
    }
}

/// Lead chat uid of `fusion-<lead>[-fast]-sidekick-<sidekick>`: `None` for
/// other uids, `Some(None)` for a pairing whose lead is not live.
fn fusion_lead(uid: &str, live: &BTreeMap<String, &ClientModelConfig>) -> Option<Option<String>> {
    let rest = uid.strip_prefix("fusion-")?;
    let cut = rest.find("-sidekick-").filter(|cut| *cut > 0)?;
    let lead = &rest[..cut];
    if live.contains_key(lead) {
        return Some(Some(lead.to_owned()));
    }
    let Some(base) = lead.strip_suffix("-fast") else {
        return Some(None);
    };
    let priority = format!("{base}-priority");
    if live.contains_key(&priority) {
        return Some(Some(priority));
    }
    Some(live.contains_key(base).then(|| base.to_owned()))
}

fn config_context_window(config: &ClientModelConfig) -> u64 {
    if config.max_tokens > 0 {
        config.max_tokens as u64
    } else if config.info_max_tokens > 0 {
        config.info_max_tokens as u64
    } else {
        200_000
    }
}

fn config_max_output_tokens(config: &ClientModelConfig) -> u32 {
    u32::try_from(config.info_max_output_tokens)
        .ok()
        .filter(|tokens| *tokens > 0)
        .unwrap_or(64_000)
}

fn config_accepts_images(config: &ClientModelConfig, uid: &str) -> bool {
    config.feature_images.unwrap_or(config.supports_images)
        && !matches!(uid, "swe-1-6" | "swe-1-6-fast")
}

/// Decode a `GetCliModelConfigsResponse`: `client_model_configs = 1`,
/// `default_override_model_config = 3 { model_uid = 3 }`.
fn normalize_model_configs(payload: &[u8]) -> Vec<DevinCatalogEntry> {
    let mut default_uid = String::new();
    let mut configs = Vec::new();
    for field in ProtoCursor::new(payload).fields() {
        match field.number {
            1 => configs.push(decode_client_model_config(field.bytes)),
            3 => {
                for nested in ProtoCursor::new(field.bytes).fields() {
                    if nested.number == 3 {
                        nested.string().trim().clone_into(&mut default_uid);
                    }
                }
            }
            _ => {}
        }
    }
    let mut live = BTreeMap::new();
    for config in configs.iter().filter(|config| !config.disabled) {
        let uid = config.model_uid.trim();
        if !uid.is_empty() {
            live.entry(uid.to_owned()).or_insert(config);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    let mut lanes = Vec::new();
    for config in &configs {
        let uid = config.model_uid.trim();
        if config.disabled
            || is_internal_display(config.info_display_option)
            || uid.is_empty()
            || !seen.insert(uid.to_owned())
        {
            continue;
        }
        let lead = match fusion_lead(uid, &live) {
            Some(None) => continue,
            Some(Some(lead)) => Some(lead),
            None => None,
        };
        let source = lead.as_deref().and_then(|lead| live.get(lead).copied());
        let limits = source.unwrap_or(config);
        let is_router =
            config.info_display_option == DISPLAY_OPTION_MODEL_ROUTER || config.info_is_model_router;
        entries.push(DevinCatalogEntry {
            id: uid.to_owned(),
            name: if config.label.trim().is_empty() {
                uid.to_owned()
            } else {
                config.label.trim().to_owned()
            },
            context_window: config_context_window(limits),
            max_output_tokens: config_max_output_tokens(limits),
            accepts_images: config_accepts_images(limits, lead.as_deref().unwrap_or(uid)),
            model_router: is_router && !config.info_has_harness_uids,
            default: uid == default_uid,
            request_model: lead,
            effort_routes: Vec::new(),
            parallel_tool_calls: limits.feature_parallel_tools,
        });
        // A router is a dispatcher, not an effort tier, even when filed under a family.
        if !is_router {
            collect_family_lane(&mut lanes, config, uid);
        }
    }
    collapse_family_lanes(&mut entries, lanes, &default_uid);
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    entries
}

/// Fetch the account catalog through `GetCliModelConfigs` and normalize it.
/// Returns `Err` on transport/decode failure; callers fall back to the seed
/// table or the cached snapshot.
pub async fn fetch_devin_model_configs(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<DevinCatalogEntry>, ProviderError> {
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        DEVIN_MODEL_CONFIGS_PATH
    );
    let mut body = Vec::with_capacity(192);
    proto_len_field(&mut body, 1, &encode_discovery_metadata(api_key));
    let response = client
        .post(&url)
        .header("content-type", "application/proto")
        .header("connect-protocol-version", "1")
        .header("accept", "*/*")
        .timeout(Duration::from_secs(15))
        .body(body)
        .send()
        .await
        .map_err(|error| ProviderError::Transport {
            safe_to_retry: error.is_connect() || error.is_timeout(),
            message: format!(
                "Devin catalog connection: {}",
                redact_values(&error.without_url().to_string(), &[api_key.to_owned()])
            ),
        })?;
    if !response.status().is_success() {
        return Err(ProviderError::Remote {
            message: format!("Devin catalog failed ({})", response.status().as_u16()),
        });
    }
    let payload = response
        .bytes()
        .await
        .map_err(|error| ProviderError::Transport {
            safe_to_retry: false,
            message: format!(
                "Devin catalog body: {}",
                redact_values(&error.without_url().to_string(), &[api_key.to_owned()])
            ),
        })?;
    if payload.len() > MAX_MODEL_CONFIGS_BYTES {
        return Err(ProviderError::InvalidResponse {
            message: "Devin catalog exceeds its bound".into(),
        });
    }
    let entries = normalize_model_configs(&payload);
    if entries.is_empty() {
        // The backend gates the catalog on the pinned client identity; an
        // empty 200 is the failure signature of a stale pin.
        return Err(ProviderError::InvalidResponse {
            message: "Devin catalog returned no usable models".into(),
        });
    }
    Ok(entries)
}

// ---------- response decoding ----------

#[derive(Default)]
struct ChatDelta {
    delta_text: String,
    delta_thinking: String,
    delta_signature: String,
    stop_reason: u64,
    tool_calls: Vec<(String, String, String)>, // id, name, arguments_json
    input_tokens: u64,
    output_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens: u64,
    has_usage: bool,
}

fn decode_chat_delta(bytes: &[u8]) -> ChatDelta {
    let mut delta = ChatDelta::default();
    for field in ProtoCursor::new(bytes).fields() {
        match field.number {
            3 => field.string().clone_into(&mut delta.delta_text),
            5 => delta.stop_reason = field.varint,
            6 => {
                // ChatToolCall { id=1, name=2, arguments_json=3 }
                let mut id = String::new();
                let mut name = String::new();
                let mut args = String::new();
                for call in ProtoCursor::new(field.bytes).fields() {
                    match call.number {
                        1 => call.string().clone_into(&mut id),
                        2 => call.string().clone_into(&mut name),
                        3 => call.string().clone_into(&mut args),
                        _ => {}
                    }
                }
                delta.tool_calls.push((id, name, args));
            }
            7 => {
                // ModelUsageStats { input=2, output=3, cache_write=4, cache_read=5 }
                for usage in ProtoCursor::new(field.bytes).fields() {
                    match usage.number {
                        2 => delta.input_tokens = usage.varint,
                        3 => delta.output_tokens = usage.varint,
                        4 => delta.cache_write_tokens = usage.varint,
                        5 => delta.cache_read_tokens = usage.varint,
                        _ => {}
                    }
                }
                delta.has_usage = true;
            }
            9 => field.string().clone_into(&mut delta.delta_thinking),
            10 => field.string().clone_into(&mut delta.delta_signature),
            _ => {}
        }
    }
    delta
}

#[derive(Default)]
struct UserJwt {
    user_jwt: String,
    custom_api_server_url: String,
}

fn decode_get_user_jwt(bytes: &[u8]) -> Option<UserJwt> {
    let mut response = UserJwt::default();
    for field in ProtoCursor::new(bytes).fields() {
        match field.number {
            1 => field.string().clone_into(&mut response.user_jwt),
            2 => field.string().clone_into(&mut response.custom_api_server_url),
            _ => {}
        }
    }
    (!response.user_jwt.is_empty()).then_some(response)
}

#[derive(Default)]
struct ModelAssignment {
    assignment_jwt: String,
    model_uid: String,
}

fn decode_assign_model(bytes: &[u8]) -> Option<ModelAssignment> {
    for field in ProtoCursor::new(bytes).fields() {
        if field.number != 1 {
            continue;
        }
        let mut assignment = ModelAssignment::default();
        for nested in ProtoCursor::new(field.bytes).fields() {
            match nested.number {
                1 => nested.string().clone_into(&mut assignment.assignment_jwt),
                2 => nested.string().clone_into(&mut assignment.model_uid),
                _ => {}
            }
        }
        if !assignment.assignment_jwt.is_empty() && !assignment.model_uid.is_empty() {
            return Some(assignment);
        }
    }
    None
}

/// Decode a unary response that may arrive as a bare protobuf or wrapped in a
/// Connect envelope (edges differ). Bare decode is tried first.
fn decode_unary<T>(
    payload: &[u8],
    decode: impl Fn(&[u8]) -> Option<T>,
) -> Option<T> {
    if let Some(decoded) = decode(payload) {
        return Some(decoded);
    }
    // Connect envelope: skip the 5-byte header when it matches the payload
    // length exactly (unary responses are a single frame).
    if payload.len() >= 5 {
        let length = u32::from_be_bytes(payload[1..5].try_into().ok()?) as usize;
        if length == payload.len() - 5 {
            return decode(&payload[5..]);
        }
    }
    None
}

// ---------- stream state ----------

#[derive(Default)]
struct DevinStreamState {
    reasoning_open: bool,
    /// Accumulated `arguments_json` per tool-call id, so cumulative and
    /// chunked argument streams both finish with the full payload.
    tool_args: BTreeMap<String, String>,
    tool_names: BTreeMap<String, String>,
    last_tool_id: Option<String>,
    stop_reason: u64,
    saw_trailer: bool,
    /// True once a usage block was forwarded upstream (server re-sends usage).
    usage_reported: bool,
    usage: UsageBreakdown,
    has_usage: bool,
    /// Latest `delta_signature`; replayed with the thinking on the next turn.
    signature: String,
    saw_output: bool,
}

// ---------- adapter ----------

pub struct DevinAdapter {
    endpoint: String,
    model_id: String,
    api_key: String,
    cascade_id: String,
    max_output_tokens: u32,
    system_prompt: Option<String>,
    response_cache_scope_id: Option<u64>,
    model_router: bool,
    reasoning_effort: Option<String>,
    /// Final `chat_model_uid` (or router uid); `model_id` is the logical model.
    chat_model_uid: String,
    parallel_tool_calls: bool,
    /// See [`compactable_history_bytes`]; recorded per prepared request.
    history_bytes: AtomicUsize,
    /// One in-flight request's pending semantic body; consumed by
    /// `connect_preflight` when auth splicing happens.
    pending: Mutex<Option<PendingChatBody>>,
    stream: Mutex<DevinStreamState>,
}

impl DevinAdapter {
    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        if !is_devin_model_id(model) {
            return Err(ProviderError::InvalidResponse {
                message: format!("unsupported Devin model: {model}"),
            });
        }
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            model_id: model.to_owned(),
            api_key: api_key.to_owned(),
            cascade_id: random_uuid(),
            max_output_tokens: devin_model(model)
                .map_or(super::DEFAULT_MAX_OUTPUT_TOKENS, |model| {
                    model.max_output_tokens
                }),
            system_prompt: None,
            response_cache_scope_id: Some(super::next_response_cache_scope_id()),
            model_router: devin_model(model).is_some_and(|model| model.model_router)
                || is_devin_router_model(model),
            reasoning_effort: reasoning_effort.map(str::to_owned),
            chat_model_uid: model.to_owned(),
            parallel_tool_calls: false,
            history_bytes: AtomicUsize::new(0),
            pending: Mutex::new(None),
            stream: Mutex::new(DevinStreamState::default()),
        })
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = tokens.max(1);
        self
    }

    /// Applies what discovery knows about this model: its wire uid at the
    /// session's effort, router status and parallel-tool support.
    pub fn with_catalog_entry(mut self, entry: &DevinCatalogEntry) -> Self {
        self.chat_model_uid = entry
            .chat_model_uid(self.reasoning_effort.as_deref())
            .to_owned();
        self.model_router = entry.model_router;
        self.parallel_tool_calls = entry.parallel_tool_calls;
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Threads the session's turns on the server: `cascade_id` is its id.
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.cascade_id = id.into();
        self
    }

    pub fn with_response_cache_scope_id(mut self, id: u64) -> Self {
        self.response_cache_scope_id = Some(id);
        self
    }

    fn lock_stream(&self) -> std::sync::MutexGuard<'_, DevinStreamState> {
        self.stream.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Build the pending chat body for `messages` + `tools`.
    fn build_pending(&self, messages: &[ProviderMessage], tools: &[Value]) -> PendingChatBody {
        let prompts = build_chat_prompts(self, messages);
        self.history_bytes.store(
            compactable_history_bytes(messages, &prompts),
            Ordering::Relaxed,
        );
        let mut body = Vec::with_capacity(4096);
        let system_prompt = self.system_prompt.as_deref().unwrap_or_default();
        proto_string_field(&mut body, 2, system_prompt);
        for parts in &prompts {
            proto_message_field(&mut body, 3, encode_chat_message_prompt(parts));
        }
        proto_varint_field(&mut body, 7, REQUEST_TYPE_CASCADE);
        proto_message_field(&mut body, 8, encode_completion_configuration(self.max_output_tokens.into()));
        proto_bool_field(&mut body, 11, !self.parallel_tool_calls);
        proto_message_field(&mut body, 12, encode_tool_choice_auto());
        proto_message_field(&mut body, 13, encode_prompt_cache_options());
        proto_string_field(&mut body, 16, &self.cascade_id);
        proto_varint_field(&mut body, 20, PLANNER_MODE_DEFAULT);
        proto_string_field(&mut body, 22, &random_uuid());

        let router_prompt = if self.model_router {
            // The router scores the current user turn on its own.
            messages
                .iter()
                .rev()
                .find(|message| matches!(message.role.as_str(), "user" | "developer"))
                .map(|message| {
                    let (prompt, images) = message_text_and_images(message);
                    encode_chat_message_prompt(&ChatPromptParts {
                        message_id: String::new(),
                        source: CHAT_SOURCE_USER,
                        prompt,
                        thinking: String::new(),
                        signature: String::new(),
                        signature_type: String::new(),
                        tool_calls: Vec::new(),
                        tool_call_id: String::new(),
                        tool_result_is_error: false,
                        images,
                    })
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        PendingChatBody {
            body,
            chat_model_uid: self.chat_model_uid.clone(),
            router_prompt,
            cascade_id: self.cascade_id.clone(),
            tools: tools.to_vec(),
        }
    }

    /// `ChatToolDefinition`s for the final uid, as repeated field 10.
    fn encode_tools(&self, tools: &[Value], chat_model_uid: &str) -> Vec<u8> {
        let gemini = is_gemini_uid(&self.model_id) || is_gemini_uid(chat_model_uid);
        let claude = [self.model_id.as_str(), chat_model_uid].iter().any(|uid| uid.to_ascii_lowercase().contains("claude"));
        let mut out = Vec::new();
        for tool in tools {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut schema = tool
                .get("input_schema")
                .or_else(|| tool.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            if gemini {
                scalarize_schema_types(&mut schema);
            }
            // Claude behind Cascade: the Messages API refuses top-level combinators.
            if claude {
                super::strip_top_level_combinators(&mut schema);
            }
            proto_message_field(
                &mut out,
                10,
                encode_tool_definition(name, description, &schema, false),
            );
        }
        out
    }

    fn prepare(&self, messages: &[ProviderMessage], tools: &[Value]) -> PreparedProviderRequest {
        let pending = self.build_pending(messages, tools);
        // The routed identity is opaque JSON: provider, model, endpoint and the
        // per-item digests — the same inputs a JSON request would fingerprint.
        let summary = json!({
            "provider": "devin",
            "model": self.model_id,
            "system_prompt": self.system_prompt.as_deref().unwrap_or_default(),
            "tools": tools.iter().map(|tool| tool.get("name").cloned().unwrap_or_default()).collect::<Vec<_>>(),
            "messages": messages.iter().map(|message| json!({
                "role": message.role,
                "content": message.content,
                "tool_calls": message.tool_calls.iter().map(|call| json!({"id": call.id, "name": call.name, "arguments": call.arguments})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        // The credential travels inside the protobuf `Metadata.api_key`, never
        // in a header; `from_parts` carries it as the scope credential so
        // credential identity + redaction still see it without a wire header.
        let headers = vec![
            ("content-type".to_owned(), "application/connect+proto".to_owned()),
            ("connect-protocol-version".to_owned(), "1".to_owned()),
            ("accept-encoding".to_owned(), "identity".to_owned()),
            ("user-agent".to_owned(), "connect-go/1.18.1 (go1.26.3)".to_owned()),
        ];
        let url = format!("{}{}", self.endpoint, DEVIN_CHAT_MESSAGE_PATH);
        let request = PreparedProviderRequest::from_parts(
            url,
            headers,
            pending.body.clone(),
            serde_json::to_string(&summary).unwrap_or_default(),
            Some(self.api_key.clone()),
            self,
        );
        *self.pending.lock().unwrap_or_else(|error| error.into_inner()) = Some(pending);
        request
    }

    /// Raw-body unary RPC: `body` is the complete protobuf request message.
    async fn unary_rpc(
        client: &reqwest::Client,
        base_url: &str,
        path: &str,
        body: Vec<u8>,
        api_key: &str,
        operation: &str,
    ) -> Result<Vec<u8>, ProviderError> {
        let url = format!("{}{}", base_url.trim_end_matches('/'), path);
        let response = client
            .post(&url)
            .header("content-type", "application/proto")
            .header("connect-protocol-version", "1")
            .header("accept", "*/*")
            .body(body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport {
                safe_to_retry: error.is_connect() || error.is_timeout(),
                message: format!(
                    "Devin {operation} connection: {}",
                    redact_values(&error.without_url().to_string(), &[api_key.to_owned()])
                ),
            })?;
        let status = response.status();
        let payload = response
            .bytes()
            .await
            .map_err(|error| ProviderError::Transport {
                safe_to_retry: false,
                message: format!(
                    "Devin {operation} body: {}",
                    redact_values(&error.without_url().to_string(), &[api_key.to_owned()])
                ),
            })?;
        if !status.is_success() {
            let detail = devin_http_error_detail(&payload);
            return Err(ProviderError::Http {
                status: status.as_u16(),
                retry_after: None,
                message: format!(
                    "devin {operation} error ({}): {}",
                    status.as_u16(),
                    redact_values(&detail, &[api_key.to_owned()])
                ),
            });
        }
        Ok(payload.to_vec())
    }
}

/// Bounded human-readable detail from an error body: UTF-8 text only, or a
/// Connect trailer's JSON `{error:{message}}`; binary payloads degrade to a
/// fixed note instead of leaking protobuf bytes into errors.
fn devin_http_error_detail(payload: &[u8]) -> String {
    const MAX_DETAIL_CHARS: usize = 512;
    if let Ok(text) = std::str::from_utf8(payload) {
        let trimmed = text.trim();
        if trimmed.starts_with('<') {
            return "proxy returned an HTML error page".into();
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(message) = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
            {
                return message.chars().take(MAX_DETAIL_CHARS).collect();
            }
        }
        return trimmed.chars().take(MAX_DETAIL_CHARS).collect();
    }
    format!("binary error body ({} bytes)", payload.len())
}

impl ProviderAdapter for DevinAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Devin
    }

    fn stream_wire(&self) -> StreamWire {
        StreamWire::Connect
    }

    fn model(&self) -> &str {
        &self.model_id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // The wire carries prompt-cache options (EPHEMERAL on the system
        // prompt) and reports cache-read/write tokens in ModelUsageStats.
        ProviderCapabilities {
            supports_prompt_cache_options: true,
            reports_cache_read_tokens: true,
            reports_cache_write_tokens: true,
            ..ProviderCapabilities::default()
        }
    }

    fn sensitive_values(&self) -> Vec<String> {
        let mut values = vec![self.api_key.clone()];
        let encoded = encode_devin_credential(&self.api_key);
        if encoded != self.api_key {
            values.push(encoded);
        }
        values
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        self.response_cache_scope_id
    }

    fn reasoning_classification(&self) -> Option<super::ReasoningClassification> {
        Some(super::ReasoningClassification::Text)
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        // Semantic JSON view used only for cache-namespace identity; the real
        // body is built in `prepare` as protobuf.
        let body = json!({
            "provider": "devin",
            "model": self.model_id,
            "prompt": prompt,
            "system_prompt": self.system_prompt.as_deref().unwrap_or_default(),
        });
        HttpRequest {
            url: format!("{}{}", self.endpoint, DEVIN_CHAT_MESSAGE_PATH),
            headers: vec![(
                "content-type".to_owned(),
                "application/connect+proto".to_owned(),
            )],
            body: serde_json::to_string(&body).unwrap_or_default(),
        }
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        Ok(self.prepare(messages, tools))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        Ok(self.prepare(messages, &[]))
    }

    fn connect_preflight<'a>(
        &'a self,
        client: &'a reqwest::Client,
        request: PreparedProviderRequest,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<PreparedProviderRequest, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .ok_or_else(|| ProviderError::InvalidResponse {
                    message: "devin: prepared request carried no pending body".into(),
                })?;
            // Reset per-request stream state before the first frame can land.
            *self.lock_stream() = DevinStreamState::default();

            // GetUserJwt: bare/protobuf unary call.
            let jwt_payload = Self::unary_rpc(
                client,
                &self.endpoint,
                DEVIN_AUTH_PATH,
                unary_request_body(encode_cli_metadata(&self.api_key, "")),
                &self.api_key,
                "auth",
            )
            .await?;
            let jwt = decode_unary(&jwt_payload, decode_get_user_jwt).ok_or_else(|| {
                ProviderError::InvalidResponse {
                    message: "Devin auth error: GetUserJwt returned an empty user JWT".into(),
                }
            })?;
            let base_url = if jwt.custom_api_server_url.trim().is_empty() {
                self.endpoint.clone()
            } else {
                jwt.custom_api_server_url.trim().trim_end_matches('/').to_owned()
            };

            // Router uids resolve through AssignModel on the same cascade id.
            let (chat_model_uid, assignment_jwt) = if self.model_router {
                let mut assign_body = Vec::with_capacity(512);
                proto_len_field(
                    &mut assign_body,
                    1,
                    &encode_cli_metadata(&self.api_key, &jwt.user_jwt),
                );
                proto_string_field(&mut assign_body, 2, &pending.chat_model_uid);
                proto_string_field(&mut assign_body, 3, &pending.cascade_id);
                proto_message_field(&mut assign_body, 5, pending.router_prompt.clone());
                let assign_payload = Self::unary_rpc(
                    client,
                    &base_url,
                    DEVIN_ASSIGN_MODEL_PATH,
                    assign_body,
                    &self.api_key,
                    "AssignModel",
                )
                .await?;
                let assignment = decode_unary(&assign_payload, decode_assign_model).ok_or_else(
                    || ProviderError::InvalidResponse {
                        message: "Devin AssignModel returned no assignment JWT and model uid".into(),
                    },
                )?;
                (assignment.model_uid, assignment.assignment_jwt)
            } else {
                (pending.chat_model_uid.clone(), String::new())
            };

            // Splice auth + resolved model into the request: field 1 metadata
            // (with user_jwt), field 21 chat_model_uid, field 26 assignment jwt,
            // then the pending body — field order is immaterial to protobuf.
            let mut body = Vec::with_capacity(pending.body.len() + 512);
            proto_len_field(
                &mut body,
                1,
                &encode_cli_metadata(&self.api_key, &jwt.user_jwt),
            );
            proto_string_field(&mut body, 21, &chat_model_uid);
            if !assignment_jwt.is_empty() {
                proto_string_field(&mut body, 26, &assignment_jwt);
            }
            body.extend_from_slice(&pending.body);
            body.extend_from_slice(&self.encode_tools(&pending.tools, &chat_model_uid));

            // Connect streaming wraps the protobuf message in its envelope:
            // 1 flag byte (0x00 uncompressed) + u32be length, like the omp
            // `frameConnectMessage` writer.
            let mut framed = Vec::with_capacity(body.len() + 5);
            framed.push(0);
            framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
            framed.extend_from_slice(&body);

            let mut request = request
                .with_url_and_body(format!("{}{}", base_url, DEVIN_CHAT_MESSAGE_PATH), framed);
            request.push_sensitive_value(jwt.user_jwt);
            if !assignment_jwt.is_empty() {
                request.push_sensitive_value(assignment_jwt);
            }
            Ok(request)
        })
    }

    fn parse_connect_frame(
        &self,
        payload: &[u8],
        trailer: bool,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        if trailer {
            return self.parse_trailer(payload);
        }
        let delta = decode_chat_delta(payload);
        let mut events = Vec::new();
        let mut state = self.lock_stream();
        if !delta.delta_signature.is_empty() {
            state.signature = delta.delta_signature;
        }
        if !delta.delta_thinking.is_empty() {
            state.saw_output = true;
            if !state.reasoning_open {
                state.reasoning_open = true;
                events.push(ProviderEvent::ReasoningStarted);
            }
            if let Some(scope_id) = self.response_cache_scope_id {
                events.push(ProviderEvent::ChatReasoning(super::ChatReasoning {
                    scope_id,
                    model: self.model_id.clone(),
                    content: delta.delta_thinking.clone(),
                    details: Vec::new(),
                }));
            }
            events.push(ProviderEvent::ReasoningDelta(delta.delta_thinking));
        }
        if !delta.delta_text.is_empty() {
            state.saw_output = true;
            if state.reasoning_open {
                state.reasoning_open = false;
                events.push(ProviderEvent::ReasoningEnded);
            }
            events.push(ProviderEvent::TextDelta(delta.delta_text));
        }
        for (id, name, args) in delta.tool_calls {
            let call_id = if !id.is_empty() {
                state.last_tool_id = Some(id.clone());
                id
            } else {
                match state.last_tool_id.clone() {
                    Some(id) => id,
                    None => continue,
                }
            };
            state.saw_output = true;
            let first_fragment = !state.tool_names.contains_key(&call_id);
            if !name.is_empty() {
                state.tool_names.insert(call_id.clone(), name);
            }
            // arguments_json may arrive cumulative (replace) or chunked (append).
            let previous = state.tool_args.get(&call_id).cloned().unwrap_or_default();
            let accumulated = if args.starts_with(&previous) {
                args
            } else {
                format!("{previous}{args}")
            };
            let delta_args = accumulated[previous.len()..].to_owned();
            state.tool_args.insert(call_id.clone(), accumulated);
            if first_fragment || !delta_args.is_empty() {
                events.push(ProviderEvent::ToolCallDelta {
                    index: None,
                    id: Some(call_id.clone()),
                    name: state.tool_names.get(&call_id).cloned(),
                    arguments: delta_args,
                });
            }
        }
        if delta.stop_reason != 0 {
            state.stop_reason = delta.stop_reason;
        }
        if delta.has_usage {
            state.has_usage = true;
            // `input_tokens` excludes cache reads and writes on this wire.
            state.usage.uncached_input_tokens = delta.input_tokens;
            state.usage.cache_write_tokens = delta.cache_write_tokens;
            state.usage.cache_read_tokens = delta.cache_read_tokens;
            state.usage.output_tokens = delta.output_tokens;
            events.push(ProviderEvent::UsagePartial {
                input_tokens: state.usage.total_input_tokens(),
                output_tokens: delta.output_tokens,
                input_complete: !state.usage_reported,
                output_complete: false,
            });
            state.usage_reported = true;
        }
        Ok(events)
    }

    fn parse_event(&self, _value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        Err(ProviderError::InvalidResponse {
            message: "devin: SSE events are not part of the Connect wire".into(),
        })
    }
}

impl DevinAdapter {
    /// End-of-stream trailer: `{error:{code,message}}` or empty. On success the
    /// accumulated tool calls finalize and the terminal usage/stop land.
    fn parse_trailer(&self, payload: &[u8]) -> Result<Vec<ProviderEvent>, ProviderError> {
        let text = String::from_utf8_lossy(payload);
        let text = text.trim();
        if !text.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                if let Some(error) = value.get("error") {
                    let code = error
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("provider rejected the stream");
                    let sensitive = self.sensitive_values();
                    let message = redact_values(message, &sensitive);
                    if self.is_large_history_rejection(code, &message) {
                        return Err(ProviderError::Api {
                            metadata: Box::new(super::ProviderErrorMetadata {
                                status: None,
                                code: Some("context_length_exceeded".to_owned()),
                                error_type: None,
                                detail_code: None,
                                retry_after: None,
                            }),
                            message: format!("devin stream rejected ({code}): {message}"),
                        });
                    }
                    let transient = matches!(
                        code,
                        "internal" | "unavailable" | "resource_exhausted" | "deadline_exceeded"
                    );
                    return Err(if transient {
                        ProviderError::TransientRemote {
                            message: format!("devin stream rejected ({code}): {message}"),
                        }
                    } else {
                        ProviderError::Api {
                            metadata: Box::new(super::ProviderErrorMetadata {
                                status: None,
                                code: Some(code.to_owned()),
                                error_type: None,
                                detail_code: None,
                                retry_after: None,
                            }),
                            message: format!("devin stream rejected ({code}): {message}"),
                        }
                    });
                }
            }
        }
        let mut state = self.lock_stream();
        state.saw_trailer = true;
        let mut events = Vec::new();
        if state.reasoning_open {
            state.reasoning_open = false;
            events.push(ProviderEvent::ReasoningEnded);
        }
        if !state.signature.is_empty() {
            if let Some(scope_id) = self.response_cache_scope_id {
                events.push(ProviderEvent::ChatReasoning(super::ChatReasoning {
                    scope_id,
                    model: self.model_id.clone(),
                    content: String::new(),
                    details: vec![json!({ "signature": state.signature })],
                }));
            }
        }
        // Arguments already streamed as deltas; a call without any still needs
        // valid JSON to execute.
        let empty_calls = state
            .tool_args
            .iter()
            .filter(|(_, arguments)| arguments.is_empty())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in empty_calls {
            events.push(ProviderEvent::ToolCallDelta {
                index: None,
                id: Some(id),
                name: None,
                arguments: "{}".to_owned(),
            });
        }
        if state.has_usage {
            events.push(ProviderEvent::UsageBreakdown { usage: state.usage });
            events.push(ProviderEvent::Usage {
                input_tokens: state.usage.total_input_tokens(),
                output_tokens: state.usage.output_tokens,
            });
        }
        let reason = if !state.tool_names.is_empty() {
            "tool_use"
        } else if state.stop_reason == STOP_REASON_MAX_TOKENS {
            "max_tokens"
        } else {
            "stop"
        };
        events.push(ProviderEvent::Stopped {
            reason: reason.to_owned(),
        });
        Ok(events)
    }

    /// Cascade reports some oversized requests as an opaque internal
    /// `invalid_argument`; only history compaction can shrink can recover it.
    fn is_large_history_rejection(&self, code: &str, message: &str) -> bool {
        code.eq_ignore_ascii_case("invalid_argument")
            && message.to_ascii_lowercase().contains("internal error")
            && !self.lock_stream().saw_output
            && self.history_bytes.load(Ordering::Relaxed) >= LARGE_HISTORY_RECOVERY_BYTES
    }
}

/// `GetUserJwt`/`GetCliModelConfigs` request bodies are a single field-1
/// metadata message; kept as a helper so every unary call encodes identically.
fn unary_request_body(metadata: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(metadata.len() + 8);
    proto_len_field(&mut out, 1, &metadata);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_prefixes_jwt_shape_only() {
        assert_eq!(
            encode_devin_credential("a.b.c"),
            "devin-session-token$a.b.c"
        );
        assert_eq!(encode_devin_credential("sk-ws-abc"), "sk-ws-abc");
        assert_eq!(
            encode_devin_credential("devin-session-token$x.y.z"),
            "devin-session-token$x.y.z"
        );
    }

    #[test]
    fn metadata_encodes_expected_fields() {
        let bytes = encode_cli_metadata("key", "jwt");
        let fields = ProtoCursor::new(&bytes).fields();
        let get = |number: u32| {
            fields
                .iter()
                .find(|field| field.number == number)
                .map(|field| field.string().to_owned())
        };
        assert_eq!(get(1).as_deref(), Some("devin-cli"));
        assert_eq!(get(3).as_deref(), Some("key"));
        assert_eq!(get(21).as_deref(), Some("jwt"));
        assert_eq!(get(28).as_deref(), Some("chisel"));
    }

    #[test]
    fn packed_displays_encode() {
        let bytes = encode_discovery_metadata("k");
        let has_displays = ProtoCursor::new(&bytes)
            .fields()
            .into_iter()
            .any(|field| field.number == 30);
        assert!(has_displays);
    }

    #[test]
    fn model_id_shape() {
        assert!(is_devin_model_id("swe-2"));
        assert!(is_devin_model_id("claude-sonnet-4-6"));
        assert!(!is_devin_model_id(""));
        assert!(!is_devin_model_id("has space"));
    }

    fn family_config(uid: &str, family: &str, effort: &str, default: bool) -> Vec<u8> {
        let mut value = Vec::new();
        proto_string_field(&mut value, 2, effort);
        let mut entry = Vec::new();
        proto_string_field(&mut entry, 1, "Reasoning Effort");
        proto_message_field(&mut entry, 2, value);
        let mut metadata = Vec::new();
        proto_string_field(&mut metadata, 1, family);
        proto_message_field(&mut metadata, 2, entry);
        let mut config = Vec::new();
        proto_string_field(&mut config, 1, uid);
        proto_string_field(&mut config, 22, uid);
        proto_message_field(&mut config, 30, metadata);
        proto_bool_field(&mut config, 31, default);
        config
    }

    #[test]
    fn family_lanes_collapse_into_one_model_with_effort_routes() {
        let mut payload = Vec::new();
        for (uid, effort, default) in [
            ("gpt-5-6-sol-low", "Low", false),
            ("gpt-5-6-sol-high", "High", true),
            ("gpt-5-6-sol-xhigh", "X High", false),
        ] {
            proto_message_field(
                &mut payload,
                1,
                family_config(uid, "GPT-5.6 Sol", effort, default),
            );
        }
        let entries = normalize_model_configs(&payload);
        assert_eq!(entries.len(), 1);
        let family = &entries[0];
        assert_eq!(family.id, "gpt-5-6-sol");
        assert_eq!(family.name, "GPT-5.6 Sol");
        assert_eq!(family.chat_model_uid(None), "gpt-5-6-sol-high");
        assert_eq!(family.chat_model_uid(Some("low")), "gpt-5-6-sol-low");
        assert_eq!(family.chat_model_uid(Some("xhigh")), "gpt-5-6-sol-xhigh");
        assert_eq!(family.chat_model_uid(Some("ultra")), "gpt-5-6-sol-high");
    }

    #[test]
    fn account_default_wins_inside_its_family() {
        let mut payload = Vec::new();
        proto_message_field(&mut payload, 1, family_config("f-low", "F", "Low", true));
        proto_message_field(&mut payload, 1, family_config("f-high", "F", "High", false));
        let mut default = Vec::new();
        proto_string_field(&mut default, 3, "f-high");
        proto_message_field(&mut payload, 3, default);
        let entries = normalize_model_configs(&payload);
        assert!(entries[0].default);
        assert_eq!(entries[0].chat_model_uid(None), "f-high");
    }

    #[test]
    fn fusion_pairing_runs_its_lead_and_is_dropped_without_one() {
        let config = |uid: &str, output: u64| {
            let mut info = Vec::new();
            proto_varint_field(&mut info, 13, output);
            let mut config = Vec::new();
            proto_string_field(&mut config, 22, uid);
            proto_message_field(&mut config, 23, info);
            config
        };
        let mut payload = Vec::new();
        proto_message_field(&mut payload, 1, config("swe-2", 1000));
        proto_message_field(&mut payload, 1, config("fusion-swe-2-fast-sidekick-a", 0));
        proto_message_field(&mut payload, 1, config("fusion-gone-sidekick-a", 0));
        let entries = normalize_model_configs(&payload);
        assert_eq!(entries.len(), 2);
        let fusion = entries.iter().find(|entry| entry.id.starts_with("fusion")).unwrap();
        assert_eq!(fusion.chat_model_uid(None), "swe-2");
        assert_eq!(fusion.max_output_tokens, 1000);
    }

    #[test]
    fn router_needs_no_harness_and_missing_output_cap_defaults() {
        let mut info = Vec::new();
        proto_varint_field(&mut info, 22, DISPLAY_OPTION_MODEL_ROUTER);
        let mut router = Vec::new();
        proto_string_field(&mut router, 22, "adaptive");
        proto_message_field(&mut router, 23, info);
        let mut payload = Vec::new();
        proto_message_field(&mut payload, 1, router);
        let entries = normalize_model_configs(&payload);
        assert!(entries[0].model_router);
        assert_eq!(entries[0].max_output_tokens, 64_000);
        assert_eq!(entries[0].context_window, 200_000);
    }

    #[test]
    fn tool_call_ids_are_sanitized_and_capped() {
        assert_eq!(wire_tool_call_id("call|item.1"), "call_item_1");
        assert_eq!(wire_tool_call_id(&"a".repeat(64)).len(), 40);
    }

    #[test]
    fn gemini_schema_types_become_scalar() {
        let mut schema = json!({"properties": {"a": {"type": ["number", "null"]}}});
        scalarize_schema_types(&mut schema);
        assert_eq!(
            schema["properties"]["a"],
            json!({"type": "number", "nullable": true})
        );
        assert!(is_gemini_uid("MODEL_GOOGLE_GEMINI_2_5_PRO"));
        assert!(is_gemini_uid("gemini-3-1-pro"));
        assert!(!is_gemini_uid("swe-1-6"));
    }

    fn adapter() -> DevinAdapter {
        DevinAdapter::new(DEVIN_DEFAULT_BASE_URL, "swe-1-6", "key", None).unwrap()
    }

    #[test]
    fn tool_call_streams_as_identified_delta_without_a_second_call() {
        let adapter = adapter();
        let mut frame = Vec::new();
        proto_message_field(&mut frame, 6, encode_tool_call("call-1", "read", ""));
        let events = adapter.parse_connect_frame(&frame, false).unwrap();
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::ToolCallDelta { id: Some(id), name: Some(name), .. }]
                if id == "call-1" && name == "read"
        ));
        let events = adapter.parse_connect_frame(b"{}", true).unwrap();
        assert!(!events
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolCall { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallDelta { arguments, .. } if arguments == "{}"
        )));
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::Stopped { reason }) if reason == "tool_use"
        ));
    }

    #[test]
    fn input_tokens_exclude_cache_on_this_wire() {
        let adapter = adapter();
        let mut usage = Vec::new();
        proto_varint_field(&mut usage, 2, 10);
        proto_varint_field(&mut usage, 3, 4);
        proto_varint_field(&mut usage, 4, 2);
        proto_varint_field(&mut usage, 5, 30);
        let mut frame = Vec::new();
        proto_message_field(&mut frame, 7, usage);
        adapter.parse_connect_frame(&frame, false).unwrap();
        let events = adapter.parse_connect_frame(b"", true).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderEvent::Usage { input_tokens: 42, output_tokens: 4 }
        )));
    }

    #[test]
    fn large_history_rejection_is_context_overflow_only_before_output() {
        let adapter = adapter();
        let trailer = br#"{"error":{"code":"invalid_argument","message":"an internal error occurred"}}"#;
        let is_overflow = |error: ProviderError| {
            matches!(error, ProviderError::Api { metadata, .. }
                if metadata.code.as_deref() == Some("context_length_exceeded"))
        };
        assert!(!is_overflow(adapter.parse_connect_frame(trailer, true).unwrap_err()));
        adapter
            .history_bytes
            .store(LARGE_HISTORY_RECOVERY_BYTES, Ordering::Relaxed);
        assert!(is_overflow(adapter.parse_connect_frame(trailer, true).unwrap_err()));
        let mut frame = Vec::new();
        proto_string_field(&mut frame, 3, "partial");
        adapter.parse_connect_frame(&frame, false).unwrap();
        assert!(!is_overflow(adapter.parse_connect_frame(trailer, true).unwrap_err()));
    }

    #[test]
    fn thinking_and_signature_replay_on_the_same_scope() {
        let adapter = adapter().with_response_cache_scope_id(9);
        let mut frame = Vec::new();
        proto_string_field(&mut frame, 9, "plan");
        proto_string_field(&mut frame, 10, "sig");
        let mut states = adapter.parse_connect_frame(&frame, false).unwrap();
        states.extend(adapter.parse_connect_frame(b"", true).unwrap());
        let mut reasoning = None::<super::super::ChatReasoning>;
        for event in states {
            if let ProviderEvent::ChatReasoning(state) = event {
                match reasoning.as_mut() {
                    Some(previous) => {
                        previous.content.push_str(&state.content);
                        previous.details.extend(state.details);
                    }
                    None => reasoning = Some(state),
                }
            }
        }
        let mut message = ProviderMessage::assistant("done", Vec::new());
        message.chat_reasoning = reasoning;
        let prompts = build_chat_prompts(&adapter, &[message.clone()]);
        assert_eq!(prompts[0].thinking, "plan");
        assert_eq!(prompts[0].signature, "sig");
        let other = DevinAdapter::new(DEVIN_DEFAULT_BASE_URL, "swe-1-6-fast", "key", None).unwrap();
        assert!(build_chat_prompts(&other, &[message])[0].signature.is_empty());
    }

    #[test]
    fn text_blocks_replace_content_instead_of_repeating_it() {
        let message = ProviderMessage::user("hello")
            .with_content_blocks(vec![super::super::ProviderContentBlock::text("hello")]);
        assert_eq!(message_text_and_images(&message).0, "hello");
    }

    #[test]
    fn parallel_tools_are_disabled_unless_the_catalog_allows_them() {
        let adapter = adapter();
        let disabled = |adapter: &DevinAdapter| {
            ProtoCursor::new(&adapter.build_pending(&[], &[]).body)
                .fields()
                .iter()
                .any(|field| field.number == 11 && field.varint == 1)
        };
        assert!(disabled(&adapter));
        let entry = DevinCatalogEntry {
            id: "swe-1-6".into(),
            parallel_tool_calls: true,
            ..DevinCatalogEntry::default()
        };
        assert!(!disabled(&adapter.with_catalog_entry(&entry)));
    }

    #[test]
    fn chat_delta_decodes_text_usage_stop() {
        let mut payload = Vec::new();
        proto_string_field(&mut payload, 3, "hello");
        proto_varint_field(&mut payload, 5, STOP_REASON_MAX_TOKENS);
        let mut usage = Vec::new();
        proto_varint_field(&mut usage, 2, 10);
        proto_varint_field(&mut usage, 3, 4);
        proto_message_field(&mut payload, 7, usage);
        let delta = decode_chat_delta(&payload);
        assert_eq!(delta.delta_text, "hello");
        assert_eq!(delta.stop_reason, STOP_REASON_MAX_TOKENS);
        assert!(delta.has_usage);
        assert_eq!(delta.input_tokens, 10);
        assert_eq!(delta.output_tokens, 4);
    }
}
