use serde::Deserialize;
use serde_json::Value;

use super::{
    endpoint::protocol_url_with, wire::WireAdapter, AnthropicAdapter, HttpRequest,
    OpenAiCompatibleAdapter, PreparedProviderRequest, ProviderAdapter, ProviderCapabilities,
    ProviderConfig, ProviderError, ProviderEvent, ProviderKind, ProviderMessage,
};

pub const COMMANDCODE_BASE_URL: &str = "https://api.commandcode.ai/provider/v1";
pub const COMMANDCODE_MODELS_URL: &str = "https://api.commandcode.ai/provider/v1/models";
pub const COMMANDCODE_DEFAULT_MODEL: &str = "deepseek/deepseek-v4.1-flash";

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandCodeApi {
    ChatCompletions,
    AnthropicMessages,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandCodeModel {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandCodeCatalogEntry {
    pub id: String,
    pub name: String,
    pub context_window: u64,
}

// Fallback registry mirrored from GET /provider/v1/models on 2026-09-12
// (69 entries, live order). The live catalog and its on-disk cache are the
// sources of truth at runtime; this list keeps offline startup and
// context-window lookups honest when neither is available.
const MODELS: &[CommandCodeModel] = &[
    model("claude-sonnet-5", "Claude Sonnet 5", 1_000_000),
    model("claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000),
    model("claude-fable-5-1", "Claude Fable 5.1", 1_000_000),
    model("claude-fable-5", "Claude Fable 5", 1_000_000),
    model("claude-opus-5", "Claude Opus 5", 1_000_000),
    model("claude-opus-4-8", "Claude Opus 4.8", 1_000_000),
    model("claude-opus-4-7", "Claude Opus 4.7", 1_000_000),
    model("claude-haiku-4-5-20251001", "Claude Haiku 4.5", 200_000),
    model("gpt-5.6-sol", "GPT-5.6 Sol", 1_050_000),
    model("gpt-5.6-terra", "GPT-5.6 Terra", 1_050_000),
    model("gpt-5.6-luna", "GPT-5.6 Luna", 1_050_000),
    model("gpt-5.5", "GPT-5.5", 400_000),
    model("gpt-5.4", "GPT-5.4", 400_000),
    model("gpt-5.3-codex", "GPT-5.3 Codex", 400_000),
    model("gpt-5.4-mini", "GPT-5.4 Mini", 400_000),
    model(
        "deepseek/deepseek-v4-pro",
        "DeepSeek V4 Pro (latest)",
        1_000_000,
    ),
    model(
        "deepseek/deepseek-v4-flash",
        "DeepSeek V4 Flash (latest)",
        1_000_000,
    ),
    model(
        "deepseek/deepseek-v4-flash-vision-exp",
        "DeepSeek V4 Flash Vision (exp)",
        1_000_000,
    ),
    model(
        "deepseek/deepseek-v4-flash-fast",
        "DeepSeek V4 Flash Fast",
        1_000_000,
    ),
    model(
        "deepseek/deepseek-v4.1-flash",
        "DeepSeek V4.1 Flash",
        1_000_000,
    ),
    model("moonshotai/Kimi-K3", "Kimi K3", 1_000_000),
    model("moonshotai/Kimi-K2.7-Code", "Kimi K2.7 Code", 256_000),
    model(
        "moonshotai/Kimi-K2.7-Code-Highspeed",
        "Kimi K2.7 Code HighSpeed",
        262_000,
    ),
    model("moonshotai/Kimi-K2.6", "Kimi K2.6", 256_000),
    model("moonshotai/Kimi-K2.5", "Kimi K2.5", 256_000),
    model("z-ai/glm-5.3-flash", "GLM-5.3 Flash", 1_048_576),
    model("zai-org/GLM-5.3", "GLM-5.3", 1_000_000),
    model("zai-org/GLM-5.2", "GLM-5.2", 1_000_000),
    model("zai-org/GLM-5.2-Fast", "GLM-5.2 Fast", 1_000_000),
    model("zai-org/GLM-5.1", "GLM-5.1", 200_000),
    model("zai-org/GLM-5", "GLM-5", 200_000),
    model("MiniMaxAI/MiniMax-M3", "MiniMax M3", 1_000_000),
    model("MiniMaxAI/MiniMax-M2.7", "MiniMax M2.7", 200_000),
    model("MiniMaxAI/MiniMax-M2.5", "MiniMax M2.5", 200_000),
    model("xiaomi/mimo-v2.5-pro", "MiMo V2.5 Pro", 1_000_000),
    model("xiaomi/mimo-v2.5", "MiMo V2.5", 1_000_000),
    model("Qwen/Qwen3.8-Max-0902", "Qwen 3.8 Max 0902", 1_000_000),
    model("Qwen/Qwen3.8-Max", "Qwen 3.8 Max", 1_000_000),
    model("Qwen/Qwen3.8-27B", "Qwen 3.8 27B", 262_144),
    model("Qwen/Qwen3.8-Flash", "Qwen 3.8 Flash", 1_000_000),
    model("Qwen/Qwen3.7-Max", "Qwen 3.7 Max", 1_000_000),
    model("Qwen/Qwen3.7-Plus", "Qwen 3.7 Plus", 1_000_000),
    model("Qwen/Qwen3.7-Flash", "Qwen 3.7 Flash", 1_000_000),
    model("Qwen/Qwen3.6-Max-Preview", "Qwen 3.6 Max Preview", 200_000),
    model("Qwen/Qwen3.6-Plus", "Qwen 3.6 Plus", 200_000),
    model("meituan/LongCat-2.0:free", "LongCat 2.0", 1_048_576),
    model("stepfun/Step-3.7-Flash", "Step 3.7 Flash", 256_000),
    model("stepfun/Step-3.5-Flash", "Step 3.5 Flash", 1_000_000),
    model("tencent/hy3-paid", "Tencent Hy3", 262_144),
    model("tencent/hy4-preview", "Tencent Hy4 Preview", 1_048_576),
    model("google/gemini-3.8-flash", "Gemini 3.8 Flash", 1_000_000),
    model("google/gemini-3.7-flash", "Gemini 3.7 Flash", 1_048_576),
    model("google/gemini-3.6-flash", "Gemini 3.6 Flash", 1_000_000),
    model("google/gemini-3.5-flash", "Gemini 3.5 Flash", 1_000_000),
    model(
        "google/gemini-3.5-flash-lite",
        "Gemini 3.5 Flash Lite",
        1_000_000,
    ),
    model(
        "google/gemini-3.1-flash-lite",
        "Gemini 3.1 Flash Lite",
        1_000_000,
    ),
    model("sakana/fugu-ultra", "Fugu Ultra", 1_000_000),
    model(
        "nvidia/nemotron-3-ultra-550b-a55b",
        "Nemotron 3 Ultra",
        1_000_000,
    ),
    model("thinkingmachines/inkling", "Inkling", 256_000),
    model("thinkingmachines/inkling-small", "Inkling Small", 1_000_000),
    model("poolside/laguna-s-2.1-free", "Laguna S 2.1", 256_000),
    model(
        "inclusionai/ling-3.0-flash-sante:free",
        "Ling 3.0 Flash Sante",
        262_144,
    ),
    model("meta/muse-spark-1.1", "Muse Spark 1.1", 1_048_576),
    model("meta/muse-spark-1.2", "Muse Spark 1.2", 1_048_576),
    model(
        "meta/muse-spark-1.2-contributor",
        "Muse Spark 1.2 Contributor",
        1_048_576,
    ),
    model("meta/muse-spark-1.3", "Muse Spark 1.3", 1_048_576),
    model(
        "meta/muse-spark-1.3-contributor",
        "Muse Spark 1.3 Contributor",
        1_048_576,
    ),
    model("xai/grok-4.5", "Grok 4.5", 500_000),
    model("xai/grok-4.6", "Grok 4.6", 500_000),
];

const fn model(id: &'static str, name: &'static str, context_window: u64) -> CommandCodeModel {
    CommandCodeModel {
        id,
        name,
        context_window,
    }
}

pub fn command_code_models() -> &'static [CommandCodeModel] {
    MODELS
}

pub fn command_code_model(id: &str) -> Option<&'static CommandCodeModel> {
    MODELS.iter().find(|model| model.id == id)
}

pub fn command_code_api(model: &str) -> CommandCodeApi {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude") || lower.contains("/claude") {
        CommandCodeApi::AnthropicMessages
    } else {
        CommandCodeApi::ChatCompletions
    }
}

pub fn is_command_code_model_id(id: &str) -> bool {
    validate_model_id(id).is_ok()
}

#[derive(Deserialize)]
struct CatalogDocument {
    object: String,
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

pub fn parse_command_code_catalog(
    bytes: &[u8],
) -> Result<Vec<CommandCodeCatalogEntry>, ProviderError> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(invalid_catalog("Command Code catalog exceeds its bound"));
    }
    let document: CatalogDocument = serde_json::from_slice(bytes)
        .map_err(|_| invalid_catalog("Command Code catalog schema is invalid"))?;
    if document.object != "list" || document.data.len() > MAX_CATALOG_ENTRIES {
        return Err(invalid_catalog("Command Code catalog schema is invalid"));
    }
    let mut models = Vec::with_capacity(document.data.len());
    let mut seen = std::collections::HashSet::new();
    for entry in document.data {
        validate_model_id(&entry.id)
            .map_err(|_| invalid_catalog("Command Code model id is invalid"))?;
        if !seen.insert(entry.id.clone()) {
            return Err(invalid_catalog(
                "Command Code catalog contains a duplicate model id",
            ));
        }
        let name = entry
            .name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| entry.id.clone());
        models.push(CommandCodeCatalogEntry {
            id: entry.id,
            name,
            context_window: entry.context_length.unwrap_or(128_000).max(1),
        });
    }
    if models.is_empty() {
        return Err(invalid_catalog("Command Code catalog has no models"));
    }
    Ok(models)
}

fn validate_model_id(id: &str) -> Result<(), ()> {
    if id.is_empty()
        || id.len() > MAX_MODEL_ID_BYTES
        || !id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-' | b':')
        })
    {
        Err(())
    } else {
        Ok(())
    }
}

fn invalid_catalog(message: &str) -> ProviderError {
    ProviderError::InvalidResponse {
        message: message.into(),
    }
}

pub struct CommandCodeAdapter {
    model_id: String,
    api: CommandCodeApi,
    wire: WireAdapter,
}

impl CommandCodeAdapter {
    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Self, ProviderError> {
        validate_model_id(model).map_err(|_| ProviderError::InvalidResponse {
            message: format!("unsupported Command Code model: {model}"),
        })?;
        let api = command_code_api(model);
        let wire = match api {
            CommandCodeApi::ChatCompletions => {
                let mut config = ProviderConfig::openai(
                    protocol_url(endpoint, "chat/completions"),
                    model,
                    api_key,
                );
                if let Some(effort) = reasoning_effort.filter(|value| !value.is_empty()) {
                    config = config.with_reasoning_effort(effort);
                }
                WireAdapter::Chat(OpenAiCompatibleAdapter::new(config)?)
            }
            CommandCodeApi::AnthropicMessages => {
                let mut config = ProviderConfig::anthropic_bearer(
                    protocol_url(endpoint, "messages"),
                    model,
                    api_key,
                );
                if let Some(effort) = reasoning_effort {
                    config = config.with_reasoning_effort(effort);
                }
                WireAdapter::Messages(AnthropicAdapter::new(config)?)
            }
        };
        Ok(Self {
            model_id: model.into(),
            api,
            wire,
        })
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.wire.set_system_prompt(prompt.into());
        self
    }

    pub fn with_response_cache_scope_id(mut self, id: u64) -> Self {
        self.wire.set_response_cache_scope_id(id);
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.wire.set_max_output_tokens(tokens);
        self
    }

    /// Sends `x-cmd-zdr: 1` on every request, opting into Command Code's
    /// zero-data-retention routing. Upstreams without ZDR capacity answer 422
    /// (`cmd_zdr_no_providers`) instead of falling back — the failure is
    /// intentional and surfaces as an ordinary provider error.
    pub fn with_zero_data_retention(mut self, enabled: bool) -> Self {
        if enabled {
            self.wire.push_extra_header("x-cmd-zdr", "1");
        }
        self
    }
}

impl ProviderAdapter for CommandCodeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::CommandCode
    }

    fn wire_kind(&self) -> ProviderKind {
        match self.api {
            CommandCodeApi::ChatCompletions => ProviderKind::OpenAiCompatible,
            CommandCodeApi::AnthropicMessages => ProviderKind::Anthropic,
        }
    }

    fn model(&self) -> &str {
        &self.model_id
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
        self.wire.build_request(prompt)
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        self.wire.build_messages_request(messages)
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.wire.build_messages_request_with_tools(messages, tools)
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let prepared = self
            .wire
            .prepare_messages_request_with_tools_checked(self, messages, tools)?;
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let prepared = self
            .wire
            .prepare_compaction_request_checked(self, messages)?;
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.wire.parse_event(value)
    }
}

fn protocol_url(endpoint: &str, route: &str) -> String {
    protocol_url_with(
        endpoint,
        route,
        &["/chat/completions", "/messages", "/models"],
    )
}
