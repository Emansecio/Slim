use super::{
    opencode::{OpenCodeAdapter, OpenCodeFlavor},
    opencode_go::{model, OpenCodeApi, OpenCodeModel},
    ProviderKind,
};

pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const OPENCODE_ZEN_MODELS_URL: &str = "https://opencode.ai/zen/v1/models";
pub const OPENCODE_ZEN_DEFAULT_MODEL: &str = "big-pickle";
/// Bearer accepted by the Zen free tier when no account key is configured.
/// The gateway still requires `x-opencode-session` on every request.
pub const OPENCODE_ZEN_PUBLIC_KEY: &str = "public";

const REASONING_NONE: &[&str] = &[];
const REASONING_LOW_HIGH_MAX: &[&str] = &["low", "high", "max"];
const REASONING_LOW_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];

// Zero-cost Zen models live on https://opencode.ai/zen/v1 (verified
// 2026-09-11). Endpoint per model follows https://opencode.ai/docs/zen/;
// limits and reasoning options follow the models.dev `opencode` catalog.
const ZEN_MODELS: &[OpenCodeModel] = &[
    // Big Pickle caps input at 160k of its 200k window (models.dev limit).
    model(
        "big-pickle",
        "Big Pickle",
        OpenCodeApi::ChatCompletions,
        160000,
        32000,
        false,
        REASONING_NONE,
    ),
    model(
        "deepseek-v4-flash-free",
        "DeepSeek V4 Flash Free",
        OpenCodeApi::ChatCompletions,
        200000,
        128000,
        false,
        REASONING_LOW_HIGH_MAX,
    ),
    model(
        "ling-3.0-flash-fin-free",
        "Ling 3.0 Flash Fin Free",
        OpenCodeApi::ChatCompletions,
        262144,
        32768,
        false,
        REASONING_NONE,
    ),
    model(
        "mimo-v2.5-free",
        "MiMo-V2.5 Free",
        OpenCodeApi::ChatCompletions,
        200000,
        32000,
        true,
        REASONING_NONE,
    ),
    // Zen routes the Muse contributor free tier through /responses like Go.
    model(
        "muse-spark-1.2-contributor-free",
        "Muse Spark 1.2 Free",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "muse-spark-1.3-contributor-free",
        "Muse Spark 1.3 Free",
        OpenCodeApi::Responses,
        1048576,
        131072,
        true,
        REASONING_LOW_XHIGH,
    ),
    model(
        "nemotron-3-ultra-free",
        "Nemotron 3 Ultra Free",
        OpenCodeApi::ChatCompletions,
        1000000,
        128000,
        false,
        REASONING_NONE,
    ),
    model(
        "nemotron-3.5-lightning-free",
        "Nemotron 3.5 Lightning Free",
        OpenCodeApi::ChatCompletions,
        262144,
        262144,
        false,
        REASONING_NONE,
    ),
];

pub fn zen_models() -> &'static [OpenCodeModel] {
    ZEN_MODELS
}

pub fn zen_model(id: &str) -> Option<&'static OpenCodeModel> {
    ZEN_MODELS.iter().find(|model| model.id == id)
}

pub struct OpenCodeZen;

impl OpenCodeFlavor for OpenCodeZen {
    const KIND: ProviderKind = ProviderKind::OpenCodeZen;
    const LABEL: &'static str = "OpenCode Zen";
    const ACCOUNT: &'static str = "opencode-zen";
    const SUPPORTS_MESSAGES_API: bool = false;
    const DROPS_EFFORT_WITHOUT_LEVELS: bool = true;

    fn lookup(model_id: &str) -> Option<&'static OpenCodeModel> {
        zen_model(model_id)
    }
}

pub type OpenCodeZenAdapter = OpenCodeAdapter<OpenCodeZen>;
