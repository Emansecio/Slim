use serde_json::Value;

use super::{
    endpoint_sensitive_values, harden_compaction_body, materialize_native_prompt_cache_key,
    normalize_messages, OpenAiCodexAdapter, PreparedProviderRequest, ProviderAdapter,
    ProviderCapabilities, ProviderConfig, ProviderError, ProviderEvent, ProviderKind,
    ProviderMessage,
};

pub const XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const XAI_DEFAULT_MODEL: &str = "grok-4.5";

const REASONING_STANDARD: &[&str] = &["low", "medium", "high"];
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
        REASONING_STANDARD,
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
    parser: OpenAiCodexAdapter,
    endpoint: String,
    api_key: String,
    max_output_tokens: u32,
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
        let endpoint = protocol_url(endpoint, "responses");
        let mut config = ProviderConfig::openai_codex(&endpoint, model.id, api_key, "xai");
        if let Some(effort) = reasoning_effort {
            config = config.with_reasoning_effort(effort);
        }
        Ok(Self {
            model,
            parser: OpenAiCodexAdapter::new(config)?,
            endpoint,
            api_key: api_key.into(),
            max_output_tokens: super::DEFAULT_MAX_OUTPUT_TOKENS,
        })
    }

    pub fn with_response_cache_scope_id(mut self, id: u64) -> Self {
        self.parser.set_response_cache_scope_id(id);
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = max_output_tokens.max(1);
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.parser.set_system_prompt(prompt);
        self
    }

    fn responses_request(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> super::HttpRequest {
        let mut body = self.parser.request_body(messages, tools);
        body["max_output_tokens"] = Value::from(self.max_output_tokens);
        super::HttpRequest {
            url: self.endpoint.clone(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", self.api_key)),
                ("accept".into(), "text/event-stream".into()),
                ("content-type".into(), "application/json".into()),
            ],
            body: body.to_string(),
        }
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

    fn capabilities(&self) -> ProviderCapabilities {
        self.parser.capabilities()
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        self.parser.materialize_prompt_cache_intent(body);
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        self.parser.response_cache_scope_id()
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.parser.system_prompt_for_budget()
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        Some(1_024)
    }

    fn sensitive_values(&self) -> Vec<String> {
        let mut values = endpoint_sensitive_values(&self.endpoint);
        values.push(self.api_key.clone());
        values
    }

    fn build_request(&self, prompt: &str) -> super::HttpRequest {
        self.responses_request(&[ProviderMessage::user(prompt)], &[])
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> super::HttpRequest {
        self.responses_request(messages, &[])
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> super::HttpRequest {
        self.responses_request(messages, tools)
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        let (mut body, stable_prefixes) = self.parser.request_body_with_prefixes(messages, tools);
        body["max_output_tokens"] = Value::from(self.max_output_tokens);
        let prepared = PreparedProviderRequest::from_http_body_with_prefixes(
            self.endpoint.clone(),
            vec![
                ("Authorization".into(), format!("Bearer {}", self.api_key)),
                ("accept".into(), "text/event-stream".into()),
                ("content-type".into(), "application/json".into()),
            ],
            body,
            self,
            stable_prefixes,
        )?;
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        normalize_messages(messages)?;
        let (mut body, _) = self.parser.request_body_with_prefixes(messages, &[]);
        body["max_output_tokens"] = Value::from(self.max_output_tokens);
        harden_compaction_body(&mut body, false)?;
        let stable_prefixes = materialize_native_prompt_cache_key(&self.parser, &mut body);
        let prepared = PreparedProviderRequest::from_http_body_with_prefixes(
            self.endpoint.clone(),
            vec![
                ("Authorization".into(), format!("Bearer {}", self.api_key)),
                ("accept".into(), "text/event-stream".into()),
                ("content-type".into(), "application/json".into()),
            ],
            body,
            self,
            stable_prefixes,
        )?;
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
        self.parser.parse_event(value)
    }
}

fn protocol_url(endpoint: &str, route: &str) -> String {
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
