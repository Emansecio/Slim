use serde_json::Value;

use super::{
    endpoint_sensitive_values, harden_compaction_body, materialize_native_prompt_cache_key,
    normalize_messages, AnthropicAdapter, HttpRequest, OpenAiCodexAdapter, OpenAiCompatibleAdapter,
    PreparedProviderRequest, ProviderAdapter, ProviderCapabilities, ProviderConfig, ProviderError,
    ProviderEvent, ProviderKind, ProviderMessage,
};

pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const OPENCODE_GO_MODELS_URL: &str = "https://opencode.ai/zen/go/v1/models";
pub const OPENCODE_GO_DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenCodeApi {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenCodeModel {
    pub id: &'static str,
    pub name: &'static str,
    pub api: OpenCodeApi,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub accepts_images: bool,
    pub reasoning_levels: &'static [&'static str],
}

const REASONING_HIGH: &[&str] = &["high"];
const REASONING_HIGH_MAX: &[&str] = &["high", "max"];
const REASONING_LOW_HIGH: &[&str] = &["low", "high"];
const REASONING_LOW_HIGH_MAX: &[&str] = &["low", "high", "max"];
const REASONING_FULL: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const REASONING_STANDARD: &[&str] = &["low", "medium", "high"];
const REASONING_LOW_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
const REASONING_MAX: &[&str] = &["max"];

const MODELS: &[OpenCodeModel] = &[
    model(
        "grok-4.5",
        "Grok 4.5",
        OpenCodeApi::Responses,
        500000,
        500000,
        true,
        REASONING_STANDARD,
    ),
    model(
        "gpt-5.6-luna",
        "GPT 5.6 Luna",
        OpenCodeApi::Responses,
        1050000,
        128000,
        true,
        REASONING_FULL,
    ),
    model(
        "glm-5.3",
        "GLM-5.3",
        OpenCodeApi::ChatCompletions,
        1000000,
        131072,
        false,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "glm-5.2",
        "GLM-5.2",
        OpenCodeApi::ChatCompletions,
        1000000,
        131072,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "glm-5.1",
        "GLM-5.1",
        OpenCodeApi::ChatCompletions,
        202752,
        32768,
        false,
        REASONING_HIGH,
    ),
    model(
        "kimi-k3",
        "Kimi K3",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_MAX,
    ),
    model(
        "kimi-k2.7-code",
        "Kimi K2.7 Code",
        OpenCodeApi::Responses,
        262144,
        262144,
        true,
        REASONING_HIGH,
    ),
    model(
        "kimi-k2.6",
        "Kimi K2.6",
        OpenCodeApi::Responses,
        262144,
        65536,
        true,
        REASONING_HIGH,
    ),
    model(
        "deepseek-flash",
        "DeepSeek V4.1 Flash",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        true,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "deepseek-v4-flash-vision-exp",
        "DeepSeek V4 Flash Vision Exp",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        true,
        REASONING_HIGH_MAX,
    ),
    model(
        "mimo-v2.5",
        "MiMo-V2.5",
        OpenCodeApi::Responses,
        1000000,
        128000,
        true,
        REASONING_HIGH,
    ),
    model(
        "mimo-v2.5-pro",
        "MiMo-V2.5-Pro",
        OpenCodeApi::Responses,
        1048576,
        128000,
        false,
        REASONING_HIGH,
    ),
    model(
        "minimax-m3",
        "MiniMax M3",
        OpenCodeApi::AnthropicMessages,
        1000000,
        131072,
        true,
        REASONING_HIGH,
    ),
    model(
        "minimax-m2.7",
        "MiniMax M2.7",
        OpenCodeApi::ChatCompletions,
        204800,
        131072,
        false,
        REASONING_HIGH,
    ),
    model(
        "minimax-m2.5",
        "MiniMax M2.5",
        OpenCodeApi::ChatCompletions,
        204800,
        131072,
        false,
        REASONING_HIGH,
    ),
    model(
        "muse-spark-1.2-contributor",
        "Muse Spark 1.2 Contributor",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    // Go routes both Muse contributor models through /responses:
    // https://opencode.ai/docs/go/#endpoints (verified 2026-09-06).
    // Live on the public catalog since 2026-09-02; metadata mirrors the 1.2
    // contributor checkpoint on the same gateway (1M context, 131072 output
    // ceiling, multimodal input, reasoning). Source: Meta model docs +
    // public launch notes, verified 2026-09-03.
    model(
        "muse-spark-1.3-contributor",
        "Muse Spark 1.3 Contributor",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "qwen3.8-max",
        "Qwen3.8 Max",
        OpenCodeApi::AnthropicMessages,
        1000000,
        131072,
        true,
        REASONING_HIGH,
    ),
    model(
        "qwen3.7-max",
        "Qwen3.7 Max",
        OpenCodeApi::ChatCompletions,
        1000000,
        65536,
        false,
        REASONING_HIGH,
    ),
    model(
        "qwen3.7-plus",
        "Qwen3.7 Plus",
        OpenCodeApi::ChatCompletions,
        1000000,
        65536,
        true,
        REASONING_HIGH,
    ),
    model(
        "qwen3.6-plus",
        "Qwen3.6 Plus",
        OpenCodeApi::ChatCompletions,
        1000000,
        65536,
        true,
        REASONING_HIGH,
    ),
    model(
        "hy3",
        "Hy3",
        OpenCodeApi::ChatCompletions,
        256000,
        64000,
        false,
        REASONING_LOW_HIGH,
    ),
    model(
        "ox-alpha-free",
        "Ox Alpha Free",
        OpenCodeApi::ChatCompletions,
        1000000,
        131072,
        true,
        REASONING_HIGH,
    ),
];

pub(super) const fn model(
    id: &'static str,
    name: &'static str,
    api: OpenCodeApi,
    context_window: u64,
    max_output_tokens: u32,
    accepts_images: bool,
    reasoning_levels: &'static [&'static str],
) -> OpenCodeModel {
    OpenCodeModel {
        id,
        name,
        api,
        context_window: Some(context_window),
        max_output_tokens: Some(max_output_tokens),
        accepts_images,
        reasoning_levels,
    }
}

pub fn open_code_models() -> &'static [OpenCodeModel] {
    MODELS
}

pub fn open_code_model(id: &str) -> Option<&'static OpenCodeModel> {
    let id = match id {
        "deepseek-v4.1-flash" => "deepseek-flash",
        other => other,
    };
    MODELS.iter().find(|model| model.id == id)
}

enum WireAdapter {
    Chat(OpenAiCompatibleAdapter),
    Messages(AnthropicAdapter),
    Responses {
        parser: OpenAiCodexAdapter,
        endpoint: String,
        api_key: String,
        max_output_tokens: u32,
    },
}

pub struct OpenCodeGoAdapter {
    model: &'static OpenCodeModel,
    wire: WireAdapter,
    session_id: String,
}

impl OpenCodeGoAdapter {
    pub fn validate_messages(&self, messages: &[ProviderMessage]) -> Result<(), ProviderError> {
        normalize_messages(messages)?;
        if !self.model.accepts_images
            && messages
                .iter()
                .flat_map(|message| &message.content_blocks)
                .any(|block| matches!(block, super::ProviderContentBlock::Image { .. }))
        {
            return Err(ProviderError::InvalidResponse {
                message: format!("OpenCode Go model {} does not accept images", self.model.id),
            });
        }
        Ok(())
    }

    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let model = open_code_model(model).ok_or_else(|| ProviderError::InvalidResponse {
            message: format!("unsupported OpenCode Go model: {model}"),
        })?;
        if reasoning_effort.is_some_and(|effort| !model.reasoning_levels.contains(&effort)) {
            return Err(ProviderError::InvalidResponse {
                message: "unsupported OpenCode Go reasoning effort".into(),
            });
        }
        let wire = match model.api {
            OpenCodeApi::ChatCompletions => {
                let mut config = ProviderConfig::openai(
                    protocol_url(endpoint, "chat/completions"),
                    model.id,
                    api_key,
                );
                if let Some(effort) = reasoning_effort {
                    config = config.with_reasoning_effort(effort);
                }
                WireAdapter::Chat(OpenAiCompatibleAdapter::new(config)?)
            }
            OpenCodeApi::AnthropicMessages => {
                WireAdapter::Messages(AnthropicAdapter::new(ProviderConfig::anthropic_bearer(
                    protocol_url(endpoint, "messages"),
                    model.id,
                    api_key,
                ))?)
            }
            OpenCodeApi::Responses => {
                let endpoint = protocol_url(endpoint, "responses");
                let mut config =
                    ProviderConfig::openai_codex(&endpoint, model.id, api_key, "opencode-go");
                if let Some(effort) = reasoning_effort {
                    config = config.with_reasoning_effort(effort);
                }
                WireAdapter::Responses {
                    parser: OpenAiCodexAdapter::new(config)?,
                    endpoint,
                    api_key: api_key.into(),
                    max_output_tokens: super::DEFAULT_MAX_OUTPUT_TOKENS,
                }
            }
        };
        Ok(Self {
            model,
            wire,
            session_id: Self::new_session_id(),
        })
    }

    pub fn new_session_id() -> String {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!(
            "slim-{}-{stamp}-{}",
            std::process::id(),
            super::next_response_cache_scope_id()
        )
    }

    pub fn with_session_id(mut self, session_id: &str) -> Self {
        use sha2::{Digest, Sha256};
        // Opaque, header-safe identity; never expose a local session path.
        self.session_id = format!("slim-{:x}", Sha256::digest(session_id.as_bytes()));
        self
    }

    fn add_session_headers(&self, headers: &mut Vec<(String, String)>) {
        headers.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("user-agent")
                && !name.eq_ignore_ascii_case("x-opencode-session")
        });
        headers.push((
            "user-agent".into(),
            concat!("slim/", env!("CARGO_PKG_VERSION")).into(),
        ));
        headers.push(("x-opencode-session".into(), self.session_id.clone()));
    }

    pub fn with_response_cache_scope_id(mut self, id: u64) -> Self {
        match &mut self.wire {
            WireAdapter::Chat(adapter) => adapter.set_response_cache_scope_id(id),
            WireAdapter::Messages(adapter) => adapter.set_response_cache_scope_id(id),
            WireAdapter::Responses { parser, .. } => parser.set_response_cache_scope_id(id),
        }
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        match &mut self.wire {
            WireAdapter::Chat(adapter) => {
                adapter.config.max_output_tokens = max_output_tokens.max(1);
            }
            WireAdapter::Messages(adapter) => {
                adapter.config.max_output_tokens = max_output_tokens.max(1);
            }
            WireAdapter::Responses {
                max_output_tokens: limit,
                ..
            } => *limit = max_output_tokens.max(1),
        }
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        match &mut self.wire {
            WireAdapter::Chat(adapter) => adapter.set_system_prompt(prompt),
            WireAdapter::Messages(adapter) => adapter.set_system_prompt(prompt),
            WireAdapter::Responses { parser, .. } => parser.set_system_prompt(prompt),
        }
        self
    }

    fn responses_request(
        parser: &OpenAiCodexAdapter,
        endpoint: &str,
        api_key: &str,
        messages: &[ProviderMessage],
        tools: &[Value],
        max_output_tokens: u32,
    ) -> HttpRequest {
        let mut body = parser.request_body(messages, tools);
        body["max_output_tokens"] = Value::from(max_output_tokens);
        HttpRequest {
            url: endpoint.into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {api_key}")),
                ("accept".into(), "text/event-stream".into()),
                ("content-type".into(), "application/json".into()),
            ],
            body: body.to_string(),
        }
    }
}

impl ProviderAdapter for OpenCodeGoAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenCodeGo
    }

    fn wire_kind(&self) -> ProviderKind {
        match self.model.api {
            OpenCodeApi::ChatCompletions => ProviderKind::OpenAiCompatible,
            OpenCodeApi::Responses => ProviderKind::OpenAiCodex,
            OpenCodeApi::AnthropicMessages => ProviderKind::Anthropic,
        }
    }

    fn model(&self) -> &str {
        self.model.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.capabilities(),
            WireAdapter::Messages(adapter) => adapter.capabilities(),
            WireAdapter::Responses { parser, .. } => parser.capabilities(),
        }
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.materialize_prompt_cache_intent(body),
            WireAdapter::Messages(adapter) => adapter.materialize_prompt_cache_intent(body),
            WireAdapter::Responses { parser, .. } => {
                parser.materialize_prompt_cache_intent(body);
            }
        }
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.response_cache_scope_id(),
            WireAdapter::Responses { parser, .. } => parser.response_cache_scope_id(),
            WireAdapter::Messages(adapter) => adapter.response_cache_scope_id(),
        }
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.system_prompt_for_budget(),
            WireAdapter::Messages(adapter) => adapter.system_prompt_for_budget(),
            WireAdapter::Responses { parser, .. } => parser.system_prompt_for_budget(),
        }
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        Some(1_024)
    }

    fn sensitive_values(&self) -> Vec<String> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.sensitive_values(),
            WireAdapter::Messages(adapter) => adapter.sensitive_values(),
            WireAdapter::Responses {
                endpoint, api_key, ..
            } => {
                let mut values = endpoint_sensitive_values(endpoint);
                values.push(api_key.clone());
                values
            }
        }
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        let mut request = match &self.wire {
            WireAdapter::Chat(adapter) => adapter.build_request(prompt),
            WireAdapter::Messages(adapter) => adapter.build_request(prompt),
            WireAdapter::Responses {
                parser,
                endpoint,
                api_key,
                max_output_tokens,
            } => Self::responses_request(
                parser,
                endpoint,
                api_key,
                &[ProviderMessage::user(prompt)],
                &[],
                *max_output_tokens,
            ),
        };
        self.add_session_headers(&mut request.headers);
        request
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        let mut request = match &self.wire {
            WireAdapter::Chat(adapter) => adapter.build_messages_request(messages),
            WireAdapter::Messages(adapter) => adapter.build_messages_request(messages),
            WireAdapter::Responses {
                parser,
                endpoint,
                api_key,
                max_output_tokens,
            } => Self::responses_request(
                parser,
                endpoint,
                api_key,
                messages,
                &[],
                *max_output_tokens,
            ),
        };
        self.add_session_headers(&mut request.headers);
        request
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        let mut request = match &self.wire {
            WireAdapter::Chat(adapter) => {
                adapter.build_messages_request_with_tools(messages, tools)
            }
            WireAdapter::Messages(adapter) => {
                adapter.build_messages_request_with_tools(messages, tools)
            }
            WireAdapter::Responses {
                parser,
                endpoint,
                api_key,
                max_output_tokens,
            } => Self::responses_request(
                parser,
                endpoint,
                api_key,
                messages,
                tools,
                *max_output_tokens,
            ),
        };
        self.add_session_headers(&mut request.headers);
        request
    }

    fn build_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<HttpRequest, ProviderError> {
        self.validate_messages(messages)?;
        Ok(self.build_messages_request_with_tools(messages, tools))
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.validate_messages(messages)?;
        let mut prepared = match &self.wire {
            WireAdapter::Chat(adapter) => {
                adapter.prepare_messages_request_with_tools_checked(messages, tools)?
            }
            WireAdapter::Messages(adapter) => {
                adapter.prepare_messages_request_with_tools_checked(messages, tools)?
            }
            WireAdapter::Responses {
                parser,
                endpoint,
                api_key,
                max_output_tokens,
            } => {
                let (mut body, stable_prefixes) =
                    parser.request_body_with_prefixes(messages, tools);
                body["max_output_tokens"] = Value::from(*max_output_tokens);
                PreparedProviderRequest::from_http_body_with_prefixes(
                    endpoint.clone(),
                    vec![
                        ("Authorization".into(), format!("Bearer {api_key}")),
                        ("accept".into(), "text/event-stream".into()),
                        ("content-type".into(), "application/json".into()),
                    ],
                    body,
                    self,
                    stable_prefixes,
                )?
            }
        };
        self.add_session_headers(&mut prepared.headers);
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.validate_messages(messages)?;
        let mut prepared = match &self.wire {
            WireAdapter::Chat(adapter) => adapter.prepare_compaction_request_checked(messages)?,
            WireAdapter::Messages(adapter) => {
                adapter.prepare_compaction_request_checked(messages)?
            }
            WireAdapter::Responses {
                parser,
                endpoint,
                api_key,
                max_output_tokens,
            } => {
                let (mut body, _) = parser.request_body_with_prefixes(messages, &[]);
                body["max_output_tokens"] = Value::from(*max_output_tokens);
                harden_compaction_body(&mut body, false)?;
                let stable_prefixes = materialize_native_prompt_cache_key(parser, &mut body);
                PreparedProviderRequest::from_http_body_with_prefixes(
                    endpoint.clone(),
                    vec![
                        ("Authorization".into(), format!("Bearer {api_key}")),
                        ("accept".into(), "text/event-stream".into()),
                        ("content-type".into(), "application/json".into()),
                    ],
                    body,
                    self,
                    stable_prefixes,
                )?
            }
        };
        self.add_session_headers(&mut prepared.headers);
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.parse_event(value),
            WireAdapter::Messages(adapter) => adapter.parse_event(value),
            WireAdapter::Responses { parser, .. } => parser.parse_event(value),
        }
    }
}

pub(super) fn protocol_url(endpoint: &str, route: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(endpoint) {
        let mut path = url.path().trim_end_matches('/').to_owned();
        for known in ["/chat/completions", "/responses", "/messages"] {
            if path.ends_with(known) {
                path.truncate(path.len() - known.len());
                break;
            }
        }
        url.set_path(&format!("{}/{route}", path.trim_end_matches('/')));
        return url.to_string();
    }

    let split = endpoint.find(['?', '#']).unwrap_or(endpoint.len());
    let (base, suffix) = endpoint.split_at(split);
    let mut base = base.trim_end_matches('/').to_owned();
    for known in ["/chat/completions", "/responses", "/messages"] {
        if base.ends_with(known) {
            base.truncate(base.len() - known.len());
            break;
        }
    }
    format!("{}/{route}{suffix}", base.trim_end_matches('/'))
}
