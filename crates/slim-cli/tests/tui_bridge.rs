use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use slim_cli::{
    spawn_tui_runtime, spawn_tui_runtime_with_resume, ProviderRequest, ProviderRunOptions,
};
use slim_core::context::{
    AdaptiveTokenEstimator, CompactionHandle, CompactionPolicy, CompactionStrategy,
};
use slim_core::provider::{
    ProviderKind, ProviderMessage, ProviderToolCall, CODEX_BUNDLED_CONTEXT_WINDOW,
};
use slim_core::runtime::CancellationToken;
use slim_core::session::{
    preflight_session, DurableEntry, DurableEntryRole, DurableOperation, DurableOperationKind,
    DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo, SessionWriter,
};
use slim_core::{
    EventKind, OperatingMode, QuestionAnswer, SessionEvent, SessionEventReceiver,
    SessionEventSender,
};
use slim_tui::api::{InteractionRequestId, ModelAlias, ReasoningEffort, UiCommand, UiEvent};
use slim_tui::app::{AppState, FollowMode, ScrollAnchor};
use slim_tui::block::{BlockKind, BlockLifecycle};
use slim_tui::reducer::{reduce, Action, Effect};

fn request(endpoint: String) -> ProviderRequest {
    ProviderRequest {
        prompt: String::new(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "tui-bridge-fixture".into(),
        api_key: "tui-bridge-secret".into(),
        account_id: None,
        timeout: Duration::from_secs(2),
    }
}

fn read_complete_http_request(stream: &mut std::net::TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    let expected_len = loop {
        let size = stream.read(&mut chunk).expect("request");
        assert!(size > 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..size]);
        let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_len = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length: ")
                    .or_else(|| line.strip_prefix("Content-Length: "))
            })
            .expect("content length")
            .parse::<usize>()
            .expect("content length number");
        break header_end + 4 + content_len;
    };
    while bytes.len() < expected_len {
        let size = stream.read(&mut chunk).expect("request body");
        assert!(size > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..size]);
    }
    String::from_utf8_lossy(&bytes[..expected_len]).into_owned()
}

#[test]
fn manual_retry_command_resumes_the_paused_request_without_another_user_message() {
    assert_manual_retry_round_trip(false);
}

#[test]
fn manual_retry_preserves_a_durable_session_without_duplicate_input() {
    assert_manual_retry_round_trip(true);
}

fn assert_manual_retry_round_trip(durable: bool) {
    let path = durable.then(|| resume_path("manual-retry"));
    if let Some(path) = &path {
        create_v2(path);
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut bodies = Vec::new();
        for index in 0..4 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "retry fixture timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let request = read_complete_http_request(&mut stream);
            bodies.push(request.split_once("\r\n\r\n").unwrap().1.to_owned());
            let (status, body) = if index < 3 {
                ("503 Service Unavailable", "unavailable")
            } else {
                ("200 OK", "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            };
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        bodies
    });
    let options = ProviderRunOptions::default().with_context_window_tokens(32_000);
    let (runtime, channels) = if let Some(path) = &path {
        spawn_tui_runtime_with_resume(
            request(format!("http://{address}")),
            path,
            options.with_workspace_root(path.parent().unwrap()),
        )
    } else {
        spawn_tui_runtime(request(format!("http://{address}")), options)
    }
    .unwrap();
    channels
        .commands
        .send(UiCommand::SendPrompt("hello".into()))
        .unwrap();
    let mut requested = false;
    let mut user_messages = 0;
    loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(6))
            .expect("retry event");
        match event {
            UiEvent::UserMessageAdded { .. } => user_messages += 1,
            UiEvent::ProviderPhaseChanged { label, .. } if label.contains("Conexão pausada") => {
                assert!(!requested);
                channels.commands.send(UiCommand::RetryProvider).unwrap();
                requested = true;
            }
            UiEvent::RunCompleted { run_id: 1 } => break,
            UiEvent::RunFailed { message, .. } => panic!("unexpected failure: {message}"),
            _ => {}
        }
    }
    assert!(requested);
    assert_eq!(user_messages, 1);
    channels.commands.send(UiCommand::Shutdown).unwrap();
    drop(runtime);
    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 4);
    assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
    if let Some(path) = path {
        let report = preflight_session(&path).expect("valid durable session after retry");
        let inputs = report
            .records
            .iter()
            .filter(|record| {
                matches!(record,
                    DurableRecord::Entry { entry, .. }
                    if entry.role == DurableEntryRole::User && entry.content == "hello"
                )
            })
            .count();
        assert_eq!(inputs, 1, "retry must not append a second durable input");
        assert!(report
            .summary
            .terminal_operation_ids
            .iter()
            .any(|id| id.starts_with("resume-tui-resume-")));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

#[cfg(windows)]
fn working_set_bytes() -> Option<usize> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = unsafe { std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>() };
    counters.cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?;
    let read = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (read != 0).then_some(counters.WorkingSetSize)
}

fn median_working_set_bytes() -> Option<usize> {
    let _ = working_set_bytes();
    let mut samples = [
        working_set_bytes()?,
        working_set_bytes()?,
        working_set_bytes()?,
    ];
    samples.sort_unstable();
    Some(samples[1])
}

#[cfg(not(windows))]
fn working_set_bytes() -> Option<usize> {
    None
}

#[test]
fn bounded_core_queue_applies_backpressure_at_capacity() {
    const DELTAS: usize = 4_096;
    const PAYLOAD_BYTES: usize = 1_024;
    const CAPACITY: usize = 64;
    const MAX_COALESCED_BYTES: usize = 64 * 1_024;

    fn drain_batch(
        receiver: &SessionEventReceiver,
        max_queue_string_capacity_bytes: &mut usize,
        received: &mut Vec<SessionEvent>,
    ) {
        let batch = receiver.try_iter().collect::<Vec<_>>();
        let batch_capacity = batch
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AssistantTextDelta { text } => Some(text.capacity()),
                _ => None,
            })
            .sum::<usize>();
        *max_queue_string_capacity_bytes = (*max_queue_string_capacity_bytes).max(batch_capacity);
        received.extend(batch);
    }

    let measured_at = Instant::now();
    let (sender, receiver) = SessionEventSender::bounded(CAPACITY, CancellationToken::new());
    let rss_before = median_working_set_bytes();
    let mut observed_high_water = 0usize;
    let mut max_queue_string_capacity_bytes = 0usize;
    let mut full_rejections = 0usize;
    let mut rss_at_high_water = None;
    let mut received = Vec::with_capacity(DELTAS + 1);
    let mut corpus = Vec::with_capacity(DELTAS + 1);

    for index in 0..DELTAS {
        let prefix = format!("{index:08}:");
        let payload = format!("{prefix}{}", "x".repeat(PAYLOAD_BYTES - prefix.len()));
        corpus.push(SessionEvent::new(
            index as u64 + 1,
            EventKind::AssistantTextDelta { text: payload },
        ));
    }
    corpus.push(SessionEvent::new(
        DELTAS as u64 + 1,
        EventKind::AssistantEnded {
            reason: "stop".into(),
        },
    ));

    for mut event in corpus {
        loop {
            match sender.try_send(event) {
                Ok(()) => {
                    observed_high_water = observed_high_water.max(sender.stats().high_watermark);
                    break;
                }
                Err(mpsc::TrySendError::Full(pending)) => {
                    full_rejections = full_rejections.saturating_add(1);
                    rss_at_high_water.get_or_insert_with(median_working_set_bytes);
                    drain_batch(
                        &receiver,
                        &mut max_queue_string_capacity_bytes,
                        &mut received,
                    );
                    event = pending;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => panic!("queue disconnected"),
            }
        }
    }
    drain_batch(
        &receiver,
        &mut max_queue_string_capacity_bytes,
        &mut received,
    );

    let logical_payload_bytes = received
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::AssistantTextDelta { text } => Some(text.len()),
            _ => None,
        })
        .sum::<usize>();
    let terminal_count = received
        .iter()
        .filter(|event| matches!(event.kind, EventKind::AssistantEnded { .. }))
        .count();
    let terminal_loss = usize::from(terminal_count != 1);
    let rss_delta = rss_before
        .zip(rss_at_high_water.flatten())
        .map(|(before, after)| {
            i128::try_from(after).expect("working set fits i128")
                - i128::try_from(before).expect("working set fits i128")
        });

    let expected_delta_events = (DELTAS * PAYLOAD_BYTES).div_ceil(MAX_COALESCED_BYTES);
    assert_eq!(received.len(), expected_delta_events + 1);
    assert_eq!(receiver.stats().queued, 0);
    assert_eq!(observed_high_water, CAPACITY);
    assert_eq!(full_rejections, 1);
    assert_eq!(logical_payload_bytes, DELTAS * PAYLOAD_BYTES);
    assert!(max_queue_string_capacity_bytes <= CAPACITY * MAX_COALESCED_BYTES);
    let mut delta_index = 0usize;
    for (event_index, event) in received.iter().take(expected_delta_events).enumerate() {
        let EventKind::AssistantTextDelta { text } = &event.kind else {
            panic!("event {event_index} is not a delta");
        };
        assert!(text.len() <= MAX_COALESCED_BYTES);
        for payload in text.as_bytes().as_chunks::<PAYLOAD_BYTES>().0 {
            let prefix = format!("{delta_index:08}:");
            assert_eq!(&payload[..prefix.len()], prefix.as_bytes());
            assert!(payload[prefix.len()..].iter().all(|byte| *byte == b'x'));
            delta_index += 1;
        }
        assert_eq!(event.seq, delta_index as u64);
    }
    assert_eq!(delta_index, DELTAS);
    assert_eq!(
        received.last().map(|event| event.seq),
        Some(DELTAS as u64 + 1)
    );
    assert_eq!(terminal_count, 1);
    assert_eq!(terminal_loss, 0);
    eprintln!(
        "SLIM_QUEUE_SLICE7 deltas={DELTAS} payload_bytes={PAYLOAD_BYTES} capacity={CAPACITY} \
         core_queue_high_water={observed_high_water} logical_payload_bytes={logical_payload_bytes} \
         queue_string_capacity_bytes={max_queue_string_capacity_bytes} full_rejections={full_rejections} \
         terminal_loss={terminal_loss} working_set_delta_bytes={} wall_time_us={}",
        rss_delta.map_or_else(|| "unavailable".into(), |value| value.to_string()),
        measured_at.elapsed().as_micros(),
    );
}

fn resume_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!(
            "slim-tui-resume-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
        .join("session.jsonl")
}

#[test]
fn tool_output_pages_from_bridge_through_reducer_into_inline_frame() {
    let root = resume_path("tool-pages").with_extension("workspace");
    fs::create_dir_all(&root).expect("workspace");
    let source = root.join("large.txt");
    fs::write(&source, format!("page-prefix-{}", "界".repeat(12_000))).expect("fixture");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let source_arg = "large.txt".to_owned();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = vec![0_u8; 128 * 1024];
            let _ = stream.read(&mut request).expect("request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .expect("headers");
            if turn == 0 {
                let event = json!({
                    "choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": "page-call",
                        "function": {
                            "name": "read",
                            "arguments": json!({"path": source_arg, "max_lines": 10}).to_string()
                        }
                    }]}}]
                });
                stream
                    .write_all(format!("data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n").as_bytes())
                    .expect("tool response");
            } else {
                stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                    .expect("final response");
            }
        }
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_workspace_root(&root),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("read large output".into()))
        .expect("prompt");

    let mut state = AppState::new();
    loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(3))
            .expect("stream event");
        let complete = event == UiEvent::RunCompleted { run_id: 1 };
        reduce(&mut state, Action::UiEventReceived(event));
        if complete {
            break;
        }
    }
    let tool_id = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Tool(_)))
        .expect("tool block")
        .id
        .clone();
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: tool_id.clone(),
        row_offset: 0,
    });
    let effects = reduce(&mut state, Action::ToggleBlock(tool_id));
    let command = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Send(command @ UiCommand::RequestContentPage { .. }) => Some(command),
            _ => None,
        })
        .expect("page request");
    channels.commands.send(command).expect("request page");
    let page = loop {
        let event = channels
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("page event");
        if matches!(event, UiEvent::ContentPageLoaded { .. }) {
            break event;
        }
        reduce(&mut state, Action::UiEventReceived(event));
    };
    reduce(&mut state, Action::UiEventReceived(page));
    let frame = slim_tui::testkit::render_terminal_text(&state, 120, 30);
    assert!(frame.contains("page-prefix"), "{frame}");

    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
    let _ = fs::remove_dir_all(root);
}
#[test]
fn list_cursor_survives_the_next_tui_prompt() {
    let root = resume_path("shared-list-cursor").with_extension("workspace");
    fs::create_dir_all(&root).expect("workspace");
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(root.join(name), name).expect("fixture");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let mut first_cursor = String::new();
        for request_index in 0..4 {
            let (mut stream, _) = listener.accept().expect("accept");
            let body = read_complete_http_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .expect("headers");
            match request_index {
                0 => {
                    let event = json!({
                        "choices": [{"delta": {"tool_calls": [{
                            "index": 0,
                            "id": "cursor-first",
                            "function": {
                                "name": "list",
                                "arguments": json!({"path": ".", "max_entries": 1}).to_string()
                            }
                        }]}}]
                    });
                    stream
                        .write_all(format!("data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n").as_bytes())
                        .expect("first tool response");
                }
                1 => {
                    let start = body.rfind("list-").expect("cursor in first tool output");
                    first_cursor = body[start..]
                        .split(|character: char| {
                            !(character.is_ascii_alphanumeric()
                                || character == '-'
                                || character == ':')
                        })
                        .next()
                        .expect("cursor token")
                        .to_owned();
                    stream
                        .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"first done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                        .expect("first final response");
                }
                2 => {
                    assert!(body.contains(&first_cursor), "history lost cursor: {body}");
                    let event = json!({
                        "choices": [{"delta": {"tool_calls": [{
                            "index": 0,
                            "id": "cursor-next",
                            "function": {
                                "name": "list",
                                "arguments": json!({
                                    "path": ".",
                                    "max_entries": 1,
                                    "cursor": first_cursor
                                })
                                .to_string()
                            }
                        }]}}]
                    });
                    stream
                        .write_all(format!("data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n").as_bytes())
                        .expect("second tool response");
                }
                3 => {
                    assert!(
                        !body.contains("list cursor expired or was invalidated"),
                        "cursor cache was lost between TUI prompts: {body}"
                    );
                    assert!(body.contains("showing entries 2-2"), "wrong page: {body}");
                    stream
                        .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"second done\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                        .expect("second final response");
                }
                _ => unreachable!(),
            }
        }
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_workspace_root(&root),
    )
    .expect("bridge");
    for (run_id, prompt) in [(1, "first page"), (2, "next page")] {
        channels
            .commands
            .send(UiCommand::SendPrompt(prompt.into()))
            .expect("prompt");
        loop {
            let event = channels
                .events_data
                .recv_timeout(Duration::from_secs(5))
                .expect("stream event");
            if event == (UiEvent::RunCompleted { run_id }) {
                break;
            }
        }
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn new_tui_emits_workspace_before_other_startup_state() {
    let workspace = std::path::PathBuf::from(r"D:\Slim");
    let mut initial = request("http://127.0.0.1:9".into());
    initial.mode = OperatingMode::ReadOnly;
    let (runtime, channels) = spawn_tui_runtime(
        initial,
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_workspace_root(workspace),
    )
    .expect("bridge");

    assert!(matches!(
        channels
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("workspace event"),
        UiEvent::WorkspaceChanged { ref cwd, .. } if cwd == r"D:\Slim"
    ));
    assert_eq!(
        channels
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("initial mode"),
        UiEvent::ModeChanged {
            mode: OperatingMode::ReadOnly,
        },
    );

    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
}

#[test]
fn unbound_interaction_response_is_visibly_rejected() {
    let (runtime, channels) = spawn_tui_runtime(
        request("http://127.0.0.1:9".into()),
        ProviderRunOptions::default().with_context_window_tokens(32_000),
    )
    .expect("bridge");
    let request_id = InteractionRequestId("approval-unbound".into());
    channels
        .commands
        .send(UiCommand::Reject {
            request_id: request_id.clone(),
        })
        .expect("reject");

    assert_eq!(
        channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("interaction acknowledgement"),
        UiEvent::InteractionAcknowledged {
            request_id,
            accepted: false,
            message: "interaction route unavailable in this host".into(),
        }
    );

    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
}

fn create_v2(path: &std::path::Path) {
    let mut repo = JsonlRepo::create(
        path,
        DurableSessionHeader::new(
            "tui-resume",
            "now",
            path.parent().unwrap().to_str().unwrap(),
            None,
            None,
        ),
    )
    .expect("v2 session");
    repo.append(DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "seed-input".into(),
            role: DurableEntryRole::User,
            content: "seed".into(),
            parent_entry_id: None,
            operation_id: "seed-op".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("seed entry");
    repo.append(DurableRecord::Operation {
        seq: 1,
        operation: DurableOperation {
            operation_id: "seed-op".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "seed-input".into(),
            },
        },
    })
    .expect("seed operation");
    repo.append(DurableRecord::Operation {
        seq: 2,
        operation: DurableOperation {
            operation_id: "seed-op".into(),
            kind: DurableOperationKind::Finished {
                outcome: slim_core::session::DurableOutcome::Success,
            },
        },
    })
    .expect("seed terminal");
}

#[test]
fn tui_resume_streams_fixture_once_after_durable_prefix() {
    let path = resume_path("stream");
    create_v2(&path);
    let workspace = path.parent().expect("workspace").to_path_buf();
    let skill_dir = workspace.join(".slim/skills/review-code");
    fs::create_dir_all(&skill_dir).expect("skill directory");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review-code\ndescription: Review code\n---\nresume-skill-body-marker\n",
    )
    .expect("skill fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let inspected_path = path.clone();
    let (request_tx, request_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = read_complete_http_request(&mut stream);
        request_tx.send(request).expect("request body");
        let prefix = fs::read(&inspected_path).expect("durable prefix");
        let prefix = String::from_utf8_lossy(&prefix);
        assert!(
            prefix.contains("provider_attempt_started"),
            "provider must observe the causal pre-effect prefix"
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"resumed tui\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("events");
    });

    let (runtime, channels) = spawn_tui_runtime_with_resume(
        request(format!("http://{address}")),
        &path,
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_workspace_root(&workspace),
    )
    .expect("resume bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt(
            "/review-code continue from tui".into(),
        ))
        .expect("prompt");
    let provider_request = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("provider request");
    assert!(
        provider_request.contains("resume-skill-body-marker"),
        "{provider_request}"
    );

    let mut text = String::new();
    while text != "resumed tui" {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("assistant delta");
        if let UiEvent::AssistantDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }
    let mut completed = false;
    while !completed {
        completed = channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("run completion")
            == UiEvent::RunCompleted { run_id: 1 };
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");

    let report = preflight_session(&path).expect("reopen");
    let seqs: Vec<_> = report.records.iter().map(DurableRecord::seq).collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    assert!(report
        .summary
        .terminal_operation_ids
        .iter()
        .any(|id| id.starts_with("resume-tui-resume-")));
    assert!(
        !fs::read_to_string(&path)
            .expect("durable session")
            .contains("resume-skill-body-marker"),
        "skill instructions must not be persisted"
    );
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn tui_resume_rejects_v1_before_starting_the_bridge() {
    let path = resume_path("v1");
    let mut writer = SessionWriter::create(&path, "legacy", "D:\\Slim").expect("v1");
    writer
        .append(&slim_core::SessionEvent::new(
            1,
            slim_core::EventKind::SessionStarted {
                session_id: "legacy".into(),
            },
        ))
        .expect("event");
    drop(writer);

    let error = match spawn_tui_runtime_with_resume(
        request("http://127.0.0.1:1".into()),
        &path,
        ProviderRunOptions::default().with_context_window_tokens(32_000),
    ) {
        Ok(_) => panic!("legacy v1 must reject"),
        Err(error) => error,
    };
    assert!(format!("{error:?}").contains("durable schema v2"));
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn tui_resume_rejects_torn_tail_before_starting_the_bridge() {
    let path = resume_path("torn");
    create_v2(&path);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open");
    file.write_all(br#"{"type":"operation""#)
        .expect("torn tail");
    drop(file);

    let error = match spawn_tui_runtime_with_resume(
        request("http://127.0.0.1:1".into()),
        &path,
        ProviderRunOptions::default().with_context_window_tokens(32_000),
    ) {
        Ok(_) => panic!("torn tail must reject"),
        Err(error) => error,
    };
    assert!(format!("{error:?}").contains("explicit recovery"));
    let _ = fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn bridge_streams_first_delta_before_provider_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        let delta = json!({"choices":[{"delta":{"content":"live"}}]});
        stream
            .write_all(format!("data: {delta}\n\n").as_bytes())
            .expect("delta");
        stream.flush().expect("flush");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release completion");
        stream
            .write_all(
                b"data: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("completion");
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_context_window_tokens(64_000),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("hello".into()))
        .expect("prompt");

    let mut saw_estimate = false;
    let mut saw_live_delta = false;
    while !saw_live_delta {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(1))
            .expect("live event before completion");
        match event {
            UiEvent::UsageEstimate {
                context_tokens,
                context_window_tokens: 64_000,
                ..
            }
            | UiEvent::UsageEstimateForRun {
                context_tokens,
                context_window_tokens: 64_000,
                ..
            } => saw_estimate = context_tokens > 0,
            UiEvent::AssistantDelta { text } if text == "live" => saw_live_delta = true,
            _ => {}
        }
    }
    assert!(saw_estimate, "estimate must precede provider completion");
    release_tx.send(()).expect("release server");

    let mut saw_usage = false;
    while !saw_usage {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(1))
            .expect("completion accounting event");
        saw_usage = matches!(
            event,
            UiEvent::Usage {
                input_tokens: 2,
                output_tokens: 1
            }
        );
    }
    let mut completed = false;
    while !completed {
        completed = channels
            .events_data
            .recv_timeout(Duration::from_secs(1))
            .expect("run completion")
            == UiEvent::RunCompleted { run_id: 1 };
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn fast_stream_over_data_capacity_drains_before_completion_without_hanging() {
    const DELTAS: usize = 1_200;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        for _ in 0..DELTAS {
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
                .expect("delta");
        }
        stream
            .write_all(
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("completion");
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default().with_context_window_tokens(32_000),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("pressure".into()))
        .expect("prompt");
    // Let the projector fill the bounded lane before the consumer starts
    // draining it. A blocking join would deadlock at this point.
    thread::sleep(Duration::from_millis(100));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    let mut data_events = Vec::new();
    let mut completed = false;
    while Instant::now() < deadline && (!completed || text.len() < DELTAS) {
        while let Ok(event) = channels.events_data.try_recv() {
            if let UiEvent::AssistantDelta { text: delta } = &event {
                text.push_str(delta);
            }
            completed |= event == UiEvent::RunCompleted { run_id: 1 };
            data_events.push(event);
        }
        while channels.events.try_recv().is_ok() {}
        if !completed || text.len() < DELTAS {
            thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(completed, "completion did not arrive before timeout");
    assert_eq!(text, "x".repeat(DELTAS));
    let mut state = AppState::new();
    for event in data_events {
        reduce(&mut state, Action::UiEventReceived(event));
    }
    assert!(matches!(
        state.blocks().last(),
        Some(block)
            if block.lifecycle == BlockLifecycle::Complete
                && block.kind() == &BlockKind::Assistant("x".repeat(DELTAS))
    ));
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn cancel_bypasses_a_full_stream_lane_without_consumer_drain() {
    const DELTAS: usize = 1_200;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        accepted_tx.send(()).expect("accepted");
        for _ in 0..DELTAS {
            if stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
                .is_err()
            {
                return;
            }
        }
        release_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("cancel observed before fixture release");
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default().with_context_window_tokens(32_000),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("cancel pressure".into()))
        .expect("prompt");
    accepted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("request accepted");
    thread::sleep(Duration::from_millis(100));
    channels
        .commands
        .send(UiCommand::CancelRun)
        .expect("cancel");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut cancelled = false;
    while Instant::now() < deadline && !cancelled {
        while let Ok(event) = channels.events.try_recv() {
            cancelled |= event == UiEvent::RunCancelled { run_id: 1 };
        }
        if !cancelled {
            thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(cancelled, "cancel did not arrive before timeout");
    release_tx.send(()).expect("release fixture");
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn model_alias_updates_next_codex_request_without_restarting_tui() {
    let config_path = std::env::temp_dir().join(format!(
        "slim-tui-bridge-config-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let previous_config = std::env::var_os("SLIM_CONFIG_FILE");
    std::env::set_var("SLIM_CONFIG_FILE", &config_path);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for fast in [true, false] {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_complete_http_request(&mut stream);
            assert!(request.starts_with("POST /codex/responses "));
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").expect("headers").1)
                    .expect("request JSON");
            assert_eq!(body["model"], "gpt-6-astra");
            assert_eq!(body["reasoning"]["effort"], "max");
            assert_eq!(body.get("service_tier"), fast.then_some(&json!("priority")));
            assert!(!body["tools"].as_array().expect("tools").is_empty());
            let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).as_bytes()).expect("response");
        }
    });
    let mut model_request = request(format!("http://{address}"));
    model_request.kind = ProviderKind::OpenAiCodex;
    model_request.model = "gpt-5.6-sol".into();
    model_request.account_id = Some("account-1".into());
    let (runtime, channels) = spawn_tui_runtime(
        model_request,
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_max_output_tokens(1_024),
    )
    .expect("bridge");
    for (index, fast) in [true, false].into_iter().enumerate() {
        channels
            .commands
            .send(UiCommand::SetModel {
                model: ModelAlias::Astra,
                effort: ReasoningEffort::Max,
                fast,
            })
            .expect("model");
        loop {
            if channels
                .events
                .recv_timeout(Duration::from_secs(2))
                .expect("model event")
                == (UiEvent::ModelChanged {
                    model: "gpt-6-astra".into(),
                })
            {
                break;
            }
        }
        if index == 0 {
            assert!(
                !config_path.exists(),
                "session model must not create defaults"
            );
        } else {
            let previous: toml::Value = fs::read_to_string(&config_path).unwrap().parse().unwrap();
            assert_eq!(
                previous["codex_fast"].as_bool(),
                Some(true),
                "session switch must not overwrite defaults"
            );
        }
        channels
            .commands
            .send(UiCommand::SaveModelDefault)
            .expect("save default");
        loop {
            if matches!(channels.events_data.recv_timeout(Duration::from_secs(2)).expect("saved event"),
                UiEvent::Notification { message } if message.contains("salvos como padrão"))
            {
                break;
            }
        }
        let saved: toml::Value = fs::read_to_string(&config_path)
            .expect("persisted config")
            .parse()
            .expect("config TOML");
        assert_eq!(saved["model"].as_str(), Some("gpt-6-astra"));
        assert_eq!(saved["effort"].as_str(), Some("max"));
        assert_eq!(saved["codex_fast"].as_bool(), Some(fast));
        channels
            .commands
            .send(UiCommand::SendPrompt("hello".into()))
            .expect("prompt");
        loop {
            match channels
                .events_data
                .recv_timeout(Duration::from_secs(3))
                .expect("completion")
            {
                UiEvent::RunCompleted { run_id } if run_id == index as u64 + 1 => break,
                UiEvent::RunFailed { message, .. } => panic!("model run failed: {message}"),
                _ => {}
            }
        }
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
    match previous_config {
        Some(value) => std::env::set_var("SLIM_CONFIG_FILE", value),
        None => std::env::remove_var("SLIM_CONFIG_FILE"),
    }
    let _ = fs::remove_file(config_path);
}

#[test]
fn codex_run_uses_bundled_catalog_context_window_instead_of_loop_default() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request);
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("response");
    });
    let mut model_request = request(format!("http://{address}"));
    model_request.kind = ProviderKind::OpenAiCodex;
    model_request.model = "gpt-5.6-sol".into();
    model_request.account_id = Some("account-1".into());
    let (runtime, channels) =
        spawn_tui_runtime(model_request, ProviderRunOptions::default()).expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("hello".into()))
        .expect("prompt");

    let mut saw_catalog_window = false;
    loop {
        match channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("codex event")
        {
            UiEvent::UsageEstimate {
                context_window_tokens,
                ..
            }
            | UiEvent::UsageEstimateForRun {
                context_window_tokens,
                ..
            } => {
                assert_eq!(context_window_tokens, CODEX_BUNDLED_CONTEXT_WINDOW);
                assert_ne!(context_window_tokens, 32_000);
                saw_catalog_window = true;
            }
            UiEvent::RunCompleted { run_id: 1 } => break,
            UiEvent::RunFailed { message, .. } => panic!("Codex run failed: {message}"),
            _ => {}
        }
    }
    assert!(
        saw_catalog_window,
        "Codex run must publish the bundled catalog window"
    );
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn known_provider_models_use_their_catalog_context_windows() {
    for (kind, model, expected_window) in [
        (ProviderKind::ClinePass, "cline-pass/qwen3.7-max", 1_000_000),
        (
            ProviderKind::CommandCode,
            "deepseek/deepseek-v4.1-flash",
            1_000_000,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; 32 * 1024];
            let _ = stream.read(&mut request).expect("request");
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("response");
        });
        let mut model_request = request(format!("http://{address}"));
        model_request.kind = kind;
        model_request.model = model.into();
        let (runtime, channels) =
            spawn_tui_runtime(model_request, ProviderRunOptions::default()).expect("bridge");
        channels
            .commands
            .send(UiCommand::SendPrompt("hello".into()))
            .expect("prompt");

        let mut observed = None;
        let mut completed = false;
        while !completed {
            match channels
                .events_data
                .recv_timeout(Duration::from_secs(2))
                .expect("provider event")
            {
                UiEvent::UsageEstimate {
                    context_window_tokens,
                    ..
                }
                | UiEvent::UsageEstimateForRun {
                    context_window_tokens,
                    ..
                } => observed = Some(context_window_tokens),
                UiEvent::RunCompleted { run_id: 1 } => completed = true,
                UiEvent::RunFailed { message, .. } => panic!("provider run failed: {message}"),
                _ => {}
            }
        }
        assert_eq!(observed, Some(expected_window), "kind={kind:?}");
        channels
            .commands
            .send(UiCommand::Shutdown)
            .expect("shutdown");
        drop(runtime);
        server.join().expect("server");
    }
}

#[test]
fn bounded_agent_stop_is_not_reported_as_completed() {
    let (runtime, channels) = spawn_tui_runtime(
        request("http://127.0.0.1:1".into()),
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_max_turns(0),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("bounded".into()))
        .expect("prompt");

    let mut stopped = false;
    while !stopped {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(1))
            .expect("bounded stop");
        assert_ne!(event, UiEvent::RunCompleted { run_id: 1 });
        stopped = matches!(event, UiEvent::RunStopped { .. });
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
}

#[test]
fn cancel_run_kills_active_shell_before_late_workspace_mutation() {
    cancel_active_shell_before_late_workspace_mutation(false);
}

#[test]
fn cancel_run_double_escape_kills_active_shell_before_late_workspace_mutation() {
    cancel_active_shell_before_late_workspace_mutation(true);
}

fn cancel_active_shell_before_late_workspace_mutation(force: bool) {
    let root = std::env::temp_dir().join(format!(
        "slim-tui-shell-cancel-{}-{force}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let marker = root.join("late.txt");
    let started = root.join("started.txt");
    let command = format!(
        "Set-Content -LiteralPath '{}' -Value started; Start-Sleep -Seconds 30; Set-Content -LiteralPath '{}' -Value late",
        started.display(),
        marker.display()
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request);
        let arguments = serde_json::json!({"command": command, "timeout_ms": 60_000}).to_string();
        let event = json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "shell-1",
                "function": {"name": "shell", "arguments": arguments}
            }]}}]
        });
        let body = format!(
            "data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("response");
    });
    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_workspace_root(&root),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("run shell".into()))
        .expect("prompt");
    loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("tool start");
        if matches!(event, UiEvent::ToolStarted { ref name, .. } if name == "shell") {
            break;
        }
    }
    // ToolStarted precedes spawn_blocking. Confirm the process itself is live
    // so this exercises native cancellation rather than pre-execution abort.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(started.exists(), "shell did not start");
    channels
        .commands
        .send(UiCommand::CancelRun)
        .expect("cancel");
    if force {
        channels
            .commands
            .send(UiCommand::CancelRun)
            .expect("force cancel");
    }
    loop {
        if channels
            .events
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel event")
            == (UiEvent::RunCancelled { run_id: 1 })
        {
            break;
        }
    }
    thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists());
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
    let _ = fs::remove_dir_all(root);
}

#[cfg(windows)]
#[test]
fn cancel_locked_write_and_patch_preserves_file_and_durable_result() {
    for name in ["write", "patch"] {
        let session = resume_path(&format!("locked-{name}"));
        let root = session.parent().unwrap();
        fs::create_dir_all(root).unwrap();
        create_v2(&session);
        let target = root.join("state.txt");
        fs::write(&target, "before").unwrap();
        let locked = fs::File::open(&target).unwrap();
        locked.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_complete_http_request(&mut stream);
            let arguments = if name == "write" {
                json!({"path":"state.txt","expected":"before","content":"after"})
            } else {
                json!({"path":"state.txt","expected":"before","replacement":"after"})
            };
            let chunk = json!({"choices":[{"delta":{"tool_calls":[{
                "index":0,"id":"locked-mutation","function":{"name":name,"arguments":arguments.to_string()}
            }]},"finish_reason":"tool_calls"}]});
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let (runtime, channels) = spawn_tui_runtime_with_resume(
            request(format!("http://{address}")),
            &session,
            ProviderRunOptions::default()
                .with_context_window_tokens(32_000)
                .with_workspace_root(root),
        )
        .unwrap();
        channels
            .commands
            .send(UiCommand::SendPrompt("mutate state".into()))
            .unwrap();
        // The progress signal is emitted only after try_lock observes the
        // external lock; unlike a sharing probe it cannot interfere with open.
        let deadline = Instant::now() + Duration::from_secs(5);
        let reached_lock = loop {
            match channels
                .events_data
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(UiEvent::ToolProgress { preview, .. }) if preview == "Waiting for file lock" => {
                    break true
                }
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        channels.commands.send(UiCommand::CancelRun).unwrap();
        let cancelled = loop {
            match channels.events.recv_timeout(Duration::from_secs(5)) {
                Ok(UiEvent::RunCancelled { .. }) => break true,
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        // Keep the external lock until cancellation is acknowledged, exactly
        // as in the regression: a detached writer used to mutate after unlock.
        locked.unlock().unwrap();
        drop(locked);
        runtime.finish().unwrap();
        server.join().unwrap();
        assert!(reached_lock, "tool did not signal its lock wait");
        assert!(
            cancelled,
            "tool did not acknowledge cancellation while locked"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
        let report = preflight_session(&session).unwrap();
        assert_eq!(report.summary.pending_count(), 0);
        let messages = slim_core::session::provider_messages_from_entries(
            report.records.iter().filter_map(|record| match record {
                DurableRecord::Entry { entry, .. } => Some(entry),
                _ => None,
            }),
        )
        .unwrap();
        let result = messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("locked-mutation"))
            .unwrap();
        assert!(
            result.content.contains("cancelled before side effect"),
            "{}",
            result.content
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn cancel_run_closes_provider_connection_and_returns_idle() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 32 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n")
            .expect("partial delta");
        stream.flush().expect("flush");
        accepted_tx.send(()).expect("accepted");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut byte = [0_u8; 1];
        assert_eq!(stream.read(&mut byte).expect("connection close"), 0);
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default().with_context_window_tokens(32_000),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("cancel me".into()))
        .expect("prompt");
    accepted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("request accepted");
    channels
        .commands
        .send(UiCommand::CancelRun)
        .expect("cancel");

    let mut cancelled = false;
    while !cancelled {
        let event = channels
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel event");
        cancelled = event == UiEvent::RunCancelled { run_id: 1 };
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    assert_eq!(
        channels
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown event"),
        UiEvent::Shutdown
    );
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn ordinary_tui_second_turn_sends_prior_user_and_assistant() {
    let root = resume_path("slash-skill-turns").with_extension("workspace");
    let skill_dir = root.join(".slim/skills/review-code");
    fs::create_dir_all(&skill_dir).expect("skill directory");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review-code\ndescription: Review code\n---\nturn-skill-body-marker\n",
    )
    .expect("skill fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (request_tx, request_rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let body = read_complete_http_request(&mut stream);
            request_tx.send(body).expect("request body");
            let answer = if turn == 0 {
                "first-answer-marker"
            } else {
                "second-answer-marker"
            };
            let events = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{answer}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                        events.len()
                    )
                    .as_bytes(),
                )
                .expect("response");
        }
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_workspace_root(&root),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt(
            "context /review-code first-user-prompt".into(),
        ))
        .expect("first prompt");
    let first = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first provider request");
    assert!(first.contains("turn-skill-body-marker"), "{first}");
    assert!(first.contains("[Skill: review-code]"), "{first}");
    let mut completed_first = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !completed_first {
        if let Ok(event) = channels
            .events_data
            .recv_timeout(Duration::from_millis(200))
        {
            completed_first = event == UiEvent::RunCompleted { run_id: 1 };
        }
        while let Ok(event) = channels.events.try_recv() {
            completed_first |= event == UiEvent::RunCancelled { run_id: 1 }
                || event == UiEvent::RunCompleted { run_id: 1 };
        }
    }
    assert!(completed_first, "first turn must complete");
    channels
        .commands
        .send(UiCommand::SendPrompt("second-user-prompt".into()))
        .expect("second prompt");
    let second = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second provider request");
    assert!(
        second.contains("context /review-code first-user-prompt"),
        "follow-up must include prior user text: {second}"
    );
    assert!(
        second.contains("first-answer-marker"),
        "follow-up must include prior assistant text: {second}"
    );
    assert!(
        second.contains("second-user-prompt"),
        "follow-up must include the new prompt: {second}"
    );
    assert!(
        !second.contains("turn-skill-body-marker") && !second.contains("[Skill: review-code]"),
        "skill instructions must be one-turn only: {second}"
    );
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn failed_nonpersistent_turn_keeps_completed_tool_evidence() {
    let root = resume_path("failed-history").with_extension("workspace");
    fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, request_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        for step in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let body = read_complete_http_request(&mut stream);
            if step == 2 {
                request_tx.send(body).unwrap();
            }
            if step == 1 {
                stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"error\":\"rejected\"}").unwrap();
            } else {
                let payload = if step == 0 {
                    json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"write-once","function":{
                        "name":"write","arguments":"{\"path\":\"done.txt\",\"content\":\"effect-marker\"}"}}]},"finish_reason":"tool_calls"}]})
                } else {
                    json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
                };
                let events = format!("data: {payload}\n\ndata: [DONE]\n\n");
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}", events.len()).as_bytes()).unwrap();
            }
        }
    });
    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default()
            .with_context_window_tokens(32_000)
            .with_workspace_root(&root),
    )
    .unwrap();
    channels
        .commands
        .send(UiCommand::SendPrompt("write once".into()))
        .unwrap();
    let mut failed = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !failed {
        if let Ok(event) = channels
            .events_data
            .recv_timeout(Duration::from_millis(100))
        {
            failed |= matches!(event, UiEvent::RunFailed { .. });
        }
        while let Ok(event) = channels.events.try_recv() {
            failed |= matches!(event, UiEvent::RunFailed { .. });
        }
    }
    assert!(failed, "first turn must fail after its tool completed");
    assert_eq!(
        fs::read_to_string(root.join("done.txt")).unwrap(),
        "effect-marker"
    );
    channels
        .commands
        .send(UiCommand::SendPrompt("continue".into()))
        .unwrap();
    let next = request_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        next.contains("write-once") && next.contains("written done.txt"),
        "{next}"
    );
    channels.commands.send(UiCommand::Shutdown).unwrap();
    drop(runtime);
    server.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fifty_turn_tui_soak_completes_without_stall_or_history_loss() {
    const TURNS: usize = 50;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (last_tx, last_rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        for turn in 0..TURNS {
            let (mut stream, _) = listener.accept().expect("accept");
            let body = read_complete_http_request(&mut stream);
            if turn + 1 == TURNS {
                last_tx.send(body).expect("last request body");
            }
            let answer = format!("soak-answer-{turn}");
            let events = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{answer}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                        events.len()
                    )
                    .as_bytes(),
                )
                .expect("response");
        }
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default().with_context_window_tokens(32_000),
    )
    .expect("bridge");
    let rss_before = median_working_set_bytes();
    let started = Instant::now();
    let mut turn_times = Vec::with_capacity(TURNS);
    for turn in 0..TURNS {
        let turn_started = Instant::now();
        channels
            .commands
            .send(UiCommand::SendPrompt(format!("soak-prompt-{turn}")))
            .expect("prompt");
        let expected = UiEvent::RunCompleted {
            run_id: turn as u64 + 1,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut completed = false;
        while Instant::now() < deadline && !completed {
            if let Ok(event) = channels
                .events_data
                .recv_timeout(Duration::from_millis(100))
            {
                completed = event == expected;
            }
            while let Ok(event) = channels.events.try_recv() {
                completed |= event == expected;
            }
        }
        assert!(completed, "soak turn {turn} stalled");
        turn_times.push(turn_started.elapsed().as_secs_f64() * 1_000.0);
    }
    let last = last_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("last provider request");
    assert!(last.contains("soak-prompt-0"));
    assert!(last.contains("soak-answer-0"));
    assert!(last.contains("soak-prompt-49"));
    assert!(
        last.len() < 1024 * 1024,
        "small soak request grew beyond 1 MiB"
    );

    turn_times.sort_by(f64::total_cmp);
    let p95 = turn_times[TURNS * 95 / 100];
    let rss_after = median_working_set_bytes();
    let rss_delta = rss_before.zip(rss_after).map(|(before, after)| {
        i128::try_from(after).expect("working set fits i128")
            - i128::try_from(before).expect("working set fits i128")
    });
    eprintln!(
        "SLIM_TUI_SOAK turns={TURNS} p95_ms={p95:.3} total_ms={:.3} last_request_bytes={} working_set_delta_bytes={}",
        started.elapsed().as_secs_f64() * 1_000.0,
        last.len(),
        rss_delta.map_or_else(|| "unavailable".into(), |value| value.to_string()),
    );

    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn ordinary_tui_reuses_compacted_history_on_the_next_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (bodies_tx, bodies_rx) = mpsc::channel::<String>();
    let server = thread::spawn(move || {
        for request_index in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            bodies_tx
                .send(read_complete_http_request(&mut stream))
                .expect("request body");
            let answer = if request_index == 0 {
                "first-answer-after-compaction"
            } else {
                "second-answer-without-recompaction"
            };
            let events = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{answer}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                        events.len()
                    )
                    .as_bytes(),
                )
                .expect("response");
        }
    });

    let oversized_marker = format!("ORIGINAL-OVERSIZED-HISTORY-{}", "x".repeat(2_000_000));
    let options = ProviderRunOptions::default()
        .with_context_window_tokens(32_000)
        .with_history(vec![
            ProviderMessage::user("small-root-instruction"),
            ProviderMessage::assistant(oversized_marker.clone(), Vec::new()),
        ])
        .with_context_window_tokens(512_000)
        .with_max_output_tokens(128);
    let (runtime, channels) =
        spawn_tui_runtime(request(format!("http://{address}")), options).expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("first-prompt".into()))
        .expect("first prompt");

    let first_provider_request = bodies_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first provider request");
    assert!(
        !first_provider_request.contains("Summarize the prior agent transcript"),
        "hard threshold without prepared must compact locally"
    );
    assert!(
        first_provider_request.contains("[Compacted context]"),
        "first turn must send the local compacted history"
    );
    assert!(first_provider_request.contains("first-prompt"));
    assert!(
        !first_provider_request.contains(&oversized_marker),
        "first turn must not resend the original oversized history"
    );
    let mut completed_first = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !completed_first {
        if let Ok(event) = channels
            .events_data
            .recv_timeout(Duration::from_millis(200))
        {
            completed_first = event == UiEvent::RunCompleted { run_id: 1 };
        }
        while let Ok(event) = channels.events.try_recv() {
            completed_first |= event == UiEvent::RunCompleted { run_id: 1 };
        }
    }
    assert!(completed_first, "first compacted turn must complete");

    channels
        .commands
        .send(UiCommand::SendPrompt("second-prompt".into()))
        .expect("second prompt");
    let second_turn_first_request = bodies_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second turn provider request");
    assert!(
        second_turn_first_request.contains("[Compacted context]"),
        "second turn must reuse the compacted history: {second_turn_first_request}"
    );
    assert!(
        !second_turn_first_request.contains(&oversized_marker),
        "second turn must not resend the original oversized history"
    );
    assert!(second_turn_first_request.contains("first-answer-after-compaction"));
    assert!(second_turn_first_request.contains("second-prompt"));

    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

const COMPACTION_FACTS: [&str; 6] = [
    "ROOT=workspace_slim",
    "EARLY=seed_4102",
    "MIDDLE=port_17419",
    "BOUNDARY=flag_582",
    "RECENT=file_729",
    "LATEST=answer_311",
];

struct TuiCompactionMetrics {
    before_tokens: u64,
    after_tokens: u64,
    summary_input_tokens: u64,
    summary_output_tokens: u64,
    facts_preserved: usize,
    stable_prefix_tokens: u64,
    next_request_ms: f64,
    summary_ms: Option<f64>,
    commits: usize,
    committed_local: bool,
    final_status: slim_core::context::CompactionStatus,
    states: Vec<slim_core::context::CompactionStatus>,
    summary_arrival_ms: Option<f64>,
}

fn compaction_corpus() -> Vec<ProviderMessage> {
    let mut history = vec![ProviderMessage::user(format!(
        "Finish the requested task. {}",
        COMPACTION_FACTS[0]
    ))];
    for index in 0..80 {
        let call_id = format!("read-{index}");
        let mut assistant =
            ProviderMessage::assistant(format!("Inspect file {index}."), Vec::new());
        assistant.tool_calls.push(ProviderToolCall {
            id: call_id.clone(),
            name: "read".into(),
            arguments: json!({"path": format!("file-{index}.txt")}).to_string(),
        });
        history.push(assistant);
        let fact = match index {
            0 => COMPACTION_FACTS[1],
            39 => COMPACTION_FACTS[2],
            62 => COMPACTION_FACTS[3],
            78 => COMPACTION_FACTS[4],
            _ => "",
        };
        history.push(ProviderMessage::tool(
            "read",
            &call_id,
            format!("file-{index} {fact} {}", "x".repeat(3_200)),
        ));
    }
    history.push(ProviderMessage::user(format!(
        "The final answer must use {}.",
        COMPACTION_FACTS[5]
    )));
    history
}

fn fixture_summary(request: &str) -> String {
    let visible_facts = COMPACTION_FACTS
        .iter()
        .filter(|fact| request.contains(**fact))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "## Goal\nContinue the task\n## Constraints\nRetain task facts\n## Progress\nHistory prepared\n## Blocked\nNone\n## Decisions\nUse recorded facts\n## Next steps\nContinue\n## Critical context\n{visible_facts}"
    )
}

fn estimate_wire_tokens(request: &str) -> u64 {
    let body = request.split_once("\r\n\r\n").expect("HTTP body").1;
    AdaptiveTokenEstimator::default().estimate(
        "openai-compatible",
        "tui-bridge-fixture",
        body.chars().count() as u64,
    )
}

fn run_tui_compaction_corpus(background: bool) -> TuiCompactionMetrics {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        let mut normal_count = 0;
        let mut first_writer = None;
        let mut summary_latency = None;
        let mut summary_text = None;
        while normal_count < 2 {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "compaction fixture timed out");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            let request = read_complete_http_request(&mut stream);
            let arrived = Instant::now();
            if request.contains("You are a context compactor") {
                assert!(request.contains(COMPACTION_FACTS[2]));
                let summary_started = Instant::now();
                thread::sleep(Duration::from_millis(20));
                let summary = fixture_summary(&request);
                let event = json!({"choices":[{"delta":{"content":summary}}]});
                let body = format!(
                    "data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
                );
                write_fixture_sse(&mut stream, &body);
                summary_latency = Some(summary_started.elapsed().as_secs_f64() * 1_000.0);
                summary_text = Some(summary);
            } else if normal_count == 0 {
                let content = "turn-padding ".repeat(7_000);
                first_writer = Some(thread::spawn(move || {
                    let event = json!({"choices":[{"delta":{
                        "content":content,
                        "tool_calls":[{"index":0,"id":"fixture-question","function":{
                            "name":"ask_question","arguments":json!({
                                "question":"Continue with the retained facts?",
                                "options":[
                                    {"label":"continue","description":"Use the stored context"},
                                    {"label":"stop","description":"Stop this run"}
                                ]
                            }).to_string()
                        }}]
                    }}]});
                    let body = format!(
                        "data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                    );
                    write_fixture_sse(&mut stream, &body);
                }));
                normal_count += 1;
            } else {
                write_fixture_sse(
                    &mut stream,
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                );
                normal_count += 1;
            }
            requests.push((request, arrived));
        }
        if let Some(writer) = first_writer {
            writer.join().expect("first response");
        }
        (requests, summary_latency, summary_text)
    });

    let policy = CompactionPolicy {
        background,
        ..CompactionPolicy::default()
    };
    let handle = CompactionHandle::new(policy);
    let options = ProviderRunOptions::default()
        .with_history(compaction_corpus())
        .with_context_window_tokens(115_000)
        .with_max_output_tokens(2_048)
        .with_compaction_handle(handle.clone());
    let (runtime, channels) =
        spawn_tui_runtime(request(format!("http://{address}")), options).expect("TUI bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("Continue the task.".into()))
        .expect("prompt");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut completed = false;
    let mut states = Vec::new();
    let mut answered = false;
    while Instant::now() < deadline && !completed {
        if let Ok(event) = channels
            .events_data
            .recv_timeout(Duration::from_millis(100))
        {
            match event {
                UiEvent::RunCompleted { run_id: 1 } => completed = true,
                UiEvent::RunFailed { message, .. } => panic!("TUI run failed: {message}"),
                UiEvent::CompactionState { state, .. } => states.push(state),
                UiEvent::QuestionRequired { request_id, .. } => {
                    assert!(!answered);
                    if background {
                        // The background summary request starts once this tool call
                        // arrives. Its completion is not observable from here (the
                        // handle stays `Preparing` until the loop resumes), so give
                        // the fixture time to answer it before resuming the loop.
                        // Only the background run has a summary to wait for.
                        thread::sleep(Duration::from_millis(120));
                    }
                    channels
                        .commands
                        .send(UiCommand::AnswerQuestion {
                            request_id,
                            answer: QuestionAnswer::option(0, "continue").expect("answer"),
                        })
                        .expect("answer question");
                    answered = true;
                }
                _ => {}
            }
        }
        while let Ok(event) = channels.events.try_recv() {
            match event {
                UiEvent::RunCompleted { run_id: 1 } => completed = true,
                UiEvent::RunFailed { message, .. } => panic!("TUI run failed: {message}"),
                UiEvent::CompactionState { state, .. } => states.push(state),
                _ => {}
            }
        }
    }
    assert!(completed, "TUI compaction run must complete");
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    let (requests, summary_ms, summary_text) = server.join().expect("server");
    assert!(answered, "the fixture question must be answered");
    let normal = requests
        .iter()
        .filter(|(request, _)| !request.contains("You are a context compactor"))
        .collect::<Vec<_>>();
    let summaries = requests
        .iter()
        .filter(|(request, _)| request.contains("You are a context compactor"))
        .collect::<Vec<_>>();
    assert_eq!(normal.len(), 2);
    assert_eq!(summaries.len(), usize::from(background));
    let first_body = normal[0].0.split_once("\r\n\r\n").unwrap().1;
    let second_body = normal[1].0.split_once("\r\n\r\n").unwrap().1;
    let first: serde_json::Value = serde_json::from_str(first_body).expect("first request JSON");
    let second: serde_json::Value = serde_json::from_str(second_body).expect("second request JSON");
    let stable_prefix_chars = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .zip(second["messages"].as_array().unwrap())
        .take_while(|(before, after)| before == after)
        .map(|(message, _)| message.to_string().chars().count() as u64)
        .sum();
    let commits = handle.take_commits();
    let final_status = handle.status();
    let committed_local = commits.first().is_some_and(|commit| {
        commit
            .summary
            .contains("[local extract; not an LLM summary]")
    });
    TuiCompactionMetrics {
        before_tokens: estimate_wire_tokens(normal[0].0.as_str()),
        after_tokens: estimate_wire_tokens(normal[1].0.as_str()),
        summary_input_tokens: summaries
            .first()
            .map_or(0, |(request, _)| estimate_wire_tokens(request)),
        summary_output_tokens: summary_text.as_deref().map_or(0, |summary| {
            AdaptiveTokenEstimator::default().estimate(
                "openai-compatible",
                "tui-bridge-fixture",
                summary.chars().count() as u64,
            )
        }),
        facts_preserved: COMPACTION_FACTS
            .iter()
            .filter(|fact| second_body.contains(**fact))
            .count(),
        stable_prefix_tokens: AdaptiveTokenEstimator::default().estimate(
            "openai-compatible",
            "tui-bridge-fixture",
            stable_prefix_chars,
        ),
        next_request_ms: normal[1].1.duration_since(normal[0].1).as_secs_f64() * 1_000.0,
        summary_ms,
        commits: commits.len(),
        committed_local,
        final_status,
        states,
        summary_arrival_ms: summaries
            .first()
            .map(|(_, at)| at.duration_since(normal[0].1).as_secs_f64() * 1_000.0),
    }
}

fn write_fixture_sse(stream: &mut std::net::TcpStream, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("fixture response");
}

#[test]
fn tui_prepares_and_applies_background_compaction_on_the_same_offline_corpus() {
    let without = run_tui_compaction_corpus(false);
    let with = run_tui_compaction_corpus(true);
    eprintln!(
        "SLIM_TUI_COMPACTION background=false before={} after={} summary_input={} summary_output={} facts={}/6 prefix={} next_ms={:.3} summary_ms={:?} summary_arrival_ms={:?} commits={} local={} status={:?} states={:?}",
        without.before_tokens,
        without.after_tokens,
        without.summary_input_tokens,
        without.summary_output_tokens,
        without.facts_preserved,
        without.stable_prefix_tokens,
        without.next_request_ms,
        without.summary_ms,
        without.summary_arrival_ms,
        without.commits,
        without.committed_local,
        without.final_status,
        without.states
    );
    eprintln!(
        "SLIM_TUI_COMPACTION background=true before={} after={} summary_input={} summary_output={} facts={}/6 prefix={} next_ms={:.3} summary_ms={:?} summary_arrival_ms={:?} commits={} local={} status={:?} states={:?}",
        with.before_tokens,
        with.after_tokens,
        with.summary_input_tokens,
        with.summary_output_tokens,
        with.facts_preserved,
        with.stable_prefix_tokens,
        with.next_request_ms,
        with.summary_ms,
        with.summary_arrival_ms,
        with.commits,
        with.committed_local,
        with.final_status,
        with.states
    );
    assert_eq!(without.before_tokens, with.before_tokens);
    assert_eq!(without.summary_input_tokens, 0);
    assert_eq!(with.commits, 1, "prepared summary should be applied");
    assert_eq!(
        without.commits, 1,
        "hard threshold should use local extract"
    );
    assert_eq!(with.facts_preserved, 6);
    assert!(without.facts_preserved < with.facts_preserved);
    assert!(without.after_tokens < without.before_tokens);
    assert!(with.after_tokens < with.before_tokens);
}

struct HardCompactionMetrics {
    trigger_tokens: u64,
    after_tokens: u64,
    summary_input_tokens: u64,
    summary_output_tokens: u64,
    facts_preserved: usize,
    middle_preserved: bool,
    request_latency_ms: f64,
    summary_latency_ms: Option<f64>,
    local_extract: bool,
}

fn run_hard_compaction_corpus(http_summary: bool) -> HardCompactionMetrics {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        let mut summary_text = None;
        let mut summary_latency_ms = None;
        loop {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "hard compaction fixture timed out"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            let request = read_complete_http_request(&mut stream);
            let arrived = Instant::now();
            let is_summary = request.contains("You are a context compactor");
            if is_summary {
                assert!(http_summary, "local hard must not request a summary");
                assert!(request.contains(COMPACTION_FACTS[2]));
                let summary_started = Instant::now();
                thread::sleep(Duration::from_millis(20));
                let summary = fixture_summary(&request);
                let event = json!({"choices":[{"delta":{"content":summary}}]});
                let body = format!(
                    "data: {event}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
                );
                write_fixture_sse(&mut stream, &body);
                summary_latency_ms = Some(summary_started.elapsed().as_secs_f64() * 1_000.0);
                summary_text = Some(summary);
            } else {
                write_fixture_sse(
                    &mut stream,
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                );
            }
            requests.push((request, arrived));
            if !is_summary {
                break;
            }
        }
        (requests, summary_text, summary_latency_ms)
    });

    let policy = CompactionPolicy {
        background: false,
        strategy: CompactionStrategy::Summary,
        ..CompactionPolicy::default()
    };
    let handle = CompactionHandle::new(policy);
    if http_summary {
        handle.request_manual("").expect("queue HTTP compaction");
    }
    let options = ProviderRunOptions::default()
        .with_history(compaction_corpus())
        .with_context_window_tokens(64_000)
        .with_max_output_tokens(2_048)
        .with_compaction_handle(handle.clone());
    let (runtime, channels) =
        spawn_tui_runtime(request(format!("http://{address}")), options).expect("TUI bridge");
    let started = Instant::now();
    channels
        .commands
        .send(UiCommand::SendPrompt("Continue the task.".into()))
        .expect("prompt");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut applied = None;
    let mut completed = false;
    while Instant::now() < deadline && !completed {
        if let Ok(event) = channels
            .events_data
            .recv_timeout(Duration::from_millis(100))
        {
            match event {
                UiEvent::RunCompleted { run_id: 1 } => completed = true,
                UiEvent::RunFailed { message, .. } => panic!("TUI run failed: {message}"),
                UiEvent::CompactionState {
                    state: slim_core::context::CompactionStatus::Applied,
                    tokens_before,
                    ..
                } => applied = Some(tokens_before),
                _ => {}
            }
        }
        while let Ok(event) = channels.events.try_recv() {
            match event {
                UiEvent::RunCompleted { run_id: 1 } => completed = true,
                UiEvent::RunFailed { message, .. } => panic!("TUI run failed: {message}"),
                _ => {}
            }
        }
    }
    assert!(completed, "hard compaction run must complete");
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    let (requests, summary_text, summary_latency_ms) = server.join().expect("server");
    assert_eq!(requests.len(), if http_summary { 2 } else { 1 });
    let task_request = requests
        .iter()
        .find(|(request, _)| !request.contains("You are a context compactor"))
        .expect("task request");
    let summary_request = requests
        .iter()
        .find(|(request, _)| request.contains("You are a context compactor"));
    let commits = handle.take_commits();
    assert_eq!(commits.len(), 1);
    HardCompactionMetrics {
        trigger_tokens: applied.expect("compaction applied event"),
        after_tokens: estimate_wire_tokens(&task_request.0),
        summary_input_tokens: summary_request
            .map_or(0, |(request, _)| estimate_wire_tokens(request)),
        summary_output_tokens: summary_text.as_deref().map_or(0, |summary| {
            AdaptiveTokenEstimator::default().estimate(
                "openai-compatible",
                "tui-bridge-fixture",
                summary.chars().count() as u64,
            )
        }),
        facts_preserved: COMPACTION_FACTS
            .iter()
            .filter(|fact| task_request.0.contains(**fact))
            .count(),
        middle_preserved: task_request.0.contains(COMPACTION_FACTS[2]),
        request_latency_ms: task_request.1.duration_since(started).as_secs_f64() * 1_000.0,
        summary_latency_ms,
        local_extract: commits[0]
            .summary
            .contains("[local extract; not an LLM summary]"),
    }
}

#[test]
fn hard_compaction_compares_local_extract_with_http_summary_on_same_corpus() {
    let uncompacted_wire_tokens = run_tui_compaction_corpus(false).before_tokens;
    let local = run_hard_compaction_corpus(false);
    let http = run_hard_compaction_corpus(true);
    eprintln!(
        "SLIM_HARD_COMPACTION mode=local before_wire={} trigger_estimate={} after={} summary_input={} summary_output={} facts={}/6 request_ms={:.3} summary_ms={:?}",
        uncompacted_wire_tokens,
        local.trigger_tokens,
        local.after_tokens,
        local.summary_input_tokens,
        local.summary_output_tokens,
        local.facts_preserved,
        local.request_latency_ms,
        local.summary_latency_ms
    );
    eprintln!(
        "SLIM_HARD_COMPACTION mode=http before_wire={} trigger_estimate={} after={} summary_input={} summary_output={} facts={}/6 request_ms={:.3} summary_ms={:?}",
        uncompacted_wire_tokens,
        http.trigger_tokens,
        http.after_tokens,
        http.summary_input_tokens,
        http.summary_output_tokens,
        http.facts_preserved,
        http.request_latency_ms,
        http.summary_latency_ms
    );
    assert_eq!(local.trigger_tokens, http.trigger_tokens);
    assert!(uncompacted_wire_tokens > local.after_tokens);
    assert!(uncompacted_wire_tokens > http.after_tokens);
    assert!(local.local_extract);
    assert!(!http.local_extract);
    assert_eq!(local.summary_input_tokens, 0);
    assert!(http.summary_input_tokens > 0);
    assert_eq!(http.facts_preserved, 6);
    assert_eq!(local.facts_preserved, 5);
    assert!(!local.middle_preserved);
    assert!(http.middle_preserved);
}
