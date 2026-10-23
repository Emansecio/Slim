use serde_json::Value;

use super::{
    endpoint::protocol_url, normalize_messages, wire::ResponsesWire, HttpRequest,
    PreparedProviderRequest, ProviderAdapter, ProviderCapabilities, ProviderError, ProviderEvent,
    ProviderKind, ProviderMessage,
};

pub const XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const XAI_DEFAULT_MODEL: &str = "grok-4.5";

const REASONING_STANDARD: &[&str] = &["low", "medium", "high"];
const REASONING_GROK_4_3: &[&str] = &["none", "low", "medium", "high", "xhigh"];
const REASONING_STANDARD_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XaiModel {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub accepts_images: bool,
    pub reasoning_levels: &'static [&'static str],
}

const fn model(
    id: &'static str,
    name: &'static str,
    context_window: u64,
    max_output_tokens: u32,
    reasoning_levels: &'static [&'static str],
) -> XaiModel {
    XaiModel {
        id,
        name,
        context_window,
        max_output_tokens,
        accepts_images: true,
        reasoning_levels,
    }
}

/// Bundle mirrors the pi xAI catalog (responses API, Bearer):
/// grok-4.3 (1M/30k), grok-4.5 + grok-4.6 (500k/500k), grok-build-0.1 (256k/256k).
const MODELS: &[XaiModel] = &[
    model(
        "grok-4.3",
        "Grok 4.3",
        1_000_000,
        30_000,
        REASONING_GROK_4_3,
    ),
    model("grok-4.5", "Grok 4.5", 500_000, 500_000, REASONING_STANDARD),
    model(
        "grok-4.6",
        "Grok 4.6",
        500_000,
        500_000,
        REASONING_STANDARD_XHIGH,
    ),
    model(
        "grok-build-0.1",
        "Grok Build 0.1",
        256_000,
        256_000,
        REASONING_STANDARD,
    ),
];

pub fn xai_models() -> &'static [XaiModel] {
    MODELS
}

pub fn xai_model(id: &str) -> Option<&'static XaiModel> {
    MODELS.iter().find(|model| model.id == id)
}

pub fn is_xai_model_id(id: &str) -> bool {
    xai_model(id).is_some()
}

/// Direct xAI adapter: responses wire (same shape as the OpenCode Go
/// responses arm), Bearer access token from OAuth device flow or `XAI_API_KEY`.
pub struct XaiAdapter {
    model: &'static XaiModel,
    wire: ResponsesWire,
}

impl XaiAdapter {
    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let model = xai_model(model).ok_or_else(|| ProviderError::InvalidResponse {
            message: format!("unsupported xAI model: {model}"),
        })?;
        if reasoning_effort.is_some_and(|effort| !model.reasoning_levels.contains(&effort)) {
            return Err(ProviderError::InvalidResponse {
                message: "unsupported xAI reasoning effort".into(),
            });
        }
        let mut wire = ResponsesWire::new(
            protocol_url(endpoint, "responses"),
            model.id,
            api_key,
            "xai",
            reasoning_effort,
        )?;
        // xAI keeps a prompt cache per server and routes by this key: one
        // conversation's requests share a server.
        wire.set_conversation_key(super::opencode::new_conversation_id());
        Ok(Self { model, wire })
    }

    /// Ties the cache routing to `session_id`, so a resumed session finds
    /// the server that cached it.
    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.wire
            .set_conversation_key(super::opencode::opaque_session_id(session_id));
        self
    }

    pub fn with_response_cache_scope_id(mut self, id: u64) -> Self {
        self.wire.parser.set_response_cache_scope_id(id);
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.wire.set_max_output_tokens(max_output_tokens);
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.wire.parser.set_system_prompt(prompt);
        self
    }
}

impl ProviderAdapter for XaiAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Xai
    }

    fn wire_kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCodex
    }

    fn model(&self) -> &str {
        self.model.id
    }

    fn reasoning_classification(&self) -> Option<crate::ReasoningClassification> {
        Some(crate::ReasoningClassification::Text)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.wire.parser.capabilities()
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        self.wire.parser.materialize_prompt_cache_intent(body);
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        self.wire.parser.response_cache_scope_id()
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.wire.parser.system_prompt_for_budget()
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        Some(1_024)
    }

    fn sensitive_values(&self) -> Vec<String> {
        self.wire.sensitive_values()
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.wire.request(&[ProviderMessage::user(prompt)], &[])
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        self.wire.request(messages, &[])
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.wire.request(messages, tools)
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        let prepared = self.wire.prepare(self, messages, tools)?;
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        let prepared = self.wire.prepare_compaction(self, messages)?;
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        if value.get("type").and_then(Value::as_str) == Some("response.reasoning_text.delta") {
            return Ok(value
                .get("delta")
                .and_then(Value::as_str)
                .map(|text| vec![ProviderEvent::ReasoningDelta(text.into())])
                .unwrap_or_default());
        }
        self.wire.parser.parse_event(value)
    }
}
