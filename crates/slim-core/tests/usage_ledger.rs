use slim_core::{
    CausalAnomalyKind, CausalConfidence, CausalShadowAction, EventKind, ProviderPhase, RequestKind,
    SessionEvent, UsageBreakdown, UsageTotals,
};

#[test]
fn ledger_separates_request_cost_drivers_and_execution_outcomes() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ContextSnapshot {
                request_kind: RequestKind::ProviderTurn,
                provider: "anthropic".into(),
                model: "claude-test".into(),
                system_bytes: 10,
                tool_schema_bytes: 20,
                history_bytes: 30,
                tool_result_bytes: 40,
                serialized_chars: 350,
                estimated_tokens: 95,
                context_window_tokens: 200_000,
            },
        ),
        SessionEvent::new(
            2,
            EventKind::ProviderPhase {
                phase: ProviderPhase::FirstByte,
                elapsed_ms: 12,
                detail: None,
            },
        ),
        SessionEvent::new(
            3,
            EventKind::ProviderPhase {
                phase: ProviderPhase::FirstSemantic,
                elapsed_ms: 20,
                detail: None,
            },
        ),
        SessionEvent::new(
            4,
            EventKind::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 60,
                    cache_write_tokens: 10,
                    cache_read_tokens: 30,
                    output_tokens: 20,
                    reasoning_tokens: 5,
                    usage_unknown: false,
                },
            },
        ),
        SessionEvent::new(
            5,
            EventKind::Usage {
                input_tokens: 100,
                output_tokens: 20,
            },
        ),
        SessionEvent::new(
            6,
            EventKind::AssistantEnded {
                reason: "end_turn".into(),
            },
        ),
        SessionEvent::new(
            7,
            EventKind::RequestCompleted {
                provider_latency_ms: 50,
                cancelled: false,
                failed: false,
            },
        ),
        SessionEvent::new(
            8,
            EventKind::CompactionAttemptStarted {
                provider: "anthropic".into(),
                model: "claude-test".into(),
                system_bytes: 1,
                history_bytes: 2,
                serialized_chars: 7,
                request_bytes: 7,
                estimated_input_tokens: 5,
            },
        ),
        SessionEvent::new(
            9,
            EventKind::CompactionAttemptCompleted {
                uncached_input_tokens: 5,
                cache_write_tokens: 0,
                cache_read_tokens: 0,
                output_tokens: 2,
                reasoning_tokens: 0,
                time_to_first_byte_ms: 4,
                time_to_first_semantic_ms: 6,
                duration_ms: 10,
                usage_known: true,
            },
        ),
        SessionEvent::new(
            10,
            EventKind::ToolFinished {
                batch_id: "batch".into(),
                call_id: "call".into(),
                name: "read".into(),
                success: true,
                duration_ms: 7,
            },
        ),
        SessionEvent::new(
            11,
            EventKind::ToolEvidenceReused {
                original_bytes: 100,
                emitted_bytes: 20,
                post_compaction: true,
            },
        ),
        SessionEvent::new(12, EventKind::ToolCallsSuppressed { count: 1 }),
        SessionEvent::new(
            13,
            EventKind::CausalAnomalyDetected {
                batch_id: "batch".into(),
                call_id: "call".into(),
                kind: CausalAnomalyKind::StagnantTurn,
                tool_name: "read".into(),
                call_fingerprint: "fingerprint".into(),
                evidence_id: "".into(),
                workspace_revision: 0,
                occurrence: 1,
                confidence: CausalConfidence::High,
                action: CausalShadowAction::Observe,
            },
        ),
        SessionEvent::new(
            14,
            EventKind::CompactionState {
                state: slim_core::context::CompactionStatus::Applied,
                reason: slim_core::context::CompactionReason::SoftThreshold,
                tokens_before: 1_000,
                tokens_after: 400,
                duration_ms: 10,
            },
        ),
    ];

    let ledger = UsageTotals::from_events(&events, true);
    assert_eq!(ledger.requests.len(), 2);
    let provider_request = ledger
        .requests
        .iter()
        .find(|request| request.request_kind == RequestKind::ProviderTurn)
        .expect("provider request");
    assert_eq!(provider_request.uncached_input_tokens, 60);
    assert_eq!(provider_request.cache_write_tokens, 10);
    assert_eq!(provider_request.cache_read_tokens, 30);
    assert_eq!(provider_request.reasoning_tokens, 5);
    assert_eq!(provider_request.tool_latency_ms, 7);
    let compaction_request = ledger
        .requests
        .iter()
        .find(|request| request.request_kind == RequestKind::Compaction)
        .expect("compaction request");
    assert_eq!(compaction_request.time_to_first_byte_ms, 4);
    assert_eq!(compaction_request.time_to_first_semantic_ms, 6);
    assert_eq!(ledger.provider_turns, 1);
    assert_eq!(ledger.tool_calls_executed, 1);
    assert_eq!(ledger.tool_calls_reused, 0);
    assert_eq!(ledger.tool_calls_suppressed, 1);
    assert_eq!(ledger.no_progress_turns, 1);
    assert_eq!(ledger.no_progress_tokens, 120);
    assert_eq!(ledger.duplicate_evidence_bytes_avoided, 80);
    assert_eq!(ledger.compaction_input_tokens, 5);
    assert_eq!(ledger.compaction_output_tokens, 2);
    assert_eq!(ledger.compaction_tokens_saved, 600);
    assert_eq!(ledger.post_compaction_reacquisitions, 1);
    assert_eq!(ledger.estimation_error_tokens, -5);
    assert!(ledger.validated_completion);
}

#[test]
fn discarded_background_compaction_is_a_failed_attempt() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::CompactionAttemptStarted {
                provider: "anthropic".into(),
                model: "claude-test".into(),
                system_bytes: 1,
                history_bytes: 2,
                serialized_chars: 7,
                request_bytes: 7,
                estimated_input_tokens: 5,
            },
        ),
        SessionEvent::new(
            2,
            EventKind::CompactionAttemptCompleted {
                uncached_input_tokens: 5,
                cache_write_tokens: 0,
                cache_read_tokens: 0,
                output_tokens: 2,
                reasoning_tokens: 0,
                time_to_first_byte_ms: 0,
                time_to_first_semantic_ms: 0,
                duration_ms: 10,
                usage_known: true,
            },
        ),
        SessionEvent::new(
            3,
            EventKind::CompactionState {
                state: slim_core::context::CompactionStatus::Discarded,
                reason: slim_core::context::CompactionReason::SoftThreshold,
                tokens_before: 1_000,
                tokens_after: 0,
                duration_ms: 10,
            },
        ),
    ];

    let ledger = UsageTotals::from_events(&events, false);
    assert!(ledger.requests[0].failed);
}

#[test]
fn local_response_cache_hit_is_known_zero_provider_usage() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ContextSnapshot {
                request_kind: RequestKind::ProviderTurn,
                provider: "openai-compatible".into(),
                model: "fixture".into(),
                system_bytes: 1,
                tool_schema_bytes: 2,
                history_bytes: 3,
                tool_result_bytes: 0,
                serialized_chars: 50,
                estimated_tokens: 14,
                context_window_tokens: 1_000,
            },
        ),
        SessionEvent::new(2, EventKind::ResponseCacheHit),
        SessionEvent::new(
            3,
            EventKind::RequestCompleted {
                provider_latency_ms: 0,
                cancelled: false,
                failed: false,
            },
        ),
    ];

    let ledger = UsageTotals::from_events(&events, false);
    assert!(ledger.requests[0].response_cache_hit);
    assert!(!ledger.requests[0].usage_unknown);
    assert_eq!(ledger.requests[0].total_tokens(), 0);
    assert_eq!(ledger.requests[0].estimation_error_tokens, 0);
}

#[test]
fn cross_component_input_overflow_marks_the_ledger() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ContextSnapshot {
                request_kind: RequestKind::ProviderTurn,
                provider: "fixture".into(),
                model: "fixture".into(),
                system_bytes: 0,
                tool_schema_bytes: 0,
                history_bytes: 0,
                tool_result_bytes: 0,
                serialized_chars: 0,
                estimated_tokens: 0,
                context_window_tokens: u64::MAX,
            },
        ),
        SessionEvent::new(
            2,
            EventKind::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: u64::MAX,
                    cache_write_tokens: 1,
                    cache_read_tokens: 0,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                    usage_unknown: false,
                },
            },
        ),
        SessionEvent::new(
            3,
            EventKind::Usage {
                input_tokens: u64::MAX,
                output_tokens: 0,
            },
        ),
        SessionEvent::new(
            4,
            EventKind::RequestCompleted {
                provider_latency_ms: 0,
                cancelled: false,
                failed: false,
            },
        ),
    ];

    let ledger = UsageTotals::from_events(&events, false);

    assert!(ledger.overflowed);
    assert_eq!(ledger.total_input_tokens(), u64::MAX);
}

#[test]
fn rejected_jev_batch_is_classified_as_a_failed_attempt() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::ContextSnapshot {
                request_kind: RequestKind::ProviderTurn,
                provider: "openai-compatible".into(),
                model: "gpt-5.6-sol".into(),
                system_bytes: 1,
                tool_schema_bytes: 2,
                history_bytes: 3,
                tool_result_bytes: 4,
                serialized_chars: 10,
                estimated_tokens: 5,
                context_window_tokens: 1_000,
            },
        ),
        SessionEvent::new(
            2,
            EventKind::Usage {
                input_tokens: 10,
                output_tokens: 2,
            },
        ),
        SessionEvent::new(
            3,
            EventKind::RequestCompleted {
                provider_latency_ms: 7,
                cancelled: false,
                failed: false,
            },
        ),
        SessionEvent::new(
            4,
            EventKind::JevActionRejected {
                expected: "read".into(),
                observed: vec!["write".into()],
            },
        ),
    ];

    let ledger = UsageTotals::from_events(&events, false);

    assert_eq!(
        ledger.provider_turns, 1,
        "the attempt must not be duplicated"
    );
    assert_eq!(ledger.requests.len(), 1);
    assert!(
        ledger.requests[0].failed,
        "a batch discarded by the Jev contract is a failed attempt"
    );
    assert_eq!(ledger.requests[0].uncached_input_tokens, 10);
}
