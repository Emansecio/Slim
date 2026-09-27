use super::{
    advance_pending_delivery, associate_projected_run, attach_workspace_to_snapshot,
    esc_forces_quit, execution_result_events, project_core_event, project_sync_tui_events,
    send_cancel_result, take_run_id, ContentStore, EventSink, PendingDeliveryStep, PendingRun,
    WakeSignal, CONTENT_ENTRY_BYTES, CONTENT_PAGE_BYTES, CONTENT_STORE_BYTES,
    CONTENT_STORE_ENTRIES, ESC_FORCE_WINDOW,
};
use crate::exit_codes::ExitCode;
use crate::headless::{ProviderExecution, ProviderHeadlessResult, ToolLoopLimits};
use slim_core::provider::ProviderKind;
use slim_core::runtime::{AgentLoopConfig, CancellationToken};
use slim_core::{EventKind, SessionEvent};
use slim_tui::api::{
    ContentHandle, InteractionRequestId, PageCursor, SessionId, ToolBatchId, ToolCallId, UiCommand,
    UiEvent,
};
use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn esc_escalates_only_inside_the_force_window() {
    let now = Instant::now();
    assert!(!esc_forces_quit(None, now));
    assert!(esc_forces_quit(Some(now), now));
    assert!(esc_forces_quit(
        Some(now),
        now + ESC_FORCE_WINDOW - Duration::from_millis(1)
    ));
    assert!(!esc_forces_quit(
        Some(now),
        now + ESC_FORCE_WINDOW + Duration::from_millis(1)
    ));
}

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
                name: "shell".into(),
                output: "exit 1\nstdout:\nstderr:\nsafe [REDACTED]".into(),
            },
        ),
        7,
        &store,
    )
    .expect("projected");
    let UiEvent::ToolOutput {
        content_handle: Some(handle),
        output: preview,
        ..
    } = projected
    else {
        panic!("expected paged final tool output");
    };
    assert_eq!(&*handle.0, "tool:7:batch:call");
    let page = store
        .lock()
        .expect("store")
        .page(&handle, Some(PageCursor(0)))
        .expect("page");
    assert_eq!(page.text, "exit 1\nstdout:\nstderr:\nsafe [REDACTED]");
    assert_eq!(preview, "exit 1 · safe [REDACTED]");
}

#[test]
fn job_completion_replaces_launch_acknowledgment_in_original_inspector() {
    let store = Default::default();
    let make_event =
        |seq, kind| project_core_event(SessionEvent::new(seq, kind), 7, &store).unwrap();
    let ack = make_event(
        1,
        EventKind::ToolOutput {
            batch_id: "batch".into(),
            call_id: "call".into(),
            name: "shell".into(),
            output: "job_id=shell-1 state=running".into(),
        },
    );
    let final_output = make_event(
        2,
        EventKind::ToolJobOutput {
            batch_id: "batch".into(),
            call_id: "call".into(),
            name: "shell".into(),
            output: "exit 1\nstdout:\nfinal marker".into(),
        },
    );
    let (
        UiEvent::ToolOutput {
            content_handle: Some(first),
            ..
        },
        UiEvent::ToolOutput {
            content_handle: Some(last),
            ..
        },
    ) = (ack, final_output)
    else {
        panic!("both outputs must project to terminal tool output");
    };
    assert_eq!(first, last);
    let page = store
        .lock()
        .unwrap()
        .page(&last, Some(PageCursor(0)))
        .unwrap();
    assert_eq!(page.text, "exit 1\nstdout:\nfinal marker");
}

#[test]
fn session_snapshot_receives_resolved_workspace_display() {
    let event = attach_workspace_to_snapshot(
        UiEvent::SessionSnapshot {
            session_id: SessionId("session".into()),
            cwd: String::new(),
            skill_names: Vec::new(),
        },
        r"D:\Slim",
    );
    assert_eq!(
        event,
        UiEvent::SessionSnapshot {
            session_id: SessionId("session".into()),
            cwd: r"D:\Slim".into(),
            skill_names: Vec::new(),
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

#[test]
fn provider_failure_is_durable_in_the_ui_after_toast_expiry() {
    let mut execution = execution(ExitCode::Provider);
    execution.result.stop = "provider_error".into();
    execution.result.text = "provider error: http 401: denied".into();
    let events = execution_result_events(7, None, Ok(Ok(execution)), true);
    assert!(matches!(events.back(), Some(UiEvent::RunFailed { .. })));
    let mut state = slim_tui::app::AppState::new();
    state.apply_event(UiEvent::run_started(7));
    for event in events {
        state.apply_event(event);
    }
    state.clock.elapsed_ms = 10_000;
    state.prune_notifications();
    assert!(!state.working);
    assert!(state.blocks().iter().any(|block| matches!(block.kind(), slim_tui::block::BlockKind::Error(message) if message.contains("http 401"))));
}

fn execution(code: ExitCode) -> ProviderExecution {
    ProviderExecution {
        turn_transcript: Vec::new(),
        task_facts: Vec::new(),
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
            tool_process_facts: Vec::new(),
            tool_job_outputs: Vec::new(),
            stop_message: None,
        },
        history: None,
        events: Vec::new(),
        tool_results: Vec::new(),
        limits: ToolLoopLimits {
            max_mutating_tool_calls: AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            max_read_tool_calls: AgentLoopConfig::DEFAULT_MAX_READ_TOOL_CALLS,
            max_total_tool_calls: AgentLoopConfig::DEFAULT_MAX_TOTAL_TOOL_CALLS,
            max_turns: AgentLoopConfig::DEFAULT_MAX_TURNS,
            max_output_tokens: slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS,
            max_result_bytes: AgentLoopConfig::default().max_result_bytes,
            context_window_tokens: AgentLoopConfig::default().context_window_tokens,
        },
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

#[test]
fn skill_discovery_warning_survives_memoized_workspace_restore() {
    use super::SkillNameMemo;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let root = std::env::temp_dir().join(format!(
        "slim-skill-warning-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let skill = root.join(".slim/skills/broken");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "invalid frontmatter").unwrap();
    let mut memo = SkillNameMemo::default();
    let first = memo.names(Some(root.clone()));
    let original_warning = memo.warnings.clone();
    memo.warnings.clear();
    let second = memo.names(Some(root.clone()));
    assert_eq!(first, second);
    assert_eq!(memo.warnings, original_warning);
    assert!(memo.warnings.contains("broken"));
    assert!(memo.warnings.contains("missing frontmatter"));
    fs::remove_dir_all(root).unwrap();
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
    let tool_output = UiEvent::ToolOutput {
        batch_id: ToolBatchId("batch-1".into()),
        call_id: ToolCallId("call-1".into()),
        name: "mcp".into(),
        output: "mcp operation outcome is uncertain".into(),
        content_handle: Some(ContentHandle("content-final".into())),
    };
    assert!(sink.send_projected(tool_output.clone(), &cancellation));
    assert_eq!(
        control_rx
            .recv()
            .expect("final tool output migrates to control before terminal"),
        tool_output
    );
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
fn projector_namespaces_patch_diff_like_its_tool_start() {
    let (batch_id, call_id) = (ToolBatchId("batch-1".into()), ToolCallId("call-1".into()));
    let started = associate_projected_run(
        UiEvent::ToolStarted {
            batch_id: batch_id.clone(),
            call_id: call_id.clone(),
            name: "patch".into(),
            arguments_summary: "path=a.rs".into(),
        },
        7,
    );
    let diff = associate_projected_run(
        UiEvent::ToolDiff {
            batch_id,
            call_id,
            diff: slim_core::ToolEditDiff::default(),
        },
        7,
    );
    let (
        UiEvent::ToolStarted {
            batch_id: started_batch,
            call_id: started_call,
            ..
        },
        UiEvent::ToolDiff {
            batch_id: diff_batch,
            call_id: diff_call,
            ..
        },
    ) = (started, diff)
    else {
        panic!("tool events");
    };
    assert_eq!((started_batch, started_call), (diff_batch, diff_call));
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
        admission: None,
        result: None,
        projector: None,
        delivery: execution_result_events(7, None, Ok(Ok(execution(ExitCode::Success))), false),
        cancellation: cancellation.clone(),
        durable: false,
        cancel_requested: false,
        content_store: Default::default(),
    };
    let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    commands.send(UiCommand::CancelRun).expect("cancel");

    assert_eq!(
        advance_pending_delivery(&mut run, &mut command_rx, &sink, &mut false, &None),
        PendingDeliveryStep::Complete
    );
    assert!(cancellation.is_cancelled());
    assert_eq!(
        control_rx.recv().expect("truthful terminal"),
        UiEvent::CancellationRequested { run_id: 7 }
    );
    assert_eq!(
        control_rx.recv().expect("cancellation started"),
        UiEvent::CancellationStarted { run_id: 7 }
    );
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
        admission: None,
        result: None,
        projector: None,
        delivery: execution_result_events(7, None, Ok(Ok(execution(ExitCode::Success))), false),
        cancellation: cancellation.clone(),
        durable: false,
        cancel_requested: false,
        content_store: Default::default(),
    };
    let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    commands.send(UiCommand::Shutdown).expect("shutdown");

    assert_eq!(
        advance_pending_delivery(&mut run, &mut command_rx, &sink, &mut false, &None),
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
        admission: None,
        result: None,
        projector: None,
        delivery: execution_result_events(7, None, Ok(Ok(execution(ExitCode::Success))), false),
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
        advance_pending_delivery(&mut run, &mut command_rx, &sink, &mut false, &None),
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
        advance_pending_delivery(&mut run, &mut command_rx, &sink, &mut false, &None),
        PendingDeliveryStep::Complete
    );
    assert_eq!(
        control_rx.recv().expect("cancellation requested"),
        UiEvent::CancellationRequested { run_id: 7 }
    );
    assert_eq!(
        control_rx.recv().expect("cancellation started"),
        UiEvent::CancellationStarted { run_id: 7 }
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
        admission: None,
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
    send_cancel_result(1, None, Ok(Ok(execution(ExitCode::Success))), &sink);
    assert_eq!(
        control_rx.recv().expect("terminal event"),
        UiEvent::RunCompleted { run_id: 1 }
    );
}

#[test]
fn cancellation_terminal_result_emits_cancelled() {
    let (sink, control_rx, _data_rx) = sink();
    send_cancel_result(1, None, Ok(Ok(execution(ExitCode::Cancelled))), &sink);
    assert_eq!(
        control_rx.recv().expect("terminal event"),
        UiEvent::RunCancelled { run_id: 1 }
    );
}
