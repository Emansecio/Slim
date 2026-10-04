//! Foreground compaction: Pi's default compaction run before a request.
//!
//! The summary comes from the session's own model in one call over the
//! history before the cut and, when the cut splits a turn, a second call over
//! that turn's prefix (`crate::context::pi_compaction`). Each call is a
//! recorded `RequestKind::Compaction` request and each is covered by the
//! bounded retry of recoverable provider errors.

use super::agent_loop::{LoopCtx, LoopState};
use super::compaction_types::is_truncated_summary;
use super::*;
use crate::context::{CompactionPreparation, SummaryRequest, SummaryRequests};

/// What a summarization call returned.
struct SummaryAnswer {
    text: String,
    usage: crate::UsageBreakdown,
}

/// The answers of a compaction's calls.
#[derive(Default)]
struct SummaryAnswers {
    history: Option<String>,
    turn_prefix: Option<String>,
    usage: crate::UsageBreakdown,
}

/// A summarization request prepared for the wire.
struct PreparedSummaryCall {
    request: PreparedProviderRequest,
    estimated_tokens: u64,
}

/// The output limit a summary retries with after running into `sent`: twice
/// as much, within the request's own cap, half the window and the model's
/// ceiling. `None` when that is no more than was sent.
fn raised_summary_limit(
    sent: u64,
    cap: u64,
    window: u64,
    model_ceiling: Option<u64>,
) -> Option<u64> {
    let raised = sent
        .saturating_mul(2)
        .min(cap)
        .min(window / 2)
        .min(model_ceiling.unwrap_or(u64::MAX));
    (raised > sent).then_some(raised)
}

/// Everything `finalize_compaction` needs besides the loop state.
struct CompactionResultInputs {
    reason: CompactionReason,
    preparation: CompactionPreparation,
    answers: SummaryAnswers,
    started: Instant,
}

impl Runtime {
    /// Whether `messages` are exactly what the last compaction left, with
    /// nothing appended since (Pi's "Already compacted").
    pub(super) fn is_already_compacted(&self, messages: &[ProviderMessage]) -> bool {
        self.compaction_handle
            .as_ref()
            .is_some_and(|handle| handle.is_already_compacted(messages))
    }

    /// Whether a compaction of `messages` would replace anything.
    pub(super) fn has_compactable_history(&self, messages: &[ProviderMessage]) -> bool {
        !self.is_already_compacted(messages)
            && prepare_compaction(
                messages,
                &self.compaction_policy().settings(),
                ContextUsage::default(),
            )
            .is_some()
    }

    /// The context estimate the trigger and the compaction share: the last
    /// response's provider usage plus an estimate of what followed it. Before
    /// any response (or after a compaction) everything is estimated, with the
    /// system prompt and tool schemas included; those are only counted when
    /// no anchor is usable, because an anchored usage already includes them.
    pub(super) fn context_usage<A: ProviderAdapter>(
        &self,
        adapter: &A,
        st: &LoopState<'_>,
        tools: &[Value],
    ) -> ContextUsage {
        let anchor = st.usage_anchor;
        let fixed_tokens = if usable_anchor(st.messages, anchor).is_some() {
            0
        } else {
            Self::fixed_context_tokens(adapter, tools)
        };
        ContextUsage {
            anchor,
            fixed_tokens,
        }
    }

    /// Estimate of the system prompt and tool schemas.
    fn fixed_context_tokens<A: ProviderAdapter>(adapter: &A, tools: &[Value]) -> u64 {
        estimate_system_and_tools_tokens(
            adapter.system_prompt_for_budget().unwrap_or_default(),
            tools,
        )
    }

    /// Compacts the history before this turn's request. Nothing to compact is
    /// not an error: the turn goes on with the history it has, and a manual
    /// request stays queued until there is something to compact.
    pub(super) async fn compact_for_turn<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        trigger: CompactionTrigger,
    ) -> Result<CompactionOutcome, ProviderError> {
        let policy = self.compaction_policy();
        if self.is_already_compacted(st.messages) {
            return Ok(CompactionOutcome::Skipped);
        }
        let Some(preparation) = prepare_compaction(st.messages, &policy.settings(), trigger.usage)
        else {
            return Ok(CompactionOutcome::Skipped);
        };
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ProviderPhase {
                phase: ProviderPhase::Compacting,
                elapsed_ms: 0,
                detail: None,
            },
        )?;
        let started = Instant::now();
        let instructions = self
            .compaction_handle
            .as_ref()
            .and_then(CompactionHandle::manual_instructions)
            .filter(|text| !text.trim().is_empty());
        let requests = preparation.summary_requests(instructions.as_deref());
        let answers = self.run_summary_calls(ctx, st, requests).await;
        let result = if self.is_cancelled() {
            Ok(CompactionOutcome::Cancelled)
        } else {
            answers.and_then(|answers| {
                self.finalize_compaction(
                    ctx,
                    st,
                    tools,
                    &policy,
                    CompactionResultInputs {
                        reason: trigger.reason,
                        preparation,
                        answers,
                        started,
                    },
                )
            })
        };
        // A failed or cancelled compaction does not leave its manual request
        // queued: it would run again on the next prompt.
        if result.is_err() || matches!(result, Ok(CompactionOutcome::Cancelled)) {
            if let Some(handle) = &self.compaction_handle {
                handle.clear_manual();
            }
        }
        if matches!(result, Ok(CompactionOutcome::Cancelled)) {
            st.next_seq = self.observed_next_seq(st.next_seq);
        }
        result
    }

    /// The history summary, then the summary of a split turn's prefix.
    async fn run_summary_calls<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        requests: SummaryRequests,
    ) -> Result<SummaryAnswers, ProviderError> {
        let mut answers = SummaryAnswers::default();
        if let Some(request) = &requests.history {
            let answer = self.summarize_with_retry(ctx, st, request).await?;
            answers.usage.absorb(answer.usage);
            answers.history = Some(answer.text);
        }
        if let Some(request) = &requests.turn_prefix {
            let answer = self.summarize_with_retry(ctx, st, request).await?;
            answers.usage.absorb(answer.usage);
            answers.turn_prefix = Some(answer.text);
        }
        Ok(answers)
    }

    /// One summarization call, retried a bounded number of times on
    /// recoverable provider errors, and once with a higher output limit when
    /// the summary ran into the limit it was sent with.
    async fn summarize_with_retry<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        request: &SummaryRequest,
    ) -> Result<SummaryAnswer, ProviderError> {
        let mut attempts = 0_u32;
        let mut raised_limit = None;
        loop {
            attempts += 1;
            let mut sent_limit = None;
            let result = self
                .summarize_once(
                    ctx.client,
                    request,
                    (
                        st.config.context_window_tokens,
                        st.config.context_reserve_tokens,
                    ),
                    raised_limit,
                    (&mut st.next_seq, &mut sent_limit),
                )
                .await;
            match result {
                Err(error)
                    if raised_limit.is_none()
                        && is_truncated_summary(&error)
                        && !self.is_cancelled() =>
                {
                    // The configured output limit is per turn, not the
                    // summary's budget: Pi sizes the summary by the reserve.
                    let Some(raised) = sent_limit.and_then(|sent| {
                        raised_summary_limit(
                            sent,
                            request.max_output_tokens,
                            st.config.context_window_tokens,
                            ctx.client.known_max_output_tokens(),
                        )
                    }) else {
                        return Err(error);
                    };
                    st.next_seq = self.observed_next_seq(st.next_seq);
                    raised_limit = Some(raised);
                }
                Err(error)
                    if st.recovery.can_retry(st.recovery.compaction_recoveries)
                        && recoverable_provider_error(&error)
                        && !self.is_cancelled() =>
                {
                    st.next_seq = self.observed_next_seq(st.next_seq);
                    let delay = match provider_recovery_delay(
                        &error,
                        st.recovery.compaction_recoveries + 1,
                        st.recovery.provider_recovery_wait,
                        st.config.provider_recovery_backoff,
                    ) {
                        Ok(delay) => delay,
                        Err(blocked) => {
                            return Err(annotate_provider_recovery_error(
                                blocked,
                                "compaction",
                                attempts,
                                st.recovery.automatic_recoveries,
                                st.recovery.compaction_recoveries,
                                "retry wait budget exhausted",
                            ))
                        }
                    };
                    st.recovery.compaction_recoveries += 1;
                    st.recovery.automatic_recoveries += 1;
                    st.recovery.provider_recovery_wait += delay;
                    self.announce_retry(
                        &mut st.next_seq,
                        &error,
                        RetryNotice {
                            phase: ProviderPhase::Compacting,
                            detail: format!("Retrying foreground compaction ({compaction_recoveries}/{MAX_PROVIDER_RECOVERIES}); waiting {} ms", delay.as_millis(), compaction_recoveries = st.recovery.compaction_recoveries),
                            attempt: st.recovery.compaction_recoveries,
                            limit: MAX_PROVIDER_RECOVERIES,
                            wait: delay,
                        },
                    )?;
                    if self.sleep_or_cancel(delay).await {
                        return Err(ProviderError::Cancelled);
                    }
                }
                Err(error) if recoverable_provider_error(&error) => {
                    return Err(annotate_provider_recovery_error(
                        error,
                        "compaction",
                        attempts,
                        st.recovery.automatic_recoveries,
                        st.recovery.compaction_recoveries,
                        if st.recovery.automatic_recoveries >= MAX_AUTOMATIC_RECOVERIES {
                            "global automatic recovery limit reached"
                        } else {
                            "consecutive compaction recovery limit reached"
                        },
                    ));
                }
                result => return result,
            }
        }
    }

    /// Sends one summarization request and records every request event.
    /// `limits` is the context window and the reserve for the output;
    /// `raised_limit` sends the call with that output limit instead of the
    /// capped one. `sequence` is the next event sequence and where the output
    /// limit the request was sent with is reported.
    async fn summarize_once<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        request: &SummaryRequest,
        limits: (u64, u64),
        raised_limit: Option<u64>,
        (next_seq, sent_limit): (&mut u64, &mut Option<u64>),
    ) -> Result<SummaryAnswer, ProviderError> {
        let call = self.prepare_summary_call(client, request, limits, raised_limit, next_seq)?;
        *sent_limit = call.request.output_token_limit();
        let started = Instant::now();
        let cancellation = CancellationToken::cancelled_or_pending(self.cancellation.clone());
        let mut collected = CompactionSummary::default();
        let provider_call_journal = self.app.run_journal.clone();
        let stream_result = client
            .stream_prepared_cancellable_observed(
                call.request,
                cancellation,
                |event| collected.push(event),
                |telemetry| persist_provider_call(&provider_call_journal, telemetry).map(|_| ()),
            )
            .await;
        self.record_compaction_stream(&mut collected, call.estimated_tokens, next_seq)?;
        let validation_result = if stream_result.is_ok() {
            collected.validate()
        } else {
            Ok(())
        };
        let request_failed = stream_result.is_err() || validation_result.is_err();
        let request_cancelled = matches!(&stream_result, Err(ProviderError::Cancelled));
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::RequestCompleted {
                provider_latency_ms: elapsed_millis(started),
                cancelled: request_cancelled,
                failed: request_failed,
            },
        )?;
        // A summarization request is a single text message.
        self.calibrate_latest_request(true);
        if let Err(error) = stream_result {
            return Err(self.redact_provider_error(error));
        }
        validation_result?;
        Ok(SummaryAnswer {
            text: collected.text,
            usage: collected.usage,
        })
    }

    /// The wire request of a summarization call, with its context snapshot
    /// recorded. A conversation too large for the window (with the room the
    /// output needs) is bounded to its head and tail; Pi has no such guard.
    /// Fails only when even the fixed parts of the prompt do not fit.
    fn prepare_summary_call<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        request: &SummaryRequest,
        (window, reserve): (u64, u64),
        raised_limit: Option<u64>,
        next_seq: &mut u64,
    ) -> Result<PreparedSummaryCall, ProviderError> {
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();
        let output_room = raised_limit.unwrap_or_else(|| request.max_output_tokens.min(reserve));
        let prepare = |request: &SummaryRequest| -> Result<(PreparedProviderRequest, u64), _> {
            let messages = request.messages();
            let prepared = match raised_limit {
                Some(limit) => client.prepare_compaction_messages_raised(&messages, limit)?,
                None => client.prepare_compaction_messages(&messages, request.max_output_tokens)?,
            };
            let tokens = self
                .token_estimator
                .estimate(provider, model, prepared.serialized_chars);
            Ok::<_, ProviderError>((prepared, tokens))
        };
        let fits = |tokens: u64| tokens.saturating_add(output_room) <= window;

        let (mut prepared, mut estimated_tokens) = prepare(request)?;
        if !fits(estimated_tokens) {
            // A path taken at most once per compaction, so it avoids building
            // wire requests per step: the serialized size is close to linear
            // in the prompt length, which the full and the smallest request
            // pin down. The length found on that model is confirmed by one
            // real request and shrunk a little when the model was optimistic.
            let fixed = request.prompt.len() - request.conversation_bytes();
            let mut smallest = request.clone();
            smallest.bound_conversation(fixed);
            let (smallest_request, smallest_tokens) = prepare(&smallest)?;
            if !fits(smallest_tokens) {
                return Err(ProviderError::InvalidResponse {
                    message: "compaction request still exceeds context window".into(),
                });
            }
            let (small_len, full_len) = (smallest.prompt.len(), request.prompt.len());
            let growth_per_mille = u128::from(
                prepared
                    .serialized_chars
                    .saturating_sub(smallest_request.serialized_chars),
            ) * 1_100
                / (full_len - small_len).max(1) as u128;
            let modeled_tokens = |length: usize| {
                let grown = (length - small_len) as u128 * growth_per_mille / 1_000;
                let chars = u128::from(smallest_request.serialized_chars) + grown;
                self.token_estimator.estimate(
                    provider,
                    model,
                    u64::try_from(chars).unwrap_or(u64::MAX),
                )
            };
            // The largest prompt the model says fits, to 256 bytes.
            let (mut low, mut high) = (small_len, full_len);
            while high - low > 256 {
                let middle = low + (high - low) / 2;
                if fits(modeled_tokens(middle)) {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            (prepared, estimated_tokens) = (smallest_request, smallest_tokens);
            let mut length = low;
            for _ in 0..8 {
                if length <= small_len {
                    break;
                }
                let mut trial = request.clone();
                trial.bound_conversation(length);
                let (trial_request, trial_tokens) = prepare(&trial)?;
                if fits(trial_tokens) {
                    (prepared, estimated_tokens) = (trial_request, trial_tokens);
                    break;
                }
                length = small_len + (length - small_len) / 10 * 9;
            }
        }
        prepared.estimated_tokens = estimated_tokens;
        let ProviderRequestComponents {
            system_bytes,
            history_bytes,
            tool_result_bytes,
            ..
        } = prepared.components;
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ContextSnapshot {
                request_kind: crate::RequestKind::Compaction,
                provider: provider.into(),
                model: model.into(),
                system_bytes,
                tool_schema_bytes: 0,
                history_bytes,
                tool_result_bytes,
                serialized_chars: prepared.serialized_chars,
                estimated_tokens,
                context_window_tokens: window,
            },
        )?;
        Ok(PreparedSummaryCall {
            request: prepared,
            estimated_tokens,
        })
    }

    /// Records what the summary stream reported: latencies, the usage
    /// breakdown, and the terminal usage or its absence.
    fn record_compaction_stream(
        &mut self,
        collected: &mut CompactionSummary,
        estimated_input_tokens: u64,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        if let Some(elapsed_ms) = collected.time_to_first_byte_ms {
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::ProviderPhase {
                    phase: ProviderPhase::FirstByte,
                    elapsed_ms,
                    detail: None,
                },
            )?;
        }
        if let Some(elapsed_ms) = collected.time_to_first_semantic_ms {
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::ProviderPhase {
                    phase: ProviderPhase::FirstSemantic,
                    elapsed_ms,
                    detail: None,
                },
            )?;
        }
        for usage in &collected.breakdown_events {
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::UsageBreakdown { usage: *usage },
            )?;
        }
        if collected.usage_events.is_empty() {
            collected.usage.usage_unknown = true;
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionUsageUnknown {
                    estimated_input_tokens,
                    reason: "terminal_usage_unavailable".into(),
                },
            )?;
        } else {
            for usage_event in &collected.usage_events {
                let kind = if usage_event.input_known && usage_event.output_known {
                    crate::EventKind::Usage {
                        input_tokens: usage_event.input_tokens,
                        output_tokens: usage_event.output_tokens,
                    }
                } else {
                    crate::EventKind::UsagePartial {
                        input_tokens: usage_event.input_tokens,
                        output_tokens: usage_event.output_tokens,
                        input_known: usage_event.input_known,
                        output_known: usage_event.output_known,
                    }
                };
                push_runtime_event(&mut self.app, next_seq, kind)?;
            }
        }
        Ok(())
    }

    /// Joins the answers, appends the file lists within the persistence
    /// limits, swaps the history, records the commit and announces the
    /// compaction.
    fn finalize_compaction<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        policy: &CompactionPolicy,
        inputs: CompactionResultInputs,
    ) -> Result<CompactionOutcome, ProviderError> {
        let CompactionResultInputs {
            reason,
            preparation,
            answers,
            started,
        } = inputs;
        let text = preparation
            .assemble_summary(answers.history.as_deref(), answers.turn_prefix.as_deref())
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        // The summarizer saw redacted history; its answer is redacted again
        // before it is stored or shown to the model.
        let text = self.redact_sensitive(&text);
        let fitted =
            fit_summary_for_persistence(&text, &preparation.file_ops, policy.summary_max_bytes)
                .map_err(|message| ProviderError::InvalidResponse { message })?;
        let first_kept_index = preparation.first_kept_index;
        let canonical_prefix_fingerprint =
            canonical_prefix_fingerprint(&st.messages[..first_kept_index]);
        let mut compacted = apply_compaction(st.messages, first_kept_index, &fitted.summary);
        // The rewritten history is a new prefix: the overlay is anchored again.
        let channel = mode::ChannelFrame::rebuilt_for(&compacted, self.mcp_awareness(ctx.mode));
        let request =
            self.prepare_loop_request(ctx.client, &mut compacted, tools, ctx.mode, &channel)?;
        let tokens_after = estimate_context_tokens(
            &compacted,
            ContextUsage {
                anchor: None,
                fixed_tokens: Self::fixed_context_tokens(ctx.client.adapter(), tools),
            },
        )
        .tokens;
        let duration_ms = elapsed_millis(started);
        if let Some(handle) = &self.compaction_handle {
            handle.commit_detailed(CompactionCommit {
                summary: fitted.summary,
                canonical_prefix_fingerprint,
                first_kept_index,
                tokens_before: preparation.tokens_before,
                tokens_after,
                input_tokens: answers.usage.total_input_tokens(),
                output_tokens: answers.usage.output_tokens,
                duration_ms,
                reason,
                read_files: fitted.files.read_files,
                modified_files: fitted.files.modified_files,
            });
            handle.clear_manual();
            // The summary replaces what the last response's usage described.
            handle.clear_usage_anchor();
            handle.mark_compacted(&compacted);
        }
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::CompactionState {
                state: crate::context::CompactionStatus::Applied,
                reason,
                tokens_before: preparation.tokens_before,
                tokens_after,
                duration_ms,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::CompactionCompleted,
        )?;
        *st.messages = compacted;
        st.channel = channel;
        st.usage_anchor = None;
        st.awaiting_response = true;
        st.recovery.compaction_recoveries = 0;
        st.governor.forget_compacted_evidence();
        Ok(CompactionOutcome::Applied(Box::new(request)))
    }

    pub(super) fn calibrate_latest_request(&mut self, calibration_eligible: bool) {
        if !calibration_eligible {
            return;
        }
        let events = self.app.events();
        let Some((start, provider, model, serialized_chars)) = events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match &event.kind {
                crate::EventKind::ContextSnapshot {
                    provider,
                    model,
                    serialized_chars,
                    ..
                } => Some((index, provider.as_str(), model.as_str(), *serialized_chars)),
                _ => None,
            })
        else {
            return;
        };
        let ledger = UsageTotals::from_events(&events[start..], false);
        let Some(request) = ledger
            .requests
            .first()
            .filter(|request| request.usable_for_calibration())
        else {
            return;
        };
        self.token_estimator.observe(
            provider,
            model,
            serialized_chars,
            request.total_input_tokens(),
        );
    }
}
