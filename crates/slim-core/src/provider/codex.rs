use std::collections::HashSet;

use serde_json::{json, Value};

use super::{
    endpoint_sensitive_values, harden_compaction_body, normalize_messages,
    provider_native_prompt_cache_key, HttpRequest, PreparedProviderRequest, ProviderAdapter,
    ProviderAuth, ProviderCapabilities, ProviderConfig, ProviderContentBlock, ProviderError,
    ProviderEvent, ProviderKind, ProviderMessage, UsageBreakdown,
};

pub struct OpenAiCodexAdapter {
    config: ProviderConfig,
}

impl OpenAiCodexAdapter {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        if config.kind != ProviderKind::OpenAiCodex {
            return Err(ProviderError::InvalidResponse {
                message: "provider kind mismatch".into(),
            });
        }
        match config.auth() {
            ProviderAuth::OAuth {
                account_id: Some(account_id),
                ..
            } if !account_id.is_empty() => Ok(Self { config }),
            _ => Err(ProviderError::InvalidResponse {
                message: "Codex OAuth requires a ChatGPT account id".into(),
            }),
        }
    }

    pub(crate) fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.config.system_prompt_override = Some(prompt.into());
    }

    fn headers(&self) -> Vec<(String, String)> {
        let ProviderAuth::OAuth {
            access_token,
            account_id: Some(account_id),
        } = self.config.auth()
        else {
            unreachable!("validated by constructor")
        };
        vec![
            ("Authorization".into(), format!("Bearer {access_token}")),
            ("chatgpt-account-id".into(), account_id.clone()),
            ("originator".into(), "slim".into()),
            ("User-Agent".into(), "slim/0.1.0".into()),
            ("OpenAI-Beta".into(), "responses=experimental".into()),
            ("accept".into(), "text/event-stream".into()),
            ("content-type".into(), "application/json".into()),
        ]
    }

    fn request(&self, messages: &[ProviderMessage], tools: &[Value]) -> HttpRequest {
        self.request_with_transport(
            codex_url(&self.config.endpoint),
            self.headers(),
            messages,
            tools,
        )
    }

    pub(super) fn request_with_transport(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        HttpRequest {
            url,
            headers,
            body: self.request_body(messages, tools).to_string(),
        }
    }

    pub(super) fn request_body(&self, messages: &[ProviderMessage], tools: &[Value]) -> Value {
        let mut input = Vec::new();
        for message in messages {
            if message.role == "tool" {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message.content,
                }));
                continue;
            }
            input.push(json!({
                "role": message.role,
                "content": codex_content(message),
            }));
            for call in &message.tool_calls {
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                }));
            }
        }
        let tools = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.get("name").cloned().unwrap_or(Value::Null),
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object"})),
                })
            })
            .collect::<Vec<_>>();
        let mut body = json!({
            "model": self.config.model,
            "store": false,
            "stream": true,
            "instructions": self.config.effective_system_prompt().unwrap_or(""),
            "input": input,
            "include": ["reasoning.encrypted_content"],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "tools": tools,
        });
        let mut reasoning = json!({ "summary": "auto" });
        if let Some(effort) = self.config.reasoning_effort.as_deref() {
            reasoning["effort"] = json!(effort);
        }
        body["reasoning"] = reasoning;
        self.materialize_prompt_cache_intent(&mut body);
        body
    }
}

fn codex_content(message: &ProviderMessage) -> Vec<Value> {
    let text_type = if message.role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    let mut content = Vec::new();
    if !message.content.is_empty() {
        content.push(json!({"type": text_type, "text": message.content}));
    }
    for block in &message.content_blocks {
        match block {
            ProviderContentBlock::Text(text) => {
                content.push(json!({"type": text_type, "text": text}));
            }
            ProviderContentBlock::Image { media_type, data } if message.role != "assistant" => {
                content.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                }));
            }
            ProviderContentBlock::Image { .. } => content.push(json!({
                "type": text_type,
                "text": "[assistant image omitted]",
            })),
            ProviderContentBlock::Audio { media_type, data } => content.push(json!({
                "type": text_type,
                "text": format!("[audio omitted: {media_type}; base64-bytes={}]", data.len()),
            })),
            ProviderContentBlock::File { media_type, data } => content.push(json!({
                "type": text_type,
                "text": format!("[file omitted: {media_type}; base64-bytes={}]", data.len()),
            })),
            ProviderContentBlock::Unsupported { kind } => content.push(json!({
                "type": text_type,
                "text": format!("[unsupported content omitted: {kind}]"),
            })),
        }
    }
    if content.is_empty() {
        content.push(json!({"type": text_type, "text": ""}));
    }
    content
}

impl ProviderAdapter for OpenAiCodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCodex
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_prompt_cache_key: true,
            reports_cache_read_tokens: true,
            reports_cache_write_tokens: true,
            ..ProviderCapabilities::default()
        }
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        if self.capabilities().supports_prompt_cache_key {
            body["prompt_cache_key"] = Value::String(provider_native_prompt_cache_key(
                self.wire_kind(),
                self.model(),
                body,
            ));
        }
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        Some(self.config.response_cache_scope_id())
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.config.effective_system_prompt()
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        Some(1_024)
    }

    fn sensitive_values(&self) -> Vec<String> {
        endpoint_sensitive_values(&self.config.endpoint)
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.request(&[ProviderMessage::user(prompt)], &[])
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        self.request(messages, &[])
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.request(messages, tools)
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        PreparedProviderRequest::from_http_body(
            codex_url(&self.config.endpoint),
            self.headers(),
            self.request_body(messages, tools),
            self,
        )
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        let mut body = self.request_body(messages, &[]);
        harden_compaction_body(&mut body, false)?;
        self.materialize_prompt_cache_intent(&mut body);
        PreparedProviderRequest::from_http_body(
            codex_url(&self.config.endpoint),
            self.headers(),
            body,
            self,
        )
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let events = match event_type {
            "response.output_text.delta" => value
                .get("delta")
                .and_then(Value::as_str)
                .map(|text| vec![ProviderEvent::TextDelta(text.into())])
                .unwrap_or_default(),
            "response.reasoning_summary_text.delta" => value
                .get("delta")
                .and_then(Value::as_str)
                .map(|text| vec![ProviderEvent::ReasoningDelta(text.into())])
                .unwrap_or_default(),
            "response.output_item.added" => {
                let item = value.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let index = required_tool_index(value.get("output_index"))?;
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or(ProviderError::MalformedToolCall)?;
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or(ProviderError::MalformedToolCall)?;
                    let arguments = match item.get("arguments") {
                        Some(arguments) => arguments
                            .as_str()
                            .ok_or(ProviderError::MalformedToolCall)?
                            .to_owned(),
                        None => String::new(),
                    };
                    vec![ProviderEvent::ToolCallDelta {
                        index: Some(index),
                        id: Some(id.into()),
                        name: Some(name.into()),
                        arguments,
                    }]
                } else if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                    vec![ProviderEvent::ReasoningStarted]
                } else {
                    Vec::new()
                }
            }
            "response.function_call_arguments.delta" => vec![ProviderEvent::ToolCallDelta {
                index: Some(required_tool_index(value.get("output_index"))?),
                id: None,
                name: None,
                arguments: value
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or(ProviderError::MalformedToolCall)?
                    .to_owned(),
            }],
            "response.output_item.done" => {
                let item = value.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let index = required_tool_index(value.get("output_index"))?;
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or(ProviderError::MalformedToolCall)?;
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or(ProviderError::MalformedToolCall)?;
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .ok_or(ProviderError::MalformedToolCall)?;
                    vec![
                        ProviderEvent::ToolCallDelta {
                            index: Some(index),
                            id: Some(id.into()),
                            name: Some(name.into()),
                            arguments: String::new(),
                        },
                        ProviderEvent::ToolCall {
                            name: name.into(),
                            arguments: arguments.into(),
                        },
                    ]
                } else if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                    vec![ProviderEvent::ReasoningEnded]
                } else {
                    Vec::new()
                }
            }
            "response.completed" | "response.incomplete" => {
                let usage = value
                    .get("response")
                    .and_then(|response| response.get("usage"));
                let mut events = Vec::new();
                if let Some(usage) = usage.filter(|usage| !usage.is_null()) {
                    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
                    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
                    let cache_read_tokens = usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cache_write_tokens = usage
                        .pointer("/input_tokens_details/cache_write_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let reasoning_tokens = usage
                        .pointer("/output_tokens_details/reasoning_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if input_tokens.is_some()
                        || output_tokens.is_some()
                        || cache_read_tokens > 0
                        || cache_write_tokens > 0
                    {
                        let cached_input_tokens = cache_read_tokens
                            .checked_add(cache_write_tokens)
                            .ok_or_else(|| ProviderError::InvalidResponse {
                                message: "Codex cached input overflowed u64".into(),
                            })?;
                        let uncached_input_tokens = input_tokens
                            .map(|tokens| {
                                tokens.checked_sub(cached_input_tokens).ok_or_else(|| {
                                    ProviderError::InvalidResponse {
                                        message: "Codex cached input exceeded total input".into(),
                                    }
                                })
                            })
                            .transpose()?
                            .unwrap_or(0);
                        events.push(ProviderEvent::UsageBreakdown {
                            usage: UsageBreakdown {
                                uncached_input_tokens,
                                cache_write_tokens,
                                cache_read_tokens,
                                output_tokens: output_tokens.unwrap_or(0),
                                reasoning_tokens,
                                usage_unknown: input_tokens.is_none() || output_tokens.is_none(),
                            },
                        });
                    }
                    match (input_tokens, output_tokens) {
                        (Some(input_tokens), Some(output_tokens)) => {
                            events.push(ProviderEvent::Usage {
                                input_tokens,
                                output_tokens,
                            });
                        }
                        (Some(input_tokens), None) => {
                            events.push(ProviderEvent::UsagePartial {
                                input_tokens,
                                output_tokens: 0,
                                input_complete: true,
                                output_complete: false,
                            });
                        }
                        (None, Some(output_tokens)) => {
                            events.push(ProviderEvent::UsagePartial {
                                input_tokens: 0,
                                output_tokens,
                                input_complete: false,
                                output_complete: true,
                            });
                        }
                        (None, None) => {}
                    }
                }
                let reason = if event_type == "response.incomplete" {
                    value
                        .pointer("/response/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("incomplete")
                } else {
                    "completed"
                };
                events.push(ProviderEvent::Stopped {
                    reason: reason.into(),
                });
                events
            }
            "error" | "response.failed" => {
                return Err(ProviderError::Remote {
                    message: value
                        .pointer("/error/message")
                        .or_else(|| value.pointer("/response/error/message"))
                        .and_then(Value::as_str)
                        .unwrap_or("Codex provider error")
                        .chars()
                        .take(512)
                        .collect(),
                })
            }
            _ => Vec::new(),
        };
        Ok(events)
    }
}

fn required_tool_index(value: Option<&Value>) -> Result<u32, ProviderError> {
    optional_tool_index(value)?.ok_or(ProviderError::MalformedToolCall)
}

fn optional_tool_index(value: Option<&Value>) -> Result<Option<u32>, ProviderError> {
    value
        .map(|value| {
            value
                .as_u64()
                .ok_or(ProviderError::MalformedToolCall)
                .and_then(|index| {
                    u32::try_from(index).map_err(|_| ProviderError::MalformedToolCall)
                })
        })
        .transpose()
}

/// Codex CLI bundled `context_window` for GPT-5.6 Sol/Terra/Luna.
///
/// This is the coding-client catalog value (~272k input). The public API
/// documents 1,050,000; ChatGPT `/backend-api/codex/models` is authoritative
/// per account and `originator` and may return 272k or 872k. The agent-loop
/// default of 32_000 is unrelated and must not be used as model truth.
pub const CODEX_BUNDLED_CONTEXT_WINDOW: u64 = 272_000;

/// `client_version` query the Codex backend requires. Slim's crate version is
/// not a Codex CLI version and would yield an empty catalog.
pub const CODEX_CATALOG_CLIENT_VERSION: &str = "0.149.0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodexModel {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCatalogEntry {
    pub slug: String,
    pub context_window: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexCatalogError {
    InvalidSchema,
    DuplicateSlug,
    Oversized,
}

impl std::fmt::Display for CodexCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSchema => "Codex catalog schema is invalid",
            Self::DuplicateSlug => "Codex catalog contains a duplicate model slug",
            Self::Oversized => "Codex catalog exceeds its bound",
        })
    }
}

impl std::error::Error for CodexCatalogError {}

const CODEX_MODELS: &[CodexModel] = &[
    CodexModel {
        id: "gpt-5.6-sol",
        name: "GPT-5.6 Sol",
        context_window: CODEX_BUNDLED_CONTEXT_WINDOW,
        max_output_tokens: 128_000,
    },
    CodexModel {
        id: "gpt-5.6-terra",
        name: "GPT-5.6 Terra",
        context_window: CODEX_BUNDLED_CONTEXT_WINDOW,
        max_output_tokens: 128_000,
    },
    CodexModel {
        id: "gpt-5.6-luna",
        name: "GPT-5.6 Luna",
        context_window: CODEX_BUNDLED_CONTEXT_WINDOW,
        max_output_tokens: 128_000,
    },
];

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_SLUG_BYTES: usize = 128;

pub fn codex_models() -> &'static [CodexModel] {
    CODEX_MODELS
}

pub fn normalize_codex_model_id(id: &str) -> Option<&'static str> {
    match id.trim().to_ascii_lowercase().as_str() {
        "sol" | "gpt-5.6-sol" => Some("gpt-5.6-sol"),
        "terra" | "gpt-5.6-terra" => Some("gpt-5.6-terra"),
        "luna" | "gpt-5.6-luna" => Some("gpt-5.6-luna"),
        _ => None,
    }
}

pub fn codex_model(id: &str) -> Option<&'static CodexModel> {
    let slug = normalize_codex_model_id(id)?;
    CODEX_MODELS.iter().find(|model| model.id == slug)
}

pub fn resolve_codex_context_window(model: &str, live: Option<&[CodexCatalogEntry]>) -> u64 {
    let Some(slug) = normalize_codex_model_id(model) else {
        return 32_000;
    };
    if let Some(live) = live {
        if let Some(entry) = live.iter().find(|entry| entry.slug == slug) {
            return entry.context_window;
        }
    }
    codex_model(slug)
        .map(|model| model.context_window)
        .unwrap_or(32_000)
}

pub fn parse_codex_catalog(bytes: &[u8]) -> Result<Vec<CodexCatalogEntry>, CodexCatalogError> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CodexCatalogError::Oversized);
    }
    let document: Value =
        serde_json::from_slice(bytes).map_err(|_| CodexCatalogError::InvalidSchema)?;
    let models = document
        .get("models")
        .and_then(Value::as_array)
        .ok_or(CodexCatalogError::InvalidSchema)?;
    if models.len() > MAX_CATALOG_ENTRIES {
        return Err(CodexCatalogError::Oversized);
    }
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for model in models {
        let slug = model
            .get("slug")
            .and_then(Value::as_str)
            .ok_or(CodexCatalogError::InvalidSchema)?;
        if slug.is_empty() || slug.len() > MAX_SLUG_BYTES || !slug_is_safe(slug) {
            return Err(CodexCatalogError::InvalidSchema);
        }
        let context_window = model
            .get("context_window")
            .and_then(Value::as_u64)
            .or_else(|| model.get("max_context_window").and_then(Value::as_u64))
            .filter(|window| *window > 0);
        let Some(context_window) = context_window else {
            continue;
        };
        if !seen.insert(slug) {
            return Err(CodexCatalogError::DuplicateSlug);
        }
        entries.push(CodexCatalogEntry {
            slug: slug.to_owned(),
            context_window,
        });
    }
    Ok(entries)
}

fn slug_is_safe(slug: &str) -> bool {
    slug.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' || ch == '/')
}

/// GET `/codex/models` sibling of the responses URL, with a pinned Codex
/// `client_version` (required by the backend; Slim's own version is rejected).
pub fn codex_models_url(endpoint: &str) -> String {
    let responses = codex_url(endpoint);
    if let Ok(mut url) = reqwest::Url::parse(&responses) {
        let path = url.path().replace("/codex/responses", "/codex/models");
        url.set_path(&path);
        url.query_pairs_mut()
            .append_pair("client_version", CODEX_CATALOG_CLIENT_VERSION);
        return url.to_string();
    }
    let models = responses.replacen("/codex/responses", "/codex/models", 1);
    if models.contains('?') {
        format!("{models}&client_version={CODEX_CATALOG_CLIENT_VERSION}")
    } else {
        format!("{models}?client_version={CODEX_CATALOG_CLIENT_VERSION}")
    }
}

fn codex_url(endpoint: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(endpoint) {
        let path = url.path().trim_end_matches('/');
        let path = if path.ends_with("/codex/responses") {
            path.to_owned()
        } else if path.ends_with("/codex") {
            format!("{path}/responses")
        } else {
            format!("{path}/codex/responses")
        };
        url.set_path(&path);
        return url.to_string();
    }

    let split = endpoint.find(['?', '#']).unwrap_or(endpoint.len());
    let (base, suffix) = endpoint.split_at(split);
    let base = base.trim_end_matches('/');
    let path = if base.ends_with("/codex/responses") {
        base.to_owned()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    };
    format!("{path}{suffix}")
}
