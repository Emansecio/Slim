mod app_handle;
mod capability_bridge;
mod economy;
mod governor;
mod loop_guard;
mod manual_retry;
mod mode;
#[cfg(test)]
mod performance;
mod shell_jobs;
mod usage;
mod workspace;

pub use economy::CompactionPricing;
pub use usage::{RequestUsage, UsageTotals};

pub use workspace::without_workspace_snapshot;

use crate::codeintel::CodeIntelligence;
use crate::context::{
    apply_compaction_selection, build_bounded_summary_prompt_with_checkpoint,
    build_bounded_summary_prompt_with_checkpoint_and_instructions, compaction_prefix_fingerprint,
    estimate_provider_message_tokens, has_compactable_history, local_emergency_summary,
    select_compaction_history, AdaptiveTokenEstimator, ArtifactStore, CompactionCommit,
    CompactionHandle, CompactionPolicy, CompactionReason, CompactionSelection, ContextBudget,
    PreparedCompaction, COMPACTION_SYSTEM_PROMPT,
};
use crate::interaction::{
    ask_question_definition, AskQuestion, InteractionRequestId, InteractionRoute,
};
use crate::mcp::{McpCancellation, McpManager, McpRequestOutcome};
use crate::model::AppHandle;
use crate::provider::{
    HttpProviderClient, PreparedProviderRequest, ProviderAdapter, ProviderCallTelemetry,
    ProviderError, ProviderEvent, ProviderKind, ProviderMessage, ProviderPhase,
    ProviderRequestComponents, ProviderToolCall,
};
use crate::session::{
    AuthorizationGrant, CapabilityCatalog, CapabilityLedgerError, DurableFact, DurableRecord,
    DurableRepo, DurableSessionHeader, MemoryRepo, TaskMutation, TaskMutationRequest,
    TaskTodoStatus,
};
use crate::skills::{
    discover_workspace, invoke_script_with_limits_and_runner, DiscoveryResult,
    SkillInvocationRequest, DEFAULT_SKILL_OUTPUT_BYTES,
};
use crate::tools::{
    present_unstructured, CodeIntelRequest, PreparedToolArguments, PreparedToolInvocation,
    PresentationBudget, ToolEffectClass, ToolExecutionOutcome, ToolExecutionReceipt,
    ToolPresentation, ToolPresentationSource, ToolRegistry, ToolResult,
};
use futures_util::StreamExt;
use governor::{CausalGovernor, GovernorObservation};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use tokio::sync::Notify;

pub use app_handle::RuntimeHandle;
pub use capability_bridge::RuntimeCapabilityBridge;
pub use loop_guard::LoopGuard;
pub use manual_retry::ManualRetryHandle;
pub use mode::mode_name;

#[derive(Clone)]
pub struct CancellationToken(Arc<CancellationState>);

struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
    mcp: McpCancellation,
    native_work: AtomicUsize,
    background_work: AtomicUsize,
    native_idle: Notify,
}

struct NativeWorkGuard {
    token: CancellationToken,
    background: bool,
}

impl Drop for NativeWorkGuard {
    fn drop(&mut self) {
        let state = &self.token.0;
        let count = if self.background {
            &state.background_work
        } else {
            &state.native_work
        };
        if count.fetch_sub(1, Ordering::AcqRel) == 1 {
            state.native_idle.notify_waiters();
        }
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(CancellationState {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
            mcp: McpCancellation::new(),
            native_work: AtomicUsize::new(0),
            background_work: AtomicUsize::new(0),
            native_idle: Notify::new(),
        }))
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::AcqRel) {
            self.0.notify.notify_waiters();
            self.0.mcp.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    fn mcp_cancellation(&self) -> McpCancellation {
        self.0.mcp.clone()
    }

    pub async fn cancelled(&self) {
        let notified = self.0.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }

    fn track_native_work(&self) -> NativeWorkGuard {
        self.0.native_work.fetch_add(1, Ordering::AcqRel);
        NativeWorkGuard {
            token: self.clone(),
            background: false,
        }
    }

    fn track_background_work(&self) -> NativeWorkGuard {
        self.0.background_work.fetch_add(1, Ordering::AcqRel);
        NativeWorkGuard {
            token: self.clone(),
            background: true,
        }
    }

    async fn wait_for_foreground_work(&self) {
        loop {
            let notified = self.0.native_idle.notified();
            if self.0.native_work.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Aborting an async task does not stop its blocking native worker.
    /// Hosts must wait for these workers before acknowledging cancellation.
    pub async fn wait_for_native_work(&self) {
        loop {
            let notified = self.0.native_idle.notified();
            if self.0.native_work.load(Ordering::Acquire) == 0
                && self.0.background_work.load(Ordering::Acquire) == 0
            {
                return;
            }
            notified.await;
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl PartialEq for CancellationToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CancellationToken {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentLoopConfig {
    /// Run ceiling. Sequential 1-tool/turn work stops here, not on tool caps.
    pub max_turns: usize,
    /// Per-turn mutating native action slots; then_run reserves a second slot.
    pub max_mutating_tool_calls: usize,
    /// Per-turn read batch cap (read/list/search). Not a run total.
    pub max_read_tool_calls: usize,
    /// Cumulative native action slots reserved across the run; then_run reserves a second slot.
    pub max_total_tool_calls: usize,
    pub max_result_bytes: usize,
    pub context_window_tokens: u64,
    pub context_reserve_tokens: u64,
    pub context_compaction_enabled: bool,
    /// First automatic provider-recovery delay; it doubles per attempt (at most 16x).
    /// A provider's retry-after still wins when it is longer.
    pub provider_recovery_backoff: std::time::Duration,
}

impl AgentLoopConfig {
    pub const DEFAULT_MAX_TURNS: usize = 128;
    pub const DEFAULT_MAX_MUTATING_TOOL_CALLS: usize = 32;
    pub const DEFAULT_MAX_READ_TOOL_CALLS: usize = 96;
    pub const DEFAULT_MAX_TOTAL_TOOL_CALLS: usize = 256;
    pub const DEFAULT_PROVIDER_RECOVERY_BACKOFF: std::time::Duration =
        std::time::Duration::from_millis(500);
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_turns: Self::DEFAULT_MAX_TURNS,
            max_mutating_tool_calls: Self::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            max_read_tool_calls: Self::DEFAULT_MAX_READ_TOOL_CALLS,
            max_total_tool_calls: Self::DEFAULT_MAX_TOTAL_TOOL_CALLS,
            max_result_bytes: 16 * 1024,
            context_window_tokens: 32_000,
            context_reserve_tokens: 4_096,
            context_compaction_enabled: true,
            provider_recovery_backoff: Self::DEFAULT_PROVIDER_RECOVERY_BACKOFF,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolCallBucket {
    Read,
    Mutating,
}

fn tool_call_bucket(name: &str) -> ToolCallBucket {
    match name {
        "read" | "list" | "search" | "code_intel" => ToolCallBucket::Read,
        _ => ToolCallBucket::Mutating,
    }
}

fn tool_call_slots(call: &ProviderToolCall) -> usize {
    if matches!(call.name.as_str(), "write" | "patch")
        && serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .is_some_and(|arguments| arguments.get("then_run").is_some_and(Value::is_object))
    {
        2
    } else {
        1
    }
}

fn mark_cancelled_tool_outcome(outcome: &mut ToolExecutionOutcome) {
    outcome.result.success = false;
    if let Some(fused_shell) = outcome.receipt.fused_shell.as_mut() {
        fused_shell.1.success = false;
    }
}

pub fn tool_call_is_read_only(name: &str) -> bool {
    matches!(tool_call_bucket(name), ToolCallBucket::Read)
}

fn tool_call_is_parallel_snapshot_read(tools: &ToolRegistry, name: &str) -> bool {
    name == "code_intel"
        || tools
            .operational_spec(name)
            .is_some_and(|spec| spec.effect_class == ToolEffectClass::SnapshotRead)
}

fn is_serial_barrier(prepared: &PreparedToolInvocation) -> bool {
    matches!(prepared.name.as_str(), "shell" | "skill" | "mcp")
        || matches!(
            &prepared.arguments,
            PreparedToolArguments::Write {
                then_run: Some(_),
                ..
            } | PreparedToolArguments::Patch {
                then_run: Some(_),
                ..
            }
        )
}

fn is_file_mutation(prepared: &PreparedToolInvocation) -> bool {
    matches!(prepared.name.as_str(), "write" | "patch") && !prepared.target_paths.is_empty()
}

fn mutation_path_key(prepared: &PreparedToolInvocation) -> Option<String> {
    prepared
        .target_paths
        .first()
        .map(|path| crate::tools::path_identity(path))
}

fn snapshot_depends_on_prior_mutation(
    prior: &PreparedToolInvocation,
    snapshot: &PreparedToolInvocation,
) -> bool {
    if !is_file_mutation(prior) {
        return false;
    }
    let Some(mutated) = prior.target_paths.first() else {
        return true;
    };
    match snapshot.name.as_str() {
        "search" => true,
        // Semantic results span the whole workspace: a patch to b.rs changes
        // the references of a symbol in a.rs. Path equality cannot prove
        // independence, so any prior file mutation blocks anticipation.
        "code_intel" => true,
        "list" => snapshot
            .target_paths
            .first()
            .is_some_and(|dir| mutated == dir || mutated.starts_with(dir)),
        _ => {
            snapshot.target_paths.is_empty()
                || snapshot.target_paths.iter().any(|path| path == mutated)
        }
    }
}

fn phase1_snapshot_indices(
    tools: &ToolRegistry,
    prepared: &[PreparedToolInvocation],
) -> Vec<usize> {
    phase1_snapshot_indices_ready(tools, prepared, 0, &vec![false; prepared.len()])
}

#[cfg(test)]
fn phase1_snapshot_indices_from(
    tools: &ToolRegistry,
    prepared: &[PreparedToolInvocation],
    from: usize,
) -> Vec<usize> {
    phase1_snapshot_indices_ready(tools, prepared, from, &vec![false; prepared.len()])
}

/// Selects snapshot reads that are ready at the current scheduler state.
///
/// A file mutation is a dependency barrier only while it is still pending.
/// Once the corresponding call has completed (successfully or otherwise), a
/// read whose dependency was that mutation may join the next parallel wave.
/// Serial barriers remain fail-closed: a pending shell/skill/MCP call blocks
/// every later snapshot until it has completed.
fn phase1_snapshot_indices_ready(
    tools: &ToolRegistry,
    prepared: &[PreparedToolInvocation],
    from: usize,
    completed: &[bool],
) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut barrier = false;
    for (index, call) in prepared.iter().enumerate().skip(from) {
        let is_completed = completed.get(index).copied().unwrap_or(false);
        if is_serial_barrier(call) && !is_completed {
            barrier = true;
        }
        if is_completed || barrier || !tool_call_is_parallel_snapshot_read(tools, &call.name) {
            continue;
        }
        let blocked = prepared[..index]
            .iter()
            .enumerate()
            .any(|(prior_index, prior)| {
                !completed.get(prior_index).copied().unwrap_or(false)
                    && snapshot_depends_on_prior_mutation(prior, call)
            });
        if !blocked {
            indices.push(index);
        }
    }
    indices
}

fn evidence_reuse_aliases(prepared_calls: &[PreparedToolInvocation]) -> Vec<usize> {
    let mut alias_of: Vec<usize> = (0..prepared_calls.len()).collect();
    let mut leaders = std::collections::HashMap::<String, usize>::new();
    for (index, prepared) in prepared_calls.iter().enumerate() {
        if !prepared.reusable_evidence() {
            continue;
        }
        if let Some(&leader) = leaders.get(&prepared.canonical_fingerprint) {
            alias_of[index] = leader;
        } else {
            leaders.insert(prepared.canonical_fingerprint.clone(), index);
        }
    }
    alias_of
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolBudgetCut {
    suppressed: usize,
    hit_run_total: bool,
    /// Both per-turn buckets are zero: no emitted call can ever execute, so
    /// retrying next turn would only spend provider requests for nothing.
    buckets_disabled: bool,
}

#[cfg(test)]
fn truncate_calls_for_budget(
    calls: &mut Vec<ProviderToolCall>,
    config: AgentLoopConfig,
    already_reserved: usize,
) -> ToolBudgetCut {
    split_calls_for_budget(calls, config, already_reserved).0
}

/// Keeps the executable prefix in `calls` and returns the suppressed tail.
fn split_calls_for_budget(
    calls: &mut Vec<ProviderToolCall>,
    config: AgentLoopConfig,
    already_reserved: usize,
) -> (ToolBudgetCut, Vec<ProviderToolCall>) {
    let mut read_used = 0usize;
    let mut mutating_used = 0usize;
    let mut total_used = 0usize;
    let mut hit_run_total = false;
    let remaining_total = config.max_total_tool_calls.saturating_sub(already_reserved);
    let accepted = calls
        .iter()
        .take_while(|call| {
            let slots = tool_call_slots(call);
            if total_used.saturating_add(slots) > remaining_total {
                hit_run_total = true;
                return false;
            }
            let (used, limit) = match tool_call_bucket(&call.name) {
                ToolCallBucket::Read => (&mut read_used, config.max_read_tool_calls),
                ToolCallBucket::Mutating => (&mut mutating_used, config.max_mutating_tool_calls),
            };
            if used.saturating_add(slots) > limit {
                return false;
            }
            *used = used.saturating_add(slots);
            total_used = total_used.saturating_add(slots);
            true
        })
        .count();
    let suppressed = calls.split_off(accepted);
    (
        ToolBudgetCut {
            suppressed: suppressed.len(),
            hit_run_total,
            buckets_disabled: config.max_read_tool_calls == 0
                && config.max_mutating_tool_calls == 0,
        },
        suppressed,
    )
}

/// Names the calls a per-turn cap dropped so the model can reissue exactly
/// those; arguments are shortened, and the message is redacted on append.
fn suppressed_calls_steer(suppressed: &[ProviderToolCall]) -> String {
    const MAX_ARGUMENT_CHARS: usize = 160;
    let mut steer = format!(
        "{} tool call(s) this turn were not executed (per-turn cap):",
        suppressed.len()
    );
    for call in suppressed {
        let mut arguments: String = call.arguments.chars().take(MAX_ARGUMENT_CHARS).collect();
        if arguments.len() < call.arguments.len() {
            arguments.push_str("...");
        }
        steer.push_str(&format!(
            "
- {} {arguments}",
            call.name
        ));
    }
    steer.push_str("
Retry only these calls next turn if they are still needed; do not repeat calls that already returned results.");
    steer
}

fn should_stop_after_tool_budget_cut(cut: ToolBudgetCut, turn: usize, max_turns: usize) -> bool {
    cut.suppressed > 0 && (cut.hit_run_total || cut.buckets_disabled || turn + 1 >= max_turns)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentLoopStop {
    ProviderCompleted,
    ProviderTruncated,
    ProviderFiltered,
    TurnLimit,
    ToolLimit,
    RepeatedFailedTool,
    /// The causal ledger asked to stop (repeated evidence / stagnant turns):
    /// continuing would only burn turns without progress.
    NoProgress,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLoopResult {
    pub next_seq: u64,
    pub turns: usize,
    pub stop: AgentLoopStop,
    pub tool_results: Vec<ToolResult>,
    pub usage: UsageTotals,
}

struct ProviderTurnResult {
    next_seq: u64,
    blocks_tools: bool,
    stop: ProviderTurnStop,
    responses_reasoning: Vec<crate::provider::ResponsesReasoning>,
    chat_reasoning: Option<crate::provider::ChatReasoning>,
}

const COMPACTION_MAX_OUTPUT_TOKENS: u64 = 2_048;

/// Best-effort closing turn after a budget stop. Sent with `tools=[]` so the
/// model answers with what it already knows instead of the user only seeing
/// `Turn limit reached / Tool budget exhausted`. Failures are retained for stop diagnostics and the
/// original `stop` is preserved.
const BUDGET_FINALIZE_PROMPT: &str = "Budget exhausted. Respond now as the final answer with what is already known: outcome, changed files/behavior, validation result, remaining risks. Do not call tools.";
const NO_PROGRESS_FINALIZE_PROMPT: &str = "Execution stopped because repeated tool work produced no new evidence or workspace progress. Give a brief final answer: what is known, what remains incomplete, and what new information or changed state would allow progress. Do not call tools or claim completion without evidence.";

/// Between-turns steer (Pit-inspired, without mid-stream abort): when a turn
/// reuses byte-identical evidence already in context, nudge the next provider
/// turn to act instead of re-reading. Capped so it can never inflate context.
const MAX_BUDGET_STEERS: usize = 2;

struct BackgroundCompactionPlan {
    selection: CompactionSelection,
    request: Option<PreparedProviderRequest>,
    provider: String,
    model: String,
    provider_identity: String,
    strategy: crate::context::CompactionStrategy,
    jev_plan: Option<crate::context::jev_prune::PreparedPrune>,
    previous_checkpoint: Option<String>,
    context_window_tokens: u64,
    reserve_tokens: u64,
    serialized_chars: u64,
    system_bytes: u64,
    history_bytes: u64,
    summary_max_bytes: usize,
    source_len: usize,
    tokens_before: u64,
    projected_tokens_after: u64,
    request_bytes: u64,
    estimated_input_tokens: u64,
    projected_savings_tokens: u64,
    estimated_cost_tokens: u64,
    safety_margin_tokens: u64,
    future_turns: u8,
    profitable: bool,
}

struct BackgroundCompactionResult {
    plan: BackgroundCompactionPlan,
    summary: String,
    usage: UsageTotals,
    time_to_first_byte_ms: Option<u64>,
    time_to_first_semantic_ms: Option<u64>,
    duration_ms: u64,
    valid: bool,
    usage_known: bool,
    cancelled: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct CompactionAttemptProgress {
    send_started: bool,
    headers_received: bool,
    first_byte_received: bool,
    first_token_received: bool,
    time_to_first_byte_ms: Option<u64>,
    time_to_first_semantic_ms: Option<u64>,
}

struct PendingBackgroundCompaction {
    task: tokio::task::JoinHandle<BackgroundCompactionResult>,
    cancellation: CancellationToken,
    progress: Arc<Mutex<CompactionAttemptProgress>>,
    jev_outcome: Arc<Mutex<Option<JevAttemptOutcome>>>,
    usage_request: RequestUsage,
    request_bytes: u64,
    estimated_input_tokens: u64,
    tokens_before: u64,
    started: Instant,
}

struct BackgroundCompactionObservers {
    progress: Arc<Mutex<CompactionAttemptProgress>>,
    jev_outcome: Arc<Mutex<Option<JevAttemptOutcome>>>,
    provider_call_journal: Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
}

struct JevAttemptOutcome {
    error: Option<String>,
    stats: crate::context::JevPruneStats,
}

/// Backstop: a background compaction task must never outlive its handle.
/// Normal paths settle via `cancel_pending_background`; this only fires on
/// early `?` returns that would otherwise detach a live HTTP stream.
impl Drop for PendingBackgroundCompaction {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Copy)]
struct CompactionUsageEvent {
    input_tokens: u64,
    output_tokens: u64,
    input_known: bool,
    output_known: bool,
}

#[derive(Default)]
struct CompactionSummary {
    text: String,
    usage: UsageTotals,
    usage_events: Vec<CompactionUsageEvent>,
    breakdown_events: Vec<crate::UsageBreakdown>,
    time_to_first_byte_ms: Option<u64>,
    time_to_first_semantic_ms: Option<u64>,
    breakdown_seen: bool,
    stop_reason: Option<String>,
    saw_tool_call: bool,
}

impl CompactionSummary {
    fn push(&mut self, event: ProviderEvent) {
        match event {
            ProviderEvent::TextDelta(delta) => self.text.push_str(&delta),
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                if !self.breakdown_seen {
                    self.usage.add(input_tokens, output_tokens);
                }
                self.usage_events.push(CompactionUsageEvent {
                    input_tokens,
                    output_tokens,
                    input_known: true,
                    output_known: true,
                });
            }
            ProviderEvent::UsagePartial {
                input_tokens,
                output_tokens,
                input_complete,
                output_complete,
            } => {
                if !self.breakdown_seen {
                    self.usage.add(input_tokens, output_tokens);
                }
                self.usage_events.push(CompactionUsageEvent {
                    input_tokens,
                    output_tokens,
                    input_known: input_complete,
                    output_known: output_complete,
                });
            }
            ProviderEvent::Stopped { reason } => self.stop_reason = Some(reason),
            ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallInputDelta { .. }
            | ProviderEvent::ToolCallComplete { .. }
            | ProviderEvent::ToolCall { .. } => self.saw_tool_call = true,
            ProviderEvent::UsageBreakdown { usage } => {
                self.breakdown_seen = true;
                self.usage.add_breakdown(usage);
                self.breakdown_events.push(usage);
            }
            ProviderEvent::ResponseCacheHit => {}
            ProviderEvent::Phase {
                phase: ProviderPhase::FirstByte,
                elapsed_ms,
            } => {
                self.time_to_first_byte_ms.get_or_insert(elapsed_ms);
            }
            ProviderEvent::Phase {
                phase: ProviderPhase::FirstSemantic,
                elapsed_ms,
            } => {
                self.time_to_first_semantic_ms.get_or_insert(elapsed_ms);
            }
            ProviderEvent::ReasoningDelta(_)
            | ProviderEvent::ResponsesReasoning(_)
            | ProviderEvent::ChatReasoning(_)
            | ProviderEvent::ReasoningStarted
            | ProviderEvent::ReasoningEnded
            | ProviderEvent::Phase { .. }
            | ProviderEvent::ToolCallProgress { .. }
            | ProviderEvent::ContentBlockStop { .. } => {}
        }
    }

    fn usage_known(&self) -> bool {
        let input_known = self.usage_events.iter().any(|event| event.input_known);
        let output_known = self.usage_events.iter().any(|event| event.output_known);
        input_known && output_known && !self.usage.usage_unknown
    }

    fn validate(&self, max_bytes: usize) -> Result<(), ProviderError> {
        let raw_reason =
            self.stop_reason
                .as_deref()
                .ok_or_else(|| ProviderError::InvalidResponse {
                    message: "summary provider stream ended without a stop reason".into(),
                })?;
        let normalized = raw_reason.trim().to_ascii_lowercase();
        if self.saw_tool_call
            || matches!(
                normalized.as_str(),
                "tool_calls" | "function_call" | "tool_use"
            )
        {
            return Err(ProviderError::InvalidResponse {
                message: "summary provider attempted to call a tool".into(),
            });
        }
        let stop = classify_provider_stop_reason(raw_reason, &[]).map_err(|error| match error {
            ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
                message: format!("summary provider returned an invalid stop reason: {message}"),
            },
            other => other,
        })?;
        match stop {
            ProviderTurnStop::Normal => {}
            ProviderTurnStop::Truncated => {
                return Err(ProviderError::InvalidResponse {
                    message: "summary provider response was truncated".into(),
                });
            }
            ProviderTurnStop::Filtered => {
                return Err(ProviderError::InvalidResponse {
                    message: "summary provider response was filtered".into(),
                });
            }
        }
        crate::context::validate_checkpoint_content(&self.text, max_bytes).map_err(|message| {
            ProviderError::InvalidResponse {
                message: format!("summary provider returned an invalid checkpoint: {message}"),
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderTurnStop {
    Normal,
    Truncated,
    Filtered,
}

impl ProviderTurnStop {
    fn error_message(self) -> &'static str {
        match self {
            Self::Normal => "provider turn did not stop with a non-normal reason",
            Self::Truncated => "provider response was truncated",
            Self::Filtered => "provider response was filtered",
        }
    }
}

fn classify_provider_stop_reason(
    raw_stop_reason: &str,
    sensitive_values: &[String],
) -> Result<ProviderTurnStop, ProviderError> {
    let stop_reason = raw_stop_reason.trim().to_ascii_lowercase();
    match stop_reason.as_str() {
        "stop" | "tool_calls" | "function_call" | "end_turn" | "tool_use" | "stop_sequence"
        | "completed" | "complete" => Ok(ProviderTurnStop::Normal),
        reason
            if reason == "length"
                || reason == "incomplete"
                || reason.contains("max_tokens")
                || reason.contains("max_output_tokens")
                || reason.contains("max_completion_tokens")
                || reason.contains("truncat") =>
        {
            Ok(ProviderTurnStop::Truncated)
        }
        reason if reason.contains("filter") || reason == "safety" => Ok(ProviderTurnStop::Filtered),
        _ => {
            let reason: String = redact_values(sensitive_values, raw_stop_reason.trim())
                .chars()
                .take(128)
                .collect();
            Err(ProviderError::InvalidResponse {
                message: format!("unsupported stop reason: {reason}"),
            })
        }
    }
}

/// Recovery counters for one agent-loop run; each bounds one recovery kind.
struct RecoveryBudget {
    initial_context_reserve: u64,
    overflow_retry_used: bool,
    provider_recoveries: u32,
    truncation_recoveries: u32,
    recovery_output_limit: Option<u64>,
    compaction_recoveries: u32,
    argument_repairs: u32,
    provider_recovery_wait: std::time::Duration,
    automatic_recoveries: u32,
    provider_attempts: u32,
    empty_recovery_used: bool,
}

impl RecoveryBudget {
    fn new(initial_context_reserve: u64) -> Self {
        Self {
            initial_context_reserve,
            overflow_retry_used: false,
            provider_recoveries: 0,
            truncation_recoveries: 0,
            recovery_output_limit: None,
            compaction_recoveries: 0,
            argument_repairs: 0,
            provider_recovery_wait: std::time::Duration::ZERO,
            automatic_recoveries: 0,
            provider_attempts: 0,
            empty_recovery_used: false,
        }
    }
}

/// Why this turn compacts before sending its request.
#[derive(Clone, Copy)]
struct CompactionTrigger {
    preflight_tokens: u64,
    manual: bool,
    over_hard: bool,
}

enum CompactionOutcome {
    /// History was replaced; the request is already prepared from it.
    Applied(Box<PreparedProviderRequest>),
    Skipped,
    Cancelled,
}

/// What the loop does after a provider attempt.
enum ProviderAttempt {
    Completed(ProviderTurnResult),
    /// Resend the current turn; it does not consume the turn budget.
    Retry,
    /// A correction request that counts as a new model turn.
    NextTurn,
}

fn cancelled_agent_loop_result(
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

pub struct Runtime {
    pub app: AppHandle,
    tools: ToolRegistry,
    shell_jobs: shell_jobs::ShellJobs,
    artifact_store: Option<ArtifactStore>,
    restored_artifact_ids: std::collections::HashSet<String>,
    conversation: Vec<ProviderMessage>,
    turn_transcript: Option<Vec<ProviderMessage>>,
    uncommitted_event_start: Option<usize>,
    pending_argument_repair: Option<String>,
    finalization_error: Option<ProviderError>,
    sensitive_values: SensitiveValues,
    cancellation: Option<CancellationToken>,
    interaction_route: Option<InteractionRoute>,
    capability_bridge: Option<RuntimeCapabilityBridge<MemoryRepo>>,
    compaction_handle: Option<CompactionHandle>,
    manual_retry: Option<ManualRetryHandle>,
    jev_judge: Option<std::sync::Arc<dyn crate::context::JevJudge>>,
    jev_economy: economy::JevEconomy,
    compaction_input_cost_micros_per_million: Option<u64>,
    compaction_pricing: Option<CompactionPricing>,
    read_presentation_bytes: usize,
    write_projection_cache: Mutex<mode::WriteProjectionCache>,
    background_compaction_enabled: bool,
    code_intel: Option<Arc<dyn CodeIntelligence>>,
    mcp: Option<Arc<McpManager>>,
    token_estimator: AdaptiveTokenEstimator,
    /// Skill discovery memoized for one loop run (`Runtime` is per-turn).
    /// `None` inside means discovery failed; callers fall back to direct
    /// discovery so error messages stay exactly as before.
    skill_discovery_cache: Option<(PathBuf, Option<DiscoveryResult>)>,
    presentation_sources: std::collections::HashMap<(String, String), ToolPresentationSource>,
}

const READ_ONLY_BATCH_CONCURRENCY: usize = 8;

struct ReadOnlyToolOutcome {
    outcome: ToolExecutionOutcome,
    duration_ms: u64,
}

/// Notification sent by a pool future immediately before it dispatches tool
/// work. The outer runtime owns the event journal and assigns the monotonic
/// sequence when it receives this notice.
struct ToolStartedNotice {
    index: usize,
    arguments: String,
}

#[derive(Default)]
struct SensitiveValues(Vec<String>);

#[derive(Clone, Copy)]
struct ToolInvocation<'a> {
    batch_id: &'a str,
    call_id: &'a str,
    name: &'a str,
    arguments: &'a str,
}

impl<'a> ToolInvocation<'a> {
    fn provider(batch_id: &'a str, call: &'a ProviderToolCall) -> Self {
        Self {
            batch_id,
            call_id: &call.id,
            name: &call.name,
            arguments: &call.arguments,
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct ToolDefinitionSetKey {
    mode: crate::OperatingMode,
    code_intel_enabled: bool,
    artifact_enabled: bool,
    interaction_enabled: bool,
    mcp_enabled: bool,
    skills_enabled: bool,
}

static TOOL_DEFINITION_SETS: LazyLock<
    Mutex<std::collections::HashMap<ToolDefinitionSetKey, Arc<[Value]>>>,
> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

impl fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("app", &self.app)
            .field("tools", &self.tools)
            .field("artifact_store", &self.artifact_store)
            .finish()
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            app: AppHandle::fake(),
            tools: ToolRegistry::default(),
            shell_jobs: shell_jobs::ShellJobs::default(),
            artifact_store: None,
            restored_artifact_ids: std::collections::HashSet::new(),
            conversation: Vec::new(),
            turn_transcript: None,
            uncommitted_event_start: None,
            pending_argument_repair: None,
            finalization_error: None,
            sensitive_values: SensitiveValues::default(),
            cancellation: None,
            interaction_route: None,
            capability_bridge: None,
            compaction_handle: None,
            manual_retry: None,
            jev_judge: None,
            jev_economy: economy::JevEconomy::default(),
            compaction_input_cost_micros_per_million: None,
            compaction_pricing: None,
            read_presentation_bytes: 64 * 1024,
            write_projection_cache: Mutex::new(mode::WriteProjectionCache::default()),
            background_compaction_enabled: false,
            code_intel: None,
            mcp: None,
            token_estimator: AdaptiveTokenEstimator::default(),
            skill_discovery_cache: None,
            presentation_sources: std::collections::HashMap::new(),
        }
    }

    pub fn with_artifact_store(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut runtime = Self::new();
        runtime.artifact_store = Some(ArtifactStore::new(root)?);
        Ok(runtime)
    }

    /// IDs come from artifact.v1 facts in the resumed durable session.
    pub fn restore_artifact_ids(&mut self, ids: &[String]) {
        self.restored_artifact_ids = ids.iter().cloned().collect();
    }

    pub fn set_tool_registry(&mut self, tools: ToolRegistry) {
        self.tools = tools;
    }

    pub fn set_cancellation_token(&mut self, cancellation: CancellationToken) {
        self.cancellation = Some(cancellation);
    }

    pub fn set_interaction_route(&mut self, route: InteractionRoute) {
        self.interaction_route = Some(route);
    }

    pub fn set_compaction_handle(&mut self, handle: CompactionHandle) {
        self.compaction_handle = Some(handle);
    }

    /// Attach the Jev judge used when the active compaction policy selects the
    /// Jev pruning strategy. `None` leaves every run on the LLM summary path
    /// without any TypeSafe call.
    pub fn set_jev_judge(&mut self, judge: Option<std::sync::Arc<dyn crate::context::JevJudge>>) {
        self.jev_economy = economy::JevEconomy::default();
        self.jev_judge = judge;
    }

    pub fn set_compaction_input_cost_micros_per_million(&mut self, price: Option<u64>) {
        self.compaction_input_cost_micros_per_million = price;
    }

    pub fn set_compaction_pricing(&mut self, pricing: Option<CompactionPricing>) {
        self.compaction_pricing = pricing;
    }

    /// Presentation only: internal read, digest and write preconditions are unchanged.
    pub fn set_read_presentation_bytes(&mut self, bytes: usize) -> Result<(), String> {
        if !(1024..=64 * 1024).contains(&bytes) {
            return Err("read presentation bytes must be between 1024 and 65536".into());
        }
        self.read_presentation_bytes = bytes;
        Ok(())
    }

    fn retain_interrupted_turn(
        &mut self,
        start: usize,
        max_bytes: usize,
    ) -> Result<(), ProviderError> {
        let events = &self.app.events()[start..];
        let mut text = events
            .iter()
            .filter_map(|event| match &event.kind {
                crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for event in events {
            let crate::EventKind::ToolStarted {
                batch_id,
                call_id,
                name,
                arguments,
            } = &event.kind
            else {
                continue;
            };
            let finished = events.iter().any(|event| {
                matches!(&event.kind,
                crate::EventKind::ToolFinished { batch_id: batch, call_id: id, .. }
                    if batch == batch_id && id == call_id)
            });
            let output = events.iter().find_map(|event| match &event.kind {
                crate::EventKind::ToolOutput {
                    batch_id: batch,
                    call_id: id,
                    output,
                    ..
                } if batch == batch_id && id == call_id => Some(output),
                _ => None,
            });
            if let Some(output) = output.filter(|_| finished) {
                calls.push(ProviderToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                let projection = self
                    .presentation_sources
                    .get(&(batch_id.clone(), call_id.clone()))
                    .map(|source| source.present(PresentationBudget { max_bytes }))
                    .unwrap_or_else(|| {
                        present_unstructured(name, output, PresentationBudget { max_bytes })
                    });
                // Artifact creation can be the interruption itself. Preserve
                // a clearly labelled fragment when an atomic record cannot
                // fit, without advancing its continuation or claiming it is
                // suitable for a patch precondition.
                let content = if !projection.complete && projection.delivered_records == 0 {
                    format!(
                        "{}\n[Interrupted output preview; not safe for patch.expected]\n{}",
                        projection.text,
                        truncate_result(output, max_bytes)
                    )
                } else {
                    projection.text
                };
                results.push(ProviderMessage::tool(name, call_id, content));
            } else {
                text.push_str(&format!("\nTool {name} ({call_id}) started without a confirmed result; its effects are unknown. Do not replay it automatically."));
            }
        }
        if text.trim().is_empty() && calls.is_empty() {
            return Ok(());
        }
        text.insert_str(0, "[Interrupted turn]\n");
        let mut messages = std::mem::take(&mut self.conversation);
        let mut appended = self
            .append_conversation_message(&mut messages, ProviderMessage::assistant(text, calls));
        for result in results {
            if appended.is_err() {
                break;
            }
            appended = self.append_conversation_message(&mut messages, result);
        }
        self.conversation = messages;
        appended
    }

    pub fn set_background_compaction_enabled(&mut self, enabled: bool) {
        self.background_compaction_enabled = enabled;
    }

    pub fn capture_turn_transcript(&mut self) {
        self.turn_transcript = Some(Vec::new());
    }

    /// Generated messages independent of compaction; excludes historical input.
    pub fn take_turn_transcript(&mut self) -> Vec<ProviderMessage> {
        self.turn_transcript.take().unwrap_or_default()
    }

    fn append_conversation_message(
        &mut self,
        messages: &mut Vec<ProviderMessage>,
        message: ProviderMessage,
    ) -> Result<(), ProviderError> {
        let message = self.redact_message(message);
        if let Some(journal) = &self.app.run_journal {
            journal
                .lock()
                .map_err(|_| journal_error("durable run lock poisoned"))?
                .record_message(message.clone())
                .map_err(journal_error)?;
        }
        if let Some(transcript) = self.turn_transcript.as_mut() {
            let mut persisted = message.clone();
            persisted.responses_reasoning.clear();
            persisted.chat_reasoning = None;
            transcript.push(persisted);
        }
        messages.push(message);
        Ok(())
    }

    /// Installs the semantic code intelligence facade used by the code_intel
    /// tool. Optional: without it the tool reports "unavailable" with a clear
    /// message instead of failing hard.
    pub fn set_code_intelligence(&mut self, code_intel: Arc<dyn CodeIntelligence>) {
        self.code_intel = Some(code_intel);
    }

    /// Installs the shared MCP manager. The `mcp` meta-tool is advertised in
    /// Auto mode only when a manager with enabled servers is installed.
    pub fn set_mcp_manager(&mut self, mcp: Option<Arc<McpManager>>) {
        self.mcp = mcp;
    }

    /// Canonical provider conversation after any compaction and completed
    /// tool turns. Interactive callers persist this instead of rebuilding a
    /// lossy transcript from rendered output.
    pub fn finalization_error(&self) -> Option<&ProviderError> {
        self.finalization_error.as_ref()
    }

    pub fn conversation(&self) -> &[ProviderMessage] {
        &self.conversation
    }

    fn tool_definition_set(
        &self,
        mode: crate::OperatingMode,
        code_intel_enabled: bool,
    ) -> Arc<[Value]> {
        let key = ToolDefinitionSetKey {
            mode,
            artifact_enabled: self.artifact_store.is_some(),
            skills_enabled: self
                .skill_discovery_cache
                .as_ref()
                .and_then(|(_, discovery)| discovery.as_ref())
                .is_none_or(|discovery| {
                    !discovery.active_entries().is_empty() || !discovery.warnings.is_empty()
                }),
            code_intel_enabled,
            interaction_enabled: self.interaction_route.is_some()
                && mode != crate::OperatingMode::Plan,
            mcp_enabled: self
                .mcp
                .as_ref()
                .is_some_and(|manager| manager.has_enabled_servers()),
        };
        let mut sets = TOOL_DEFINITION_SETS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(definitions) = sets.get(&key) {
            return Arc::clone(definitions);
        }
        let mut tools = self
            .tools
            .definitions_for_mode_shared(mode)
            .as_ref()
            .to_vec();
        crate::tools::compact_provider_definitions(&mut tools);
        if !code_intel_enabled {
            tools.retain(|definition| {
                definition
                    .get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|name| name != "code_intel")
            });
        }
        if key.artifact_enabled {
            tools.push(artifact_read_definition());
        }
        if key.interaction_enabled {
            tools.push(ask_question_definition());
        }
        if mode.allows_mutation() {
            tools.push(shell_jobs::definition());
            tools.push(todo_tool_definition());
            if key.skills_enabled {
                tools.push(skill_tool_definition());
            }
            if key.mcp_enabled {
                tools.push(mcp_tool_definition());
            }
        }
        let definitions: Arc<[Value]> = tools.into();
        sets.insert(key, Arc::clone(&definitions));
        definitions
    }

    fn provider_tool_definitions(&self, mode: crate::OperatingMode) -> Arc<[Value]> {
        self.tool_definition_set(mode, self.code_intel.is_some())
    }

    /// Tool schemas advertised to the provider for a loop in `mode`.
    pub fn advertised_tool_definitions(&self, mode: crate::OperatingMode) -> Vec<Value> {
        self.provider_tool_definitions(mode).as_ref().to_vec()
    }

    fn workspace_tool_definitions(&self, mode: crate::OperatingMode, cwd: &Path) -> Arc<[Value]> {
        let code_intel_enabled = self
            .code_intel
            .as_ref()
            .is_some_and(|backend| backend.supports_workspace(cwd));
        self.tool_definition_set(mode, code_intel_enabled)
    }

    pub fn tools_for_mode(&self, mode: crate::OperatingMode) -> Vec<&'static str> {
        self.tools.names_for_mode(mode)
    }

    /// Registers an exact value that must not cross a runtime boundary.
    ///
    /// Runtime diagnostics intentionally omit the storage so they can never
    /// print the secret itself.
    pub fn register_sensitive_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() && !self.sensitive_values.0.iter().any(|item| item == &value) {
            self.sensitive_values.0.push(value);
            self.sensitive_values
                .0
                .sort_by_key(|value| std::cmp::Reverse(value.len()));
        }
    }

    /// Replaces every exact registered sensitive value in `input`.
    pub fn redact_sensitive(&self, input: &str) -> String {
        redact_values(&self.sensitive_values.0, input)
    }

    /// The display diff quotes file lines, so it is redacted like output.
    fn redacted_edit_diff(
        &self,
        diff: Option<&crate::ToolEditDiff>,
    ) -> Option<crate::ToolEditDiff> {
        let redact_lines = |lines: &[String]| {
            lines
                .iter()
                .map(|line| self.redact_sensitive(line))
                .collect::<Vec<_>>()
        };
        diff.map(|diff| crate::ToolEditDiff {
            path: self.redact_sensitive(&diff.path),
            hunks: diff
                .hunks
                .iter()
                .map(|hunk| crate::ToolEditHunk {
                    start_line: hunk.start_line,
                    removed: redact_lines(&hunk.removed),
                    added: redact_lines(&hunk.added),
                })
                .collect(),
            truncated: diff.truncated,
        })
    }

    fn redact_provider_error(&self, error: ProviderError) -> ProviderError {
        crate::provider::redact_provider_error_values(error, &self.sensitive_values.0)
    }

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
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let result = self
            .run_provider_messages_with_tools(client, messages, &[], next_seq, false)
            .await;
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let result = result?;
        if result.stop != ProviderTurnStop::Normal {
            return Err(ProviderError::InvalidResponse {
                message: result.stop.error_message().into(),
            });
        }
        Ok(result.next_seq)
    }

    async fn run_provider_messages_with_tools<A: ProviderAdapter>(
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
            client.prepare_finalization_messages(messages)?
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

    async fn run_provider_messages_with_tools_after_snapshot<A: ProviderAdapter>(
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
        let cancellation = self.cancellation.clone();
        let cancellation = async move {
            match cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
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
                    message: "provider completed without assistant text or tool calls".into(),
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

    /// Budget gate for the closing call: preflights a tool-free request over
    /// `messages` the same way loop turns are checked. An adapter without a
    /// structural envelope bound cannot be gated and proceeds as before.
    fn finalization_fits_budget<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        config: &AgentLoopConfig,
    ) -> bool {
        let Some(serialized_chars) =
            estimate_unprepared_request_chars(client.adapter(), messages, &[], None)
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
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        // Bound skill-discovery memoization to one entry-point call, mirroring
        // the reset in `prepare_loop_capabilities` for the agent loop.
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
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let provider_turn = provider_result?;
        if provider_turn.stop != ProviderTurnStop::Normal {
            return Err(ProviderError::InvalidResponse {
                message: provider_turn.stop.error_message().into(),
            });
        }
        let blocks_tools = provider_turn.blocks_tools;
        let mut calls = tool_calls_since(&self.app, event_start);
        if blocks_tools {
            calls.clear();
        }
        let batch_id = format!("slim-batch-direct-{}", provider_turn.next_seq);
        assign_missing_call_ids(&mut calls, &batch_id);
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
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
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
        self.tools
            .configure_artifacts(self.artifact_store.clone(), &self.sensitive_values.0);
        self.shell_jobs = shell_jobs::ShellJobs::default();
        self.uncommitted_event_start = None;
        self.finalization_error = None;
        if let Some(handle) = &self.compaction_handle {
            // Commits belong to this run; the summary and generation survive.
            drop(handle.take_commits());
        }
        if let Some(journal) = &self.app.run_journal {
            journal
                .lock()
                .map_err(|_| journal_error("durable run lock poisoned"))?
                .configure_output(self.artifact_store.clone(), config.max_result_bytes);
        }
        let _job_scope = self.shell_jobs.scope();
        // The loop owns the only copy of the history while it runs and hands it
        // back on every exit path; a run that stops before seeding keeps it.
        let mut messages = std::mem::take(&mut self.conversation);
        let mut result = self
            .run_agent_loop_inner(
                client,
                initial_messages,
                mode,
                cwd,
                next_seq,
                config,
                &mut messages,
            )
            .await;
        self.conversation = messages;
        self.shell_jobs.shutdown().await;
        if let Some(start) = self.uncommitted_event_start.take() {
            self.retain_interrupted_turn(start, config.max_result_bytes)?;
        }
        let mut completion_seq =
            self.observed_next_seq(result.as_ref().map_or(next_seq, |r| r.next_seq));
        let mut messages = std::mem::take(&mut self.conversation);
        let delivered = self.deliver_shell_completions(
            &mut messages,
            config.max_result_bytes,
            &mut completion_seq,
        );
        self.conversation = messages;
        delivered?;
        if let Ok(result) = &mut result {
            result.next_seq = completion_seq;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_agent_loop_inner<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        initial_messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        mut config: AgentLoopConfig,
        messages: &mut Vec<ProviderMessage>,
    ) -> Result<AgentLoopResult, ProviderError> {
        if self.is_cancelled() {
            return Ok(AgentLoopResult {
                next_seq,
                turns: 0,
                stop: AgentLoopStop::Cancelled,
                tool_results: Vec::new(),
                usage: UsageTotals::default(),
            });
        }
        if config.max_turns == 0 {
            return Ok(AgentLoopResult {
                next_seq,
                turns: 0,
                stop: AgentLoopStop::TurnLimit,
                tool_results: Vec::new(),
                usage: UsageTotals::default(),
            });
        }
        let loop_event_start = self.app.events().len();
        let run_start_seq = next_seq;
        let cwd = cwd.as_ref();
        self.prepare_loop_capabilities(cwd)?;
        let mut seeded = self.redact_messages(initial_messages);
        self.add_initial_workspace_context(client.adapter(), &mut seeded, mode, cwd, config);
        for message in &mut seeded {
            message
                .responses_reasoning
                .retain(|state| state.belongs_to(client.adapter()));
            if message
                .chat_reasoning
                .as_ref()
                .is_some_and(|state| !state.belongs_to(client.adapter()))
            {
                message.chat_reasoning = None;
            }
        }
        let seed_elision = elide_superseded_tool_outputs(&mut seeded);
        *messages = seeded;
        let mut next_seq = next_seq;
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
        let mut all_results = Vec::new();
        let mut reserved_tool_slots = 0usize;
        let mut guard = LoopGuard::default();
        let mut governor = CausalGovernor::default();
        let mut turns = 0;
        let mut stop = AgentLoopStop::TurnLimit;
        let mut budget_steers_used = 0usize;
        let mut todo_cadence = TodoCadence::default();
        let mut compaction_applied = false;
        let mut recovery = RecoveryBudget::new(config.context_reserve_tokens);
        let mut pending_background = None;
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();

        // Transport retries resend the current turn: they are bounded by their
        // own recovery budgets and do not advance `turn`.
        let mut turn = 0;
        let mut announced_turn = 0;
        while turn < config.max_turns {
            if self.is_cancelled() {
                drop(
                    self.cancel_pending_background(
                        &mut pending_background,
                        &mut next_seq,
                        "agent_loop_cancelled",
                    )
                    .await?,
                );
                return Ok(AgentLoopResult {
                    next_seq,
                    turns,
                    stop: AgentLoopStop::Cancelled,
                    tool_results: all_results,
                    usage: usage_since(&self.app, loop_event_start),
                });
            }
            self.deliver_shell_completions(messages, config.max_result_bytes, &mut next_seq)?;
            turns = turn + 1;
            if turn > announced_turn {
                announced_turn = turn;
                if let Some(handle) = &self.compaction_handle {
                    handle.completed_turn();
                }
            }
            // Skills stay fixed for the run. A newly available code backend
            // still needs to be advertised after a project marker is created.
            let tools = self.workspace_tool_definitions(mode, cwd);
            let (preflight_chars, preflight_tokens, mut serialized_request) = {
                let overlay = self.overlay_channel(messages, mode);
                let structural_preflight_chars = estimate_unprepared_request_chars(
                    client.adapter(),
                    overlay.view(),
                    tools.as_ref(),
                    None,
                );
                match structural_preflight_chars {
                    Some(preflight_chars) => (
                        preflight_chars,
                        self.token_estimator
                            .estimate(provider, model, preflight_chars),
                        None,
                    ),
                    None => {
                        let mut request =
                            client.prepare_messages_with_tools(overlay.view(), tools.as_ref())?;
                        let preflight_tokens = self.token_estimator.estimate(
                            provider,
                            model,
                            request.serialized_chars,
                        );
                        request.estimated_tokens = preflight_tokens;
                        (request.serialized_chars, preflight_tokens, Some(request))
                    }
                }
            };
            let compaction_policy = self
                .compaction_handle
                .as_ref()
                .map(CompactionHandle::policy)
                .unwrap_or_default();
            drop(
                self.finish_background_if_ready(
                    &mut pending_background,
                    &compaction_policy,
                    &mut next_seq,
                )
                .await?,
            );
            let has_compactable = has_compactable_history(messages);
            let manual_compaction = self
                .compaction_handle
                .as_ref()
                .and_then(CompactionHandle::manual_instructions)
                .is_some();
            let prepared_overflow_retry = recovery.overflow_retry_used
                && self.compaction_handle.as_ref().is_some_and(|handle| {
                    handle.status() == crate::context::CompactionStatus::Ready
                });
            let over_hard = compaction_policy.is_over_hard(
                preflight_tokens,
                config.context_window_tokens,
                config.context_reserve_tokens,
            );
            let over_soft =
                compaction_policy.is_over_soft(preflight_tokens, config.context_window_tokens);
            let prepared_ready = self
                .compaction_handle
                .as_ref()
                .is_some_and(|handle| handle.status() == crate::context::CompactionStatus::Ready);
            let should_compact = config.context_compaction_enabled
                && compaction_policy.enabled
                && (manual_compaction
                    || prepared_overflow_retry
                    || over_hard
                    || (over_soft && prepared_ready))
                && has_compactable;
            if should_compact {
                let trigger = CompactionTrigger {
                    preflight_tokens,
                    manual: manual_compaction,
                    over_hard,
                };
                match self
                    .compact_for_turn(
                        client,
                        messages,
                        initial_messages,
                        cwd,
                        &tools,
                        mode,
                        trigger,
                        &config,
                        &compaction_policy,
                        &mut governor,
                        run_start_seq,
                        &mut recovery,
                        &mut pending_background,
                        &mut next_seq,
                    )
                    .await?
                {
                    CompactionOutcome::Applied(request) => {
                        serialized_request = Some(*request);
                        compaction_applied = true;
                        guard = LoopGuard::default();
                    }
                    CompactionOutcome::Skipped => {}
                    CompactionOutcome::Cancelled => {
                        return Ok(cancelled_agent_loop_result(
                            next_seq,
                            turns,
                            all_results,
                            Vec::new(),
                            usage_since(&self.app, loop_event_start),
                        ));
                    }
                }
            }
            let mut serialized_request = match serialized_request {
                Some(request) => request,
                None => self.prepare_loop_request(client, messages, &tools, mode)?,
            };
            if let Some(limit) = recovery.recovery_output_limit {
                serialized_request =
                    client.with_recovery_output_limit(serialized_request, limit)?;
            }
            let current_output_limit = serialized_request.output_token_limit();
            let serialized_chars = serialized_request.serialized_chars;
            if !compaction_applied && recovery.recovery_output_limit.is_none() {
                debug_assert!(serialized_chars <= preflight_chars);
            }
            let ProviderRequestComponents {
                system_bytes,
                tool_schema_bytes,
                history_bytes,
                tool_result_bytes,
            } = serialized_request.components;
            let estimated_tokens = self
                .token_estimator
                .estimate(provider, model, serialized_chars);
            serialized_request.estimated_tokens = estimated_tokens;
            let prepared_budget = ContextBudget::new(
                config.context_window_tokens,
                estimated_tokens,
                config.context_reserve_tokens,
            );
            if !prepared_budget.can_fit(config.context_reserve_tokens) {
                drop(
                    self.cancel_pending_background(
                        &mut pending_background,
                        &mut next_seq,
                        "context_window_exceeded",
                    )
                    .await?,
                );
                return Err(ProviderError::InvalidResponse {
                    message: if compaction_applied {
                        "context window still exceeded after compaction".into()
                    } else {
                        "context window exceeded and compaction is unavailable".into()
                    },
                });
            }
            let remaining_model_turns = config.max_turns.saturating_sub(turn + 1);
            let mut background_plan = if pending_background.is_none() {
                self.build_background_compaction_plan(
                    client,
                    messages,
                    &compaction_policy,
                    ContextBudget::new(
                        config.context_window_tokens,
                        estimated_tokens,
                        config.context_reserve_tokens,
                    ),
                    should_compact,
                    remaining_model_turns,
                )
                .await
            } else {
                None
            };
            let event_start = self.app.events().len();
            self.uncommitted_event_start = Some(event_start);
            let request_next_seq = checked_next_seq(next_seq)?;
            checked_next_seq(request_next_seq)?;
            let snapshot = crate::SessionEvent::new(
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
                    context_window_tokens: config.context_window_tokens,
                },
            );
            self.app
                .push_event(snapshot)
                .map_err(|message| ProviderError::InvalidResponse {
                    message: message.into(),
                })?;
            next_seq = request_next_seq;
            if self.is_cancelled() {
                push_runtime_event(
                    &mut self.app,
                    &mut next_seq,
                    crate::EventKind::RequestCompleted {
                        provider_latency_ms: 0,
                        cancelled: true,
                        failed: true,
                    },
                )?;
                drop(
                    self.cancel_pending_background(
                        &mut pending_background,
                        &mut next_seq,
                        "agent_loop_cancelled",
                    )
                    .await?,
                );
                return Ok(AgentLoopResult {
                    next_seq,
                    turns,
                    stop: AgentLoopStop::Cancelled,
                    tool_results: all_results,
                    usage: usage_since(&self.app, loop_event_start),
                });
            }
            let provider_result = self
                .run_provider_messages_with_tools_after_snapshot(
                    client,
                    messages_are_text_only(messages),
                    serialized_request,
                    next_seq,
                    true,
                )
                .await;
            recovery.provider_attempts = recovery.provider_attempts.saturating_add(1);
            next_seq = provider_result
                .as_ref()
                .map_or_else(|_| self.observed_next_seq(next_seq), |turn| turn.next_seq);
            drop(
                self.finish_background_if_ready(
                    &mut pending_background,
                    &compaction_policy,
                    &mut next_seq,
                )
                .await?,
            );
            if self.is_cancelled() {
                drop(
                    self.cancel_pending_background(
                        &mut pending_background,
                        &mut next_seq,
                        "agent_loop_cancelled",
                    )
                    .await?,
                );
                return Ok(AgentLoopResult {
                    next_seq,
                    turns,
                    stop: AgentLoopStop::Cancelled,
                    tool_results: all_results,
                    usage: usage_since(&self.app, loop_event_start),
                });
            }
            let provider_turn = match self
                .settle_provider_attempt(
                    provider_result,
                    &mut recovery,
                    &mut pending_background,
                    messages,
                    &mut config,
                    event_start,
                    has_compactable,
                    turn,
                    &mut next_seq,
                )
                .await?
            {
                ProviderAttempt::Completed(provider_turn) => provider_turn,
                ProviderAttempt::Retry => continue,
                ProviderAttempt::NextTurn => {
                    turn += 1;
                    continue;
                }
            };
            let blocks_tools = provider_turn.blocks_tools;
            let mut calls = tool_calls_since(&self.app, event_start);
            if blocks_tools {
                calls.clear();
            }
            if calls.is_empty() {
                let assistant_text = self
                    .app
                    .events()
                    .get(event_start..)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|event| match &event.kind {
                        crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                let mut assistant = ProviderMessage::assistant(assistant_text, Vec::new());
                assistant.responses_reasoning = provider_turn.responses_reasoning;
                assistant.chat_reasoning = provider_turn.chat_reasoning;
                self.append_conversation_message(messages, assistant)?;
                self.uncommitted_event_start = None;
                // An idle agent waits on local job events, not more model calls.
                // Completed output resumes the loop as an explicit harness message.
                if provider_turn.stop == ProviderTurnStop::Normal && self.shell_jobs.running() {
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
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
                            &mut next_seq,
                            crate::EventKind::ProviderPhase {
                                phase: ProviderPhase::PreparingTool,
                                elapsed_ms: 0,
                                detail: Some(detail),
                            },
                        )?;
                        self.shell_jobs.wait().await;
                        if self.deliver_shell_completions(
                            messages,
                            config.max_result_bytes,
                            &mut next_seq,
                        )? {
                            break;
                        }
                    }
                    self.deliver_shell_completions(
                        messages,
                        config.max_result_bytes,
                        &mut next_seq,
                    )?;
                    turn += 1;
                    continue;
                }
                if self.deliver_shell_completions(
                    messages,
                    config.max_result_bytes,
                    &mut next_seq,
                )? {
                    turn += 1;
                    continue;
                }
                // Truncated tools are never executed. Preserve the partial answer
                // and completed tool results, then ask for a small next step.
                // Count these requests against the normal turn limit as well.
                if provider_turn.stop == ProviderTurnStop::Truncated
                    && recovery.truncation_recoveries < 2
                    && turn + 1 < config.max_turns
                {
                    recovery.truncation_recoveries += 1;
                    if let Some(limit) = current_output_limit.and_then(|current| {
                        client.next_recovery_output_limit(current, config.context_window_tokens)
                    }) {
                        recovery.recovery_output_limit = Some(limit);
                        config.context_reserve_tokens = config.context_reserve_tokens.max(limit);
                    }
                    self.append_conversation_message(messages, ProviderMessage::user(
                        "The previous response reached its output limit. Continue from the preserved progress with one small next step or a concise answer. Do not repeat completed actions. Tool calls from the truncated response were not executed; reissue any needed call with complete arguments."
                    ))?;
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::ProviderPhase {
                            phase: ProviderPhase::Connecting,
                            elapsed_ms: 0,
                            detail: Some(format!(
                                "Recovering truncated response ({truncation_recoveries}/2); output limit {}",
                                recovery.recovery_output_limit.or(current_output_limit)
                                    .map_or_else(|| "provider default".into(), |limit| limit.to_string()),
                                truncation_recoveries = recovery.truncation_recoveries,
                            )),
                        },
                    )?;
                    turn += 1;
                    continue;
                }
                if provider_turn.stop == ProviderTurnStop::Normal
                    && mode == crate::OperatingMode::Auto
                    && turn + 1 < config.max_turns
                    && todo_cadence.before_final(&self.todo_items())
                {
                    self.append_conversation_message(
                        messages,
                        ProviderMessage::user(TODO_FINAL_REVIEW),
                    )?;
                    turn += 1;
                    continue;
                }
                stop = match provider_turn.stop {
                    ProviderTurnStop::Normal => AgentLoopStop::ProviderCompleted,
                    ProviderTurnStop::Truncated => AgentLoopStop::ProviderTruncated,
                    ProviderTurnStop::Filtered => AgentLoopStop::ProviderFiltered,
                };
                break;
            }

            let (budget_cut, suppressed) =
                split_calls_for_budget(&mut calls, config, reserved_tool_slots);
            // Reserve both native actions before executing a fused call. A failed
            // edit still consumes its reserved shell slot for this run.
            reserved_tool_slots = reserved_tool_slots
                .saturating_add(calls.iter().map(tool_call_slots).sum::<usize>());
            let suppressed_calls = budget_cut.suppressed;
            if suppressed_calls > 0 {
                push_runtime_event(
                    &mut self.app,
                    &mut next_seq,
                    crate::EventKind::ToolCallsSuppressed {
                        count: suppressed_calls as u64,
                    },
                )?;
            }

            if let Some(plan) = background_plan.take().filter(|_| suppressed_calls == 0) {
                if plan.profitable {
                    if let Some(handle) = &self.compaction_handle {
                        handle.mark_preparing();
                    }
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::CompactionAttemptStarted {
                            provider: plan.provider.clone(),
                            model: plan.model.clone(),
                            system_bytes: plan.system_bytes,
                            history_bytes: plan.history_bytes,
                            serialized_chars: plan.serialized_chars,
                            request_bytes: plan.request_bytes,
                            estimated_input_tokens: plan.estimated_input_tokens,
                        },
                    )?;
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::CompactionState {
                            state: crate::context::CompactionStatus::Preparing,
                            reason: crate::context::CompactionReason::SoftThreshold,
                            tokens_before: plan.tokens_before,
                            tokens_after: 0,
                            duration_ms: 0,
                        },
                    )?;
                    let progress = Arc::new(Mutex::new(CompactionAttemptProgress::default()));
                    let task_progress = Arc::clone(&progress);
                    let usage_request = RequestUsage {
                        request_kind: crate::RequestKind::Compaction,
                        provider: plan.provider.clone(),
                        model: plan.model.clone(),
                        system_bytes: plan.system_bytes,
                        history_bytes: plan.history_bytes,
                        estimated_input_tokens: plan.estimated_input_tokens,
                        ..RequestUsage::default()
                    };
                    let request_bytes = plan.request_bytes;
                    let estimated_input_tokens = plan.estimated_input_tokens;
                    let tokens_before = plan.tokens_before;
                    let cancellation = CancellationToken::new();
                    let task_cancellation = cancellation.clone();
                    let background_client = (*client).clone();
                    let jev_judge = self.jev_judge.clone();
                    let token_estimator = self.token_estimator.clone();
                    let jev_outcome = Arc::new(Mutex::new(None));
                    let task_outcome = Arc::clone(&jev_outcome);
                    let provider_call_journal = self.app.run_journal.clone();
                    let task = tokio::spawn(async move {
                        run_background_compaction(
                            background_client,
                            plan,
                            jev_judge,
                            token_estimator,
                            Some(task_cancellation),
                            BackgroundCompactionObservers {
                                progress: task_progress,
                                jev_outcome: task_outcome,
                                provider_call_journal,
                            },
                        )
                        .await
                    });
                    pending_background = Some(PendingBackgroundCompaction {
                        task,
                        cancellation,
                        progress,
                        jev_outcome,
                        usage_request,
                        request_bytes,
                        estimated_input_tokens,
                        tokens_before,
                        started: Instant::now(),
                    });
                } else {
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::CompactionSkippedBelowBreakEven {
                            projected_savings_tokens: plan.projected_savings_tokens,
                            estimated_cost_tokens: plan.estimated_cost_tokens,
                            safety_margin_tokens: plan.safety_margin_tokens,
                            future_turns: plan.future_turns,
                        },
                    )?;
                }
            }

            let batch_id = format!("slim-batch-{turn}-{next_seq}");
            assign_missing_call_ids(&mut calls, &batch_id);
            if let Some(journal) = self.app.run_journal.as_ref().filter(|_| !calls.is_empty()) {
                let text = self.app.events()[event_start..]
                    .iter()
                    .filter_map(|event| {
                        if let crate::EventKind::AssistantTextDelta { text } = &event.kind {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<String>();
                let assistant =
                    self.redact_message(ProviderMessage::assistant(text, calls.clone()));
                journal
                    .lock()
                    .map_err(|_| journal_error("durable run lock poisoned"))?
                    .begin_tools(&batch_id, assistant, &calls)
                    .map_err(journal_error)?;
            }
            let (mut results, following_seq) = match self
                .execute_provider_tool_batch(mode, cwd, &batch_id, &calls, next_seq, &mut governor)
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    drop(
                        self.cancel_pending_background(
                            &mut pending_background,
                            &mut next_seq,
                            "tool_execution_error",
                        )
                        .await,
                    );
                    return Err(error);
                }
            };
            next_seq = following_seq;
            let mut reacquisitions = governor.take_post_compaction_reacquisitions();
            if self.is_cancelled() {
                drop(
                    self.cancel_pending_background(
                        &mut pending_background,
                        &mut next_seq,
                        "agent_loop_cancelled",
                    )
                    .await?,
                );
                return Ok(cancelled_agent_loop_result(
                    next_seq,
                    turns,
                    all_results,
                    results,
                    usage_since(&self.app, loop_event_start),
                ));
            }
            next_seq = match self
                .materialize_results(&mut results, config.max_result_bytes, None, next_seq)
                .await
            {
                Ok(following_seq) => following_seq,
                Err(error) => {
                    drop(
                        self.cancel_pending_background(
                            &mut pending_background,
                            &mut next_seq,
                            "tool_materialization_error",
                        )
                        .await?,
                    );
                    return Err(error);
                }
            };
            drop(
                self.finish_background_if_ready(
                    &mut pending_background,
                    &compaction_policy,
                    &mut next_seq,
                )
                .await?,
            );
            let assistant_text = self
                .app
                .events()
                .get(event_start..)
                .unwrap_or_default()
                .iter()
                .filter_map(|event| match &event.kind {
                    crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            let mut assistant = ProviderMessage::assistant(assistant_text, calls.clone());
            assistant.responses_reasoning = provider_turn.responses_reasoning;
            assistant.chat_reasoning = provider_turn.chat_reasoning;
            self.append_conversation_message(messages, assistant)?;
            let mut presentations = self.plan_tool_presentations(
                client,
                messages,
                tools.as_ref(),
                mode,
                cwd,
                &batch_id,
                &calls,
                &results,
                config,
            );
            let force_artifacts = results
                .iter()
                .zip(&presentations)
                .map(|(result, presentation)| result.artifact.is_none() && !presentation.complete)
                .collect::<Vec<_>>();
            if force_artifacts.iter().any(|forced| *forced) {
                next_seq = match self
                    .materialize_results(
                        &mut results,
                        config.max_result_bytes,
                        Some(&force_artifacts),
                        next_seq,
                    )
                    .await
                {
                    Ok(following_seq) => following_seq,
                    Err(error) => {
                        drop(
                            self.cancel_pending_background(
                                &mut pending_background,
                                &mut next_seq,
                                "tool_materialization_error",
                            )
                            .await?,
                        );
                        return Err(error);
                    }
                };
                presentations = self.plan_tool_presentations(
                    client,
                    messages,
                    tools.as_ref(),
                    mode,
                    cwd,
                    &batch_id,
                    &calls,
                    &results,
                    config,
                );
            }
            let repeated_failure_in_batch = self
                .record_batch_results(
                    &calls,
                    &results,
                    &presentations,
                    &mut reacquisitions,
                    &mut guard,
                    &mut pending_background,
                    messages,
                    event_start,
                    &mut next_seq,
                )
                .await?;
            self.uncommitted_event_start = None;
            if mode == crate::OperatingMode::Auto
                && turn + 1 < config.max_turns
                && todo_cadence.after_batch(&self.todo_items())
            {
                self.append_conversation_message(
                    messages,
                    ProviderMessage::user(TODO_PROGRESS_REVIEW),
                )?;
            }
            all_results.extend(results);
            if suppressed_calls > 0 {
                if should_stop_after_tool_budget_cut(budget_cut, turn, config.max_turns) {
                    stop = AgentLoopStop::ToolLimit;
                    break;
                }
                self.append_conversation_message(
                    messages,
                    ProviderMessage::user(suppressed_calls_steer(&suppressed)),
                )?;
            }
            if repeated_failure_in_batch {
                stop = AgentLoopStop::RepeatedFailedTool;
                push_runtime_event(
                    &mut self.app,
                    &mut next_seq,
                    crate::EventKind::TerminalError {
                        message: "repeated failed tool call blocked".into(),
                    },
                )?;
                break;
            }
            if governor.stop_requested() {
                stop = AgentLoopStop::NoProgress;
                self.app.discard_projected_payloads();
                break;
            }
            let causal_steer = self.app.events()[event_start..].iter().find_map(|event| {
                if let crate::EventKind::CausalAnomalyDetected {
                    tool_name,
                    call_id,
                    confidence: crate::CausalConfidence::High,
                    action: crate::CausalShadowAction::WouldReuse | crate::CausalShadowAction::WouldWarn,
                    ..
                } = &event.kind {
                    Some(format!("Tool {tool_name} call {call_id} repeated evidence without a relevant state change. Use the existing result or change the approach. Retry only after the dependency changes or if the needed content is no longer available; otherwise finish with the known outcome and blocker."))
                } else {
                    None
                }
            });
            if let Some(steer) = causal_steer.filter(|_| {
                budget_steers_used < MAX_BUDGET_STEERS
                    && !self.is_cancelled()
                    && turn + 1 < config.max_turns
            }) {
                budget_steers_used += 1;
                self.append_conversation_message(messages, ProviderMessage::user(steer))?;
            }
            if turn + 1 == config.max_turns {
                stop = AgentLoopStop::TurnLimit;
            }
            turn += 1;
            self.app.discard_projected_payloads();
        }

        let final_compaction_policy = self
            .compaction_handle
            .as_ref()
            .map(CompactionHandle::policy)
            .unwrap_or_default();
        drop(
            self.finish_background_if_ready(
                &mut pending_background,
                &final_compaction_policy,
                &mut next_seq,
            )
            .await?,
        );
        drop(
            self.cancel_pending_background(
                &mut pending_background,
                &mut next_seq,
                "agent_loop_completed",
            )
            .await?,
        );

        // A terminal budget/filter/stop must not leave hidden native work alive.
        self.shell_jobs.shutdown().await;
        self.deliver_shell_completions(messages, config.max_result_bytes, &mut next_seq)?;

        if matches!(
            stop,
            AgentLoopStop::TurnLimit
                | AgentLoopStop::ToolLimit
                | AgentLoopStop::NoProgress
                | AgentLoopStop::RepeatedFailedTool
        ) && !self.is_cancelled()
        {
            self.request_final_answer(
                client,
                messages,
                stop,
                &config,
                &final_compaction_policy,
                &governor,
                run_start_seq,
                initial_messages,
                cwd,
                &mut next_seq,
            )
            .await?;
        }

        if self.is_cancelled() {
            stop = AgentLoopStop::Cancelled;
        }
        if stop == AgentLoopStop::ProviderCompleted {
            let verified = governor.validations_satisfied()
                && runtime_goal_assurance(&self.app.events()[loop_event_start..]);
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::GoalAssurance { verified },
            )?;
        }
        Ok(AgentLoopResult {
            next_seq,
            turns,
            stop,
            tool_results: all_results,
            usage: usage_since(&self.app, loop_event_start),
        })
    }

    /// Appends one tool message per executed call, reports reused evidence
    /// and folds each outcome into the loop guard. Returns whether a repeated
    /// failed call must stop the run.
    #[allow(clippy::too_many_arguments)]
    async fn record_batch_results(
        &mut self,
        calls: &[ProviderToolCall],
        results: &[ToolResult],
        presentations: &[ToolPresentation],
        reacquisitions: &mut std::collections::HashSet<String>,
        guard: &mut LoopGuard,
        pending_background: &mut Option<PendingBackgroundCompaction>,
        messages: &mut Vec<ProviderMessage>,
        event_start: usize,
        next_seq: &mut u64,
    ) -> Result<bool, ProviderError> {
        let mut repeated_failure_in_batch = false;
        let mut mutation_succeeded = false;
        for ((call, result), presentation) in
            calls.iter().zip(results.iter()).zip(presentations.iter())
        {
            let mutation_changed = matches!(call.name.as_str(), "write" | "patch")
                && self.app.events()[event_start..].iter().any(|event| {
                    matches!(&event.kind, crate::EventKind::CausalProgressObserved {
                        call_id, kind: crate::CausalProgressKind::WorkspaceChanged, ..
                    } if call_id.as_ref() == call.id)
                });
            let full_output = presentation.text.clone();
            let tool_name = self.redact_sensitive(&call.name);
            let duplicate_pointer = format!(
                "[duplicate {} result omitted; identical output already in context]",
                tool_name
            );
            let duplicate_in_active_context = result.success
                && ((tool_output_already_in_context(messages, &tool_name, &full_output)
                    && duplicate_pointer.len() < full_output.len())
                    || (tool_output_already_in_context(messages, &tool_name, &result.output)
                        && duplicate_pointer.len() < result.output.len()));
            let full_output_bytes = if full_output == duplicate_pointer {
                result.output.len() as u64
            } else {
                full_output.len() as u64
            };
            if result.success || mutation_changed {
                guard.record_success(&call.name);
                mutation_succeeded |= matches!(call.name.as_str(), "write" | "patch");
                if matches!(
                    call.name.as_str(),
                    "shell" | "write" | "patch" | "ask_question"
                ) {
                    repeated_failure_in_batch = false;
                }
            }
            let output = if duplicate_in_active_context {
                duplicate_pointer
            } else {
                full_output
            };
            if duplicate_in_active_context {
                if let Err(error) = push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ToolEvidenceReused {
                        original_bytes: full_output_bytes,
                        emitted_bytes: output.len() as u64,
                        post_compaction: false,
                    },
                ) {
                    drop(
                        self.cancel_pending_background(
                            pending_background,
                            next_seq,
                            "tool_evidence_reused",
                        )
                        .await?,
                    );
                    return Err(error);
                }
            }
            if reacquisitions.remove(&call.id) && !duplicate_in_active_context {
                if let Err(error) = push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ToolEvidenceReused {
                        original_bytes: 0,
                        emitted_bytes: 0,
                        post_compaction: true,
                    },
                ) {
                    drop(
                        self.cancel_pending_background(
                            pending_background,
                            next_seq,
                            "tool_evidence_reused",
                        )
                        .await?,
                    );
                    return Err(error);
                }
            }
            self.append_conversation_message(
                messages,
                ProviderMessage::tool(tool_name, self.redact_sensitive(&call.id), output),
            )?;
            // A volatile operation may change state even when it fails.
            // Use the existing causal boundary instead of treating its exit
            // status as proof that the workspace stayed unchanged.
            let volatile_boundary = self.app.events()[event_start..].iter().any(|event| {
                matches!(&event.kind, crate::EventKind::CausalBoundaryObserved {
                    call_id, kind: crate::CausalBoundaryKind::PotentiallyVolatile, ..
                } if call_id.as_ref() == call.id || call_id.as_ref() == format!("{}:then_run", call.id))
            });
            let repeated_failure = if volatile_boundary {
                *guard = LoopGuard::default();
                repeated_failure_in_batch = false;
                false
            } else if result.success || mutation_changed {
                false
            } else {
                // Reuse the governor's prepared identity and dependency state.
                // Validation shells also use the observed workspace revision.
                let causal_identity =
                    self.app.events()[event_start..]
                        .iter()
                        .find_map(|event| match &event.kind {
                            crate::EventKind::CausalProgressObserved {
                                call_id,
                                call_fingerprint,
                                ..
                            }
                            | crate::EventKind::CausalAnomalyDetected {
                                call_id,
                                call_fingerprint,
                                ..
                            } if call_id.as_ref() == call.id => Some(call_fingerprint.as_ref()),
                            crate::EventKind::CausalBoundaryObserved {
                                call_id,
                                call_fingerprint,
                                ..
                            } if call_id.as_ref() == call.id => Some(call_fingerprint.as_ref()),
                            _ => None,
                        });
                let accepted = match causal_identity {
                    Some(fingerprint) => {
                        guard.accept_canonical(&call.name, fingerprint, &result.output)
                    }
                    None => guard.accept(&call.name, &call.arguments, &result.output),
                };
                !accepted
            };
            repeated_failure_in_batch |= repeated_failure;
        }
        if mutation_succeeded {
            let elision = elide_superseded_tool_outputs(messages);
            if elision.elided > 0 {
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ToolEvidenceElided {
                        count: u64::from(elision.elided),
                        original_bytes: elision.original_bytes,
                        emitted_bytes: elision.emitted_bytes,
                    },
                )?;
            }
        }
        Ok(repeated_failure_in_batch)
    }

    /// After a budget or no-progress stop, asks once for a tool-free final
    /// answer that fits the context window; failures are kept in
    /// `finalization_error` instead of failing the run.
    #[allow(clippy::too_many_arguments)]
    async fn request_final_answer<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &mut Vec<ProviderMessage>,
        stop: AgentLoopStop,
        config: &AgentLoopConfig,
        final_compaction_policy: &CompactionPolicy,
        governor: &CausalGovernor,
        run_start_seq: u64,
        initial_messages: &[ProviderMessage],
        cwd: &Path,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let finalize_event_start = self.app.events().len();
        self.uncommitted_event_start = Some(finalize_event_start);
        let mut final_messages = messages.clone();
        final_messages.push(ProviderMessage::user(
            if matches!(
                stop,
                AgentLoopStop::NoProgress | AgentLoopStop::RepeatedFailedTool
            ) {
                NO_PROGRESS_FINALIZE_PROMPT
            } else {
                BUDGET_FINALIZE_PROMPT
            },
        ));
        // The closing call obeys the same context budget as loop turns:
        // shrink the carried history with the existing local mechanism and
        // skip a request that still cannot fit instead of spending a doomed
        // provider round-trip.
        if !self.finalization_fits_budget(client, &final_messages, config) {
            if let Ok(selection) = select_compaction_history(
                &final_messages,
                &compaction_policy_for_window(
                    final_compaction_policy.clone(),
                    config.context_window_tokens,
                ),
            ) {
                let summary = self.redact_sensitive(&local_emergency_summary(&selection));
                let summary = self
                    .archive_compaction_summary(
                        &selection,
                        summary,
                        &governor.compaction_snapshot(run_start_seq),
                        initial_messages,
                        cwd,
                    )
                    .await?;
                if let Ok(compacted) =
                    apply_compaction_selection(&final_messages, &selection, summary)
                {
                    final_messages = compacted;
                }
            }
        }
        if !self.finalization_fits_budget(client, &final_messages, config) {
            self.finalization_error = Some(ProviderError::InvalidResponse {
                message: "final response request exceeds the context window".into(),
            });
            *next_seq = self.observed_next_seq(*next_seq);
        } else {
            match self
                .run_provider_messages_with_tools(client, &final_messages, &[], *next_seq, true)
                .await
            {
                Ok(turn) => {
                    *next_seq = turn.next_seq;
                    let text = self.app.events()[finalize_event_start..]
                        .iter()
                        .filter_map(|event| match &event.kind {
                            crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
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
                        let mut assistant = ProviderMessage::assistant(text, Vec::new());
                        assistant.responses_reasoning = turn.responses_reasoning;
                        assistant.chat_reasoning = turn.chat_reasoning;
                        self.append_conversation_message(messages, assistant)?;
                    }
                    self.uncommitted_event_start = None;
                }
                Err(error) => {
                    self.finalization_error = Some(self.redact_provider_error(error));
                    *next_seq = self.observed_next_seq(*next_seq);
                }
            }
        }
        Ok(())
    }

    /// Compacts the history before this turn's request: applies a prepared
    /// summary, runs a manual/overflow summary with bounded retries, or falls
    /// back to the local emergency summary over the hard threshold.
    #[allow(clippy::too_many_arguments)]
    async fn compact_for_turn<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &mut Vec<ProviderMessage>,
        initial_messages: &[ProviderMessage],
        cwd: &Path,
        tools: &[Value],
        mode: crate::OperatingMode,
        trigger: CompactionTrigger,
        config: &AgentLoopConfig,
        compaction_policy: &CompactionPolicy,
        governor: &mut CausalGovernor,
        run_start_seq: u64,
        recovery: &mut RecoveryBudget,
        pending_background: &mut Option<PendingBackgroundCompaction>,
        next_seq: &mut u64,
    ) -> Result<CompactionOutcome, ProviderError> {
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();
        let provider_identity = format!(
            "{:?}:{}",
            client.adapter().wire_kind(),
            client.adapter().model()
        );
        let mut prepared = self
            .compaction_handle
            .as_ref()
            .and_then(|handle| handle.take_prepared(messages, &provider_identity));
        if prepared.is_none() && pending_background.is_some() {
            drop(
                self.cancel_pending_background(
                    pending_background,
                    next_seq,
                    "foreground_compaction_required",
                )
                .await?,
            );
            prepared = self
                .compaction_handle
                .as_ref()
                .and_then(|handle| handle.take_prepared(messages, &provider_identity));
        }
        if let Some(prepared) = prepared {
            let tokens_before = trigger.preflight_tokens;
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
                recent_tokens: estimate_provider_message_tokens(
                    &messages[prepared.first_kept_index..],
                )
                .saturating_add(estimate_provider_message_tokens(&prepared.pinned)),
            };
            let summary = self.redact_sensitive(&prepared.summary);
            let summary_result = self
                .archive_compaction_summary(
                    &selection,
                    summary,
                    &governor.compaction_snapshot(run_start_seq),
                    initial_messages,
                    cwd,
                )
                .await;
            let summary = match summary_result {
                Err(ProviderError::Cancelled) => {
                    return Ok(CompactionOutcome::Cancelled);
                }
                result => result?,
            };
            *messages = apply_compaction_selection(messages, &selection, summary.clone()).map_err(
                |message| ProviderError::InvalidResponse {
                    message: message.into(),
                },
            )?;
            let mut request = self.prepare_loop_request(client, messages, tools, mode)?;
            let tokens_after =
                self.token_estimator
                    .estimate(provider, model, request.serialized_chars);
            request.estimated_tokens = tokens_after;
            let applied = request;
            if let Some(handle) = &self.compaction_handle {
                handle.commit_detailed(crate::context::CompactionCommit {
                    summary,
                    prefix_fingerprint: prepared.prefix_fingerprint,
                    first_kept_index: prepared.first_kept_index,
                    tokens_before,
                    tokens_after,
                    input_tokens: prepared.input_tokens,
                    output_tokens: prepared.output_tokens,
                    duration_ms: prepared.duration_ms,
                    reason: crate::context::CompactionReason::HardThreshold,
                    generation: 0,
                });
            }
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionState {
                    state: crate::context::CompactionStatus::Applied,
                    reason: crate::context::CompactionReason::HardThreshold,
                    tokens_before,
                    tokens_after,
                    duration_ms: prepared.duration_ms,
                },
            )?;
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionCompleted,
            )?;
            governor.forget_compacted_evidence();
            return Ok(CompactionOutcome::Applied(Box::new(applied)));
        } else if trigger.manual {
            push_runtime_event(
                &mut self.app,
                next_seq,
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
                        client,
                        messages,
                        initial_messages,
                        cwd,
                        &governor.compaction_snapshot(run_start_seq),
                        tools,
                        mode,
                        trigger.preflight_tokens,
                        *next_seq,
                        config.context_window_tokens,
                        config.context_reserve_tokens,
                        if recovery.overflow_retry_used {
                            CompactionReason::Overflow
                        } else {
                            CompactionReason::Manual
                        },
                    )
                    .await;
                match result {
                    Err(error)
                        if recovery.compaction_recoveries < MAX_PROVIDER_RECOVERIES
                            && recovery.automatic_recoveries < MAX_AUTOMATIC_RECOVERIES
                            && recoverable_provider_error(&error)
                            && !self.is_cancelled() =>
                    {
                        *next_seq = self.observed_next_seq(*next_seq);
                        let delay = match provider_recovery_delay(
                            &error,
                            recovery.compaction_recoveries + 1,
                            recovery.provider_recovery_wait,
                            config.provider_recovery_backoff,
                        ) {
                            Ok(delay) => delay,
                            Err(blocked) => {
                                break Err(annotate_provider_recovery_error(
                                    blocked,
                                    "compaction",
                                    compaction_attempts,
                                    recovery.automatic_recoveries,
                                    recovery.compaction_recoveries,
                                    "retry wait budget exhausted",
                                ))
                            }
                        };
                        recovery.compaction_recoveries += 1;
                        recovery.automatic_recoveries += 1;
                        recovery.provider_recovery_wait += delay;
                        let reason = self.redact_sensitive(&provider_retry_reason(&error));
                        push_runtime_event(&mut self.app, next_seq, crate::EventKind::ProviderPhase {
                                phase: ProviderPhase::Compacting, elapsed_ms: 0,
                                detail: Some(format!("Retrying foreground compaction ({compaction_recoveries}/{MAX_PROVIDER_RECOVERIES}); waiting {} ms", delay.as_millis(), compaction_recoveries = recovery.compaction_recoveries)),
                            })?;
                        push_runtime_event(
                            &mut self.app,
                            next_seq,
                            crate::EventKind::RetryScheduled {
                                attempt: recovery.compaction_recoveries,
                                limit: MAX_PROVIDER_RECOVERIES,
                                wait_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                                reason: Some(reason),
                            },
                        )?;
                        let cancellation = self.cancellation.clone();
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {},
                            _ = async {
                                match cancellation {
                                    Some(token) => token.cancelled().await,
                                    None => std::future::pending::<()>().await,
                                }
                            } => {},
                        }
                        if self.is_cancelled() {
                            break Err(ProviderError::Cancelled);
                        }
                    }
                    Err(error) if recoverable_provider_error(&error) => {
                        break Err(annotate_provider_recovery_error(
                            error,
                            "compaction",
                            compaction_attempts,
                            recovery.automatic_recoveries,
                            recovery.compaction_recoveries,
                            if recovery.automatic_recoveries >= MAX_AUTOMATIC_RECOVERIES {
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
                *next_seq = self.observed_next_seq(*next_seq);
                return Ok(CompactionOutcome::Cancelled);
            }
            let (compacted, summary_usage, following_seq, compacted_request) = compact_result?;
            *messages = compacted;
            let applied = compacted_request;
            drop(summary_usage);
            *next_seq = following_seq;
            recovery.compaction_recoveries = 0;
            governor.forget_compacted_evidence();
            return Ok(CompactionOutcome::Applied(Box::new(applied)));
        } else if trigger.over_hard {
            let selection = select_compaction_history(
                messages,
                &compaction_policy_for_window(
                    compaction_policy.clone(),
                    config.context_window_tokens,
                ),
            )
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
            let summary = self.redact_sensitive(&local_emergency_summary(&selection));
            let summary_result = self
                .archive_compaction_summary(
                    &selection,
                    summary,
                    &governor.compaction_snapshot(run_start_seq),
                    initial_messages,
                    cwd,
                )
                .await;
            let summary = match summary_result {
                Err(ProviderError::Cancelled) => {
                    return Ok(CompactionOutcome::Cancelled);
                }
                result => result?,
            };
            let tokens_before = trigger.preflight_tokens;
            let prefix_fingerprint = compaction_prefix_fingerprint(&selection.summarized);
            *messages = apply_compaction_selection(messages, &selection, summary.clone()).map_err(
                |message| ProviderError::InvalidResponse {
                    message: message.into(),
                },
            )?;
            let mut request = self.prepare_loop_request(client, messages, tools, mode)?;
            let tokens_after =
                self.token_estimator
                    .estimate(provider, model, request.serialized_chars);
            request.estimated_tokens = tokens_after;
            let applied = request;
            if let Some(handle) = &self.compaction_handle {
                handle.commit_detailed(crate::context::CompactionCommit {
                    summary,
                    prefix_fingerprint,
                    first_kept_index: selection.first_kept_index,
                    tokens_before,
                    tokens_after,
                    input_tokens: 0,
                    output_tokens: 0,
                    duration_ms: 0,
                    reason: crate::context::CompactionReason::HardThreshold,
                    generation: 0,
                });
            }
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionState {
                    state: crate::context::CompactionStatus::Applied,
                    reason: crate::context::CompactionReason::HardThreshold,
                    tokens_before,
                    tokens_after,
                    duration_ms: 0,
                },
            )?;
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::CompactionCompleted,
            )?;
            governor.forget_compacted_evidence();
            return Ok(CompactionOutcome::Applied(Box::new(applied)));
        }
        Ok(CompactionOutcome::Skipped)
    }

    /// Applies the recovery policy to one provider attempt. Transport retries
    /// resend the current turn; a correction request is a new model turn.
    #[allow(clippy::too_many_arguments)]
    async fn settle_provider_attempt(
        &mut self,
        result: Result<ProviderTurnResult, ProviderError>,
        recovery: &mut RecoveryBudget,
        pending_background: &mut Option<PendingBackgroundCompaction>,
        messages: &mut Vec<ProviderMessage>,
        config: &mut AgentLoopConfig,
        event_start: usize,
        has_compactable: bool,
        turn: usize,
        next_seq: &mut u64,
    ) -> Result<ProviderAttempt, ProviderError> {
        match result {
            Ok(turn) => {
                recovery.provider_recoveries = 0;
                recovery.empty_recovery_used = false;
                Ok(ProviderAttempt::Completed(turn))
            }
            Err(error)
                if is_empty_provider_response(&error)
                    && !recovery.empty_recovery_used
                    && recovery.provider_recoveries < MAX_PROVIDER_RECOVERIES
                    && recovery.automatic_recoveries < MAX_AUTOMATIC_RECOVERIES
                    && !request_emitted_tools(&self.app, event_start) =>
            {
                recovery.empty_recovery_used = true;
                recovery.automatic_recoveries += 1;
                recovery.provider_recoveries += 1;
                let reason = self.redact_sensitive(&provider_retry_reason(&error));
                self.append_conversation_message(messages, ProviderMessage::user(
                    "The provider returned an empty response. Produce an effective answer or make the needed tool call; do not return an empty response.",
                ))?;
                self.uncommitted_event_start = None;
                push_runtime_event(&mut self.app, next_seq, crate::EventKind::ThinkingEnded)?;
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ProviderPhase {
                        phase: ProviderPhase::Connecting,
                        elapsed_ms: 0,
                        detail: Some(format!(
                            "Recovering empty provider response ({automatic_recoveries}/{MAX_AUTOMATIC_RECOVERIES})",
                            automatic_recoveries = recovery.automatic_recoveries,
                        )),
                    },
                )?;
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::RetryScheduled {
                        attempt: recovery.automatic_recoveries,
                        limit: MAX_AUTOMATIC_RECOVERIES,
                        wait_ms: 0,
                        reason: Some(reason),
                    },
                )?;
                Ok(ProviderAttempt::Retry)
            }
            Err(error)
                if recovery.recovery_output_limit.is_some()
                    && recovery.truncation_recoveries < 2
                    && is_output_limit_rejection(&error)
                    && !has_causal_provider_output(&self.app, event_start) =>
            {
                // Unknown gateways may accept a smaller ceiling than our
                // fallback. Use the last recovery at the original limit.
                recovery.truncation_recoveries = 2;
                recovery.recovery_output_limit = None;
                config.context_reserve_tokens = recovery.initial_context_reserve;
                self.uncommitted_event_start = None;
                push_runtime_event(&mut self.app, next_seq, crate::EventKind::ProviderPhase {
                    phase: ProviderPhase::Connecting,
                    elapsed_ms: 0,
                    detail: Some("Provider rejected the larger output budget; continuing at the original limit (recovery 2/2)".into()),
                })?;
                Ok(ProviderAttempt::Retry)
            }
            Err(ProviderError::MalformedToolCall) if self.pending_argument_repair.is_some() => {
                let note = self.pending_argument_repair.take().unwrap_or_default();
                let partial = self.app.events()[event_start..]
                    .iter()
                    .filter_map(|event| match &event.kind {
                        crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if !partial.is_empty() {
                    self.append_conversation_message(
                        messages,
                        ProviderMessage::assistant(partial, Vec::new()),
                    )?;
                }
                self.append_conversation_message(messages, ProviderMessage::user(format!(
                    "[Tool argument validation]\nThe entire previous tool batch was rejected before execution. No tools from that batch ran. Correct the arguments as JSON objects before requesting tools again.\n{note}"
                )))?;
                self.uncommitted_event_start = None;
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::AssistantEnded {
                        reason: "tool_arguments_rejected".into(),
                    },
                )?;
                if recovery.argument_repairs >= 2 || turn + 1 >= config.max_turns {
                    drop(
                        self.cancel_pending_background(
                            pending_background,
                            next_seq,
                            "argument_repair_limit",
                        )
                        .await?,
                    );
                    return Err(ProviderError::InvalidResponse { message: format!(
                        "tool arguments remain invalid after {argument_repairs} repair retries or the configured turn limit; rejected batch was not executed; task remains pending: {note}",
                        argument_repairs = recovery.argument_repairs,
                    ) });
                }
                recovery.argument_repairs += 1;
                push_runtime_event(&mut self.app, next_seq, crate::EventKind::ThinkingEnded)?;
                push_runtime_event(&mut self.app, next_seq, crate::EventKind::ProviderPhase {
                    phase: ProviderPhase::Connecting, elapsed_ms: 0,
                    detail: Some(format!("Repairing tool arguments ({argument_repairs}/2); rejected batch was not executed", argument_repairs = recovery.argument_repairs)),
                })?;
                Ok(ProviderAttempt::NextTurn)
            }
            Err(error)
                if !recovery.overflow_retry_used
                    && self.compaction_handle.is_some()
                    && is_context_overflow_error(&error)
                    && !has_causal_provider_output(&self.app, event_start)
                    && has_compactable =>
            {
                recovery.overflow_retry_used = true;
                *next_seq = self.observed_next_seq(*next_seq);
                let has_prepared = self.compaction_handle.as_ref().is_some_and(|handle| {
                    handle.status() == crate::context::CompactionStatus::Ready
                });
                if !has_prepared {
                    drop(
                        self.cancel_pending_background(
                            pending_background,
                            next_seq,
                            "context_overflow_retry",
                        )
                        .await?,
                    );
                    if let Some(handle) = &self.compaction_handle {
                        handle.invalidate();
                        let _ = handle.request_manual("");
                    }
                }
                Ok(ProviderAttempt::Retry)
            }
            Err(error)
                if recovery.provider_recoveries < MAX_PROVIDER_RECOVERIES
                    && recovery.automatic_recoveries < MAX_AUTOMATIC_RECOVERIES
                    && recoverable_provider_error(&error)
                    && !request_emitted_tools(&self.app, event_start) =>
            {
                let delay = match provider_recovery_delay(
                    &error,
                    recovery.provider_recoveries + 1,
                    recovery.provider_recovery_wait,
                    config.provider_recovery_backoff,
                ) {
                    Ok(delay) => delay,
                    Err(blocked) => {
                        drop(
                            self.cancel_pending_background(
                                pending_background,
                                next_seq,
                                "provider_retry_budget",
                            )
                            .await?,
                        );
                        if self
                            .wait_for_manual_retry(
                                &error,
                                event_start,
                                config.provider_recovery_backoff,
                                next_seq,
                            )
                            .await?
                        {
                            return Ok(ProviderAttempt::Retry);
                        }
                        return Err(annotate_provider_recovery_error(
                            blocked,
                            "provider",
                            recovery.provider_attempts,
                            recovery.automatic_recoveries,
                            recovery.provider_recoveries,
                            "retry wait budget exhausted",
                        ));
                    }
                };
                recovery.provider_recovery_wait += delay;
                recovery.provider_recoveries += 1;
                recovery.automatic_recoveries += 1;
                let reason = self.redact_sensitive(&provider_retry_reason(&error));
                let partial = self.app.events()[event_start..]
                    .iter()
                    .filter_map(|event| match &event.kind {
                        crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if !partial.is_empty() {
                    self.append_conversation_message(
                        messages,
                        ProviderMessage::assistant(
                            format!("[Interrupted turn]\n{partial}"),
                            Vec::new(),
                        ),
                    )?;
                    self.append_conversation_message(messages, ProviderMessage::user(
                        "The provider failed while generating the previous response. Continue from the preserved partial response and existing tool results. Do not repeat completed actions or claim that the interrupted response completed the task."
                    ))?;
                    push_runtime_event(
                        &mut self.app,
                        next_seq,
                        crate::EventKind::AssistantEnded {
                            reason: "interrupted".into(),
                        },
                    )?;
                }
                self.uncommitted_event_start = None;
                push_runtime_event(&mut self.app, next_seq, crate::EventKind::ThinkingEnded)?;
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ProviderPhase {
                        phase: ProviderPhase::Connecting,
                        elapsed_ms: 0,
                        detail: Some(format!(
                            "Retrying provider ({provider_recoveries}/{MAX_PROVIDER_RECOVERIES}); waiting {} ms",
                            delay.as_millis(),
                            provider_recoveries = recovery.provider_recoveries,
                        )),
                    },
                )?;
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::RetryScheduled {
                        attempt: recovery.provider_recoveries,
                        limit: MAX_PROVIDER_RECOVERIES,
                        wait_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        reason: Some(reason),
                    },
                )?;
                let cancellation = self.cancellation.clone();
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {},
                    _ = async {
                        match cancellation {
                            Some(token) => token.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {},
                }
                Ok(ProviderAttempt::Retry)
            }
            Err(error) => {
                drop(
                    self.cancel_pending_background(pending_background, next_seq, "provider_error")
                        .await?,
                );
                let recovery_reason =
                    if is_empty_provider_response(&error) && recovery.empty_recovery_used {
                        "repeated empty provider response"
                    } else if request_emitted_tools(&self.app, event_start) {
                        "tool effects were emitted; request was not replayed"
                    } else if recovery.automatic_recoveries >= MAX_AUTOMATIC_RECOVERIES {
                        "global automatic recovery limit reached"
                    } else if recovery.provider_recoveries >= MAX_PROVIDER_RECOVERIES {
                        "consecutive provider recovery limit reached"
                    } else {
                        "automatic recovery stopped"
                    };
                if self
                    .wait_for_manual_retry(
                        &error,
                        event_start,
                        config.provider_recovery_backoff,
                        next_seq,
                    )
                    .await?
                {
                    return Ok(ProviderAttempt::Retry);
                }
                Err(
                    if recoverable_provider_error(&error) || is_empty_provider_response(&error) {
                        annotate_provider_recovery_error(
                            error,
                            "provider",
                            recovery.provider_attempts,
                            recovery.automatic_recoveries,
                            recovery.provider_recoveries,
                            recovery_reason,
                        )
                    } else {
                        error
                    },
                )
            }
        }
    }

    async fn finish_background_if_ready(
        &mut self,
        pending: &mut Option<PendingBackgroundCompaction>,
        policy: &CompactionPolicy,
        next_seq: &mut u64,
    ) -> Result<UsageTotals, ProviderError> {
        if pending
            .as_ref()
            .is_none_or(|attempt| !attempt.task.is_finished())
        {
            return Ok(UsageTotals::default());
        }
        let mut attempt = pending
            .take()
            .ok_or_else(|| ProviderError::InvalidResponse {
                message: "finished compaction attempt disappeared".into(),
            })?;
        let joined = (&mut attempt.task).await;
        self.finish_background_attempt(attempt, joined, policy, next_seq)
    }

    fn finish_background_attempt(
        &mut self,
        attempt: PendingBackgroundCompaction,
        joined: Result<BackgroundCompactionResult, tokio::task::JoinError>,
        policy: &CompactionPolicy,
        next_seq: &mut u64,
    ) -> Result<UsageTotals, ProviderError> {
        let progress = compaction_progress_snapshot(&attempt.progress);
        let fallback_duration = elapsed_millis(attempt.started);
        self.emit_background_jev_outcome(&attempt.jev_outcome, next_seq)?;
        let result = match joined {
            Ok(result) => result,
            Err(_) => {
                let observed_usage =
                    background_compaction_cancellation_usage(&attempt, None, fallback_duration);
                self.record_background_cancellation(
                    next_seq,
                    attempt.request_bytes,
                    attempt.estimated_input_tokens,
                    attempt.tokens_before,
                    fallback_duration,
                    progress,
                    "background_task_failed",
                )?;
                return Ok(observed_usage);
            }
        };
        if result.cancelled {
            let observed_usage = background_compaction_cancellation_usage(
                &attempt,
                Some(&result.plan),
                result.duration_ms,
            );
            self.record_background_cancellation(
                next_seq,
                result.plan.request_bytes,
                result.plan.estimated_input_tokens,
                result.plan.tokens_before,
                result.duration_ms,
                progress,
                "provider_request_cancelled",
            )?;
            return Ok(observed_usage);
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
        let observed_usage = background_compaction_completed_usage(&attempt, &result);
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
            return Ok(observed_usage);
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
        Ok(observed_usage)
    }

    fn emit_background_jev_outcome(
        &mut self,
        outcome: &Arc<Mutex<Option<JevAttemptOutcome>>>,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let outcome = match outcome.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
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

    async fn cancel_pending_background(
        &mut self,
        pending: &mut Option<PendingBackgroundCompaction>,
        next_seq: &mut u64,
        reason: &str,
    ) -> Result<UsageTotals, ProviderError> {
        let Some(mut attempt) = pending.take() else {
            return Ok(UsageTotals::default());
        };
        let joined = tokio::select! {
            biased;
            result = &mut attempt.task => Some(result),
            _ = std::future::ready(()) => None,
        };
        if let Some(joined) = joined {
            let policy = self
                .compaction_handle
                .as_ref()
                .map(CompactionHandle::policy)
                .unwrap_or_default();
            return self.finish_background_attempt(attempt, joined, &policy, next_seq);
        }

        attempt.cancellation.cancel();
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut attempt.task).await {
            Ok(joined) => {
                let policy = self
                    .compaction_handle
                    .as_ref()
                    .map(CompactionHandle::policy)
                    .unwrap_or_default();
                self.finish_background_attempt(attempt, joined, &policy, next_seq)
            }
            Err(_) => {
                attempt.task.abort();
                let _ = (&mut attempt.task).await;
                self.emit_background_jev_outcome(&attempt.jev_outcome, next_seq)?;
                let duration_ms = elapsed_millis(attempt.started);
                let observed_usage =
                    background_compaction_cancellation_usage(&attempt, None, duration_ms);
                self.record_background_cancellation(
                    next_seq,
                    attempt.request_bytes,
                    attempt.estimated_input_tokens,
                    attempt.tokens_before,
                    duration_ms,
                    compaction_progress_snapshot(&attempt.progress),
                    reason,
                )?;
                Ok(observed_usage)
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "persisted cancellation telemetry is intentionally explicit"
    )]
    fn record_background_cancellation(
        &mut self,
        next_seq: &mut u64,
        request_bytes: u64,
        estimated_input_tokens: u64,
        tokens_before: u64,
        duration_ms: u64,
        progress: CompactionAttemptProgress,
        reason: &str,
    ) -> Result<(), ProviderError> {
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

    fn jev_plan_can_pay(
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

    async fn build_background_compaction_plan<A: ProviderAdapter>(
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
        let mut retained_conversation: Vec<ProviderMessage> = selection
            .summarized
            .iter()
            .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
            .cloned()
            .collect();
        retained_conversation.push(ProviderMessage::user(selection.root_instruction.clone()));
        retained_conversation.extend(selection.pinned.iter().cloned());
        retained_conversation.extend(selection.kept.iter().cloned());
        let projected_retained_tokens = estimate_provider_message_tokens(&retained_conversation);
        let projected_tokens_after = fixed_request_tokens
            .saturating_add(projected_retained_tokens)
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
        let observed_turns = usage_since(&self.app, 0)
            .requests
            .iter()
            .filter(|request| {
                request.request_kind == crate::RequestKind::ProviderTurn
                    && !request.failed
                    && !request.cancelled
            })
            .count();
        let future_turns =
            economy::future_turns(remaining_model_turns, observed_turns, completed, open);
        let projected_savings_tokens = tokens_before
            .saturating_sub(projected_tokens_after)
            .saturating_mul(u64::from(future_turns));
        // The optional Jev pre-pass has a separate price comparison.
        let jev_plan = (policy.strategy == crate::context::CompactionStrategy::Jev
            && self.jev_judge.is_some())
        .then(|| crate::context::jev_prune::PreparedPrune::new(&selection, None, &summarized))
        .filter(|plan| self.jev_plan_can_pay(plan, preflight_input_tokens));
        let strategy = if jev_plan.is_some() {
            crate::context::CompactionStrategy::Jev
        } else {
            crate::context::CompactionStrategy::Summary
        };
        let estimated_cost_tokens =
            preflight_input_tokens.saturating_add(COMPACTION_MAX_OUTPUT_TOKENS);
        let safety_margin_tokens = estimated_cost_tokens.div_ceil(4);
        let profitable = self.compaction_pricing.map_or_else(
            || {
                projected_savings_tokens
                    > estimated_cost_tokens.saturating_add(safety_margin_tokens)
            },
            |pricing| {
                pricing.can_pay(
                    tokens_before,
                    projected_tokens_after,
                    preflight_input_tokens,
                    COMPACTION_MAX_OUTPUT_TOKENS,
                    future_turns,
                )
            },
        );
        if !profitable {
            return Some(BackgroundCompactionPlan {
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
                projected_savings_tokens,
                estimated_cost_tokens,
                safety_margin_tokens,
                future_turns,
                profitable: false,
            });
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
        let request_bytes = u64::try_from(request.body.len()).unwrap_or(u64::MAX);
        let estimated_input_tokens =
            self.token_estimator
                .estimate(provider, model, request.serialized_chars);
        debug_assert!(request.serialized_chars <= preflight_chars);
        request.estimated_tokens = estimated_input_tokens;
        let serialized_chars = request.serialized_chars;
        Some(BackgroundCompactionPlan {
            selection,
            request: Some(request),
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
            serialized_chars,
            system_bytes,
            history_bytes,
            source_len: messages.len(),
            tokens_before,
            projected_tokens_after,
            request_bytes,
            estimated_input_tokens,
            projected_savings_tokens,
            estimated_cost_tokens,
            safety_margin_tokens,
            future_turns,
            profitable: true,
        })
    }

    async fn prepare_provider_tool_invocations(
        &self,
        mode: crate::OperatingMode,
        cwd: &Path,
        calls: &[ProviderToolCall],
    ) -> Result<Vec<PreparedToolInvocation>, ProviderError> {
        let tools = self.tools.clone();
        let cwd = cwd.to_path_buf();
        // Identical (name, arguments) pairs prepare identically within one
        // batch (same mode, workspace and specs; only timing differs), and
        // evidence aliasing maps them to a single execution anyway: prepare
        // once per distinct call and expand by clone in the original order.
        let mut index_of: std::collections::HashMap<(&str, &str), usize> =
            std::collections::HashMap::with_capacity(calls.len());
        let mut distinct = Vec::with_capacity(calls.len());
        let mut remap = Vec::with_capacity(calls.len());
        for call in calls {
            if let Some(&found) = index_of.get(&(call.name.as_str(), call.arguments.as_str())) {
                remap.push(found);
            } else {
                index_of.insert(
                    (call.name.as_str(), call.arguments.as_str()),
                    distinct.len(),
                );
                distinct.push((call.name.clone(), call.arguments.clone()));
                remap.push(distinct.len() - 1);
            }
        }
        let prepared =
            tokio::task::spawn_blocking(move || tools.prepare_invocations(mode, cwd, &distinct))
                .await
                .map_err(|error| ProviderError::InvalidResponse {
                    message: format!("tool preparation task failed: {error}"),
                })?;
        Ok(remap
            .into_iter()
            .map(|index| {
                let mut prepared = prepared[index].clone();
                if self.shell_jobs.running() || calls.iter().any(|call| call.name == "shell") {
                    if let Some(spec) = prepared.spec.as_mut() {
                        spec.cacheability = crate::tools::ToolCacheability::None;
                        spec.replay_policy = crate::tools::ToolReplayPolicy::Never;
                    }
                }
                prepared
            })
            .collect())
    }

    fn add_initial_workspace_context<A: ProviderAdapter>(
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
        let fits = |messages: &[ProviderMessage]| {
            estimate_unprepared_request_chars(adapter, messages, tools.as_ref(), None).is_some_and(
                |chars| {
                    let tokens = self.token_estimator.estimate(
                        crate::provider::provider_kind_name(adapter.kind()),
                        adapter.model(),
                        chars,
                    );
                    let policy = CompactionPolicy::default();
                    !policy.is_over_soft(tokens, config.context_window_tokens)
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

    fn overlay_channel<'a>(
        &self,
        messages: &'a mut [ProviderMessage],
        mode: crate::OperatingMode,
    ) -> mode::ChannelOverlay<'a> {
        mode::ChannelOverlay::apply(
            messages,
            mode,
            self.interaction_route.is_some() && mode != crate::OperatingMode::Plan,
            Some(
                &mut self
                    .write_projection_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            ),
        )
    }

    fn prepare_loop_request<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        messages: &mut [ProviderMessage],
        tools: &[Value],
        mode: crate::OperatingMode,
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let overlay = self.overlay_channel(messages, mode);
        client.prepare_messages_with_tools(overlay.view(), tools)
    }

    #[cfg(test)]
    fn add_session_channel_context(
        &self,
        messages: &mut [ProviderMessage],
        mode: crate::OperatingMode,
    ) {
        if self.is_cancelled() {
            return;
        }
        for message in messages.iter_mut().filter(|message| message.role == "user") {
            if let Some(index) = message.content.find(mode::CHANNEL_MARKER) {
                message.content.truncate(index);
            }
        }
        let Some(message) = messages
            .iter_mut()
            .rev()
            .find(|message| message.role == "user")
        else {
            return;
        };
        let can_ask = self.interaction_route.is_some() && mode != crate::OperatingMode::Plan;
        message
            .content
            .push_str(mode::channel_stanza(mode, can_ask));
    }

    async fn execute_provider_tool_batch(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        batch_id: &str,
        calls: &[ProviderToolCall],
        next_seq: u64,
        governor: &mut CausalGovernor,
    ) -> Result<(Vec<ToolResult>, u64), ProviderError> {
        // Even callers without a host token own their native work until it
        // stops. Dropping a blocking-task future is not a termination barrier.
        let previous = self.cancellation.clone();
        let cancellation = previous.clone().unwrap_or_default();
        self.cancellation = Some(cancellation.clone());
        let result = self
            .execute_provider_tool_batch_inner(mode, cwd, batch_id, calls, next_seq, governor)
            .await;
        if result.is_err() || cancellation.is_cancelled() {
            cancellation.cancel();
            cancellation.wait_for_native_work().await;
        } else {
            // Managed jobs outlive a batch, but remain owned by the run.
            cancellation.wait_for_foreground_work().await;
        }
        self.cancellation = previous;
        result
    }

    async fn execute_provider_tool_batch_inner(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        batch_id: &str,
        calls: &[ProviderToolCall],
        mut next_seq: u64,
        governor: &mut CausalGovernor,
    ) -> Result<(Vec<ToolResult>, u64), ProviderError> {
        if calls.is_empty() || self.is_cancelled() {
            return Ok((Vec::new(), next_seq));
        }
        self.presentation_sources.clear();
        // Preparation is pure (arg parsing + path resolution): prepare the
        // whole batch once so the workspace root is canonicalized a single
        // time instead of once per call. Independent snapshot reads run first
        // so a later read of an unrelated file does not wait on a mutation.
        // Results stay in the original call order.
        let prepared_all = self
            .prepare_provider_tool_invocations(mode, cwd, calls)
            .await?;
        for call in calls {
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolPrepared {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                },
            )?;
        }
        let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
        let phase1 = phase1_snapshot_indices(&self.tools, &prepared_all);
        next_seq = self
            .apply_snapshot_segment(
                batch_id,
                calls,
                &prepared_all,
                &phase1,
                &mut results,
                next_seq,
                governor,
            )
            .await?;
        let mut completed = vec![false; calls.len()];
        for &item in &phase1 {
            completed[item] = results[item].is_some();
        }
        let mut index = 0;
        while index < calls.len() {
            if self.is_cancelled() {
                break;
            }
            if results[index].is_some() {
                index += 1;
                continue;
            }
            if is_file_mutation(&prepared_all[index]) && !is_serial_barrier(&prepared_all[index]) {
                let mut cluster = vec![index];
                let mut seen = std::collections::HashSet::new();
                if let Some(key) = mutation_path_key(&prepared_all[index]) {
                    seen.insert(key);
                }
                let mut look = index + 1;
                while look < calls.len() {
                    if results[look].is_some() {
                        look += 1;
                        continue;
                    }
                    if !is_file_mutation(&prepared_all[look])
                        || is_serial_barrier(&prepared_all[look])
                    {
                        break;
                    }
                    let Some(key) = mutation_path_key(&prepared_all[look]) else {
                        break;
                    };
                    if !seen.insert(key) {
                        break;
                    }
                    cluster.push(look);
                    look += 1;
                }
                if cluster.len() >= 2 {
                    let subset_calls = cluster
                        .iter()
                        .map(|&item| calls[item].clone())
                        .collect::<Vec<_>>();
                    let subset_prepared = cluster
                        .iter()
                        .map(|&item| prepared_all[item].clone())
                        .collect();
                    let (segment_results, following_seq) = self
                        .execute_independent_mutation_segment(
                            cwd,
                            batch_id,
                            &subset_calls,
                            subset_prepared,
                            next_seq,
                            governor,
                        )
                        .await?;
                    next_seq = following_seq;
                    for (item, result) in cluster.iter().zip(segment_results) {
                        results[*item] = Some(result);
                        completed[*item] = true;
                    }
                    if !self.is_cancelled() {
                        let ready = phase1_snapshot_indices_ready(
                            &self.tools,
                            &prepared_all,
                            0,
                            &completed,
                        );
                        if ready.len() >= 2 {
                            next_seq = self
                                .apply_snapshot_segment(
                                    batch_id,
                                    calls,
                                    &prepared_all,
                                    &ready,
                                    &mut results,
                                    next_seq,
                                    governor,
                                )
                                .await?;
                            for &item in &ready {
                                completed[item] = results[item].is_some();
                            }
                        }
                    }
                    continue;
                }
            }
            let call = &calls[index];
            let prepared = prepared_all[index].clone();
            let (pending, observations) =
                governor.observe_before_identified(&prepared, batch_id, &call.id);
            self.emit_governor_observations(observations, &mut next_seq)?;
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolAdmitted {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                },
            )?;
            let execution = self
                .execute_provider_tool_call(
                    mode,
                    cwd,
                    ToolInvocation::provider(batch_id, call),
                    &prepared,
                    next_seq,
                )
                .await;
            let (outcome, following_seq) = match execution {
                Ok(completed) => completed,
                Err(ProviderError::Cancelled) if self.is_cancelled() => break,
                Err(error) => return Err(error),
            };
            next_seq = following_seq;
            let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
            self.emit_governor_observations(observations, &mut next_seq)?;
            if let Some(presentation) = outcome.receipt.presentation.clone() {
                self.presentation_sources
                    .insert((batch_id.to_owned(), call.id.clone()), presentation);
            }
            results[index] = Some(outcome.result);
            completed[index] = true;
            let finished_mutation = is_file_mutation(&prepared);
            let finished_barrier = is_serial_barrier(&prepared);
            index += 1;
            if (finished_mutation || finished_barrier) && !self.is_cancelled() {
                let ready =
                    phase1_snapshot_indices_ready(&self.tools, &prepared_all, 0, &completed);
                if ready.len() >= 2 {
                    next_seq = self
                        .apply_snapshot_segment(
                            batch_id,
                            calls,
                            &prepared_all,
                            &ready,
                            &mut results,
                            next_seq,
                            governor,
                        )
                        .await?;
                    for &item in &ready {
                        completed[item] = results[item].is_some();
                    }
                }
            }
        }
        if !self.is_cancelled() {
            let observations = governor.finish_turn();
            self.emit_governor_observations(observations, &mut next_seq)?;
        }
        let results = results.into_iter().flatten().collect();
        Ok((results, next_seq))
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_snapshot_segment(
        &mut self,
        batch_id: &str,
        calls: &[ProviderToolCall],
        prepared_all: &[PreparedToolInvocation],
        indices: &[usize],
        results: &mut [Option<ToolResult>],
        next_seq: u64,
        governor: &mut CausalGovernor,
    ) -> Result<u64, ProviderError> {
        if indices.is_empty() {
            return Ok(next_seq);
        }
        let subset_calls = indices
            .iter()
            .map(|&index| calls[index].clone())
            .collect::<Vec<_>>();
        let subset_prepared = indices
            .iter()
            .map(|&index| prepared_all[index].clone())
            .collect();
        let (segment_results, following_seq) = self
            .execute_read_only_tool_segment(
                batch_id,
                &subset_calls,
                subset_prepared,
                next_seq,
                governor,
            )
            .await?;
        for (index, result) in indices.iter().zip(segment_results) {
            results[*index] = Some(result);
        }
        Ok(following_seq)
    }

    async fn execute_read_only_tool_segment(
        &mut self,
        batch_id: &str,
        calls: &[ProviderToolCall],
        prepared_calls: Vec<PreparedToolInvocation>,
        mut next_seq: u64,
        governor: &mut CausalGovernor,
    ) -> Result<(Vec<ToolResult>, u64), ProviderError> {
        let alias_of = evidence_reuse_aliases(&prepared_calls);

        let call_ids = calls.iter().map(|call| call.id.clone()).collect::<Vec<_>>();
        let preflights = governor.observe_before_batch(&prepared_calls, batch_id, &call_ids);
        if self.is_cancelled() {
            return Ok((Vec::new(), next_seq));
        }
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::channel::<ToolStartedNotice>(calls.len().max(1));
        let mut pending_calls = Vec::with_capacity(calls.len());
        for (index, (call, (pending, observations))) in calls.iter().zip(preflights).enumerate() {
            self.emit_governor_observations(observations, &mut next_seq)?;
            pending_calls.push(pending);
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolAdmitted {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                },
            )?;
            // Evidence aliases have no executor future. Keep a lifecycle
            // block for those synthetic results; real leaders announce
            // ToolStarted from their pool future below.
            if alias_of[index] != index {
                let arguments = self.redact_sensitive(&call.arguments);
                push_runtime_event(
                    &mut self.app,
                    &mut next_seq,
                    crate::EventKind::ToolStarted {
                        batch_id: batch_id.into(),
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments,
                    },
                )?;
            }
        }

        let tools = self.tools.clone();
        let code_intel = self.code_intel.clone();
        let cancellation = self.cancellation.clone();
        let started_arguments = calls
            .iter()
            .map(|call| self.redact_sensitive(&call.arguments))
            .collect::<Vec<_>>();
        let started_tx_for_futures = started_tx.clone();
        let futures = calls
            .iter()
            .cloned()
            .zip(prepared_calls.iter().cloned())
            .enumerate()
            .filter_map(|(index, (call, prepared))| {
                if alias_of[index] != index {
                    return None;
                }
                let tools = tools.clone();
                let code_intel = code_intel.clone();
                let cancellation = cancellation.clone();
                let started_tx = started_tx_for_futures.clone();
                let arguments = started_arguments[index].clone();
                Some(async move {
                    let _ = started_tx
                        .send(ToolStartedNotice { index, arguments })
                        .await;
                    let started_at = Instant::now();
                    let revision_before = tools.workspace_revision();
                    let mut outcome = if call.name == "code_intel" {
                        let request = prepared_code_intel_request(&prepared);
                        let (mut result, semantic_presentation) =
                            run_code_intel_request(code_intel, request, cancellation.clone()).await;
                        // Code-intel runs outside the registry's synchronous
                        // executor. Keep the same admission feedback contract
                        // as native tools, including batch calls, cache aliases,
                        // and the model-facing prompt copy.
                        let prefix =
                            crate::tools::admission_output_prefix(&prepared.admission_notes);
                        if let Some(prefix) = &prefix {
                            result.output.insert_str(0, prefix);
                        }
                        let presentation = semantic_presentation.map(|presentation| {
                            ToolPresentationSource::CodeIntel {
                                prefix: prefix.unwrap_or_default(),
                                presentation,
                            }
                        });
                        ToolExecutionOutcome {
                            result,
                            receipt: ToolExecutionReceipt {
                                presentation,
                                ..ToolExecutionReceipt::unobserved(
                                    &prepared,
                                    revision_before,
                                    tools.workspace_revision(),
                                    u64::try_from(started_at.elapsed().as_micros())
                                        .unwrap_or(u64::MAX),
                                )
                            },
                        }
                    } else {
                        let result_name = call.name.clone();
                        let fallback_prepared = prepared.clone();
                        let fallback_tools = tools.clone();
                        let cancellation_for_tool = cancellation.clone();
                        let native_work = cancellation_for_tool
                            .as_ref()
                            .map(CancellationToken::track_native_work);
                        match tokio::task::spawn_blocking(move || {
                            let _native_work = native_work;
                            tools.execute_prepared_with_cancellation_and_progress(
                                &prepared,
                                cancellation_for_tool.as_ref(),
                                |_| {},
                            )
                        })
                        .await
                        {
                            Ok(outcome) => outcome,
                            Err(error) => ToolExecutionOutcome {
                                result: ToolResult {
                                    name: result_name,
                                    success: false,
                                    output: format!("read-only tool task failed: {error}"),
                                    artifact: None,
                                },
                                receipt: ToolExecutionReceipt::unobserved(
                                    &fallback_prepared,
                                    revision_before,
                                    fallback_tools.workspace_revision(),
                                    u64::try_from(started_at.elapsed().as_micros())
                                        .unwrap_or(u64::MAX),
                                ),
                            },
                        }
                    };
                    if cancellation
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        outcome.result.success = false;
                    }
                    (
                        index,
                        ReadOnlyToolOutcome {
                            outcome,
                            duration_ms: u64::try_from(started_at.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                        },
                    )
                })
            });
        drop(started_tx);
        let mut outcomes =
            futures_util::stream::iter(futures).buffer_unordered(READ_ONLY_BATCH_CONCURRENCY);
        let mut completed = std::iter::repeat_with(|| None)
            .take(calls.len())
            .collect::<Vec<Option<ToolExecutionOutcome>>>();
        let mut started_open = true;
        loop {
            let next = tokio::select! {
                notice = started_rx.recv(), if started_open => {
                    match notice {
                        Some(notice) => push_tool_started_notice(
                            &mut self.app,
                            &mut next_seq,
                            batch_id,
                            calls,
                            notice,
                        )?,
                        None => started_open = false,
                    }
                    continue;
                }
                next = outcomes.next() => next,
            };
            let Some((index, outcome)) = next else {
                drain_tool_started_notices(
                    &mut self.app,
                    &mut next_seq,
                    batch_id,
                    calls,
                    &mut started_rx,
                )?;
                break;
            };
            drain_tool_started_notices(
                &mut self.app,
                &mut next_seq,
                batch_id,
                calls,
                &mut started_rx,
            )?;
            let call = &calls[index];
            let mut outcome = outcome;
            outcome.outcome.result.output = self.redact_sensitive(&outcome.outcome.result.output);
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolOutput {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: outcome.outcome.result.name.clone(),
                    output: outcome.outcome.result.output.clone(),
                },
            )?;
            let edit_diff = self.redacted_edit_diff(outcome.outcome.receipt.edit_diff.as_ref());
            push_tool_result_facts(
                &mut self.app,
                &mut next_seq,
                batch_id,
                &call.id,
                &outcome.outcome.result.name,
                outcome.outcome.receipt.process.as_ref(),
                edit_diff,
            )?;
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolFinished {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: outcome.outcome.result.name.clone(),
                    success: outcome.outcome.result.success,
                    duration_ms: outcome.duration_ms,
                },
            )?;
            completed[index] = Some(outcome.outcome);
        }
        drain_tool_started_notices(
            &mut self.app,
            &mut next_seq,
            batch_id,
            calls,
            &mut started_rx,
        )?;
        for index in 0..calls.len() {
            let leader = alias_of[index];
            if leader == index {
                continue;
            }
            let mut reused = completed[leader]
                .as_ref()
                .expect("evidence leader completed")
                .clone();
            reused.receipt.execution_us = 0;
            reused.receipt.finalization_us = 0;
            if let Some(presentation) = reused.receipt.presentation.take() {
                let from =
                    crate::tools::admission_output_prefix(&prepared_calls[leader].admission_notes)
                        .unwrap_or_default();
                let to =
                    crate::tools::admission_output_prefix(&prepared_calls[index].admission_notes)
                        .unwrap_or_default();
                reused.receipt.presentation = Some(presentation.replace_prefix(from, to));
            }
            replace_admission_prefix(
                &mut reused.result.output,
                &prepared_calls[leader].admission_notes,
                &prepared_calls[index].admission_notes,
            );
            let call = &calls[index];
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolOutput {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: reused.result.name.clone(),
                    output: reused.result.output.clone(),
                },
            )?;
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolFinished {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: reused.result.name.clone(),
                    success: reused.result.success,
                    duration_ms: 0,
                },
            )?;
            completed[index] = Some(reused);
        }
        let completed = completed
            .into_iter()
            .map(|outcome| outcome.expect("every read-only tool future yields one result"))
            .collect::<Vec<_>>();
        for (call, outcome) in calls.iter().zip(&completed) {
            if let Some(presentation) = outcome.receipt.presentation.clone() {
                self.presentation_sources
                    .insert((batch_id.to_owned(), call.id.clone()), presentation);
            }
        }
        let governor_calls = pending_calls
            .into_iter()
            .zip(&completed)
            .map(|(pending, outcome)| {
                // Single redact per call: already redacted at production, so
                // only the owned clone the batch API requires remains.
                (pending, outcome.result.clone(), outcome.receipt.clone())
            })
            .collect();
        let observations = governor.observe_snapshot_batch_after(governor_calls);
        for call_observations in observations {
            self.emit_governor_observations(call_observations, &mut next_seq)?;
        }
        let results = completed
            .into_iter()
            .map(|outcome| outcome.result)
            .collect();
        Ok((results, next_seq))
    }

    async fn execute_independent_mutation_segment(
        &mut self,
        cwd: &Path,
        batch_id: &str,
        calls: &[ProviderToolCall],
        prepared_calls: Vec<PreparedToolInvocation>,
        mut next_seq: u64,
        governor: &mut CausalGovernor,
    ) -> Result<(Vec<ToolResult>, u64), ProviderError> {
        let call_ids = calls.iter().map(|call| call.id.clone()).collect::<Vec<_>>();
        let preflights = governor.observe_before_batch(&prepared_calls, batch_id, &call_ids);
        if self.is_cancelled() {
            return Ok((Vec::new(), next_seq));
        }
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::channel::<ToolStartedNotice>(calls.len().max(1));
        let mut pending_calls = Vec::with_capacity(calls.len());
        for (call, (pending, observations)) in calls.iter().zip(preflights) {
            self.emit_governor_observations(observations, &mut next_seq)?;
            pending_calls.push(pending);
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolAdmitted {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                },
            )?;
        }

        let tools = self.tools.clone();
        let cancellation = self.cancellation.clone();
        let started_arguments = calls
            .iter()
            .map(|call| self.redact_sensitive(&call.arguments))
            .collect::<Vec<_>>();
        let started_tx_for_futures = started_tx.clone();
        let futures = calls
            .iter()
            .cloned()
            .zip(prepared_calls.iter().cloned())
            .enumerate()
            .map(|(index, (call, prepared))| {
                let tools = tools.clone();
                let cancellation = cancellation.clone();
                let started_tx = started_tx_for_futures.clone();
                let arguments = started_arguments[index].clone();
                async move {
                    let _ = started_tx
                        .send(ToolStartedNotice { index, arguments })
                        .await;
                    let started_at = Instant::now();
                    let revision_before = tools.workspace_revision();
                    let result_name = call.name.clone();
                    let fallback_prepared = prepared.clone();
                    let fallback_tools = tools.clone();
                    let cancellation_for_tool = cancellation.clone();
                    let native_work = cancellation_for_tool
                        .as_ref()
                        .map(CancellationToken::track_native_work);
                    let mut outcome = match tokio::task::spawn_blocking(move || {
                        let _native_work = native_work;
                        tools.execute_prepared_with_cancellation_and_progress(
                            &prepared,
                            cancellation_for_tool.as_ref(),
                            |_| {},
                        )
                    })
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => ToolExecutionOutcome {
                            result: ToolResult {
                                name: result_name,
                                success: false,
                                output: format!("mutation tool task failed: {error}"),
                                artifact: None,
                            },
                            receipt: ToolExecutionReceipt::unobserved(
                                &fallback_prepared,
                                revision_before,
                                fallback_tools.workspace_revision(),
                                u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
                            ),
                        },
                    };
                    if cancellation
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        outcome.result.success = false;
                    }
                    (
                        index,
                        ReadOnlyToolOutcome {
                            outcome,
                            duration_ms: u64::try_from(started_at.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                        },
                    )
                }
            });
        drop(started_tx);
        let mut outcomes =
            futures_util::stream::iter(futures).buffer_unordered(READ_ONLY_BATCH_CONCURRENCY);
        let mut completed = std::iter::repeat_with(|| None)
            .take(calls.len())
            .collect::<Vec<Option<ToolExecutionOutcome>>>();
        let mut first_error = None;
        let mut started_open = true;
        loop {
            let next = tokio::select! {
                notice = started_rx.recv(), if started_open => {
                    match notice {
                        Some(notice) => push_tool_started_notice(
                            &mut self.app,
                            &mut next_seq,
                            batch_id,
                            calls,
                            notice,
                        )?,
                        None => started_open = false,
                    }
                    continue;
                }
                next = outcomes.next() => next,
            };
            let Some((index, outcome)) = next else {
                drain_tool_started_notices(
                    &mut self.app,
                    &mut next_seq,
                    batch_id,
                    calls,
                    &mut started_rx,
                )?;
                break;
            };
            drain_tool_started_notices(
                &mut self.app,
                &mut next_seq,
                batch_id,
                calls,
                &mut started_rx,
            )?;
            let call = &calls[index];
            let mut outcome = outcome;
            outcome.outcome.result.output = self.redact_sensitive(&outcome.outcome.result.output);
            let output_result = push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolOutput {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: outcome.outcome.result.name.clone(),
                    output: outcome.outcome.result.output.clone(),
                },
            );
            let edit_diff = self.redacted_edit_diff(outcome.outcome.receipt.edit_diff.as_ref());
            let process_result = push_tool_result_facts(
                &mut self.app,
                &mut next_seq,
                batch_id,
                &call.id,
                &outcome.outcome.result.name,
                outcome.outcome.receipt.process.as_ref(),
                edit_diff,
            );
            let finish_result = push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ToolFinished {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: outcome.outcome.result.name.clone(),
                    success: outcome.outcome.result.success,
                    duration_ms: outcome.duration_ms,
                },
            );
            for result in [output_result, process_result, finish_result] {
                if let Err(error) = result {
                    if let Some(token) = &cancellation {
                        token.cancel();
                    }
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            completed[index] = Some(outcome.outcome);
        }
        drain_tool_started_notices(
            &mut self.app,
            &mut next_seq,
            batch_id,
            calls,
            &mut started_rx,
        )?;
        let completed = completed
            .into_iter()
            .map(|outcome| outcome.expect("every independent mutation yields one result"))
            .collect::<Vec<_>>();
        for (call, (prepared, outcome)) in calls.iter().zip(prepared_calls.iter().zip(&completed)) {
            self.notify_code_intel_after_mutation(
                cwd,
                ToolInvocation::provider(batch_id, call),
                prepared,
                outcome,
            )
            .await;
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        for (pending, outcome) in pending_calls.into_iter().zip(&completed) {
            let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
            self.emit_governor_observations(observations, &mut next_seq)?;
        }
        let results = completed
            .into_iter()
            .map(|outcome| outcome.result)
            .collect();
        Ok((results, next_seq))
    }

    fn emit_governor_observations(
        &mut self,
        observations: Vec<GovernorObservation>,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        for observation in observations {
            let kind = match observation {
                GovernorObservation::Progress {
                    batch_id,
                    call_id,
                    kind,
                    tool_name,
                    call_fingerprint,
                    evidence_id,
                    workspace_revision,
                } => crate::EventKind::CausalProgressObserved {
                    batch_id: batch_id.into_boxed_str(),
                    call_id: call_id.into_boxed_str(),
                    kind,
                    tool_name: tool_name.into_boxed_str(),
                    call_fingerprint: call_fingerprint.into_boxed_str(),
                    evidence_id: evidence_id.into_boxed_str(),
                    workspace_revision,
                },
                GovernorObservation::Boundary {
                    batch_id,
                    call_id,
                    kind,
                    tool_name,
                    call_fingerprint,
                    uncertainty_epoch,
                } => crate::EventKind::CausalBoundaryObserved {
                    batch_id: batch_id.into_boxed_str(),
                    call_id: call_id.into_boxed_str(),
                    kind,
                    tool_name: tool_name.into_boxed_str(),
                    call_fingerprint: call_fingerprint.into_boxed_str(),
                    uncertainty_epoch,
                },
                GovernorObservation::Anomaly {
                    batch_id,
                    call_id,
                    kind,
                    tool_name,
                    call_fingerprint,
                    evidence_id,
                    workspace_revision,
                    occurrence,
                    confidence,
                    action,
                } => crate::EventKind::CausalAnomalyDetected {
                    batch_id: batch_id.into_boxed_str(),
                    call_id: call_id.into_boxed_str(),
                    kind,
                    tool_name: tool_name.into_boxed_str(),
                    call_fingerprint: call_fingerprint.into_boxed_str(),
                    evidence_id: evidence_id.into_boxed_str(),
                    workspace_revision,
                    occurrence,
                    confidence,
                    action,
                },
            };
            push_runtime_event(&mut self.app, next_seq, kind)?;
        }
        Ok(())
    }

    async fn execute_provider_tool_call(
        &mut self,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        next_seq: u64,
    ) -> Result<(ToolExecutionOutcome, u64), ProviderError> {
        let started_at = Instant::now();
        let revision_before = self.tools.workspace_revision();
        if invocation.name == "shell_job" || (invocation.name == "shell" && mode.allows_mutation())
        {
            return self
                .execute_managed_shell(mode, invocation, prepared, next_seq)
                .await;
        }
        let special = if invocation.name == "ask_question" {
            Some(self.execute_ask_question(invocation, next_seq).await?)
        } else if invocation.name == "artifact_read" {
            Some(self.execute_artifact_read(invocation, next_seq)?)
        } else if invocation.name == "todo" {
            Some(self.execute_todo(mode, invocation, next_seq)?)
        } else if invocation.name == "skill" {
            Some(
                self.execute_skill(mode, cwd.as_ref(), invocation, next_seq)
                    .await?,
            )
        } else if invocation.name == "code_intel" {
            Some(
                self.execute_code_intel(mode, cwd, invocation, prepared, next_seq)
                    .await?,
            )
        } else if invocation.name == "mcp" {
            Some(self.execute_mcp(mode, invocation, next_seq).await?)
        } else {
            let cwd_path = cwd.as_ref().to_path_buf();
            let (outcome, following) = self
                .execute_tool_call_async(invocation, prepared, next_seq)
                .await?;
            // Ordered LSP sync after successful workspace mutations so a
            // following semantic query cannot overtake didChange/didSave.
            self.notify_code_intel_after_mutation(&cwd_path, invocation, prepared, &outcome)
                .await;
            return Ok((outcome, following));
        };
        let (result, following) = special.expect("special tool branch returns a result");
        Ok((
            ToolExecutionOutcome {
                result,
                receipt: ToolExecutionReceipt::unobserved(
                    prepared,
                    revision_before,
                    self.tools.workspace_revision(),
                    u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
                ),
            },
            following,
        ))
    }

    fn execute_artifact_read(
        &mut self,
        invocation: ToolInvocation<'_>,
        mut seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            id: String,
            #[serde(default)]
            offset: usize,
            #[serde(default = "default_artifact_page_bytes")]
            max_bytes: usize,
        }

        let redacted_arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments: redacted_arguments,
            },
        )?;
        let output = (|| -> Result<String, String> {
            let args: Arguments = serde_json::from_str(invocation.arguments)
                .map_err(|_| "artifact_read requires id, offset and max_bytes".to_owned())?;
            if !(4..=16 * 1024).contains(&args.max_bytes) {
                return Err("artifact_read max_bytes must be 4..=16384".into());
            }
            if !self.restored_artifact_ids.contains(&args.id) && !self.app.events().iter().any(|event| {
                matches!(&event.kind, crate::EventKind::ArtifactStored { id, .. } if id == &args.id)
            }) {
                return Err("artifact id is not available in this session".into());
            }
            let store = self
                .artifact_store
                .as_ref()
                .ok_or("artifact storage is unavailable")?;
            let bytes = store
                .read_id(&args.id)
                .map_err(|error| format!("artifact could not be verified: {:?}", error.kind()))?;
            let content =
                std::str::from_utf8(&bytes).map_err(|_| "artifact is not UTF-8 text".to_owned())?;
            if args.offset > content.len() || !content.is_char_boundary(args.offset) {
                return Err("artifact offset is not a UTF-8 boundary".into());
            }
            let mut end = args
                .offset
                .saturating_add(args.max_bytes)
                .min(content.len());
            while end > args.offset && !content.is_char_boundary(end) {
                end -= 1;
            }
            if end == args.offset && end < content.len() {
                return Err("artifact max_bytes is too small for the next character".into());
            }
            Ok(serde_json::json!({
                "id": args.id,
                "offset": args.offset,
                "next_offset": end,
                "eof": end == content.len(),
                "content": &content[args.offset..end],
            })
            .to_string())
        })();
        let (success, output) = match output {
            Ok(output) => (true, output),
            Err(error) => (false, error),
        };
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output: output.clone(),
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                success,
                duration_ms: 0,
            },
        )?;
        Ok((
            ToolResult {
                name: invocation.name.into(),
                success,
                output,
                artifact: None,
            },
            seq,
        ))
    }

    /// Runs one code_intel tool call against the installed CodeIntelligence
    /// facade. Mirrors the native tool event flow; only this async path can
    /// serve it because LSP queries await language-server responses.
    async fn execute_code_intel(
        &mut self,
        _mode: crate::OperatingMode,
        _cwd: impl AsRef<Path>,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut seq = next_seq;
        let started_at = Instant::now();
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let request = prepared_code_intel_request(prepared);
        let (mut outcome, semantic_presentation) =
            run_code_intel_request(self.code_intel.clone(), request, self.cancellation.clone())
                .await;
        if self.is_cancelled() {
            outcome.success = false;
        }
        if let Some(prefix) = crate::tools::admission_output_prefix(&prepared.admission_notes) {
            outcome.output.insert_str(0, &prefix);
            if let Some(presentation) = semantic_presentation {
                self.presentation_sources.insert(
                    (
                        invocation.batch_id.to_owned(),
                        invocation.call_id.to_owned(),
                    ),
                    ToolPresentationSource::CodeIntel {
                        prefix,
                        presentation,
                    },
                );
            }
        } else if let Some(presentation) = semantic_presentation {
            self.presentation_sources.insert(
                (
                    invocation.batch_id.to_owned(),
                    invocation.call_id.to_owned(),
                ),
                ToolPresentationSource::CodeIntel {
                    prefix: String::new(),
                    presentation,
                },
            );
        }
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let output = self.redact_sensitive(&outcome.output);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                success: outcome.success,
                duration_ms,
            },
        )?;
        Ok((outcome, seq))
    }

    /// Runs one `mcp` meta-tool call against the shared manager. The manager
    /// connects lazily on first use; list/describe keep schemas out of the
    /// prompt until the model asks for them. Event name is the canonical
    /// `mcp.{server}.{tool}` for calls so transcripts stay meaningful.
    async fn execute_mcp(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut seq = next_seq;
        let started_at = Instant::now();
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let cancellation = self
            .cancellation
            .as_ref()
            .map(CancellationToken::mcp_cancellation)
            .unwrap_or_default();
        let result = if mode.allows_mutation() {
            run_mcp_dispatch(self.mcp.clone(), invocation.arguments, cancellation).await
        } else {
            ToolResult {
                name: "mcp".into(),
                success: false,
                output: "mcp is only available in Auto mode".into(),
                artifact: None,
            }
        };
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let output = self.redact_sensitive(&result.output);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                success: result.success,
                duration_ms,
            },
        )?;
        Ok((result, seq))
    }

    /// Best-effort didChange/didSave push after the agent wrote a file. The
    /// warm-only implementation may no-op, but when it does synchronize, the
    /// await establishes protocol order before the next tool call.
    async fn notify_code_intel_after_mutation(
        &self,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        outcome: &ToolExecutionOutcome,
    ) {
        if (!outcome.result.success
            && outcome.receipt.mutations.is_empty()
            && !outcome.receipt.effects_uncertain)
            || !matches!(invocation.name, "write" | "patch")
        {
            return;
        }
        let Some(code_intel) = self.code_intel.as_ref() else {
            return;
        };
        let Some(absolute) = prepared.target_paths.first().cloned() else {
            return;
        };
        let text = if outcome.receipt.effects_uncertain || outcome.receipt.fused_shell.is_some() {
            None
        } else {
            match &prepared.arguments {
                PreparedToolArguments::Write { content, .. } => {
                    Some(crate::codeintel::CodeIntelFileUpdate {
                        text: content.clone(),
                        patch: None,
                    })
                }
                PreparedToolArguments::Patch { .. } => outcome.receipt.synced_text.clone(),
                _ => None,
            }
        };
        let synchronize = async {
            if let Some(update) = text {
                code_intel.notify_file_updated(cwd, &absolute, update).await;
            } else {
                code_intel.notify_file_changed(cwd, &absolute, None).await;
            }
        };
        if let Some(cancellation) = &self.cancellation {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {},
                _ = synchronize => {},
            }
        } else {
            synchronize.await;
        }
    }

    /// ask_question routes through the interaction channel (TUI/headless
    /// respond out-of-band); the result is the structured human answer.
    async fn execute_ask_question(
        &mut self,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut seq = next_seq;
        let started_at = Instant::now();
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;

        let route = self.interaction_route.clone();
        let cancellation = self.cancellation.clone();
        let mut result = match route {
            None => ToolResult {
                name: invocation.name.into(),
                success: false,
                output: "ask_question unavailable: no interaction route configured".into(),
                artifact: None,
            },
            Some(route) => match AskQuestion::parse(invocation.arguments) {
                Err(error) => ToolResult {
                    name: invocation.name.into(),
                    success: false,
                    output: format!("invalid ask_question arguments: {error}"),
                    artifact: None,
                },
                Ok(question) => {
                    // The request id is the provider tool-call id. The TUI
                    // namespaces it by run only at the projection boundary.
                    match InteractionRequestId::new(invocation.call_id.to_owned()) {
                        Err(_) => ToolResult {
                            name: invocation.name.into(),
                            success: false,
                            output: "invalid interaction request id".into(),
                            artifact: None,
                        },
                        Ok(request_id) => match route.register(request_id.clone()) {
                            Err(error) => ToolResult {
                                name: invocation.name.into(),
                                success: false,
                                output: format!("ask_question: {error}"),
                                artifact: None,
                            },
                            Ok(pending) => {
                                push_runtime_event(
                                    &mut self.app,
                                    &mut seq,
                                    crate::EventKind::QuestionRequired {
                                        request_id: request_id.as_str().to_owned(),
                                        question: question.question,
                                        options: question.options,
                                        persisted: false,
                                    },
                                )?;

                                let received = if let Some(cancellation) = cancellation {
                                    tokio::select! {
                                        answer = pending.receive() => Some(answer),
                                        _ = cancellation.cancelled() => None,
                                    }
                                } else {
                                    Some(pending.receive().await)
                                };

                                match received {
                                    Some(Ok(answer)) => {
                                        let output =
                                            serde_json::to_string(&answer).map_err(|error| {
                                                ProviderError::InvalidResponse {
                                                    message: format!(
                                                    "failed to serialize question answer: {error}"
                                                ),
                                                }
                                            })?;
                                        push_runtime_event(
                                            &mut self.app,
                                            &mut seq,
                                            crate::EventKind::InteractionAcknowledged {
                                                request_id: request_id.as_str().to_owned(),
                                                accepted: true,
                                                message: "answer accepted".into(),
                                            },
                                        )?;
                                        ToolResult {
                                            name: invocation.name.into(),
                                            success: true,
                                            output,
                                            artifact: None,
                                        }
                                    }
                                    Some(Err(error)) => {
                                        push_runtime_event(
                                            &mut self.app,
                                            &mut seq,
                                            crate::EventKind::InteractionAcknowledged {
                                                request_id: request_id.as_str().to_owned(),
                                                accepted: false,
                                                message: error.to_string(),
                                            },
                                        )?;
                                        ToolResult {
                                            name: invocation.name.into(),
                                            success: false,
                                            output: format!("interaction declined: {error}"),
                                            artifact: None,
                                        }
                                    }
                                    None => {
                                        push_runtime_event(
                                            &mut self.app,
                                            &mut seq,
                                            crate::EventKind::InteractionAcknowledged {
                                                request_id: request_id.as_str().to_owned(),
                                                accepted: false,
                                                message: "interaction cancelled".into(),
                                            },
                                        )?;
                                        ToolResult {
                                            name: invocation.name.into(),
                                            success: false,
                                            output: "ask_question cancelled".into(),
                                            artifact: None,
                                        }
                                    }
                                }
                            }
                        },
                    }
                }
            },
        };
        if self.is_cancelled() {
            result.success = false;
        }
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let output = self.redact_sensitive(&result.output);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                success: result.success,
                duration_ms,
            },
        )?;
        Ok((result, seq))
    }

    fn execute_todo(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut seq = next_seq;
        let started_at = Instant::now();
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let before = self
            .capability_bridge
            .as_ref()
            .and_then(|bridge| bridge.todo("session").map(todo_changed_items));
        let result = self.apply_todo_tool(mode, invocation);
        if let Some(items) = self
            .capability_bridge
            .as_ref()
            .and_then(|bridge| bridge.todo("session").map(todo_changed_items))
        {
            if result.success || before.as_ref() != Some(&items) {
                push_runtime_event(
                    &mut self.app,
                    &mut seq,
                    crate::EventKind::TodoChanged { items },
                )?;
            }
        }
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let output = self.redact_sensitive(&result.output);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                success: result.success,
                duration_ms,
            },
        )?;
        Ok((result, seq))
    }

    /// Applies todo mutations through the capability bridge (entity "session").
    fn apply_todo_tool(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
    ) -> ToolResult {
        if !mode.allows_mutation() {
            return ToolResult {
                name: invocation.name.into(),
                success: false,
                output: "todo is only available in Auto mode".into(),
                artifact: None,
            };
        }
        let args: Value = match serde_json::from_str(invocation.arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolResult {
                    name: invocation.name.into(),
                    success: false,
                    output: format!("invalid todo arguments: {error}"),
                    artifact: None,
                }
            }
        };
        let mutations = match parse_todo_mutations(&args) {
            Ok(mutations) => mutations,
            Err(error) => {
                return ToolResult {
                    name: invocation.name.into(),
                    success: false,
                    output: format!("invalid todo arguments: {error}; no items changed"),
                    artifact: None,
                }
            }
        };
        let Some(bridge) = self.capability_bridge.as_mut() else {
            return ToolResult {
                name: invocation.name.into(),
                success: false,
                output: "todo unavailable: no capability bridge configured".into(),
                artifact: None,
            };
        };
        let mut lines = Vec::new();
        for (index, (label, mutation)) in mutations.into_iter().enumerate() {
            // Models trained on full-list todo writes resend every entry
            // without ids. A status entry whose title matches one existing
            // item exactly is that item's update, not a new duplicate.
            let mutation = match mutation {
                TaskMutation::TodoAdd {
                    title,
                    status: Some(status),
                } => {
                    let matched = bridge.todo("session").and_then(|tracker| {
                        let mut hits = tracker
                            .items()
                            .iter()
                            .filter(|item| item.title.trim() == title.trim());
                        match (hits.next(), hits.next()) {
                            (Some(item), None) => Some(item.id),
                            _ => None,
                        }
                    });
                    match matched {
                        Some(id) => TaskMutation::TodoSetStatus {
                            reason: None,
                            id: Some(id),
                            status,
                        },
                        None => TaskMutation::TodoAdd {
                            title,
                            status: Some(status),
                        },
                    }
                }
                other => other,
            };
            let target = match &mutation {
                TaskMutation::TodoSetStatus { id, .. } => *id,
                _ => None,
            };
            let revision = bridge.task_revision("session").saturating_add(1);
            let request = TaskMutationRequest {
                idempotency_key: format!("{}-{index}", invocation.call_id),
                entity_id: "session".into(),
                revision,
                mutation,
            };
            match bridge.apply_task_mutation(request, mode, AuthorizationGrant::Explicit) {
                Ok(_changed) => {
                    if let Some(item) = bridge.todo("session").and_then(|tracker| {
                        target.map_or_else(
                            || tracker.items().last(),
                            |id| tracker.items().iter().find(|item| item.id == id),
                        )
                    }) {
                        lines.push(format!(
                            "todo {} [{}]: {}",
                            item.id,
                            todo_status_name(item.status),
                            item.title
                        ));
                    }
                }
                Err(error) => {
                    lines.push(format!("todo rejected ({label}): {error}"));
                    if let Some(tracker) = bridge.todo("session") {
                        lines.push(
                            "Current items (earlier successful entries remain applied):".into(),
                        );
                        lines.extend(tracker.items().iter().map(|item| {
                            format!(
                                "todo {} [{}]: {}",
                                item.id,
                                todo_status_name(item.status),
                                item.title
                            )
                        }));
                    }
                    return ToolResult {
                        name: invocation.name.into(),
                        success: false,
                        output: lines.join("\n"),
                        artifact: None,
                    };
                }
            }
        }
        ToolResult {
            name: invocation.name.into(),
            success: true,
            output: lines.join("\n"),
            artifact: None,
        }
    }

    /// Skill discovery memoized for the current loop run, keyed by cwd.
    /// Returns `None` when discovery fails so the dispatcher falls back to
    /// direct discovery with its exact historical error messages.
    fn cached_skill_discovery(&mut self, cwd: &Path) -> Option<DiscoveryResult> {
        if let Some((dir, discovery)) = self.skill_discovery_cache.as_ref() {
            if dir == cwd {
                return discovery.clone();
            }
        }
        let discovery = discover_workspace(cwd).ok();
        self.skill_discovery_cache = Some((cwd.to_path_buf(), discovery.clone()));
        discovery
    }

    async fn execute_skill(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut seq = next_seq;
        let started_at = Instant::now();
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let cwd = cwd.to_path_buf();
        let arguments_for_task = invocation.arguments.to_owned();
        let cancellation = self.cancellation.clone();
        let process_runner = self.tools.process_runner().clone();
        let cached_discovery = self.cached_skill_discovery(&cwd);
        let native_work = cancellation
            .as_ref()
            .map(CancellationToken::track_native_work);
        let result = tokio::task::spawn_blocking(move || {
            let _native_work = native_work;
            run_skill_dispatch(
                mode,
                &cwd,
                &arguments_for_task,
                cancellation.as_ref(),
                &process_runner,
                cached_discovery,
            )
        })
        .await
        .unwrap_or_else(|error| ToolResult {
            name: "skill".into(),
            success: false,
            output: format!("skill task failed: {error}"),
            artifact: None,
        });
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let output = self.redact_sensitive(&result.output);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                success: result.success,
                duration_ms,
            },
        )?;
        Ok((result, seq))
    }

    fn prepare_loop_capabilities(&mut self, cwd: &Path) -> Result<(), ProviderError> {
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

    /// Restore structured task records only; no tool or external operation is replayed.
    pub fn restore_task_facts(
        &mut self,
        facts: &[DurableFact],
        cwd: &Path,
    ) -> Result<(), ProviderError> {
        let catalog = CapabilityCatalog::with_native_tools();
        let header = DurableSessionHeader::new(
            "loop",
            "now",
            cwd.to_string_lossy().into_owned(),
            None,
            None,
        );
        let mut repo = MemoryRepo::new(header);
        for (index, fact) in facts.iter().enumerate() {
            if fact.namespace != "task.v1" {
                return Err(ProviderError::InvalidResponse {
                    message: "invalid task state namespace".into(),
                });
            }
            repo.append(DurableRecord::Fact {
                seq: index as u64,
                fact: fact.clone(),
            })
            .map_err(|error| ProviderError::InvalidResponse {
                message: format!("task state: {error}"),
            })?;
        }
        let bridge = RuntimeCapabilityBridge::new(repo, catalog).map_err(capability_error)?;
        self.capability_bridge = Some(bridge);
        Ok(())
    }

    pub fn task_facts(&self) -> Vec<DurableFact> {
        self.capability_bridge
            .as_ref()
            .into_iter()
            .flat_map(|bridge| bridge.service().repo().records())
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. } if fact.namespace == "task.v1" => {
                    let mut fact = fact.clone();
                    fact.key = self.redact_sensitive(&fact.key);
                    redact_task_value(&mut fact.value, &self.sensitive_values.0);
                    Some(fact)
                }
                _ => None,
            })
            .collect()
    }

    pub fn todo_items(&self) -> Vec<crate::TodoChangedItem> {
        self.capability_bridge
            .as_ref()
            .and_then(|bridge| bridge.todo("session"))
            .map(todo_changed_items)
            .unwrap_or_default()
    }

    pub fn execute_tool(
        &mut self,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.tools
            .configure_artifacts(self.artifact_store.clone(), &self.sensitive_values.0);
        let batch_id = format!("slim-batch-direct-{next_seq}");
        let call_id = format!("slim-call-direct-{next_seq}");
        if name == "artifact_read" {
            return self.execute_artifact_read(
                ToolInvocation {
                    batch_id: &batch_id,
                    call_id: &call_id,
                    name,
                    arguments,
                },
                next_seq,
            );
        }
        let cwd = cwd.as_ref();
        let prepared = self.tools.prepare_invocation(mode, cwd, name, arguments);
        self.execute_tool_call(
            ToolInvocation {
                batch_id: &batch_id,
                call_id: &call_id,
                name,
                arguments,
            },
            &prepared,
            next_seq,
        )
    }

    async fn execute_tool_call_async(
        &mut self,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        next_seq: u64,
    ) -> Result<(ToolExecutionOutcome, u64), ProviderError> {
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut following_seq = next_seq;
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let started_at = Instant::now();
        let tools = self.tools.clone();
        let fallback_tools = tools.clone();
        let revision_before = tools.workspace_revision();
        let prepared = prepared.clone();
        let fallback_prepared = prepared.clone();
        let cancellation = self.cancellation.clone();
        let result_name = invocation.name.to_owned();
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let native_work = cancellation
            .as_ref()
            .map(CancellationToken::track_native_work);
        let task = tokio::task::spawn_blocking(move || {
            let _native_work = native_work;
            tools.execute_prepared_with_cancellation_and_progress(
                &prepared,
                cancellation.as_ref(),
                |progress| {
                    let _ = progress_tx.send(progress);
                },
            )
        });
        tokio::pin!(task);
        let mut progress_error = None;
        let mut progress_open = true;
        let mut outcome = loop {
            tokio::select! {
                joined = &mut task => {
                    break joined.unwrap_or_else(|error| ToolExecutionOutcome {
                        result: ToolResult {
                            name: result_name.clone(),
                            success: false,
                            output: format!("native tool task failed: {error}"),
                            artifact: None,
                        },
                        receipt: ToolExecutionReceipt::unobserved(
                            &fallback_prepared,
                            revision_before,
                            fallback_tools.workspace_revision(),
                            u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
                        ),
                    });
                }
                progress = progress_rx.recv(), if progress_open => {
                    let Some(progress) = progress else {
                        progress_open = false;
                        continue;
                    };
                    if progress_error.is_none() {
                        let preview = self.redact_sensitive(&progress.preview);
                        if let Err(error) = push_runtime_transient_event(
                            &mut self.app,
                            &mut following_seq,
                            crate::EventKind::ToolProgress {
                                batch_id: invocation.batch_id.into(),
                                call_id: invocation.call_id.into(),
                                name: invocation.name.into(),
                                preview,
                            },
                        ) {
                            if let Some(cancellation) = &self.cancellation {
                                cancellation.cancel();
                            }
                            progress_error = Some(error);
                        }
                    }
                }
            }
        };
        while let Ok(progress) = progress_rx.try_recv() {
            if progress_error.is_some() {
                break;
            }
            let preview = self.redact_sensitive(&progress.preview);
            if let Err(error) = push_runtime_transient_event(
                &mut self.app,
                &mut following_seq,
                crate::EventKind::ToolProgress {
                    batch_id: invocation.batch_id.into(),
                    call_id: invocation.call_id.into(),
                    name: invocation.name.into(),
                    preview,
                },
            ) {
                progress_error = Some(error);
            }
        }
        if let Some(error) = progress_error {
            return Err(error);
        }
        if self.is_cancelled() {
            mark_cancelled_tool_outcome(&mut outcome);
        }
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        outcome.result.output = self.redact_sensitive(&outcome.result.output);
        self.record_existing_artifact(&outcome.result, &mut following_seq)?;
        if let Some(fused_shell) = outcome.receipt.fused_shell.as_mut() {
            fused_shell.1.output = self.redact_sensitive(&fused_shell.1.output);
        }
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: outcome.result.name.clone(),
                output: outcome.result.output.clone(),
            },
        )?;
        let edit_diff = self.redacted_edit_diff(outcome.receipt.edit_diff.as_ref());
        push_tool_result_facts(
            &mut self.app,
            &mut following_seq,
            invocation.batch_id,
            invocation.call_id,
            &outcome.result.name,
            outcome.receipt.process.as_ref(),
            edit_diff,
        )?;
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: outcome.result.name.clone(),
                success: outcome.result.success,
                duration_ms,
            },
        )?;
        Ok((outcome, following_seq))
    }

    fn execute_tool_call(
        &mut self,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let mut following_seq = next_seq;
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let started_at = Instant::now();
        let sensitive_values = self.sensitive_values.0.clone();
        let mut app = std::mem::replace(&mut self.app, AppHandle::fake());
        let mut progress_error = None;
        let mut outcome = self.tools.execute_prepared_with_cancellation_and_progress(
            prepared,
            self.cancellation.as_ref(),
            |progress| {
                if progress_error.is_some() {
                    return;
                }
                let preview = redact_values(&sensitive_values, &progress.preview);
                if let Err(error) = push_runtime_transient_event(
                    &mut app,
                    &mut following_seq,
                    crate::EventKind::ToolProgress {
                        batch_id: invocation.batch_id.into(),
                        call_id: invocation.call_id.into(),
                        name: invocation.name.into(),
                        preview,
                    },
                ) {
                    progress_error = Some(error);
                }
            },
        );
        self.app = app;
        if let Some(error) = progress_error {
            return Err(error);
        }
        // Tool cancellation is cooperative: the process may already have
        // produced a side effect, but the ledger must still close the
        // ToolStarted phase with output and a terminal ToolFinished event.
        if self.is_cancelled() {
            outcome.result.success = false;
        }
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        outcome.result.output = self.redact_sensitive(&outcome.result.output);
        self.record_existing_artifact(&outcome.result, &mut following_seq)?;
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: outcome.result.name.clone(),
                output: outcome.result.output.clone(),
            },
        )?;
        let edit_diff = self.redacted_edit_diff(outcome.receipt.edit_diff.as_ref());
        push_tool_result_facts(
            &mut self.app,
            &mut following_seq,
            invocation.batch_id,
            invocation.call_id,
            &outcome.result.name,
            outcome.receipt.process.as_ref(),
            edit_diff,
        )?;
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: outcome.result.name.clone(),
                success: outcome.result.success,
                duration_ms,
            },
        )?;
        Ok((outcome.result, following_seq))
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    fn observed_next_seq(&self, fallback: u64) -> u64 {
        self.app
            .events()
            .last()
            .and_then(|event| event.seq.checked_add(1))
            .unwrap_or(fallback)
    }

    fn calibrate_latest_request(&mut self, calibration_eligible: bool) {
        if !calibration_eligible {
            return;
        }
        let events = self.app.events();
        let Some(start) = events
            .iter()
            .rposition(|event| matches!(event.kind, crate::EventKind::ContextSnapshot { .. }))
        else {
            return;
        };
        let (provider, model, serialized_chars) = match &events[start].kind {
            crate::EventKind::ContextSnapshot {
                provider,
                model,
                serialized_chars,
                ..
            } => (provider.clone(), model.clone(), *serialized_chars),
            _ => return,
        };
        let ledger = UsageTotals::from_events(&events[start..], false);
        let Some(request) = ledger.requests.first() else {
            return;
        };
        if request.usage_unknown
            || request.response_cache_hit
            || request.cancelled
            || request.failed
        {
            return;
        }
        self.token_estimator.observe(
            &provider,
            &model,
            serialized_chars,
            request.total_input_tokens(),
        );
    }

    async fn archive_compaction_summary(
        &self,
        selection: &CompactionSelection,
        summary: String,
        execution_facts: &str,
        initial_messages: &[ProviderMessage],
        cwd: &Path,
    ) -> Result<String, ProviderError> {
        let max_bytes = self
            .compaction_handle
            .as_ref()
            .map(|handle| handle.policy().summary_max_bytes)
            .unwrap_or_else(|| CompactionPolicy::default().summary_max_bytes);
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
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
        let append_block = |target: &mut String, block: &str| {
            if block.is_empty() {
                return;
            }
            if !target.is_empty() {
                target.push_str("\n\n");
            }
            target.push_str(block);
        };
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
            // Elision changes only the active view. Restore uniquely identified
            // original outputs before archiving; never reread a mutated file.
            let mut originals = std::collections::HashMap::new();
            let historical = initial_messages.iter().filter_map(|message| {
                (message.role == "tool").then_some((
                    message.name.as_deref()?,
                    message.tool_call_id.as_deref()?,
                    message.content.as_str(),
                ))
            });
            let observed = self
                .app
                .events()
                .iter()
                .filter_map(|event| match &event.kind {
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
            let mut recovered = selection.summarized.clone();
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
            let transcript = crate::context::recovery_transcript(&self.redact_messages(&recovered));
            let artifact = store.preview("context-history", transcript.as_bytes());
            let read_path = artifact
                .path
                .strip_prefix(cwd)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"));
            if let Some(path) = read_path {
                let path = serde_json::to_string(&path).expect("path serializes");
                append_block(&mut retained, &format!("[Prior visible transcript: use read on {path} with offset=1 for an index of user-role messages (including runtime notices), checkpoints and tool_call_id evidence. Follow indexed offset/max_lines and pagination to recover historical text; earlier checkpoints link earlier archives. Apply later user corrections. Opaque reasoning and binary attachments are not included.]"));
            } else {
                append_block(&mut retained, &format!("[Prior visible transcript archived at {}; native read cannot access this artifact outside the workspace.]", artifact.path.display()));
            }
            artifact_write = Some((store, transcript));
        }
        let mut retained_with_manifest = retained.clone();
        append_block(&mut retained_with_manifest, &manifest);
        let retained_with_manifest = self.redact_sensitive(&retained_with_manifest);
        let final_checkpoint = match crate::context::fit_checkpoint_content(
            &summary,
            &retained_with_manifest,
            max_bytes,
        ) {
            Ok(checkpoint) => checkpoint,
            Err(_) if !manifest.is_empty() => {
                let retained = self.redact_sensitive(&retained);
                crate::context::fit_checkpoint_content(&summary, &retained, max_bytes).map_err(
                    |message| ProviderError::InvalidResponse {
                        message: format!("final compaction checkpoint rejected: {message}"),
                    },
                )?
            }
            Err(message) => {
                return Err(ProviderError::InvalidResponse {
                    message: format!("final compaction checkpoint rejected: {message}"),
                });
            }
        };
        if self.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if let Some((store, transcript)) = artifact_write {
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
            if self.is_cancelled() {
                crate::context::ArtifactStore::discard_staged(staged).map_err(|_| {
                    ProviderError::InvalidResponse {
                        message:
                            "cancelled compaction could not discard its staged recovery artifact"
                                .into(),
                    }
                })?;
                return Err(ProviderError::Cancelled);
            }
            let commit_cancellation = self.cancellation.clone();
            let committed = tokio::task::spawn_blocking(move || {
                // This check and the atomic publication share one blocking job.
                // Cancellation wins until this linearization point; after it,
                // the checkpoint transaction is committed and the shared,
                // content-addressed artifact must never be rolled back.
                if commit_cancellation
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
        }
        Ok(final_checkpoint)
    }

    fn redact_message(&self, mut message: ProviderMessage) -> ProviderMessage {
        if self.sensitive_values.0.is_empty() {
            return message;
        }
        message.content = self.redact_sensitive(&message.content);
        message.name = message.name.map(|value| self.redact_sensitive(&value));
        message.tool_call_id = message
            .tool_call_id
            .map(|value| self.redact_sensitive(&value));
        for call in &mut message.tool_calls {
            call.id = self.redact_sensitive(&call.id);
            call.name = self.redact_sensitive(&call.name);
            call.arguments = self.redact_sensitive(&call.arguments);
        }
        for block in &mut message.content_blocks {
            match block {
                crate::provider::ProviderContentBlock::Text(text)
                | crate::provider::ProviderContentBlock::Unsupported { kind: text } => {
                    *text = self.redact_sensitive(text);
                }
                crate::provider::ProviderContentBlock::Image { media_type, data }
                | crate::provider::ProviderContentBlock::Audio { media_type, data }
                | crate::provider::ProviderContentBlock::File { media_type, data } => {
                    *media_type = self.redact_sensitive(media_type);
                    *data = self.redact_sensitive(data);
                }
            }
        }
        message
    }

    fn redact_messages(&self, messages: &[ProviderMessage]) -> Vec<ProviderMessage> {
        if self.sensitive_values.0.is_empty() {
            return messages.to_vec();
        }
        messages
            .iter()
            .cloned()
            .map(|message| self.redact_message(message))
            .collect()
    }

    fn record_existing_artifact(
        &mut self,
        result: &ToolResult,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let Some(handle) = result.artifact.as_ref() else {
            return Ok(());
        };
        if self.app.events().iter().any(|event| {
            matches!(&event.kind, crate::EventKind::ArtifactStored { id, .. } if id == &handle.id)
        }) {
            return Ok(());
        }
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ArtifactStored {
                id: handle.id.clone(),
                size: handle.size,
            },
        )
    }

    async fn materialize_results(
        &mut self,
        results: &mut [ToolResult],
        max_result_bytes: usize,
        force: Option<&[bool]>,
        mut next_seq: u64,
    ) -> Result<u64, ProviderError> {
        let Some(store) = self.artifact_store.clone() else {
            return Ok(next_seq);
        };
        let jobs = results
            .iter_mut()
            .enumerate()
            .filter(|(index, result)| {
                result.artifact.is_none()
                    && (result.output.len() > max_result_bytes
                        || force
                            .and_then(|forced| forced.get(*index))
                            .copied()
                            .unwrap_or(false))
            })
            .map(|(index, result)| {
                let store = store.clone();
                let label = format!("tool-{}", result.name);
                let output = std::mem::take(&mut result.output);
                async move {
                    tokio::task::spawn_blocking(move || {
                        let handle = store.put(&label, output.as_bytes())?;
                        Ok::<_, std::io::Error>((index, output, handle))
                    })
                    .await
                    .map_err(|error| ProviderError::InvalidResponse {
                        message: format!("artifact worker: {error}"),
                    })?
                    .map_err(|error| ProviderError::InvalidResponse {
                        message: format!("artifact: {error}"),
                    })
                }
            })
            .collect::<Vec<_>>();
        let stored = futures_util::stream::iter(jobs)
            .buffered(READ_ONLY_BATCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        for stored in stored {
            let (index, output, handle) = stored?;
            let result = &mut results[index];
            result.output = output;
            result.artifact = Some(handle.clone());
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ArtifactStored {
                    id: handle.id,
                    size: handle.size,
                },
            )?;
        }
        Ok(next_seq)
    }

    // This boundary needs the request and batch inputs together to measure the
    // exact provider envelope before committing any individual presentation.
    #[allow(clippy::too_many_arguments)]
    fn plan_tool_presentations<A: ProviderAdapter>(
        &self,
        client: &HttpProviderClient<A>,
        base_messages: &[ProviderMessage],
        tools: &[Value],
        mode: crate::OperatingMode,
        cwd: &Path,
        batch_id: &str,
        calls: &[ProviderToolCall],
        results: &[ToolResult],
        config: AgentLoopConfig,
    ) -> Vec<ToolPresentation> {
        let compaction_policy = self
            .compaction_handle
            .as_ref()
            .map(CompactionHandle::policy)
            .unwrap_or_default();
        // The loop treats the hard threshold as inclusive (`>=`). Keep the
        // projected request strictly below the same configured line so a
        // newly completed tool batch does not immediately trigger a second
        // compaction that would add summary overhead to the batch.
        let hard_threshold = (config.context_compaction_enabled && compaction_policy.enabled)
            .then(|| compaction_policy.hard_threshold_tokens(config.context_window_tokens));
        let targets = results
            .iter()
            .zip(calls)
            .map(|(result, call)| {
                let cap = if result.name == "read" {
                    self.read_presentation_bytes
                } else {
                    config.max_result_bytes
                };
                // `result.output` is post-redaction while the presentation
                // source holds the raw projection; a few bytes of divergence
                // (e.g. "[REDACTED]" shorter than the secret) must not divert
                // the whole result to an artifact, so size by the real need.
                let projected = self
                    .presentation_sources
                    .get(&(batch_id.to_owned(), call.id.clone()))
                    .map(ToolPresentationSource::full_len)
                    .unwrap_or(0);
                result.output.len().max(projected).min(cap)
            })
            .collect::<Vec<_>>();
        let artifact_references = results
            .iter()
            .map(|result| Self::artifact_reference(result, cwd))
            .collect::<Vec<_>>();
        let build = |scale: usize| {
            // Duplicate detection must see the same context the assembly loop
            // will: base history plus this batch's already chosen tool
            // messages. Otherwise a repeated result inside one batch is
            // budgeted as another full copy and can shrink the first
            // occurrence even when one copy plus the notice would fit.
            let mut batch_messages = Vec::with_capacity(calls.len());
            results
                .iter()
                .zip(&targets)
                .zip(calls)
                .zip(&artifact_references)
                .map(|(((result, target), call), suffix)| {
                    // A retained complete result needs only an identity
                    // pointer. Account for that before allocating page space,
                    // otherwise a tight budget can hide the duplicate behind
                    // a zero-record projection and defeat evidence reuse.
                    let name = self.redact_sensitive(&call.name);
                    let duplicate = format!(
                        "[duplicate {name} result omitted; identical output already in context]"
                    );
                    let already_in_context = |text: &str| {
                        duplicate.len() < text.len()
                            && (tool_output_already_in_context(base_messages, &name, text)
                                || tool_output_already_in_context(&batch_messages, &name, text))
                    };
                    let presentation = if result.success && already_in_context(&result.output) {
                        ToolPresentation::complete(duplicate)
                    } else {
                        let allowance = target.saturating_mul(scale).div_ceil(1000);
                        let body_budget =
                            allowance.saturating_sub(suffix.as_ref().map_or(0, String::len));
                        let mut presentation = self
                            .presentation_sources
                            .get(&(batch_id.to_owned(), call.id.clone()))
                            .map(|source| {
                                source.present(PresentationBudget {
                                    max_bytes: body_budget,
                                })
                            })
                            .unwrap_or_else(|| {
                                present_unstructured(
                                    &result.name,
                                    &result.output,
                                    PresentationBudget {
                                        max_bytes: body_budget,
                                    },
                                )
                            });
                        if let Some(suffix) = suffix {
                            if presentation.text.len().saturating_add(suffix.len()) <= allowance {
                                presentation.text.push_str(suffix);
                            } else {
                                // Keep the recovery handle even when the aggregate
                                // budget cannot carry both the selected records and
                                // metadata. The explicit over-budget notice is a
                                // safer contract than an unreachable artifact.
                                presentation.text.push('\n');
                                presentation.text.push_str(suffix);
                            }
                        }
                        // The assembly loop also collapses a projection that is
                        // already verbatim in context (e.g. an identical
                        // truncation); mirror it so the budgeted size matches
                        // the emitted one.
                        if result.success && already_in_context(&presentation.text) {
                            ToolPresentation::complete(duplicate)
                        } else {
                            presentation
                        }
                    };
                    // Mirror `append_conversation_message`: the retained wire
                    // message is the redacted presentation, and it is what the
                    // next duplicate check compares against.
                    batch_messages.push(self.redact_message(ProviderMessage::tool(
                        name,
                        self.redact_sensitive(&call.id),
                        presentation.text.clone(),
                    )));
                    presentation
                })
                .collect::<Vec<_>>()
        };
        let fits = |presentations: &[ToolPresentation]| {
            let mut candidate = base_messages.to_vec();
            for (call, presentation) in calls.iter().zip(presentations) {
                candidate.push(ProviderMessage::tool(
                    self.redact_sensitive(&call.name),
                    self.redact_sensitive(&call.id),
                    presentation.text.clone(),
                ));
            }
            // The loop's preflight uses the conservative structural estimate
            // before preparing the wire request, and that estimate never
            // under-counts the serialized body. When the adapter bounds its
            // request envelope the structural estimate alone is the decision
            // input — the exact path the loop already takes — so transport
            // metadata is only built for adapters without the bound.
            let overlay = self.overlay_channel(&mut candidate, mode);
            let structural_chars =
                estimate_unprepared_request_chars(client.adapter(), overlay.view(), tools, None);
            drop(overlay);
            let budget_estimated = match structural_chars {
                Some(chars) => self.token_estimator.estimate(
                    crate::provider::provider_kind_name(client.adapter().kind()),
                    client.adapter().model(),
                    chars,
                ),
                None => {
                    let Ok(request) =
                        self.prepare_loop_request(client, &mut candidate, tools, mode)
                    else {
                        return false;
                    };
                    self.token_estimator.estimate(
                        crate::provider::provider_kind_name(client.adapter().kind()),
                        client.adapter().model(),
                        request.serialized_chars,
                    )
                }
            };
            let under_hard_threshold =
                hard_threshold.is_none_or(|threshold| budget_estimated < threshold);
            under_hard_threshold
                && budget_estimated.saturating_add(config.context_reserve_tokens)
                    <= config.context_window_tokens
        };

        let full = build(1000);
        if fits(&full) {
            return full;
        }

        let mut low = 0usize;
        let mut high = 999usize;
        let mut best = build(0);
        if fits(&best) {
            while low <= high {
                let middle = low.saturating_add(high).div_ceil(2);
                let candidate = build(middle);
                if fits(&candidate) {
                    best = candidate;
                    low = middle.saturating_add(1);
                } else {
                    high = middle.saturating_sub(1);
                }
            }
        }
        best
    }

    fn artifact_reference(result: &ToolResult, cwd: &Path) -> Option<String> {
        result.artifact.as_ref().map(|handle| {
            let read_path = std::fs::canonicalize(cwd)
                .ok()
                .zip(std::fs::canonicalize(&handle.path).ok())
                .and_then(|(root, path)| path.strip_prefix(root).ok().map(Path::to_path_buf))
                .map(|path| path.to_string_lossy().replace('\\', "/"));
            match read_path {
                Some(path) => format!(
                    "[artifact id={} size={} path={path}]",
                    handle.id, handle.size
                ),
                None => format!(
                    "[artifact id={} size={} path={} (use artifact_read with this id)]",
                    handle.id,
                    handle.size,
                    handle.path.display()
                ),
            }
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "compaction must retain the exact request budget and wire-estimation inputs"
    )]
    async fn compact_before_send<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        initial_messages: &[ProviderMessage],
        cwd: &Path,
        execution_facts: &str,
        tools: &[Value],
        mode: crate::OperatingMode,
        tokens_before: u64,
        mut next_seq: u64,
        context_window_tokens: u64,
        reserve_tokens: u64,
        reason: CompactionReason,
    ) -> Result<
        (
            Vec<ProviderMessage>,
            UsageTotals,
            u64,
            PreparedProviderRequest,
        ),
        ProviderError,
    > {
        let handle = self.compaction_handle.clone();
        let policy = handle
            .as_ref()
            .map(CompactionHandle::policy)
            .unwrap_or_default();
        let capped_policy = compaction_policy_for_window(policy.clone(), context_window_tokens);
        let selection = select_compaction_history(messages, &capped_policy).map_err(|message| {
            ProviderError::InvalidResponse {
                message: message.into(),
            }
        })?;
        let previous_summary = handle.as_ref().and_then(CompactionHandle::previous_summary);
        let manual_instructions = handle
            .as_ref()
            .and_then(CompactionHandle::manual_instructions);
        let mut summarized = selection.summarized_for_prompt();
        // Jev pruning strategy: judge and drop stale tool calls/results
        // verbatim, then let the same LLM summarize the smaller prefix. Any
        // failure or insufficient reduction falls back to the unchanged
        // prefix, never to a silently empty summary.
        if policy.strategy == crate::context::CompactionStrategy::Jev {
            let jev_plan = crate::context::jev_prune::PreparedPrune::new(
                &selection,
                manual_instructions.as_deref(),
                &summarized,
            );
            let jev_cannot_pay =
                !self.jev_plan_can_pay(&jev_plan, estimate_provider_message_tokens(&summarized));
            match &self.jev_judge {
                Some(judge) if jev_cannot_pay => {
                    let metadata = judge.metadata();
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::CompactionJevFallback {
                            detail:
                                "estimated summary savings cannot pay for Jev pre-pass with margin"
                                    .into(),
                            batches: 0,
                            batches_started: 0,
                            batches_completed: 0,
                            input_tokens: Some(0),
                            output_tokens: Some(0),
                            usage_unknown: false,
                            backend: metadata.backend,
                            requested_model: metadata.requested_model,
                            model: None,
                            duration_ms: 0,
                        },
                    )?;
                }
                Some(judge) => {
                    match crate::context::jev_prune::prune_prepared(
                        &**judge,
                        jev_plan,
                        &mut summarized,
                        self.cancellation.as_ref(),
                    )
                    .await
                    {
                        Ok(stats) => {
                            self.jev_economy.observe(&stats);
                            push_runtime_event(
                                &mut self.app,
                                &mut next_seq,
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
                            )?;
                        }
                        Err(failure) => {
                            let cancelled =
                                matches!(failure.error, crate::context::JevPruneError::Cancelled);
                            let detail = self.redact_sensitive(&failure.to_string());
                            let stats = *failure.stats;
                            self.jev_economy.observe(&stats);
                            push_runtime_event(
                                &mut self.app,
                                &mut next_seq,
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
                                },
                            )?;
                            if cancelled {
                                return Err(ProviderError::Cancelled);
                            }
                        }
                    }
                }
                None => {
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::CompactionJevFallback {
                            detail: "no Jev credential configured (set TYPESAFE_API_KEY or AI_GATEWAY_API_KEY)".into(),
                            batches: 0,
                            batches_started: 0,
                            batches_completed: 0,
                            input_tokens: Some(0),
                            output_tokens: Some(0),
                            usage_unknown: false,
                            backend: None,
                            requested_model: None,
                            model: None,
                            duration_ms: 0,
                        },
                    )?;
                }
            }
        }
        let summary_prompt = build_bounded_summary_prompt_with_checkpoint_and_instructions(
            &summarized,
            previous_summary.as_deref(),
            manual_instructions.as_deref(),
            context_window_tokens,
            reserve_tokens,
        )
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
        let summary_messages = vec![ProviderMessage::user(summary_prompt)];
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();
        let mut serialized_request = client.prepare_compaction_messages(&summary_messages)?;
        let serialized_chars = serialized_request.serialized_chars;
        let summary_request_tokens =
            self.token_estimator
                .estimate(provider, model, serialized_chars);
        serialized_request.estimated_tokens = summary_request_tokens;
        if summary_request_tokens.saturating_add(reserve_tokens) > context_window_tokens {
            return Err(ProviderError::InvalidResponse {
                message: "compaction request still exceeds context window".into(),
            });
        }
        let ProviderRequestComponents {
            system_bytes,
            history_bytes,
            tool_result_bytes,
            ..
        } = serialized_request.components;
        let ledger_start = self.app.events().len();
        push_runtime_event(
            &mut self.app,
            &mut next_seq,
            crate::EventKind::ContextSnapshot {
                request_kind: crate::RequestKind::Compaction,
                provider: provider.into(),
                model: model.into(),
                system_bytes,
                tool_schema_bytes: 0,
                history_bytes,
                tool_result_bytes,
                serialized_chars,
                estimated_tokens: summary_request_tokens,
                context_window_tokens,
            },
        )?;

        let started = Instant::now();
        let cancellation = self.cancellation.clone();
        let cancellation = async move {
            match cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let mut collected = CompactionSummary::default();
        let provider_call_journal = self.app.run_journal.clone();
        let stream_result = client
            .stream_prepared_cancellable_observed(
                serialized_request,
                cancellation,
                |event| collected.push(event),
                |telemetry| persist_provider_call(&provider_call_journal, telemetry).map(|_| ()),
            )
            .await;

        if let Some(elapsed_ms) = collected.time_to_first_byte_ms {
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
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
                &mut next_seq,
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
                &mut next_seq,
                crate::EventKind::UsageBreakdown { usage: *usage },
            )?;
        }
        if collected.usage_events.is_empty() {
            collected.usage.usage_unknown = true;
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::CompactionUsageUnknown {
                    estimated_input_tokens: summary_request_tokens,
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
                push_runtime_event(&mut self.app, &mut next_seq, kind)?;
            }
        }
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
        self.calibrate_latest_request(messages_are_text_only(&summary_messages));
        if let Err(error) = stream_result {
            return Err(self.redact_provider_error(error));
        }
        validation_result?;

        let summary = self.redact_sensitive(&collected.text);
        let summary = self
            .archive_compaction_summary(&selection, summary, execution_facts, initial_messages, cwd)
            .await?;
        let prefix_fingerprint = compaction_prefix_fingerprint(&selection.summarized);
        let compacted = apply_compaction_selection(messages, &selection, summary.clone()).map_err(
            |message| ProviderError::InvalidResponse {
                message: message.into(),
            },
        )?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut compacted_messages = compacted;
        let mut compacted_request =
            self.prepare_loop_request(client, &mut compacted_messages, tools, mode)?;
        let tokens_after =
            self.token_estimator
                .estimate(provider, model, compacted_request.serialized_chars);
        compacted_request.estimated_tokens = tokens_after;
        if let Some(handle) = handle {
            handle.commit_detailed(CompactionCommit {
                summary,
                prefix_fingerprint,
                first_kept_index: selection.first_kept_index,
                tokens_before,
                tokens_after,
                input_tokens: collected.usage.total_input_tokens(),
                output_tokens: collected.usage.output_tokens,
                duration_ms,
                reason,
                generation: 0,
            });
            handle.clear_manual();
        }
        push_runtime_event(
            &mut self.app,
            &mut next_seq,
            crate::EventKind::CompactionState {
                state: crate::context::CompactionStatus::Applied,
                reason,
                tokens_before,
                tokens_after,
                duration_ms,
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut next_seq,
            crate::EventKind::CompactionCompleted,
        )?;
        let summary_usage = usage_since(&self.app, ledger_start);
        Ok((
            compacted_messages,
            summary_usage,
            next_seq,
            compacted_request,
        ))
    }
}

/// `None` means the price of one side is unknown. Only TypeSafe's default Jev
/// model has a published static rate; custom models and Vercel stay unpriced.
fn jev_prepass_can_pay(
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
    Some(
        u128::from(max_summary_input_savings_tokens).saturating_mul(summary_price)
            > (u128::from(jev_input_tokens)
                * u128::from(crate::context::TYPESAFE_JEV_INPUT_MICROS_PER_MILLION))
                * 5
                / 4,
    )
}

fn code_intel_action_name(request: &CodeIntelRequest) -> &'static str {
    match request {
        CodeIntelRequest::Status { .. } => "status",
        CodeIntelRequest::Definition(_) => "definition",
        CodeIntelRequest::References(_) => "references",
        CodeIntelRequest::Hover(_) => "hover",
        CodeIntelRequest::Symbols(_) => "symbol",
        CodeIntelRequest::Diagnostics(_) => "diagnostics",
    }
}

async fn run_code_intel_request(
    code_intel: Option<Arc<dyn CodeIntelligence>>,
    request: Result<CodeIntelRequest, String>,
    cancellation: Option<CancellationToken>,
) -> (ToolResult, Option<crate::tools::CodeIntelPresentation>) {
    let Some(code_intel) = code_intel else {
        return (
            ToolResult {
                name: "code_intel".into(),
                success: false,
                output: "code_intel unavailable: no language-server manager configured".into(),
                artifact: None,
            },
            None,
        );
    };
    let request = match request {
        Ok(request) => request,
        Err(message) => {
            return (
                ToolResult {
                    name: "code_intel".into(),
                    success: false,
                    output: format!("code_intel: {message}"),
                    artifact: None,
                },
                None,
            );
        }
    };
    let action = code_intel_action_name(&request);
    let result = match request {
        CodeIntelRequest::Status { workspace } => code_intel.status(&workspace).await,
        CodeIntelRequest::Definition(mut query) => {
            query.cancellation = cancellation;
            code_intel.definition(&query).await
        }
        CodeIntelRequest::References(mut query) => {
            query.cancellation = cancellation;
            code_intel.references(&query).await
        }
        CodeIntelRequest::Hover(mut query) => {
            query.cancellation = cancellation;
            code_intel.hover(&query).await
        }
        CodeIntelRequest::Symbols(mut query) => {
            query.cancellation = cancellation;
            code_intel.symbols(&query).await
        }
        CodeIntelRequest::Diagnostics(mut query) => {
            query.cancellation = cancellation;
            code_intel.diagnostics(&query).await
        }
    };
    // Error payloads ("error" key) are failures: governors and turn
    // accounting must not observe them as successful tool calls.
    let success = result.payload.get("error").is_none();
    let presentation = crate::tools::presentation_for_code_intel(action, &result);
    (
        ToolResult {
            name: "code_intel".into(),
            success,
            output: presentation.full.clone(),
            artifact: None,
        },
        Some(presentation),
    )
}

fn prepared_code_intel_request(
    prepared: &PreparedToolInvocation,
) -> Result<CodeIntelRequest, String> {
    if let Some(error) = &prepared.error {
        return Err(error.clone());
    }
    match &prepared.arguments {
        PreparedToolArguments::CodeIntel(request) => Ok(request.clone()),
        _ => Err("prepared code_intel arguments are unavailable".into()),
    }
}

const TODO_PROGRESS_REVIEW: &str = "[Todo progress review] Reconcile evidenced transitions using returned IDs alongside independent work, before the final answer. Keep unfinished work pending/blocked; do not repeat work.";
const TODO_FINAL_REVIEW: &str = "[Todo final review] Before answering, reconcile task statuses using returned IDs and actual results. Leave unfinished work pending/blocked and explain it; interruption is not cancellation. Do not expand scope.";

#[derive(Default)]
struct TodoCadence {
    snapshot: Vec<crate::TodoChangedItem>,
    unchanged_batches: usize,
    reminders: usize,
    final_reviewed: bool,
}

impl TodoCadence {
    fn unfinished(items: &[crate::TodoChangedItem]) -> bool {
        items
            .iter()
            .any(|item| matches!(item.status.as_str(), "pending" | "in_progress"))
    }

    fn after_batch(&mut self, items: &[crate::TodoChangedItem]) -> bool {
        if self.snapshot != items {
            self.snapshot = items.to_vec();
            self.unchanged_batches = 0;
            if Self::unfinished(items) && self.reminders < 2 {
                self.reminders += 1;
                return true;
            }
            return false;
        }
        if !Self::unfinished(items) {
            return false;
        }
        self.unchanged_batches = self.unchanged_batches.saturating_add(1);
        if self.unchanged_batches < 4 || self.reminders >= 2 {
            return false;
        }
        self.unchanged_batches = 0;
        self.reminders += 1;
        true
    }

    fn before_final(&mut self, items: &[crate::TodoChangedItem]) -> bool {
        if self.final_reviewed || !Self::unfinished(items) {
            return false;
        }
        self.final_reviewed = true;
        true
    }
}

fn default_artifact_page_bytes() -> usize {
    8 * 1024
}

fn artifact_read_definition() -> Value {
    serde_json::json!({
        "name": "artifact_read",
        "description": "Read a verified text artifact by id from this session. Use next_offset to continue; an incomplete shell capture is marked in the artifact.",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0, "default": 0},
                "max_bytes": {"type": "integer", "minimum": 4, "maximum": 16384, "default": 8192}
            },
            "required": ["id"],
            "additionalProperties": false
        }
    })
}

fn todo_tool_definition() -> Value {
    json!({
        "name": "todo",
        "description": "Track multi-step work when needed/requested. Update at transitions and before answering, based on results, not command success alone. Ordered entries: add {title,status?}; update {id,status,reason?} using returned IDs. One in_progress: complete the previous step before starting the next. Leave unfinished work pending/blocked; give blockers a short reason (requires id). Titles do not rename. Failure retains earlier applied entries.",
        "input_schema": {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "id": {"type": ["string", "integer"], "minimum": 0},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "blocked", "cancelled"]},
                            "reason": {"type": "string", "minLength": 1, "maxLength": 1024}
                        },
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        }
    })
}

fn skill_tool_definition() -> Value {
    json!({
        "name": "skill",
        "description": "List skills with {list:true}; load SKILL.md by name. Scripts require explicit host trust and cannot run from model tool calls.",
        "input_schema": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "list": {"type": "boolean"},
                "script": {"type": "string", "description": "Filename inside skill directory; no ./ or absolute path. Default run.ps1."}
            },
            "additionalProperties": false
        }
    })
}

fn mcp_tool_definition() -> Value {
    json!({
        "name": "mcp",
        "description": "MCP bridge to configured external tool servers. {list:true} → servers+status (never connects). {server,list:true,offset} → its tools paged, offset defaults 0 (connects lazily). {server,tool,describe:true} → tool input schema. {server,tool,arguments:{...}} → call the tool.",
        "input_schema": {
            "type": "object",
            "properties": {
                "list": {"type": "boolean"},
                "server": {"type": "string"},
                "tool": {"type": "string"},
                "describe": {"type": "boolean"},
                "arguments": {"type": "object"},
                "offset": {"type": "integer", "minimum": 0}
            },
            "additionalProperties": false
        }
    })
}

/// Upper bound for one MCP call's rendered output; the provider-facing copy
/// is additionally capped by `max_result_bytes` downstream.
const MAX_MCP_CALL_OUTPUT_BYTES: usize = 64 * 1024;

async fn run_mcp_dispatch(
    manager: Option<Arc<McpManager>>,
    arguments: &str,
    cancellation: McpCancellation,
) -> ToolResult {
    fn result(output: impl Into<String>, success: bool) -> ToolResult {
        ToolResult {
            name: "mcp".into(),
            success,
            output: output.into(),
            artifact: None,
        }
    }
    let Some(manager) = manager else {
        return result(
            "mcp unavailable: no MCP servers configured (set [mcp.servers] in slim.toml)",
            false,
        );
    };
    let args: Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(error) => return result(format!("invalid mcp arguments: {error}"), false),
    };
    let usage = || {
        crate::mcp::McpError::Protocol(
            "usage: {list:true} | {server,list:true} | {server,tool,describe:true} | {server,tool,arguments}".into(),
        )
    };
    let list = match args.get("list") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return result(format!("mcp error: {}", usage()), false),
    };
    let describe = match args.get("describe") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return result(format!("mcp error: {}", usage()), false),
    };
    let offset = match args.get("offset") {
        None | Some(Value::Null) => 0usize,
        Some(value) => match value.as_u64() {
            Some(offset) => offset as usize,
            None => return result(format!("mcp error: {}", usage()), false),
        },
    };
    let server = args.get("server").and_then(Value::as_str);
    let tool = args.get("tool").and_then(Value::as_str);
    let outcome: McpRequestOutcome<McpDispatch> = match (list, server, tool, describe) {
        (true, None, None, _) => {
            McpRequestOutcome::Completed(Ok(McpDispatch::Text(manager.list_servers())))
        }
        (true, Some(server), _, _) => manager
            .list_tools_text_cancellable(server, offset, cancellation.clone())
            .await
            .map(McpDispatch::Text),
        (false, Some(server), Some(tool), true) => manager
            .describe_cancellable(server, tool, cancellation.clone())
            .await
            .map(McpDispatch::Text),
        (false, Some(server), Some(tool), false) => {
            let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
            manager
                .call_cancellable(server, tool, arguments, cancellation)
                .await
                .map(|value| render_mcp_call_result(&value))
        }
        _ => McpRequestOutcome::Completed(Err(usage())),
    };
    match outcome {
        McpRequestOutcome::Completed(Ok(McpDispatch::Text(output))) => result(output, true),
        McpRequestOutcome::Completed(Ok(McpDispatch::Call { output, is_error })) => {
            result(output, !is_error)
        }
        McpRequestOutcome::Completed(Err(error)) => result(format!("mcp error: {error}"), false),
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => result(
            format!("mcp operation stopped before tools/call was sent: {interruption:?}; transport cleanup: {cleanup:?}"),
            false,
        ),
        McpRequestOutcome::OutcomeUncertain { interruption, cleanup } => result(
            format!(
                "mcp operation outcome is uncertain: {interruption:?}; transport cleanup: {cleanup:?}; do not replay the call automatically"
            ),
            false,
        ),
    }
}

enum McpDispatch {
    Text(String),
    Call { output: String, is_error: bool },
}

/// Renders a `tools/call` result: text content concatenated, non-text items
/// serialized compactly, `isError` mapped to tool failure. Output is bounded
/// so a hostile server cannot flood the event log/journal.
fn render_mcp_call_result(value: &Value) -> McpDispatch {
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let output = match value.get("content").and_then(Value::as_array) {
        Some(content) => {
            let mut text = String::new();
            for item in content {
                if !text.is_empty() {
                    text.push('\n');
                }
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        text.push_str(item.get("text").and_then(Value::as_str).unwrap_or(""))
                    }
                    _ => text.push_str(&serde_json::to_string(item).unwrap_or_default()),
                }
                if text.len() > MAX_MCP_CALL_OUTPUT_BYTES {
                    break;
                }
            }
            text
        }
        None => serde_json::to_string_pretty(value).unwrap_or_default(),
    };
    McpDispatch::Call {
        output: truncate_result(&output, MAX_MCP_CALL_OUTPUT_BYTES),
        is_error,
    }
}

fn todo_changed_items(tracker: &crate::task::TodoTracker) -> Vec<crate::TodoChangedItem> {
    tracker
        .items()
        .iter()
        .map(|item| crate::TodoChangedItem {
            reason: item.reason.clone(),
            id: Some(item.id),
            title: item.title.clone(),
            status: todo_status_name(item.status).to_owned(),
        })
        .collect()
}

fn todo_status_name(status: crate::task::TodoStatus) -> &'static str {
    match status {
        crate::task::TodoStatus::Pending => "pending",
        crate::task::TodoStatus::InProgress => "in_progress",
        crate::task::TodoStatus::Completed => "completed",
        crate::task::TodoStatus::Blocked => "blocked",
        crate::task::TodoStatus::Cancelled => "cancelled",
    }
}

fn todo_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn parse_todo_entry(entry: &Value) -> Result<(String, TaskMutation), String> {
    let reason = entry
        .get("reason")
        .map(|value| {
            let text = value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty() && text.len() <= 1024)
                .ok_or_else(|| "reason must be nonempty text of at most 1024 bytes".to_owned())?;
            if entry.get("id").is_none() {
                return Err("reason requires an id/status update".to_owned());
            }
            Ok(text.to_owned())
        })
        .transpose()?;
    let status = entry
        .get("status")
        .map(|value| {
            value
                .as_str()
                .and_then(todo_status_from_name)
                .ok_or_else(|| {
                    "status must be pending, in_progress, completed, blocked, or cancelled"
                        .to_owned()
                })
        })
        .transpose()?;
    if let Some(value) = entry.get("id") {
        let id = todo_text(value)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| "id must be an unsigned integer returned by todo".to_owned())?;
        let status = status.ok_or_else(|| "an id update requires status".to_owned())?;
        return Ok((
            format!("todo {id}"),
            TaskMutation::TodoSetStatus {
                reason,
                id: Some(id),
                status,
            },
        ));
    }
    let title = match entry {
        Value::String(_) => todo_text(entry),
        _ => entry
            .get("content")
            .or_else(|| entry.get("title"))
            .and_then(todo_text),
    };
    if let Some(title) = title {
        return Ok((title.clone(), TaskMutation::TodoAdd { title, status }));
    }
    Err("entry requires a nonempty title/content or an id/status update".into())
}

fn parse_todo_mutations(args: &Value) -> Result<Vec<(String, TaskMutation)>, String> {
    match args.get("todos").unwrap_or(args) {
        Value::Array(entries) if entries.is_empty() => {
            Err("todos must contain at least one entry".into())
        }
        Value::Array(entries) => entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                parse_todo_entry(entry).map_err(|error| format!("entry {}: {error}", index + 1))
            })
            .collect(),
        entry => parse_todo_entry(entry).map(|entry| vec![entry]),
    }
}

fn todo_status_from_name(name: &str) -> Option<TaskTodoStatus> {
    match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "pending" | "todo" => Some(TaskTodoStatus::Pending),
        "in_progress" | "inprogress" => Some(TaskTodoStatus::InProgress),
        "completed" | "complete" | "done" => Some(TaskTodoStatus::Completed),
        "blocked" => Some(TaskTodoStatus::Blocked),
        "cancelled" | "canceled" => Some(TaskTodoStatus::Cancelled),
        _ => None,
    }
}

fn capability_error(error: CapabilityLedgerError) -> ProviderError {
    ProviderError::InvalidResponse {
        message: format!("capability: {error}"),
    }
}

fn skill_list_output(cwd: &Path, cached: Option<&DiscoveryResult>) -> ToolResult {
    if let Some(discovery) = cached {
        return render_skill_list(discovery);
    }
    let discovery = match discover_workspace(cwd) {
        Ok(discovery) => discovery,
        Err(error) => {
            return ToolResult {
                name: "skill".into(),
                success: false,
                output: format!("skill discovery failed: {error}"),
                artifact: None,
            }
        }
    };
    render_skill_list(&discovery)
}

fn render_skill_list(discovery: &DiscoveryResult) -> ToolResult {
    let mut lines: Vec<String> = discovery
        .active_entries()
        .iter()
        .map(|entry| format!("{}: {}", entry.name, entry.metadata.description))
        .collect();
    if lines.is_empty() {
        lines.push("no skills found".into());
    }
    lines.extend(discovery.diagnostic_lines());
    ToolResult {
        name: "skill".into(),
        success: true,
        output: lines.join("\n"),
        artifact: None,
    }
}

fn run_skill_dispatch(
    mode: crate::OperatingMode,
    cwd: &Path,
    arguments: &str,
    cancellation: Option<&CancellationToken>,
    process_runner: &crate::process::ProcessRunner,
    cached: Option<DiscoveryResult>,
) -> ToolResult {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(error) => {
            return ToolResult {
                name: "skill".into(),
                success: false,
                output: format!("invalid skill arguments: {error}"),
                artifact: None,
            }
        }
    };
    let list = match args.get("list") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return ToolResult {
                name: "skill".into(),
                success: false,
                output: "invalid skill arguments: list must be a boolean".into(),
                artifact: None,
            }
        }
    };
    let script = match args.get("script") {
        None => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => {
            return ToolResult {
                name: "skill".into(),
                success: false,
                output: "invalid skill arguments: script must be a string".into(),
                artifact: None,
            }
        }
    };
    if list {
        return skill_list_output(cwd, cached.as_ref());
    }
    let Some(name) = args
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return ToolResult {
            name: "skill".into(),
            success: false,
            output: "skill needs a name (or list:true)".into(),
            artifact: None,
        };
    };
    let discovery = match cached {
        Some(discovery) => discovery,
        None => {
            let Ok(discovery) = discover_workspace(cwd) else {
                return ToolResult {
                    name: "skill".into(),
                    success: false,
                    output: format!("skill discovery failed for {name}"),
                    artifact: None,
                };
            };
            discovery
        }
    };
    let Some(entry) = discovery.active(name) else {
        return ToolResult {
            name: "skill".into(),
            success: false,
            output: format!("skill not found: {name}"),
            artifact: None,
        };
    };
    if script.is_none() {
        return match crate::skills::read_body(entry.path.join("SKILL.md")) {
            Ok(body) => ToolResult {
                name: "skill".into(),
                success: true,
                output: body,
                artifact: None,
            },
            Err(error) => ToolResult {
                name: "skill".into(),
                success: false,
                output: format!("skill instructions failed: {error}"),
                artifact: None,
            },
        };
    }
    let script = crate::skills::default_skill_script(script);
    if let Some(body) = crate::skills::fallback_skill_body(&entry.path, script) {
        return ToolResult {
            name: "skill".into(),
            success: true,
            output: body,
            artifact: None,
        };
    }
    match invoke_script_with_limits_and_runner(
        process_runner,
        SkillInvocationRequest {
            skill_dir: &entry.path,
            script,
            mode,
            trusted: false,
            timeout: crate::skills::DEFAULT_SKILL_TIMEOUT,
            max_output_bytes: DEFAULT_SKILL_OUTPUT_BYTES,
            cancellation,
        },
    ) {
        Ok(output) => {
            let success = output.status.success();
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !success {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!(
                    "exit {}\n{stderr}",
                    output
                        .status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "n/a".into())
                ));
            }
            ToolResult {
                name: "skill".into(),
                success,
                output: text,
                artifact: None,
            }
        }
        Err(error) => ToolResult {
            name: "skill".into(),
            success: false,
            output: format!("skill failed: {error}"),
            artifact: None,
        },
    }
}

fn push_runtime_event(
    app: &mut AppHandle,
    next_seq: &mut u64,
    kind: crate::EventKind,
) -> Result<(), ProviderError> {
    let persistence = if let Some(journal) = &app.run_journal {
        journal
            .lock()
            .map_err(|_| journal_error("durable run lock poisoned"))
            .and_then(|mut journal| journal.record_event(&kind).map_err(journal_error))
    } else {
        Ok(())
    };
    app.push_event(crate::SessionEvent::new(*next_seq, kind))
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
    *next_seq = checked_next_seq(*next_seq)?;
    persistence?;
    Ok(())
}

fn push_tool_started_notice(
    app: &mut AppHandle,
    next_seq: &mut u64,
    batch_id: &str,
    calls: &[ProviderToolCall],
    notice: ToolStartedNotice,
) -> Result<(), ProviderError> {
    let Some(call) = calls.get(notice.index) else {
        return Err(ProviderError::InvalidResponse {
            message: format!("tool start notice index {} is out of range", notice.index),
        });
    };
    push_runtime_event(
        app,
        next_seq,
        crate::EventKind::ToolStarted {
            batch_id: batch_id.to_owned(),
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: notice.arguments,
        },
    )
}

fn drain_tool_started_notices(
    app: &mut AppHandle,
    next_seq: &mut u64,
    batch_id: &str,
    calls: &[ProviderToolCall],
    started_rx: &mut tokio::sync::mpsc::Receiver<ToolStartedNotice>,
) -> Result<(), ProviderError> {
    while let Ok(notice) = started_rx.try_recv() {
        push_tool_started_notice(app, next_seq, batch_id, calls, notice)?;
    }
    Ok(())
}

/// Persist and publish process execution facts immediately before the
/// terminal `ToolFinished` boundary. A missing receipt is expected for
/// non-native or synthetic tools and produces no event.
/// Structured facts that follow a call's `ToolOutput`: the backing process
/// and, for a successful patch, its display diff (already redacted).
fn push_tool_result_facts(
    app: &mut AppHandle,
    next_seq: &mut u64,
    batch_id: &str,
    call_id: &str,
    name: &str,
    process: Option<&crate::process::ProcessExecutionFacts>,
    edit_diff: Option<crate::ToolEditDiff>,
) -> Result<(), ProviderError> {
    if let Some(process) = process {
        push_runtime_event(
            app,
            next_seq,
            crate::EventKind::ToolProcessFinished {
                batch_id: batch_id.to_owned(),
                call_id: call_id.to_owned(),
                name: name.to_owned(),
                process: process.clone(),
            },
        )?;
    }
    if let Some(diff) = edit_diff {
        push_runtime_event(
            app,
            next_seq,
            crate::EventKind::ToolEditApplied {
                batch_id: batch_id.to_owned(),
                call_id: call_id.to_owned(),
                name: name.to_owned(),
                diff,
            },
        )?;
    }
    Ok(())
}

/// Replace only the exact admission prefix generated for a known prepared
/// invocation. This lets an in-batch evidence alias carry the current call's
/// notes without treating arbitrary tool output as a marker.
fn replace_admission_prefix(output: &mut String, from: &[String], to: &[String]) {
    if let Some(prefix) = crate::tools::admission_output_prefix(from) {
        if output.starts_with(&prefix) {
            output.drain(..prefix.len());
        }
    }
    if let Some(prefix) = crate::tools::admission_output_prefix(to) {
        output.insert_str(0, &prefix);
    }
}

fn journal_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::InvalidResponse {
        message: error.to_string(),
    }
}

pub(crate) fn persist_provider_call(
    journal: &Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
    telemetry: ProviderCallTelemetry,
) -> Result<Option<String>, ProviderError> {
    let Some(journal) = journal else {
        return Ok(None);
    };
    journal
        .lock()
        .map_err(|_| journal_error("durable run lock poisoned"))?
        .record_provider_call(&telemetry)
        .map(Some)
        .map_err(journal_error)
}

pub(crate) fn persist_provider_validation_failure(
    journal: &Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
    provider_call_id: Option<&str>,
    error: &ProviderError,
) -> Result<(), ProviderError> {
    let (Some(journal), Some(provider_call_id)) = (journal, provider_call_id) else {
        return Ok(());
    };
    journal
        .lock()
        .map_err(|_| journal_error("durable run lock poisoned"))?
        .record_provider_validation_failure(provider_call_id, error)
        .map_err(journal_error)
}

fn push_runtime_transient_event(
    app: &mut AppHandle,
    next_seq: &mut u64,
    kind: crate::EventKind,
) -> Result<(), ProviderError> {
    app.push_transient_event(crate::SessionEvent::new(*next_seq, kind))
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
    *next_seq = checked_next_seq(*next_seq)?;
    Ok(())
}

fn checked_next_seq(seq: u64) -> Result<u64, ProviderError> {
    seq.checked_add(1)
        .ok_or_else(|| ProviderError::InvalidResponse {
            message: "event sequence overflow".into(),
        })
}

fn usage_since(app: &AppHandle, start: usize) -> UsageTotals {
    UsageTotals::from_events(app.events().get(start..).unwrap_or_default(), false)
}

fn background_compaction_completed_usage(
    attempt: &PendingBackgroundCompaction,
    result: &BackgroundCompactionResult,
) -> UsageTotals {
    let mut request = attempt.usage_request.clone();
    request.system_bytes = result.plan.system_bytes;
    request.history_bytes = result.plan.history_bytes;
    request.estimated_input_tokens = result.plan.estimated_input_tokens;
    request.uncached_input_tokens = result.usage.uncached_input_tokens;
    request.cache_write_tokens = result.usage.cache_write_tokens;
    request.cache_read_tokens = result.usage.cache_read_tokens;
    request.output_tokens = result.usage.output_tokens;
    request.reasoning_tokens = result.usage.reasoning_tokens;
    request.usage_unknown = !result.usage_known;
    request.time_to_first_byte_ms = result.time_to_first_byte_ms.unwrap_or(0);
    request.time_to_first_semantic_ms = result.time_to_first_semantic_ms.unwrap_or(0);
    request.provider_latency_ms = result.duration_ms;
    UsageTotals::from_compaction_request(request)
}

fn background_compaction_cancellation_usage(
    attempt: &PendingBackgroundCompaction,
    result_plan: Option<&BackgroundCompactionPlan>,
    duration_ms: u64,
) -> UsageTotals {
    let progress = compaction_progress_snapshot(&attempt.progress);
    let mut request = attempt.usage_request.clone();
    if let Some(plan) = result_plan {
        request.system_bytes = plan.system_bytes;
        request.history_bytes = plan.history_bytes;
        request.estimated_input_tokens = plan.estimated_input_tokens;
    }
    request.usage_unknown = true;
    request.time_to_first_byte_ms = progress.time_to_first_byte_ms.unwrap_or(0);
    request.time_to_first_semantic_ms = progress.time_to_first_semantic_ms.unwrap_or(0);
    request.provider_latency_ms = duration_ms;
    request.cancelled = true;
    request.failed = true;
    UsageTotals::from_compaction_request(request)
}

async fn run_background_compaction<A: ProviderAdapter>(
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
        match observers.jev_outcome.lock() {
            Ok(mut slot) => *slot = Some(outcome),
            Err(poisoned) => *poisoned.into_inner() = Some(outcome),
        }
    }
    let cancellation_future = {
        let cancellation = cancellation.clone();
        async move {
            match cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        }
    };
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

fn rebuild_pruned_compaction_request<A: ProviderAdapter>(
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

/// `write · 4,2 KB` (or just the size while the name is unknown): the detail of a
/// `PreparingTool` phase that reports a call still being written.
fn tool_progress_detail(name: Option<&str>, bytes: u64) -> String {
    let size = if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0).replace('.', ",")
    };
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{name} · {size}"),
        None => size,
    }
}

fn update_compaction_progress(
    progress: &Arc<Mutex<CompactionAttemptProgress>>,
    event: &ProviderEvent,
) {
    let mut state = match progress.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
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

fn compaction_progress_snapshot(
    progress: &Arc<Mutex<CompactionAttemptProgress>>,
) -> CompactionAttemptProgress {
    match progress.lock() {
        Ok(state) => *state,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn is_output_limit_rejection(error: &ProviderError) -> bool {
    let message = match error {
        ProviderError::Http {
            status: 400 | 422,
            message,
            ..
        } => message,
        ProviderError::Api { metadata, message } if matches!(metadata.status, Some(400 | 422)) => {
            message
        }
        _ => return false,
    };
    let lower = message.to_ascii_lowercase();
    ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .iter()
        .any(|key| lower.contains(key))
}

fn is_context_overflow_error(error: &ProviderError) -> bool {
    if let ProviderError::Api { metadata, .. } = error {
        if metadata
            .status
            .is_some_and(|status| !matches!(status, 400 | 413 | 422))
        {
            return false;
        }
        if matches!(
            metadata.classification_code(),
            Some("context_length_exceeded" | "context_window_exceeded" | "prompt_too_long")
        ) {
            return true;
        }
        if !matches!(
            metadata.classification_code(),
            Some("invalid_request_error" | "bad_request")
        ) {
            return false;
        }
    }
    if matches!(error, ProviderError::Http { status, .. } if !matches!(status, 400 | 413 | 422)) {
        return false;
    }
    let message = match error {
        ProviderError::Api { message, .. }
        | ProviderError::TransientRemote { message }
        | ProviderError::Remote { message }
        | ProviderError::Http { message, .. } => message.as_str(),
        ProviderError::InvalidResponse { message } => message.as_str(),
        _ => return false,
    };
    let lower = message.to_ascii_lowercase();
    [
        "maximum context length",
        "context length exceeded",
        "context window exceeded",
        "exceeds context window",
        "prompt too long",
        "prompt is too long",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

// Retry only the current request. Tool calls already emitted in this request
// are not repeated. Partial assistant text is preserved and the model is told
// to continue. Auth, spend-cap, cancellation, malformed calls and empty
// completions stay terminal. Post-send transport timeouts are uncertain at the
// HTTP layer (`safe_to_retry: false`) but are safe to reissue here when no tool
// effects exist.
fn recoverable_provider_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport { .. } | ProviderError::TransientRemote { .. } => true,
        ProviderError::Api { metadata, .. } => metadata.is_transient(),
        ProviderError::Http { status, .. } => {
            matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
        }
        ProviderError::InvalidResponse { message } => matches!(
            message.as_str(),
            "provider stream ended before completion"
                | "provider stream ended without a stop reason"
        ),
        _ => false,
    }
}

const MAX_AUTOMATIC_RECOVERIES: u32 = 6;

fn is_empty_provider_response(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::InvalidResponse { message }
            if message == "provider completed without assistant text or tool calls"
    )
}

fn annotate_provider_recovery_error(
    error: ProviderError,
    scope: &str,
    attempts: u32,
    automatic_recoveries: u32,
    consecutive_recoveries: u32,
    reason: &str,
) -> ProviderError {
    let suffix = format!(
        "; automatic recovery stopped ({reason}): {scope} attempts={attempts}; automatic recoveries={automatic_recoveries}/{MAX_AUTOMATIC_RECOVERIES}; consecutive {scope} recoveries={consecutive_recoveries}/{MAX_PROVIDER_RECOVERIES}; work remains pending"
    );
    match error {
        ProviderError::Api { metadata, message } => ProviderError::Api {
            metadata,
            message: format!("{message}{suffix}"),
        },
        ProviderError::Http {
            status,
            retry_after,
            message,
        } => ProviderError::Http {
            status,
            retry_after,
            message: format!("{message}{suffix}"),
        },
        ProviderError::Transport {
            safe_to_retry,
            message,
        } => ProviderError::Transport {
            safe_to_retry,
            message: format!("{message}{suffix}"),
        },
        ProviderError::TransientRemote { message } => ProviderError::TransientRemote {
            message: format!("{message}{suffix}"),
        },
        ProviderError::Remote { message } => ProviderError::Remote {
            message: format!("{message}{suffix}"),
        },
        ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
            message: format!("{message}{suffix}"),
        },
        other => other,
    }
}

fn request_emitted_tools(app: &AppHandle, event_start: usize) -> bool {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .any(|event| {
            matches!(
                event.kind,
                crate::EventKind::ProviderToolCall { .. }
                    | crate::EventKind::ToolCall { .. }
                    | crate::EventKind::ToolOutput { .. }
                    | crate::EventKind::ToolStarted { .. }
            )
        })
}

const MAX_PROVIDER_RECOVERIES: u32 = 2;
const MAX_PROVIDER_RECOVERY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

fn provider_recovery_backoff(base: std::time::Duration, attempt: u32) -> std::time::Duration {
    let shift = attempt.saturating_sub(1).min(4);
    base.saturating_mul(1u32 << shift)
}

fn requested_provider_recovery_delay(
    error: &ProviderError,
    attempt: u32,
    base: std::time::Duration,
) -> std::time::Duration {
    let backoff = provider_recovery_backoff(base, attempt);
    match error {
        ProviderError::Api { metadata, .. } => {
            backoff.max(metadata.retry_after.unwrap_or_default())
        }
        ProviderError::Http {
            retry_after: Some(delay),
            ..
        } => backoff.max(*delay),
        _ => backoff,
    }
}

fn provider_recovery_delay(
    error: &ProviderError,
    attempt: u32,
    waited: std::time::Duration,
    base: std::time::Duration,
) -> Result<std::time::Duration, ProviderError> {
    let requested = requested_provider_recovery_delay(error, attempt, base);
    let remaining = MAX_PROVIDER_RECOVERY_WAIT.saturating_sub(waited);
    if requested > remaining {
        return Err(retry_wait_budget_exceeded(error, requested, remaining));
    }
    Ok(requested)
}

fn provider_retry_reason(error: &ProviderError) -> String {
    match error {
        ProviderError::Transport { message, .. }
        | ProviderError::TransientRemote { message }
        | ProviderError::Remote { message }
        | ProviderError::Http { message, .. }
        | ProviderError::Api { message, .. }
        | ProviderError::InvalidResponse { message } => message.clone(),
        ProviderError::MalformedToolCall => "malformed tool call".into(),
        ProviderError::Cancelled => "cancelled".into(),
    }
}

fn retry_wait_budget_exceeded(
    error: &ProviderError,
    requested: std::time::Duration,
    remaining: std::time::Duration,
) -> ProviderError {
    let suffix = format!(
        "; automatic retry requires a wait of {} ms, exceeding the remaining wait budget of {} ms; no early retry was sent; work remains pending",
        requested.as_millis(),
        remaining.as_millis()
    );
    match error {
        ProviderError::Api { metadata, message } => ProviderError::Api {
            metadata: metadata.clone(),
            message: format!("{message}{suffix}"),
        },
        ProviderError::Http {
            status,
            retry_after,
            message,
        } => ProviderError::Http {
            status: *status,
            retry_after: *retry_after,
            message: format!("{message}{suffix}"),
        },
        ProviderError::Transport {
            safe_to_retry,
            message,
        } => ProviderError::Transport {
            safe_to_retry: *safe_to_retry,
            message: format!("{message}{suffix}"),
        },
        ProviderError::TransientRemote { message } => ProviderError::TransientRemote {
            message: format!("{message}{suffix}"),
        },
        ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
            message: format!("{message}{suffix}"),
        },
        other => other.clone(),
    }
}

fn has_causal_provider_output(app: &AppHandle, event_start: usize) -> bool {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .any(|event| {
            matches!(
                &event.kind,
                crate::EventKind::AssistantTextDelta { .. }
                    | crate::EventKind::ReasoningDelta { .. }
                    | crate::EventKind::ProviderToolCall { .. }
                    | crate::EventKind::ToolCall { .. }
                    | crate::EventKind::ToolOutput { .. }
            )
        })
}

fn tool_calls_since(app: &AppHandle, event_start: usize) -> Vec<ProviderToolCall> {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ProviderToolCall {
                id,
                name,
                arguments,
            } => Some(ProviderToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            crate::EventKind::ToolCall { name, arguments } => Some(ProviderToolCall {
                id: String::new(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn validate_tool_arguments(name: &str, arguments: &str) -> Result<(), ProviderError> {
    if name.trim().is_empty()
        || !serde_json::from_str::<Value>(&normalize_tool_arguments(arguments))
            .is_ok_and(|value| value.is_object())
    {
        Err(ProviderError::MalformedToolCall)
    } else {
        Ok(())
    }
}

fn normalize_tool_arguments(raw: &str) -> Cow<'_, str> {
    let trimmed = strip_json_fence(raw.trim());
    if let Ok(Value::String(inner)) = serde_json::from_str::<Value>(trimmed) {
        if serde_json::from_str::<Value>(&inner).is_ok_and(|value| value.is_object()) {
            return Cow::Owned(inner);
        }
        if let Some(repaired) = repaired_json_object(&inner) {
            return Cow::Owned(repaired);
        }
    }
    if serde_json::from_str::<Value>(trimmed).is_ok_and(|value| value.is_object()) {
        return Cow::Borrowed(trimmed);
    }
    repaired_json_object(trimmed)
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(trimmed))
}

/// A strict-parseable object is left to the caller; otherwise only invalid
/// string escapes are repaired and the result is kept when it now parses as an
/// object. This keeps every other malformed payload failing closed.
fn repaired_json_object(candidate: &str) -> Option<String> {
    let repaired = repair_invalid_json_escapes(candidate)?;
    serde_json::from_str::<Value>(&repaired)
        .is_ok_and(|value| value.is_object())
        .then_some(repaired)
}

/// Doubles the backslash of escapes `serde_json` rejects inside strings: `\`
/// followed by a character that cannot start a valid escape, or `\u` without
/// four hex digits. Valid escapes and the surrounding structure are copied
/// verbatim, so valid JSON never changes.
fn repair_invalid_json_escapes(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut repaired = String::with_capacity(raw.len() + 8);
    let mut in_string = false;
    let mut changed = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'"' {
            in_string = !in_string;
            repaired.push('"');
            index += 1;
            continue;
        }
        if in_string && byte == b'\\' {
            if let Some(&escape) = bytes.get(index + 1) {
                if matches!(
                    escape,
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                ) {
                    repaired.push('\\');
                    repaired.push(escape as char);
                    index += 2;
                    continue;
                }
                if escape == b'u'
                    && index + 6 <= bytes.len()
                    && bytes[index + 2..index + 6]
                        .iter()
                        .all(u8::is_ascii_hexdigit)
                {
                    repaired.push_str(&raw[index..index + 6]);
                    index += 6;
                    continue;
                }
            }
            repaired.push_str("\\\\");
            changed = true;
            index += 1;
            continue;
        }
        let character = raw[index..].chars().next().expect("char boundary");
        repaired.push(character);
        index += character.len_utf8();
    }
    changed.then_some(repaired)
}

fn strip_json_fence(raw: &str) -> &str {
    let Some(rest) = raw.strip_prefix("```") else {
        return raw;
    };
    let rest = rest
        .strip_prefix("json")
        .or_else(|| rest.strip_prefix("JSON"))
        .unwrap_or(rest);
    let rest = rest.trim_start();
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(raw)
}

fn assign_missing_call_ids(calls: &mut [ProviderToolCall], batch_id: &str) {
    for (index, call) in calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("{batch_id}-tool-{index}");
        }
    }
}

fn messages_are_text_only(messages: &[ProviderMessage]) -> bool {
    messages.iter().all(|message| {
        message.responses_reasoning.is_empty()
            && message.chat_reasoning.is_none()
            && message
                .content_blocks
                .iter()
                .all(|block| matches!(block, crate::provider::ProviderContentBlock::Text(_)))
    })
}

fn estimate_unprepared_request_chars<A: ProviderAdapter>(
    adapter: &A,
    messages: &[ProviderMessage],
    tools: &[Value],
    system_prompt_override: Option<&str>,
) -> Option<u64> {
    const MESSAGE_ENVELOPE_CHARS: u64 = 256;
    const TOOL_ENVELOPE_CHARS: u64 = 128;
    // Without the adapter's envelope bound there is no estimate at all; ask
    // before scanning so adapters without one skip the whole walk.
    let request_envelope_chars = adapter.request_envelope_upper_bound_chars()?;
    let system_chars = system_prompt_override
        .or_else(|| adapter.system_prompt_for_budget())
        .map_or(0, estimate_json_string_chars);
    let message_chars = messages.iter().fold(0_u64, |total, message| {
        let scalar_chars = [
            Some(message.role.as_str()),
            Some(message.content.as_str()),
            message.name.as_deref(),
            message.tool_call_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(estimate_json_string_chars)
        .fold(0_u64, u64::saturating_add);
        let call_chars = message.tool_calls.iter().fold(0_u64, |total, call| {
            total
                .saturating_add(TOOL_ENVELOPE_CHARS)
                .saturating_add(estimate_json_string_chars(&call.id))
                .saturating_add(estimate_json_string_chars(&call.name))
                .saturating_add(estimate_json_string_chars(&call.arguments))
        });
        let block_chars = message.content_blocks.iter().fold(0_u64, |total, block| {
            let payload = match block {
                crate::provider::ProviderContentBlock::Text(text) => {
                    estimate_json_string_chars(text)
                }
                crate::provider::ProviderContentBlock::Image { media_type, data }
                | crate::provider::ProviderContentBlock::Audio { media_type, data }
                | crate::provider::ProviderContentBlock::File { media_type, data } => {
                    estimate_json_string_chars(media_type)
                        .saturating_add(estimate_json_string_chars(data))
                }
                crate::provider::ProviderContentBlock::Unsupported { kind } => {
                    estimate_json_string_chars(kind)
                }
            };
            total
                .saturating_add(MESSAGE_ENVELOPE_CHARS)
                .saturating_add(payload)
        });
        total
            .saturating_add(MESSAGE_ENVELOPE_CHARS)
            .saturating_add(scalar_chars)
            .saturating_add(call_chars)
            .saturating_add(block_chars)
            .saturating_add(message.chat_reasoning.as_ref().map_or(0, |state| {
                state.details.iter().fold(
                    estimate_json_string_chars(&state.content),
                    |total, detail| total.saturating_add(estimate_json_chars(detail)),
                )
            }))
            .saturating_add(
                message
                    .responses_reasoning
                    .iter()
                    .map(|state| estimate_json_chars(&state.item))
                    .sum::<u64>(),
            )
    });
    let tool_chars = tools
        .iter()
        .map(estimate_json_chars)
        .fold(0_u64, |total, chars| {
            total
                .saturating_add(TOOL_ENVELOPE_CHARS)
                .saturating_add(chars)
        });
    Some(
        request_envelope_chars
            .saturating_add(estimate_json_string_chars(adapter.model()))
            .saturating_add(system_chars)
            .saturating_add(message_chars)
            .saturating_add(tool_chars),
    )
}

fn estimate_json_string_chars(value: &str) -> u64 {
    // Byte scan equivalent to the per-char version: multi-byte UTF-8
    // sequences contribute 1 via the lead byte; continuation bytes add 0.
    value.bytes().fold(2_u64, |total, byte| {
        total.saturating_add(match byte {
            0x00..=0x1f => 6,
            b'"' | b'\\' => 2,
            0x80..=0xbf => 0,
            _ => 1,
        })
    })
}

fn estimate_json_chars(value: &Value) -> u64 {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number.to_string().len() as u64,
        Value::String(value) => estimate_json_string_chars(value),
        Value::Array(values) => values
            .iter()
            .map(estimate_json_chars)
            .fold(2_u64, |total, chars| {
                total.saturating_add(chars).saturating_add(1)
            }),
        Value::Object(values) => values.iter().fold(2_u64, |total, (key, value)| {
            total
                .saturating_add(estimate_json_string_chars(key))
                .saturating_add(estimate_json_chars(value))
                .saturating_add(2)
        }),
    }
}

#[cfg(test)]
fn request_component_bytes(body: &str) -> (u64, u64, u64, u64) {
    crate::provider::provider_request_component_bytes(body)
}

fn runtime_goal_assurance(events: &[crate::SessionEvent]) -> bool {
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

fn compaction_policy_for_window(
    mut policy: CompactionPolicy,
    context_window_tokens: u64,
) -> CompactionPolicy {
    policy.keep_recent_tokens = policy.keep_recent_for_window(context_window_tokens);
    policy
}

fn tool_output_already_in_context(
    messages: &[ProviderMessage],
    tool_name: &str,
    output: &str,
) -> bool {
    // Consult the actual retained history, including resumed turns. A summary
    // or another omission marker is not a replacement for the original result.
    !output.starts_with("[duplicate ")
        && messages.iter().rev().any(|message| {
            message.role == "tool"
                && message.name.as_deref() == Some(tool_name)
                && message.content_blocks.is_empty()
                && message.content == output
        })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ElisionStats {
    elided: u32,
    original_bytes: u64,
    emitted_bytes: u64,
}

fn tool_call_path(arguments: &str) -> Option<(String, String)> {
    let raw = serde_json::from_str::<Value>(arguments)
        .ok()?
        .get("path")?
        .as_str()?
        .to_owned();
    let key = crate::tools::path_identity(Path::new(&raw));
    (!key.is_empty()).then_some((key, raw))
}

/// Replace retained tool outputs whose evidence a later mutation of the same
/// path provably superseded. A `read` observed before a successful whole-file
/// `write` describes bytes that no longer exist; a failed write/patch recovery
/// body embeds the old file and is dead weight once any later mutation of that
/// path succeeded. Only live wire content is elided — durable entries keep the
/// full output — and the pointer never outlives its usefulness (reads after
/// the last write and the mutation's own success result are preserved).
fn elide_superseded_tool_outputs(messages: &mut [ProviderMessage]) -> ElisionStats {
    let mut call_paths = std::collections::HashMap::<String, (String, String)>::new();
    for message in messages
        .iter()
        .filter(|message| message.role == "assistant")
    {
        for call in &message.tool_calls {
            let Some((key, raw)) = tool_call_path(&call.arguments) else {
                continue;
            };
            call_paths.insert(call.id.clone(), (key, raw));
        }
    }
    let mut latest_write = std::collections::HashMap::<String, usize>::new();
    let mut latest_mutation = std::collections::HashMap::<String, usize>::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != "tool" || !message.content_blocks.is_empty() {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let Some((key, _)) = call_paths.get(call_id) else {
            continue;
        };
        match message.name.as_deref() {
            Some("write") if message.content.starts_with("written ") => {
                latest_write.insert(key.clone(), index);
                latest_mutation.insert(key.clone(), index);
            }
            Some("patch") if message.content.starts_with("patched ") => {
                latest_mutation.insert(key.clone(), index);
            }
            _ => {}
        }
    }
    if latest_write.is_empty() && latest_mutation.is_empty() {
        return ElisionStats::default();
    }
    let mut stats = ElisionStats::default();
    for (index, message) in messages.iter_mut().enumerate() {
        if message.role != "tool" || !message.content_blocks.is_empty() {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let Some((key, raw)) = call_paths.get(call_id) else {
            continue;
        };
        let name = message.name.as_deref().unwrap_or("");
        let pointer = match name {
            "read" if latest_write.get(key).is_some_and(|&later| later > index) => format!(
                "[superseded read output elided; {raw} was overwritten by a later write]",
                raw = raw.as_str()
            ),
            "write" | "patch"
                if write_output_is_recovery(&message.content)
                    && latest_mutation.get(key).is_some_and(|&later| later > index) =>
            {
                format!(
                    "[superseded {name} failure output elided; {raw} was updated by a later mutation]",
                    raw = raw.as_str()
                )
            }
            _ => continue,
        };
        if pointer.len() < message.content.len() {
            stats.elided = stats.elided.saturating_add(1);
            stats.original_bytes = stats
                .original_bytes
                .saturating_add(u64::try_from(message.content.len()).unwrap_or(u64::MAX));
            stats.emitted_bytes = stats
                .emitted_bytes
                .saturating_add(u64::try_from(pointer.len()).unwrap_or(u64::MAX));
            message.content = pointer;
        }
    }
    stats
}

fn truncate_result(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_owned();
    }
    let mut end = max_bytes;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &output[..end])
}

fn write_output_is_recovery(output: &str) -> bool {
    output.contains("Current file is below")
        || output.contains("Current file edges are below")
        || output.contains("Suggested unique expected:")
        || output.contains("Example context only for the first match at line ")
}

fn redact_task_value(value: &mut Value, sensitive_values: &[String]) {
    match value {
        Value::String(text) => *text = redact_values(sensitive_values, text),
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| redact_task_value(value, sensitive_values)),
        Value::Object(values) => values
            .values_mut()
            .for_each(|value| redact_task_value(value, sensitive_values)),
        _ => {}
    }
}

pub(crate) fn redact_values(sensitive_values: &[String], input: &str) -> String {
    if !sensitive_values
        .iter()
        .any(|value| input.contains(value.as_str()))
    {
        return input.to_owned();
    }
    sensitive_values
        .iter()
        .fold(input.to_owned(), |redacted, value| {
            if redacted.contains(value.as_str()) {
                redacted.replace(value, "[REDACTED]")
            } else {
                redacted
            }
        })
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
struct BufferedToolCall {
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    legacy_seen: bool,
    input_delta_seen: bool,
    malformed: bool,
}

impl BufferedToolCall {
    fn new(index: Option<u32>, id: Option<String>, name: Option<String>) -> Self {
        Self {
            index,
            id,
            name,
            arguments: String::new(),
            legacy_seen: false,
            input_delta_seen: false,
            malformed: false,
        }
    }
}

/// Converts the raw provider event stream into the runtime ledger, redacting
/// secrets on the fly and buffering streamed tool calls per adapter style.
pub(crate) struct ProviderStreamNormalizer {
    kind: ProviderKind,
    next_seq: u64,
    openai_calls: Vec<BufferedToolCall>,
    anthropic_calls: Vec<BufferedToolCall>,
    standalone_calls: Vec<BufferedToolCall>,
    sensitive_values: Vec<String>,
    text_pending: String,
    reasoning_pending: String,
    reasoning_open: bool,
    reasoning_classification: Option<crate::ReasoningClassification>,
    responses_reasoning: Vec<crate::provider::ResponsesReasoning>,
    chat_reasoning: Option<crate::provider::ChatReasoning>,
    stopped: bool,
    error: Option<ProviderError>,
    stop_reason: Option<String>,
    terminal_usage_seen: bool,
    input_usage_complete_seen: bool,
    output_usage_complete_seen: bool,
    published_tool_calls: usize,
    preparing_tool_announced: bool,
}

impl ProviderStreamNormalizer {
    pub(crate) fn new(kind: ProviderKind, next_seq: u64, sensitive_values: Vec<String>) -> Self {
        Self {
            kind,
            next_seq,
            openai_calls: Vec::new(),
            anthropic_calls: Vec::new(),
            standalone_calls: Vec::new(),
            sensitive_values,
            text_pending: String::new(),
            reasoning_pending: String::new(),
            reasoning_open: false,
            reasoning_classification: None,
            responses_reasoning: Vec::new(),
            chat_reasoning: None,
            stopped: false,
            error: None,
            stop_reason: None,
            terminal_usage_seen: false,
            input_usage_complete_seen: false,
            output_usage_complete_seen: false,
            published_tool_calls: 0,
            preparing_tool_announced: false,
        }
    }

    pub(crate) fn with_reasoning_classification(
        mut self,
        classification: Option<crate::ReasoningClassification>,
    ) -> Self {
        self.reasoning_classification = classification;
        self
    }

    fn next_seq(&self) -> u64 {
        self.next_seq
    }

    fn prune_unused_tool_slots(&mut self) {
        prune_unused_buffered_slots(&mut self.openai_calls);
        prune_unused_buffered_slots(&mut self.standalone_calls);
    }

    fn take_open_anthropic_calls(&mut self) {
        self.standalone_calls.append(&mut self.anthropic_calls);
    }

    fn argument_repair_note(&mut self) -> Option<String> {
        if self.error.is_some() || !self.stopped {
            return None;
        }
        self.take_open_anthropic_calls();
        self.prune_unused_tool_slots();
        let reason = self.stop_reason.as_deref()?;
        if classify_provider_stop_reason(reason, &self.sensitive_values).ok()?
            != ProviderTurnStop::Normal
            || !(stop_requires_tool_calls(reason)
                || (self.kind == ProviderKind::Anthropic && !self.standalone_calls.is_empty())
                || (self.kind == ProviderKind::OpenAiCodex
                    && reason.eq_ignore_ascii_case("completed")))
        {
            return None;
        }
        let mut identities = std::collections::BTreeSet::new();
        let mut notes = Vec::new();
        for call in self.openai_calls.iter().chain(&self.standalone_calls) {
            let id = call.id.as_deref().filter(|id| !id.trim().is_empty())?;
            let name = call
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())?;
            if call.malformed || !identities.insert(id) {
                return None;
            }
            let issue =
                match serde_json::from_str::<Value>(&normalize_tool_arguments(&call.arguments)) {
                    Ok(value) if value.is_object() => continue,
                    Ok(_) => "arguments must be a JSON object".into(),
                    Err(error) => error.to_string(),
                };
            notes.push(format!(
                "call {} ({}): {}; received {}",
                truncate_result(&redact_values(&self.sensitive_values, id), 128),
                truncate_result(&redact_values(&self.sensitive_values, name), 128),
                issue,
                truncate_result(&redact_values(&self.sensitive_values, &call.arguments), 512)
            ));
        }
        (!notes.is_empty()).then(|| {
            truncate_result(
                &redact_values(&self.sensitive_values, &notes.join("\n")),
                4096,
            )
        })
    }

    pub(crate) fn push(&mut self, app: &mut AppHandle, event: ProviderEvent) {
        // A protocol failure blocks content and tools, not observed usage.
        // Keep validating accounting and preserve the original failure.
        if self.error.is_some()
            && !matches!(
                &event,
                ProviderEvent::Usage { .. }
                    | ProviderEvent::UsagePartial { .. }
                    | ProviderEvent::UsageBreakdown { .. }
            )
        {
            return;
        }
        if let Err(error) = self.push_inner(app, event) {
            self.error.get_or_insert(error);
        }
    }

    fn finish(mut self, app: &mut AppHandle) -> Result<ProviderTurnResult, ProviderError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.stopped {
            return Err(ProviderError::InvalidResponse {
                message: "provider stream ended without a stop reason".into(),
            });
        }
        self.take_open_anthropic_calls();
        if !self.anthropic_calls.is_empty() {
            return Err(ProviderError::MalformedToolCall);
        }
        self.flush_text(app)?;
        let raw_stop_reason =
            self.stop_reason
                .clone()
                .ok_or_else(|| ProviderError::InvalidResponse {
                    message: "provider stream ended without a stop reason".into(),
                })?;
        let stop = classify_provider_stop_reason(&raw_stop_reason, &self.sensitive_values)?;
        let codex_completed_with_calls = self.kind == ProviderKind::OpenAiCodex
            && raw_stop_reason.trim().eq_ignore_ascii_case("completed")
            && (!self.openai_calls.is_empty() || !self.standalone_calls.is_empty());
        let anthropic_completed_with_calls =
            self.kind == ProviderKind::Anthropic && !self.standalone_calls.is_empty();
        if stop == ProviderTurnStop::Normal
            && (stop_requires_tool_calls(&raw_stop_reason)
                || codex_completed_with_calls
                || anthropic_completed_with_calls)
        {
            self.prune_unused_tool_slots();
            let mut calls = std::mem::take(&mut self.openai_calls);
            calls.sort_by_key(|call| call.index.unwrap_or(u32::MAX));
            calls.extend(std::mem::take(&mut self.standalone_calls));
            if calls.is_empty() && self.published_tool_calls == 0 {
                return Err(ProviderError::InvalidResponse {
                    message: "provider required tool execution but emitted no complete tool call"
                        .into(),
                });
            }
            let mut call_ids = std::collections::BTreeSet::new();
            for call in &calls {
                validate_buffered_call(call)?;
                if sensitive_tool_arguments(&call.arguments, &self.sensitive_values)
                    || [
                        call.id.as_deref().unwrap_or(""),
                        call.name.as_deref().unwrap_or(""),
                    ]
                    .iter()
                    .any(|value| {
                        self.sensitive_values
                            .iter()
                            .any(|secret| !secret.is_empty() && value.contains(secret))
                    })
                {
                    return Err(ProviderError::InvalidResponse {
                        message: "tool call contains registered sensitive material; use a configured credential reference".into(),
                    });
                }
                if call.id.as_deref().is_some_and(|id| !call_ids.insert(id)) {
                    return Err(ProviderError::MalformedToolCall);
                }
            }
            for call in calls {
                publish_buffered_call(app, &mut self.next_seq, call)?;
                self.published_tool_calls = self.published_tool_calls.saturating_add(1);
            }
        } else {
            self.openai_calls.clear();
            self.standalone_calls.clear();
        }
        push_runtime_event(
            app,
            &mut self.next_seq,
            crate::EventKind::AssistantEnded {
                reason: redact_values(&self.sensitive_values, &raw_stop_reason),
            },
        )?;
        // Tool side effects of an aborted turn must not run: only a normal
        // stop lets the loop execute the buffered tool calls.
        let blocks_tools = !matches!(stop, ProviderTurnStop::Normal);
        Ok(ProviderTurnResult {
            next_seq: self.next_seq,
            blocks_tools,
            stop,
            responses_reasoning: self.responses_reasoning,
            chat_reasoning: self.chat_reasoning,
        })
    }

    /// Lossless finish used by the provider-facing one-shot runner: returns
    /// the next sequence without the turn classification.
    pub(crate) fn finish_free(self, app: &mut AppHandle) -> Result<u64, ProviderError> {
        if self.error.is_some() {
            return self.finish(app).map(|turn| turn.next_seq);
        }
        let raw_stop_reason =
            self.stop_reason
                .as_deref()
                .ok_or_else(|| ProviderError::InvalidResponse {
                    message: "provider stream ended without a stop reason".into(),
                })?;
        if classify_provider_stop_reason(raw_stop_reason, &self.sensitive_values)?
            != ProviderTurnStop::Normal
        {
            return Err(ProviderError::InvalidResponse {
                message: "provider did not complete successfully".into(),
            });
        }
        let turn = self.finish(app)?;
        Ok(turn.next_seq)
    }

    fn push_inner(
        &mut self,
        app: &mut AppHandle,
        event: ProviderEvent,
    ) -> Result<(), ProviderError> {
        if self.stopped
            && !matches!(
                &event,
                ProviderEvent::Usage { .. }
                    | ProviderEvent::UsagePartial { .. }
                    | ProviderEvent::UsageBreakdown { .. }
                    | ProviderEvent::Stopped { .. }
            )
        {
            return Err(ProviderError::InvalidResponse {
                message: "provider emitted events after stop".into(),
            });
        }
        if event_closes_reasoning(&event) {
            self.close_reasoning(app)?;
        }
        match event {
            ProviderEvent::Phase { phase, elapsed_ms } => push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::ProviderPhase {
                    phase,
                    elapsed_ms,
                    detail: None,
                },
            ),
            // The call is still being written: show it as the preparing phase with
            // its size. Transient (not journaled, coalescible), the same event the
            // final, held tool call later announces without a size.
            ProviderEvent::ToolCallProgress { name, bytes } => push_runtime_transient_event(
                app,
                &mut self.next_seq,
                crate::EventKind::ProviderPhase {
                    phase: crate::provider::ProviderPhase::PreparingTool,
                    elapsed_ms: 0,
                    detail: Some(tool_progress_detail(name.as_deref(), bytes)),
                },
            ),
            ProviderEvent::ResponsesReasoning(state) => {
                self.responses_reasoning.push(state);
                Ok(())
            }
            ProviderEvent::ChatReasoning(state) => {
                if let Some(previous) = self.chat_reasoning.as_mut() {
                    if previous.scope_id != state.scope_id || previous.model != state.model {
                        return Err(ProviderError::InvalidResponse {
                            message: "Chat reasoning scope changed during a response".into(),
                        });
                    }
                    previous.content.push_str(&state.content);
                    previous.details.extend(state.details);
                } else {
                    self.chat_reasoning = Some(state);
                }
                Ok(())
            }
            ProviderEvent::TextDelta(text) => {
                let text = take_redacted_stream_chunk(
                    &mut self.text_pending,
                    &text,
                    &self.sensitive_values,
                    false,
                );
                if text.is_empty() {
                    Ok(())
                } else {
                    push_runtime_event(
                        app,
                        &mut self.next_seq,
                        crate::EventKind::AssistantTextDelta { text },
                    )
                }
            }
            ProviderEvent::ReasoningDelta(text) => {
                self.open_reasoning(app)?;
                let text = take_redacted_stream_chunk(
                    &mut self.reasoning_pending,
                    &text,
                    &self.sensitive_values,
                    false,
                );
                if text.is_empty() {
                    Ok(())
                } else {
                    push_runtime_event(
                        app,
                        &mut self.next_seq,
                        crate::EventKind::ReasoningDelta { text },
                    )
                }
            }
            ProviderEvent::ReasoningStarted => self.open_reasoning(app),
            ProviderEvent::ReasoningEnded => self.close_reasoning(app),
            ProviderEvent::UsageBreakdown { usage } => push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::UsageBreakdown { usage },
            ),
            ProviderEvent::ResponseCacheHit => {
                push_runtime_event(app, &mut self.next_seq, crate::EventKind::ResponseCacheHit)
            }
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                if self.terminal_usage_seen {
                    return Err(ProviderError::InvalidResponse {
                        message: "provider emitted terminal usage more than once".into(),
                    });
                }
                self.terminal_usage_seen = true;
                push_runtime_event(
                    app,
                    &mut self.next_seq,
                    crate::EventKind::Usage {
                        input_tokens,
                        output_tokens,
                    },
                )
            }
            ProviderEvent::UsagePartial {
                input_tokens,
                output_tokens,
                input_complete,
                output_complete,
            } => {
                if (input_complete && self.input_usage_complete_seen)
                    || (output_complete && self.output_usage_complete_seen)
                {
                    return Err(ProviderError::InvalidResponse {
                        message: "provider repeated a complete usage component".into(),
                    });
                }
                self.input_usage_complete_seen |= input_complete;
                self.output_usage_complete_seen |= output_complete;
                push_runtime_event(
                    app,
                    &mut self.next_seq,
                    crate::EventKind::UsagePartial {
                        input_tokens,
                        output_tokens,
                        input_known: input_complete,
                        output_known: output_complete,
                    },
                )
            }
            ProviderEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } if matches!(
                self.kind,
                ProviderKind::OpenAiCompatible | ProviderKind::OpenAiCodex
            ) =>
            {
                let preparing_name = (!self.preparing_tool_announced)
                    .then(|| name.as_deref().map(str::trim))
                    .flatten()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                append_openai_delta(&mut self.openai_calls, index, id, name, arguments)?;
                if let Some(name) = preparing_name {
                    self.preparing_tool_announced = true;
                    push_runtime_event(
                        app,
                        &mut self.next_seq,
                        crate::EventKind::ProviderPhase {
                            phase: crate::provider::ProviderPhase::PreparingTool,
                            elapsed_ms: 0,
                            detail: Some(redact_values(&self.sensitive_values, &name)),
                        },
                    )?;
                }
                Ok(())
            }
            ProviderEvent::ToolCallComplete {
                index,
                id,
                name,
                arguments,
            } if self.kind == ProviderKind::OpenAiCodex => {
                append_openai_delta(
                    &mut self.openai_calls,
                    Some(index),
                    Some(id),
                    Some(name.clone()),
                    String::new(),
                )?;
                let call = self
                    .openai_calls
                    .iter_mut()
                    .find(|call| call.index == Some(index))
                    .ok_or(ProviderError::MalformedToolCall)?;
                if call.malformed {
                    return Err(ProviderError::MalformedToolCall);
                }
                // The final item is a snapshot of this identified call, not a
                // second call or a fragment to append. Never match by content.
                attach_legacy_call(std::slice::from_mut(call), &name, &arguments)?;
                // Validate arguments at the terminal boundary so a complete,
                // identified but invalid JSON call can be repaired by the loop.
                Ok(())
            }
            ProviderEvent::ToolCallStart { index, id, name }
                if self.kind == ProviderKind::Anthropic =>
            {
                start_anthropic_call(&mut self.anthropic_calls, index, id, name.clone())?;
                push_runtime_event(
                    app,
                    &mut self.next_seq,
                    crate::EventKind::ProviderPhase {
                        phase: crate::provider::ProviderPhase::PreparingTool,
                        elapsed_ms: 0,
                        detail: Some(redact_values(&self.sensitive_values, &name)),
                    },
                )
            }
            ProviderEvent::ToolCallInputDelta {
                index,
                partial_json,
            } if self.kind == ProviderKind::Anthropic => {
                let Some(call) = self
                    .anthropic_calls
                    .iter_mut()
                    .find(|call| call.index == Some(index))
                else {
                    return Err(ProviderError::MalformedToolCall);
                };
                if call.legacy_seen {
                    call.arguments.clear();
                    call.legacy_seen = false;
                }
                call.input_delta_seen = true;
                call.arguments.push_str(&partial_json);
                Ok(())
            }
            ProviderEvent::ContentBlockStop { index } if self.kind == ProviderKind::Anthropic => {
                let Some(position) = self
                    .anthropic_calls
                    .iter()
                    .position(|call| call.index == Some(index))
                else {
                    return Ok(());
                };
                // Buffer the whole batch until the message's terminal reason
                // is known. A malformed sibling must prevent every execution.
                self.standalone_calls
                    .push(self.anthropic_calls.remove(position));
                Ok(())
            }
            ProviderEvent::ToolCall { name, arguments } => {
                if self.kind == ProviderKind::Anthropic
                    && attach_legacy_call(&mut self.anthropic_calls, &name, &arguments)?
                {
                    return Ok(());
                }
                if matches!(
                    self.kind,
                    ProviderKind::OpenAiCompatible | ProviderKind::OpenAiCodex
                ) && attach_legacy_call(&mut self.openai_calls, &name, &arguments)?
                {
                    return Ok(());
                }
                validate_tool_arguments(&name, &arguments)?;
                self.standalone_calls.push(BufferedToolCall {
                    index: None,
                    id: None,
                    name: Some(name),
                    arguments,
                    legacy_seen: true,
                    input_delta_seen: false,
                    malformed: false,
                });
                Ok(())
            }
            ProviderEvent::Stopped { reason } => {
                if let Some(existing) = &self.stop_reason {
                    if existing.trim().eq_ignore_ascii_case(reason.trim()) {
                        return Ok(());
                    }
                    return Err(ProviderError::InvalidResponse {
                        message: "provider emitted more than one stop reason".into(),
                    });
                }
                self.stop_reason = Some(reason);
                self.flush_text(app)?;
                self.stopped = true;
                Ok(())
            }
            ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallComplete { .. }
            | ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallInputDelta { .. }
            | ProviderEvent::ContentBlockStop { .. } => Err(ProviderError::MalformedToolCall),
        }
    }

    fn flush_text(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        self.flush_assistant(app)?;
        self.flush_reasoning(app)
    }

    fn flush_assistant(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        let text =
            take_redacted_stream_chunk(&mut self.text_pending, "", &self.sensitive_values, true);
        if !text.is_empty() {
            push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::AssistantTextDelta { text },
            )?;
        }
        Ok(())
    }

    fn flush_reasoning(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        let reasoning = take_redacted_stream_chunk(
            &mut self.reasoning_pending,
            "",
            &self.sensitive_values,
            true,
        );
        if !reasoning.is_empty() {
            push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::ReasoningDelta { text: reasoning },
            )?;
        }
        Ok(())
    }

    fn open_reasoning(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        self.flush_assistant(app)?;
        if self.reasoning_open {
            return Ok(());
        }
        if let Some(classification) = self.reasoning_classification.take() {
            push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::ReasoningClassification { classification },
            )?;
        }
        push_runtime_event(app, &mut self.next_seq, crate::EventKind::ThinkingStarted)?;
        self.reasoning_open = true;
        Ok(())
    }

    fn close_reasoning(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        self.flush_reasoning(app)?;
        if !std::mem::take(&mut self.reasoning_open) {
            return Ok(());
        }
        push_runtime_event(app, &mut self.next_seq, crate::EventKind::ThinkingEnded)
    }
}

fn stop_requires_tool_calls(reason: &str) -> bool {
    matches!(
        reason.trim().to_ascii_lowercase().as_str(),
        "tool_calls" | "function_call" | "tool_use"
    )
}

fn take_redacted_stream_chunk(
    pending: &mut String,
    delta: &str,
    sensitive_values: &[String],
    flush: bool,
) -> String {
    pending.push_str(delta);
    if sensitive_values.is_empty() {
        return std::mem::take(pending);
    }
    let split_at = if flush {
        pending.len()
    } else {
        safe_stream_split(pending, sensitive_values)
    };
    let tail = pending[split_at..].to_owned();
    let ready = redact_values(sensitive_values, &pending[..split_at]);
    *pending = tail;
    ready
}

fn safe_stream_split(input: &str, sensitive_values: &[String]) -> usize {
    let held_bytes = sensitive_values
        .iter()
        .flat_map(|value| {
            value
                .char_indices()
                .skip(1)
                .map(move |(index, _)| &value[..index])
        })
        .filter(|prefix| input.ends_with(prefix))
        .map(str::len)
        .max()
        .unwrap_or(0);
    let mut split_at = input.len().saturating_sub(held_bytes);
    loop {
        let adjusted = sensitive_values
            .iter()
            .flat_map(|value| {
                input
                    .match_indices(value)
                    .map(move |(start, _)| (start, start + value.len()))
            })
            .filter(|(start, end)| *start < split_at && split_at < *end)
            .map(|(start, _)| start)
            .min()
            .unwrap_or(split_at);
        if adjusted == split_at {
            return split_at;
        }
        split_at = adjusted;
    }
}

fn event_closes_reasoning(event: &ProviderEvent) -> bool {
    match event {
        ProviderEvent::TextDelta(text) => !text.trim().is_empty(),
        ProviderEvent::ToolCallDelta { name, .. } => {
            name.as_deref().is_some_and(|name| !name.trim().is_empty())
        }
        ProviderEvent::ToolCallStart { .. }
        | ProviderEvent::ToolCallInputDelta { .. }
        | ProviderEvent::ToolCallComplete { .. }
        | ProviderEvent::ToolCall { .. }
        | ProviderEvent::ContentBlockStop { .. }
        | ProviderEvent::Stopped { .. } => true,
        _ => false,
    }
}

fn buffered_call_has_name(call: &BufferedToolCall) -> bool {
    call.name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

fn buffered_call_is_complete(call: &BufferedToolCall) -> bool {
    if call.malformed {
        return false;
    }
    buffered_call_has_name(call)
        && serde_json::from_str::<Value>(&normalize_tool_arguments(&call.arguments))
            .is_ok_and(|value| value.is_object())
}

fn prune_unused_buffered_slots(calls: &mut Vec<BufferedToolCall>) {
    if !calls.iter().any(buffered_call_has_name) {
        return;
    }
    let has_complete = calls.iter().any(buffered_call_is_complete);
    calls.retain(|call| {
        if call.malformed || buffered_call_is_complete(call) {
            return true;
        }
        if has_complete
            && call.arguments.trim().is_empty()
            && call.id.as_deref().is_none_or(|id| id.trim().is_empty())
        {
            return false;
        }
        buffered_call_has_name(call) || !call.arguments.trim().is_empty()
    });
}

/// Share the executable call assembler with the transport's secret gate so
/// index/id fallback and repeated headers cannot change redaction semantics.
pub(crate) fn tool_events_contain_sensitive_values(
    events: &[ProviderEvent],
    secrets: &[String],
) -> bool {
    if secrets.is_empty() {
        return false;
    }
    let sensitive = |value: &str| {
        secrets
            .iter()
            .any(|secret| !secret.is_empty() && value.contains(secret))
    };
    let mut calls = Vec::new();
    for event in events {
        let assembled = match event {
            ProviderEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => append_openai_delta(
                &mut calls,
                *index,
                id.clone(),
                name.clone(),
                arguments.clone(),
            ),
            ProviderEvent::ToolCallStart { index, id, name } => append_openai_delta(
                &mut calls,
                Some(*index),
                Some(id.clone()),
                Some(name.clone()),
                String::new(),
            ),
            ProviderEvent::ToolCallInputDelta {
                index,
                partial_json,
            } => append_openai_delta(&mut calls, Some(*index), None, None, partial_json.clone()),
            ProviderEvent::ToolCallComplete {
                id,
                name,
                arguments,
                ..
            } => {
                if sensitive(id) || sensitive(name) || sensitive_tool_arguments(arguments, secrets)
                {
                    return true;
                }
                Ok(())
            }
            ProviderEvent::ToolCall { name, arguments } => {
                if sensitive(name) || sensitive_tool_arguments(arguments, secrets) {
                    return true;
                }
                Ok(())
            }
            _ => Ok(()),
        };
        if assembled.is_err() {
            return true;
        }
    }
    calls.iter().any(|call| {
        sensitive_tool_arguments(&call.arguments, secrets)
            || sensitive(call.id.as_deref().unwrap_or(""))
            || sensitive(call.name.as_deref().unwrap_or(""))
    })
}

fn sensitive_tool_arguments(arguments: &str, secrets: &[String]) -> bool {
    // `normalize_tool_arguments` unwraps one JSON-string layer at publish
    // time, so the gate cannot stop at a single decode: a string value that
    // itself parses as JSON is checked at every level the executor can reach.
    // Decoded text strictly shrinks per level and nested JSON quoting grows
    // ~2x outward, so real payloads stay far below this bound.
    const MAX_UNWRAP_DEPTH: u32 = 8;
    fn contains(value: &Value, secret: &str, depth: u32) -> bool {
        match value {
            Value::String(text) => {
                text.contains(secret)
                    || (depth > 0
                        && serde_json::from_str::<Value>(text)
                            .is_ok_and(|inner| contains(&inner, secret, depth - 1)))
            }
            Value::Array(values) => values.iter().any(|value| contains(value, secret, depth)),
            Value::Object(values) => values
                .iter()
                .any(|(key, value)| key.contains(secret) || contains(value, secret, depth)),
            _ => value.to_string().contains(secret),
        }
    }
    let mut parsed = None;
    secrets.iter().any(|secret| {
        !secret.is_empty()
            && (arguments.contains(secret)
                || parsed
                    .get_or_insert_with(|| serde_json::from_str::<Value>(arguments).ok())
                    .as_ref()
                    .is_some_and(|value| contains(value, secret, MAX_UNWRAP_DEPTH)))
    })
}

fn append_openai_delta(
    calls: &mut Vec<BufferedToolCall>,
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
) -> Result<(), ProviderError> {
    let id = id.filter(|id| !id.is_empty());
    let name = name.filter(|name| !name.trim().is_empty());
    if index.is_none() && id.is_none() && name.is_none() && arguments.is_empty() {
        let mut call = BufferedToolCall::new(None, None, None);
        call.malformed = true;
        calls.push(call);
        return Ok(());
    }
    let by_index = index.and_then(|value| calls.iter().position(|call| call.index == Some(value)));
    let by_id = id.as_ref().and_then(|value| {
        calls
            .iter()
            .position(|call| call.id.as_ref() == Some(value))
    });
    if let (Some(index_position), Some(id_position)) = (by_index, by_id) {
        if index_position != id_position {
            let mut call = BufferedToolCall::new(index, id, name);
            call.arguments = arguments;
            call.malformed = true;
            calls.push(call);
            return Ok(());
        }
    }
    let position = if let Some(position) = by_index.or(by_id) {
        position
    } else if index.is_none() && id.is_none() {
        let candidates = calls
            .iter()
            .enumerate()
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [position] => *position,
            [] if name.is_some() => {
                calls.push(BufferedToolCall::new(index, id.clone(), name.clone()));
                calls.len() - 1
            }
            _ => {
                let mut call = BufferedToolCall::new(index, id, name);
                call.arguments = arguments;
                call.malformed = true;
                calls.push(call);
                return Ok(());
            }
        }
    } else {
        if calls.iter().any(|call| {
            index.is_some_and(|value| call.index == Some(value))
                || id
                    .as_ref()
                    .is_some_and(|value| call.id.as_ref() == Some(value))
        }) {
            let mut call = BufferedToolCall::new(index, id, name);
            call.arguments = arguments;
            call.malformed = true;
            calls.push(call);
            return Ok(());
        }
        calls.push(BufferedToolCall::new(index, id.clone(), name.clone()));
        calls.len() - 1
    };
    let call = &mut calls[position];
    if let (Some(existing), Some(incoming)) = (&call.index, index) {
        if *existing != incoming {
            call.malformed = true;
        }
    } else if call.index.is_none() {
        call.index = index;
    }
    if let (Some(existing), Some(incoming)) = (&call.id, &id) {
        if existing != incoming {
            call.malformed = true;
        }
    } else if call.id.is_none() {
        call.id = id;
    }
    if let Some(incoming) = name {
        if let Some(existing) = &call.name {
            if existing != &incoming {
                call.malformed = true;
            }
        } else {
            call.name = Some(incoming);
        }
    }
    call.arguments.push_str(&arguments);
    Ok(())
}

fn start_anthropic_call(
    calls: &mut Vec<BufferedToolCall>,
    index: u32,
    id: String,
    name: String,
) -> Result<(), ProviderError> {
    if id.is_empty()
        || name.is_empty()
        || calls
            .iter()
            .any(|call| call.index == Some(index) || call.id.as_deref() == Some(id.as_str()))
    {
        return Err(ProviderError::MalformedToolCall);
    }
    calls.push(BufferedToolCall::new(Some(index), Some(id), Some(name)));
    Ok(())
}

fn attach_legacy_call(
    calls: &mut [BufferedToolCall],
    name: &str,
    arguments: &str,
) -> Result<bool, ProviderError> {
    let matches = calls
        .iter()
        .enumerate()
        .filter(|(_, call)| call.name.as_deref() == Some(name))
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    let exact_matches = matches
        .iter()
        .copied()
        .filter(|position| calls[*position].arguments == arguments)
        .collect::<Vec<_>>();
    let matches = if exact_matches.len() == 1 {
        exact_matches
    } else {
        matches
    };
    let Some(position) = (match matches.as_slice() {
        [] => return Ok(false),
        [position] => Some(*position),
        _ => return Err(ProviderError::MalformedToolCall),
    }) else {
        return Ok(false);
    };
    let call = &mut calls[position];
    if call.input_delta_seen {
        return Ok(true);
    }
    if call.arguments.is_empty() || call.arguments == "{}" {
        arguments.clone_into(&mut call.arguments);
        call.legacy_seen = true;
    } else if call.arguments != arguments {
        if serde_json::from_str::<Value>(&call.arguments).is_err()
            && serde_json::from_str::<Value>(arguments).is_ok()
        {
            arguments.clone_into(&mut call.arguments);
            call.legacy_seen = true;
        } else {
            return Err(ProviderError::MalformedToolCall);
        }
    }
    Ok(true)
}

fn publish_buffered_call(
    app: &mut AppHandle,
    next_seq: &mut u64,
    call: BufferedToolCall,
) -> Result<(), ProviderError> {
    validate_buffered_call(&call)?;
    let Some(name) = call.name.filter(|name| !name.trim().is_empty()) else {
        return Err(ProviderError::MalformedToolCall);
    };
    let arguments = normalize_tool_arguments(&call.arguments).into_owned();
    validate_tool_arguments(&name, &arguments)?;
    push_runtime_event(
        app,
        next_seq,
        crate::EventKind::ProviderToolCall {
            id: call.id.unwrap_or_default(),
            name,
            arguments,
        },
    )
}

fn validate_buffered_call(call: &BufferedToolCall) -> Result<(), ProviderError> {
    if call.malformed {
        return Err(ProviderError::MalformedToolCall);
    }
    validate_tool_arguments(call.name.as_deref().unwrap_or_default(), &call.arguments)
}

#[cfg(test)]
mod tests;
