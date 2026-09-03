mod auth;
mod cli;
mod code_intel;
pub mod codex_catalog;
pub mod command_code_catalog;
mod config;
mod exit_codes;
mod headless;
mod jsonl;
pub mod oauth;
pub mod opencode_go_catalog;
mod tui;

use slim_core::AppHandle;

pub use auth::{
    delete_api_key, delete_api_key_file, load_auth_credential, load_auth_file, redact,
    resolve_api_key, resolve_provider_credential, save_api_key, save_api_key_file, AuthError,
    ProviderCredential,
};
pub use cli::{run_cli, CliOutput};
pub use code_intel::install_code_intel;
pub use config::Config;
pub use exit_codes::ExitCode;
pub use headless::{
    load_local_images, render_provider_jsonl, render_provider_text, render_provider_verbose_text,
    render_text, run_fake_headless, run_provider_headless, run_provider_headless_with_options,
    run_provider_headless_with_resume, run_provider_headless_with_resume_and_options,
    run_provider_headless_with_session, run_provider_headless_with_session_and_options,
    HeadlessRequest, HeadlessResult, OutputFormat, ProviderHeadlessResult, ProviderRequest,
    ProviderRunOptions, UsageCostSummary, MAX_IMAGE_BYTES,
};
pub use jsonl::render_jsonl;
pub use tui::{
    run_provider_tui_turn, run_tui, spawn_tui_runtime, spawn_tui_runtime_with_resume, TuiError,
    TuiRuntimeHandle,
};

pub fn compose_app() -> AppHandle {
    AppHandle::fake()
}

/// G248: model strings are provider-specific — a globally configured model
/// (slim.toml, `SLIM_MODEL`, `--model`) belongs to the provider it was chosen
/// for. Returns `true` when `model` validates against `kind`; OpenAI-compatible
/// and Anthropic accept free-form ids, so they always pass.
pub(crate) fn provider_compatible_model(
    kind: slim_core::provider::ProviderKind,
    model: &str,
) -> bool {
    use slim_core::provider::{open_code_model, ProviderKind};
    use slim_tui::api::ModelAlias;
    match kind {
        ProviderKind::OpenAiCodex => ModelAlias::parse(model).is_some(),
        ProviderKind::OpenCodeGo => open_code_model(model).is_some(),
        ProviderKind::ClinePass => slim_core::provider::is_clinepass_model_id(model),
        ProviderKind::CommandCode => slim_core::provider::is_command_code_model_id(model),
        ProviderKind::Anthropic | ProviderKind::OpenAiCompatible => true,
    }
}

#[cfg(test)]
mod provider_model_tests {
    use slim_core::provider::ProviderKind;

    use super::provider_compatible_model;
    use crate::cli::default_provider_model;
    use slim_tui::api::ModelAlias;

    #[test]
    fn headless_codex_default_matches_tui_sol() {
        assert_eq!(
            default_provider_model(ProviderKind::OpenAiCodex),
            ModelAlias::Sol.id()
        );
        assert_ne!(
            default_provider_model(ProviderKind::OpenAiCodex),
            "gpt-5.3-codex"
        );
    }

    #[test]
    fn anthropic_default_is_shared_between_headless_and_tui() {
        assert_eq!(
            default_provider_model(ProviderKind::Anthropic),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            crate::tui::defaults_for_test(ProviderKind::Anthropic),
            (
                crate::cli::default_provider_endpoint(ProviderKind::Anthropic),
                default_provider_model(ProviderKind::Anthropic),
            )
        );
    }

    #[test]
    fn codex_accepts_aliases_only() {
        assert!(provider_compatible_model(ProviderKind::OpenAiCodex, "luna"));
        assert!(provider_compatible_model(
            ProviderKind::OpenAiCodex,
            "gpt-5.6-terra"
        ));
        assert!(!provider_compatible_model(
            ProviderKind::OpenAiCodex,
            "deepseek-v4-flash"
        ));
        assert!(!provider_compatible_model(
            ProviderKind::OpenAiCodex,
            "cline-pass/kimi-k3"
        ));
    }

    #[test]
    fn opencode_go_accepts_catalog_ids_only() {
        assert!(provider_compatible_model(
            ProviderKind::OpenCodeGo,
            "deepseek-v4-flash"
        ));
        assert!(!provider_compatible_model(ProviderKind::OpenCodeGo, "luna"));
        assert!(!provider_compatible_model(
            ProviderKind::OpenCodeGo,
            "cline-pass/kimi-k3"
        ));
    }

    #[test]
    fn clinepass_accepts_catalog_ids_only() {
        assert!(provider_compatible_model(
            ProviderKind::ClinePass,
            "cline-pass/kimi-k3"
        ));
        assert!(!provider_compatible_model(ProviderKind::ClinePass, "luna"));
        assert!(!provider_compatible_model(
            ProviderKind::ClinePass,
            "deepseek-v4-flash"
        ));
        assert!(provider_compatible_model(
            ProviderKind::ClinePass,
            "cline-pass/future-open-model"
        ));
    }

    #[test]
    fn command_code_accepts_live_ids() {
        assert!(provider_compatible_model(
            ProviderKind::CommandCode,
            "deepseek/deepseek-v4-flash"
        ));
        assert!(provider_compatible_model(
            ProviderKind::CommandCode,
            "stealth/ox-alpha"
        ));
        assert!(!provider_compatible_model(
            ProviderKind::CommandCode,
            "bad id"
        ));
    }

    #[test]
    fn free_form_providers_accept_anything() {
        assert!(provider_compatible_model(
            ProviderKind::Anthropic,
            "claude-anything"
        ));
        assert!(provider_compatible_model(
            ProviderKind::OpenAiCompatible,
            "anything/9.9"
        ));
    }
}
