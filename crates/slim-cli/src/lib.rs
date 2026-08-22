mod auth;
mod cli;
mod config;
mod exit_codes;
mod headless;
mod jsonl;
pub mod oauth;
mod tui;

use slim_core::AppHandle;

pub use auth::{load_auth_file, redact, resolve_api_key, AuthError};
pub use cli::{run_cli, CliOutput};
pub use config::Config;
pub use exit_codes::ExitCode;
pub use headless::{
    load_local_images, render_provider_jsonl, render_provider_text, render_text, run_fake_headless,
    run_provider_headless, run_provider_headless_with_options, run_provider_headless_with_session,
    run_provider_headless_with_session_and_options, HeadlessRequest, HeadlessResult, OutputFormat,
    ProviderHeadlessResult, ProviderRequest, ProviderRunOptions, MAX_IMAGE_BYTES,
};
pub use jsonl::render_jsonl;
pub use tui::{run_provider_tui_turn, run_tui, spawn_tui_runtime, TuiError, TuiRuntimeHandle};

pub fn compose_app() -> AppHandle {
    AppHandle::fake()
}
