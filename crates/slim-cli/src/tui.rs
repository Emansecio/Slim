use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slim_core::mcp::{McpManager, McpServerStatus};
use slim_core::provider::{
    clinepass_model, fetch_clinepass_catalog, is_clinepass_model_id, is_command_code_model_id,
    is_xai_model_id, open_code_model, zen_model, ProviderKind, CLINEPASS_BASE_URL,
    CLINEPASS_DEFAULT_MODEL, COMMANDCODE_BASE_URL, COMMANDCODE_DEFAULT_MODEL, OPENCODE_GO_BASE_URL,
    OPENCODE_GO_DEFAULT_MODEL, OPENCODE_ZEN_BASE_URL, OPENCODE_ZEN_DEFAULT_MODEL,
    OPENCODE_ZEN_PUBLIC_KEY, XAI_BASE_URL, XAI_DEFAULT_MODEL,
};
use slim_core::runtime::CancellationToken;
use slim_core::session::{DurableSessionHeader, JsonlRepo, SessionFormat, SessionPreflight};
use slim_core::{
    interaction_route, EventKind, InteractionRequestId as CoreInteractionRequestId,
    InteractionResponder, ProviderError, ProviderMessage, SessionEvent, SessionEventSender,
};
use slim_tui::api::{
    ClinePassCatalogSource, CommandCodeCatalogSource, ContentHandle, ContentRequestId,
    InteractionRequestId, LoginProvider, McpServerView, McpStatusView, ModelAlias,
    OpenCodeCatalogSource, OpenCodeModelView, PageCursor, PromptAdmission, ReasoningEffort,
    ToolBatchId, ToolCallId, TranscriptMessage, TranscriptRole, UiChannels, UiCommand, UiEvent,
    WakeSignal, ZenCatalogSource, STREAM_EVENT_CAPACITY,
};

use crate::cli::parse_cli_args;
use crate::command_code_catalog::CommandCodeCatalog;
use crate::exit_codes::ExitCode;
use crate::headless::{
    execute_provider_turn, execute_provider_turn_async, format_run_stop_message,
    resolve_max_mutating_tool_calls, resolve_max_read_tool_calls, resolve_max_turns,
    resolve_timeout_secs, resume_messages_from_preflight,
    run_provider_resume_with_preflight_events_interactive_async, McpHandle, OutputFormat,
    ProviderExecution, ProviderRequest, ProviderRunOptions, SkillInstructions,
    MAX_SLASH_SKILL_BODY_BYTES,
};
use crate::oauth::{
    FreshCredential, OAuthCredential, OAuthError, OAuthProgress, OAuthProvider, OAuthService,
};
use crate::opencode_go_catalog::{CatalogSnapshot, CatalogSource, OpenCodeCatalog};
use crate::opencode_zen_catalog::OpenCodeZenCatalog;
use crate::{delete_api_key, load_local_images, resolve_provider_credential, save_api_key};

pub struct TuiRuntimeHandle {
    shutdown: Option<mpsc::Sender<UiCommand>>,
    worker: Option<thread::JoinHandle<Vec<String>>>,
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
    /// `/rename` before the session file exists (sessions are created lazily
    /// by the first prompt); written beside the journal at creation.
    pending_session_title: Option<String>,
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

fn restored_todo_event(preflight: &SessionPreflight) -> Result<UiEvent, String> {
    let mut runtime = slim_core::runtime::Runtime::new();
    let cwd = preflight
        .header
        .as_ref()
        .ok_or("session header unavailable")?;
    runtime
        .restore_task_facts(
            &crate::headless::session_task_facts(preflight),
            std::path::Path::new(&cwd.cwd),
        )
        .map_err(provider_error_message)?;
    UiEvent::from_core(slim_core::SessionEvent::new(
        0,
        slim_core::EventKind::TodoChanged {
            items: runtime.todo_items(),
        },
    ))
    .ok_or_else(|| "task state event unavailable".into())
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

/// Skill names for slash completion, discovered here (never on the TUI
/// thread) and attached to the workspace events that carry them.
fn workspace_skill_names(workspace_root: &Path, warnings: &mut String) -> Vec<String> {
    warnings.clear();
    if !workspace_root.is_dir() {
        return Vec::new();
    }
    slim_core::skills::discover_workspace(workspace_root)
        .map(|discovery| {
            *warnings = discovery.diagnostic_lines().join("\n");
            discovery
                .active_entries()
                .iter()
                .filter(|entry| valid_slash_skill_name(&entry.name))
                .map(|entry| entry.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Memoized variant of [`workspace_skill_names`] so the startup
/// WorkspaceChanged + SessionRestored pair scans the same workspace once.
/// `warnings` holds the discovery diagnostics of the latest lookup.
#[derive(Default)]
struct SkillNameMemo {
    cached: Option<(PathBuf, Vec<String>, String)>,
    warnings: String,
}

impl SkillNameMemo {
    fn names(&mut self, root: Option<PathBuf>) -> Vec<String> {
        let Some(root) = root else {
            self.warnings.clear();
            return Vec::new();
        };
        let root = root.canonicalize().unwrap_or(root);
        if let Some((_, names, cached_warnings)) = self
            .cached
            .as_ref()
            .filter(|(cached, _, _)| *cached == root)
        {
            self.warnings.clone_from(cached_warnings);
            return names.clone();
        }
        let names = workspace_skill_names(&root, &mut self.warnings);
        self.cached = Some((root, names.clone(), self.warnings.clone()));
        names
    }
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

/// Fresh `tui-…` session id; callers retry with a new suffix on collision.
fn tui_session_id(timestamp: u128) -> String {
    let suffix = NEXT_TUI_SESSION_SUFFIX.fetch_add(1, Ordering::Relaxed);
    format!("tui-{timestamp}-{}-{suffix}", std::process::id())
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
        let id = tui_session_id(timestamp);
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

/// Validation chain every session switch shares (automatic, by id and rewind).
/// `label` prefixes the refusal reason for the caller's audience.
fn selected_from_preflight(
    preflight: SessionPreflight,
    label: &str,
) -> Result<SelectedTuiSession, String> {
    super::headless::ensure_resume_preflight(&preflight)
        .map_err(|error| format!("{label}: {error}"))?;
    slim_core::session::resume_plan_from_preflight(&preflight)
        .map_err(|error| format!("{label}: {error}"))?;
    let history = resume_messages_from_preflight(&preflight).map_err(provider_error_message)?;
    Ok(SelectedTuiSession { preflight, history })
}

fn select_previous_tui_session(
    workspace_root: &Path,
    current_path: Option<&Path>,
) -> Result<Option<SelectedTuiSession>, String> {
    let current_path = current_path.and_then(|path| fs::canonicalize(path).ok());
    let mut candidates = sessions::scan_tui_sessions(workspace_root)?;
    candidates.retain(|candidate| current_path.as_ref() != Some(&candidate.path));
    candidates.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
    while let Some(candidate) = candidates.pop() {
        let preflight = match slim_core::session::preflight_session(&candidate.path) {
            Ok(preflight) => preflight,
            Err(_) => continue,
        };
        if preflight.format != Some(SessionFormat::DurableV2)
            || (preflight.can_resume_v2() && preflight.records.is_empty())
        {
            continue;
        }
        let selected = selected_from_preflight(preflight, "previous session cannot be resumed")?;
        let has_user = selected
            .history
            .iter()
            .any(|message| message.role == "user");
        let has_assistant = selected
            .history
            .iter()
            .any(|message| message.role == "assistant");
        if has_user
            && has_assistant
            && !selected.preflight.summary.terminal_operation_ids.is_empty()
        {
            return Ok(Some(selected));
        }
    }
    Ok(None)
}

/// First-line decode of a durable session header. The strict deserialize
/// rejects non-v2 schema versions and non-session record types. The line
/// read is capped so a hostile or corrupt .jsonl in the sessions directory
/// cannot be loaded into memory whole.
fn read_session_header(path: &Path) -> Option<DurableSessionHeader> {
    const MAX_HEADER_LINE_BYTES: u64 = 64 * 1024;
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(std::io::Read::take(file, MAX_HEADER_LINE_BYTES));
    let mut first_line = Vec::new();
    std::io::BufRead::read_until(&mut reader, b'\n', &mut first_line).ok()?;
    serde_json::from_slice(&first_line).ok()
}

fn transcript_messages(history: &[ProviderMessage]) -> Vec<TranscriptMessage> {
    let mut restored = Vec::new();
    let mut pending = std::collections::BTreeMap::new();
    for (index, message) in history.iter().enumerate() {
        match message.role.as_str() {
            "user" => restored.push(TranscriptMessage {
                role: TranscriptRole::User,
                text: message.content.clone(),
            }),
            "assistant" => {
                if !message.content.trim().is_empty() {
                    restored.push(TranscriptMessage {
                        role: TranscriptRole::Assistant,
                        text: message.content.clone(),
                    });
                }
                for call in &message.tool_calls {
                    pending.insert(call.id.as_str(), restored.len());
                    restored.push(TranscriptMessage {
                        role: TranscriptRole::Tool {
                            batch_id: ToolBatchId(format!("history-batch-{index}").into()),
                            call_id: ToolCallId(format!("history-{index}:{}", call.id).into()),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                        text: String::new(),
                    });
                }
            }
            "tool" => {
                if let Some(position) = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| pending.remove(id))
                {
                    restored[position].text.clone_from(&message.content);
                }
            }
            _ => {}
        }
    }
    restored
}

fn session_transcript(preflight: &SessionPreflight) -> Result<Vec<TranscriptMessage>, String> {
    let history = slim_core::session::provider_messages_from_records(preflight.records.iter())
        .map_err(str::to_owned)?;
    Ok(transcript_messages(&history))
}

/// Persists a `/rename` made before the session file existed, then tells the
/// TUI the current session's title (`None` when unnamed). Runs after every
/// session creation and switch so the rail is right in each.
fn announce_session_title(startup: &mut TuiStartup, sink: &EventSink) {
    if let (Some(title), Some(path)) = (
        startup.pending_session_title.take(),
        startup.resume_path.as_deref(),
    ) {
        if let Err(message) = sessions::write_session_title(path, &title) {
            let _ = sink.send(UiEvent::Notification { message });
        }
    }
    let title = startup
        .resume_path
        .as_deref()
        .and_then(sessions::read_session_title);
    let _ = sink.send(UiEvent::SessionTitleChanged { title });
}

/// Replaces the conversation with `selected`: history, task state, artifact
/// ids, tool registry, compaction state, pending `!` notes and the journal the
/// next prompt appends to; then tells the TUI. Shared by `ResumePrevious`,
/// `ResumeSession` and `RewindSession`. Returns false, changing nothing, when
/// the transcript or task state to show cannot be built.
fn switch_session(
    startup: &mut TuiStartup,
    sink: &EventSink,
    skill_memo: &mut SkillNameMemo,
    user_shell_context: &UserShellContext,
    selected: SelectedTuiSession,
    notice: &str,
) -> bool {
    let workspace = startup
        .options
        .workspace_root
        .clone()
        .expect("workspace root initialized");
    let session_id = selected
        .preflight
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".into());
    let cwd = display_workspace_path(&workspace);
    let messages = match session_transcript(&selected.preflight) {
        Ok(messages) => messages,
        Err(message) => {
            sink.send(UiEvent::RunFailed {
                run_id: None,
                message,
            });
            return false;
        }
    };
    let todo_event = match restored_todo_event(&selected.preflight) {
        Ok(event) => event,
        Err(message) => {
            sink.send(UiEvent::RunFailed {
                run_id: None,
                message,
            });
            return false;
        }
    };
    if let Some(policy) = startup
        .options
        .compaction
        .as_ref()
        .map(slim_core::context::CompactionHandle::policy)
    {
        startup.options.compaction = Some(slim_core::context::CompactionHandle::new(policy));
    }
    startup.options.history = selected.history;
    startup.options.task_facts = crate::headless::session_task_facts(&selected.preflight);
    startup.options.artifact_ids = crate::headless::session_artifact_ids(&selected.preflight);
    startup.options.tool_registry = None;
    startup.options.ensure_shared_tool_registry();
    startup.resume_path = Some(selected.preflight.path.clone());
    startup.resume_preflight = Some(selected.preflight);
    startup.pending_session_title = None;
    user_shell_context.clear();
    let skill_names = skill_memo.names(Some(workspace));
    let _ = sink.send(UiEvent::SessionRestored {
        session_id: slim_tui::api::SessionId(session_id.into()),
        cwd,
        messages,
        skill_names,
    });
    announce_session_title(startup, sink);
    let _ = sink.send(todo_event);
    if !skill_memo.warnings.is_empty() {
        sink.send(UiEvent::Notification {
            message: skill_memo.warnings.clone(),
        });
    }
    let _ = sink.send(UiEvent::Notification {
        message: notice.to_owned(),
    });
    true
}

pub fn run_tui(args: Vec<String>) -> Result<(), TuiError> {
    let oauth = OAuthService::production()
        .map_err(|error| TuiError::new(ExitCode::Auth, error.to_string()))?;
    let startup = prepare_tui(args, &oauth)?;
    let initial_prompt = startup.initial_prompt.clone();
    let (runtime, channels) = spawn_tui_session(startup, oauth).map_err(tui_provider_error)?;
    let result = slim_tui::run_app_with_initial_prompt(channels, initial_prompt)
        .map_err(|error| TuiError::new(ExitCode::Internal, error.to_string()));
    let worker_result = runtime.finish();
    if let Ok(warnings) = &worker_result {
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
    }
    result.and(worker_result.map(|_| ()))
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
    let mut compaction_policy = layered_config
        .compaction_policy()
        .map_err(|error| TuiError::new(ExitCode::Internal, format!("config error: {error}")))?;
    if let Some(value) = std::env::var("SLIM_COMPACTOR")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        compaction_policy.strategy = slim_core::context::CompactionStrategy::parse(&value)
            .map_err(|error| TuiError::new(ExitCode::InputRequired, error))?;
    }
    let jev_prune = if compaction_policy.strategy == slim_core::context::CompactionStrategy::Jev {
        crate::config::jev_prune_config_from_env()
            .map_err(|error| TuiError::new(ExitCode::InputRequired, error))?
    } else {
        None
    };
    let endpoint_override = parsed
        .endpoint
        .or_else(|| std::env::var("SLIM_ENDPOINT").ok())
        .or(layered_config.endpoint);
    let explicit_model = parsed.model.or_else(|| std::env::var("SLIM_MODEL").ok());
    let model_override = explicit_model.clone().or(layered_config.model);
    let configured_effort = parsed
        .effort
        .or_else(|| std::env::var("SLIM_EFFORT").ok())
        .filter(|value| !value.is_empty())
        .or(layered_config.effort)
        .filter(|value| !value.is_empty())
        .map(|value| {
            ReasoningEffort::parse(&value).ok_or_else(|| {
                TuiError::new(
                    ExitCode::InputRequired,
                    "unsupported configured reasoning effort",
                )
            })
        })
        .transpose()?;
    let mut effort = configured_effort.unwrap_or(ReasoningEffort::High);
    let explicit_provider = parsed
        .provider
        .or_else(|| std::env::var("SLIM_PROVIDER").ok());

    let (mut request, oauth_session) = if let Some(provider_name) = explicit_provider {
        let kind = provider_kind(&provider_name)?;
        let api_request = api_key_request(
            kind,
            parsed.mode,
            endpoint_override.as_deref(),
            model_override.as_deref(),
        )?;
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
            let api_request = api_key_request(
                kind,
                parsed.mode,
                endpoint_override.as_deref(),
                model_override.as_deref(),
            )?;
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
        if parsed.codex_fast.is_some() && request.kind != ProviderKind::OpenAiCodex {
            return Err(TuiError::new(
                ExitCode::InputRequired,
                "--fast/--normal require the openai-codex provider",
            ));
        }
        if request.kind == ProviderKind::OpenAiCodex {
            if let Some(alias) = ModelAlias::parse(&request.model) {
                if !ReasoningEffort::supported(alias).contains(&effort) {
                    return Err(TuiError::new(
                        ExitCode::InputRequired,
                        "configured effort is unsupported by the Codex model",
                    ));
                }
            }
        }
        if explicit_model
            .as_deref()
            .is_some_and(|model| !crate::provider_compatible_model(request.kind, model))
        {
            return Err(TuiError::new(
                ExitCode::InputRequired,
                "explicit model is unsupported by the selected provider",
            ));
        }
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
    if let Some(model) = request
        .as_ref()
        .filter(|request| request.kind == ProviderKind::OpenCodeZen)
        .and_then(|request| zen_model(&request.model))
    {
        // Models without reasoning levels expose no effort knob; a globally
        // configured effort is left unsent by the adapter, not an error.
        if !model.reasoning_levels.is_empty() && !model.reasoning_levels.contains(&effort.id()) {
            if configured_effort.is_some() {
                return Err(TuiError::new(
                    ExitCode::InputRequired,
                    "configured effort is unsupported by the OpenCode Zen model",
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
    if let Some(jev_prune) = jev_prune {
        options = options.with_jev_prune(jev_prune);
    }
    if let Some(experiment_id) = parsed.experiment_id {
        options = options.with_experiment_id(experiment_id);
    }
    if let Some(task_id) = parsed.task_id {
        options = options.with_task_id(task_id);
    }
    if let Some(manager) = application_code_intelligence {
        options = options.with_code_intelligence(manager);
    }
    if let Some(calls) = layered_config.max_mutating_tool_calls {
        options = options.with_max_tool_calls(calls);
    }
    if let Some(calls) = layered_config.max_read_tool_calls {
        options = options.with_max_read_tool_calls(calls);
    }
    if let Some(calls) = layered_config.max_total_tool_calls {
        options = options.with_max_total_tool_calls(calls);
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
    options.codex_fast = parsed
        .codex_fast
        .or(layered_config.codex_fast)
        .unwrap_or(false);
    if configured_effort.is_some() {
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
        pending_session_title: None,
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
        "opencode-zen" | "opencode_zen" | "zen" => Ok(ProviderKind::OpenCodeZen),
        "clinepass" | "cline-pass" | "cp" => Ok(ProviderKind::ClinePass),
        "command-code" | "commandcode" | "cmd" => Ok(ProviderKind::CommandCode),
        "xai" | "grok" => Ok(ProviderKind::Xai),
        _ => Err(TuiError::new(
            ExitCode::Provider,
            "unsupported provider; use openai-compatible, openai-codex, anthropic, opencode-go, opencode-zen, clinepass, command-code, or xai",
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

fn activate_zen_provider(
    startup: &mut TuiStartup,
    oauth: &OAuthService,
    model: &str,
) -> Result<bool, String> {
    if startup
        .request
        .as_ref()
        .is_some_and(|request| request.kind == ProviderKind::OpenCodeZen)
    {
        return Ok(false);
    }
    let api_key = oauth
        .activate_api_key("opencode-zen")
        .map_err(|error| format!("Authentication: {error}"))?
        .unwrap_or_else(|| OPENCODE_ZEN_PUBLIC_KEY.into());
    startup.request = Some(ProviderRequest {
        prompt: String::new(),
        mode: startup.mode,
        kind: ProviderKind::OpenCodeZen,
        endpoint: startup
            .endpoint_override
            .clone()
            .unwrap_or_else(|| OPENCODE_ZEN_BASE_URL.into()),
        model: model.into(),
        api_key,
        account_id: None,
        timeout: startup.timeout,
    });
    startup.oauth_session = None;
    Ok(true)
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
    let credential = match resolve_provider_credential(kind).map_err(tui_auth_error)? {
        Some(credential) => credential,
        // Zen's free tier answers the literal `public` bearer; the adapter
        // still sends the required `x-opencode-session` header.
        None if kind == ProviderKind::OpenCodeZen => crate::auth::ProviderCredential {
            access: OPENCODE_ZEN_PUBLIC_KEY.into(),
            account_id: None,
            oauth: false,
        },
        None => return Ok(None),
    };
    if credential.oauth {
        return Ok(None);
    }
    let api_key = credential.access;
    let account_id = if kind == ProviderKind::OpenAiCodex {
        Some(crate::oauth::codex_account_id(&api_key).map_err(tui_auth_error)?)
    } else {
        None
    };
    let (default_endpoint, default_model) = defaults(kind);
    // G248: a configured model only applies when it belongs to this provider;
    // otherwise the provider default is used (no cross-kind leakage).
    let model = model
        .filter(|model| crate::provider_compatible_model(kind, model))
        .map(str::to_owned)
        .unwrap_or_else(|| default_model.to_owned());
    let model = crate::canonical_provider_model(kind, &model);
    Ok(Some(ProviderRequest {
        prompt: String::new(),
        mode,
        kind,
        endpoint: endpoint.unwrap_or(default_endpoint).into(),
        model,
        api_key,
        account_id,
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
    let model = crate::canonical_provider_model(kind, &model);
    let account_id =
        match provider {
            OAuthProvider::Anthropic => Some(String::new()),
            OAuthProvider::OpenAiCodex => Some(credential.account_id.clone().ok_or_else(|| {
                TuiError::new(ExitCode::Auth, "Codex OAuth account id is missing")
            })?),
            OAuthProvider::Xai => None,
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
        OAuthProvider::Xai => ProviderKind::Xai,
    }
}

impl TuiRuntimeHandle {
    /// Shut down and observe the worker result without exposing panic payloads.
    pub fn finish(mut self) -> Result<Vec<String>, TuiError> {
        self.shutdown_and_join()
    }

    fn shutdown_and_join(&mut self) -> Result<Vec<String>, TuiError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(UiCommand::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| {
                TuiError::new(
                    ExitCode::Internal,
                    "TUI worker thread failed; run completion and prior effects are unverified",
                )
            })
        } else {
            Ok(Vec::new())
        }
    }
}

impl Drop for TuiRuntimeHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_and_join();
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
            pending_session_title: None,
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
            pending_session_title: None,
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
    startup
        .options
        .provider_session_id
        .get_or_insert_with(slim_core::provider::OpenCodeGoAdapter::new_session_id);
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
    let (data_tx, data_rx) = mpsc::sync_channel::<UiEvent>(STREAM_EVENT_CAPACITY);
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
        .stack_size(crate::headless::AGENT_LOOP_STACK_BYTES)
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
    let (model, label) = match request.kind {
        ProviderKind::OpenCodeGo => (open_code_model(&request.model)?, "OpenCode Go"),
        ProviderKind::OpenCodeZen => (zen_model(&request.model)?, "OpenCode Zen"),
        _ => return None,
    };
    if model.accepts_images {
        None
    } else {
        Some(format!("{label} model {} does not accept images", model.id))
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
            // Gated signal: the consumer probes the lanes before waiting, so
            // per-event SetEvent/pipe writes only matter while it is parked.
            self.wake.notify_waiter();
            return true;
        }
        false
    }

    fn send_control(&self, event: UiEvent) -> bool {
        if self.control().send(event).is_ok() {
            self.wake.notify_waiter();
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
            self.wake.notify_waiter();
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
            // Causal accounting, interaction, final tool output/boundaries
            // and the AssistantEnded fence migrate off a full data lane after cancel.
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
                    self.wake.notify_waiter();
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
    admission: Option<PromptAdmission>,
    cancellation_from_preparation: bool,
    task: tokio::task::JoinHandle<Result<ProviderExecution, ProviderError>>,
    projector: thread::JoinHandle<()>,
    cancellation: CancellationToken,
    durable: bool,
    content_store: SharedContentStore,
    interaction_responder: Option<InteractionResponder>,
    manual_retry: slim_core::runtime::ManualRetryHandle,
    workspace_root: Option<PathBuf>,
}

struct ActiveRunLaunch {
    request: ProviderRequest,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
}

struct PendingRun {
    run_id: u64,
    admission: Option<PromptAdmission>,
    result: Option<Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>>,
    projector: Option<thread::JoinHandle<()>>,
    delivery: VecDeque<UiEvent>,
    cancellation: CancellationToken,
    durable: bool,
    cancel_requested: bool,
    content_store: SharedContentStore,
    workspace_root: Option<PathBuf>,
}

struct ActivePromptPreparation {
    admission: PromptAdmission,
    prompt: String,
    skill_instructions: Option<SkillInstructions>,
    task: tokio::task::JoinHandle<Result<Option<FreshCredential>, OAuthError>>,
}

/// Prompt text and its admission identity for a run about to start.
struct PromptRunInput {
    prompt: String,
    admission: Option<PromptAdmission>,
    skill_instructions: Option<SkillInstructions>,
    /// `!command` output not yet told to the model; delivered with this prompt.
    user_shell_context: UserShellContext,
}

fn start_prompt_run(
    input: PromptRunInput,
    startup: &mut TuiStartup,
    sink: &EventSink,
    content_store: SharedContentStore,
    next_run_id: &mut u64,
    active: &mut Option<ActiveRun>,
    skill_memo: &mut SkillNameMemo,
) -> Result<(), String> {
    let PromptRunInput {
        prompt,
        admission,
        skill_instructions,
        user_shell_context,
    } = input;
    if startup.request.is_none() {
        return Err("No provider connected. Use /login.".into());
    }
    if !startup.image_labels.is_empty() {
        if let Some(message) = image_model_error(startup.request.as_ref()) {
            return Err(message);
        }
    }
    // Before any session or run event exists: a refused mention fails the
    // prompt while the draft is still the user's.
    let mentions = match startup
        .options
        .workspace_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
    {
        Some(root) => load_prompt_mentions(&root, &prompt)?,
        None => MentionAttachments::default(),
    };
    match create_tui_session(startup) {
        Ok(Some((session_id, cwd))) => {
            let skill_names = skill_memo.names(startup.options.workspace_root.clone());
            let _ = sink.send(UiEvent::SessionSnapshot {
                session_id: slim_tui::api::SessionId(session_id.into()),
                cwd,
                skill_names,
            });
            announce_session_title(startup, sink);
            if !skill_memo.warnings.is_empty() {
                let _ = sink.send(UiEvent::Notification {
                    message: skill_memo.warnings.clone(),
                });
            }
        }
        Ok(None) => {}
        Err(message) => return Err(message),
    }
    let request = startup
        .request
        .as_mut()
        .expect("provider checked before session creation");
    request.prompt.clone_from(&prompt);
    let api_key = request.api_key.clone();
    let request = request.clone();
    let Some(run_id) = take_run_id(next_run_id) else {
        return Err("TUI run identity exhausted".into());
    };
    let tool_budget = match (
        resolve_max_mutating_tool_calls(&startup.options),
        resolve_max_read_tool_calls(&startup.options),
        resolve_max_turns(&startup.options),
    ) {
        (Ok(max_mutating), Ok(max_read), Ok(max_turns)) => (max_mutating, max_read, max_turns),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
            return Err(provider_error_message(error));
        }
    };
    if let Some(admission) = admission {
        let _ = sink.send(UiEvent::PromptRunStarted {
            admission,
            run_id,
            max_mutating_tool_calls: tool_budget.0,
            max_read_tool_calls: tool_budget.1,
            max_turns: tool_budget.2,
        });
    } else {
        let _ = sink.send(UiEvent::run_started_with_budget(
            run_id,
            tool_budget.0,
            tool_budget.1,
            tool_budget.2,
        ));
    }
    let mut display_prompt = redact_for_ui(&prompt, &api_key);
    if !startup.image_labels.is_empty() {
        display_prompt.push_str("\n\n");
        for label in &startup.image_labels {
            display_prompt.push_str(&format!("[image · {label}]\n"));
        }
        display_prompt.pop();
    }
    let shell_notes = user_shell_context.snapshot();
    let labels = shell_notes
        .iter()
        .map(UserShellNote::label)
        .chain(mentions.labels.iter().cloned())
        .collect::<Vec<_>>();
    if !labels.is_empty() {
        display_prompt.push_str("\n\n");
        display_prompt.push_str(&labels.join("\n"));
    }
    let _ = sink.send(UiEvent::UserMessageAdded {
        text: display_prompt,
    });
    let mut run_options = startup.options.clone();
    run_options.content_blocks.extend(
        shell_notes
            .iter()
            .map(|note| slim_core::provider::ProviderContentBlock::text(note.text.clone())),
    );
    run_options.content_blocks.extend(mentions.blocks);
    let resume_preflight = startup.resume_preflight.take();
    match start_active_run(
        run_id,
        admission,
        ActiveRunLaunch {
            request,
            options: run_options,
            skill_instructions,
        },
        startup.resume_path.clone(),
        resume_preflight,
        sink.clone(),
        content_store,
    ) {
        Ok(run) => {
            startup.options.content_blocks.clear();
            startup.image_labels.clear();
            let _ = sink.send(UiEvent::AttachmentsChanged { labels: Vec::new() });
            *active = Some(run);
            user_shell_context.consume(&shell_notes);
        }
        Err(message) => {
            if let Some(admission) = admission {
                let _ = sink.send(UiEvent::PromptRunFailed {
                    admission,
                    run_id: Some(run_id),
                    message,
                });
            } else {
                let _ = sink.send(UiEvent::RestoreDraft { text: prompt });
                let _ = sink.send(UiEvent::RunFailed {
                    run_id: Some(run_id),
                    message,
                });
            }
        }
    }
    Ok(())
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
    /// Grace period after the first Esc elapsed while the run still ignores
    /// the cancellation token: force the abort instead of waiting forever.
    CancelTimeout,
    /// One-second status poll while /mcp stays open over an active run.
    McpTick,
}

enum LoginEvent {
    Command(Option<UiCommand>),
    Finished(Result<Result<OAuthCredential, OAuthError>, tokio::task::JoinError>),
}

enum PromptPreparationEvent {
    Command(Option<UiCommand>),
    Finished(Result<Result<Option<FreshCredential>, OAuthError>, tokio::task::JoinError>),
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

/// Applies a mode change to the startup state and the active request.
fn apply_mode_change(startup: &mut TuiStartup, sink: &EventSink, mode: slim_core::OperatingMode) {
    startup.mode = mode;
    if let Some(request) = startup.request.as_mut() {
        request.mode = mode;
    }
    let _ = sink.send(UiEvent::ModeChanged { mode });
}

fn run_worker(
    mut startup: TuiStartup,
    oauth: OAuthService,
    command_rx: mpsc::Receiver<UiCommand>,
    sink: EventSink,
) -> Vec<String> {
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel();
    let forwarder = thread::spawn(move || {
        while let Ok(command) = command_rx.recv() {
            let shutdown = command == UiCommand::Shutdown;
            if async_tx.send(command).is_err() || shutdown {
                break;
            }
        }
    });
    // Two workers suffice: heavy work runs on spawn_blocking threads, and the
    // async tasks (login, catalogs, provider driver) are IO-bound.
    let tokio_runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = sink.send(UiEvent::RunFailed {
                run_id: None,
                message: format!("runtime: {error}"),
            });
            return Vec::new();
        }
    };
    let warnings = tokio_runtime.block_on(async move {
        let (oauth_warning_stop, mut oauth_warning_stop_rx) = tokio::sync::watch::channel(false);
        let warning_oauth = oauth.clone();
        let warning_sink = sink.clone();
        let oauth_warning_forwarder = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        for message in warning_oauth.take_warnings() {
                            let _ = warning_sink.send(UiEvent::Notification { message });
                        }
                    }
                    changed = oauth_warning_stop_rx.changed() => {
                        if changed.is_err() || *oauth_warning_stop_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
        let mut skill_memo = SkillNameMemo::default();
        let cwd = startup
            .options
            .workspace_root
            .as_deref()
            .map(display_workspace_path)
            .unwrap_or_default();
        let skill_names = skill_memo.names(startup.options.workspace_root.clone());
        sink.send(UiEvent::WorkspaceChanged { cwd, skill_names });
        if let Some(preflight) = startup.resume_preflight.as_ref() {
            match session_transcript(preflight) {
                Ok(messages) => {
                    let todo_event = match restored_todo_event(preflight) {
                        Ok(event) => event,
                        Err(message) => { sink.send(UiEvent::RunFailed { run_id: None, message }); return Vec::new(); }
                    };
                    if let Some(header) = preflight.header.as_ref() {
                        let skill_names = skill_memo.names(Some(PathBuf::from(&header.cwd)));
                        sink.send(UiEvent::SessionRestored {
                            session_id: slim_tui::api::SessionId(header.id.clone().into()),
                            cwd: header.cwd.clone(),
                            messages,
                            skill_names,
                        });
                        announce_session_title(&mut startup, &sink);
                        sink.send(todo_event);
                    }
                }
                Err(message) => { sink.send(UiEvent::RunFailed { run_id: None, message }); return Vec::new(); }
            }
        }
        if startup.initial_prompt.is_some() && startup.resume_path.is_none() {
            match create_tui_session(&mut startup) {
                Ok(Some((session_id, cwd))) => {
                    let skill_names = skill_memo.names(startup.options.workspace_root.clone());
                    sink.send(UiEvent::SessionSnapshot {
                        session_id: slim_tui::api::SessionId(session_id.into()),
                        cwd,
                        skill_names,
                    });
                    announce_session_title(&mut startup, &sink);
                }
                Ok(None) => {}
                Err(message) => {
                    sink.send(UiEvent::RunFailed { run_id: None, message });
                }
            }
        }
        // Restore clears old notifications; publish discovery diagnostics afterwards.
        if !skill_memo.warnings.is_empty() {
            sink.send(UiEvent::Notification { message: skill_memo.warnings.clone() });
        }
        sink.send(UiEvent::ModeChanged { mode: startup.mode });
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
                        ProviderKind::OpenCodeZen => Some(LoginProvider::OpenCodeZen),
                        ProviderKind::ClinePass => Some(LoginProvider::ClinePass),
                        ProviderKind::CommandCode => Some(LoginProvider::CommandCode),
                        ProviderKind::Xai => Some(LoginProvider::Xai),
                        ProviderKind::OpenAiCodex => Some(LoginProvider::OpenAiCodex),
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
        let _ = sink.send(UiEvent::CodexSpeedChanged {
            fast: startup.options.codex_fast,
        });
        if !startup.image_labels.is_empty() {
            let _ = sink.send(UiEvent::AttachmentsChanged {
                labels: startup.image_labels.clone(),
            });
        }
        let mut active: Option<ActiveRun> = None;
        let mut user_shell: Option<UserShellRun> = None;
        let user_shell_context = UserShellContext::default();
        let mut preparing: Option<ActivePromptPreparation> = None;
        let mut ignored_prep_cancel = false;
        let mut pending: Option<PendingRun> = None;
        let mut last_esc_at: Option<Instant> = None;
        let mut login: Option<ActiveLogin> = None;
        let mut next_run_id = 1_u64;
        let content_store = SharedContentStore::default();
        let open_code_catalog = OpenCodeCatalog::production().ok();
        let zen_catalog = OpenCodeZenCatalog::production().ok();
        let command_code_catalog = CommandCodeCatalog::production().ok();
        let cline_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        let open_code_inflight = Arc::new(AtomicBool::new(false));
        let zen_inflight = Arc::new(AtomicBool::new(false));
        let cline_inflight = Arc::new(AtomicBool::new(false));
        let command_code_inflight = Arc::new(AtomicBool::new(false));
        let open_code_gen = Arc::new(AtomicU64::new(0));
        let zen_gen = Arc::new(AtomicU64::new(0));
        let cline_gen = Arc::new(AtomicU64::new(0));
        let command_code_gen = Arc::new(AtomicU64::new(0));
        // Application-scoped MCP manager: built once from layered config,
        // never spawns a process until a server is actually exercised.
        if startup.options.mcp.is_none() {
            let cwd = startup
                .options
                .workspace_root
                .clone()
                .unwrap_or_default();
            if let Ok(layered) = crate::config::load_layered() {
                if let Some(manager) = crate::mcp::build_mcp_manager(&layered.mcp, &cwd) {
                    startup.options.mcp = Some(McpHandle::new(manager));
                }
            }
        }
        let mut mcp_manager = startup
            .options
            .mcp
            .as_ref()
            .map(|handle| handle.manager().clone());
        let mut mcp_watch = false;
        // Shared with spawned /mcp ops: an op that already pushed a snapshot
        // records the revision it published so the watch tick doesn't
        // re-send an identical one a second later.
        let mcp_seen_revision = Arc::new(AtomicU64::new(0_u64));
        let mut mcp_watch_tick = tokio::time::interval(Duration::from_secs(1));
        let mcp_inflight = Arc::new(Mutex::new(HashSet::<String>::new()));
        loop {
            if let Some(mut prompt_prep) = preparing.take() {
                let wake = tokio::select! {
                    biased;
                    command = async_rx.recv() => PromptPreparationEvent::Command(command),
                    result = &mut prompt_prep.task => PromptPreparationEvent::Finished(result),
                };
                match wake {
                    PromptPreparationEvent::Command(Some(UiCommand::CancelPromptPreparation { admission }))
                        if admission == prompt_prep.admission =>
                    {
                        prompt_prep.task.abort();
                        let _ = sink.send(UiEvent::PromptPreparationCancelled { admission });
                    }
                    PromptPreparationEvent::Command(Some(UiCommand::CancelRun)) => {
                        let admission = prompt_prep.admission;
                        prompt_prep.task.abort();
                        let _ = sink.send(UiEvent::PromptPreparationCancelled { admission });
                    }
                    PromptPreparationEvent::Command(Some(UiCommand::Shutdown))
                    | PromptPreparationEvent::Command(None) => {
                        let admission = prompt_prep.admission;
                        prompt_prep.task.abort();
                        let _ = sink.send(UiEvent::PromptPreparationCancelled { admission });
                        let _ = sink.send(UiEvent::Shutdown);
                        break;
                    }
                    PromptPreparationEvent::Command(Some(UiCommand::McpWatch { on })) => {
                        mcp_watch = on;
                        preparing = Some(prompt_prep);
                    }
                    PromptPreparationEvent::Command(Some(UiCommand::RequestWorkspaceFiles {
                        request_id,
                    })) => {
                        serve_workspace_files(
                            startup.options.workspace_root.clone(),
                            &sink,
                            request_id,
                        );
                        preparing = Some(prompt_prep);
                    }
                    PromptPreparationEvent::Command(Some(UiCommand::McpRefresh)) => {
                        if let Some(manager) = mcp_manager.as_ref() {
                            let _ = sink.send(UiEvent::McpServersChanged {
                                servers: mcp_server_views(manager),
                            });
                        }
                        preparing = Some(prompt_prep);
                    }
                    PromptPreparationEvent::Command(Some(UiCommand::CancelPromptPreparation { .. })) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: "A preparação ativa pertence a outro prompt".into(),
                        });
                        preparing = Some(prompt_prep);
                    }
                    PromptPreparationEvent::Command(Some(UiCommand::RunUserShell {
                        request_id,
                        ..
                    })) => {
                        reject_user_shell(&sink, request_id, "Aguarde ou cancele a preparação ativa");
                        preparing = Some(prompt_prep);
                    }
                    PromptPreparationEvent::Command(Some(command))
                        if sessions::is_session_command(&command) =>
                    {
                        sessions::refuse_session_command(
                            &sink,
                            &command,
                            "Aguarde ou cancele a preparação ativa",
                        );
                        preparing = Some(prompt_prep);
                    }
                    PromptPreparationEvent::Command(Some(_)) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Aguarde ou cancele a preparação ativa".into(),
                        });
                        preparing = Some(prompt_prep);
                    }
                    PromptPreparationEvent::Finished(result) => {
                        match result {
                            Ok(Ok(Some(fresh))) => {
                                if let Some(message) = fresh.persistence_warning {
                                    let _ = sink.send(UiEvent::Notification { message });
                                }
                                let credential = fresh.credential;
                                let Some(provider) = startup
                                    .oauth_session
                                    .as_ref()
                                    .map(|(provider, _)| *provider)
                                else {
                                    let _ = sink.send(UiEvent::PromptPreparationFailed {
                                        admission: prompt_prep.admission,
                                        message: "OAuth session disappeared during prompt preparation".into(),
                                    });
                                    continue;
                                };
                                match oauth_request(
                                    provider,
                                    &credential,
                                    startup.mode,
                                    startup.endpoint_override.as_deref(),
                                    startup.model_override.as_deref(),
                                ) {
                                    Ok(request) => {
                                        startup.request = Some(request);
                                        startup.oauth_session = Some((provider, credential));
                                    }
                                    Err(error) => {
                                        let _ = sink.send(UiEvent::PromptPreparationFailed {
                                            admission: prompt_prep.admission,
                                            message: error.to_string(),
                                        });
                                        continue;
                                    }
                                }
                            }
                            Ok(Ok(None)) => {}
                            Ok(Err(error)) => {
                                let _ = sink.send(UiEvent::PromptPreparationFailed {
                                    admission: prompt_prep.admission,
                                    message: format!("Authentication: {error}"),
                                });
                                continue;
                            }
                            Err(_) => {
                                let _ = sink.send(UiEvent::PromptPreparationFailed {
                                    admission: prompt_prep.admission,
                                    message: "OAuth credential preparation task failed".into(),
                                });
                                continue;
                            }
                        }
                        if let Err(message) = start_prompt_run(
                            PromptRunInput {
                                prompt: prompt_prep.prompt,
                                admission: Some(prompt_prep.admission),
                                skill_instructions: prompt_prep.skill_instructions,
                                user_shell_context: user_shell_context.clone(),
                            },
                            &mut startup,
                            &sink,
                            content_store.clone(),
                            &mut next_run_id,
                            &mut active,
                            &mut skill_memo,
                        ) {
                            let _ = sink.send(UiEvent::PromptPreparationFailed {
                                admission: prompt_prep.admission,
                                message,
                            });
                        }
                    }
                }
                continue;
            }
            if let Some(mut run) = pending.take() {
                if let Some(projector) = run.projector.take() {
                    if projector.is_finished() {
                        let _ = projector.join();
                        let result = run.result.take().expect("pending result");
                        if run.cancel_requested {
                            send_cancel_result(run.run_id, run.admission, result, &sink);
                            continue;
                        }
                        let deferred = std::mem::take(&mut run.delivery);
                        run.delivery = execution_result_events(
                            run.run_id,
                            run.admission,
                            result,
                            run.durable,
                        );
                        run.delivery.extend(deferred);
                    } else {
                        run.projector = Some(projector);
                        // Wake on either the pacing tick or a command so
                        // Cancel/Shutdown stays responsive while the provider
                        // task unwinds.
                        tokio::select! {
                            command = async_rx.recv() => {
                                if dispatch_pending_command(
                                    &mut run,
                                    command,
                                    &sink,
                                    &mut mcp_watch,
                                    &mcp_manager,
                                ) {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                        }
                        pending = Some(run);
                        continue;
                    }
                }

                match advance_pending_delivery(
                    &mut run,
                    &mut async_rx,
                    &sink,
                    &mut mcp_watch,
                    &mcp_manager,
                ) {
                    PendingDeliveryStep::Complete => {}
                    PendingDeliveryStep::Pending => {
                        // Backpressure pacing plus command wake: a cancel no
                        // longer waits for the drain to finish first.
                        tokio::select! {
                            command = async_rx.recv() => {
                                if dispatch_pending_command(
                                    &mut run,
                                    command,
                                    &sink,
                                    &mut mcp_watch,
                                    &mcp_manager,
                                ) {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                        }
                        pending = Some(run);
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
                    // Read-only MCP state stays live behind the login overlay:
                    // a dropped McpWatch toggle would wedge the /mcp view.
                    LoginEvent::Command(Some(UiCommand::McpWatch { on })) => {
                        mcp_watch = on;
                    }
                    LoginEvent::Command(Some(UiCommand::McpRefresh)) => {
                        if let Some(manager) = mcp_manager.as_ref() {
                            let _ = sink.send(UiEvent::McpServersChanged {
                                servers: mcp_server_views(manager),
                            });
                        }
                    }
                    LoginEvent::Command(Some(UiCommand::RequestWorkspaceFiles { request_id })) => {
                        serve_workspace_files(startup.options.workspace_root.clone(), &sink, request_id);
                    }
                    LoginEvent::Command(Some(UiCommand::RunUserShell { request_id, .. })) => {
                        reject_user_shell(&sink, request_id, "Conclua ou cancele o login antes de rodar comandos");
                    }
                    LoginEvent::Command(Some(command)) if sessions::is_session_command(&command) => {
                        sessions::refuse_session_command(
                            &sink,
                            &command,
                            "Conclua ou cancele o login",
                        );
                    }
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
                    match last_esc_at {
                        Some(armed_at) => {
                            let grace = ESC_GRACE_PERIOD.saturating_sub(
                                Instant::now().saturating_duration_since(armed_at),
                            );
                            tokio::select! {
                                command = async_rx.recv() => ActiveEvent::Command(command),
                                result = &mut run.task => ActiveEvent::Finished(Box::new(result)),
                                _ = tokio::time::sleep(grace) => ActiveEvent::CancelTimeout,
                                _ = mcp_watch_tick.tick(), if mcp_watch => ActiveEvent::McpTick,
                            }
                        }
                        None => tokio::select! {
                            command = async_rx.recv() => ActiveEvent::Command(command),
                            result = &mut run.task => ActiveEvent::Finished(Box::new(result)),
                            _ = mcp_watch_tick.tick(), if mcp_watch => ActiveEvent::McpTick,
                        },
                    }
                };
                match wake {
                    ActiveEvent::Command(Some(UiCommand::RetryProvider)) => {
                        let accepted = active.as_ref().is_some_and(|run| {
                            !run.cancellation.is_cancelled() && run.manual_retry.request()
                        });
                        if !accepted {
                            let _ = sink.send(UiEvent::Notification {
                                message: "Nenhuma falha de conexão está aguardando /retry".into(),
                            });
                        }
                    }
                    ActiveEvent::Command(None) => {
                        let _ = abort_active(&mut active).await;
                        break;
                    }
                    ActiveEvent::Command(Some(UiCommand::CancelPromptPreparation { admission })) => {
                        if let Some(run) = active.as_mut().filter(|run| {
                            run.admission == Some(admission)
                        }) {
                            if !run.cancellation_from_preparation {
                                run.cancellation_from_preparation = true;
                                run.cancellation.cancel();
                                let now = Instant::now();
                                last_esc_at = Some(now);
                                let run_id = run.run_id;
                                let _ = sink.send_control(UiEvent::CancellationRequested { run_id });
                                let _ = sink.send_control(UiEvent::CancellationStarted { run_id });
                            }
                            ignored_prep_cancel = true;
                        } else {
                            let _ = sink.send(UiEvent::Notification {
                                message: "No matching prompt preparation is active".into(),
                            });
                        }
                    }
                    ActiveEvent::Command(Some(UiCommand::CancelRun)) => {
                        if ignored_prep_cancel {
                            ignored_prep_cancel = false;
                            continue;
                        }
                        let now = Instant::now();
                        if esc_forces_quit(last_esc_at, now) {
                            pending =
                                abort_active_with_grace(&mut active, Duration::ZERO).await;
                            last_esc_at = None;
                        } else if let Some(run) = active.as_mut() {
                            let run_id = run.run_id;
                            run.cancellation.cancel();
                            last_esc_at = Some(now);
                            let _ = sink.send_control(UiEvent::CancellationRequested { run_id });
                            // The host has accepted the cancellation token and
                            // started unwinding the active run. RunCancelled
                            // remains the terminal confirmation.
                            let _ = sink.send_control(UiEvent::CancellationStarted { run_id });
                            // Hint only: try_send so a full lane (backpressure)
                            // can never wedge the cancel path itself.
                            let _ = sink.try_send(UiEvent::Notification {
                                message: "Interrupção solicitada para a execução atual".into(),
                            });
                        }
                    }
                    ActiveEvent::CancelTimeout => {
                        pending =
                            abort_active_with_grace(&mut active, Duration::ZERO).await;
                        last_esc_at = None;
                    }
                    ActiveEvent::Command(Some(UiCommand::Shutdown)) => {
                        let _ = abort_active(&mut active).await;
                        let _ = sink.send(UiEvent::Shutdown);
                        break;
                    }
                    ActiveEvent::Command(Some(UiCommand::SetMode(_))) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Aguarde ou cancele a execução antes de trocar o modo".into(),
                        });
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
                    ActiveEvent::Command(Some(UiCommand::RequestWorkspaceFiles {
                        request_id,
                    })) => {
                        serve_workspace_files(
                            startup.options.workspace_root.clone(),
                            &sink,
                            request_id,
                        );
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
                    // Read-only MCP state is safe mid-run: the overlay stays
                    // viewable; mutating actions still hit the catch-all.
                    ActiveEvent::Command(Some(UiCommand::McpWatch { on })) => {
                        mcp_watch = on;
                    }
                    ActiveEvent::Command(Some(UiCommand::McpRefresh)) => {
                        if let Some(manager) = mcp_manager.as_ref() {
                            let _ = sink.send(UiEvent::McpServersChanged {
                                servers: mcp_server_views(manager),
                            });
                        }
                    }
                    ActiveEvent::Command(Some(UiCommand::RunUserShell { request_id, .. })) => {
                        reject_user_shell(
                            &sink,
                            request_id,
                            "Aguarde ou cancele a execução antes de rodar um comando com !",
                        );
                    }
                    ActiveEvent::Command(Some(command)) if sessions::is_session_command(&command) => {
                        sessions::refuse_session_command(
                            &sink,
                            &command,
                            "Aguarde ou cancele a execução",
                        );
                    }
                    ActiveEvent::Command(Some(_)) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: "a run is already active".into(),
                        });
                    }
                    ActiveEvent::McpTick => {
                        if let Some(manager) = mcp_manager.as_ref() {
                            let revision = manager.revision();
                            if revision != mcp_seen_revision.load(Ordering::Relaxed) {
                                mcp_seen_revision.store(revision, Ordering::Relaxed);
                                let _ = sink.send(UiEvent::McpServersChanged {
                                    servers: mcp_server_views(manager),
                                });
                            }
                        }
                    }
                    ActiveEvent::Finished(result) => {
                        // An Esc-armed run that settles on its own still counts
                        // as user-cancelled, so the terminal reads RunCancelled
                        // (not RunFailed) via send_cancel_result.
                        let esc_cancelled = last_esc_at.is_some();
                        last_esc_at = None;
                        if let Ok(Ok(execution)) = &*result {
                            if execution.history.is_some() {
                                startup.options.task_facts.clone_from(&execution.task_facts);
                            }
                            if let Some(preflight) = execution.resume_preflight.clone() {
                                startup.options.artifact_ids =
                                    crate::headless::session_artifact_ids(&preflight);
                                startup.resume_path = Some(preflight.path.clone());
                                startup.resume_preflight = Some(preflight);
                            }
                            if let Some(history) = execution.history.as_ref() {
                                startup.options.history.clone_from(history);
                            } else if execution.result.code == ExitCode::Success {
                                if let Some(request) = startup.request.as_ref() {
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
                            ignored_prep_cancel = false;
                            pending = Some(PendingRun {
                                run_id: run.run_id,
                                admission: run.admission,
                                result: Some(*result),
                                projector: Some(run.projector),
                                delivery: VecDeque::new(),
                                cancellation: run.cancellation,
                                durable,
                                cancel_requested: esc_cancelled,
                                content_store: run.content_store,
                                workspace_root: run.workspace_root,
                            });
                        }
                    }
                }
                continue;
            }

            let command = if mcp_watch {
                // Status polling only runs while the /mcp overlay is open; a
                // revision counter keeps identical snapshots off the UI lane.
                tokio::select! {
                    command = async_rx.recv() => command,
                    _ = mcp_watch_tick.tick() => {
                        if let Some(manager) = mcp_manager.as_ref() {
                            let revision = manager.revision();
                            if revision != mcp_seen_revision.load(Ordering::Relaxed) {
                                mcp_seen_revision.store(revision, Ordering::Relaxed);
                                let _ = sink.send(UiEvent::McpServersChanged {
                                    servers: mcp_server_views(manager),
                                });
                            }
                        }
                        continue;
                    }
                }
            } else {
                async_rx.recv().await
            };
            let Some(command) = command else {
                break;
            };
            match command {
                UiCommand::PreparePrompt { prompt, admission }
                    if !prompt.trim().is_empty() =>
                {
                    let skill_instructions = match startup.options.workspace_root.as_deref() {
                        Some(workspace_root) => match resolve_slash_skill_command(workspace_root, &prompt) {
                            Ok(Some(SlashSkillCommand::Selected { name })) => {
                                let _ = sink.send(UiEvent::PromptPreparationHandled {
                                    admission,
                                    restore_draft: Some(format!("/{name} ")),
                                });
                                let _ = sink.send(UiEvent::Notification {
                                    message: format!("Skill /{name} selected. Add a task and send."),
                                });
                                continue;
                            }
                            Ok(Some(SlashSkillCommand::Invoke { name, body, source })) => {
                                Some(SkillInstructions { name, body, source })
                            }
                            Ok(None) => None,
                            Err(message) => {
                                let _ = sink.send(UiEvent::PromptPreparationFailed {
                                    admission,
                                    message,
                                });
                                continue;
                            }
                        },
                        None => None,
                    };

                    if startup.resume_path.is_none() {
                        match create_tui_session(&mut startup) {
                            Ok(Some((session_id, cwd))) => {
                                let skill_names =
                                    skill_memo.names(startup.options.workspace_root.clone());
                                let _ = sink.send(UiEvent::SessionSnapshot {
                                    session_id: slim_tui::api::SessionId(session_id.into()),
                                    cwd,
                                    skill_names,
                                });
                                announce_session_title(&mut startup, &sink);
                            }
                            Ok(None) => {}
                            Err(message) => {
                                let _ = sink.send(UiEvent::PromptPreparationFailed {
                                    admission,
                                    message,
                                });
                                continue;
                            }
                        }
                    }

                    let Some((provider, credential)) = startup.oauth_session.clone() else {
                        if startup.request.is_none() {
                            let _ = sink.send(UiEvent::PromptPreparationFailed {
                                admission,
                                message: "No provider connected. Use /login.".into(),
                            });
                            continue;
                        }
                        let task = tokio::spawn(async {
                            Ok::<Option<FreshCredential>, OAuthError>(None)
                        });
                        preparing = Some(ActivePromptPreparation {
                            admission,
                            prompt,
                            skill_instructions,
                            task,
                        });
                        continue;
                    };
                    let request = match oauth.request_fresh_credential(provider, credential) {
                        Ok(request) => request,
                        Err(error) => {
                            let _ = sink.send(UiEvent::PromptPreparationFailed {
                                admission,
                                message: format!("Authentication: {error}"),
                            });
                            continue;
                        }
                    };
                    let task = tokio::spawn(async move { request.wait().await.map(Some) });
                    let _ = sink.send(UiEvent::ActivityChanged {
                        label: "Checking authentication".into(),
                    });
                    preparing = Some(ActivePromptPreparation {
                        admission,
                        prompt,
                        skill_instructions,
                        task,
                    });
                }
                UiCommand::PreparePrompt { admission, .. } => {
                    let _ = sink.send(UiEvent::PromptPreparationFailed {
                        admission,
                        message: "prompt cannot be empty".into(),
                    });
                }
                UiCommand::CancelPromptPreparation { .. } => {
                    let _ = sink.send(UiEvent::Notification {
                        message: "No prompt preparation is active".into(),
                    });
                }
                UiCommand::ResumePrevious => {
                    let workspace = startup
                        .options
                        .workspace_root
                        .clone()
                        .expect("workspace root initialized");
                    match select_previous_tui_session(
                        &workspace,
                        startup.resume_path.as_deref(),
                    ) {
                        Ok(Some(selected)) => {
                            switch_session(
                                &mut startup,
                                &sink,
                                &mut skill_memo,
                                &user_shell_context,
                                selected,
                                "Previous session restored. Send a prompt to continue.",
                            );
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
                UiCommand::RenameSession { title } => {
                    sessions::rename_session(&mut startup, &sink, &title);
                }
                UiCommand::ListSessions { request_id } => {
                    sessions::serve_session_list(&startup, &sink, request_id);
                }
                UiCommand::ListTurns { request_id } => {
                    sessions::serve_turn_list(&startup, &sink, request_id);
                }
                UiCommand::ResumeSession { .. } | UiCommand::RewindSession { .. }
                    if user_shell.as_ref().is_some_and(UserShellRun::is_running) =>
                {
                    let _ = sink.send(UiEvent::Notification {
                        message: "Aguarde o comando com ! terminar antes de trocar de sessão".into(),
                    });
                }
                UiCommand::ResumeSession { id } => {
                    sessions::resume_session_by_id(
                        &mut startup,
                        &sink,
                        &mut skill_memo,
                        &user_shell_context,
                        &id,
                    );
                }
                UiCommand::RewindSession { first_seq } => {
                    sessions::rewind_current_session(
                        &mut startup,
                        &sink,
                        &mut skill_memo,
                        &user_shell_context,
                        first_seq,
                    );
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
                        LoginProvider::OpenCodeZen => {
                            let saved = tokio::task::spawn_blocking(move || {
                                save_api_key(ProviderKind::OpenCodeZen, &key_to_save)
                            })
                            .await;
                            match saved {
                                Ok(Ok(())) => {
                                    let model = startup
                                        .model_override
                                        .as_deref()
                                        .filter(|model| zen_model(model).is_some())
                                        .unwrap_or(OPENCODE_ZEN_DEFAULT_MODEL)
                                        .to_owned();
                                    startup.request = Some(ProviderRequest {
                                        prompt: String::new(),
                                        mode: startup.mode,
                                        kind: ProviderKind::OpenCodeZen,
                                        endpoint: startup
                                            .endpoint_override
                                            .clone()
                                            .unwrap_or_else(|| OPENCODE_ZEN_BASE_URL.into()),
                                        model: model.clone(),
                                        api_key: key,
                                        account_id: None,
                                        timeout: startup.timeout,
                                    });
                                    startup.oauth_session = None;
                                    let _ = sink.send(UiEvent::AuthStateChanged {
                                        provider: Some(LoginProvider::OpenCodeZen),
                                        authenticated: true,
                                    });
                                    let _ = sink.send(UiEvent::ModelChanged { model });
                                    let _ = sink.send(UiEvent::Notification {
                                        message: "Connected: OpenCode Zen".into(),
                                    });
                                }
                                Ok(Err(error)) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: error.to_string(),
                                    });
                                }
                                Err(_) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: "OpenCode Zen key save task failed".into(),
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
                        LoginProvider::Xai => {
                            let saved = tokio::task::spawn_blocking(move || {
                                save_api_key(ProviderKind::Xai, &key_to_save)
                            })
                            .await;
                            match saved {
                                Ok(Ok(())) => {
                                    let model = startup
                                        .model_override
                                        .as_deref()
                                        .filter(|m| is_xai_model_id(m))
                                        .unwrap_or(XAI_DEFAULT_MODEL)
                                        .to_owned();
                                    startup.request = Some(ProviderRequest {
                                        prompt: String::new(),
                                        mode: startup.mode,
                                        kind: ProviderKind::Xai,
                                        endpoint: startup
                                            .endpoint_override
                                            .clone()
                                            .unwrap_or_else(|| XAI_BASE_URL.into()),
                                        model: model.clone(),
                                        api_key: key,
                                        account_id: None,
                                        timeout: startup.timeout,
                                    });
                                    startup.oauth_session = None;
                                    let _ = sink.send(UiEvent::AuthStateChanged {
                                        provider: Some(LoginProvider::Xai),
                                        authenticated: true,
                                    });
                                    let _ = sink.send(UiEvent::ModelChanged { model });
                                    let _ = sink.send(UiEvent::Notification {
                                        message: "Connected: xAI".into(),
                                    });
                                }
                                Ok(Err(error)) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: error.to_string(),
                                    });
                                }
                                Err(_) => {
                                    let _ = sink.send(UiEvent::LoginFailed {
                                        message: "xAI key save task failed".into(),
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
                UiCommand::RefreshZenModels => {
                    let sink = sink.clone();
                    let Some(catalog) = zen_catalog.clone() else {
                        let _ = sink.send(UiEvent::Notification {
                            message: "OpenCode Zen catalog path is unavailable".into(),
                        });
                        continue;
                    };
                    let _ = sink.send(zen_catalog_event(catalog.load_or_fallback()));
                    if zen_inflight.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    let generation = zen_gen.fetch_add(1, Ordering::AcqRel) + 1;
                    let inflight = zen_inflight.clone();
                    let gen_cell = zen_gen.clone();
                    tokio::spawn(async move {
                        let result = catalog.refresh().await;
                        inflight.store(false, Ordering::Release);
                        if gen_cell.load(Ordering::Acquire) != generation {
                            return;
                        }
                        match result {
                            Ok(snapshot) => {
                                let _ = sink.send(zen_catalog_event(snapshot));
                            }
                            Err(error) => {
                                let _ = sink.send(UiEvent::Notification {
                                    message: format!(
                                        "OpenCode Zen live catalog unavailable; using cache/fallback: {error}"
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
                                            reasoning_levels: slim_core::provider::gateway_reasoning_levels(ProviderKind::ClinePass, &entry.id).iter().filter_map(|level| ReasoningEffort::parse(level)).collect(),
                                            id: entry.id,
                                            name: entry.name,
                                            context_window_tokens: entry.context_window,
                                            max_output_tokens: bundled
                                                .map(|model| model.max_output_tokens as u64)
                                                .unwrap_or(131_072),
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
                                        reasoning_levels: slim_core::provider::gateway_reasoning_levels(ProviderKind::ClinePass, m.id).iter().filter_map(|level| ReasoningEffort::parse(level)).collect(),
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
                                    | slim_core::ProviderError::Api { message, .. }
                                    | slim_core::ProviderError::TransientRemote { message } | slim_core::ProviderError::Remote { message } | slim_core::ProviderError::Http { message, .. } => message,
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
                    startup.effort = effort;
                    startup.options.reasoning_effort = Some(effort.id().into());
                    let _ = sink.send(UiEvent::Notification {
                        message: "Modelo aplicado nesta sessão · /model --default para salvar o padrão".into(),
                    });
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::SetZenModel { model, effort } => {
                    let supported = zen_model(&model).is_some_and(|model| {
                        model.reasoning_levels.is_empty()
                            || model.reasoning_levels.contains(&effort.id())
                    });
                    if !supported {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Unsupported OpenCode Zen model or reasoning effort".into(),
                        });
                        continue;
                    }
                    let switched = match activate_zen_provider(&mut startup, &oauth, &model) {
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
                            provider: Some(LoginProvider::OpenCodeZen),
                            authenticated: true,
                        });
                        let _ = sink.send(UiEvent::Notification {
                            message: "Connected: OpenCode Zen".into(),
                        });
                    }
                    startup.model_override = Some(model.clone());
                    startup.effort = effort;
                    startup.options.reasoning_effort = zen_model(&model)
                        .filter(|model| model.reasoning_levels.is_empty())
                        .map_or_else(|| Some(effort.id().into()), |_| None);
                    let _ = sink.send(UiEvent::Notification {
                        message: "Modelo aplicado nesta sessão · /model --default para salvar o padrão".into(),
                    });
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
                    let levels = slim_core::provider::gateway_reasoning_levels(ProviderKind::ClinePass, &model);
                    if !levels.is_empty() && !levels.contains(&effort.id()) {
                        let _ = sink.send(UiEvent::Notification { message: "Unsupported reasoning effort for this model".into() });
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
                    startup.effort = effort;
                    startup.options.reasoning_effort = Some(effort.id().into());
                    let _ = sink.send(UiEvent::Notification {
                        message: "Modelo aplicado nesta sessão · /model --default para salvar o padrão".into(),
                    });
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
                    let levels = slim_core::provider::gateway_reasoning_levels(ProviderKind::CommandCode, &model);
                    if !levels.is_empty() && !levels.contains(&effort.id()) {
                        let _ = sink.send(UiEvent::Notification { message: "Unsupported reasoning effort for this model".into() });
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
                    startup.effort = effort;
                    startup.options.reasoning_effort = Some(effort.id().into());
                    let _ = sink.send(UiEvent::Notification {
                        message: "Modelo aplicado nesta sessão · /model --default para salvar o padrão".into(),
                    });
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::SetXaiModel { model, effort } => {
                    if !is_xai_model_id(&model) {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Unsupported xAI model".into(),
                        });
                        continue;
                    }
                    let levels = slim_core::provider::gateway_reasoning_levels(ProviderKind::Xai, &model);
                    if !levels.is_empty() && !levels.contains(&effort.id()) {
                        let _ = sink.send(UiEvent::Notification { message: "Unsupported reasoning effort for this model".into() });
                        continue;
                    }
                    let switched = match activate_saved_api_key_provider(
                        &mut startup,
                        &oauth,
                        ProviderKind::Xai,
                        "xai",
                        XAI_BASE_URL,
                        &model,
                        "xAI",
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
                            provider: Some(LoginProvider::Xai),
                            authenticated: true,
                        });
                        let _ = sink.send(UiEvent::Notification {
                            message: "Connected: xAI".into(),
                        });
                    }
                    startup.model_override = Some(model.clone());
                    startup.effort = effort;
                    startup.options.reasoning_effort = Some(effort.id().into());
                    let _ = sink.send(UiEvent::Notification {
                        message: "Modelo aplicado nesta sessão · /model --default para salvar o padrão".into(),
                    });
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
                                    | ProviderKind::OpenCodeZen
                                    | ProviderKind::ClinePass
                                    | ProviderKind::CommandCode
                                    | ProviderKind::Xai
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
                            ProviderKind::Xai => (&["SLIM_API_KEY", "XAI_API_KEY"], "xAI"),
                            ProviderKind::OpenCodeZen => {
                                (&["SLIM_API_KEY", "OPENCODE_API_KEY"], "OpenCode Zen")
                            }
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
                UiCommand::RetryProvider => {
                    let _ = sink.send(UiEvent::Notification {
                        message: "Nenhuma falha de conexão está aguardando /retry".into(),
                    });
                }
                UiCommand::SaveModelDefault => {
                    if let Some(request) = startup.request.as_ref() {
                        persist_model(&sink, &request.model, startup.effort.id(),
                            (request.kind == ProviderKind::OpenAiCodex).then_some(startup.options.codex_fast));
                    } else {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Selecione um modelo antes de salvar o padrão".into(),
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
                            let skill_names =
                                skill_memo.names(startup.options.workspace_root.clone());
                            let _ = sink.send(UiEvent::SessionSnapshot {
                                session_id: slim_tui::api::SessionId(session_id.into()),
                                cwd,
                                skill_names,
                            });
                            announce_session_title(&mut startup, &sink);
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
                        None,
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
                            last_esc_at = None;
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
                    apply_mode_change(&mut startup, &sink, mode);
                }
                UiCommand::SetModel {
                    model: alias,
                    effort,
                    fast,
                } => {
                    if !ReasoningEffort::supported(alias).contains(&effort) {
                        let _ = sink.send(UiEvent::Notification {
                            message: "Unsupported reasoning effort for this Codex model".into(),
                        });
                        continue;
                    }
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
                    startup.effort = effort;
                    startup.options.reasoning_effort = Some(effort.id().into());
                    if let Some(request) = startup.request.as_mut() {
                        request.model.clone_from(&model);
                    }
                    startup.options.codex_fast = fast;
                    let _ = sink.send(UiEvent::Notification {
                        message: "Modelo aplicado nesta sessão · /model --default para salvar o padrão".into(),
                    });
                    let _ = sink.send(UiEvent::CodexSpeedChanged { fast });
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::McpWatch { on } => {
                    mcp_watch = on;
                }
                UiCommand::McpRefresh => match crate::config::load_layered() {
                    Ok(layered) => {
                        match mcp_manager.as_ref() {
                            Some(manager) => {
                                manager.reconcile(crate::mcp::specs_from_config(&layered.mcp));
                            }
                            None => {
                                let cwd = startup
                                    .options
                                    .workspace_root
                                    .clone()
                                    .unwrap_or_default();
                                if let Some(manager) =
                                    crate::mcp::build_mcp_manager(&layered.mcp, &cwd)
                                {
                                    startup.options.mcp = Some(McpHandle::new(manager.clone()));
                                    mcp_manager = Some(manager);
                                }
                            }
                        }
                        mcp_seen_revision.store(
                            mcp_manager.as_ref().map_or(0, |manager| manager.revision()),
                            Ordering::Relaxed,
                        );
                        let _ = sink.send(UiEvent::McpServersChanged {
                            servers: mcp_manager
                                .as_ref()
                                .map(|manager| mcp_server_views(manager))
                                .unwrap_or_default(),
                        });
                    }
                    Err(error) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: format!("config: {error}"),
                        });
                    }
                },
                UiCommand::McpTest { name } => {
                    spawn_mcp_op(
                        &mcp_manager,
                        &mcp_inflight,
                        &mcp_seen_revision,
                        &sink,
                        name,
                        |manager, name| async move {
                            manager
                                .test(&name)
                                .await
                                .map(|count| format!("ok — {count} tool(s)"))
                        },
                    );
                }
                UiCommand::McpReconnect { name } => {
                    spawn_mcp_op(
                        &mcp_manager,
                        &mcp_inflight,
                        &mcp_seen_revision,
                        &sink,
                        name,
                        |manager, name| async move {
                            manager.reconnect(&name).await.map(|()| "reconnected".to_owned())
                        },
                    );
                }
                UiCommand::McpDisconnect { name } => {
                    spawn_mcp_op(
                        &mcp_manager,
                        &mcp_inflight,
                        &mcp_seen_revision,
                        &sink,
                        name,
                        |manager, name| async move {
                            manager
                                .disconnect(&name)
                                .await
                                .map(|()| "disconnected".to_owned())
                        },
                    );
                }
                UiCommand::McpRemove { name } => {
                    match crate::config::remove_mcp_server(&name) {
                        Ok(edited) => {
                            let mut still_defined = false;
                            if let Some(manager) = mcp_manager.as_ref() {
                                // Reconcile against the reloaded config: the
                                // server may survive in another layer, and a
                                // blind remove would kill it anyway.
                                still_defined = match crate::config::load_layered() {
                                    Ok(layered) => {
                                        manager.reconcile(crate::mcp::specs_from_config(
                                            &layered.mcp,
                                        ));
                                        layered.mcp.servers.contains_key(&name)
                                    }
                                    Err(_) => {
                                        manager.remove(&name);
                                        false
                                    }
                                };
                                mcp_seen_revision
                                    .store(manager.revision(), Ordering::Relaxed);
                                let _ = sink.send(UiEvent::McpServersChanged {
                                    servers: mcp_server_views(manager),
                                });
                            }
                            let message = match edited {
                                Some(path) => {
                                    if still_defined {
                                        format!(
                                            "mcp {name} removed from {} (still defined in another layer)",
                                            path.display()
                                        )
                                    } else {
                                        format!("mcp {name} removed from {}", path.display())
                                    }
                                }
                                None => format!("mcp {name} is not in slim.toml"),
                            };
                            let _ = sink.send(UiEvent::Notification { message });
                        }
                        Err(error) => {
                            let _ = sink.send(UiEvent::Notification {
                                message: format!("mcp remove {name}: {error}"),
                            });
                        }
                    }
                }
                UiCommand::McpAdd {
                    name,
                    command,
                    args,
                    url,
                    global,
                } => {
                    let file = crate::config::FileMcpServerConfig {
                        command,
                        args: if args.is_empty() { None } else { Some(args) },
                        url,
                        ..crate::config::FileMcpServerConfig::default()
                    };
                    let server = crate::config::McpServerConfig {
                        command: file.command.clone(),
                        args: file.args.clone().unwrap_or_default(),
                        url: file.url.clone(),
                        ..crate::config::McpServerConfig::default()
                    };
                    let mut check = crate::config::McpConfig::default();
                    check.servers.insert(name.clone(), server.clone());
                    let path = if global {
                        crate::config::global_config_path()
                    } else {
                        Some(PathBuf::from(crate::config::PROJECT_CONFIG_FILE))
                    };
                    let result = check
                        .validate()
                        .and_then(|()| {
                            path.clone()
                                .ok_or_else(|| "global config path unavailable".to_owned())
                        })
                        .and_then(|path| {
                            crate::config::upsert_mcp_server_to(&path, &name, &file)
                                .map(|()| path)
                        });
                    match result {
                        Ok(path) => {
                            // Reconcile from the merged config so the live
                            // entry matches what the next load_layered sees
                            // (other layers may contribute env/headers/etc).
                            let cwd = startup
                                .options
                                .workspace_root
                                .clone()
                                .unwrap_or_default();
                            let manager = match mcp_manager.as_ref() {
                                Some(manager) => manager.clone(),
                                None => {
                                    let manager = Arc::new(McpManager::new(
                                        std::collections::BTreeMap::new(),
                                        cwd,
                                        Default::default(),
                                    ));
                                    startup.options.mcp =
                                        Some(McpHandle::new(manager.clone()));
                                    mcp_manager = Some(manager.clone());
                                    manager
                                }
                            };
                            if let Ok(layered) = crate::config::load_layered() {
                                manager.reconcile(crate::mcp::specs_from_config(&layered.mcp));
                            } else {
                                manager.upsert(crate::mcp::server_spec(&name, &server));
                            }
                            mcp_seen_revision.store(manager.revision(), Ordering::Relaxed);
                            let _ = sink.send(UiEvent::McpServersChanged {
                                servers: mcp_server_views(&manager),
                            });
                            let _ = sink.send(UiEvent::Notification {
                                message: format!("mcp {name} saved to {}", path.display()),
                            });
                        }
                        Err(error) => {
                            let _ = sink.send(UiEvent::Notification {
                                message: format!("mcp add {name}: {error}"),
                            });
                        }
                    }
                }
                UiCommand::CancelRun => {
                    if let Some(run) = user_shell.as_ref() {
                        run.cancellation.cancel();
                    }
                }
                UiCommand::RunUserShell {
                    request_id,
                    command,
                } => {
                    if user_shell.as_ref().is_some_and(UserShellRun::is_running) {
                        reject_user_shell(&sink, request_id, "Já há um comando com ! rodando");
                    } else {
                        match start_user_shell(
                            request_id,
                            command,
                            &startup,
                            &sink,
                            &user_shell_context,
                        ) {
                            Ok(run) => user_shell = Some(run),
                            Err(message) => reject_user_shell(&sink, request_id, &message),
                        }
                    }
                }
                UiCommand::Shutdown => {
                    if let Some(run) = user_shell.as_ref() {
                        run.cancellation.cancel();
                    }
                    let _ = sink.send(UiEvent::Shutdown);
                    break;
                }
                UiCommand::RequestContentPage {
                    handle,
                    request_id,
                    cursor,
                } => serve_content_page(&content_store, &sink, handle, request_id, cursor),
                UiCommand::RequestWorkspaceFiles { request_id } => serve_workspace_files(
                    startup.options.workspace_root.clone(),
                    &sink,
                    request_id,
                ),
            }
        }
        if let Some(manager) = mcp_manager.as_ref() {
            manager.disconnect_all().await;
        }
        if let Some(code_intelligence) = startup.options.code_intelligence.take() {
            code_intelligence.shutdown().await;
        }
        let _ = oauth_warning_stop.send(true);
        let _ = oauth_warning_forwarder.await;
        oauth.shutdown().await
    });
    let _ = forwarder.join();
    warnings
}

/// Scrubs text headed for UI surfaces: the CLI-side redactor plus every
/// configured MCP env/header value (a stderr tail can echo them).
fn redact_mcp_text(manager: &McpManager, input: &str) -> String {
    crate::auth::redact_with_secrets(input, &manager.sensitive_values())
}

/// Maps manager state to the UI view: target/error lines are bounded,
/// redacted, and never carry header or env values.
fn mcp_server_views(manager: &McpManager) -> Vec<McpServerView> {
    manager
        .statuses()
        .into_iter()
        .map(|info| {
            let (status, tools, error) = match &info.status {
                McpServerStatus::Disabled => (McpStatusView::Disabled, None, None),
                McpServerStatus::Disconnected => (McpStatusView::Disconnected, None, None),
                McpServerStatus::Connecting => (McpStatusView::Connecting, None, None),
                McpServerStatus::Ready { tools } => (McpStatusView::Ready, Some(tools.len()), None),
                McpServerStatus::Failed { error } => {
                    let first = error.lines().next().unwrap_or("");
                    let first = if first.chars().count() > 160 {
                        format!("{}…", first.chars().take(160).collect::<String>())
                    } else {
                        first.to_owned()
                    };
                    (
                        McpStatusView::Failed,
                        None,
                        Some(redact_mcp_text(manager, &first)),
                    )
                }
            };
            McpServerView {
                name: info.name,
                transport: info.transport,
                target: info.target,
                status,
                tools,
                error,
            }
        })
        .collect()
}

/// Runs a `/mcp` action off the UI lane: one in-flight op per server, a
/// notification with the outcome, then a fresh status snapshot.
fn spawn_mcp_op<F, Fut>(
    mcp_manager: &Option<Arc<McpManager>>,
    mcp_inflight: &Arc<Mutex<HashSet<String>>>,
    mcp_seen_revision: &Arc<AtomicU64>,
    sink: &EventSink,
    name: String,
    op: F,
) where
    F: FnOnce(Arc<McpManager>, String) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<String, slim_core::mcp::McpError>> + Send,
{
    let Some(manager) = mcp_manager.clone() else {
        let _ = sink.send(UiEvent::Notification {
            message: "no MCP servers configured".into(),
        });
        return;
    };
    {
        let mut guard = mcp_inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.insert(name.clone()) {
            return;
        }
    }
    let inflight = Arc::clone(mcp_inflight);
    let seen_revision = Arc::clone(mcp_seen_revision);
    let sink = sink.clone();
    tokio::spawn(async move {
        let result = op(manager.clone(), name.clone()).await;
        inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&name);
        // Bound and redact: op errors can embed server-controlled stderr or
        // remote strings that may echo configured env/header secrets.
        let detail = match &result {
            Ok(summary) => summary.chars().take(160).collect::<String>(),
            Err(error) => error.to_string(),
        };
        let detail = redact_mcp_text(&manager, &detail);
        let _ = sink.send(UiEvent::Notification {
            message: format!("mcp {name}: {detail}"),
        });
        seen_revision.store(manager.revision(), Ordering::Relaxed);
        let _ = sink.send(UiEvent::McpServersChanged {
            servers: mcp_server_views(&manager),
        });
    });
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
    mcp_watch: &mut bool,
    mcp_manager: &Option<Arc<McpManager>>,
) -> PendingDeliveryStep {
    let Some(event) = run.delivery.pop_front() else {
        return PendingDeliveryStep::Complete;
    };
    match sink.try_send(event) {
        Ok(()) if run.delivery.is_empty() => PendingDeliveryStep::Complete,
        Ok(()) => PendingDeliveryStep::Pending,
        Err(mpsc::TrySendError::Full(event)) => {
            run.delivery.push_front(event);
            if service_pending_command(run, commands, sink, mcp_watch, mcp_manager) {
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

/// Handles one already-received command (or channel close) while a run is
/// mid-delivery. Returns true when the worker should shut down.
fn dispatch_pending_command(
    run: &mut PendingRun,
    command: Option<UiCommand>,
    sink: &EventSink,
    mcp_watch: &mut bool,
    mcp_manager: &Option<Arc<McpManager>>,
) -> bool {
    match command {
        Some(UiCommand::CancelPromptPreparation { admission })
            if run.admission == Some(admission) =>
        {
            run.request_cancel();
            let _ = sink.send_control(UiEvent::CancellationRequested { run_id: run.run_id });
            let _ = sink.send_control(UiEvent::CancellationStarted { run_id: run.run_id });
        }
        Some(UiCommand::CancelRun) => {
            run.request_cancel();
            let _ = sink.send_control(UiEvent::CancellationRequested { run_id: run.run_id });
            let _ = sink.send_control(UiEvent::CancellationStarted { run_id: run.run_id });
        }
        Some(UiCommand::RequestContentPage {
            handle,
            request_id,
            cursor,
        }) => serve_content_page(&run.content_store, sink, handle, request_id, cursor),
        Some(UiCommand::RequestWorkspaceFiles { request_id }) => {
            serve_workspace_files(run.workspace_root.clone(), sink, request_id);
        }
        Some(UiCommand::RunUserShell { request_id, .. }) => {
            reject_user_shell(
                sink,
                request_id,
                "Aguarde a execução terminar antes de rodar um comando com !",
            );
        }
        Some(
            UiCommand::AnswerInput { request_id, .. }
            | UiCommand::Approve { request_id }
            | UiCommand::Reject { request_id },
        ) => run.delivery.push_back(unbound_interaction_ack(request_id)),
        // Read-only MCP state must survive the drain: a dropped watch toggle
        // would leave an open /mcp overlay stale until the next action.
        Some(UiCommand::McpWatch { on }) => *mcp_watch = on,
        Some(UiCommand::McpRefresh) => {
            if let Some(manager) = mcp_manager.as_ref() {
                let _ = sink.send(UiEvent::McpServersChanged {
                    servers: mcp_server_views(manager),
                });
            }
        }
        Some(command) if sessions::is_session_command(&command) => {
            sessions::refuse_session_command(sink, &command, "Aguarde a execução terminar");
        }
        Some(UiCommand::Shutdown) | None => {
            run.request_cancel();
            sink.send_control(UiEvent::Shutdown);
            return true;
        }
        Some(_) => {}
    }
    false
}

fn service_pending_command(
    run: &mut PendingRun,
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    sink: &EventSink,
    mcp_watch: &mut bool,
    mcp_manager: &Option<Arc<McpManager>>,
) -> bool {
    match commands.try_recv() {
        Ok(command) => dispatch_pending_command(run, Some(command), sink, mcp_watch, mcp_manager),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
            dispatch_pending_command(run, None, sink, mcp_watch, mcp_manager)
        }
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => false,
    }
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
fn persist_model(sink: &EventSink, model: &str, effort: &str, codex_fast: Option<bool>) {
    match crate::config::save_global_model(model, effort, codex_fast) {
        Ok(_) => {
            let _ = sink.send(UiEvent::Notification {
                message: "Modelo e esforço salvos como padrão para novas sessões".into(),
            });
        }
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

fn zen_catalog_event(snapshot: crate::opencode_zen_catalog::CatalogSnapshot) -> UiEvent {
    let models = snapshot
        .model_ids
        .iter()
        .filter_map(|id| zen_model(id))
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
        crate::opencode_zen_catalog::CatalogSource::Live => ZenCatalogSource::Live,
        crate::opencode_zen_catalog::CatalogSource::Cache => ZenCatalogSource::Cache,
        crate::opencode_zen_catalog::CatalogSource::Fallback => ZenCatalogSource::Fallback,
    };
    UiEvent::ZenCatalogLoaded { models, source }
}

fn command_code_catalog_event(snapshot: crate::command_code_catalog::CatalogSnapshot) -> UiEvent {
    let models = snapshot
        .models
        .into_iter()
        .map(|model| OpenCodeModelView {
            reasoning_levels: slim_core::provider::gateway_reasoning_levels(
                ProviderKind::CommandCode,
                &model.id,
            )
            .iter()
            .filter_map(|level| ReasoningEffort::parse(level))
            .collect(),
            id: model.id,
            name: model.name,
            context_window_tokens: model.context_window,
            max_output_tokens: 0,
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
        LoginProvider::Xai => Some(OAuthProvider::Xai),
        LoginProvider::OpenCodeGo
        | LoginProvider::OpenCodeZen
        | LoginProvider::ClinePass
        | LoginProvider::CommandCode => None,
    }
}

fn login_provider(provider: OAuthProvider) -> LoginProvider {
    match provider {
        OAuthProvider::Anthropic => LoginProvider::Anthropic,
        OAuthProvider::OpenAiCodex => LoginProvider::OpenAiCodex,
        OAuthProvider::Xai => LoginProvider::Xai,
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
        UiEvent::ToolOutput {
            batch_id,
            call_id,
            name,
            output,
            content_handle,
        } => {
            let (batch_id, call_id) = identity(batch_id, call_id);
            UiEvent::ToolOutput {
                batch_id,
                call_id,
                name,
                output,
                content_handle,
            }
        }
        UiEvent::ToolDiff {
            batch_id,
            call_id,
            diff,
        } => {
            let (batch_id, call_id) = identity(batch_id, call_id);
            UiEvent::ToolDiff {
                batch_id,
                call_id,
                diff,
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

const USER_SHELL_UI_BYTES: usize = 16 * 1024;
const USER_SHELL_CONTEXT_BYTES: usize = 8 * 1024;
const USER_SHELL_CONTEXT_NOTES: usize = 5;

/// What one `!command` printed, kept until the next prompt tells the model.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UserShellNote {
    /// Assigned by `UserShellContext::push`; orders notes and identifies what a run carried.
    seq: u64,
    command: String,
    /// Block text handed to the provider with the next prompt.
    text: String,
}

impl UserShellNote {
    fn label(&self) -> String {
        format!("[shell · {}]", truncate_chars(&self.command, 60))
    }
}

/// Notes from `!command` runs not yet delivered to the model. Shared with the
/// runner thread, which appends when a command ends.
#[derive(Clone, Debug, Default)]
struct UserShellContext(Arc<Mutex<UserShellNotes>>);

#[derive(Debug, Default)]
struct UserShellNotes {
    next_seq: u64,
    notes: Vec<UserShellNote>,
}

impl UserShellContext {
    fn push(&self, mut note: UserShellNote) {
        if let Ok(mut held) = self.0.lock() {
            note.seq = held.next_seq;
            held.next_seq += 1;
            held.notes.push(note);
            let excess = held.notes.len().saturating_sub(USER_SHELL_CONTEXT_NOTES);
            held.notes.drain(..excess);
        }
    }

    fn snapshot(&self) -> Vec<UserShellNote> {
        self.0
            .lock()
            .map(|held| held.notes.clone())
            .unwrap_or_default()
    }

    /// Drops the notes a started run carried (`carried` is a snapshot). A note
    /// pushed after the snapshot, even one that evicted an older note, stays.
    fn consume(&self, carried: &[UserShellNote]) {
        let Some(last) = carried.iter().map(|note| note.seq).max() else {
            return;
        };
        if let Ok(mut held) = self.0.lock() {
            held.notes.retain(|note| note.seq > last);
        }
    }

    /// Drops every pending note; a session switch must not leak them into the new conversation.
    fn clear(&self) {
        if let Ok(mut held) = self.0.lock() {
            held.notes.clear();
        }
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// `text` cut to at most `max_bytes` on a char boundary, with a note when cut.
fn cap_text_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[saída cortada em {} KB]",
        &text[..end],
        max_bytes / 1024
    )
}

/// A `!command` in flight on its own thread.
struct UserShellRun {
    cancellation: CancellationToken,
    done: Arc<AtomicBool>,
}

impl UserShellRun {
    fn is_running(&self) -> bool {
        !self.done.load(Ordering::Acquire)
    }
}

/// Always ends a `RunUserShell` request, also when the runner unwinds.
struct UserShellFinish {
    sink: EventSink,
    request_id: u64,
    done: Arc<AtomicBool>,
}

impl Drop for UserShellFinish {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let _ = self.sink.send(UiEvent::UserShellFinished {
            request_id: self.request_id,
        });
    }
}

fn reject_user_shell(sink: &EventSink, request_id: u64, message: &str) {
    let _ = sink.send(UiEvent::Notification {
        message: message.to_owned(),
    });
    let _ = sink.send(UiEvent::UserShellFinished { request_id });
}

/// Runs `command` for the user through the same `shell` tool the agent uses,
/// so mode gating, confinement to the workspace, the shared registry's
/// workspace revision and process cancellation all apply unchanged. Only Auto
/// mode may run it. The transcript shows it as a `shell` block marked `!`; a
/// bounded copy of its output is held for the model's next prompt.
fn start_user_shell(
    request_id: u64,
    command: String,
    startup: &TuiStartup,
    sink: &EventSink,
    context: &UserShellContext,
) -> Result<UserShellRun, String> {
    if startup.mode != slim_core::OperatingMode::Auto {
        return Err("Comandos com ! exigem o modo Auto (/mode auto)".into());
    }
    let command = command.trim().to_owned();
    if command.is_empty() {
        return Err("Uso: !COMANDO".into());
    }
    let cwd = startup.options.workspace_root.clone().map_or_else(
        || std::env::current_dir().map_err(|error| format!("current directory: {error}")),
        Ok,
    )?;
    let registry = startup
        .options
        .tool_registry
        .as_ref()
        .map(|shared| shared.registry())
        .unwrap_or_default();
    let secret = startup
        .request
        .as_ref()
        .map(|request| request.api_key.clone())
        .unwrap_or_default();
    let cancellation = CancellationToken::new();
    let done = Arc::new(AtomicBool::new(false));
    let finish = UserShellFinish {
        sink: sink.clone(),
        request_id,
        done: done.clone(),
    };
    let runner_sink = sink.clone();
    let runner_context = context.clone();
    let runner_cancellation = cancellation.clone();
    thread::Builder::new()
        .name("slim-user-shell".into())
        .spawn(move || {
            let _finish = finish;
            let sink = runner_sink;
            let batch_id = ToolBatchId(format!("user-shell-{request_id}").into());
            let call_id = ToolCallId(format!("user-shell-{request_id}").into());
            let shown = redact_for_ui(&command, &secret);
            let _ = sink.send(UiEvent::ToolStarted {
                batch_id: batch_id.clone(),
                call_id: call_id.clone(),
                name: "shell".into(),
                arguments_summary: format!("! {}", truncate_chars(&shown, 200)),
            });
            let arguments = serde_json::json!({ "command": command }).to_string();
            let started = Instant::now();
            let result = registry.execute_with_cancellation_and_progress(
                slim_core::OperatingMode::Auto,
                &cwd,
                "shell",
                &arguments,
                Some(&runner_cancellation),
                |progress| {
                    let _ = sink.send(UiEvent::ToolProgress {
                        batch_id: batch_id.clone(),
                        call_id: call_id.clone(),
                        name: "shell".into(),
                        preview: redact_for_ui(&progress.preview, &secret),
                        content_handle: None,
                    });
                },
            );
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let output = redact_for_ui(&result.output, &secret);
            let _ = sink.send(UiEvent::ToolOutput {
                batch_id: batch_id.clone(),
                call_id: call_id.clone(),
                name: "shell".into(),
                output: cap_text_bytes(&output, USER_SHELL_UI_BYTES),
                content_handle: None,
            });
            let _ = sink.send(UiEvent::ToolEnded {
                batch_id,
                call_id,
                name: "shell".into(),
                success: result.success,
                duration_ms,
            });
            runner_context.push(UserShellNote {
                seq: 0,
                command: shown.clone(),
                text: format!(
                    "[Comando executado pelo usuário com !: {shown}]\n{}\n[Fim do comando]",
                    cap_text_bytes(&output, USER_SHELL_CONTEXT_BYTES)
                ),
            });
        })
        .map_err(|error| format!("could not start the command thread: {error}"))?;
    Ok(UserShellRun { cancellation, done })
}

/// Candidate ceiling for `@` completion; one more is requested to detect the cut.
const WORKSPACE_FILE_LIMIT: usize = 20_000;

/// Lists workspace files off the worker loop (a large tree can take a while)
/// and answers `RequestWorkspaceFiles`. A failure still answers, with a
/// notification and an empty list, so the popup never waits on a request that
/// will not come back.
fn serve_workspace_files(root: Option<PathBuf>, sink: &EventSink, request_id: u64) {
    let answer = sink.clone();
    let fallback = sink.clone();
    let spawned = thread::Builder::new()
        .name("slim-workspace-files".into())
        .spawn(move || {
            let root = root.or_else(|| std::env::current_dir().ok());
            let listed = match root {
                Some(root) => {
                    slim_core::list_workspace_files(&root, WORKSPACE_FILE_LIMIT + 1, None)
                        .map_err(|error| error.to_string())
                }
                None => Err("workspace directory is unavailable".to_owned()),
            };
            let mut paths = listed.unwrap_or_else(|message| {
                let _ = answer.send(UiEvent::Notification {
                    message: format!("Não foi possível listar arquivos: {message}"),
                });
                Vec::new()
            });
            let truncated = paths.len() > WORKSPACE_FILE_LIMIT;
            paths.truncate(WORKSPACE_FILE_LIMIT);
            let _ = answer.send(UiEvent::WorkspaceFiles {
                request_id,
                paths,
                truncated,
            });
        });
    if spawned.is_err() {
        let _ = fallback.send(UiEvent::WorkspaceFiles {
            request_id,
            paths: Vec::new(),
            truncated: false,
        });
    }
}

const MENTION_MAX_FILES: usize = 8;
const MENTION_FILE_BYTES: usize = 256 * 1024;
const MENTION_TOTAL_BYTES: usize = 1024 * 1024;

/// File contents pulled in by the `@path` tokens of one prompt.
#[derive(Debug, Default)]
struct MentionAttachments {
    blocks: Vec<slim_core::provider::ProviderContentBlock>,
    /// One `[arquivo · path · size]` line per attached file, for the transcript.
    labels: Vec<String>,
}

fn format_mention_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0).replace('.', ",")
    }
}

/// Resolves every `@path` in `prompt` that names an existing file inside the
/// workspace and loads it through the confined core loader. Tokens that name
/// nothing (`@override`, `a@b.com`) stay plain text. A refused file
/// (secret-looking, binary, outside the workspace) or a hit limit fails the
/// whole prompt, so the model never sees a half-attached request.
fn load_prompt_mentions(root: &Path, prompt: &str) -> Result<MentionAttachments, String> {
    let paths = slim_core::mention_paths_in_prompt(root, prompt);
    if paths.len() > MENTION_MAX_FILES {
        return Err(format!(
            "Máximo de {MENTION_MAX_FILES} arquivos por prompt com @ (encontrados {})",
            paths.len()
        ));
    }
    let mut attachments = MentionAttachments::default();
    let mut total = 0usize;
    for path in paths {
        let budget = MENTION_FILE_BYTES.min(MENTION_TOTAL_BYTES - total);
        if budget == 0 {
            return Err(format!(
                "Limite de {} de arquivos anexados por prompt excedido em @{path}",
                format_mention_size(MENTION_TOTAL_BYTES)
            ));
        }
        let file = slim_core::load_mention_file(root, &path, budget).map_err(|error| {
            let reason = match error {
                slim_core::MentionError::Sensitive => {
                    "o nome parece conter segredos e não é anexado".to_owned()
                }
                slim_core::MentionError::Binary => "não é um arquivo de texto".to_owned(),
                slim_core::MentionError::OutsideWorkspace => "fora do workspace".to_owned(),
                slim_core::MentionError::NotRegularFile => "não é um arquivo comum".to_owned(),
                slim_core::MentionError::NotFound => "arquivo não encontrado".to_owned(),
                other => other.to_string(),
            };
            format!("@{path}: {reason}")
        })?;
        total += file.bytes;
        let note = if file.truncated {
            format!(" (truncado em {})", format_mention_size(file.bytes))
        } else {
            String::new()
        };
        attachments
            .blocks
            .push(slim_core::provider::ProviderContentBlock::text(format!(
                "[Arquivo anexado pelo usuário: {}{note}]\n{}\n[Fim de {}]",
                file.path, file.text, file.path
            )));
        attachments.labels.push(format!(
            "[arquivo · {} · {}{}]",
            file.path,
            format_mention_size(file.bytes),
            if file.truncated { " · truncado" } else { "" }
        ));
    }
    Ok(attachments)
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
    let (batch_id, call_id, name, output, job_output) = match kind {
        EventKind::ToolOutput {
            batch_id,
            call_id,
            name,
            output,
        } => (batch_id, call_id, name, output, false),
        EventKind::ToolJobOutput {
            batch_id,
            call_id,
            name,
            output,
        } => (batch_id, call_id, name, output, true),
        kind => return UiEvent::from_core(SessionEvent::new(seq, kind)),
    };
    let handle = ContentHandle(format!("tool:{run_id}:{batch_id}:{call_id}").into());
    let preview = slim_tui::api::tool_output_preview(&name, &output);
    let content_handle = store
        .lock()
        .map(|mut store| store.insert_owned(handle.clone(), output))
        .is_ok()
        .then_some(handle);
    let mut projected = UiEvent::from_core(SessionEvent::new(
        seq,
        if job_output {
            EventKind::ToolJobOutput {
                batch_id,
                call_id,
                name,
                output: preview,
            }
        } else {
            EventKind::ToolOutput {
                batch_id,
                call_id,
                name,
                output: preview,
            }
        },
    ))?;
    if let UiEvent::ToolOutput {
        content_handle: projected_handle,
        ..
    } = &mut projected
    {
        *projected_handle = content_handle;
    }
    Some(projected)
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
    admission: Option<PromptAdmission>,
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
    // Fresh discovery per run (off the UI thread): skills added mid-session
    // appear on the next prompt.
    let projector_skill_names = workspace_skill_names(&workspace_root, &mut String::new());
    let cancellation = CancellationToken::new();
    let (core_tx, core_rx) = SessionEventSender::bounded(1_024, cancellation.clone());
    let projector_cancellation = cancellation.clone();
    let projector_content_store = content_store.clone();
    let projector = thread::Builder::new()
        .name("slim-tui-projector".into())
        .spawn(move || {
            for event in core_rx {
                if let Some(event) = project_core_event(event, run_id, &projector_content_store) {
                    let mut event = attach_workspace_to_snapshot(event, &cwd_display);
                    if let UiEvent::SessionSnapshot { skill_names, .. } = &mut event {
                        skill_names.clone_from(&projector_skill_names);
                    }
                    let event = associate_projected_run(event, run_id);
                    if !sink.send_projected(event, &projector_cancellation) {
                        break;
                    }
                }
            }
        })
        .map_err(|error| format!("TUI projector thread: {error}"))?;
    options.cancellation = Some(cancellation.clone());
    let manual_retry = slim_core::runtime::ManualRetryHandle::default();
    options.manual_retry = Some(manual_retry.clone());
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
                false,
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
        admission,
        cancellation_from_preparation: false,
        task,
        projector,
        cancellation,
        durable,
        content_store,
        interaction_responder: Some(interaction_responder),
        manual_retry,
        workspace_root: Some(workspace_root),
    })
}

/// Esc escalation window while a run is active (Claude Code / OpenCode parity):
/// first Esc asks via the cancellation token (graceful, partial work kept), a
/// second Esc inside the window forces (`task.abort()`, no grace period) for
/// hung tool calls that ignore the token.
const ESC_FORCE_WINDOW: Duration = Duration::from_secs(1);
/// Grace after the first Esc for the run to observe the cancellation token
/// before the worker forces the abort (also the backstop for the L2 path).
const ESC_GRACE_PERIOD: Duration = Duration::from_secs(2);

fn esc_forces_quit(last_esc_at: Option<Instant>, now: Instant) -> bool {
    last_esc_at.is_some_and(|at| now.duration_since(at) <= ESC_FORCE_WINDOW)
}

async fn abort_active(active: &mut Option<ActiveRun>) -> Option<PendingRun> {
    abort_active_with_grace(active, ESC_GRACE_PERIOD).await
}

async fn abort_active_with_grace(
    active: &mut Option<ActiveRun>,
    grace: Duration,
) -> Option<PendingRun> {
    if let Some(mut run) = active.take() {
        let durable = run.durable;
        run.cancellation.cancel();
        let result = match tokio::time::timeout(grace, &mut run.task).await {
            Ok(result) => result,
            Err(_) => {
                run.task.abort();
                let result = (&mut run.task).await;
                run.cancellation.wait_for_native_work().await;
                result
            }
        };
        Some(PendingRun {
            run_id: run.run_id,
            admission: run.admission,
            result: Some(result),
            projector: Some(run.projector),
            delivery: VecDeque::new(),
            cancellation: run.cancellation,
            durable,
            cancel_requested: true,
            content_store: run.content_store,
            workspace_root: run.workspace_root,
        })
    } else {
        None
    }
}

fn run_stop_message(execution: &ProviderExecution) -> String {
    if let Some(message) = &execution.result.stop_message {
        return message.clone();
    }
    if execution.result.stop == "provider_error" {
        return execution.result.text.clone();
    }
    format_run_stop_message(
        &execution.result.stop,
        &execution.tool_results,
        execution.limits,
    )
}

fn send_cancel_result(
    run_id: u64,
    admission: Option<PromptAdmission>,
    result: Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>,
    sink: &EventSink,
) {
    let event = match result {
        Ok(Ok(execution)) => match execution.result.code {
            ExitCode::Success => UiEvent::RunCompleted { run_id },
            ExitCode::Cancelled => UiEvent::RunCancelled { run_id },
            _ if execution.result.stop == "provider_error" => UiEvent::RunFailed {
                run_id: Some(run_id),
                message: run_stop_message(&execution),
            },
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
    if let Some(admission) = admission {
        let _ = sink.send_control(correlate_prompt_event(event, admission));
    } else {
        sink.send_control(event);
    }
}

fn execution_result_events(
    run_id: u64,
    admission: Option<PromptAdmission>,
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
                _ if execution.result.stop == "provider_error" => UiEvent::RunFailed {
                    run_id: Some(run_id),
                    message: run_stop_message(&execution),
                },
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
    events.push_back(admission.map_or(terminal.clone(), |admission| {
        correlate_prompt_event(terminal, admission)
    }));
    events
}

fn correlate_prompt_event(event: UiEvent, admission: PromptAdmission) -> UiEvent {
    match event {
        UiEvent::RunCompleted { run_id } => UiEvent::PromptRunCompleted { admission, run_id },
        UiEvent::RunStopped { run_id, message } => UiEvent::PromptRunStopped {
            admission,
            run_id,
            message,
        },
        UiEvent::RunCancelled { run_id } => UiEvent::PromptRunCancelled { admission, run_id },
        UiEvent::RunFailed { run_id, message } => UiEvent::PromptRunFailed {
            admission,
            run_id,
            message,
        },
        event => event,
    }
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
        | ProviderError::TransientRemote { .. }
        | ProviderError::Remote { .. }
        | ProviderError::Api { .. }
        | ProviderError::Http { .. } => ExitCode::Provider,
        ProviderError::InvalidResponse { .. } => ExitCode::Internal,
    };
    TuiError::new(code, provider_error_message(error))
}

fn provider_error_message(error: ProviderError) -> String {
    match error {
        ProviderError::Cancelled => "provider request cancelled".into(),
        ProviderError::Transport { message, .. } => format!("provider transport failed: {message}"),
        ProviderError::MalformedToolCall => "provider returned a malformed tool call".into(),
        ProviderError::TransientRemote { message }
        | ProviderError::Remote { message }
        | ProviderError::Api { message, .. }
        | ProviderError::Http { message, .. }
        | ProviderError::InvalidResponse { message } => message,
    }
}

mod sessions;

#[cfg(test)]
mod local_session_tests;

#[cfg(test)]
mod session_management_tests;

#[cfg(test)]
mod slash_skill_tests;

#[cfg(test)]
mod cancel_tests;

#[cfg(test)]
mod mention_tests;

#[cfg(test)]
mod user_shell_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod restored_tool_tests;
