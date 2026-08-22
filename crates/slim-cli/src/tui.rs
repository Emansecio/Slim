use std::fmt;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use slim_core::provider::ProviderKind;
use slim_core::runtime::CancellationToken;
use slim_core::{ProviderError, SessionEvent};
use slim_tui::api::{LoginProvider, ModelAlias, ReasoningEffort, UiChannels, UiCommand, UiEvent};

use crate::cli::parse_cli_args;
use crate::exit_codes::ExitCode;
use crate::headless::{
    execute_provider_turn, execute_provider_turn_async, OutputFormat, ProviderExecution,
    ProviderRequest, ProviderRunOptions,
};
use crate::oauth::{OAuthCredential, OAuthError, OAuthProgress, OAuthProvider, OAuthService};
use crate::{load_local_images, resolve_api_key};

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
    mode: slim_core::OperatingMode,
    effort: ReasoningEffort,
    endpoint_override: Option<String>,
    model_override: Option<String>,
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
    let initial_prompt = parsed
        .prompt
        .or_else(|| (!parsed.positional.is_empty()).then(|| parsed.positional.join(" ")));
    let layered_config = crate::config::load_layered()
        .map_err(|error| TuiError::new(ExitCode::Internal, format!("config error: {error}")))?;
    let endpoint_override = parsed
        .endpoint
        .or_else(|| std::env::var("SLIM_ENDPOINT").ok())
        .or(layered_config.endpoint);
    let model_override = parsed
        .model
        .or_else(|| std::env::var("SLIM_MODEL").ok())
        .or(layered_config.model);
    let effort = std::env::var("SLIM_EFFORT")
        .ok()
        .and_then(|value| ReasoningEffort::parse(&value))
        .or_else(|| {
            layered_config
                .effort
                .as_deref()
                .and_then(ReasoningEffort::parse)
        })
        .unwrap_or(ReasoningEffort::High);
    let explicit_provider = parsed
        .provider
        .or_else(|| std::env::var("SLIM_PROVIDER").ok());
    let active_oauth = oauth.active().ok().flatten();

    let (request, oauth_session) = if let Some(provider_name) = explicit_provider {
        let kind = provider_kind(&provider_name)?;
        let api_request = (kind != ProviderKind::OpenAiCodex)
            .then(|| {
                api_key_request(
                    kind,
                    parsed.mode,
                    endpoint_override.as_deref(),
                    model_override.as_deref(),
                )
            })
            .flatten();
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
    } else if let Some(request) = api_key_request(
        ProviderKind::OpenAiCompatible,
        parsed.mode,
        endpoint_override.as_deref(),
        model_override.as_deref(),
    ) {
        (Some(request), None)
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
    };

    let content_blocks = load_local_images(&parsed.image_paths)
        .map_err(|error| TuiError::new(ExitCode::InputRequired, format!("image: {error}")))?;
    let mut options = ProviderRunOptions::default().with_content_blocks(content_blocks);
    if request.as_ref().is_some_and(|request| {
        request.kind == ProviderKind::OpenAiCodex || ModelAlias::parse(&request.model).is_some()
    }) {
        options = options.with_reasoning_effort(effort.id());
    }
    Ok(TuiStartup {
        request,
        oauth_session,
        options,
        initial_prompt,
        mode: parsed.mode,
        effort,
        endpoint_override,
        model_override,
    })
}

fn provider_kind(name: &str) -> Result<ProviderKind, TuiError> {
    match name.to_ascii_lowercase().as_str() {
        "openai" | "openai-compatible" | "openai_compatible" => Ok(ProviderKind::OpenAiCompatible),
        "openai-codex" | "codex" => Ok(ProviderKind::OpenAiCodex),
        "anthropic" | "claude" => Ok(ProviderKind::Anthropic),
        _ => Err(TuiError::new(
            ExitCode::Provider,
            "unsupported provider; use openai-compatible, openai-codex, or anthropic",
        )),
    }
}

fn defaults(kind: ProviderKind) -> (&'static str, &'static str) {
    match kind {
        ProviderKind::OpenAiCompatible => {
            ("https://api.openai.com/v1/chat/completions", "gpt-4o-mini")
        }
        ProviderKind::OpenAiCodex => ("https://chatgpt.com/backend-api", ModelAlias::Sol.id()),
        ProviderKind::Anthropic => ("https://api.anthropic.com/v1/messages", "claude-sonnet-4-6"),
    }
}

fn api_key_request(
    kind: ProviderKind,
    mode: slim_core::OperatingMode,
    endpoint: Option<&str>,
    model: Option<&str>,
) -> Option<ProviderRequest> {
    let api_key = resolve_api_key(kind).ok().flatten()?;
    let (default_endpoint, default_model) = defaults(kind);
    Some(ProviderRequest {
        prompt: String::new(),
        mode,
        kind,
        endpoint: endpoint.unwrap_or(default_endpoint).into(),
        model: model.unwrap_or(default_model).into(),
        api_key,
        account_id: None,
        timeout: Duration::from_secs(120),
    })
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
        model: model.unwrap_or(default_model).into(),
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
    let mut events = Vec::with_capacity(execution.events.len() + 1);
    events.push(UiEvent::UserMessageAdded { text: prompt });
    events.extend(execution.events.into_iter().filter_map(UiEvent::from_core));
    Ok(events)
}

pub fn spawn_tui_runtime(
    request: ProviderRequest,
    mut options: ProviderRunOptions,
) -> Result<(TuiRuntimeHandle, UiChannels), ProviderError> {
    let oauth = OAuthService::production().map_err(|error| ProviderError::InvalidResponse {
        message: error.to_string(),
    })?;
    if options.reasoning_effort.is_none()
        && (request.kind == ProviderKind::OpenAiCodex
            || ModelAlias::parse(&request.model).is_some())
    {
        options.reasoning_effort = Some(ReasoningEffort::High.id().into());
    }
    let effort = options
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .unwrap_or(ReasoningEffort::High);
    spawn_tui_session(
        TuiStartup {
            mode: request.mode,
            effort,
            request: Some(request),
            oauth_session: None,
            options,
            initial_prompt: None,
            endpoint_override: None,
            model_override: None,
        },
        oauth,
    )
}

fn spawn_tui_session(
    startup: TuiStartup,
    oauth: OAuthService,
) -> Result<(TuiRuntimeHandle, UiChannels), ProviderError> {
    let (command_tx, command_rx) = mpsc::channel();
    // Bounded lanes per spec §10.1: control 256 (lossless, blocking send),
    // data 1024 (coalescible under backpressure).
    let (control_tx, control_rx) = mpsc::sync_channel::<UiEvent>(256);
    let (data_tx, data_rx) = mpsc::sync_channel::<UiEvent>(1024);
    let sink = EventSink {
        control: control_tx,
        data: data_tx,
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
            events: control_rx,
            events_data: data_rx,
        },
    ))
}

/// Routes events into the two bounded lanes (§10.1). Both sends block when
/// full (lossless backpressure); the consumer coalesces data deltas through
/// the runtime coalescer, so no event is dropped here.
#[derive(Clone)]
struct EventSink {
    control: mpsc::SyncSender<UiEvent>,
    data: mpsc::SyncSender<UiEvent>,
}

impl EventSink {
    fn send(&self, event: UiEvent) {
        if event.is_control() {
            let _ = self.control.send(event);
        } else {
            let _ = self.data.send(event);
        }
    }
}

struct ActiveRun {
    task: tokio::task::JoinHandle<Result<ProviderExecution, ProviderError>>,
    projector: thread::JoinHandle<()>,
    cancellation: CancellationToken,
}

struct ActiveLogin {
    provider: OAuthProvider,
    task: tokio::task::JoinHandle<Result<OAuthCredential, OAuthError>>,
    progress: tokio::task::JoinHandle<()>,
    cancel: tokio::sync::watch::Sender<bool>,
}

enum ActiveEvent {
    Command(Option<UiCommand>),
    Finished(Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>),
}

enum LoginEvent {
    Command(Option<UiCommand>),
    Finished(Result<Result<OAuthCredential, OAuthError>, tokio::task::JoinError>),
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
                message: format!("runtime: {error}"),
            });
            return;
        }
    };
    tokio_runtime.block_on(async move {
        let provider = startup
            .oauth_session
            .as_ref()
            .map(|(provider, _)| login_provider(*provider));
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
        let mut active: Option<ActiveRun> = None;
        let mut login: Option<ActiveLogin> = None;
        loop {
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
                                            message: error.to_string(),
                                        });
                                    }
                                }
                            }
                            Ok(Err(error)) => {
                                let _ = sink.send(UiEvent::LoginProgress {
                                    message: error.to_string(),
                                });
                            }
                            Err(_) => {
                                let _ = sink.send(UiEvent::LoginProgress {
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
                        result = &mut run.task => ActiveEvent::Finished(result),
                    }
                };
                match wake {
                    ActiveEvent::Command(None) => {
                        abort_active(&mut active).await;
                        break;
                    }
                    ActiveEvent::Command(Some(UiCommand::CancelRun)) => {
                        abort_active(&mut active).await;
                        let _ = sink.send(UiEvent::RunCancelled);
                    }
                    ActiveEvent::Command(Some(UiCommand::Shutdown)) => {
                        abort_active(&mut active).await;
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
                    ActiveEvent::Command(Some(_)) => {
                        let _ = sink.send(UiEvent::Notification {
                            message: "a run is already active".into(),
                        });
                    }
                    ActiveEvent::Finished(result) => {
                        if let Some(run) = active.take() {
                            let _ = run.projector.join();
                        }
                        send_execution_result(result, &sink) ;
                    }
                }
                continue;
            }

            let Some(command) = async_rx.recv().await else {
                break;
            };
            match command {
                UiCommand::StartLogin(provider) => {
                    login = Some(start_login(
                        oauth.clone(),
                        oauth_provider(provider),
                        sink.clone(),
                    ));
                }
                UiCommand::CancelLogin => {}
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
                                    message: error.to_string(),
                                });
                            }
                        }
                    } else {
                        let _ = sink.send(UiEvent::Notification {
                            message: "No OAuth session is active".into(),
                        });
                    }
                }
                UiCommand::SendPrompt(prompt) if !prompt.trim().is_empty() => {
                    if let Some((provider, credential)) = startup.oauth_session.take() {
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
                                    message: error.to_string(),
                                });
                                continue;
                            }
                        }
                    }
                    let Some(request) = startup.request.as_mut() else {
                        let _ = sink.send(UiEvent::Notification {
                            message: "No provider connected. Use /login.".into(),
                        });
                        continue;
                    };
                    request.prompt = prompt.clone();
                    let _ = sink.send(UiEvent::RunStarted);
                    let _ = sink.send(UiEvent::UserMessageAdded {
                        text: redact_for_ui(&prompt, &request.api_key),
                    });
                    let run_options = startup.options.clone();
                    startup.options.content_blocks.clear();
                    match start_active_run(request.clone(), run_options, sink.clone()) {
                        Ok(run) => active = Some(run),
                        Err(message) => {
                            let _ = sink.send(UiEvent::RunFailed { message });
                        }
                    }
                }
                UiCommand::SendPrompt(_) => {
                    let _ = sink.send(UiEvent::Notification {
                        message: "prompt cannot be empty".into(),
                    });
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
                    if startup
                        .request
                        .as_ref()
                        .is_some_and(|request| request.kind != ProviderKind::OpenAiCodex)
                    {
                        let _ = sink.send(UiEvent::Notification {
                            message: "GPT-5.6 aliases require an OpenAI Codex connection".into(),
                        });
                        continue;
                    }
                    let model = alias.id().to_owned();
                    startup.model_override = Some(model.clone());
                    startup.options.reasoning_effort = Some(effort.id().into());
                    if let Some(request) = startup.request.as_mut() {
                        request.model = model.clone();
                    }
                    let _ = sink.send(UiEvent::ModelChanged { model });
                    let _ = sink.send(UiEvent::EffortChanged { effort });
                }
                UiCommand::CancelRun => {}
                UiCommand::Shutdown => {
                    let _ = sink.send(UiEvent::Shutdown);
                    break;
                }
                UiCommand::RequestContentPage { .. } => {
                    let _ = sink.send(UiEvent::Notification {
                        message: "content paging is unavailable for this run".into(),
                    });
                }
            }
        }
    });
    let _ = forwarder.join();
}

fn start_login(
    oauth: OAuthService,
    provider: OAuthProvider,
    sink: EventSink,
) -> ActiveLogin {
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

fn oauth_provider(provider: LoginProvider) -> OAuthProvider {
    match provider {
        LoginProvider::Anthropic => OAuthProvider::Anthropic,
        LoginProvider::OpenAiCodex => OAuthProvider::OpenAiCodex,
    }
}

fn login_provider(provider: OAuthProvider) -> LoginProvider {
    match provider {
        OAuthProvider::Anthropic => LoginProvider::Anthropic,
        OAuthProvider::OpenAiCodex => LoginProvider::OpenAiCodex,
    }
}

fn start_active_run(
    request: ProviderRequest,
    mut options: ProviderRunOptions,
    sink: EventSink,
) -> Result<ActiveRun, String> {
    let (core_tx, core_rx) = mpsc::channel::<SessionEvent>();
    let projector = thread::Builder::new()
        .name("slim-tui-projector".into())
        .spawn(move || {
            for event in core_rx {
                if let Some(event) = UiEvent::from_core(event) {
                    sink.send(event);
                }
            }
        })
        .map_err(|error| format!("TUI projector thread: {error}"))?;
    let cancellation = CancellationToken::new();
    options.cancellation = Some(cancellation.clone());
    let task = tokio::spawn(execute_provider_turn_async(
        request,
        None,
        options,
        Some(core_tx),
    ));
    Ok(ActiveRun {
        task,
        projector,
        cancellation,
    })
}

async fn abort_active(active: &mut Option<ActiveRun>) {
    if let Some(mut run) = active.take() {
        run.cancellation.cancel();
        run.task.abort();
        let _ = (&mut run.task).await;
        let _ = run.projector.join();
    }
}

fn send_execution_result(
    result: Result<Result<ProviderExecution, ProviderError>, tokio::task::JoinError>,
    sink: &EventSink,
) {
    let event = match result {
        Ok(Ok(execution)) => {
            if execution.events.is_empty() && !execution.result.text.is_empty() {
                let _ = sink.send(UiEvent::Notification {
                    message: execution.result.text.clone(),
                });
            }
            if execution.result.code == ExitCode::Success {
                UiEvent::RunCompleted
            } else {
                UiEvent::RunStopped {
                    message: format!("stop={}", execution.result.stop),
                }
            }
        }
        Ok(Err(error)) => UiEvent::RunFailed {
            message: provider_error_message(error),
        },
        Err(_) => UiEvent::RunFailed {
            message: "TUI runtime task failed".into(),
        },
    };
    let _ = sink.send(event);
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
mod tests {
    use std::sync::{Arc, Mutex, OnceLock};

    use crate::oauth::{BrowserLauncher, OAuthEndpoints, OAuthError, OAuthService, OAuthStore};

    use super::prepare_tui;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct NoBrowser;

    impl BrowserLauncher for NoBrowser {
        fn open(&self, _url: &str) -> Result<(), OAuthError> {
            Ok(())
        }
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
}
