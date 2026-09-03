use serde::Deserialize;
use serde_json::Value;

use super::{
    AnthropicAdapter, OpenAiCompatibleAdapter, PreparedProviderRequest, ProviderAdapter,
    ProviderCapabilities, ProviderConfig, ProviderError, ProviderEvent, ProviderKind,
    ProviderMessage,
};

pub const COMMANDCODE_BASE_URL: &str = "https://api.commandcode.ai/provider/v1";
pub const COMMANDCODE_MODELS_URL: &str = "https://api.commandcode.ai/provider/v1/models";
pub const COMMANDCODE_DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";

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

const MODELS: &[CommandCodeModel] = &[
    model("claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000),
    model("claude-opus-4-7", "Claude Opus 4.7", 1_000_000),
    model("gpt-5.6-sol", "GPT-5.6 Sol", 1_050_000),
    model("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash", 1_000_000),
    model("deepseek/deepseek-v4-pro", "DeepSeek V4 Pro", 1_000_000),
    model("moonshotai/Kimi-K2.7-Code", "Kimi K2.7 Code", 256_000),
    model("zai-org/GLM-5.3", "GLM-5.3", 1_000_000),
    model("MiniMaxAI/MiniMax-M3", "MiniMax M3", 1_000_000),
    model("xiaomi/mimo-v2.5", "MiMo V2.5", 1_000_000),
    model("Qwen/Qwen3.7-Max", "Qwen 3.7 Max", 1_000_000),
    model("google/gemini-3.7-flash", "Gemini 3.7 Flash", 1_048_576),
    model("stealth/ox-alpha", "Ox Alpha", 1_048_576),
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
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
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

fn protocol_url(endpoint: &str, route: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(endpoint) {
        let mut path = url.path().trim_end_matches('/').to_owned();
        for known in ["/chat/completions", "/messages", "/models"] {
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
    for known in ["/chat/completions", "/messages", "/models"] {
        if base.ends_with(known) {
            base.truncate(base.len() - known.len());
            break;
        }
    }
    format!("{}/{route}{suffix}", base.trim_end_matches('/'))
}

enum WireAdapter {
    Chat(OpenAiCompatibleAdapter),
    Messages(AnthropicAdapter),
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
                WireAdapter::Messages(AnthropicAdapter::new(ProviderConfig::anthropic_bearer(
                    protocol_url(endpoint, "messages"),
                    model,
                    api_key,
                ))?)
            }
        };
        Ok(Self {
            model_id: model.into(),
            api,
            wire,
        })
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        match &mut self.wire {
            WireAdapter::Chat(adapter) => adapter.set_system_prompt(prompt),
            WireAdapter::Messages(adapter) => adapter.set_system_prompt(prompt),
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
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.capabilities(),
            WireAdapter::Messages(adapter) => adapter.capabilities(),
        }
    }

    fn materialize_prompt_cache_intent(&self, body: &mut Value) {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.materialize_prompt_cache_intent(body),
            WireAdapter::Messages(adapter) => adapter.materialize_prompt_cache_intent(body),
        }
    }

    fn response_cache_scope_id(&self) -> Option<u64> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.response_cache_scope_id(),
            WireAdapter::Messages(adapter) => adapter.response_cache_scope_id(),
        }
    }

    fn system_prompt_for_budget(&self) -> Option<&str> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.system_prompt_for_budget(),
            WireAdapter::Messages(adapter) => adapter.system_prompt_for_budget(),
        }
    }

    fn request_envelope_upper_bound_chars(&self) -> Option<u64> {
        Some(1_024)
    }

    fn sensitive_values(&self) -> Vec<String> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.sensitive_values(),
            WireAdapter::Messages(adapter) => adapter.sensitive_values(),
        }
    }

    fn build_request(&self, prompt: &str) -> super::HttpRequest {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.build_request(prompt),
            WireAdapter::Messages(adapter) => adapter.build_request(prompt),
        }
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> super::HttpRequest {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.build_messages_request(messages),
            WireAdapter::Messages(adapter) => adapter.build_messages_request(messages),
        }
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> super::HttpRequest {
        match &self.wire {
            WireAdapter::Chat(adapter) => {
                adapter.build_messages_request_with_tools(messages, tools)
            }
            WireAdapter::Messages(adapter) => {
                adapter.build_messages_request_with_tools(messages, tools)
            }
        }
    }

    fn prepare_messages_request_with_tools_checked(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let prepared = match &self.wire {
            WireAdapter::Chat(adapter) => {
                adapter.prepare_messages_request_with_tools_checked(messages, tools)?
            }
            WireAdapter::Messages(adapter) => {
                adapter.prepare_messages_request_with_tools_checked(messages, tools)?
            }
        };
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn prepare_compaction_request_checked(
        &self,
        messages: &[ProviderMessage],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let prepared = match &self.wire {
            WireAdapter::Chat(adapter) => adapter.prepare_compaction_request_checked(messages)?,
            WireAdapter::Messages(adapter) => {
                adapter.prepare_compaction_request_checked(messages)?
            }
        };
        Ok(prepared.with_routing_identity(self.kind(), self.wire_kind(), self.model()))
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        match &self.wire {
            WireAdapter::Chat(adapter) => adapter.parse_event(value),
            WireAdapter::Messages(adapter) => adapter.parse_event(value),
        }
    }
}
