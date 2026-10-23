//! Wire-format dispatch shared by the adapters that front a hosted gateway
//! (OpenCode Go/Zen, xAI, Command Code): one enum over the three protocols
//! instead of a hand-written `match` per adapter and per trait method.

use serde_json::Value;

use super::{
    endpoint_sensitive_values, harden_compaction_body, materialize_native_prompt_cache_key,
    AnthropicAdapter, HttpRequest, OpenAiCodexAdapter, OpenAiCompatibleAdapter,
    PreparedProviderRequest, ProviderAdapter, ProviderCapabilities, ProviderConfig, ProviderError,
    ProviderEvent, ProviderKind, ProviderMessage,
};

/// OpenAI Responses over a plain Bearer key, parsed by the Codex adapter.
pub(super) struct ResponsesWire {
    pub(super) parser: OpenAiCodexAdapter,
    endpoint: String,
    api_key: String,
    max_output_tokens: u32,
    extra_headers: Vec<(String, String)>,
    /// Narrows the prompt cache key to one conversation, where the provider
    /// routes its cache by conversation.
    conversation_key: Option<String>,
}

impl ResponsesWire {
    pub(super) fn new(
        endpoint: String,
        model: &str,
        api_key: &str,
        account_id: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let mut config = ProviderConfig::openai_codex(&endpoint, model, api_key, account_id);
        if let Some(effort) = reasoning_effort {
            config = config.with_reasoning_effort(effort);
        }
        Ok(Self {
            parser: OpenAiCodexAdapter::new(config)?,
            endpoint,
            api_key: api_key.into(),
            max_output_tokens: super::DEFAULT_MAX_OUTPUT_TOKENS,
            extra_headers: Vec::new(),
            conversation_key: None,
        })
    }

    pub(super) fn set_conversation_key(&mut self, key: String) {
        self.conversation_key = Some(key);
    }

    fn scope_prompt_cache_key(&self, body: &mut Value) {
        let Some(conversation) = &self.conversation_key else {
            return;
        };
        if let Some(key) = body.get("prompt_cache_key").and_then(Value::as_str) {
            body["prompt_cache_key"] = Value::String(format!("{key}:{conversation}"));
        }
    }

    pub(super) fn set_max_output_tokens(&mut self, max_output_tokens: u32) {
        self.max_output_tokens = max_output_tokens.max(1);
    }

    pub(super) fn sensitive_values(&self) -> Vec<String> {
        let mut values = endpoint_sensitive_values(&self.endpoint);
        values.push(self.api_key.clone());
        values
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Authorization".into(), format!("Bearer {}", self.api_key)),
            ("accept".into(), "text/event-stream".into()),
            ("content-type".into(), "application/json".into()),
        ];
        headers.extend(self.extra_headers.iter().cloned());
        headers
    }

    pub(super) fn request(&self, messages: &[ProviderMessage], tools: &[Value]) -> HttpRequest {
        let mut body = self.parser.request_body(messages, tools);
        body["max_output_tokens"] = Value::from(self.max_output_tokens);
        self.scope_prompt_cache_key(&mut body);
        HttpRequest {
            url: self.endpoint.clone(),
            headers: self.headers(),
            body: body.to_string(),
        }
    }

    pub(super) fn prepare(
        &self,
        owner: &dyn ProviderAdapter,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let (mut body, stable_prefixes) = self.parser.request_body_with_prefixes(messages, tools);
        body["max_output_tokens"] = Value::from(self.max_output_tokens);
        self.scope_prompt_cache_key(&mut body);
        PreparedProviderRequest::from_http_body_with_prefixes(
            self.endpoint.clone(),
            self.headers(),
            body,
            owner,
            stable_prefixes,
        )
    }

    pub(super) fn prepare_compaction(
        &self,
        owner: &dyn ProviderAdapter,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let (mut body, _) = self.parser.request_body_with_prefixes(messages, &[]);
        body["max_output_tokens"] = Value::from(self.max_output_tokens);
        harden_compaction_body(&mut body, false)?;
        let stable_prefixes = materialize_native_prompt_cache_key(&self.parser, &mut body);
        self.scope_prompt_cache_key(&mut body);
        PreparedProviderRequest::from_http_body_with_prefixes(
            self.endpoint.clone(),
            self.headers(),
            body,
            owner,
            stable_prefixes,
        )
    }
}

pub(super) enum WireAdapter {
    Chat(OpenAiCompatibleAdapter),
    Messages(AnthropicAdapter),
    Responses(ResponsesWire),
}

impl WireAdapter {
    pub(super) fn wire_kind(&self) -> ProviderKind {
        match self {
            Self::Chat(_) => ProviderKind::OpenAiCompatible,
            Self::Messages(_) => ProviderKind::Anthropic,
            Self::Responses(_) => ProviderKind::OpenAiCodex,
        }
    }

    pub(super) fn set_response_cache_scope_id(&mut self, id: u64) {
        match self {
            Self::Chat(adapter) => adapter.set_response_cache_scope_id(id),
            Self::Messages(adapter) => adapter.set_response_cache_scope_id(id),
            Self::Responses(wire) => wire.parser.set_response_cache_scope_id(id),
        }
    }

    pub(super) fn set_max_output_tokens(&mut self, max_output_tokens: u32) {
        match self {
            Self::Chat(adapter) => adapter.config.max_output_tokens = max_output_tokens.max(1),
            Self::Messages(adapter) => adapter.config.max_output_tokens = max_output_tokens.max(1),
            Self::Responses(wire) => wire.set_max_output_tokens(max_output_tokens),
        }
    }

    pub(super) fn set_system_prompt(&mut self, prompt: String) {
        match self {
            Self::Chat(adapter) => adapter.set_system_prompt(prompt),
            Self::Messages(adapter) => adapter.set_system_prompt(prompt),
            Self::Responses(wire) => wire.parser.set_system_prompt(prompt),
        }
    }

    pub(super) fn push_extra_header(&mut self, name: &str, value: &str) {
        match self {
            Self::Chat(adapter) => adapter.config.push_extra_header(name, value),
            Self::Messages(adapter) => adapter.config.push_extra_header(name, value),
            Self::Responses(wire) => wire.extra_headers.push((name.into(), value.into())),
        }
    }

    pub(super) fn capabilities(&self) -> ProviderCapabilities {
        match self {
            Self::Chat(adapter) => adapter.capabilities(),
            Self::Messages(adapter) => adapter.capabilities(),
            Self::Responses(wire) => wire.parser.capabilities(),
        }
    }

    pub(super) fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        match self {
            Self::Chat(adapter) => adapter.materialize_prompt_cache_intent(body),
            Self::Messages(adapter) => adapter.materialize_prompt_cache_intent(body),
            Self::Responses(wire) => wire.parser.materialize_prompt_cache_intent(body),
        }
    }

    pub(super) fn response_cache_scope_id(&self) -> Option<u64> {
        match self {
            Self::Chat(adapter) => adapter.response_cache_scope_id(),
            Self::Messages(adapter) => adapter.response_cache_scope_id(),
            Self::Responses(wire) => wire.parser.response_cache_scope_id(),
        }
    }

    pub(super) fn system_prompt_for_budget(&self) -> Option<&str> {
        match self {
            Self::Chat(adapter) => adapter.system_prompt_for_budget(),
            Self::Messages(adapter) => adapter.system_prompt_for_budget(),
            Self::Responses(wire) => wire.parser.system_prompt_for_budget(),
        }
    }

    pub(super) fn sensitive_values(&self) -> Vec<String> {
        match self {
            Self::Chat(adapter) => adapter.sensitive_values(),
            Self::Messages(adapter) => adapter.sensitive_values(),
            Self::Responses(wire) => wire.sensitive_values(),
        }
    }

    pub(super) fn build_request(&self, prompt: &str) -> HttpRequest {
        match self {
            Self::Chat(adapter) => adapter.build_request(prompt),
            Self::Messages(adapter) => adapter.build_request(prompt),
            Self::Responses(wire) => wire.request(&[ProviderMessage::user(prompt)], &[]),
        }
    }

    pub(super) fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        match self {
            Self::Chat(adapter) => adapter.build_messages_request(messages),
            Self::Messages(adapter) => adapter.build_messages_request(messages),
            Self::Responses(wire) => wire.request(messages, &[]),
        }
    }

    pub(super) fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        match self {
            Self::Chat(adapter) => adapter.build_messages_request_with_tools(messages, tools),
            Self::Messages(adapter) => adapter.build_messages_request_with_tools(messages, tools),
            Self::Responses(wire) => wire.request(messages, tools),
        }
    }

    /// `owner` is the adapter that fronts this wire; the Responses arm uses it
    /// for the request identity (sensitive values, cache keys).
    pub(super) fn prepare_messages_request_with_tools_checked(
        &self,
        owner: &dyn ProviderAdapter,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        match self {
            Self::Chat(adapter) => {
                adapter.prepare_messages_request_with_tools_checked(messages, tools)
            }
            Self::Messages(adapter) => {
                adapter.prepare_messages_request_with_tools_checked(messages, tools)
            }
            Self::Responses(wire) => wire.prepare(owner, messages, tools),
        }
    }

    pub(super) fn prepare_compaction_request_checked(
        &self,
        owner: &dyn ProviderAdapter,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        match self {
            Self::Chat(adapter) => adapter.prepare_compaction_request_checked(messages),
            Self::Messages(adapter) => adapter.prepare_compaction_request_checked(messages),
            Self::Responses(wire) => wire.prepare_compaction(owner, messages),
        }
    }

    pub(super) fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        match self {
            Self::Chat(adapter) => adapter.parse_event(value),
            Self::Messages(adapter) => adapter.parse_event(value),
            Self::Responses(wire) => wire.parser.parse_event(value),
        }
    }
}
