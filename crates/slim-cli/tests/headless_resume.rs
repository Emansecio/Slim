#[path = "../../../tests/support/budget_finalization.rs"]
mod budget_finalization;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slim_cli::{
    run_cli, run_provider_headless_with_resume, run_provider_headless_with_resume_and_options,
    ExitCode, ProviderRequest,
};
use slim_core::provider::ProviderKind;
use slim_core::session::{
    preflight_session, recover_durable_v2, DurableEntry, DurableEntryRole, DurableOperation,
    DurableOperationKind, DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo,
    SessionWriter,
};
use slim_core::OperatingMode;

fn temp_path(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir()
        .join(format!(
            "slim-stage8-{label}-{}-{stamp}",
            std::process::id()
        ))
        .join("session.jsonl")
}

fn create_v2(path: &Path) {
    let mut repo = JsonlRepo::create(
        path,
        DurableSessionHeader::new(
            "stage8",
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

fn create_v2_with_history(path: &Path) {
    let mut repo = JsonlRepo::create(
        path,
        DurableSessionHeader::new("history", "now", "D:\\Slim", None, None),
    )
    .expect("v2 history session");
    repo.append(DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "history-user".into(),
            role: DurableEntryRole::User,
            content: "old question".into(),
            parent_entry_id: None,
            operation_id: "history-op".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("history user");
    repo.append(DurableRecord::Entry {
        seq: 1,
        entry: DurableEntry {
            entry_id: "history-assistant".into(),
            role: DurableEntryRole::Assistant,
            content: "old answer".into(),
            parent_entry_id: Some("history-user".into()),
            operation_id: "history-op".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("history assistant");
}

fn add_tool_call_metadata(path: &Path) {
    let mut repo = JsonlRepo::open(path).expect("open history session");
    repo.append(DurableRecord::Entry {
        seq: 2,
        entry: DurableEntry {
            entry_id: "history-tool-metadata".into(),
            role: DurableEntryRole::Assistant,
            content: "tool-backed answer".into(),
            parent_entry_id: Some("history-assistant".into()),
            operation_id: "history-op".into(),
            tool_call_id: Some("tool-call-1".into()),
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("tool call metadata");
}

fn request(endpoint: String) -> ProviderRequest {
    ProviderRequest {
        prompt: "continue explicitly".into(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "fixture-model".into(),
        api_key: "fixture-secret".into(),
        account_id: None,
        timeout: Duration::from_secs(2),
    }
}

fn spawn_fixture(requests: Arc<AtomicUsize>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture accept timed out");
                    thread::yield_now();
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        requests.fetch_add(1, Ordering::SeqCst);
        let mut raw = [0_u8; 32 * 1024];
        let _ = stream.read(&mut raw).expect("request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream
            .write_all(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"resumed\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("events");
    });
    (format!("http://{address}"), server)
}

fn spawn_capturing_fixture(
    request_body: Arc<Mutex<Option<String>>>,
    tool_call: bool,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture accept timed out");
                    thread::yield_now();
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 16 * 1024];
        let (header_end, content_length) = loop {
            let size = stream.read(&mut chunk).expect("request");
            assert!(size > 0, "request closed before body");
            raw.extend_from_slice(&chunk[..size]);
            let text = String::from_utf8_lossy(&raw);
            let Some(header_end) = text.find("\r\n\r\n").map(|index| index + 4) else {
                continue;
            };
            let Some(content_length) = text.lines().find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
            }) else {
                continue;
            };
            if raw.len() >= header_end + content_length {
                break (header_end, content_length);
            }
        };
        let body = String::from_utf8_lossy(&raw[header_end..header_end + content_length]);
        *request_body.lock().expect("request body lock") = Some(body.into_owned());
        let payload = if tool_call {
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"stage8-side-effect.txt\\\",\\\"content\\\":\\\"bad\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n".as_slice()
        } else {
            b"data: {\"choices\":[{\"delta\":{\"content\":\"assistant fixture-secret\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".as_slice()
        };
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        stream.write_all(payload).expect("events");
        drop(stream);
        if tool_call {
            budget_finalization::reject_budget_finalization(&listener);
        }
    });
    (format!("http://{address}"), server)
}

#[test]
fn healthy_resume_has_one_provider_request_and_preserves_prefix() {
    let path = temp_path("healthy");
    create_v2(&path);
    let before = std::fs::read(&path).expect("prefix");
    let requests = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_fixture(Arc::clone(&requests));

    let result = run_provider_headless_with_resume(request(endpoint), &path).expect("resume");
    server.join().expect("server");

    assert_eq!(result.code, ExitCode::Success);
    assert_eq!(result.text, "resumed");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let after = std::fs::read(&path).expect("after");
    assert!(after.starts_with(&before), "durable prefix changed");
    let report = preflight_session(&path).expect("preflight");
    let seqs: Vec<_> = report.records.iter().map(DurableRecord::seq).collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    let operations: Vec<_> = report
        .records
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Operation { operation, .. } => Some(operation.operation_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        operations
            .iter()
            .filter(|id| id.starts_with("resume-"))
            .count(),
        4
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn resume_reconstructs_history_and_redacts_known_secret_before_persisting() {
    let path = temp_path("history");
    create_v2_with_history(&path);
    let request_body = Arc::new(Mutex::new(None));
    let (endpoint, server) = spawn_capturing_fixture(Arc::clone(&request_body), false);
    let mut provider_request = request(endpoint);
    provider_request.prompt = "new prompt fixture-secret".into();

    let result = run_provider_headless_with_resume(provider_request, &path).expect("resume");
    server.join().expect("server");
    assert_eq!(result.code, ExitCode::Success);
    assert_eq!(result.text, "assistant [REDACTED]");
    let body = request_body
        .lock()
        .expect("body lock")
        .clone()
        .expect("captured body");
    let body: serde_json::Value = serde_json::from_str(&body).expect("request json");
    let messages = body["messages"].as_array().expect("messages");
    let serialized = serde_json::to_string(messages).expect("messages json");
    assert!(serialized.contains("old question"));
    assert!(serialized.contains("old answer"));
    assert!(serialized.contains("new prompt [REDACTED]"));
    assert!(!serialized.contains("fixture-secret"));
    let durable = std::fs::read_to_string(&path).expect("durable jsonl");
    assert!(!durable.contains("fixture-secret"));
    let report = preflight_session(&path).expect("preflight after resume");
    assert!(report.records.iter().any(|record| {
        matches!(
            record,
            DurableRecord::Entry { entry, .. }
                if entry.role == DurableEntryRole::User
                    && entry.content == "new prompt [REDACTED]"
                    && entry.parent_entry_id.as_deref() == Some("history-assistant")
        )
    }));
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn resume_rejects_tool_call_metadata_history_without_provider_or_mutation() {
    let path = temp_path("tool-history-metadata");
    create_v2_with_history(&path);
    add_tool_call_metadata(&path);
    let before = std::fs::read(&path).expect("prefix");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking probe listener");
    let endpoint = format!("http://{}", listener.local_addr().expect("probe address"));

    let error = run_provider_headless_with_resume(request(endpoint), &path)
        .expect_err("tool-call metadata must fail closed");
    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { ref message }
            if message.contains("tool-call metadata")
    ));
    assert_eq!(std::fs::read(&path).expect("after rejection"), before);
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn resume_explicit_zero_tool_budget_blocks_side_effect_and_records_failed_terminal() {
    let path = temp_path("tool-blocked");
    create_v2(&path);
    let workspace = path.parent().expect("parent").to_path_buf();
    std::fs::create_dir_all(&workspace).expect("workspace");
    let request_body = Arc::new(Mutex::new(None));
    let (endpoint, server) = spawn_capturing_fixture(Arc::clone(&request_body), true);
    let result = run_provider_headless_with_resume_and_options(
        request(endpoint),
        &path,
        slim_cli::ProviderRunOptions::default()
            .with_workspace_root(&workspace)
            .with_max_tool_calls(0)
            .with_max_read_tool_calls(0),
    )
    .expect("tool-limit is a truthful result");
    server.join().expect("server");
    assert_eq!(result.code, ExitCode::Tool);
    assert_eq!(result.stop, "tool_limit");
    let rendered = slim_cli::render_provider_text(&result);
    assert!(rendered.contains("Task remains pending"), "{rendered}");
    assert!(
        rendered.contains("Final response failed") && rendered.contains("503"),
        "{rendered}"
    );
    let jsonl: serde_json::Value =
        serde_json::from_str(slim_cli::render_provider_jsonl(&result).unwrap().trim()).unwrap();
    assert!(jsonl["stop_message"].as_str().unwrap().contains("503"));
    assert!(!workspace.join("stage8-side-effect.txt").exists());
    let report = preflight_session(&path).expect("report");
    assert!(report.records.iter().any(|record| matches!(record,
        DurableRecord::Entry { entry, .. } if entry.content.contains("[Run stopped]") && entry.content.contains("503"))));
    assert!(report.records.iter().any(|record| {
        matches!(
            record,
            DurableRecord::Operation {
                operation: DurableOperation {
                    kind: DurableOperationKind::Finished {
                        outcome: slim_core::session::DurableOutcome::Failed
                    },
                    ..
                },
                ..
            }
        )
    }));
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn resume_surfaces_pending_old_work_instead_of_silently_ignoring_it() {
    let path = temp_path("pending");
    let mut repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("pending", "now", "D:\\Slim", None, None),
    )
    .expect("pending session");
    repo.append(DurableRecord::Entry {
        seq: 0,
        entry: DurableEntry {
            entry_id: "pending-input".into(),
            role: DurableEntryRole::User,
            content: "old work".into(),
            parent_entry_id: None,
            operation_id: "pending-op".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
        },
    })
    .expect("pending entry");
    repo.append(DurableRecord::Operation {
        seq: 1,
        operation: DurableOperation {
            operation_id: "pending-op".into(),
            kind: DurableOperationKind::Started {
                input_entry_id: "pending-input".into(),
            },
        },
    })
    .expect("pending operation");
    drop(repo);
    let before = std::fs::read(&path).expect("before");
    let error = run_provider_headless_with_resume(request("http://127.0.0.1:1".into()), &path)
        .expect_err("pending work requires explicit decision");
    assert!(format!("{error:?}").contains("explicit decision"));
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn v1_and_torn_tail_resume_reject_without_mutation_and_recover_is_explicit() {
    let v1 = temp_path("v1");
    let mut writer = SessionWriter::create(&v1, "legacy", "D:\\Slim").expect("v1");
    writer
        .append(&slim_core::SessionEvent::new(
            1,
            slim_core::EventKind::SessionStarted {
                session_id: "legacy".into(),
            },
        ))
        .expect("event");
    drop(writer);
    let v1_before = std::fs::read(&v1).expect("v1 bytes");
    let mut v1_request = request("http://127.0.0.1:1".into());
    v1_request.mode = OperatingMode::Plan;
    let v1_error =
        run_provider_headless_with_resume(v1_request, &v1).expect_err("v1 plan resume must reject");
    assert!(format!("{v1_error:?}").contains("durable resume"));
    assert_eq!(std::fs::read(&v1).expect("v1 unchanged"), v1_before);

    let torn = temp_path("torn");
    create_v2(&torn);
    let tail = br#"{"type":"operation""#;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&torn)
        .expect("open torn");
    file.write_all(tail).expect("tail");
    drop(file);
    let torn_before = std::fs::read(&torn).expect("torn bytes");
    let torn_error = run_provider_headless_with_resume(request("http://127.0.0.1:1".into()), &torn)
        .expect_err("torn must reject");
    assert!(format!("{torn_error:?}").contains("explicit recovery"));
    assert_eq!(std::fs::read(&torn).expect("torn unchanged"), torn_before);
    let report = preflight_session(&torn).expect("torn preflight");
    assert!(matches!(
        report.status,
        slim_core::session::PreflightStatus::TornTail { .. }
    ));
    recover_durable_v2(&torn).expect("explicit recover");
    assert_ne!(std::fs::read(&torn).expect("recovered bytes"), torn_before);
    assert!(PathBuf::from(format!("{}.quarantine", torn.display())).exists());

    let _ = std::fs::remove_dir_all(v1.parent().expect("v1 parent"));
    let _ = std::fs::remove_dir_all(torn.parent().expect("torn parent"));
}

#[test]
fn cli_rejects_ambiguous_session_modes_and_requires_prompt_for_resume() {
    let ambiguous = run_cli(
        ["--headless", "--session", "a.jsonl", "--resume", "b.jsonl"],
        "",
    );
    assert_eq!(ambiguous.code, ExitCode::InputRequired);
    assert!(ambiguous.stderr.contains("mutually exclusive"));

    let path = temp_path("prompt");
    create_v2(&path);
    let output = run_cli(["--headless", "--resume", path.to_str().expect("path")], "");
    assert_eq!(output.code, ExitCode::InputRequired);
    assert!(output.stderr.contains("explicit --prompt"));
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn recover_is_strictly_recovery_only_and_does_not_run_provider() {
    let path = temp_path("recover-only");
    create_v2(&path);
    let before = std::fs::read(&path).expect("before");
    let output = run_cli(
        [
            "--headless",
            "--recover",
            path.to_str().expect("path"),
            "--prompt",
            "must not execute",
            "--endpoint",
            "http://127.0.0.1:1",
        ],
        "",
    );
    assert_eq!(output.code, ExitCode::InputRequired);
    assert!(output.stderr.contains("recovery-only"));
    assert_eq!(std::fs::read(&path).expect("unchanged"), before);
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn explicit_abandon_resolves_queue_states_is_locked_and_idempotent() {
    let path = temp_path("abandon-queue");
    create_v2(&path);
    let mut repo = JsonlRepo::open_no_repair(&path).unwrap();
    for (id, state) in [("pending", 0), ("claimed", 1), ("suspended", 2)] {
        repo.append(DurableRecord::Operation {
            seq: repo.next_seq().unwrap(),
            operation: DurableOperation {
                operation_id: id.into(),
                kind: DurableOperationKind::QueueIntent {
                    input_entry_id: None,
                },
            },
        })
        .unwrap();
        if state > 0 {
            repo.append(DurableRecord::Operation {
                seq: repo.next_seq().unwrap(),
                operation: DurableOperation {
                    operation_id: id.into(),
                    kind: DurableOperationKind::Claimed,
                },
            })
            .unwrap();
        }
        if state > 1 {
            repo.append(DurableRecord::Operation {
                seq: repo.next_seq().unwrap(),
                operation: DurableOperation {
                    operation_id: id.into(),
                    kind: DurableOperationKind::Suspended {
                        reason: "interrupted".into(),
                    },
                },
            })
            .unwrap();
        }
    }
    let before = std::fs::read(&path).unwrap();
    let args = [
        "--headless",
        "--recover",
        path.to_str().unwrap(),
        "--abandon-pending",
    ];
    assert_eq!(
        run_cli(args, "").code,
        ExitCode::Blocked,
        "live writer must block recovery"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(repo);
    assert_eq!(
        run_cli(["--headless", "--abandon-pending"], "").code,
        ExitCode::InputRequired
    );
    assert_eq!(
        run_cli(["--headless", "--recover", path.to_str().unwrap()], "").code,
        ExitCode::Success
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "repair alone cannot abandon work"
    );
    let output = run_cli(args, "");
    assert_eq!(output.code, ExitCode::Success, "{}", output.stderr);
    let after = std::fs::read(&path).unwrap();
    assert!(after.starts_with(&before));
    assert_eq!(run_cli(args, "").code, ExitCode::Success);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        after,
        "second decision adds nothing"
    );
    let report = preflight_session(&path).unwrap();
    assert_eq!(
        report.summary.pending_count()
            + report.summary.claimed_count()
            + report.summary.suspended_count(),
        0
    );
    slim_core::session::resume_plan_from_preflight(&report).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn failed_provider_after_tools_persists_the_redacted_reason_and_tool_history() {
    for partial in [false, true] {
        let path = temp_path("failed-after-tools");
        create_v2(&path);
        let workspace = path.parent().unwrap();
        std::fs::write(workspace.join("source.txt"), "retained evidence").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            // A truncated stream is recoverable: the runtime retries it up to
            // MAX_PROVIDER_RECOVERIES, so the partial branch must keep serving.
            let turns = if partial { 4 } else { 2 };
            for turn in 0..turns {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "fixture accept deadline");
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 16 * 1024];
                loop {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let (status, body) = if turn == 0 {
                    let call = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"read-source","function":{"name":"read","arguments":"{\"path\":\"source.txt\"}"}}]},"finish_reason":"tool_calls"}]});
                    (200, format!("data: {call}\n\ndata: [DONE]\n\n"))
                } else {
                    let body = String::from_utf8(request).unwrap();
                    assert!(body.contains("retained evidence"));
                    if partial {
                        (
                            200,
                            format!(
                                "data: {}\n\n",
                                serde_json::json!({"choices":[{"delta":{"content":"visible partial fixture-secret ".repeat(20)}}]})
                            ),
                        )
                    } else {
                        (401, "denied fixture-secret".into())
                    }
                };
                stream.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
            }
        });
        let result = run_provider_headless_with_resume_and_options(
            request(endpoint),
            &path,
            slim_cli::ProviderRunOptions::default().with_workspace_root(workspace),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(result.code, ExitCode::Provider);
        assert_eq!(result.stop, "provider_error");
        let report = preflight_session(&path).unwrap();
        let entries: Vec<_> = report
            .records
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Entry { entry, .. } => Some(entry),
                _ => None,
            })
            .collect();
        assert!(entries
            .iter()
            .any(|entry| entry.role == DurableEntryRole::Tool
                && entry.content.contains("retained evidence")));
        let failure = entries.last().unwrap();
        assert!(
            failure.content.starts_with("[Run failed]\n"),
            "{}",
            failure.content
        );
        if partial {
            assert!(entries
                .iter()
                .any(|entry| entry.content.starts_with("[Interrupted turn]")
                    && entry.content.contains("visible partial [REDACTED]")));
            assert!(failure.content.contains("stream ended"));
        } else {
            assert!(failure.content.contains("http 401"));
            assert!(failure.content.contains("[REDACTED]"));
        }
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("fixture-secret"));
        std::fs::remove_dir_all(workspace).unwrap();
    }
}

#[test]
fn todo_state_survives_failed_turn_and_durable_resume() {
    let path = temp_path("todo-state");
    create_v2(&path);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for turn in 0..4 {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {turn} missing");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0; 16384];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&chunk[..n]);
                if let Some(end) = raw.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&raw[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if raw.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(raw).unwrap();
            let (status, body) = if turn == 1 {
                assert!(request.contains("todo 0 [pending]: saved task"));
                (401, "fixture failure".to_owned())
            } else if turn == 3 {
                assert!(
                    request.contains("todo 0 [completed]: saved task"),
                    "{request}"
                );
                assert!(!request.contains("todo not found"));
                (
                    200,
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        serde_json::json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
                    ),
                )
            } else {
                let arguments = if turn == 0 {
                    serde_json::json!({"todos":[{"title":"saved task"}]})
                } else {
                    serde_json::json!({"todos":[{"id":0,"status":"completed"}]})
                };
                let call = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("todo-{turn}"),"function":{"name":"todo","arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]});
                (200, format!("data: {call}\n\ndata: [DONE]\n\n"))
            };
            stream.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
        }
    });
    let first = run_provider_headless_with_resume(request(endpoint.clone()), &path).unwrap();
    assert_eq!(first.code, ExitCode::Provider);
    let blocker = first.stop_message.as_deref().unwrap();
    assert!(
        blocker.contains("Pending tasks:") && blocker.contains("saved task"),
        "{blocker}"
    );
    let report = preflight_session(&path).unwrap();
    let facts: Vec<_> = report
        .records
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Fact { fact, .. } if fact.namespace == "task.v1" => Some(fact),
            _ => None,
        })
        .collect();
    assert_eq!(facts.len(), 1);
    assert!(!serde_json::to_string(&facts)
        .unwrap()
        .contains("fixture-secret"));
    let second = run_provider_headless_with_resume(request(endpoint), &path).unwrap();
    assert_eq!(second.code, ExitCode::Success);
    server.join().unwrap();
    let report = preflight_session(&path).unwrap();
    let facts: Vec<_> = report
        .records
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Fact { fact, .. } if fact.namespace == "task.v1" => Some(fact.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(facts.len(), 2, "old mutation must not be persisted twice");
    let mut runtime = slim_core::runtime::Runtime::new();
    runtime
        .restore_task_facts(&facts, path.parent().unwrap())
        .unwrap();
    assert_eq!(runtime.todo_items().len(), 1);
    assert_eq!(runtime.todo_items()[0].status, "completed");
    assert!(runtime.app.events().is_empty());
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn credential_bearing_tool_is_rejected_before_durable_effects() {
    let path = temp_path("secret-tool-rejected");
    create_v2(&path);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = [0; 32768];
        let _ = stream.read(&mut bytes).unwrap();
        let payload = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"unsafe-todo",
            "function":{"name":"todo","arguments":"{\"todos\":[{\"title\":\"saved fixture-secret\"}]}"}}]},"finish_reason":"tool_calls"}]});
        let body = format!("data: {payload}\n\ndata: [DONE]\n\n");
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
    });
    let result = run_provider_headless_with_resume(request(endpoint), &path);
    server.join().unwrap();
    let Err(error) = result else {
        panic!("sensitive-material rejection must propagate fail-closed as Err")
    };
    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { ref message }
            if message.contains("registered sensitive material")
    ));
    let report = preflight_session(&path).unwrap();
    assert!(!report.records.iter().any(|record| matches!(record,
        DurableRecord::Fact { fact, .. } if fact.namespace == "task.v1")));
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("fixture-secret"));
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
