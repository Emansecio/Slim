use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::oauth::{BrowserLauncher, OAuthEndpoints, OAuthError, OAuthService, OAuthStore};
use slim_tui::api::{PromptAdmission, UiCommand, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action, Effect};

use super::{prepare_tui, spawn_tui_session, TuiStartup};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
fn command_code_catalog_refresh_preserves_effort_capabilities() {
    for source in [
        crate::command_code_catalog::CatalogSource::Live,
        crate::command_code_catalog::CatalogSource::Cache,
        crate::command_code_catalog::CatalogSource::Fallback,
    ] {
        let models = slim_core::provider::parse_command_code_catalog(
            br#"{"object":"list","data":[{"id":"deepseek/deepseek-v4.1-flash"},{"id":"stealth/unknown"}]}"#,
        )
        .unwrap();
        let super::UiEvent::CommandCodeCatalogLoaded { models, .. } =
            super::command_code_catalog_event(crate::command_code_catalog::CatalogSnapshot {
                models,
                source,
            })
        else {
            panic!("catalog event")
        };
        assert_eq!(
            models[0].reasoning_levels,
            vec![
                super::ReasoningEffort::Low,
                super::ReasoningEffort::High,
                super::ReasoningEffort::Max,
            ]
        );
        assert!(models[1].reasoning_levels.is_empty());
    }
}

#[test]
fn worker_join_reports_panic_and_normal_shutdown_is_success() {
    let failed = super::TuiRuntimeHandle {
        shutdown: None,
        worker: Some(std::thread::spawn(|| panic!("fixture worker failure"))),
    }
    .finish()
    .unwrap_err();
    assert_eq!(failed.code(), crate::ExitCode::Internal);
    assert!(failed.to_string().contains("effects are unverified"));
    assert!(!failed.to_string().contains("fixture worker failure"));
    let (tx, rx) = std::sync::mpsc::channel();
    super::TuiRuntimeHandle {
        shutdown: Some(tx),
        worker: Some(std::thread::spawn(move || {
            assert_eq!(rx.recv().unwrap(), super::UiCommand::Shutdown);
            Vec::new()
        })),
    }
    .finish()
    .unwrap();
}

#[test]
fn esc_cancels_blocked_oauth_preparation_without_starting_a_provider_run() {
    let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let variables = [
        "SLIM_AUTH_FILE",
        "SLIM_CONFIG_FILE",
        "SLIM_PROVIDER",
        "SLIM_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
    ];
    let _restore_environment = RestoreEnvironment::capture(variables);
    for name in variables {
        std::env::remove_var(name);
    }

    let root = std::env::temp_dir().join(format!(
        "slim-tui-oauth-cancel-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("fixture directory");
    let _remove_fixture = RemoveDirectoryOnDrop(root.clone());
    let workspace_root = root.join("workspace");
    std::fs::create_dir_all(&workspace_root).expect("fixture workspace");
    let auth_path = root.join("auth.json");
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "").expect("isolated config");
    std::env::set_var("SLIM_AUTH_FILE", &auth_path);
    std::env::set_var("SLIM_CONFIG_FILE", &config_path);

    let mut server = BlockedOAuthRefreshServer::new();
    let token_url = format!("{}/oauth/token", server.endpoint);
    let provider_url = format!("{}/v1/messages", server.endpoint);
    let store = OAuthStore::at(&auth_path);
    let observed_store = store.clone();
    store
        .save(
            crate::oauth::OAuthProvider::Anthropic,
            &crate::oauth::OAuthCredential {
                access: "expired-access-fixture".into(),
                refresh: "expired-refresh-fixture".into(),
                expires: 1,
                account_id: None,
            },
        )
        .expect("expired OAuth fixture");
    let oauth = OAuthService::new(
        OAuthEndpoints {
            anthropic_token: token_url,
            ..OAuthEndpoints::default()
        },
        Arc::new(NoBrowser),
        store,
    )
    .expect("OAuth service");
    let mut startup = prepare_tui(
        vec![
            "--tui".into(),
            "--provider".into(),
            "anthropic".into(),
            "--endpoint".into(),
            provider_url,
        ],
        &oauth,
    )
    .expect("TUI startup");
    startup.options.workspace_root = Some(workspace_root);
    let (runtime, channels) = spawn_tui_session(startup, oauth).expect("TUI runtime");
    let _release_before_runtime_shutdown =
        ReleaseOAuthResponseOnDrop(Arc::clone(&server.release_response));

    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("cancel while refreshing");
    let prepare = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    )
    .into_iter()
    .find_map(|effect| match effect {
        Effect::Send(UiCommand::PreparePrompt { prompt, admission }) => Some((prompt, admission)),
        _ => None,
    })
    .expect("Enter synchronously admits the prompt");
    let (prompt, admission): (String, PromptAdmission) = prepare;
    assert_eq!(prompt, "cancel while refreshing");
    assert_eq!(state.prompt_preparation().unwrap().admission, admission);
    let command_sent = channels
        .commands
        .send(UiCommand::PreparePrompt { prompt, admission })
        .is_ok();

    let observed_request = server.requests.recv_timeout(Duration::from_secs(3)).ok();
    let cancel_started = Instant::now();
    let cancel_command = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    )
    .into_iter()
    .find_map(|effect| match effect {
        Effect::Send(
            command @ UiCommand::CancelPromptPreparation {
                admission: requested,
            },
        ) if requested == admission => Some(command),
        _ => None,
    });
    let cancel_sent = cancel_command.is_some_and(|command| channels.commands.send(command).is_ok());

    let mut cancellation_seen = false;
    let mut cancel_latency = None;
    let mut run_started = false;
    let cancel_deadline = cancel_started + Duration::from_secs(1);
    while Instant::now() < cancel_deadline && !cancellation_seen {
        while let Ok(event) = channels.events.try_recv() {
            match event {
                UiEvent::PromptPreparationCancelled {
                    admission: event_admission,
                } if event_admission == admission => {
                    cancellation_seen = true;
                    cancel_latency = Some(cancel_started.elapsed());
                    reduce(
                        &mut state,
                        Action::UiEventReceived(UiEvent::PromptPreparationCancelled {
                            admission: event_admission,
                        }),
                    );
                }
                UiEvent::PromptRunStarted {
                    admission: event_admission,
                    ..
                } if event_admission == admission => run_started = true,
                UiEvent::RunStarted { .. } => run_started = true,
                _ => {}
            }
        }
        while let Ok(event) = channels.events_data.try_recv() {
            match event {
                UiEvent::PromptPreparationCancelled {
                    admission: event_admission,
                } if event_admission == admission => {
                    cancellation_seen = true;
                    cancel_latency = Some(cancel_started.elapsed());
                    reduce(
                        &mut state,
                        Action::UiEventReceived(UiEvent::PromptPreparationCancelled {
                            admission: event_admission,
                        }),
                    );
                }
                UiEvent::PromptRunStarted {
                    admission: event_admission,
                    ..
                } if event_admission == admission => run_started = true,
                UiEvent::RunStarted { .. } => run_started = true,
                _ => {}
            }
        }
        if !cancellation_seen {
            thread::sleep(Duration::from_millis(5));
        }
    }
    let preparation_cleared = !state.prompt_is_busy();
    let prompt_restored = state.composer.payload() == "cancel while refreshing";

    let paths_before_resubmission = server
        .paths
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    server.release();

    let persistence_deadline = Instant::now() + Duration::from_secs(3);
    let mut persisted_successor = None;
    while Instant::now() < persistence_deadline {
        if let Ok(Some(credential)) =
            observed_store.credential(crate::oauth::OAuthProvider::Anthropic)
        {
            if credential.access == "rotated-access" && credential.refresh == "rotated-refresh" {
                persisted_successor = Some(credential);
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }

    let resubmit = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    )
    .into_iter()
    .find_map(|effect| match effect {
        Effect::Send(UiCommand::PreparePrompt { prompt, admission }) => Some((prompt, admission)),
        _ => None,
    })
    .expect("restored direct prompt can be submitted again");
    let (resubmitted_prompt, resubmitted_admission) = resubmit;
    assert_eq!(resubmitted_prompt, "cancel while refreshing");
    assert_eq!(
        state.prompt_preparation().unwrap().admission,
        resubmitted_admission
    );
    let resubmission_sent = channels
        .commands
        .send(UiCommand::PreparePrompt {
            prompt: resubmitted_prompt,
            admission: resubmitted_admission,
        })
        .is_ok();

    let mut successor_run_started = false;
    let mut successor_run_completed = false;
    let mut observed_run_admissions = Vec::new();
    let mut observed_other_events = Vec::new();
    let run_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < run_deadline && !successor_run_completed {
        for event in channels
            .events
            .try_iter()
            .chain(channels.events_data.try_iter())
        {
            match event {
                UiEvent::PromptRunStarted {
                    admission: started_admission,
                    run_id,
                    ..
                } => {
                    assert_ne!(
                        started_admission, admission,
                        "cancelled prompt reached RunStarted after refresh completion (run {run_id})"
                    );
                    successor_run_started |= started_admission == resubmitted_admission;
                    observed_run_admissions.push(started_admission);
                }
                UiEvent::RunStarted { run_id, .. } => {
                    panic!("uncorrelated run {run_id} started after cancelling the direct prompt")
                }
                UiEvent::PromptRunCompleted { admission, .. }
                    if admission == resubmitted_admission =>
                {
                    successor_run_completed = true;
                }
                other => observed_other_events.push(format!("{other:?}")),
            }
        }
        if !successor_run_completed {
            thread::sleep(Duration::from_millis(5));
        }
    }
    let next_provider_request = server.requests.recv_timeout(Duration::from_secs(3)).ok();

    let finish_result = runtime.finish();
    for event in channels
        .events
        .try_iter()
        .chain(channels.events_data.try_iter())
    {
        match event {
            UiEvent::PromptRunStarted {
                admission: event_admission,
                ..
            } if event_admission == admission => run_started = true,
            UiEvent::RunStarted { .. } => run_started = true,
            _ => {}
        }
    }
    let request_paths = server
        .paths
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let observed_requests = server
        .observed_requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    server.stop();

    assert!(command_sent, "prompt command reached the TUI bridge");
    assert!(observed_request.as_ref().is_some_and(|request| {
        request.path == "/oauth/token"
            && request.x_api_key.is_none()
            && String::from_utf8_lossy(&request.body).contains("expired-refresh-fixture")
    }));
    assert!(cancel_sent, "Esc emitted the matching preparation cancel");
    assert!(cancellation_seen, "worker acknowledged cancellation");
    assert!(cancel_latency.is_some_and(|elapsed| elapsed <= Duration::from_secs(1)));
    assert!(
        preparation_cleared,
        "cancel terminal returned state to idle"
    );
    assert!(prompt_restored, "cancelled direct prompt was restored");
    assert!(!run_started, "cancelled prompt never reached RunStarted");
    assert_eq!(
        paths_before_resubmission,
        vec!["/oauth/token".to_owned()],
        "cancellation before resubmission sent no inference request"
    );
    assert!(
        resubmission_sent,
        "restored prompt reached the TUI bridge again"
    );
    assert!(
        successor_run_started,
        "the resubmitted prompt reached the correlated run lifecycle; observed start admissions: {observed_run_admissions:?}; other events: {observed_other_events:?}; provider request: {next_provider_request:?}"
    );
    assert!(
        successor_run_completed,
        "the resubmitted prompt completed with the persisted successor"
    );
    assert!(
        matches!(
            next_provider_request,
            Some(HttpFixtureRequest { ref path, ref authorization, .. })
                if path == "/v1/messages"
                    && authorization.as_deref() == Some("Bearer rotated-access")
        ),
        "the next prompt used the durably persisted OAuth successor: {next_provider_request:?}; paths: {request_paths:?}; persisted before resubmission: {}; store credential: {:?}; run started/completed: {successor_run_started}/{successor_run_completed}; start admissions: {observed_run_admissions:?}; other events: {observed_other_events:?}; finish: {finish_result:?}; auth file: {}",
        persisted_successor.is_some(),
        observed_store.credential(crate::oauth::OAuthProvider::Anthropic),
        std::fs::read_to_string(&auth_path).unwrap_or_else(|error| error.to_string())
    );
    assert_eq!(
        request_paths,
        vec!["/oauth/token".to_owned(), "/v1/messages".to_owned()],
        "only the refresh and the resubmitted inference request were sent"
    );
    assert!(
        finish_result.is_ok(),
        "worker shutdown completed: {finish_result:?}"
    );
    assert!(observed_requests.iter().any(|request| {
        request.path == "/v1/messages"
            && request.authorization.as_deref() == Some("Bearer rotated-access")
    }));
    assert!(
        persisted_successor.is_some(),
        "the dispatched refresh successor was durably persisted before resubmission; observed credential: {:?}; provider request: {next_provider_request:?}; run started: {successor_run_started}; run completed: {successor_run_completed}; finish: {finish_result:?}; auth file: {}",
        observed_store.credential(crate::oauth::OAuthProvider::Anthropic),
        std::fs::read_to_string(&auth_path).unwrap_or_else(|error| error.to_string())
    );
}

#[test]
fn deepseek_flash_catalog_displays_v4_1_with_canonical_selection_id() {
    let super::UiEvent::OpenCodeCatalogLoaded { models, source } =
        super::open_code_catalog_event(super::CatalogSnapshot {
            model_ids: vec!["deepseek-flash".into(), "deepseek-v4-flash".into()],
            source: super::CatalogSource::Live,
        })
    else {
        panic!("catalog event")
    };
    assert_eq!(source, super::OpenCodeCatalogSource::Live);
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "deepseek-flash");
    assert_eq!(models[0].name, "DeepSeek V4.1 Flash");
    assert_eq!(models[1].id, "deepseek-v4-flash");
    assert_eq!(models[1].name, "DeepSeek V4 Flash");
}

#[test]
fn muse_catalog_offers_efforts_through_xhigh() {
    let super::UiEvent::OpenCodeCatalogLoaded { models, .. } =
        super::open_code_catalog_event(super::CatalogSnapshot {
            model_ids: vec![
                "muse-spark-1.2-contributor".into(),
                "muse-spark-1.3-contributor".into(),
            ],
            source: super::CatalogSource::Live,
        })
    else {
        panic!("catalog event")
    };
    assert_eq!(models.len(), 2);
    for model in models {
        assert_eq!(
            model
                .reasoning_levels
                .iter()
                .map(|effort| effort.id())
                .collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh"]
        );
    }
}

fn wait_for_auth_provider(channels: &slim_tui::api::UiChannels, expected: super::LoginProvider) {
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

struct RestoreEnvironment<const N: usize> {
    names: [&'static str; N],
    values: [Option<std::ffi::OsString>; N],
}

impl<const N: usize> RestoreEnvironment<N> {
    fn capture(names: [&'static str; N]) -> Self {
        Self {
            names,
            values: names.map(std::env::var_os),
        }
    }
}

impl<const N: usize> Drop for RestoreEnvironment<N> {
    fn drop(&mut self) {
        for (name, value) in self.names.into_iter().zip(self.values.iter_mut()) {
            match value.take() {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

struct ReleaseOAuthResponseOnDrop(Arc<(Mutex<bool>, Condvar)>);

struct RemoveDirectoryOnDrop(std::path::PathBuf);

impl Drop for RemoveDirectoryOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for ReleaseOAuthResponseOnDrop {
    fn drop(&mut self) {
        let (released, changed) = &*self.0;
        *released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        changed.notify_all();
    }
}

struct BlockedOAuthRefreshServer {
    endpoint: String,
    paths: Arc<Mutex<Vec<String>>>,
    observed_requests: Arc<Mutex<Vec<HttpFixtureRequest>>>,
    requests: mpsc::Receiver<HttpFixtureRequest>,
    release_response: Arc<(Mutex<bool>, Condvar)>,
    stopping: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Debug)]
struct HttpFixtureRequest {
    path: String,
    authorization: Option<String>,
    x_api_key: Option<String>,
    body: Vec<u8>,
}

impl BlockedOAuthRefreshServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("OAuth fixture bind");
        listener.set_nonblocking(true).expect("OAuth nonblocking");
        let address = listener.local_addr().expect("OAuth fixture address");
        let (request_tx, requests) = mpsc::channel();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let observed_paths = Arc::clone(&paths);
        let observed_requests = Arc::new(Mutex::new(Vec::new()));
        let request_records = Arc::clone(&observed_requests);
        let release_response = Arc::new((Mutex::new(false), Condvar::new()));
        let response_gate = Arc::clone(&release_response);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stopping);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut responders = Vec::new();
            while !stop_worker.load(Ordering::Acquire) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let Some(request) = read_http_request(&mut stream) else {
                            continue;
                        };
                        let path = request.path.clone();
                        observed_paths
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(path.clone());
                        request_records
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(request.clone());
                        let _ = request_tx.send(request);
                        let gate = Arc::clone(&response_gate);
                        responders.push(thread::spawn(move || {
                            if path == "/oauth/token" {
                                let (released, changed) = &*gate;
                                let mut released = released
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                while !*released {
                                    released = changed
                                        .wait(released)
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                }
                                write_http_response(
                                    &mut stream,
                                    "200 OK",
                                    r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
                                );
                            } else if path == "/v1/messages" {
                                write_http_response_with_content_type(
                                    &mut stream,
                                    "200 OK",
                                    "text/event-stream",
                                    concat!(
                                        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                                        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                                        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"successor used\"}}\n\n",
                                        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                                        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n"
                                    ),
                                );
                            } else {
                                write_http_response(
                                    &mut stream,
                                    "500 Internal Server Error",
                                    r#"{"error":"unexpected provider request"}"#,
                                );
                            }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            for responder in responders {
                let _ = responder.join();
            }
        });
        Self {
            endpoint: format!("http://{address}"),
            paths,
            observed_requests,
            requests,
            release_response,
            stopping,
            worker: Some(worker),
        }
    }

    fn release(&self) {
        let (released, changed) = &*self.release_response;
        *released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        changed.notify_all();
    }

    fn stop(&mut self) {
        self.release();
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("OAuth fixture worker");
        }
    }
}

impl Drop for BlockedOAuthRefreshServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpFixtureRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    while headers.len() < 64 * 1024 {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                headers.push(byte[0]);
                if headers.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let headers = String::from_utf8(headers).ok()?;
    let content_length = headers.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).ok()?;
    let mut lines = headers.split("\r\n");
    let path = lines.next()?.split_whitespace().nth(1)?.to_owned();
    let x_api_key = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("x-api-key")
            .then(|| value.trim().to_owned())
    });
    let authorization = headers.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    });
    Some(HttpFixtureRequest {
        path,
        authorization,
        x_api_key,
        body,
    })
}

fn write_http_response(stream: &mut TcpStream, status: &str, body: &str) {
    write_http_response_with_content_type(stream, status, "application/json", body);
}

fn write_http_response_with_content_type(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
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
        vec![
            "--tui".into(),
            "--provider".into(),
            "codex".into(),
            "--experiment-id".into(),
            "exp-arm-b".into(),
            "--task-id".into(),
            "repo-17".into(),
        ],
        &oauth,
    )
    .expect("signed-out startup");
    assert!(startup.request.is_none());
    assert_eq!(startup.options.experiment_id.as_deref(), Some("exp-arm-b"));
    assert_eq!(startup.options.task_id.as_deref(), Some("repo-17"));

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
    let oauth =
        OAuthService::new(OAuthEndpoints::default(), Arc::new(NoBrowser), store).expect("service");
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
fn codex_api_key_and_environment_credentials_are_used_during_tui_startup() {
    let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let variables = [
        "SLIM_AUTH_FILE",
        "SLIM_PROVIDER",
        "SLIM_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "OPENAI_API_KEY",
    ];
    let previous = variables.map(std::env::var_os);
    for name in variables {
        std::env::remove_var(name);
    }

    let root =
        std::env::temp_dir().join(format!("slim-codex-api-key-startup-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("auth directory");
    let auth = root.join("auth.json");
    let oauth_token = codex_token("oauth-account-fixture");
    let saved_key = codex_token("saved-key-account-fixture");
    let provider = slim_core::provider::ProviderKind::OpenAiCodex;
    let store = OAuthStore::at(&auth);
    store
        .save(
            crate::oauth::OAuthProvider::OpenAiCodex,
            &crate::oauth::OAuthCredential {
                access: oauth_token,
                refresh: "synthetic-refresh-fixture".into(),
                expires: u64::MAX,
                account_id: Some("oauth-account-fixture".into()),
            },
        )
        .expect("save OAuth credential");
    crate::save_api_key_file(&auth, provider, &saved_key).expect("select saved API key");
    std::env::set_var("SLIM_AUTH_FILE", &auth);

    let observed = {
        let oauth = OAuthService::new(
            OAuthEndpoints::default(),
            Arc::new(NoBrowser),
            OAuthStore::at(&auth),
        )
        .expect("service");
        let from_file = prepare_tui(vec!["--tui".into()], &oauth).expect("saved API key startup");
        let from_file = from_file.request.expect("Codex API-key request");
        assert_eq!(from_file.api_key, saved_key);
        assert_eq!(
            from_file.account_id.as_deref(),
            Some("saved-key-account-fixture")
        );
        assert_eq!(
            from_file.kind,
            slim_core::provider::ProviderKind::OpenAiCodex
        );

        let provider_token = codex_token("provider-env-account-fixture");
        std::env::set_var("CODEX_ACCESS_TOKEN", &provider_token);
        let from_provider_env = prepare_tui(vec!["--tui".into()], &oauth)
            .expect("Codex provider environment startup")
            .request
            .expect("Codex provider environment request");

        std::env::remove_var("CODEX_ACCESS_TOKEN");
        let slim_token = codex_token("slim-env-account-fixture");
        std::env::set_var("SLIM_API_KEY", &slim_token);
        let from_slim_env = prepare_tui(
            vec!["--tui".into(), "--provider".into(), "codex".into()],
            &oauth,
        )
        .expect("Codex SLIM_API_KEY startup")
        .request
        .expect("Codex SLIM_API_KEY request");
        (
            from_provider_env.api_key,
            from_provider_env.account_id,
            from_slim_env.api_key,
            from_slim_env.account_id,
        )
    };

    for (name, value) in variables.into_iter().zip(previous) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    let _ = std::fs::remove_dir_all(root);

    assert_eq!(observed.0, codex_token("provider-env-account-fixture"));
    assert_eq!(observed.1.as_deref(), Some("provider-env-account-fixture"));
    assert_eq!(observed.2, codex_token("slim-env-account-fixture"));
    assert_eq!(observed.3.as_deref(), Some("slim-env-account-fixture"));
}

#[test]
fn background_oauth_failure_reaches_tui_notification_once() {
    let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let variables = ["SLIM_AUTH_FILE", "SLIM_PROVIDER", "SLIM_API_KEY"];
    let previous = variables.map(std::env::var_os);
    for name in variables {
        std::env::remove_var(name);
    }

    let root = std::env::temp_dir().join(format!("slim-tui-oauth-warning-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("auth directory");
    let auth = root.join("auth.json");
    let store = OAuthStore::at(&auth);
    let expired = crate::oauth::OAuthCredential {
        access: "synthetic-access-fixture".into(),
        refresh: "synthetic-refresh-fixture".into(),
        expires: 1,
        account_id: None,
    };
    store
        .save(crate::oauth::OAuthProvider::Anthropic, &expired)
        .expect("save expired OAuth credential");
    std::env::set_var("SLIM_AUTH_FILE", &auth);

    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("loopback address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let mut request = [0; 4096];
                    let _ = stream.read(&mut request);
                    let body = r#"{"error":"fixture-rejected"}"#;
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        }
    });
    let oauth = OAuthService::new(
        OAuthEndpoints {
            anthropic_token: format!("http://{address}"),
            ..OAuthEndpoints::default()
        },
        Arc::new(NoBrowser),
        store,
    )
    .expect("service");
    let startup = prepare_tui(vec!["--tui".into()], &oauth).expect("TUI startup");
    let (runtime, channels) = spawn_tui_session(startup, oauth.clone()).expect("TUI runtime");
    oauth.start_background_refresh_for_test(crate::oauth::OAuthProvider::Anthropic, expired);

    let mut notifications = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && notifications.is_empty() {
        while let Ok(event) = channels.events.try_recv() {
            if let super::UiEvent::Notification { message } = event {
                notifications.push(message);
            }
        }
        while let Ok(event) = channels.events_data.try_recv() {
            if let super::UiEvent::Notification { message } = event {
                notifications.push(message);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    let server_received = server.join().expect("loopback server");
    thread::sleep(Duration::from_millis(250));
    while let Ok(event) = channels.events.try_recv() {
        if let super::UiEvent::Notification { message } = event {
            notifications.push(message);
        }
    }
    while let Ok(event) = channels.events_data.try_recv() {
        if let super::UiEvent::Notification { message } = event {
            notifications.push(message);
        }
    }
    runtime.finish().expect("TUI shutdown");

    for (name, value) in variables.into_iter().zip(previous) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    let _ = std::fs::remove_dir_all(root);

    assert!(
        server_received,
        "background refresh reached loopback endpoint"
    );
    assert_eq!(
        notifications.len(),
        1,
        "warning should be drained exactly once"
    );
    assert!(notifications[0].contains("OAuth"));
    assert!(!notifications[0].contains("synthetic-access-fixture"));
    assert!(!notifications[0].contains("synthetic-refresh-fixture"));
}

fn codex_token(account_id: &str) -> String {
    use base64::Engine as _;
    let claims = serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
    });
    format!(
        "header.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
    )
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
        let root =
            std::env::temp_dir().join(format!("slim-restore-oauth-{index}-{}", std::process::id()));
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
        "SLIM_EFFORT",
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
    std::env::set_var("SLIM_EFFORT", "low");
    let default_startup =
        prepare_tui(vec!["--tui".into()], &oauth).expect("default environment credential");
    let sent_effort = default_startup.options.reasoning_effort;
    let default = default_startup
        .request
        .map(|request| (request.kind, request.api_key));
    std::env::set_var("SLIM_EFFORT", "invalid-effort");
    let invalid_effort = prepare_tui(vec!["--tui".into()], &oauth).err();
    std::env::remove_var("SLIM_EFFORT");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::set_var("OPENCODE_API_KEY", "opencode-environment");
    let explicit = prepare_tui(
        vec!["--tui".into(), "--provider".into(), "opencode-go".into()],
        &oauth,
    )
    .expect("explicit environment credential")
    .request
    .map(|request| (request.kind, request.api_key));

    let invalid_model = prepare_tui(
        vec![
            "--provider".into(),
            "opencode-go".into(),
            "--model".into(),
            "unknown-model".into(),
        ],
        &oauth,
    )
    .err();
    let _ = std::fs::remove_dir_all(root);
    for (name, value) in variables.into_iter().zip(previous) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    assert_eq!(sent_effort.as_deref(), Some("low"));
    assert!(invalid_effort.is_some());
    assert!(invalid_model.is_some());
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
    let oauth =
        OAuthService::new(OAuthEndpoints::default(), Arc::new(NoBrowser), store).expect("service");
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
            fast: false,
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
    let oauth =
        OAuthService::new(OAuthEndpoints::default(), Arc::new(NoBrowser), store).expect("service");
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
            fast: false,
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
