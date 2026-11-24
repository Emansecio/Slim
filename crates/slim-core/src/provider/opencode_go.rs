use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use super::{
    opencode::{OpenCodeAdapter, OpenCodeFlavor},
    ProviderKind,
};

pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const OPENCODE_GO_CONSOLE_MODELS_URL: &str = "https://opencode.ai/console/api/v1/models";
pub const OPENCODE_GO_MODELS_URL: &str = "https://opencode.ai/inference/go/v1/models";
pub const OPENCODE_GO_OPENAI_BASE_URL: &str = "https://opencode.ai/inference/go/openai/v1";
pub const OPENCODE_GO_DEFAULT_MODEL: &str = "deepseek-v4.1-flash";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OpenCodeModelSpec {
    pub id: String,
    pub name: String,
    pub api: OpenCodeApi,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub accepts_images: bool,
    pub reasoning_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl From<&OpenCodeModel> for OpenCodeModelSpec {
    fn from(model: &OpenCodeModel) -> Self {
        Self {
            id: model.id.to_owned(),
            name: model.name.to_owned(),
            api: model.api,
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            accepts_images: model.accepts_images,
            reasoning_levels: model
                .reasoning_levels
                .iter()
                .map(|level| (*level).to_owned())
                .collect(),
            base_url: None,
        }
    }
}

const REASONING_NONE: &[&str] = &[];
const REASONING_HIGH: &[&str] = &["high"];
const REASONING_HIGH_MAX: &[&str] = &["high", "max"];
const REASONING_LOW_HIGH: &[&str] = &["low", "high"];
const REASONING_LOW_HIGH_XHIGH: &[&str] = &["low", "high", "xhigh"];
const REASONING_LOW_MEDIUM_XHIGH: &[&str] = &["low", "medium", "xhigh"];
const REASONING_LOW_HIGH_MAX: &[&str] = &["low", "high", "max"];
const REASONING_FULL: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const REASONING_STANDARD: &[&str] = &["low", "medium", "high"];
const REASONING_LOW_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
const REASONING_MAX: &[&str] = &["max"];

// Fallback catalog matching https://opencode.ai/inference/go/v1/models and
// https://opencode.ai/console/api/v1/models (verified 2026-11-20). Live
// refreshes supersede this table so new models work without a release.
const MODELS: &[OpenCodeModel] = &[
    model(
        "deepseek-v4.1-flash",
        "DeepSeek V4.1 Flash",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        384_000,
        true,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        384_000,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "glm-5.3",
        "GLM-5.3",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        131_072,
        false,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "glm-5.3-flash",
        "GLM-5.3-Flash",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        131_072,
        true,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "grok-4.7",
        "Grok 4.7",
        OpenCodeApi::Responses,
        500_000,
        500_000,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "gpt-6-luna",
        "GPT-6 Luna",
        OpenCodeApi::Responses,
        1_050_000,
        128_000,
        true,
        REASONING_FULL,
    ),
    model(
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        OpenCodeApi::Responses,
        1_050_000,
        128_000,
        true,
        REASONING_FULL,
    ),
    model(
        "claude-haiku-5-5",
        "Claude Haiku 5.5",
        OpenCodeApi::AnthropicMessages,
        1_000_000,
        128_000,
        true,
        REASONING_FULL,
    ),
    model(
        "gemini-4-argon",
        "Gemini 4 Argon",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        65_536,
        true,
        REASONING_STANDARD,
    ),
    model(
        "kimi-k3.1-pro",
        "Kimi K3.1 Pro",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        1_048_576,
        true,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "kimi-k3",
        "Kimi K3",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        1_048_576,
        true,
        REASONING_MAX,
    ),
    model(
        "kimi-k2.7-code",
        "Kimi K2.7 Code",
        OpenCodeApi::ChatCompletions,
        262_144,
        262_144,
        true,
        REASONING_NONE,
    ),
    model(
        "mimo-v2.6-pro",
        "MiMo-V2.6-Pro",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        131_072,
        true,
        REASONING_NONE,
    ),
    model(
        "mimo-v2.6-flash",
        "MiMo-V2.6-Flash",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        131_072,
        true,
        REASONING_NONE,
    ),
    model(
        "minimax-m3.1-flash",
        "MiniMax M3.1 Flash",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        512_000,
        true,
        REASONING_FULL,
    ),
    model(
        "minimax-m3",
        "MiniMax-M3",
        OpenCodeApi::AnthropicMessages,
        1_000_000,
        131_072,
        true,
        REASONING_NONE,
    ),
    model(
        "qwen4-max",
        "Qwen4 Max",
        OpenCodeApi::AnthropicMessages,
        1_000_000,
        262_144,
        true,
        REASONING_LOW_HIGH_XHIGH,
    ),
    model(
        "qwen3.8-max",
        "Qwen3.8 Max",
        OpenCodeApi::AnthropicMessages,
        1_000_000,
        131_072,
        true,
        REASONING_LOW_MEDIUM_XHIGH,
    ),
    model(
        "qwen3.8-flash",
        "Qwen3.8 Flash",
        OpenCodeApi::AnthropicMessages,
        1_000_000,
        131_072,
        true,
        REASONING_LOW_MEDIUM_XHIGH,
    ),
    model(
        "qwen3.7-plus",
        "Qwen3.7 Plus",
        OpenCodeApi::AnthropicMessages,
        1_000_000,
        65_536,
        true,
        REASONING_NONE,
    ),
    model(
        "hy4",
        "Hy4",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        65_536,
        true,
        REASONING_HIGH,
    ),
    model(
        "hy4-preview",
        "Hy4 preview",
        OpenCodeApi::ChatCompletions,
        1_024_000,
        64_000,
        false,
        REASONING_HIGH,
    ),
    model(
        "hy4-flash-free",
        "Hy4 flash Free",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        65_536,
        true,
        REASONING_HIGH,
    ),
    model(
        "hy3",
        "Hy3",
        OpenCodeApi::ChatCompletions,
        256_000,
        128_000,
        false,
        REASONING_LOW_HIGH,
    ),
    model(
        "longcat-2.0",
        "LongCat-2.0",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        131_072,
        false,
        REASONING_NONE,
    ),
    model(
        "longcat-2.5-free",
        "LongCat 2.5 Free",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        262_144,
        true,
        REASONING_NONE,
    ),
    model(
        "muse-spark-1.3-contributor",
        "Muse Spark 1.3 Contributor",
        OpenCodeApi::Responses,
        1_048_576,
        131_072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "muse-spark-1.2-contributor",
        "Muse Spark 1.2 Contributor",
        OpenCodeApi::Responses,
        1_048_576,
        131_072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "step-5-preview",
        "Step 5 Preview",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        65_536,
        true,
        REASONING_STANDARD,
    ),
    model(
        "beam-free",
        "Beam Free",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        131_072,
        false,
        REASONING_FULL,
    ),
    model(
        "iceman-contributor-free",
        "Iceman Contributor Free",
        OpenCodeApi::ChatCompletions,
        262_144,
        131_072,
        false,
        REASONING_NONE,
    ),
];

// Retired Go model ids kept resolvable for existing configs, sessions and
// tests; omitted from the fallback picker list in `open_code_models()`.
const LEGACY_MODELS: &[OpenCodeModel] = &[
    model(
        "grok-4.5",
        "Grok 4.5",
        OpenCodeApi::Responses,
        500_000,
        500_000,
        true,
        REASONING_STANDARD,
    ),
    model(
        "glm-5.2",
        "GLM-5.2",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        131_072,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "glm-5.1",
        "GLM-5.1",
        OpenCodeApi::ChatCompletions,
        202_752,
        32_768,
        false,
        REASONING_HIGH,
    ),
    model(
        "kimi-k2.6",
        "Kimi K2.6",
        OpenCodeApi::Responses,
        262_144,
        65_536,
        true,
        REASONING_HIGH,
    ),
    model(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        384_000,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "deepseek-v4-flash-vision-exp",
        "DeepSeek V4 Flash Vision Exp",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        384_000,
        true,
        REASONING_HIGH_MAX,
    ),
    model(
        "mimo-v2.5",
        "MiMo-V2.5",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        128_000,
        true,
        REASONING_HIGH,
    ),
    model(
        "mimo-v2.5-pro",
        "MiMo-V2.5-Pro",
        OpenCodeApi::ChatCompletions,
        1_048_576,
        128_000,
        false,
        REASONING_HIGH,
    ),
    model(
        "minimax-m2.7",
        "MiniMax M2.7",
        OpenCodeApi::ChatCompletions,
        204_800,
        131_072,
        false,
        REASONING_HIGH,
    ),
    model(
        "minimax-m2.5",
        "MiniMax M2.5",
        OpenCodeApi::ChatCompletions,
        204_800,
        131_072,
        false,
        REASONING_HIGH,
    ),
    model(
        "qwen3.7-max",
        "Qwen3.7 Max",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        65_536,
        false,
        REASONING_HIGH,
    ),
    model(
        "qwen3.6-plus",
        "Qwen3.6 Plus",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        65_536,
        true,
        REASONING_HIGH,
    ),
    model(
        "ox-alpha-free",
        "Ox Alpha Free",
        OpenCodeApi::ChatCompletions,
        1_000_000,
        131_072,
        true,
        REASONING_HIGH,
    ),
];

static LIVE_MODELS: RwLock<Vec<OpenCodeModelSpec>> = RwLock::new(Vec::new());

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
        "deepseek-flash" => "deepseek-v4.1-flash",
        other => other,
    };
    MODELS
        .iter()
        .chain(LEGACY_MODELS.iter())
        .find(|model| model.id == id)
}

/// Validates that a live catalog `base_url` points to the official OpenCode
/// host over HTTPS with no credentials, query or fragment.
pub fn is_trusted_opencode_base_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("opencode.ai")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && (parsed.path().starts_with("/inference/") || parsed.path().starts_with("/zen/"))
}

/// Records models returned by the live or cached OpenCode Go catalog so new
/// models and updated metadata work immediately across adapters and budgets.
pub fn register_open_code_models(models: &[OpenCodeModelSpec]) {
    let mut live = LIVE_MODELS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *live = models.to_vec();
}

/// Resolves an OpenCode Go model id against the live/cached catalog first,
/// falling back to the bundled registry and legacy aliases.
pub fn resolve_open_code_model(id: &str) -> Option<OpenCodeModelSpec> {
    let canonical = match id {
        "deepseek-flash" => "deepseek-v4.1-flash",
        other => other,
    };
    let live = LIVE_MODELS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(found) = live
        .iter()
        .find(|model| model.id == id || model.id == canonical)
    {
        return Some(found.clone());
    }
    drop(live);
    open_code_model(id).map(OpenCodeModelSpec::from)
}

pub struct OpenCodeGo;

impl OpenCodeFlavor for OpenCodeGo {
    const KIND: ProviderKind = ProviderKind::OpenCodeGo;
    const LABEL: &'static str = "OpenCode Go";
    const ACCOUNT: &'static str = "opencode-go";
    const DEFAULT_BASE_URL: &'static str = OPENCODE_GO_BASE_URL;
    const SUPPORTS_MESSAGES_API: bool = true;
    const DROPS_EFFORT_WITHOUT_LEVELS: bool = true;

    fn lookup(model_id: &str) -> Option<OpenCodeModelSpec> {
        resolve_open_code_model(model_id)
    }
}

pub type OpenCodeGoAdapter = OpenCodeAdapter<OpenCodeGo>;
