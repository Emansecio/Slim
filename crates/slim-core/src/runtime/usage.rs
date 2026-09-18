use crate::context::CompactionStatus;
use crate::{CausalAnomalyKind, EventKind, ProviderPhase, RequestKind, SessionEvent};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestUsage {
    pub request_kind: RequestKind,
    pub provider: String,
    pub model: String,
    pub uncached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub usage_unknown: bool,
    pub response_cache_hit: bool,
    pub system_bytes: u64,
    pub tool_schema_bytes: u64,
    pub history_bytes: u64,
    pub tool_result_bytes: u64,
    pub provider_latency_ms: u64,
    pub time_to_first_byte_ms: u64,
    pub time_to_first_semantic_ms: u64,
    pub tool_latency_ms: u64,
    pub retry_count: u64,
    pub cancelled: bool,
    pub estimated_input_tokens: u64,
    pub estimation_error_tokens: i64,
    pub failed: bool,
}

impl RequestUsage {
    pub fn total_input_tokens(&self) -> u64 {
        input_token_total(
            self.uncached_input_tokens,
            self.cache_write_tokens,
            self.cache_read_tokens,
        )
        .0
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens().saturating_add(self.output_tokens)
    }
    fn input_tokens_overflowed(&self) -> bool {
        input_token_total(
            self.uncached_input_tokens,
            self.cache_write_tokens,
            self.cache_read_tokens,
        )
        .1
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageTotals {
    pub requests: Vec<RequestUsage>,
    pub uncached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub usage_unknown: bool,
    pub system_bytes: u64,
    pub tool_schema_bytes: u64,
    pub history_bytes: u64,
    pub tool_result_bytes: u64,
    pub provider_latency_ms: u64,
    pub tool_latency_ms: u64,
    pub retry_count: u64,
    pub cancelled_requests: u64,
    pub provider_turns: u64,
    #[serde(default)]
    pub jev_decisions: u64,
    #[serde(default)]
    pub jev_http_attempts: u64,
    #[serde(default)]
    pub jev_input_tokens: u64,
    #[serde(default)]
    pub jev_output_tokens: u64,
    #[serde(default)]
    pub jev_latency_ms: u64,
    #[serde(default)]
    pub jev_usage_unknown: bool,
    pub tool_calls_executed: u64,
    pub tool_calls_reused: u64,
    pub tool_calls_suppressed: u64,
    pub no_progress_turns: u64,
    pub no_progress_tokens: u64,
    pub duplicate_evidence_bytes_avoided: u64,
    pub compaction_input_tokens: u64,
    pub compaction_output_tokens: u64,
    pub compaction_tokens_saved: u64,
    pub post_compaction_reacquisitions: u64,
    pub estimation_error_tokens: i64,
    pub validated_completion: bool,
    pub overflowed: bool,
}

#[derive(Default)]
struct OpenRequest {
    usage: RequestUsage,
    fallback_input_tokens: u64,
    fallback_output_tokens: u64,
    saw_breakdown: bool,
    saw_terminal_usage: bool,
    saw_partial_usage: bool,
    input_usage_known: bool,
    output_usage_known: bool,
    no_progress: bool,
}

impl UsageTotals {
    pub fn from_events(events: &[SessionEvent], validated_completion: bool) -> Self {
        let mut totals = Self {
            validated_completion,
            ..Self::default()
        };
        let mut open: Option<OpenRequest> = None;
        let mut background_compaction: Option<RequestUsage> = None;
        let mut next_provider_retry_count = 0;

        for event in events {
            match &event.kind {
                EventKind::ContextSnapshot {
                    request_kind,
                    provider,
                    model,
                    system_bytes,
                    tool_schema_bytes,
                    history_bytes,
                    tool_result_bytes,
                    serialized_chars: _,
                    estimated_tokens,
                    context_window_tokens: _,
                } => {
                    if let Some(previous) = open.take() {
                        let previous_kind = previous.usage.request_kind;
                        let retry_count = totals.finish_request(previous);
                        if previous_kind == RequestKind::ProviderTurn {
                            next_provider_retry_count = retry_count;
                        }
                    }
                    open = Some(OpenRequest {
                        usage: RequestUsage {
                            request_kind: *request_kind,
                            provider: provider.clone(),
                            model: model.clone(),
                            system_bytes: *system_bytes,
                            tool_schema_bytes: *tool_schema_bytes,
                            history_bytes: *history_bytes,
                            tool_result_bytes: *tool_result_bytes,
                            estimated_input_tokens: *estimated_tokens,
                            retry_count: if *request_kind == RequestKind::ProviderTurn {
                                next_provider_retry_count
                            } else {
                                0
                            },
                            ..RequestUsage::default()
                        },
                        ..OpenRequest::default()
                    });
                }
                EventKind::JevDecisionCompleted {
                    attempts,
                    model,
                    input_tokens,
                    output_tokens,
                    state_bytes,
                    duration_ms,
                    cancelled,
                    failed,
                    ..
                } => {
                    if let Some(previous) = open.take() {
                        next_provider_retry_count = totals.finish_request(previous);
                    }
                    add(
                        &mut totals.jev_http_attempts,
                        u64::from(*attempts),
                        &mut totals.overflowed,
                    );
                    totals.push_jev_request(RequestUsage {
                        request_kind: RequestKind::JevDecision,
                        provider: "typesafe".into(),
                        model: model.clone(),
                        uncached_input_tokens: input_tokens.unwrap_or_default(),
                        output_tokens: output_tokens.unwrap_or_default(),
                        usage_unknown: *attempts > 0
                            && (input_tokens.is_none() || output_tokens.is_none() || *attempts > 1),
                        history_bytes: *state_bytes,
                        provider_latency_ms: *duration_ms,
                        retry_count: u64::from(attempts.saturating_sub(1)),
                        cancelled: *cancelled,
                        failed: *failed,
                        ..RequestUsage::default()
                    });
                }
                EventKind::JevActionRejected { .. } => {
                    // The batch never executed, but the provider request did:
                    // classify it as a failed attempt without duplicating it.
                    match &mut open {
                        Some(request) => request.usage.failed = true,
                        None => {
                            if let Some(request) =
                                totals.requests.iter_mut().rev().find(|request| {
                                    request.request_kind == RequestKind::ProviderTurn
                                })
                            {
                                request.failed = true;
                            }
                        }
                    }
                }
                EventKind::UsageBreakdown { usage } => {
                    if let Some(request) = &mut open {
                        request.saw_breakdown = true;
                        add(
                            &mut request.usage.uncached_input_tokens,
                            usage.uncached_input_tokens,
                            &mut totals.overflowed,
                        );
                        add(
                            &mut request.usage.cache_write_tokens,
                            usage.cache_write_tokens,
                            &mut totals.overflowed,
                        );
                        add(
                            &mut request.usage.cache_read_tokens,
                            usage.cache_read_tokens,
                            &mut totals.overflowed,
                        );
                        add(
                            &mut request.usage.output_tokens,
                            usage.output_tokens,
                            &mut totals.overflowed,
                        );
                        add(
                            &mut request.usage.reasoning_tokens,
                            usage.reasoning_tokens,
                            &mut totals.overflowed,
                        );
                        request.usage.usage_unknown |= usage.usage_unknown;
                    }
                }
                EventKind::UsagePartial {
                    input_tokens,
                    output_tokens,
                    input_known,
                    output_known,
                } => {
                    if let Some(request) = &mut open {
                        add(
                            &mut request.fallback_input_tokens,
                            *input_tokens,
                            &mut totals.overflowed,
                        );
                        add(
                            &mut request.fallback_output_tokens,
                            *output_tokens,
                            &mut totals.overflowed,
                        );
                        request.saw_partial_usage = true;
                        request.input_usage_known |= *input_known;
                        request.output_usage_known |= *output_known;
                    }
                }
                EventKind::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    if let Some(request) = &mut open {
                        add(
                            &mut request.fallback_input_tokens,
                            *input_tokens,
                            &mut totals.overflowed,
                        );
                        add(
                            &mut request.fallback_output_tokens,
                            *output_tokens,
                            &mut totals.overflowed,
                        );
                        if !request.saw_partial_usage {
                            request.input_usage_known = true;
                            request.output_usage_known = true;
                        }
                        request.saw_terminal_usage = true;
                    }
                }
                EventKind::ResponseCacheHit => {
                    if let Some(request) = &mut open {
                        request.usage.response_cache_hit = true;
                        request.saw_terminal_usage = true;
                        request.input_usage_known = true;
                        request.output_usage_known = true;
                    }
                }
                EventKind::ProviderPhase {
                    phase, elapsed_ms, ..
                } => {
                    if let Some(request) = &mut open {
                        match phase {
                            ProviderPhase::FirstByte => {
                                request.usage.time_to_first_byte_ms = *elapsed_ms
                            }
                            ProviderPhase::FirstSemantic => {
                                request.usage.time_to_first_semantic_ms = *elapsed_ms
                            }
                            _ => {}
                        }
                    }
                }
                EventKind::RequestCompleted {
                    provider_latency_ms,
                    cancelled,
                    failed,
                } => {
                    if let Some(request) = &mut open {
                        request.usage.provider_latency_ms = *provider_latency_ms;
                        request.usage.cancelled = *cancelled;
                        request.usage.failed = *failed;
                    }
                }
                EventKind::ToolFinished { duration_ms, .. } => {
                    add(&mut totals.tool_calls_executed, 1, &mut totals.overflowed);
                    add(
                        &mut totals.tool_latency_ms,
                        *duration_ms,
                        &mut totals.overflowed,
                    );
                    if let Some(request) = &mut open {
                        add(
                            &mut request.usage.tool_latency_ms,
                            *duration_ms,
                            &mut totals.overflowed,
                        );
                    }
                }
                EventKind::ToolEvidenceReused {
                    original_bytes,
                    emitted_bytes,
                    post_compaction,
                } => {
                    add(
                        &mut totals.duplicate_evidence_bytes_avoided,
                        original_bytes.saturating_sub(*emitted_bytes),
                        &mut totals.overflowed,
                    );
                    if *post_compaction {
                        add(
                            &mut totals.post_compaction_reacquisitions,
                            1,
                            &mut totals.overflowed,
                        );
                    }
                }
                EventKind::ToolEvidenceElided {
                    original_bytes,
                    emitted_bytes,
                    ..
                } => add(
                    &mut totals.duplicate_evidence_bytes_avoided,
                    original_bytes.saturating_sub(*emitted_bytes),
                    &mut totals.overflowed,
                ),
                EventKind::ToolCallsSuppressed { count } => add(
                    &mut totals.tool_calls_suppressed,
                    *count,
                    &mut totals.overflowed,
                ),
                EventKind::CausalAnomalyDetected {
                    kind: CausalAnomalyKind::StagnantTurn | CausalAnomalyKind::NoProgressCandidate,
                    ..
                } => {
                    add(&mut totals.no_progress_turns, 1, &mut totals.overflowed);
                    if let Some(request) = &mut open {
                        request.no_progress = true;
                    }
                }
                EventKind::CompactionAttemptStarted {
                    provider,
                    model,
                    system_bytes,
                    history_bytes,
                    estimated_input_tokens,
                    ..
                } => {
                    if let Some(mut unfinished) = background_compaction.take() {
                        unfinished.usage_unknown = true;
                        unfinished.failed = true;
                        totals.push_compaction_request(unfinished);
                    }
                    background_compaction = Some(RequestUsage {
                        request_kind: RequestKind::Compaction,
                        provider: provider.clone(),
                        model: model.clone(),
                        system_bytes: *system_bytes,
                        history_bytes: *history_bytes,
                        estimated_input_tokens: *estimated_input_tokens,
                        ..RequestUsage::default()
                    });
                }
                EventKind::CompactionAttemptCompleted {
                    uncached_input_tokens,
                    cache_write_tokens,
                    cache_read_tokens,
                    output_tokens,
                    reasoning_tokens,
                    time_to_first_byte_ms,
                    time_to_first_semantic_ms,
                    duration_ms,
                    usage_known,
                } => {
                    let mut request = background_compaction.take().unwrap_or(RequestUsage {
                        request_kind: RequestKind::Compaction,
                        ..RequestUsage::default()
                    });
                    request.uncached_input_tokens = *uncached_input_tokens;
                    request.cache_write_tokens = *cache_write_tokens;
                    request.cache_read_tokens = *cache_read_tokens;
                    request.output_tokens = *output_tokens;
                    request.reasoning_tokens = *reasoning_tokens;
                    request.usage_unknown = !*usage_known;
                    request.time_to_first_byte_ms = *time_to_first_byte_ms;
                    request.time_to_first_semantic_ms = *time_to_first_semantic_ms;
                    request.provider_latency_ms = *duration_ms;
                    if !request.usage_unknown {
                        request.estimation_error_tokens = signed_difference(
                            request.estimated_input_tokens,
                            request.total_input_tokens(),
                        );
                    }
                    totals.push_compaction_request(request);
                }
                EventKind::CompactionAttemptCancelled {
                    request_bytes,
                    estimated_input_tokens,
                    time_to_first_byte_ms,
                    time_to_first_semantic_ms,
                    duration_ms,
                    ..
                } => {
                    let mut request = background_compaction.take().unwrap_or(RequestUsage {
                        request_kind: RequestKind::Compaction,
                        history_bytes: *request_bytes,
                        estimated_input_tokens: *estimated_input_tokens,
                        ..RequestUsage::default()
                    });
                    request.usage_unknown = true;
                    request.time_to_first_byte_ms = *time_to_first_byte_ms;
                    request.time_to_first_semantic_ms = *time_to_first_semantic_ms;
                    request.provider_latency_ms = *duration_ms;
                    request.cancelled = true;
                    request.failed = true;
                    totals.push_compaction_request(request);
                }
                EventKind::CompactionUsageUnknown { .. } => totals.usage_unknown = true,
                EventKind::CompactionState {
                    state,
                    tokens_before,
                    tokens_after,
                    ..
                } if *state == CompactionStatus::Applied => add(
                    &mut totals.compaction_tokens_saved,
                    tokens_before.saturating_sub(*tokens_after),
                    &mut totals.overflowed,
                ),
                EventKind::CompactionState { state, .. }
                    if *state == CompactionStatus::Discarded =>
                {
                    if let Some(request) = totals
                        .requests
                        .iter_mut()
                        .rev()
                        .find(|request| request.request_kind == RequestKind::Compaction)
                    {
                        request.failed = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(request) = open {
            totals.finish_request(request);
        }
        if let Some(mut request) = background_compaction {
            request.usage_unknown = true;
            request.failed = true;
            totals.push_compaction_request(request);
        }
        totals.mark_input_overflow();
        totals
    }

    pub fn total_input_tokens(&self) -> u64 {
        input_token_total(
            self.uncached_input_tokens,
            self.cache_write_tokens,
            self.cache_read_tokens,
        )
        .0
    }

    fn mark_input_overflow(&mut self) {
        let totals_overflowed = input_token_total(
            self.uncached_input_tokens,
            self.cache_write_tokens,
            self.cache_read_tokens,
        )
        .1;
        let request_overflowed = self
            .requests
            .iter()
            .any(RequestUsage::input_tokens_overflowed);
        self.overflowed |= totals_overflowed || request_overflowed;
    }

    pub(super) fn from_compaction_request(mut request: RequestUsage) -> Self {
        if !request.usage_unknown {
            request.estimation_error_tokens =
                signed_difference(request.estimated_input_tokens, request.total_input_tokens());
        }
        let mut totals = Self::default();
        totals.push_compaction_request(request);
        totals
    }

    pub(super) fn add(&mut self, input_tokens: u64, output_tokens: u64) {
        add(
            &mut self.uncached_input_tokens,
            input_tokens,
            &mut self.overflowed,
        );
        add(&mut self.output_tokens, output_tokens, &mut self.overflowed);
        self.mark_input_overflow();
    }

    pub(super) fn add_breakdown(&mut self, usage: crate::UsageBreakdown) {
        add(
            &mut self.uncached_input_tokens,
            usage.uncached_input_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.cache_write_tokens,
            usage.cache_write_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.cache_read_tokens,
            usage.cache_read_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.output_tokens,
            usage.output_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.reasoning_tokens,
            usage.reasoning_tokens,
            &mut self.overflowed,
        );
        self.usage_unknown |= usage.usage_unknown;
        self.mark_input_overflow();
    }

    fn push_jev_request(&mut self, request: RequestUsage) {
        add(&mut self.jev_decisions, 1, &mut self.overflowed);
        add(
            &mut self.jev_input_tokens,
            request.total_input_tokens(),
            &mut self.overflowed,
        );
        add(
            &mut self.jev_output_tokens,
            request.output_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.jev_latency_ms,
            request.provider_latency_ms,
            &mut self.overflowed,
        );
        self.jev_usage_unknown |= request.usage_unknown;
        self.usage_unknown |= request.usage_unknown;
        if request.cancelled {
            add(&mut self.cancelled_requests, 1, &mut self.overflowed);
        }
        self.requests.push(request);
    }

    fn finish_request(&mut self, mut open: OpenRequest) -> u64 {
        if !open.saw_breakdown {
            open.usage.uncached_input_tokens = open.fallback_input_tokens;
            open.usage.output_tokens = open.fallback_output_tokens;
        }
        open.usage.usage_unknown |=
            !open.saw_terminal_usage || !open.input_usage_known || !open.output_usage_known;
        if open.usage.request_kind != RequestKind::JevDecision
            && !open.usage.usage_unknown
            && !open.usage.response_cache_hit
        {
            open.usage.estimation_error_tokens = signed_difference(
                open.usage.estimated_input_tokens,
                open.usage.total_input_tokens(),
            );
        }
        let retry = if open.usage.failed || open.usage.cancelled {
            open.usage.retry_count.saturating_add(1)
        } else {
            0
        };
        if open.usage.request_kind == RequestKind::Compaction {
            self.push_compaction_request(open.usage);
            return retry;
        }
        if open.usage.request_kind == RequestKind::JevDecision {
            self.push_jev_request(open.usage);
            return retry;
        }
        add(&mut self.provider_turns, 1, &mut self.overflowed);
        add(
            &mut self.uncached_input_tokens,
            open.usage.uncached_input_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.cache_write_tokens,
            open.usage.cache_write_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.cache_read_tokens,
            open.usage.cache_read_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.output_tokens,
            open.usage.output_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.reasoning_tokens,
            open.usage.reasoning_tokens,
            &mut self.overflowed,
        );
        add(
            &mut self.system_bytes,
            open.usage.system_bytes,
            &mut self.overflowed,
        );
        add(
            &mut self.tool_schema_bytes,
            open.usage.tool_schema_bytes,
            &mut self.overflowed,
        );
        add(
            &mut self.history_bytes,
            open.usage.history_bytes,
            &mut self.overflowed,
        );
        add(
            &mut self.tool_result_bytes,
            open.usage.tool_result_bytes,
            &mut self.overflowed,
        );
        add(
            &mut self.provider_latency_ms,
            open.usage.provider_latency_ms,
            &mut self.overflowed,
        );
        self.retry_count = self.retry_count.max(open.usage.retry_count);
        self.estimation_error_tokens = self
            .estimation_error_tokens
            .saturating_add(open.usage.estimation_error_tokens);
        self.usage_unknown |= open.usage.usage_unknown;
        if open.usage.cancelled {
            add(&mut self.cancelled_requests, 1, &mut self.overflowed);
        }
        if open.no_progress {
            add(
                &mut self.no_progress_tokens,
                open.usage.total_tokens(),
                &mut self.overflowed,
            );
        }
        self.requests.push(open.usage);
        retry
    }

    fn push_compaction_request(&mut self, request: RequestUsage) {
        self.overflowed |= request.input_tokens_overflowed();
        add(
            &mut self.compaction_input_tokens,
            request.total_input_tokens(),
            &mut self.overflowed,
        );
        add(
            &mut self.compaction_output_tokens,
            request.output_tokens,
            &mut self.overflowed,
        );
        self.usage_unknown |= request.usage_unknown;
        if !request.usage_unknown {
            self.estimation_error_tokens = self
                .estimation_error_tokens
                .saturating_add(request.estimation_error_tokens);
        }
        if request.cancelled {
            add(&mut self.cancelled_requests, 1, &mut self.overflowed);
        }
        self.requests.push(request);
    }
}

fn input_token_total(
    uncached_input_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens: u64,
) -> (u64, bool) {
    let total = u128::from(uncached_input_tokens)
        + u128::from(cache_write_tokens)
        + u128::from(cache_read_tokens);
    match u64::try_from(total) {
        Ok(total) => (total, false),
        Err(_) => (u64::MAX, true),
    }
}

fn add(slot: &mut u64, value: u64, overflowed: &mut bool) {
    let (sum, did_overflow) = slot.overflowing_add(value);
    *slot = if did_overflow { u64::MAX } else { sum };
    *overflowed |= did_overflow;
}

fn signed_difference(estimated: u64, actual: u64) -> i64 {
    let difference = i128::from(estimated) - i128::from(actual);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}
