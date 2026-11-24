use std::time::Duration;

use slim_core::provider::{
    AnthropicAdapter, ClinePassAdapter, CommandCodeAdapter, DevinAdapter, HttpProviderClient,
    OpenAiCodexAdapter, OpenAiCompatibleAdapter, OpenCodeGoAdapter, OpenCodeZenAdapter,
    OpenDesignAdapter, ProviderAdapter, ProviderConfig, ProviderError, ProviderEvent, ProviderKind,
    ProviderTimeouts, ThinkingDisplay, XaiAdapter, DEFAULT_MAX_OUTPUT_TOKENS,
};
use slim_core::runtime::CancellationToken;
use slim_core::ProviderMessage;

use crate::auth::{resolve_provider_credential, ProviderCredential};
use crate::headless::{command_code_zero_data_retention, devin_entry};
use crate::oauth::{codex_account_id, OAuthProvider, OAuthService};
use crate::tui::shared_tui_runtime;

const DEFAULT_MICRO_SYSTEM_PROMPT: &str = "Answer concisely with only the requested output.";

#[derive(Clone, Debug)]
pub struct FastMicroRequest {
    pub provider: ProviderKind,
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub account_id: Option<String>,
    pub oauth: bool,
    pub system_prompt: Option<String>,
    pub prompt: String,
    pub max_output_tokens: u32,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct ResolvedFastAuth {
    pub api_key: String,
    pub account_id: Option<String>,
    pub oauth: bool,
}

/// Resolves and refreshes stored credentials on the shared async runtime so
/// ephemeral micro-calls never construct a throwaway Tokio runtime.
pub fn resolve_fast_provider_auth(provider: ProviderKind) -> Result<ResolvedFastAuth, String> {
    let cred = match resolve_provider_credential(provider) {
        Ok(Some(c)) => c,
        Ok(None) if provider == ProviderKind::OpenCodeZen => ProviderCredential {
            access: slim_core::provider::OPENCODE_ZEN_PUBLIC_KEY.into(),
            account_id: None,
            oauth: false,
        },
        Ok(None) => return Err("nenhuma credencial encontrada para o provedor escolhido".into()),
        Err(e) => return Err(format!("não consegui ler o login do provedor: {e}")),
    };

    if !cred.oauth {
        let access = cred.access;
        let account_id = match provider {
            ProviderKind::OpenAiCodex => cred
                .account_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .or_else(|| codex_account_id(&access).ok()),
            _ => None,
        };
        return Ok(ResolvedFastAuth {
            api_key: access,
            account_id,
            oauth: false,
        });
    }

    let oauth_provider = match provider {
        ProviderKind::Anthropic => OAuthProvider::Anthropic,
        ProviderKind::OpenAiCodex => OAuthProvider::OpenAiCodex,
        ProviderKind::Xai => OAuthProvider::Xai,
        _ => return Err("login OAuth não disponível para este provedor".into()),
    };

    let runtime = shared_tui_runtime()?;
    runtime.block_on(async move {
        let service = OAuthService::production().map_err(|e| e.to_string())?;
        let stored = service
            .credential(oauth_provider)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "login OAuth ausente no arquivo de credenciais".to_string())?;
        let fresh = service
            .fresh_credential(oauth_provider, stored)
            .await
            .map_err(|e| format!("não consegui renovar o login: {e}"));
        service.shutdown().await;
        let fresh = fresh?;
        let access = fresh.credential.access;
        let account_id = match provider {
            ProviderKind::OpenAiCodex => fresh
                .credential
                .account_id
                .filter(|id| !id.is_empty())
                .or_else(|| codex_account_id(&access).ok()),
            _ => None,
        };
        Ok(ResolvedFastAuth {
            api_key: access,
            account_id,
            oauth: true,
        })
    })
}

async fn execute_adapter<A: ProviderAdapter>(
    adapter: A,
    prompt: &str,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<String, ProviderError> {
    let timeouts = if timeout.is_zero() {
        ProviderTimeouts::production(Duration::from_secs(30))
    } else {
        ProviderTimeouts::production(timeout)
    };
    let cache = slim_core::provider::ProviderCache::default_persistent();
    let client = HttpProviderClient::with_shared_transport_and_cache(adapter, timeouts, cache)?;
    let prepared = client.prepare_ephemeral_messages(&[ProviderMessage::user(prompt)])?;
    let mut out = String::new();
    client
        .stream_prepared_cancellable(prepared, cancel.cancelled(), |event| {
            if let ProviderEvent::TextDelta(delta) = event {
                out.push_str(&delta);
            }
        })
        .await?;
    Ok(out)
}

/// Runs a zero-overhead single-turn completion directly against the provider
/// adapter: no LSP, no MCP servers, no repo_map, no tool schemas, no cache writes.
pub fn run_fast_micro_completion(
    req: FastMicroRequest,
    cancel: CancellationToken,
) -> Result<String, ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    let runtime = shared_tui_runtime().map_err(|message| ProviderError::InvalidResponse {
        message: format!("shared runtime unavailable: {message}"),
    })?;
    let max_output_tokens = if req.max_output_tokens == 0 {
        DEFAULT_MAX_OUTPUT_TOKENS
    } else {
        req.max_output_tokens
    };
    let system_prompt = req
        .system_prompt
        .unwrap_or_else(|| DEFAULT_MICRO_SYSTEM_PROMPT.to_owned());

    runtime.block_on(async move {
        let model = crate::canonical_provider_model(req.provider, &req.model);
        let endpoint = req.endpoint;
        let api_key = req.api_key;
        let prompt = req.prompt;
        let timeout = req.timeout;

        match req.provider {
            ProviderKind::OpenAiCompatible => {
                let config = ProviderConfig::openai(endpoint, model, api_key)
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                let adapter = OpenAiCompatibleAdapter::new(config)?;
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::OpenAiCodex => {
                let account_id = req.account_id.ok_or_else(|| ProviderError::InvalidResponse {
                    message: "Codex OAuth account id is required".into(),
                })?;
                let config = ProviderConfig::openai_codex(endpoint, model, api_key, account_id)
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                let adapter = OpenAiCodexAdapter::new(config)?;
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::Anthropic => {
                let mut config = if req.oauth {
                    ProviderConfig::anthropic_oauth(endpoint, model, api_key)
                } else {
                    ProviderConfig::anthropic(endpoint, model, api_key)
                };
                config = config
                    .with_max_output_tokens(max_output_tokens)
                    .with_thinking_display(ThinkingDisplay::Omitted)
                    .with_system_prompt(system_prompt);
                let adapter = AnthropicAdapter::new(config)?;
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::OpenCodeGo => {
                let adapter = OpenCodeGoAdapter::new(&endpoint, &model, &api_key, None)?
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::OpenCodeZen => {
                let adapter = OpenCodeZenAdapter::new(&endpoint, &model, &api_key, None)?
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::ClinePass => {
                let adapter = ClinePassAdapter::new(&endpoint, &model, &api_key, None)?
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::CommandCode => {
                let adapter = CommandCodeAdapter::new(&endpoint, &model, &api_key, None)?
                    .with_max_output_tokens(max_output_tokens)
                    .with_zero_data_retention(command_code_zero_data_retention())
                    .with_system_prompt(system_prompt);
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::Xai => {
                let adapter = XaiAdapter::new(&endpoint, &model, &api_key, None)?
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::OpenDesign => {
                let adapter = OpenDesignAdapter::new(&endpoint, &model, &api_key, None)?
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
            ProviderKind::Devin => {
                let mut adapter = DevinAdapter::new(&endpoint, &model, &api_key, None)?
                    .with_max_output_tokens(max_output_tokens)
                    .with_system_prompt(system_prompt);
                if let Some(entry) = devin_entry(&model) {
                    adapter = adapter.with_catalog_entry(&entry)?;
                }
                execute_adapter(adapter, &prompt, timeout, cancel).await
            }
        }
    })
}
