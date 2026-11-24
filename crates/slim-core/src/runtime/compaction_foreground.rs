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
use crate::context::{
    build_chunk_merge_summary_request, canonical_prefix_fingerprint, find_turn_start_index,
    CompactionPreparation, SpeculativeCompaction, SummaryModelPolicy, SummaryRequest,
    INITIAL_IN_PLACE_SUMMARY_MAX_TOKENS,
};

const MAX_PARALLEL_COMPACTION_CHUNKS: usize = 4;

pub(super) fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn merge_compaction_instructions(manual: Option<&str>, working_set: Option<&str>) -> Option<String> {
    match (
        manual.map(str::trim).filter(|s| !s.is_empty()),
        working_set.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(m), Some(w)) => Some(format!("{m}\n\n{w}")),
        (Some(m), None) => Some(m.to_owned()),
        (None, Some(w)) => Some(w.to_owned()),
        (None, None) => None,
    }
}

fn truncate_working_set_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn is_fast_model_rejection(error: &ProviderError) -> bool {
    match error {
        ProviderError::Http {
            status, message, ..
        } => {
            let msg = message.to_ascii_lowercase();
            matches!(status, 400 | 404 | 422) && !msg.contains("context") && !msg.contains("token")
        }
        ProviderError::Api { metadata, message } => {
            if metadata
                .classification_code()
                .is_some_and(|code| code.contains("context") || code.contains("rate_limit"))
            {
                return false;
            }
            let code = metadata.code.as_deref().unwrap_or("").to_ascii_lowercase();
            let err_type = metadata
                .error_type
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            let msg = message.to_ascii_lowercase();
            code.contains("model")
                || err_type.contains("not_found")
                || (msg.contains("model")
                    && (msg.contains("invalid")
                        || msg.contains("not found")
                        || msg.contains("unsupported")
                        || msg.contains("permission")))
        }
        ProviderError::Remote { message } | ProviderError::InvalidResponse { message } => {
            let msg = message.to_ascii_lowercase();
            (msg.contains("model")
                && (msg.contains("invalid")
                    || msg.contains("not found")
                    || msg.contains("unsupported")))
                || msg.contains("not_found_error")
        }
        _ => false,
    }
}

/// Snapshot metadata carried with a [`PreparedSummaryCall`] so concurrent calls
/// can execute first and record cleanly ordered runtime events afterward.
struct SummarySnapshotMeta {
    provider: String,
    model: String,
    system_bytes: u64,
    history_bytes: u64,
    tool_result_bytes: u64,
    serialized_chars: u64,
    estimated_tokens: u64,
    context_window_tokens: u64,
}

/// Output of streaming a single summarization call without holding `&mut Runtime`.
struct RawSummaryStream {
    snapshot: SummarySnapshotMeta,
    collected: CompactionSummary,
    estimated_tokens: u64,
    latency_ms: u64,
    stream_result: Result<(), ProviderError>,
    calibration_eligible: bool,
    early_tool_abort: bool,
}

/// Splits a multi-message span into sequential slices each fitting roughly
/// within `target_chunk_tokens` (always advancing by at least one message, and
/// preferring to break on `"user"` / `"assistant"` boundaries rather than
/// separating a tool call from its tool result).
fn partition_messages_into_chunks(
    messages: &[ProviderMessage],
    target_chunk_tokens: u64,
) -> Vec<&[ProviderMessage]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut acc = 0_u64;
    for (index, message) in messages.iter().enumerate() {
        let tokens = crate::context::estimate_tokens(message).max(1);
        let at_clean_boundary = index > start && message.role != "tool";
        if at_clean_boundary && acc.saturating_add(tokens) > target_chunk_tokens {
            chunks.push(&messages[start..index]);
            start = index;
            acc = tokens;
        } else {
            acc = acc.saturating_add(tokens);
        }
    }
    if start < messages.len() {
        chunks.push(&messages[start..]);
    }
    chunks
}

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
    /// Single-call cache-friendly in-place summary (covers both complete turns
    /// and split turns without requiring a separate turn-prefix call).
    in_place_combined: Option<String>,
    usage: crate::UsageBreakdown,
}

/// A summarization request prepared for the wire.
struct PreparedSummaryCall {
    request: PreparedProviderRequest,
    estimated_tokens: u64,
    snapshot: SummarySnapshotMeta,
}

/// Streams a prepared summarization call without mutably borrowing [`Runtime`],
/// allowing split-turn and Map chunk requests to execute concurrently. If the
/// provider attempts to call a tool despite instructions/`tool_choice`, the
/// stream aborts immediately rather than buffering a full argument payload.
async fn execute_summary_stream<A: ProviderAdapter>(
    client: &HttpProviderClient<A>,
    call: PreparedSummaryCall,
    parent_cancel: Option<CancellationToken>,
    run_journal: Option<std::sync::Arc<std::sync::Mutex<crate::session::ManualRunJournal>>>,
    calibration_eligible: bool,
) -> RawSummaryStream {
    let started = Instant::now();
    let early_abort = CancellationToken::new();
    let early_abort_signal = early_abort.clone();
    let parent_token = parent_cancel.clone();
    let cancellation = async move {
        tokio::select! {
            _ = CancellationToken::cancelled_or_pending(parent_token) => {}
            _ = early_abort_signal.cancelled() => {}
        }
    };
    let mut collected = CompactionSummary::default();
    let stream_result = client
        .stream_prepared_cancellable_observed(
            call.request,
            cancellation,
            |event| {
                collected.push(event);
                if collected.saw_tool_call {
                    early_abort.cancel();
                }
            },
            |telemetry| persist_provider_call(&run_journal, telemetry).map(|_| ()),
        )
        .await;
    let parent_cancelled = parent_cancel.is_some_and(|token| token.is_cancelled());
    let early_tool_abort =
        matches!(&stream_result, Err(ProviderError::Cancelled))
            && collected.saw_tool_call
            && !parent_cancelled;
    RawSummaryStream {
        snapshot: call.snapshot,
        collected,
        estimated_tokens: call.estimated_tokens,
        latency_ms: elapsed_millis(started),
        stream_result,
        calibration_eligible,
        early_tool_abort,
    }
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
    build_next_request: bool,
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

    fn collect_open_todo_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        for todo in &self.scratchpad.open_todos {
            let trimmed = todo.trim();
            if !trimmed.is_empty() && !labels.iter().any(|existing| existing == trimmed) {
                labels.push(trimmed.to_owned());
            }
        }
        for item in self.todo_items() {
            if !matches!(item.status.as_str(), "completed" | "cancelled")
                && !item.title.trim().is_empty()
                && !labels.iter().any(|existing| existing == item.title.trim())
            {
                labels.push(item.title.trim().to_owned());
            }
        }
        labels
    }

    async fn collect_compaction_working_set(&self, cwd: &Path) -> Option<String> {
        let scratch = self.scratchpad.render_compact(1200);
        let todos = self.collect_open_todo_labels();
        let git_diff_stat = if cwd.exists() {
            let mut cmd = tokio::process::Command::new("git");
            cmd.arg("-C").arg(cwd).args(["diff", "--stat"]);
            cmd.kill_on_drop(true);
            match tokio::time::timeout(std::time::Duration::from_millis(250), cmd.output()).await {
                Ok(Ok(output)) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    if text.is_empty() {
                        None
                    } else {
                        Some(truncate_working_set_bytes(&text, 500).to_owned())
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        if scratch.is_none() && todos.is_empty() && git_diff_stat.is_none() {
            return None;
        }
        let mut body = String::from("<working-set>\n");
        if let Some(scratch) = &scratch {
            body.push_str(scratch);
            if !body.ends_with('\n') {
                body.push('\n');
            }
        }
        if !todos.is_empty() {
            body.push_str("Open TODOs:\n");
            for todo in todos.iter().take(12) {
                body.push_str("- ");
                body.push_str(todo);
                body.push('\n');
            }
        }
        if let Some(stat) = git_diff_stat {
            body.push_str("Git diff --stat:\n");
            body.push_str(&stat);
            if !body.ends_with('\n') {
                body.push('\n');
            }
        }
        let redacted = self.redact_sensitive(&body);
        let capped = truncate_working_set_bytes(&redacted, 1380).trim_end();
        Some(format!("{capped}\n</working-set>"))
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
        let effective_settings =
            policy.effective_settings_for_window(st.config.context_window_tokens);
        let Some(mut preparation) =
            prepare_compaction(st.messages, &effective_settings, trigger.usage)
        else {
            return Ok(CompactionOutcome::Skipped);
        };
        // Keep the policy's configured reserve for summary output sizing even
        // when the trigger threshold bumped `effective_settings.reserve_tokens`
        // for >= 400k windows.
        preparation.settings.reserve_tokens = policy.reserve_tokens;
        let instructions = self
            .compaction_handle
            .as_ref()
            .and_then(CompactionHandle::manual_instructions)
            .filter(|text| !text.trim().is_empty());

        // Wave 4: apply a pre-computed speculative checkpoint instantaneously
        // when no custom `/compact <instructions>` override was given.
        let mut partial_speculative_applied = false;
        if instructions.is_none() {
            let prefix_fp = canonical_prefix_fingerprint(
                &st.messages[..preparation.first_kept_index],
            );
            if let Some(spec) = self.compaction_handle.as_ref().and_then(|handle| {
                handle.take_exact_speculative(&prefix_fp, preparation.first_kept_index)
            }) {
                return self.apply_speculative(
                    ctx,
                    st,
                    tools,
                    &policy,
                    (trigger.reason, trigger.build_next_request),
                    preparation,
                    spec,
                );
            }
            if let Some(spec) = self.compaction_handle.as_ref().and_then(|handle| {
                handle.take_partial_speculative(st.messages, preparation.first_kept_index)
            }) {
                // Rolling compaction: at the automatic threshold, a checkpoint
                // that already brings the context under 70% of the trigger is
                // taken at its own, earlier cut. No summarizer call; what it
                // leaves is summarized by the next compaction.
                let limit = st
                    .config
                    .context_window_tokens
                    .saturating_sub(effective_settings.reserve_tokens);
                if trigger.reason == CompactionReason::Threshold
                    && estimate_context_tokens(
                        &apply_compaction(st.messages, spec.first_kept_index, &spec.summary),
                        ContextUsage {
                            anchor: None,
                            fixed_tokens: Self::fixed_context_tokens(ctx.client.adapter(), tools),
                        },
                    )
                    .tokens
                    .saturating_mul(10)
                        < limit.saturating_mul(7)
                {
                    preparation.first_kept_index = spec.first_kept_index;
                    return self.apply_speculative(
                        ctx,
                        st,
                        tools,
                        &policy,
                        (trigger.reason, trigger.build_next_request),
                        preparation,
                        spec,
                    );
                }
                let history_end = if preparation.is_split_turn {
                    find_turn_start_index(st.messages, preparation.first_kept_index, 0)
                        .unwrap_or(preparation.first_kept_index)
                } else {
                    preparation.first_kept_index
                };
                if spec.first_kept_index < history_end && !preparation.messages_to_summarize.is_empty() {
                    preparation.previous_summary = Some(spec.summary);
                    preparation.messages_to_summarize = st.messages
                        [spec.first_kept_index..history_end]
                        .iter()
                        .filter(|message| !matches!(message.role.as_str(), "system" | "developer"))
                        .cloned()
                        .collect();
                    partial_speculative_applied = true;
                }
            }
        }

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
        let working_set = self.collect_compaction_working_set(ctx.cwd).await;
        let effective_instructions =
            merge_compaction_instructions(instructions.as_deref(), working_set.as_deref());
        let (try_in_place, fast_model) =
            self.resolve_compaction_routing(ctx.client.adapter(), st, &policy);
        let in_place = if try_in_place && !partial_speculative_applied {
            self.summarize_in_place(
                ctx,
                st,
                tools,
                &preparation,
                effective_instructions.as_deref(),
            )
            .await
        } else {
            None
        };
        let answers = match in_place {
            Some(answer) => Ok(SummaryAnswers {
                history: None,
                turn_prefix: None,
                in_place_combined: Some(answer.text),
                usage: answer.usage,
            }),
            None if self.is_cancelled() => Err(ProviderError::Cancelled),
            None => {
                self.run_summary_calls_for_preparation(
                    ctx,
                    st,
                    &preparation,
                    effective_instructions.as_deref(),
                    fast_model,
                )
                .await
            }
        };
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
                        build_next_request: trigger.build_next_request,
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

    /// Finishes a compaction from a stored checkpoint, with no summarizer call.
    #[allow(clippy::too_many_arguments)]
    fn apply_speculative<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        policy: &CompactionPolicy,
        (reason, build_next_request): (CompactionReason, bool),
        preparation: CompactionPreparation,
        spec: SpeculativeCompaction,
    ) -> Result<CompactionOutcome, ProviderError> {
        let answers = SummaryAnswers {
            history: None,
            turn_prefix: None,
            in_place_combined: Some(spec.summary),
            usage: crate::UsageBreakdown {
                uncached_input_tokens: spec.input_tokens,
                output_tokens: spec.output_tokens,
                ..crate::UsageBreakdown::default()
            },
        };
        let result = self.finalize_compaction(
            ctx,
            st,
            tools,
            policy,
            CompactionResultInputs {
                reason,
                preparation,
                answers,
                started: Instant::now(),
                build_next_request,
            },
        );
        if result.is_err() {
            if let Some(handle) = &self.compaction_handle {
                handle.clear_manual();
            }
        }
        result
    }

    fn resolve_compaction_routing<A: ProviderAdapter>(
        &self,
        adapter: &A,
        st: &LoopState<'_>,
        policy: &CompactionPolicy,
    ) -> (bool, Option<(String, u64)>) {
        let provider_name = crate::provider::provider_kind_name(adapter.kind());
        let active_model = adapter.model();
        let warm_cache = self.compaction_handle.as_ref().is_some_and(|handle| {
            handle.has_warm_cache(provider_name, active_model, current_unix_secs())
        });
        let fast_model = match &policy.summary_model {
            SummaryModelPolicy::Same => None,
            SummaryModelPolicy::Explicit(model_id) => {
                let failed = self
                    .compaction_handle
                    .as_ref()
                    .is_some_and(|handle| handle.is_fast_model_failed(provider_name, model_id));
                (!failed).then(|| (model_id.clone(), st.config.context_window_tokens))
            }
            SummaryModelPolicy::Auto => adapter
                .fast_compaction_model()
                .filter(|(fast_id, _)| {
                    !self
                        .compaction_handle
                        .as_ref()
                        .is_some_and(|handle| handle.is_fast_model_failed(provider_name, fast_id))
                })
                .map(|(fast_id, fast_window)| (fast_id.to_owned(), fast_window)),
        };
        let try_in_place = !matches!(policy.summary_model, SummaryModelPolicy::Explicit(_))
            && (warm_cache || fast_model.is_none());
        (try_in_place, fast_model)
    }

    /// Runs a side-effect-free speculative compaction over the live history so a
    /// subsequent automatic threshold compaction or bare `/compact` finishes in
    /// < 5 ms. Never emits UI events, mutates session history, or touches journals.
    pub(super) async fn run_speculative_compaction_inner<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        messages: &mut Vec<ProviderMessage>,
        config: AgentLoopConfig,
        anchor: Option<UsageAnchor>,
        channel: mode::ChannelFrame,
    ) -> Result<Option<SpeculativeCompaction>, ProviderError> {
        let policy = self.compaction_policy();
        if self.is_cancelled() || !config.context_compaction_enabled || !policy.enabled || !policy.speculative {
            return Ok(None);
        }
        if self.is_already_compacted(messages) {
            return Ok(None);
        }
        let tools = self.workspace_tool_definitions(ctx.mode, ctx.cwd);
        let mut st = LoopState::new(messages, 1, config, anchor, channel);
        let usage = self.context_usage(ctx.client.adapter(), &st, &tools);
        let effective_settings =
            policy.effective_settings_for_window(st.config.context_window_tokens);
        let Some(mut preparation) = prepare_compaction(st.messages, &effective_settings, usage)
        else {
            return Ok(None);
        };
        preparation.settings.reserve_tokens = policy.reserve_tokens;
        let prefix_fp =
            canonical_prefix_fingerprint(&st.messages[..preparation.first_kept_index]);
        if self
            .compaction_handle
            .as_ref()
            .is_some_and(|handle| handle.has_speculative_for(&prefix_fp, preparation.first_kept_index))
        {
            return Ok(None);
        }
        let working_set = self.collect_compaction_working_set(ctx.cwd).await;
        let (try_in_place, fast_model) =
            self.resolve_compaction_routing(ctx.client.adapter(), &st, &policy);
        let in_place = if try_in_place {
            self.summarize_in_place(ctx, &mut st, &tools, &preparation, working_set.as_deref())
                .await
        } else {
            None
        };
        let answers = match in_place {
            Some(answer) => SummaryAnswers {
                history: None,
                turn_prefix: None,
                in_place_combined: Some(answer.text),
                usage: answer.usage,
            },
            None if self.is_cancelled() => return Ok(None),
            None => {
                self.run_summary_calls_for_preparation(
                    ctx,
                    &mut st,
                    &preparation,
                    working_set.as_deref(),
                    fast_model,
                )
                .await?
            }
        };
        if self.is_cancelled() {
            return Ok(None);
        }
        let raw_text = match answers.in_place_combined {
            Some(combined) => combined,
            None => preparation
                .assemble_summary(answers.history.as_deref(), answers.turn_prefix.as_deref())
                .map_err(|message| ProviderError::InvalidResponse {
                    message: message.into(),
                })?,
        };
        let redacted = self.redact_sensitive(&raw_text);
        let file_lists = crate::context::compute_file_lists(&preparation.file_ops);
        let open_todos = self.collect_open_todo_labels();
        let summary = crate::context::validate_and_repair_summary(
            &redacted,
            &file_lists.modified_files,
            &open_todos,
        );
        Ok(Some(SpeculativeCompaction {
            canonical_prefix_fingerprint: prefix_fp,
            first_kept_index: preparation.first_kept_index,
            summary,
            read_files: file_lists.read_files,
            modified_files: file_lists.modified_files,
            tokens_before: preparation.tokens_before,
            input_tokens: answers.usage.total_input_tokens(),
            output_tokens: answers.usage.output_tokens,
            created_at_unix_secs: current_unix_secs(),
        }))
    }

    /// The history summary asked for in place: the turn's own request (its
    /// system prompt, tools and channel overlay over the history before the
    /// cut, which the provider has just cached) followed by the summary
    /// instruction, so that history is read from the cache instead of being
    /// sent again as one serialized message. Only where Slim steers the
    /// provider's prompt cache (a cache key or explicit breakpoints): there
    /// the prefix is known to be cached. `None` when it does not apply, does
    /// not fit or brings no usable summary (a tool call among them); the
    /// serialized summary then runs as before.
    async fn summarize_in_place<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        preparation: &CompactionPreparation,
        instructions: Option<&str>,
    ) -> Option<SummaryAnswer> {
        let adapter = ctx.client.adapter();
        let capabilities = adapter.capabilities();
        let provider_name = crate::provider::provider_kind_name(adapter.kind());
        let has_observed_warm_cache = self.compaction_handle.as_ref().is_some_and(|handle| {
            handle.has_warm_cache(provider_name, adapter.model(), current_unix_secs())
        });
        if !(capabilities.supports_prompt_cache_key
            || capabilities.supports_explicit_cache_breakpoints
            || capabilities.supports_prompt_cache_options
            || has_observed_warm_cache)
        {
            return None;
        }
        let (prompt, output_cap) = preparation.in_place_summary_prompt(instructions)?;
        let mut messages = st.messages.get(..preparation.first_kept_index)?.to_vec();
        // Avoid consecutive `user` messages when the cut lands right after a
        // steer/reminder user message.
        if let Some(last) = messages.last_mut().filter(|m| m.role == "user") {
            if !last.content.is_empty() {
                last.content.push_str("\n\n");
            }
            last.content.push_str(&prompt);
        } else {
            messages.push(ProviderMessage::user(prompt));
        }
        let calibration_eligible = messages_are_text_only(&messages);
        let request = self
            .prepare_loop_request(ctx.client, &mut messages, tools, ctx.mode, &st.channel)
            .and_then(|request| ctx.client.prepare_in_place_compaction(request, output_cap))
            .ok()?;
        let provider = crate::provider::provider_kind_name(adapter.kind());
        let model = adapter.model();
        let window = st.config.context_window_tokens;
        let estimated_tokens =
            self.token_estimator
                .estimate(provider, model, request.serialized_chars);
        let output_room = request.output_token_limit().unwrap_or(output_cap);
        if estimated_tokens.saturating_add(output_room) > window {
            return None;
        }
        let request = request
            .with_estimated_tokens(estimated_tokens)
            .with_turn_routing(&st.routing);
        let ProviderRequestComponents {
            system_bytes,
            tool_schema_bytes,
            history_bytes,
            tool_result_bytes,
            ..
        } = request.components;
        let snapshot = SummarySnapshotMeta {
            provider: provider.into(),
            model: model.into(),
            system_bytes,
            history_bytes,
            tool_result_bytes,
            serialized_chars: request.serialized_chars,
            estimated_tokens,
            context_window_tokens: window,
        };
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ContextSnapshot {
                request_kind: crate::RequestKind::Compaction,
                provider: provider.into(),
                model: model.into(),
                system_bytes,
                tool_schema_bytes,
                history_bytes,
                tool_result_bytes,
                serialized_chars: request.serialized_chars,
                estimated_tokens,
                context_window_tokens: window,
            },
        )
        .ok()?;
        let call = PreparedSummaryCall {
            request,
            estimated_tokens,
            snapshot,
        };
        let raw = execute_summary_stream(
            ctx.client,
            call,
            self.cancellation.clone(),
            self.app.run_journal.clone(),
            calibration_eligible,
        )
        .await;
        let result = self.finish_summary_stream(raw, &mut st.next_seq);
        st.next_seq = self.observed_next_seq(st.next_seq);
        match result {
            Ok(answer) => Some(answer),
            Err(_) if self.is_cancelled() => None,
            Err(error) => {
                let _ = push_runtime_event(
                    &mut self.app,
                    &mut st.next_seq,
                    crate::EventKind::ProviderPhase {
                        phase: ProviderPhase::Compacting,
                        elapsed_ms: 0,
                        detail: Some(format!(
                            "in-place summary unavailable ({}); summarizing the serialized history",
                            provider_retry_reason(&error)
                        )),
                    },
                );
                None
            }
        }
    }

    /// Summarizes the preparation's spans with cache-aware fast-model routing
    /// and automatic transparent fallback to the session's active model if the
    /// fast sibling is rejected by the provider.
    async fn run_summary_calls_for_preparation<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        preparation: &CompactionPreparation,
        instructions: Option<&str>,
        fast_model: Option<(String, u64)>,
    ) -> Result<SummaryAnswers, ProviderError> {
        let model_override = fast_model
            .as_ref()
            .map(|(model, window)| (model.as_str(), *window));
        match self
            .run_summary_calls_inner(ctx, st, preparation, instructions, model_override)
            .await
        {
            Ok(answers) => Ok(answers),
            Err(error)
                if model_override.is_some()
                    && !self.is_cancelled()
                    && is_fast_model_rejection(&error) =>
            {
                if let (Some(handle), Some((fast_id, _))) =
                    (&self.compaction_handle, fast_model.as_ref())
                {
                    let provider_name = crate::provider::provider_kind_name(ctx.client.adapter().kind());
                    handle.mark_fast_model_failed(provider_name, fast_id);
                }
                self.run_summary_calls_inner(ctx, st, preparation, instructions, None)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    async fn run_summary_calls_inner<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        preparation: &CompactionPreparation,
        instructions: Option<&str>,
        model_override: Option<(&str, u64)>,
    ) -> Result<SummaryAnswers, ProviderError> {
        let mut serialized_prep = preparation.clone();
        elide_superseded_tool_outputs(&mut serialized_prep.messages_to_summarize);
        elide_superseded_tool_outputs(&mut serialized_prep.turn_prefix_messages);
        let requests = serialized_prep.summary_requests(instructions);
        let mut answers = SummaryAnswers::default();
        let target_override = model_override.map(|(model, _)| model);
        let effective_window = model_override
            .map(|(_, window)| st.config.context_window_tokens.min(window))
            .unwrap_or(st.config.context_window_tokens);

        // When both history and turn_prefix exist and each fits a single call,
        // run them concurrently via `tokio::join!` instead of serially (~45% faster).
        if let (Some(hist_req), Some(prefix_req)) =
            (requests.history.as_ref(), requests.turn_prefix.as_ref())
        {
            let fits_single = |req: &SummaryRequest, span: &[ProviderMessage]| {
                if span.len() <= 1 {
                    return true;
                }
                let provider = crate::provider::provider_kind_name(ctx.client.adapter().kind());
                let model = target_override.unwrap_or_else(|| ctx.client.adapter().model());
                let output_room = req.max_output_tokens.min(st.config.context_reserve_tokens);
                let approx = self
                    .token_estimator
                    .estimate(provider, model, req.prompt.len() as u64);
                approx.saturating_add(output_room) <= effective_window
            };
            if fits_single(hist_req, &serialized_prep.messages_to_summarize)
                && fits_single(prefix_req, &serialized_prep.turn_prefix_messages)
            {
                let limits = (effective_window, st.config.context_reserve_tokens);
                let hist_call = self.build_summary_call(
                    ctx.client,
                    hist_req,
                    limits,
                    None,
                    target_override,
                )?;
                let prefix_call = self.build_summary_call(
                    ctx.client,
                    prefix_req,
                    limits,
                    None,
                    target_override,
                )?;
                let cancel = self.cancellation.clone();
                let journal = self.app.run_journal.clone();
                let (raw_hist, raw_prefix) = tokio::join!(
                    execute_summary_stream(ctx.client, hist_call, cancel.clone(), journal.clone(), true),
                    execute_summary_stream(ctx.client, prefix_call, cancel, journal, true),
                );
                st.next_seq = self.observed_next_seq(st.next_seq);
                let hist_answer = self
                    .complete_or_retry_leg(
                        ctx,
                        st,
                        hist_req,
                        raw_hist,
                        effective_window,
                        target_override,
                    )
                    .await?;
                let prefix_answer = self
                    .complete_or_retry_leg(
                        ctx,
                        st,
                        prefix_req,
                        raw_prefix,
                        effective_window,
                        target_override,
                    )
                    .await?;
                answers.usage.absorb(hist_answer.usage);
                answers.usage.absorb(prefix_answer.usage);
                answers.history = Some(hist_answer.text);
                answers.turn_prefix = Some(prefix_answer.text);
                return Ok(answers);
            }
        }

        if let Some(request) = &requests.history {
            let answer = self
                .summarize_span_or_chunked(
                    ctx,
                    st,
                    request,
                    &serialized_prep.messages_to_summarize,
                    serialized_prep.settings.reserve_tokens,
                    instructions,
                    serialized_prep.previous_summary.as_deref(),
                    model_override,
                )
                .await?;
            answers.usage.absorb(answer.usage);
            answers.history = Some(answer.text);
        }
        if let Some(request) = &requests.turn_prefix {
            let answer = self
                .summarize_span_or_chunked(
                    ctx,
                    st,
                    request,
                    &serialized_prep.turn_prefix_messages,
                    serialized_prep.settings.reserve_tokens,
                    None,
                    None,
                    model_override,
                )
                .await?;
            answers.usage.absorb(answer.usage);
            answers.turn_prefix = Some(answer.text);
        }
        Ok(answers)
    }

    async fn complete_or_retry_leg<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        request: &SummaryRequest,
        raw: RawSummaryStream,
        effective_window: u64,
        model_override: Option<&str>,
    ) -> Result<SummaryAnswer, ProviderError> {
        self.emit_summary_snapshot(&raw.snapshot, &mut st.next_seq)?;
        match self.finish_summary_stream(raw, &mut st.next_seq) {
            Ok(answer) => Ok(answer),
            Err(error) if !self.is_cancelled() && (is_truncated_summary(&error) || recoverable_provider_error(&error)) => {
                self.summarize_with_retry(ctx, st, request, effective_window, model_override)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    /// Runs a single summary request when it fits the target window; when a
    /// multi-message span exceeds the window, partitions it into window-sized
    /// chunks, summarizes up to 4 chunks in parallel (Map), and merges them in
    /// one final Reduce call instead of serial rolling chains.
    #[allow(clippy::too_many_arguments)]
    async fn summarize_span_or_chunked<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        full_request: &SummaryRequest,
        span_messages: &[ProviderMessage],
        reserve_tokens: u64,
        instructions: Option<&str>,
        initial_previous_summary: Option<&str>,
        model_override: Option<(&str, u64)>,
    ) -> Result<SummaryAnswer, ProviderError> {
        let target_override = model_override.map(|(model, _)| model);
        let window = model_override
            .map(|(_, w)| st.config.context_window_tokens.min(w))
            .unwrap_or(st.config.context_window_tokens);
        let output_room = full_request
            .max_output_tokens
            .min(st.config.context_reserve_tokens);
        let provider = crate::provider::provider_kind_name(ctx.client.adapter().kind());
        let model = target_override.unwrap_or_else(|| ctx.client.adapter().model());
        let approx_tokens = self
            .token_estimator
            .estimate(provider, model, full_request.prompt.len() as u64);
        if span_messages.len() <= 1 || approx_tokens.saturating_add(output_room) <= window {
            return self
                .summarize_with_retry(ctx, st, full_request, window, target_override)
                .await;
        }
        let target_chunk_tokens = window
            .saturating_sub(output_room)
            .saturating_mul(55)
            .saturating_div(100)
            .max(2_048);
        let chunks = partition_messages_into_chunks(span_messages, target_chunk_tokens);
        let substantial_chunks = chunks
            .iter()
            .filter(|chunk| {
                let tokens: u64 = chunk
                    .iter()
                    .map(crate::context::estimate_tokens)
                    .sum();
                tokens >= 512
            })
            .count();
        if chunks.len() <= 1 || substantial_chunks < 2 {
            return self
                .summarize_with_retry(ctx, st, full_request, window, target_override)
                .await;
        }
        let chunk_requests: Vec<SummaryRequest> = chunks
            .iter()
            .map(|chunk| {
                crate::context::build_history_summary_request(
                    *chunk,
                    reserve_tokens,
                    instructions,
                    None,
                )
            })
            .collect();
        let limits = (window, st.config.context_reserve_tokens);
        let mut total_usage = crate::UsageBreakdown::default();
        let mut chunk_summaries = Vec::with_capacity(chunk_requests.len());

        for batch in chunk_requests.chunks(MAX_PARALLEL_COMPACTION_CHUNKS) {
            if self.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let mut prepared_batch = Vec::with_capacity(batch.len());
            for req in batch {
                prepared_batch.push(self.build_summary_call(
                    ctx.client,
                    req,
                    limits,
                    None,
                    target_override,
                )?);
            }
            let cancel = self.cancellation.clone();
            let journal = self.app.run_journal.clone();
            let futures = prepared_batch.into_iter().map(|call| {
                execute_summary_stream(ctx.client, call, cancel.clone(), journal.clone(), true)
            });
            let raws = futures_util::future::join_all(futures).await;
            st.next_seq = self.observed_next_seq(st.next_seq);
            for (req, raw) in batch.iter().zip(raws.into_iter()) {
                let answer = self
                    .complete_or_retry_leg(ctx, st, req, raw, window, target_override)
                    .await?;
                total_usage.absorb(answer.usage);
                chunk_summaries.push(answer.text);
            }
        }

        let merge_request = build_chunk_merge_summary_request(
            &chunk_summaries,
            reserve_tokens,
            instructions,
            initial_previous_summary,
        );
        let merged = self
            .summarize_with_retry(ctx, st, &merge_request, window, target_override)
            .await?;
        total_usage.absorb(merged.usage);
        Ok(SummaryAnswer {
            text: merged.text,
            usage: total_usage,
        })
    }

    /// One summarization call, retried a bounded number of times on
    /// recoverable provider errors, and once with a higher output limit when
    /// the summary ran into the limit it was sent with.
    async fn summarize_with_retry<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        request: &SummaryRequest,
        effective_window: u64,
        model_override: Option<&str>,
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
                    (effective_window, st.config.context_reserve_tokens),
                    raised_limit,
                    model_override,
                    (&mut st.next_seq, &mut sent_limit),
                )
                .await;
            match result {
                Err(error)
                    if raised_limit.is_none()
                        && is_truncated_summary(&error)
                        && !self.is_cancelled() =>
                {
                    // Allow doubling when the initial cap was constrained by `INITIAL_IN_PLACE_SUMMARY_MAX_TOKENS` /
                    // `INITIAL_HISTORY_SUMMARY_MAX_TOKENS`, while honoring small explicit reserves (< 8k).
                    let raise_cap = if sent_limit.is_some_and(|sent| sent >= INITIAL_IN_PLACE_SUMMARY_MAX_TOKENS) {
                        request.max_output_tokens.max(32_768)
                    } else {
                        request.max_output_tokens
                    };
                    let Some(raised) = sent_limit.and_then(|sent| {
                        raised_summary_limit(
                            sent,
                            raise_cap,
                            effective_window,
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
    async fn summarize_once<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        request: &SummaryRequest,
        limits: (u64, u64),
        raised_limit: Option<u64>,
        model_override: Option<&str>,
        (next_seq, sent_limit): (&mut u64, &mut Option<u64>),
    ) -> Result<SummaryAnswer, ProviderError> {
        let call = self.build_summary_call(client, request, limits, raised_limit, model_override)?;
        *sent_limit = call.request.output_token_limit();
        self.emit_summary_snapshot(&call.snapshot, next_seq)?;
        let raw = execute_summary_stream(
            client,
            call,
            self.cancellation.clone(),
            self.app.run_journal.clone(),
            true,
        )
        .await;
        self.finish_summary_stream(raw, next_seq)
    }

    fn emit_summary_snapshot(
        &mut self,
        snapshot: &SummarySnapshotMeta,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ContextSnapshot {
                request_kind: crate::RequestKind::Compaction,
                provider: snapshot.provider.clone(),
                model: snapshot.model.clone(),
                system_bytes: snapshot.system_bytes,
                tool_schema_bytes: 0,
                history_bytes: snapshot.history_bytes,
                tool_result_bytes: snapshot.tool_result_bytes,
                serialized_chars: snapshot.serialized_chars,
                estimated_tokens: snapshot.estimated_tokens,
                context_window_tokens: snapshot.context_window_tokens,
            },
        )
    }

    fn finish_summary_stream(
        &mut self,
        mut raw: RawSummaryStream,
        next_seq: &mut u64,
    ) -> Result<SummaryAnswer, ProviderError> {
        self.record_compaction_stream(&mut raw.collected, raw.estimated_tokens, next_seq)?;
        if raw.early_tool_abort {
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::RequestCompleted {
                    provider_latency_ms: raw.latency_ms,
                    cancelled: false,
                    failed: true,
                },
            )?;
            self.calibrate_latest_request(raw.calibration_eligible);
            return Err(ProviderError::InvalidResponse {
                message: "summary provider attempted to call a tool".into(),
            });
        }
        let validation_result = if raw.stream_result.is_ok() {
            raw.collected.validate()
        } else {
            Ok(())
        };
        let request_failed = raw.stream_result.is_err() || validation_result.is_err();
        let request_cancelled = matches!(&raw.stream_result, Err(ProviderError::Cancelled));
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::RequestCompleted {
                provider_latency_ms: raw.latency_ms,
                cancelled: request_cancelled,
                failed: request_failed,
            },
        )?;
        self.calibrate_latest_request(raw.calibration_eligible);
        if let Err(error) = raw.stream_result {
            return Err(self.redact_provider_error(error));
        }
        validation_result?;
        Ok(SummaryAnswer {
            text: raw.collected.text,
            usage: raw.collected.usage,
        })
    }

    /// Builds the wire request of a summarization call without emitting UI
    /// events yet so concurrent calls can prepare cleanly before running.
    fn build_summary_call<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        request: &SummaryRequest,
        (window, reserve): (u64, u64),
        raised_limit: Option<u64>,
        model_override: Option<&str>,
    ) -> Result<PreparedSummaryCall, ProviderError> {
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = model_override.unwrap_or_else(|| client.adapter().model());
        let output_room = raised_limit.unwrap_or_else(|| request.max_output_tokens.min(reserve));
        let prepare = |request: &SummaryRequest| -> Result<(PreparedProviderRequest, u64), _> {
            let messages = request.messages();
            let prepared = match raised_limit {
                Some(limit) => client.prepare_compaction_messages_with_model(
                    &messages,
                    limit,
                    true,
                    model_override,
                )?,
                None => client.prepare_compaction_messages_with_model(
                    &messages,
                    request.max_output_tokens,
                    false,
                    model_override,
                )?,
            };
            let tokens = self
                .token_estimator
                .estimate(provider, model, prepared.serialized_chars);
            Ok::<_, ProviderError>((prepared, tokens))
        };
        let fits = |tokens: u64| tokens.saturating_add(output_room) <= window;

        let (mut prepared, mut estimated_tokens) = prepare(request)?;
        if !fits(estimated_tokens) {
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
        Ok(PreparedSummaryCall {
            snapshot: SummarySnapshotMeta {
                provider: provider.into(),
                model: model.into(),
                system_bytes,
                history_bytes,
                tool_result_bytes,
                serialized_chars: prepared.serialized_chars,
                estimated_tokens,
                context_window_tokens: window,
            },
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
            build_next_request,
        } = inputs;
        let text = match answers.in_place_combined {
            Some(combined) => combined,
            None => preparation
                .assemble_summary(answers.history.as_deref(), answers.turn_prefix.as_deref())
                .map_err(|message| ProviderError::InvalidResponse {
                    message: message.into(),
                })?,
        };
        // The summarizer saw redacted history; its answer is redacted again
        // before it is stored or shown to the model.
        let text = self.redact_sensitive(&text);
        let file_lists = crate::context::compute_file_lists(&preparation.file_ops);
        let open_todos = self.collect_open_todo_labels();
        let text = crate::context::validate_and_repair_summary(
            &text,
            &file_lists.modified_files,
            &open_todos,
        );
        let fitted =
            fit_summary_for_persistence(&text, &preparation.file_ops, policy.summary_max_bytes)
                .map_err(|message| ProviderError::InvalidResponse { message })?;
        let first_kept_index = preparation.first_kept_index;
        let canonical_prefix_fingerprint =
            canonical_prefix_fingerprint(&st.messages[..first_kept_index]);
        let mut compacted = apply_compaction(st.messages, first_kept_index, &fitted.summary);
        // The rewritten history is a new prefix: the overlay is anchored again.
        let channel = self.rebuilt_channel_frame(ctx.mode, &compacted);
        let request = if build_next_request {
            Some(Box::new(self.prepare_loop_request(
                ctx.client,
                &mut compacted,
                tools,
                ctx.mode,
                &channel,
            )?))
        } else {
            None
        };
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
        Ok(CompactionOutcome::Applied(request))
    }

    pub(super) fn calibrate_latest_request(&mut self, calibration_eligible: bool) {
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
            .filter(|request| !request.cancelled && !request.response_cache_hit)
        else {
            return;
        };
        if calibration_eligible && request.usable_for_calibration() {
            let total_input = request.total_input_tokens();
            self.token_estimator
                .observe(provider, model, serialized_chars, total_input);
            if let Some(handle) = &self.compaction_handle {
                handle.observe_token_calibration(provider, model, serialized_chars, total_input);
            }
        }
        let cached = request
            .cache_read_tokens
            .saturating_add(request.cache_write_tokens);
        if cached > 0 {
            self.prompt_cache_active = true;
            if let Some(handle) = &self.compaction_handle {
                let ttl_secs = crate::context::CompactionHandle::warm_cache_ttl_secs(provider);
                handle.record_warm_cache(provider, model, current_unix_secs(), ttl_secs, cached);
            }
        }
    }
}
