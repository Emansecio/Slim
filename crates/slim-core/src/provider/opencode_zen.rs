use serde_json::Value;

use super::{
    endpoint_sensitive_values, harden_compaction_body, materialize_native_prompt_cache_key,
    normalize_messages,
    opencode_go::{model, protocol_url, OpenCodeApi, OpenCodeModel},
    HttpRequest, OpenAiCodexAdapter, OpenAiCompatibleAdapter, PreparedProviderRequest,
    ProviderAdapter, ProviderCapabilities, ProviderConfig, ProviderError, ProviderEvent,
    ProviderKind, ProviderMessage, ReasoningOff,
};

pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const OPENCODE_ZEN_MODELS_URL: &str = "https://opencode.ai/zen/v1/models";
pub const OPENCODE_ZEN_DEFAULT_MODEL: &str = "big-pickle";
/// Bearer accepted by the Zen free tier when no account key is configured.
/// The gateway still requires `x-opencode-session` on every request.
pub const OPENCODE_ZEN_PUBLIC_KEY: &str = "public";

const REASONING_NONE: &[&str] = &[];
const REASONING_LOW_HIGH_MAX: &[&str] = &["low", "high", "max"];
const REASONING_LOW_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];

// Zero-cost Zen models live on https://opencode.ai/zen/v1 (verified
// 2026-09-11). Endpoint per model follows https://opencode.ai/docs/zen/;
// limits and reasoning options follow the models.dev `opencode` catalog.
const ZEN_MODELS: &[OpenCodeModel] = &[
    // Big Pickle caps input at 160k of its 200k window (models.dev limit).
    model(
        "big-pickle",
        "Big Pickle",
        OpenCodeApi::ChatCompletions,
        160000,
        32000,
        false,
        REASONING_NONE,
    ),
    model(
        "deepseek-v4-flash-free",
        "DeepSeek V4 Flash Free",
        OpenCodeApi::ChatCompletions,
        200000,
        128000,
        false,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "ling-3.0-flash-fin-free",
        "Ling 3.0 Flash Fin Free",
        OpenCodeApi::ChatCompletions,
        262144,
        32768,
        false,
        REASONING_NONE,
    ),
    model(
        "mimo-v2.5-free",
        "MiMo-V2.5 Free",
        OpenCodeApi::ChatCompletions,
        200000,
        32000,
        true,
        REASONING_NONE,
    ),
    // Zen routes the Muse contributor free tier through /responses like Go.
    model(
        "muse-spark-1.2-contributor-free",
        "Muse Spark 1.2 Free",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "muse-spark-1.3-contributor-free",
        "Muse Spark 1.3 Free",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "nemotron-3-ultra-free",
        "Nemotron 3 Ultra Free",
        OpenCodeApi::ChatCompletions,
        1000000,
        128000,
        false,
        REASONING_NONE,
    ),
    model(
        "nemotron-3.5-lightning-free",
        "Nemotron 3.5 Lightning Free",
        OpenCodeApi::ChatCompletions,
        262144,
        262144,
        false,
        REASONING_NONE,
    ),
];

pub fn zen_models() -> &'static [OpenCodeModel] {
    ZEN_MODELS
}

pub fn zen_model(id: &str) -> Option<&'static OpenCodeModel> {
    ZEN_MODELS.iter().find(|model| model.id == id)
}

enum WireAdapter {
    Chat(OpenAiCompatibleAdapter),
    Responses {
        parser: OpenAiCodexAdapter,
        endpoint: String,
        api_key: String,
        max_output_tokens: u32,
    },
}

pub struct OpenCodeZenAdapter {
    model: &'static OpenCodeModel,
    wire: WireAdapter,
    session_id: String,
}

impl OpenCodeZenAdapter {
    pub fn validate_messages(&self, messages: &[ProviderMessage]) -> Result<(), ProviderError> {
        normalize_messages(messages)?;
        if !self.model.accepts_images
            && messages
                .iter()
                .flat_map(|message| &message.content_blocks)
                .any(|block| matches!(block, super::ProviderContentBlock::Image { .. }))
        {
            return Err(ProviderError::InvalidResponse {
                message: format!(
                    "OpenCode Zen model {} does not accept images",
                    self.model.id
                ),
            });
        }
        Ok(())
    }

    pub fn new(
        endpoint: &str,
        model_id: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let model = zen_model(model_id).ok_or_else(|| ProviderError::InvalidResponse {
            message: format!("unsupported OpenCode Zen model: {model_id}"),
        })?;
        // Models without reasoning levels have no effort knob: a globally
        // configured effort (e.g. slim.toml written for another provider) is
        // dropped instead of poisoning the request.
        let reasoning_effort = match reasoning_effort {
            Some(effort) if model.reasoning_levels.contains(&effort) => Some(effort),
            Some(_) if model.reasoning_levels.is_empty() => None,
            Some(_) => {
                return Err(ProviderError::InvalidResponse {
                    message: "unsupported OpenCode Zen reasoning effort".into(),
                })
            }
            None => None,
        };
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
            OpenCodeApi::Responses => {
                let endpoint = protocol_url(endpoint, "responses");
                let mut config =
                    ProviderConfig::openai_codex(&endpoint, model.id, api_key, "opencode-zen");
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
            OpenCodeApi::AnthropicMessages => {
                return Err(ProviderError::InvalidResponse {
                    message: format!(
                        "OpenCode Zen model {} uses the messages API, which is not wired",
                        model.id
                    ),
                })
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
            WireAdapter::Responses { parser, .. } => parser.set_response_cache_scope_id(id),
        }
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        match &mut self.wire {
            WireAdapter::Chat(adapter) => {
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

impl ProviderAdapter for OpenCodeZenAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenCodeZen
    }

    fn reasoning_off(&self) -> Option<ReasoningOff> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.reasoning_off(),
            WireAdapter::Responses { .. } => None,
        }
    }

    fn set_reasoning_disabled(&mut self) -> Result<(), ProviderError> {
        match &mut self.wire {
            WireAdapter::Chat(adapter) => adapter.set_reasoning_disabled(),
            WireAdapter::Responses { .. } => Err(ProviderError::InvalidResponse {
                message: "Jev requires documented native reasoning OFF; the Responses route has none. No fallback was applied.".into(),
            }),
        }
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
            WireAdapter::Responses { parser, .. } => parser.capabilities(),
        }
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.materialize_prompt_cache_intent(body),
            WireAdapter::Responses { parser, .. } => {
                parser.materialize_prompt_cache_intent(body);
            }
        }
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.response_cache_scope_id(),
            WireAdapter::Responses { parser, .. } => parser.response_cache_scope_id(),
        }
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.system_prompt_for_budget(),
            WireAdapter::Responses { parser, .. } => parser.system_prompt_for_budget(),
        }
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        Some(1_024)
    }

    fn sensitive_values(&self) -> Vec<String> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.sensitive_values(),
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
            WireAdapter::Responses { parser, .. } => parser.parse_event(value),
        }
    }
}
