use super::presentation::PresentationBatch;
use super::tool_batch::BatchRecord;
use super::tool_schedule::ToolBudgetCut;
use super::*;

const TRUNCATION_RECOVERY_PROMPT: &str = "The previous response reached its output limit. Continue from the preserved progress with one small next step or a concise answer. Do not repeat completed actions. Tool calls from the truncated response were not executed; reissue any needed call with complete arguments.";

/// Inputs of one agent-loop run that never change while it runs.
pub(super) struct LoopCtx<'a, A: ProviderAdapter> {
    pub(super) client: &'a HttpProviderClient<A>,
    pub(super) initial_messages: &'a [ProviderMessage],
    pub(super) mode: crate::OperatingMode,
    pub(super) cwd: &'a Path,
    pub(super) provider: &'static str,
    pub(super) model: &'a str,
    loop_event_start: usize,
}

impl<'a, A: ProviderAdapter> LoopCtx<'a, A> {
    pub(super) fn new(
        client: &'a HttpProviderClient<A>,
        initial_messages: &'a [ProviderMessage],
        mode: crate::OperatingMode,
        cwd: &'a Path,
        loop_event_start: usize,
    ) -> Self {
        let adapter = client.adapter();
        Self {
            client,
            initial_messages,
            mode,
            cwd,
            provider: crate::provider::provider_kind_name(adapter.kind()),
            model: adapter.model(),
            loop_event_start,
        }
    }
}

/// Mutable state carried from one turn of the loop to the next.
pub(super) struct LoopState<'m> {
    pub(super) messages: &'m mut Vec<ProviderMessage>,
    pub(super) next_seq: u64,
    pub(super) config: AgentLoopConfig,
    all_results: Vec<ToolResult>,
    reserved_tool_slots: usize,
    guard: LoopGuard,
    pub(super) governor: CausalGovernor,
    pub(super) turn: usize,
    turns: usize,
    stop: AgentLoopStop,
    budget_steers_used: usize,
    todo_cadence: TodoCadence,
    compaction_applied: bool,
    /// The last response whose provider usage anchors the context estimate:
    /// the previous run's (kept by the compaction handle) until this run
    /// gets a response of its own. A compaction drops it, its summary
    /// replaces what it described.
    pub(super) usage_anchor: Option<UsageAnchor>,
    /// A compaction happened and no response was appended since: automatic
    /// threshold compaction waits for one, so a window too small to get under
    /// the threshold cannot compact on every request.
    pub(super) awaiting_response: bool,
    pub(super) recovery: RecoveryBudget,
    /// Where the channel overlay goes in every request of this run.
    pub(super) channel: mode::ChannelFrame,
}

impl<'m> LoopState<'m> {
    fn new(
        messages: &'m mut Vec<ProviderMessage>,
        next_seq: u64,
        config: AgentLoopConfig,
        usage_anchor: Option<UsageAnchor>,
        channel: mode::ChannelFrame,
    ) -> Self {
        Self {
            messages,
            next_seq,
            config,
            all_results: Vec::new(),
            reserved_tool_slots: 0,
            guard: LoopGuard::default(),
            governor: CausalGovernor::default(),
            turn: 0,
            turns: 0,
            stop: AgentLoopStop::TurnLimit,
            budget_steers_used: 0,
            todo_cadence: TodoCadence::default(),
            compaction_applied: false,
            usage_anchor,
            awaiting_response: false,
            recovery: RecoveryBudget::new(config.context_reserve_tokens),
            channel,
        }
    }
}

/// A turn's request, ready to send.
struct PreparedTurn {
    request: PreparedProviderRequest,
    tools: Arc<[Value]>,
    current_output_limit: Option<u64>,
}

/// A turn whose provider attempt completed.
struct ActiveTurn {
    provider_turn: ProviderTurnResult,
    tools: Arc<[Value]>,
    current_output_limit: Option<u64>,
    event_start: usize,
    /// What the provider reported for this response, when it did.
    usage: Option<crate::UsageBreakdown>,
    /// Another model turn fits under `max_turns` after this one.
    more_turns: bool,
}

/// One executed tool batch, with what recording it needs.
struct ToolBatch {
    id: String,
    calls: Vec<ProviderToolCall>,
    results: Vec<ToolResult>,
    reacquisitions: std::collections::HashSet<String>,
    assistant_text: String,
}

struct BatchReport {
    budget_cut: ToolBudgetCut,
    suppressed: Vec<ProviderToolCall>,
    repeated_failure: bool,
}

/// How the loop leaves a turn early. Phases return it as their `Err` so `?`
/// propagates both these exits and a `ProviderError`.
enum TurnExit {
    /// Resend the current turn without consuming the turn budget.
    Retry,
    /// Advance to the next turn.
    NextTurn,
    /// End the run with this result.
    Return(Box<AgentLoopResult>),
    Failed(ProviderError),
}

impl From<ProviderError> for TurnExit {
    fn from(error: ProviderError) -> Self {
        Self::Failed(error)
    }
}

/// How a turn without tool calls ends.
enum TurnEnd {
    NextTurn,
    Stop(AgentLoopStop),
}

impl AgentLoopResult {
    fn empty(next_seq: u64, stop: AgentLoopStop) -> Self {
        Self {
            next_seq,
            turns: 0,
            stop,
            tool_results: Vec::new(),
            usage: UsageTotals::default(),
        }
    }
}

/// Unwraps a phase result, or leaves the current turn the way it asks to.
macro_rules! proceed {
    ($st:ident, $phase:expr) => {
        match $phase {
            Ok(value) => value,
            Err(TurnExit::Retry) => continue,
            Err(TurnExit::NextTurn) => {
                $st.turn += 1;
                continue;
            }
            Err(TurnExit::Return(result)) => return Ok(*result),
            Err(TurnExit::Failed(error)) => return Err(error),
        }
    };
}

pub(super) fn cancelled_agent_loop_result(
    next_seq: u64,
    turns: usize,
    mut all_results: Vec<ToolResult>,
    completed_batch_prefix: Vec<ToolResult>,
    usage: UsageTotals,
) -> AgentLoopResult {
    all_results.extend(completed_batch_prefix);
    AgentLoopResult {
        next_seq,
        turns,
        stop: AgentLoopStop::Cancelled,
        tool_results: all_results,
        usage,
    }
}

pub(super) fn runtime_goal_assurance(events: &[crate::SessionEvent]) -> bool {
    if events
        .iter()
        .any(|event| matches!(event.kind, crate::EventKind::TerminalError { .. }))
    {
        return false;
    }
    let last_mutation =
        events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match event.kind {
                crate::EventKind::CausalProgressObserved {
                    kind: crate::CausalProgressKind::WorkspaceChanged,
                    workspace_revision,
                    ..
                } => Some((index, workspace_revision)),
                _ => None,
            });
    let (validation_start, minimum_revision) = last_mutation
        .map(|(index, revision)| (index.saturating_add(1), revision))
        .unwrap_or((0, 0));
    events.iter().skip(validation_start).any(|event| {
        matches!(
            event.kind,
            crate::EventKind::CausalProgressObserved {
                kind: crate::CausalProgressKind::ValidationGreen,
                workspace_revision,
                ..
            } if workspace_revision >= minimum_revision
        )
    })
}

impl Runtime {
    pub async fn run_provider<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        prompt: &str,
        next_seq: u64,
    ) -> Result<u64, ProviderError> {
        self.run_provider_messages(client, &[ProviderMessage::user(prompt)], next_seq)
            .await
    }

    pub async fn run_provider_messages<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        next_seq: u64,
    ) -> Result<u64, ProviderError> {
        self.ensure_not_cancelled()?;
        let result = self
            .run_provider_messages_with_tools(client, messages, &[], next_seq, false)
            .await;
        self.ensure_not_cancelled()?;
        let result = result?;
        result.stop.ensure_normal()?;
        Ok(result.next_seq)
    }

    pub(super) async fn run_provider_messages_with_tools<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        tools: &[Value],
        next_seq: u64,
        finalize: bool,
    ) -> Result<ProviderTurnResult, ProviderError> {
        let redacted;
        let messages: &[ProviderMessage] = if self.sensitive_values.0.is_empty() {
            messages
        } else {
            redacted = self.redact_messages(messages);
            redacted.as_slice()
        };
        let mut request = if finalize {
            client.prepare_finalization_messages(messages, tools)?
        } else {
            client.prepare_messages_with_tools(messages, tools)?
        };
        let serialized_chars = request.serialized_chars;
        let ProviderRequestComponents {
            system_bytes,
            tool_schema_bytes,
            history_bytes,
            tool_result_bytes,
        } = request.components;
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();
        let estimated_tokens = self
            .token_estimator
            .estimate(provider, model, serialized_chars);
        request.estimated_tokens = estimated_tokens;
        let request_next_seq = checked_next_seq(next_seq)?;
        checked_next_seq(request_next_seq)?;
        self.app
            .push_event(crate::SessionEvent::new(
                next_seq,
                crate::EventKind::ContextSnapshot {
                    request_kind: crate::RequestKind::ProviderTurn,
                    provider: provider.into(),
                    model: model.into(),
                    system_bytes,
                    tool_schema_bytes,
                    history_bytes,
                    tool_result_bytes,
                    serialized_chars,
                    estimated_tokens,
                    // Direct APIs do not own an AgentLoopConfig denominator.
                    context_window_tokens: 0,
                },
            ))
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        self.run_provider_messages_with_tools_after_snapshot(
            client,
            messages_are_text_only(messages),
            request,
            request_next_seq,
            false,
        )
        .await
    }

    pub(super) async fn run_provider_messages_with_tools_after_snapshot<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        calibration_eligible: bool,
        request: PreparedProviderRequest,
        next_seq: u64,
        require_output: bool,
    ) -> Result<ProviderTurnResult, ProviderError> {
        self.pending_argument_repair = None;
        let mut sensitive_values = self.sensitive_values.0.clone();
        sensitive_values.extend_from_slice(request.sensitive_values());
        crate::provider::normalize_sensitive_values(&mut sensitive_values);
        let mut normalizer =
            ProviderStreamNormalizer::new(client.adapter().wire_kind(), next_seq, sensitive_values)
                .with_reasoning_classification(client.adapter().reasoning_classification());
        let output_start = self.app.events().len();
        let request_started = Instant::now();
        let cancellation = CancellationToken::cancelled_or_pending(self.cancellation.clone());
        let provider_call_journal = self.app.run_journal.clone();
        let mut provider_call_id = None;
        let stream_result = client
            .stream_prepared_cancellable_observed(
                request,
                cancellation,
                |event| {
                    normalizer.push(&mut self.app, event);
                },
                |telemetry| {
                    provider_call_id = persist_provider_call(&provider_call_journal, telemetry)?;
                    Ok(())
                },
            )
            .await;
        let stream_succeeded = stream_result.is_ok();
        let cancelled = matches!(&stream_result, Err(ProviderError::Cancelled));
        if stream_result.is_err() {
            normalizer.flush_text(&mut self.app)?;
        }
        if require_output && stream_result.is_ok() {
            self.pending_argument_repair = normalizer.argument_repair_note();
        }
        let result = match stream_result {
            Ok(()) => normalizer.finish(&mut self.app),
            Err(ProviderError::Cancelled) if self.is_cancelled() => Ok(ProviderTurnResult {
                next_seq: normalizer.next_seq(),
                blocks_tools: true,
                stop: ProviderTurnStop::Normal,
                responses_reasoning: Vec::new(),
                chat_reasoning: None,
            }),
            Err(error) => Err(self.redact_provider_error(error)),
        };
        let result = result.and_then(|turn| {
            if require_output
                && turn.stop == ProviderTurnStop::Normal
                && !self.app.events()[output_start..]
                    .iter()
                    .any(|event| match &event.kind {
                        crate::EventKind::AssistantTextDelta { text } => !text.trim().is_empty(),
                        crate::EventKind::ProviderToolCall { .. }
                        | crate::EventKind::ToolCall { .. } => true,
                        _ => false,
                    })
            {
                Err(ProviderError::InvalidResponse {
                    message: EMPTY_RESPONSE_MESSAGE.into(),
                })
            } else {
                Ok(turn)
            }
        });
        if stream_succeeded {
            if let Err(error) = &result {
                persist_provider_validation_failure(
                    &provider_call_journal,
                    provider_call_id.as_deref(),
                    error,
                )?;
            }
        }
        let provider_latency_ms = elapsed_millis(request_started);
        match result {
            Ok(mut turn) => {
                push_runtime_event(
                    &mut self.app,
                    &mut turn.next_seq,
                    crate::EventKind::RequestCompleted {
                        provider_latency_ms,
                        cancelled,
                        failed: cancelled || turn.stop != ProviderTurnStop::Normal,
                    },
                )?;
                self.calibrate_latest_request(calibration_eligible);
                Ok(turn)
            }
            Err(error) => {
                let mut completion_seq = self.observed_next_seq(next_seq);
                push_runtime_event(
                    &mut self.app,
                    &mut completion_seq,
                    crate::EventKind::RequestCompleted {
                        provider_latency_ms,
                        cancelled,
                        failed: true,
                    },
                )?;
                self.calibrate_latest_request(calibration_eligible);
                Err(error)
            }
        }
    }

    /// Budget gate for the closing call: preflights a request over `messages`
    /// (and the `tools` it keeps, none on most wires) the same way loop turns
    /// are checked. An adapter without a structural envelope bound cannot be
    /// gated and proceeds ungated.
    pub(super) fn finalization_fits_budget<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        tools: &[Value],
        config: &AgentLoopConfig,
    ) -> bool {
        let Some(serialized_chars) =
            estimate_unprepared_request_chars(client.adapter(), messages, tools, None)
        else {
            return true;
        };
        let estimated_tokens = self.token_estimator.estimate(
            crate::provider::provider_kind_name(client.adapter().kind()),
            client.adapter().model(),
            serialized_chars,
        );
        ContextBudget::new(
            config.context_window_tokens,
            estimated_tokens,
            config.context_reserve_tokens,
        )
        .can_fit(config.context_reserve_tokens)
    }

    pub async fn run_provider_turn<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        prompt: &str,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
    ) -> Result<(u64, Vec<ToolResult>), ProviderError> {
        self.run_provider_messages_turn(
            client,
            &[ProviderMessage::user(prompt)],
            mode,
            cwd,
            next_seq,
        )
        .await
    }

    pub async fn run_provider_messages_turn<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
    ) -> Result<(u64, Vec<ToolResult>), ProviderError> {
        self.ensure_not_cancelled()?;
        // Skill discovery is memoized per entry-point call.
        self.skill_discovery_cache = None;
        self.write_projection_cache
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let event_start = self.app.events().len();
        self.cached_skill_discovery(cwd.as_ref());
        let tools = self.workspace_tool_definitions(mode, cwd.as_ref());
        let provider_result = self
            .run_provider_messages_with_tools(client, messages, tools.as_ref(), next_seq, false)
            .await;
        self.ensure_not_cancelled()?;
        let provider_turn = provider_result?;
        provider_turn.stop.ensure_normal()?;
        let blocks_tools = provider_turn.blocks_tools;
        let mut calls = tool_calls_since(&self.app, event_start);
        if blocks_tools {
            calls.clear();
        }
        let batch_id = format!("slim-batch-direct-{}", provider_turn.next_seq);
        assign_missing_call_ids(&mut calls, &batch_id);
        self.codemode.remaining_calls = AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS
            .saturating_sub(
                calls
                    .iter()
                    .filter(|call| !tool_call_is_read_only(&call.name))
                    .map(tool_call_slots)
                    .sum::<usize>(),
            );
        self.codemode.used_calls = 0;
        let mut governor = CausalGovernor::default();
        let (results, next_seq) = self
            .execute_provider_tool_batch(
                mode,
                cwd.as_ref(),
                &batch_id,
                &calls,
                provider_turn.next_seq,
                &mut governor,
            )
            .await?;
        self.ensure_not_cancelled()?;
        Ok((next_seq, results))
    }

    pub async fn run_agent_loop<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        prompt: &str,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        config: AgentLoopConfig,
    ) -> Result<AgentLoopResult, ProviderError> {
        self.run_agent_loop_with_messages(
            client,
            &[ProviderMessage::user(prompt)],
            mode,
            cwd,
            next_seq,
            config,
        )
        .await
    }

    pub async fn run_agent_loop_with_messages<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        initial_messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        config: AgentLoopConfig,
    ) -> Result<AgentLoopResult, ProviderError> {
        self.run_scoped(
            client,
            initial_messages,
            mode,
            cwd.as_ref(),
            next_seq,
            config,
            false,
        )
        .await
    }

    /// Compacts `initial_messages` as a manual `/compact` does while the
    /// session is idle: the summary is produced right away, no model turn
    /// follows, and the compacted history is left in `conversation()`. The
    /// commit waits in the compaction handle like any other. Nothing to
    /// compact is not an error: the history comes back as it was.
    pub async fn compact_messages<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        initial_messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        config: AgentLoopConfig,
    ) -> Result<AgentLoopResult, ProviderError> {
        self.run_scoped(
            client,
            initial_messages,
            mode,
            cwd.as_ref(),
            next_seq,
            config,
            true,
        )
        .await
    }

    /// The run scaffolding both entry points share (journal, shell jobs and
    /// the history hand-back) around either the loop or a lone compaction.
    #[allow(clippy::too_many_arguments)]
    async fn run_scoped<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        initial_messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: &Path,
        next_seq: u64,
        config: AgentLoopConfig,
        compact_only: bool,
    ) -> Result<AgentLoopResult, ProviderError> {
        self.tools
            .configure_artifacts(self.artifact_store.clone(), &self.sensitive_values.0);
        if !self.session_shell_jobs {
            self.shell_jobs = ShellJobs::new(self.shell_jobs.limits())
                .map_err(|message| ProviderError::InvalidResponse { message })?;
        }
        self.uncommitted_event_start = None;
        self.finalization_error = None;
        if let Some(handle) = &self.compaction_handle {
            // Commits belong to this run; the compacted history survives.
            drop(handle.take_commits());
        }
        if let Some(journal) = &self.app.run_journal {
            journal
                .lock()
                .map_err(|_| journal_error("durable run lock poisoned"))?
                .configure_output(self.artifact_store.clone(), config.max_result_bytes);
        }
        self.shell_job_run_start = self.shell_jobs.last_id();
        self.shell_jobs
            .attach_journal(self.app.run_journal.as_ref());
        self.shell_jobs.record_snapshot();
        let _job_scope = (!self.session_shell_jobs).then(|| self.shell_jobs.scope());
        // The loop owns the only copy of the history while it runs and hands it
        // back on every exit path; a run that stops before seeding keeps it.
        let mut messages = std::mem::take(&mut self.conversation);
        let ctx = LoopCtx::new(client, initial_messages, mode, cwd, self.app.events().len());
        let mut result = if compact_only {
            self.compact_inner(&ctx, next_seq, config, &mut messages)
                .await
        } else {
            self.run_agent_loop_inner(&ctx, next_seq, config, &mut messages)
                .await
        };
        self.conversation = messages;
        if !self.session_shell_jobs {
            self.shell_jobs.shutdown().await;
        }
        if let Some(start) = self.uncommitted_event_start.take() {
            self.retain_interrupted_turn(start, config.max_result_bytes)?;
        }
        let mut completion_seq =
            self.observed_next_seq(result.as_ref().map_or(next_seq, |r| r.next_seq));
        let mut messages = std::mem::take(&mut self.conversation);
        let delivered = if self.session_shell_jobs && compact_only {
            Ok(false)
        } else {
            self.deliver_shell_completions(
                &mut messages,
                config.max_result_bytes,
                &mut completion_seq,
            )
        };
        self.conversation = messages;
        delivered?;
        if let Ok(result) = &mut result {
            result.next_seq = completion_seq;
        }
        result
    }

    /// A lone manual compaction of the seeded history: no request follows.
    /// Whatever the outcome (an early return included) the request is
    /// answered: it does not wait for a later boundary.
    async fn compact_inner<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        next_seq: u64,
        config: AgentLoopConfig,
        messages: &mut Vec<ProviderMessage>,
    ) -> Result<AgentLoopResult, ProviderError> {
        let result = self.compact_seeded(ctx, next_seq, config, messages).await;
        if let Some(handle) = &self.compaction_handle {
            handle.clear_manual();
        }
        result
    }

    /// Manual compaction is Pi's /compact: `[compaction] enabled` gates only
    /// the automatic triggers, so it does not apply here.
    async fn compact_seeded<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        next_seq: u64,
        config: AgentLoopConfig,
        messages: &mut Vec<ProviderMessage>,
    ) -> Result<AgentLoopResult, ProviderError> {
        // Seed first: a run cancelled (or refused) before the summarizer
        // still hands back the unchanged history instead of an empty one.
        *messages = self.redact_messages(ctx.initial_messages);
        if self.is_cancelled() {
            return Ok(AgentLoopResult::empty(next_seq, AgentLoopStop::Cancelled));
        }
        if !config.context_compaction_enabled {
            return Err(ProviderError::InvalidResponse {
                message: "compaction is disabled".into(),
            });
        }
        self.prepare_loop_capabilities(ctx.cwd)?;
        // The summarizer reads what a run's model would: own reasoning only
        // and no superseded tool output.
        retain_own_reasoning(ctx.client.adapter(), messages);
        let elision = elide_superseded_tool_outputs(messages);
        // An elision changed what the last response's usage described.
        let anchor = if elision.elided > 0 {
            if let Some(handle) = &self.compaction_handle {
                handle.clear_usage_anchor();
            }
            None
        } else {
            self.stored_usage_anchor(ctx, messages)
        };
        let channel = self.channel_frame(ctx.mode, messages);
        let mut st = LoopState::new(messages, next_seq, config, anchor, channel);
        let tools = self.workspace_tool_definitions(ctx.mode, ctx.cwd);
        let usage = self.context_usage(ctx.client.adapter(), &st, &tools);
        let trigger = CompactionTrigger {
            reason: CompactionReason::Manual,
            required: true,
            usage,
        };
        let outcome = self.compact_for_turn(ctx, &mut st, &tools, trigger).await;
        let stop = match outcome? {
            CompactionOutcome::Cancelled => AgentLoopStop::Cancelled,
            CompactionOutcome::Applied(_) | CompactionOutcome::Skipped => {
                AgentLoopStop::ProviderCompleted
            }
        };
        Ok(AgentLoopResult::empty(st.next_seq, stop))
    }

    pub(super) async fn run_agent_loop_inner<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        next_seq: u64,
        config: AgentLoopConfig,
        messages: &mut Vec<ProviderMessage>,
    ) -> Result<AgentLoopResult, ProviderError> {
        if self.is_cancelled() {
            return Ok(AgentLoopResult::empty(next_seq, AgentLoopStop::Cancelled));
        }
        if config.max_turns == 0 {
            return Ok(AgentLoopResult::empty(next_seq, AgentLoopStop::TurnLimit));
        }
        let mut st = self.seed_loop(ctx, next_seq, config, messages)?;

        // Transport retries resend the current turn: they are bounded by their
        // own recovery budgets and do not advance `turn`.
        while st.turn < st.config.max_turns {
            proceed!(st, self.begin_turn(ctx, &mut st).await);
            let prepared = proceed!(st, self.prepare_request(ctx, &mut st).await);
            let mut active = proceed!(st, self.call_provider(ctx, &mut st, prepared).await);
            let calls = if active.provider_turn.blocks_tools {
                Vec::new()
            } else {
                tool_calls_since(&self.app, active.event_start)
            };
            if calls.is_empty() {
                match self.finish_or_continue(ctx, &mut st, &mut active).await? {
                    TurnEnd::NextTurn => {
                        st.turn += 1;
                        continue;
                    }
                    TurnEnd::Stop(stop) => {
                        st.stop = stop;
                        break;
                    }
                }
            }
            let report = proceed!(
                st,
                self.run_tool_batch(ctx, &mut st, &mut active, calls).await
            );
            if let Some(stop) = self.after_batch(ctx, &mut st, &active, &report).await? {
                st.stop = stop;
                break;
            }
            st.turn += 1;
            self.app.discard_projected_payloads();
        }
        self.finalize_loop(ctx, &mut st).await
    }

    /// Prepares the run: capabilities, the seeded history and the loop state.
    fn seed_loop<'m, A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        mut next_seq: u64,
        config: AgentLoopConfig,
        messages: &'m mut Vec<ProviderMessage>,
    ) -> Result<LoopState<'m>, ProviderError> {
        self.prepare_loop_capabilities(ctx.cwd)?;
        let mut seeded = self.redact_messages(ctx.initial_messages);
        self.add_initial_workspace_context(
            ctx.client.adapter(),
            &mut seeded,
            ctx.mode,
            ctx.cwd,
            config,
        );
        retain_own_reasoning(ctx.client.adapter(), &mut seeded);
        let seed_elision = elide_superseded_tool_outputs(&mut seeded);
        *messages = seeded;
        if seed_elision.elided > 0 {
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolEvidenceElided {
                    count: u64::from(seed_elision.elided),
                    original_bytes: seed_elision.original_bytes,
                    emitted_bytes: seed_elision.emitted_bytes,
                },
            )?;
        }
        self.edited = EditedFiles::default();
        // An elision changed what the last response's usage described.
        let anchor = if seed_elision.elided > 0 {
            if let Some(handle) = &self.compaction_handle {
                handle.clear_usage_anchor();
            }
            None
        } else {
            self.stored_usage_anchor(ctx, messages)
        };
        let channel = self.channel_frame(ctx.mode, messages);
        Ok(LoopState::new(messages, next_seq, config, anchor, channel))
    }

    /// The usage anchor the previous run left in the compaction handle, when
    /// this run's history still has that response where it was.
    fn stored_usage_anchor<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        messages: &[ProviderMessage],
    ) -> Option<UsageAnchor> {
        self.compaction_handle
            .as_ref()?
            .usage_anchor(Some(&usage_source(ctx.provider, ctx.model)), messages)
    }

    /// Cancellation, shell-job delivery and turn bookkeeping.
    async fn begin_turn<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<(), TurnExit> {
        if self.is_cancelled() {
            return Err(self.cancel_loop(ctx, st, Vec::new()));
        }
        self.deliver_loop_shell_jobs(st)?;
        st.turns = st.turn + 1;
        Ok(())
    }

    /// Builds this turn's request: preflight, compaction when Pi's trigger
    /// (or a manual or overflow request) asks for it, output-limit recovery
    /// and the final context gate.
    async fn prepare_request<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<PreparedTurn, TurnExit> {
        // Skills stay fixed for the run. A newly available code backend
        // still needs to be advertised after a project marker is created.
        let tools = self.workspace_tool_definitions(ctx.mode, ctx.cwd);
        let (preflight_chars, preflight_tokens, mut request) =
            self.preflight_request(ctx, st.messages, &tools, &st.channel)?;
        if let Some(trigger) = self.compaction_trigger(ctx, st, &tools, preflight_tokens) {
            match self.compact_for_turn(ctx, st, &tools, trigger).await {
                Ok(CompactionOutcome::Applied(applied)) => {
                    request = Some(*applied);
                    st.compaction_applied = true;
                    st.guard = LoopGuard::default();
                }
                Ok(CompactionOutcome::Skipped) => {}
                Ok(CompactionOutcome::Cancelled) => {
                    return Err(TurnExit::Return(self.cancelled_result(ctx, st, Vec::new())));
                }
                Err(error) if trigger.required || matches!(error, ProviderError::Cancelled) => {
                    return Err(error.into());
                }
                Err(error) => {
                    // Pi reports a failed automatic compaction and sends the
                    // request as it is; the context gate below still rejects
                    // a request that does not fit. The next attempt waits for
                    // a response, as after a compaction.
                    let error = self.redact_provider_error(error);
                    st.next_seq = self.observed_next_seq(st.next_seq);
                    push_runtime_event(
                        &mut self.app,
                        &mut st.next_seq,
                        crate::EventKind::auto_compaction_failed(&provider_retry_reason(&error)),
                    )?;
                    st.awaiting_response = true;
                }
            }
        }
        let mut request = match request {
            Some(request) => request,
            None => {
                self.prepare_loop_request(ctx.client, st.messages, &tools, ctx.mode, &st.channel)?
            }
        };
        if let Some(limit) = st.recovery.recovery_output_limit {
            request = ctx.client.with_recovery_output_limit(request, limit)?;
        }
        let current_output_limit = request.output_token_limit();
        // The estimate counts an image as a fixed figure, not as its base64.
        let estimated_chars = request
            .serialized_chars
            .saturating_sub(image_payload_discount_chars(st.messages));
        if !st.compaction_applied && st.recovery.recovery_output_limit.is_none() {
            debug_assert!(estimated_chars <= preflight_chars);
        }
        let estimated_tokens =
            self.token_estimator
                .estimate(ctx.provider, ctx.model, estimated_chars);
        request.estimated_tokens = estimated_tokens;
        let budget = ContextBudget::new(
            st.config.context_window_tokens,
            estimated_tokens,
            st.config.context_reserve_tokens,
        );
        if !budget.can_fit(st.config.context_reserve_tokens) {
            return Err(ProviderError::InvalidResponse {
                message: if st.compaction_applied {
                    "context window still exceeded after compaction".into()
                } else {
                    "context window exceeded and compaction is unavailable".into()
                },
            }
            .into());
        }
        Ok(PreparedTurn {
            request,
            tools,
            current_output_limit,
        })
    }

    /// Whether this turn compacts before its request, and why: a manual or
    /// overflow request, Pi's threshold (the context estimate above the window
    /// minus the reserve), or the request not fitting the context gate that
    /// follows (`preflight_tokens` and the output reserve against the window:
    /// the reserve there is the output limit, which can exceed Pi's). The
    /// threshold waits for a response after a compaction, so a window too
    /// small to get under it cannot compact on every request. `[compaction]
    /// enabled` gates the automatic reasons; a manual request always runs.
    fn compaction_trigger<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
        preflight_tokens: u64,
    ) -> Option<CompactionTrigger> {
        let overflow = std::mem::take(&mut st.recovery.overflow_compaction_pending);
        let policy = self.compaction_policy();
        if !st.config.context_compaction_enabled {
            return None;
        }
        let manual = self
            .compaction_handle
            .as_ref()
            .and_then(CompactionHandle::manual_instructions)
            .is_some();
        let usage = self.context_usage(ctx.client.adapter(), st, tools);
        // The request not fitting the gate is a hard need; Pi's line alone is
        // not.
        let over_gate = preflight_tokens.saturating_add(st.config.context_reserve_tokens)
            > st.config.context_window_tokens;
        let (reason, required) = if overflow && policy.enabled {
            (CompactionReason::Overflow, true)
        } else if manual {
            (CompactionReason::Manual, true)
        } else if policy.enabled
            && !st.awaiting_response
            && (over_gate
                || should_compact(
                    estimate_context_tokens(st.messages, usage).tokens,
                    st.config.context_window_tokens,
                    &policy.settings(),
                ))
        {
            (CompactionReason::Threshold, over_gate)
        } else {
            return None;
        };
        Some(CompactionTrigger {
            reason,
            required,
            usage,
        })
    }

    /// Estimates the turn's request before compaction: structurally when the
    /// adapter has an envelope bound, otherwise by preparing it.
    fn preflight_request<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        messages: &mut [ProviderMessage],
        tools: &[Value],
        channel: &mode::ChannelFrame,
    ) -> Result<(u64, u64, Option<PreparedProviderRequest>), ProviderError> {
        let overlay = self.overlay_channel(messages, ctx.mode, channel);
        let structural_chars =
            estimate_unprepared_request_chars(ctx.client.adapter(), overlay.view(), tools, None);
        match structural_chars {
            Some(chars) => Ok((
                chars,
                self.token_estimator
                    .estimate(ctx.provider, ctx.model, chars),
                None,
            )),
            None => {
                let mut request = ctx
                    .client
                    .prepare_messages_with_tools(overlay.view(), tools)?;
                let chars = request
                    .serialized_chars
                    .saturating_sub(image_payload_discount_chars(overlay.view()));
                let tokens = self
                    .token_estimator
                    .estimate(ctx.provider, ctx.model, chars);
                request.estimated_tokens = tokens;
                Ok((chars, tokens, Some(request)))
            }
        }
    }

    /// Records the request snapshot and calls the provider, then applies the
    /// recovery policy to the attempt.
    async fn call_provider<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        prepared: PreparedTurn,
    ) -> Result<ActiveTurn, TurnExit> {
        let PreparedTurn {
            request,
            tools,
            current_output_limit,
        } = prepared;
        let event_start = self.app.events().len();
        self.uncommitted_event_start = Some(event_start);
        self.push_request_snapshot(ctx, st, &request)?;
        if self.is_cancelled() {
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::RequestCompleted {
                    provider_latency_ms: 0,
                    cancelled: true,
                    failed: true,
                },
            )?;
            return Err(self.cancel_loop(ctx, st, Vec::new()));
        }
        let provider_result = self
            .run_provider_messages_with_tools_after_snapshot(
                ctx.client,
                messages_are_text_only(st.messages),
                request,
                st.next_seq,
                true,
            )
            .await;
        st.recovery.provider_attempts = st.recovery.provider_attempts.saturating_add(1);
        st.next_seq = provider_result.as_ref().map_or_else(
            |_| self.observed_next_seq(st.next_seq),
            |turn| turn.next_seq,
        );
        if self.is_cancelled() {
            return Err(self.cancel_loop(ctx, st, Vec::new()));
        }
        let provider_turn = match self
            .settle_provider_attempt(provider_result, st, &AttemptCtx { event_start })
            .await?
        {
            ProviderAttempt::Completed(provider_turn) => provider_turn,
            ProviderAttempt::Retry => return Err(TurnExit::Retry),
            ProviderAttempt::NextTurn => return Err(TurnExit::NextTurn),
        };
        let usage = self.response_usage(event_start);
        Ok(ActiveTurn {
            provider_turn,
            tools,
            current_output_limit,
            event_start,
            usage,
            // `max_turns` never changes during a run, so this holds for the
            // rest of the turn.
            more_turns: st.turn + 1 < st.config.max_turns,
        })
    }

    /// What the provider reported for the response requested since
    /// `event_start`; a cancelled request or a response-cache hit reports
    /// nothing usable.
    fn response_usage(&self, event_start: usize) -> Option<crate::UsageBreakdown> {
        let ledger = UsageTotals::from_events(self.app.events().get(event_start..)?, false);
        let request = ledger.requests.first()?;
        (!request.cancelled && !request.response_cache_hit).then_some(crate::UsageBreakdown {
            uncached_input_tokens: request.uncached_input_tokens,
            cache_write_tokens: request.cache_write_tokens,
            cache_read_tokens: request.cache_read_tokens,
            output_tokens: request.output_tokens,
            reasoning_tokens: request.reasoning_tokens,
            usage_unknown: request.usage_unknown,
        })
    }

    /// Anchors the context estimate on the response just appended to the
    /// history. A response without usable usage leaves the previous anchor, as
    /// Pi keeps using the last response that had some.
    /// The anchor also goes to the compaction handle, which carries it into
    /// the next run.
    fn anchor_usage<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        usage: Option<crate::UsageBreakdown>,
    ) {
        st.awaiting_response = false;
        let index = st.messages.len().saturating_sub(1);
        if let Some(anchor) = usage.and_then(|usage| UsageAnchor::from_breakdown(index, &usage)) {
            st.usage_anchor = Some(anchor);
            if let Some(handle) = &self.compaction_handle {
                handle.record_usage_anchor(
                    &usage_source(ctx.provider, ctx.model),
                    st.messages,
                    anchor,
                );
            }
        }
    }

    fn push_request_snapshot<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        request: &PreparedProviderRequest,
    ) -> Result<(), ProviderError> {
        let request_next_seq = checked_next_seq(st.next_seq)?;
        checked_next_seq(request_next_seq)?;
        let ProviderRequestComponents {
            system_bytes,
            tool_schema_bytes,
            history_bytes,
            tool_result_bytes,
        } = request.components;
        let snapshot = crate::SessionEvent::new(
            st.next_seq,
            crate::EventKind::ContextSnapshot {
                request_kind: crate::RequestKind::ProviderTurn,
                provider: ctx.provider.into(),
                model: ctx.model.into(),
                system_bytes,
                tool_schema_bytes,
                history_bytes,
                tool_result_bytes,
                serialized_chars: request.serialized_chars,
                estimated_tokens: request.estimated_tokens,
                context_window_tokens: st.config.context_window_tokens,
            },
        );
        self.app
            .push_event(snapshot)
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        st.next_seq = request_next_seq;
        Ok(())
    }

    /// A turn without tool calls: waits on shell jobs, recovers a truncated
    /// response, asks for the final todo review, or ends the run.
    async fn finish_or_continue<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        active: &mut ActiveTurn,
    ) -> Result<TurnEnd, ProviderError> {
        let assistant_text = assistant_text_since(&self.app, active.event_start);
        let assistant = active
            .provider_turn
            .take_assistant_message(assistant_text, Vec::new());
        self.append_conversation_message(st.messages, assistant)?;
        self.anchor_usage(ctx, st, active.usage);
        self.uncommitted_event_start = None;
        let provider_stop = active.provider_turn.stop;
        // An idle agent waits on local job events, not more model calls.
        // Completed output resumes the loop as an explicit harness message.
        if !self.session_shell_jobs
            && provider_stop == ProviderTurnStop::Normal
            && self.shell_jobs.running()
        {
            self.await_shell_jobs(st).await?;
            return Ok(TurnEnd::NextTurn);
        }
        if self.deliver_loop_shell_jobs(st)? {
            return Ok(TurnEnd::NextTurn);
        }
        // Truncated tools are never executed. Preserve the partial answer
        // and completed tool results, then ask for a small next step.
        if provider_stop == ProviderTurnStop::Truncated
            && st.recovery.truncation_recoveries < MAX_TRUNCATION_RECOVERIES
            && active.more_turns
        {
            self.recover_truncated_response(ctx, st, active.current_output_limit)?;
            return Ok(TurnEnd::NextTurn);
        }
        if provider_stop == ProviderTurnStop::Normal
            && ctx.mode == crate::OperatingMode::Auto
            && active.more_turns
            && st.todo_cadence.before_final(&self.todo_items())
        {
            self.append_conversation_message(
                st.messages,
                ProviderMessage::user(TODO_FINAL_REVIEW),
            )?;
            return Ok(TurnEnd::NextTurn);
        }
        Ok(TurnEnd::Stop(match provider_stop {
            ProviderTurnStop::Normal => AgentLoopStop::ProviderCompleted,
            ProviderTurnStop::Truncated => AgentLoopStop::ProviderTruncated,
            ProviderTurnStop::Filtered => AgentLoopStop::ProviderFiltered,
        }))
    }

    /// Blocks until a shell job completes (or the run is cancelled), then
    /// delivers what finished.
    async fn await_shell_jobs(&mut self, st: &mut LoopState<'_>) -> Result<(), ProviderError> {
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ProviderPhase {
                phase: ProviderPhase::PreparingTool,
                elapsed_ms: 0,
                detail: Some("Aguardando jobs de shell; conclusão automática".into()),
            },
        )?;
        while self.shell_jobs.running() && !self.is_cancelled() {
            let detail = self.redact_sensitive(&self.shell_jobs.progress_summary());
            push_runtime_transient_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::ProviderPhase {
                    phase: ProviderPhase::PreparingTool,
                    elapsed_ms: 0,
                    detail: Some(detail),
                },
            )?;
            self.shell_jobs.wait().await;
            if self.deliver_loop_shell_jobs(st)? {
                break;
            }
        }
        self.deliver_loop_shell_jobs(st)?;
        Ok(())
    }

    /// Spends one truncation recovery: raises the output limit when the
    /// provider allows it and asks the model for a smaller next step.
    fn recover_truncated_response<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        current_output_limit: Option<u64>,
    ) -> Result<(), ProviderError> {
        st.recovery.truncation_recoveries += 1;
        if let Some(limit) = current_output_limit.and_then(|current| {
            ctx.client
                .next_recovery_output_limit(current, st.config.context_window_tokens)
        }) {
            st.recovery.recovery_output_limit = Some(limit);
            st.config.context_reserve_tokens = st.config.context_reserve_tokens.max(limit);
        }
        self.append_conversation_message(
            st.messages,
            ProviderMessage::user(TRUNCATION_RECOVERY_PROMPT),
        )?;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ProviderPhase {
                phase: ProviderPhase::Connecting,
                elapsed_ms: 0,
                detail: Some(format!(
                    "Recovering truncated response ({truncation_recoveries}/{MAX_TRUNCATION_RECOVERIES}); output limit {}",
                    st.recovery.recovery_output_limit.or(current_output_limit)
                        .map_or_else(|| "provider default".into(), |limit| limit.to_string()),
                    truncation_recoveries = st.recovery.truncation_recoveries,
                )),
            },
        )?;
        Ok(())
    }

    /// A turn with tool calls: budget cut, batch execution and result
    /// recording.
    async fn run_tool_batch<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        active: &mut ActiveTurn,
        mut calls: Vec<ProviderToolCall>,
    ) -> Result<BatchReport, TurnExit> {
        let (budget_cut, suppressed) =
            split_calls_for_budget(&mut calls, st.config, st.reserved_tool_slots);
        // Reserve both native actions before executing a fused call. A failed
        // edit still consumes its reserved shell slot for this run.
        st.reserved_tool_slots = st
            .reserved_tool_slots
            .saturating_add(calls.iter().map(tool_call_slots).sum::<usize>());
        let mutating_slots = calls
            .iter()
            .filter(|call| !tool_call_is_read_only(&call.name))
            .map(tool_call_slots)
            .sum::<usize>();
        self.codemode.remaining_calls = st
            .config
            .max_total_tool_calls
            .saturating_sub(st.reserved_tool_slots)
            .min(
                st.config
                    .max_mutating_tool_calls
                    .saturating_sub(mutating_slots),
            );
        self.codemode.used_calls = 0;
        if budget_cut.suppressed > 0 {
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::ToolCallsSuppressed {
                    count: budget_cut.suppressed as u64,
                },
            )?;
        }
        let batch = self
            .execute_batch(ctx, st, active.event_start, calls)
            .await?;
        st.reserved_tool_slots = st
            .reserved_tool_slots
            .saturating_add(self.codemode.used_calls);
        let repeated_failure = self.record_batch(ctx, st, active, batch).await?;
        Ok(BatchReport {
            budget_cut,
            suppressed,
            repeated_failure,
        })
    }

    /// Journals, executes and materializes one tool batch. Cancellation
    /// returns the results that completed as the run's tail.
    async fn execute_batch<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        event_start: usize,
        mut calls: Vec<ProviderToolCall>,
    ) -> Result<ToolBatch, TurnExit> {
        let id = format!("slim-batch-{}-{}", st.turn, st.next_seq);
        assign_missing_call_ids(&mut calls, &id);
        // Tool execution emits no assistant text, so this snapshot also
        // serves the assistant message appended after the batch.
        let assistant_text = assistant_text_since(&self.app, event_start);
        if let Some(journal) = self.app.run_journal.as_ref().filter(|_| !calls.is_empty()) {
            let assistant = self.redact_message(ProviderMessage::assistant(
                assistant_text.clone(),
                calls.clone(),
            ));
            journal
                .lock()
                .map_err(|_| journal_error("durable run lock poisoned"))?
                .begin_tools(&id, assistant, &calls)
                .map_err(journal_error)?;
        }
        let (mut results, following_seq) = match self
            .execute_provider_tool_batch(
                ctx.mode,
                ctx.cwd,
                &id,
                &calls,
                st.next_seq,
                &mut st.governor,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => return Err(error.into()),
        };
        st.next_seq = following_seq;
        let reacquisitions = st.governor.take_post_compaction_reacquisitions();
        if self.is_cancelled() {
            return Err(self.cancel_loop(ctx, st, results));
        }
        st.next_seq = match self
            .materialize_results(&mut results, st.config.max_result_bytes, None, st.next_seq)
            .await
        {
            Ok(following_seq) => following_seq,
            Err(error) => return Err(error.into()),
        };
        Ok(ToolBatch {
            id,
            calls,
            results,
            reacquisitions,
            assistant_text,
        })
    }

    /// Appends the assistant turn, plans how each result is presented and
    /// folds the outcomes into the loop guard. Returns whether a repeated
    /// failed call must stop the run.
    async fn record_batch<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        active: &mut ActiveTurn,
        batch: ToolBatch,
    ) -> Result<bool, ProviderError> {
        let ToolBatch {
            id,
            calls,
            mut results,
            mut reacquisitions,
            assistant_text,
        } = batch;
        if !ctx.client.adapter().accepts_tool_result_images() {
            super::native_mcp::render::strip_unaccepted_media(&mut results);
        }
        let assistant = active
            .provider_turn
            .take_assistant_message(assistant_text, calls.clone());
        self.append_conversation_message(st.messages, assistant)?;
        self.anchor_usage(ctx, st, active.usage);
        let mut presentations = self.plan_tool_presentations(
            ctx,
            st,
            active.tools.as_ref(),
            &PresentationBatch {
                id: &id,
                calls: &calls,
                results: &results,
            },
        );
        let force_artifacts = results
            .iter()
            .zip(&presentations)
            .map(|(result, presentation)| result.artifact.is_none() && !presentation.complete)
            .collect::<Vec<_>>();
        if force_artifacts.iter().any(|forced| *forced) {
            st.next_seq = match self
                .materialize_results(
                    &mut results,
                    st.config.max_result_bytes,
                    Some(&force_artifacts),
                    st.next_seq,
                )
                .await
            {
                Ok(following_seq) => following_seq,
                Err(error) => return Err(error),
            };
            presentations = self.plan_tool_presentations(
                ctx,
                st,
                active.tools.as_ref(),
                &PresentationBatch {
                    id: &id,
                    calls: &calls,
                    results: &results,
                },
            );
        }
        let outcome = self
            .record_batch_results(
                BatchRecord {
                    calls: &calls,
                    results: &results,
                    presentations: &presentations,
                    reacquisitions: &mut reacquisitions,
                    event_start: active.event_start,
                },
                &mut st.guard,
                st.messages,
                &mut st.next_seq,
            )
            .await?;
        let repeated_failure = outcome.repeated_failure;
        if outcome.elided {
            // The usage anchored on this response counted outputs that are
            // now pointers: Pi distrusts usage captured before a context edit.
            st.usage_anchor = None;
            if let Some(handle) = &self.compaction_handle {
                handle.clear_usage_anchor();
            }
        }
        self.uncommitted_event_start = None;
        if self.app.events()[active.event_start..].iter().any(|event| {
            matches!(&event.kind, crate::EventKind::CausalProgressObserved { kind, .. }
                if proves_task_progress(*kind))
        }) {
            st.recovery.renew_after_progress();
        }
        st.all_results.extend(results);
        Ok(repeated_failure)
    }

    /// Decides whether the loop stops after a batch, before appending any
    /// steer (the final answer must not see guidance for turns that never
    /// run); otherwise appends the review and steer messages for the next turn.
    async fn after_batch<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        active: &ActiveTurn,
        report: &BatchReport,
    ) -> Result<Option<AgentLoopStop>, ProviderError> {
        let suppressed_calls = report.budget_cut.suppressed;
        if suppressed_calls > 0
            && should_stop_after_tool_budget_cut(report.budget_cut, st.turn, st.config.max_turns)
        {
            return Ok(Some(AgentLoopStop::ToolLimit));
        }
        if report.repeated_failure {
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::TerminalError {
                    message: "repeated failed tool call blocked".into(),
                },
            )?;
            return Ok(Some(AgentLoopStop::RepeatedFailedTool));
        }
        if st.governor.stop_requested() {
            self.app.discard_projected_payloads();
            return Ok(Some(AgentLoopStop::NoProgress));
        }
        if ctx.mode == crate::OperatingMode::Auto
            && active.more_turns
            && st.todo_cadence.after_batch(&self.todo_items())
        {
            self.append_conversation_message(
                st.messages,
                ProviderMessage::user(TODO_PROGRESS_REVIEW),
            )?;
        }
        if suppressed_calls > 0 {
            self.append_conversation_message(
                st.messages,
                ProviderMessage::user(suppressed_calls_steer(&report.suppressed)),
            )?;
        }
        let causal_steer = causal_reuse_steer(&self.app.events()[active.event_start..]);
        if let Some(steer) = causal_steer.filter(|_| {
            st.budget_steers_used < MAX_BUDGET_STEERS && !self.is_cancelled() && active.more_turns
        }) {
            st.budget_steers_used += 1;
            self.append_conversation_message(st.messages, ProviderMessage::user(steer))?;
        }
        if active.more_turns && !self.is_cancelled() {
            if let Some(note) = self.post_edit_diagnostics_note(ctx.cwd).await {
                self.append_conversation_message(st.messages, ProviderMessage::user(note))?;
            }
        }
        Ok(None)
    }

    /// Everything after the turn loop: settles shell jobs, asks for a final
    /// answer after a budget stop, and builds the result.
    async fn finalize_loop<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<AgentLoopResult, ProviderError> {
        // A terminal budget/filter/stop must not leave hidden native work alive.
        if !self.session_shell_jobs {
            self.shell_jobs.shutdown().await;
        }
        self.deliver_loop_shell_jobs(st)?;

        if matches!(
            st.stop,
            AgentLoopStop::TurnLimit
                | AgentLoopStop::ToolLimit
                | AgentLoopStop::NoProgress
                | AgentLoopStop::RepeatedFailedTool
        ) && !self.is_cancelled()
        {
            self.request_final_answer(ctx, st).await?;
        }

        if self.is_cancelled() {
            st.stop = AgentLoopStop::Cancelled;
        }
        if st.stop == AgentLoopStop::ProviderCompleted {
            let verified = st.governor.validations_satisfied()
                && runtime_goal_assurance(&self.app.events()[ctx.loop_event_start..]);
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::GoalAssurance { verified },
            )?;
        }
        Ok(AgentLoopResult {
            next_seq: st.next_seq,
            turns: st.turns,
            stop: st.stop,
            tool_results: std::mem::take(&mut st.all_results),
            usage: usage_since(&self.app, ctx.loop_event_start),
        })
    }

    fn cancelled_result<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        completed_batch_prefix: Vec<ToolResult>,
    ) -> Box<AgentLoopResult> {
        Box::new(cancelled_agent_loop_result(
            st.next_seq,
            st.turns,
            std::mem::take(&mut st.all_results),
            completed_batch_prefix,
            usage_since(&self.app, ctx.loop_event_start),
        ))
    }

    /// Builds the cancelled result.
    fn cancel_loop<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        completed_batch_prefix: Vec<ToolResult>,
    ) -> TurnExit {
        TurnExit::Return(self.cancelled_result(ctx, st, completed_batch_prefix))
    }

    fn deliver_loop_shell_jobs(&mut self, st: &mut LoopState<'_>) -> Result<bool, ProviderError> {
        self.deliver_shell_completions(st.messages, st.config.max_result_bytes, &mut st.next_seq)
    }

    /// After a budget or no-progress stop, asks once for a final answer
    /// without tool calls that fits the context window; failures are kept in
    /// `finalization_error` instead of failing the run. Where the wire allows
    /// it the request keeps the tool definitions and forbids calls, so it
    /// shares the turns' cached prefix; elsewhere it carries no tools.
    async fn request_final_answer<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<(), ProviderError> {
        let finalize_event_start = self.app.events().len();
        self.uncommitted_event_start = Some(finalize_event_start);
        let closing_prompt = if matches!(
            st.stop,
            AgentLoopStop::NoProgress | AgentLoopStop::RepeatedFailedTool
        ) {
            NO_PROGRESS_FINALIZE_PROMPT
        } else {
            BUDGET_FINALIZE_PROMPT
        };
        let closing_tools: Arc<[Value]> = if ctx.client.finalization_keeps_tools() {
            self.workspace_tool_definitions(ctx.mode, ctx.cwd)
        } else {
            Arc::from(Vec::new())
        };
        let mut final_messages = st.messages.clone();
        final_messages.push(ProviderMessage::user(closing_prompt));
        // The closing call obeys the same context budget as loop turns:
        // compact the carried history first (Pi's compaction, like any turn)
        // and skip a request that still cannot fit instead of spending a
        // doomed provider round-trip.
        if !self.finalization_fits_budget(ctx.client, &final_messages, &closing_tools, &st.config)
            && st.config.context_compaction_enabled
            && self.compaction_policy().enabled
        {
            let trigger = CompactionTrigger {
                reason: CompactionReason::Threshold,
                required: true,
                usage: self.context_usage(ctx.client.adapter(), st, &closing_tools),
            };
            match self
                .compact_for_turn(ctx, st, &closing_tools, trigger)
                .await
            {
                Ok(CompactionOutcome::Applied(_)) => {
                    st.compaction_applied = true;
                    final_messages = st.messages.clone();
                    final_messages.push(ProviderMessage::user(closing_prompt));
                }
                Ok(CompactionOutcome::Skipped) => {}
                Ok(CompactionOutcome::Cancelled) => return Ok(()),
                Err(error) => {
                    self.finalization_error = Some(self.redact_provider_error(error));
                    st.next_seq = self.observed_next_seq(st.next_seq);
                    return Ok(());
                }
            }
        }
        if !self.finalization_fits_budget(ctx.client, &final_messages, &closing_tools, &st.config) {
            self.finalization_error = Some(ProviderError::InvalidResponse {
                message: "final response request exceeds the context window".into(),
            });
            st.next_seq = self.observed_next_seq(st.next_seq);
        } else {
            // With the tools kept the request shares the turns' cached prefix,
            // so its messages are sent as theirs are: the channel overlay and
            // the write projection. A tool-free request shares none.
            let outcome = if closing_tools.is_empty() {
                self.run_provider_messages_with_tools(
                    ctx.client,
                    &final_messages,
                    &closing_tools,
                    st.next_seq,
                    true,
                )
                .await
            } else {
                let overlay = self.overlay_channel(&mut final_messages, ctx.mode, &st.channel);
                self.run_provider_messages_with_tools(
                    ctx.client,
                    overlay.view(),
                    &closing_tools,
                    st.next_seq,
                    true,
                )
                .await
            };
            match outcome {
                Ok(mut turn) => {
                    st.next_seq = turn.next_seq;
                    let text = assistant_text_since(&self.app, finalize_event_start);
                    if turn.stop != ProviderTurnStop::Normal || text.trim().is_empty() {
                        self.finalization_error = Some(ProviderError::InvalidResponse {
                            message: "final response was truncated, filtered, or empty".into(),
                        });
                    }
                    if !text.trim().is_empty() {
                        let text = if turn.stop == ProviderTurnStop::Normal {
                            text
                        } else {
                            format!("[Incomplete final response]\n{text}")
                        };
                        let assistant = turn.take_assistant_message(text, Vec::new());
                        self.append_conversation_message(st.messages, assistant)?;
                    }
                    self.uncommitted_event_start = None;
                }
                Err(error) => {
                    self.finalization_error = Some(self.redact_provider_error(error));
                    st.next_seq = self.observed_next_seq(st.next_seq);
                }
            }
        }
        Ok(())
    }

    pub(super) fn add_initial_workspace_context<A: ProviderAdapter>(
        &self,
        adapter: &A,
        messages: &mut [ProviderMessage],
        mode: crate::OperatingMode,
        cwd: &Path,
        config: AgentLoopConfig,
    ) {
        // Only a new conversation: never append stale listings on resume or
        // rewrite an existing tool/opaque reasoning continuation.
        if messages.len() != 1
            || messages[0].role != "user"
            || messages[0].content.contains(workspace::SNAPSHOT_MARKER)
            || self.is_cancelled()
        {
            return;
        }
        let tools = self.workspace_tool_definitions(mode, cwd);
        let settings = self.compaction_policy().settings();
        let fits = |messages: &[ProviderMessage]| {
            estimate_unprepared_request_chars(adapter, messages, tools.as_ref(), None).is_some_and(
                |chars| {
                    let tokens = self.token_estimator.estimate(
                        crate::provider::provider_kind_name(adapter.kind()),
                        adapter.model(),
                        chars,
                    );
                    !should_compact(tokens, config.context_window_tokens, &settings)
                        && tokens.saturating_add(config.context_reserve_tokens)
                            <= config.context_window_tokens
                },
            )
        };
        if !fits(messages) {
            return;
        }
        let Some(snapshot) = workspace::initial_paths(cwd, self.cancellation.as_ref()) else {
            return;
        };
        let original_len = messages[0].content.len();
        messages[0]
            .content
            .push_str(&self.redact_sensitive(&snapshot));
        if !fits(messages) || self.is_cancelled() {
            messages[0].content.truncate(original_len);
        }
    }

    /// The channel frame of a run over `messages`. Auto only: which MCP
    /// servers exist, without touching the system prompt or the tool
    /// definitions (server-provided text, redacted).
    pub(super) fn channel_frame(
        &self,
        mode: crate::OperatingMode,
        messages: &[ProviderMessage],
    ) -> mode::ChannelFrame {
        mode::ChannelFrame::new(messages, self.mcp_awareness(mode))
    }

    /// The MCP server block of the overlay as it is now (Auto only).
    pub(super) fn mcp_awareness(&self, mode: crate::OperatingMode) -> Option<String> {
        self.mcp
            .as_ref()
            .filter(|_| mode.allows_mutation())
            .and_then(|manager| manager.awareness_block())
            .map(|block| self.redact_sensitive(&block))
    }

    pub(super) fn overlay_channel<'a>(
        &self,
        messages: &'a mut [ProviderMessage],
        mode: crate::OperatingMode,
        frame: &mode::ChannelFrame,
    ) -> mode::ChannelOverlay<'a> {
        mode::ChannelOverlay::apply_with_mcp(
            messages,
            mode,
            self.can_ask(mode),
            Some(&mut lock_mutex(&self.write_projection_cache)),
            frame,
        )
    }

    pub(super) fn prepare_loop_request<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        messages: &mut [ProviderMessage],
        tools: &[Value],
        mode: crate::OperatingMode,
        frame: &mode::ChannelFrame,
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let overlay = self.overlay_channel(messages, mode, frame);
        client.prepare_messages_with_tools(overlay.view(), tools)
    }

    pub(super) fn prepare_loop_capabilities(&mut self, cwd: &Path) -> Result<(), ProviderError> {
        self.restore_task_facts(&self.task_facts(), cwd)?;
        // Bound skill-discovery memoization to one loop run.
        self.skill_discovery_cache = None;
        self.write_projection_cache
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.cached_skill_discovery(cwd);
        Ok(())
    }
}

/// The steer for a batch whose tool repeated evidence without a state change.
fn causal_reuse_steer(events: &[crate::SessionEvent]) -> Option<String> {
    events.iter().find_map(|event| {
        if let crate::EventKind::CausalAnomalyDetected {
            tool_name,
            call_id,
            confidence: crate::CausalConfidence::High,
            action: crate::CausalShadowAction::WouldReuse | crate::CausalShadowAction::WouldWarn,
            ..
        } = &event.kind
        {
            Some(format!("Tool {tool_name} call {call_id} repeated evidence without a relevant state change. Use the existing result or change the approach. Retry only after the dependency changes or if the needed content is no longer available; otherwise finish with the known outcome and blocker."))
        } else {
            None
        }
    })
}

/// Who reported a usage anchor: usage of one model says nothing about the
/// context another one would count.
fn usage_source(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// Drops opaque reasoning state that another provider produced.
fn retain_own_reasoning<A: ProviderAdapter>(adapter: &A, messages: &mut [ProviderMessage]) {
    for message in messages {
        message
            .responses_reasoning
            .retain(|state| state.belongs_to(adapter));
        if message
            .chat_reasoning
            .as_ref()
            .is_some_and(|state| !state.belongs_to(adapter))
        {
            message.chat_reasoning = None;
        }
    }
}
