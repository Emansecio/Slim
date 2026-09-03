use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{InteractionRequestId, UiEvent};
use slim_tui::app::{ActivityPhase, AppState, FrameClock};
use slim_tui::block::{BlockKind, BlockLifecycle};
use slim_tui::reducer::{reduce, Action};
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn caps(reduced_motion: bool) -> Capabilities {
    Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion,
    }
}

fn render_state(state: &AppState, capabilities: Capabilities) -> Vec<String> {
    render_state_at(state, capabilities, 80, 24)
}

fn render_state_at(
    state: &AppState,
    capabilities: Capabilities,
    width: u16,
    height: u16,
) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut cache = WrapCache::default();
    terminal
        .draw(|frame| render_frame(frame, state, capabilities, &mut cache))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect()
        })
        .collect()
}

#[test]
fn activity_projects_known_phase_and_elapsed() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::Tick(FrameClock {
            frame: 1,
            elapsed_ms: 1_000,
        }),
    );
    reduce(&mut state, Action::UiEventReceived(UiEvent::run_started(1)));
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::ThinkingStarted),
    );
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::ThinkingDelta {
            text: "plan".into(),
        }),
    );
    reduce(
        &mut state,
        Action::Tick(FrameClock {
            frame: 31,
            elapsed_ms: 3_500,
        }),
    );

    let frame = render_state(&state, caps(false)).join("\n");
    assert!(frame.contains("Thinking"), "{frame}");
    assert!(frame.contains("2s"), "{frame}");
}

#[test]
fn explicit_thinking_end_completes_the_block_and_waits_for_provider() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "inspect first".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);

    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::AwaitingProvider)
    ));
    assert!(matches!(
        state.blocks().last().map(|block| (&block.lifecycle, block.kind())),
        Some((BlockLifecycle::Complete, BlockKind::Thinking(text))) if text == "inspect first"
    ));
    let frame = render_state(&state, caps(false)).join("\n");
    assert!(frame.contains("Waiting for provider"), "{frame}");
}

#[test]
fn empty_thinking_lifecycle_still_waits_for_provider() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingEnded);

    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::AwaitingProvider)
    ));
    assert!(state.blocks().is_empty());
}

#[test]
fn thinking_delta_without_start_is_diagnostic_and_requests_resync() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingDelta {
        text: "orphaned".into(),
    });

    assert!(state.snapshot_resync_needed());
    assert!(state.activity.is_none());
    assert!(state
        .blocks()
        .iter()
        .all(|block| !matches!(block.kind(), BlockKind::Thinking(_))));
    assert!(matches!(
        state.blocks().last().map(|block| block.kind()),
        Some(BlockKind::Error(message)) if message.contains("reasoning stream gap")
    ));
}

#[test]
fn provider_gap_after_tool_is_not_reported_as_responding() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ProviderPhaseChanged {
        phase: slim_core::ProviderPhase::FirstSemantic,
        label: "Provider responding".into(),
        elapsed_ms: 1,
    });
    assert!(state.activity.is_none());
    state.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("call".into()),
        name: "read".into(),
        arguments_summary: String::new(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("call".into()),
        name: "read".into(),
        success: true,
        duration_ms: 1,
    });
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::AwaitingProvider)
    ));

    state.apply_event(UiEvent::ThinkingStarted);
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::Thinking)
    ));
    state.apply_event(UiEvent::ThinkingEnded);
    state.apply_event(UiEvent::AssistantDelta {
        text: "answer".into(),
    });
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::Responding)
    ));
}

#[test]
fn named_tool_fragment_keeps_a_late_redaction_tail_in_the_same_assistant_block() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta {
        text: "I will inspect it".into(),
    });
    assert!(matches!(
        state.blocks().last().map(|block| block.lifecycle),
        Some(BlockLifecycle::Streaming)
    ));

    state.apply_event(UiEvent::ProviderPhaseChanged {
        phase: slim_core::ProviderPhase::PreparingTool,
        label: "Preparing tool · shell".into(),
        elapsed_ms: 0,
    });

    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::External(label)) if label == "Preparing tool · shell"
    ));
    state.apply_event(UiEvent::AssistantDelta { text: ".".into() });
    let assistants = state
        .blocks()
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::Assistant(text) => Some((text.as_str(), block.lifecycle)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistants,
        vec![("I will inspect it.", BlockLifecycle::Streaming)]
    );
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::External(label)) if label == "Preparing tool · shell"
    ));

    state.apply_event(UiEvent::AssistantEnded);
    assert!(matches!(
        state.blocks().last().map(|block| block.lifecycle),
        Some(BlockLifecycle::Complete)
    ));
}

#[test]
fn thinking_boundaries_and_deltas_cannot_revive_a_terminal_run() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunCancelled { run_id: 7 });
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "late".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);

    assert!(!state.working);
    assert!(state.activity.is_none());
    assert!(matches!(
        state.blocks().last().map(|block| (&block.lifecycle, block.kind())),
        Some((BlockLifecycle::Cancelled, BlockKind::Thinking(text))) if text == "late"
    ));
}

#[test]
fn phase_changes_reset_elapsed_but_repeated_deltas_do_not() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::Tick(FrameClock {
            frame: 1,
            elapsed_ms: 1_000,
        }),
    );
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta { text: "a".into() });
    reduce(
        &mut state,
        Action::Tick(FrameClock {
            frame: 2,
            elapsed_ms: 2_000,
        }),
    );
    state.apply_event(UiEvent::ThinkingDelta { text: "b".into() });
    assert_eq!(state.activity.as_ref().expect("thinking").started_ms, 1_000);

    state.apply_event(UiEvent::ThinkingEnded);
    state.apply_event(UiEvent::AssistantDelta {
        text: "answer".into(),
    });
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::Responding)
    ));
    assert_eq!(
        state.activity.as_ref().expect("responding").started_ms,
        2_000
    );

    state.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("call".into()),
        name: "read".into(),
        arguments_summary: String::new(),
    });
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::RunningTool(name)) if name == "read"
    ));
    state.apply_event(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("call".into()),
        name: "read".into(),
        success: true,
        duration_ms: 1,
    });
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::AwaitingProvider)
    ));
    state.apply_event(UiEvent::InputRequired {
        request_id: InteractionRequestId("input-activity".into()),
        prompt: "choose".into(),
        options: Vec::new(),
        persisted: false,
    });
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::WaitingForInput)
    ));
    state.apply_event(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("missing".into()),
        name: "missing".into(),
        success: true,
        duration_ms: 1,
    });
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::WaitingForInput)
    ));
}

#[test]
fn no_color_uses_ascii_lifecycle_glyphs_without_reflow() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("call".into()),
        name: "read".into(),
        arguments_summary: String::new(),
    });
    let color = caps(false);
    let none = Capabilities {
        color_depth: ColorDepth::None,
        ..color
    };
    let moving = render_state(&state, color);
    let plain = render_state(&state, none);
    let color_text = moving.join("\n");
    let plain_text = plain.join("\n");
    assert!(
        color_text.contains('◒') || color_text.contains('◓'),
        "{color_text}"
    );
    assert!(plain_text.contains('~'), "{plain_text}");
    assert!(
        !plain_text.contains('◒') && !plain_text.contains('◓'),
        "{plain_text}"
    );
    assert_eq!(moving.len(), plain.len());
    assert_eq!(
        moving
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()))
            .collect::<Vec<_>>(),
        plain
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn visible_activity_rail_prevents_working_footer_duplication() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    let frame = render_state(&state, caps(false)).join("\n");
    assert_eq!(frame.matches("Working").count(), 1, "{frame}");
    assert!(frame.contains("Ctrl+C cancel"), "{frame}");
}

#[test]
fn reduced_motion_removes_caret_without_reflow() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta {
        text: "streaming answer".into(),
    });
    let moving = render_state(&state, caps(false));
    let reduced = render_state(&state, caps(true));

    assert!(moving.join("\n").contains('▌'));
    assert!(!reduced.join("\n").contains('▌'));
    assert_eq!(moving.len(), reduced.len());
    assert_eq!(
        moving
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()))
            .collect::<Vec<_>>(),
        reduced
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn activity_updates_replace_state_without_appending_transcript() {
    let mut state = AppState::new();
    for label in ["searching", "waiting"] {
        state.apply_event(UiEvent::ActivityChanged {
            label: label.into(),
        });
    }

    assert!(state.blocks().is_empty());
    assert!(matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(ActivityPhase::External(label)) if label == "waiting"
    ));
}

#[test]
fn working_elapsed_is_anchored_to_run_start() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::Tick(FrameClock {
            frame: 60,
            elapsed_ms: 5_000,
        }),
    );
    state.apply_event(UiEvent::run_started(1));
    reduce(
        &mut state,
        Action::Tick(FrameClock {
            frame: 96,
            elapsed_ms: 8_000,
        }),
    );

    let buffer = render_state(&state, caps(false)).join("\n");
    assert!(buffer.contains("Working · 3s"));
}

#[test]
fn terminal_events_close_streaming_lifecycles_and_activity() {
    for terminal in [
        UiEvent::RunCompleted { run_id: 1 },
        UiEvent::RunStopped {
            run_id: 1,
            message: "stopped".into(),
        },
        UiEvent::RunFailed {
            run_id: Some(1),
            message: "failed".into(),
        },
        UiEvent::FatalError {
            run_id: Some(1),
            message: "fatal".into(),
        },
    ] {
        let mut state = AppState::new();
        state.apply_event(UiEvent::run_started(1));
        state.apply_event(UiEvent::ThinkingDelta {
            text: "thought".into(),
        });
        state.apply_event(UiEvent::AssistantDelta {
            text: "answer".into(),
        });
        state.apply_event(UiEvent::ToolStarted {
            batch_id: slim_tui::api::ToolBatchId("batch".into()),
            call_id: slim_tui::api::ToolCallId("shell".into()),
            name: "shell".into(),
            arguments_summary: String::new(),
        });
        let status_revision = state.revisions.status;
        state.apply_event(terminal);

        assert!(state.revisions.status > status_revision);
        assert!(!state.working);
        assert!(state.activity.is_none());
        assert!(state.blocks().iter().all(|block| {
            !matches!(
                block.kind(),
                BlockKind::Assistant(_) | BlockKind::Thinking(_) | BlockKind::Tool(_)
            ) || block.lifecycle != BlockLifecycle::Streaming
        }));
    }
}

#[test]
fn late_activity_transitions_after_cancel_are_ignored() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::RunCancelled { run_id: 1 });
    state.apply_event(UiEvent::ActivityChanged {
        label: "late".into(),
    });
    state.apply_event(UiEvent::InputRequired {
        request_id: InteractionRequestId("late-input".into()),
        prompt: "late".into(),
        options: Vec::new(),
        persisted: false,
    });

    assert!(!state.working);
    assert!(state.activity.is_none());
}

#[test]
fn expedited_terminal_rejects_the_same_queued_run_start() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunCancelled { run_id: 7 });
    state.apply_event(UiEvent::run_started(7));
    state.apply_event(UiEvent::AssistantDelta {
        text: "late".into(),
    });

    assert!(!state.working);
    assert!(state.activity.is_none());
    assert!(matches!(
        state.blocks().last().map(|block| (&block.lifecycle, block.kind())),
        Some((BlockLifecycle::Cancelled, BlockKind::Assistant(text))) if text == "late"
    ));

    state.apply_event(UiEvent::run_started(8));
    assert!(state.working);
}

#[test]
fn first_equal_run_terminal_outcome_is_immutable() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::FatalError {
        run_id: Some(7),
        message: "fatal".into(),
    });
    state.apply_event(UiEvent::RunStopped {
        run_id: 7,
        message: "later stop".into(),
    });
    state.apply_event(UiEvent::run_started(7));
    state.apply_event(UiEvent::AssistantDelta {
        text: "failed tail".into(),
    });

    assert!(!state.working);
    assert!(matches!(
        state.blocks().last().map(|block| (&block.lifecycle, block.kind())),
        Some((BlockLifecycle::Failed, BlockKind::Assistant(text))) if text == "failed tail"
    ));
    assert!(!state
        .notifications
        .iter()
        .any(|notice| notice == "later stop"));
}

#[test]
fn newer_expedited_terminal_supersedes_older_terminal_tail() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    state.apply_event(UiEvent::RunCancelled { run_id: 2 });
    state.apply_event(UiEvent::run_started(2));
    state.apply_event(UiEvent::AssistantDelta {
        text: "late".into(),
    });

    assert!(!state.working);
    assert!(state.activity.is_none());
    assert!(matches!(
        state.blocks().last().map(|block| (&block.lifecycle, block.kind())),
        Some((BlockLifecycle::Cancelled, BlockKind::Assistant(text))) if text == "late"
    ));

    state.apply_event(UiEvent::run_started(3));
    assert!(state.working);
}

#[test]
fn expedited_terminal_tail_preserves_truthful_outcome() {
    for (terminal, expected) in [
        (
            UiEvent::RunCompleted { run_id: 7 },
            BlockLifecycle::Complete,
        ),
        (
            UiEvent::RunFailed {
                run_id: Some(7),
                message: "failed".into(),
            },
            BlockLifecycle::Failed,
        ),
        (
            UiEvent::RunStopped {
                run_id: 7,
                message: "stopped".into(),
            },
            BlockLifecycle::Cancelled,
        ),
        (
            UiEvent::RunCancelled { run_id: 7 },
            BlockLifecycle::Cancelled,
        ),
    ] {
        let mut state = AppState::new();
        state.apply_event(terminal);
        state.apply_event(UiEvent::run_started(7));
        state.apply_event(UiEvent::AssistantDelta {
            text: "tail".into(),
        });
        state.apply_event(UiEvent::ToolStarted {
            batch_id: slim_tui::api::ToolBatchId("batch".into()),
            call_id: slim_tui::api::ToolCallId("late-tool".into()),
            name: "late-tool".into(),
            arguments_summary: String::new(),
        });

        let tail = &state.blocks()[state.blocks().len() - 2..];
        assert!(tail.iter().all(|block| block.lifecycle == expected));
        assert!(!state.working);
        assert!(state.activity.is_none());
    }
}

#[test]
fn new_run_output_never_reopens_a_terminal_block() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta { text: "old".into() });
    state.apply_event(UiEvent::RunCancelled { run_id: 1 });
    state.apply_event(UiEvent::run_started(2));
    state.apply_event(UiEvent::AssistantDelta { text: "new".into() });

    let assistants: Vec<_> = state
        .blocks()
        .iter()
        .filter(|block| matches!(block.kind(), BlockKind::Assistant(_)))
        .collect();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].lifecycle, BlockLifecycle::Cancelled);
    assert!(matches!(assistants[0].kind(), BlockKind::Assistant(text) if text == "old"));
    assert_eq!(assistants[1].lifecycle, BlockLifecycle::Streaming);
    assert!(matches!(assistants[1].kind(), BlockKind::Assistant(text) if text == "new"));
}

#[test]
fn late_stream_deltas_after_cancel_stay_cancelled_without_activity() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunCancelled { run_id: 1 });
    state.apply_event(UiEvent::AssistantDelta {
        text: "late".into(),
    });
    state.apply_event(UiEvent::ThinkingDelta {
        text: "late thought".into(),
    });

    assert!(state.activity.is_none());
    assert!(state.blocks().iter().all(|block| {
        matches!(
            block.kind(),
            BlockKind::Assistant(_) | BlockKind::Thinking(_)
        ) && block.lifecycle == BlockLifecycle::Cancelled
    }));
}

#[test]
fn terminal_run_events_clear_transient_activity() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta { text: "x".into() });
    assert!(state.activity.is_some());
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    assert!(state.activity.is_none());
}

#[test]
fn turn_budget_warns_once_at_eighty_percent() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started_with_budget(1, 32, 96, 5));
    for request_id in 1..=4 {
        state.apply_event(UiEvent::UsageEstimateForRun {
            run_id: 1,
            request_id,
            context_tokens: 100,
            context_window_tokens: 128_000,
        });
    }
    let warnings: Vec<_> = state
        .notifications
        .iter()
        .filter(|notice| notice.message.starts_with("Turn budget:"))
        .collect();
    assert_eq!(warnings.len(), 1, "{:?}", state.notifications);
    assert!(warnings[0].message.contains("4/5"));
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 1,
        request_id: 5,
        context_tokens: 120,
        context_window_tokens: 128_000,
    });
    assert_eq!(
        state
            .notifications
            .iter()
            .filter(|notice| notice.message.starts_with("Turn budget:"))
            .count(),
        1
    );
}

#[test]
fn this_turn_tool_budget_warns_at_eighty_percent_and_resets_on_next_turn() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started_with_budget(1, 32, 5, 128));
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 1,
        request_id: 1,
        context_tokens: 100,
        context_window_tokens: 128_000,
    });
    for index in 0..4 {
        let call_id = slim_tui::api::ToolCallId(format!("call-{index}").into());
        state.apply_event(UiEvent::ToolStarted {
            batch_id: slim_tui::api::ToolBatchId("batch".into()),
            call_id: call_id.clone(),
            name: "read".into(),
            arguments_summary: String::new(),
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id: slim_tui::api::ToolBatchId("batch".into()),
            call_id,
            name: "read".into(),
            success: true,
            duration_ms: 1,
        });
    }
    let warnings: Vec<_> = state
        .notifications
        .iter()
        .filter(|notice| notice.message.starts_with("Tool budget:"))
        .collect();
    assert_eq!(warnings.len(), 1, "{:?}", state.notifications);
    assert!(warnings[0].message.contains("read 4/5"));
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 1,
        request_id: 2,
        context_tokens: 140,
        context_window_tokens: 128_000,
    });
    assert_eq!(state.tools_used_read, 0);
    let call_id = slim_tui::api::ToolCallId("next-turn".into());
    state.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch-2".into()),
        call_id: call_id.clone(),
        name: "read".into(),
        arguments_summary: String::new(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("batch-2".into()),
        call_id,
        name: "read".into(),
        success: true,
        duration_ms: 1,
    });
    assert_eq!(state.tools_used_read, 1);
    assert_eq!(
        state
            .notifications
            .iter()
            .filter(|notice| notice.message.starts_with("Tool budget:"))
            .count(),
        1,
        "next turn must not warn until 80% of this-turn batch"
    );
}
