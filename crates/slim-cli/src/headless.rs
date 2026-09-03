use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde::Serialize;
use slim_core::context::CompactionHandle;
use slim_core::provider::{
    clinepass_model, codex_model, command_code_model, open_code_model,
    resolve_codex_context_window, AnthropicAdapter, ClinePassAdapter, CommandCodeAdapter,
    HttpProviderClient, OpenAiCodexAdapter, OpenAiCompatibleAdapter, OpenCodeGoAdapter,
    ProviderConfig, ProviderContentBlock, ProviderError, ProviderKind, ProviderPricing,
    ProviderTimeouts, DEFAULT_MAX_OUTPUT_TOKENS,
};
use slim_core::runtime::{
    tool_call_is_read_only, AgentLoopConfig, AgentLoopStop, CancellationToken,
};
use slim_core::session::{
    open_resume_v2, preflight_session, DurableErrorClass, DurableOutcome, DurableRepo,
    ManualExecutor, ManualRunSpec, ProviderResponse, SessionFormat, SessionPreflight,
    SessionWriter,
};
use slim_core::tools::ToolRegistry;
use slim_core::{
    EventKind, InteractionRoute, OperatingMode, ProviderMessage, RequestKind, Runtime,
    SessionEvent, SessionEventSender, UsageTotals,
};

use crate::codex_catalog::{should_fetch_live_codex_catalog, CodexCatalog};
use crate::exit_codes::ExitCode;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Text,
    Jsonl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessRequest {
    pub prompt: String,
    pub mode: OperatingMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessResult {
    pub code: ExitCode,
    pub message: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderRequest {
    pub prompt: String,
    pub mode: OperatingMode,
    pub kind: ProviderKind,
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub account_id: Option<String>,
    pub timeout: Duration,
}

/// Maximum size accepted for one local CLI image: 20 MiB.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
pub(crate) const MAX_SLASH_SKILL_BODY_BYTES: usize = 8 * 1024;
const MAX_SLASH_SKILL_SYSTEM_PROMPT_BYTES: usize = 10_000;

/// Cloneable identity for one application-scoped LSP manager. Equality is
/// pointer identity so ProviderRunOptions remains deterministic in tests.
#[derive(Clone)]
pub struct CodeIntelligenceHandle(Arc<slim_lsp::LspCodeIntelligence>);

impl CodeIntelligenceHandle {
    pub fn new(manager: Arc<slim_lsp::LspCodeIntelligence>) -> Self {
        Self(manager)
    }

    fn manager(&self) -> &Arc<slim_lsp::LspCodeIntelligence> {
        &self.0
    }

    pub(crate) fn warm_workspace(&self, workspace: std::path::PathBuf) {
        self.0.warm_workspace(workspace);
    }

    pub(crate) async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}

impl std::fmt::Debug for CodeIntelligenceHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodeIntelligenceHandle")
            .field("manager", &"application-scoped")
            .finish()
    }
}

impl PartialEq for CodeIntelligenceHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CodeIntelligenceHandle {}

/// Cloneable registry retained by one interactive host across per-turn runtimes.
#[derive(Clone)]
pub struct SharedToolRegistry(Arc<ToolRegistry>);

impl SharedToolRegistry {
    fn new() -> Self {
        Self(Arc::new(ToolRegistry::default()))
    }

    fn registry(&self) -> ToolRegistry {
        self.0.as_ref().clone()
    }
}

impl std::fmt::Debug for SharedToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedToolRegistry")
            .field("registry", &"interactive-session-scoped")
            .finish()
    }
}

impl PartialEq for SharedToolRegistry {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SharedToolRegistry {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderRunOptions {
    pub content_blocks: Vec<ProviderContentBlock>,
    pub history: Vec<ProviderMessage>,
    pub workspace_root: Option<PathBuf>,
    pub artifact_root: Option<PathBuf>,
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub max_turns: Option<usize>,
    /// Mutating-tool budget (write/patch/shell/todo/skill/ask_question).
    pub max_tool_calls: Option<usize>,
    pub max_read_tool_calls: Option<usize>,
    pub max_result_bytes: Option<usize>,
    pub cancellation: Option<CancellationToken>,
    pub compaction: Option<CompactionHandle>,
    /// Shared for the whole host application; cloned into per-turn runtimes.
    pub code_intelligence: Option<CodeIntelligenceHandle>,
    /// Native tool snapshots retained for the lifetime of an interactive host.
    pub tool_registry: Option<SharedToolRegistry>,
    /// TUI Plan runs the read-only agent loop. Headless Plan stays abort-only.
    pub allow_plan_loop: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInstructions {
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) source: PathBuf,
}

impl ProviderRunOptions {
    pub fn with_content_blocks(mut self, blocks: Vec<ProviderContentBlock>) -> Self {
        self.content_blocks = blocks;
        self
    }

    pub fn with_history(mut self, history: Vec<ProviderMessage>) -> Self {
        self.history = history;
        self
    }

    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    pub fn with_artifact_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.artifact_root = Some(root.into());
        self
    }

    pub fn with_context_window_tokens(mut self, tokens: u64) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    pub fn with_max_turns(mut self, turns: usize) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn with_max_tool_calls(mut self, calls: usize) -> Self {
        self.max_tool_calls = Some(calls);
        self
    }

    pub fn with_max_read_tool_calls(mut self, calls: usize) -> Self {
        self.max_read_tool_calls = Some(calls);
        self
    }

    pub fn with_max_result_bytes(mut self, bytes: usize) -> Self {
        self.max_result_bytes = Some(bytes);
        self
    }

    pub fn with_compaction_handle(mut self, handle: CompactionHandle) -> Self {
        self.compaction = Some(handle);
        self
    }

    pub fn with_code_intelligence(mut self, manager: Arc<slim_lsp::LspCodeIntelligence>) -> Self {
        self.code_intelligence = Some(CodeIntelligenceHandle::new(manager));
        self
    }

    pub(crate) fn ensure_shared_tool_registry(&mut self) {
        if self.tool_registry.is_none() {
            self.tool_registry = Some(SharedToolRegistry::new());
        }
    }

    pub fn with_allow_plan_loop(mut self, allow: bool) -> Self {
        self.allow_plan_loop = allow;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderHeadlessResult {
    pub code: ExitCode,
    pub provider: ProviderKind,
    pub model: String,
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub stop_reason: Option<String>,
    pub stop: String,
    pub cost_micros: Option<u64>,
    pub usage_complete: bool,
    pub usage_overflowed: bool,
    pub usage: UsageTotals,
    pub costs: UsageCostSummary,
    pub validation_source: Option<String>,
    pub tool_summary_lines: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct UsageCostSummary {
    pub total_micros: Option<u64>,
    pub cost_per_validated_completion_micros: Option<u64>,
    pub failed_attempts_micros: Option<u64>,
    pub compaction_micros: Option<u64>,
    pub cancelled_estimated_micros: Option<u64>,
}

pub(crate) struct ProviderExecution {
    pub result: ProviderHeadlessResult,
    pub history: Option<Vec<ProviderMessage>>,
    pub events: Vec<SessionEvent>,
    pub tool_results: Vec<slim_core::tools::ToolResult>,
    pub max_mutating_tool_calls: usize,
    pub max_read_tool_calls: usize,
    pub max_turns: usize,
    pub max_output_tokens: u32,
    pub resume_preflight: Option<SessionPreflight>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolLoopLimits {
    pub max_mutating_tool_calls: usize,
    pub max_read_tool_calls: usize,
    pub max_turns: usize,
    pub max_output_tokens: u32,
}

#[derive(Serialize)]
struct JsonlResult<'a> {
    version: u8,
    kind: &'a str,
}

pub fn run_fake_headless(request: HeadlessRequest) -> HeadlessResult {
    if request.prompt.trim().is_empty() {
        return HeadlessResult {
            code: ExitCode::InputRequired,
            message: "input_required".into(),
        };
    }
    if request.mode == OperatingMode::Plan {
        return HeadlessResult {
            code: ExitCode::ApprovalRequired,
            message: "approval_required".into(),
        };
    }
    HeadlessResult {
        code: ExitCode::Success,
        message: "success".into(),
    }
}

pub fn run_provider_headless(
    request: ProviderRequest,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_with_options(request, ProviderRunOptions::default())
}

pub fn run_provider_headless_with_options(
    request: ProviderRequest,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_inner(request, None, options)
}

pub fn run_provider_headless_with_session(
    request: ProviderRequest,
    session_path: impl AsRef<Path>,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_with_session_and_options(
        request,
        session_path,
        ProviderRunOptions::default(),
    )
}

pub fn run_provider_headless_with_session_and_options(
    request: ProviderRequest,
    session_path: impl AsRef<Path>,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_inner(request, Some(session_path.as_ref()), options)
}

/// Execute one explicitly requested prompt against a healthy durable v2
/// session. This is intentionally separate from the legacy `--session` path:
/// v1 remains the default writer and is never silently migrated.
pub fn run_provider_headless_with_resume(
    request: ProviderRequest,
    session_path: impl AsRef<Path>,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_headless_with_resume_and_options(
        request,
        session_path,
        ProviderRunOptions::default(),
    )
}

/// Resume a durable v2 session after a read-only preflight and persist a new
/// causal provider operation around exactly one provider turn. Existing
/// pending/claimed/terminal work is only reconstructed; it is never replayed
/// by this function.
pub fn run_provider_headless_with_resume_and_options(
    request: ProviderRequest,
    session_path: impl AsRef<Path>,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    let preflight = preflight_session(session_path.as_ref())
        .map_err(|error| resume_error(error.to_string()))?;
    run_provider_resume_with_preflight_events(request, preflight, options, None)
        .map(|execution| execution.result)
}

pub(crate) fn run_provider_headless_with_resume_preflight_and_options(
    request: ProviderRequest,
    preflight: SessionPreflight,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_resume_with_preflight_events(request, preflight, options, None)
        .map(|execution| execution.result)
}

pub(crate) fn run_provider_resume_with_preflight_events(
    request: ProviderRequest,
    preflight: SessionPreflight,
    options: ProviderRunOptions,
    event_sender: Option<SessionEventSender>,
) -> Result<ProviderExecution, ProviderError> {
    block_on_provider(run_provider_resume_with_preflight_events_inner(
        request,
        preflight,
        options,
        event_sender,
        None,
        None,
    ))
}

/// TUI-only durable continuation. The provider operation is still persisted
/// before execution and an interrupted operation remains non-resumable, but
/// the live TUI keeps its ordinary tool budgets and interaction route.
pub(crate) async fn run_provider_resume_with_preflight_events_interactive_async(
    request: ProviderRequest,
    preflight: SessionPreflight,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
    event_sender: Option<SessionEventSender>,
    interaction_route: InteractionRoute,
) -> Result<ProviderExecution, ProviderError> {
    run_provider_resume_with_preflight_events_inner(
        request,
        preflight,
        options,
        event_sender,
        Some(interaction_route),
        skill_instructions,
    )
    .await
}

async fn run_provider_resume_with_preflight_events_inner(
    request: ProviderRequest,
    preflight: SessionPreflight,
    options: ProviderRunOptions,
    event_sender: Option<SessionEventSender>,
    interaction_route: Option<InteractionRoute>,
    skill_instructions: Option<SkillInstructions>,
) -> Result<ProviderExecution, ProviderError> {
    ensure_resume_preflight(&preflight).map_err(resume_error)?;
    if request.prompt.trim().is_empty() {
        return Ok(empty_provider_execution(input_required_result(&request)));
    }
    if request.mode == OperatingMode::Plan && !options.allow_plan_loop {
        return execute_provider_turn_with_skill_async(request, None, options, skill_instructions)
            .await;
    }

    let DurableProviderHistory {
        messages: history,
        parent_entry_id,
        mut entry_ids,
        applied_checkpoint_id,
    } = durable_provider_history(&preflight)?;
    let (mut repo, _plan) =
        open_resume_v2(&preflight).map_err(|error| resume_error(error.to_string()))?;
    let first_seq = repo
        .next_seq()
        .map_err(|error| resume_error(error.to_string()))?;
    let operation_id = format!("resume-{}-{first_seq}", repo.header().id);
    let input_entry_id = format!("{operation_id}-input");
    let assistant_entry_id = format!("{operation_id}-assistant");
    let attempt_id = format!("{operation_id}-attempt");
    let redacted_prompt = redact_secret(&request.prompt, &request.api_key);
    let mut persisted_messages = history.clone();
    persisted_messages.push(ProviderMessage::user(redacted_prompt.clone()));
    entry_ids.push(Some(input_entry_id.clone()));
    let mut spec = ManualRunSpec::new(
        operation_id.clone(),
        attempt_id,
        input_entry_id,
        assistant_entry_id,
        redacted_prompt,
        first_seq,
    );
    if let Some(parent_entry_id) = parent_entry_id {
        spec = spec.with_parent_entry_id(parent_entry_id);
    }
    let mut options = options;
    options.history = history;
    let compaction_generation = options
        .compaction
        .as_ref()
        .map(slim_core::context::CompactionHandle::generation)
        .unwrap_or(0);
    let compaction_handle = options.compaction.clone();
    let mut executor = DurableProviderExecutor::new(
        request,
        options,
        skill_instructions,
        event_sender,
        interaction_route,
    );
    let drive_result =
        slim_core::session::drive_manual_async(&mut repo, spec, classify_provider_error, || {
            executor.execute_async()
        })
        .await;
    match drive_result {
        Ok(()) => {
            if let Some(commit) = compaction_handle
                .as_ref()
                .filter(|handle| handle.generation() > compaction_generation)
                .and_then(slim_core::context::CompactionHandle::last_commit)
            {
                if let Some(first_kept_entry_id) = durable_checkpoint_anchor(
                    &persisted_messages,
                    &entry_ids,
                    commit.first_kept_index,
                    &commit.prefix_fingerprint,
                ) {
                    let seq = repo
                        .next_seq()
                        .map_err(|error| resume_error(error.to_string()))?;
                    repo.append(slim_core::session::DurableRecord::Compaction {
                        seq,
                        checkpoint: slim_core::session::CompactionCheckpoint {
                            checkpoint_id: format!("compact-{}-{seq}", repo.header().id),
                            summary: commit.summary,
                            first_kept_entry_id,
                            prefix_fingerprint: commit.prefix_fingerprint,
                            previous_checkpoint_id: applied_checkpoint_id,
                            tokens_before: commit.tokens_before,
                            tokens_after: commit.tokens_after,
                            input_tokens: Some(commit.input_tokens),
                            output_tokens: Some(commit.output_tokens),
                            duration_ms: commit.duration_ms,
                            reason: commit.reason,
                            read_files: Vec::new(),
                            modified_files: Vec::new(),
                        },
                    })
                    .map_err(|error| resume_error(error.to_string()))?;
                }
            }
            let mut execution = executor
                .execution
                .ok_or_else(|| resume_error("durable provider execution produced no result"))?;
            execution.resume_preflight = Some(SessionPreflight::from_open_repo(&repo));
            Ok(execution)
        }
        Err(slim_core::session::ManualDriveError::Execute(error)) => {
            let seq = repo
                .next_seq()
                .map_err(|error| resume_error(error.to_string()))?;
            let kind = if matches!(error, ProviderError::Cancelled) {
                slim_core::session::DurableOperationKind::Aborted
            } else {
                slim_core::session::DurableOperationKind::Finished {
                    outcome: DurableOutcome::Failed,
                }
            };
            repo.append(slim_core::session::DurableRecord::Operation {
                seq,
                operation: slim_core::session::DurableOperation { operation_id, kind },
            })
            .map_err(|error| resume_error(error.to_string()))?;
            Err(error)
        }
        Err(other) => Err(resume_error(format!("{other:?}"))),
    }
}

fn durable_checkpoint_anchor(
    messages: &[ProviderMessage],
    entry_ids: &[Option<String>],
    first_kept_index: usize,
    prefix_fingerprint: &str,
) -> Option<String> {
    if messages.len() != entry_ids.len()
        || first_kept_index >= messages.len()
        || slim_core::context::compaction_prefix_fingerprint(&messages[..first_kept_index])
            != prefix_fingerprint
    {
        return None;
    }
    entry_ids.get(first_kept_index)?.clone()
}

pub(crate) fn ensure_resume_preflight(preflight: &SessionPreflight) -> Result<(), String> {
    if preflight.format != Some(SessionFormat::DurableV2) {
        return Err(
            "resume requires durable schema v2; legacy session schema v1 is not migrated"
                .to_owned(),
        );
    }
    if !preflight.can_resume_v2() {
        return Err(match &preflight.status {
            slim_core::session::PreflightStatus::TornTail { .. } => {
                "resume requires explicit recovery before using a torn durable session tail"
                    .to_owned()
            }
            slim_core::session::PreflightStatus::Invalid { message } => {
                format!("resume rejected invalid durable session: {message}")
            }
            slim_core::session::PreflightStatus::UnsupportedSchema { schema_version } => {
                format!("resume rejected unsupported session schema v{schema_version}")
            }
            slim_core::session::PreflightStatus::Healthy if preflight.needs_separator => {
                "resume requires explicit recovery before appending to a session without a final separator".into()
            }
            slim_core::session::PreflightStatus::Healthy if preflight.sequence_overflow => {
                "resume rejected durable session sequence overflow".into()
            }
            slim_core::session::PreflightStatus::Healthy => {
                "resume rejected durable session preflight".into()
            }
        });
    }
    if preflight.summary.pending_count() > 0
        || preflight.summary.claimed_count() > 0
        || preflight.summary.suspended_count() > 0
    {
        return Err(
            "resume requires an explicit decision for existing pending, claimed, or suspended durable work"
                .into(),
        );
    }
    Ok(())
}

struct DurableProviderHistory {
    messages: Vec<ProviderMessage>,
    parent_entry_id: Option<String>,
    entry_ids: Vec<Option<String>>,
    applied_checkpoint_id: Option<String>,
}

fn durable_provider_history(
    preflight: &SessionPreflight,
) -> Result<DurableProviderHistory, ProviderError> {
    let mut history = Vec::new();
    let mut entry_ids = Vec::new();
    let mut parent_entry_id = None;
    for record in &preflight.records {
        let slim_core::session::DurableRecord::Entry { entry, .. } = record else {
            continue;
        };
        if entry.tool_call_id.is_some() {
            return Err(resume_error(
                "resume cannot reconstruct durable tool-call metadata safely",
            ));
        }
        parent_entry_id = Some(entry.entry_id.clone());
        match entry.role {
            slim_core::session::DurableEntryRole::User => {
                history.push(ProviderMessage::user(entry.content.clone()));
                entry_ids.push(Some(entry.entry_id.clone()));
            }
            slim_core::session::DurableEntryRole::Assistant => {
                history.push(ProviderMessage::assistant(
                    entry.content.clone(),
                    Vec::new(),
                ));
                entry_ids.push(Some(entry.entry_id.clone()));
            }
            slim_core::session::DurableEntryRole::Tool => {
                return Err(resume_error(
                    "resume cannot reconstruct durable tool-role history safely",
                ))
            }
        }
    }
    let mut applied_checkpoint_id: Option<String> = None;
    for checkpoint in preflight.records.iter().filter_map(|record| {
        if let slim_core::session::DurableRecord::Compaction { checkpoint, .. } = record {
            Some(checkpoint)
        } else {
            None
        }
    }) {
        if checkpoint.previous_checkpoint_id.as_deref() != applied_checkpoint_id.as_deref() {
            continue;
        }
        let Some(anchor_index) = entry_ids.iter().position(|entry_id| {
            entry_id.as_deref() == Some(checkpoint.first_kept_entry_id.as_str())
        }) else {
            continue;
        };
        let prefix = slim_core::context::compaction_prefix_fingerprint(&history[..anchor_index]);
        if prefix != checkpoint.prefix_fingerprint
            || checkpoint.summary.trim().is_empty()
            || checkpoint.summary.len() > slim_core::session::MAX_COMPACTION_SUMMARY_BYTES
        {
            continue;
        }
        let Some((root_index, root)) = history
            .iter()
            .enumerate()
            .find(|(_, message)| message.role == "user")
        else {
            continue;
        };
        let mut restored = vec![
            root.clone(),
            ProviderMessage::user(format!(
                "[Compacted context]\n{}",
                checkpoint.summary.trim()
            )),
        ];
        let mut restored_ids = vec![entry_ids[root_index].clone(), None];
        restored.extend(history[anchor_index..].iter().cloned());
        restored_ids.extend(entry_ids[anchor_index..].iter().cloned());
        history = restored;
        entry_ids = restored_ids;
        applied_checkpoint_id = Some(checkpoint.checkpoint_id.clone());
    }
    Ok(DurableProviderHistory {
        messages: history,
        parent_entry_id,
        entry_ids,
        applied_checkpoint_id,
    })
}

pub(crate) fn resume_messages_from_preflight(
    preflight: &SessionPreflight,
) -> Result<Vec<ProviderMessage>, ProviderError> {
    durable_provider_history(preflight).map(|history| history.messages)
}

fn redact_secret(input: &str, secret: &str) -> String {
    let redacted = crate::redact(input);
    if secret.is_empty() {
        redacted
    } else {
        redacted.replace(secret, "[REDACTED]")
    }
}

fn resume_error(message: impl Into<String>) -> ProviderError {
    ProviderError::InvalidResponse {
        message: format!("durable resume: {}", message.into()),
    }
}

fn input_required_result(request: &ProviderRequest) -> ProviderHeadlessResult {
    ProviderHeadlessResult {
        code: ExitCode::InputRequired,
        provider: request.kind,
        model: request.model.clone(),
        text: "input_required".into(),
        input_tokens: None,
        output_tokens: None,
        stop_reason: None,
        stop: "input_required".into(),
        cost_micros: None,
        usage_complete: false,
        usage_overflowed: false,
        usage: UsageTotals::default(),
        costs: UsageCostSummary::default(),
        validation_source: None,
        tool_summary_lines: Vec::new(),
    }
}

struct DurableProviderExecutor {
    request: ProviderRequest,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
    event_sender: Option<SessionEventSender>,
    interaction_route: Option<InteractionRoute>,
    execution: Option<ProviderExecution>,
}

impl DurableProviderExecutor {
    fn new(
        request: ProviderRequest,
        options: ProviderRunOptions,
        skill_instructions: Option<SkillInstructions>,
        event_sender: Option<SessionEventSender>,
        interaction_route: Option<InteractionRoute>,
    ) -> Self {
        Self {
            request,
            options,
            skill_instructions,
            event_sender,
            interaction_route,
            execution: None,
        }
    }
}

impl DurableProviderExecutor {
    async fn execute_async(&mut self) -> Result<ProviderResponse, ProviderError> {
        let mut options = self.options.clone();
        if self.interaction_route.is_none() {
            // Headless durable resume has no interactive decision surface and
            // keeps the existing fail-closed tool contract.
            options.max_tool_calls = Some(0);
            options.max_read_tool_calls = Some(0);
        }
        let execution = execute_provider_turn_with_events_and_interaction_async(
            self.request.clone(),
            options,
            self.skill_instructions.clone(),
            self.event_sender.clone(),
            self.interaction_route.clone(),
        )
        .await?;
        let usage = match (
            execution.result.input_tokens,
            execution.result.output_tokens,
        ) {
            (None, None) => None,
            (input_tokens, output_tokens) => Some(slim_core::session::DurableUsage {
                operation_id: String::new(),
                attempt_id: String::new(),
                input_tokens,
                output_tokens,
            }),
        };
        let outcome = match execution.result.code {
            ExitCode::Success => slim_core::session::DurableOutcome::Success,
            ExitCode::Cancelled => slim_core::session::DurableOutcome::Cancelled,
            _ => slim_core::session::DurableOutcome::Failed,
        };
        let response =
            ProviderResponse::with_outcome(execution.result.text.clone(), usage, outcome);
        self.execution = Some(execution);
        Ok(response)
    }
}

impl ManualExecutor for DurableProviderExecutor {
    type Error = ProviderError;

    fn execute(
        &mut self,
        _effect: &slim_core::session::Effect,
    ) -> Result<ProviderResponse, Self::Error> {
        block_on_provider(self.execute_async())
    }

    fn classify_error(&self, error: &Self::Error) -> DurableErrorClass {
        classify_provider_error(error)
    }
}

fn classify_provider_error(error: &ProviderError) -> DurableErrorClass {
    match error {
        ProviderError::Transport { safe_to_retry } => DurableErrorClass::Transport {
            safe_to_retry: *safe_to_retry,
        },
        ProviderError::Remote { .. } => DurableErrorClass::Remote,
        ProviderError::InvalidResponse { .. } | ProviderError::MalformedToolCall => {
            DurableErrorClass::Invalid
        }
        ProviderError::Cancelled => DurableErrorClass::Cancelled,
    }
}

fn run_provider_headless_inner(
    request: ProviderRequest,
    session_path: Option<&Path>,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    execute_provider_turn(request, session_path, options).map(|execution| execution.result)
}

pub(crate) fn execute_provider_turn(
    request: ProviderRequest,
    session_path: Option<&Path>,
    options: ProviderRunOptions,
) -> Result<ProviderExecution, ProviderError> {
    execute_provider_turn_with_skill(request, session_path, options, None)
}

fn execute_provider_turn_with_skill(
    request: ProviderRequest,
    session_path: Option<&Path>,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
) -> Result<ProviderExecution, ProviderError> {
    block_on_provider(execute_provider_turn_with_skill_async(
        request,
        session_path.map(Path::to_path_buf),
        options,
        skill_instructions,
    ))
}

async fn execute_provider_turn_with_skill_async(
    request: ProviderRequest,
    session_path: Option<PathBuf>,
    mut options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
) -> Result<ProviderExecution, ProviderError> {
    let local_code_intelligence = attach_local_code_intelligence(&mut options);
    let result = execute_provider_turn_async(
        request,
        session_path,
        options,
        skill_instructions,
        None,
        None,
    )
    .await;
    if let Some(manager) = local_code_intelligence {
        manager.shutdown().await;
    }
    result
}

async fn execute_provider_turn_with_events_and_interaction_async(
    request: ProviderRequest,
    mut options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
    event_sender: Option<SessionEventSender>,
    interaction_route: Option<InteractionRoute>,
) -> Result<ProviderExecution, ProviderError> {
    let local_code_intelligence = attach_local_code_intelligence(&mut options);
    let result = execute_provider_turn_async(
        request,
        None,
        options,
        skill_instructions,
        event_sender,
        interaction_route,
    )
    .await;
    if let Some(manager) = local_code_intelligence {
        manager.shutdown().await;
    }
    result
}

fn block_on_provider<T>(
    future: impl std::future::Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    shared_provider_runtime()?.block_on(future)
}

/// Process-wide Tokio runtime for synchronous headless entry points.
/// Building one runtime per provider call paid thread-pool spawn on every
/// run; the runtime is thread-safe and `block_on` may be shared.
fn shared_provider_runtime() -> Result<&'static tokio::runtime::Runtime, ProviderError> {
    static PROVIDER_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> =
        std::sync::OnceLock::new();
    if let Some(runtime) = PROVIDER_RUNTIME.get() {
        return Ok(runtime);
    }
    let runtime = tokio::runtime::Runtime::new().map_err(|error| {
        ProviderError::InvalidResponse {
            message: format!("runtime: {error}"),
        }
    })?;
    let _ = PROVIDER_RUNTIME.set(runtime);
    PROVIDER_RUNTIME.get().ok_or_else(|| ProviderError::InvalidResponse {
        message: "runtime: shared provider runtime unavailable".into(),
    })
}

fn attach_local_code_intelligence(
    options: &mut ProviderRunOptions,
) -> Option<CodeIntelligenceHandle> {
    if options.code_intelligence.is_some() {
        return None;
    }
    let manager = crate::config::load_layered()
        .ok()
        .and_then(|layered| crate::code_intel::build_code_intelligence(&layered.lsp))?;
    let handle = CodeIntelligenceHandle::new(manager);
    options.code_intelligence = Some(handle.clone());
    Some(handle)
}

/// Headless-only stderr telemetry. A live TUI projection (`event_sender`)
/// must not print here: the composer holds the cursor, and Windows raw mode
/// paints/injects the line into the input box.
#[cfg(test)]
fn provider_cache_report_line(
    stats: &slim_core::provider::ProviderCacheStats,
    interactive_surface: bool,
) -> Option<String> {
    if interactive_surface || (stats.hits == 0 && stats.misses == 0) {
        return None;
    }
    let total = stats.hits.saturating_add(stats.misses);
    Some(format!(
        "SLIM_PROVIDER_CACHE hits={} misses={} hit_rate={:.2}% evictions={} \
         entries={} retained_bytes={}",
        stats.hits,
        stats.misses,
        stats.hits as f64 * 100.0 / total as f64,
        stats.evictions,
        stats.entries,
        stats.retained_bytes,
    ))
}

pub(crate) async fn execute_provider_turn_async(
    request: ProviderRequest,
    session_path: Option<PathBuf>,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
    event_sender: Option<SessionEventSender>,
    interaction_route: Option<slim_core::InteractionRoute>,
) -> Result<ProviderExecution, ProviderError> {
    if request.prompt.trim().is_empty() {
        return Ok(empty_provider_execution(ProviderHeadlessResult {
            code: ExitCode::InputRequired,
            provider: request.kind,
            model: request.model,
            text: "input_required".into(),
            input_tokens: None,
            output_tokens: None,
            stop_reason: None,
            stop: "input_required".into(),
            cost_micros: None,
            usage_complete: false,
            usage_overflowed: false,
            usage: UsageTotals::default(),
            costs: UsageCostSummary::default(),
            validation_source: None,
            tool_summary_lines: Vec::new(),
        }));
    }
    let skill_user_prefix = skill_instructions
        .as_ref()
        .map(validated_skill_user_prefix)
        .transpose()?;
    if request.mode == OperatingMode::Plan && !options.allow_plan_loop {
        return Ok(empty_provider_execution(ProviderHeadlessResult {
            code: ExitCode::ApprovalRequired,
            provider: request.kind,
            model: request.model,
            text: "approval_required".into(),
            input_tokens: None,
            output_tokens: None,
            stop_reason: None,
            stop: "approval_required".into(),
            cost_micros: None,
            usage_complete: false,
            usage_overflowed: false,
            usage: UsageTotals::default(),
            costs: UsageCostSummary::default(),
            validation_source: None,
            tool_summary_lines: Vec::new(),
        }));
    }

    let open_code_spec = if request.kind == ProviderKind::OpenCodeGo {
        Some(
            open_code_model(&request.model).ok_or_else(|| ProviderError::InvalidResponse {
                message: format!("unsupported OpenCode Go model: {}", request.model),
            })?,
        )
    } else {
        None
    };
    let catalog_override_absent = options.context_window_tokens.is_none()
        && std::env::var_os("SLIM_CONTEXT_WINDOW_TOKENS").is_none();
    let live_codex = if request.kind == ProviderKind::OpenAiCodex
        && catalog_override_absent
        && should_fetch_live_codex_catalog(&request.endpoint)
    {
        match (request.account_id.as_deref(), CodexCatalog::production()) {
            (Some(account_id), Ok(catalog)) => {
                let cached = catalog
                    .load_cached(&request.endpoint, account_id)
                    .ok()
                    .flatten();
                catalog.refresh_in_background(&request.endpoint, &request.api_key, account_id);
                cached.map(|snapshot| snapshot.entries)
            }
            _ => None,
        }
    } else {
        None
    };
    let context_window_tokens = if catalog_override_absent {
        if let Some(model) = open_code_spec {
            model
                .context_window
                .unwrap_or(AgentLoopConfig::default().context_window_tokens)
        } else {
            known_model_context_window(request.kind, &request.model, live_codex.as_deref())
                .unwrap_or(resolve_context_window_tokens(None)?)
        }
    } else {
        resolve_context_window_tokens(options.context_window_tokens)?
    };
    let max_output_tokens =
        resolve_max_output_tokens(options.max_output_tokens, request.kind, &request.model)?;
    let reasoning_effort = options.reasoning_effort.clone();
    if let Some(model) = open_code_spec {
        if model
            .context_window
            .is_some_and(|limit| context_window_tokens > limit)
            || model
                .max_output_tokens
                .is_some_and(|limit| max_output_tokens > limit)
        {
            return Err(ProviderError::InvalidResponse {
                message: "OpenCode Go context or output limit exceeds model metadata".into(),
            });
        }
        if reasoning_effort
            .as_deref()
            .is_some_and(|effort| !model.reasoning_levels.contains(&effort))
        {
            return Err(ProviderError::InvalidResponse {
                message: "unsupported OpenCode Go reasoning effort".into(),
            });
        }
        if options
            .content_blocks
            .iter()
            .any(|block| matches!(block, ProviderContentBlock::Image { .. }))
            && !model.accepts_images
        {
            return Err(ProviderError::InvalidResponse {
                message: format!("OpenCode Go model {} does not accept images", model.id),
            });
        }
    }

    let provider = request.kind;
    let model = request.model.clone();
    let api_key = request.api_key.clone();
    let cancellation = options.cancellation.clone();
    let cwd = options
        .workspace_root
        .clone()
        .unwrap_or(
            std::env::current_dir().map_err(|error| ProviderError::InvalidResponse {
                message: format!("current directory: {error}"),
            })?,
        );
    let mut session_writer = if let Some(path) = session_path.as_ref() {
        let cwd_text = cwd.display().to_string();
        let session_id = format!("slim-{}-{}", std::process::id(), next_session_suffix());
        Some(
            SessionWriter::create(path, &session_id, &cwd_text).map_err(|error| {
                ProviderError::InvalidResponse {
                    message: format!("session: {error}"),
                }
            })?,
        )
    } else {
        None
    };
    let max_mutating_tool_calls = resolve_max_mutating_tool_calls(&options)?;
    let max_read_tool_calls = resolve_max_read_tool_calls(&options)?;
    let max_turns = resolve_max_turns(&options)?;
    let max_result_bytes = resolve_max_result_bytes(options.max_result_bytes)?;
    let artifact_root = options
        .artifact_root
        .unwrap_or_else(|| cwd.join(".slim").join("artifacts"));
    let mut runtime = Runtime::with_artifact_store(&artifact_root).map_err(|error| {
        ProviderError::InvalidResponse {
            message: format!("artifact store: {error}"),
        }
    })?;
    if let Some(tools) = options.tool_registry.as_ref() {
        runtime.set_tool_registry(tools.registry());
    }
    if let Some(sender) = event_sender {
        runtime.app.set_event_sender(sender);
    }
    if let Some(cancellation) = cancellation {
        runtime.set_cancellation_token(cancellation);
    }
    if let Some(interaction_route) = interaction_route {
        runtime.set_interaction_route(interaction_route);
    }
    if let Some(handle) = options.compaction.clone() {
        runtime.set_compaction_handle(handle);
        // Background summaries are break-even-gated by the loop and still
        // honor `[compaction] background = false`; headless runs overlap
        // them like interactive ones instead of stalling foreground at the
        // hard threshold.
        runtime.set_background_compaction_enabled(true);
    }
    runtime.register_sensitive_value(&request.api_key);
    // Per-turn Runtime, application-scoped language-server pool.
    if let Some(code_intelligence) = options.code_intelligence.as_ref() {
        runtime.set_code_intelligence(code_intelligence.manager().clone());
        code_intelligence.warm_workspace(cwd.clone());
    }
    let provider_timeouts = ProviderTimeouts::production(request.timeout);
    let mut loop_config = AgentLoopConfig {
        context_window_tokens,
        context_reserve_tokens: max_output_tokens as u64,
        ..AgentLoopConfig::default()
    };
    loop_config.max_turns = max_turns;
    loop_config.max_mutating_tool_calls = max_mutating_tool_calls;
    loop_config.max_read_tool_calls = max_read_tool_calls;
    loop_config.max_result_bytes = max_result_bytes;
    let tool_limits = ToolLoopLimits {
        max_mutating_tool_calls: loop_config.max_mutating_tool_calls,
        max_read_tool_calls: loop_config.max_read_tool_calls,
        max_turns: loop_config.max_turns,
        max_output_tokens,
    };
    let user_text = match skill_user_prefix.as_deref() {
        Some(prefix) => format!("{prefix}{}", request.prompt),
        None => request.prompt.clone(),
    };
    let initial_message =
        ProviderMessage::user(user_text).with_content_blocks(options.content_blocks);
    let mut initial_messages = options.history.clone();
    initial_messages.push(initial_message);
    let loop_result = match provider {
        ProviderKind::OpenAiCompatible => {
            let mut config =
                ProviderConfig::openai(request.endpoint, request.model, api_key.clone())
                    .with_max_output_tokens(max_output_tokens);
            if let Some(effort) = reasoning_effort.as_deref() {
                config = config.with_reasoning_effort(effort);
            }
            let adapter = OpenAiCompatibleAdapter::new(config)?;
            let client = HttpProviderClient::with_shared_transport(adapter, provider_timeouts)?;
            runtime
                .run_agent_loop_with_messages(
                    &client,
                    &initial_messages,
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
        ProviderKind::OpenAiCodex => {
            let account_id =
                request
                    .account_id
                    .clone()
                    .ok_or_else(|| ProviderError::InvalidResponse {
                        message: "Codex OAuth account id is required".into(),
                    })?;
            let mut config = ProviderConfig::openai_codex(
                request.endpoint,
                request.model,
                api_key.clone(),
                account_id,
            )
            .with_max_output_tokens(max_output_tokens);
            if let Some(effort) = reasoning_effort.as_deref() {
                config = config.with_reasoning_effort(effort);
            }
            let adapter = OpenAiCodexAdapter::new(config)?;
            let client = HttpProviderClient::with_shared_transport(adapter, provider_timeouts)?;
            runtime
                .run_agent_loop_with_messages(
                    &client,
                    &initial_messages,
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
        ProviderKind::Anthropic => {
            let config = if request.account_id.is_some() {
                ProviderConfig::anthropic_oauth(request.endpoint, request.model, api_key.clone())
            } else {
                ProviderConfig::anthropic(request.endpoint, request.model, api_key.clone())
            };
            let adapter = AnthropicAdapter::new(config.with_max_output_tokens(max_output_tokens))?;
            let client = HttpProviderClient::with_shared_transport(adapter, provider_timeouts)?;
            runtime
                .run_agent_loop_with_messages(
                    &client,
                    &initial_messages,
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
        ProviderKind::OpenCodeGo => {
            let adapter = OpenCodeGoAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?
            .with_max_output_tokens(max_output_tokens);
            let client = HttpProviderClient::with_shared_transport(adapter, provider_timeouts)?;
            runtime
                .run_agent_loop_with_messages(
                    &client,
                    &initial_messages,
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
        ProviderKind::ClinePass => {
            let adapter = ClinePassAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?;
            let client = HttpProviderClient::with_shared_transport(adapter, provider_timeouts)?;
            runtime
                .run_agent_loop_with_messages(
                    &client,
                    &initial_messages,
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
        ProviderKind::CommandCode => {
            let adapter = CommandCodeAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?;
            let client = HttpProviderClient::with_shared_transport(adapter, provider_timeouts)?;
            runtime
                .run_agent_loop_with_messages(
                    &client,
                    &initial_messages,
                    request.mode,
                    &cwd,
                    1,
                    loop_config,
                )
                .await
        }
    }
    .map_err(|error| redact_provider_error(error, &api_key));

    let events = runtime.app.drain_events();
    if let Some(writer) = session_writer.as_mut() {
        writer
            .append_batch(&events)
            .map_err(|error| ProviderError::InvalidResponse {
                message: format!("session: {error}"),
            })?;
    }

    let mut text = String::new();
    let mut tool_text = Vec::new();
    let mut stop_reason = None;
    for event in &events {
        match &event.kind {
            EventKind::AssistantTextDelta { text: delta } => text.push_str(delta),
            EventKind::ToolOutput { name, output, .. } => {
                tool_text.push(format!("tool {name}: {output}"))
            }
            EventKind::AssistantEnded { reason } => stop_reason = Some(reason.clone()),
            _ => {}
        }
    }
    if text.is_empty() {
        text = tool_text.join("\n");
    }
    let (validated_completion, stop, code, tool_results) = match loop_result {
        Ok(loop_result) => (
            derive_validated_completion(loop_result.stop, &events),
            stop_name(loop_result.stop).to_owned(),
            exit_code_for_stop(loop_result.stop),
            loop_result.tool_results,
        ),
        Err(error) => {
            let (code, stop, message) = provider_failure_details(error);
            text = message;
            (false, stop.to_owned(), code, Vec::new())
        }
    };
    let usage = UsageTotals::from_events(&events, validated_completion);
    let (input_tokens, output_tokens) = legacy_provider_usage(&usage);
    let costs = cost_summary_for_usage(&usage, resolve_pricing());
    let cost_micros = costs.total_micros;
    text = runtime.redact_sensitive(&text);
    let history = runtime.conversation().to_vec();
    let tool_summary_lines = summarize_tool_events(&events);
    Ok(ProviderExecution {
        result: ProviderHeadlessResult {
            code,
            provider,
            model,
            text,
            input_tokens,
            output_tokens,
            stop_reason,
            stop,
            cost_micros,
            usage_complete: !usage.requests.is_empty() && !usage.usage_unknown && !usage.overflowed,
            usage_overflowed: usage.overflowed,
            usage,
            costs,
            validation_source: validated_completion.then(|| "derived_runtime".into()),
            tool_summary_lines,
        },
        history: Some(history),
        events,
        tool_results,
        max_mutating_tool_calls: tool_limits.max_mutating_tool_calls,
        max_read_tool_calls: tool_limits.max_read_tool_calls,
        max_turns: tool_limits.max_turns,
        max_output_tokens: tool_limits.max_output_tokens,
        resume_preflight: None,
    })
}

fn skill_user_message_prefix(skill: &SkillInstructions) -> String {
    format!(
        "[Skill: {}]\nSkill file: {}\nResolve relative paths in these instructions from the skill file's directory.\nFollow these skill instructions for this request only:\n\n{}\n\n",
        skill.name,
        skill.source.display(),
        skill.body.trim()
    )
}

fn validated_skill_user_prefix(skill: &SkillInstructions) -> Result<String, ProviderError> {
    if skill.body.len() > MAX_SLASH_SKILL_BODY_BYTES {
        return Err(ProviderError::InvalidResponse {
            message: format!(
                "skill instructions exceed the {}-byte slash limit",
                MAX_SLASH_SKILL_BODY_BYTES
            ),
        });
    }
    let prefix = skill_user_message_prefix(skill);
    if prefix.len() > MAX_SLASH_SKILL_SYSTEM_PROMPT_BYTES {
        return Err(ProviderError::InvalidResponse {
            message: format!(
                "skill user prefix exceeds the {}-byte context-safe limit",
                MAX_SLASH_SKILL_SYSTEM_PROMPT_BYTES
            ),
        });
    }
    Ok(prefix)
}

#[cfg(test)]
mod skill_prompt_tests {
    use slim_core::provider::ProviderError;

    use super::{
        skill_user_message_prefix, validated_skill_user_prefix, SkillInstructions,
        MAX_SLASH_SKILL_BODY_BYTES,
    };

    #[test]
    fn invoked_skill_prefix_is_suitable_for_the_first_user_message() {
        let skill = SkillInstructions {
            name: "review-code".into(),
            body: "Inspect the change carefully.".into(),
            source: "D:/Slim/.slim/skills/review-code/SKILL.md".into(),
        };
        let prefix = skill_user_message_prefix(&skill);
        let user_text = format!("{prefix}Fix this PR.");

        assert!(prefix.starts_with("[Skill: review-code]"));
        assert!(prefix.contains("D:/Slim/.slim/skills/review-code/SKILL.md"));
        assert!(prefix.contains(&skill.body));
        assert!(user_text.ends_with("Fix this PR."));
    }

    #[test]
    fn invoked_skill_rejects_a_user_prefix_above_the_context_safe_limit() {
        let skill = SkillInstructions {
            name: "review-code".into(),
            body: "x".repeat(MAX_SLASH_SKILL_BODY_BYTES),
            source: "p".repeat(3_000).into(),
        };

        let error = validated_skill_user_prefix(&skill).expect_err("oversized prefix");

        assert!(matches!(
            error,
            ProviderError::InvalidResponse { message }
                if message.contains("context-safe limit")
        ));
    }
}

fn derive_validated_completion(stop: AgentLoopStop, events: &[SessionEvent]) -> bool {
    if stop != AgentLoopStop::ProviderCompleted
        || events
            .iter()
            .any(|event| matches!(event.kind, EventKind::TerminalError { .. }))
    {
        return false;
    }

    let last_mutation =
        events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match &event.kind {
                EventKind::CausalProgressObserved {
                    kind: slim_core::CausalProgressKind::WorkspaceChanged,
                    workspace_revision,
                    ..
                } => Some((index, *workspace_revision)),
                _ => None,
            });
    let last_assurance =
        events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match event.kind {
                EventKind::GoalAssurance { verified } => Some((index, verified)),
                _ => None,
            });
    let Some((assurance_index, true)) = last_assurance else {
        return false;
    };
    let Some((last_mutation_index, last_mutation_revision)) = last_mutation else {
        return true;
    };
    if assurance_index <= last_mutation_index {
        return false;
    }

    events.iter().skip(last_mutation_index + 1).any(|event| {
        matches!(
            &event.kind,
            EventKind::CausalProgressObserved {
                kind: slim_core::CausalProgressKind::ValidationGreen,
                workspace_revision,
                ..
            } if *workspace_revision >= last_mutation_revision
        )
    })
}

fn legacy_provider_usage(usage: &UsageTotals) -> (Option<u64>, Option<u64>) {
    let provider_requests = usage
        .requests
        .iter()
        .filter(|request| request.request_kind == RequestKind::ProviderTurn)
        .collect::<Vec<_>>();
    if provider_requests.is_empty()
        || provider_requests
            .iter()
            .any(|request| request.usage_unknown)
    {
        return (None, None);
    }
    let input = provider_requests.iter().try_fold(0_u64, |total, request| {
        total.checked_add(request.total_input_tokens())
    });
    let output = provider_requests.iter().try_fold(0_u64, |total, request| {
        total.checked_add(request.output_tokens)
    });
    (input, output)
}

fn summarize_tool_events(events: &[SessionEvent]) -> Vec<String> {
    struct FinishedTool {
        batch_id: String,
        name: String,
        success: bool,
        duration_ms: u64,
        reason: String,
    }

    // Last output/preview per call, so `✕` rows carry the same short reason
    // the TUI projects for a failed tool. Success rows never carry output
    // text (spec §15.1); empty call ids are never correlated.
    let mut reasons = std::collections::HashMap::<&str, &str>::new();
    for event in events {
        match &event.kind {
            EventKind::ToolOutput {
                call_id, output, ..
            } if !call_id.is_empty() => {
                reasons.insert(call_id.as_str(), output.as_str());
            }
            EventKind::ToolProgress {
                call_id, preview, ..
            } if !call_id.is_empty() => {
                reasons.insert(call_id.as_str(), preview.as_str());
            }
            _ => {}
        }
    }

    let tools = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolFinished {
                batch_id,
                call_id,
                name,
                success,
                duration_ms,
                ..
            } => {
                let reason = if *success || call_id.is_empty() {
                    String::new()
                } else {
                    reasons
                        .get(call_id.as_str())
                        .map(|text| short_failure_reason(text))
                        .unwrap_or_default()
                };
                Some(FinishedTool {
                    batch_id: batch_id.clone(),
                    name: sanitize_timeline_name(name),
                    success: *success,
                    duration_ms: *duration_ms,
                    reason,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut index = 0usize;
    while index < tools.len() {
        let tool = &tools[index];
        if tool.success && !tool.batch_id.is_empty() {
            let mut end = index + 1;
            while tools
                .get(end)
                .is_some_and(|candidate| candidate.success && candidate.batch_id == tool.batch_id)
            {
                end += 1;
            }
            if end - index > 1 {
                let members = &tools[index..end];
                let duration_ms = members.iter().fold(0u64, |total, member| {
                    total.saturating_add(member.duration_ms)
                });
                lines.push(format!(
                    "✓ {} tools · {} · {duration_ms}ms",
                    members.len(),
                    summarize_timeline_names(members.iter().map(|member| member.name.as_str()))
                ));
                index = end;
                continue;
            }
        }
        let status = if tool.success { "✓" } else { "✕" };
        let failure = if tool.success { "" } else { " · failed" };
        let reason = if tool.reason.is_empty() {
            String::new()
        } else {
            format!(" · {}", tool.reason)
        };
        lines.push(format!(
            "{status} {}{failure}{reason} · {}ms",
            tool.name, tool.duration_ms
        ));
        index += 1;
    }
    lines
}

/// First redacted line of a tool output/preview for `✕` timeline rows.
/// Mirrors the TUI failed-tool short reason: success rows never call this.
fn short_failure_reason(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn sanitize_timeline_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .filter(|character| !character.is_control())
        .take(64)
        .collect::<String>();
    if sanitized.is_empty() {
        "tool".into()
    } else {
        sanitized
    }
}

fn summarize_timeline_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for name in names {
        if let Some((_, count)) = counts.iter_mut().find(|(seen, _)| *seen == name) {
            *count = count.saturating_add(1);
        } else {
            counts.push((name, 1));
        }
    }
    let hidden = counts.len().saturating_sub(3);
    let mut summary = counts
        .iter()
        .take(3)
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name} ×{count}")
            } else {
                (*name).to_owned()
            }
        })
        .collect::<Vec<_>>();
    if hidden > 0 {
        summary.push(format!("+{hidden}"));
    }
    summary.join(", ")
}

#[cfg(test)]
mod durable_checkpoint_tests {
    use slim_core::ProviderMessage;

    use super::durable_checkpoint_anchor;

    #[test]
    fn checkpoint_anchor_requires_an_exact_durable_prefix() {
        let messages = vec![
            ProviderMessage::user("root"),
            ProviderMessage::assistant("answer", Vec::new()),
            ProviderMessage::user("next"),
        ];
        let entry_ids = vec![Some("u1".into()), Some("a1".into()), Some("u2".into())];
        let fingerprint = slim_core::context::compaction_prefix_fingerprint(&messages[..2]);

        assert_eq!(
            durable_checkpoint_anchor(&messages, &entry_ids, 2, &fingerprint).as_deref(),
            Some("u2")
        );
        let compacted_ids = [Some("u1".into()), None, Some("u2".into())];
        assert_eq!(
            durable_checkpoint_anchor(&messages, &compacted_ids, 2, &fingerprint).as_deref(),
            Some("u2")
        );
        assert!(durable_checkpoint_anchor(
            &messages,
            &compacted_ids,
            1,
            &slim_core::context::compaction_prefix_fingerprint(&messages[..1]),
        )
        .is_none());
        assert!(
            durable_checkpoint_anchor(&messages, &entry_ids, messages.len(), &fingerprint,)
                .is_none()
        );
    }
}

#[cfg(test)]
mod tool_timeline_tests {
    use super::summarize_tool_events;
    use slim_core::{EventKind, SessionEvent};

    fn finished(
        seq: u64,
        batch_id: &str,
        name: &str,
        success: bool,
        duration_ms: u64,
    ) -> SessionEvent {
        SessionEvent::new(
            seq,
            EventKind::ToolFinished {
                batch_id: batch_id.into(),
                call_id: format!("call-{seq}"),
                name: name.into(),
                success,
                duration_ms,
            },
        )
    }

    #[test]
    fn groups_only_consecutive_successful_members_of_one_batch() {
        let events = vec![
            finished(1, "a", "read", true, 7),
            finished(2, "a", "read", true, 8),
            finished(3, "a", "shell", true, 9),
            finished(4, "a", "write", false, 4),
            finished(5, "a", "read", true, 5),
            finished(6, "b", "search", true, 6),
        ];
        assert_eq!(
            summarize_tool_events(&events),
            [
                "✓ 3 tools · read ×2, shell · 24ms",
                "✕ write · failed · 4ms",
                "✓ read · 5ms",
                "✓ search · 6ms",
            ]
        );
    }

    #[test]
    fn timeline_never_includes_arguments_or_output_events() {
        let events = vec![
            SessionEvent::new(
                1,
                EventKind::ToolStarted {
                    batch_id: "a".into(),
                    call_id: "call-1".into(),
                    name: "read".into(),
                    arguments: "secret-path".into(),
                },
            ),
            SessionEvent::new(
                2,
                EventKind::ToolOutput {
                    batch_id: "a".into(),
                    call_id: "call-1".into(),
                    name: "read".into(),
                    output: "secret-output".into(),
                },
            ),
            finished(3, "a", "read", true, 2),
        ];
        let timeline = summarize_tool_events(&events).join("\n");
        assert_eq!(timeline, "✓ read · 2ms");
        assert!(!timeline.contains("secret"));
    }

    #[test]
    fn failed_tool_line_carries_first_output_line_as_reason() {
        let events = vec![
            SessionEvent::new(
                1,
                EventKind::ToolOutput {
                    batch_id: "a".into(),
                    call_id: "call-9".into(),
                    name: "shell".into(),
                    output: "exit 1: file not found\nmore details here".into(),
                },
            ),
            SessionEvent::new(
                2,
                EventKind::ToolFinished {
                    batch_id: "a".into(),
                    call_id: "call-9".into(),
                    name: "shell".into(),
                    success: false,
                    duration_ms: 3,
                },
            ),
        ];
        assert_eq!(
            summarize_tool_events(&events),
            ["✕ shell · failed · exit 1: file not found · 3ms"]
        );
    }
}

#[cfg(test)]
mod validation_derivation_tests {
    use super::derive_validated_completion;
    use slim_core::{runtime::AgentLoopStop, CausalProgressKind, EventKind, SessionEvent};

    fn progress(seq: u64, kind: CausalProgressKind) -> SessionEvent {
        SessionEvent::new(
            seq,
            EventKind::CausalProgressObserved {
                batch_id: "batch".into(),
                call_id: format!("call-{seq}").into(),
                kind,
                tool_name: "shell".into(),
                call_fingerprint: "fingerprint".into(),
                evidence_id: "evidence".into(),
                workspace_revision: seq,
            },
        )
    }

    #[test]
    fn validation_must_follow_the_last_workspace_mutation() {
        let mut events = vec![
            progress(1, CausalProgressKind::WorkspaceChanged),
            progress(2, CausalProgressKind::ValidationGreen),
        ];
        assert!(!derive_validated_completion(
            AgentLoopStop::ProviderCompleted,
            &events
        ));

        events.push(SessionEvent::new(
            3,
            EventKind::GoalAssurance { verified: true },
        ));
        assert!(derive_validated_completion(
            AgentLoopStop::ProviderCompleted,
            &events
        ));

        events.push(progress(4, CausalProgressKind::WorkspaceChanged));
        assert!(!derive_validated_completion(
            AgentLoopStop::ProviderCompleted,
            &events
        ));
    }
}

#[derive(Clone, Copy)]
struct UsagePricing {
    provider: ProviderPricing,
    cache_write_micros_per_million: Option<u64>,
    cache_read_micros_per_million: Option<u64>,
}

fn cost_summary_for_usage(usage: &UsageTotals, pricing: Option<UsagePricing>) -> UsageCostSummary {
    let Some(pricing) = pricing else {
        return UsageCostSummary::default();
    };
    let total_micros = (!usage.overflowed
        && !usage.requests.is_empty()
        && usage.requests.iter().all(|request| !request.usage_unknown))
    .then(|| sum_request_costs(usage.requests.iter(), pricing))
    .flatten();
    let failed_attempts_micros = (!usage.overflowed)
        .then(|| {
            sum_request_costs(
                usage
                    .requests
                    .iter()
                    .filter(|request| request.failed && !request.cancelled),
                pricing,
            )
        })
        .flatten();
    let compaction_micros = (!usage.overflowed)
        .then(|| {
            sum_request_costs(
                usage
                    .requests
                    .iter()
                    .filter(|request| request.request_kind == RequestKind::Compaction),
                pricing,
            )
        })
        .flatten();
    let cancelled_estimated_micros = (!usage.overflowed)
        .then(|| {
            sum_cancelled_estimated_costs(
                usage.requests.iter().filter(|request| request.cancelled),
                pricing,
            )
        })
        .flatten();
    UsageCostSummary {
        total_micros,
        cost_per_validated_completion_micros: usage
            .validated_completion
            .then_some(total_micros)
            .flatten(),
        failed_attempts_micros,
        compaction_micros,
        cancelled_estimated_micros,
    }
}

fn sum_request_costs<'a>(
    mut requests: impl Iterator<Item = &'a slim_core::RequestUsage>,
    pricing: UsagePricing,
) -> Option<u64> {
    let weighted = requests.try_fold(0_u128, |total, request| {
        if request.usage_unknown {
            return None;
        }
        request_weighted_cost(request, pricing).and_then(|cost| total.checked_add(cost))
    })?;
    weighted_cost_micros(weighted)
}

fn sum_cancelled_estimated_costs<'a>(
    mut requests: impl Iterator<Item = &'a slim_core::RequestUsage>,
    pricing: UsagePricing,
) -> Option<u64> {
    let weighted = requests.try_fold(0_u128, |total, request| {
        let observed = request_weighted_cost(request, pricing)?;
        let unknown_input_tokens = request
            .estimated_input_tokens
            .saturating_sub(request.total_input_tokens());
        let estimated = u128::from(unknown_input_tokens)
            .checked_mul(u128::from(pricing.provider.input_micros_per_million))?;
        observed
            .checked_add(estimated)
            .and_then(|cost| total.checked_add(cost))
    })?;
    weighted_cost_micros(weighted)
}

fn request_weighted_cost(request: &slim_core::RequestUsage, pricing: UsagePricing) -> Option<u128> {
    let cache_write_rate = if request.cache_write_tokens == 0 {
        0
    } else {
        pricing.cache_write_micros_per_million?
    };
    let cache_read_rate = if request.cache_read_tokens == 0 {
        0
    } else {
        pricing.cache_read_micros_per_million?
    };
    u128::from(request.uncached_input_tokens)
        .checked_mul(u128::from(pricing.provider.input_micros_per_million))?
        .checked_add(
            u128::from(request.cache_write_tokens).checked_mul(u128::from(cache_write_rate))?,
        )?
        .checked_add(
            u128::from(request.cache_read_tokens).checked_mul(u128::from(cache_read_rate))?,
        )?
        .checked_add(
            u128::from(request.output_tokens)
                .checked_mul(u128::from(pricing.provider.output_micros_per_million))?,
        )
}

fn weighted_cost_micros(weighted: u128) -> Option<u64> {
    weighted.checked_div(1_000_000)?.try_into().ok()
}

#[cfg(test)]
mod usage_cost_tests {
    use super::{cost_summary_for_usage, UsagePricing};
    use slim_core::{ProviderPricing, RequestKind, RequestUsage, UsageTotals};

    #[test]
    fn costs_separate_failed_compaction_cancelled_and_validated_work() {
        let pricing = UsagePricing {
            provider: ProviderPricing {
                input_micros_per_million: 1_000_000,
                output_micros_per_million: 2_000_000,
            },
            cache_write_micros_per_million: Some(3_000_000),
            cache_read_micros_per_million: Some(500_000),
        };
        let usage = UsageTotals {
            validated_completion: true,
            requests: vec![
                RequestUsage {
                    request_kind: RequestKind::ProviderTurn,
                    uncached_input_tokens: 10,
                    cache_write_tokens: 2,
                    cache_read_tokens: 4,
                    output_tokens: 3,
                    ..RequestUsage::default()
                },
                RequestUsage {
                    request_kind: RequestKind::ProviderTurn,
                    uncached_input_tokens: 5,
                    output_tokens: 1,
                    failed: true,
                    ..RequestUsage::default()
                },
                RequestUsage {
                    request_kind: RequestKind::Compaction,
                    uncached_input_tokens: 4,
                    output_tokens: 2,
                    ..RequestUsage::default()
                },
            ],
            ..UsageTotals::default()
        };

        let costs = cost_summary_for_usage(&usage, Some(pricing));
        assert_eq!(costs.total_micros, Some(39));
        assert_eq!(costs.cost_per_validated_completion_micros, Some(39));
        assert_eq!(costs.failed_attempts_micros, Some(7));
        assert_eq!(costs.compaction_micros, Some(8));

        let cancelled = UsageTotals {
            requests: vec![RequestUsage {
                request_kind: RequestKind::ProviderTurn,
                usage_unknown: true,
                cancelled: true,
                estimated_input_tokens: 6,
                ..RequestUsage::default()
            }],
            usage_unknown: true,
            ..UsageTotals::default()
        };
        let costs = cost_summary_for_usage(&cancelled, Some(pricing));
        assert_eq!(costs.total_micros, None);
        assert_eq!(costs.cancelled_estimated_micros, Some(6));

        let overflowed = UsageTotals {
            requests: usage.requests,
            overflowed: true,
            ..UsageTotals::default()
        };
        let costs = cost_summary_for_usage(&overflowed, Some(pricing));
        assert_eq!(costs.total_micros, None);
        assert_eq!(costs.failed_attempts_micros, None);
        assert_eq!(costs.compaction_micros, None);
    }
    #[test]
    fn request_costs_round_after_aggregation() {
        let pricing = UsagePricing {
            provider: ProviderPricing {
                input_micros_per_million: 1_500_000,
                output_micros_per_million: 0,
            },
            cache_write_micros_per_million: Some(0),
            cache_read_micros_per_million: Some(0),
        };
        let usage = UsageTotals {
            requests: vec![
                RequestUsage {
                    uncached_input_tokens: 1,
                    ..RequestUsage::default()
                },
                RequestUsage {
                    uncached_input_tokens: 1,
                    ..RequestUsage::default()
                },
            ],
            ..UsageTotals::default()
        };

        let costs = cost_summary_for_usage(&usage, Some(pricing));

        assert_eq!(costs.total_micros, Some(3));
    }

    #[test]
    fn cancelled_cost_combines_observed_usage_with_unknown_input_remainder() {
        let pricing = UsagePricing {
            provider: ProviderPricing {
                input_micros_per_million: 1_000_000,
                output_micros_per_million: 2_000_000,
            },
            cache_write_micros_per_million: Some(3_000_000),
            cache_read_micros_per_million: Some(500_000),
        };
        let usage = UsageTotals {
            requests: vec![RequestUsage {
                uncached_input_tokens: 2,
                cache_write_tokens: 1,
                cache_read_tokens: 3,
                output_tokens: 4,
                estimated_input_tokens: 10,
                usage_unknown: true,
                cancelled: true,
                ..RequestUsage::default()
            }],
            usage_unknown: true,
            ..UsageTotals::default()
        };

        let costs = cost_summary_for_usage(&usage, Some(pricing));

        assert_eq!(costs.cancelled_estimated_micros, Some(18));
    }
}

fn next_session_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

pub fn render_text(result: &HeadlessResult) -> String {
    format!("{}\n", result.message)
}

pub fn render_jsonl(result: &HeadlessResult) -> Result<String, serde_json::Error> {
    let line = serde_json::to_string(&JsonlResult {
        version: 1,
        kind: &result.message,
    })?;
    Ok(format!("{line}\n"))
}

#[derive(Serialize)]
struct ProviderJsonlResult<'a> {
    version: u8,
    kind: &'a str,
    provider: &'a str,
    model: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<&'a str>,
    stop: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_micros: Option<u64>,
    usage_complete: bool,
    usage_overflowed: bool,
    usage_unknown: bool,
    usage: ProviderUsageJson<'a>,
    costs: &'a UsageCostSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_hit_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation_source: Option<&'a str>,
}

#[derive(Serialize)]
struct ProviderUsageJson<'a> {
    #[serde(flatten)]
    totals: &'a UsageTotals,
    compaction_tokens_saved_estimated: bool,
}

pub fn render_provider_text(result: &ProviderHeadlessResult) -> String {
    format!("{}\n", result.text)
}

fn provider_telemetry(result: &ProviderHeadlessResult) -> String {
    let input = result
        .input_tokens
        .map_or_else(|| "?".into(), |tokens| tokens.to_string());
    let output = result
        .output_tokens
        .map_or_else(|| "?".into(), |tokens| tokens.to_string());
    let cache_hit_ratio = cache_hit_ratio(&result.usage)
        .map_or_else(|| "?".into(), |ratio| format!("{:.2}%", ratio * 100.0));
    let estimation_error = if result.usage.usage_unknown || result.usage.overflowed {
        "?".into()
    } else {
        result.usage.estimation_error_tokens.to_string()
    };
    let cost = |value: Option<u64>| value.map_or_else(|| "?".into(), |value| value.to_string());
    format!(
        concat!(
            "stop={} validation={}\n",
            "usage_complete={} usage_unknown={} usage_overflowed={} input_tokens={input} output_tokens={output}\n",
            "ledger uncached_input={} cache_write={} cache_read={} reasoning={} cache_hit_ratio={cache_hit_ratio}\n",
            "execution provider_turns={} tool_calls_executed={} tool_calls_reused={} tool_calls_suppressed={} no_progress_turns={} no_progress_tokens={}\n",
            "economy duplicate_evidence_bytes_avoided={} compaction_input={} compaction_output={} compaction_saved_estimated={} post_compaction_reacquisitions={} estimation_error={}\n",
            "cost total={} per_validated_completion={} failed_attempts={} compaction={} cancelled_estimated={}\n"
        ),
        result.stop,
        result.validation_source.as_deref().unwrap_or("unvalidated"),
        result.usage_complete,
        result.usage.usage_unknown,
        result.usage_overflowed,
        result.usage.uncached_input_tokens,
        result.usage.cache_write_tokens,
        result.usage.cache_read_tokens,
        result.usage.reasoning_tokens,
        result.usage.provider_turns,
        result.usage.tool_calls_executed,
        result.usage.tool_calls_reused,
        result.usage.tool_calls_suppressed,
        result.usage.no_progress_turns,
        result.usage.no_progress_tokens,
        result.usage.duplicate_evidence_bytes_avoided,
        result.usage.compaction_input_tokens,
        result.usage.compaction_output_tokens,
        result.usage.compaction_tokens_saved,
        result.usage.post_compaction_reacquisitions,
        estimation_error,
        cost(result.costs.total_micros),
        cost(result.costs.cost_per_validated_completion_micros),
        cost(result.costs.failed_attempts_micros),
        cost(result.costs.compaction_micros),
        cost(result.costs.cancelled_estimated_micros),
        input = input,
        output = output,
        cache_hit_ratio = cache_hit_ratio,
    )
}

fn cache_hit_ratio(usage: &UsageTotals) -> Option<f64> {
    if usage.usage_unknown || usage.overflowed {
        return None;
    }
    let total = usage.total_input_tokens();
    (total > 0).then(|| usage.cache_read_tokens as f64 / total as f64)
}

pub fn render_provider_verbose_text(result: &ProviderHeadlessResult) -> String {
    let mut output = render_provider_text(result);
    output.push('\n');
    if !result.tool_summary_lines.is_empty() {
        output.push_str("timeline:\n");
        for line in &result.tool_summary_lines {
            output.push_str(line);
            output.push('\n');
        }
        output.push('\n');
    }
    output.push_str(&provider_telemetry(result));
    output
}

pub fn render_provider_jsonl(result: &ProviderHeadlessResult) -> Result<String, serde_json::Error> {
    let provider = match result.provider {
        ProviderKind::OpenAiCompatible => "openai-compatible",
        ProviderKind::OpenAiCodex => "openai-codex",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenCodeGo => "opencode-go",
        ProviderKind::ClinePass => "clinepass",
        ProviderKind::CommandCode => "command-code",
    };
    let kind = match result.code {
        ExitCode::Success => "assistant",
        ExitCode::ApprovalRequired => "approval_required",
        ExitCode::InputRequired => "input_required",
        _ => "provider_result",
    };
    let line = serde_json::to_string(&ProviderJsonlResult {
        version: 2,
        kind,
        provider,
        model: &result.model,
        text: &result.text,
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
        stop_reason: result.stop_reason.as_deref(),
        stop: &result.stop,
        cost_micros: result.cost_micros,
        usage_complete: result.usage_complete,
        usage_overflowed: result.usage_overflowed,
        usage_unknown: result.usage.usage_unknown,
        usage: ProviderUsageJson {
            totals: &result.usage,
            compaction_tokens_saved_estimated: true,
        },
        costs: &result.costs,
        cache_hit_ratio: cache_hit_ratio(&result.usage),
        validation_source: result.validation_source.as_deref(),
    })?;
    Ok(format!("{line}\n"))
}

pub fn load_local_images(paths: &[String]) -> Result<Vec<ProviderContentBlock>, String> {
    paths
        .iter()
        .map(|path| load_local_image(Path::new(path)))
        .collect()
}

fn load_local_image(path: &Path) -> Result<ProviderContentBlock, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("image cannot be read: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("image must be a regular non-symlink file".into());
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image exceeds the {} MiB limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let media_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Err("image extension must be png, jpg, jpeg, gif, or webp".into()),
    };
    let file =
        std::fs::File::open(path).map_err(|error| format!("image cannot be read: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("image cannot be read: {error}"))?;
    if bytes.is_empty() {
        return Err("image cannot be empty".into());
    }
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!(
            "image exceeds the {} MiB limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok(ProviderContentBlock::image(
        media_type,
        encode_base64(&bytes),
    ))
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn resolve_context_window_tokens(explicit: Option<u64>) -> Result<u64, ProviderError> {
    if let Some(value) = explicit {
        return (value > 0)
            .then_some(value)
            .ok_or_else(|| ProviderError::InvalidResponse {
                message: "context window tokens must be positive".into(),
            });
    }
    parse_positive_env_u64("SLIM_CONTEXT_WINDOW_TOKENS").map_or_else(
        |message| Err(ProviderError::InvalidResponse { message }),
        |value| Ok(value.unwrap_or(AgentLoopConfig::default().context_window_tokens)),
    )
}

fn known_model_context_window(
    kind: ProviderKind,
    model: &str,
    live_codex: Option<&[slim_core::provider::CodexCatalogEntry]>,
) -> Option<u64> {
    match kind {
        ProviderKind::OpenAiCodex => Some(resolve_codex_context_window(model, live_codex)),
        ProviderKind::ClinePass => clinepass_model(model).map(|model| model.context_window),
        ProviderKind::CommandCode => command_code_model(model).map(|model| model.context_window),
        ProviderKind::Anthropic => command_code_model(model).map(|model| model.context_window),
        ProviderKind::OpenAiCompatible | ProviderKind::OpenCodeGo => None,
    }
}

pub(crate) fn resolve_max_output_tokens(
    explicit: Option<u32>,
    kind: ProviderKind,
    model: &str,
) -> Result<u32, ProviderError> {
    let (value, is_explicit) = if let Some(value) = explicit {
        if value == 0 {
            return Err(ProviderError::InvalidResponse {
                message: "max output tokens must be positive".into(),
            });
        }
        (value as u64, true)
    } else {
        let value = parse_positive_env_u64("SLIM_MAX_OUTPUT_TOKENS")
            .map_err(|message| ProviderError::InvalidResponse { message })?;
        match value {
            Some(value) => (value, true),
            None => return Ok(DEFAULT_MAX_OUTPUT_TOKENS),
        }
    };

    if is_explicit
        && known_model_max_output_tokens(kind, model).is_some_and(|limit| value > limit as u64)
    {
        return Err(ProviderError::InvalidResponse {
            message: if kind == ProviderKind::OpenCodeGo {
                "OpenCode Go context or output limit exceeds model metadata"
            } else {
                "max output tokens exceeds model metadata"
            }
            .into(),
        });
    }
    u32::try_from(value).map_err(|_| ProviderError::InvalidResponse {
        message: "SLIM_MAX_OUTPUT_TOKENS must fit in a positive 32-bit integer".into(),
    })
}

fn known_model_max_output_tokens(kind: ProviderKind, model: &str) -> Option<u32> {
    match kind {
        ProviderKind::OpenAiCodex => codex_model(model).map(|model| model.max_output_tokens),
        ProviderKind::ClinePass => clinepass_model(model).map(|model| model.max_output_tokens),
        ProviderKind::OpenCodeGo => {
            open_code_model(model).and_then(|model| model.max_output_tokens)
        }
        ProviderKind::Anthropic | ProviderKind::CommandCode | ProviderKind::OpenAiCompatible => {
            None
        }
    }
}

pub(crate) const DEFAULT_PROVIDER_TIMEOUT_SECS: u64 = 120;
const MAX_PROVIDER_TIMEOUT_SECS: u64 = 3600;
const MAX_RESULT_BYTES_HARD_CAP: usize = 1024 * 1024;

pub(crate) fn resolve_timeout_secs(explicit: Option<u64>) -> Result<Duration, ProviderError> {
    let seconds = if let Some(value) = explicit {
        if value == 0 {
            return Err(ProviderError::InvalidResponse {
                message: "timeout_secs must be positive".into(),
            });
        }
        value
    } else {
        parse_positive_env_u64("SLIM_TIMEOUT_SECS")
            .map_err(|message| ProviderError::InvalidResponse { message })?
            .unwrap_or(DEFAULT_PROVIDER_TIMEOUT_SECS)
    };
    Ok(Duration::from_secs(
        seconds.clamp(1, MAX_PROVIDER_TIMEOUT_SECS),
    ))
}

pub(crate) fn resolve_max_result_bytes(explicit: Option<usize>) -> Result<usize, ProviderError> {
    if let Some(value) = explicit {
        if value == 0 {
            return Err(ProviderError::InvalidResponse {
                message: "max_result_bytes must be positive".into(),
            });
        }
        return Ok(value.min(MAX_RESULT_BYTES_HARD_CAP));
    }
    parse_positive_env_usize("SLIM_MAX_RESULT_BYTES")
        .map_err(|message| ProviderError::InvalidResponse { message })
        .map(|value| {
            value
                .map(|value| value.min(MAX_RESULT_BYTES_HARD_CAP))
                .unwrap_or(16 * 1024)
        })
}

fn parse_positive_env_usize(name: &str) -> Result<Option<usize>, String> {
    parse_positive_env_u64(name).map(|value| value.map(|value| value as usize))
}

const MAX_MUTATING_TOOL_CALLS_HARD_CAP: usize = 256;
const MAX_READ_TOOL_CALLS_HARD_CAP: usize = 512;
const MAX_TURNS_HARD_CAP: usize = 1024;

fn clamp_mutating_tool_calls(value: usize) -> usize {
    value.min(MAX_MUTATING_TOOL_CALLS_HARD_CAP)
}

fn clamp_read_tool_calls(value: usize) -> usize {
    value.min(MAX_READ_TOOL_CALLS_HARD_CAP)
}

fn clamp_max_turns(value: usize) -> usize {
    value.min(MAX_TURNS_HARD_CAP)
}

pub(crate) fn resolve_max_mutating_tool_calls(
    options: &ProviderRunOptions,
) -> Result<usize, ProviderError> {
    if let Some(value) = options.max_tool_calls {
        return Ok(clamp_mutating_tool_calls(value));
    }
    parse_positive_env_usize("SLIM_MAX_MUTATING_TOOL_CALLS")
        .map_err(|message| ProviderError::InvalidResponse { message })
        .map(|value| {
            value
                .map(clamp_mutating_tool_calls)
                .unwrap_or(AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS)
        })
}

pub(crate) fn resolve_max_read_tool_calls(
    options: &ProviderRunOptions,
) -> Result<usize, ProviderError> {
    if let Some(value) = options.max_read_tool_calls {
        return Ok(clamp_read_tool_calls(value));
    }
    parse_positive_env_usize("SLIM_MAX_READ_TOOL_CALLS")
        .map_err(|message| ProviderError::InvalidResponse { message })
        .map(|value| {
            value
                .map(clamp_read_tool_calls)
                .unwrap_or(AgentLoopConfig::DEFAULT_MAX_READ_TOOL_CALLS)
        })
}

pub(crate) fn resolve_max_turns(options: &ProviderRunOptions) -> Result<usize, ProviderError> {
    if let Some(value) = options.max_turns {
        return Ok(clamp_max_turns(value));
    }
    parse_positive_env_usize("SLIM_MAX_TURNS")
        .map_err(|message| ProviderError::InvalidResponse { message })
        .map(|value| {
            value
                .map(clamp_max_turns)
                .unwrap_or(AgentLoopConfig::DEFAULT_MAX_TURNS)
        })
}

fn parse_positive_env_u64(name: &str) -> Result<Option<u64>, String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    let parsed = value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))?;
    Ok(Some(parsed))
}

fn empty_provider_execution(result: ProviderHeadlessResult) -> ProviderExecution {
    ProviderExecution {
        result,
        history: None,
        events: Vec::new(),
        tool_results: Vec::new(),
        max_mutating_tool_calls: AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS,
        max_read_tool_calls: AgentLoopConfig::DEFAULT_MAX_READ_TOOL_CALLS,
        max_turns: AgentLoopConfig::DEFAULT_MAX_TURNS,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        resume_preflight: None,
    }
}

pub(crate) fn format_run_stop_message(
    stop: &str,
    tool_results: &[slim_core::tools::ToolResult],
    limits: ToolLoopLimits,
) -> String {
    match stop {
        "tool_limit" if limits.max_read_tool_calls == 0 && limits.max_mutating_tool_calls == 0 => {
            "Durable resume cannot execute tools yet. Start a new run to continue with tools."
                .into()
        }
        "tool_limit" => {
            let (read_used, mutating_used) = count_tool_results_by_bucket(tool_results);
            let breakdown = summarize_tool_results(tool_results);
            if breakdown.is_empty() {
                format!(
                    "Tool budget exhausted (read {read_used}/{}, mutating {mutating_used}/{}). Send a follow-up to continue.",
                    limits.max_read_tool_calls, limits.max_mutating_tool_calls
                )
            } else {
                format!(
                    "Tool budget exhausted (read {read_used}/{}, mutating {mutating_used}/{}): {breakdown}. Send a follow-up to continue.",
                    limits.max_read_tool_calls, limits.max_mutating_tool_calls
                )
            }
        }
        "turn_limit" => format!(
            "Turn limit reached ({n}/{n}). Send a follow-up to continue.",
            n = limits.max_turns
        ),
        "provider_truncated" => format!(
            "Output truncated (max_output_tokens={}). Send a follow-up to continue.",
            limits.max_output_tokens
        ),
        "repeated_failed_tool" => "Repeated failed tool blocked.".into(),
        other => format!("Run stopped ({other})."),
    }
}

fn count_tool_results_by_bucket(tool_results: &[slim_core::tools::ToolResult]) -> (usize, usize) {
    let mut read = 0;
    let mut mutating = 0;
    for result in tool_results {
        if tool_call_is_read_only(&result.name) {
            read += 1;
        } else {
            mutating += 1;
        }
    }
    (read, mutating)
}

fn summarize_tool_results(tool_results: &[slim_core::tools::ToolResult]) -> String {
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for result in tool_results {
        *counts.entry(result.name.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(name, count)| format!("{count} {name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn stop_name(stop: AgentLoopStop) -> &'static str {
    match stop {
        AgentLoopStop::ProviderCompleted => "provider_completed",
        AgentLoopStop::ProviderTruncated => "provider_truncated",
        AgentLoopStop::ProviderFiltered => "provider_filtered",
        AgentLoopStop::TurnLimit => "turn_limit",
        AgentLoopStop::ToolLimit => "tool_limit",
        AgentLoopStop::RepeatedFailedTool => "repeated_failed_tool",
        AgentLoopStop::Cancelled => "cancelled",
    }
}

fn exit_code_for_stop(stop: AgentLoopStop) -> ExitCode {
    match stop {
        AgentLoopStop::ProviderCompleted => ExitCode::Success,
        AgentLoopStop::ProviderTruncated | AgentLoopStop::ProviderFiltered => ExitCode::Provider,
        AgentLoopStop::TurnLimit | AgentLoopStop::RepeatedFailedTool => ExitCode::Blocked,
        AgentLoopStop::ToolLimit => ExitCode::Tool,
        AgentLoopStop::Cancelled => ExitCode::Cancelled,
    }
}

fn provider_failure_details(error: ProviderError) -> (ExitCode, &'static str, String) {
    match error {
        ProviderError::Cancelled => (
            ExitCode::Cancelled,
            "cancelled",
            "provider request cancelled".into(),
        ),
        ProviderError::Transport { .. } => (
            ExitCode::Provider,
            "provider_error",
            "provider transport failed".into(),
        ),
        ProviderError::MalformedToolCall => (
            ExitCode::Provider,
            "provider_error",
            "provider returned a malformed tool call".into(),
        ),
        ProviderError::Remote { message } | ProviderError::InvalidResponse { message } => (
            ExitCode::Provider,
            "provider_error",
            format!("provider error: {}", crate::redact(&message)),
        ),
    }
}

fn redact_provider_error(error: ProviderError, secret: &str) -> ProviderError {
    let redact = |message: String| {
        if secret.is_empty() {
            message
        } else {
            message.replace(secret, "[REDACTED]")
        }
    };
    match error {
        ProviderError::Remote { message } => ProviderError::Remote {
            message: redact(message),
        },
        ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
            message: redact(message),
        },
        error => error,
    }
}

fn resolve_pricing() -> Option<UsagePricing> {
    let input = std::env::var("SLIM_INPUT_COST_MICROS_PER_MILLION")
        .ok()?
        .parse()
        .ok()?;
    let output = std::env::var("SLIM_OUTPUT_COST_MICROS_PER_MILLION")
        .ok()?
        .parse()
        .ok()?;
    let cache_write_micros_per_million = std::env::var("SLIM_CACHE_WRITE_COST_MICROS_PER_MILLION")
        .ok()
        .and_then(|value| value.parse().ok());
    let cache_read_micros_per_million = std::env::var("SLIM_CACHE_READ_COST_MICROS_PER_MILLION")
        .ok()
        .and_then(|value| value.parse().ok());
    Some(UsagePricing {
        provider: ProviderPricing {
            input_micros_per_million: input,
            output_micros_per_million: output,
        },
        cache_write_micros_per_million,
        cache_read_micros_per_million,
    })
}

#[cfg(test)]
mod stop_message_tests {
    use super::{format_run_stop_message, ToolLoopLimits};
    use slim_core::tool_call_is_read_only;
    use slim_core::tools::ToolResult;

    fn tool_result(name: &str) -> ToolResult {
        ToolResult {
            name: name.into(),
            success: true,
            output: "ok".into(),
            artifact: None,
        }
    }

    #[test]
    fn tool_limit_message_includes_bucket_counts_and_breakdown() {
        let results = vec![
            tool_result("search"),
            tool_result("read"),
            tool_result("write"),
        ];
        let message = format_run_stop_message(
            "tool_limit",
            &results,
            ToolLoopLimits {
                max_mutating_tool_calls: 32,
                max_read_tool_calls: 96,
                max_turns: 128,
                max_output_tokens: 4096,
            },
        );
        assert!(message.starts_with("Tool budget exhausted (read 2/96, mutating 1/32):"));
        assert!(message.contains("1 read, 1 search, 1 write"));
        assert!(message.contains("Send a follow-up to continue."));
        assert!(tool_call_is_read_only("search"));
    }

    #[test]
    fn turn_limit_and_repeated_failed_tool_messages_are_humanized() {
        let limits = ToolLoopLimits {
            max_mutating_tool_calls: 32,
            max_read_tool_calls: 96,
            max_turns: 128,
            max_output_tokens: 4096,
        };
        assert_eq!(
            format_run_stop_message("turn_limit", &[], limits),
            "Turn limit reached (128/128). Send a follow-up to continue."
        );
        assert_eq!(
            format_run_stop_message("repeated_failed_tool", &[], limits),
            "Repeated failed tool blocked."
        );
        assert_eq!(
            format_run_stop_message("provider_truncated", &[], limits),
            "Output truncated (max_output_tokens=4096). Send a follow-up to continue."
        );
        assert_eq!(
            format_run_stop_message(
                "tool_limit",
                &[],
                ToolLoopLimits {
                    max_mutating_tool_calls: 0,
                    max_read_tool_calls: 0,
                    max_turns: 128,
                    max_output_tokens: 4096,
                },
            ),
            "Durable resume cannot execute tools yet. Start a new run to continue with tools."
        );
    }
}

#[cfg(test)]
mod resolve_budget_tests {
    use super::{
        execute_provider_turn, resolve_max_mutating_tool_calls, resolve_max_output_tokens,
        resolve_max_read_tool_calls, resolve_max_result_bytes, resolve_max_turns,
        resolve_timeout_secs, ProviderRequest, ProviderRunOptions, DEFAULT_PROVIDER_TIMEOUT_SECS,
    };
    use slim_core::runtime::AgentLoopConfig;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let saved = vars
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect::<Vec<_>>();
        for (name, value) in vars {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
        f();
        for (name, prior) in saved {
            match prior {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    #[test]
    fn explicit_options_override_env_defaults() {
        with_env(
            &[
                ("SLIM_MAX_MUTATING_TOOL_CALLS", Some("64")),
                ("SLIM_MAX_READ_TOOL_CALLS", Some("128")),
                ("SLIM_MAX_TURNS", Some("32")),
            ],
            || {
                let options = ProviderRunOptions::default()
                    .with_max_tool_calls(8)
                    .with_max_read_tool_calls(24)
                    .with_max_turns(4);
                assert_eq!(
                    resolve_max_mutating_tool_calls(&options).expect("mutating"),
                    8
                );
                assert_eq!(resolve_max_read_tool_calls(&options).expect("read"), 24);
                assert_eq!(resolve_max_turns(&options).expect("turns"), 4);
            },
        );
    }

    #[test]
    fn env_vars_apply_when_options_unset() {
        with_env(
            &[
                ("SLIM_MAX_MUTATING_TOOL_CALLS", Some("48")),
                ("SLIM_MAX_READ_TOOL_CALLS", Some("120")),
                ("SLIM_MAX_TURNS", Some("64")),
            ],
            || {
                let options = ProviderRunOptions::default();
                assert_eq!(
                    resolve_max_mutating_tool_calls(&options).expect("mutating"),
                    48
                );
                assert_eq!(resolve_max_read_tool_calls(&options).expect("read"), 120);
                assert_eq!(resolve_max_turns(&options).expect("turns"), 64);
            },
        );
    }

    #[test]
    fn defaults_apply_without_env_or_options() {
        with_env(
            &[
                ("SLIM_MAX_MUTATING_TOOL_CALLS", None),
                ("SLIM_MAX_READ_TOOL_CALLS", None),
                ("SLIM_MAX_TURNS", None),
            ],
            || {
                let options = ProviderRunOptions::default();
                assert_eq!(
                    resolve_max_mutating_tool_calls(&options).expect("mutating"),
                    AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS
                );
                assert_eq!(
                    resolve_max_read_tool_calls(&options).expect("read"),
                    AgentLoopConfig::DEFAULT_MAX_READ_TOOL_CALLS
                );
                assert_eq!(
                    resolve_max_turns(&options).expect("turns"),
                    AgentLoopConfig::DEFAULT_MAX_TURNS
                );
            },
        );
    }

    #[test]
    fn max_turns_hard_cap_clamps_options_and_env() {
        with_env(&[("SLIM_MAX_TURNS", Some("4096"))], || {
            assert_eq!(
                resolve_max_turns(&ProviderRunOptions::default()).expect("env clamp"),
                1024
            );
            let options = ProviderRunOptions::default().with_max_turns(2048);
            assert_eq!(resolve_max_turns(&options).expect("options clamp"), 1024);
        });
    }

    #[test]
    fn max_turns_env_zero_is_rejected() {
        with_env(&[("SLIM_MAX_TURNS", Some("0"))], || {
            let error = resolve_max_turns(&ProviderRunOptions::default()).expect_err("zero");
            assert!(matches!(
                error,
                slim_core::ProviderError::InvalidResponse { .. }
            ));
        });
    }

    #[test]
    fn explicit_zero_max_turns_is_preserved() {
        with_env(&[("SLIM_MAX_TURNS", Some("64"))], || {
            let options = ProviderRunOptions::default().with_max_turns(0);
            assert_eq!(resolve_max_turns(&options).expect("zero option"), 0);
        });
    }

    #[test]
    fn default_max_output_is_operational_default_without_env_or_options() {
        with_env(&[("SLIM_MAX_OUTPUT_TOKENS", None)], || {
            let tokens = resolve_max_output_tokens(
                None,
                slim_core::provider::ProviderKind::OpenAiCodex,
                "gpt-5.6-sol",
            )
            .expect("default output cap");
            assert_eq!(tokens, slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS);
        });
    }

    #[test]
    fn explicit_and_env_max_output_are_bounded_by_catalog() {
        with_env(&[("SLIM_MAX_OUTPUT_TOKENS", Some("128001"))], || {
            assert!(resolve_max_output_tokens(
                None,
                slim_core::provider::ProviderKind::OpenAiCodex,
                "gpt-5.6-sol",
            )
            .is_err());
        });
        assert!(resolve_max_output_tokens(
            Some(128_001),
            slim_core::provider::ProviderKind::OpenAiCodex,
            "gpt-5.6-sol",
        )
        .is_err());
    }

    #[test]
    fn max_output_options_and_env_beat_catalog() {
        with_env(&[("SLIM_MAX_OUTPUT_TOKENS", Some("2048"))], || {
            assert_eq!(
                resolve_max_output_tokens(
                    None,
                    slim_core::provider::ProviderKind::OpenAiCodex,
                    "gpt-5.6-sol",
                )
                .expect("env"),
                2048
            );
            assert_eq!(
                resolve_max_output_tokens(
                    Some(512),
                    slim_core::provider::ProviderKind::OpenAiCodex,
                    "gpt-5.6-sol",
                )
                .expect("options"),
                512
            );
        });
    }

    #[test]
    fn compatible_and_command_code_fall_back_to_default_max_output() {
        with_env(&[("SLIM_MAX_OUTPUT_TOKENS", None)], || {
            assert_eq!(
                resolve_max_output_tokens(
                    None,
                    slim_core::provider::ProviderKind::OpenAiCompatible,
                    "gpt-4o-mini",
                )
                .expect("compat"),
                slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS
            );
            assert_eq!(
                resolve_max_output_tokens(
                    None,
                    slim_core::provider::ProviderKind::CommandCode,
                    slim_core::provider::COMMANDCODE_DEFAULT_MODEL,
                )
                .expect("command-code"),
                slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS
            );
        });
    }

    #[test]
    fn timeout_zero_is_rejected_and_idle_clamps_to_one_hour() {
        with_env(&[("SLIM_TIMEOUT_SECS", None)], || {
            let error = resolve_timeout_secs(Some(0)).expect_err("zero");
            assert!(matches!(
                error,
                slim_core::ProviderError::InvalidResponse { .. }
            ));
            assert_eq!(
                resolve_timeout_secs(None).expect("default").as_secs(),
                DEFAULT_PROVIDER_TIMEOUT_SECS
            );
            assert_eq!(
                resolve_timeout_secs(Some(9_000)).expect("clamp").as_secs(),
                3600
            );
        });
        with_env(&[("SLIM_TIMEOUT_SECS", Some("0"))], || {
            assert!(resolve_timeout_secs(None).is_err());
        });
    }

    #[test]
    fn max_result_bytes_zero_is_rejected_and_hard_cap_is_one_mib() {
        with_env(&[("SLIM_MAX_RESULT_BYTES", None)], || {
            let error = resolve_max_result_bytes(Some(0)).expect_err("zero");
            assert!(matches!(
                error,
                slim_core::ProviderError::InvalidResponse { .. }
            ));
            assert_eq!(resolve_max_result_bytes(None).expect("default"), 16 * 1024);
            assert_eq!(
                resolve_max_result_bytes(Some(2 * 1024 * 1024)).expect("clamp"),
                1024 * 1024
            );
        });
    }

    #[test]
    fn opencode_rejects_max_output_above_model_metadata() {
        with_env(
            &[
                ("SLIM_MAX_OUTPUT_TOKENS", None),
                ("SLIM_CONTEXT_WINDOW_TOKENS", None),
            ],
            || {
                let request = ProviderRequest {
                    prompt: "hi".into(),
                    mode: slim_core::OperatingMode::Auto,
                    kind: slim_core::provider::ProviderKind::OpenCodeGo,
                    endpoint: slim_core::provider::OPENCODE_GO_BASE_URL.into(),
                    model: slim_core::provider::OPENCODE_GO_DEFAULT_MODEL.into(),
                    api_key: "key".into(),
                    account_id: None,
                    timeout: std::time::Duration::from_secs(1),
                };
                match execute_provider_turn(
                    request,
                    None,
                    ProviderRunOptions::default().with_max_output_tokens(384_001),
                ) {
                    Err(error) => assert!(
                        matches!(
                            error,
                            slim_core::ProviderError::InvalidResponse { ref message }
                                if message.contains("OpenCode Go context or output limit")
                        ),
                        "{error:?}"
                    ),
                    Ok(_) => panic!("OpenCode overflow must reject before the network"),
                }
            },
        );
    }
}

#[cfg(test)]
mod plan_loop_tests {
    use super::{execute_provider_turn, ProviderRequest, ProviderRunOptions};
    use crate::exit_codes::ExitCode;
    use slim_core::provider::ProviderKind;
    use slim_core::OperatingMode;

    #[test]
    fn headless_plan_still_aborts_without_allow_plan_loop() {
        let execution = execute_provider_turn(
            ProviderRequest {
                prompt: "plan this".into(),
                mode: OperatingMode::Plan,
                kind: ProviderKind::OpenAiCompatible,
                endpoint: "http://127.0.0.1:1".into(),
                model: "unused".into(),
                api_key: "secret".into(),
                account_id: None,
                timeout: std::time::Duration::from_secs(1),
            },
            None,
            ProviderRunOptions::default(),
        )
        .expect("plan abort");
        assert_eq!(execution.result.code, ExitCode::ApprovalRequired);
        assert_eq!(execution.result.text, "approval_required");
        assert!(execution.events.is_empty());
    }
}

#[cfg(test)]
mod cache_report_tests {
    use super::provider_cache_report_line;
    use slim_core::provider::ProviderCacheStats;

    #[test]
    fn interactive_surface_never_emits_cache_line() {
        let stats = ProviderCacheStats {
            hits: 0,
            misses: 1,
            evictions: 0,
            entries: 1,
            retained_bytes: 32,
        };
        assert_eq!(provider_cache_report_line(&stats, true), None);
    }

    #[test]
    fn headless_run_emits_cache_line_after_a_miss() {
        let stats = ProviderCacheStats {
            hits: 0,
            misses: 1,
            evictions: 0,
            entries: 1,
            retained_bytes: 32,
        };
        let line = provider_cache_report_line(&stats, false).expect("headless report");
        assert!(line.starts_with("SLIM_PROVIDER_CACHE "));
        assert!(line.contains("misses=1"));
        assert!(!line.contains('\n'));
    }
}

#[cfg(test)]
mod compaction_resume_tests {
    use super::{durable_provider_history, DurableProviderHistory};
    use slim_core::context::{compaction_prefix_fingerprint, CompactionReason};
    use slim_core::session::{
        preflight_session, CompactionCheckpoint, DurableEntry, DurableEntryRole, DurableRecord,
        DurableRepo, DurableSessionHeader, JsonlRepo,
    };
    use slim_core::ProviderMessage;

    fn fixture_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "slim-compaction-resume-{label}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn durable_history_uses_only_a_matching_checkpoint_prefix() {
        for (label, fingerprint_matches) in [("valid", true), ("stale", false)] {
            let path = fixture_path(label);
            let mut repo = JsonlRepo::create(
                &path,
                DurableSessionHeader::new("session", "now", "D:\\Slim", None, None),
            )
            .expect("create");
            let entries = [
                ("root", DurableEntryRole::User, "root instruction", None),
                (
                    "old",
                    DurableEntryRole::Assistant,
                    "old answer",
                    Some("root"),
                ),
                (
                    "kept",
                    DurableEntryRole::User,
                    "recent question",
                    Some("old"),
                ),
            ];
            for (seq, (id, role, content, parent)) in entries.into_iter().enumerate() {
                repo.append(DurableRecord::Entry {
                    seq: seq as u64,
                    entry: DurableEntry {
                        entry_id: id.into(),
                        role,
                        content: content.into(),
                        parent_entry_id: parent.map(str::to_owned),
                        operation_id: format!("op-{id}"),
                        tool_call_id: None,
                    },
                })
                .expect("append entry");
            }
            let prefix = [
                ProviderMessage::user("root instruction"),
                ProviderMessage::assistant("old answer", Vec::new()),
            ];
            repo.append(DurableRecord::Compaction {
                seq: 3,
                checkpoint: CompactionCheckpoint {
                    checkpoint_id: "compact-1".into(),
                    summary: "## Goal\nContinue".into(),
                    first_kept_entry_id: "kept".into(),
                    prefix_fingerprint: if fingerprint_matches {
                        compaction_prefix_fingerprint(&prefix)
                    } else {
                        "0000000000000000".into()
                    },
                    previous_checkpoint_id: None,
                    tokens_before: 100,
                    tokens_after: 20,
                    input_tokens: Some(5),
                    output_tokens: Some(2),
                    duration_ms: 1,
                    reason: CompactionReason::HardThreshold,
                    read_files: vec![],
                    modified_files: vec![],
                },
            })
            .expect("append checkpoint");
            repo.append(DurableRecord::Entry {
                seq: 4,
                entry: DurableEntry {
                    entry_id: "kept-2".into(),
                    role: DurableEntryRole::Assistant,
                    content: "recent answer".into(),
                    parent_entry_id: Some("kept".into()),
                    operation_id: "op-kept-2".into(),
                    tool_call_id: None,
                },
            })
            .expect("append second kept entry");
            if fingerprint_matches {
                let chained_prefix = [
                    ProviderMessage::user("root instruction"),
                    ProviderMessage::user("[Compacted context]\n## Goal\nContinue"),
                    ProviderMessage::user("recent question"),
                ];
                repo.append(DurableRecord::Compaction {
                    seq: 5,
                    checkpoint: CompactionCheckpoint {
                        checkpoint_id: "compact-2".into(),
                        summary: "## Goal\nSecond".into(),
                        first_kept_entry_id: "kept-2".into(),
                        prefix_fingerprint: compaction_prefix_fingerprint(&chained_prefix),
                        previous_checkpoint_id: Some("compact-1".into()),
                        tokens_before: 80,
                        tokens_after: 15,
                        input_tokens: Some(4),
                        output_tokens: Some(2),
                        duration_ms: 1,
                        reason: CompactionReason::HardThreshold,
                        read_files: vec![],
                        modified_files: vec![],
                    },
                })
                .expect("append chained checkpoint");
                repo.append(DurableRecord::Compaction {
                    seq: 6,
                    checkpoint: CompactionCheckpoint {
                        checkpoint_id: "compact-invalid".into(),
                        summary: "ignored".into(),
                        first_kept_entry_id: "kept-2".into(),
                        prefix_fingerprint: "0000000000000000".into(),
                        previous_checkpoint_id: Some("compact-2".into()),
                        tokens_before: 15,
                        tokens_after: 10,
                        input_tokens: Some(1),
                        output_tokens: Some(1),
                        duration_ms: 1,
                        reason: CompactionReason::HardThreshold,
                        read_files: vec![],
                        modified_files: vec![],
                    },
                })
                .expect("append invalid checkpoint");
            }
            drop(repo);

            let preflight = preflight_session(&path).expect("preflight");
            let DurableProviderHistory {
                messages: history,
                parent_entry_id: parent,
                entry_ids: ids,
                applied_checkpoint_id,
            } = durable_provider_history(&preflight).expect("history");
            assert_eq!(parent.as_deref(), Some("kept-2"));
            if fingerprint_matches {
                assert_eq!(history.len(), 3);
                assert_eq!(history[0].content, "root instruction");
                assert!(history[1].content.contains("Second"));
                assert_eq!(history[2].content, "recent answer");
                assert_eq!(ids, vec![Some("root".into()), None, Some("kept-2".into())]);
                assert_eq!(applied_checkpoint_id.as_deref(), Some("compact-2"));
            } else {
                assert_eq!(history.len(), 4);
                assert_eq!(history[1].content, "old answer");
                assert_eq!(applied_checkpoint_id, None);
            }
            std::fs::remove_file(&path).expect("cleanup data");
            let lock = path.with_extension("jsonl.lock");
            if lock.exists() {
                std::fs::remove_file(lock).expect("cleanup lock");
            }
        }
    }
}

#[cfg(test)]
mod resume_preflight_transport_tests {
    use super::{run_provider_resume_with_preflight_events, ProviderRequest, ProviderRunOptions};
    use slim_core::provider::{ProviderError, ProviderKind};
    use slim_core::session::{preflight_session, DurableSessionHeader, JsonlRepo};
    use slim_core::OperatingMode;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn fixture_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "slim-resume-preflight-transport-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn transported_preflight_remains_the_snapshot_used_by_resume() {
        let path = fixture_path();
        JsonlRepo::create(
            &path,
            DurableSessionHeader::new("first", "now", "D:\\Slim", None, None),
        )
        .expect("first repo");
        let preflight = preflight_session(&path).expect("preflight");
        std::fs::remove_file(&path).expect("replace fixture");
        JsonlRepo::create(
            &path,
            DurableSessionHeader::new("replacement", "later", "D:\\Slim", None, None),
        )
        .expect("replacement repo");

        let error = match run_provider_resume_with_preflight_events(
            ProviderRequest {
                prompt: "continue".into(),
                mode: OperatingMode::Auto,
                kind: ProviderKind::OpenAiCompatible,
                endpoint: "http://127.0.0.1:1".into(),
                model: "offline".into(),
                api_key: "local-fixture".into(),
                account_id: None,
                timeout: Duration::from_millis(50),
            },
            preflight,
            ProviderRunOptions::default(),
            None,
        ) {
            Ok(_) => panic!("replacement after preflight must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ProviderError::InvalidResponse { ref message }
                if message.contains("changed after read-only preflight")
        ));

        let _ = std::fs::remove_file(path);
    }
}
