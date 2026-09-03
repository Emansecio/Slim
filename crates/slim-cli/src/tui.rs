use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slim_core::provider::{
    clinepass_model, fetch_clinepass_catalog, is_clinepass_model_id, is_command_code_model_id,
    open_code_model, ProviderKind, CLINEPASS_BASE_URL, CLINEPASS_DEFAULT_MODEL,
    COMMANDCODE_BASE_URL, COMMANDCODE_DEFAULT_MODEL, OPENCODE_GO_BASE_URL,
    OPENCODE_GO_DEFAULT_MODEL,
};
use slim_core::runtime::CancellationToken;
use slim_core::session::{DurableSessionHeader, JsonlRepo, SessionFormat, SessionPreflight};
use slim_core::{
    interaction_route, EventKind, InteractionRequestId as CoreInteractionRequestId,
    InteractionResponder, ProviderError, ProviderMessage, SessionEvent, SessionEventSender,
};
use slim_tui::api::{
    ClinePassCatalogSource, CommandCodeCatalogSource, ContentHandle, ContentRequestId,
    InteractionRequestId, LoginProvider, ModelAlias, OpenCodeCatalogSource, OpenCodeModelView,
    PageCursor, ReasoningEffort, ToolBatchId, ToolCallId, TranscriptMessage, TranscriptRole,
    UiChannels, UiCommand, UiEvent, WakeSignal,
};

use crate::cli::parse_cli_args;
use crate::command_code_catalog::CommandCodeCatalog;
use crate::exit_codes::ExitCode;
use crate::headless::{
    execute_provider_turn, execute_provider_turn_async, format_run_stop_message,
    resolve_max_mutating_tool_calls, resolve_max_read_tool_calls, resolve_max_turns,
    resolve_timeout_secs, resume_messages_from_preflight,
    run_provider_resume_with_preflight_events_interactive_async, OutputFormat, ProviderExecution,
    ProviderRequest, ProviderRunOptions, SkillInstructions, ToolLoopLimits,
    MAX_SLASH_SKILL_BODY_BYTES,
};
use crate::oauth::{OAuthCredential, OAuthError, OAuthProgress, OAuthProvider, OAuthService};
use crate::opencode_go_catalog::{CatalogSnapshot, CatalogSource, OpenCodeCatalog};
use crate::{delete_api_key, load_local_images, resolve_provider_credential, save_api_key};

pub struct TuiRuntimeHandle {
    shutdown: Option<mpsc::Sender<UiCommand>>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiError {
    code: ExitCode,
    message: String,
}

impl TuiError {
    fn new(code: ExitCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> ExitCode {
        self.code
    }
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TuiError {}

struct TuiStartup {
    request: Option<ProviderRequest>,
    oauth_session: Option<(OAuthProvider, OAuthCredential)>,
    options: ProviderRunOptions,
    initial_prompt: Option<String>,
    image_labels: Vec<String>,
    resume_path: Option<PathBuf>,
    resume_preflight: Option<SessionPreflight>,
    persist_sessions: bool,
    mode: slim_core::OperatingMode,
    effort: ReasoningEffort,
    endpoint_override: Option<String>,
    model_override: Option<String>,
    timeout: Duration,
}

static NEXT_TUI_SESSION_SUFFIX: AtomicU64 = AtomicU64::new(1);

struct SelectedTuiSession {
    preflight: SessionPreflight,
    history: Vec<ProviderMessage>,
}

enum SlashSkillCommand {
    Selected {
        name: String,
    },
    Invoke {
        name: String,
        body: String,
        source: PathBuf,
    },
}

fn valid_slash_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn resolve_slash_skill_command(
    workspace_root: &Path,
    prompt: &str,
) -> Result<Option<SlashSkillCommand>, String> {
    let trimmed = prompt.trim();
    let mut candidates = trimmed
        .split_whitespace()
        .filter_map(|token| {
            let name = token.strip_prefix('/')?;
            (valid_slash_skill_name(name) && !slim_tui::reducer::is_native_slash_command(name))
                .then_some((token, name))
        })
        .peekable();
    if candidates.peek().is_none() {
        return Ok(None);
    }
    let discovery = slim_core::skills::discover_workspace(workspace_root)
        .map_err(|error| format!("skill discovery: {error}"))?;
    let Some((token, name, entry)) = candidates
        .find_map(|(token, name)| discovery.active(name).map(|entry| (token, name, entry)))
    else {
        return Ok(None);
    };
    if trimmed == token {
        return Ok(Some(SlashSkillCommand::Selected {
            name: name.to_owned(),
        }));
    }
    let body = slim_core::skills::read_body(entry.path.join("SKILL.md"))
        .map_err(|error| format!("skill /{name}: {error}"))?;
    if body.trim().is_empty() {
        return Err(format!("skill /{name} has no instructions"));
    }
    if body.len() > MAX_SLASH_SKILL_BODY_BYTES {
        return Err(format!(
            "skill /{name} instructions exceed the {MAX_SLASH_SKILL_BODY_BYTES}-byte slash limit"
        ));
    }
    Ok(Some(SlashSkillCommand::Invoke {
        name: name.to_owned(),
        body,
        source: entry.path.join("SKILL.md"),
    }))
}

fn workspace_sessions_dir(workspace_root: &Path, create: bool) -> Result<Option<PathBuf>, String> {
    let workspace_root =
        fs::canonicalize(workspace_root).map_err(|error| format!("workspace path: {error}"))?;
    let requested = workspace_root.join(".slim").join("sessions");
    if create {
        fs::create_dir_all(&requested)
            .map_err(|error| format!("create session directory: {error}"))?;
    } else if !requested.exists() {
        return Ok(None);
    }
    let sessions =
        fs::canonicalize(&requested).map_err(|error| format!("session directory: {error}"))?;
    if !sessions.starts_with(&workspace_root) {
        return Err("session directory resolves outside the workspace".into());
    }
    Ok(Some(sessions))
}

fn create_tui_session(startup: &mut TuiStartup) -> Result<Option<(String, String)>, String> {
    if !startup.persist_sessions || startup.resume_path.is_some() {
        return Ok(None);
    }
    let workspace = startup
        .options
        .workspace_root
        .as_deref()
        .ok_or("workspace root is unavailable")?;
    let canonical_workspace =
        fs::canonicalize(workspace).map_err(|error| format!("workspace path: {error}"))?;
    let cwd = canonical_workspace
        .to_str()
        .ok_or("workspace path is not valid Unicode")?
        .to_owned();
    let sessions = workspace_sessions_dir(&canonical_workspace, true)?
        .ok_or("session directory is unavailable")?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch")?
        .as_nanos();

    for _ in 0..16 {
        let suffix = NEXT_TUI_SESSION_SUFFIX.fetch_add(1, Ordering::Relaxed);
        let id = format!("tui-{timestamp}-{}-{suffix}", std::process::id());
        let path = sessions.join(format!("{id}.jsonl"));
        let header = DurableSessionHeader::new(&id, timestamp.to_string(), &cwd, None, None);
        match JsonlRepo::create(&path, header) {
            Ok(repo) => {
                drop(repo);
                let preflight = slim_core::session::preflight_session(&path)
                    .map_err(|error| format!("session preflight: {error}"))?;
                super::headless::ensure_resume_preflight(&preflight)
                    .map_err(|error| format!("session preflight: {error}"))?;
                startup.resume_path = Some(preflight.path.clone());
                startup.resume_preflight = Some(preflight);
                return Ok(Some((id, display_workspace_path(&canonical_workspace))));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create session: {error}")),
        }
    }
    Err("could not allocate a unique session file".into())
}

fn system_time_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn select_previous_tui_session(
    workspace_root: &Path,
    current_path: Option<&Path>,
) -> Result<Option<SelectedTuiSession>, String> {
    let canonical_workspace =
        fs::canonicalize(workspace_root).map_err(|error| format!("workspace path: {error}"))?;
    let Some(sessions) = workspace_sessions_dir(&canonical_workspace, false)? else {
        return Ok(None);
    };
    let current_path = current_path.and_then(|path| fs::canonicalize(path).ok());
    let mut candidates = Vec::new();

    for entry in fs::read_dir(&sessions).map_err(|error| format!("read sessions: {error}"))? {
        let entry = entry.map_err(|error| format!("read session entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("session entry type: {error}"))?;
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let canonical_path = match fs::canonicalize(&path) {
            Ok(path) if path.starts_with(&sessions) => path,
            _ => continue,
        };
        if current_path.as_ref() == Some(&canonical_path) {
            continue;
        }
        let preflight = match slim_core::session::preflight_session(&canonical_path) {
            Ok(preflight) => preflight,
            Err(_) => continue,
        };
        let Some(header) = preflight.header.as_ref() else {
            continue;
        };
        if preflight.format != Some(SessionFormat::DurableV2)
            || !header.id.starts_with("tui-")
            || canonical_path.file_stem().and_then(|stem| stem.to_str()) != Some(header.id.as_str())
        {
            continue;
        }
        let header_cwd = match fs::canonicalize(&header.cwd) {
            Ok(cwd) => cwd,
            Err(_) => continue,
        };
        if header_cwd != canonical_workspace {
            continue;
        }
        if preflight.can_resume_v2() && preflight.records.is_empty() {
            continue;
        }
        let created = header.timestamp.parse::<u128>().unwrap_or(0);
        let modified = fs::metadata(&canonical_path)
            .and_then(|metadata| metadata.modified())
            .map(system_time_nanos)
            .unwrap_or(created);
        candidates.push((
            modified,
            created,
            header.id.clone(),
            canonical_path,
            preflight,
        ));
    }

    candidates.sort_by(|left, right| {
        (&left.0, &left.1, &left.2, &left.3).cmp(&(&right.0, &right.1, &right.2, &right.3))
    });
    while let Some((_, _, _, _, preflight)) = candidates.pop() {
        super::headless::ensure_resume_preflight(&preflight)
            .map_err(|error| format!("previous session cannot be resumed: {error}"))?;
        slim_core::session::resume_plan_from_preflight(&preflight)
            .map_err(|error| format!("previous session cannot be resumed: {error}"))?;
        let history = resume_messages_from_preflight(&preflight).map_err(provider_error_message)?;
        let has_user = history.iter().any(|message| message.role == "user");
        let has_assistant = history.iter().any(|message| message.role == "assistant");
        if has_user && has_assistant && !preflight.summary.terminal_operation_ids.is_empty() {
            return Ok(Some(SelectedTuiSession { preflight, history }));
        }
    }
    Ok(None)
}

fn transcript_messages(history: &[ProviderMessage]) -> Vec<TranscriptMessage> {
    history
        .iter()
        .filter_map(|message| match message.role.as_str() {
            "user" => Some(TranscriptMessage {
                role: TranscriptRole::User,
                text: message.content.clone(),
            }),
            "assistant" => Some(TranscriptMessage {
                role: TranscriptRole::Assistant,
                text: message.content.clone(),
            }),
            _ => None,
        })
        .collect()
}

pub fn run_tui(args: Vec<String>) -> Result<(), TuiError> {
    let oauth = OAuthService::production()
        .map_err(|error| TuiError::new(ExitCode::Auth, error.to_string()))?;
    let startup = prepare_tui(args, &oauth)?;
    let initial_prompt = startup.initial_prompt.clone();
    let (runtime, channels) = spawn_tui_session(startup, oauth).map_err(tui_provider_error)?;
    if let Some(prompt) = initial_prompt {
        channels
            .commands
            .send(UiCommand::SendPrompt(prompt))
            .map_err(|_| TuiError::new(ExitCode::Internal, "TUI runtime disconnected"))?;
    }
    let result = slim_tui::run_app(channels)
        .map_err(|error| TuiError::new(ExitCode::Internal, error.to_string()));
    drop(runtime);
    result
}

fn prepare_tui(args: Vec<String>, oauth: &OAuthService) -> Result<TuiStartup, TuiError> {
    let parsed = parse_cli_args(&args)
        .map_err(|output| TuiError::new(output.code, output.stderr.trim_end().to_owned()))?;
    if parsed.format == OutputFormat::Jsonl {
        return Err(TuiError::new(
            ExitCode::Internal,
            "--jsonl is available only in headless mode",
        ));
    }
    if parsed.session_path.is_some() {
        return Err(TuiError::new(
            ExitCode::InputRequired,
            "TUI session resume/persistence is scheduled for the session milestone",
        ));
    }
    if parsed.recover_path.is_some() {
        return Err(TuiError::new(
            ExitCode::InputRequired,
            "--recover is available only in headless mode",
        ));
    }
    let (resume_path, resume_preflight) = if let Some(path) = parsed.resume_path.as_deref() {
        if parsed
            .prompt
            .as_deref()
            .or_else(|| (!parsed.positional.is_empty()).then_some("positional"))
            .is_none()
        {
            return Err(TuiError::new(
                ExitCode::InputRequired,
                "resume requires an explicit --prompt",
            ));
        }
        let preflight = slim_core::session::preflight_session(path).map_err(|error| {
            TuiError::new(ExitCode::Blocked, format!("durable resume: {error}"))
        })?;
        super::headless::ensure_resume_preflight(&preflight).map_err(|error| {
            TuiError::new(ExitCode::Blocked, format!("durable resume: {error}"))
        })?;
        slim_core::session::resume_plan_from_preflight(&preflight).map_err(|error| {
            TuiError::new(ExitCode::Blocked, format!("durable resume: {error}"))
        })?;
        (Some(preflight.path.clone()), Some(preflight))
    } else {
        (None, None)
    };
    let initial_prompt = parsed
        .prompt
        .or_else(|| (!parsed.positional.is_empty()).then(|| parsed.positional.join(" ")));
    let layered_config = crate::config::load_layered()
        .map_err(|error| TuiError::new(ExitCode::Internal, format!("config error: {error}")))?;
    let application_code_intelligence =
        crate::code_intel::build_code_intelligence(&layered_config.lsp);
    let timeout = resolve_timeout_secs(layered_config.timeout_secs).map_err(|error| {
        TuiError::new(
            ExitCode::InputRequired,
            match error {
                ProviderError::InvalidResponse { message } => message,
                other => format!("{other:?}"),
            },
        )
    })?;
    let compaction_policy = layered_config
        .compaction_policy()
        .map_err(|error| TuiError::new(ExitCode::Internal, format!("config error: {error}")))?;
    let endpoint_override = parsed
        .endpoint
        .or_else(|| std::env::var("SLIM_ENDPOINT").ok())
        .or(layered_config.endpoint);
    let model_override = parsed
        .model
        .or_else(|| std::env::var("SLIM_MODEL").ok())
        .or(layered_config.model);
    let configured_effort = std::env::var("SLIM_EFFORT")
        .ok()
        .and_then(|value| ReasoningEffort::parse(&value))
        .or_else(|| {
            layered_config
                .effort
                .as_deref()
                .and_then(ReasoningEffort::parse)
        });
    let mut effort = configured_effort.unwrap_or(ReasoningEffort::High);
    let explicit_provider = parsed
        .provider
        .or_else(|| std::env::var("SLIM_PROVIDER").ok());

    let (mut request, oauth_session) = if let Some(provider_name) = explicit_provider {
        let kind = provider_kind(&provider_name)?;
        let api_request = if kind != ProviderKind::OpenAiCodex {
            api_key_request(
                kind,
                parsed.mode,
                endpoint_override.as_deref(),
                model_override.as_deref(),
            )?
        } else {
            None
        };
        if api_request.is_some() {
            (api_request, None)
        } else if let Some((provider, credential)) = oauth
            .active()
            .map_err(tui_auth_error)?
            .filter(|(provider, _)| provider_kind_for_oauth(*provider) == kind)
        {
            let request = oauth_request(
                provider,
                &credential,
                parsed.mode,
                endpoint_override.as_deref(),
                model_override.as_deref(),
            )?;
            (Some(request), Some((provider, credential)))
        } else {
            (None, None)
        }
    } else if let Some(request) = api_key_request(
        ProviderKind::OpenAiCompatible,
        parsed.mode,
        endpoint_override.as_deref(),
        model_override.as_deref(),
    )? {
        (Some(request), None)
    } else {
        let active_provider = oauth.active_provider_key().map_err(tui_auth_error)?;
        let active_oauth = oauth.active().map_err(tui_auth_error)?;
        if let Some(provider_name) = active_provider {
            let kind = provider_kind(&provider_name)?;
            let api_request = if kind != ProviderKind::OpenAiCodex {
                api_key_request(
                    kind,
                    parsed.mode,
                    endpoint_override.as_deref(),
                    model_override.as_deref(),
                )?
            } else {
                None
            };
            if api_request.is_some() {
                (api_request, None)
            } else if let Some((provider, credential)) =
                active_oauth.filter(|(provider, _)| provider_kind_for_oauth(*provider) == kind)
            {
                let request = oauth_request(
                    provider,
                    &credential,
                    parsed.mode,
                    endpoint_override.as_deref(),
                    model_override.as_deref(),
                )?;
                (Some(request), Some((provider, credential)))
            } else {
                (None, None)
            }
        } else if let Some((provider, credential)) = active_oauth {
            let request = oauth_request(
                provider,
                &credential,
                parsed.mode,
                endpoint_override.as_deref(),
                model_override.as_deref(),
            )?;
            (Some(request), Some((provider, credential)))
        } else {
            (None, None)
        }
    };
    if let Some(request) = request.as_mut() {
        request.timeout = timeout;
    }

    if let Some(model) = request
        .as_ref()
        .filter(|request| request.kind == ProviderKind::OpenCodeGo)
        .and_then(|request| open_code_model(&request.model))
    {
        if !model.reasoning_levels.contains(&effort.id()) {
            if configured_effort.is_some() {
                return Err(TuiError::new(
                    ExitCode::InputRequired,
                    "configured effort is unsupported by the OpenCode Go model",
                ));
            }
            effort = model
                .reasoning_levels
                .iter()
                .find_map(|level| ReasoningEffort::parse(level))
                .unwrap_or(ReasoningEffort::High);
        }
    }
    let image_labels = parsed
        .image_paths
        .iter()
        .map(|path| {
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_owned()
        })
        .collect();
    let content_blocks = load_local_images(&parsed.image_paths)
        .map_err(|error| TuiError::new(ExitCode::InputRequired, format!("image: {error}")))?;
    let mut options = ProviderRunOptions::default()
        .with_content_blocks(content_blocks)
        .with_compaction_handle(slim_core::context::CompactionHandle::new(compaction_policy));
    if let Some(manager) = application_code_intelligence {
        options = options.with_code_intelligence(manager);
    }
    if let Some(calls) = layered_config.max_mutating_tool_calls {
        options = options.with_max_tool_calls(calls);
    }
    if let Some(calls) = layered_config.max_read_tool_calls {
        options = options.with_max_read_tool_calls(calls);
    }
    if let Some(turns) = layered_config.max_turns {
        options = options.with_max_turns(turns);
    }
    if let Some(tokens) = layered_config.max_output_tokens {
        options = options.with_max_output_tokens(tokens);
    }
    if let Some(bytes) = layered_config.max_result_bytes {
        options = options.with_max_result_bytes(bytes);
    }
    if configured_effort.is_some()
        && request.as_ref().is_some_and(|request| {
            matches!(
                request.kind,
                ProviderKind::OpenAiCodex
                    | ProviderKind::OpenCodeGo
                    | ProviderKind::ClinePass
                    | ProviderKind::CommandCode
            ) || ModelAlias::parse(&request.model).is_some()
        })
    {
        options = options.with_reasoning_effort(effort.id());
    }
    Ok(TuiStartup {
        request,
        oauth_session,
        options,
        initial_prompt,
        image_labels,
        resume_path,
        resume_preflight,
        persist_sessions: true,
        mode: parsed.mode,
        effort,
        endpoint_override,
        model_override,
        timeout,
    })
}

fn provider_kind(name: &str) -> Result<ProviderKind, TuiError> {
    match name.to_ascii_lowercase().as_str() {
        "openai" | "openai-compatible" | "openai_compatible" => Ok(ProviderKind::OpenAiCompatible),
        "openai-codex" | "codex" => Ok(ProviderKind::OpenAiCodex),
        "anthropic" | "claude" => Ok(ProviderKind::Anthropic),
        "opencode-go" | "opencode_go" | "go" => Ok(ProviderKind::OpenCodeGo),
        "clinepass" | "cline-pass" | "cp" => Ok(ProviderKind::ClinePass),
        "command-code" | "commandcode" | "cmd" => Ok(ProviderKind::CommandCode),
        _ => Err(TuiError::new(
            ExitCode::Provider,
            "unsupported provider; use openai-compatible, openai-codex, anthropic, opencode-go, clinepass, or command-code",
        )),
    }
}

#[cfg(test)]
pub(crate) fn defaults_for_test(kind: ProviderKind) -> (&'static str, &'static str) {
    defaults(kind)
}

fn defaults(kind: ProviderKind) -> (&'static str, &'static str) {
    (
        crate::cli::default_provider_endpoint(kind),
        crate::cli::default_provider_model(kind),
    )
}

fn activate_saved_api_key_provider(
    startup: &mut TuiStartup,
    oauth: &OAuthService,
    kind: ProviderKind,
    provider_key: &str,
    endpoint: &str,
    model: &str,
    label: &str,
) -> Result<bool, String> {
    if startup
        .request
        .as_ref()
        .is_some_and(|request| request.kind == kind)
    {
        return Ok(false);
    }
    let api_key = oauth
        .activate_api_key(provider_key)
        .map_err(|error| format!("Authentication: {error}"))?
        .ok_or_else(|| format!("{label} login is not saved. Use /login."))?;
    startup.request = Some(ProviderRequest {
        prompt: String::new(),
        mode: startup.mode,
        kind,
        endpoint: startup
            .endpoint_override
            .clone()
            .unwrap_or_else(|| endpoint.into()),
        model: model.into(),
        api_key,
        account_id: None,
        timeout: startup.timeout,
    });
    startup.oauth_session = None;
    Ok(true)
}

fn api_key_request(
    kind: ProviderKind,
    mode: slim_core::OperatingMode,
    endpoint: Option<&str>,
    model: Option<&str>,
) -> Result<Option<ProviderRequest>, TuiError> {
    let Some(credential) = resolve_provider_credential(kind).map_err(tui_auth_error)? else {
        return Ok(None);
    };
    if credential.oauth {
        return Ok(None);
    }
    let api_key = credential.access;
    let (default_endpoint, default_model) = defaults(kind);
    // G248: a configured model only applies when it belongs to this provider;
    // otherwise the provider default is used (no cross-kind leakage).
    let model = model
        .filter(|model| crate::provider_compatible_model(kind, model))
        .map(str::to_owned)
        .unwrap_or_else(|| default_model.to_owned());
    Ok(Some(ProviderRequest {
        prompt: String::new(),
        mode,
        kind,
        endpoint: endpoint.unwrap_or(default_endpoint).into(),
        model,
        api_key,
        account_id: None,
        timeout: Duration::from_secs(120),
    }))
}

fn tui_auth_error(error: impl fmt::Display) -> TuiError {
    TuiError::new(ExitCode::Auth, format!("authentication: {error}"))
}

fn oauth_request(
    provider: OAuthProvider,
    credential: &OAuthCredential,
    mode: slim_core::OperatingMode,
    endpoint: Option<&str>,
    model: Option<&str>,
) -> Result<ProviderRequest, TuiError> {
    let kind = provider_kind_for_oauth(provider);
    let (default_endpoint, default_model) = defaults(kind);
    // G248: a configured model only applies when it belongs to this provider.
    let model = model
        .filter(|model| crate::provider_compatible_model(kind, model))
        .map(str::to_owned)
        .unwrap_or_else(|| default_model.to_owned());
    let account_id =
        match provider {
            OAuthProvider::Anthropic => Some(String::new()),
            OAuthProvider::OpenAiCodex => Some(credential.account_id.clone().ok_or_else(|| {
                TuiError::new(ExitCode::Auth, "Codex OAuth account id is missing")
            })?),
        };
    Ok(ProviderRequest {
        prompt: String::new(),
        mode,
        kind,
        endpoint: endpoint.unwrap_or(default_endpoint).into(),
        model,
        api_key: credential.access.clone(),
        account_id,
        timeout: Duration::from_secs(120),
    })
}

fn provider_kind_for_oauth(provider: OAuthProvider) -> ProviderKind {
    match provider {
        OAuthProvider::Anthropic => ProviderKind::Anthropic,
        OAuthProvider::OpenAiCodex => ProviderKind::OpenAiCodex,
    }
}

impl Drop for TuiRuntimeHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(UiCommand::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn run_provider_tui_turn(
    request: ProviderRequest,
    options: ProviderRunOptions,
) -> Result<Vec<UiEvent>, ProviderError> {
    let prompt = redact_for_ui(&request.prompt, &request.api_key);
    let execution = execute_provider_turn(request, None, options)?;
    project_sync_tui_events(prompt, execution.events)
}

static NEXT_SYNC_TUI_RUN_ID: AtomicU64 = AtomicU64::new(1);

fn project_sync_tui_events(
    prompt: String,
    core_events: Vec<SessionEvent>,
) -> Result<Vec<UiEvent>, ProviderError> {
    let run_id = NEXT_SYNC_TUI_RUN_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| ProviderError::InvalidResponse {
            message: "synchronous TUI run identity exhausted".into(),
        })?;
    let scope = format!("sync-run-{run_id}");
    let mut events = Vec::with_capacity(core_events.len() + 1);
    events.push(UiEvent::UserMessageAdded { text: prompt });
    events.extend(
        core_events
            .into_iter()
            .filter_map(UiEvent::from_core)
            .map(|event| namespace_projected_ids(event, &scope)),
    );
    Ok(events)
}

pub fn spawn_tui_runtime(
    request: ProviderRequest,
    options: ProviderRunOptions,
) -> Result<(TuiRuntimeHandle, UiChannels), ProviderError> {
    let oauth = OAuthService::production().map_err(|error| ProviderError::InvalidResponse {
        message: error.to_string(),
    })?;
    let effort = options
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .unwrap_or(ReasoningEffort::High);
    let timeout = request.timeout;
    spawn_tui_session(
        TuiStartup {
            mode: request.mode,
            effort,
            request: Some(request),
            oauth_session: None,
            options,
            initial_prompt: None,
            image_labels: Vec::new(),
            resume_path: None,
            resume_preflight: None,
            persist_sessions: false,
            endpoint_override: None,
            model_override: None,
            timeout,
        },
        oauth,
    )
}

/// Spawn the same TUI bridge with an explicit, preflighted durable resume.
/// This is the programmatic seam used by the CLI and offline bridge tests;
/// ordinary TUI callers retain the non-durable path above.
pub fn spawn_tui_runtime_with_resume(
    request: ProviderRequest,
    session_path: impl Into<PathBuf>,
    options: ProviderRunOptions,
) -> Result<(TuiRuntimeHandle, UiChannels), ProviderError> {
    let oauth = OAuthService::production().map_err(|error| ProviderError::InvalidResponse {
        message: error.to_string(),
    })?;
    let effort = options
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .unwrap_or(ReasoningEffort::High);
    let path = session_path.into();
    let preflight = slim_core::session::preflight_session(&path).map_err(|error| {
        ProviderError::InvalidResponse {
            message: format!("durable resume: {error}"),
        }
    })?;
    super::headless::ensure_resume_preflight(&preflight).map_err(|error| {
        ProviderError::InvalidResponse {
            message: format!("durable resume: {error}"),
        }
    })?;
    slim_core::session::resume_plan_from_preflight(&preflight).map_err(|error| {
        ProviderError::InvalidResponse {
            message: format!("durable resume: {error}"),
        }
    })?;
    let timeout = request.timeout;
    spawn_tui_session(
        TuiStartup {
            mode: request.mode,
            effort,
            request: Some(request),
            oauth_session: None,
            options,
            initial_prompt: None,
            image_labels: Vec::new(),
            resume_path: Some(preflight.path.clone()),
            resume_preflight: Some(preflight),
            persist_sessions: false,
            endpoint_override: None,
            model_override: None,
            timeout,
        },
        oauth,
    )
}

fn spawn_tui_session(
    mut startup: TuiStartup,
    oauth: OAuthService,
) -> Result<(TuiRuntimeHandle, UiChannels), ProviderError> {
    startup.options.ensure_shared_tool_registry();
    if startup.options.workspace_root.is_none() {
        startup.options.workspace_root =
            Some(
                std::env::current_dir().map_err(|error| ProviderError::InvalidResponse {
                    message: format!("current directory: {error}"),
                })?,
            );
    }
    let (command_tx, command_rx) = mpsc::channel();
    // Bounded lanes per spec §10.1: immediate control 256 and ordered stream
    // 1024. Both are lossless/blocking; adjacent deltas may coalesce downstream.
    let (control_tx, control_rx) = mpsc::sync_channel::<UiEvent>(256);
    let (data_tx, data_rx) = mpsc::sync_channel::<UiEvent>(1024);
    let wake = WakeSignal::new().map_err(|error| ProviderError::InvalidResponse {
        message: format!("TUI wake signal: {error}"),
    })?;
    let lane_space = WakeSignal::new().map_err(|error| ProviderError::InvalidResponse {
        message: format!("TUI lane space signal: {error}"),
    })?;
    let sink = EventSink {
        control: Some(control_tx),
        data: Some(data_tx),
        wake: wake.clone(),
        lane_space: lane_space.clone(),
        #[cfg(test)]
        drop_probe: None,
    };
    let worker = thread::Builder::new()
        .name("slim-tui-runtime".into())
        .spawn(move || run_worker(startup, oauth, command_rx, sink))
        .map_err(|error| ProviderError::InvalidResponse {
            message: format!("tui runtime thread: {error}"),
        })?;
    Ok((
        TuiRuntimeHandle {
            shutdown: Some(command_tx.clone()),
            worker: Some(worker),
        },
        UiChannels {
            commands: command_tx,
            wake,
            lane_space,
            events: control_rx,
            events_data: data_rx,
        },
    ))
}

fn image_model_error(request: Option<&ProviderRequest>) -> Option<String> {
    let request = request?;
    if request.kind != ProviderKind::OpenCodeGo {
        return None;
    }
    let model = open_code_model(&request.model)?;
    if model.accepts_images {
        None
    } else {
        Some(format!(
            "OpenCode Go model {} does not accept images",
            model.id
        ))
    }
}

/// Routes events into the two bounded lanes (§10.1). Both sends block when
/// full (lossless backpressure); the consumer coalesces data deltas through
/// the runtime coalescer, so no event is dropped here.
#[cfg(test)]
#[derive(Clone)]
struct DropProbe {
    reached_pre_wake: std::sync::Arc<std::sync::Barrier>,
    release_wake: std::sync::Arc<std::sync::Barrier>,
}

#[derive(Clone)]
struct EventSink {
    control: Option<mpsc::SyncSender<UiEvent>>,
    data: Option<mpsc::SyncSender<UiEvent>>,
    wake: WakeSignal,
    lane_space: WakeSignal,
    #[cfg(test)]
    drop_probe: Option<DropProbe>,
}

impl EventSink {
    fn control(&self) -> &mpsc::SyncSender<UiEvent> {
        self.control.as_ref().expect("live control sender")
    }

    fn data(&self) -> &mpsc::SyncSender<UiEvent> {
        self.data.as_ref().expect("live data sender")
    }

    fn send(&self, event: UiEvent) -> bool {
        if event.is_control() {
            return self.send_control(event);
        }
        if self.data().send(event).is_ok() {
            self.wake.notify();
            return true;
        }
        false
    }

    fn send_control(&self, event: UiEvent) -> bool {
        if self.control().send(event).is_ok() {
            self.wake.notify();
            return true;
        }
        false
    }

    fn try_send(&self, event: UiEvent) -> Result<(), mpsc::TrySendError<UiEvent>> {
        let result = if event.is_control() {
            self.control().try_send(event)
        } else {
            self.data().try_send(event)
        };
        if result.is_ok() {
            self.wake.notify();
        }
        result
    }

    /// Projected stream delivery is lossless while a run is active. After
    /// cancellation, visual deltas may be discarded to break backpressure.
    /// Accounting, interaction and tool boundaries migrate to control after
    /// cancellation. Tool progress is visual and may be discarded so a full
    /// stream lane cannot prevent the terminal outcome.
    fn send_projected(&self, mut event: UiEvent, cancellation: &CancellationToken) -> bool {
        let causal_telemetry = event.is_causal_telemetry();
        let survives_cancellation = event.survives_cancellation();
        loop {
            let cancelled = cancellation.is_cancelled();
            if cancelled && !survives_cancellation {
                // Drop visual deltas after cancellation, but keep draining the
                // core receiver so later causal suffixes are still delivered.
                return true;
            }
            // Causal accounting, interaction, tool boundaries and the
            // AssistantEnded fence migrate off a full data lane after cancel.
            // Sequential projection preserves their order before the terminal.
            let sender = if event.is_control()
                || (cancelled && (causal_telemetry || survives_cancellation))
            {
                self.control()
            } else {
                self.data()
            };
            match sender.try_send(event) {
                Ok(()) => {
                    self.wake.notify();
                    return true;
                }
                Err(mpsc::TrySendError::Full(pending)) => {
                    if cancellation.is_cancelled() && !survives_cancellation {
                        return true;
                    }
                    event = pending;
                    let _ = self.lane_space.wait_timeout(Duration::from_millis(50));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
            }
        }
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        // Field Drop runs after this method. Explicitly close this clone's
        // senders first so the last-producer wake observes Disconnected.
        drop(self.control.take());
        drop(self.data.take());
        #[cfg(test)]
        if let Some(probe) = self.drop_probe.take() {
            probe.reached_pre_wake.wait();
            probe.release_wake.wait();
        }
        self.wake.notify();
    }
}

struct ActiveRun {
    run_id: u64,
    task: tokio::task::JoinHandle<Result<ProviderExecution, ProviderError>>,
    projector: thread::JoinHandle<()>,
    cancellation: CancellationToken,
    durable: bool,
    content_store: SharedContentStore,
    interaction_responder: Option<InteractionResponder>,
}

struct ActiveRunLaunch {
    request: ProviderRequest,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
}

struct PendingRun {
    run_id: u64,
    result: Option<Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>>,
    projector: Option<thread::JoinHandle<()>>,
    delivery: VecDeque<UiEvent>,
    cancellation: CancellationToken,
    durable: bool,
    cancel_requested: bool,
    content_store: SharedContentStore,
}

const CONTENT_PAGE_BYTES: usize = 16 * 1024;
const CONTENT_ENTRY_BYTES: usize = 2 * 1024 * 1024;
const CONTENT_STORE_BYTES: usize = 8 * 1024 * 1024;
const CONTENT_STORE_ENTRIES: usize = 128;
const CONTENT_TRUNCATED_MARKER: &str = "\n[output truncated at 2 MiB]";

type SharedContentStore = Arc<Mutex<ContentStore>>;

#[derive(Debug)]
struct StoredContent {
    handle: ContentHandle,
    text: String,
    retained_bytes: usize,
}

#[derive(Debug, Default)]
struct ContentStore {
    entries: VecDeque<StoredContent>,
    retained_bytes: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct ContentPage {
    text: String,
    next_cursor: Option<PageCursor>,
}

impl ContentStore {
    fn insert_owned(&mut self, handle: ContentHandle, mut text: String) {
        if let Some(index) = self.entries.iter().position(|entry| entry.handle == handle) {
            if let Some(previous) = self.entries.remove(index) {
                self.retained_bytes = self.retained_bytes.saturating_sub(previous.retained_bytes);
            }
        }

        if text.len() > CONTENT_ENTRY_BYTES {
            let target = CONTENT_ENTRY_BYTES.saturating_sub(CONTENT_TRUNCATED_MARKER.len());
            let end = utf8_boundary_at_or_before(&text, target);
            text.truncate(end);
            text.push_str(CONTENT_TRUNCATED_MARKER);
            text.shrink_to_fit();
        }
        let retained_bytes = text.capacity().saturating_add(handle.0.len());
        self.entries.push_back(StoredContent {
            handle,
            text,
            retained_bytes,
        });
        self.retained_bytes = self.retained_bytes.saturating_add(retained_bytes);
        while self.entries.len() > CONTENT_STORE_ENTRIES
            || self.retained_bytes > CONTENT_STORE_BYTES
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(evicted.retained_bytes);
        }
    }

    #[cfg(test)]
    fn insert(&mut self, handle: ContentHandle, output: &str) {
        self.insert_owned(handle, output.to_owned());
    }

    fn page(
        &self,
        handle: &ContentHandle,
        cursor: Option<PageCursor>,
    ) -> Result<ContentPage, String> {
        let Some(entry) = self.entries.iter().find(|entry| &entry.handle == handle) else {
            return Err("Tool output is no longer retained".into());
        };
        let start = cursor
            .map(|cursor| usize::try_from(cursor.0).map_err(|_| "Invalid content page cursor"))
            .transpose()?
            .unwrap_or(0);
        if start > entry.text.len() || !entry.text.is_char_boundary(start) {
            return Err("Invalid content page cursor".into());
        }
        let target_end = start
            .saturating_add(CONTENT_PAGE_BYTES)
            .min(entry.text.len());
        let end = utf8_boundary_at_or_before(&entry.text, target_end).max(start);
        Ok(ContentPage {
            text: entry.text[start..end].to_owned(),
            next_cursor: (end < entry.text.len()).then_some(PageCursor(end as u64)),
        })
    }
}

fn utf8_boundary_at_or_before(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl PendingRun {
    fn request_cancel(&mut self) {
        self.cancel_requested = true;
        self.cancellation.cancel();
    }
}

struct ActiveLogin {
    provider: OAuthProvider,
    task: tokio::task::JoinHandle<Result<OAuthCredential, OAuthError>>,
    progress: tokio::task::JoinHandle<()>,
    cancel: tokio::sync::watch::Sender<bool>,
}

enum ActiveEvent {
    Command(Option<UiCommand>),
    Finished(Box<Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>>),
}

enum LoginEvent {
    Command(Option<UiCommand>),
    Finished(Result<Result<OAuthCredential, OAuthError>, tokio::task::JoinError>),
}

fn unbound_interaction_ack(request_id: InteractionRequestId) -> UiEvent {
    UiEvent::InteractionAcknowledged {
        request_id,
        accepted: false,
        message: "interaction route unavailable in this host".into(),
    }
}

fn reject_unbound_interaction(sink: &EventSink, request_id: InteractionRequestId) {
    sink.send(unbound_interaction_ack(request_id));
}

fn answer_active_question(
    run: &ActiveRun,
    request_id: InteractionRequestId,
    answer: slim_core::QuestionAnswer,
    sink: &EventSink,
) {
    let prefix = format!("run-{}:", run.run_id);
    let Some(core_id) = request_id.0.strip_prefix(&prefix) else {
        sink.send(UiEvent::InteractionAcknowledged {
            request_id,
            accepted: false,
            message: "question does not belong to the active run".into(),
        });
        return;
    };
    let result = run
        .interaction_responder
        .as_ref()
        .ok_or_else(|| "question route unavailable for this run".to_owned())
        .and_then(|responder| {
            let core_id =
                CoreInteractionRequestId::new(core_id).map_err(|error| error.to_string())?;
            responder
                .answer(core_id, answer)
                .map_err(|error| error.to_string())
        });
    if let Err(message) = result {
        sink.send(UiEvent::InteractionAcknowledged {
            request_id,
            accepted: false,
            message,
        });
    }
}

fn run_worker(
    mut startup: TuiStartup,
    oauth: OAuthService,
    command_rx: mpsc::Receiver<UiCommand>,
    sink: EventSink,
) {
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel();
    let forwarder = thread::spawn(move || {
        while let Ok(command) = command_rx.recv() {
            let shutdown = command == UiCommand::Shutdown;
            if async_tx.send(command).is_err() || shutdown {
                break;
            }
        }
    });
    let tokio_runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = sink.send(UiEvent::RunFailed {
                run_id: None,
                message: format!("runtime: {error}"),
            });
            return;
        }
    };
    tokio_runtime.block_on(async move {
        let cwd = startup
            .options
            .workspace_root
            .as_deref()
            .map(display_workspace_path)
            .unwrap_or_default();
        sink.send(UiEvent::WorkspaceChanged { cwd });
        if let Some(root) = startup.options.workspace_root.clone() {
            if let Some(intel) = startup.options.code_intelligence.as_ref() {
                intel.warm_workspace(root);
            }
        }
        let provider = startup
            .oauth_session
            .as_ref()
            .map(|(provider, _)| login_provider(*provider))
            .or_else(|| {
                // G241: API-key providers must report their identity on boot
                // so the status bar reflects who is connected.
                startup.request.as_ref().and_then(|request| {
                    match request.kind {
                        ProviderKind::OpenCodeGo => Some(LoginProvider::OpenCodeGo),
                        ProviderKind::ClinePass => Some(LoginProvider::ClinePass),
                        ProviderKind::CommandCode => Some(LoginProvider::CommandCode),
                        _ => None,
                    }
                })
            });
        let _ = sink.send(UiEvent::AuthStateChanged {
            provider,
            authenticated: startup.request.is_some(),
        });
        let model = startup
            .request
            .as_ref()
            .map(|request| request.model.clone())
            .or_else(|| startup.model_override.clone())
            .unwrap_or_else(|| ModelAlias::Sol.id().into());
        let _ = sink.send(UiEvent::ModelChanged { model });
        let _ = sink.send(UiEvent::EffortChanged {
            effort: startup.effort,
        });
        if !startup.image_labels.is_empty() {
            let _ = sink.send(UiEvent::AttachmentsChanged {
                labels: startup.image_labels.clone(),
            });
        }
        let mut active: Option<ActiveRun> = None;
        let mut pending: Option<PendingRun> = None;
        let mut login: Option<ActiveLogin> = None;
        let mut next_run_id = 1_u64;
        let content_store = SharedContentStore::default();
        let open_code_catalog = OpenCodeCatalog::production().ok();
        let command_code_catalog = CommandCodeCatalog::production().ok();
        let cline_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        let open_code_inflight = Arc::new(AtomicBool::new(false));
        let cline_inflight = Arc::new(AtomicBool::new(false));
        let command_code_inflight = Arc::new(AtomicBool::new(false));
        let open_code_gen = Arc::new(AtomicU64::new(0));
        let cline_gen = Arc::new(AtomicU64::new(0));
        let command_code_gen = Arc::new(AtomicU64::new(0));
        loop {
            if let Some(mut run) = pending.take() {
                if let Some(projector) = run.projector.take() {
                    if projector.is_finished() {
                        let _ = projector.join();
                        let result = run.result.take().expect("pending result");
                        if run.cancel_requested {
                            send_cancel_result(run.run_id, result, &sink);
                            continue;
                        }
                        let deferred = std::mem::take(&mut run.delivery);
                        run.delivery = execution_result_events(run.run_id, result, run.durable);
                        run.delivery.extend(deferred);
                    } else {
                        run.projector = Some(projector);
                        if service_pending_command(&mut run, &mut async_rx, &sink) {
                            break;
                        }
                        pending = Some(run);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        continue;
                    }
                }

                match advance_pending_delivery(&mut run, &mut async_rx, &sink) {
                    PendingDeliveryStep::Complete => {}
                    PendingDeliveryStep::Pending => {
                        pending = Some(run);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    PendingDeliveryStep::Shutdown | PendingDeliveryStep::Disconnected => break,
                }
                continue;
            }

            if login.is_some() {
                let wake = {
                    let current = login.as_mut().expect("checked");
                    tokio::select! {
                        command = async_rx.recv() => LoginEvent::Command(command),
                        result = &mut current.task => LoginEvent::Finished(result),
                    }
                };
                match wake {
                    LoginEvent::Command(Some(UiCommand::CancelLogin)) => {
                        cancel_login(&mut login).await;
                        let _ = sink.send(UiEvent::LoginProgress {
                            message: "Login cancelled".into(),
                        });
                    }
                    LoginEvent::Command(Some(UiCommand::Shutdown)) | LoginEvent::Command(None) => {
                        cancel_login(&mut login).await;
                        let _ = sink.send(UiEvent::Shutdown);
                        break;
                    }
                    LoginEvent::Command(Some(
                        UiCommand::AnswerInput { request_id, .. }
                        | UiCommand::AnswerQuestion { request_id, .. }
                        | UiCommand::Approve { request_id }
                        | UiCommand::Reject { request_id },
                    )) => reject_unbound_interaction(&sink, request_id),
                    LoginEvent::Command(Some(_)) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: "login is already active".into(),
                        });
                    }
                    LoginEvent::Finished(result) => {
                        let current = login.take().expect("active login");
                        let _ = current.progress.await;
                        match result {
                            Ok(Ok(credential)) => {
                                match oauth_request(
                                    current.provider,
                                    &credential,
                                    startup.mode,
                                    startup.endpoint_override.as_deref(),
                                    startup.model_override.as_deref(),
                                ) {
                                    Ok(request) => {
                                        startup.request = Some(request);
                                        startup.oauth_session =
                                            Some((current.provider, credential));
                                        let _ = sink.send(UiEvent::AuthStateChanged {
                                            provider: Some(login_provider(current.provider)),
                                            authenticated: true,
                                        });
                                        let _ = sink.send(UiEvent::Notification {
                                            message: format!(
                                                "Connected: {}",
                                                current.provider.label()
                                            ),
                                        });
                                    }
                                    Err(error) => {
                                        let _ = sink.send(UiEvent::RunFailed {
                                            run_id: None,
                                            message: error.to_string(),
                                        });
                                    }
                                }
                            }
                            Ok(Err(error)) => {
                                let _ = sink.send(UiEvent::LoginFailed {
                                    message: error.to_string(),
                                });
                            }
                            Err(_) => {
                                let _ = sink.send(UiEvent::LoginFailed {
                                    message: "OAuth task failed".into(),
                                });
                            }
                        }
                    }
                }
                continue;
            }

            if active.is_some() {
                let wake = {
                    let run = active.as_mut().expect("checked");
                    tokio::select! {
                        command = async_rx.recv() => ActiveEvent::Command(command),
                        result = &mut run.task => ActiveEvent::Finished(Box::new(result)),
                    }
                };
                match wake {
                    ActiveEvent::Command(None) => {
                        let _ = abort_active(&mut active).await;
                        break;
                    }
                    ActiveEvent::Command(Some(UiCommand::CancelRun)) => {
                        pending = abort_active(&mut active).await;
                    }
                    ActiveEvent::Command(Some(UiCommand::Shutdown)) => {
                        let _ = abort_active(&mut active).await;
                        let _ = sink.send(UiEvent::Shutdown);
                        break;
                    }
                    ActiveEvent::Command(Some(UiCommand::SetMode(mode))) => {
                        startup.mode = mode;
                        if let Some(request) = startup.request.as_mut() {
                            request.mode = mode;
                        }
                        let _ = sink.send(UiEvent::ModeChanged { mode });
                    }
                    ActiveEvent::Command(Some(UiCommand::Compact { instructions })) => {
                        request_manual_compaction(&startup.options, instructions, &sink);
                    }
                    ActiveEvent::Command(Some(UiCommand::RequestContentPage {
                        handle,
                        request_id,
                        cursor,
                    })) => {
                        let store = &active.as_ref().expect("active run").content_store;
                        serve_content_page(store, &sink, handle, request_id, cursor);
                    }
                    ActiveEvent::Command(Some(UiCommand::AnswerQuestion {
                        request_id,
                        answer,
                    })) => {
                        let run = active.as_ref().expect("active run");
                        answer_active_question(run, request_id, answer, &sink);
                    }
                    ActiveEvent::Command(Some(
                        UiCommand::AnswerInput { request_id, .. }
                        | UiCommand::Approve { request_id }
                        | UiCommand::Reject { request_id },
                    )) => reject_unbound_interaction(&sink, request_id),
                    ActiveEvent::Command(Some(_)) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: "a run is already active".into(),
                        });
                    }
                    ActiveEvent::Finished(result) => {
                        if let Ok(Ok(execution)) = &*result {
                            if let Some(preflight) = execution.resume_preflight.clone() {
                                startup.resume_path = Some(preflight.path.clone());
                                startup.resume_preflight = Some(preflight);
                            }
                            if execution.result.code == ExitCode::Success {
                                if let Some(history) = execution.history.as_ref() {
                                    startup.options.history.clone_from(history);
                                } else if let Some(request) = startup.request.as_ref() {
                                    record_completed_turn(
                                        &mut startup.options,
                                        &request.prompt,
                                        &execution.result.text,
                                    );
                                }
                            }
                        }
                        let durable = active.as_ref().is_some_and(|run| run.durable);
                        if let Some(run) = active.take() {
                            pending = Some(PendingRun {
                                run_id: run.run_id,
                                result: Some(*result),
                                projector: Some(run.projector),
                                delivery: VecDeque::new(),
                                cancellation: run.cancellation,
                                durable,
                                cancel_requested: false,
                                content_store: run.content_store,
                            });
                        }
                    }
                }
                continue;
            }

            let Some(command) = async_rx.recv().await else {
                break;
            };
            match command {
                UiCommand::ResumePrevious => {
                    let workspace = startup
                        .options
                        .workspace_root
                        .as_deref()
                        .expect("workspace root initialized");
                    match select_previous_tui_session(
                        workspace,
                        startup.resume_path.as_deref(),
                    ) {
                        Ok(Some(selected)) => {
                            let session_id = selected
                                .preflight
                                .session_id
                                .clone()
                                .unwrap_or_else(|| "unknown".into());
                            let cwd = display_workspace_path(workspace);
                            let messages = transcript_messages(&selected.history);
                            if let Some(policy) = startup
                                .options
                                .compaction
                                .as_ref()
                                .map(slim_core::context::CompactionHandle::policy)
                            {
                                startup.options.compaction =
                                    Some(slim_core::context::CompactionHandle::new(policy));
                            }
                            startup.options.history = selected.history;
                            startup.resume_path = Some(selected.preflight.path.clone());
                            startup.resume_preflight = Some(selected.preflight);
                            let _ = sink.send(UiEvent::SessionRestored {
                                session_id: slim_tui::api::SessionId(session_id.into()),
                                cwd,
                                messages,
                            });
                            let _ = sink.send(UiEvent::Notification {
                                message: "Previous session restored. Send a prompt to continue."
                                    .into(),
                            });
                        }
                        Ok(None) => {
                            let _ = sink.send(UiEvent::Notification {
                                message: "No resumable session for this directory".into(),
                            });
                        }
                        Err(message) => {
                            let _ = sink.send(UiEvent::Notification { message });
                        }
                    }
                }
                UiCommand::StartLogin(provider) => {
                    if let Some(provider) = oauth_provider(provider) {
                        login = Some(start_login(oauth.clone(), provider, sink.clone()));
                    } else {
                        let _ = sink.send(UiEvent::LoginFailed {
                            message: "This provider uses the API-key form.".into(),
                        });
                    }
                }
                UiCommand::SaveApiKey { provider, api_key } => {
                    let key = api_key.expose().to_owned();
                    let key_to_save = key.clone();
                    match provider {
                        LoginProvider::OpenCodeGo => {
                            let saved = tokio::task::spawn_blocking(move || {
                                save_api_key(ProviderKind::OpenCodeGo, &key_to_save)
                            })
                            .await;
                            match saved {
                                Ok(Ok(())) => {
                                    let model = startup
                                        .model_override
                                        .as_deref()
                                        .filter(|model| open_code_model(model).is_some())
                                        .unwrap_or(OPENCODE_GO_DEFAULT_MODEL)
                                        .to_owned();
                                    startup.request = Some(ProviderRequest {
                                        prompt: String::new(),
                                        mode: startup.mode,
                                        kind: ProviderKind::OpenCodeGo,
                                        endpoint: startup
                                            .endpoint_override
                                            .clone()
                                            .unwrap_or_else(|| OPENCODE_GO_BASE_URL.into()),
                                        model: model.clone(),
                                        api_key: key,
                                        account_id: None,
                                        timeout: startup.timeout,
                                    });
                                    startup.oauth_session = None;
                                    let _ = sink.send(UiEvent::AuthStateChanged {
                                        provider: Some(LoginProvider::OpenCodeGo),
                                        authenticated: true,
                                    });
                                    let _ = sink.send(UiEvent::ModelChanged { model });
                                    let _ = sink.send(UiEvent::Notification {
                                        message: "Connected: OpenCode Go".into(),
                                    });
                                }
                                Ok(Err(error)) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: error.to_string(),
                                    });
                                }
                                Err(_) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: "OpenCode Go key save task failed".into(),
                                    });
                                }
                            }
                        }
                        LoginProvider::ClinePass => {
                            let saved = tokio::task::spawn_blocking(move || {
                                save_api_key(ProviderKind::ClinePass, &key_to_save)
                            })
                            .await;
                            match saved {
                                Ok(Ok(())) => {
                                    let model = startup
                                        .model_override
                                        .as_deref()
                                        .filter(|m| is_clinepass_model_id(m))
                                        .unwrap_or(CLINEPASS_DEFAULT_MODEL)
                                        .to_owned();
                                    startup.request = Some(ProviderRequest {
                                        prompt: String::new(),
                                        mode: startup.mode,
                                        kind: ProviderKind::ClinePass,
                                        endpoint: startup
                                            .endpoint_override
                                            .clone()
                                            .unwrap_or_else(|| CLINEPASS_BASE_URL.into()),
                                        model: model.clone(),
                                        api_key: key,
                                        account_id: None,
                                        timeout: startup.timeout,
                                    });
                                    startup.oauth_session = None;
                                    let _ = sink.send(UiEvent::AuthStateChanged {
                                        provider: Some(LoginProvider::ClinePass),
                                        authenticated: true,
                                    });
                                    let _ = sink.send(UiEvent::ModelChanged { model });
                                    let _ = sink.send(UiEvent::Notification {
                                        message: "Connected: ClinePass".into(),
                                    });
                                }
                                Ok(Err(error)) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: error.to_string(),
                                    });
                                }
                                Err(_) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: "ClinePass key save task failed".into(),
                                    });
                                }
                            }
                        }
                        LoginProvider::CommandCode => {
                            let saved = tokio::task::spawn_blocking(move || {
                                save_api_key(ProviderKind::CommandCode, &key_to_save)
                            })
                            .await;
                            match saved {
                                Ok(Ok(())) => {
                                    let model = startup
                                        .model_override
                                        .as_deref()
                                        .filter(|m| is_command_code_model_id(m))
                                        .unwrap_or(COMMANDCODE_DEFAULT_MODEL)
                                        .to_owned();
                                    startup.request = Some(ProviderRequest {
                                        prompt: String::new(),
                                        mode: startup.mode,
                                        kind: ProviderKind::CommandCode,
                                        endpoint: startup
                                            .endpoint_override
                                            .clone()
                                            .unwrap_or_else(|| COMMANDCODE_BASE_URL.into()),
                                        model: model.clone(),
                                        api_key: key,
                                        account_id: None,
                                        timeout: startup.timeout,
                                    });
                                    startup.oauth_session = None;
                                    let _ = sink.send(UiEvent::AuthStateChanged {
                                        provider: Some(LoginProvider::CommandCode),
                                        authenticated: true,
                                    });
                                    let _ = sink.send(UiEvent::ModelChanged { model });
                                    let _ = sink.send(UiEvent::Notification {
                                        message: "Connected: Command Code".into(),
                                    });
                                }
                                Ok(Err(error)) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: error.to_string(),
                                    });
                                }
                                Err(_) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: "Command Code key save task failed".into(),
                                    });
                                }
                            }
                        }
                        _ => {
                            let _ = sink.send(UiEvent::LoginFailed {
                                message: "API-key login is unavailable for this provider."
                                    .into(),
                            });
                        }
                    }
                }
                UiCommand::CancelLogin => {}
                UiCommand::RefreshOpenCodeModels => {
                    let sink = sink.clone();
                    let Some(catalog) = open_code_catalog.clone() else {
                        let _ = sink.send(UiEvent::Notification {
                            message: "OpenCode Go catalog path is unavailable".into(),
                        });
                        continue;
                    };
                    let _ = sink.send(open_code_catalog_event(catalog.load_or_fallback()));
                    if open_code_inflight.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    let generation = open_code_gen.fetch_add(1, Ordering::AcqRel) + 1;
                    let inflight = open_code_inflight.clone();
                    let gen_cell = open_code_gen.clone();
                    tokio::spawn(async move {
                        let result = catalog.refresh().await;
                        inflight.store(false, Ordering::Release);
                        if gen_cell.load(Ordering::Acquire) != generation {
                            return;
                        }
                        match result {
                            Ok(snapshot) => {
                                let _ = sink.send(open_code_catalog_event(snapshot));
                            }
                            Err(error) => {
                                let _ = sink.send(UiEvent::Notification {
                                    message: format!(
                                        "OpenCode Go live catalog unavailable; using cache/fallback: {error}"
                                    ),
                                });
                            }
                        }
                    });
                }
                UiCommand::RefreshClinePassModels => {
                    let sink = sink.clone();
                    // G240: only the connected ClinePass credential may be
                    // used as bearer; keys belonging to other providers are not.
                    let api_key = startup
                        .request
                        .as_ref()
                        .filter(|request| request.kind == ProviderKind::ClinePass)
                        .map(|r| r.api_key.clone())
                        .unwrap_or_default();
                    let catalog_endpoint = startup
                        .endpoint_override
                        .clone()
                        .unwrap_or_else(|| CLINEPASS_BASE_URL.into());
                    if cline_inflight.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    let generation = cline_gen.fetch_add(1, Ordering::AcqRel) + 1;
                    let inflight = cline_inflight.clone();
                    let gen_cell = cline_gen.clone();
                    let client = cline_client.clone();
                    tokio::spawn(async move {
                        let url = format!(
                            "{}models",
                            catalog_endpoint.trim_end_matches("chat/completions")
                        );
                        let live = match fetch_clinepass_catalog(&client, &url, &api_key).await {
                            Ok(entries) => entries
                                    .into_iter()
                                    .map(|entry| {
                                        let bundled = clinepass_model(&entry.id);
                                        OpenCodeModelView {
                                            id: entry.id,
                                            name: entry.name,
                                            context_window_tokens: entry.context_window,
                                            max_output_tokens: bundled
                                                .map(|model| model.max_output_tokens as u64)
                                                .unwrap_or(131_072),
                                            reasoning_levels: Vec::new(),
                                            accepts_images: bundled
                                                .map(|model| model.accepts_images)
                                                .unwrap_or(false),
                                        }
                                    })
                                    .collect::<Vec<_>>(),
                            _ => Vec::new(),
                        };
                        inflight.store(false, Ordering::Release);
                        if gen_cell.load(Ordering::Acquire) != generation {
                            return;
                        }
                        // G240: a 200 with a malformed/empty body must not wipe
                        // the group — fall back to the built-in catalog instead.
                        let (models, source) = if live.is_empty() {
                            (
                                slim_core::provider::clinepass_models()
                                    .iter()
                                    .map(|m| OpenCodeModelView {
                                        id: m.id.to_owned(),
                                        name: m.name.to_owned(),
                                        context_window_tokens: m.context_window,
                                        max_output_tokens: m.max_output_tokens as u64,
                                        reasoning_levels: Vec::new(),
                                        accepts_images: m.accepts_images,
                                    })
                                    .collect::<Vec<_>>(),
                                ClinePassCatalogSource::Cache,
                            )
                        } else {
                            (live, ClinePassCatalogSource::Live)
                        };
                        let _ = sink.send(UiEvent::ClinePassCatalogLoaded { models, source });
                    });
                }
                UiCommand::RefreshCommandCodeModels => {
                    let sink = sink.clone();
                    let Some(catalog) = command_code_catalog.clone() else {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Command Code catalog path is unavailable".into(),
                        });
                        continue;
                    };
                    let _ = sink.send(command_code_catalog_event(catalog.load_or_fallback()));
                    if command_code_inflight.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    let generation = command_code_gen.fetch_add(1, Ordering::AcqRel) + 1;
                    let inflight = command_code_inflight.clone();
                    let gen_cell = command_code_gen.clone();
                    tokio::spawn(async move {
                        let result = catalog.refresh().await;
                        inflight.store(false, Ordering::Release);
                        if gen_cell.load(Ordering::Acquire) != generation {
                            return;
                        }
                        match result {
                            Ok(snapshot) => {
                                let _ = sink.send(command_code_catalog_event(snapshot));
                            }
                            Err(error) => {
                                let detail = match error {
                                    slim_core::ProviderError::InvalidResponse { message }
                                    | slim_core::ProviderError::Remote { message } => message,
                                    _ => "Command Code catalog request failed".into(),
                                };
                                let _ = sink.send(UiEvent::Notification {
                                    message: format!(
                                        "Command Code live catalog unavailable; using cache/fallback: {detail}"
                                    ),
                                });
                            }
                        }
                    });
                }
                UiCommand::SetOpenCodeModel { model, effort } => {
                    let supported = open_code_model(&model)
                        .is_some_and(|model| model.reasoning_levels.contains(&effort.id()));
                    if !supported {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Unsupported OpenCode Go model or reasoning effort".into(),
                        });
                        continue;
                    }
                    let switched = match activate_saved_api_key_provider(
                        &mut startup,
                        &oauth,
                        ProviderKind::OpenCodeGo,
                        "opencode-go",
                        OPENCODE_GO_BASE_URL,
                        &model,
                        "OpenCode Go",
                    ) {
                        Ok(switched) => switched,
                        Err(message) => {
                            let _ = sink.send(UiEvent::Notification { message });
                            continue;
                        }
                    };
                    let Some(request) = startup.request.as_mut() else {
                        continue;
                    };
                    request.model.clone_from(&model);
                    if switched {
                        let _ = sink.send(UiEvent::AuthStateChanged {
                            provider: Some(LoginProvider::OpenCodeGo),
                            authenticated: true,
                        });
                        let _ = sink.send(UiEvent::Notification {
                            message: "Connected: OpenCode Go".into(),
                        });
                    }
                    startup.model_override = Some(model.clone());
                    startup.options.reasoning_effort = Some(effort.id().into());
                    persist_model(&sink, &model, effort.id());
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::SetClinePassModel { model, effort } => {
                    // Fail fast on an unknown id instead of deferring to the
                    // adapter (G249 TUI-side validation mirrored here). Live
                    // `cline-pass/*` slugs are valid even when absent from the
                    // bundled catalog.
                    if !is_clinepass_model_id(&model) {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Unsupported ClinePass model".into(),
                        });
                        continue;
                    }
                    let switched = match activate_saved_api_key_provider(
                        &mut startup,
                        &oauth,
                        ProviderKind::ClinePass,
                        "clinepass",
                        CLINEPASS_BASE_URL,
                        &model,
                        "ClinePass",
                    ) {
                        Ok(switched) => switched,
                        Err(message) => {
                            let _ = sink.send(UiEvent::Notification { message });
                            continue;
                        }
                    };
                    let Some(request) = startup.request.as_mut() else {
                        continue;
                    };
                    request.model.clone_from(&model);
                    if switched {
                        let _ = sink.send(UiEvent::AuthStateChanged {
                            provider: Some(LoginProvider::ClinePass),
                            authenticated: true,
                        });
                        let _ = sink.send(UiEvent::Notification {
                            message: "Connected: ClinePass".into(),
                        });
                    }
                    startup.model_override = Some(model.clone());
                    startup.options.reasoning_effort = Some(effort.id().into());
                    persist_model(&sink, &model, effort.id());
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::SetCommandCodeModel { model, effort } => {
                    if !is_command_code_model_id(&model) {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Unsupported Command Code model".into(),
                        });
                        continue;
                    }
                    let switched = match activate_saved_api_key_provider(
                        &mut startup,
                        &oauth,
                        ProviderKind::CommandCode,
                        "command-code",
                        COMMANDCODE_BASE_URL,
                        &model,
                        "Command Code",
                    ) {
                        Ok(switched) => switched,
                        Err(message) => {
                            let _ = sink.send(UiEvent::Notification { message });
                            continue;
                        }
                    };
                    let Some(request) = startup.request.as_mut() else {
                        continue;
                    };
                    request.model.clone_from(&model);
                    if switched {
                        let _ = sink.send(UiEvent::AuthStateChanged {
                            provider: Some(LoginProvider::CommandCode),
                            authenticated: true,
                        });
                        let _ = sink.send(UiEvent::Notification {
                            message: "Connected: Command Code".into(),
                        });
                    }
                    startup.model_override = Some(model.clone());
                    startup.options.reasoning_effort = Some(effort.id().into());
                    persist_model(&sink, &model, effort.id());
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::Logout => {
                    if let Some((provider, _)) = startup.oauth_session.take() {
                        match oauth.logout(provider) {
                            Ok(()) => {
                                startup.request = None;
                                let _ = sink.send(UiEvent::AuthStateChanged {
                                    provider: None,
                                    authenticated: false,
                                });
                            }
                            Err(error) => {
                                let _ = sink.send(UiEvent::RunFailed {
                                    run_id: None,
                                    message: error.to_string(),
                                });
                            }
                        }
                    } else if let Some(kind) = startup
                        .request
                        .as_ref()
                        .map(|request| request.kind)
                        .filter(|kind| {
                            matches!(
                                kind,
                                ProviderKind::OpenCodeGo
                                    | ProviderKind::ClinePass
                                    | ProviderKind::CommandCode
                            )
                        })
                    {
                        let (environment_keys, label): (&[&str], &str) = match kind {
                            ProviderKind::ClinePass => {
                                (&["SLIM_API_KEY", "CLINEPASS_API_KEY"], "ClinePass")
                            }
                            ProviderKind::CommandCode => (
                                &["SLIM_API_KEY", "COMMANDCODE_API_KEY", "CMD_API_KEY"],
                                "Command Code",
                            ),
                            _ => (&["SLIM_API_KEY", "OPENCODE_API_KEY"], "OpenCode Go"),
                        };
                        match tokio::task::spawn_blocking(move || delete_api_key(kind)).await {
                            Ok(Ok(())) => {
                                startup.request = None;
                                let _ = sink.send(UiEvent::AuthStateChanged {
                                    provider: None,
                                    authenticated: false,
                                });
                                if environment_keys
                                    .iter()
                                    .any(|key| std::env::var_os(key).is_some())
                                {
                                    let _ = sink.send(UiEvent::Notification {
                                        message: format!(
                                            "{label} disconnected; environment API key still has precedence"
                                        ),
                                    });
                                }
                            }
                            Ok(Err(error)) => {
                                let _ = sink.send(UiEvent::RunFailed {
                                    run_id: None,
                                    message: error.to_string(),
                                });
                            }
                            Err(_) => {
                                let _ = sink.send(UiEvent::RunFailed {
                                    run_id: None,
                                    message: format!("{label} logout task failed"),
                                });
                            }
                        }
                    } else {
                        let _ = sink.send(UiEvent::Notification {
                            message: "No provider session is active".into(),
                        });
                    }
                }
                UiCommand::SendPrompt(prompt) if !prompt.trim().is_empty() => {
                    let skill_instructions = match startup.options.workspace_root.as_deref() {
                        Some(workspace_root) => {
                            match resolve_slash_skill_command(workspace_root, &prompt) {
                                Ok(Some(SlashSkillCommand::Selected { name })) => {
                                    let _ = sink.send(UiEvent::RestoreDraft {
                                        text: format!("/{name} "),
                                    });
                                    let _ = sink.send(UiEvent::Notification {
                                        message: format!(
                                            "Skill /{name} selected. Add a task and send."
                                        ),
                                    });
                                    continue;
                                }
                                Ok(Some(SlashSkillCommand::Invoke { name, body, source })) => {
                                    Some(SkillInstructions { name, body, source })
                                }
                                Ok(None) => None,
                                Err(message) => {
                                    let _ = sink.send(UiEvent::RestoreDraft {
                                        text: prompt.clone(),
                                    });
                                    let _ = sink.send(UiEvent::RunFailed {
                                        run_id: None,
                                        message,
                                    });
                                    continue;
                                }
                            }
                        }
                        None => None,
                    };
                    if let Some((provider, credential)) = startup.oauth_session.take() {
                        let _ = sink.send(UiEvent::ActivityChanged {
                            label: "Checking authentication".into(),
                        });
                        match oauth.fresh_credential(provider, credential.clone()).await {
                            Ok(fresh) => {
                                if let Some(message) = fresh.persistence_warning {
                                    let _ = sink.send(UiEvent::Notification { message });
                                }
                                let credential = fresh.credential;
                                match oauth_request(
                                    provider,
                                    &credential,
                                    startup.mode,
                                    startup.endpoint_override.as_deref(),
                                    startup.model_override.as_deref(),
                                ) {
                                    Ok(request) => startup.request = Some(request),
                                    Err(error) => {
                                        startup.oauth_session = Some((provider, credential));
                                        let _ = sink.send(UiEvent::RestoreDraft {
                                            text: prompt.clone(),
                                        });
                                        let _ = sink.send(UiEvent::RunFailed {
                                            run_id: None,
                                            message: error.to_string(),
                                        });
                                        continue;
                                    }
                                }
                                startup.oauth_session = Some((provider, credential));
                            }
                            Err(error) => {
                                startup.oauth_session = Some((provider, credential));
                                let _ = sink.send(UiEvent::RestoreDraft {
                                    text: prompt.clone(),
                                });
                                let _ = sink.send(UiEvent::RunFailed {
                                    run_id: None,
                                    message: error.to_string(),
                                });
                                continue;
                            }
                        }
                    }
                    let Some(request) = startup.request.as_ref() else {
                        let _ = sink.send(UiEvent::RestoreDraft {
                            text: prompt.clone(),
                        });
                        let _ = sink.send(UiEvent::Notification {
                            message: "No provider connected. Use /login.".into(),
                        });
                        continue;
                    };
                    if !startup.image_labels.is_empty() {
                        if let Some(message) = image_model_error(Some(request)) {
                            let _ = sink.send(UiEvent::RestoreDraft {
                                text: prompt.clone(),
                            });
                            let _ = sink.send(UiEvent::RunFailed {
                                run_id: None,
                                message,
                            });
                            continue;
                        }
                    }
                    match create_tui_session(&mut startup) {
                        Ok(Some((session_id, cwd))) => {
                            let _ = sink.send(UiEvent::SessionSnapshot {
                                session_id: slim_tui::api::SessionId(session_id.into()),
                                cwd,
                            });
                        }
                        Ok(None) => {}
                        Err(message) => {
                            let _ = sink.send(UiEvent::RestoreDraft {
                                text: prompt.clone(),
                            });
                            let _ = sink.send(UiEvent::RunFailed {
                                run_id: None,
                                message,
                            });
                            continue;
                        }
                    }
                    let request = startup
                        .request
                        .as_mut()
                        .expect("provider checked before session creation");
                    request.prompt.clone_from(&prompt);
                    let Some(run_id) = take_run_id(&mut next_run_id) else {
                        let _ = sink.send(UiEvent::FatalError {
                            run_id: None,
                            message: "TUI run identity exhausted".into(),
                        });
                        continue;
                    };
                    let tool_budget = match (
                        resolve_max_mutating_tool_calls(&startup.options),
                        resolve_max_read_tool_calls(&startup.options),
                        resolve_max_turns(&startup.options),
                    ) {
                        (Ok(max_mutating), Ok(max_read), Ok(max_turns)) => {
                            (max_mutating, max_read, max_turns)
                        }
                        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                            let _ = sink.send(UiEvent::RunFailed {
                                run_id: None,
                                message: provider_error_message(error),
                            });
                            continue;
                        }
                    };
                    let _ = sink.send(UiEvent::run_started_with_budget(
                        run_id,
                        tool_budget.0,
                        tool_budget.1,
                        tool_budget.2,
                    ));
                    let mut display_prompt = redact_for_ui(&prompt, &request.api_key);
                    if !startup.image_labels.is_empty() {
                        display_prompt.push_str("\n\n");
                        for label in &startup.image_labels {
                            display_prompt.push_str(&format!("[image · {label}]\n"));
                        }
                        display_prompt.pop();
                    }
                    let _ = sink.send(UiEvent::UserMessageAdded { text: display_prompt });
                    let run_options = startup.options.clone();
                    let resume_preflight = startup.resume_preflight.take();
                    match start_active_run(
                        run_id,
                        ActiveRunLaunch {
                            request: request.clone(),
                            options: run_options,
                            skill_instructions,
                        },
                        startup.resume_path.clone(),
                        resume_preflight,
                        sink.clone(),
                        content_store.clone(),
                    ) {
                        Ok(run) => {
                            startup.options.content_blocks.clear();
                            startup.image_labels.clear();
                            let _ = sink.send(UiEvent::AttachmentsChanged { labels: Vec::new() });
                            active = Some(run);
                        }
                        Err(message) => {
                            let _ = sink.send(UiEvent::RestoreDraft {
                                text: prompt.clone(),
                            });
                            let _ = sink.send(UiEvent::RunFailed {
                                run_id: Some(run_id),
                                message,
                            });
                        }
                    }
                }
                UiCommand::SendPrompt(_) => {
                    let _ = sink.send(UiEvent::Notification {
                        message: "prompt cannot be empty".into(),
                    });
                }
                UiCommand::AttachImage(path) => {
                    if let Some(message) = image_model_error(startup.request.as_ref()) {
                        let _ = sink.send(UiEvent::Notification { message });
                        continue;
                    }
                    if startup.image_labels.len() >= 8 {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Image limit reached (8)".into(),
                        });
                        continue;
                    }
                    match load_local_images(std::slice::from_ref(&path)) {
                        Ok(mut blocks) => {
                            startup.options.content_blocks.append(&mut blocks);
                            let label = Path::new(&path)
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or(&path)
                                .to_owned();
                            startup.image_labels.push(label);
                            let _ = sink.send(UiEvent::AttachmentsChanged {
                                labels: startup.image_labels.clone(),
                            });
                        }
                        Err(error) => {
                            let _ = sink.send(UiEvent::Notification {
                                message: format!("image: {error}"),
                            });
                        }
                    }
                }
                UiCommand::Compact { instructions } => {
                    let window = startup
                        .options
                        .context_window_tokens
                        .unwrap_or(32_000);
                    let mut policy = startup
                        .options
                        .compaction
                        .as_ref()
                        .map(slim_core::context::CompactionHandle::policy)
                        .unwrap_or_default();
                    policy.keep_recent_tokens = policy.keep_recent_for_window(window);
                    if slim_core::context::select_compaction_history(
                        &startup.options.history,
                        &policy,
                    )
                    .is_err()
                    {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Nothing to compact yet".into(),
                        });
                    } else {
                        request_manual_compaction(&startup.options, instructions, &sink);
                    }
                }
                UiCommand::AnswerInput { request_id, .. }
                | UiCommand::AnswerQuestion { request_id, .. }
                | UiCommand::Approve { request_id }
                | UiCommand::Reject { request_id } => {
                    reject_unbound_interaction(&sink, request_id);
                }
                UiCommand::SetMode(mode) => {
                    startup.mode = mode;
                    if let Some(request) = startup.request.as_mut() {
                        request.mode = mode;
                    }
                    let _ = sink.send(UiEvent::ModeChanged { mode });
                }
                UiCommand::SetModel {
                    model: alias,
                    effort,
                } => {
                    let switching_provider = !startup
                        .request
                        .as_ref()
                        .is_some_and(|request| request.kind == ProviderKind::OpenAiCodex);
                    let model = alias.id().to_owned();
                    if switching_provider {
                        let provider = OAuthProvider::OpenAiCodex;
                        let credential = match oauth.credential(provider) {
                            Ok(Some(credential)) => credential,
                            Ok(None) => {
                                let _ = sink.send(UiEvent::Notification {
                                    message: "OpenAI Codex login is not saved. Use /login.".into(),
                                });
                                continue;
                            }
                            Err(error) => {
                                let _ = sink.send(UiEvent::Notification {
                                    message: format!("Authentication: {error}"),
                                });
                                continue;
                            }
                        };
                        let request = match oauth_request(
                            provider,
                            &credential,
                            startup.mode,
                            None,
                            Some(&model),
                        ) {
                            Ok(request) => request,
                            Err(error) => {
                                let _ = sink.send(UiEvent::Notification {
                                    message: error.to_string(),
                                });
                                continue;
                            }
                        };
                        if let Err(error) = oauth.activate(provider, &credential) {
                            let _ = sink.send(UiEvent::Notification {
                                message: format!("Authentication: {error}"),
                            });
                            continue;
                        }
                        startup.request = Some(request);
                        startup.oauth_session = Some((provider, credential));
                        let _ = sink.send(UiEvent::AuthStateChanged {
                            provider: Some(LoginProvider::OpenAiCodex),
                            authenticated: true,
                        });
                        let _ = sink.send(UiEvent::Notification {
                            message: "Connected: OpenAI Codex".into(),
                        });
                    }
                    startup.model_override = Some(model.clone());
                    startup.options.reasoning_effort = Some(effort.id().into());
                    if let Some(request) = startup.request.as_mut() {
                        request.model.clone_from(&model);
                    }
                    persist_model(&sink, &model, effort.id());
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::CancelRun => {}
                UiCommand::Shutdown => {
                    let _ = sink.send(UiEvent::Shutdown);
                    break;
                }
                UiCommand::RequestContentPage {
                    handle,
                    request_id,
                    cursor,
                } => serve_content_page(&content_store, &sink, handle, request_id, cursor),
            }
        }
        if let Some(code_intelligence) = startup.options.code_intelligence.take() {
            code_intelligence.shutdown().await;
        }
    });
    let _ = forwarder.join();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingDeliveryStep {
    Complete,
    Pending,
    Shutdown,
    Disconnected,
}

fn advance_pending_delivery(
    run: &mut PendingRun,
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    sink: &EventSink,
) -> PendingDeliveryStep {
    let Some(event) = run.delivery.pop_front() else {
        return PendingDeliveryStep::Complete;
    };
    match sink.try_send(event) {
        Ok(()) if run.delivery.is_empty() => PendingDeliveryStep::Complete,
        Ok(()) => PendingDeliveryStep::Pending,
        Err(mpsc::TrySendError::Full(event)) => {
            run.delivery.push_front(event);
            if service_pending_command(run, commands, sink) {
                return PendingDeliveryStep::Shutdown;
            }
            if run.cancel_requested {
                if let Some(index) = run.delivery.iter().rposition(UiEvent::is_run_terminal) {
                    let terminal = run.delivery.remove(index).expect("terminal index");
                    sink.send_control(terminal);
                }
                for acknowledgement in std::mem::take(&mut run.delivery)
                    .into_iter()
                    .filter(|event| matches!(event, UiEvent::InteractionAcknowledged { .. }))
                {
                    sink.send_control(acknowledgement);
                }
                PendingDeliveryStep::Complete
            } else {
                PendingDeliveryStep::Pending
            }
        }
        Err(mpsc::TrySendError::Disconnected(_)) => PendingDeliveryStep::Disconnected,
    }
}

fn service_pending_command(
    run: &mut PendingRun,
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    sink: &EventSink,
) -> bool {
    match commands.try_recv() {
        Ok(UiCommand::CancelRun) => run.request_cancel(),
        Ok(UiCommand::RequestContentPage {
            handle,
            request_id,
            cursor,
        }) => serve_content_page(&run.content_store, sink, handle, request_id, cursor),
        Ok(
            UiCommand::AnswerInput { request_id, .. }
            | UiCommand::Approve { request_id }
            | UiCommand::Reject { request_id },
        ) => run.delivery.push_back(unbound_interaction_ack(request_id)),
        Ok(UiCommand::Shutdown) | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
            run.request_cancel();
            sink.send_control(UiEvent::Shutdown);
            return true;
        }
        Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
    }
    false
}

fn start_login(oauth: OAuthService, provider: OAuthProvider, sink: EventSink) -> ActiveLogin {
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let progress = tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            let event = match progress {
                OAuthProgress::Message(message) => UiEvent::LoginProgress { message },
                OAuthProgress::AuthUrl { url, user_code } => UiEvent::LoginUrl {
                    url: url.into(),
                    user_code: user_code.map(Into::into),
                },
            };
            sink.send(event);
        }
    });
    let task = tokio::spawn(async move { oauth.login(provider, progress_tx, cancel_rx).await });
    ActiveLogin {
        provider,
        task,
        progress,
        cancel: cancel_tx,
    }
}

async fn cancel_login(login: &mut Option<ActiveLogin>) {
    if let Some(mut login) = login.take() {
        let _ = login.cancel.send(true);
        login.task.abort();
        let _ = (&mut login.task).await;
        login.progress.abort();
        let _ = login.progress.await;
    }
}

fn take_run_id(next_run_id: &mut u64) -> Option<u64> {
    let run_id = *next_run_id;
    *next_run_id = run_id.checked_add(1)?;
    Some(run_id)
}

/// Persists the selected model+effort into the global config so the choice
/// survives restarts. Failures are surfaced as a non-fatal toast.
fn persist_model(sink: &EventSink, model: &str, effort: &str) {
    match crate::config::save_global_model(model, effort) {
        Ok(_) => {}
        Err(error) => {
            let _ = sink.send(UiEvent::Notification {
                message: format!("model selection not persisted: {error}"),
            });
        }
    }
}

fn open_code_catalog_event(snapshot: CatalogSnapshot) -> UiEvent {
    let models = snapshot
        .model_ids
        .iter()
        .filter_map(|id| open_code_model(id))
        .map(|model| OpenCodeModelView {
            id: model.id.to_owned(),
            name: model.name.to_owned(),
            context_window_tokens: model.context_window.unwrap_or_default(),
            max_output_tokens: model.max_output_tokens.unwrap_or_default() as u64,
            reasoning_levels: model
                .reasoning_levels
                .iter()
                .filter_map(|level| ReasoningEffort::parse(level))
                .collect(),
            accepts_images: model.accepts_images,
        })
        .collect();
    let source = match snapshot.source {
        CatalogSource::Live => OpenCodeCatalogSource::Live,
        CatalogSource::Cache => OpenCodeCatalogSource::Cache,
        CatalogSource::Fallback => OpenCodeCatalogSource::Fallback,
    };
    UiEvent::OpenCodeCatalogLoaded { models, source }
}

fn command_code_catalog_event(snapshot: crate::command_code_catalog::CatalogSnapshot) -> UiEvent {
    let models = snapshot
        .models
        .into_iter()
        .map(|model| OpenCodeModelView {
            id: model.id,
            name: model.name,
            context_window_tokens: model.context_window,
            max_output_tokens: 0,
            reasoning_levels: Vec::new(),
            accepts_images: false,
        })
        .collect();
    let source = match snapshot.source {
        crate::command_code_catalog::CatalogSource::Live => CommandCodeCatalogSource::Live,
        crate::command_code_catalog::CatalogSource::Cache => CommandCodeCatalogSource::Cache,
        crate::command_code_catalog::CatalogSource::Fallback => CommandCodeCatalogSource::Fallback,
    };
    UiEvent::CommandCodeCatalogLoaded { models, source }
}

fn oauth_provider(provider: LoginProvider) -> Option<OAuthProvider> {
    match provider {
        LoginProvider::Anthropic => Some(OAuthProvider::Anthropic),
        LoginProvider::OpenAiCodex => Some(OAuthProvider::OpenAiCodex),
        LoginProvider::OpenCodeGo | LoginProvider::ClinePass | LoginProvider::CommandCode => None,
    }
}

fn login_provider(provider: OAuthProvider) -> LoginProvider {
    match provider {
        OAuthProvider::Anthropic => LoginProvider::Anthropic,
        OAuthProvider::OpenAiCodex => LoginProvider::OpenAiCodex,
    }
}

fn associate_projected_run(event: UiEvent, run_id: u64) -> UiEvent {
    let scope = format!("run-{run_id}");
    let event = namespace_projected_ids(event, &scope);
    match event {
        UiEvent::FatalError {
            run_id: None,
            message,
        } => UiEvent::FatalError {
            run_id: Some(run_id),
            message,
        },
        UiEvent::UsageEstimate {
            request_id,
            context_tokens,
            context_window_tokens,
        } => UiEvent::UsageEstimateForRun {
            run_id,
            request_id,
            context_tokens,
            context_window_tokens,
        },
        event => event,
    }
}

fn namespace_projected_ids(event: UiEvent, scope: &str) -> UiEvent {
    let identity = |batch_id: ToolBatchId, call_id: ToolCallId| {
        (
            ToolBatchId(format!("{scope}:{}", batch_id.0).into()),
            ToolCallId(format!("{scope}:{}", call_id.0).into()),
        )
    };
    let interaction_identity = |request_id: InteractionRequestId| {
        InteractionRequestId(format!("{scope}:{}", request_id.0).into())
    };
    match event {
        UiEvent::ToolStarted {
            batch_id,
            call_id,
            name,
            arguments_summary,
        } => {
            let (batch_id, call_id) = identity(batch_id, call_id);
            UiEvent::ToolStarted {
                batch_id,
                call_id,
                name,
                arguments_summary,
            }
        }
        UiEvent::ToolProgress {
            batch_id,
            call_id,
            name,
            preview,
            content_handle,
        } => {
            let (batch_id, call_id) = identity(batch_id, call_id);
            UiEvent::ToolProgress {
                batch_id,
                call_id,
                name,
                preview,
                content_handle,
            }
        }
        UiEvent::ToolEnded {
            batch_id,
            call_id,
            name,
            success,
            duration_ms,
        } => {
            let (batch_id, call_id) = identity(batch_id, call_id);
            UiEvent::ToolEnded {
                batch_id,
                call_id,
                name,
                success,
                duration_ms,
            }
        }
        UiEvent::ApprovalRequired {
            request_id,
            summary,
            persisted,
        } => UiEvent::ApprovalRequired {
            request_id: interaction_identity(request_id),
            summary,
            persisted,
        },
        UiEvent::InputRequired {
            request_id,
            prompt,
            options,
            persisted,
        } => UiEvent::InputRequired {
            request_id: interaction_identity(request_id),
            prompt,
            options,
            persisted,
        },
        UiEvent::QuestionRequired {
            request_id,
            question,
            options,
            persisted,
        } => UiEvent::QuestionRequired {
            request_id: interaction_identity(request_id),
            question,
            options,
            persisted,
        },
        UiEvent::InteractionAcknowledged {
            request_id,
            accepted,
            message,
        } => UiEvent::InteractionAcknowledged {
            request_id: interaction_identity(request_id),
            accepted,
            message,
        },
        event => event,
    }
}

fn attach_workspace_to_snapshot(mut event: UiEvent, cwd_display: &str) -> UiEvent {
    if let UiEvent::SessionSnapshot { cwd, .. } = &mut event {
        cwd_display.clone_into(cwd);
    }
    event
}

fn display_workspace_path(path: &Path) -> String {
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
        if let Ok(relative) = path.strip_prefix(&home) {
            if relative.as_os_str().is_empty() {
                return "~".into();
            }
            return format!(
                "~{}{}",
                std::path::MAIN_SEPARATOR,
                relative.to_string_lossy()
            );
        }
    }
    path.to_string_lossy().into_owned()
}

fn serve_content_page(
    store: &SharedContentStore,
    sink: &EventSink,
    handle: ContentHandle,
    request_id: ContentRequestId,
    cursor: Option<PageCursor>,
) {
    let result = store
        .lock()
        .map_err(|_| "Tool output store is unavailable".to_owned())
        .and_then(|store| store.page(&handle, cursor));
    let event = match result {
        Ok(page) => UiEvent::ContentPageLoaded {
            handle,
            request_id,
            cursor,
            text: page.text,
            next_cursor: page.next_cursor,
        },
        Err(message) => UiEvent::ContentPageFailed {
            handle,
            request_id,
            cursor,
            message,
        },
    };
    sink.send(event);
}

fn project_core_event(
    event: SessionEvent,
    run_id: u64,
    store: &SharedContentStore,
) -> Option<UiEvent> {
    let SessionEvent { seq, kind } = event;
    let EventKind::ToolOutput {
        batch_id,
        call_id,
        name,
        output,
    } = kind
    else {
        return UiEvent::from_core(SessionEvent::new(seq, kind));
    };
    let handle = ContentHandle(format!("tool:{run_id}:{batch_id}:{call_id}").into());
    let preview = bounded_tool_output_preview(&output);
    let content_handle = store
        .lock()
        .map(|mut store| store.insert_owned(handle.clone(), output))
        .is_ok()
        .then_some(handle);
    let mut projected = UiEvent::from_core(SessionEvent::new(
        seq,
        EventKind::ToolOutput {
            batch_id,
            call_id,
            name,
            output: preview,
        },
    ))?;
    if let UiEvent::ToolProgress {
        content_handle: projected_handle,
        ..
    } = &mut projected
    {
        *projected_handle = content_handle;
    }
    Some(projected)
}

fn bounded_tool_output_preview(output: &str) -> String {
    let mut chars = output.lines().next().unwrap_or("").chars();
    let mut preview = chars.by_ref().take(512).collect::<String>();
    if chars.next().is_some() {
        preview.push('.');
    }
    preview
}

fn record_completed_turn(options: &mut ProviderRunOptions, user: &str, assistant: &str) {
    options.history.push(ProviderMessage::user(user));
    options
        .history
        .push(ProviderMessage::assistant(assistant, Vec::new()));
}

fn request_manual_compaction(options: &ProviderRunOptions, instructions: String, sink: &EventSink) {
    let result = options
        .compaction
        .as_ref()
        .ok_or("compaction is unavailable")
        .and_then(|handle| handle.request_manual(instructions));
    let message = match result {
        Ok(()) => "Compaction queued for the next safe boundary".to_owned(),
        Err(error) => error.to_owned(),
    };
    let _ = sink.send(UiEvent::Notification { message });
}

fn start_active_run(
    run_id: u64,
    launch: ActiveRunLaunch,
    resume_path: Option<PathBuf>,
    resume_preflight: Option<SessionPreflight>,
    sink: EventSink,
    content_store: SharedContentStore,
) -> Result<ActiveRun, String> {
    let ActiveRunLaunch {
        request,
        mut options,
        skill_instructions,
    } = launch;
    let workspace_root = options.workspace_root.clone().map_or_else(
        || std::env::current_dir().map_err(|error| format!("current directory: {error}")),
        Ok,
    )?;
    options.workspace_root = Some(workspace_root.clone());
    let cwd_display = display_workspace_path(&workspace_root);
    let cancellation = CancellationToken::new();
    let (core_tx, core_rx) = SessionEventSender::bounded(1_024, cancellation.clone());
    let projector_cancellation = cancellation.clone();
    let projector_content_store = content_store.clone();
    let projector = thread::Builder::new()
        .name("slim-tui-projector".into())
        .spawn(move || {
            for event in core_rx {
                if let Some(event) = project_core_event(event, run_id, &projector_content_store) {
                    let event = attach_workspace_to_snapshot(event, &cwd_display);
                    let event = associate_projected_run(event, run_id);
                    if !sink.send_projected(event, &projector_cancellation) {
                        break;
                    }
                }
            }
        })
        .map_err(|error| format!("TUI projector thread: {error}"))?;
    options.cancellation = Some(cancellation.clone());
    options.allow_plan_loop = true;
    let durable = resume_path.is_some();
    let (runtime_interaction_route, interaction_responder) = interaction_route();
    let task = tokio::spawn(async move {
        if let Some(preflight) = resume_preflight {
            run_provider_resume_with_preflight_events_interactive_async(
                request,
                preflight,
                options,
                skill_instructions,
                Some(core_tx),
                runtime_interaction_route,
            )
            .await
        } else if let Some(path) = resume_path {
            let preflight = tokio::task::spawn_blocking(move || {
                slim_core::session::preflight_session(path).map_err(|error| {
                    ProviderError::InvalidResponse {
                        message: format!("durable resume: {error}"),
                    }
                })
            })
            .await
            .map_err(|error| ProviderError::InvalidResponse {
                message: format!("durable TUI worker: {error}"),
            })??;
            run_provider_resume_with_preflight_events_interactive_async(
                request,
                preflight,
                options,
                skill_instructions,
                Some(core_tx),
                runtime_interaction_route,
            )
            .await
        } else {
            execute_provider_turn_async(
                request,
                None,
                options,
                skill_instructions,
                Some(core_tx),
                Some(runtime_interaction_route),
            )
            .await
        }
    });
    Ok(ActiveRun {
        run_id,
        task,
        projector,
        cancellation,
        durable,
        content_store,
        interaction_responder: Some(interaction_responder),
    })
}

async fn abort_active(active: &mut Option<ActiveRun>) -> Option<PendingRun> {
    if let Some(mut run) = active.take() {
        let durable = run.durable;
        run.cancellation.cancel();
        let result = match tokio::time::timeout(Duration::from_secs(2), &mut run.task).await {
            Ok(result) => result,
            Err(_) => {
                run.task.abort();
                (&mut run.task).await
            }
        };
        Some(PendingRun {
            run_id: run.run_id,
            result: Some(result),
            projector: Some(run.projector),
            delivery: VecDeque::new(),
            cancellation: run.cancellation,
            durable,
            cancel_requested: true,
            content_store: run.content_store,
        })
    } else {
        None
    }
}

fn run_stop_message(execution: &ProviderExecution) -> String {
    if execution.result.stop == "provider_error" {
        return execution.result.text.clone();
    }
    format_run_stop_message(
        &execution.result.stop,
        &execution.tool_results,
        ToolLoopLimits {
            max_mutating_tool_calls: execution.max_mutating_tool_calls,
            max_read_tool_calls: execution.max_read_tool_calls,
            max_turns: execution.max_turns,
            max_output_tokens: execution.max_output_tokens,
        },
    )
}

fn send_cancel_result(
    run_id: u64,
    result: Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>,
    sink: &EventSink,
) {
    let event = match result {
        Ok(Ok(execution)) => match execution.result.code {
            ExitCode::Success => UiEvent::RunCompleted { run_id },
            ExitCode::Cancelled => UiEvent::RunCancelled { run_id },
            _ => UiEvent::RunStopped {
                run_id,
                message: run_stop_message(&execution),
            },
        },
        Ok(Err(ProviderError::Cancelled)) => UiEvent::RunCancelled { run_id },
        Ok(Err(error)) => UiEvent::RunFailed {
            run_id: Some(run_id),
            message: provider_error_message(error),
        },
        Err(error) if error.is_cancelled() => UiEvent::RunCancelled { run_id },
        Err(_) => UiEvent::RunFailed {
            run_id: Some(run_id),
            message: "TUI runtime task failed".into(),
        },
    };
    // Cancellation authorizes abandoning the buffered presentation tail. Send
    // its truthful terminal on control so a full stream lane cannot hide it;
    // run identity prevents an overtaken start from clearing this tombstone.
    sink.send_control(event);
}

fn execution_result_events(
    run_id: u64,
    result: Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>,
    durable_streamed: bool,
) -> VecDeque<UiEvent> {
    let mut events = VecDeque::new();
    let terminal = match result {
        Ok(Ok(execution)) => {
            if !durable_streamed && execution.events.is_empty() && !execution.result.text.is_empty()
            {
                events.push_back(UiEvent::Notification {
                    message: execution.result.text.clone(),
                });
            }
            match execution.result.code {
                ExitCode::Success => UiEvent::RunCompleted { run_id },
                ExitCode::Cancelled => UiEvent::RunCancelled { run_id },
                _ => UiEvent::RunStopped {
                    run_id,
                    message: run_stop_message(&execution),
                },
            }
        }
        Ok(Err(error)) => UiEvent::RunFailed {
            run_id: Some(run_id),
            message: provider_error_message(error),
        },
        Err(_) => UiEvent::RunFailed {
            run_id: Some(run_id),
            message: "TUI runtime task failed".into(),
        },
    };
    events.push_back(terminal);
    events
}

fn redact_for_ui(input: &str, secret: &str) -> String {
    if secret.is_empty() {
        input.to_owned()
    } else {
        input.replace(secret, "[REDACTED]")
    }
}

fn tui_provider_error(error: ProviderError) -> TuiError {
    let code = match &error {
        ProviderError::Cancelled => ExitCode::Cancelled,
        ProviderError::Transport { .. }
        | ProviderError::MalformedToolCall
        | ProviderError::Remote { .. } => ExitCode::Provider,
        ProviderError::InvalidResponse { .. } => ExitCode::Internal,
    };
    TuiError::new(code, provider_error_message(error))
}

fn provider_error_message(error: ProviderError) -> String {
    match error {
        ProviderError::Cancelled => "provider request cancelled".into(),
        ProviderError::Transport { .. } => "provider transport failed".into(),
        ProviderError::MalformedToolCall => "provider returned a malformed tool call".into(),
        ProviderError::Remote { message } | ProviderError::InvalidResponse { message } => message,
    }
}

#[cfg(test)]
mod local_session_tests {
    use std::path::Path;

    use slim_core::session::{
        DurableOperation, DurableOperationKind, DurableOutcome, DurableRecord, DurableRepo,
        DurableSessionHeader, JsonlRepo, ManualDrive, ManualExecutor, ManualRunSpec,
        ProviderResponse,
    };

    use super::select_previous_tui_session;

    struct FixtureExecutor;
    struct FailingExecutor;

    impl ManualExecutor for FixtureExecutor {
        type Error = std::io::Error;

        fn execute(
            &mut self,
            _effect: &slim_core::session::Effect,
        ) -> Result<ProviderResponse, Self::Error> {
            Ok(ProviderResponse::new("previous answer", None))
        }
    }

    impl ManualExecutor for FailingExecutor {
        type Error = std::io::Error;

        fn execute(
            &mut self,
            _effect: &slim_core::session::Effect,
        ) -> Result<ProviderResponse, Self::Error> {
            Err(std::io::Error::other("fixture failure"))
        }
    }

    fn create_completed(path: &Path, id: &str, cwd: &Path) {
        let cwd = std::fs::canonicalize(cwd).expect("canonical cwd");
        let header =
            DurableSessionHeader::new(id, "1", cwd.to_str().expect("unicode cwd"), None, None);
        let mut repo = JsonlRepo::create(path, header).expect("create session");
        ManualDrive::new(&mut repo, &mut FixtureExecutor)
            .run(ManualRunSpec::new(
                format!("{id}-operation"),
                format!("{id}-attempt"),
                format!("{id}-user"),
                format!("{id}-assistant"),
                "previous question",
                0,
            ))
            .expect("complete turn");
    }

    fn create_failed_terminal(path: &Path, id: &str, cwd: &Path) {
        let cwd = std::fs::canonicalize(cwd).expect("canonical cwd");
        let header =
            DurableSessionHeader::new(id, "3", cwd.to_str().expect("unicode cwd"), None, None);
        let mut repo = JsonlRepo::create(path, header).expect("create failed session");
        let operation_id = format!("{id}-operation");
        let result = ManualDrive::new(&mut repo, &mut FailingExecutor).run(ManualRunSpec::new(
            operation_id.clone(),
            format!("{id}-attempt"),
            format!("{id}-user"),
            format!("{id}-assistant"),
            "failed question",
            0,
        ));
        assert!(result.is_err());
        let seq = repo.next_seq().expect("next failed seq");
        repo.append(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id,
                kind: DurableOperationKind::Finished {
                    outcome: DurableOutcome::Failed,
                },
            },
        })
        .expect("finish failed operation");
    }

    #[test]
    fn previous_session_selection_stays_in_the_current_workspace() {
        let root = std::env::temp_dir().join(format!(
            "slim-local-resume-{}-{}",
            std::process::id(),
            super::system_time_nanos(std::time::SystemTime::now())
        ));
        let foreign = root.with_extension("foreign");
        std::fs::create_dir_all(root.join(".slim/sessions")).expect("session directory");
        std::fs::create_dir_all(&foreign).expect("foreign directory");
        let sessions = root.join(".slim/sessions");
        create_completed(&sessions.join("tui-valid.jsonl"), "tui-valid", &root);
        create_completed(&sessions.join("tui-foreign.jsonl"), "tui-foreign", &foreign);
        let current = sessions.join("tui-current.jsonl");
        let current_header = DurableSessionHeader::new(
            "tui-current",
            "2",
            std::fs::canonicalize(&root)
                .expect("canonical root")
                .to_str()
                .expect("unicode root"),
            None,
            None,
        );
        drop(JsonlRepo::create(&current, current_header).expect("current session"));
        create_failed_terminal(&sessions.join("tui-failed.jsonl"), "tui-failed", &root);

        let selected = select_previous_tui_session(&root, Some(&current))
            .expect("selection")
            .expect("previous session");
        assert_eq!(selected.preflight.session_id.as_deref(), Some("tui-valid"));
        assert_eq!(selected.history[0].content, "previous question");
        assert_eq!(selected.history[1].content, "previous answer");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&foreign);
    }
}

#[cfg(test)]
mod slash_skill_tests {
    use super::{resolve_slash_skill_command, SlashSkillCommand, MAX_SLASH_SKILL_BODY_BYTES};

    #[test]
    fn slash_skill_metadata_is_lazy_and_body_loads_only_for_a_task() {
        let root = std::env::temp_dir().join(format!(
            "slim-slash-skill-{}-{}",
            std::process::id(),
            super::system_time_nanos(std::time::SystemTime::now())
        ));
        let skill_dir = root.join(".slim/skills/review-code");
        let skill_file = skill_dir.join("SKILL.md");
        std::fs::create_dir_all(&skill_dir).expect("skill directory");
        let native_collision = root.join(".slim/skills/models");
        std::fs::create_dir_all(&native_collision).expect("collision directory");
        std::fs::write(
            native_collision.join("SKILL.md"),
            "---\nname: models\ndescription: Must not shadow the native command\n---\nIgnored.\n",
        )
        .expect("collision fixture");

        let mut metadata_with_invalid_body =
            b"---\nname: review-code\ndescription: Review code\n---\n".to_vec();
        metadata_with_invalid_body.push(0xff);
        std::fs::write(&skill_file, metadata_with_invalid_body).expect("lazy skill fixture");

        assert!(matches!(
            resolve_slash_skill_command(&root, "/models"),
            Ok(None)
        ));
        assert!(matches!(
            resolve_slash_skill_command(&root, "/review-code"),
            Ok(Some(SlashSkillCommand::Selected { name })) if name == "review-code"
        ));

        std::fs::write(
            &skill_file,
            "---\nname: review-code\ndescription: Review code\n---\nInspect the change carefully.\n",
        )
        .expect("invokable skill fixture");
        assert!(matches!(
            resolve_slash_skill_command(&root, "/review-code check this"),
            Ok(Some(SlashSkillCommand::Invoke { name, body, source }))
                if name == "review-code"
                    && body == "Inspect the change carefully.\n"
                    && source == skill_file
        ));
        assert!(matches!(
            resolve_slash_skill_command(&root, "please /review-code check this"),
            Ok(Some(SlashSkillCommand::Invoke { name, .. })) if name == "review-code"
        ));

        std::fs::write(
            &skill_file,
            format!(
                "---\nname: review-code\ndescription: Review code\n---\n{}",
                "x".repeat(MAX_SLASH_SKILL_BODY_BYTES)
            ),
        )
        .expect("maximum-size skill fixture");
        assert!(matches!(
            resolve_slash_skill_command(&root, "/review-code check this"),
            Ok(Some(SlashSkillCommand::Invoke { body, .. }))
                if body.len() == MAX_SLASH_SKILL_BODY_BYTES
        ));

        std::fs::write(
            &skill_file,
            format!(
                "---\nname: review-code\ndescription: Review code\n---\n{}",
                "x".repeat(MAX_SLASH_SKILL_BODY_BYTES + 1)
            ),
        )
        .expect("oversized skill fixture");
        let error = match resolve_slash_skill_command(&root, "/review-code check this") {
            Err(error) => error,
            Ok(_) => panic!("oversized skill must be rejected"),
        };
        assert!(error.contains("slash limit"), "{error}");
        assert!(matches!(
            resolve_slash_skill_command(&root, "/missing check this"),
            Ok(None)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::{
        advance_pending_delivery, associate_projected_run, attach_workspace_to_snapshot,
        execution_result_events, project_core_event, project_sync_tui_events, send_cancel_result,
        take_run_id, ContentStore, EventSink, PendingDeliveryStep, PendingRun, WakeSignal,
        CONTENT_ENTRY_BYTES, CONTENT_PAGE_BYTES, CONTENT_STORE_BYTES, CONTENT_STORE_ENTRIES,
    };
    use crate::exit_codes::ExitCode;
    use crate::headless::{ProviderExecution, ProviderHeadlessResult};
    use slim_core::provider::ProviderKind;
    use slim_core::runtime::{AgentLoopConfig, CancellationToken};
    use slim_core::{EventKind, SessionEvent};
    use slim_tui::api::{
        ContentHandle, InteractionRequestId, PageCursor, SessionId, ToolBatchId, ToolCallId,
        UiCommand, UiEvent,
    };
    use std::collections::VecDeque;
    use std::sync::mpsc;

    #[test]
    fn content_store_pages_on_utf8_boundaries_at_sixteen_kibibytes() {
        let handle = ContentHandle("unicode".into());
        let output = "界".repeat(CONTENT_PAGE_BYTES);
        let mut store = ContentStore::default();
        store.insert(handle.clone(), &output);

        let first = store.page(&handle, None).expect("first page");
        assert!(first.text.len() <= CONTENT_PAGE_BYTES);
        assert!(first.text.is_char_boundary(first.text.len()));
        let cursor = first.next_cursor.expect("next cursor");
        assert_eq!(cursor.0 as usize, first.text.len());
        let second = store.page(&handle, Some(cursor)).expect("second page");
        assert!(second.text.len() <= CONTENT_PAGE_BYTES);
        assert!(output.starts_with(&(first.text + &second.text)));
    }

    #[test]
    fn content_store_takes_ownership_without_copying_bounded_output() {
        let handle = ContentHandle("owned".into());
        let mut output = String::with_capacity(8 * 1024);
        output.push_str(&"x".repeat(4 * 1024));
        let allocation = output.as_ptr();
        let mut store = ContentStore::default();

        store.insert_owned(handle.clone(), output);

        let retained = store
            .entries
            .iter()
            .find(|entry| entry.handle == handle)
            .expect("owned output retained");
        assert_eq!(retained.text.as_ptr(), allocation);
    }

    #[test]
    fn content_store_enforces_entry_count_per_output_and_total_byte_caps() {
        let mut store = ContentStore::default();
        for index in 0..=CONTENT_STORE_ENTRIES {
            store.insert(ContentHandle(format!("small-{index}").into()), "x");
        }
        assert!(store.entries.len() <= CONTENT_STORE_ENTRIES);
        assert!(store.page(&ContentHandle("small-0".into()), None).is_err());

        let oversized = "z".repeat(CONTENT_ENTRY_BYTES + 100);
        store.insert(ContentHandle("oversized".into()), &oversized);
        let retained = store
            .entries
            .iter()
            .find(|entry| entry.handle == ContentHandle("oversized".into()))
            .expect("oversized retained");
        assert!(retained.text.len() <= CONTENT_ENTRY_BYTES);
        assert!(retained.text.ends_with("[output truncated at 2 MiB]"));

        for index in 0..8 {
            store.insert(
                ContentHandle(format!("large-{index}").into()),
                &"y".repeat(CONTENT_ENTRY_BYTES),
            );
        }
        assert!(store.retained_bytes <= CONTENT_STORE_BYTES);
    }

    #[test]
    fn projector_namespaces_and_registers_redacted_tool_output() {
        let store = Default::default();
        let projected = project_core_event(
            SessionEvent::new(
                3,
                EventKind::ToolOutput {
                    batch_id: "batch".into(),
                    call_id: "call".into(),
                    name: "read".into(),
                    output: "safe [REDACTED]".into(),
                },
            ),
            7,
            &store,
        )
        .expect("projected");
        let UiEvent::ToolProgress {
            content_handle: Some(handle),
            ..
        } = projected
        else {
            panic!("expected paged tool progress");
        };
        assert_eq!(&*handle.0, "tool:7:batch:call");
        let page = store
            .lock()
            .expect("store")
            .page(&handle, Some(PageCursor(0)))
            .expect("page");
        assert_eq!(page.text, "safe [REDACTED]");
    }

    #[test]
    fn session_snapshot_receives_resolved_workspace_display() {
        let event = attach_workspace_to_snapshot(
            UiEvent::SessionSnapshot {
                session_id: SessionId("session".into()),
                cwd: String::new(),
            },
            r"D:\Slim",
        );
        assert_eq!(
            event,
            UiEvent::SessionSnapshot {
                session_id: SessionId("session".into()),
                cwd: r"D:\Slim".into(),
            }
        );
    }

    #[test]
    fn run_identity_exhaustion_fails_closed_without_reuse() {
        let mut next = u64::MAX - 1;
        assert_eq!(take_run_id(&mut next), Some(u64::MAX - 1));
        assert_eq!(next, u64::MAX);
        assert_eq!(take_run_id(&mut next), None);
        assert_eq!(take_run_id(&mut next), None);
    }

    fn execution(code: ExitCode) -> ProviderExecution {
        ProviderExecution {
            result: ProviderHeadlessResult {
                code,
                provider: ProviderKind::OpenAiCompatible,
                model: "fixture".into(),
                text: String::new(),
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                stop: "fixture".into(),
                cost_micros: None,
                usage_complete: false,
                usage_overflowed: false,
                usage: slim_core::UsageTotals::default(),
                costs: crate::headless::UsageCostSummary::default(),
                validation_source: None,
                tool_summary_lines: Vec::new(),
            },
            history: None,
            events: Vec::new(),
            tool_results: Vec::new(),
            max_mutating_tool_calls: AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            max_read_tool_calls: AgentLoopConfig::DEFAULT_MAX_READ_TOOL_CALLS,
            max_turns: AgentLoopConfig::DEFAULT_MAX_TURNS,
            max_output_tokens: slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS,
            resume_preflight: None,
        }
    }

    fn sink() -> (EventSink, mpsc::Receiver<UiEvent>, mpsc::Receiver<UiEvent>) {
        let (control, control_rx) = mpsc::sync_channel(4);
        let (data, data_rx) = mpsc::sync_channel(4);
        (
            EventSink {
                control: Some(control),
                data: Some(data),
                wake: WakeSignal::default(),
                lane_space: WakeSignal::default(),
                drop_probe: None,
            },
            control_rx,
            data_rx,
        )
    }

    #[cfg(windows)]
    #[test]
    fn final_sender_teardown_precedes_wake_probe() {
        let (mut sink, control_rx, data_rx) = sink();
        let wake = sink.wake.clone();
        let reached_pre_wake = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release_wake = std::sync::Arc::new(std::sync::Barrier::new(2));
        sink.drop_probe = Some(super::DropProbe {
            reached_pre_wake: reached_pre_wake.clone(),
            release_wake: release_wake.clone(),
        });
        let dropper = std::thread::spawn(move || drop(sink));

        reached_pre_wake.wait();
        assert!(matches!(
            control_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert!(matches!(
            data_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert!(
            !wake
                .wait_timeout(std::time::Duration::ZERO)
                .expect("wake remains unsignaled at pre-wake barrier"),
            "wake cannot precede sender disconnection"
        );
        release_wake.wait();
        dropper.join().expect("drop completes after wake release");
        assert!(
            wake.wait_timeout(std::time::Duration::from_millis(50))
                .expect("final wake is observable"),
            "teardown must emit its final wake"
        );
    }

    #[cfg(windows)]
    #[test]
    fn projected_send_resumes_when_lane_space_is_signaled() {
        let (sink, _control_rx, data_rx) = sink();
        let cancellation = CancellationToken::new();
        for _ in 0..4 {
            assert!(sink.send(UiEvent::AssistantDelta { text: "x".into() }));
        }
        let space = sink.lane_space.clone();
        let handle = std::thread::spawn(move || {
            sink.send_projected(UiEvent::AssistantDelta { text: "y".into() }, &cancellation)
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = data_rx.recv().expect("drain one data event");
        space.notify();
        assert!(handle.join().expect("projected send completes"));
    }

    #[test]
    fn normal_projector_keeps_request_accounting_and_fence_on_stream_lane() {
        let (sink, control_rx, data_rx) = sink();
        let cancellation = CancellationToken::new();
        for event in [
            UiEvent::UsageEstimate {
                request_id: 1,
                context_tokens: 11,
                context_window_tokens: 100,
            },
            UiEvent::Usage {
                input_tokens: 7,
                output_tokens: 3,
            },
            UiEvent::AssistantEnded,
        ] {
            assert!(sink.send_projected(event, &cancellation));
        }
        assert!(matches!(
            control_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(
            data_rx.recv().expect("snapshot"),
            UiEvent::UsageEstimate { request_id: 1, .. }
        ));
        assert!(matches!(
            data_rx.recv().expect("usage"),
            UiEvent::Usage { .. }
        ));
        assert_eq!(
            data_rx.recv().expect("assistant fence"),
            UiEvent::AssistantEnded
        );
    }

    #[test]
    fn cancelled_projector_drops_visual_progress_and_migrates_causal_suffix() {
        let (sink, control_rx, data_rx) = sink();
        let tool_started = UiEvent::ToolStarted {
            batch_id: ToolBatchId("batch-1".into()),
            call_id: ToolCallId("call-1".into()),
            name: "shell".into(),
            arguments_summary: "command=safe".into(),
        };
        for index in 0..4 {
            sink.data()
                .send(UiEvent::ToolProgress {
                    batch_id: ToolBatchId("batch-1".into()),
                    call_id: ToolCallId(format!("queued-{index}").into()),
                    name: "shell".into(),
                    preview: format!("fills capacity {index}"),
                    content_handle: None,
                })
                .expect("fill");
        }
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(sink.send_projected(
            UiEvent::Notification {
                message: "discard me".into(),
            },
            &cancellation,
        ));

        let sender = sink.clone();
        let token = cancellation.clone();
        let projector = std::thread::spawn(move || {
            sender.send_projected(
                UiEvent::UsageEstimate {
                    request_id: 9,
                    context_tokens: 11,
                    context_window_tokens: 100,
                },
                &token,
            )
        });
        assert!(projector.join().expect("projector"));
        assert!(matches!(
            control_rx.recv().expect("causal telemetry on control"),
            UiEvent::UsageEstimate { request_id: 9, .. }
        ));
        assert!(sink.send_projected(UiEvent::AssistantEnded, &cancellation));
        assert_eq!(
            control_rx.recv().expect("exactness fence on control"),
            UiEvent::AssistantEnded
        );
        assert!(sink.send_projected(tool_started.clone(), &cancellation));
        assert_eq!(
            control_rx.recv().expect("tool start migrates to control"),
            tool_started
        );
        let tool_progress = UiEvent::ToolProgress {
            batch_id: ToolBatchId("batch-1".into()),
            call_id: ToolCallId("call-1".into()),
            name: "shell".into(),
            preview: "partial output".into(),
            content_handle: Some(ContentHandle("content-1".into())),
        };
        assert!(sink.send_projected(tool_progress.clone(), &cancellation));
        let tool_ended = UiEvent::ToolEnded {
            batch_id: ToolBatchId("batch-1".into()),
            call_id: ToolCallId("call-1".into()),
            name: "shell".into(),
            success: false,
            duration_ms: 9,
        };
        assert!(sink.send_projected(tool_ended.clone(), &cancellation));
        assert_eq!(
            control_rx
                .recv()
                .expect("tool terminal migrates to control"),
            tool_ended
        );
        let remaining = data_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(remaining.len(), 4, "full data lane remains untouched");
        assert!(!remaining.contains(&tool_progress));
        assert!(!remaining.contains(&tool_ended));
    }

    #[test]
    fn projector_associates_fatal_error_with_active_run() {
        assert_eq!(
            associate_projected_run(
                UiEvent::FatalError {
                    run_id: None,
                    message: "fatal".into(),
                },
                7,
            ),
            UiEvent::FatalError {
                run_id: Some(7),
                message: "fatal".into(),
            }
        );
        assert_eq!(
            associate_projected_run(
                UiEvent::UsageEstimate {
                    request_id: 1,
                    context_tokens: 10,
                    context_window_tokens: 100,
                },
                7,
            ),
            UiEvent::UsageEstimateForRun {
                run_id: 7,
                request_id: 1,
                context_tokens: 10,
                context_window_tokens: 100,
            }
        );
    }

    #[test]
    fn projector_namespaces_tool_identity_by_run() {
        let tool = UiEvent::ToolStarted {
            batch_id: ToolBatchId("batch-1".into()),
            call_id: ToolCallId("call-1".into()),
            name: "read".into(),
            arguments_summary: "{}".into(),
        };
        let first = associate_projected_run(tool.clone(), 1);
        let second = associate_projected_run(tool, 2);
        let identities = [first, second].map(|event| match event {
            UiEvent::ToolStarted {
                batch_id, call_id, ..
            } => (batch_id, call_id),
            _ => panic!("tool start"),
        });
        assert_ne!(identities[0], identities[1]);
    }

    #[test]
    fn projector_namespaces_interaction_identity_and_matching_ack_by_run() {
        let request = UiEvent::InputRequired {
            request_id: InteractionRequestId("input-1".into()),
            prompt: "choose".into(),
            options: Vec::new(),
            persisted: true,
        };
        let ack = UiEvent::InteractionAcknowledged {
            request_id: InteractionRequestId("input-1".into()),
            accepted: true,
            message: "accepted".into(),
        };
        let first = associate_projected_run(request.clone(), 1);
        let second = associate_projected_run(request, 2);
        let first_ack = associate_projected_run(ack, 1);

        let UiEvent::InputRequired {
            request_id: first_id,
            ..
        } = first
        else {
            panic!("input request")
        };
        let UiEvent::InputRequired {
            request_id: second_id,
            ..
        } = second
        else {
            panic!("input request")
        };
        let UiEvent::InteractionAcknowledged {
            request_id: ack_id, ..
        } = first_ack
        else {
            panic!("interaction ack")
        };
        assert_ne!(first_id, second_id);
        assert_eq!(first_id, ack_id);
    }

    #[test]
    fn cancelled_projector_preserves_interaction_requests() {
        let (sink, control_rx, _data_rx) = sink();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let request = UiEvent::InputRequired {
            request_id: InteractionRequestId("input-cancelled".into()),
            prompt: "persist me".into(),
            options: Vec::new(),
            persisted: true,
        };

        assert!(sink.send_projected(request.clone(), &cancellation));
        assert_eq!(control_rx.recv().expect("preserved request"), request);
    }

    #[test]
    fn synchronous_projection_namespaces_reused_tool_identity_per_turn() {
        let core_events = || {
            vec![SessionEvent::new(
                1,
                EventKind::ToolStarted {
                    batch_id: "batch-1".into(),
                    call_id: "call-1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            )]
        };
        let first = project_sync_tui_events("one".into(), core_events()).expect("first turn");
        let second = project_sync_tui_events("two".into(), core_events()).expect("second turn");
        let identities = [first, second].map(|events| match &events[1] {
            UiEvent::ToolStarted {
                batch_id, call_id, ..
            } => (batch_id.clone(), call_id.clone()),
            _ => panic!("tool start"),
        });
        assert_ne!(identities[0], identities[1]);
    }

    #[test]
    fn exact_full_pending_delivery_services_cancel_without_losing_truth() {
        let (sink, control_rx, _data_rx) = sink();
        for index in 0..4 {
            sink.data()
                .send(UiEvent::Notification {
                    message: format!("fills capacity {index}"),
                })
                .expect("fill");
        }
        let cancellation = CancellationToken::new();
        let mut run = PendingRun {
            run_id: 7,
            result: None,
            projector: None,
            delivery: execution_result_events(7, Ok(Ok(execution(ExitCode::Success))), false),
            cancellation: cancellation.clone(),
            durable: false,
            cancel_requested: false,
            content_store: Default::default(),
        };
        let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        commands.send(UiCommand::CancelRun).expect("cancel");

        assert_eq!(
            advance_pending_delivery(&mut run, &mut command_rx, &sink),
            PendingDeliveryStep::Complete
        );
        assert!(cancellation.is_cancelled());
        assert_eq!(
            control_rx.recv().expect("truthful terminal"),
            UiEvent::RunCompleted { run_id: 7 }
        );
    }

    #[test]
    fn exact_full_pending_delivery_services_shutdown() {
        let (sink, control_rx, _data_rx) = sink();
        for index in 0..4 {
            sink.data()
                .send(UiEvent::Notification {
                    message: format!("fills capacity {index}"),
                })
                .expect("fill");
        }
        let cancellation = CancellationToken::new();
        let mut run = PendingRun {
            run_id: 7,
            result: None,
            projector: None,
            delivery: execution_result_events(7, Ok(Ok(execution(ExitCode::Success))), false),
            cancellation: cancellation.clone(),
            durable: false,
            cancel_requested: false,
            content_store: Default::default(),
        };
        let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        commands.send(UiCommand::Shutdown).expect("shutdown");

        assert_eq!(
            advance_pending_delivery(&mut run, &mut command_rx, &sink),
            PendingDeliveryStep::Shutdown
        );
        assert!(cancellation.is_cancelled());
        assert_eq!(
            control_rx.recv().expect("shutdown event"),
            UiEvent::Shutdown
        );
    }

    #[test]
    fn exact_full_pending_delivery_queues_unbound_interaction_ack_without_blocking() {
        let (sink, control_rx, _data_rx) = sink();
        for index in 0..4 {
            sink.data()
                .send(UiEvent::Notification {
                    message: format!("fills capacity {index}"),
                })
                .expect("fill");
        }
        let request_id = InteractionRequestId("pending-input".into());
        let mut run = PendingRun {
            run_id: 7,
            result: None,
            projector: None,
            delivery: execution_result_events(7, Ok(Ok(execution(ExitCode::Success))), false),
            cancellation: CancellationToken::new(),
            durable: false,
            cancel_requested: false,
            content_store: Default::default(),
        };
        let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        commands
            .send(UiCommand::AnswerInput {
                request_id: request_id.clone(),
                answer: "answer".into(),
            })
            .expect("answer");

        assert_eq!(
            advance_pending_delivery(&mut run, &mut command_rx, &sink),
            PendingDeliveryStep::Pending
        );
        assert_eq!(
            run.delivery.back(),
            Some(&UiEvent::InteractionAcknowledged {
                request_id: request_id.clone(),
                accepted: false,
                message: "interaction route unavailable in this host".into(),
            })
        );

        commands.send(UiCommand::CancelRun).expect("cancel");
        assert_eq!(
            advance_pending_delivery(&mut run, &mut command_rx, &sink),
            PendingDeliveryStep::Complete
        );
        assert_eq!(
            control_rx.recv().expect("terminal remains authoritative"),
            UiEvent::RunCompleted { run_id: 7 }
        );
        assert_eq!(
            control_rx.recv().expect("ack remains visible"),
            UiEvent::InteractionAcknowledged {
                request_id,
                accepted: false,
                message: "interaction route unavailable in this host".into(),
            }
        );
    }

    #[test]
    fn pending_run_cancel_is_recorded_and_interrupts_projector() {
        let cancellation = CancellationToken::new();
        let mut run = PendingRun {
            run_id: 1,
            result: Some(Ok(Ok(execution(ExitCode::Success)))),
            projector: Some(std::thread::spawn(|| {})),
            delivery: VecDeque::new(),
            cancellation: cancellation.clone(),
            durable: false,
            cancel_requested: false,
            content_store: Default::default(),
        };

        run.request_cancel();

        assert!(run.cancel_requested);
        assert!(cancellation.is_cancelled());
        run.projector
            .take()
            .expect("projector")
            .join()
            .expect("join");
    }

    #[test]
    fn cancel_after_durable_success_emits_completed_from_task_result() {
        let (sink, control_rx, _data_rx) = sink();
        send_cancel_result(1, Ok(Ok(execution(ExitCode::Success))), &sink);
        assert_eq!(
            control_rx.recv().expect("terminal event"),
            UiEvent::RunCompleted { run_id: 1 }
        );
    }

    #[test]
    fn cancellation_terminal_result_emits_cancelled() {
        let (sink, control_rx, _data_rx) = sink();
        send_cancel_result(1, Ok(Ok(execution(ExitCode::Cancelled))), &sink);
        assert_eq!(
            control_rx.recv().expect("terminal event"),
            UiEvent::RunCancelled { run_id: 1 }
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, OnceLock};

    use crate::oauth::{BrowserLauncher, OAuthEndpoints, OAuthError, OAuthService, OAuthStore};

    use super::{prepare_tui, spawn_tui_session, TuiStartup};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn wait_for_auth_provider(
        channels: &slim_tui::api::UiChannels,
        expected: super::LoginProvider,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if matches!(
                channels.events.try_recv(),
                Ok(super::UiEvent::AuthStateChanged {
                    provider: Some(provider),
                    authenticated: true,
                }) if provider == expected
            ) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("saved provider was not activated: {expected:?}");
    }

    struct NoBrowser;

    impl BrowserLauncher for NoBrowser {
        fn open(&self, _url: &str) -> Result<(), OAuthError> {
            Ok(())
        }
    }

    #[test]
    fn known_text_only_model_rejects_tui_image_before_send() {
        let mut request = crate::ProviderRequest {
            prompt: String::new(),
            mode: slim_core::OperatingMode::Auto,
            kind: slim_core::provider::ProviderKind::OpenCodeGo,
            endpoint: slim_core::provider::OPENCODE_GO_BASE_URL.into(),
            model: "deepseek-v4-flash".into(),
            api_key: "fixture-key".into(),
            account_id: None,
            timeout: std::time::Duration::from_secs(120),
        };
        assert_eq!(
            super::image_model_error(Some(&request)),
            Some("OpenCode Go model deepseek-v4-flash does not accept images".into())
        );

        request.model = "deepseek-v4-flash-vision-exp".into();
        assert_eq!(super::image_model_error(Some(&request)), None);
    }

    #[test]
    fn text_only_model_restores_prompt_when_attachment_is_pending() {
        let root = std::env::temp_dir().join(format!(
            "slim-tui-image-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let oauth = OAuthService::new(
            OAuthEndpoints::default(),
            Arc::new(NoBrowser),
            OAuthStore::at(root.join("auth.json")),
        )
        .expect("oauth");
        let startup = TuiStartup {
            request: Some(crate::ProviderRequest {
                prompt: String::new(),
                mode: slim_core::OperatingMode::Auto,
                kind: slim_core::provider::ProviderKind::OpenCodeGo,
                endpoint: slim_core::provider::OPENCODE_GO_BASE_URL.into(),
                model: "deepseek-v4-flash".into(),
                api_key: "fixture-key".into(),
                account_id: None,
                timeout: std::time::Duration::from_secs(120),
            }),
            oauth_session: None,
            options: crate::ProviderRunOptions::default(),
            initial_prompt: None,
            image_labels: vec!["screen.png".into()],
            resume_path: None,
            resume_preflight: None,
            persist_sessions: false,
            mode: slim_core::OperatingMode::Auto,
            effort: super::ReasoningEffort::High,
            endpoint_override: None,
            model_override: None,
            timeout: std::time::Duration::from_secs(120),
        };
        let (runtime, channels) = spawn_tui_session(startup, oauth).expect("runtime");
        channels
            .commands
            .send(super::UiCommand::SendPrompt("keep this draft".into()))
            .expect("send prompt");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut restored = false;
        let mut rejected = false;
        let mut errors = Vec::new();
        while std::time::Instant::now() < deadline && !(restored && rejected) {
            let mut received = false;
            for event in channels
                .events
                .try_iter()
                .chain(channels.events_data.try_iter())
            {
                received = true;
                match event {
                    super::UiEvent::RestoreDraft { text } => {
                        restored = text == "keep this draft";
                    }
                    super::UiEvent::RunFailed { message, .. } => {
                        rejected = message.contains("does not accept images");
                        errors.push(message);
                    }
                    _ => {}
                }
            }
            if !received {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
        assert!(restored, "prompt was not restored");
        assert!(
            rejected,
            "model incompatibility was not visible: {errors:?}"
        );
    }

    #[test]
    fn tui_preparation_succeeds_without_any_credential() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = std::env::temp_dir().join(format!("slim-signed-out-{}", std::process::id()));
        let auth = root.join("missing-auth.json");
        let previous_auth = std::env::var_os("SLIM_AUTH_FILE");
        let previous_slim_key = std::env::var_os("SLIM_API_KEY");
        let previous_codex_key = std::env::var_os("CODEX_ACCESS_TOKEN");
        std::env::set_var("SLIM_AUTH_FILE", &auth);
        std::env::remove_var("SLIM_API_KEY");
        std::env::remove_var("CODEX_ACCESS_TOKEN");
        let oauth = OAuthService::new(
            OAuthEndpoints::default(),
            Arc::new(NoBrowser),
            OAuthStore::at(auth),
        )
        .expect("service");
        let startup = prepare_tui(
            vec!["--tui".into(), "--provider".into(), "codex".into()],
            &oauth,
        )
        .expect("signed-out startup");
        assert!(startup.request.is_none());

        match previous_auth {
            Some(value) => std::env::set_var("SLIM_AUTH_FILE", value),
            None => std::env::remove_var("SLIM_AUTH_FILE"),
        }
        match previous_slim_key {
            Some(value) => std::env::set_var("SLIM_API_KEY", value),
            None => std::env::remove_var("SLIM_API_KEY"),
        }
        match previous_codex_key {
            Some(value) => std::env::set_var("CODEX_ACCESS_TOKEN", value),
            None => std::env::remove_var("CODEX_ACCESS_TOKEN"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn environment_api_key_overrides_active_oauth_for_default_provider() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = std::env::temp_dir().join(format!("slim-auth-priority-{}", std::process::id()));
        let auth = root.join("auth.json");
        let store = OAuthStore::at(&auth);
        store
            .save(
                crate::oauth::OAuthProvider::Anthropic,
                &crate::oauth::OAuthCredential {
                    access: "oauth-access".into(),
                    refresh: "oauth-refresh".into(),
                    expires: u64::MAX,
                    account_id: None,
                },
            )
            .expect("oauth store");
        let previous_auth = std::env::var_os("SLIM_AUTH_FILE");
        let previous_openai = std::env::var_os("OPENAI_API_KEY");
        let previous_slim = std::env::var_os("SLIM_API_KEY");
        std::env::set_var("SLIM_AUTH_FILE", &auth);
        std::env::set_var("OPENAI_API_KEY", "environment-key");
        std::env::remove_var("SLIM_API_KEY");
        let oauth = OAuthService::new(OAuthEndpoints::default(), Arc::new(NoBrowser), store)
            .expect("service");
        let startup = prepare_tui(vec!["--tui".into()], &oauth).expect("startup");
        let request = startup.request.expect("provider request");
        assert_eq!(
            request.kind,
            slim_core::provider::ProviderKind::OpenAiCompatible
        );
        assert_eq!(request.api_key, "environment-key");
        assert!(startup.oauth_session.is_none());

        match previous_auth {
            Some(value) => std::env::set_var("SLIM_AUTH_FILE", value),
            None => std::env::remove_var("SLIM_AUTH_FILE"),
        }
        match previous_openai {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match previous_slim {
            Some(value) => std::env::set_var("SLIM_API_KEY", value),
            None => std::env::remove_var("SLIM_API_KEY"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn active_api_key_provider_is_restored_after_restart() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let variables = [
            "SLIM_AUTH_FILE",
            "SLIM_PROVIDER",
            "SLIM_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENCODE_API_KEY",
            "CLINEPASS_API_KEY",
            "COMMANDCODE_API_KEY",
            "CMD_API_KEY",
        ];
        let previous = variables.map(std::env::var_os);
        for name in variables {
            std::env::remove_var(name);
        }

        let cases = [
            slim_core::provider::ProviderKind::OpenAiCompatible,
            slim_core::provider::ProviderKind::Anthropic,
            slim_core::provider::ProviderKind::OpenCodeGo,
            slim_core::provider::ProviderKind::ClinePass,
            slim_core::provider::ProviderKind::CommandCode,
        ];
        let mut observed = Vec::new();
        let mut expected = Vec::new();
        for (index, expected_kind) in cases.into_iter().enumerate() {
            let root = std::env::temp_dir().join(format!(
                "slim-restore-active-{index}-{}",
                std::process::id()
            ));
            let auth = root.join("auth.json");
            let key = format!("persisted-key-{index}");
            crate::save_api_key_file(&auth, expected_kind, &key).expect("save provider key");
            std::env::set_var("SLIM_AUTH_FILE", &auth);

            let oauth = OAuthService::new(
                OAuthEndpoints::default(),
                Arc::new(NoBrowser),
                OAuthStore::at(&auth),
            )
            .expect("restarted service");
            let startup = prepare_tui(vec!["--tui".into()], &oauth).expect("restart startup");
            observed.push(
                startup
                    .request
                    .map(|request| (request.kind, request.api_key)),
            );
            expected.push(Some((expected_kind, key)));
            let _ = std::fs::remove_dir_all(root);
        }

        for (name, value) in variables.into_iter().zip(previous) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        assert_eq!(observed, expected);
    }

    #[test]
    fn active_oauth_provider_is_restored_after_restart() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let variables = [
            "SLIM_AUTH_FILE",
            "SLIM_PROVIDER",
            "SLIM_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "CODEX_ACCESS_TOKEN",
        ];
        let previous = variables.map(std::env::var_os);
        for name in variables {
            std::env::remove_var(name);
        }

        let cases = [
            (
                crate::oauth::OAuthProvider::Anthropic,
                slim_core::provider::ProviderKind::Anthropic,
                None,
                "anthropic",
            ),
            (
                crate::oauth::OAuthProvider::OpenAiCodex,
                slim_core::provider::ProviderKind::OpenAiCodex,
                Some("account-1".to_owned()),
                "codex",
            ),
        ];
        let mut observed = Vec::new();
        let mut explicit_observed = Vec::new();
        let mut expected = Vec::new();
        for (index, (provider, expected_kind, account_id, provider_name)) in
            cases.into_iter().enumerate()
        {
            let root = std::env::temp_dir()
                .join(format!("slim-restore-oauth-{index}-{}", std::process::id()));
            let auth = root.join("auth.json");
            let access = format!("oauth-access-{index}");
            OAuthStore::at(&auth)
                .save(
                    provider,
                    &crate::oauth::OAuthCredential {
                        access: access.clone(),
                        refresh: format!("oauth-refresh-{index}"),
                        expires: u64::MAX,
                        account_id,
                    },
                )
                .expect("save OAuth credential");
            std::env::set_var("SLIM_AUTH_FILE", &auth);

            let oauth = OAuthService::new(
                OAuthEndpoints::default(),
                Arc::new(NoBrowser),
                OAuthStore::at(&auth),
            )
            .expect("restarted service");
            let startup = prepare_tui(vec!["--tui".into()], &oauth).expect("restart startup");
            observed.push((
                startup
                    .request
                    .map(|request| (request.kind, request.api_key)),
                startup.oauth_session.map(|(provider, _)| provider),
            ));
            let explicit = prepare_tui(
                vec!["--tui".into(), "--provider".into(), provider_name.into()],
                &oauth,
            )
            .expect("explicit provider startup");
            explicit_observed.push((
                explicit
                    .request
                    .map(|request| (request.kind, request.api_key)),
                explicit.oauth_session.map(|(provider, _)| provider),
            ));
            expected.push((Some((expected_kind, access)), Some(provider)));
            let _ = std::fs::remove_dir_all(root);
        }

        for (name, value) in variables.into_iter().zip(previous) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        assert_eq!(observed, expected);
        assert_eq!(explicit_observed, expected);
    }

    #[test]
    fn malformed_auth_file_is_reported_instead_of_appearing_signed_out() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let variables = [
            "SLIM_AUTH_FILE",
            "SLIM_PROVIDER",
            "SLIM_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "CODEX_ACCESS_TOKEN",
        ];
        let previous = variables.map(std::env::var_os);
        for name in variables {
            std::env::remove_var(name);
        }
        let root = std::env::temp_dir().join(format!(
            "slim-malformed-auth-startup-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("auth directory");
        let auth = root.join("auth.json");
        std::fs::write(&auth, b"{\"version\":1,").expect("malformed auth fixture");
        std::env::set_var("SLIM_AUTH_FILE", &auth);
        let oauth = OAuthService::new(
            OAuthEndpoints::default(),
            Arc::new(NoBrowser),
            OAuthStore::at(&auth),
        )
        .expect("service");

        let observed = prepare_tui(vec!["--tui".into()], &oauth)
            .err()
            .map(|error| (error.code(), error.to_string()));

        let _ = std::fs::remove_dir_all(root);
        for (name, value) in variables.into_iter().zip(previous) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        assert!(matches!(
            observed,
            Some((crate::ExitCode::Auth, message))
                if message.starts_with("authentication: auth file")
        ));
    }

    #[test]
    fn environment_credentials_do_not_read_a_lower_priority_malformed_auth_file() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let variables = [
            "SLIM_AUTH_FILE",
            "SLIM_PROVIDER",
            "SLIM_API_KEY",
            "OPENAI_API_KEY",
            "OPENCODE_API_KEY",
        ];
        let previous = variables.map(std::env::var_os);
        for name in variables {
            std::env::remove_var(name);
        }
        let root = std::env::temp_dir().join(format!(
            "slim-env-over-malformed-auth-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("auth directory");
        let auth = root.join("auth.json");
        std::fs::write(&auth, b"{\"version\":1,").expect("malformed auth fixture");
        std::env::set_var("SLIM_AUTH_FILE", &auth);
        let oauth = OAuthService::new(
            OAuthEndpoints::default(),
            Arc::new(NoBrowser),
            OAuthStore::at(&auth),
        )
        .expect("service");

        std::env::set_var("OPENAI_API_KEY", "openai-environment");
        let default = prepare_tui(vec!["--tui".into()], &oauth)
            .expect("default environment credential")
            .request
            .map(|request| (request.kind, request.api_key));
        std::env::remove_var("OPENAI_API_KEY");
        std::env::set_var("OPENCODE_API_KEY", "opencode-environment");
        let explicit = prepare_tui(
            vec!["--tui".into(), "--provider".into(), "opencode-go".into()],
            &oauth,
        )
        .expect("explicit environment credential")
        .request
        .map(|request| (request.kind, request.api_key));

        let _ = std::fs::remove_dir_all(root);
        for (name, value) in variables.into_iter().zip(previous) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        assert_eq!(
            default,
            Some((
                slim_core::provider::ProviderKind::OpenAiCompatible,
                "openai-environment".into()
            ))
        );
        assert_eq!(
            explicit,
            Some((
                slim_core::provider::ProviderKind::OpenCodeGo,
                "opencode-environment".into()
            ))
        );
    }

    #[test]
    fn selecting_codex_model_activates_saved_login_from_opencode() {
        let root =
            std::env::temp_dir().join(format!("slim-model-provider-switch-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("auth directory");
        let auth = root.join("auth.json");
        let store = OAuthStore::at(&auth);
        store
            .save(
                crate::oauth::OAuthProvider::OpenAiCodex,
                &crate::oauth::OAuthCredential {
                    access: "codex-access".into(),
                    refresh: "codex-refresh".into(),
                    expires: u64::MAX,
                    account_id: Some("account-1".into()),
                },
            )
            .expect("save Codex login");
        store
            .save_api_key("opencode-go", "opencode-key")
            .expect("make OpenCode active");
        let oauth = OAuthService::new(OAuthEndpoints::default(), Arc::new(NoBrowser), store)
            .expect("service");
        let startup = TuiStartup {
            request: Some(crate::ProviderRequest {
                prompt: String::new(),
                mode: slim_core::OperatingMode::Auto,
                kind: slim_core::provider::ProviderKind::OpenCodeGo,
                endpoint: slim_core::provider::OPENCODE_GO_BASE_URL.into(),
                model: slim_core::provider::OPENCODE_GO_DEFAULT_MODEL.into(),
                api_key: "opencode-key".into(),
                account_id: None,
                timeout: std::time::Duration::from_secs(120),
            }),
            oauth_session: None,
            options: crate::ProviderRunOptions::default(),
            initial_prompt: None,
            image_labels: Vec::new(),
            resume_path: None,
            resume_preflight: None,
            persist_sessions: false,
            mode: slim_core::OperatingMode::Auto,
            effort: super::ReasoningEffort::High,
            endpoint_override: None,
            model_override: None,
            timeout: std::time::Duration::from_secs(120),
        };
        let (runtime, channels) = spawn_tui_session(startup, oauth).expect("runtime");
        channels
            .commands
            .send(super::UiCommand::SetModel {
                model: super::ModelAlias::Sol,
                effort: super::ReasoningEffort::High,
            })
            .expect("select Codex model");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut switched = false;
        while std::time::Instant::now() < deadline {
            if matches!(
                channels.events.try_recv(),
                Ok(super::UiEvent::AuthStateChanged {
                    provider: Some(super::LoginProvider::OpenAiCodex),
                    authenticated: true,
                })
            ) {
                switched = true;
                break;
            }
            if let Ok(super::UiEvent::Notification { message }) = channels.events_data.try_recv() {
                if message.contains("require an OpenAI Codex connection") {
                    panic!("saved Codex login was ignored: {message}");
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(switched, "saved Codex login was not activated");
        channels
            .commands
            .send(super::UiCommand::Shutdown)
            .expect("shutdown");
        drop(runtime);
        assert_eq!(
            OAuthStore::at(&auth)
                .active_provider_key()
                .expect("active provider"),
            Some("openai-codex".into())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn model_overlay_switches_across_all_saved_provider_groups() {
        let root = std::env::temp_dir().join(format!(
            "slim-all-model-provider-switches-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("auth directory");
        let auth = root.join("auth.json");
        crate::auth::save_api_key_file(
            &auth,
            slim_core::provider::ProviderKind::OpenCodeGo,
            "opencode-key",
        )
        .expect("save OpenCode key");
        crate::auth::save_api_key_file(
            &auth,
            slim_core::provider::ProviderKind::ClinePass,
            "clinepass-key",
        )
        .expect("save ClinePass key");
        crate::auth::save_api_key_file(
            &auth,
            slim_core::provider::ProviderKind::CommandCode,
            "command-code-key",
        )
        .expect("save Command Code key");
        let store = OAuthStore::at(&auth);
        let codex_credential = crate::oauth::OAuthCredential {
            access: "codex-access".into(),
            refresh: "codex-refresh".into(),
            expires: u64::MAX,
            account_id: Some("account-1".into()),
        };
        store
            .save(crate::oauth::OAuthProvider::OpenAiCodex, &codex_credential)
            .expect("save Codex login");
        let oauth = OAuthService::new(OAuthEndpoints::default(), Arc::new(NoBrowser), store)
            .expect("service");
        let startup = TuiStartup {
            request: Some(crate::ProviderRequest {
                prompt: String::new(),
                mode: slim_core::OperatingMode::Auto,
                kind: slim_core::provider::ProviderKind::OpenAiCodex,
                endpoint: crate::cli::default_provider_endpoint(
                    slim_core::provider::ProviderKind::OpenAiCodex,
                )
                .into(),
                model: super::ModelAlias::Sol.id().into(),
                api_key: codex_credential.access.clone(),
                account_id: codex_credential.account_id.clone(),
                timeout: std::time::Duration::from_secs(120),
            }),
            oauth_session: Some((crate::oauth::OAuthProvider::OpenAiCodex, codex_credential)),
            options: crate::ProviderRunOptions::default(),
            initial_prompt: None,
            image_labels: Vec::new(),
            resume_path: None,
            resume_preflight: None,
            persist_sessions: false,
            mode: slim_core::OperatingMode::Auto,
            effort: super::ReasoningEffort::High,
            endpoint_override: None,
            model_override: None,
            timeout: std::time::Duration::from_secs(120),
        };
        let (runtime, channels) = spawn_tui_session(startup, oauth).expect("runtime");

        channels
            .commands
            .send(super::UiCommand::SetOpenCodeModel {
                model: slim_core::provider::OPENCODE_GO_DEFAULT_MODEL.into(),
                effort: super::ReasoningEffort::High,
            })
            .expect("select OpenCode model");
        wait_for_auth_provider(&channels, super::LoginProvider::OpenCodeGo);
        assert_eq!(
            OAuthStore::at(&auth)
                .active_provider_key()
                .expect("active OpenCode provider"),
            Some("opencode-go".into())
        );

        channels
            .commands
            .send(super::UiCommand::SetClinePassModel {
                model: slim_core::provider::CLINEPASS_DEFAULT_MODEL.into(),
                effort: super::ReasoningEffort::High,
            })
            .expect("select ClinePass model");
        wait_for_auth_provider(&channels, super::LoginProvider::ClinePass);
        assert_eq!(
            OAuthStore::at(&auth)
                .active_provider_key()
                .expect("active ClinePass provider"),
            Some("clinepass".into())
        );

        channels
            .commands
            .send(super::UiCommand::SetCommandCodeModel {
                model: slim_core::provider::COMMANDCODE_DEFAULT_MODEL.into(),
                effort: super::ReasoningEffort::High,
            })
            .expect("select Command Code model");
        wait_for_auth_provider(&channels, super::LoginProvider::CommandCode);
        assert_eq!(
            OAuthStore::at(&auth)
                .active_provider_key()
                .expect("active Command Code provider"),
            Some("command-code".into())
        );

        channels
            .commands
            .send(super::UiCommand::SetModel {
                model: super::ModelAlias::Sol,
                effort: super::ReasoningEffort::High,
            })
            .expect("select Codex model");
        wait_for_auth_provider(&channels, super::LoginProvider::OpenAiCodex);

        channels
            .commands
            .send(super::UiCommand::Shutdown)
            .expect("shutdown");
        drop(runtime);
        assert_eq!(
            OAuthStore::at(&auth)
                .active_provider_key()
                .expect("active provider"),
            Some("openai-codex".into())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
