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
/// A closing request ([`Runtime::request_final_answer`]): whether it forbids
/// tool calls on the wire, and the run whose routing it shares.
#[derive(Clone, Copy)]
pub(super) struct Closing<'r> {
    pub(super) forbid_tools: bool,
    pub(super) routing: &'r crate::provider::TurnRouting,
}

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
    /// When the run started, for `time_budget`.
    started: Instant,
    /// The time budget's finalize phase began and the model was told.
    finalize_steered: bool,
    /// Sticky routing and prefix diagnosis shared by this run's requests.
    pub(super) routing: crate::provider::TurnRouting,
    /// Whether any workspace mutation succeeded so far in this run.
    pub(super) had_mutation: bool,
    /// Whether the most recent check after a mutation passed cleanly with no failures.
    pub(super) clean_check_passed: bool,
    /// Whether this run has already observed a warm prompt cache hit (>=2k cached tokens).
    pub(super) had_warm_cache_hit: bool,
    /// Set to true when compaction rewrites the prefix in the current turn.
    pub(super) compacted_this_turn: bool,
    /// Cause classification from comparing consecutive request prefixes.
    pub(super) last_prefix_diagnosis: String,
    /// Background mid-run speculative compaction spawned during long tool batches.
    mid_run_speculative: Option<(CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl<'m> LoopState<'m> {
    pub(super) fn new(
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
            started: Instant::now(),
            finalize_steered: false,
            routing: crate::provider::TurnRouting::default(),
            had_mutation: false,
            clean_check_passed: false,
            had_warm_cache_hit: false,
            compacted_this_turn: false,
            last_prefix_diagnosis: String::new(),
            mid_run_speculative: None,
        }
    }

    pub(super) fn cancel_mid_run_speculative(&mut self) {
        if let Some((cancel, handle)) = self.mid_run_speculative.take() {
            cancel.cancel();
            handle.abort();
        }
    }

    fn time_budget_spent(&self) -> bool {
        self.config
            .time_budget
            .is_some_and(|budget| self.started.elapsed() >= budget)
    }

    /// The output reserve of a request whose input is estimated at
    /// `input_tokens`: the configured reserve, or with a floor configured
    /// the room left in the window down to that floor. The request's output
    /// limit follows it. A truncation recovery's raised limit keeps its
    /// reserve: the model needs the room.
    fn output_reserve(&self, input_tokens: u64) -> u64 {
        output_reserve_for(
            &self.config,
            self.recovery.recovery_output_limit.is_some(),
            input_tokens,
        )
    }
}

fn output_reserve_for(
    config: &AgentLoopConfig,
    has_recovery_limit: bool,
    input_tokens: u64,
) -> u64 {
    let configured = config.context_reserve_tokens;
    let floor = config.context_reserve_floor_tokens;
    if floor == 0 || has_recovery_limit {
        return configured;
    }
    let room = config.context_window_tokens.saturating_sub(input_tokens);
    configured.min(room.max(floor))
}

impl Drop for LoopState<'_> {
    fn drop(&mut self) {
        self.cancel_mid_run_speculative();
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
    /// `(answer, reason)` from `then_run.final` or `codemode` `finish(answer)`:
    /// the run may end with it, without another turn.
    concluded: Option<(String, &'static str)>,
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
    pub fn empty(next_seq: u64, stop: AgentLoopStop) -> Self {
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
        self.ensure_not_cancelled()?;
        let result = self
            .run_provider_messages_with_tools(
                client,
                &[ProviderMessage::user(prompt)],
                &[],
                next_seq,
                None,
            )
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
        closing: Option<Closing<'_>>,
    ) -> Result<ProviderTurnResult, ProviderError> {
        let redacted;
        let messages: &[ProviderMessage] = if self.sensitive_values.0.is_empty() {
            messages
        } else {
            redacted = self.redact_messages(messages);
            redacted.as_slice()
        };
        let mut request = match closing {
            Some(closing) => client
                .prepare_finalization_messages(messages, tools, closing.forbid_tools)?
                .with_turn_routing(closing.routing),
            None => client.prepare_messages_with_tools(messages, tools)?,
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
            .map_err(event_rejected)?;
        self.run_provider_messages_with_tools_after_snapshot(
            client,
            messages_are_text_only(messages),
            request,
            request_next_seq,
            false,
            None,
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
        early_dispatch: Option<(crate::OperatingMode, PathBuf, usize)>,
    ) -> Result<ProviderTurnResult, ProviderError> {
        self.clear_speculative_reads();
        self.pending_argument_repair = None;
        let mut sensitive_values = self.sensitive_values.0.clone();
        sensitive_values.extend_from_slice(request.sensitive_values());
        crate::provider::normalize_sensitive_values(&mut sensitive_values);
        let adapter = client.adapter();
        let mut normalizer =
            ProviderStreamNormalizer::new(adapter.wire_kind(), next_seq, sensitive_values)
                .with_reasoning_classification(adapter.reasoning_classification())
                .with_reasoning_provenance(adapter.reasoning_scope_fingerprint())
                .with_thinking_scope(
                    adapter.response_cache_scope_id(),
                    adapter.model(),
                    request.thinking_binding(),
                );
        let output_start = self.app.events().len();
        let request_started = Instant::now();
        let run_cancellation = self.cancellation.clone();
        let cancellation = CancellationToken::cancelled_or_pending(run_cancellation.clone());
        let provider_call_journal = self.app.run_journal.clone();
        let mut provider_call_id = None;
        let spec_cancel = CancellationToken::new();
        let mut speculator = early_dispatch
            .filter(|_| speculative_v2_enabled())
            .map(|(mode, cwd, max_read_calls)| {
                let canonical_workspace = std::fs::canonicalize(&cwd).ok();
                let mut stream_secrets = self.sensitive_values.0.clone();
                stream_secrets.extend_from_slice(request.sensitive_values());
                crate::provider::normalize_sensitive_values(&mut stream_secrets);
                StreamSpeculator::new(StreamSpeculatorConfig {
                    mode,
                    cwd,
                    canonical_workspace,
                    max_read_calls,
                    tools: self.tools.clone(),
                    code_intel: self.code_intel.clone(),
                    spec_cancel: spec_cancel.clone(),
                    run_cancel: run_cancellation.clone(),
                    sensitive_values: stream_secrets,
                })
            });
        let stream_result = client
            .stream_prepared_cancellable_with_speculation(
                request,
                cancellation,
                |event| {
                    normalizer.push(&mut self.app, event);
                },
                |telemetry| {
                    provider_call_id = persist_provider_call(&provider_call_journal, telemetry)?;
                    Ok(())
                },
                |raw_event| {
                    if let Some(spec) = speculator.as_mut() {
                        spec.on_event(raw_event);
                    }
                },
            )
            .await;
        if let Some(spec) = speculator {
            let (local_speculative, _prewarms) = spec.finish(stream_result.is_ok() && !self.is_cancelled());
            if !local_speculative.is_empty() {
                for (id, entry) in local_speculative {
                    self.speculative_reads.insert(id, entry);
                }
                self.speculative_cancel = Some(spec_cancel);
            }
        }
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
            .run_provider_messages_with_tools(client, messages, tools.as_ref(), next_seq, None)
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
        self.codemode.remaining_calls = usize::MAX;
        self.codemode.remaining_mutating_calls = Some(
            AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS.saturating_sub(
                calls
                    .iter()
                    .filter(|call| !tool_call_is_read_only(&call.name))
                    .map(tool_call_slots)
                    .sum::<usize>(),
            ),
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
    /// Renews the remote provider's KV-cache TTL using the exact active `tools`,
    /// `system`, and `ChannelFrame` prefix with a minimal-output request. Does
    /// not mutate journals or emit UI events. Returns `(cache_read, cache_write)`.
    pub async fn run_keep_alive_ping<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        initial_messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        config: AgentLoopConfig,
    ) -> Result<(u64, u64), ProviderError> {
        let cwd = cwd.as_ref();
        self.prepare_loop_capabilities(cwd)?;
        let tools = self.workspace_tool_definitions_for(mode, cwd, config.stable_tool_envelope);
        let mut seeded = self.redact_messages(initial_messages);
        let write_penalty = client
            .adapter()
            .cache_write_penalty_multiplier()
            .max(1);
        let _ = elide_superseded_tool_outputs_with_penalty(&mut seeded, write_penalty);
        if seeded.last().is_none_or(|m| m.role != "user") {
            seeded.push(ProviderMessage::user("."));
        }
        let preserve = config.stable_tool_envelope || self.frozen_tools_override.is_some();
        let frame = self.readonly_channel_frame_for(mode, &seeded, preserve);
        let request = {
            let overlay = self.overlay_channel(&mut seeded, mode, &frame);
            client.prepare_keep_alive_ping(overlay.view(), &tools)?
        };
        let mut read = 0u64;
        let mut write = 0u64;
        client
            .stream_prepared_cancellable(request, std::future::pending(), |event| {
                if let ProviderEvent::UsageBreakdown { usage } = event {
                    read = read.max(usage.cache_read_tokens);
                    write = write.max(usage.cache_write_tokens);
                }
            })
            .await?;
        let cached = read.saturating_add(write);
        if cached > 0 {
            if let Some(handle) = &self.compaction_handle {
                let provider = crate::provider::provider_kind_name(client.adapter().kind());
                let model = client.adapter().model();
                let ttl_secs = crate::context::CompactionHandle::warm_cache_ttl_secs(provider);
                handle.record_warm_cache(
                    provider,
                    model,
                    super::compaction_foreground::current_unix_secs(),
                    ttl_secs,
                    cached,
                );
            }
        }
        Ok((read, write))
    }

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
            lock_journal(journal)?
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
        // Keep the live prefix untouched so cache-friendly in-place compaction
        // reads the provider's warm cache instead of re-ingesting it.
        let anchor = self.seed_usage_anchor(ctx, messages, 0);
        let channel = self.channel_frame_for(
            ctx.mode,
            messages,
            config.stable_tool_envelope || self.frozen_tools_override.is_some(),
        );
        let mut st = LoopState::new(messages, next_seq, config, anchor, channel);
        let tools = self.workspace_tool_definitions_for(
            ctx.mode,
            ctx.cwd,
            st.config.stable_tool_envelope,
        );
        let usage = self.context_usage(ctx.client.adapter(), &st, &tools);
        let trigger = CompactionTrigger {
            reason: CompactionReason::Manual,
            required: true,
            usage,
            build_next_request: false,
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

    /// Runs a read-only background speculative compaction over `initial_messages`
    /// without mutating live history, emitting UI events, or touching journals.
    pub async fn run_speculative_compaction<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        initial_messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        config: AgentLoopConfig,
    ) -> Result<Option<crate::context::SpeculativeCompaction>, ProviderError> {
        let cwd = cwd.as_ref();
        if self.is_cancelled() || !config.context_compaction_enabled {
            return Ok(None);
        }
        self.prepare_loop_capabilities(cwd)?;
        let mut messages = self.redact_messages(initial_messages);
        if self.is_already_compacted(&messages) {
            return Ok(None);
        }
        let ctx = LoopCtx::new(client, initial_messages, mode, cwd, 0);
        let anchor = self.seed_usage_anchor(&ctx, &messages, 0);
        let channel = self.readonly_channel_frame(mode, &messages);
        self.run_speculative_compaction_inner(&ctx, &mut messages, config, anchor, channel)
            .await
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
            if st.time_budget_spent() {
                st.stop = AgentLoopStop::TimeLimit;
                break;
            }
            if let Some(token_limit) = st.config.max_total_tokens {
                let spent = usage_since(&self.app, ctx.loop_event_start);
                let total = spent
                    .uncached_input_tokens
                    .saturating_add(spent.cache_write_tokens)
                    .saturating_add(spent.cache_read_tokens)
                    .saturating_add(spent.output_tokens);
                if total >= token_limit {
                    st.stop = AgentLoopStop::TokenLimit;
                    break;
                }
            }
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
    fn seed_loop<'m, A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        mut next_seq: u64,
        config: AgentLoopConfig,
        messages: &'m mut Vec<ProviderMessage>,
    ) -> Result<LoopState<'m>, ProviderError> {
        self.prepare_loop_capabilities(ctx.cwd)?;
        let caps = ctx.client.adapter().capabilities();
        let now = super::compaction_foreground::current_unix_secs();
        // Asked first: `has_warm_cache` drops the expired entry.
        let cache_expired = self
            .compaction_handle
            .as_ref()
            .is_some_and(|handle| handle.warm_cache_expired(ctx.provider, ctx.model, now));
        let warm_cache = self
            .compaction_handle
            .as_ref()
            .is_some_and(|handle| handle.has_warm_cache(ctx.provider, ctx.model, now));
        self.prompt_cache_active = caps.supports_explicit_cache_breakpoints
            || caps.supports_prompt_cache_key
            || caps.supports_prompt_cache_options
            || warm_cache;
        self.cache_write_penalty = ctx
            .client
            .adapter()
            .cache_write_penalty_multiplier()
            .max(if warm_cache { 2 } else { 1 });
        let mut seeded = self.redact_messages(ctx.initial_messages);
        self.add_initial_workspace_context(
            ctx.client.adapter(),
            &mut seeded,
            ctx.mode,
            ctx.cwd,
            config,
        );
        // Opaque reasoning stays attached to live history and is filtered on
        // the wire via `belongs_to(adapter)` so A → B → A round-trips keep A's
        // signed thinking blocks and prompt cache intact.
        // The provider may still hold the previous run's prefix in its cache:
        // weigh saved bytes against cache-write re-ingestion penalty. A prefix
        // known to have expired is rewritten whole either way: no penalty.
        let write_penalty = if cache_expired {
            1
        } else {
            self.cache_write_penalty.max(1)
        };
        let seed_elision =
            elide_superseded_tool_outputs_with_penalty(&mut seeded, write_penalty);
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
        self.tuning_closed = false;
        self.run_guards = run_guards::RunGuards::default();
        let preserve_channel_modes = config.stable_tool_envelope || self.frozen_tools_override.is_some();
        if self.subagent_depth == 0 {
            if let Some(reg) = self.subagents.as_ref() {
                let parent_tools = self.workspace_tool_definitions_for(
                    ctx.mode,
                    ctx.cwd,
                    config.stable_tool_envelope,
                );
                reg.set_spawner(
                    Arc::new(super::native_subagent::ConcreteSubagentSpawner {
                        client: ctx.client.clone(),
                        mode: ctx.mode,
                        cwd: ctx.cwd.to_path_buf(),
                        base_config: config.clone(),
                        artifact_store: self.artifact_store.clone(),
                        sensitive_values: self.sensitive_values.clone(),
                        code_intel: self.code_intel.clone(),
                        mcp: self.mcp.clone(),
                        compaction_policy: self.compaction_policy(),
                        initial_workspace_snapshot: self
                            .compaction_handle
                            .as_ref()
                            .and_then(|handle| handle.initial_workspace_snapshot()),
                        skill_discovery: self.skill_discovery_cache.clone(),
                        parent_tools: Some(parent_tools),
                    }),
                    ctx.mode,
                );
            }
        }
        let anchor = self.seed_usage_anchor(ctx, messages, seed_elision.elided);
        let channel = self.channel_frame_for(ctx.mode, messages, preserve_channel_modes);
        let mut st = LoopState::new(messages, next_seq, config, anchor, channel);
        st.had_warm_cache_hit = warm_cache;
        Ok(st)
    }

    /// The usage anchor the previous run left in the compaction handle, when
    /// this run's history still has that response where it was. An elision
    /// (`elided` outputs) changed what the last response's usage described,
    /// so the anchor is dropped, in the handle too.
    fn seed_usage_anchor<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        messages: &[ProviderMessage],
        elided: u32,
    ) -> Option<UsageAnchor> {
        let handle = self.compaction_handle.as_ref();
        if elided > 0 {
            if let Some(handle) = handle {
                handle.clear_usage_anchor();
            }
            return None;
        }
        handle?.usage_anchor(Some(&usage_source(ctx.provider, ctx.model)), messages)
    }

    /// Cancellation, shell-job delivery and turn bookkeeping.
    async fn begin_turn<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<(), TurnExit> {
        if let Some(gate) = self.pause_gate.clone() {
            let cancel = self.cancellation.clone().unwrap_or_default();
            if !gate.wait_if_paused(&cancel).await {
                return Err(self.cancel_loop(ctx, st, Vec::new()));
            }
        }
        if self.is_cancelled() {
            return Err(self.cancel_loop(ctx, st, Vec::new()));
        }
        self.deliver_loop_shell_jobs(st)?;
        self.deliver_steers(st)?;
        self.deliver_finished_subagents(st, ctx.mode)?;
        st.turns = st.turn + 1;
        Ok(())
    }

    /// Tells the model, once per subagent, that one it left running has ended
    /// and still holds a result nobody collected.
    fn deliver_finished_subagents(
        &mut self,
        st: &mut LoopState<'_>,
        mode: crate::OperatingMode,
    ) -> Result<(), ProviderError> {
        let ended = self
            .subagents
            .as_ref()
            .map(|registry| registry.take_unannounced_terminal())
            .unwrap_or_default();
        if ended.is_empty() {
            return Ok(());
        }
        let collect_hint = if mode == crate::OperatingMode::Code {
            r#"slim.subagent_control({ action: "wait", id })"#
        } else {
            r#"subagent_control (action: "wait", id)"#
        };
        self.append_conversation_message(
            st.messages,
            ProviderMessage::user(format!(
                "[Subagents ended with uncollected results: {}. Collect each with {collect_hint}.]",
                ended.join(", ")
            )),
        )
    }

    /// Builds this turn's request: preflight, compaction when Pi's trigger
    /// (or a manual or overflow request) asks for it, output-limit recovery
    /// and the final context gate.
    async fn prepare_request<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<PreparedTurn, TurnExit> {
        if ctx.mode == crate::OperatingMode::Code {
            codemode::prewarm_worker_cell();
        }
        let tools = self.workspace_tool_definitions_for(
            ctx.mode,
            ctx.cwd,
            st.config.stable_tool_envelope,
        );
        if !self.prompt_cache_active {
            let dropped = drop_earlier_reasoning(st.messages);
            if dropped.elided > 0 {
                st.routing.reset_delta_chain();
                st.usage_anchor = None;
                if let Some(handle) = &self.compaction_handle {
                    handle.clear_usage_anchor();
                }
            }
        }
        let (preflight_chars, mut request, trigger) =
            self.preflight_request_and_trigger(ctx, st, &tools)?;
        if let Some(trigger) = trigger {
            if self.try_pre_threshold_micro_prune(st, trigger)? {
                st.routing.reset_delta_chain();
                request = None;
            } else {
                match self.compact_for_turn(ctx, st, &tools, trigger).await {
                    Ok(CompactionOutcome::Applied(applied)) => {
                        st.routing.reset_delta_chain();
                        request = applied.map(|boxed| *boxed);
                        st.compaction_applied = true;
                        st.compacted_this_turn = true;
                        st.guard = LoopGuard::default();
                        // What the guards remember was read is now a summary.
                        self.run_guards = run_guards::RunGuards::default();
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
        }
        let mut request = match request {
            Some(request) => request,
            None => {
                self.prepare_loop_request(ctx.client, st.messages, &tools, ctx.mode, &st.channel)?
            }
        };
        // A history that still does not fit is cut down step by step (see
        // `shrink_history_to_fit`) before the run gives up on it.
        let mut shrink_step = 0;
        let current_output_limit = loop {
            if let Some(limit) = st.recovery.recovery_output_limit {
                request = ctx.client.with_recovery_output_limit(request, limit)?;
            }
            let mut current_output_limit = request.output_token_limit();
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
            // A window nearly full leaves the answer the room that is left
            // instead of compacting for a limit the answer rarely reaches;
            // the output limit on the wire follows the shrunk reserve.
            let reserve = st.output_reserve(estimated_tokens);
            if reserve < st.config.context_reserve_tokens
                && current_output_limit.is_some_and(|limit| limit > reserve)
            {
                request = ctx.client.with_output_limit_ceiling(request, reserve)?;
                current_output_limit = request.output_token_limit();
            }
            request.estimated_tokens = estimated_tokens;
            let budget =
                ContextBudget::new(st.config.context_window_tokens, estimated_tokens, reserve);
            if budget.can_fit(reserve) {
                break current_output_limit;
            }
            let Some(stats) = shrink_history_to_fit(st.messages, &mut shrink_step) else {
                return Err(ProviderError::InvalidResponse {
                    message: if st.compaction_applied {
                        "context window still exceeded after compaction".into()
                    } else {
                        "context window exceeded and compaction is unavailable".into()
                    },
                }
                .into());
            };
            // What the last response's usage described has changed.
            st.routing.reset_delta_chain();
            st.usage_anchor = None;
            if let Some(handle) = &self.compaction_handle {
                handle.clear_usage_anchor();
            }
            let detail = if stats.emitted_bytes == 0 {
                format!("dropped the reasoning of {} earlier answers", stats.elided)
            } else {
                format!(
                    "cut {} tool results from {} to {} bytes",
                    stats.elided, stats.original_bytes, stats.emitted_bytes
                )
            };
            st.next_seq = self.observed_next_seq(st.next_seq);
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::context_trimmed(&detail),
            )?;
            request =
                self.prepare_loop_request(ctx.client, st.messages, &tools, ctx.mode, &st.channel)?;
        };
        if st.config.adaptive_reasoning && st.clean_check_passed {
            request = ctx
                .client
                .adapter()
                .with_adaptive_reasoning_effort(request, "low");
        }
        Ok(PreparedTurn {
            request,
            tools,
            current_output_limit,
        })
    }

    /// Estimates the turn's request and evaluates compaction triggers in a
    /// single `ChannelOverlay` pass: normal turns serialize their request right
    /// inside the same overlay instead of applying and dropping it twice.
    fn preflight_request_and_trigger<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        tools: &[Value],
    ) -> Result<(u64, Option<PreparedProviderRequest>, Option<CompactionTrigger>), ProviderError> {
        let overflow = std::mem::take(&mut st.recovery.overflow_compaction_pending);
        let config = st.config.clone();
        let awaiting_response = st.awaiting_response;
        let has_recovery_limit = st.recovery.recovery_output_limit.is_some();
        let enabled = config.context_compaction_enabled;
        let policy = self.compaction_policy();
        let precheck = if enabled {
            let manual = self
                .compaction_handle
                .as_ref()
                .and_then(CompactionHandle::manual_instructions)
                .is_some();
            let usage = self.context_usage(ctx.client.adapter(), st, tools);
            let policy_triggered = policy.enabled
                && !awaiting_response
                && should_compact(
                    estimate_context_tokens(st.messages, usage).tokens,
                    config.context_window_tokens,
                    &policy.effective_settings_for_window(config.context_window_tokens),
                );
            Some((manual, usage, policy_triggered))
        } else {
            None
        };

        let overlay = self.overlay_channel(st.messages, ctx.mode, &st.channel);
        let structural_chars =
            estimate_unprepared_request_chars(ctx.client.adapter(), overlay.view(), tools, None);
        let (chars, tokens, mut request) = match structural_chars {
            Some(chars) => (
                chars,
                self.token_estimator
                    .estimate(ctx.provider, ctx.model, chars),
                None,
            ),
            None => {
                let mut request =
                    self.prepare_overlay_request(ctx.client, overlay.view(), tools)?;
                let chars = request
                    .serialized_chars
                    .saturating_sub(image_payload_discount_chars(overlay.view()));
                let tokens = self
                    .token_estimator
                    .estimate(ctx.provider, ctx.model, chars);
                request.estimated_tokens = tokens;
                (chars, tokens, Some(request))
            }
        };

        let trigger = precheck.and_then(|(manual, usage, policy_triggered)| {
            let over_gate = tokens.saturating_add(output_reserve_for(
                &config,
                has_recovery_limit,
                tokens,
            )) > config.context_window_tokens;
            let (reason, required) = if overflow && policy.enabled {
                (CompactionReason::Overflow, true)
            } else if manual {
                (CompactionReason::Manual, true)
            } else if policy.enabled && !awaiting_response && (over_gate || policy_triggered) {
                (CompactionReason::Threshold, over_gate)
            } else {
                return None;
            };
            Some(CompactionTrigger {
                reason,
                required,
                usage,
                build_next_request: true,
            })
        });

        if trigger.is_none() && request.is_none() {
            request = Some(self.prepare_overlay_request(ctx.client, overlay.view(), tools)?);
        }
        Ok((chars, request, trigger))
    }

    /// Deterministically collapses large (>4 KiB) read/search tool results strictly
    /// before the compaction cut point when a non-required soft threshold triggers
    /// and no speculative summary is already ready, reclaiming >=15% of the window
    /// in 0 ms without an LLM call.
    fn try_pre_threshold_micro_prune(
        &mut self,
        st: &mut LoopState<'_>,
        trigger: CompactionTrigger,
    ) -> Result<bool, TurnExit> {
        if trigger.reason != CompactionReason::Threshold || trigger.required {
            return Ok(false);
        }
        let policy = self.compaction_policy();
        let settings = policy.effective_settings_for_window(st.config.context_window_tokens);
        let Some(preparation) = prepare_compaction(st.messages, &settings, trigger.usage) else {
            return Ok(false);
        };
        // Pruning rewrites the prefix a ready checkpoint summarizes.
        if self
            .compaction_handle
            .as_ref()
            .is_some_and(|handle| handle.has_valid_speculative(st.messages))
        {
            return Ok(false);
        }
        let window = st.config.context_window_tokens;
        let savings_chars =
            estimate_micro_prune_savings_chars(st.messages, preparation.first_kept_index);
        let saved_tokens = savings_chars / 4;
        let min_savings = (window.saturating_mul(15) / 100).min(16_000).max(1);
        if window == 0 || saved_tokens < min_savings {
            return Ok(false);
        }
        let current_tokens = estimate_context_tokens(st.messages, trigger.usage).tokens;
        if current_tokens.saturating_sub(saved_tokens) > window.saturating_sub(settings.reserve_tokens) {
            return Ok(false);
        }
        let stats = micro_prune_before_cut(st.messages, preparation.first_kept_index);
        if stats.elided == 0 {
            return Ok(false);
        }
        st.usage_anchor = None;
        if let Some(handle) = &self.compaction_handle {
            handle.clear_usage_anchor();
        }
        let detail = format!(
            "pruned {} old read outputs before compaction ({} to {} bytes)",
            stats.elided, stats.original_bytes, stats.emitted_bytes
        );
        st.next_seq = self.observed_next_seq(st.next_seq);
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::context_trimmed(&detail),
        )?;
        Ok(true)
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
        let item_hashes = request.input_item_hashes();
        st.last_prefix_diagnosis = st
            .routing
            .diagnose_prefix_change(request.stable_prefixes, item_hashes);
        let provider_result = self
            .run_provider_messages_with_tools_after_snapshot(
                ctx.client,
                messages_are_text_only(st.messages),
                request.with_turn_routing(&st.routing),
                st.next_seq,
                true,
                Some((
                    ctx.mode,
                    ctx.cwd.to_path_buf(),
                    st.config.max_read_tool_calls,
                )),
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
        if let Some(u) = usage {
            let total_input = u.total_input_tokens();
            if st.had_warm_cache_hit && !st.compacted_this_turn && total_input >= 8_000 {
                let hit_ratio_pct =
                    ((u.cache_read_tokens.saturating_mul(100)) / total_input.max(1)).min(100) as u8;
                if hit_ratio_pct < 40 {
                    push_runtime_event(
                        &mut self.app,
                        &mut st.next_seq,
                        crate::EventKind::CacheDriftDetected {
                            provider: ctx.provider.to_owned(),
                            model: ctx.model.to_owned(),
                            hit_ratio_pct,
                            input_tokens: total_input,
                            cache_read_tokens: u.cache_read_tokens,
                            cause: st.last_prefix_diagnosis.clone(),
                        },
                    )?;
                }
            }
            if u.cache_read_tokens >= 2_000 {
                st.had_warm_cache_hit = true;
            }
        }
        st.compacted_this_turn = false;
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
        self.app.push_event(snapshot).map_err(event_rejected)?;
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
        let provider_stop = active.provider_turn.stop;
        let assistant_text = assistant_text_since(&self.app, active.event_start);
        // When a model's safety filter refuses before emitting any text (0
        // output tokens), do not append an empty phantom assistant turn or
        // anchor usage on the blocked response.
        if !(provider_stop == ProviderTurnStop::Filtered && assistant_text.trim().is_empty()) {
            let assistant = active
                .provider_turn
                .take_assistant_message(assistant_text, Vec::new());
            self.append_conversation_message(st.messages, assistant)?;
            self.anchor_usage(ctx, st, active.usage);
        }
        self.uncommitted_event_start = None;
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
        if provider_stop != ProviderTurnStop::Normal {
            st.routing.reset_delta_chain();
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
            && ctx.mode.allows_mutation()
            && active.more_turns
            && st.todo_cadence.before_final(&self.todo_items())
        {
            self.append_conversation_message(
                st.messages,
                ProviderMessage::user(TODO_FINAL_REVIEW),
            )?;
            return Ok(TurnEnd::NextTurn);
        }
        // A message sent while the model was producing this "final" answer keeps
        // the run alive: it is delivered (and answered) on the next turn.
        if self.steer_pending() {
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
        // The wait wakes every 100 ms; only a changed summary is news.
        let mut last_detail = String::new();
        while self.shell_jobs.running() && !self.is_cancelled() {
            let detail = self.redact_sensitive(&self.shell_jobs.progress_summary());
            if detail != last_detail {
                push_runtime_transient_event(
                    &mut self.app,
                    &mut st.next_seq,
                    crate::EventKind::ProviderPhase {
                        phase: ProviderPhase::PreparingTool,
                        elapsed_ms: 0,
                        detail: Some(detail.clone()),
                    },
                )?;
                last_detail = detail;
            }
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
            .saturating_sub(st.reserved_tool_slots);
        self.codemode.remaining_mutating_calls = Some(
            st.config
                .max_mutating_tool_calls
                .saturating_sub(mutating_slots),
        );
        self.codemode.used_calls = 0;
        self.codemode.pending_final_answer = None;
        self.codemode.cell_effects.clear();
        if budget_cut.suppressed > 0 {
            st.routing.reset_delta_chain();
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::ToolCallsSuppressed {
                    count: budget_cut.suppressed as u64,
                },
            )?;
        }
        if calls
            .iter()
            .any(|call| matches!(call.name.as_str(), "shell" | "subagent" | "codemode" | "batch_code"))
        {
            self.maybe_spawn_mid_run_speculative(ctx, st);
        }
        if self.subagent_depth == 0
            && self.subagents.is_some()
            && super::native_subagent::needs_fork_context(&calls)
        {
            let forked = super::native_subagent::sanitize_fork_context(st.messages);
            let mut anchors = self
                .compaction_handle
                .as_ref()
                .map(|handle| handle.export_channel_anchors(&forked))
                .unwrap_or_default();
            if let Some(current_idx) = st.channel.current_anchor(&forked) {
                if !anchors.iter().any(|(idx, _, _)| *idx == current_idx) {
                    anchors.push((
                        current_idx,
                        ctx.mode,
                        st.channel.applied_text(ctx.mode, self.can_ask(ctx.mode)),
                    ));
                }
            }
            self.active_fork_channel_anchors = anchors;
            self.active_fork_context = Some(Arc::from(forked));
        }
        let batch_result = self.execute_batch(ctx, st, active.event_start, calls).await;
        self.active_fork_context = None;
        self.active_fork_channel_anchors.clear();
        let batch = batch_result?;
        st.reserved_tool_slots = st
            .reserved_tool_slots
            .saturating_add(self.codemode.used_calls);
        let codemode_final = self.codemode.pending_final_answer.take();
        let concluded = then_run_final_answer(&batch.calls, &batch.results)
            .map(|answer| (answer, "then_run_passed"))
            .or_else(|| {
                (ctx.mode == crate::OperatingMode::Code
                    && !batch.calls.is_empty()
                    && batch.calls.len() == batch.results.len()
                    && batch.results.iter().all(|result| result.success)
                    && batch.calls.iter().any(|call| call.name == "codemode"))
                .then_some(codemode_final)
                .flatten()
                .map(|answer| (answer, "codemode_finish"))
            });
        let repeated_failure = self.record_batch(ctx, st, active, batch).await?;
        Ok(BatchReport {
            budget_cut,
            suppressed,
            repeated_failure,
            concluded,
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
        if calls.iter().any(|call| call.id.trim().is_empty()) {
            st.routing.reset_delta_chain();
        }
        assign_missing_call_ids(&mut calls, &id);
        // Tool execution emits no assistant text, so this snapshot also
        // serves the assistant message appended after the batch.
        let assistant_text = assistant_text_since(&self.app, event_start);
        if let Some(journal) = self.app.run_journal.as_ref().filter(|_| !calls.is_empty()) {
            let assistant = self.redact_message(ProviderMessage::assistant(
                assistant_text.clone(),
                calls.clone(),
            ));
            lock_journal(journal)?
                .begin_tools(&id, assistant, &calls)
                .map_err(journal_error)?;
        }
        let (mut results, following_seq) = self
            .execute_provider_tool_batch(
                ctx.mode,
                ctx.cwd,
                &id,
                &calls,
                st.next_seq,
                &mut st.governor,
            )
            .await?;
        st.next_seq = following_seq;
        let reacquisitions = st.governor.take_post_compaction_reacquisitions();
        if self.is_cancelled() {
            return Err(self.cancel_loop(ctx, st, results));
        }
        st.next_seq = self
            .materialize_results(&mut results, st.config.max_result_bytes, None, st.next_seq)
            .await?;
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
            st.next_seq = self
                .materialize_results(
                    &mut results,
                    st.config.max_result_bytes,
                    Some(&force_artifacts),
                    st.next_seq,
                )
                .await?;
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
        let allow_elision = !(ctx.client.adapter().capabilities().supports_previous_response_id
            && st.routing.has_active_delta_chain());
        let outcome = self
            .record_batch_results(
                BatchRecord {
                    calls: &calls,
                    results: &results,
                    presentations: &presentations,
                    reacquisitions: &mut reacquisitions,
                    event_start: active.event_start,
                    allow_elision,
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
        let batch_all_ok = results.iter().all(|result| result.success);
        let mutated_this_batch = self.app.events()[active.event_start..].iter().any(|event| {
            matches!(
                &event.kind,
                crate::EventKind::CausalProgressObserved {
                    kind: crate::CausalProgressKind::WorkspaceChanged,
                    ..
                }
            )
        });
        let verified_this_batch = calls.iter().any(|call| {
            is_verification_check_call(call)
                || (matches!(call.name.as_str(), "codemode" | "batch_code")
                    && self.codemode.effects_for(&call.id).had_validation_green)
        });
        if !batch_all_ok {
            st.clean_check_passed = false;
        } else if mutated_this_batch {
            st.had_mutation = true;
            st.clean_check_passed = verified_this_batch;
        } else if st.had_mutation && verified_this_batch {
            st.clean_check_passed = true;
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
        let messages_before = st.messages.len();
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
        // A passing `then_run.final` or `codemode` `finish()` concludes the
        // run: advisory steers must not hold it.
        if report.concluded.is_none() && active.more_turns && !self.is_cancelled() {
            if let Some(steer) = self.enter_finalize_phase(st) {
                self.append_conversation_message(st.messages, ProviderMessage::user(steer))?;
            }
        }
        if report.concluded.is_none()
            && ctx.mode.allows_mutation()
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
        let causal_steer = report
            .concluded
            .is_none()
            .then(|| causal_reuse_steer(&self.app.events()[active.event_start..]))
            .flatten();
        if let Some(steer) = causal_steer.filter(|_| {
            st.budget_steers_used < MAX_BUDGET_STEERS && !self.is_cancelled() && active.more_turns
        }) {
            st.budget_steers_used += 1;
            self.append_conversation_message(st.messages, ProviderMessage::user(steer))?;
        }
        if active.more_turns && !self.is_cancelled() {
            let concluded_green = report.concluded.is_some() && st.clean_check_passed;
            if let Some(note) = self
                .post_edit_diagnostics_note_for(ctx.cwd, concluded_green)
                .await
            {
                self.append_conversation_message(st.messages, ProviderMessage::user(note))?;
            }
        }
        // Conclusion on success: the model already wrote its closing answer
        // into `then_run.final` or `finish(answer)`, so a passing check ends
        // the run here instead of paying a turn that would only restate it.
        // Anything the harness still has to deliver (diagnostics, user steer,
        // suppressed calls, running jobs) keeps the loop going.
        if let Some((answer, reason)) = report.concluded.as_ref() {
            if suppressed_calls == 0
                && st.messages.len() == messages_before
                && !self.shell_jobs.running()
                && !self.steer_pending()
                && !self.is_cancelled()
            {
                self.conclude_with_answer(st, answer, reason)?;
                return Ok(Some(AgentLoopStop::ProviderCompleted));
            }
        }
        Ok(None)
    }

    /// From three quarters of the time budget on, tuning-only patches are
    /// refused; the model is told once, with the numbers, to finish.
    fn enter_finalize_phase(&mut self, st: &mut LoopState<'_>) -> Option<String> {
        let budget = st.config.time_budget?;
        let elapsed = st.started.elapsed();
        if st.finalize_steered || elapsed < budget.mul_f64(0.75) || elapsed >= budget {
            return None;
        }
        st.finalize_steered = true;
        self.tuning_closed = true;
        Some(format!(
            "[Time budget] {} of {} used. Finalize now: start nothing new; tuning-only patches (numbers or colours) are refused from here on. Fix only what fails the task's own check, run the final validation and answer. The run stops when the budget is spent.",
            budget_clock(elapsed),
            budget_clock(budget),
        ))
    }

    /// Ends the run with `answer` as the assistant's final text, recorded as
    /// a provider turn would record it.
    fn conclude_with_answer(
        &mut self,
        st: &mut LoopState<'_>,
        answer: &str,
        reason: &str,
    ) -> Result<(), ProviderError> {
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::AssistantTextDelta {
                text: answer.to_owned(),
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::AssistantEnded {
                reason: reason.to_owned(),
            },
        )?;
        self.append_conversation_message(
            st.messages,
            ProviderMessage::assistant(answer.to_owned(), Vec::new()),
        )?;
        Ok(())
    }

    /// Everything after the turn loop: settles shell jobs, asks for a final
    /// answer after a budget stop, and builds the result.
    async fn finalize_loop<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<AgentLoopResult, ProviderError> {
        st.cancel_mid_run_speculative();
        // A terminal budget/filter/stop must not leave hidden native work alive.
        if !self.session_shell_jobs {
            self.shell_jobs.shutdown().await;
        }
        self.deliver_loop_shell_jobs(st)?;

        if matches!(
            st.stop,
            AgentLoopStop::TurnLimit
                | AgentLoopStop::ToolLimit
                | AgentLoopStop::TimeLimit
                | AgentLoopStop::TokenLimit
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
            crate::memory::persist_scratchpad_learnings(ctx.cwd, &self.scratchpad);
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
        let mut tool_results = std::mem::take(&mut st.all_results);
        tool_results.extend(completed_batch_prefix);
        Box::new(AgentLoopResult {
            next_seq: st.next_seq,
            turns: st.turns,
            stop: AgentLoopStop::Cancelled,
            tool_results,
            usage: usage_since(&self.app, ctx.loop_event_start),
        })
    }

    /// Builds the cancelled result.
    fn cancel_loop<A: ProviderAdapter>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        completed_batch_prefix: Vec<ToolResult>,
    ) -> TurnExit {
        st.cancel_mid_run_speculative();
        TurnExit::Return(self.cancelled_result(ctx, st, completed_batch_prefix))
    }

    fn maybe_spawn_mid_run_speculative<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) {
        if self.subagent_depth > 0
            || self.is_cancelled()
            || st
                .mid_run_speculative
                .as_ref()
                .is_some_and(|(_, handle)| !handle.is_finished())
            || self.is_already_compacted(st.messages)
        {
            return;
        }
        let Some(handle) = self.compaction_handle.clone() else {
            return;
        };
        let policy = self.compaction_policy();
        if !st.config.context_compaction_enabled || !policy.enabled || !policy.speculative {
            return;
        }
        let tools = self.workspace_tool_definitions_for(
            ctx.mode,
            ctx.cwd,
            st.config.stable_tool_envelope,
        );
        let usage = self.context_usage(ctx.client.adapter(), st, &tools);
        let current_tokens = estimate_context_tokens(st.messages, usage).tokens;
        let window = st.config.context_window_tokens;
        let effective_settings = policy.effective_settings_for_window(window);
        if window == 0
            || current_tokens.saturating_mul(100) < window.saturating_mul(70)
            || crate::context::should_compact(current_tokens, window, &effective_settings)
        {
            return;
        }
        // One checkpoint per cycle: an older one is still taken at its own cut.
        if crate::context::prepare_compaction(st.messages, &effective_settings, usage).is_none()
            || handle.has_valid_speculative(st.messages)
        {
            return;
        }
        let cancel = CancellationToken::new();
        let job_cancel = cancel.clone();
        let parent_cancel = self.cancellation.clone();
        let client = ctx.client.clone();
        let snapshot_messages = st.messages.clone();
        let mode = ctx.mode;
        let cwd = ctx.cwd.to_path_buf();
        let config = st.config.clone();
        let sensitive_values = self.sensitive_values.clone();
        let mcp = self.mcp.clone();
        let code_intel = self.code_intel.clone();
        let skill_discovery = self.skill_discovery_cache.clone();
        let workspace_snapshot = handle.initial_workspace_snapshot();
        let scratchpad = self.scratchpad.clone();
        let join = tokio::spawn(async move {
            let mut spec_runtime = Runtime::new();
            spec_runtime.sensitive_values = sensitive_values;
            spec_runtime.mcp = mcp;
            spec_runtime.code_intel = code_intel;
            spec_runtime.preseeded_skill_discovery = skill_discovery;
            spec_runtime.preseeded_workspace_snapshot = workspace_snapshot;
            spec_runtime.scratchpad = scratchpad;
            spec_runtime.set_cancellation_token(job_cancel.clone());
            spec_runtime.set_compaction_handle(handle.clone());
            let fut = spec_runtime.run_speculative_compaction(
                &client,
                &snapshot_messages,
                mode,
                &cwd,
                config,
            );
            tokio::pin!(fut);
            let outcome = tokio::select! {
                res = &mut fut => res,
                _ = CancellationToken::cancelled_or_pending(parent_cancel) => {
                    job_cancel.cancel();
                    return;
                }
            };
            if let Ok(Some(spec)) = outcome {
                handle.store_speculative(spec);
            }
        });
        st.mid_run_speculative = Some((cancel, join));
    }

    fn deliver_loop_shell_jobs(&mut self, st: &mut LoopState<'_>) -> Result<bool, ProviderError> {
        self.deliver_shell_completions(st.messages, st.config.max_result_bytes, &mut st.next_seq)
    }

    /// Delivers user messages sent mid-run: each is journaled and appended so
    /// the next provider call reads them in FIFO order.
    fn deliver_steers(&mut self, st: &mut LoopState<'_>) -> Result<(), ProviderError> {
        let Some(inbox) = self.steer_inbox.clone() else {
            return Ok(());
        };
        for text in inbox.drain() {
            self.append_conversation_message(st.messages, ProviderMessage::user(text))?;
        }
        Ok(())
    }

    pub(super) fn steer_pending(&self) -> bool {
        self.steer_inbox
            .as_ref()
            .is_some_and(|inbox| !inbox.is_empty())
    }

    /// After a budget or no-progress stop, asks for a final answer without
    /// tool calls that fits the context window; failures are kept in
    /// `finalization_error` instead of failing the run. The first request is
    /// shaped like the turns (same tools and tool choice, the closing prompt
    /// appended), so it reads their cached prefix: forbidding calls on the wire
    /// or dropping the tools invalidates the cached history. Tool calls in its
    /// answer are never run; only when it brought no text does a second
    /// request forbid them (or carry no tools, where the wire cannot).
    async fn request_final_answer<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
    ) -> Result<(), ProviderError> {
        let closing_prompt = match st.stop {
            AgentLoopStop::NoProgress if st.governor.tuning_stop() => TUNING_FINALIZE_PROMPT,
            AgentLoopStop::NoProgress | AgentLoopStop::RepeatedFailedTool => {
                NO_PROGRESS_FINALIZE_PROMPT
            }
            _ => BUDGET_FINALIZE_PROMPT,
        };
        let turn_tools = self.workspace_tool_definitions_for(
            ctx.mode,
            ctx.cwd,
            st.config.stable_tool_envelope,
        );
        let mut final_messages = st.messages.clone();
        final_messages.push(ProviderMessage::user(closing_prompt));
        // The closing call obeys the same context budget as loop turns:
        // compact the carried history first (Pi's compaction, like any turn)
        // and skip a request that still cannot fit instead of spending a
        // doomed provider round-trip.
        if !self.finalization_fits_budget(ctx.client, &final_messages, &turn_tools, &st.config)
            && st.config.context_compaction_enabled
            && self.compaction_policy().enabled
        {
            let trigger = CompactionTrigger {
                reason: CompactionReason::Threshold,
                required: true,
                usage: self.context_usage(ctx.client.adapter(), st, &turn_tools),
                build_next_request: true,
            };
            match self.compact_for_turn(ctx, st, &turn_tools, trigger).await {
                Ok(CompactionOutcome::Applied(_)) => {
                    st.routing.reset_delta_chain();
                    st.compaction_applied = true;
                    st.compacted_this_turn = true;
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
        if !self.finalization_fits_budget(ctx.client, &final_messages, &turn_tools, &st.config) {
            self.finalization_error = Some(ProviderError::InvalidResponse {
                message: "final response request exceeds the context window".into(),
            });
            st.next_seq = self.observed_next_seq(st.next_seq);
            return Ok(());
        }
        if !self
            .closing_attempt(ctx, st, &mut final_messages, &turn_tools, false)
            .await?
        {
            return Ok(());
        }
        // The answer was tool calls only: ask again with calls forbidden.
        let forbidding_tools: Arc<[Value]> = if ctx.client.finalization_keeps_tools() {
            turn_tools
        } else {
            Arc::from(Vec::new())
        };
        self.closing_attempt(ctx, st, &mut final_messages, &forbidding_tools, true)
            .await?;
        Ok(())
    }

    /// One closing request. Returns whether it should be asked again with
    /// tool calls forbidden: its answer was tool calls and no text, which are
    /// recorded as suppressed and never run.
    async fn closing_attempt<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        ctx: &LoopCtx<'_, A>,
        st: &mut LoopState<'_>,
        final_messages: &mut [ProviderMessage],
        tools: &Arc<[Value]>,
        forbid_tools: bool,
    ) -> Result<bool, ProviderError> {
        let finalize_event_start = self.app.events().len();
        self.uncommitted_event_start = Some(finalize_event_start);
        st.routing.reset_delta_chain();
        let closing = Some(Closing {
            forbid_tools,
            routing: &st.routing,
        });
        // With the tools kept the request shares the turns' cached prefix,
        // so its messages are sent as theirs are: the channel overlay and
        // the write projection. A tool-free request shares none.
        let outcome = if tools.is_empty() {
            self.run_provider_messages_with_tools(
                ctx.client,
                final_messages,
                tools,
                st.next_seq,
                closing,
            )
            .await
        } else {
            let overlay = self.overlay_channel(final_messages, ctx.mode, &st.channel);
            self.run_provider_messages_with_tools(
                ctx.client,
                overlay.view(),
                tools,
                st.next_seq,
                closing,
            )
            .await
        };
        let mut turn = match outcome {
            Ok(turn) => turn,
            Err(error) => {
                self.finalization_error = Some(self.redact_provider_error(error));
                st.next_seq = self.observed_next_seq(st.next_seq);
                return Ok(false);
            }
        };
        st.next_seq = turn.next_seq;
        let text = assistant_text_since(&self.app, finalize_event_start);
        let ignored_calls = tool_calls_since(&self.app, finalize_event_start).len();
        if ignored_calls > 0 {
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::ToolCallsSuppressed {
                    count: ignored_calls as u64,
                },
            )?;
        }
        if !forbid_tools
            && ignored_calls > 0
            && turn.stop == ProviderTurnStop::Normal
            && text.trim().is_empty()
        {
            self.uncommitted_event_start = None;
            return Ok(true);
        }
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
        Ok(false)
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
            || messages[0].content.contains(workspace::CONTEXT_HINT_MARKER)
            || self.is_cancelled()
        {
            return;
        }
        let raw_user_prompt = messages[0].content.clone();
        let tools = self.workspace_tool_definitions_for(mode, cwd, config.stable_tool_envelope);
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
        let Some(snapshot) = self
            .preseeded_workspace_snapshot
            .clone()
            .or_else(|| {
                workspace::initial_workspace_context(cwd, self.cancellation.as_ref())
                    .map(|snap| snap.to_string())
            })
        else {
            return;
        };
        let original_len = messages[0].content.len();
        let redacted_snapshot = self.redact_sensitive(&snapshot);
        messages[0].content.push_str(&redacted_snapshot);
        let mut snapshot_fitted = true;
        if !fits(messages) || self.is_cancelled() {
            snapshot_fitted = false;
            messages[0].content.truncate(original_len);
            if !self.is_cancelled() && self.preseeded_workspace_snapshot.is_none() {
                if let Some(paths_only) = workspace::initial_paths(cwd, self.cancellation.as_ref()) {
                    let redacted_paths = self.redact_sensitive(&paths_only);
                    messages[0].content.push_str(&redacted_paths);
                    if !fits(messages) || self.is_cancelled() {
                        messages[0].content.truncate(original_len);
                    } else {
                        snapshot_fitted = true;
                        if let Some(handle) = &self.compaction_handle {
                            handle.set_initial_workspace_snapshot(redacted_paths);
                        }
                    }
                }
            }
        } else if let Some(handle) = &self.compaction_handle {
            handle.set_initial_workspace_snapshot(redacted_snapshot);
        }

        if snapshot_fitted && !self.is_cancelled() {
            if let Some(hint) = workspace::query_context_hint(cwd, &raw_user_prompt) {
                let redacted_hint = self.redact_sensitive(&hint);
                let pre_hint_len = messages[0].content.len();
                messages[0].content.push_str(&redacted_hint);
                if !fits(messages) || self.is_cancelled() {
                    messages[0].content.truncate(pre_hint_len);
                }
            }
        }
    }

    /// The channel frame of a run over `messages`. Auto only: which MCP
    /// servers exist, without touching the system prompt or the tool
    /// definitions (server-provided text, redacted).
    ///
    /// The anchors of the session's earlier runs keep the text they carried
    /// (the compaction handle outlives a run): a prompt whose stanza moved
    /// away would change the request from that message on, and the provider
    /// would write the whole previous run to its cache again instead of
    /// reading it.
    pub(super) fn channel_frame(
        &self,
        mode: crate::OperatingMode,
        messages: &[ProviderMessage],
    ) -> mode::ChannelFrame {
        self.channel_frame_for(mode, messages, false)
    }

    pub(super) fn channel_frame_for(
        &self,
        mode: crate::OperatingMode,
        messages: &[ProviderMessage],
        preserve_prior_modes: bool,
    ) -> mode::ChannelFrame {
        let frame = self.readonly_channel_frame_for(mode, messages, preserve_prior_modes);
        self.record_channel_anchor(mode, messages, &frame);
        frame
    }

    pub(super) fn readonly_channel_frame(
        &self,
        mode: crate::OperatingMode,
        messages: &[ProviderMessage],
    ) -> mode::ChannelFrame {
        self.readonly_channel_frame_for(mode, messages, false)
    }

    pub(super) fn readonly_channel_frame_for(
        &self,
        mode: crate::OperatingMode,
        messages: &[ProviderMessage],
        preserve_prior_modes: bool,
    ) -> mode::ChannelFrame {
        let prior = self
            .compaction_handle
            .as_ref()
            .map(|handle| {
                if preserve_prior_modes {
                    handle.channel_anchors_preserving_modes(messages)
                } else {
                    handle.channel_anchors(messages, mode)
                }
            })
            .unwrap_or_default();
        let subagents_on = self.subagent_depth == 0 && self.subagents.is_some();
        mode::ChannelFrame::with_prior(messages, self.mcp_awareness(mode), prior)
            .with_subagents(subagents_on)
    }

    /// [`Self::channel_frame`] for a history a compaction just rewrote: the
    /// new prefix is cached from scratch, so earlier anchors are forgotten.
    pub(super) fn rebuilt_channel_frame(
        &self,
        mode: crate::OperatingMode,
        messages: &[ProviderMessage],
    ) -> mode::ChannelFrame {
        if let Some(handle) = &self.compaction_handle {
            handle.clear_channel_anchors();
        }
        let subagents_on = self.subagent_depth == 0 && self.subagents.is_some();
        let frame = mode::ChannelFrame::rebuilt_for(messages, self.mcp_awareness(mode))
            .with_subagents(subagents_on);
        self.record_channel_anchor(mode, messages, &frame);
        frame
    }

    fn record_channel_anchor(
        &self,
        mode: crate::OperatingMode,
        messages: &[ProviderMessage],
        frame: &mode::ChannelFrame,
    ) {
        let Some(handle) = &self.compaction_handle else {
            return;
        };
        if let Some(index) = frame.current_anchor(messages) {
            handle.record_channel_anchor(
                messages,
                index,
                mode,
                frame.applied_text(mode, self.can_ask(mode)),
            );
        }
    }

    /// The MCP server block of the overlay as it is now (Auto only).
    pub(super) fn mcp_awareness(&self, mode: crate::OperatingMode) -> Option<String> {
        if !mode.allows_mutation() {
            return None;
        }
        let mut block = self
            .mcp
            .as_ref()
            .and_then(|manager| manager.awareness_block_for(mode == crate::OperatingMode::Code));
        if mode != crate::OperatingMode::Code {
            let (_, deferred) = self.split_direct_mcp_definitions();
            if !deferred.is_empty() {
                let compact = crate::mcp::compact_deferred_direct_list(&deferred);
                match &mut block {
                    Some(existing) => {
                        existing.push_str("\n\n");
                        existing.push_str(&compact);
                    }
                    None => block = Some(compact),
                }
            }
        }
        block.map(|text| self.redact_sensitive(&text))
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

    fn prepare_overlay_request<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        view: &[ProviderMessage],
        tools: &[Value],
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let mut request = match client.prepare_messages_with_tools(view, tools) {
            Ok(request) => request,
            Err(err) if client.adapter().wire_kind() == ProviderKind::Anthropic => {
                if let Some(degraded) =
                    crate::provider::image_limits::degrade_history_images_for_anthropic(view)
                {
                    client.prepare_messages_with_tools(&degraded, tools)?
                } else {
                    return Err(err);
                }
            }
            Err(err) => return Err(err),
        };
        if let Some(target) = self.model_override.as_deref() {
            request = crate::provider::override_request_model(client.adapter(), request, target);
        } else if self.deep_reasoning {
            request =
                crate::provider::raise_request_reasoning_effort(client.adapter(), request, "high");
        }
        Ok(request)
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
        self.prepare_overlay_request(client, overlay.view(), tools)
    }

    pub(super) fn prepare_loop_capabilities(&mut self, cwd: &Path) -> Result<(), ProviderError> {
        self.restore_task_facts(&self.task_facts(), cwd)?;
        if let Some(handle) = &self.compaction_handle {
            self.token_estimator = handle.token_estimator();
        }
        // Snapshot direct MCP tool definitions once per run so a lazy server
        // finishing its handshake mid-run does not mutate `tools` and bust the
        // provider prompt cache.
        self.frozen_direct_mcp_tools = self
            .mcp
            .as_ref()
            .map(|manager| manager.direct_tool_definitions());
        // Bound skill-discovery memoization to one loop run (unless preseeded by parent).
        self.skill_discovery_cache = self
            .preseeded_skill_discovery
            .take()
            .filter(|(dir, _)| dir == cwd);
        self.write_projection_cache
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.cached_skill_discovery(cwd);
        Ok(())
    }
}

/// Seconds under a minute, tenths of a minute above.
fn budget_clock(duration: std::time::Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.0} s")
    } else {
        format!("{:.1} min", secs / 60.0)
    }
}

/// The steer for a batch whose tool repeated evidence without a state change.
fn causal_reuse_steer(events: &[crate::SessionEvent]) -> Option<String> {
    events.iter().find_map(|event| match &event.kind {
        crate::EventKind::CausalAnomalyDetected {
            kind: crate::CausalAnomalyKind::TuningLoop,
            occurrence,
            action: crate::CausalShadowAction::WouldWarn,
            ..
        } => Some(format!("{occurrence} consecutive turns only retuned numbers or colours. Check the result against the task's own criterion: if it passes, finish now with the current values; if it fails, make a structural fix instead of another retune. More tuning-only turns end the run.")),
        crate::EventKind::CausalAnomalyDetected {
            tool_name,
            call_id,
            confidence: crate::CausalConfidence::High,
            action: crate::CausalShadowAction::WouldReuse | crate::CausalShadowAction::WouldWarn,
            ..
        } => Some(format!("Tool {tool_name} call {call_id} repeated evidence without a relevant state change. Use the existing result or change the approach. Retry only after the dependency changes or if the needed content is no longer available; otherwise finish with the known outcome and blocker.")),
        _ => None,
    })
}

/// Who reported a usage anchor: usage of one model says nothing about the
/// context another one would count.
fn usage_source(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// `then_run.final` when every call of the batch succeeded and the
/// write/patch carrying it reported a passing fused check; the last one wins.
fn then_run_final_answer(calls: &[ProviderToolCall], results: &[ToolResult]) -> Option<String> {
    if calls.len() != results.len() || results.iter().any(|result| !result.success) {
        return None;
    }
    calls
        .iter()
        .zip(results)
        .rev()
        .filter(|(call, _)| matches!(call.name.as_str(), "write" | "patch"))
        .filter(|(_, result)| result.output.contains(crate::tools::THEN_RUN_PASSED))
        .find_map(|(call, _)| {
            let arguments = serde_json::from_str::<Value>(&call.arguments).ok()?;
            let answer = arguments.get("then_run")?.get("final")?.as_str()?.trim();
            (!answer.is_empty()).then(|| answer.to_owned())
        })
}

fn is_verification_check_call(call: &ProviderToolCall) -> bool {
    let command = match call.name.as_str() {
        "shell" => serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_owned)),
        "write" | "patch" => serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|v| {
                v.get("then_run")
                    .and_then(|tr| tr.get("command"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }),
        _ => None,
    };
    let Some(cmd) = command else {
        return false;
    };
    let lower = cmd.to_ascii_lowercase();
    [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "pytest",
        "npm test",
        "vitest",
        "jest",
        "go test",
        "tsc",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}
