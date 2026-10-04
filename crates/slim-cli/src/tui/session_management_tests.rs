use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use slim_core::session::{
    list_turns, preflight_session, DurableSessionHeader, JsonlRepo, ManualDrive, ManualExecutor,
    ManualRunSpec, ProviderResponse, SessionWriter,
};
use slim_tui::api::{
    SessionListItem, TranscriptMessage, TranscriptRole, TurnListItem, UiChannels, UiCommand,
    UiEvent,
};

use super::sessions::{
    clean_session_title, list_workspace_sessions, read_session_title, valid_tui_session_id,
    write_session_title,
};
use super::{spawn_tui_session, TuiStartup};
use crate::oauth::{BrowserLauncher, OAuthEndpoints, OAuthError, OAuthService, OAuthStore};

// ---------------------------------------------------------------- fixtures

struct Workspace(PathBuf);

impl Workspace {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "slim-session-mgmt-{label}-{}-{}",
            std::process::id(),
            super::system_time_nanos(SystemTime::now())
        ));
        std::fs::create_dir_all(root.join(".slim").join("sessions")).expect("sessions dir");
        Self(std::fs::canonicalize(root).expect("canonical workspace"))
    }

    fn sessions(&self) -> PathBuf {
        self.0.join(".slim").join("sessions")
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions().join(format!("{id}.jsonl"))
    }

    fn cwd(&self) -> &str {
        self.0.to_str().expect("unicode workspace")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Answering;
struct Failing;

impl ManualExecutor for Answering {
    type Error = std::io::Error;

    fn execute(
        &mut self,
        _effect: &slim_core::session::Effect,
    ) -> Result<ProviderResponse, Self::Error> {
        Ok(ProviderResponse::new("previous answer", None))
    }
}

impl ManualExecutor for Failing {
    type Error = std::io::Error;

    fn execute(
        &mut self,
        _effect: &slim_core::session::Effect,
    ) -> Result<ProviderResponse, Self::Error> {
        Err(std::io::Error::other("fixture failure"))
    }
}

fn header(id: &str, cwd: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "1", cwd, None, None)
}

/// One finished turn per prompt.
fn create_session(ws: &Workspace, id: &str, prompts: &[&str]) -> PathBuf {
    let path = ws.session_path(id);
    let mut repo = JsonlRepo::create(&path, header(id, ws.cwd())).expect("create session");
    for (index, prompt) in prompts.iter().enumerate() {
        let seq = repo.next_seq().expect("next seq");
        ManualDrive::new(&mut repo, &mut Answering)
            .run(ManualRunSpec::new(
                format!("{id}-op-{index}"),
                format!("{id}-attempt-{index}"),
                format!("{id}-user-{index}"),
                format!("{id}-assistant-{index}"),
                *prompt,
                seq,
            ))
            .expect("finish turn");
    }
    path
}

fn create_empty_session(ws: &Workspace, id: &str) -> PathBuf {
    let path = ws.session_path(id);
    drop(JsonlRepo::create(&path, header(id, ws.cwd())).expect("create empty session"));
    path
}

/// Appends a turn whose operation never reaches a terminal record.
fn append_open_turn(path: &Path, id: &str, prompt: &str) {
    let mut repo = JsonlRepo::open_no_repair(path).expect("open session");
    let seq = repo.next_seq().expect("next seq");
    let result = ManualDrive::new(&mut repo, &mut Failing).run(ManualRunSpec::new(
        format!("{id}-open-op"),
        format!("{id}-open-attempt"),
        format!("{id}-open-user"),
        format!("{id}-open-assistant"),
        prompt,
        seq,
    ));
    assert!(
        result.is_err(),
        "the failing executor must leave the turn open"
    );
}

fn set_modified(path: &Path, time: SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(time))
        .expect("set mtime");
}

fn file_names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn jsonl_ids(dir: &Path) -> BTreeSet<String> {
    file_names(dir)
        .into_iter()
        .filter_map(|name| name.strip_suffix(".jsonl").map(str::to_owned))
        .collect()
}

// ------------------------------------------------------------------ worker

struct NoBrowser;

impl BrowserLauncher for NoBrowser {
    fn open(&self, _url: &str) -> Result<(), OAuthError> {
        Ok(())
    }
}

struct Harness {
    runtime: Option<super::TuiRuntimeHandle>,
    channels: UiChannels,
    seen: Vec<UiEvent>,
    next_probe: u64,
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.runtime.take());
    }
}

/// A real worker over `ws`, optionally resumed at `resume` and/or pointed at a
/// mock provider. Sessions persist, so the first prompt creates one.
fn start(ws: &Workspace, resume: Option<&Path>, endpoint: Option<&str>) -> Harness {
    let oauth = OAuthService::new(
        OAuthEndpoints::default(),
        Arc::new(NoBrowser),
        OAuthStore::at(ws.0.join("auth.json")),
    )
    .expect("oauth");
    let options = crate::ProviderRunOptions {
        workspace_root: Some(ws.0.clone()),
        ..crate::ProviderRunOptions::default()
    }
    .with_context_window_tokens(32_000);
    let (kind, endpoint) = match endpoint {
        Some(endpoint) => (
            slim_core::provider::ProviderKind::OpenAiCompatible,
            endpoint.to_owned(),
        ),
        None => (
            slim_core::provider::ProviderKind::OpenCodeGo,
            slim_core::provider::OPENCODE_GO_BASE_URL.to_owned(),
        ),
    };
    let preflight = resume.map(|path| preflight_session(path).expect("resume preflight"));
    let startup = TuiStartup {
        request: Some(crate::ProviderRequest {
            prompt: String::new(),
            mode: slim_core::OperatingMode::Auto,
            kind,
            endpoint,
            model: "fixture-model".into(),
            api_key: "fixture-key".into(),
            account_id: None,
            timeout: Duration::from_secs(20),
        }),
        oauth_session: None,
        options,
        initial_prompt: None,
        image_labels: Vec::new(),
        resume_path: preflight.as_ref().map(|preflight| preflight.path.clone()),
        resume_preflight: preflight,
        pending_session_title: None,
        persist_sessions: true,
        mode: slim_core::OperatingMode::Auto,
        effort: super::ReasoningEffort::High,
        endpoint_override: None,
        model_override: None,
        timeout: Duration::from_secs(20),
    };
    let (runtime, channels) = spawn_tui_session(startup, oauth).expect("runtime");
    let mut harness = Harness {
        runtime: Some(runtime),
        channels,
        seen: Vec::new(),
        next_probe: 1_000_000,
    };
    // Startup events are read here, so a later `mark` only sees what a test causes.
    harness.wait_idle();
    harness
}

impl Harness {
    fn send(&self, command: UiCommand) {
        self.channels.commands.send(command).expect("send command");
    }

    /// Reads events until `done` holds over everything seen so far.
    fn until(&mut self, what: &str, done: impl Fn(&[UiEvent]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if done(&self.seen) {
                return;
            }
            let batch: Vec<UiEvent> = self
                .channels
                .events
                .try_iter()
                .chain(self.channels.events_data.try_iter())
                .collect();
            if batch.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.seen.extend(batch);
        }
        panic!("timed out waiting for {what}; saw {:?}", self.seen);
    }

    /// The `Notification` texts that arrive after `mark`, first one awaited.
    fn notice_after(&mut self, mark: usize) -> String {
        self.until("a notification", |seen| {
            seen[mark..]
                .iter()
                .any(|event| matches!(event, UiEvent::Notification { .. }))
        });
        self.seen[mark..]
            .iter()
            .find_map(|event| match event {
                UiEvent::Notification { message } => Some(message.clone()),
                _ => None,
            })
            .expect("notification")
    }

    fn sessions_listed(&mut self, request_id: u64) -> (Vec<SessionListItem>, Option<String>, u64) {
        self.send(UiCommand::ListSessions { request_id });
        self.until("SessionsListed", |seen| {
            seen.iter().any(|event| {
                matches!(event, UiEvent::SessionsListed { request_id: id, .. } if *id == request_id)
            })
        });
        self.seen
            .iter()
            .find_map(|event| match event {
                UiEvent::SessionsListed {
                    request_id: id,
                    items,
                    error,
                    now_ms,
                } if *id == request_id => Some((items.clone(), error.clone(), *now_ms)),
                _ => None,
            })
            .expect("SessionsListed")
    }

    fn turns_listed(&mut self, request_id: u64) -> (Vec<TurnListItem>, Option<String>) {
        self.send(UiCommand::ListTurns { request_id });
        self.until("TurnsListed", |seen| {
            seen.iter().any(|event| {
                matches!(event, UiEvent::TurnsListed { request_id: id, .. } if *id == request_id)
            })
        });
        self.seen
            .iter()
            .find_map(|event| match event {
                UiEvent::TurnsListed {
                    request_id: id,
                    items,
                    error,
                } if *id == request_id => Some((items.clone(), error.clone())),
                _ => None,
            })
            .expect("TurnsListed")
    }

    /// Blocks until the worker is idle again after a run (it refuses session
    /// commands until then), probing with the list request itself.
    fn wait_idle(&mut self) {
        for _ in 0..200 {
            self.next_probe += 1;
            let probe = self.next_probe;
            let (_, error, _) = self.sessions_listed(probe);
            if error.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("worker never became idle; saw {:?}", self.seen);
    }

    fn run_prompt(&mut self, prompt: &str) {
        let mark = self.seen.len();
        self.send(UiCommand::SendPrompt(prompt.into()));
        self.until("the run to complete", |seen| {
            seen[mark..]
                .iter()
                .any(|event| matches!(event, UiEvent::RunCompleted { .. }))
        });
        self.wait_idle();
    }
}

fn restored_after(seen: &[UiEvent], mark: usize) -> Option<(String, Vec<TranscriptMessage>)> {
    seen[mark..].iter().find_map(|event| match event {
        UiEvent::SessionRestored {
            session_id,
            messages,
            ..
        } => Some((session_id.0.to_string(), messages.clone())),
        _ => None,
    })
}

fn titles_after(seen: &[UiEvent], mark: usize) -> Vec<Option<String>> {
    seen[mark..]
        .iter()
        .filter_map(|event| match event {
            UiEvent::SessionTitleChanged { title } => Some(title.clone()),
            _ => None,
        })
        .collect()
}

fn user_texts(messages: &[TranscriptMessage]) -> Vec<&str> {
    messages
        .iter()
        .filter(|message| matches!(message.role, TranscriptRole::User))
        .map(|message| message.text.as_str())
        .collect()
}

// -------------------------------------------------------------- mock provider

fn read_request_body(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    let expected = loop {
        let size = stream.read(&mut chunk).expect("request");
        assert!(size > 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..size]);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let length = String::from_utf8_lossy(&bytes[..end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .expect("content length");
        break end + 4 + length;
    };
    while bytes.len() < expected {
        let size = stream.read(&mut chunk).expect("request body");
        assert!(size > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..size]);
    }
    String::from_utf8_lossy(&bytes[..expected]).into_owned()
}

struct Provider {
    endpoint: String,
    bodies: mpsc::Receiver<String>,
    release_first: mpsc::Sender<()>,
}

/// Serves `answers` streamed completions ("answer-N-marker"). With `hold_first`
/// the first response waits for `release_first`, keeping that run active.
fn provider(answers: usize, hold_first: bool) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let (body_tx, bodies) = mpsc::channel();
    let (release_first, release_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        for index in 0..answers {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            let body = read_request_body(&mut stream);
            let _ = body_tx.send(body);
            if index == 0 && hold_first {
                let _ = release_rx.recv_timeout(Duration::from_secs(30));
            }
            let events = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"answer-{index}-marker\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                events.len()
            );
        }
    });
    Provider {
        endpoint,
        bodies,
        release_first,
    }
}

// -------------------------------------------------------------------- sidecar

#[test]
fn titles_are_one_line_capped_and_free_of_control_characters() {
    assert_eq!(clean_session_title("  a\nb\t\u{1b}[31mc \r\n"), "a b [31mc");
    assert_eq!(clean_session_title("a\u{202E}b\u{200B}c"), "abc");
    assert_eq!(clean_session_title(" \n\t "), "");

    let long = clean_session_title(&"x".repeat(200));
    assert_eq!(long.chars().count(), 80);
    assert!(long.ends_with('…'));
    let wide = clean_session_title(&"界".repeat(100));
    assert!(wide.ends_with('…'));
    assert!(
        wide.chars().count() <= 40,
        "two cells per character: {wide}"
    );
    assert_eq!(clean_session_title("curto"), "curto");
}

#[test]
fn sidecar_round_trips_replaces_atomically_and_tolerates_bad_files() {
    let ws = Workspace::new("sidecar");
    let session = ws.session_path("tui-a");
    let meta = ws.sessions().join("tui-a.meta.json");
    assert_eq!(read_session_title(&session), None, "missing sidecar");

    write_session_title(&session, "Primeiro nome").expect("write");
    assert_eq!(
        read_session_title(&session).as_deref(),
        Some("Primeiro nome")
    );
    write_session_title(&session, "Segundo nome").expect("replace");
    assert_eq!(
        read_session_title(&session).as_deref(),
        Some("Segundo nome")
    );
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta).expect("sidecar")).expect("json");
    assert_eq!(stored["title"], "Segundo nome");
    assert!(stored["updated_ns"].as_u64().is_some_and(|ns| ns > 0));
    assert_eq!(
        file_names(&ws.sessions()),
        BTreeSet::from(["tui-a.meta.json".to_owned()]),
        "no temp file may survive a write"
    );

    write_session_title(&session, "").expect("clear");
    assert_eq!(read_session_title(&session), None);
    assert!(!meta.exists(), "clearing removes the sidecar");
    write_session_title(&session, "").expect("clearing twice is fine");

    for garbage in [
        b"not json".as_slice(),
        b"{\"title\": 7, \"updated_ns\": 1}",
        b"{\"title\": null, \"updated_ns\": 1}",
        b"",
    ] {
        std::fs::write(&meta, garbage).expect("garbage");
        assert_eq!(read_session_title(&session), None);
    }
    std::fs::write(&meta, vec![b' '; 64 * 1024]).expect("oversized");
    assert_eq!(read_session_title(&session), None);
    std::fs::write(&meta, "{\"title\": \"a\\nb\\u001b[0m\", \"updated_ns\": 1}").expect("edited");
    assert_eq!(read_session_title(&session).as_deref(), Some("a b [0m"));
}

// -------------------------------------------------------------------- listing

#[test]
fn listing_is_confined_to_this_workspaces_tui_sessions() {
    let ws = Workspace::new("listing");
    let foreign = Workspace::new("listing-foreign");
    let now = SystemTime::now();
    let ago = |seconds| now - Duration::from_secs(seconds);

    let newest = create_session(&ws, "tui-new", &["second question"]);
    set_modified(&newest, ago(10));
    let titled_empty = create_empty_session(&ws, "tui-titled-empty");
    write_session_title(&titled_empty, "So um nome").unwrap();
    set_modified(&titled_empty, ago(20));
    let older = create_session(&ws, "tui-old", &["first question\nsecond line", "later"]);
    write_session_title(&older, "Meu titulo").unwrap();
    set_modified(&older, ago(30));
    let busy = create_session(&ws, "tui-busy", &["busy question"]);
    set_modified(&busy, ago(40));
    create_empty_session(&ws, "tui-empty");
    let other_cwd = foreign.session_path("tui-foreign");
    drop(JsonlRepo::create(&other_cwd, header("tui-foreign", foreign.cwd())).unwrap());
    std::fs::copy(&other_cwd, ws.session_path("tui-foreign")).unwrap();
    create_session(&ws, "plain", &["not a tui session"]);
    std::fs::write(
        ws.session_path("tui-mismatch"),
        std::fs::read_to_string(create_session(&ws, "tui-source", &["copied"]))
            .unwrap()
            .replacen("tui-source", "tui-other", 1),
    )
    .unwrap();
    std::fs::write(ws.session_path("tui-junk"), "not json at all\n").unwrap();
    let v1 = ws.session_path("tui-v1");
    drop(SessionWriter::create(&v1, "tui-v1", ws.cwd()).expect("v1"));
    std::fs::remove_file(ws.session_path("tui-source")).unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&newest, ws.session_path("tui-link")).unwrap();

    let before = file_names(&ws.sessions());
    let held = JsonlRepo::open_no_repair(&busy).expect("hold the lock");
    let items = list_workspace_sessions(&ws.0, Some(&newest)).expect("list");
    drop(held);
    assert_eq!(
        file_names(&ws.sessions()),
        before,
        "listing must not create or remove any file"
    );

    let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(
        ids,
        ["tui-new", "tui-titled-empty", "tui-old", "tui-busy"],
        "most recent first; foreign, non-tui, mismatched, junk, v1, empty and symlinked are out"
    );
    let by_id = |id: &str| items.iter().find(|item| item.id == id).unwrap();
    assert!(by_id("tui-new").current && !by_id("tui-new").in_use);
    assert_eq!(by_id("tui-new").first_prompt, "second question");
    assert!(by_id("tui-busy").in_use && !by_id("tui-busy").current);
    assert!(!by_id("tui-old").in_use);
    assert_eq!(by_id("tui-old").title.as_deref(), Some("Meu titulo"));
    assert_eq!(by_id("tui-old").first_prompt, "first question");
    assert_eq!(by_id("tui-titled-empty").first_prompt, "");
    assert_eq!(
        by_id("tui-titled-empty").title.as_deref(),
        Some("So um nome")
    );
    assert_eq!(
        by_id("tui-new").bytes,
        std::fs::metadata(&newest).unwrap().len()
    );
    let expected_ms = ago(10)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        by_id("tui-new").updated_ms.abs_diff(expected_ms) < 5,
        "mtime in ms"
    );

    let after = list_workspace_sessions(&ws.0, None).expect("list again");
    assert!(
        after.iter().all(|item| !item.in_use && !item.current),
        "a released lock is not in use"
    );
    assert_eq!(file_names(&ws.sessions()), before);
}

#[test]
fn listing_keeps_only_the_fifty_most_recent() {
    let ws = Workspace::new("listing-cap");
    let now = SystemTime::now();
    for index in 0..55_u64 {
        let path = create_session(&ws, &format!("tui-cap-{index:02}"), &["question"]);
        set_modified(&path, now - Duration::from_secs(1_000 - index));
    }
    let items = list_workspace_sessions(&ws.0, None).expect("list");
    assert_eq!(items.len(), 50);
    assert_eq!(items[0].id, "tui-cap-54");
    assert_eq!(items[49].id, "tui-cap-05");
}

#[test]
fn session_ids_are_validated_before_they_name_a_path() {
    for good in ["tui-1-2-3", "tui-abc_DEF-9", "tui-x"] {
        assert!(valid_tui_session_id(good), "{good}");
    }
    let too_long = format!("tui-{}", "a".repeat(200));
    for bad in [
        "",
        "tui-",
        "tui",
        "x-tui-1",
        "../x",
        "..\\x",
        "tui-../x",
        "tui-a/b",
        "tui-a\\b",
        "tui-a.b",
        "tui-a:b",
        "/etc/passwd",
        "C:\\Windows",
        "tui-a b",
        "tui-\u{e9}",
        too_long.as_str(),
    ] {
        assert!(!valid_tui_session_id(bad), "{bad:?}");
    }
}

#[test]
fn a_session_lock_is_reported_and_probing_leaves_no_lock_behind() {
    let ws = Workspace::new("lock-probe");
    let path = create_session(&ws, "tui-locked", &["question"]);
    let lock = PathBuf::from(format!("{}.lock", path.display()));
    assert!(!super::sessions::session_in_use(&path));
    let held = JsonlRepo::open_no_repair(&path).expect("hold");
    assert!(super::sessions::session_in_use(&path));
    assert!(
        super::sessions::session_in_use(&path),
        "probing twice changes nothing"
    );
    drop(held);
    assert!(!super::sessions::session_in_use(&path));
    std::fs::remove_file(&lock).expect("remove lock");
    assert!(!super::sessions::session_in_use(&path));
    assert!(!lock.exists(), "a probe must not create the lock file");
    JsonlRepo::open_no_repair(&path).expect("still openable after probes");
}

// ------------------------------------------------------------ list + resume

#[test]
fn worker_lists_sessions_and_announces_the_restored_title() {
    let ws = Workspace::new("worker-list");
    let current = create_session(&ws, "tui-current", &["current question"]);
    write_session_title(&current, "Sessao atual").unwrap();
    let other = create_session(&ws, "tui-other", &["other question"]);
    set_modified(&other, SystemTime::now() - Duration::from_secs(60));

    let mut harness = start(&ws, Some(&current), None);
    let (items, error, now_ms) = harness.sessions_listed(7);
    assert_eq!(error, None);
    assert!(
        now_ms > 1_700_000_000_000,
        "host clock in unix ms: {now_ms}"
    );
    let current_item = items.iter().find(|item| item.id == "tui-current").unwrap();
    assert!(current_item.current);
    assert_eq!(current_item.title.as_deref(), Some("Sessao atual"));
    assert!(items
        .iter()
        .any(|item| item.id == "tui-other" && !item.current));
    assert_eq!(
        titles_after(&harness.seen, 0),
        [Some("Sessao atual".to_owned())],
        "startup restore announces the sidecar title"
    );
}

#[test]
fn resume_refuses_bad_ids_and_ineligible_sessions_then_restores_a_good_one() {
    let ws = Workspace::new("worker-resume");
    let foreign = Workspace::new("worker-resume-foreign");
    let current = create_session(&ws, "tui-current", &["current question"]);
    let good = create_session(&ws, "tui-good", &["good question", "second good"]);
    write_session_title(&good, "Bom").unwrap();
    let good_before = std::fs::read(&good).unwrap();
    create_empty_session(&ws, "tui-empty");
    let torn = create_session(&ws, "tui-torn", &["torn question"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&torn)
        .unwrap()
        .write_all(br#"{"type":"operation""#)
        .unwrap();
    drop(SessionWriter::create(ws.session_path("tui-v1"), "tui-v1", ws.cwd()).unwrap());
    let other_cwd = create_session(&foreign, "tui-foreign", &["foreign question"]);
    std::fs::copy(&other_cwd, ws.session_path("tui-foreign")).unwrap();
    let busy = create_session(&ws, "tui-busy", &["busy question"]);
    let _held = JsonlRepo::open_no_repair(&busy).expect("hold the lock");

    let mut harness = start(&ws, Some(&current), None);
    let refused = |harness: &mut Harness, id: &str| -> String {
        let mark = harness.seen.len();
        harness.send(UiCommand::ResumeSession { id: id.into() });
        let message = harness.notice_after(mark);
        assert!(
            restored_after(&harness.seen, mark).is_none(),
            "{id:?} must not switch sessions"
        );
        message
    };

    for bad in [
        "../x",
        "..\\x",
        "/abs/path",
        "C:\\abs",
        "x-tui-1",
        "tui-",
        "tui-a/b",
        "tui-a.b",
    ] {
        assert_eq!(
            refused(&mut harness, bad),
            "Identificador de sessão inválido",
            "{bad}"
        );
    }
    assert_eq!(
        refused(&mut harness, "tui-missing"),
        "Sessão não encontrada neste diretório"
    );
    for hidden in ["tui-v1", "tui-foreign"] {
        assert_eq!(
            refused(&mut harness, hidden),
            "Sessão não encontrada neste diretório",
            "{hidden} is not a session of this workspace"
        );
    }
    assert!(refused(&mut harness, "tui-torn").starts_with("Sessão não pode ser retomada"));
    let empty = refused(&mut harness, "tui-empty");
    assert!(
        empty.contains("conversa concluída") || empty.starts_with("Sessão não pode ser retomada"),
        "{empty}"
    );
    assert_eq!(
        refused(&mut harness, "tui-busy"),
        "A sessão está em uso por outro processo"
    );
    assert_eq!(
        refused(&mut harness, "tui-current"),
        "Esta já é a sessão atual"
    );

    let mark = harness.seen.len();
    harness.send(UiCommand::ResumeSession {
        id: "tui-good".into(),
    });
    harness.until("the good session to restore", |seen| {
        restored_after(seen, mark).is_some() && !titles_after(seen, mark).is_empty()
    });
    let (id, messages) = restored_after(&harness.seen, mark).unwrap();
    assert_eq!(id, "tui-good");
    assert_eq!(user_texts(&messages), ["good question", "second good"]);
    assert_eq!(titles_after(&harness.seen, mark), [Some("Bom".to_owned())]);
    assert_eq!(
        harness.notice_after(mark),
        "Sessão retomada. Envie um prompt para continuar."
    );
    assert_eq!(
        std::fs::read(&good).unwrap(),
        good_before,
        "resume is read-only"
    );

    let (items, error) = harness.turns_listed(2);
    assert_eq!(error, None);
    assert_eq!(items.len(), 2, "the resumed session is now the current one");
    let (list, _, _) = harness.sessions_listed(3);
    assert!(
        list.iter()
            .find(|item| item.id == "tui-good")
            .unwrap()
            .current
    );
    assert!(
        !list
            .iter()
            .find(|item| item.id == "tui-current")
            .unwrap()
            .current
    );
}

// --------------------------------------------------------------------- rewind

#[test]
fn rewind_forks_before_the_chosen_turn_and_hands_back_its_prompt() {
    let ws = Workspace::new("worker-rewind");
    let prompts = [
        "first prompt",
        "second prompt\nwith a second line",
        "third prompt",
    ];
    let current = create_session(&ws, "tui-current", &prompts);
    write_session_title(&current, "Nome mantido").unwrap();
    let original = std::fs::read(&current).unwrap();
    let sessions_before = jsonl_ids(&ws.sessions());

    let mut harness = start(&ws, Some(&current), None);
    let (turns, error) = harness.turns_listed(4);
    assert_eq!(error, None);
    assert_eq!(
        turns.iter().map(|turn| turn.index).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        turns
            .iter()
            .map(|turn| turn.prompt.as_str())
            .collect::<Vec<_>>(),
        ["first prompt", "second prompt", "third prompt"]
    );
    assert!(turns
        .windows(2)
        .all(|pair| pair[0].first_seq < pair[1].first_seq));

    let mark = harness.seen.len();
    harness.send(UiCommand::RewindSession {
        first_seq: turns[1].first_seq,
    });
    harness.until("the rewind to finish", |seen| {
        restored_after(seen, mark).is_some()
            && seen[mark..]
                .iter()
                .any(|event| matches!(event, UiEvent::RestoreDraft { .. }))
            && !titles_after(seen, mark).is_empty()
    });
    let (child_id, messages) = restored_after(&harness.seen, mark).unwrap();
    assert_ne!(child_id, "tui-current");
    assert!(valid_tui_session_id(&child_id), "{child_id}");
    assert_eq!(user_texts(&messages), ["first prompt"]);
    assert_eq!(messages.len(), 2, "one earlier turn: prompt and answer");
    assert!(harness.seen[mark..].iter().any(|event| matches!(event,
        UiEvent::RestoreDraft { text } if text == "second prompt\nwith a second line")));
    assert_eq!(
        harness.notice_after(mark),
        "Voltou 2 turno(s). Arquivos alterados não foram restaurados; veja Ctrl+D."
    );
    assert_eq!(
        titles_after(&harness.seen, mark),
        [Some("Nome mantido".to_owned())]
    );

    let child = ws.session_path(&child_id);
    assert!(child.exists());
    let child_preflight = preflight_session(&child).unwrap();
    let child_header = child_preflight.header.as_ref().unwrap();
    assert_eq!(child_header.id, child_id, "header id equals the file stem");
    assert_eq!(child_header.parent_id.as_deref(), Some("tui-current"));
    assert_eq!(list_turns(&child).unwrap().len(), 1);
    assert_eq!(
        std::fs::read(&current).unwrap(),
        original,
        "original untouched"
    );
    assert_eq!(
        jsonl_ids(&ws.sessions()),
        sessions_before
            .iter()
            .cloned()
            .chain([child_id.clone()])
            .collect::<BTreeSet<_>>(),
        "exactly one new session"
    );

    let (turns_now, _) = harness.turns_listed(5);
    assert_eq!(turns_now.len(), 1, "the child is the current session now");
    let (list, _, _) = harness.sessions_listed(6);
    assert!(
        list.iter()
            .find(|item| item.id == child_id)
            .unwrap()
            .current
    );
    assert_eq!(
        list.iter()
            .find(|item| item.id == child_id)
            .unwrap()
            .title
            .as_deref(),
        Some("Nome mantido")
    );
}

#[test]
fn rewinding_to_the_first_turn_gives_an_empty_valid_session() {
    let ws = Workspace::new("worker-rewind-zero");
    let current = create_session(&ws, "tui-current", &["only prompt", "next prompt"]);
    let original = std::fs::read(&current).unwrap();
    let first_seq = list_turns(&current).unwrap()[0].first_seq;

    let mut harness = start(&ws, Some(&current), None);
    let mark = harness.seen.len();
    harness.send(UiCommand::RewindSession { first_seq });
    harness.until("the rewind to finish", |seen| {
        restored_after(seen, mark).is_some()
            && seen[mark..]
                .iter()
                .any(|event| matches!(event, UiEvent::RestoreDraft { .. }))
    });
    let (child_id, messages) = restored_after(&harness.seen, mark).unwrap();
    assert!(messages.is_empty(), "no earlier turn to show: {messages:?}");
    assert!(harness.seen[mark..].iter().any(|event| matches!(event,
        UiEvent::RestoreDraft { text } if text == "only prompt")));
    assert_eq!(
        harness.notice_after(mark),
        "Voltou 2 turno(s). Arquivos alterados não foram restaurados; veja Ctrl+D."
    );
    let child = ws.session_path(&child_id);
    let preflight = preflight_session(&child).unwrap();
    assert!(crate::headless::ensure_resume_preflight(&preflight).is_ok());
    assert!(list_turns(&child).unwrap().is_empty());
    assert_eq!(std::fs::read(&current).unwrap(), original);

    let (items, error) = harness.turns_listed(3);
    assert!(items.is_empty());
    assert_eq!(
        error.as_deref(),
        Some("Nenhuma conversa salva nesta sessão")
    );
    let (list, _, _) = harness.sessions_listed(4);
    assert!(
        list.iter().all(|item| item.id != child_id),
        "an empty child stays out of the /resume list"
    );
}

#[test]
fn rewind_refuses_unknown_and_unfinished_turns_without_creating_files() {
    let ws = Workspace::new("worker-rewind-refused");
    let current = create_session(&ws, "tui-current", &["done prompt"]);
    append_open_turn(&current, "tui-current", "unfinished prompt");
    let turns = list_turns(&current).unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns[0].terminal && !turns[1].terminal);
    let original = std::fs::read(&current).unwrap();
    let before = file_names(&ws.sessions());

    let mut harness = start(&ws, Some(&current), None);
    let (listed, error) = harness.turns_listed(1);
    assert_eq!(error, None);
    assert_eq!(listed.len(), 1, "only finished turns are offered");
    assert_eq!(listed[0].first_seq, turns[0].first_seq);

    for (first_seq, message) in [
        (9_999, "Turno não encontrado nesta sessão"),
        (turns[1].first_seq, "Este turno ainda não terminou"),
    ] {
        let mark = harness.seen.len();
        harness.send(UiCommand::RewindSession { first_seq });
        assert_eq!(harness.notice_after(mark), message);
        assert!(restored_after(&harness.seen, mark).is_none());
        assert!(!harness.seen[mark..]
            .iter()
            .any(|event| matches!(event, UiEvent::RestoreDraft { .. })));
    }
    assert_eq!(file_names(&ws.sessions()), before, "no file may be created");
    assert_eq!(std::fs::read(&current).unwrap(), original);
}

#[test]
fn turns_and_rewind_without_a_saved_session_are_answered() {
    let ws = Workspace::new("worker-no-session");
    let mut harness = start(&ws, None, None);
    let (items, error) = harness.turns_listed(1);
    assert!(items.is_empty());
    assert_eq!(
        error.as_deref(),
        Some("Nenhuma conversa salva nesta sessão")
    );
    let mark = harness.seen.len();
    harness.send(UiCommand::RewindSession { first_seq: 0 });
    assert_eq!(
        harness.notice_after(mark),
        "Nenhuma conversa salva nesta sessão"
    );
    let (list, error, _) = harness.sessions_listed(2);
    assert!(list.is_empty());
    assert_eq!(error, None);
}

// -------------------------------------------------------------------- rename

#[test]
fn rename_persists_beside_the_session_and_can_be_cleared() {
    let ws = Workspace::new("worker-rename");
    let current = create_session(&ws, "tui-current", &["question"]);
    let mut harness = start(&ws, Some(&current), None);

    let mark = harness.seen.len();
    harness.send(UiCommand::RenameSession {
        title: "  Novo\nnome ".into(),
    });
    harness.until("the rename", |seen| !titles_after(seen, mark).is_empty());
    assert_eq!(
        titles_after(&harness.seen, mark),
        [Some("Novo nome".to_owned())]
    );
    assert_eq!(harness.notice_after(mark), "Sessão renomeada: Novo nome");
    assert_eq!(read_session_title(&current).as_deref(), Some("Novo nome"));

    let mark = harness.seen.len();
    harness.send(UiCommand::RenameSession {
        title: "   ".into(),
    });
    harness.until("the clear", |seen| !titles_after(seen, mark).is_empty());
    assert_eq!(titles_after(&harness.seen, mark), [None]);
    assert_eq!(harness.notice_after(mark), "Nome da sessão removido");
    assert_eq!(read_session_title(&current), None);
}

#[test]
fn title_survives_a_switch_and_is_reannounced_each_time() {
    let ws = Workspace::new("worker-title-switch");
    let a = create_session(&ws, "tui-a", &["question a"]);
    write_session_title(&a, "Alfa").unwrap();
    create_session(&ws, "tui-b", &["question b"]);

    let mut harness = start(&ws, Some(&ws.session_path("tui-b")), None);
    harness.wait_idle();
    assert_eq!(titles_after(&harness.seen, 0), [None], "unnamed at startup");

    let mark = harness.seen.len();
    harness.send(UiCommand::ResumeSession { id: "tui-a".into() });
    harness.until("switch to a", |seen| !titles_after(seen, mark).is_empty());
    assert_eq!(titles_after(&harness.seen, mark), [Some("Alfa".to_owned())]);

    let mark = harness.seen.len();
    harness.send(UiCommand::ResumeSession { id: "tui-b".into() });
    harness.until("switch back to b", |seen| {
        !titles_after(seen, mark).is_empty()
    });
    assert_eq!(titles_after(&harness.seen, mark), [None]);
    assert_eq!(
        read_session_title(&a).as_deref(),
        Some("Alfa"),
        "a keeps its name"
    );
}

// ------------------------------------------------------------ end to end

#[test]
fn rename_before_the_first_prompt_is_written_at_creation_and_rewind_continues_truncated() {
    let ws = Workspace::new("worker-e2e");
    let provider = provider(3, false);
    let mut harness = start(&ws, None, Some(&provider.endpoint));

    harness.send(UiCommand::RenameSession {
        title: "Rascunho".into(),
    });
    harness.until("the rename", |seen| !titles_after(seen, 0).is_empty());
    assert_eq!(
        titles_after(&harness.seen, 0),
        [Some("Rascunho".to_owned())]
    );
    assert!(
        jsonl_ids(&ws.sessions()).is_empty(),
        "no session file before the first prompt"
    );

    harness.run_prompt("hello world");
    let ids = jsonl_ids(&ws.sessions());
    assert_eq!(ids.len(), 1);
    let session = ws.session_path(ids.iter().next().unwrap());
    assert_eq!(read_session_title(&session).as_deref(), Some("Rascunho"));
    assert_eq!(
        titles_after(&harness.seen, 0),
        [Some("Rascunho".to_owned()), Some("Rascunho".to_owned())],
        "creation re-announces the title"
    );
    let first_body = provider
        .bodies
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(first_body.contains("hello world"));

    harness.run_prompt("second question");
    provider
        .bodies
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (turns, error) = harness.turns_listed(10);
    assert_eq!(error, None);
    assert_eq!(
        turns
            .iter()
            .map(|turn| turn.prompt.as_str())
            .collect::<Vec<_>>(),
        ["hello world", "second question"]
    );

    let mark = harness.seen.len();
    harness.send(UiCommand::RewindSession {
        first_seq: turns[1].first_seq,
    });
    harness.until("the rewind", |seen| {
        restored_after(seen, mark).is_some()
            && seen[mark..]
                .iter()
                .any(|event| matches!(event, UiEvent::RestoreDraft { .. }))
    });
    assert!(harness.seen[mark..].iter().any(|event| matches!(event,
        UiEvent::RestoreDraft { text } if text == "second question")));

    harness.run_prompt("third question");
    let body = provider
        .bodies
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(body.contains("hello world"), "kept turn is sent: {body}");
    assert!(
        body.contains("answer-0-marker"),
        "kept answer is sent: {body}"
    );
    assert!(body.contains("third question"), "{body}");
    assert!(
        !body.contains("second question") && !body.contains("answer-1-marker"),
        "the rewound turn must not reach the model: {body}"
    );
    assert_eq!(jsonl_ids(&ws.sessions()).len(), 2, "original and child");
}

// ----------------------------------------------------- refusals off the idle path

#[test]
fn every_session_command_is_answered_while_a_run_is_active() {
    let ws = Workspace::new("worker-active");
    let provider = provider(1, true);
    let mut harness = start(&ws, None, Some(&provider.endpoint));
    let mark = harness.seen.len();
    harness.send(UiCommand::SendPrompt("hold this run".into()));
    provider
        .bodies
        .recv_timeout(Duration::from_secs(10))
        .unwrap();

    harness.send(UiCommand::ListSessions { request_id: 21 });
    harness.send(UiCommand::ListTurns { request_id: 22 });
    harness.send(UiCommand::RenameSession { title: "x".into() });
    harness.send(UiCommand::ResumeSession { id: "tui-x".into() });
    harness.send(UiCommand::RewindSession { first_seq: 0 });
    harness.until("all five answers", |seen| {
        let listed = seen[mark..].iter().any(|event| {
            matches!(event,
            UiEvent::SessionsListed { request_id: 21, items, error: Some(error), .. }
                if items.is_empty() && error.contains("Aguarde ou cancele a execução"))
        });
        let turns = seen[mark..].iter().any(|event| {
            matches!(event,
            UiEvent::TurnsListed { request_id: 22, items, error: Some(error) }
                if items.is_empty() && error.contains("Aguarde ou cancele a execução"))
        });
        let notices = seen[mark..]
            .iter()
            .filter(|event| {
                matches!(event, UiEvent::Notification { message }
                if message.starts_with("Aguarde ou cancele a execução antes de"))
            })
            .count();
        listed && turns && notices == 3
    });
    assert!(
        !harness.seen[mark..]
            .iter()
            .any(|event| matches!(event, UiEvent::SessionTitleChanged { title: Some(_) })),
        "a refused rename changes nothing"
    );

    provider.release_first.send(()).unwrap();
    harness.until("the run to complete", |seen| {
        seen[mark..]
            .iter()
            .any(|event| matches!(event, UiEvent::RunCompleted { .. }))
    });
    harness.wait_idle();
}

#[test]
fn mcp_changes_are_refused_in_portuguese_while_a_run_is_active() {
    let ws = Workspace::new("worker-active-mcp");
    let provider = provider(1, true);
    let mut harness = start(&ws, None, Some(&provider.endpoint));
    let mark = harness.seen.len();
    harness.send(UiCommand::SendPrompt("hold this run".into()));
    provider
        .bodies
        .recv_timeout(Duration::from_secs(10))
        .unwrap();

    harness.send(UiCommand::McpEnable {
        name: "web".into(),
        enabled: true,
    });
    harness.send(UiCommand::McpTrust {
        trust: true,
        name: None,
    });
    harness.send(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: None,
    });
    harness.until("three refusals", |seen| {
        seen[mark..]
            .iter()
            .filter(|event| {
                matches!(event, UiEvent::Notification { message }
                if message == "Aguarde ou cancele a execução antes de alterar servidores MCP")
            })
            .count()
            == 3
    });
    assert!(
        !harness.seen[mark..].iter().any(|event| matches!(event,
            UiEvent::Notification { message } if message.contains("already active"))),
        "the refusal is not the English catch-all"
    );

    provider.release_first.send(()).unwrap();
    harness.until("the run to complete", |seen| {
        seen[mark..]
            .iter()
            .any(|event| matches!(event, UiEvent::RunCompleted { .. }))
    });
    harness.wait_idle();
}

#[test]
fn a_prompt_admitted_while_a_run_is_active_has_its_admission_resolved() {
    use slim_tui::api::{PromptAdmission, PromptGeneration, PromptId, PromptOrigin};

    let ws = Workspace::new("worker-active-prompt");
    let provider = provider(1, true);
    let mut harness = start(&ws, None, Some(&provider.endpoint));
    let mark = harness.seen.len();
    harness.send(UiCommand::SendPrompt("hold this run".into()));
    provider
        .bodies
        .recv_timeout(Duration::from_secs(10))
        .unwrap();

    let admission = PromptAdmission {
        id: PromptId(7),
        generation: PromptGeneration(7),
        origin: PromptOrigin::Direct,
    };
    harness.send(UiCommand::PreparePrompt {
        prompt: "too early".into(),
        admission,
    });
    harness.until("the admission to be refused", |seen| {
        seen[mark..].iter().any(|event| {
            matches!(event,
            UiEvent::PromptPreparationFailed { admission: refused, message }
                if *refused == admission && message.contains("Aguarde ou cancele a execução"))
        })
    });

    provider.release_first.send(()).unwrap();
    harness.until("the run to complete", |seen| {
        seen[mark..]
            .iter()
            .any(|event| matches!(event, UiEvent::RunCompleted { .. }))
    });
    harness.wait_idle();
}

#[test]
fn refusal_answers_carry_the_request_id_and_never_leave_a_list_hanging() {
    let (sink, control_rx, data_rx) = super::cancel_tests::sink_for_tests();
    let commands = [
        UiCommand::ListSessions { request_id: 3 },
        UiCommand::ListTurns { request_id: 4 },
        UiCommand::RenameSession { title: "t".into() },
        UiCommand::ResumeSession { id: "tui-x".into() },
        UiCommand::RewindSession { first_seq: 1 },
    ];
    for command in &commands {
        assert!(super::sessions::is_session_command(command));
        super::sessions::refuse_session_command(&sink, command, "Conclua ou cancele o login");
    }
    assert!(!super::sessions::is_session_command(&UiCommand::CancelRun));
    match control_rx.try_recv().expect("sessions answer") {
        UiEvent::SessionsListed {
            request_id: 3,
            items,
            error: Some(error),
            ..
        } => {
            assert!(items.is_empty());
            assert_eq!(
                error,
                "Conclua ou cancele o login antes de listar as sessões"
            );
        }
        other => panic!("{other:?}"),
    }
    match control_rx.try_recv().expect("turns answer") {
        UiEvent::TurnsListed {
            request_id: 4,
            items,
            error: Some(error),
        } => {
            assert!(items.is_empty());
            assert_eq!(
                error,
                "Conclua ou cancele o login antes de listar os turnos"
            );
        }
        other => panic!("{other:?}"),
    }
    let notices: Vec<String> = data_rx
        .try_iter()
        .filter_map(|event| match event {
            UiEvent::Notification { message } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        [
            "Conclua ou cancele o login antes de renomear a sessão",
            "Conclua ou cancele o login antes de retomar outra sessão",
            "Conclua ou cancele o login antes de voltar a um turno",
        ]
    );
}

#[test]
fn a_run_finishing_delivery_still_answers_session_commands() {
    let (sink, control_rx, data_rx) = super::cancel_tests::sink_for_tests();
    let mut run = super::PendingRun {
        jobs: None,
        run_id: 7,
        admission: None,
        result: None,
        projector: None,
        delivery: Default::default(),
        cancellation: slim_core::runtime::CancellationToken::new(),
        durable: false,
        cancel_requested: false,
        content_store: Default::default(),
        workspace_root: None,
    };
    for command in [
        UiCommand::ListSessions { request_id: 8 },
        UiCommand::RewindSession { first_seq: 1 },
    ] {
        let shutdown = super::dispatch_pending_command(
            &mut run,
            Some(command),
            &sink,
            &mut false,
            &None,
            &Default::default(),
        );
        assert!(!shutdown);
    }
    assert!(matches!(
        control_rx.try_recv().expect("list answer"),
        UiEvent::SessionsListed { request_id: 8, error: Some(error), .. }
            if error.starts_with("Aguarde a execução terminar")
    ));
    assert!(matches!(
        data_rx.try_recv().expect("rewind notice"),
        UiEvent::Notification { message } if message.starts_with("Aguarde a execução terminar")
    ));
}

#[test]
fn a_run_finishing_delivery_refuses_mcp_changes_and_still_serves_a_waiting_sign_in() {
    let (sink, _control_rx, data_rx) = super::cancel_tests::sink_for_tests();
    let mut run = super::PendingRun {
        jobs: None,
        run_id: 7,
        admission: None,
        result: None,
        projector: None,
        delivery: Default::default(),
        cancellation: slim_core::runtime::CancellationToken::new(),
        durable: false,
        cancel_requested: false,
        content_store: Default::default(),
        workspace_root: None,
    };
    let logins: super::mcp_login::McpLogins = Default::default();
    let (cancelled, mut pasted) = super::mcp_login::register_for_tests(&logins, "web");
    let mut dispatch = |command: UiCommand| {
        assert!(!super::dispatch_pending_command(
            &mut run,
            Some(command),
            &sink,
            &mut false,
            &None,
            &logins,
        ));
    };
    // Changes wait for the run: the user is told, in Portuguese.
    for command in [
        UiCommand::McpEnable {
            name: "web".into(),
            enabled: true,
        },
        UiCommand::McpTrust {
            trust: true,
            name: None,
        },
        UiCommand::McpLogin {
            name: "web".into(),
            redirect_url: None,
        },
    ] {
        dispatch(command);
        assert!(matches!(
            data_rx.try_recv().expect("refusal notice"),
            UiEvent::Notification { message }
                if message == "Aguarde ou cancele a execução antes de alterar servidores MCP"
        ));
    }
    // Finishing or dismissing a sign-in already waiting is not a change.
    dispatch(UiCommand::McpLogin {
        name: "web".into(),
        redirect_url: Some("http://127.0.0.1:9/callback?code=1&state=2".into()),
    });
    assert_eq!(
        pasted.try_recv().expect("pasted redirect delivered"),
        "http://127.0.0.1:9/callback?code=1&state=2"
    );
    assert!(!*cancelled.borrow());
    dispatch(UiCommand::McpLoginCancel { name: "web".into() });
    assert!(*cancelled.borrow(), "the sign-in was cancelled");
    assert!(data_rx.try_recv().is_err(), "no stray notices");
}

#[test]
fn idle_background_completion_is_not_a_model_turn_and_next_prompt_carries_it_once() {
    use slim_tui::api::{PromptAdmission, PromptGeneration, PromptId, PromptOrigin};
    let ws = Workspace::new("job-next-prompt");
    let provider = provider(1, false);
    let mut harness = start(&ws, None, Some(&provider.endpoint));
    let command = if cfg!(windows) {
        "Write-Output 'READY'; while (!(Test-Path 'release-job')) { Start-Sleep -Milliseconds 20 }; Write-Output 'IDLE-DONE'"
    } else {
        "printf 'READY\n'; while [ ! -f release-job ]; do sleep 0.02; done; printf 'IDLE-DONE\n'"
    };
    harness.send(UiCommand::RunBackgroundShell {
        command: command.into(),
    });
    harness.until("job start", |seen| {
        seen.iter().any(
            |e| matches!(e,UiEvent::JobsChanged{jobs} if jobs.iter().any(|j|j.state=="running")),
        )
    });
    std::fs::write(ws.0.join("release-job"), "go").unwrap();
    harness.until("idle job notification",|seen|seen.iter().any(|e|matches!(e,UiEvent::Notification{message} if message.contains("shell-1")&&message.contains("concluído"))));
    assert!(
        provider.bodies.try_recv().is_err(),
        "idle completion must not call the provider"
    );
    harness.send(UiCommand::PreparePrompt {
        admission: PromptAdmission {
            id: PromptId(900),
            generation: PromptGeneration(900),
            origin: PromptOrigin::Direct,
        },
        prompt: "Use the background result".into(),
    });
    harness.until("prompt with completion", |seen| {
        seen.iter()
            .any(|e| matches!(e, UiEvent::PromptRunCompleted { .. }))
    });
    let body = provider
        .bodies
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(body.matches("[Shell job completion:").count(), 1, "{body}");
    assert!(body.contains("IDLE-DONE"), "{body}");
    harness.wait_idle();
    let id = jsonl_ids(&ws.sessions()).into_iter().next().unwrap();
    let metadata = super::session_job_metadata(&preflight_session(ws.session_path(&id)).unwrap());
    assert_eq!(metadata[0].state, "completed");
}

#[test]
fn session_switch_stops_background_jobs_and_preserves_terminal_metadata() {
    let ws = Workspace::new("jobs-switch");
    let current = create_session(&ws, "tui-current", &["current"]);
    create_session(&ws, "tui-next", &["next"]);
    let mut harness = start(&ws, Some(&current), None);
    let command = if cfg!(windows) {
        "Write-Output 'READY'; while (!(Test-Path 'release-job')) { Start-Sleep -Milliseconds 20 }; Set-Content -LiteralPath escaped 'bad'"
    } else {
        "printf 'READY\n'; while [ ! -f release-job ]; do sleep 0.02; done; printf bad > escaped"
    };
    harness.send(UiCommand::RunBackgroundShell {
        command: command.into(),
    });
    harness.until("job start", |seen| {
        seen.iter().any(
            |e| matches!(e,UiEvent::JobsChanged{jobs} if jobs.iter().any(|j|j.state=="running")),
        )
    });
    let mark = harness.seen.len();
    harness.send(UiCommand::ResumeSession {
        id: "tui-next".into(),
    });
    harness.until("session switch", |seen| {
        restored_after(seen, mark).is_some()
    });
    harness.wait_idle();
    let metadata = super::session_job_metadata(&preflight_session(&current).unwrap());
    assert_eq!(metadata[0].state, "cancelled");
    std::fs::write(ws.0.join("release-job"), "go").unwrap();
    assert!(!ws.0.join("escaped").exists());
}

#[test]
fn job_metadata_refusal_happens_before_prompt_run_started() {
    use slim_tui::api::{PromptAdmission, PromptGeneration, PromptId, PromptOrigin};
    let ws = Workspace::new("job-refusal");
    let current = create_session(&ws, "tui-current", &["current"]);
    let provider = provider(1, false);
    let mut harness = start(&ws, Some(&current), Some(&provider.endpoint));
    let _held = JsonlRepo::open_no_repair(&current).unwrap();
    harness.send(UiCommand::RunBackgroundShell {
        command: if cfg!(windows) {
            "Write-Output done"
        } else {
            "printf done"
        }
        .into(),
    });
    harness.until("job start", |seen| {
        seen.iter()
            .any(|e| matches!(e,UiEvent::JobsChanged{jobs} if !jobs.is_empty()))
    });
    let mark = harness.seen.len();
    harness.send(UiCommand::PreparePrompt {
        admission: PromptAdmission {
            id: PromptId(800),
            generation: PromptGeneration(800),
            origin: PromptOrigin::Direct,
        },
        prompt: "new prompt".into(),
    });
    harness.until("metadata refusal",|seen|seen[mark..].iter().any(|e|matches!(e,UiEvent::PromptPreparationFailed{message,..} if message.contains("Metadados"))));
    assert!(
        !harness.seen[mark..]
            .iter()
            .any(|e| matches!(e, UiEvent::PromptRunStarted { .. })),
        "{:?}",
        &harness.seen[mark..]
    );
    assert!(provider.bodies.try_recv().is_err());
}
