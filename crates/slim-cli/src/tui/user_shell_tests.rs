use std::sync::Arc;
use std::time::{Duration, Instant};

use slim_tui::api::{UiChannels, UiCommand, UiEvent};

use super::{spawn_tui_session, TuiStartup};
use crate::oauth::{BrowserLauncher, OAuthEndpoints, OAuthError, OAuthService, OAuthStore};

struct NoBrowser;

impl BrowserLauncher for NoBrowser {
    fn open(&self, _url: &str) -> Result<(), OAuthError> {
        Ok(())
    }
}

struct Harness {
    runtime: Option<super::TuiRuntimeHandle>,
    channels: UiChannels,
    root: std::path::PathBuf,
    seen: Vec<UiEvent>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.runtime.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn harness(mode: slim_core::OperatingMode, label: &str) -> Harness {
    let root = std::env::temp_dir().join(format!(
        "slim-user-shell-{label}-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    std::fs::create_dir_all(&root).expect("workspace");
    let oauth = OAuthService::new(
        OAuthEndpoints::default(),
        Arc::new(NoBrowser),
        OAuthStore::at(root.join("auth.json")),
    )
    .expect("oauth");
    let options = crate::ProviderRunOptions {
        workspace_root: Some(root.clone()),
        ..crate::ProviderRunOptions::default()
    };
    let startup = TuiStartup {
        request: Some(crate::ProviderRequest {
            prompt: String::new(),
            mode,
            kind: slim_core::provider::ProviderKind::OpenCodeGo,
            endpoint: slim_core::provider::OPENCODE_GO_BASE_URL.into(),
            model: "deepseek-v4-flash".into(),
            api_key: "fixture-key".into(),
            account_id: None,
            timeout: Duration::from_secs(120),
        }),
        oauth_session: None,
        options,
        initial_prompt: None,
        image_labels: Vec::new(),
        resume_path: None,
        resume_preflight: None,
        pending_session_title: None,
        persist_sessions: false,
        mode,
        effort: super::ReasoningEffort::High,
        endpoint_override: None,
        model_override: None,
        timeout: Duration::from_secs(120),
    };
    let (runtime, channels) = spawn_tui_session(startup, oauth).expect("runtime");
    Harness {
        runtime: Some(runtime),
        channels,
        root,
        seen: Vec::new(),
    }
}

impl Harness {
    fn send(&self, command: UiCommand) {
        self.channels.commands.send(command).expect("send command");
    }

    /// Collects events until `done` matches one; everything seen is kept.
    fn until(&mut self, what: &str, done: impl Fn(&UiEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let mut received = false;
            let batch: Vec<UiEvent> = self
                .channels
                .events
                .try_iter()
                .chain(self.channels.events_data.try_iter())
                .collect();
            for event in batch {
                received = true;
                let finished = done(&event);
                self.seen.push(event);
                if finished {
                    return;
                }
            }
            if !received {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        panic!("timed out waiting for {what}; saw {:?}", self.seen);
    }
}

fn is_finished(event: &UiEvent, id: u64) -> bool {
    matches!(event, UiEvent::UserShellFinished { request_id } if *request_id == id)
}

#[test]
fn auto_mode_runs_the_command_and_shows_it_as_a_marked_shell_block() {
    let mut harness = harness(slim_core::OperatingMode::Auto, "auto");
    harness.send(UiCommand::RunUserShell {
        request_id: 5,
        command: "echo slim-user-shell-marker".into(),
    });
    harness.until("the command to finish", |event| is_finished(event, 5));

    let started = harness
        .seen
        .iter()
        .find_map(|event| match event {
            UiEvent::ToolStarted {
                name,
                arguments_summary,
                ..
            } => Some((name.clone(), arguments_summary.clone())),
            _ => None,
        })
        .expect("ToolStarted");
    assert_eq!(started.0, "shell");
    assert!(
        started.1.starts_with("! echo slim-user-shell-marker"),
        "{started:?}"
    );
    assert!(
        harness.seen.iter().any(|event| matches!(event,
            UiEvent::ToolOutput { output, .. } if output.contains("slim-user-shell-marker"))),
        "output must reach the transcript: {:?}",
        harness.seen
    );
    assert!(harness
        .seen
        .iter()
        .any(|event| matches!(event, UiEvent::ToolEnded { success: true, .. })));
    assert!(is_finished(harness.seen.last().expect("events"), 5));
}

#[test]
fn read_only_and_plan_refuse_without_spawning_anything() {
    for mode in [
        slim_core::OperatingMode::ReadOnly,
        slim_core::OperatingMode::Plan,
    ] {
        let mut harness = harness(mode, "refuse");
        harness.send(UiCommand::RunUserShell {
            request_id: 9,
            command: "echo nope".into(),
        });
        harness.until("the refusal", |event| is_finished(event, 9));
        assert!(
            harness.seen.iter().any(|event| matches!(event,
                UiEvent::Notification { message } if message.contains("modo Auto"))),
            "{mode:?}: {:?}",
            harness.seen
        );
        assert!(
            !harness
                .seen
                .iter()
                .any(|event| matches!(event, UiEvent::ToolStarted { .. })),
            "{mode:?}: nothing may run"
        );
    }
}

#[test]
fn cancel_stops_a_long_command_and_still_finishes_the_request() {
    let mut harness = harness(slim_core::OperatingMode::Auto, "cancel");
    let long = if cfg!(windows) {
        "ping -n 60 127.0.0.1"
    } else {
        "sleep 60"
    };
    harness.send(UiCommand::RunUserShell {
        request_id: 3,
        command: long.into(),
    });
    harness.until("the command to start", |event| {
        matches!(event, UiEvent::ToolStarted { .. })
    });
    let cancelled_at = Instant::now();
    harness.send(UiCommand::CancelRun);
    harness.until("the cancelled command to finish", |event| {
        is_finished(event, 3)
    });
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(20),
        "cancellation took {:?}",
        cancelled_at.elapsed()
    );
    assert!(harness
        .seen
        .iter()
        .any(|event| matches!(event, UiEvent::ToolEnded { success: false, .. })));
}

#[test]
fn idle_worker_answers_workspace_file_requests() {
    let mut harness = harness(slim_core::OperatingMode::Auto, "files");
    std::fs::write(harness.root.join("notes.md"), "n").unwrap();
    harness.send(UiCommand::RequestWorkspaceFiles { request_id: 12 });
    harness.until("the file list", |event| {
        matches!(event, UiEvent::WorkspaceFiles { request_id: 12, .. })
    });
    let listed = harness
        .seen
        .iter()
        .find_map(|event| match event {
            UiEvent::WorkspaceFiles { paths, .. } => Some(paths.clone()),
            _ => None,
        })
        .expect("list");
    assert!(listed.contains(&"notes.md".to_owned()), "{listed:?}");
}

#[test]
fn context_keeps_the_latest_notes_and_consumes_only_what_a_run_carried() {
    let context = super::UserShellContext::default();
    for index in 0..7 {
        context.push(super::UserShellNote {
            seq: 0,
            command: format!("cmd{index}"),
            text: format!("out{index}"),
        });
    }
    let held = context.snapshot();
    assert_eq!(held.len(), 5, "bounded");
    assert_eq!(held[0].command, "cmd2");
    // A note that lands while a run starts is not lost.
    context.push(super::UserShellNote {
        seq: 0,
        command: "late".into(),
        text: "late".into(),
    });
    context.consume(&held);
    let left = context.snapshot();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].command, "late");
    assert_eq!(held[0].label(), "[shell · cmd2]");
}

#[test]
fn output_cap_cuts_on_a_char_boundary_and_says_so() {
    let text = "ação".repeat(10);
    let cut = super::cap_text_bytes(&text, 7);
    assert!(cut.starts_with("açã"), "{cut}");
    assert!(cut.contains("saída cortada"));
    assert_eq!(super::cap_text_bytes("curto", 1024), "curto");
}

#[test]
fn job_output_read_never_emits_a_metadata_echo() {
    let root = std::env::temp_dir();
    let jobs = slim_core::runtime::ShellJobs::default();
    let context = super::JobCommandContext {
        jobs,
        mode: slim_core::OperatingMode::Auto,
        cwd: root.clone(),
        tools: Default::default(),
        store: slim_core::context::ArtifactStore::new(root).unwrap(),
        secrets: Vec::new(),
    };
    let (control_tx, control_rx) = std::sync::mpsc::sync_channel(8);
    let (data_tx, data_rx) = std::sync::mpsc::sync_channel(8);
    let sink = super::EventSink {
        control: Some(control_tx),
        data: Some(data_tx),
        wake: slim_tui::api::WakeSignal::new().unwrap(),
        lane_space: slim_tui::api::WakeSignal::new().unwrap(),
        drop_probe: None,
    };
    assert!(context
        .handle(
            UiCommand::JobOutput {
                id: "missing".into(),
                offset: None,
                before: None
            },
            &sink
        )
        .is_none());
    let events: Vec<_> = control_rx.try_iter().chain(data_rx.try_iter()).collect();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, UiEvent::JobsChanged { .. })),
        "read must not echo metadata: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_jobs_persist_start_and_idle_end_without_a_model_turn() {
    let directory = std::env::temp_dir().join(format!(
        "slim-job-journal-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("session.jsonl");
    let repo = slim_core::session::JsonlRepo::create(
        &path,
        slim_core::session::DurableSessionHeader::new(
            "jobs",
            "now",
            directory.to_str().unwrap(),
            None,
            None,
        ),
    )
    .unwrap();
    drop(repo);
    let jobs = slim_core::runtime::ShellJobs::default();
    let store = slim_core::context::ArtifactStore::new(directory.join("artifacts")).unwrap();
    let id = jobs
        .start_user(
            Default::default(),
            &directory,
            if cfg!(windows) {
                "Write-Output 'idle-done'"
            } else {
                "printf idle-done"
            },
            &[],
            store,
        )
        .unwrap();
    let mut startup = super::TuiStartup {
        request: None,
        oauth_session: None,
        options: Default::default(),
        initial_prompt: None,
        image_labels: Vec::new(),
        resume_path: None,
        resume_preflight: None,
        pending_session_title: None,
        persist_sessions: true,
        mode: slim_core::OperatingMode::Auto,
        effort: super::ReasoningEffort::High,
        endpoint_override: None,
        model_override: None,
        timeout: Duration::from_secs(10),
    };
    startup.options.shell_jobs = Some(jobs.clone());
    startup.resume_path = Some(path.clone());
    let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = super::EventSink {
        control: None,
        data: None,
        wake: slim_tui::api::WakeSignal::new().unwrap(),
        lane_space: slim_tui::api::WakeSignal::new().unwrap(),
        drop_probe: None,
    };
    let command = tokio::time::timeout(
        Duration::from_secs(1),
        super::receive_tui_command(
            &mut rx,
            &mut startup,
            &sink,
            true,
            tokio::time::Instant::now(),
        ),
    )
    .await
    .unwrap();
    assert!(matches!(command, Some(UiCommand::PersistJobs)));
    assert!(
        super::session_job_metadata(&slim_core::session::preflight_session(&path).unwrap())
            .is_empty(),
        "the cancellable receiver must not open the journal"
    );
    super::persist_job_metadata(&mut startup).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while jobs.running() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    super::persist_job_metadata(&mut startup).await.unwrap();
    let preflight = slim_core::session::preflight_session(&path).unwrap();
    let info = super::session_job_metadata(&preflight);
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].id, id);
    assert_eq!(info[0].state, "completed");
    assert!(jobs.take_completions()[0].1.contains("idle-done"));
    assert!(jobs.take_completions().is_empty());
    drop(startup);
    drop(jobs);
    std::fs::remove_dir_all(directory).unwrap();
}
