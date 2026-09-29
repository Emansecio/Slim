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

struct JevUsage<'a> {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    usage_unknown: bool,
    failed: bool,
    backend: Option<&'a str>,
    requested_model: Option<&'a str>,
    model: Option<&'a str>,
    duration_ms: u64,
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

    /// A background compaction that never finished has no confirmed usage.
    fn abandon(&mut self) {
        self.usage_unknown = true;
        self.failed = true;
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
    pub jev_input_tokens: u64,
    pub jev_output_tokens: u64,
    pub jev_latency_ms: u64,
    pub jev_usage_unknown: bool,
    /// Confirmed Jev input tokens for which the TypeSafe standard price is
    /// known. This is deliberately separate from `jev_input_tokens`: tokens
    /// from Vercel/custom backends must not be treated as TypeSafe cost.
    pub jev_priced_input_tokens: u64,
    pub jev_failed_priced_input_tokens: u64,
    pub jev_failed_usage_unknown: bool,
    /// At least one Jev event carried usage for a backend/model whose price
    /// is not part of Slim's static catalogue.
    pub jev_pricing_unknown: bool,
    pub jev_failed_pricing_unknown: bool,
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
/// `ContextSnapshot`; background compaction attempts by their own events.
struct LedgerBuilder {
    totals: UsageTotals,
    open: Option<OpenRequest>,
    background: Option<RequestUsage>,
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
            background: None,
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
            EventKind::CompactionJevPruned { .. }
            | EventKind::CompactionJevFallback { .. }
            | EventKind::CompactionUsageUnknown { .. }
            | EventKind::CompactionState { .. } => self.on_compaction(kind),
            EventKind::CompactionAttemptStarted { .. }
            | EventKind::CompactionAttemptCompleted { .. }
            | EventKind::CompactionAttemptCancelled { .. } => self.on_compaction_attempt(kind),
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
            EventKind::CompactionJevPruned {
                input_tokens,
                output_tokens,
                usage_unknown,
                backend,
                requested_model,
                model,
                duration_ms,
                ..
            }
            | EventKind::CompactionJevFallback {
                input_tokens,
                output_tokens,
                usage_unknown,
                backend,
                requested_model,
                model,
                duration_ms,
                ..
            } => self.totals.record_jev_usage(JevUsage {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                usage_unknown: *usage_unknown,
                failed: matches!(kind, EventKind::CompactionJevFallback { .. }),
                backend: backend.as_deref(),
                requested_model: requested_model.as_deref(),
                model: model.as_deref(),
                duration_ms: *duration_ms,
            }),
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
                CompactionStatus::Discarded => {
                    if let Some(request) = self
                        .totals
                        .requests
                        .iter_mut()
                        .rev()
                        .find(|request| request.request_kind == RequestKind::Compaction)
                    {
                        request.failed = true;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// A background compaction attempt: one request opened by `Started`,
    /// settled by `Completed` or `Cancelled`.
    fn on_compaction_attempt(&mut self, kind: &EventKind) {
        match kind {
            EventKind::CompactionAttemptStarted {
                provider,
                model,
                system_bytes,
                history_bytes,
                estimated_input_tokens,
                ..
            } => {
                self.abandon_background();
                self.background = Some(RequestUsage {
                    provider: provider.clone(),
                    model: model.clone(),
                    system_bytes: *system_bytes,
                    ..compaction_request(*history_bytes, *estimated_input_tokens)
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
                system_bytes,
                history_bytes,
                estimated_input_tokens,
            } => {
                let mut request = self
                    .background
                    .take()
                    .unwrap_or_else(|| compaction_request(0, 0));
                if let Some(system_bytes) = system_bytes {
                    request.system_bytes = *system_bytes;
                }
                if let Some(history_bytes) = history_bytes {
                    request.history_bytes = *history_bytes;
                }
                if let Some(estimated_input_tokens) = estimated_input_tokens {
                    request.estimated_input_tokens = *estimated_input_tokens;
                }
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
                self.totals.push_compaction_request(request);
            }
            EventKind::CompactionAttemptCancelled {
                request_bytes,
                estimated_input_tokens,
                time_to_first_byte_ms,
                time_to_first_semantic_ms,
                duration_ms,
                ..
            } => {
                let mut request = self
                    .background
                    .take()
                    .unwrap_or_else(|| compaction_request(*request_bytes, *estimated_input_tokens));
                request.abandon();
                request.time_to_first_byte_ms = *time_to_first_byte_ms;
                request.time_to_first_semantic_ms = *time_to_first_semantic_ms;
                request.provider_latency_ms = *duration_ms;
                request.cancelled = true;
                self.totals.push_compaction_request(request);
            }
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

    fn abandon_background(&mut self) {
        if let Some(mut unfinished) = self.background.take() {
            unfinished.abandon();
            self.totals.push_compaction_request(unfinished);
        }
    }

    fn finish(mut self) -> UsageTotals {
        self.close_open();
        self.abandon_background();
        self.totals.mark_input_overflow();
        self.totals
    }
}

/// A compaction attempt whose start event was not seen.
fn compaction_request(history_bytes: u64, estimated_input_tokens: u64) -> RequestUsage {
    RequestUsage {
        request_kind: RequestKind::Compaction,
        history_bytes,
        estimated_input_tokens,
        ..RequestUsage::default()
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

    fn record_jev_usage(&mut self, usage: JevUsage<'_>) {
        let JevUsage {
            input_tokens,
            output_tokens,
            usage_unknown,
            failed,
            backend,
            requested_model,
            model,
            duration_ms,
        } = usage;
        if let Some(tokens) = input_tokens {
            bump!(self, jev_input_tokens, tokens);
            bump!(self, compaction_input_tokens, tokens);
        }
        if let Some(tokens) = output_tokens {
            bump!(self, jev_output_tokens, tokens);
            bump!(self, compaction_output_tokens, tokens);
        }
        let missing_counts = input_tokens.is_none() || output_tokens.is_none();
        if usage_unknown || missing_counts {
            self.jev_usage_unknown = true;
            self.usage_unknown = true;
            if failed {
                self.jev_failed_usage_unknown = true;
            }
        }

        if is_known_typesafe_jev(backend, requested_model, model) {
            if let Some(tokens) = input_tokens.filter(|tokens| *tokens > 0) {
                bump!(self, jev_priced_input_tokens, tokens);
                if failed {
                    bump!(self, jev_failed_priced_input_tokens, tokens);
                }
            }
        } else {
            let has_confirmed_usage = input_tokens.is_some_and(|tokens| tokens > 0)
                || output_tokens.is_some_and(|tokens| tokens > 0);
            if has_confirmed_usage || missing_counts {
                if failed {
                    self.jev_failed_pricing_unknown = true;
                }
                self.jev_pricing_unknown = true;
            }
        }
        bump!(self, jev_latency_ms, duration_ms);
    }
}

/// TypeSafe's standard Jev price is stable for the built-in model and its
/// public aliases. Vercel and caller-selected/dynamic models are intentionally
/// left unpriced until the provider supplies a catalog entry.
fn is_known_typesafe_jev(
    backend: Option<&str>,
    requested_model: Option<&str>,
    resolved_model: Option<&str>,
) -> bool {
    if !backend.is_some_and(|backend| backend.eq_ignore_ascii_case("typesafe")) {
        return false;
    }
    let models = [requested_model, resolved_model];
    models
        .iter()
        .flatten()
        .all(|model| is_known_typesafe_model(model))
        && models.iter().flatten().next().is_some()
}

fn is_known_typesafe_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "jev-1.13.0" | "typesafe-ai/jev" | "typesafe-ai/jev-1.13.0" | "jev"
    )
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
