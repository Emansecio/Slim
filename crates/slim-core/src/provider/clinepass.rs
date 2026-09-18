use serde::Deserialize;
use serde_json::Value;

use super::{
    OpenAiCompatibleAdapter, PreparedProviderRequest, ProviderAdapter, ProviderCapabilities,
    ProviderConfig, ProviderError, ProviderEvent, ProviderKind, ProviderMessage, ReasoningOff,
};

pub const CLINEPASS_BASE_URL: &str = "https://api.cline.bot/api/v1/chat/completions";
pub const CLINEPASS_MODELS_URL: &str = "https://api.cline.bot/api/v1/models";
pub const CLINEPASS_DEFAULT_MODEL: &str = "cline-pass/qwen3.7-max";

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClinePassModel {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub accepts_images: bool,
}

const fn model(
    id: &'static str,
    name: &'static str,
    context_window: u64,
    max_output_tokens: u32,
    accepts_images: bool,
) -> ClinePassModel {
    ClinePassModel {
        id,
        name,
        context_window,
        max_output_tokens,
        accepts_images,
    }
}

const MODELS: &[ClinePassModel] = &[
    model("cline-pass/glm-5.3", "GLM-5.3", 1_000_000, 131_072, true),
    model("cline-pass/glm-5.2", "GLM-5.2", 1_000_000, 131_072, true),
    model("cline-pass/kimi-k3", "Kimi K3", 1_000_000, 128_000, true),
    model(
        "cline-pass/kimi-k2.7-code",
        "Kimi K2.7 Code",
        1_000_000,
        128_000,
        false,
    ),
    model(
        "cline-pass/kimi-k2.6",
        "Kimi K2.6",
        1_000_000,
        128_000,
        false,
    ),
    model(
        "cline-pass/deepseek-v4-pro",
        "DeepSeek V4 Pro",
        1_000_000,
        384_000,
        false,
    ),
    model(
        "cline-pass/deepseek-v4-flash",
        "DeepSeek V4 Flash",
        1_000_000,
        384_000,
        false,
    ),
    model(
        "cline-pass/mimo-v2.5",
        "MiMo-V2.5",
        1_000_000,
        128_000,
        true,
    ),
    model(
        "cline-pass/mimo-v2.5-pro",
        "MiMo-V2.5-Pro",
        1_048_576,
        128_000,
        false,
    ),
    model(
        "cline-pass/minimax-m3",
        "MiniMax M3",
        1_000_000,
        131_072,
        true,
    ),
    model(
        "cline-pass/qwen3.8-max",
        "Qwen3.8 Max",
        1_000_000,
        131_072,
        true,
    ),
    model(
        "cline-pass/qwen3.7-max",
        "Qwen3.7 Max",
        1_000_000,
        65_536,
        false,
    ),
    model(
        "cline-pass/qwen3.7-plus",
        "Qwen3.7 Plus",
        1_000_000,
        65_536,
        true,
    ),
];

pub fn clinepass_models() -> &'static [ClinePassModel] {
    MODELS
}

pub fn clinepass_model(id: &str) -> Option<&'static ClinePassModel> {
    MODELS.iter().find(|model| model.id == id)
}

pub fn is_clinepass_model_id(id: &str) -> bool {
    id.starts_with("cline-pass/") && validate_model_id(id)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClinePassCatalogEntry {
    pub id: String,
    pub name: String,
    pub context_window: u64,
}

#[derive(Deserialize)]
struct CatalogDocument {
    #[serde(default)]
    object: Option<String>,
    data: Vec<CatalogEntry>,
}

#[derive(Deserialize)]
struct CatalogEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
}

pub fn parse_clinepass_catalog(bytes: &[u8]) -> Result<Vec<ClinePassCatalogEntry>, ProviderError> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(ProviderError::InvalidResponse {
            message: "ClinePass catalog exceeds its bound".into(),
        });
    }
    let document: CatalogDocument =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::InvalidResponse {
            message: "ClinePass catalog schema is invalid".into(),
        })?;
    if document
        .object
        .as_deref()
        .is_some_and(|object| object != "list")
    {
        return Err(ProviderError::InvalidResponse {
            message: "ClinePass catalog schema is invalid".into(),
        });
    }
    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in document.data {
        if !is_clinepass_model_id(&entry.id) {
            continue;
        }
        // The endpoint also lists the general Cline catalog; bound the
        // eligible ClinePass subset, while MAX_CATALOG_BYTES bounds the input.
        if models.len() >= MAX_CATALOG_ENTRIES {
            return Err(ProviderError::InvalidResponse {
                message: "ClinePass catalog exceeds its model bound".into(),
            });
        }
        if !seen.insert(entry.id.clone()) {
            return Err(ProviderError::InvalidResponse {
                message: "ClinePass catalog contains a duplicate model id".into(),
            });
        }
        let name = entry
            .name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                entry
                    .id
                    .strip_prefix("cline-pass/")
                    .unwrap_or(&entry.id)
                    .to_owned()
            });
        models.push(ClinePassCatalogEntry {
            id: entry.id,
            name,
            context_window: entry.context_length.unwrap_or(1_000_000).max(1),
        });
    }
    if models.is_empty() {
        return Err(ProviderError::InvalidResponse {
            message: "ClinePass catalog has no cline-pass models".into(),
        });
    }
    Ok(models)
}

pub async fn fetch_clinepass_catalog(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<Vec<ClinePassCatalogEntry>, ProviderError> {
    let mut response = client
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .map_err(|error| ProviderError::Transport {
            safe_to_retry: error.is_connect() || error.is_timeout(),
            message: super::redact_values(
                &format!("ClinePass catalog connection: {}", error.without_url()),
                &[api_key.to_owned()],
            ),
        })?;
    if !response.status().is_success() {
        return Err(ProviderError::Remote {
            message: format!("ClinePass catalog failed ({})", response.status().as_u16()),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(ProviderError::InvalidResponse {
            message: "ClinePass catalog exceeds its bound".into(),
        });
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_CATALOG_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ProviderError::Transport {
            safe_to_retry: true,
            message: super::redact_values(
                &format!("ClinePass catalog stream: {}", error.without_url()),
                &[api_key.to_owned()],
            ),
        })?
    {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_CATALOG_BYTES)
        {
            return Err(ProviderError::InvalidResponse {
                message: "ClinePass catalog exceeds its bound".into(),
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_clinepass_catalog(&bytes)
}

fn validate_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
}

pub struct ClinePassAdapter {
    inner: OpenAiCompatibleAdapter,
    model_id: String,
}

impl ClinePassAdapter {
    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        if !is_clinepass_model_id(model) {
            return Err(ProviderError::InvalidResponse {
                message: format!("unsupported ClinePass model: {model}"),
            });
        }
        let mut config = ProviderConfig::openai(endpoint, model, api_key);
        if let Some(effort) = reasoning_effort.filter(|e| !e.is_empty()) {
            config = config.with_reasoning_effort(effort);
        }
        let inner = OpenAiCompatibleAdapter::new(config)?;
        Ok(Self {
            inner,
            model_id: model.into(),
        })
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.inner.set_system_prompt(prompt);
        self
    }

    pub fn with_response_cache_scope_id(mut self, id: u64) -> Self {
        self.inner.set_response_cache_scope_id(id);
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.inner.config.max_output_tokens = tokens.max(1);
        self
    }
}

impl ProviderAdapter for ClinePassAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClinePass
    }

    fn reasoning_off(&self) -> Option<ReasoningOff> {
        self.inner.reasoning_off()
    }

    fn set_reasoning_disabled(&mut self) -> Result<(), ProviderError> {
        self.inner.set_reasoning_disabled()
    }

    fn wire_kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCompatible
    }

    fn model(&self) -> &str {
        &self.model_id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        self.inner.materialize_prompt_cache_intent(body);
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        self.inner.response_cache_scope_id()
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        self.inner.system_prompt_for_budget()
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        self.inner.request_envelope_upper_bound_chars()
    }

    fn sensitive_values(&self) -> Vec<String> {
        self.inner.sensitive_values()
    }

    fn build_request(&self, prompt: &str) -> super::HttpRequest {
        self.inner.build_request(prompt)
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> super::HttpRequest {
        self.inner.build_messages_request(messages)
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> super::HttpRequest {
        self.inner
            .build_messages_request_with_tools(messages, tools)
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        Ok(self
            .inner
            .prepare_messages_request_with_tools_checked(messages, tools)?
            .with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        Ok(self
            .inner
            .prepare_compaction_request_checked(messages)?
            .with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.inner.parse_event(value)
    }
}
