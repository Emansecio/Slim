use super::{
    opencode::{OpenCodeAdapter, OpenCodeFlavor},
    ProviderKind,
};

pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const OPENCODE_GO_MODELS_URL: &str = "https://opencode.ai/zen/go/v1/models";
pub const OPENCODE_GO_DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

const REASONING_HIGH: &[&str] = &["high"];
const REASONING_HIGH_MAX: &[&str] = &["high", "max"];
const REASONING_LOW_HIGH: &[&str] = &["low", "high"];
const REASONING_LOW_HIGH_MAX: &[&str] = &["low", "high", "max"];
const REASONING_FULL: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const REASONING_STANDARD: &[&str] = &["low", "medium", "high"];
const REASONING_LOW_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
const REASONING_MAX: &[&str] = &["max"];

// Protocol per model follows the endpoint table of
// https://opencode.ai/v2/docs/console/go/ (verified 2026-10-22); the
// `/zen/go/v1` paths it serves under `/inference/go` still answer.
const MODELS: &[OpenCodeModel] = &[
    model(
        "grok-4.5",
        "Grok 4.5",
        OpenCodeApi::Responses,
        500000,
        500000,
        true,
        REASONING_STANDARD,
    ),
    model(
        "gpt-5.6-luna",
        "GPT 5.6 Luna",
        OpenCodeApi::Responses,
        1050000,
        128000,
        true,
        REASONING_FULL,
    ),
    model(
        "glm-5.3",
        "GLM-5.3",
        OpenCodeApi::ChatCompletions,
        1000000,
        131072,
        false,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "glm-5.2",
        "GLM-5.2",
        OpenCodeApi::ChatCompletions,
        1000000,
        131072,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "glm-5.1",
        "GLM-5.1",
        OpenCodeApi::ChatCompletions,
        202752,
        32768,
        false,
        REASONING_HIGH,
    ),
    model(
        "kimi-k3",
        "Kimi K3",
        OpenCodeApi::ChatCompletions,
        1048576,
        131072,
        true,
        REASONING_MAX,
    ),
    model(
        "kimi-k2.7-code",
        "Kimi K2.7 Code",
        OpenCodeApi::ChatCompletions,
        262144,
        262144,
        true,
        REASONING_HIGH,
    ),
    model(
        "kimi-k2.6",
        "Kimi K2.6",
        OpenCodeApi::Responses,
        262144,
        65536,
        true,
        REASONING_HIGH,
    ),
    model(
        "deepseek-flash",
        "DeepSeek V4.1 Flash",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        true,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        false,
        REASONING_HIGH_MAX,
    ),
    model(
        "deepseek-v4-flash-vision-exp",
        "DeepSeek V4 Flash Vision Exp",
        OpenCodeApi::ChatCompletions,
        1000000,
        384000,
        true,
        REASONING_HIGH_MAX,
    ),
    model(
        "mimo-v2.5",
        "MiMo-V2.5",
        OpenCodeApi::ChatCompletions,
        1000000,
        128000,
        true,
        REASONING_HIGH,
    ),
    model(
        "mimo-v2.5-pro",
        "MiMo-V2.5-Pro",
        OpenCodeApi::ChatCompletions,
        1048576,
        128000,
        false,
        REASONING_HIGH,
    ),
    model(
        "minimax-m3",
        "MiniMax M3",
        OpenCodeApi::AnthropicMessages,
        1000000,
        131072,
        true,
        REASONING_HIGH,
    ),
    model(
        "minimax-m2.7",
        "MiniMax M2.7",
        OpenCodeApi::ChatCompletions,
        204800,
        131072,
        false,
        REASONING_HIGH,
    ),
    model(
        "minimax-m2.5",
        "MiniMax M2.5",
        OpenCodeApi::ChatCompletions,
        204800,
        131072,
        false,
        REASONING_HIGH,
    ),
    model(
        "muse-spark-1.2-contributor",
        "Muse Spark 1.2 Contributor",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    // Go routes both Muse contributor models through /responses:
    // https://opencode.ai/docs/go/#endpoints (verified 2026-09-06).
    // Live on the public catalog since 2026-09-02; metadata mirrors the 1.2
    // contributor checkpoint on the same gateway (1M context, 131072 output
    // ceiling, multimodal input, reasoning). Source: Meta model docs +
    // public launch notes, verified 2026-09-03.
    model(
        "muse-spark-1.3-contributor",
        "Muse Spark 1.3 Contributor",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "qwen3.8-max",
        "Qwen3.8 Max",
        OpenCodeApi::AnthropicMessages,
        1000000,
        131072,
        true,
        REASONING_HIGH,
    ),
    model(
        "qwen3.7-max",
        "Qwen3.7 Max",
        OpenCodeApi::ChatCompletions,
        1000000,
        65536,
        false,
        REASONING_HIGH,
    ),
    model(
        "qwen3.7-plus",
        "Qwen3.7 Plus",
        OpenCodeApi::AnthropicMessages,
        1000000,
        65536,
        true,
        REASONING_HIGH,
    ),
    model(
        "qwen3.6-plus",
        "Qwen3.6 Plus",
        OpenCodeApi::ChatCompletions,
        1000000,
        65536,
        true,
        REASONING_HIGH,
    ),
    model(
        "hy3",
        "Hy3",
        OpenCodeApi::ChatCompletions,
        256000,
        64000,
        false,
        REASONING_LOW_HIGH,
    ),
    model(
        "ox-alpha-free",
        "Ox Alpha Free",
        OpenCodeApi::ChatCompletions,
        1000000,
        131072,
        true,
        REASONING_HIGH,
    ),
];

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
        "deepseek-v4.1-flash" => "deepseek-flash",
        other => other,
    };
    MODELS.iter().find(|model| model.id == id)
}

pub struct OpenCodeGo;

impl OpenCodeFlavor for OpenCodeGo {
    const KIND: ProviderKind = ProviderKind::OpenCodeGo;
    const LABEL: &'static str = "OpenCode Go";
    const ACCOUNT: &'static str = "opencode-go";
    const SUPPORTS_MESSAGES_API: bool = true;
    const DROPS_EFFORT_WITHOUT_LEVELS: bool = false;

    fn lookup(model_id: &str) -> Option<&'static OpenCodeModel> {
        open_code_model(model_id)
    }
}

pub type OpenCodeGoAdapter = OpenCodeAdapter<OpenCodeGo>;
