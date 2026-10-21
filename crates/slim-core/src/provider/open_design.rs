use serde::Deserialize;
use serde_json::Value;

use super::{
    OpenAiCompatibleAdapter, PreparedProviderRequest, ProviderAdapter, ProviderCapabilities,
    ProviderConfig, ProviderError, ProviderEvent, ProviderKind, ProviderMessage,
};

/// Open Design Cloud's model gateway, in its OpenAI Chat Completions format.
pub const OPEN_DESIGN_BASE_URL: &str = "https://amr-link.open-design.ai/v1/chat/completions";
pub const OPEN_DESIGN_MODELS_URL: &str = "https://amr-link.open-design.ai/v1/models";
pub const OPEN_DESIGN_DEFAULT_MODEL: &str = "deepseek-v4.1-flash";

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenDesignModel {
    pub id: &'static str,
    pub name: &'static str,
    /// The gateway's `context_budget`, which is below the model's own limit.
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
) -> OpenDesignModel {
    OpenDesignModel {
        id,
        name,
        context_window,
        max_output_tokens,
        accepts_images,
    }
}

/// The catalog as `GET /v1/models` listed it on 19/10/2026, except where a
/// context window is set here on purpose: these windows win over the live
/// catalog's budget, which fills in only models this table does not know.
/// DeepSeek V4.1 Flash runs at 1M (its model limit is 1,048,000) instead of
/// the gateway's 328,000 budget.
const MODELS: &[OpenDesignModel] = &[
    model(
        "deepseek-v4.1-flash",
        "DeepSeek V4.1 Flash",
        1_000_000,
        128_000,
        true,
    ),
    model(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        328_000,
        128_000,
        false,
    ),
    model(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        232_000,
        32_000,
        false,
    ),
    model(
        "deepseek-v4-flash-vision-exp",
        "DeepSeek V4 Flash Vision (exp)",
        232_000,
        32_000,
        true,
    ),
    model(
        "claude-haiku-5.5",
        "Claude Haiku 5.5",
        228_000,
        128_000,
        true,
    ),
    model("gpt-6-luna", "GPT-6 Luna", 400_000, 128_000, true),
    model("glm-5.3-flash", "GLM-5.3 Flash", 232_000, 32_000, true),
    model("glm-5.3-flashx", "GLM-5.3 FlashX", 232_000, 32_000, true),
    model("kimi-k2.7-code", "Kimi K2.7 Code", 262_000, 32_000, true),
    model("mimo-v2.6-flash", "MiMo-V2.6 Flash", 232_000, 32_000, true),
    model("mimo-v2.6-pro", "MiMo-V2.6 Pro", 232_000, 32_000, true),
    model("hy4-preview", "HY4 Preview", 232_000, 32_000, false),
];

pub fn open_design_models() -> &'static [OpenDesignModel] {
    MODELS
}

pub fn open_design_model(id: &str) -> Option<&'static OpenDesignModel> {
    MODELS.iter().find(|model| model.id == id)
}

/// The gateway lists bare ids; any well-formed one may be routed.
pub fn is_open_design_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenDesignCatalogEntry {
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub accepts_images: bool,
    /// The gateway's own default model.
    pub default: bool,
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
    architecture: Option<CatalogArchitecture>,
    #[serde(default)]
    metadata: Option<CatalogMetadata>,
    #[serde(default)]
    default: bool,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Default, Deserialize)]
struct CatalogArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Default, Deserialize)]
struct CatalogMetadata {
    #[serde(default)]
    context_limit: Option<u64>,
    #[serde(default)]
    context_budget: Option<u64>,
    #[serde(default)]
    output_budget: Option<u64>,
}

pub fn parse_open_design_catalog(
    bytes: &[u8],
) -> Result<Vec<OpenDesignCatalogEntry>, ProviderError> {
    let invalid = |message: &str| ProviderError::InvalidResponse {
        message: format!("Open Design catalog {message}"),
    };
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(invalid("exceeds its bound"));
    }
    let document: CatalogDocument =
        serde_json::from_slice(bytes).map_err(|_| invalid("schema is invalid"))?;
    if document
        .object
        .as_deref()
        .is_some_and(|object| object != "list")
    {
        return Err(invalid("schema is invalid"));
    }
    if document.data.len() > MAX_CATALOG_ENTRIES {
        return Err(invalid("exceeds its model bound"));
    }
    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in document.data {
        if !entry.enabled || !is_open_design_model_id(&entry.id) {
            continue;
        }
        if !seen.insert(entry.id.clone()) {
            return Err(invalid("contains a duplicate model id"));
        }
        let known = open_design_model(&entry.id);
        let metadata = entry.metadata.unwrap_or_default();
        // A window set in the table wins; otherwise the budget is what the
        // gateway accepts and the limit is the model's.
        let context_window = known
            .map(|model| model.context_window)
            .or(metadata.context_budget)
            .or(metadata.context_limit)
            .unwrap_or(128_000)
            .max(1);
        let max_output_tokens = metadata
            .output_budget
            .and_then(|tokens| u32::try_from(tokens).ok())
            .or(known.map(|model| model.max_output_tokens))
            .unwrap_or(super::DEFAULT_MAX_OUTPUT_TOKENS)
            .max(1);
        let accepts_images = entry.architecture.map_or_else(
            || known.is_some_and(|model| model.accepts_images),
            |architecture| {
                architecture
                    .input_modalities
                    .iter()
                    .any(|modality| modality == "image")
            },
        );
        let name = entry
            .name
            .filter(|name| !name.is_empty())
            .or_else(|| known.map(|model| model.name.to_owned()))
            .unwrap_or_else(|| entry.id.clone());
        models.push(OpenDesignCatalogEntry {
            id: entry.id,
            name,
            context_window,
            max_output_tokens,
            accepts_images,
            default: entry.default,
        });
    }
    if models.is_empty() {
        return Err(invalid("has no enabled models"));
    }
    Ok(models)
}

pub async fn fetch_open_design_catalog(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<Vec<OpenDesignCatalogEntry>, ProviderError> {
    let mut response = client
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .map_err(|error| ProviderError::Transport {
            safe_to_retry: error.is_connect() || error.is_timeout(),
            message: super::redact_values(
                &format!("Open Design catalog connection: {}", error.without_url()),
                &[api_key.to_owned()],
            ),
        })?;
    if !response.status().is_success() {
        return Err(ProviderError::Remote {
            message: format!(
                "Open Design catalog failed ({})",
                response.status().as_u16()
            ),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(ProviderError::InvalidResponse {
            message: "Open Design catalog exceeds its bound".into(),
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
                &format!("Open Design catalog stream: {}", error.without_url()),
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
                message: "Open Design catalog exceeds its bound".into(),
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_open_design_catalog(&bytes)
}

pub struct OpenDesignAdapter {
    inner: OpenAiCompatibleAdapter,
    model_id: String,
}

impl OpenDesignAdapter {
    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        if !is_open_design_model_id(model) {
            return Err(ProviderError::InvalidResponse {
                message: format!("unsupported Open Design model: {model}"),
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

impl ProviderAdapter for OpenDesignAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenDesign
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

#[cfg(test)]
mod tests {
    use super::{is_open_design_model_id, parse_open_design_catalog, OpenDesignAdapter};
    use crate::provider::{ProviderAdapter, ProviderKind};

    /// The shape `GET /v1/models` returns, trimmed to what the parser reads.
    const CATALOG: &str = r#"{"object":"list","data":[
        {"id":"deepseek-v4.1-flash","object":"model","owned_by":"vela",
         "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},
         "metadata":{"cost":"low","capability":"advanced","context_limit":1048000,"context_budget":328000,"output_budget":128000},
         "default":true,"enabled":true},
        {"id":"hy4-preview","object":"model",
         "architecture":{"input_modalities":["text"]},
         "metadata":{"context_limit":1048000,"context_budget":232000,"output_budget":32000},
         "default":false,"enabled":true},
        {"id":"retired-model","object":"model","enabled":false},
        {"id":"brand-new","object":"model"}
    ]}"#;

    #[test]
    fn catalog_uses_the_gateway_budgets_and_skips_disabled_models() {
        let models = parse_open_design_catalog(CATALOG.as_bytes()).expect("catalog");
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["deepseek-v4.1-flash", "hy4-preview", "brand-new"]);

        let flash = &models[0];
        assert_eq!(
            flash.context_window, 1_000_000,
            "the table's window, not the gateway budget"
        );
        assert_eq!(flash.max_output_tokens, 128_000);
        assert!(flash.accepts_images);
        assert!(flash.default);
        assert_eq!(flash.name, "DeepSeek V4.1 Flash");

        assert_eq!(
            models[1].context_window, 232_000,
            "unknown model: context_budget, not context_limit"
        );
        assert!(!models[1].accepts_images);
        assert_eq!(models[1].max_output_tokens, 32_000);

        // Unknown and without metadata: conservative defaults.
        assert_eq!(models[2].name, "brand-new");
        assert_eq!(models[2].context_window, 128_000);
        assert!(!models[2].accepts_images);
    }

    #[test]
    fn catalog_rejects_duplicates_and_empty_lists() {
        let duplicate = r#"{"data":[{"id":"a"},{"id":"a"}]}"#;
        assert!(parse_open_design_catalog(duplicate.as_bytes()).is_err());
        let empty = r#"{"object":"list","data":[{"id":"x","enabled":false}]}"#;
        assert!(parse_open_design_catalog(empty.as_bytes()).is_err());
        assert!(parse_open_design_catalog(b"[]").is_err());
    }

    #[test]
    fn adapter_speaks_chat_completions_under_its_own_kind() {
        assert!(is_open_design_model_id("deepseek-v4.1-flash"));
        assert!(!is_open_design_model_id("bad model"));
        let adapter = OpenDesignAdapter::new(
            super::OPEN_DESIGN_BASE_URL,
            "deepseek-v4.1-flash",
            "sk-amr-test",
            Some("high"),
        )
        .expect("adapter");
        assert_eq!(adapter.kind(), ProviderKind::OpenDesign);
        assert_eq!(adapter.wire_kind(), ProviderKind::OpenAiCompatible);
        let request =
            adapter.build_messages_request(&[crate::provider::ProviderMessage::user("hi")]);
        assert_eq!(request.url, super::OPEN_DESIGN_BASE_URL);
        assert!(request.body.contains("\"model\":\"deepseek-v4.1-flash\""));
        assert!(
            OpenDesignAdapter::new(super::OPEN_DESIGN_BASE_URL, "bad model", "k", None).is_err()
        );
    }
}
