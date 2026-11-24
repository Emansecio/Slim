//! The agent runtime: one turn's provider loop, tool execution and compaction.
//!
//! Invariants:
//! - A `Runtime` lives for one turn. While the loop runs it holds the only copy
//!   of the history and hands it back on every exit path.
//! - A tool batch runs in phases: snapshot reads in parallel, then in call
//!   order independent file mutations as parallel waves and every other call
//!   alone. Serial barriers (shell, skill, MCP, CodeMode) fail closed: no later read runs
//!   ahead of a pending barrier.
//! - `sensitive_values` has no empty entries and is sorted by decreasing length.
//!   Streamed text and tool outputs are redacted before they reach the events,
//!   the governor or the history; a tool call whose arguments carry a secret is
//!   rejected, not redacted.
//! - Cancellation is acknowledged only after native (blocking) work has stopped.
//!
//! Modules:
//! - `agent_loop`: the turn loop and its phases; `config`: loop config and results.
//! - `tool_batch`: batch execution, pool waves and tool lifecycle events;
//!   `tool_schedule`: which calls may run together, budgets and aliases.
//! - `tool_arguments`: normalization and validation of raw tool arguments.
//! - `stream_normalizer`: provider events to ledger events, redaction, call buffering.
//! - `redaction`: sensitive-value registry and redaction; `events`: ledger helpers.
//! - `presentation`: budgeted size of each tool result in the next request;
//!   `history_elision`: duplicate and superseded tool outputs; `request_estimate`:
//!   structural request size.
//! - `recovery`: provider error policy, retry and stop-reason classification;
//!   `manual_retry`: the user-triggered retry handle.
//! - `compaction_foreground`, `compaction_types`, `overflow`: Pi's compaction run
//!   before a request, its shared types, and context-overflow detection by text.
//! - `governor`: causal progress ledger; `loop_guard`: repeated failed-call detector.
//! - `native_*`: runtime-executed tools (artifact, ask, code_intel, mcp, skill, todo);
//!   `shell_jobs`: run-scoped background shell jobs.
//! - `codemode`: MCP composition and durable values; `codemode::engine`: isolated JS worker.
//! - `cancellation`: token and native-work barrier; `capability_bridge`: durable
//!   task state; `usage`: token accounting; `workspace`: initial path snapshot;
//!   `mode`: mode names, write projections and the harness channel.

mod agent_loop;
mod cancellation;
mod capability_bridge;
mod codemode;
mod compaction_foreground;
mod compaction_types;
mod config;
pub(crate) mod distill;
mod events;
mod governor;
mod history_elision;
mod loop_guard;
mod manual_retry;
mod mode;
mod native_artifact;
mod native_ask;
mod native_code_intel;
mod native_history;
mod native_mcp;
mod native_mcp_direct;
mod native_skill;
mod native_subagent;
mod native_todo;
mod speculative;
mod overflow;
#[cfg(test)]
mod performance;
mod presentation;
mod recovery;
mod redaction;
mod request_estimate;
mod run_guards;
pub(crate) mod scratchpad;
pub(crate) mod shell_fold;
mod shell_jobs;
mod steer;
mod stream_normalizer;
#[cfg(test)]
mod temp_root;
mod tool_arguments;
mod tool_batch;
mod tool_schedule;
mod usage;
mod workspace;

#[cfg(test)]
use agent_loop::runtime_goal_assurance;
pub use cancellation::CancellationToken;
use compaction_types::{CompactionOutcome, CompactionSummary, CompactionTrigger};
use config::ProviderTurnResult;
pub use config::{AgentLoopConfig, AgentLoopResult, AgentLoopStop};
use events::{
    assistant_text_since, checked_next_seq, drain_tool_started_notices, duration_millis,
    elapsed_micros, elapsed_millis, event_rejected, journal_error, lock_journal,
    push_runtime_event, push_runtime_transient_event, push_tool_progress, push_tool_result_facts,
    push_tool_started_notice, replace_admission_prefix, tool_calls_since, usage_since,
};
pub(crate) use events::{persist_provider_call, persist_provider_validation_failure};
#[cfg(test)]
use history_elision::ElisionStats;
use history_elision::{
    drop_earlier_reasoning, duplicate_pointer, elide_superseded_tool_outputs,
    elide_superseded_tool_outputs_with_penalty, estimate_micro_prune_savings_chars,
    micro_prune_before_cut, shrink_history_to_fit, tool_output_already_in_context,
    truncate_result,
};
use native_artifact::artifact_read_definition;
use native_code_intel::{prepared_code_intel_request, run_code_intel_request, EditedFiles};
use native_history::history_recall_definition;
use native_mcp::mcp_tool_definition;
use native_skill::skill_tool_definition;
#[cfg(test)]
use native_skill::{render_skill_list, run_skill_dispatch};
#[cfg(test)]
use native_todo::parse_todo_mutations;
use native_todo::{todo_tool_definition, TodoCadence, TODO_FINAL_REVIEW, TODO_PROGRESS_REVIEW};
use recovery::{
    annotate_provider_recovery_error, classify_provider_stop_reason, proves_task_progress,
    provider_recovery_delay, provider_retry_reason, recoverable_provider_error,
    request_emitted_tools, requested_provider_recovery_delay, AttemptCtx, ProviderAttempt,
    ProviderTurnStop, RecoveryBudget, RetryNotice, EMPTY_RESPONSE_MESSAGE,
    MAX_AUTOMATIC_RECOVERIES, MAX_PROVIDER_RECOVERIES, MAX_TRUNCATION_RECOVERIES,
    NO_STOP_REASON_MESSAGE,
};
#[cfg(test)]
use recovery::{is_context_overflow_error, provider_recovery_backoff, MAX_PROVIDER_RECOVERY_WAIT};
pub(crate) use redaction::redact_values;
use redaction::{redact_task_value, SensitiveValues};
use request_estimate::{
    estimate_unprepared_request_chars, image_payload_discount_chars, messages_are_text_only,
};
#[cfg(test)]
use stream_normalizer::sensitive_tool_arguments;
pub(crate) use stream_normalizer::{
    take_redacted_stream_chunk, tool_events_contain_sensitive_values, ProviderStreamNormalizer,
};
use tool_arguments::{
    assign_missing_call_ids, object_arguments, validate_tool_arguments, ArgumentShape,
};
pub use tool_schedule::tool_call_is_read_only;
use tool_schedule::{
    disjoint_ready_reads_for_mutations, evidence_reuse_aliases, independent_mutation_cluster,
    is_file_mutation, is_serial_barrier, mark_cancelled_tool_outcome, phase1_snapshot_indices,
    phase1_snapshot_indices_ready, should_stop_after_tool_budget_cut, split_calls_for_budget,
    suppressed_calls_steer, tool_call_slots, PoolOutcome, ToolInvocation, ToolStartedNotice,
    BATCH_CONCURRENCY, BUDGET_FINALIZE_PROMPT, MAX_BUDGET_STEERS, NO_PROGRESS_FINALIZE_PROMPT,
    TUNING_CLOSED_ERROR, TUNING_FINALIZE_PROMPT,
};
pub use usage::{RequestUsage, UsageTotals};

pub use workspace::without_workspace_snapshot;

use crate::codeintel::CodeIntelligence;
use crate::context::{
    apply_compaction, estimate_context_tokens, estimate_system_and_tools_tokens,
    fit_summary_for_persistence, prepare_compaction, should_compact, usable_anchor,
    AdaptiveTokenEstimator, ArtifactStore, CompactionCommit, CompactionHandle, CompactionPolicy,
    CompactionReason, ContextBudget, ContextUsage, UsageAnchor,
};
use crate::interaction::{
    ask_question_definition, AskQuestion, InteractionRequestId, InteractionRoute,
};
use crate::mcp::{McpCancellation, McpManager, McpRequestOutcome};
use crate::model::AppHandle;
use crate::provider::{
    is_transient_http_status, HttpProviderClient, PreparedProviderRequest, ProviderAdapter,
    ProviderCallTelemetry, ProviderError, ProviderEvent, ProviderKind, ProviderMessage,
    ProviderPhase, ProviderRequestComponents, ProviderToolCall, STREAM_ENDED_EARLY_MESSAGE,
};
use crate::session::{
    AuthorizationGrant, CapabilityCatalog, CapabilityLedgerError, DurableFact, DurableRecord,
    DurableRepo, DurableSessionHeader, MemoryRepo, TaskMutation, TaskMutationRequest,
    TaskTodoStatus,
};
use crate::skills::{discover_workspace, DiscoveryResult};
use crate::tools::{
    lock_mutex, present_unstructured, CodeIntelRequest, PreparedToolArguments,
    PreparedToolInvocation, PresentationBudget, ToolEffectClass, ToolExecutionOutcome,
    ToolExecutionReceipt, ToolPresentation, ToolPresentationSource, ToolRegistry, ToolResult,
};
use futures_util::StreamExt;
use governor::{CausalGovernor, GovernorObservation};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use tokio::sync::Notify;

pub use capability_bridge::RuntimeCapabilityBridge;
pub use loop_guard::LoopGuard;
pub use manual_retry::ManualRetryHandle;
pub use mode::mode_name;
pub use shell_jobs::{ShellJobInfo, ShellJobLimits, ShellJobOutput, ShellJobScope, ShellJobs};
pub use native_subagent::{
    PauseGate, SubagentDelivery, SubagentInfo, SubagentIsolation, SubagentModelTier,
    SubagentOutputFormat, SubagentRegistry, SubagentResumeOutcome, SubagentSpawnRequest,
    SubagentStatus, SubagentStep, MAX_CONCURRENT_SUBAGENTS,
};
pub use steer::SteerInbox;

pub struct Runtime {
    pub app: AppHandle,
    tools: ToolRegistry,
    shell_jobs: shell_jobs::ShellJobs,
    session_shell_jobs: bool,
    shell_job_run_start: u64,
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
    steer_inbox: Option<SteerInbox>,
    /// The shell call about to run is the last pending one of its batch;
    /// consumed by the inline wait.
    shell_ends_batch: bool,
    read_presentation_bytes: usize,
    write_projection_cache: Mutex<mode::WriteProjectionCache>,
    code_intel: Option<Arc<dyn CodeIntelligence>>,
    edited: EditedFiles,
    /// The run is in its time budget's finalize phase: tuning-only patches
    /// are refused.
    tuning_closed: bool,
    /// Re-read, full-rewrite and failing-command holds of this run.
    run_guards: run_guards::RunGuards,
    mcp: Option<Arc<McpManager>>,
    codemode: codemode::CodeMode,
    token_estimator: AdaptiveTokenEstimator,
    /// Skill discovery memoized for one loop run (`Runtime` is per-turn).
    /// `None` inside means discovery failed; callers fall back to direct
    /// discovery, which owns the error messages.
    skill_discovery_cache: Option<(PathBuf, Option<DiscoveryResult>)>,
    /// Optional skill discovery inherited from a parent runtime (e.g. ad-hoc
    /// subagents) so the first loop run skips redundant disk scanning.
    preseeded_skill_discovery: Option<(PathBuf, Option<DiscoveryResult>)>,
    /// Optional redacted workspace snapshot inherited from a parent runtime
    /// so ad-hoc subagents avoid repeating the workspace directory walk.
    preseeded_workspace_snapshot: Option<String>,
    presentation_sources: std::collections::HashMap<(String, String), ToolPresentationSource>,
    subagents: Option<SubagentRegistry>,
    subagent_depth: u8,
    pause_gate: Option<PauseGate>,
    /// Direct MCP tool definitions snapshotted once per run so a lazy MCP
    /// server connecting mid-run never mutates `tools` and busts prompt cache.
    frozen_direct_mcp_tools: Option<Vec<Value>>,
    /// Direct MCP tool names promoted to full schemas during this session
    /// (append-only to preserve the cached tool prefix).
    promoted_direct_mcp_tools: std::collections::BTreeSet<String>,
    /// Compact structured working memory updated as tools succeed.
    scratchpad: scratchpad::SessionScratchpad,
    /// Whether the active adapter steers a provider prompt cache (higher
    /// break-even threshold before rewriting earlier history).
    prompt_cache_active: bool,
    /// Cached-write / cached-read break-even multiplier for this provider.
    cache_write_penalty: usize,
    /// Durable session JSONL archive backing `history_recall` once the session
    /// has compacted history.
    session_history_archive: Option<PathBuf>,
    /// Speculative snapshot reads dispatched during streaming before the full batch settled.
    speculative_reads: std::collections::HashMap<String, SpeculativeReadEntry>,
    speculative_cancel: Option<CancellationToken>,
    /// Optional model override (e.g. `model_tier: "fast"` subagents).
    model_override: Option<String>,
    /// Whether this runtime requests higher reasoning effort when lower is configured.
    deep_reasoning: bool,
    /// Pre-sanitized parent conversation snapshot available during a `subagent` batch.
    active_fork_context: Option<Arc<[ProviderMessage]>>,
    /// Exact parent tools array frozen for `fork_context` children so KV-cache
    /// prefixes remain byte-identical across fork boundaries.
    frozen_tools_override: Option<Arc<[Value]>>,
    /// Parent's active channel anchors snapshot during a `subagent` batch.
    active_fork_channel_anchors: Vec<(usize, crate::OperatingMode, String)>,
    /// MCP server names explicitly permitted even in `ReadOnly`/`Plan` mode
    /// (e.g. trusted host board server `slim_swarm`).
    mcp_readonly_allowlist: std::collections::BTreeSet<String>,
    /// True inside a `readonly` child subagent even when inheriting Auto tools.
    subagent_readonly: bool,
}

pub(super) use speculative::{
    speculative_v2_enabled, SpeculativeOutcome, SpeculativeReadEntry, StreamSpeculator,
    StreamSpeculatorConfig,
};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct ToolDefinitionSetKey {
    mode: crate::OperatingMode,
    code_intel_enabled: bool,
    artifact_enabled: bool,
    history_recall_enabled: bool,
    interaction_enabled: bool,
    mcp_enabled: bool,
    skills_enabled: bool,
    subagents_enabled: bool,
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
            session_shell_jobs: false,
            shell_job_run_start: 0,
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
            steer_inbox: None,
            shell_ends_batch: false,
            read_presentation_bytes: 64 * 1024,
            write_projection_cache: Mutex::new(mode::WriteProjectionCache::default()),
            code_intel: None,
            edited: EditedFiles::default(),
            tuning_closed: false,
            run_guards: run_guards::RunGuards::default(),
            mcp: None,
            codemode: codemode::CodeMode::default(),
            token_estimator: AdaptiveTokenEstimator::default(),
            skill_discovery_cache: None,
            preseeded_skill_discovery: None,
            preseeded_workspace_snapshot: None,
            presentation_sources: std::collections::HashMap::new(),
            subagents: None,
            subagent_depth: 0,
            pause_gate: None,
            frozen_direct_mcp_tools: None,
            promoted_direct_mcp_tools: std::collections::BTreeSet::new(),
            scratchpad: scratchpad::SessionScratchpad::default(),
            prompt_cache_active: false,
            cache_write_penalty: 1,
            session_history_archive: None,
            speculative_reads: std::collections::HashMap::new(),
            speculative_cancel: None,
            model_override: None,
            deep_reasoning: false,
            active_fork_context: None,
            frozen_tools_override: None,
            active_fork_channel_anchors: Vec::new(),
            mcp_readonly_allowlist: std::collections::BTreeSet::new(),
            subagent_readonly: false,
        }
    }

    pub(super) fn clear_speculative_reads(&mut self) {
        if let Some(cancel) = self.speculative_cancel.take() {
            cancel.cancel();
        }
        for (_, entry) in self.speculative_reads.drain() {
            entry.handle.abort();
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

    pub fn set_session_shell_jobs(&mut self, jobs: ShellJobs) {
        self.shell_jobs = jobs;
        self.session_shell_jobs = true;
    }
    pub fn set_shell_job_limits(&mut self, limits: ShellJobLimits) -> Result<(), String> {
        self.shell_jobs = ShellJobs::new(limits)?;
        Ok(())
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

    /// Whether `ask_question` is advertised in `mode`. The tool set and the
    /// channel stanza must both derive from this so they never disagree.
    fn can_ask(&self, mode: crate::OperatingMode) -> bool {
        self.interaction_route.is_some() && mode != crate::OperatingMode::Plan
    }

    pub fn set_compaction_handle(&mut self, handle: CompactionHandle) {
        self.compaction_handle = Some(handle);
    }

    fn compaction_policy(&self) -> CompactionPolicy {
        self.compaction_handle
            .as_ref()
            .map(CompactionHandle::policy)
            .unwrap_or_default()
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
        let mut text = assistant_text_since(&self.app, start);
        // Index finishes and the first output of each call once.
        let mut finished = std::collections::HashSet::<(&str, &str)>::new();
        let mut outputs = std::collections::HashMap::<(&str, &str), &String>::new();
        for event in events {
            match &event.kind {
                crate::EventKind::ToolFinished {
                    batch_id, call_id, ..
                } => {
                    finished.insert((batch_id.as_str(), call_id.as_str()));
                }
                crate::EventKind::ToolOutput {
                    batch_id,
                    call_id,
                    output,
                    ..
                } => {
                    outputs
                        .entry((batch_id.as_str(), call_id.as_str()))
                        .or_insert(output);
                }
                _ => {}
            }
        }
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
            let key = (batch_id.as_str(), call_id.as_str());
            let output = outputs
                .get(&key)
                .copied()
                .filter(|_| finished.contains(&key));
            if let Some(output) = output {
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
                let _ = write!(text, "\nTool {name} ({call_id}) started without a confirmed result; its effects are unknown. Do not replay it automatically.");
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
            lock_journal(journal)?
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

    /// Permits an explicit trusted MCP server (for example a host-owned board
    /// server) to be advertised and called even in `ReadOnly` / `Plan` mode.
    pub fn allow_readonly_mcp_server(&mut self, server: impl Into<String>) {
        self.mcp_readonly_allowlist.insert(server.into());
    }

    /// Error from the best-effort closing turn after a budget stop, if any.
    pub fn finalization_error(&self) -> Option<&ProviderError> {
        self.finalization_error.as_ref()
    }

    /// Canonical provider conversation after any compaction and completed
    /// tool turns. Interactive callers persist this instead of rebuilding a
    /// lossy transcript from rendered output.
    pub fn conversation(&self) -> &[ProviderMessage] {
        &self.conversation
    }

    fn base_tool_definition_set(
        &self,
        mode: crate::OperatingMode,
        code_intel_enabled: bool,
    ) -> Arc<[Value]> {
        let key = ToolDefinitionSetKey {
            mode,
            artifact_enabled: self.artifact_store.is_some(),
            history_recall_enabled: self.session_history_archive.is_some(),
            skills_enabled: self
                .skill_discovery_cache
                .as_ref()
                .and_then(|(_, discovery)| discovery.as_ref())
                .is_none_or(|discovery| {
                    !discovery.active_entries().is_empty() || !discovery.warnings.is_empty()
                }),
            code_intel_enabled,
            interaction_enabled: self.can_ask(mode),
            mcp_enabled: self
                .mcp
                .as_ref()
                .is_some_and(|manager| manager.has_enabled_servers()),
            subagents_enabled: self.subagent_depth == 0 && self.subagents.is_some(),
        };
        let mut sets = lock_mutex(&TOOL_DEFINITION_SETS);
        if let Some(definitions) = sets.get(&key) {
            return Arc::clone(definitions);
        }
        if mode == crate::OperatingMode::Code {
            // Code mode: one orchestration tool; workspace and MCP tools are
            // reached from the program, never advertised one by one.
            let mut tools = vec![codemode::definition_for_with_subagents(
                mode,
                code_intel_enabled,
                key.mcp_enabled,
                key.subagents_enabled,
            )];
            if key.artifact_enabled {
                tools.push(artifact_read_definition());
            }
            if key.history_recall_enabled {
                tools.push(history_recall_definition());
            }
            if key.interaction_enabled {
                tools.push(ask_question_definition());
            }
            // Meta tools whose result belongs to the conversation, not to a
            // program: the visible plan and skill instructions. Subagents run via
            // `slim.subagent(...)` and `slim.subagent_control(...)` inside `codemode`
            // (top-level calls remain accepted as fallback without duplicating schemas).
            tools.push(todo_tool_definition());
            if key.skills_enabled {
                tools.push(skill_tool_definition());
            }
            let definitions: Arc<[Value]> = tools.into();
            sets.insert(key, Arc::clone(&definitions));
            return definitions;
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
        if key.history_recall_enabled {
            tools.push(history_recall_definition());
        }
        if key.interaction_enabled {
            tools.push(ask_question_definition());
        }
        if key.subagents_enabled {
            tools.push(native_subagent::subagent_definition());
            tools.push(native_subagent::subagent_control_definition());
        }
        if mode.allows_mutation() {
            tools.push(shell_jobs::definition());
            tools.push(todo_tool_definition());
            if key.skills_enabled {
                tools.push(skill_tool_definition());
            }
            if key.mcp_enabled {
                tools.push(mcp_tool_definition());
                tools.push(codemode::definition(mode, code_intel_enabled));
            }
        }
        if mode == crate::OperatingMode::Auto && codemode::batch_code_enabled() {
            tools.push(codemode::batch_code_definition(code_intel_enabled));
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
        self.workspace_tool_definitions_for(mode, cwd, false)
    }

    fn workspace_tool_definitions_for(
        &self,
        mode: crate::OperatingMode,
        cwd: &Path,
        stable_envelope: bool,
    ) -> Arc<[Value]> {
        if let Some(frozen) = self.frozen_tools_override.as_ref() {
            return Arc::clone(frozen);
        }
        let advertised_mode = if stable_envelope && mode != crate::OperatingMode::Code {
            crate::OperatingMode::Auto
        } else {
            mode
        };
        let code_intel_enabled = self
            .code_intel
            .as_ref()
            .is_some_and(|backend| backend.supports_workspace(cwd));
        self.tool_definition_set(advertised_mode, code_intel_enabled)
    }

    pub fn tools_for_mode(&self, mode: crate::OperatingMode) -> Vec<&'static str> {
        self.tools.names_for_mode(mode)
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    fn ensure_not_cancelled(&self) -> Result<(), ProviderError> {
        if self.is_cancelled() {
            Err(ProviderError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn observed_next_seq(&self, fallback: u64) -> u64 {
        self.app
            .events()
            .last()
            .and_then(|event| event.seq.checked_add(1))
            .unwrap_or(fallback)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
