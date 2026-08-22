use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use slim_core::provider::{
    AnthropicAdapter, HttpProviderClient, OpenAiCodexAdapter, OpenAiCompatibleAdapter,
    ProviderConfig, ProviderContentBlock, ProviderError, ProviderKind, ProviderPricing,
};
use slim_core::runtime::{AgentLoopConfig, AgentLoopStop, CancellationToken};
use slim_core::session::SessionWriter;
use slim_core::{EventKind, OperatingMode, ProviderMessage, Runtime, SessionEvent};

use crate::exit_codes::ExitCode;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Text,
    Jsonl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessRequest {
    pub prompt: String,
    pub mode: OperatingMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessResult {
    pub code: ExitCode,
    pub message: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderRequest {
    pub prompt: String,
    pub mode: OperatingMode,
    pub kind: ProviderKind,
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub account_id: Option<String>,
    pub timeout: Duration,
}

/// Maximum size accepted for one local CLI image: 20 MiB.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderRunOptions {
    pub content_blocks: Vec<ProviderContentBlock>,
    pub workspace_root: Option<PathBuf>,
    pub artifact_root: Option<PathBuf>,
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub max_turns: Option<usize>,
    pub max_tool_calls: Option<usize>,
    pub cancellation: Option<CancellationToken>,
}

impl ProviderRunOptions {
    pub fn with_content_blocks(mut self, blocks: Vec<ProviderContentBlock>) -> Self {
        self.content_blocks = blocks;
        self
    }

    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    pub fn with_artifact_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.artifact_root = Some(root.into());
        self
    }

    pub fn with_context_window_tokens(mut self, tokens: u64) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    pub fn with_max_turns(mut self, turns: usize) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn with_max_tool_calls(mut self, calls: usize) -> Self {
        self.max_tool_calls = Some(calls);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderHeadlessResult {
    pub code: ExitCode,
    pub provider: ProviderKind,
    pub model: String,
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub stop_reason: Option<String>,
    pub stop: String,
    pub cost_micros: Option<u64>,
}

pub(crate) struct ProviderExecution {
    pub result: ProviderHeadlessResult,
    pub events: Vec<SessionEvent>,
}

#[derive(Serialize)]
struct JsonlResult<'a> {
    version: u8,
    kind: &'a str,
}

pub fn run_fake_headless(request: HeadlessRequest) -> HeadlessResult {
    if request.prompt.trim().is_empty() {
        return HeadlessResult {
            code: ExitCode::InputRequired,
            message: "input_required".into(),
        };
    }
    if request.mode == OperatingMode::Plan {
        return HeadlessResult {
            code: ExitCode::ApprovalRequired,
            message: "approval_required".into(),
        };
    }
    HeadlessResult {
        code: ExitCode::Success,
        message: "success".into(),
    }
}

pub fn run_provider_headless(
    request: ProviderRequest,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_with_options(request, ProviderRunOptions::default())
}

pub fn run_provider_headless_with_options(
    request: ProviderRequest,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_inner(request, None, options)
}

pub fn run_provider_headless_with_session(
    request: ProviderRequest,
    session_path: impl AsRef<Path>,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_with_session_and_options(
        request,
        session_path,
        ProviderRunOptions::default(),
    )
}

pub fn run_provider_headless_with_session_and_options(
    request: ProviderRequest,
    session_path: impl AsRef<Path>,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_inner(request, Some(session_path.as_ref()), options)
}

fn run_provider_headless_inner(
    request: ProviderRequest,
    session_path: Option<&Path>,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    execute_provider_turn(request, session_path, options).map(|execution| execution.result)
}

pub(crate) fn execute_provider_turn(
    request: ProviderRequest,
    session_path: Option<&Path>,
    options: ProviderRunOptions,
) -> Result<ProviderExecution, ProviderError> {
    let tokio_runtime =
        tokio::runtime::Runtime::new().map_err(|error| ProviderError::InvalidResponse {
            message: format!("runtime: {error}"),
        })?;
    tokio_runtime.block_on(execute_provider_turn_async(
        request,
        session_path.map(Path::to_path_buf),
        options,
        None,
    ))
}

pub(crate) async fn execute_provider_turn_async(
    request: ProviderRequest,
    session_path: Option<PathBuf>,
    options: ProviderRunOptions,
    event_sender: Option<std::sync::mpsc::Sender<SessionEvent>>,
) -> Result<ProviderExecution, ProviderError> {
    if request.prompt.trim().is_empty() {
        return Ok(ProviderExecution {
            result: ProviderHeadlessResult {
                code: ExitCode::InputRequired,
                provider: request.kind,
                model: request.model,
                text: "input_required".into(),
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                stop: "input_required".into(),
                cost_micros: None,
            },
            events: Vec::new(),
        });
    }
    if request.mode == OperatingMode::Plan {
        return Ok(ProviderExecution {
            result: ProviderHeadlessResult {
                code: ExitCode::ApprovalRequired,
                provider: request.kind,
                model: request.model,
                text: "approval_required".into(),
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                stop: "approval_required".into(),
                cost_micros: None,
            },
            events: Vec::new(),
        });
    }

    let context_window_tokens = resolve_context_window_tokens(options.context_window_tokens)?;
    let max_output_tokens = resolve_max_output_tokens(options.max_output_tokens)?;
    let reasoning_effort = options.reasoning_effort.clone().unwrap_or_default();

    let provider = request.kind;
    let model = request.model.clone();
    let api_key = request.api_key.clone();
    let cancellation = options.cancellation.clone();
    let cwd = options
        .workspace_root
        .unwrap_or(
            std::env::current_dir().map_err(|error| ProviderError::InvalidResponse {
                message: format!("current directory: {error}"),
            })?,
        );
    let artifact_root = options
        .artifact_root
        .unwrap_or_else(|| cwd.join(".slim").join("artifacts"));
    let mut runtime = Runtime::with_artifact_store(&artifact_root).map_err(|error| {
        ProviderError::InvalidResponse {
            message: format!("artifact store: {error}"),
        }
    })?;
    if let Some(sender) = event_sender {
        runtime.app.set_event_sender(sender);
    }
    if let Some(cancellation) = cancellation {
        runtime.set_cancellation_token(cancellation);
    }
    runtime.register_sensitive_value(&request.api_key);
    let initial_message =
        ProviderMessage::user(request.prompt.clone()).with_content_blocks(options.content_blocks);
    let mut loop_config = AgentLoopConfig {
        context_window_tokens,
        context_reserve_tokens: max_output_tokens as u64,
        ..AgentLoopConfig::default()
    };
    if let Some(max_turns) = options.max_turns {
        loop_config.max_turns = max_turns;
    }
    if let Some(max_tool_calls) = options.max_tool_calls {
        loop_config.max_tool_calls = max_tool_calls;
    }
    let loop_result = match provider {
        ProviderKind::OpenAiCompatible => {
            let adapter = OpenAiCompatibleAdapter::new(
                ProviderConfig::openai(request.endpoint, request.model, api_key.clone())
                    .with_max_output_tokens(max_output_tokens)
                    .with_reasoning_effort(reasoning_effort.clone()),
            )?;
            let client = HttpProviderClient::new(adapter, request.timeout)?;
            runtime
                .run_agent_loop_with_message(
                    &client,
                    initial_message.clone(),
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
        ProviderKind::OpenAiCodex => {
            let account_id =
                request
                    .account_id
                    .clone()
                    .ok_or_else(|| ProviderError::InvalidResponse {
                        message: "Codex OAuth account id is required".into(),
                    })?;
            let adapter = OpenAiCodexAdapter::new(
                ProviderConfig::openai_codex(
                    request.endpoint,
                    request.model,
                    api_key.clone(),
                    account_id,
                )
                .with_max_output_tokens(max_output_tokens)
                .with_reasoning_effort(reasoning_effort),
            )?;
            let client = HttpProviderClient::new(adapter, request.timeout)?;
            runtime
                .run_agent_loop_with_message(
                    &client,
                    initial_message.clone(),
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
        ProviderKind::Anthropic => {
            let config = if request.account_id.is_some() {
                ProviderConfig::anthropic_oauth(request.endpoint, request.model, api_key.clone())
            } else {
                ProviderConfig::anthropic(request.endpoint, request.model, api_key.clone())
            };
            let adapter = AnthropicAdapter::new(config.with_max_output_tokens(max_output_tokens))?;
            let client = HttpProviderClient::new(adapter, request.timeout)?;
            runtime
                .run_agent_loop_with_message(
                    &client,
                    initial_message,
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
    }
    .map_err(|error| redact_provider_error(error, &api_key))?;

    let events = runtime.app.drain_events();
    if let Some(path) = session_path {
        let cwd_text = cwd.display().to_string();
        let session_id = format!("slim-{}-{}", std::process::id(), next_session_suffix());
        let mut writer = SessionWriter::create(&path, &session_id, &cwd_text).map_err(|error| {
            ProviderError::InvalidResponse {
                message: format!("session: {error}"),
            }
        })?;
        for event in &events {
            writer
                .append(event)
                .map_err(|error| ProviderError::InvalidResponse {
                    message: format!("session: {error}"),
                })?;
        }
    }

    let mut text = String::new();
    let mut tool_text = Vec::new();
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    let mut stop_reason = None;
    for event in &events {
        match &event.kind {
            EventKind::AssistantTextDelta { text: delta } => text.push_str(delta),
            EventKind::ToolOutput { name, output } => {
                tool_text.push(format!("tool {name}: {output}"))
            }
            EventKind::Usage {
                input_tokens: input,
                output_tokens: output,
            } => {
                input_tokens = Some(input_tokens.unwrap_or_default().saturating_add(*input));
                output_tokens = Some(output_tokens.unwrap_or_default().saturating_add(*output));
            }
            EventKind::AssistantEnded { reason } => stop_reason = Some(reason.clone()),
            _ => {}
        }
    }
    if text.is_empty() {
        text = tool_text.join("\n");
    }
    let cost_micros = resolve_pricing().map(|pricing| {
        pricing.cost_micros(
            input_tokens.unwrap_or_default() as u64,
            output_tokens.unwrap_or_default() as u64,
        )
    });
    let stop = stop_name(loop_result.stop).to_owned();
    let code = exit_code_for_stop(loop_result.stop);
    text = runtime.redact_sensitive(&text);
    Ok(ProviderExecution {
        result: ProviderHeadlessResult {
            code,
            provider,
            model,
            text,
            input_tokens,
            output_tokens,
            stop_reason,
            stop,
            cost_micros,
        },
        events,
    })
}

fn next_session_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

pub fn render_text(result: &HeadlessResult) -> String {
    format!("{}\n", result.message)
}

pub fn render_jsonl(result: &HeadlessResult) -> Result<String, serde_json::Error> {
    let line = serde_json::to_string(&JsonlResult {
        version: 1,
        kind: &result.message,
    })?;
    Ok(format!("{line}\n"))
}

#[derive(Serialize)]
struct ProviderJsonlResult<'a> {
    version: u8,
    kind: &'a str,
    provider: &'a str,
    model: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<&'a str>,
    stop: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_micros: Option<u64>,
}

pub fn render_provider_text(result: &ProviderHeadlessResult) -> String {
    format!("stop={}\n{}\n", result.stop, result.text)
}

pub fn render_provider_jsonl(result: &ProviderHeadlessResult) -> Result<String, serde_json::Error> {
    let provider = match result.provider {
        ProviderKind::OpenAiCompatible => "openai-compatible",
        ProviderKind::OpenAiCodex => "openai-codex",
        ProviderKind::Anthropic => "anthropic",
    };
    let kind = match result.code {
        ExitCode::Success => "assistant",
        ExitCode::ApprovalRequired => "approval_required",
        ExitCode::InputRequired => "input_required",
        _ => "provider_result",
    };
    let line = serde_json::to_string(&ProviderJsonlResult {
        version: 1,
        kind,
        provider,
        model: &result.model,
        text: &result.text,
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
        stop_reason: result.stop_reason.as_deref(),
        stop: &result.stop,
        cost_micros: result.cost_micros,
    })?;
    Ok(format!("{line}\n"))
}

pub fn load_local_images(paths: &[String]) -> Result<Vec<ProviderContentBlock>, String> {
    paths
        .iter()
        .map(|path| load_local_image(Path::new(path)))
        .collect()
}

fn load_local_image(path: &Path) -> Result<ProviderContentBlock, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("image cannot be read: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("image must be a regular non-symlink file".into());
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image exceeds the {} MiB limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let media_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Err("image extension must be png, jpg, jpeg, gif, or webp".into()),
    };
    let file =
        std::fs::File::open(path).map_err(|error| format!("image cannot be read: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("image cannot be read: {error}"))?;
    if bytes.is_empty() {
        return Err("image cannot be empty".into());
    }
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!(
            "image exceeds the {} MiB limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok(ProviderContentBlock::image(
        media_type,
        encode_base64(&bytes),
    ))
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        output.push(ALPHABET[(first >> 2) as usize] as char);
        if chunk.len() == 1 {
            output.push(ALPHABET[((first & 0x03) << 4) as usize] as char);
            output.push_str("==");
        } else {
            let second = chunk[1];
            output.push(ALPHABET[((first & 0x03) << 4 | second >> 4) as usize] as char);
            if chunk.len() == 2 {
                output.push(ALPHABET[((second & 0x0f) << 2) as usize] as char);
                output.push('=');
            } else {
                let third = chunk[2];
                output.push(ALPHABET[((second & 0x0f) << 2 | third >> 6) as usize] as char);
                output.push(ALPHABET[(third & 0x3f) as usize] as char);
            }
        }
    }
    output
}

fn resolve_context_window_tokens(explicit: Option<u64>) -> Result<u64, ProviderError> {
    if let Some(value) = explicit {
        return (value > 0)
            .then_some(value)
            .ok_or_else(|| ProviderError::InvalidResponse {
                message: "context window tokens must be positive".into(),
            });
    }
    parse_positive_env_u64("SLIM_CONTEXT_WINDOW_TOKENS").map_or_else(
        |message| Err(ProviderError::InvalidResponse { message }),
        |value| Ok(value.unwrap_or(AgentLoopConfig::default().context_window_tokens)),
    )
}

fn resolve_max_output_tokens(explicit: Option<u32>) -> Result<u32, ProviderError> {
    if let Some(value) = explicit {
        return (value > 0)
            .then_some(value)
            .ok_or_else(|| ProviderError::InvalidResponse {
                message: "max output tokens must be positive".into(),
            });
    }
    parse_positive_env_u64("SLIM_MAX_OUTPUT_TOKENS")
        .map_err(|message| ProviderError::InvalidResponse { message })
        .and_then(|value| {
            let value = value.unwrap_or(slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS as u64);
            u32::try_from(value).map_err(|_| ProviderError::InvalidResponse {
                message: "SLIM_MAX_OUTPUT_TOKENS must fit in a positive 32-bit integer".into(),
            })
        })
}

fn parse_positive_env_u64(name: &str) -> Result<Option<u64>, String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    let parsed = value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))?;
    Ok(Some(parsed))
}

fn stop_name(stop: AgentLoopStop) -> &'static str {
    match stop {
        AgentLoopStop::ProviderCompleted => "provider_completed",
        AgentLoopStop::TurnLimit => "turn_limit",
        AgentLoopStop::ToolLimit => "tool_limit",
        AgentLoopStop::RepeatedFailedTool => "repeated_failed_tool",
    }
}

fn exit_code_for_stop(stop: AgentLoopStop) -> ExitCode {
    match stop {
        AgentLoopStop::ProviderCompleted => ExitCode::Success,
        AgentLoopStop::TurnLimit | AgentLoopStop::RepeatedFailedTool => ExitCode::Blocked,
        AgentLoopStop::ToolLimit => ExitCode::Tool,
    }
}

fn redact_provider_error(error: ProviderError, secret: &str) -> ProviderError {
    let redact = |message: String| {
        if secret.is_empty() {
            message
        } else {
            message.replace(secret, "[REDACTED]")
        }
    };
    match error {
        ProviderError::Remote { message } => ProviderError::Remote {
            message: redact(message),
        },
        ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
            message: redact(message),
        },
        error => error,
    }
}

fn resolve_pricing() -> Option<ProviderPricing> {
    let input = std::env::var("SLIM_INPUT_COST_MICROS_PER_MILLION")
        .ok()?
        .parse()
        .ok()?;
    let output = std::env::var("SLIM_OUTPUT_COST_MICROS_PER_MILLION")
        .ok()?
        .parse()
        .ok()?;
    Some(ProviderPricing {
        input_micros_per_million: input,
        output_micros_per_million: output,
    })
}
