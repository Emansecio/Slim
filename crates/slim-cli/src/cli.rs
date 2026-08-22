use std::time::Duration;

use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;

use crate::{
    load_local_images, redact, render_jsonl, render_provider_jsonl, render_provider_text,
    render_text, resolve_api_key, run_fake_headless, run_provider_headless_with_options,
    run_provider_headless_with_session_and_options, ExitCode, HeadlessRequest, OutputFormat,
    ProviderRequest, ProviderRunOptions,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliOutput {
    pub code: ExitCode,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) struct ParsedArgs {
    pub mode: OperatingMode,
    pub format: OutputFormat,
    pub prompt: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub session_path: Option<String>,
    pub image_paths: Vec<String>,
    pub positional: Vec<String>,
    pub tui: bool,
    pub headless: bool,
}

pub(crate) fn parse_cli_args(args: &[String]) -> Result<ParsedArgs, CliOutput> {
    let mut parsed = ParsedArgs {
        mode: OperatingMode::Auto,
        format: OutputFormat::Text,
        prompt: None,
        provider: None,
        model: None,
        endpoint: None,
        session_path: None,
        image_paths: Vec::new(),
        positional: Vec::new(),
        tui: false,
        headless: false,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--tui" => parsed.tui = true,
            "--headless" => parsed.headless = true,
            "--plan" => parsed.mode = OperatingMode::Plan,
            "--read-only" => parsed.mode = OperatingMode::ReadOnly,
            "--jsonl" => parsed.format = OutputFormat::Jsonl,
            "--prompt" | "--provider" | "--model" | "--endpoint" | "--session" | "--image" => {
                let option = args[index].clone();
                index += 1;
                let value = args.get(index).cloned().ok_or_else(|| {
                    failure(ExitCode::Internal, &format!("missing value for {option}\n"))
                })?;
                match option.as_str() {
                    "--prompt" => parsed.prompt = Some(value),
                    "--provider" => parsed.provider = Some(value),
                    "--model" => parsed.model = Some(value),
                    "--endpoint" => parsed.endpoint = Some(value),
                    "--session" => parsed.session_path = Some(value),
                    "--image" => parsed.image_paths.push(value),
                    _ => unreachable!(),
                }
            }
            value if value.starts_with('-') => {
                return Err(failure(
                    ExitCode::Internal,
                    &format!("unknown option: {value}\n"),
                ))
            }
            value => parsed.positional.push(value.to_owned()),
        }
        index += 1;
    }
    Ok(parsed)
}

pub fn run_cli<I, S>(args: I, stdin: &str) -> CliOutput
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return success(
            "Slim coding agent\nUsage: Slim [TUI OPTIONS]\n       Slim --headless [--plan|--read-only|--jsonl|--provider NAME|--model MODEL|--endpoint URL|--session PATH|--image PATH|--prompt TEXT]\n",
        );
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        return success("slim 0.1.0\n");
    }

    let ParsedArgs {
        mode,
        format,
        prompt,
        provider,
        model,
        endpoint,
        session_path,
        image_paths,
        positional,
        tui: _,
        headless: _,
    } = match parse_cli_args(&args) {
        Ok(parsed) => parsed,
        Err(output) => return output,
    };
    let prompt = prompt.unwrap_or_else(|| {
        if positional.is_empty() {
            stdin.to_owned()
        } else {
            positional.join(" ")
        }
    });

    let environment_provider = std::env::var("SLIM_PROVIDER").ok();
    let provider_requested = provider.is_some()
        || endpoint.is_some()
        || model.is_some()
        || session_path.is_some()
        || !image_paths.is_empty()
        || environment_provider.is_some();
    if provider_requested {
        let provider_name = provider
            .or(environment_provider)
            .unwrap_or_else(|| "openai-compatible".into());
        let kind = match provider_name.to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "openai_compatible" => ProviderKind::OpenAiCompatible,
            "openai-codex" | "codex" => ProviderKind::OpenAiCodex,
            "anthropic" | "claude" => ProviderKind::Anthropic,
            _ => {
                return failure(
                    ExitCode::Provider,
                    "unsupported provider; use openai-compatible, openai-codex, or anthropic\n",
                )
            }
        };
        let default_endpoint = match kind {
            ProviderKind::OpenAiCompatible => "https://api.openai.com/v1/chat/completions",
            ProviderKind::OpenAiCodex => "https://chatgpt.com/backend-api",
            ProviderKind::Anthropic => "https://api.anthropic.com/v1/messages",
        };
        let default_model = match kind {
            ProviderKind::OpenAiCompatible => "gpt-4o-mini",
            ProviderKind::OpenAiCodex => "gpt-5.3-codex",
            ProviderKind::Anthropic => "claude-3-5-sonnet-latest",
        };
        let layered_config = match crate::config::load_layered() {
            Ok(config) => config,
            Err(error) => {
                return failure(ExitCode::Internal, &format!("config error: {error}\n"))
            }
        };
        let api_key = match resolve_api_key(kind) {
            Ok(Some(api_key)) => api_key,
            Ok(None) => {
                return failure(
                    ExitCode::Auth,
                    "provider selected but no API key was found (SLIM_API_KEY, provider-specific key, or auth.json)\n",
                )
            }
            Err(error) => {
                return failure(ExitCode::Auth, &format!("provider auth configuration invalid: {error}\n"))
            }
        };
        let content_blocks = match load_local_images(&image_paths) {
            Ok(blocks) => blocks,
            Err(error) => return failure(ExitCode::InputRequired, &format!("image: {error}\n")),
        };
        let request = ProviderRequest {
            prompt,
            mode,
            kind,
            endpoint: endpoint
                .or_else(|| std::env::var("SLIM_ENDPOINT").ok())
                .or(layered_config.endpoint)
                .unwrap_or_else(|| default_endpoint.into()),
            model: model
                .or_else(|| std::env::var("SLIM_MODEL").ok())
                .or(layered_config.model)
                .unwrap_or_else(|| default_model.into()),
            api_key,
            account_id: None,
            timeout: Duration::from_secs(120),
        };
        // Reasoning effort: CLI/TUI env override first, then slim.toml
        // (global + project layers). Empty string means "unset".
        let effort = std::env::var("SLIM_EFFORT")
            .ok()
            .filter(|value| !value.is_empty())
            .or(layered_config.effort);
        let mut options = ProviderRunOptions::default().with_content_blocks(content_blocks);
        if let Some(effort) = effort {
            options = options.with_reasoning_effort(effort);
        }
        let provider_result = match session_path {
            Some(path) => run_provider_headless_with_session_and_options(request, path, options),
            None => run_provider_headless_with_options(request, options),
        };
        let result = match provider_result {
            Ok(result) => result,
            Err(error) => return provider_failure(error),
        };
        let stdout = match format {
            OutputFormat::Text => render_provider_text(&result),
            OutputFormat::Jsonl => match render_provider_jsonl(&result) {
                Ok(output) => output,
                Err(error) => {
                    return failure(ExitCode::Internal, &format!("render error: {error}\n"))
                }
            },
        };
        return CliOutput {
            code: result.code,
            stdout,
            stderr: String::new(),
        };
    }

    let result = run_fake_headless(HeadlessRequest { prompt, mode });
    let stdout = match format {
        OutputFormat::Text => render_text(&result),
        OutputFormat::Jsonl => match render_jsonl(&result) {
            Ok(output) => output,
            Err(error) => return failure(ExitCode::Internal, &format!("render error: {error}\n")),
        },
    };
    CliOutput {
        code: result.code,
        stdout,
        stderr: String::new(),
    }
}

fn success(stdout: &str) -> CliOutput {
    CliOutput {
        code: ExitCode::Success,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

fn failure(code: ExitCode, stderr: &str) -> CliOutput {
    CliOutput {
        code,
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

fn provider_failure(error: slim_core::ProviderError) -> CliOutput {
    let (code, message) = match error {
        slim_core::ProviderError::Cancelled => {
            (ExitCode::Cancelled, "provider request cancelled".into())
        }
        slim_core::ProviderError::Transport { .. } => {
            (ExitCode::Provider, "provider transport failed".into())
        }
        slim_core::ProviderError::MalformedToolCall => (
            ExitCode::Provider,
            "provider returned a malformed tool call".into(),
        ),
        slim_core::ProviderError::Remote { message }
        | slim_core::ProviderError::InvalidResponse { message } => (
            ExitCode::Provider,
            format!("provider error: {}", redact(&message)),
        ),
    };
    failure(code, &format!("{message}\n"))
}
