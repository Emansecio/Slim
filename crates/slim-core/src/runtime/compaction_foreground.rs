use super::agent_loop::{LoopCtx, LoopState};
use super::*;

/// Jev's input cost is inflated by 5/4 before comparing it with the savings.
pub(super) const JEV_COST_MARGIN: (u128, u128) = (5, 4);

/// `None` means the price of one side is unknown. Only TypeSafe's default Jev
/// model has a published static rate; custom models and Vercel stay unpriced.
pub(super) fn jev_prepass_can_pay(
    jev_input_tokens: u64,
    max_summary_input_savings_tokens: u64,
    summary_input_micros_per_million: Option<u64>,
    judge: &crate::context::JevJudgeMetadata,
) -> Option<bool> {
    if judge.backend.as_deref() != Some("typesafe")
        || judge.requested_model.as_deref() != Some(crate::context::DEFAULT_JEV_MODEL)
    {
        return None;
    }
    let summary_price = u128::from(summary_input_micros_per_million?);
    let (margin_numerator, margin_denominator) = JEV_COST_MARGIN;
    Some(
        u128::from(max_summary_input_savings_tokens).saturating_mul(summary_price)
            > (u128::from(jev_input_tokens)
                * u128::from(crate::context::TYPESAFE_JEV_INPUT_MICROS_PER_MILLION))
                * margin_numerator
                / margin_denominator,
    )
}

pub(super) fn compaction_policy_for_window(
    mut policy: CompactionPolicy,
    context_window_tokens: u64,
) -> CompactionPolicy {
    policy.keep_recent_tokens = policy.keep_recent_for_window(context_window_tokens);
    policy
}

impl Runtime {
    /// Compacts the history before this turn's request: applies a prepared
    /// summary, runs a manual/overflow summary with bounded retries, or falls
    /// back to the local emergency summary over the hard threshold.
    pub(super) async fn compact_for_turn<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        trigger: CompactionTrigger,
        compaction_policy: &CompactionPolicy,
    ) -> Result<CompactionOutcome, ProviderError> {
        let provider_identity = format!(
            "{:?}:{}",
            ctx.client.adapter().wire_kind(),
            ctx.client.adapter().model()
        );
        let mut prepared = self
            .compaction_handle
            .as_ref()
            .and_then(|handle| handle.take_prepared(st.messages, &provider_identity));
        if prepared.is_none() && st.pending_background.is_some() {
            self.cancel_pending_background(
                &mut st.pending_background,
                &mut st.next_seq,
                "foreground_compaction_required",
            )
            .await?;
            prepared = self
                .compaction_handle
                .as_ref()
                .and_then(|handle| handle.take_prepared(st.messages, &provider_identity));
        }
        if let Some(prepared) = prepared {
            self.apply_prepared_compaction(ctx, st, tools, trigger, prepared)
                .await
        } else if trigger.manual {
            self.run_manual_compaction(ctx, st, tools, trigger).await
        } else if trigger.over_hard {
            self.run_emergency_compaction(ctx, st, tools, trigger, compaction_policy)
                .await
        } else {
            Ok(CompactionOutcome::Skipped)
        }
    }

    /// Redacts and archives `summary`; `None` means cancellation won.
    async fn archive_for_turn<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        st: &LoopState<'_>,
        selection: &CompactionSelection,
        summary: &str,
    ) -> Result<Option<String>, ProviderError> {
        let summary = self.redact_sensitive(summary);
        match self
            .archive_compaction_summary(
                selection,
                summary,
                &st.governor.compaction_snapshot(ctx.run_start_seq),
                ctx.initial_messages,
                ctx.cwd,
            )
            .await
        {
            Err(ProviderError::Cancelled) => Ok(None),
            result => result.map(Some),
        }
    }

    /// Applies the summary a background compaction prepared.
    async fn apply_prepared_compaction<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        trigger: CompactionTrigger,
        prepared: PreparedCompaction,
    ) -> Result<CompactionOutcome, ProviderError> {
        let messages: &[ProviderMessage] = st.messages;
        let selection = CompactionSelection {
            root_instruction: messages
                .iter()
                .find(|message| message.role == "user")
                .map(|message| message.content.clone())
                .unwrap_or_default(),
            summarized: messages[..prepared.first_kept_index].to_vec(),
            pinned: prepared.pinned.clone(),
            kept: messages[prepared.first_kept_index..].to_vec(),
            first_kept_index: prepared.first_kept_index,
            recent_tokens: estimate_provider_message_tokens(&messages[prepared.first_kept_index..])
                .saturating_add(estimate_provider_message_tokens(&prepared.pinned)),
        };
        let Some(summary) = self
            .archive_for_turn(ctx, st, &selection, &prepared.summary)
            .await?
        else {
            return Ok(CompactionOutcome::Cancelled);
        };
        self.finish_local_compaction(
            ctx,
            st,
            &selection,
            summary,
            LocalCompactionCommit {
                prefix_fingerprint: prepared.prefix_fingerprint,
                input_tokens: prepared.input_tokens,
                output_tokens: prepared.output_tokens,
                duration_ms: prepared.duration_ms,
                tokens_before: trigger.preflight_tokens,
            },
            tools,
        )
    }

    /// A manual or overflow summary from the provider, retried a bounded
    /// number of times on recoverable errors.
    async fn run_manual_compaction<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        trigger: CompactionTrigger,
    ) -> Result<CompactionOutcome, ProviderError> {
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ProviderPhase {
                phase: ProviderPhase::Compacting,
                elapsed_ms: 0,
                detail: None,
            },
        )?;
        let mut compaction_attempts = 0_u32;
        let compact_result = loop {
            compaction_attempts += 1;
            let result = self
                .compact_before_send(
                    ctx.client,
                    CompactionInputs {
                        messages: st.messages,
                        initial_messages: ctx.initial_messages,
                        cwd: ctx.cwd,
                        facts: &st.governor.compaction_snapshot(ctx.run_start_seq),
                        tools,
                        mode: ctx.mode,
                        tokens_before: trigger.preflight_tokens,
                        window: st.config.context_window_tokens,
                        reserve: st.config.context_reserve_tokens,
                        reason: if trigger.overflow {
                            CompactionReason::Overflow
                        } else {
                            CompactionReason::Manual
                        },
                    },
                    st.next_seq,
                )
                .await;
            match result {
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
                            break Err(annotate_provider_recovery_error(
                                blocked,
                                "compaction",
                                compaction_attempts,
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
                        break Err(ProviderError::Cancelled);
                    }
                }
                Err(error) if recoverable_provider_error(&error) => {
                    break Err(annotate_provider_recovery_error(
                        error,
                        "compaction",
                        compaction_attempts,
                        st.recovery.automatic_recoveries,
                        st.recovery.compaction_recoveries,
                        if st.recovery.automatic_recoveries >= MAX_AUTOMATIC_RECOVERIES {
                            "global automatic recovery limit reached"
                        } else {
                            "consecutive compaction recovery limit reached"
                        },
                    ));
                }
                result => break result,
            }
        };
        if compact_result.is_err() {
            if let Some(handle) = &self.compaction_handle {
                handle.invalidate();
                handle.clear_manual();
            }
        }
        if self.is_cancelled() {
            st.next_seq = self.observed_next_seq(st.next_seq);
            return Ok(CompactionOutcome::Cancelled);
        }
        let applied = compact_result?;
        *st.messages = applied.messages;
        st.next_seq = applied.next_seq;
        st.recovery.compaction_recoveries = 0;
        st.governor.forget_compacted_evidence();
        Ok(CompactionOutcome::Applied(Box::new(applied.request)))
    }

    /// Over the hard threshold with nothing prepared: a local summary.
    async fn run_emergency_compaction<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        trigger: CompactionTrigger,
        compaction_policy: &CompactionPolicy,
    ) -> Result<CompactionOutcome, ProviderError> {
        let selection = select_compaction_history(
            st.messages,
            &compaction_policy_for_window(
                compaction_policy.clone(),
                st.config.context_window_tokens,
            ),
        )
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
        let Some(summary) = self
            .archive_for_turn(ctx, st, &selection, &local_emergency_summary(&selection))
            .await?
        else {
            return Ok(CompactionOutcome::Cancelled);
        };
        self.finish_local_compaction(
            ctx,
            st,
            &selection,
            summary,
            LocalCompactionCommit {
                prefix_fingerprint: compaction_prefix_fingerprint(&selection.summarized),
                input_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
                tokens_before: trigger.preflight_tokens,
            },
            tools,
        )
    }

    /// Applies a local hard-threshold compaction: swaps the history, rebuilds
    /// the request, records the commit and announces the result.
    pub(super) fn finish_local_compaction<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        selection: &CompactionSelection,
        summary: String,
        commit: LocalCompactionCommit,
        tools: &[Value],
    ) -> Result<CompactionOutcome, ProviderError> {
        *st.messages = apply_compaction_selection(st.messages, selection, summary.clone())
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        let mut request = self.prepare_loop_request(ctx.client, st.messages, tools, ctx.mode)?;
        let tokens_after =
            self.token_estimator
                .estimate(ctx.provider, ctx.model, request.serialized_chars);
        request.estimated_tokens = tokens_after;
        if let Some(handle) = &self.compaction_handle {
            handle.commit_detailed(crate::context::CompactionCommit {
                summary,
                prefix_fingerprint: commit.prefix_fingerprint,
                first_kept_index: selection.first_kept_index,
                tokens_before: commit.tokens_before,
                tokens_after,
                input_tokens: commit.input_tokens,
                output_tokens: commit.output_tokens,
                duration_ms: commit.duration_ms,
                reason: crate::context::CompactionReason::HardThreshold,
                generation: 0,
            });
        }
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::CompactionState {
                state: crate::context::CompactionStatus::Applied,
                reason: crate::context::CompactionReason::HardThreshold,
                tokens_before: commit.tokens_before,
                tokens_after,
                duration_ms: commit.duration_ms,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::CompactionCompleted,
        )?;
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

    pub(super) async fn archive_compaction_summary(
        &self,
        selection: &CompactionSelection,
        summary: String,
        execution_facts: &str,
        initial_messages: &[ProviderMessage],
        cwd: &Path,
    ) -> Result<String, ProviderError> {
        let max_bytes = self.compaction_policy().summary_max_bytes;
        self.ensure_not_cancelled()?;
        let summary = self.redact_sensitive(&summary);
        crate::context::validate_checkpoint_content(&summary, usize::MAX).map_err(|message| {
            ProviderError::InvalidResponse {
                message: format!("compaction checkpoint rejected before archival: {message}"),
            }
        })?;
        let mut retained = String::new();
        if !execution_facts.is_empty() {
            retained.push_str("[Runtime facts at compaction; subsequent actions may invalidate them. Prior-run facts remain historical, not proof of current state.]\n");
            retained.push_str(&self.redact_sensitive(execution_facts));
        }
        let mut manifest = self.redact_sensitive(&crate::context::tool_call_manifest_with_limit(
            &selection.summarized,
            usize::MAX,
        ));
        let mut preflight = retained.clone();
        if self.artifact_store.is_some() {
            append_block(
                &mut preflight,
                "[Prior visible transcript archive reference pending.]",
            );
        }
        let mut with_manifest = preflight.clone();
        append_block(&mut with_manifest, &manifest);
        if crate::context::fit_checkpoint_content(&summary, &with_manifest, max_bytes).is_err() {
            manifest.clear();
        }
        crate::context::fit_checkpoint_content(&summary, &preflight, max_bytes).map_err(
            |message| ProviderError::InvalidResponse {
                message: format!(
                    "compaction checkpoint cannot retain operational metadata: {message}"
                ),
            },
        )?;
        let mut artifact_write = None;
        if let Some(store) = self.artifact_store.clone() {
            let recovered = restore_superseded_outputs(
                initial_messages,
                self.app.events(),
                &selection.summarized,
            );
            let transcript = crate::context::recovery_transcript(&self.redact_messages(&recovered));
            append_block(
                &mut retained,
                &transcript_reference(&store, &transcript, cwd),
            );
            artifact_write = Some((store, transcript));
        }
        let final_checkpoint =
            self.fit_final_checkpoint(&summary, &retained, &manifest, max_bytes)?;
        self.ensure_not_cancelled()?;
        if let Some((store, transcript)) = artifact_write {
            commit_context_artifact(store, transcript, self.cancellation.clone()).await?;
        }
        Ok(final_checkpoint)
    }

    /// The checkpoint text with the operational metadata that fits: the
    /// manifest is dropped, not the retained facts, when both cannot fit.
    fn fit_final_checkpoint(
        &self,
        summary: &str,
        retained: &str,
        manifest: &str,
        max_bytes: usize,
    ) -> Result<String, ProviderError> {
        let mut retained_with_manifest = retained.to_owned();
        append_block(&mut retained_with_manifest, manifest);
        let retained_with_manifest = self.redact_sensitive(&retained_with_manifest);
        match crate::context::fit_checkpoint_content(summary, &retained_with_manifest, max_bytes) {
            Ok(checkpoint) => Ok(checkpoint),
            Err(_) if !manifest.is_empty() => {
                let retained = self.redact_sensitive(retained);
                crate::context::fit_checkpoint_content(summary, &retained, max_bytes).map_err(
                    |message| ProviderError::InvalidResponse {
                        message: format!("final compaction checkpoint rejected: {message}"),
                    },
                )
            }
            Err(message) => Err(ProviderError::InvalidResponse {
                message: format!("final compaction checkpoint rejected: {message}"),
            }),
        }
    }

    /// Summarizes the compacted prefix with the provider, archives the result
    /// and applies it. Every request event of the summary is recorded.
    pub(super) async fn compact_before_send<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        inputs: CompactionInputs<'_>,
        mut next_seq: u64,
    ) -> Result<CompactionApplied, ProviderError> {
        let handle = self.compaction_handle.clone();
        let policy = self.compaction_policy();
        let capped_policy = compaction_policy_for_window(policy.clone(), inputs.window);
        let selection =
            select_compaction_history(inputs.messages, &capped_policy).map_err(|message| {
                ProviderError::InvalidResponse {
                    message: message.into(),
                }
            })?;
        let previous_summary = handle.as_ref().and_then(CompactionHandle::previous_summary);
        let manual_instructions = handle
            .as_ref()
            .and_then(CompactionHandle::manual_instructions);
        let mut summarized = selection.summarized_for_prompt();
        if policy.strategy == crate::context::CompactionStrategy::Jev {
            self.run_jev_prepass(
                &selection,
                manual_instructions.as_deref(),
                &mut summarized,
                &mut next_seq,
            )
            .await?;
        }
        let summary_request = self.prepare_summary_request(
            client,
            &inputs,
            &summarized,
            previous_summary.as_deref(),
            manual_instructions.as_deref(),
            &mut next_seq,
        )?;
        let started = Instant::now();
        let cancellation = CancellationToken::cancelled_or_pending(self.cancellation.clone());
        let mut collected = CompactionSummary::default();
        let provider_call_journal = self.app.run_journal.clone();
        let stream_result = client
            .stream_prepared_cancellable_observed(
                summary_request.request,
                cancellation,
                |event| collected.push(event),
                |telemetry| persist_provider_call(&provider_call_journal, telemetry).map(|_| ()),
            )
            .await;
        self.record_compaction_stream(
            &mut collected,
            summary_request.estimated_tokens,
            &mut next_seq,
        )?;
        let validation_result = if stream_result.is_ok() {
            collected.validate(policy.summary_max_bytes)
        } else {
            Ok(())
        };
        let request_failed = stream_result.is_err() || validation_result.is_err();
        let request_cancelled = matches!(&stream_result, Err(ProviderError::Cancelled));
        push_runtime_event(
            &mut self.app,
            &mut next_seq,
            crate::EventKind::RequestCompleted {
                provider_latency_ms: elapsed_millis(started),
                cancelled: request_cancelled,
                failed: request_failed,
            },
        )?;
        self.calibrate_latest_request(summary_request.text_only);
        if let Err(error) = stream_result {
            return Err(self.redact_provider_error(error));
        }
        validation_result?;
        self.finalize_compaction(client, &inputs, &selection, &collected, started, next_seq)
            .await
    }

    /// Jev pruning strategy: judge and drop stale tool calls/results
    /// verbatim, then let the same LLM summarize the smaller prefix. Any
    /// failure or insufficient reduction falls back to the unchanged
    /// prefix, never to a silently empty summary.
    async fn run_jev_prepass(
        &mut self,
        selection: &CompactionSelection,
        manual_instructions: Option<&str>,
        summarized: &mut [ProviderMessage],
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let jev_plan = crate::context::jev_prune::PreparedPrune::new(
            selection,
            manual_instructions,
            summarized,
        );
        let jev_cannot_pay =
            !self.jev_plan_can_pay(&jev_plan, estimate_provider_message_tokens(summarized));
        match &self.jev_judge {
            Some(judge) if jev_cannot_pay => {
                let metadata = judge.metadata();
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    jev_fallback_event(
                        "estimated summary savings cannot pay for Jev pre-pass with margin".into(),
                        jev_unattempted_stats(metadata.backend, metadata.requested_model),
                    ),
                )
            }
            Some(judge) => {
                match crate::context::jev_prune::prune_prepared(
                    &**judge,
                    jev_plan,
                    summarized,
                    self.cancellation.as_ref(),
                )
                .await
                {
                    Ok(stats) => {
                        self.jev_economy.observe(&stats);
                        push_runtime_event(
                            &mut self.app,
                            next_seq,
                            crate::EventKind::CompactionJevPruned {
                                pairs_total: stats.pairs_total as u64,
                                pairs_dropped: stats.pairs_dropped as u64,
                                results_truncated: stats.results_truncated as u64,
                                batches: stats.batches as u64,
                                batches_started: stats.batches_started as u64,
                                batches_completed: stats.batches_completed as u64,
                                estimated_saved_tokens: stats.estimated_saved_tokens,
                                input_tokens: stats.input_tokens,
                                output_tokens: stats.output_tokens,
                                usage_unknown: stats.usage_unknown,
                                backend: stats.backend,
                                requested_model: stats.requested_model,
                                model: stats.model,
                                duration_ms: stats.duration_ms,
                            },
                        )
                    }
                    Err(failure) => {
                        let cancelled =
                            matches!(failure.error, crate::context::JevPruneError::Cancelled);
                        let detail = self.redact_sensitive(&failure.to_string());
                        let stats = *failure.stats;
                        self.jev_economy.observe(&stats);
                        push_runtime_event(
                            &mut self.app,
                            next_seq,
                            jev_fallback_event(detail, stats),
                        )?;
                        if cancelled {
                            return Err(ProviderError::Cancelled);
                        }
                        Ok(())
                    }
                }
            }
            None => push_runtime_event(
                &mut self.app,
                next_seq,
                jev_fallback_event(
                    "no Jev credential configured (set TYPESAFE_API_KEY or AI_GATEWAY_API_KEY)"
                        .into(),
                    jev_unattempted_stats(None, None),
                ),
            ),
        }
    }

    /// The summary request for the (possibly pruned) prefix, rejected when it
    /// cannot fit the window with the reserve; its context snapshot is
    /// recorded.
    fn prepare_summary_request<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        inputs: &CompactionInputs<'_>,
        summarized: &[ProviderMessage],
        previous_summary: Option<&str>,
        manual_instructions: Option<&str>,
        next_seq: &mut u64,
    ) -> Result<SummaryRequest, ProviderError> {
        let summary_prompt = build_bounded_summary_prompt_with_checkpoint_and_instructions(
            summarized,
            previous_summary,
            manual_instructions,
            inputs.window,
            inputs.reserve,
        )
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
        let summary_messages = vec![ProviderMessage::user(summary_prompt)];
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();
        let mut request = client.prepare_compaction_messages(&summary_messages)?;
        let serialized_chars = request.serialized_chars;
        let estimated_tokens = self
            .token_estimator
            .estimate(provider, model, serialized_chars);
        request.estimated_tokens = estimated_tokens;
        if estimated_tokens.saturating_add(inputs.reserve) > inputs.window {
            return Err(ProviderError::InvalidResponse {
                message: "compaction request still exceeds context window".into(),
            });
        }
        let ProviderRequestComponents {
            system_bytes,
            history_bytes,
            tool_result_bytes,
            ..
        } = request.components;
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
                serialized_chars,
                estimated_tokens,
                context_window_tokens: inputs.window,
            },
        )?;
        Ok(SummaryRequest {
            request,
            estimated_tokens,
            text_only: messages_are_text_only(&summary_messages),
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

    /// Archives the validated summary, applies it to the history, records
    /// the commit and announces the compaction.
    async fn finalize_compaction<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        inputs: &CompactionInputs<'_>,
        selection: &CompactionSelection,
        collected: &CompactionSummary,
        started: Instant,
        mut next_seq: u64,
    ) -> Result<CompactionApplied, ProviderError> {
        let summary = self.redact_sensitive(&collected.text);
        let summary = self
            .archive_compaction_summary(
                selection,
                summary,
                inputs.facts,
                inputs.initial_messages,
                inputs.cwd,
            )
            .await?;
        let prefix_fingerprint = compaction_prefix_fingerprint(&selection.summarized);
        let mut compacted_messages =
            apply_compaction_selection(inputs.messages, selection, summary.clone()).map_err(
                |message| ProviderError::InvalidResponse {
                    message: message.into(),
                },
            )?;
        let duration_ms = elapsed_millis(started);
        let mut compacted_request =
            self.prepare_loop_request(client, &mut compacted_messages, inputs.tools, inputs.mode)?;
        let tokens_after = self.token_estimator.estimate(
            crate::provider::provider_kind_name(client.adapter().kind()),
            client.adapter().model(),
            compacted_request.serialized_chars,
        );
        compacted_request.estimated_tokens = tokens_after;
        if let Some(handle) = &self.compaction_handle {
            handle.commit_detailed(CompactionCommit {
                summary,
                prefix_fingerprint,
                first_kept_index: selection.first_kept_index,
                tokens_before: inputs.tokens_before,
                tokens_after,
                input_tokens: collected.usage.total_input_tokens(),
                output_tokens: collected.usage.output_tokens,
                duration_ms,
                reason: inputs.reason,
                generation: 0,
            });
            handle.clear_manual();
        }
        push_runtime_event(
            &mut self.app,
            &mut next_seq,
            crate::EventKind::CompactionState {
                state: crate::context::CompactionStatus::Applied,
                reason: inputs.reason,
                tokens_before: inputs.tokens_before,
                tokens_after,
                duration_ms,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut next_seq,
            crate::EventKind::CompactionCompleted,
        )?;
        Ok(CompactionApplied {
            messages: compacted_messages,
            request: compacted_request,
            next_seq,
        })
    }
}

/// One foreground summary request: what to compact and under which budget.
pub(super) struct CompactionInputs<'a> {
    messages: &'a [ProviderMessage],
    initial_messages: &'a [ProviderMessage],
    cwd: &'a Path,
    /// Runtime facts kept beside the checkpoint.
    facts: &'a str,
    tools: &'a [Value],
    mode: crate::OperatingMode,
    tokens_before: u64,
    window: u64,
    reserve: u64,
    reason: CompactionReason,
}

/// A foreground compaction that replaced the history.
pub(super) struct CompactionApplied {
    messages: Vec<ProviderMessage>,
    request: PreparedProviderRequest,
    next_seq: u64,
}

struct SummaryRequest {
    request: PreparedProviderRequest,
    estimated_tokens: u64,
    text_only: bool,
}

fn append_block(target: &mut String, block: &str) {
    if block.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push_str("\n\n");
    }
    target.push_str(block);
}

/// Jev's fallback: `stats` carries what the attempt spent.
fn jev_fallback_event(detail: String, stats: crate::context::JevPruneStats) -> crate::EventKind {
    crate::EventKind::CompactionJevFallback {
        detail,
        batches: stats.batches as u64,
        batches_started: stats.batches_started as u64,
        batches_completed: stats.batches_completed as u64,
        input_tokens: stats.input_tokens,
        output_tokens: stats.output_tokens,
        usage_unknown: stats.usage_unknown,
        backend: stats.backend,
        requested_model: stats.requested_model,
        model: stats.model,
        duration_ms: stats.duration_ms,
    }
}

/// Stats of a pre-pass that never ran: no batches, zero known usage.
fn jev_unattempted_stats(
    backend: Option<String>,
    requested_model: Option<String>,
) -> crate::context::JevPruneStats {
    crate::context::JevPruneStats {
        input_tokens: Some(0),
        output_tokens: Some(0),
        backend,
        requested_model,
        ..Default::default()
    }
}

/// Elision changes only the active view. Restores uniquely identified
/// original outputs before archiving; never rereads a mutated file.
fn restore_superseded_outputs(
    initial_messages: &[ProviderMessage],
    events: &[crate::SessionEvent],
    summarized: &[ProviderMessage],
) -> Vec<ProviderMessage> {
    let mut originals = std::collections::HashMap::new();
    let historical = initial_messages.iter().filter_map(|message| {
        (message.role == "tool").then_some((
            message.name.as_deref()?,
            message.tool_call_id.as_deref()?,
            message.content.as_str(),
        ))
    });
    let observed = events.iter().filter_map(|event| match &event.kind {
        crate::EventKind::ToolOutput {
            name,
            call_id,
            output,
            ..
        } => Some((name.as_str(), call_id.as_str(), output.as_str())),
        _ => None,
    });
    for (name, id, output) in historical.chain(observed) {
        if output.starts_with("[superseded ") {
            continue;
        }
        originals
            .entry((name, id))
            .and_modify(|value| {
                if *value != Some(output) {
                    *value = None;
                }
            })
            .or_insert(Some(output));
    }
    let mut recovered = summarized.to_vec();
    for message in &mut recovered {
        if message.role == "tool" && message.content.starts_with("[superseded ") {
            if let Some(Some(original)) = originals.get(&(
                message.name.as_deref().unwrap_or_default(),
                message.tool_call_id.as_deref().unwrap_or_default(),
            )) {
                message.content = (*original).to_owned();
            }
        }
    }
    recovered
}

/// The retained-context block that tells the model where the archived
/// transcript is and how to read it.
fn transcript_reference(store: &ArtifactStore, transcript: &str, cwd: &Path) -> String {
    let artifact = store.preview("context-history", transcript.as_bytes());
    let read_path = artifact
        .path
        .strip_prefix(cwd)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"));
    if let Some(path) = read_path {
        let path = serde_json::to_string(&path).expect("path serializes");
        format!("[Prior visible transcript: use read on {path} with offset=1 for an index of user-role messages (including runtime notices), checkpoints and tool_call_id evidence. Follow indexed offset/max_lines and pagination to recover historical text; earlier checkpoints link earlier archives. Apply later user corrections. Opaque reasoning and binary attachments are not included.]")
    } else {
        format!("[Prior visible transcript archived at {}; native read cannot access this artifact outside the workspace.]", artifact.path.display())
    }
}

/// Stages the transcript and publishes it atomically, unless cancellation
/// wins first.
async fn commit_context_artifact(
    store: ArtifactStore,
    transcript: String,
    cancellation: Option<CancellationToken>,
) -> Result<(), ProviderError> {
    let stage_store = store.clone();
    let staged = tokio::task::spawn_blocking(move || {
        stage_store.stage("context-history", transcript.as_bytes())
    })
    .await
    .map_err(|_| ProviderError::InvalidResponse {
        message: "context artifact worker failed".into(),
    })?
    .map_err(|_| ProviderError::InvalidResponse {
        message: "context artifact could not be staged".into(),
    })?;
    if cancellation
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        crate::context::ArtifactStore::discard_staged(staged).map_err(|_| {
            ProviderError::InvalidResponse {
                message: "cancelled compaction could not discard its staged recovery artifact"
                    .into(),
            }
        })?;
        return Err(ProviderError::Cancelled);
    }
    let committed = tokio::task::spawn_blocking(move || {
        // This check and the atomic publication share one blocking job.
        // Cancellation wins until this linearization point; after it,
        // the checkpoint transaction is committed and the shared,
        // content-addressed artifact must never be rolled back.
        if cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            crate::context::ArtifactStore::discard_staged(staged)?;
            Ok(None)
        } else {
            store.commit_staged(staged).map(Some)
        }
    })
    .await
    .map_err(|_| ProviderError::InvalidResponse {
        message: "context artifact worker failed".into(),
    })?
    .map_err(|_| ProviderError::InvalidResponse {
        message: "context artifact could not be committed".into(),
    })?;
    if committed.is_none() {
        return Err(ProviderError::Cancelled);
    }
    Ok(())
}
