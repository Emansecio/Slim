use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;
use slim_tui::api::ModelAlias;

use crate::oauth::{OAuthProvider, OAuthService};
use crate::{
    load_local_images, redact, render_jsonl, render_provider_jsonl, render_provider_text,
    render_provider_verbose_text, render_text, run_fake_headless,
    run_provider_headless_with_options, run_provider_headless_with_resume_and_options,
    run_provider_headless_with_session_and_options, ExitCode, HeadlessRequest, HeadlessResult,
    OutputFormat, ProviderRequest, ProviderRunOptions,
};
use slim_core::session::{
    preflight_session, recover_durable_v2, PreflightStatus, SessionFormat, SessionPreflight,
};

use crate::headless::resolve_timeout_secs;

const HELP: &str = "Slim coding agent

Usage:
  Slim [TUI OPTIONS]
  Slim --headless [OPTIONS] [PROMPT...]

Modes:
  --tui              Open the fullscreen TUI (default)
  --headless         Run one prompt without the TUI
  --fake             Use the deterministic offline provider
  --plan             Allow inspection without workspace mutations
  --read-only        Disable workspace mutations
  --jev              TypeSafe-controlled actions, native reasoning OFF (TYPESAFE_API_KEY)

Provider:
  --provider NAME    Provider route
  --model MODEL      Model identifier (Codex: astra, sol, terra, luna)
  --effort LEVEL     Reasoning effort
  --fast             Enable Codex Fast (higher usage)
  --normal           Use normal Codex speed
  --endpoint URL     Override the provider endpoint

Input and sessions:
  --prompt TEXT      Prompt text; positional text or stdin also works
  --image PATH       Attach a local image (repeatable)
  --session PATH     Persist the run to a session file
  --resume PATH      Continue an existing session
  --recover PATH     Repair a durable session without running a prompt
  --abandon-pending  With --recover: abandon unfinished work; effects stay unverified
  --experiment-id ID Label durable run telemetry for a benchmark experiment
  --task-id ID       Label durable run telemetry for a benchmark task

Output:
  --verbose          Include detailed human-readable events
  --jsonl            Emit machine-readable JSON Lines

Other:
  -h, --help         Show this help
  -V, --version      Show the version

Examples:
  Slim
  Slim --headless --fake \"Summarize this repository\"
  Slim --headless --provider anthropic --model MODEL --prompt \"Review src\"
";

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
    pub effort: Option<String>,
    pub codex_fast: Option<bool>,
    pub endpoint: Option<String>,
    pub session_path: Option<String>,
    pub resume_path: Option<String>,
    pub recover_path: Option<String>,
    pub abandon_pending: bool,
    pub experiment_id: Option<String>,
    pub task_id: Option<String>,
    pub image_paths: Vec<String>,
    pub positional: Vec<String>,
    pub tui: bool,
    pub headless: bool,
    pub fake: bool,
    pub verbose: bool,
}

pub(crate) fn parse_cli_args(args: &[String]) -> Result<ParsedArgs, CliOutput> {
    let mut parsed = ParsedArgs {
        mode: OperatingMode::Auto,
        format: OutputFormat::Text,
        prompt: None,
        provider: None,
        model: None,
        effort: None,
        codex_fast: None,
        endpoint: None,
        session_path: None,
        resume_path: None,
        recover_path: None,
        abandon_pending: false,
        experiment_id: None,
        task_id: None,
        image_paths: Vec::new(),
        positional: Vec::new(),
        tui: false,
        headless: false,
        fake: false,
        verbose: false,
    };
    let mut index = 0;
    // Parsed flags are the only evidence of a selected mode: values such as
    // `--prompt "--jev"` are prompt text, not a mode selection.
    let mut jev_flag = false;
    let mut opposed_mode_flag = false;
    while index < args.len() {
        match args[index].as_str() {
            "--tui" => parsed.tui = true,
            "--headless" => parsed.headless = true,
            "--fake" => {
                parsed.fake = true;
                opposed_mode_flag = true;
            }
            "--abandon-pending" => parsed.abandon_pending = true,
            "--fast" => parsed.codex_fast = Some(true),
            "--normal" => parsed.codex_fast = Some(false),
            "--verbose" => parsed.verbose = true,
            "--jev" => {
                parsed.mode = OperatingMode::Jev;
                jev_flag = true;
            }
            "--plan" => {
                parsed.mode = OperatingMode::Plan;
                opposed_mode_flag = true;
            }
            "--read-only" => {
                parsed.mode = OperatingMode::ReadOnly;
                opposed_mode_flag = true;
            }
            "--jsonl" => parsed.format = OutputFormat::Jsonl,
            "--prompt" | "--provider" | "--model" | "--endpoint" | "--session" | "--resume"
            | "--recover" | "--image" | "--effort" | "--experiment-id" | "--task-id" => {
                let option = args[index].clone();
                index += 1;
                let value = args.get(index).cloned().ok_or_else(|| {
                    failure(ExitCode::Internal, &format!("missing value for {option}\n"))
                })?;
                match option.as_str() {
                    "--prompt" => parsed.prompt = Some(value),
                    "--provider" => parsed.provider = Some(value),
                    "--model" => parsed.model = Some(value),
                    "--effort" => parsed.effort = Some(value),
                    "--endpoint" => parsed.endpoint = Some(value),
                    "--session" => parsed.session_path = Some(value),
                    "--resume" => parsed.resume_path = Some(value),
                    "--recover" => parsed.recover_path = Some(value),
                    "--image" => parsed.image_paths.push(value),
                    "--experiment-id" => parsed.experiment_id = Some(value),
                    "--task-id" => parsed.task_id = Some(value),
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
    if jev_flag {
        if opposed_mode_flag {
            return Err(failure(
                ExitCode::InputRequired,
                "--jev is incompatible with --plan, --read-only and --fake\n",
            ));
        }
        if parsed
            .effort
            .as_deref()
            .is_some_and(|effort| !matches!(effort, "none" | "off"))
        {
            return Err(failure(
                ExitCode::InputRequired,
                "--jev requires native reasoning OFF; remove --effort or use none\n",
            ));
        }
        // OFF is imposed by the Jev provider policy, not saved as a TUI effort.
        parsed.effort = None;
    }
    let session_modes = [
        parsed.session_path.is_some(),
        parsed.resume_path.is_some(),
        parsed.recover_path.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if session_modes > 1 {
        return Err(failure(
            ExitCode::InputRequired,
            "session flags are mutually exclusive; use only one of --session, --resume, or --recover\n",
        ));
    }
    if parsed.abandon_pending && parsed.recover_path.is_none() {
        return Err(failure(
            ExitCode::InputRequired,
            "--abandon-pending requires --recover PATH\n",
        ));
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
        return success(HELP);
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
        effort,
        codex_fast,
        endpoint,
        session_path,
        resume_path,
        recover_path,
        abandon_pending,
        experiment_id,
        task_id,
        image_paths,
        positional,
        tui: _,
        headless: _,
        fake,
        verbose,
    } = match parse_cli_args(&args) {
        Ok(parsed) => parsed,
        Err(output) => return output,
    };
    if verbose && format == OutputFormat::Jsonl {
        return failure(
            ExitCode::InputRequired,
            "--verbose is available only with human text output; remove --jsonl\n",
        );
    }
    if fake && (experiment_id.is_some() || task_id.is_some()) {
        return failure(
            ExitCode::InputRequired,
            "--experiment-id and --task-id require a provider-backed run; remove --fake\n",
        );
    }
    let prompt = prompt.unwrap_or_else(|| {
        if positional.is_empty() {
            stdin.to_owned()
        } else {
            positional.join(" ")
        }
    });
    let environment_provider = std::env::var("SLIM_PROVIDER").ok();

    if recover_path.is_some()
        && (!prompt.trim().is_empty()
            || provider.is_some()
            || model.is_some()
            || endpoint.is_some()
            || experiment_id.is_some()
            || task_id.is_some()
            || !image_paths.is_empty()
            || mode != OperatingMode::Auto)
    {
        return failure(
            ExitCode::InputRequired,
            "--recover is recovery-only; remove prompt, positional input, and provider options\n",
        );
    }

    let resume_preflight = if let Some(path) = resume_path.as_deref() {
        if prompt.trim().is_empty() {
            return failure(
                ExitCode::InputRequired,
                "resume requires an explicit --prompt\n",
            );
        }
        match validate_resume_path(path) {
            Ok(preflight) => Some(preflight),
            Err(output) => return output,
        }
    } else {
        None
    };

    if let Some(path) = recover_path.as_deref() {
        if let Err(output) = recover_path_explicitly(path, abandon_pending) {
            return output;
        }
        if prompt.trim().is_empty() {
            return recovery_output(format_from_args(&args), abandon_pending);
        }
    }

    let provider_requested = mode == OperatingMode::Jev
        || provider.is_some()
        || endpoint.is_some()
        || model.is_some()
        || session_path.is_some()
        || resume_path.is_some()
        || recover_path.is_some()
        || experiment_id.is_some()
        || task_id.is_some()
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
            "opencode-go" | "opencode_go" | "go" => ProviderKind::OpenCodeGo,
            "opencode-zen" | "opencode_zen" | "zen" => ProviderKind::OpenCodeZen,
            "clinepass" | "cline-pass" | "cp" => ProviderKind::ClinePass,
            "command-code" | "commandcode" | "cmd" => ProviderKind::CommandCode,
            "xai" | "grok" => ProviderKind::Xai,
            _ => {
                return failure(
                    ExitCode::Provider,
                    "unsupported provider; use openai-compatible, openai-codex, anthropic, opencode-go, opencode-zen, clinepass, command-code, or xai\n",
                )
            }
        };
        let default_endpoint = default_provider_endpoint(kind);
        if codex_fast.is_some() && kind != ProviderKind::OpenAiCodex {
            return failure(
                ExitCode::InputRequired,
                "--fast/--normal require the openai-codex provider\n",
            );
        }
        let default_model = default_provider_model(kind);
        let layered_config = match crate::config::load_layered() {
            Ok(config) => config,
            Err(error) => return failure(ExitCode::Internal, &format!("config error: {error}\n")),
        };
        let compaction_policy = match layered_config.compaction_policy() {
            Ok(policy) => policy,
            Err(error) => return failure(ExitCode::Internal, &format!("config error: {error}\n")),
        };
        let credential = match crate::auth::resolve_provider_credential(kind) {
            Ok(Some(credential)) => credential,
            // The Zen free tier authenticates with the literal `public`
            // bearer; the adapter still sends `x-opencode-session`.
            Ok(None) if kind == ProviderKind::OpenCodeZen => {
                crate::auth::ProviderCredential {
                    access: slim_core::provider::OPENCODE_ZEN_PUBLIC_KEY.into(),
                    account_id: None,
                    oauth: false,
                }
            }
            Ok(None) => {
                return failure(
                    ExitCode::Auth,
                    "provider selected but no API key or OAuth credential was found (SLIM_API_KEY, provider-specific key, or auth.json)\n",
                )
            }
            Err(error) => {
                return failure(ExitCode::Auth, &format!("provider auth configuration invalid: {error}\n"))
            }
        };
        let (api_key, account_id) = if credential.oauth {
            match refresh_headless_oauth(kind) {
                Ok(refreshed) => refreshed,
                Err(error) => {
                    return failure(
                        ExitCode::Auth,
                        &format!("provider auth configuration invalid: {error}\n"),
                    )
                }
            }
        } else {
            let api_key = credential.access.clone();
            let account_id = match kind {
                ProviderKind::OpenAiCodex => {
                    let from_store = credential
                        .account_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned);
                    match from_store.or_else(|| crate::oauth::codex_account_id(&api_key).ok()) {
                        Some(account_id) => Some(account_id),
                        None => {
                            return failure(
                                ExitCode::Auth,
                                "Codex OAuth account id is required; use a ChatGPT access token JWT or TUI /login\n",
                            )
                        }
                    }
                }
                _ => None,
            };
            (api_key, account_id)
        };
        let content_blocks = match load_local_images(&image_paths) {
            Ok(blocks) => blocks,
            Err(error) => return failure(ExitCode::InputRequired, &format!("image: {error}\n")),
        };
        // G248: a configured model only applies when it validates against the
        // active provider kind; otherwise the provider default is used.
        let explicit_model = model.or_else(|| std::env::var("SLIM_MODEL").ok());
        if explicit_model
            .as_deref()
            .is_some_and(|model| !crate::provider_compatible_model(kind, model))
        {
            return failure(
                ExitCode::InputRequired,
                "explicit model is unsupported by the selected provider\n",
            );
        }
        let model = explicit_model
            .or(layered_config.model)
            .filter(|model| crate::provider_compatible_model(kind, model))
            .unwrap_or_else(|| default_model.into());
        let model = crate::canonical_provider_model(kind, &model);
        let timeout = match resolve_timeout_secs(layered_config.timeout_secs) {
            Ok(timeout) => timeout,
            Err(error) => return provider_failure(error),
        };
        let request = ProviderRequest {
            prompt,
            mode,
            kind,
            endpoint: endpoint
                .or_else(|| std::env::var("SLIM_ENDPOINT").ok())
                .or(layered_config.endpoint)
                .unwrap_or_else(|| default_endpoint.into()),
            model,
            api_key,
            account_id,
            timeout,
        };
        // Reasoning effort: CLI/TUI env override first, then slim.toml
        // (global + project layers). Empty string means "unset".
        let codex_fast = codex_fast.or(layered_config.codex_fast).unwrap_or(false);
        let effort = effort
            .or_else(|| std::env::var("SLIM_EFFORT").ok())
            .filter(|value| !value.is_empty())
            .or(layered_config.effort);
        let mut options = ProviderRunOptions::default()
            .with_content_blocks(content_blocks)
            .with_compaction_handle(slim_core::context::CompactionHandle::new(compaction_policy));
        if let Some(experiment_id) = experiment_id {
            options = options.with_experiment_id(experiment_id);
        }
        if let Some(task_id) = task_id {
            options = options.with_task_id(task_id);
        }
        if let Some(effort) = effort {
            options = options.with_reasoning_effort(effort);
        }
        options.codex_fast = codex_fast;
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
        let provider_result = match resume_preflight {
            Some(preflight) => {
                crate::headless::run_provider_headless_with_resume_preflight_and_options(
                    request, preflight, options,
                )
            }
            None => match recover_path {
                Some(path) => run_provider_headless_with_resume_and_options(request, path, options),
                None => match session_path {
                    Some(path) => {
                        run_provider_headless_with_session_and_options(request, path, options)
                    }
                    None => run_provider_headless_with_options(request, options),
                },
            },
        };
        let result = match provider_result {
            Ok(result) => result,
            Err(error) => return provider_failure(error),
        };
        let rendered = match format {
            OutputFormat::Text if verbose => render_provider_verbose_text(&result),
            OutputFormat::Text => render_provider_text(&result),
            OutputFormat::Jsonl => match render_provider_jsonl(&result) {
                Ok(output) => output,
                Err(error) => {
                    return failure(ExitCode::Internal, &format!("render error: {error}\n"))
                }
            },
        };
        let (stdout, stderr) = if format == OutputFormat::Text && result.stop == "provider_error" {
            (String::new(), rendered)
        } else {
            (rendered, String::new())
        };
        return CliOutput {
            code: result.code,
            stdout,
            stderr,
        };
    }

    if !fake {
        return failure(
            ExitCode::Auth,
            "no provider connected; pass --provider with credentials, or --fake for the offline checkpoint\n",
        );
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

fn refresh_headless_oauth(kind: ProviderKind) -> Result<(String, Option<String>), String> {
    let provider = match kind {
        ProviderKind::Anthropic => OAuthProvider::Anthropic,
        ProviderKind::OpenAiCodex => OAuthProvider::OpenAiCodex,
        ProviderKind::Xai => OAuthProvider::Xai,
        _ => return Err("OAuth is not available for this provider".into()),
    };
    let oauth = OAuthService::production().map_err(|error| error.to_string())?;
    let stored = oauth
        .credential(provider)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "OAuth credential is missing from the auth store".to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let fresh = runtime
        .block_on(oauth.fresh_credential(provider, stored))
        .map_err(|error| error.to_string())?;
    let account_id = match provider {
        OAuthProvider::OpenAiCodex => Some(
            fresh
                .credential
                .account_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    "Codex OAuth account id is required; use a ChatGPT access token JWT or TUI /login"
                        .to_string()
                })?,
        ),
        OAuthProvider::Anthropic => Some(fresh.credential.account_id.unwrap_or_default()),
        OAuthProvider::Xai => None,
    };
    Ok((fresh.credential.access, account_id))
}

pub(crate) fn default_provider_endpoint(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAiCompatible => "https://api.openai.com/v1/chat/completions",
        ProviderKind::OpenAiCodex => slim_core::provider::CODEX_BACKEND_ENDPOINT,
        ProviderKind::Anthropic => "https://api.anthropic.com/v1/messages",
        ProviderKind::OpenCodeGo => slim_core::provider::OPENCODE_GO_BASE_URL,
        ProviderKind::OpenCodeZen => slim_core::provider::OPENCODE_ZEN_BASE_URL,
        ProviderKind::ClinePass => slim_core::provider::CLINEPASS_BASE_URL,
        ProviderKind::CommandCode => slim_core::provider::COMMANDCODE_BASE_URL,
        ProviderKind::Xai => slim_core::provider::XAI_BASE_URL,
    }
}

pub(crate) fn default_provider_model(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAiCompatible => "gpt-4o-mini",
        ProviderKind::OpenAiCodex => ModelAlias::Sol.id(),
        ProviderKind::Anthropic => "claude-sonnet-4-6",
        ProviderKind::OpenCodeGo => slim_core::provider::OPENCODE_GO_DEFAULT_MODEL,
        ProviderKind::OpenCodeZen => slim_core::provider::OPENCODE_ZEN_DEFAULT_MODEL,
        ProviderKind::ClinePass => slim_core::provider::CLINEPASS_DEFAULT_MODEL,
        ProviderKind::CommandCode => slim_core::provider::COMMANDCODE_DEFAULT_MODEL,
        ProviderKind::Xai => slim_core::provider::XAI_DEFAULT_MODEL,
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
        slim_core::ProviderError::Transport { message, .. } => (
            ExitCode::Provider,
            format!("provider transport failed: {message}"),
        ),
        slim_core::ProviderError::MalformedToolCall => (
            ExitCode::Provider,
            "provider returned a malformed tool call".into(),
        ),
        slim_core::ProviderError::TransientRemote { message }
        | slim_core::ProviderError::Remote { message }
        | slim_core::ProviderError::Api { message, .. }
        | slim_core::ProviderError::Http { message, .. }
        | slim_core::ProviderError::InvalidResponse { message } => (
            ExitCode::Provider,
            format!("provider error: {}", redact(&message)),
        ),
    };
    failure(code, &format!("{message}\n"))
}

fn validate_resume_path(path: &str) -> Result<SessionPreflight, CliOutput> {
    let report = match preflight_session(path) {
        Ok(report) => report,
        Err(error) => {
            return Err(failure(
                ExitCode::Blocked,
                &format!("resume preflight failed: {error}\n"),
            ))
        }
    };
    if report.format != Some(SessionFormat::DurableV2) {
        return Err(failure(
            ExitCode::Blocked,
            "resume requires durable schema v2; legacy session schema v1 is not migrated\n",
        ));
    }
    if !report.can_resume_v2() {
        let message = match report.status {
            PreflightStatus::TornTail { .. } => {
                "resume requires explicit recovery before using a torn durable session tail"
            }
            PreflightStatus::Invalid { .. } => "resume rejected invalid durable session",
            PreflightStatus::UnsupportedSchema { .. } => "resume rejected unsupported session schema",
            PreflightStatus::Healthy if report.needs_separator => {
                "resume requires explicit recovery before appending to a session without a final separator"
            }
            PreflightStatus::Healthy if report.sequence_overflow => {
                "resume rejected durable session sequence overflow"
            }
            PreflightStatus::Healthy => "resume rejected durable session preflight",
        };
        return Err(failure(ExitCode::Blocked, &format!("{message}\n")));
    }
    if report.summary.pending_count() > 0
        || report.summary.claimed_count() > 0
        || report.summary.suspended_count() > 0
    {
        return Err(failure(
            ExitCode::Blocked,
            "resume requires an explicit decision for existing pending, claimed, or suspended durable work; inspect prior effects, then use Slim --headless --recover PATH --abandon-pending to abandon without replay\n",
        ));
    }
    Ok(report)
}

fn recover_path_explicitly(path: &str, abandon_pending: bool) -> Result<(), CliOutput> {
    match recover_durable_v2(path) {
        Ok(mut repo) => {
            if abandon_pending {
                abandon_pending_work(&mut repo).map_err(|error| {
                    failure(
                        ExitCode::Blocked,
                        &format!("pending work recovery rejected: {error}\n"),
                    )
                })?;
            }
            drop(repo);
            Ok(())
        }
        Err(error) => Err(failure(
            ExitCode::Blocked,
            &format!("explicit recovery rejected: {error}\n"),
        )),
    }
}

/// The caller explicitly abandons ownership of unfinished work, never its effects.
/// The repository lock remains held throughout reconstruction and append.
fn abandon_pending_work(repo: &mut slim_core::session::JsonlRepo) -> std::io::Result<()> {
    use slim_core::session::{
        DurableEntry, DurableEntryRole, DurableErrorClass, DurableOperation, DurableOperationKind,
        DurableRecord, DurableRepo, ResumePlan,
    };
    let report = SessionPreflight::from_open_repo(repo);
    let plan = ResumePlan::from_records(repo.records()).map_err(std::io::Error::other)?;
    // Canonical tool-phase logs still need their own reconciliation. Streaming
    // conversation entries can be closed with explicit unknown-result records.
    if !plan.tools().incomplete().is_empty() {
        return Err(std::io::Error::other(
            "unfinished durable tool phases need reconciliation; no execution result was inferred",
        ));
    }
    let missing_results =
        slim_core::session::recovery_tool_results(repo.records().iter().filter_map(|record| {
            match record {
                DurableRecord::Entry { entry, .. } => Some(entry),
                _ => None,
            }
        }))
        .map_err(std::io::Error::other)?;
    let mut suffix = Vec::new();
    let mut seq = repo.next_seq()?;
    let mut parent = repo.records().iter().rev().find_map(|record| match record {
        DurableRecord::Entry { entry, .. } => Some(entry.entry_id.clone()),
        _ => None,
    });
    for mut entry in missing_results {
        if !report
            .summary
            .pending_operation_ids
            .contains(&entry.operation_id)
            && !report
                .summary
                .claimed_operation_ids
                .contains(&entry.operation_id)
            && !report
                .summary
                .suspended_operation_ids
                .contains(&entry.operation_id)
        {
            return Err(std::io::Error::other(
                "incomplete tool transcript belongs to terminal work",
            ));
        }
        entry.entry_id = format!("recovery-result-{seq}");
        entry.parent_entry_id = parent.take();
        parent = Some(entry.entry_id.clone());
        suffix.push(DurableRecord::Entry { seq, entry });
        seq = seq
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("recovery sequence overflow"))?;
    }
    for operation_id in report
        .summary
        .pending_operation_ids
        .iter()
        .chain(&report.summary.claimed_operation_ids)
        .chain(&report.summary.suspended_operation_ids)
    {
        for attempt in plan
            .attempts()
            .attempts_for(operation_id)
            .iter()
            .filter(|attempt| attempt.outcome.is_none())
        {
            suffix.push(DurableRecord::Operation {
                seq,
                operation: DurableOperation {
                    operation_id: operation_id.clone(),
                    kind: DurableOperationKind::ProviderAttemptFailed {
                        attempt_id: attempt.attempt_id.clone(),
                        error: DurableErrorClass::Unknown,
                    },
                },
            });
            seq = seq
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("recovery sequence overflow"))?;
        }
        let mut entry_id = format!("recovery-{seq}");
        while plan.state().entries().contains_key(&entry_id) {
            entry_id.push('_');
        }
        suffix.push(DurableRecord::Entry { seq, entry: DurableEntry {
            entry_id: entry_id.clone(), role: DurableEntryRole::Assistant,
            content: "[Recovery decision] The user explicitly abandoned this unfinished operation without replay. Prior effects remain unverified and were not undone. Inspect the workspace and external state before repeating any action. This record does not confirm task completion.".into(),
            parent_entry_id: parent.take(), operation_id: operation_id.clone(),
            tool_call_id: None, tool_calls: Vec::new(), content_blocks: Vec::new(),
        }});
        parent = Some(entry_id);
        seq = seq
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("recovery sequence overflow"))?;
        suffix.push(DurableRecord::Operation {
            seq,
            operation: DurableOperation {
                operation_id: operation_id.clone(),
                kind: DurableOperationKind::Aborted,
            },
        });
        seq = seq
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("recovery sequence overflow"))?;
    }
    repo.append_batch(suffix)
}

fn format_from_args(args: &[String]) -> OutputFormat {
    if args.iter().any(|arg| arg == "--jsonl") {
        OutputFormat::Jsonl
    } else {
        OutputFormat::Text
    }
}

fn recovery_output(format: OutputFormat, abandoned: bool) -> CliOutput {
    let result = HeadlessResult {
        code: ExitCode::Success,
        message: if abandoned {
            "recovery_complete: unfinished work abandoned without replay; effects remain unverified"
                .into()
        } else {
            "recovery_complete".into()
        },
    };
    let stdout = match format {
        OutputFormat::Text => render_text(&result),
        OutputFormat::Jsonl => {
            render_jsonl(&result).unwrap_or_else(|_| "recovery_complete\n".into())
        }
    };
    CliOutput {
        code: ExitCode::Success,
        stdout,
        stderr: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_cli_args;

    #[test]
    fn benchmark_labels_are_parsed_as_explicit_values() {
        let args = [
            "--headless",
            "--experiment-id",
            "jev-arm-b",
            "--task-id",
            "repo-17",
            "--prompt",
            "inspect",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let parsed = parse_cli_args(&args).expect("parse benchmark labels");
        assert_eq!(parsed.experiment_id.as_deref(), Some("jev-arm-b"));
        assert_eq!(parsed.task_id.as_deref(), Some("repo-17"));
        assert_eq!(parsed.prompt.as_deref(), Some("inspect"));
    }
}
