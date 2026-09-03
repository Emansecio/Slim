mod app_handle;
mod capability_bridge;
mod governor;
mod loop_guard;
mod mode;
mod queue;
mod usage;

pub use usage::{RequestUsage, UsageTotals};

use crate::codeintel::CodeIntelligence;
use crate::context::{
    apply_compaction_selection, build_bounded_summary_prompt_with_checkpoint,
    compaction_prefix_fingerprint, estimate_provider_message_tokens, has_compactable_history,
    local_emergency_summary, select_compaction_history, AdaptiveTokenEstimator, ArtifactHandle,
    ArtifactStore, CompactionCommit, CompactionHandle, CompactionPolicy, CompactionReason,
    CompactionSelection, ContextBudget, PreparedCompaction, COMPACTION_SYSTEM_PROMPT,
};
use crate::interaction::{
    ask_question_definition, AskQuestion, InteractionRequestId, InteractionRoute,
};
use crate::mcp::McpCatalog;
use crate::model::AppHandle;
use crate::provider::{
    HttpProviderClient, PreparedProviderRequest, ProviderAdapter, ProviderError, ProviderEvent,
    ProviderKind, ProviderMessage, ProviderPhase, ProviderRequestComponents, ProviderToolCall,
};
use crate::session::{
    AuthorizationGrant, CapabilityCatalog, CapabilityLedgerError, DurableRepoLike,
    DurableSessionHeader, MemoryRepo, TaskMutation, TaskMutationRequest, TaskTodoStatus,
};
use crate::skills::{
    discover_workspace, invoke_script_with_limits_and_runner, DiscoveryResult,
    SkillInvocationRequest, DEFAULT_SKILL_OUTPUT_BYTES,
};
use crate::tools::{
    render_code_intel, CodeIntelRequest, PreparedToolArguments, PreparedToolInvocation,
    ToolEffectClass, ToolExecutionOutcome, ToolExecutionReceipt, ToolRegistry, ToolResult,
};
use futures_util::StreamExt;
use governor::{CausalGovernor, GovernorObservation};
use serde_json::{json, Value};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Notify;

pub use app_handle::RuntimeHandle;
pub use capability_bridge::{
    execute_native_tool, InProcessCapabilityAdapter, RuntimeCapabilityAdapter,
    RuntimeCapabilityBridge, RuntimeCapabilityTarget,
};
pub use loop_guard::LoopGuard;
pub use mode::mode_name;
pub use queue::PromptQueue;

#[derive(Clone)]
pub struct CancellationToken(Arc<CancellationState>);

struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(CancellationState {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }))
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::AcqRel) {
            self.0.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        let notified = self.0.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
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
    /// Per-turn mutating batch cap (write/patch/shell/todo/skill/â€¦). Not a run total.
    pub max_mutating_tool_calls: usize,
    /// Per-turn read batch cap (read/list/search). Not a run total.
    pub max_read_tool_calls: usize,
    pub max_result_bytes: usize,
    pub context_window_tokens: u64,
    pub context_reserve_tokens: u64,
    pub context_compaction_enabled: bool,
}

impl AgentLoopConfig {
    pub const DEFAULT_MAX_TURNS: usize = 128;
    pub const DEFAULT_MAX_MUTATING_TOOL_CALLS: usize = 32;
    pub const DEFAULT_MAX_READ_TOOL_CALLS: usize = 96;
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_turns: Self::DEFAULT_MAX_TURNS,
            max_mutating_tool_calls: Self::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            max_read_tool_calls: Self::DEFAULT_MAX_READ_TOOL_CALLS,
            max_result_bytes: 16 * 1024,
            context_window_tokens: 32_000,
            context_reserve_tokens: 4_096,
            context_compaction_enabled: true,
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

pub fn tool_call_is_read_only(name: &str) -> bool {
    matches!(tool_call_bucket(name), ToolCallBucket::Read)
}

fn tool_call_is_parallel_snapshot_read(tools: &ToolRegistry, name: &str) -> bool {
    name == "code_intel"
        || tools
            .operational_spec(name)
            .is_some_and(|spec| spec.effect_class == ToolEffectClass::SnapshotRead)
}

/// Parallel-segment predicate over the *prepared* invocation: snapshot reads
/// plus allowlisted validation shells (e.g. `cargo check`), whose effect
/// class is only known after argument parsing. Budgets are untouched —
/// validation stays in the mutating bucket via [`tool_call_bucket`].
fn tool_call_is_parallel_read(
    tools: &ToolRegistry,
    prepared: &PreparedToolInvocation,
    name: &str,
) -> bool {
    tool_call_is_parallel_snapshot_read(tools, name)
        || (prepared.error.is_none()
            && prepared
                .spec
                .is_some_and(|spec| spec.effect_class == ToolEffectClass::Validation))
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

fn truncate_calls_for_budget(calls: &mut Vec<ProviderToolCall>, config: AgentLoopConfig) -> usize {
    let original_len = calls.len();
    let mut read_used = 0usize;
    let mut mutating_used = 0usize;
    calls.retain(|call| match tool_call_bucket(&call.name) {
        ToolCallBucket::Read => {
            if read_used < config.max_read_tool_calls {
                read_used += 1;
                true
            } else {
                false
            }
        }
        ToolCallBucket::Mutating => {
            if mutating_used < config.max_mutating_tool_calls {
                mutating_used += 1;
                true
            } else {
                false
            }
        }
    });
    original_len.saturating_sub(calls.len())
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
}

const COMPACTION_MAX_OUTPUT_TOKENS: u64 = 2_048;

/// Best-effort closing turn after a budget stop. Sent with `tools=[]` so the
/// model answers with what it already knows instead of the user only seeing
/// `Turn limit reached / Tool budget exhausted`. Failures are ignored and the
/// original `stop` is preserved.
const BUDGET_FINALIZE_PROMPT: &str = "Budget exhausted. Respond now as the final answer with what is already known: outcome, changed files/behavior, validation result, remaining risks. Do not call tools.";

/// Between-turns steer (Pit-inspired, without mid-stream abort): when a turn
/// reuses byte-identical evidence already in context, nudge the next provider
/// turn to act instead of re-reading. Capped so it can never inflate context.
const MAX_BUDGET_STEERS: usize = 2;
const BUDGET_STEER_PROMPT: &str = "Identical tool output is already in context (duplicate omitted). Do not re-read the same target; act on what you have or give the final answer now.";

struct BackgroundCompactionPlan {
    selection: CompactionSelection,
    request: Option<PreparedProviderRequest>,
    provider: String,
    model: String,
    provider_identity: String,
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
    progress: Arc<Mutex<CompactionAttemptProgress>>,
    usage_request: RequestUsage,
    request_bytes: u64,
    estimated_input_tokens: u64,
    tokens_before: u64,
    started: Instant,
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
            | ProviderEvent::ReasoningStarted
            | ProviderEvent::ReasoningEnded
            | ProviderEvent::Phase { .. }
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
        if self.text.trim().is_empty() {
            return Err(ProviderError::InvalidResponse {
                message: "summary provider returned an empty summary".into(),
            });
        }
        if self.text.len() > max_bytes {
            return Err(ProviderError::InvalidResponse {
                message: format!(
                    "summary provider exceeded the configured limit of {max_bytes} bytes"
                ),
            });
        }
        let mut lines = self.text.lines().map(str::trim);
        for heading in [
            "## Goal",
            "## Constraints",
            "## Progress",
            "## Blocked",
            "## Decisions",
            "## Next steps",
            "## Critical context",
        ] {
            if !lines.any(|line| line == heading) {
                return Err(ProviderError::InvalidResponse {
                    message: format!("summary provider omitted required heading: {heading}"),
                });
            }
        }
        Ok(())
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
    artifact_store: Option<ArtifactStore>,
    conversation: Vec<ProviderMessage>,
    sensitive_values: SensitiveValues,
    cancellation: Option<CancellationToken>,
    interaction_route: Option<InteractionRoute>,
    capability_bridge: Option<RuntimeCapabilityBridge<MemoryRepo>>,
    compaction_handle: Option<CompactionHandle>,
    background_compaction_enabled: bool,
    code_intel: Option<Arc<dyn CodeIntelligence>>,
    token_estimator: AdaptiveTokenEstimator,
    /// Skill discovery memoized for one loop run (`Runtime` is per-turn).
    /// `None` inside means discovery failed; callers fall back to direct
    /// discovery so error messages stay exactly as before.
    skill_discovery_cache: Option<(PathBuf, Option<DiscoveryResult>)>,
}

const READ_ONLY_BATCH_CONCURRENCY: usize = 8;

struct ReadOnlyToolOutcome {
    outcome: ToolExecutionOutcome,
    duration_ms: u64,
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
            artifact_store: None,
            conversation: Vec::new(),
            sensitive_values: SensitiveValues::default(),
            cancellation: None,
            interaction_route: None,
            capability_bridge: None,
            compaction_handle: None,
            background_compaction_enabled: false,
            code_intel: None,
            token_estimator: AdaptiveTokenEstimator::default(),
            skill_discovery_cache: None,
        }
    }

    pub fn with_artifact_store(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut runtime = Self::new();
        runtime.artifact_store = Some(ArtifactStore::new(root)?);
        Ok(runtime)
    }

    pub fn set_artifact_store(&mut self, store: ArtifactStore) {
        self.artifact_store = Some(store);
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

    pub fn set_background_compaction_enabled(&mut self, enabled: bool) {
        self.background_compaction_enabled = enabled;
    }

    /// Installs the semantic code intelligence facade used by the code_intel
    /// tool. Optional: without it the tool reports "unavailable" with a clear
    /// message instead of failing hard.
    pub fn set_code_intelligence(&mut self, code_intel: Arc<dyn CodeIntelligence>) {
        self.code_intel = Some(code_intel);
    }

    /// Canonical provider conversation after any compaction and completed
    /// tool turns. Interactive callers persist this instead of rebuilding a
    /// lossy transcript from rendered output.
    pub fn conversation(&self) -> &[ProviderMessage] {
        &self.conversation
    }

    fn provider_tool_definitions(&self, mode: crate::OperatingMode) -> Vec<Value> {
        let mut tools = self.tools.definitions_for_mode(mode);
        if self.code_intel.is_none() {
            tools.retain(|definition| {
                definition
                    .get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|name| name != "code_intel")
            });
        }
        if self.interaction_route.is_some() && mode == crate::OperatingMode::Auto {
            tools.push(ask_question_definition());
        }
        if mode == crate::OperatingMode::Auto {
            tools.push(todo_tool_definition());
            tools.push(skill_tool_definition());
        }
        tools
    }

    /// Tool schemas advertised to the provider for a loop in `mode`.
    pub fn advertised_tool_definitions(&self, mode: crate::OperatingMode) -> Vec<Value> {
        self.provider_tool_definitions(mode)
    }

    /// Builds the single runtime capability catalog from native tools,
    /// discovered skills and the supplied MCP contracts. Discovery and MCP
    /// inputs are metadata only; no process or transport is opened here.
    pub fn capability_catalog(
        &self,
        discovery: &DiscoveryResult,
        mcp_catalogs: &[McpCatalog],
    ) -> Result<CapabilityCatalog, CapabilityLedgerError> {
        let mut catalog = CapabilityCatalog::with_native_tools();
        catalog.add_skills(discovery)?;
        for mcp in mcp_catalogs {
            catalog.add_mcp_catalog(mcp)?;
        }
        Ok(catalog)
    }

    /// Opens the durable capability bridge used by runtime callers.
    pub fn open_capability_bridge<R: DurableRepoLike>(
        &self,
        repo: R,
        discovery: &DiscoveryResult,
        mcp_catalogs: &[McpCatalog],
    ) -> Result<RuntimeCapabilityBridge<R>, CapabilityLedgerError> {
        let catalog = self.capability_catalog(discovery, mcp_catalogs)?;
        RuntimeCapabilityBridge::new(
            repo,
            catalog,
            discovery,
            mcp_catalogs,
            self.tools.clone(),
            self.cancellation.clone().unwrap_or_default(),
        )
    }

    /// Productive runtime seam for capability requests. CLI/TUI/headless
    /// callers can keep their durable bridge outside `Runtime` while routing
    /// every request through the same runtime-owned policy boundary.
    pub fn dispatch_capability<R, A>(
        &self,
        bridge: &mut RuntimeCapabilityBridge<R>,
        request: crate::session::CapabilityRequest,
        adapter: &mut A,
    ) -> Result<crate::session::CapabilityDispatch, CapabilityLedgerError>
    where
        R: DurableRepoLike,
        A: RuntimeCapabilityAdapter,
    {
        bridge.dispatch(request, adapter)
    }

    pub fn enqueue_capability<R: DurableRepoLike>(
        &self,
        bridge: &mut RuntimeCapabilityBridge<R>,
        request: crate::session::CapabilityRequest,
    ) -> Result<(), CapabilityLedgerError> {
        bridge.enqueue_capability(request)
    }

    pub fn dispatch_queued_capability<R, A>(
        &self,
        bridge: &mut RuntimeCapabilityBridge<R>,
        queue_id: &str,
        adapter: &mut A,
    ) -> Result<crate::session::CapabilityDispatch, CapabilityLedgerError>
    where
        R: DurableRepoLike,
        A: RuntimeCapabilityAdapter,
    {
        bridge.dispatch_queued(queue_id, adapter)
    }

    pub fn cancel_capability<R: DurableRepoLike>(
        &self,
        bridge: &mut RuntimeCapabilityBridge<R>,
        queue_id: &str,
        mode: crate::OperatingMode,
        authorization: crate::session::AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError> {
        bridge.cancel_capability(queue_id, mode, authorization)
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

    fn redact_provider_error(&self, error: ProviderError) -> ProviderError {
        match error {
            ProviderError::Remote { message } => ProviderError::Remote {
                message: self.redact_sensitive(&message),
            },
            ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
                message: self.redact_sensitive(&message),
            },
            other => other,
        }
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
            .run_provider_messages_with_tools(client, messages, &[], next_seq)
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
    ) -> Result<ProviderTurnResult, ProviderError> {
        let redacted = self.redact_messages(messages);
        let mut request = client.prepare_messages_with_tools(&redacted, tools)?;
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
            messages_are_text_only(&redacted),
            request,
            request_next_seq,
        )
        .await
    }

    async fn run_provider_messages_with_tools_after_snapshot<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        calibration_eligible: bool,
        request: PreparedProviderRequest,
        next_seq: u64,
    ) -> Result<ProviderTurnResult, ProviderError> {
        let mut app = std::mem::replace(&mut self.app, AppHandle::fake());
        let mut sensitive_values = self.sensitive_values.0.clone();
        sensitive_values.extend(client.adapter().sensitive_values());
        crate::provider::normalize_sensitive_values(&mut sensitive_values);
        let mut normalizer =
            ProviderStreamNormalizer::new(client.adapter().wire_kind(), next_seq, sensitive_values);
        let request_started = Instant::now();
        let cancellation = self.cancellation.clone();
        let cancellation = async move {
            match cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let stream_result = client
            .stream_prepared_cancellable(request, cancellation, |event| {
                normalizer.push(&mut app, event);
            })
            .await;
        let cancelled = matches!(&stream_result, Err(ProviderError::Cancelled));
        let result = match stream_result {
            Ok(()) => normalizer.finish(&mut app),
            Err(ProviderError::Cancelled) if self.is_cancelled() => Ok(ProviderTurnResult {
                next_seq: normalizer.next_seq(),
                blocks_tools: true,
                stop: ProviderTurnStop::Normal,
            }),
            Err(error) => Err(self.redact_provider_error(error)),
        };
        self.app = app;
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
        let event_start = self.app.events().len();
        let tools = self.provider_tool_definitions(mode);
        let provider_result = self
            .run_provider_messages_with_tools(client, messages, &tools, next_seq)
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

    pub async fn run_agent_loop_with_message<A: ProviderAdapter + Send + Sync + 'static>(
        &mut self,
        client: &HttpProviderClient<A>,
        message: ProviderMessage,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        config: AgentLoopConfig,
    ) -> Result<AgentLoopResult, ProviderError> {
        self.run_agent_loop_with_messages(client, &[message], mode, cwd, next_seq, config)
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
        let cwd = cwd.as_ref();
        self.prepare_loop_capabilities(cwd)?;
        let mut messages = self.redact_messages(initial_messages);
        self.conversation.clone_from(&messages);
        let mut next_seq = next_seq;
        let mut all_results = Vec::new();
        let mut guard = LoopGuard::default();
        let mut governor = CausalGovernor::default();
        let mut turns = 0;
        let mut stop = AgentLoopStop::TurnLimit;
        let mut budget_steers_used = 0usize;
        let mut active_tool_outputs: std::collections::HashSet<u64> =
            std::collections::HashSet::new();
        let mut historical_tool_outputs: std::collections::HashSet<u64> =
            std::collections::HashSet::new();
        let mut compaction_applied = false;
        let mut overflow_retry_used = false;
        let mut pending_background = None;
        let tools = self.provider_tool_definitions(mode);
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();

        let mut turn = 0;
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
            turns = turn + 1;
            if turn > 0 {
                if let Some(handle) = &self.compaction_handle {
                    handle.completed_turn();
                }
            }
            let structural_preflight_chars =
                estimate_unprepared_request_chars(client.adapter(), &messages, &tools, None);
            let (preflight_chars, preflight_tokens, mut serialized_request) =
                match structural_preflight_chars {
                    Some(preflight_chars) => (
                        preflight_chars,
                        self.token_estimator
                            .estimate(provider, model, preflight_chars),
                        None,
                    ),
                    None => {
                        let mut request = client.prepare_messages_with_tools(&messages, &tools)?;
                        let preflight_tokens = self.token_estimator.estimate(
                            provider,
                            model,
                            request.serialized_chars,
                        );
                        request.estimated_tokens = preflight_tokens;
                        (request.serialized_chars, preflight_tokens, Some(request))
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
            let has_compactable = has_compactable_history(&messages);
            let manual_compaction = self
                .compaction_handle
                .as_ref()
                .and_then(CompactionHandle::manual_instructions)
                .is_some();
            let prepared_overflow_retry = overflow_retry_used
                && self.compaction_handle.as_ref().is_some_and(|handle| {
                    handle.status() == crate::context::CompactionStatus::Ready
                });
            let projected_tokens = preflight_tokens.saturating_add(config.context_reserve_tokens);
            let over_hard = projected_tokens
                >= compaction_policy.hard_threshold_tokens(config.context_window_tokens);
            let over_soft = projected_tokens
                >= compaction_policy.soft_threshold_tokens(config.context_window_tokens);
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
                let provider_identity = format!(
                    "{:?}:{}",
                    client.adapter().wire_kind(),
                    client.adapter().model()
                );
                let mut prepared = self
                    .compaction_handle
                    .as_ref()
                    .and_then(|handle| handle.take_prepared(&messages, &provider_identity));
                if prepared.is_none() && pending_background.is_some() {
                    drop(
                        self.cancel_pending_background(
                            &mut pending_background,
                            &mut next_seq,
                            "foreground_compaction_required",
                        )
                        .await?,
                    );
                    prepared = self
                        .compaction_handle
                        .as_ref()
                        .and_then(|handle| handle.take_prepared(&messages, &provider_identity));
                }
                if let Some(prepared) = prepared {
                    let tokens_before = preflight_tokens;
                    let selection = CompactionSelection {
                        root_instruction: messages
                            .iter()
                            .find(|message| message.role == "user")
                            .map(|message| message.content.clone())
                            .unwrap_or_default(),
                        summarized: messages[..prepared.first_kept_index].to_vec(),
                        kept: messages[prepared.first_kept_index..].to_vec(),
                        first_kept_index: prepared.first_kept_index,
                        recent_tokens: estimate_provider_message_tokens(
                            &messages[prepared.first_kept_index..],
                        ),
                    };
                    let summary = self.redact_sensitive(&prepared.summary);
                    messages = apply_compaction_selection(&messages, &selection, summary.clone())
                        .map_err(|message| ProviderError::InvalidResponse {
                        message: message.into(),
                    })?;
                    let mut request = client.prepare_messages_with_tools(&messages, &tools)?;
                    let tokens_after =
                        self.token_estimator
                            .estimate(provider, model, request.serialized_chars);
                    request.estimated_tokens = tokens_after;
                    serialized_request = Some(request);
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
                        &mut next_seq,
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
                        &mut next_seq,
                        crate::EventKind::CompactionCompleted,
                    )?;
                    compaction_applied = true;
                    active_tool_outputs.clear();
                } else if manual_compaction {
                    push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::ProviderPhase {
                            phase: ProviderPhase::Compacting,
                            elapsed_ms: 0,
                            detail: None,
                        },
                    )?;
                    let compact_result = self
                        .compact_before_send(
                            client,
                            &messages,
                            &tools,
                            preflight_tokens,
                            next_seq,
                            config.context_window_tokens,
                            config.context_reserve_tokens,
                            if overflow_retry_used {
                                CompactionReason::Overflow
                            } else {
                                CompactionReason::Manual
                            },
                        )
                        .await;
                    if compact_result.is_err() {
                        if let Some(handle) = &self.compaction_handle {
                            handle.invalidate();
                            handle.clear_manual();
                        }
                    }
                    if self.is_cancelled() {
                        return Ok(AgentLoopResult {
                            next_seq: self.observed_next_seq(next_seq),
                            turns,
                            stop: AgentLoopStop::Cancelled,
                            tool_results: all_results,
                            usage: usage_since(&self.app, loop_event_start),
                        });
                    }
                    let (compacted, summary_usage, following_seq, compacted_request) =
                        compact_result?;
                    messages = compacted;
                    serialized_request = Some(compacted_request);
                    drop(summary_usage);
                    next_seq = following_seq;
                    compaction_applied = true;
                    active_tool_outputs.clear();
                } else if over_hard {
                    let selection = select_compaction_history(
                        &messages,
                        &compaction_policy_for_window(
                            compaction_policy.clone(),
                            config.context_window_tokens,
                        ),
                    )
                    .map_err(|message| ProviderError::InvalidResponse {
                        message: message.into(),
                    })?;
                    let summary = self.redact_sensitive(&local_emergency_summary(&selection));
                    let tokens_before = preflight_tokens;
                    let prefix_fingerprint = compaction_prefix_fingerprint(&selection.summarized);
                    messages = apply_compaction_selection(&messages, &selection, summary.clone())
                        .map_err(|message| ProviderError::InvalidResponse {
                        message: message.into(),
                    })?;
                    let mut request = client.prepare_messages_with_tools(&messages, &tools)?;
                    let tokens_after =
                        self.token_estimator
                            .estimate(provider, model, request.serialized_chars);
                    request.estimated_tokens = tokens_after;
                    serialized_request = Some(request);
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
                        &mut next_seq,
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
                        &mut next_seq,
                        crate::EventKind::CompactionCompleted,
                    )?;
                    compaction_applied = true;
                    active_tool_outputs.clear();
                }
            }
            let mut serialized_request = match serialized_request {
                Some(request) => request,
                None => client.prepare_messages_with_tools(&messages, &tools)?,
            };
            let serialized_chars = serialized_request.serialized_chars;
            if !compaction_applied {
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
            let mut background_plan = pending_background
                .is_none()
                .then(|| {
                    self.build_background_compaction_plan(
                        client,
                        &messages,
                        &compaction_policy,
                        ContextBudget::new(
                            config.context_window_tokens,
                            estimated_tokens,
                            config.context_reserve_tokens,
                        ),
                        should_compact,
                        remaining_model_turns,
                    )
                })
                .flatten();
            let event_start = self.app.events().len();
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
                    messages_are_text_only(&messages),
                    serialized_request,
                    next_seq,
                )
                .await;
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
            let provider_turn = match provider_result {
                Ok(turn) => turn,
                Err(error)
                    if !overflow_retry_used
                        && self.compaction_handle.is_some()
                        && is_context_overflow_error(&error)
                        && !has_causal_provider_output(&self.app, event_start)
                        && has_compactable =>
                {
                    overflow_retry_used = true;
                    next_seq = self.observed_next_seq(next_seq);
                    let has_prepared = self.compaction_handle.as_ref().is_some_and(|handle| {
                        handle.status() == crate::context::CompactionStatus::Ready
                    });
                    if !has_prepared {
                        drop(
                            self.cancel_pending_background(
                                &mut pending_background,
                                &mut next_seq,
                                "context_overflow_retry",
                            )
                            .await?,
                        );
                        if let Some(handle) = &self.compaction_handle {
                            handle.invalidate();
                            let _ = handle.request_manual("");
                        }
                    }
                    continue;
                }
                Err(error) => {
                    drop(
                        self.cancel_pending_background(
                            &mut pending_background,
                            &mut next_seq,
                            "provider_error",
                        )
                        .await?,
                    );
                    return Err(error);
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
                messages.push(
                    self.redact_message(ProviderMessage::assistant(assistant_text, Vec::new())),
                );
                stop = match provider_turn.stop {
                    ProviderTurnStop::Normal => AgentLoopStop::ProviderCompleted,
                    ProviderTurnStop::Truncated => AgentLoopStop::ProviderTruncated,
                    ProviderTurnStop::Filtered => AgentLoopStop::ProviderFiltered,
                };
                break;
            }

            let suppressed_calls = truncate_calls_for_budget(&mut calls, config);
            if suppressed_calls > 0 {
                push_runtime_event(
                    &mut self.app,
                    &mut next_seq,
                    crate::EventKind::ToolCallsSuppressed {
                        count: suppressed_calls as u64,
                    },
                )?;
                let batch_id = format!("slim-batch-{turn}-{next_seq}");
                assign_missing_call_ids(&mut calls, &batch_id);
                let (mut results, following_seq) = match self
                    .execute_provider_tool_batch(
                        mode,
                        cwd,
                        &batch_id,
                        &calls,
                        next_seq,
                        &mut governor,
                    )
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
                            .await?,
                        );
                        return Err(error);
                    }
                };
                next_seq = following_seq;
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
                    .materialize_results(&mut results, config.max_result_bytes, next_seq)
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
                all_results.extend(results);
                stop = AgentLoopStop::ToolLimit;
                break;
            }

            if let Some(plan) = background_plan.take() {
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
                    let cancellation = self.cancellation.clone();
                    let background_client = (*client).clone();
                    let task = tokio::spawn(async move {
                        run_background_compaction(
                            background_client,
                            plan,
                            cancellation,
                            task_progress,
                        )
                        .await
                    });
                    pending_background = Some(PendingBackgroundCompaction {
                        task,
                        progress,
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
                        .await?,
                    );
                    return Err(error);
                }
            };
            next_seq = following_seq;
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
                .materialize_results(&mut results, config.max_result_bytes, next_seq)
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
            let mut tool_calls = calls.clone();
            for (call, result) in calls.iter().zip(results.iter()) {
                if result.success && matches!(call.name.as_str(), "write" | "patch") {
                    if let Some(tool_call) = tool_calls
                        .iter_mut()
                        .find(|tool_call| tool_call.id == call.id)
                    {
                        tool_call.arguments =
                            stub_mutating_tool_arguments(&call.name, &tool_call.arguments);
                    }
                }
            }
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
            messages
                .push(self.redact_message(ProviderMessage::assistant(assistant_text, tool_calls)));
            let mut saw_duplicate_this_turn = false;
            for (call, result) in calls.iter().zip(results.iter()) {
                let full_output = prompt_output(result, config.max_result_bytes);
                let full_output_bytes = full_output.len() as u64;
                let output_hash = tool_output_hash(&full_output);
                let duplicate_in_active_context =
                    result.success && active_tool_outputs.contains(&output_hash);
                let post_compaction_reacquisition = result.success
                    && compaction_applied
                    && !duplicate_in_active_context
                    && historical_tool_outputs.contains(&output_hash);
                if result.success {
                    active_tool_outputs.insert(output_hash);
                    historical_tool_outputs.insert(output_hash);
                }
                // Single redact per call: `output` was already redacted at
                // production, so only the wire-derived name/id are redacted
                // here instead of re-scanning the whole content.
                let tool_name = self.redact_sensitive(&call.name);
                let output = if duplicate_in_active_context {
                    // Byte-identical rerun of an earlier successful tool:
                    // the content is already in context, so only a
                    // pointer goes on the wire (token dedup, TOK-03).
                    format!(
                        "[duplicate {} result omitted; identical output already in context]",
                        tool_name
                    )
                } else if post_compaction_reacquisition {
                    format!(
                        "[compacted {} result omitted; identical output was summarized — re-read if needed]",
                        tool_name
                    )
                } else {
                    full_output
                };
                if duplicate_in_active_context || post_compaction_reacquisition {
                    if duplicate_in_active_context {
                        saw_duplicate_this_turn = true;
                    }
                    if let Err(error) = push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::ToolEvidenceReused {
                            original_bytes: full_output_bytes,
                            emitted_bytes: output.len() as u64,
                            post_compaction: post_compaction_reacquisition,
                        },
                    ) {
                        drop(
                            self.cancel_pending_background(
                                &mut pending_background,
                                &mut next_seq,
                                "tool_evidence_reused",
                            )
                            .await?,
                        );
                        return Err(error);
                    }
                }
                messages.push(ProviderMessage::tool(
                    tool_name,
                    self.redact_sensitive(&call.id),
                    output,
                ));
                if !result.success && !guard.accept(&call.name, &call.arguments, &result.output) {
                    all_results.extend(results.clone());
                    stop = AgentLoopStop::RepeatedFailedTool;
                    self.conversation.clone_from(&messages);
                    if let Err(error) = push_runtime_event(
                        &mut self.app,
                        &mut next_seq,
                        crate::EventKind::TerminalError {
                            message: "repeated failed tool call blocked".into(),
                        },
                    ) {
                        drop(
                            self.cancel_pending_background(
                                &mut pending_background,
                                &mut next_seq,
                                "repeated_failed_tool",
                            )
                            .await?,
                        );
                        return Err(error);
                    }
                    drop(
                        self.cancel_pending_background(
                            &mut pending_background,
                            &mut next_seq,
                            "repeated_failed_tool",
                        )
                        .await?,
                    );
                    return Ok(AgentLoopResult {
                        next_seq,
                        turns,
                        stop,
                        tool_results: all_results,
                        usage: usage_since(&self.app, loop_event_start),
                    });
                }
            }
            all_results.extend(results);
            if governor.stop_requested() {
                stop = AgentLoopStop::NoProgress;
                self.app.discard_projected_payloads();
                break;
            }
            if saw_duplicate_this_turn
                && budget_steers_used < MAX_BUDGET_STEERS
                && !self.is_cancelled()
                && turn + 1 < config.max_turns
            {
                budget_steers_used += 1;
                messages.push(self.redact_message(ProviderMessage::user(BUDGET_STEER_PROMPT)));
            }
            if turn + 1 == config.max_turns {
                stop = AgentLoopStop::TurnLimit;
            }
            turn += 1;
            self.app.discard_projected_payloads();
        }

        if matches!(
            stop,
            AgentLoopStop::TurnLimit | AgentLoopStop::ToolLimit | AgentLoopStop::NoProgress
        ) && !self.is_cancelled()
        {
            let finalize_event_start = self.app.events().len();
            let mut final_messages = messages.clone();
            final_messages.push(ProviderMessage::user(BUDGET_FINALIZE_PROMPT));
            match self
                .run_provider_messages_with_tools(client, &final_messages, &[], next_seq)
                .await
            {
                Ok(turn) => {
                    next_seq = turn.next_seq;
                    let text = self.app.events()[finalize_event_start..]
                        .iter()
                        .filter_map(|event| match &event.kind {
                            crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    if !text.trim().is_empty() {
                        messages.push(
                            self.redact_message(ProviderMessage::assistant(text, Vec::new())),
                        );
                    }
                }
                Err(_) => {
                    next_seq = self.observed_next_seq(next_seq);
                }
            }
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
        self.conversation = messages;
        if stop == AgentLoopStop::ProviderCompleted {
            let verified = runtime_goal_assurance(&self.app.events()[loop_event_start..]);
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
        let result = match joined {
            Ok(result) => result,
            Err(_) => {
                let observed_usage =
                    background_compaction_cancellation_usage(&attempt, fallback_duration);
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
            let observed_usage =
                background_compaction_cancellation_usage(&attempt, result.duration_ms);
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
        if !result.valid || summary.trim().is_empty() || summary.len() > policy.summary_max_bytes {
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

        attempt.task.abort();
        let duration_ms = elapsed_millis(attempt.started);
        let observed_usage = background_compaction_cancellation_usage(&attempt, duration_ms);
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

    fn build_background_compaction_plan<A: ProviderAdapter>(
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
            || budget.used_tokens.saturating_add(budget.reserve_tokens)
                < policy.soft_threshold_tokens(budget.window_tokens)
        {
            return None;
        }
        let capped_policy = compaction_policy_for_window(policy.clone(), budget.window_tokens);
        let selection = select_compaction_history(messages, &capped_policy).ok()?;
        let provider = crate::provider::provider_kind_name(client.adapter().kind());
        let model = client.adapter().model();
        let previous_summary = handle.previous_summary();
        let prompt = build_bounded_summary_prompt_with_checkpoint(
            &selection.summarized,
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
        let root_tokens =
            estimate_provider_message_tokens(&[ProviderMessage::user(&selection.root_instruction)]);
        let projected_tokens_after = root_tokens
            .saturating_add(selection.recent_tokens)
            .saturating_add(COMPACTION_MAX_OUTPUT_TOKENS);
        let tokens_before = budget.used_tokens;
        let future_turns = u8::try_from(remaining_model_turns.min(2)).unwrap_or(2);
        let projected_savings_tokens = tokens_before
            .saturating_sub(projected_tokens_after)
            .saturating_mul(u64::from(future_turns));
        let estimated_cost_tokens =
            preflight_input_tokens.saturating_add(COMPACTION_MAX_OUTPUT_TOKENS);
        let safety_margin_tokens = estimated_cost_tokens.div_ceil(4);
        let profitable =
            projected_savings_tokens > estimated_cost_tokens.saturating_add(safety_margin_tokens);
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
            .map(|index| prepared[index].clone())
            .collect())
    }

    async fn execute_provider_tool_batch(
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
        // Preparation is pure (arg parsing + path resolution): prepare the
        // whole batch once so the workspace root is canonicalized a single
        // time instead of once per call. Execution below stays sequential and
        // interleaved with governor observations, so ordering is unchanged.
        let prepared_all = self
            .prepare_provider_tool_invocations(mode, cwd, calls)
            .await?;
        let mut results = Vec::with_capacity(calls.len());
        let mut segment_start = 0;
        while segment_start < calls.len() {
            if self.is_cancelled() {
                break;
            }
            let read_only = tool_call_is_parallel_read(
                &self.tools,
                &prepared_all[segment_start],
                &calls[segment_start].name,
            );
            let mut segment_end = segment_start + 1;
            if read_only {
                while segment_end < calls.len()
                    && tool_call_is_parallel_read(
                        &self.tools,
                        &prepared_all[segment_end],
                        &calls[segment_end].name,
                    )
                {
                    segment_end += 1;
                }
            }
            let segment = &calls[segment_start..segment_end];
            if read_only && segment.len() >= 2 {
                let (segment_results, following_seq) = self
                    .execute_read_only_tool_segment(
                        batch_id,
                        segment,
                        prepared_all[segment_start..segment_end].to_vec(),
                        next_seq,
                        governor,
                    )
                    .await?;
                next_seq = following_seq;
                results.extend(segment_results);
            } else {
                let call = &segment[0];
                if self.is_cancelled() {
                    break;
                }
                let prepared = prepared_all[segment_start].clone();
                let (pending, observations) =
                    governor.observe_before_identified(&prepared, batch_id, &call.id);
                self.emit_governor_observations(observations, &mut next_seq)?;
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
                // Single redact per call: the outcome was already redacted at
                // production, so the governor borrows it directly.
                let observations =
                    governor.observe_after(pending, &outcome.result, &outcome.receipt);
                self.emit_governor_observations(observations, &mut next_seq)?;
                results.push(outcome.result);
            }
            segment_start = segment_end;
        }
        if !self.is_cancelled() {
            let observations = governor.finish_turn();
            self.emit_governor_observations(observations, &mut next_seq)?;
        }
        Ok((results, next_seq))
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
        let mut pending_calls = Vec::with_capacity(calls.len());
        for (call, (pending, observations)) in calls.iter().zip(preflights) {
            self.emit_governor_observations(observations, &mut next_seq)?;
            pending_calls.push(pending);
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

        let tools = self.tools.clone();
        let code_intel = self.code_intel.clone();
        let cancellation = self.cancellation.clone();
        let futures = calls
            .iter()
            .cloned()
            .zip(prepared_calls)
            .enumerate()
            .filter_map(|(index, (call, prepared))| {
                if alias_of[index] != index {
                    return None;
                }
                let tools = tools.clone();
                let code_intel = code_intel.clone();
                let cancellation = cancellation.clone();
                Some(async move {
                    let started_at = Instant::now();
                    let revision_before = tools.workspace_revision();
                    let mut outcome = if call.name == "code_intel" {
                        let request = prepared_code_intel_request(&prepared);
                        let result =
                            run_code_intel_request(code_intel, request, cancellation.clone()).await;
                        ToolExecutionOutcome {
                            result,
                            receipt: ToolExecutionReceipt::unobserved(
                                &prepared,
                                revision_before,
                                tools.workspace_revision(),
                                u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
                            ),
                        }
                    } else {
                        let result_name = call.name.clone();
                        let fallback_prepared = prepared.clone();
                        let fallback_tools = tools.clone();
                        let cancellation_for_tool = cancellation.clone();
                        match tokio::task::spawn_blocking(move || {
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
        let mut outcomes =
            futures_util::stream::iter(futures).buffer_unordered(READ_ONLY_BATCH_CONCURRENCY);
        let mut completed = std::iter::repeat_with(|| None)
            .take(calls.len())
            .collect::<Vec<Option<ToolExecutionOutcome>>>();
        while let Some((index, outcome)) = outcomes.next().await {
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
        let governor_calls = pending_calls
            .into_iter()
            .zip(&completed)
            .map(|(pending, outcome)| {
                // Single redact per call: already redacted at production, so
                // only the owned clone the batch API requires remains.
                (
                    pending,
                    outcome.result.clone(),
                    outcome.receipt.clone(),
                )
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
        let special = if invocation.name == "ask_question" {
            Some(self.execute_ask_question(invocation, next_seq).await?)
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
        let mut outcome =
            run_code_intel_request(self.code_intel.clone(), request, self.cancellation.clone())
                .await;
        if self.is_cancelled() {
            outcome.success = false;
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
        if !outcome.result.success || !matches!(invocation.name, "write" | "patch") {
            return;
        }
        let Some(code_intel) = self.code_intel.as_ref() else {
            return;
        };
        let Some(absolute) = prepared.target_paths.first().cloned() else {
            return;
        };
        let text = match &prepared.arguments {
            PreparedToolArguments::Write { content, .. } => Some(content.clone()),
            PreparedToolArguments::Patch { .. } => outcome.receipt.synced_text.clone(),
            _ => None,
        };
        code_intel.notify_file_changed(cwd, &absolute, text).await;
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
        let result = self.apply_todo_tool(mode, invocation);
        if result.success {
            if let Some(items) = self
                .capability_bridge
                .as_ref()
                .and_then(|bridge| bridge.todo("session").map(todo_changed_items))
            {
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
        if mode != crate::OperatingMode::Auto {
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
        let mutations = parse_todo_mutations(&args);
        if mutations.is_empty() {
            return ToolResult {
                name: invocation.name.into(),
                success: false,
                output: "todo needs at least one {title} or {id,status} entry".into(),
                artifact: None,
            };
        }
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
            let revision = bridge.task_revision("session").saturating_add(1);
            let request = TaskMutationRequest {
                idempotency_key: format!("{}-{index}", invocation.call_id),
                entity_id: "session".into(),
                revision,
                mutation,
            };
            match bridge.apply_task_mutation(request, mode, AuthorizationGrant::Explicit) {
                Ok(_changed) => lines.push(format!("todo updated: {label}")),
                Err(error) => {
                    return ToolResult {
                        name: invocation.name.into(),
                        success: false,
                        output: format!("todo rejected: {error}"),
                        artifact: None,
                    }
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
        let result = tokio::task::spawn_blocking(move || {
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
        let catalog = CapabilityCatalog::with_native_tools();
        let header = DurableSessionHeader::new(
            "loop",
            "now",
            cwd.to_string_lossy().into_owned(),
            None,
            None,
        );
        let bridge = RuntimeCapabilityBridge::new(
            MemoryRepo::new(header),
            catalog,
            &DiscoveryResult::default(),
            &[],
            self.tools.clone(),
            self.cancellation.clone().unwrap_or_default(),
        )
        .map_err(capability_error)?;
        self.capability_bridge = Some(bridge);
        // Bound skill-discovery memoization to one loop run.
        self.skill_discovery_cache = None;
        Ok(())
    }

    pub fn execute_tool(
        &mut self,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        let batch_id = format!("slim-batch-direct-{next_seq}");
        let call_id = format!("slim-call-direct-{next_seq}");
        self.execute_tool_call(
            mode,
            cwd,
            ToolInvocation {
                batch_id: &batch_id,
                call_id: &call_id,
                name,
                arguments,
            },
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
        let task = tokio::task::spawn_blocking(move || {
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
            outcome.result.success = false;
        }
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        outcome.result.output = self.redact_sensitive(&outcome.result.output);
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
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        invocation: ToolInvocation<'_>,
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
        let mut result = self.tools.execute_with_cancellation_and_progress(
            mode,
            cwd,
            invocation.name,
            invocation.arguments,
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
            result.success = false;
        }
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        result.output = self.redact_sensitive(&result.output);
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: result.name.clone(),
                output: result.output.clone(),
            },
        )?;
        push_runtime_event(
            &mut self.app,
            &mut following_seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: result.name.clone(),
                success: result.success,
                duration_ms,
            },
        )?;
        Ok((result, following_seq))
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

    fn redact_message(&self, mut message: ProviderMessage) -> ProviderMessage {
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
                crate::provider::ProviderContentBlock::Image { data, .. }
                | crate::provider::ProviderContentBlock::Audio { data, .. }
                | crate::provider::ProviderContentBlock::File { data, .. } => {
                    *data = self.redact_sensitive(data);
                }
            }
        }
        message
    }

    fn redact_messages(&self, messages: &[ProviderMessage]) -> Vec<ProviderMessage> {
        messages
            .iter()
            .cloned()
            .map(|message| self.redact_message(message))
            .collect()
    }

    async fn materialize_results(
        &mut self,
        results: &mut [ToolResult],
        max_result_bytes: usize,
        mut next_seq: u64,
    ) -> Result<u64, ProviderError> {
        let Some(store) = self.artifact_store.clone() else {
            return Ok(next_seq);
        };
        let jobs = results
            .iter_mut()
            .enumerate()
            .filter(|(_, result)| result.output.len() > max_result_bytes)
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
            self.app
                .push_event(crate::SessionEvent::new(
                    next_seq,
                    crate::EventKind::ArtifactStored {
                        id: handle.id,
                        size: handle.size,
                    },
                ))
                .map_err(|message| ProviderError::InvalidResponse {
                    message: message.into(),
                })?;
            next_seq += 1;
        }
        Ok(next_seq)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "compaction must retain the exact request budget and wire-estimation inputs"
    )]
    async fn compact_before_send<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        tools: &[Value],
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
        let summary_prompt = build_bounded_summary_prompt_with_checkpoint(
            &selection.summarized,
            previous_summary.as_deref(),
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
        let stream_result = client
            .stream_prepared_cancellable(serialized_request, cancellation, |event| {
                collected.push(event)
            })
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
        let prefix_fingerprint = compaction_prefix_fingerprint(&selection.summarized);
        let compacted = apply_compaction_selection(messages, &selection, summary.clone()).map_err(
            |message| ProviderError::InvalidResponse {
                message: message.into(),
            },
        )?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut compacted_request = client.prepare_messages_with_tools(&compacted, tools)?;
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
        Ok((compacted, summary_usage, next_seq, compacted_request))
    }
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
) -> ToolResult {
    let Some(code_intel) = code_intel else {
        return ToolResult {
            name: "code_intel".into(),
            success: false,
            output: "code_intel unavailable: no language-server manager configured".into(),
            artifact: None,
        };
    };
    let request = match request {
        Ok(request) => request,
        Err(message) => {
            return ToolResult {
                name: "code_intel".into(),
                success: false,
                output: format!("code_intel: {message}"),
                artifact: None,
            };
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
    ToolResult {
        name: "code_intel".into(),
        success,
        output: render_code_intel(action, &result),
        artifact: None,
    }
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

fn todo_tool_definition() -> Value {
    json!({
        "name": "todo",
        "description": "Track work items for the current session (Auto mode). Accepts a single {title} to add, a {id,status} pair to update, or a todos array of {title/content, id, status} entries.",
        "input_schema": {
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "content": {"type": "string"},
                "id": {"type": "string"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "blocked", "cancelled"]},
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "content": {"type": "string"},
                            "id": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "blocked", "cancelled"]}
                        },
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        }
    })
}

fn skill_tool_definition() -> Value {
    json!({
        "name": "skill",
        "description": "Run a discovered skill (Auto mode) or list available skills with {list:true}. Default script is run.ps1 inside the skill directory; if that file is missing, the SKILL.md body is returned. Skill bodies are never injected into context.",
        "input_schema": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "list": {"type": "boolean"},
                "script": {"type": "string", "description": "Relative filename inside the skill directory. Default: run.ps1. Do not pass ./ or an absolute path."}
            },
            "additionalProperties": false
        }
    })
}

fn todo_changed_items(tracker: &crate::task::TodoTracker) -> Vec<crate::TodoChangedItem> {
    tracker
        .items()
        .iter()
        .map(|item| crate::TodoChangedItem {
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

fn parse_todo_entry(entry: &Value) -> Option<(String, TaskMutation)> {
    let title = match entry {
        Value::String(_) => todo_text(entry),
        _ => entry
            .get("content")
            .or_else(|| entry.get("title"))
            .and_then(todo_text),
    };
    if let Some(title) = title {
        return Some((title.clone(), TaskMutation::TodoAdd { title }));
    }
    let id = entry.get("id").and_then(todo_text)?;
    let status = entry
        .get("status")
        .and_then(todo_text)
        .as_deref()
        .and_then(todo_status_from_name)?;
    Some((format!("todo {id}"), TaskMutation::TodoSetStatus { status }))
}

fn parse_todo_mutations(args: &Value) -> Vec<(String, TaskMutation)> {
    let mut mutations = Vec::new();
    if let Some(todos) = args.get("todos") {
        match todos {
            Value::Array(entries) => {
                for entry in entries {
                    if let Some(item) = parse_todo_entry(entry) {
                        mutations.push(item);
                    }
                }
            }
            other => {
                if let Some(item) = parse_todo_entry(other) {
                    mutations.push(item);
                }
            }
        }
    }
    if mutations.is_empty() {
        if let Some(item) = parse_todo_entry(args) {
            mutations.push(item);
        }
    }
    mutations
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
    if args.get("list").and_then(Value::as_bool).unwrap_or(false) {
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
    let script = crate::skills::default_skill_script(args.get("script").and_then(Value::as_str));
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
            trusted: true,
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
    app.push_event(crate::SessionEvent::new(*next_seq, kind))
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
    *next_seq = checked_next_seq(*next_seq)?;
    Ok(())
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
    duration_ms: u64,
) -> UsageTotals {
    let progress = compaction_progress_snapshot(&attempt.progress);
    let mut request = attempt.usage_request.clone();
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
    cancellation: Option<CancellationToken>,
    progress: Arc<Mutex<CompactionAttemptProgress>>,
) -> BackgroundCompactionResult {
    let started = Instant::now();
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
            .stream_prepared_cancellable(request, cancellation_future, |event| {
                update_compaction_progress(&progress, &event);
                collected.push(event);
            })
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

fn is_context_overflow_error(error: &ProviderError) -> bool {
    let message = match error {
        ProviderError::Remote { message } => message.as_str(),
        ProviderError::InvalidResponse { message } => message.as_str(),
        _ => return false,
    };
    let lower = message.to_ascii_lowercase();
    [
        "context",
        "window",
        "token",
        "max_tokens",
        "prompt too long",
        "length",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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
    if name.is_empty()
        || !serde_json::from_str::<Value>(arguments).is_ok_and(|value| value.is_object())
    {
        Err(ProviderError::MalformedToolCall)
    } else {
        Ok(())
    }
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
        message
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
    });
    let tool_chars = tools
        .iter()
        .map(estimate_json_chars)
        .fold(0_u64, |total, chars| {
            total
                .saturating_add(TOOL_ENVELOPE_CHARS)
                .saturating_add(chars)
        });
    let request_envelope_chars = adapter.request_envelope_upper_bound_chars()?;
    Some(
        request_envelope_chars
            .saturating_add(estimate_json_string_chars(adapter.model()))
            .saturating_add(system_chars)
            .saturating_add(message_chars)
            .saturating_add(tool_chars),
    )
}

fn estimate_json_string_chars(value: &str) -> u64 {
    value.chars().fold(2_u64, |total, character| {
        total.saturating_add(match character {
            '\u{0000}'..='\u{001f}' => 6,
            '"' | '\\' => 2,
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

fn stub_mutating_tool_arguments(name: &str, arguments: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_owned();
    };
    let Some(object) = value.as_object_mut() else {
        return arguments.to_owned();
    };
    match name {
        "write" => {
            if let Some(content) = object.get("content").and_then(Value::as_str) {
                object.insert(
                    "content".into(),
                    Value::String(format!("[omitted {} bytes]", content.len())),
                );
            }
        }
        "patch" => {
            for key in ["expected", "replacement"] {
                if let Some(text) = object.get(key).and_then(Value::as_str) {
                    object.insert(
                        key.into(),
                        Value::String(format!("[omitted {} bytes]", text.len())),
                    );
                }
            }
        }
        _ => {}
    }
    value.to_string()
}

fn tool_output_hash(output: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    output.hash(&mut hasher);
    hasher.finish()
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

fn truncate_shell_result(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_owned();
    }
    let head_target = max_bytes / 2 + max_bytes % 2;
    let tail_target = max_bytes - head_target;
    let mut head_end = head_target;
    while !output.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = output.len() - tail_target;
    while !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail_bytes = output.len() - tail_start;
    let discarded_bytes = output
        .len()
        .saturating_sub(head_end.saturating_add(tail_bytes));
    format!(
        "{}\n[truncated {discarded_bytes} bytes by result limit; model sees first {head_end} and last {tail_bytes} bytes]\n{}",
        &output[..head_end],
        &output[tail_start..],
    )
}

fn prompt_output(result: &ToolResult, max_bytes: usize) -> String {
    if result.output.len() <= max_bytes {
        return result.output.clone();
    }
    let preview = if result.name == "shell" {
        truncate_shell_result(&result.output, max_bytes)
    } else {
        truncate_result(&result.output, max_bytes)
    };
    if let Some(ArtifactHandle { id, size, path }) = &result.artifact {
        format!(
            "{preview}\n[artifact id={id} size={size} path={}]",
            path.display()
        )
    } else {
        preview
    }
}

fn redact_values(sensitive_values: &[String], input: &str) -> String {
    sensitive_values
        .iter()
        .fold(input.to_owned(), |redacted, value| {
            redacted.replace(value, "[REDACTED]")
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

    fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub(crate) fn push(&mut self, app: &mut AppHandle, event: ProviderEvent) {
        if self.error.is_some() {
            return;
        }
        if let Err(error) = self.push_inner(app, event) {
            self.error = Some(error);
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
        if stop == ProviderTurnStop::Normal
            && (stop_requires_tool_calls(&raw_stop_reason) || codex_completed_with_calls)
        {
            let mut calls = std::mem::take(&mut self.openai_calls);
            calls.sort_by_key(|call| call.index.unwrap_or(u32::MAX));
            calls.extend(std::mem::take(&mut self.standalone_calls));
            if calls.is_empty() && self.published_tool_calls == 0 {
                return Err(ProviderError::InvalidResponse {
                    message: "provider required tool execution but emitted no complete tool call"
                        .into(),
                });
            }
            for call in &calls {
                validate_buffered_call(call)?;
            }
            for call in calls {
                let call = redact_buffered_call(call, &self.sensitive_values);
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
            )
        {
            return Err(ProviderError::InvalidResponse {
                message: "provider emitted events after stop".into(),
            });
        }
        if matches!(
            &event,
            ProviderEvent::TextDelta(_)
                | ProviderEvent::ToolCallDelta { .. }
                | ProviderEvent::ToolCallStart { .. }
                | ProviderEvent::ToolCallInputDelta { .. }
                | ProviderEvent::ToolCall { .. }
                | ProviderEvent::ContentBlockStop { .. }
                | ProviderEvent::Stopped { .. }
        ) {
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
                let call = redact_buffered_call(
                    self.anthropic_calls.remove(position),
                    &self.sensitive_values,
                );
                publish_buffered_call(app, &mut self.next_seq, call)?;
                self.published_tool_calls = self.published_tool_calls.saturating_add(1);
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
                if self.stop_reason.is_some() {
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

fn redact_buffered_call(
    mut call: BufferedToolCall,
    sensitive_values: &[String],
) -> BufferedToolCall {
    call.id = call.id.map(|value| redact_values(sensitive_values, &value));
    call.name = call
        .name
        .map(|value| redact_values(sensitive_values, &value));
    call.arguments = redact_values(sensitive_values, &call.arguments);
    call
}

fn append_openai_delta(
    calls: &mut Vec<BufferedToolCall>,
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
) -> Result<(), ProviderError> {
    let id = id.filter(|id| !id.is_empty());
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
    let Some(name) = call.name.filter(|name| !name.is_empty()) else {
        return Err(ProviderError::MalformedToolCall);
    };
    validate_tool_arguments(&name, &call.arguments)?;
    push_runtime_event(
        app,
        next_seq,
        crate::EventKind::ProviderToolCall {
            id: call.id.unwrap_or_default(),
            name,
            arguments: call.arguments,
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
mod tests {
    use super::*;
    use serde_json::json;

    struct FixtureCodeIntel {
        fail: bool,
    }

    #[async_trait::async_trait]
    impl crate::codeintel::CodeIntelligence for FixtureCodeIntel {
        async fn status(
            &self,
            _workspace: &Path,
        ) -> crate::codeintel::CodeIntelOutcome {
            if self.fail {
                return crate::codeintel::CodeIntelOutcome::unavailable(
                    "fixture",
                    "request timed out",
                );
            }
            crate::codeintel::CodeIntelOutcome {
                meta: crate::codeintel::CodeIntelMeta {
                    server: "fixture".into(),
                    state: crate::codeintel::CodeIntelServerState::Ready,
                    completeness: crate::codeintel::CodeIntelCompleteness::Complete,
                    document_version: None,
                    stale: false,
                    elapsed_ms: 0,
                },
                payload: json!({ "servers": [], "summary": "ok" }),
            }
        }

        async fn definition(
            &self,
            _query: &crate::codeintel::CodeIntelPositionQuery,
        ) -> crate::codeintel::CodeIntelOutcome {
            if self.fail {
                return crate::codeintel::CodeIntelOutcome::unavailable(
                    "fixture",
                    "request timed out",
                );
            }
            crate::codeintel::CodeIntelOutcome {
                meta: crate::codeintel::CodeIntelMeta {
                    server: "fixture".into(),
                    state: crate::codeintel::CodeIntelServerState::Ready,
                    completeness: crate::codeintel::CodeIntelCompleteness::Complete,
                    document_version: Some(1),
                    stale: false,
                    elapsed_ms: 0,
                },
                payload: json!({ "found": false, "file": "", "line": 0, "column": 0 }),
            }
        }

        async fn references(
            &self,
            _query: &crate::codeintel::CodeIntelPositionQuery,
        ) -> crate::codeintel::CodeIntelOutcome {
            crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
        }

        async fn hover(
            &self,
            _query: &crate::codeintel::CodeIntelPositionQuery,
        ) -> crate::codeintel::CodeIntelOutcome {
            crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
        }

        async fn symbols(
            &self,
            _query: &crate::codeintel::CodeIntelSymbolQuery,
        ) -> crate::codeintel::CodeIntelOutcome {
            crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
        }

        async fn diagnostics(
            &self,
            _query: &crate::codeintel::CodeIntelDiagnosticsQuery,
        ) -> crate::codeintel::CodeIntelOutcome {
            crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
        }

        async fn notify_file_changed(
            &self,
            _workspace: &Path,
            _path: &Path,
            _text: Option<String>,
        ) {
        }
    }

    #[tokio::test]
    async fn code_intel_error_payload_reports_failure() {
        let request = Ok(CodeIntelRequest::Status {
            workspace: std::path::PathBuf::from("D:/demo"),
        });
        let result =
            run_code_intel_request(Some(Arc::new(FixtureCodeIntel { fail: true })), request, None)
                .await;
        assert!(!result.success);
        assert!(result.output.contains("request timed out"));
    }

    #[tokio::test]
    async fn code_intel_completed_negative_answer_stays_successful() {
        let request = Ok(CodeIntelRequest::Definition(
            crate::codeintel::CodeIntelPositionQuery {
                workspace: std::path::PathBuf::from("D:/demo"),
                path: std::path::PathBuf::from("D:/demo/main.rs"),
                line: 1,
                column: 1,
                symbol: None,
                max_results: 20,
                cancellation: None,
            },
        ));
        let result =
            run_code_intel_request(Some(Arc::new(FixtureCodeIntel { fail: false })), request, None)
                .await;
        assert!(result.success);
        assert!(result.output.contains("not found"));
    }

    #[test]
    fn todo_parser_accepts_content_numeric_id_and_string_list() {
        let content = parse_todo_mutations(&json!({"content": "map the leak"}));
        assert_eq!(content.len(), 1);
        assert!(matches!(
            &content[0].1,
            TaskMutation::TodoAdd { title } if title == "map the leak"
        ));

        let numbered = parse_todo_mutations(&json!({
            "todos": [{"id": 1, "status": "inProgress"}]
        }));
        assert_eq!(numbered.len(), 1);
        assert!(matches!(
            numbered[0].1,
            TaskMutation::TodoSetStatus {
                status: TaskTodoStatus::InProgress
            }
        ));

        let listed = parse_todo_mutations(&json!({"todos": ["ship n2", "verify gate"]}));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].0, "ship n2");
        assert_eq!(listed[1].0, "verify gate");
    }

    #[test]
    fn single_redact_removes_sensitive_values_before_governor_relay() {
        let mut runtime = Runtime::new();
        runtime.register_sensitive_value("private-answer");
        let once = runtime.redact_sensitive("selected private-answer");
        assert_eq!(once, "selected [REDACTED]");
        assert_eq!(runtime.redact_sensitive(&once), once);
    }

    #[tokio::test]
    async fn batch_prepare_dedups_identical_raw_calls_in_order() {
        let runtime = Runtime::new();
        let calls = vec![
            ProviderToolCall {
                id: "one".into(),
                name: "read".into(),
                arguments: r#"{"path":".","max_lines":1}"#.into(),
            },
            ProviderToolCall {
                id: "two".into(),
                name: "read".into(),
                arguments: r#"{"path":".","max_lines":1}"#.into(),
            },
            ProviderToolCall {
                id: "three".into(),
                name: "list".into(),
                arguments: r#"{"path":".","max_entries":1}"#.into(),
            },
        ];
        let prepared = runtime
            .prepare_provider_tool_invocations(
                crate::OperatingMode::Auto,
                std::env::temp_dir().as_path(),
                &calls,
            )
            .await
            .expect("batch prepare");
        assert_eq!(prepared.len(), 3);
        assert_eq!(
            prepared[0].canonical_fingerprint,
            prepared[1].canonical_fingerprint
        );
        assert_eq!(prepared[0].target_paths, prepared[1].target_paths);
        assert_ne!(
            prepared[0].canonical_fingerprint,
            prepared[2].canonical_fingerprint
        );
    }

    #[test]
    fn skill_discovery_is_memoized_per_cwd_for_the_run() {
        fn write_skill(root: &std::path::Path, name: &str) {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).expect("skill dir");
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: fixture\n---\nbody\n"),
            )
            .expect("skill fixture");
        }

        let root = std::env::temp_dir().join(format!(
            "slim-skill-memo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let workspace = root.join("workspace");
        write_skill(&workspace.join(".slim").join("skills"), "first");
        let mut runtime = Runtime::new();
        let first = runtime
            .cached_skill_discovery(&workspace)
            .expect("first discovery");
        assert!(first.active("first").is_some());
        write_skill(&workspace.join(".slim").join("skills"), "second");
        let second = runtime
            .cached_skill_discovery(&workspace)
            .expect("memoized discovery");
        assert!(second.active("second").is_none());
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("other cwd");
        let other = runtime
            .cached_skill_discovery(&elsewhere)
            .expect("other cwd discovery");
        assert!(other.active("first").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compaction_summary_rejects_missing_required_headings() {
        let summary = CompactionSummary {
            text: "plain summary".into(),
            stop_reason: Some("stop".into()),
            ..CompactionSummary::default()
        };

        assert!(matches!(
            summary.validate(64 * 1024),
            Err(ProviderError::InvalidResponse { ref message })
                if message.contains("required heading")
        ));
    }

    #[test]
    fn compaction_summary_retains_first_response_timings() {
        let mut summary = CompactionSummary::default();
        summary.push(ProviderEvent::Phase {
            phase: ProviderPhase::FirstByte,
            elapsed_ms: 12,
        });
        summary.push(ProviderEvent::Phase {
            phase: ProviderPhase::FirstSemantic,
            elapsed_ms: 20,
        });

        assert_eq!(summary.time_to_first_byte_ms, Some(12));
        assert_eq!(summary.time_to_first_semantic_ms, Some(20));
    }

    #[test]
    fn multimodal_requests_are_not_eligible_for_text_estimator_calibration() {
        let text = ProviderMessage::user("hello");
        let image = ProviderMessage::user("").with_content_blocks(vec![
            crate::provider::ProviderContentBlock::image("image/png", "aGVsbG8="),
        ]);

        assert!(messages_are_text_only(&[text]));
        assert!(!messages_are_text_only(&[image]));
    }

    #[test]
    fn request_component_bytes_follow_serialized_wire_values() {
        let system = json!({"role": "system", "content": "rules\n"});
        let history = json!({"role": "user", "content": "a\"b"});
        let tool_result = json!({"role": "tool", "content": "line\n"});
        let tools = json!([{"type": "function", "name": "read"}]);
        let body = json!({
            "messages": [&system, &history, &tool_result],
            "tools": &tools,
        })
        .to_string();

        assert_eq!(
            request_component_bytes(&body),
            (
                serde_json::to_vec(&system).expect("system").len() as u64,
                serde_json::to_vec(&tools).expect("tools").len() as u64,
                serde_json::to_vec(&history).expect("history").len() as u64,
                serde_json::to_vec(&tool_result).expect("tool result").len() as u64,
            )
        );
    }

    #[test]
    fn runtime_goal_assurance_requires_validation_after_the_latest_mutation() {
        let progress = |seq, kind, revision| {
            crate::SessionEvent::new(
                seq,
                crate::EventKind::CausalProgressObserved {
                    batch_id: "batch".into(),
                    call_id: format!("call-{seq}").into(),
                    kind,
                    tool_name: "shell".into(),
                    call_fingerprint: "fingerprint".into(),
                    evidence_id: "evidence".into(),
                    workspace_revision: revision,
                },
            )
        };
        assert!(!runtime_goal_assurance(&[]));
        assert!(runtime_goal_assurance(&[progress(
            1,
            crate::CausalProgressKind::ValidationGreen,
            0,
        )]));
        assert!(!runtime_goal_assurance(&[
            progress(1, crate::CausalProgressKind::ValidationGreen, 0),
            progress(2, crate::CausalProgressKind::WorkspaceChanged, 1),
        ]));
    }

    #[test]
    fn cancel_pending_background_harvests_finished_task() {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                let plan = BackgroundCompactionPlan {
                    selection: CompactionSelection {
                        root_instruction: "root".into(),
                        summarized: vec![ProviderMessage::user("root")],
                        kept: Vec::new(),
                        first_kept_index: 1,
                        recent_tokens: 0,
                    },
                    request: None,
                    provider: "provider".into(),
                    model: "model".into(),
                    provider_identity: "provider:model".into(),
                    serialized_chars: 1,
                    system_bytes: 0,
                    history_bytes: 0,
                    summary_max_bytes: 64 * 1024,
                    source_len: 1,
                    tokens_before: 1,
                    projected_tokens_after: 1,
                    request_bytes: 1,
                    estimated_input_tokens: 1,
                    projected_savings_tokens: 1,
                    estimated_cost_tokens: 1,
                    safety_margin_tokens: 1,
                    future_turns: 1,
                    profitable: true,
                };
                let task = tokio::spawn(async move {
                    BackgroundCompactionResult {
                        plan,
                        summary: String::new(),
                        usage: UsageTotals::default(),
                        time_to_first_byte_ms: None,
                        time_to_first_semantic_ms: None,
                        duration_ms: 1,
                        valid: false,
                        usage_known: false,
                        cancelled: false,
                    }
                });
                while !task.is_finished() {
                    tokio::task::yield_now().await;
                }
                let progress = Arc::new(Mutex::new(CompactionAttemptProgress::default()));
                let mut pending = Some(PendingBackgroundCompaction {
                    task,
                    progress,
                    usage_request: RequestUsage {
                        request_kind: crate::RequestKind::Compaction,
                        provider: "provider".into(),
                        model: "model".into(),
                        history_bytes: 1,
                        estimated_input_tokens: 1,
                        ..RequestUsage::default()
                    },
                    request_bytes: 1,
                    estimated_input_tokens: 1,
                    tokens_before: 1,
                    started: Instant::now(),
                });
                let mut runtime = Runtime::new();
                let mut next_seq = 1;

                let usage = runtime
                    .cancel_pending_background(&mut pending, &mut next_seq, "loop_finished")
                    .await
                    .expect("settle finished task");

                assert!(pending.is_none());
                assert_eq!(usage.provider_turns, 0);
                assert_eq!(usage.requests.len(), 1);
                assert_eq!(
                    usage.requests[0].request_kind,
                    crate::RequestKind::Compaction
                );
                assert!(usage.requests[0].usage_unknown);
                assert!(runtime.app.events().iter().any(|event| matches!(
                    event.kind,
                    crate::EventKind::CompactionAttemptCompleted { .. }
                )));
                assert!(!runtime.app.events().iter().any(|event| matches!(
                    event.kind,
                    crate::EventKind::CompactionAttemptCancelled { .. }
                )));
            });
    }

    #[tokio::test]
    async fn dropping_pending_background_compaction_aborts_its_task() {
        let probe = std::sync::Arc::new(());
        let weak = std::sync::Arc::downgrade(&probe);
        let task = tokio::spawn(async move {
            let _held = probe;
            std::future::pending::<()>().await;
            unreachable!("background compaction task must be aborted on drop");
        });
        drop(PendingBackgroundCompaction {
            task,
            progress: Arc::new(Mutex::new(CompactionAttemptProgress::default())),
            usage_request: RequestUsage {
                ..RequestUsage::default()
            },
            request_bytes: 0,
            estimated_input_tokens: 0,
            tokens_before: 0,
            started: Instant::now(),
        });
        for _ in 0..100 {
            if weak.upgrade().is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            weak.upgrade().is_none(),
            "dropping the handle must abort the task and free its state"
        );
    }

    #[test]
    fn shell_prompt_output_preserves_head_and_tail_at_global_limit() {
        let result = ToolResult {
            name: "shell".into(),
            success: true,
            output: "HEADmiddleTAIL".into(),
            artifact: None,
        };

        assert_eq!(
            prompt_output(&result, 8),
            "HEAD\n[truncated 6 bytes by result limit; model sees first 4 and last 4 bytes]\nTAIL"
        );
    }
}
