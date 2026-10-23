//! The OpenCode gateway adapter shared by the Go and Zen plans. The two differ
//! only in catalog, labels, account id and how strictly a reasoning effort or
//! the Anthropic `messages` API is handled; see [`OpenCodeFlavor`].

use std::marker::PhantomData;

use serde_json::Value;

use super::{
    endpoint::protocol_url,
    opencode_go::{OpenCodeApi, OpenCodeModel},
    wire::{ResponsesWire, WireAdapter},
    AnthropicAdapter, HttpRequest, OpenAiCompatibleAdapter, PreparedProviderRequest,
    ProviderAdapter, ProviderCapabilities, ProviderConfig, ProviderContentBlock, ProviderError,
    ProviderEvent, ProviderKind, ProviderMessage,
};

pub trait OpenCodeFlavor: 'static {
    const KIND: ProviderKind;
    /// Human label used in error messages, e.g. `OpenCode Go`.
    const LABEL: &'static str;
    /// Account id sent with the Responses wire, e.g. `opencode-go`.
    const ACCOUNT: &'static str;
    /// Whether models on the Anthropic `messages` API are wired.
    const SUPPORTS_MESSAGES_API: bool;
    /// When true, a reasoning effort on a model without reasoning levels is
    /// dropped (a globally configured effort may come from another provider);
    /// when false, it is rejected like any other unsupported effort.
    const DROPS_EFFORT_WITHOUT_LEVELS: bool;

    fn lookup(model_id: &str) -> Option<&'static OpenCodeModel>;
}

pub struct OpenCodeAdapter<F: OpenCodeFlavor> {
    model: &'static OpenCodeModel,
    wire: WireAdapter,
    session_id: String,
    flavor: PhantomData<fn() -> F>,
}

impl<F: OpenCodeFlavor> OpenCodeAdapter<F> {
    pub fn validate_messages(&self, messages: &[ProviderMessage]) -> Result<(), ProviderError> {
        super::normalize_messages(messages)?;
        if !self.model.accepts_images
            && messages
                .iter()
                .flat_map(|message| &message.content_blocks)
                .any(|block| matches!(block, ProviderContentBlock::Image { .. }))
        {
            return Err(ProviderError::InvalidResponse {
                message: format!(
                    "{} model {} does not accept images",
                    F::LABEL,
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
        let model = F::lookup(model_id).ok_or_else(|| ProviderError::InvalidResponse {
            message: format!("unsupported {} model: {model_id}", F::LABEL),
        })?;
        let reasoning_effort = match reasoning_effort {
            Some(effort) if model.reasoning_levels.contains(&effort) => Some(effort),
            Some(_) if F::DROPS_EFFORT_WITHOUT_LEVELS && model.reasoning_levels.is_empty() => None,
            Some(_) => {
                return Err(ProviderError::InvalidResponse {
                    message: format!("unsupported {} reasoning effort", F::LABEL),
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
            OpenCodeApi::AnthropicMessages if F::SUPPORTS_MESSAGES_API => {
                WireAdapter::Messages(AnthropicAdapter::new(ProviderConfig::anthropic_bearer(
                    protocol_url(endpoint, "messages"),
                    model.id,
                    api_key,
                ))?)
            }
            OpenCodeApi::AnthropicMessages => {
                return Err(ProviderError::InvalidResponse {
                    message: format!(
                        "{} model {} uses the messages API, which is not wired",
                        F::LABEL,
                        model.id
                    ),
                })
            }
            OpenCodeApi::Responses => WireAdapter::Responses(ResponsesWire::new(
                protocol_url(endpoint, "responses"),
                model.id,
                api_key,
                F::ACCOUNT,
                reasoning_effort,
            )?),
        };
        Ok(Self {
            model,
            wire,
            session_id: Self::new_session_id(),
            flavor: PhantomData,
        })
    }

    pub fn new_session_id() -> String {
        new_conversation_id()
    }

    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = opaque_session_id(session_id);
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
        self.wire.set_response_cache_scope_id(id);
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.wire.set_max_output_tokens(max_output_tokens);
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.wire.set_system_prompt(prompt.into());
        self
    }

    fn with_session_headers(&self, mut request: HttpRequest) -> HttpRequest {
        self.add_session_headers(&mut request.headers);
        request
    }

    fn finish(&self, mut prepared: PreparedProviderRequest) -> PreparedProviderRequest {
        self.add_session_headers(&mut prepared.headers);
        prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model())
    }
}

impl<F: OpenCodeFlavor> ProviderAdapter for OpenCodeAdapter<F> {
    fn kind(&self) -> ProviderKind {
        F::KIND
    }

    fn wire_kind(&self) -> ProviderKind {
        self.wire.wire_kind()
    }

    fn model(&self) -> &str {
        self.model.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.wire.capabilities()
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        self.wire.materialize_prompt_cache_intent(body);
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        self.wire.response_cache_scope_id()
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.wire.system_prompt_for_budget()
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        Some(1_024)
    }

    fn sensitive_values(&self) -> Vec<String> {
        self.wire.sensitive_values()
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.with_session_headers(self.wire.build_request(prompt))
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        self.with_session_headers(self.wire.build_messages_request(messages))
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.with_session_headers(self.wire.build_messages_request_with_tools(messages, tools))
    }

    fn build_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<HttpRequest, ProviderError> {
        self.validate_messages(messages)?;
        if self.wire_kind() == ProviderKind::Anthropic {
            super::image_limits::validate_anthropic_images(messages)?;
        }
        Ok(self.build_messages_request_with_tools(messages, tools))
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.validate_messages(messages)?;
        let prepared = self
            .wire
            .prepare_messages_request_with_tools_checked(self, messages, tools)?;
        Ok(self.finish(prepared))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        self.validate_messages(messages)?;
        let prepared = self
            .wire
            .prepare_compaction_request_checked(self, messages)?;
        Ok(self.finish(prepared))
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.wire.parse_event(value)
    }
}

/// A fresh conversation identity for one adapter instance.
pub(super) fn new_conversation_id() -> String {
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

/// Opaque, header-safe identity of a session; never exposes a local session
/// path.
pub(super) fn opaque_session_id(session_id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("slim-{:x}", Sha256::digest(session_id.as_bytes()))
}
