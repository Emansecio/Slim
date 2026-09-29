use super::*;

pub(super) use super::economy::TurnForecast;

pub(super) async fn run_background_compaction<A: ProviderAdapter>(
    client: HttpProviderClient<A>,
    mut plan: BackgroundCompactionPlan,
    jev_judge: Option<Arc<dyn crate::context::JevJudge>>,
    token_estimator: AdaptiveTokenEstimator,
    cancellation: Option<CancellationToken>,
    observers: BackgroundCompactionObservers,
) -> BackgroundCompactionResult {
    let started = Instant::now();
    if plan.strategy == crate::context::CompactionStrategy::Jev {
        let outcome = match &jev_judge {
            Some(judge) => {
                let mut summarized = plan.selection.summarized_for_prompt();
                let prepared = plan.jev_plan.take().unwrap_or_else(|| {
                    crate::context::jev_prune::PreparedPrune::new(
                        &plan.selection,
                        None,
                        &summarized,
                    )
                });
                match crate::context::jev_prune::prune_prepared(
                    &**judge,
                    prepared,
                    &mut summarized,
                    cancellation.as_ref(),
                )
                .await
                {
                    Ok(stats) => {
                        let mut jev_error = None;
                        match rebuild_pruned_compaction_request(
                            &client,
                            &plan,
                            &summarized,
                            &token_estimator,
                        ) {
                            Ok(request) => {
                                plan.serialized_chars = request.serialized_chars;
                                plan.system_bytes = request.components.system_bytes;
                                plan.history_bytes = request.components.history_bytes;
                                plan.request_bytes =
                                    u64::try_from(request.body.len()).unwrap_or(u64::MAX);
                                plan.estimated_input_tokens = request.estimated_tokens;
                                plan.request = Some(request);
                            }
                            Err(detail) => jev_error = Some(detail),
                        }
                        JevAttemptOutcome {
                            error: jev_error,
                            stats,
                        }
                    }
                    Err(failure) => JevAttemptOutcome {
                        error: Some(failure.to_string()),
                        stats: *failure.stats,
                    },
                }
            }
            None => JevAttemptOutcome {
                error: Some(
                    "no Jev credential configured (set TYPESAFE_API_KEY or AI_GATEWAY_API_KEY)"
                        .into(),
                ),
                stats: crate::context::JevPruneStats {
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    ..crate::context::JevPruneStats::default()
                },
            },
        };
        *lock_mutex(&observers.jev_outcome) = Some(outcome);
    }
    let cancellation_future = CancellationToken::cancelled_or_pending(cancellation.clone());
    let mut collected = CompactionSummary::default();
    let stream_result = if let Some(request) = plan.request.take() {
        client
            .stream_prepared_cancellable_observed(
                request,
                cancellation_future,
                |event| {
                    update_compaction_progress(&observers.progress, &event);
                    collected.push(event);
                },
                |telemetry| {
                    persist_provider_call(&observers.provider_call_journal, telemetry).map(|_| ())
                },
            )
            .await
    } else {
        Err(ProviderError::InvalidResponse {
            message: "background compaction request was not prepared".into(),
        })
    };
    let cancelled = matches!(stream_result, Err(ProviderError::Cancelled));
    let usage_known = collected.usage_known();
    let valid = stream_result.is_ok() && collected.validate(plan.summary_max_bytes).is_ok();
    BackgroundCompactionResult {
        plan,
        summary: collected.text,
        usage: collected.usage,
        time_to_first_byte_ms: collected.time_to_first_byte_ms,
        time_to_first_semantic_ms: collected.time_to_first_semantic_ms,
        duration_ms: elapsed_millis(started),
        valid,
        usage_known,
        cancelled,
    }
}

pub(super) fn rebuild_pruned_compaction_request<A: ProviderAdapter>(
    client: &HttpProviderClient<A>,
    plan: &BackgroundCompactionPlan,
    summarized: &[ProviderMessage],
    token_estimator: &AdaptiveTokenEstimator,
) -> Result<PreparedProviderRequest, String> {
    let prompt = build_bounded_summary_prompt_with_checkpoint(
        summarized,
        plan.previous_checkpoint.as_deref(),
        plan.context_window_tokens,
        plan.reserve_tokens,
    )
    .map_err(|message| format!("jev pruned prefix could not be repacked: {message}"))?;
    let summary_messages = [ProviderMessage::user(prompt)];
    let mut request = client
        .prepare_compaction_messages(&summary_messages)
        .map_err(|error| format!("jev pruned request could not be prepared: {error:?}"))?;
    let provider = crate::provider::provider_kind_name(client.adapter().kind());
    request.estimated_tokens =
        token_estimator.estimate(provider, client.adapter().model(), request.serialized_chars);
    if request.estimated_tokens.saturating_add(plan.reserve_tokens) > plan.context_window_tokens {
        return Err("jev pruned request exceeds context window and reserve".into());
    }
    Ok(request)
}

pub(super) fn update_compaction_progress(
    progress: &Arc<Mutex<CompactionAttemptProgress>>,
    event: &ProviderEvent,
) {
    let mut state = lock_mutex(progress);
    match event {
        ProviderEvent::Phase {
            phase: ProviderPhase::Connecting,
            ..
        } => state.send_started = true,
        ProviderEvent::Phase {
            phase: ProviderPhase::HeadersReceived,
            ..
        } => state.headers_received = true,
        ProviderEvent::Phase {
            phase: ProviderPhase::FirstByte,
            elapsed_ms,
        } => {
            state.first_byte_received = true;
            state.time_to_first_byte_ms.get_or_insert(*elapsed_ms);
        }
        ProviderEvent::Phase {
            phase: ProviderPhase::FirstSemantic,
            elapsed_ms,
        } => {
            state.time_to_first_semantic_ms.get_or_insert(*elapsed_ms);
        }
        ProviderEvent::TextDelta(text) if !text.is_empty() => state.first_token_received = true,
        _ => {}
    }
}

pub(super) fn compaction_progress_snapshot(
    progress: &Arc<Mutex<CompactionAttemptProgress>>,
) -> CompactionAttemptProgress {
    *lock_mutex(progress)
}

impl Runtime {
    pub(super) async fn finish_background_if_ready(
        &mut self,
        pending: &mut Option<PendingBackgroundCompaction>,
        policy: &CompactionPolicy,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        if pending
            .as_ref()
            .is_none_or(|attempt| !attempt.task.is_finished())
        {
            return Ok(());
        }
        let mut attempt = pending
            .take()
            .ok_or_else(|| ProviderError::InvalidResponse {
                message: "finished compaction attempt disappeared".into(),
            })?;
        let joined = (&mut attempt.task).await;
        self.finish_background_attempt(attempt, joined, policy, next_seq)
    }

    pub(super) fn finish_background_attempt(
        &mut self,
        attempt: PendingBackgroundCompaction,
        joined: Result<BackgroundCompactionResult, tokio::task::JoinError>,
        policy: &CompactionPolicy,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let progress = compaction_progress_snapshot(&attempt.progress);
        let fallback_duration = elapsed_millis(attempt.started);
        self.emit_background_jev_outcome(&attempt.jev_outcome, next_seq)?;
        let result = match joined {
            Ok(result) => result,
            Err(_) => {
                self.record_background_cancellation(
                    next_seq,
                    BackgroundCancellation {
                        request_bytes: attempt.request_bytes,
                        estimated_input_tokens: attempt.estimated_input_tokens,
                        tokens_before: attempt.tokens_before,
                        duration_ms: fallback_duration,
                        progress,
                    },
                    "background_task_failed",
                )?;
                return Ok(());
            }
        };
        if result.cancelled {
            self.record_background_cancellation(
                next_seq,
                BackgroundCancellation {
                    request_bytes: result.plan.request_bytes,
                    estimated_input_tokens: result.plan.estimated_input_tokens,
                    tokens_before: result.plan.tokens_before,
                    duration_ms: result.duration_ms,
                    progress,
                },
                "provider_request_cancelled",
            )?;
            return Ok(());
        }

        if result.usage_known && !result.usage.usage_unknown {
            self.token_estimator.observe(
                &result.plan.provider,
                &result.plan.model,
                result.plan.serialized_chars,
                result.usage.total_input_tokens(),
            );
        }

        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::CompactionAttemptCompleted {
                uncached_input_tokens: result.usage.uncached_input_tokens,
                cache_write_tokens: result.usage.cache_write_tokens,
                cache_read_tokens: result.usage.cache_read_tokens,
                output_tokens: result.usage.output_tokens,
                reasoning_tokens: result.usage.reasoning_tokens,
                time_to_first_byte_ms: result.time_to_first_byte_ms.unwrap_or(0),
                time_to_first_semantic_ms: result.time_to_first_semantic_ms.unwrap_or(0),
                duration_ms: result.duration_ms,
                usage_known: result.usage_known,
                system_bytes: Some(result.plan.system_bytes),
                history_bytes: Some(result.plan.history_bytes),
                estimated_input_tokens: Some(result.plan.estimated_input_tokens),
            },
        )?;
        let compaction_input_tokens = result.usage.total_input_tokens();
        let compaction_output_tokens = result.usage.output_tokens;
        if !result.usage_known {
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionUsageUnknown {
                    estimated_input_tokens: result.plan.estimated_input_tokens,
                    reason: "terminal_usage_unavailable".into(),
                },
            )?;
        }

        let summary = self.redact_sensitive(&result.summary);
        if !result.valid
            || crate::context::validate_checkpoint_content(&summary, policy.summary_max_bytes)
                .is_err()
        {
            if let Some(handle) = &self.compaction_handle {
                handle.background_failed();
            }
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionState {
                    state: crate::context::CompactionStatus::Discarded,
                    reason: crate::context::CompactionReason::SoftThreshold,
                    tokens_before: result.plan.tokens_before,
                    tokens_after: 0,
                    duration_ms: result.duration_ms,
                },
            )?;
            return Ok(());
        }

        if let Some(handle) = &self.compaction_handle {
            handle.store_prepared(PreparedCompaction {
                summary,
                prefix_fingerprint: compaction_prefix_fingerprint(
                    &result.plan.selection.summarized,
                ),
                first_kept_index: result.plan.selection.first_kept_index,
                pinned: result.plan.selection.pinned.clone(),
                source_len: result.plan.source_len,
                provider_identity: result.plan.provider_identity,
                input_tokens: compaction_input_tokens,
                output_tokens: compaction_output_tokens,
                duration_ms: result.duration_ms,
            });
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionState {
                    state: crate::context::CompactionStatus::Ready,
                    reason: crate::context::CompactionReason::SoftThreshold,
                    tokens_before: result.plan.tokens_before,
                    tokens_after: result.plan.projected_tokens_after,
                    duration_ms: result.duration_ms,
                },
            )?;
        }
        Ok(())
    }

    pub(super) fn emit_background_jev_outcome(
        &mut self,
        outcome: &Arc<Mutex<Option<JevAttemptOutcome>>>,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let outcome = lock_mutex(outcome).take();
        let Some(outcome) = outcome else {
            return Ok(());
        };
        self.jev_economy.observe(&outcome.stats);
        if let Some(detail) = outcome.error {
            let detail = self.redact_sensitive(&detail);
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionJevFallback {
                    detail,
                    batches: outcome.stats.batches as u64,
                    batches_started: outcome.stats.batches_started as u64,
                    batches_completed: outcome.stats.batches_completed as u64,
                    input_tokens: outcome.stats.input_tokens,
                    output_tokens: outcome.stats.output_tokens,
                    usage_unknown: outcome.stats.usage_unknown,
                    backend: outcome.stats.backend,
                    requested_model: outcome.stats.requested_model,
                    model: outcome.stats.model,
                    duration_ms: outcome.stats.duration_ms,
                },
            )?;
        } else {
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionJevPruned {
                    pairs_total: outcome.stats.pairs_total as u64,
                    pairs_dropped: outcome.stats.pairs_dropped as u64,
                    results_truncated: outcome.stats.results_truncated as u64,
                    batches: outcome.stats.batches as u64,
                    batches_started: outcome.stats.batches_started as u64,
                    batches_completed: outcome.stats.batches_completed as u64,
                    estimated_saved_tokens: outcome.stats.estimated_saved_tokens,
                    input_tokens: outcome.stats.input_tokens,
                    output_tokens: outcome.stats.output_tokens,
                    usage_unknown: outcome.stats.usage_unknown,
                    backend: outcome.stats.backend,
                    requested_model: outcome.stats.requested_model,
                    model: outcome.stats.model,
                    duration_ms: outcome.stats.duration_ms,
                },
            )?;
        }
        Ok(())
    }

    /// Emits `event`; when that fails, settles the background compaction
    /// first. The original error prevails over a failed settle.
    pub(super) async fn push_or_cancel_background(
        &mut self,
        pending: &mut Option<PendingBackgroundCompaction>,
        next_seq: &mut u64,
        event: crate::EventKind,
        reason: &str,
    ) -> Result<(), ProviderError> {
        if let Err(error) = push_runtime_event(&mut self.app, next_seq, event) {
            let _ = self
                .cancel_pending_background(pending, next_seq, reason)
                .await;
            return Err(error);
        }
        Ok(())
    }

    pub(super) async fn cancel_pending_background(
        &mut self,
        pending: &mut Option<PendingBackgroundCompaction>,
        next_seq: &mut u64,
        reason: &str,
    ) -> Result<(), ProviderError> {
        let Some(mut attempt) = pending.take() else {
            return Ok(());
        };
        let joined = tokio::select! {
            biased;
            result = &mut attempt.task => Some(result),
            _ = std::future::ready(()) => None,
        };
        if let Some(joined) = joined {
            let policy = self.compaction_policy();
            return self.finish_background_attempt(attempt, joined, &policy, next_seq);
        }

        attempt.cancellation.cancel();
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut attempt.task).await {
            Ok(joined) => {
                let policy = self.compaction_policy();
                self.finish_background_attempt(attempt, joined, &policy, next_seq)
            }
            Err(_) => {
                attempt.task.abort();
                let _ = (&mut attempt.task).await;
                self.emit_background_jev_outcome(&attempt.jev_outcome, next_seq)?;
                let duration_ms = elapsed_millis(attempt.started);
                self.record_background_cancellation(
                    next_seq,
                    BackgroundCancellation {
                        request_bytes: attempt.request_bytes,
                        estimated_input_tokens: attempt.estimated_input_tokens,
                        tokens_before: attempt.tokens_before,
                        duration_ms,
                        progress: compaction_progress_snapshot(&attempt.progress),
                    },
                    reason,
                )
            }
        }
    }

    pub(super) fn record_background_cancellation(
        &mut self,
        next_seq: &mut u64,
        cancellation: BackgroundCancellation,
        reason: &str,
    ) -> Result<(), ProviderError> {
        let BackgroundCancellation {
            request_bytes,
            estimated_input_tokens,
            tokens_before,
            duration_ms,
            progress,
        } = cancellation;
        if let Some(handle) = &self.compaction_handle {
            handle.background_failed();
        }
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::CompactionAttemptCancelled {
                request_bytes,
                estimated_input_tokens,
                time_to_first_byte_ms: progress.time_to_first_byte_ms.unwrap_or(0),
                time_to_first_semantic_ms: progress.time_to_first_semantic_ms.unwrap_or(0),
                duration_ms,
                send_started: progress.send_started,
                headers_received: progress.headers_received,
                first_byte_received: progress.first_byte_received,
                first_token_received: progress.first_token_received,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::CompactionUsageUnknown {
                estimated_input_tokens,
                reason: reason.into(),
            },
        )?;
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::CompactionState {
                state: crate::context::CompactionStatus::Discarded,
                reason: crate::context::CompactionReason::SoftThreshold,
                tokens_before,
                tokens_after: 0,
                duration_ms,
            },
        )
    }

    pub(super) fn jev_plan_can_pay(
        &self,
        plan: &crate::context::jev_prune::PreparedPrune,
        summary_tokens: u64,
    ) -> bool {
        let crate::context::JevInputEstimate::Eligible(input) = plan.input else {
            return false;
        };
        let Some(judge) = &self.jev_judge else {
            return false;
        };
        let savings = self
            .jev_economy
            .expected_savings(input, plan.max_saved_tokens.min(summary_tokens));
        let price = self
            .compaction_pricing
            .map(|p| p.cache_read.min(p.input))
            .or(self.compaction_input_cost_micros_per_million);
        jev_prepass_can_pay(input, savings, price, &judge.metadata()) != Some(false)
    }

    pub(super) fn build_background_compaction_plan<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        policy: &CompactionPolicy,
        budget: ContextBudget,
        foreground_required: bool,
        remaining_model_turns: usize,
    ) -> Option<BackgroundCompactionPlan> {
        let handle = self.compaction_handle.as_ref()?;
        if foreground_required
            || remaining_model_turns == 0
            || !self.background_compaction_enabled
            || !policy.enabled
            || !policy.background
            || !handle.can_prepare_background()
            || !policy.is_over_soft(budget.used_tokens, budget.window_tokens)
        {
            return None;
        }
        let capped_policy = compaction_policy_for_window(policy.clone(), budget.window_tokens);
        let selection = select_compaction_history(messages, &capped_policy).ok()?;
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();
        let previous_summary = handle.previous_summary();
        let summarized = selection.summarized_for_prompt();
        let prompt = build_bounded_summary_prompt_with_checkpoint(
            &summarized,
            previous_summary.as_deref(),
            budget.window_tokens,
            budget.reserve_tokens,
        )
        .ok()?;
        let summary_messages = [ProviderMessage::user(prompt)];
        let preflight_chars = estimate_unprepared_request_chars(
            client.adapter(),
            &summary_messages,
            &[],
            Some(COMPACTION_SYSTEM_PROMPT),
        )?;
        let preflight_input_tokens =
            self.token_estimator
                .estimate(provider, model, preflight_chars);
        let history_tokens_before = estimate_provider_message_tokens(messages);
        let fixed_request_tokens = budget.used_tokens.saturating_sub(history_tokens_before);
        let projected_tokens_after = fixed_request_tokens
            .saturating_add(projected_retained_tokens(&selection))
            .saturating_add(COMPACTION_MAX_OUTPUT_TOKENS);
        let tokens_before = budget.used_tokens;
        let items = self.todo_items();
        let completed = items
            .iter()
            .filter(|item| item.status == "completed")
            .count();
        let open = items
            .iter()
            .filter(|item| matches!(item.status.as_str(), "pending" | "in_progress"))
            .count();
        let economics = compaction_economics(
            self.compaction_pricing,
            TurnForecast {
                remaining: remaining_model_turns,
                observed: completed_provider_turns(self.app.events()),
                completed,
                open,
            },
            tokens_before,
            projected_tokens_after,
            preflight_input_tokens,
        );
        // Priced separately from the summary (see `jev_plan_can_pay`).
        let jev_plan = (policy.strategy == crate::context::CompactionStrategy::Jev
            && self.jev_judge.is_some())
        .then(|| crate::context::jev_prune::PreparedPrune::new(&selection, None, &summarized))
        .filter(|plan| self.jev_plan_can_pay(plan, preflight_input_tokens));
        let strategy = if jev_plan.is_some() {
            crate::context::CompactionStrategy::Jev
        } else {
            crate::context::CompactionStrategy::Summary
        };
        // Priced but not yet prepared; a profitable plan gains its request below.
        let mut plan = BackgroundCompactionPlan {
            selection,
            request: None,
            provider: provider.into(),
            model: model.into(),
            provider_identity: format!(
                "{:?}:{}",
                client.adapter().wire_kind(),
                client.adapter().model()
            ),
            strategy,
            jev_plan,
            previous_checkpoint: previous_summary,
            context_window_tokens: budget.window_tokens,
            reserve_tokens: budget.reserve_tokens,
            summary_max_bytes: policy.summary_max_bytes,
            serialized_chars: 0,
            system_bytes: 0,
            history_bytes: 0,
            source_len: messages.len(),
            tokens_before,
            projected_tokens_after,
            request_bytes: 0,
            estimated_input_tokens: preflight_input_tokens,
            projected_savings_tokens: economics.projected_savings_tokens,
            estimated_cost_tokens: economics.estimated_cost_tokens,
            safety_margin_tokens: economics.safety_margin_tokens,
            future_turns: economics.future_turns,
            profitable: false,
        };
        if !economics.profitable {
            return Some(plan);
        }
        if preflight_input_tokens.saturating_add(budget.reserve_tokens) > budget.window_tokens {
            return None;
        }
        let mut request = client.prepare_compaction_messages(&summary_messages).ok()?;
        let ProviderRequestComponents {
            system_bytes,
            history_bytes,
            ..
        } = request.components;
        plan.request_bytes = u64::try_from(request.body.len()).unwrap_or(u64::MAX);
        plan.estimated_input_tokens =
            self.token_estimator
                .estimate(provider, model, request.serialized_chars);
        debug_assert!(request.serialized_chars <= preflight_chars);
        request.estimated_tokens = plan.estimated_input_tokens;
        plan.serialized_chars = request.serialized_chars;
        plan.system_bytes = system_bytes;
        plan.history_bytes = history_bytes;
        plan.request = Some(request);
        plan.profitable = true;
        Some(plan)
    }
}

/// The persisted telemetry of a background compaction that did not finish.
pub(super) struct BackgroundCancellation {
    pub(super) request_bytes: u64,
    pub(super) estimated_input_tokens: u64,
    pub(super) tokens_before: u64,
    pub(super) duration_ms: u64,
    pub(super) progress: CompactionAttemptProgress,
}

/// Whether a background compaction pays for itself, with the figures behind
/// the decision.
pub(super) struct CompactionEconomics {
    pub(super) future_turns: u8,
    pub(super) projected_savings_tokens: u64,
    pub(super) estimated_cost_tokens: u64,
    pub(super) safety_margin_tokens: u64,
    pub(super) profitable: bool,
}

/// Savings accrue on each future turn; the cost is the summary request plus
/// its output, with a quarter as safety margin (or the caller's prices).
pub(super) fn compaction_economics(
    pricing: Option<CompactionPricing>,
    forecast: TurnForecast,
    tokens_before: u64,
    projected_tokens_after: u64,
    input_tokens: u64,
) -> CompactionEconomics {
    let future_turns = forecast.future_turns();
    let projected_savings_tokens = tokens_before
        .saturating_sub(projected_tokens_after)
        .saturating_mul(u64::from(future_turns));
    let estimated_cost_tokens = input_tokens.saturating_add(COMPACTION_MAX_OUTPUT_TOKENS);
    let safety_margin_tokens = estimated_cost_tokens.div_ceil(4);
    let profitable = pricing.map_or_else(
        || projected_savings_tokens > estimated_cost_tokens.saturating_add(safety_margin_tokens),
        |pricing| {
            pricing.can_pay(
                tokens_before,
                projected_tokens_after,
                input_tokens,
                COMPACTION_MAX_OUTPUT_TOKENS,
                future_turns,
            )
        },
    );
    CompactionEconomics {
        future_turns,
        projected_savings_tokens,
        estimated_cost_tokens,
        safety_margin_tokens,
        profitable,
    }
}

/// Estimated tokens of the history a compaction would keep: system and
/// developer messages, the root instruction, the pinned and the kept
/// messages. The estimate is a sum over messages, so no history is copied.
pub(super) fn projected_retained_tokens(selection: &CompactionSelection) -> u64 {
    let instructions = selection
        .summarized
        .iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        .map(|message| estimate_provider_message_tokens(std::slice::from_ref(message)))
        .sum::<u64>();
    let root = ProviderMessage::user(selection.root_instruction.clone());
    instructions
        + estimate_provider_message_tokens(std::slice::from_ref(&root))
        + estimate_provider_message_tokens(&selection.pinned)
        + estimate_provider_message_tokens(&selection.kept)
}
