use crate::context::CompactionStatus;
use crate::{CausalAnomalyKind, EventKind, ProviderPhase, RequestKind, SessionEvent};
use serde::{Deserialize, Serialize};

/// `add(&mut $t.$field, $value, &mut $t.overflowed)`: saturating counter bump
/// on a ledger that owns its own overflow flag.
macro_rules! bump {
    ($t:expr, $field:ident, $value:expr) => {
        add(&mut $t.$field, $value, &mut $t.overflowed)
    };
}

/// Saturating `$dst.f += $src.f` for each listed field; overflow is recorded
/// in `$flag`.
macro_rules! sum {
    ($dst:expr, $src:expr, $flag:expr; $($field:ident),+ $(,)?) => {
        $(add(&mut $dst.$field, $src.$field, $flag);)+
    };
}

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
        self.checked_input_tokens().0
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens().saturating_add(self.output_tokens)
    }

    /// Whether the request's input tokens are a measured provider figure that
    /// may train the token estimator.
    pub(crate) fn usable_for_calibration(&self) -> bool {
        !self.usage_unknown && !self.response_cache_hit && !self.cancelled && !self.failed
    }

    fn input_tokens_overflowed(&self) -> bool {
        self.checked_input_tokens().1
    }

    fn checked_input_tokens(&self) -> (u64, bool) {
        input_token_total(
            self.uncached_input_tokens,
            self.cache_write_tokens,
            self.cache_read_tokens,
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
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
    pub tool_calls_executed: u64,
    /// Calls whose result was already in the active context and was replaced
    /// by a pointer. Post-compaction reacquisitions are counted separately.
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

impl OpenRequest {
    fn apply(&mut self, kind: &EventKind, overflowed: &mut bool) {
        match kind {
            EventKind::UsageBreakdown { usage } => {
                self.saw_breakdown = true;
                sum!(
                    self.usage, usage, overflowed;
                    uncached_input_tokens, cache_write_tokens, cache_read_tokens,
                    output_tokens, reasoning_tokens
                );
                self.usage.usage_unknown |= usage.usage_unknown;
            }
            EventKind::UsagePartial {
                input_tokens,
                output_tokens,
                input_known,
                output_known,
            } => {
                self.add_fallback(*input_tokens, *output_tokens, overflowed);
                self.saw_partial_usage = true;
                self.input_usage_known |= *input_known;
                self.output_usage_known |= *output_known;
            }
            EventKind::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.add_fallback(*input_tokens, *output_tokens, overflowed);
                if !self.saw_partial_usage {
                    self.input_usage_known = true;
                    self.output_usage_known = true;
                }
                self.saw_terminal_usage = true;
            }
            EventKind::ResponseCacheHit => {
                self.usage.response_cache_hit = true;
                self.saw_terminal_usage = true;
                self.input_usage_known = true;
                self.output_usage_known = true;
            }
            EventKind::ProviderPhase {
                phase, elapsed_ms, ..
            } => match phase {
                ProviderPhase::FirstByte => self.usage.time_to_first_byte_ms = *elapsed_ms,
                ProviderPhase::FirstSemantic => self.usage.time_to_first_semantic_ms = *elapsed_ms,
                _ => {}
            },
            EventKind::RequestCompleted {
                provider_latency_ms,
                cancelled,
                failed,
            } => {
                self.usage.provider_latency_ms = *provider_latency_ms;
                self.usage.cancelled = *cancelled;
                self.usage.failed = *failed;
            }
            _ => {}
        }
    }

    fn add_fallback(&mut self, input_tokens: u64, output_tokens: u64, overflowed: &mut bool) {
        add(&mut self.fallback_input_tokens, input_tokens, overflowed);
        add(&mut self.fallback_output_tokens, output_tokens, overflowed);
    }

    /// Settles the request's usage once no more events can reach it: a
    /// provider breakdown wins over the terminal/partial fallback counters.
    fn resolve_usage(&mut self) {
        if !self.saw_breakdown {
            self.usage.uncached_input_tokens = self.fallback_input_tokens;
            self.usage.output_tokens = self.fallback_output_tokens;
        }
        self.usage.usage_unknown |=
            !self.saw_terminal_usage || !self.input_usage_known || !self.output_usage_known;
        if !self.usage.usage_unknown && !self.usage.response_cache_hit {
            self.usage.estimation_error_tokens = signed_difference(
                self.usage.estimated_input_tokens,
                self.usage.total_input_tokens(),
            );
        }
    }
}

/// Folds a session's events into a ledger. Provider requests are delimited by
/// `ContextSnapshot`.
struct LedgerBuilder {
    totals: UsageTotals,
    open: Option<OpenRequest>,
    next_retry: u64,
}

impl LedgerBuilder {
    fn new(validated_completion: bool) -> Self {
        Self {
            totals: UsageTotals {
                validated_completion,
                ..UsageTotals::default()
            },
            open: None,
            next_retry: 0,
        }
    }

    fn apply(&mut self, kind: &EventKind) {
        match kind {
            EventKind::ContextSnapshot {
                request_kind,
                provider,
                model,
                system_bytes,
                tool_schema_bytes,
                history_bytes,
                tool_result_bytes,
                estimated_tokens,
                ..
            } => self.on_snapshot(RequestUsage {
                request_kind: *request_kind,
                provider: provider.clone(),
                model: model.clone(),
                system_bytes: *system_bytes,
                tool_schema_bytes: *tool_schema_bytes,
                history_bytes: *history_bytes,
                tool_result_bytes: *tool_result_bytes,
                estimated_input_tokens: *estimated_tokens,
                ..RequestUsage::default()
            }),
            EventKind::UsageBreakdown { .. }
            | EventKind::UsagePartial { .. }
            | EventKind::Usage { .. }
            | EventKind::ResponseCacheHit
            | EventKind::ProviderPhase { .. }
            | EventKind::RequestCompleted { .. } => {
                if let Some(request) = &mut self.open {
                    request.apply(kind, &mut self.totals.overflowed);
                }
            }
            EventKind::ToolFinished { .. }
            | EventKind::ToolEvidenceReused { .. }
            | EventKind::ToolEvidenceElided { .. }
            | EventKind::ToolCallsSuppressed { .. }
            | EventKind::CausalAnomalyDetected { .. } => self.on_tool(kind),
            EventKind::CompactionUsageUnknown { .. } | EventKind::CompactionState { .. } => {
                self.on_compaction(kind)
            }
            _ => {}
        }
    }

    fn on_snapshot(&mut self, mut usage: RequestUsage) {
        self.close_open();
        if usage.request_kind == RequestKind::ProviderTurn {
            usage.retry_count = self.next_retry;
        }
        self.open = Some(OpenRequest {
            usage,
            ..OpenRequest::default()
        });
    }

    fn on_tool(&mut self, kind: &EventKind) {
        let totals = &mut self.totals;
        match kind {
            EventKind::ToolFinished { duration_ms, .. } => {
                bump!(totals, tool_calls_executed, 1);
                bump!(totals, tool_latency_ms, *duration_ms);
                if let Some(request) = &mut self.open {
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
                bump!(
                    totals,
                    duplicate_evidence_bytes_avoided,
                    original_bytes.saturating_sub(*emitted_bytes)
                );
                // A post-compaction event is a reacquisition (the tool ran
                // again); only in-context duplicates are reused calls.
                if *post_compaction {
                    bump!(totals, post_compaction_reacquisitions, 1);
                } else {
                    bump!(totals, tool_calls_reused, 1);
                }
            }
            EventKind::ToolEvidenceElided {
                original_bytes,
                emitted_bytes,
                ..
            } => bump!(
                totals,
                duplicate_evidence_bytes_avoided,
                original_bytes.saturating_sub(*emitted_bytes)
            ),
            EventKind::ToolCallsSuppressed { count } => {
                bump!(totals, tool_calls_suppressed, *count)
            }
            EventKind::CausalAnomalyDetected {
                kind: CausalAnomalyKind::StagnantTurn | CausalAnomalyKind::NoProgressCandidate,
                ..
            } => {
                bump!(totals, no_progress_turns, 1);
                if let Some(request) = &mut self.open {
                    request.no_progress = true;
                }
            }
            _ => {}
        }
    }

    fn on_compaction(&mut self, kind: &EventKind) {
        match kind {
            EventKind::CompactionUsageUnknown { .. } => self.totals.usage_unknown = true,
            EventKind::CompactionState {
                state,
                tokens_before,
                tokens_after,
                ..
            } => match state {
                CompactionStatus::Applied => bump!(
                    self.totals,
                    compaction_tokens_saved,
                    tokens_before.saturating_sub(*tokens_after)
                ),
                CompactionStatus::Idle => {}
            },
            _ => {}
        }
    }

    /// Settles the open provider request, remembering the retry count the
    /// next provider turn inherits.
    fn close_open(&mut self) {
        if let Some(previous) = self.open.take() {
            let previous_kind = previous.usage.request_kind;
            let retry_count = self.totals.finish_request(previous);
            if previous_kind == RequestKind::ProviderTurn {
                self.next_retry = retry_count;
            }
        }
    }

    fn finish(mut self) -> UsageTotals {
        self.close_open();
        self.totals.mark_input_overflow();
        self.totals
    }
}

impl UsageTotals {
    pub fn from_events(events: &[SessionEvent], validated_completion: bool) -> Self {
        let mut ledger = LedgerBuilder::new(validated_completion);
        for event in events {
            ledger.apply(&event.kind);
        }
        ledger.finish()
    }

    pub fn total_input_tokens(&self) -> u64 {
        self.checked_input_tokens().0
    }

    fn checked_input_tokens(&self) -> (u64, bool) {
        input_token_total(
            self.uncached_input_tokens,
            self.cache_write_tokens,
            self.cache_read_tokens,
        )
    }

    fn mark_input_overflow(&mut self) {
        let totals_overflowed = self.checked_input_tokens().1;
        let request_overflowed = self
            .requests
            .iter()
            .any(RequestUsage::input_tokens_overflowed);
        self.overflowed |= totals_overflowed || request_overflowed;
    }

    /// Settles a request and returns the retry count its successor inherits.
    fn finish_request(&mut self, mut open: OpenRequest) -> u64 {
        open.resolve_usage();
        let retry = if open.usage.failed || open.usage.cancelled {
            open.usage.retry_count.saturating_add(1)
        } else {
            0
        };
        if open.usage.request_kind == RequestKind::Compaction {
            self.push_compaction_request(open.usage);
        } else {
            self.fold_provider_turn(open.usage, open.no_progress);
        }
        retry
    }

    fn fold_provider_turn(&mut self, usage: RequestUsage, no_progress: bool) {
        bump!(self, provider_turns, 1);
        sum!(
            self, usage, &mut self.overflowed;
            uncached_input_tokens, cache_write_tokens, cache_read_tokens, output_tokens,
            reasoning_tokens, system_bytes, tool_schema_bytes, history_bytes,
            tool_result_bytes, provider_latency_ms
        );
        self.retry_count = self.retry_count.max(usage.retry_count);
        self.fold_outcome(&usage);
        if no_progress {
            bump!(self, no_progress_tokens, usage.total_tokens());
        }
        self.requests.push(usage);
    }

    /// Adds a compaction request. Its input-token overflow reaches
    /// `overflowed` through `mark_input_overflow` when the ledger finishes.
    fn push_compaction_request(&mut self, request: RequestUsage) {
        bump!(self, compaction_input_tokens, request.total_input_tokens());
        bump!(self, compaction_output_tokens, request.output_tokens);
        self.fold_outcome(&request);
        self.requests.push(request);
    }

    /// Outcome fields shared by every settled request. The estimation error
    /// is only ever non-zero for a request with known usage.
    fn fold_outcome(&mut self, request: &RequestUsage) {
        self.estimation_error_tokens = self
            .estimation_error_tokens
            .saturating_add(request.estimation_error_tokens);
        self.usage_unknown |= request.usage_unknown;
        if request.cancelled {
            bump!(self, cancelled_requests, 1);
        }
    }
}

/// Sum of the three input-token buckets, saturated at `u64::MAX` (the flag
/// reports that saturation).
fn input_token_total(
    uncached_input_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens: u64,
) -> (u64, bool) {
    uncached_input_tokens
        .checked_add(cache_write_tokens)
        .and_then(|total| total.checked_add(cache_read_tokens))
        .map_or((u64::MAX, true), |total| (total, false))
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
