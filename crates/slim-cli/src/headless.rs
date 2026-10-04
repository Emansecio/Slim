use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use serde::Serialize;
use sha2::{Digest, Sha256};
use slim_core::context::{CompactionHandle, CompactionPolicy};
use slim_core::provider::{
    clinepass_model, codex_model, command_code_model, history_response_cache_scope,
    open_code_model, xai_model, zen_model, AnthropicAdapter, ClinePassAdapter, CommandCodeAdapter,
    HttpProviderClient, OpenAiCodexAdapter, OpenAiCompatibleAdapter, OpenCodeGoAdapter,
    OpenCodeZenAdapter, ProviderAdapter, ProviderConfig, ProviderContentBlock, ProviderError,
    ProviderKind, ProviderPricing, ProviderTimeouts, XaiAdapter, DEFAULT_MAX_OUTPUT_TOKENS,
};
use slim_core::runtime::{
    tool_call_is_read_only, AgentLoopConfig, AgentLoopResult, AgentLoopStop, CancellationToken,
};
use slim_core::session::{
    preflight_session, provider_messages_from_entries, DurableEntry, DurableErrorClass,
    DurableOutcome, DurableRepo, DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec,
    ProviderResponse, RunTelemetryContext, RunTelemetryTerminal, SessionFormat, SessionPreflight,
};
use slim_core::tools::ToolRegistry;
use slim_core::{
    EventKind, InteractionRoute, OperatingMode, ProviderMessage, RequestKind, Runtime,
    SessionEvent, SessionEventSender, UsageTotals,
};

use crate::codex_catalog::{should_fetch_live_codex_catalog, CodexCatalog};
use crate::command_code_catalog::CommandCodeCatalog;
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutableIdentity {
    sha256: Option<String>,
    error: Option<String>,
}

static EXECUTABLE_IDENTITY: OnceLock<ExecutableIdentity> = OnceLock::new();

fn executable_identity() -> &'static ExecutableIdentity {
    EXECUTABLE_IDENTITY.get_or_init(|| {
        let path = match std::env::current_exe() {
            Ok(path) => path,
            Err(_) => {
                return ExecutableIdentity {
                    sha256: None,
                    error: Some("current_exe_unavailable".into()),
                };
            }
        };
        executable_identity_from_path(&path)
    })
}

fn executable_identity_from_path(path: &Path) -> ExecutableIdentity {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return ExecutableIdentity {
                sha256: None,
                error: Some("executable_read_failed".into()),
            };
        }
    };
    let digest = Sha256::digest(bytes);
    ExecutableIdentity {
        sha256: Some(format!("{digest:x}")),
        error: None,
    }
}

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

/// Cloneable identity for the application-scoped MCP manager. Equality is
/// pointer identity so ProviderRunOptions remains deterministic in tests.
#[derive(Clone)]
pub struct McpHandle(Arc<slim_core::mcp::McpManager>);

impl McpHandle {
    pub fn new(manager: Arc<slim_core::mcp::McpManager>) -> Self {
        Self(manager)
    }

    pub fn manager(&self) -> &Arc<slim_core::mcp::McpManager> {
        &self.0
    }
}

impl std::fmt::Debug for McpHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpHandle")
            .field("manager", &"application-scoped")
            .finish()
    }
}

impl PartialEq for McpHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for McpHandle {}

/// Cloneable registry retained by one interactive host across per-turn runtimes.
#[derive(Clone)]
pub struct SharedToolRegistry(Arc<ToolRegistry>);

impl SharedToolRegistry {
    fn new() -> Self {
        Self(Arc::new(ToolRegistry::default()))
    }

    pub(crate) fn registry(&self) -> ToolRegistry {
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
    /// Stable conversation identity for provider routing, shared across turns.
    pub provider_session_id: Option<String>,
    /// Optional benchmark grouping identifier persisted only in durable run
    /// telemetry. It is never added to the provider prompt.
    pub experiment_id: Option<String>,
    /// Optional caller-defined task identifier persisted only in durable run
    /// telemetry. It is never added to the provider prompt.
    pub task_id: Option<String>,
    pub content_blocks: Vec<ProviderContentBlock>,
    pub history: Vec<ProviderMessage>,
    pub task_facts: Vec<slim_core::session::DurableFact>,
    pub artifact_ids: Vec<String>,
    pub shell_jobs: Option<slim_core::runtime::ShellJobs>,
    pub shell_job_limits: slim_core::runtime::ShellJobLimits,
    pub workspace_root: Option<PathBuf>,
    pub artifact_root: Option<PathBuf>,
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub codex_fast: bool,
    pub max_turns: Option<usize>,
    /// Mutating-tool budget (write/patch/shell/todo/skill/ask_question).
    pub max_tool_calls: Option<usize>,
    pub max_read_tool_calls: Option<usize>,
    pub max_total_tool_calls: Option<usize>,
    pub max_result_bytes: Option<usize>,
    pub cancellation: Option<CancellationToken>,
    pub compaction: Option<CompactionHandle>,
    /// Interactive-only retry, scoped to one active run. Never persisted.
    pub manual_retry: Option<slim_core::runtime::ManualRetryHandle>,
    /// Shared for the whole host application; cloned into per-turn runtimes.
    pub code_intelligence: Option<CodeIntelligenceHandle>,
    /// Application-scoped MCP manager; connections stay lazy per server.
    pub mcp: Option<McpHandle>,
    /// Native tool snapshots retained for the lifetime of an interactive host.
    pub tool_registry: Option<SharedToolRegistry>,
    /// TUI Plan runs the read-only agent loop. Headless Plan stays abort-only.
    pub allow_plan_loop: bool,
    /// Run only a manual compaction of `history`: no prompt, no model turn.
    /// Set by the TUI for `/compact` while the session is idle.
    pub compact_only: bool,
    /// Start MCP servers defined by the workspace's `slim.toml` for this run
    /// without a stored trust decision (`--trust-project`). Never persisted.
    pub trust_project: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInstructions {
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) source: PathBuf,
}

impl ProviderRunOptions {
    pub fn with_experiment_id(mut self, experiment_id: impl Into<String>) -> Self {
        self.experiment_id = Some(experiment_id.into());
        self
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

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

    pub fn with_max_total_tool_calls(mut self, calls: usize) -> Self {
        self.max_total_tool_calls = Some(calls);
        self
    }

    pub fn with_max_result_bytes(mut self, bytes: usize) -> Self {
        self.max_result_bytes = Some(bytes);
        self
    }

    pub fn with_trust_project(mut self, trust: bool) -> Self {
        self.trust_project = trust;
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
    /// Local stop evidence, independent of whether the model produced a final answer.
    pub stop_message: Option<String>,
    pub cost_micros: Option<u64>,
    pub usage_complete: bool,
    pub usage_overflowed: bool,
    pub usage: UsageTotals,
    pub costs: UsageCostSummary,
    pub validation_source: Option<String>,
    pub tool_summary_lines: Vec<String>,
    /// Typed process observations emitted by native tool executions. These
    /// remain separate from the legacy tool result/success fields.
    pub tool_process_facts: Vec<ToolProcessFact>,
    /// Final outputs of background shell jobs, keyed to the original call.
    pub tool_job_outputs: Vec<ToolJobOutputFact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolProcessFact {
    pub batch_id: String,
    pub call_id: String,
    pub name: String,
    pub process: slim_core::process::ProcessExecutionFacts,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolJobOutputFact {
    pub batch_id: String,
    pub call_id: String,
    pub name: String,
    pub output: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct UsageCostSummary {
    pub total_micros: Option<u64>,
    pub cost_per_validated_completion_micros: Option<u64>,
    pub failed_attempts_micros: Option<u64>,
    pub compaction_micros: Option<u64>,
    pub cancelled_estimated_micros: Option<u64>,
    /// True when a usage component could not be confirmed (for example an
    /// interrupted provider request). Cost fields involving that component remain
    /// `None` instead of presenting a partial number.
    pub usage_unknown: bool,
    /// True when a usage component has no static price catalogue entry.
    pub pricing_unknown: bool,
}

pub(crate) struct ProviderExecution {
    pub result: ProviderHeadlessResult,
    pub history: Option<Vec<ProviderMessage>>,
    pub turn_transcript: Vec<ProviderMessage>,
    pub task_facts: Vec<slim_core::session::DurableFact>,
    pub events: Vec<SessionEvent>,
    pub tool_results: Vec<slim_core::tools::ToolResult>,
    pub limits: ToolLoopLimits,
    pub resume_preflight: Option<SessionPreflight>,
    /// Durable-session problems the run did not fail on (a compaction
    /// checkpoint that could not be anchored or applied): the interface shows
    /// them so none is silent.
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolLoopLimits {
    pub max_mutating_tool_calls: usize,
    pub max_read_tool_calls: usize,
    pub max_total_tool_calls: usize,
    pub max_turns: usize,
    pub max_output_tokens: u32,
    pub max_result_bytes: usize,
    pub context_window_tokens: u64,
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
        .map(into_result_reporting_warnings)
}

pub(crate) fn run_provider_headless_with_resume_preflight_and_options(
    request: ProviderRequest,
    preflight: SessionPreflight,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    run_provider_resume_with_preflight_events(request, preflight, options, None)
        .map(into_result_reporting_warnings)
}

/// The result of a run no interface watches: its warnings go to stderr.
fn into_result_reporting_warnings(execution: ProviderExecution) -> ProviderHeadlessResult {
    for warning in &execution.warnings {
        eprintln!("warning: {warning}");
    }
    execution.result
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
    if request.prompt.trim().is_empty() && !options.compact_only {
        return Ok(empty_provider_execution(input_required_result(&request)));
    }
    if request.mode == OperatingMode::Plan && !options.allow_plan_loop {
        return execute_provider_turn_with_local_lsp(
            request,
            false,
            options,
            skill_instructions,
            None,
            None,
        )
        .await;
    }

    // Replay and validation can scan a large JSONL. Keep that work off the
    // async executor while preserving the expected-prefix check under its lock.
    let (history, repo, preflight) = tokio::task::spawn_blocking(move || {
        let history = durable_provider_history(&preflight)?;
        let repo = JsonlRepo::open_no_repair_expected(
            &preflight.path,
            preflight
                .header
                .as_ref()
                .ok_or_else(|| resume_error("missing durable session header"))?,
            &preflight.records,
        )
        .map_err(|error| resume_error(error.to_string()))?;
        Ok::<_, ProviderError>((history, repo, preflight))
    })
    .await
    .map_err(|error| resume_error(format!("session replay worker failed: {error}")))??;
    let DurableProviderHistory {
        messages: history,
        parent_entry_id,
        entry_ids: _,
        applied_checkpoint_id: _,
        skipped_checkpoints: mut warnings,
    } = history;
    let workspace = PathBuf::from(
        &preflight
            .header
            .as_ref()
            .ok_or_else(|| resume_error("missing session header"))?
            .cwd,
    );
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|error| resume_error(format!("session workspace: {error}")))?;
    if let Some(requested) = options.workspace_root.as_ref() {
        let requested = std::fs::canonicalize(requested)
            .map_err(|error| resume_error(format!("workspace: {error}")))?;
        if requested != workspace {
            return Err(resume_error(
                "requested workspace differs from the session workspace",
            ));
        }
    }
    // The journal drives its own records; building the full ResumePlan
    // (reducer + attempt/tool/queue ledgers) here would be discarded work on
    // every prompt. ensure_resume_preflight already covered the same gates.
    let first_seq = repo
        .next_seq()
        .map_err(|error| resume_error(error.to_string()))?;
    let operation_id = format!("resume-{}-{first_seq}", repo.header().id);
    let input_entry_id = format!("{operation_id}-input");
    let assistant_entry_id = format!("{operation_id}-assistant");
    let attempt_id = format!("{operation_id}-attempt");
    let mut options = options;
    options.provider_session_id = Some(repo.header().id.clone());
    options.workspace_root = Some(workspace);
    if options.compact_only {
        if !keep_live_history(&options.history, &history) {
            options.history = history;
        }
        options.task_facts = session_task_facts(&preflight);
        options.artifact_ids = session_artifact_ids(&preflight);
        return run_manual_compaction(request, options, event_sender, repo, warnings).await;
    }
    // Attach before the first redaction so MCP env/header values are
    // scrubbed from the journal too.
    let (local_mcp, mcp_warnings) = attach_local_mcp(&mut options);
    warnings.extend(mcp_warnings);
    if let Some(manager) = &local_mcp {
        warnings
            .extend(crate::mcp::await_direct_startup(manager, options.cancellation.as_ref()).await);
    }
    let input_message = redact_durable_message(
        ProviderMessage::user(&request.prompt).with_content_blocks(options.content_blocks.clone()),
        &durable_secrets(&request, &options),
    );
    let redacted_prompt = input_message.content.clone();
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
    spec.input_content_blocks = input_message.content_blocks;
    // Same-process TUI continuation keeps the live conversation when it is
    // the durable prefix plus adapter-scoped reasoning. Cold resume leaves
    // options.history empty and uses the JSONL reconstruction.
    if !keep_live_history(&options.history, &history) {
        options.history = history;
    }
    options.task_facts = session_task_facts(&preflight);
    options.artifact_ids = session_artifact_ids(&preflight);
    spec = spec.with_run_telemetry(durable_run_telemetry_context(&request, &options)?);
    let compaction_handle = options.compaction.clone();
    let journal = std::sync::Arc::new(std::sync::Mutex::new(
        ManualRunJournal::start(repo, spec).map_err(|error| resume_error(error.to_string()))?,
    ));
    let mut executor = DurableProviderExecutor::new(
        request,
        options,
        skill_instructions,
        event_sender,
        interaction_route,
        journal.clone(),
    );
    let drive_result = match executor.execute_async().await {
        Ok(response) => journal
            .lock()
            .map_err(|_| resume_error("durable run lock poisoned"))?
            .finish(response)
            .map_err(slim_core::session::ManualDriveError::Persist),
        Err(error) => {
            journal
                .lock()
                .map_err(|_| resume_error("durable run lock poisoned"))?
                .fail_attempt(classify_provider_error(&error))
                .map_err(|error| resume_error(error.to_string()))?;
            Err(slim_core::session::ManualDriveError::Execute(error))
        }
    };
    if let Some(manager) = local_mcp {
        manager.disconnect_all().await;
    }
    // Commits belong to this run: take them whatever its outcome, so a failed
    // or cancelled run still persists the compactions it applied and none is
    // left in the handle for a later run to mis-anchor.
    let commits = compaction_handle
        .as_ref()
        .map(slim_core::context::CompactionHandle::take_commits)
        .unwrap_or_default();
    match drive_result {
        Ok(()) => {
            warnings.extend(persist_commits_off_executor(&journal, commits).await?);
            let mut journal_guard = journal
                .lock()
                .map_err(|_| resume_error("durable run lock poisoned"))?;
            let repo = journal_guard.repo_mut();
            let mut execution = executor
                .execution
                .ok_or_else(|| resume_error("durable provider execution produced no result"))?;
            execution.resume_preflight = Some(SessionPreflight::from_open_repo(repo));
            warnings.append(&mut execution.warnings);
            execution.warnings = warnings;
            Ok(execution)
        }
        Err(slim_core::session::ManualDriveError::Execute(error)) => {
            {
                let mut journal_guard = journal
                    .lock()
                    .map_err(|_| resume_error("durable run lock poisoned"))?;
                let repo = journal_guard.repo_mut();
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
            }
            // The run's own failure stays the reported result (a cancel must
            // keep reading as one); a checkpoint that cannot be appended to the
            // journal that just recorded that failure is dropped with it, and
            // the session replays the entries it would have replaced.
            let _ = persist_commits_off_executor(&journal, commits).await;
            Err(error)
        }
        Err(other) => Err(resume_error(format!("{other:?}"))),
    }
}

/// A manual /compact of a durable session while it is idle: the summary is
/// produced without a model turn and its checkpoint is appended to the
/// session. There is no prompt and so no operation record: a compaction that
/// fails or is cancelled leaves the session as it was.
async fn run_manual_compaction(
    request: ProviderRequest,
    options: ProviderRunOptions,
    event_sender: Option<SessionEventSender>,
    mut repo: JsonlRepo,
    mut warnings: Vec<String>,
) -> Result<ProviderExecution, ProviderError> {
    let compaction_handle = options.compaction.clone();
    let result =
        execute_provider_turn_async(request, false, options, None, event_sender, None).await;
    // The commits are this run's whatever its outcome, so none is left in the
    // handle for a later run to mis-anchor.
    let commits = compaction_handle
        .as_ref()
        .map(slim_core::context::CompactionHandle::take_commits)
        .unwrap_or_default();
    let mut execution = result?;
    if !commits.is_empty() {
        // Replaying a large journal per commit stays off the async executor.
        let persisted = tokio::task::spawn_blocking(move || {
            let warnings = persist_compaction_commits(&mut repo, commits);
            (repo, warnings)
        })
        .await
        .map_err(|error| resume_error(format!("checkpoint worker failed: {error}")))?;
        repo = persisted.0;
        warnings.extend(persisted.1?);
    }
    execution.resume_preflight = Some(SessionPreflight::from_open_repo(&repo));
    warnings.append(&mut execution.warnings);
    execution.warnings = warnings;
    Ok(execution)
}

/// [`persist_compaction_commits`] on the blocking pool: every commit replays
/// the whole journal, which must not stall the executor's other tasks. The
/// journal lock is held on the blocking thread for the duration.
async fn persist_commits_off_executor(
    journal: &std::sync::Arc<std::sync::Mutex<ManualRunJournal>>,
    commits: Vec<slim_core::context::CompactionCommit>,
) -> Result<Vec<String>, ProviderError> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let journal = journal.clone();
    tokio::task::spawn_blocking(move || {
        let mut journal = journal
            .lock()
            .map_err(|_| resume_error("durable run lock poisoned"))?;
        persist_compaction_commits(journal.repo_mut(), commits)
    })
    .await
    .map_err(|error| resume_error(format!("checkpoint worker failed: {error}")))?
}

/// Appends the checkpoints a run applied, in order. Each one is anchored on
/// the history persisted so far, so it chains onto the previous checkpoint. A
/// commit whose summarized prefix no longer matches the persisted entries has
/// no durable anchor and is skipped: the session stays valid and replays the
/// entries the checkpoint would have replaced. The fingerprint covers only
/// what the journal records, so this is not expected; each skip is returned as
/// a warning for the interface to show.
fn persist_compaction_commits(
    repo: &mut JsonlRepo,
    commits: Vec<slim_core::context::CompactionCommit>,
) -> Result<Vec<String>, ProviderError> {
    let mut warnings = Vec::new();
    for commit in commits {
        let persisted = durable_provider_history(&SessionPreflight::from_open_repo(repo))?;
        let Some(first_kept_entry_id) = durable_checkpoint_anchor(
            &persisted.messages,
            &persisted.entry_ids,
            commit.first_kept_index,
            &commit.canonical_prefix_fingerprint,
        ) else {
            warnings.push(format!(
                "A compaction ({} -> {} tokens) could not be saved to the session: its history does not match the journal. The session keeps the entries it replaced.",
                commit.tokens_before, commit.tokens_after
            ));
            continue;
        };
        let seq = repo
            .next_seq()
            .map_err(|error| resume_error(error.to_string()))?;
        repo.append(slim_core::session::DurableRecord::Compaction {
            seq,
            checkpoint: slim_core::session::CompactionCheckpoint {
                checkpoint_id: format!("compact-{}-{seq}", repo.header().id),
                summary: commit.summary,
                first_kept_entry_id,
                // The journal's own order: what the rebuild checks.
                prefix_fingerprint: slim_core::context::compaction_prefix_fingerprint(
                    &persisted.messages[..commit.first_kept_index],
                ),
                previous_checkpoint_id: persisted.applied_checkpoint_id,
                tokens_before: commit.tokens_before,
                tokens_after: commit.tokens_after,
                input_tokens: Some(commit.input_tokens),
                output_tokens: Some(commit.output_tokens),
                duration_ms: commit.duration_ms,
                reason: commit.reason,
                read_files: commit.read_files,
                modified_files: commit.modified_files,
            },
        })
        .map_err(|error| resume_error(error.to_string()))?;
    }
    Ok(warnings)
}

/// The entry a commit's checkpoint keeps from, when the persisted history
/// holds the commit's summarized prefix. A parallel batch's results are
/// journaled as the calls complete and live in call order, so the prefixes
/// compare by their canonical fingerprint.
fn durable_checkpoint_anchor(
    messages: &[ProviderMessage],
    entry_ids: &[Option<String>],
    first_kept_index: usize,
    canonical_prefix_fingerprint: &str,
) -> Option<String> {
    if messages.len() != entry_ids.len()
        || first_kept_index >= messages.len()
        || messages[first_kept_index].role == "tool"
        || slim_core::context::canonical_prefix_fingerprint(&messages[..first_kept_index])
            != canonical_prefix_fingerprint
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
            "resume requires an explicit decision for existing pending, claimed, or suspended durable work; inspect prior effects, then use Slim --headless --recover PATH --abandon-pending to abandon without replay"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn session_task_facts(
    preflight: &SessionPreflight,
) -> Vec<slim_core::session::DurableFact> {
    preflight
        .records
        .iter()
        .filter_map(|record| match record {
            slim_core::session::DurableRecord::Fact { fact, .. } if fact.namespace == "task.v1" => {
                Some(fact.clone())
            }
            _ => None,
        })
        .collect()
}

pub(crate) fn session_artifact_ids(preflight: &SessionPreflight) -> Vec<String> {
    preflight
        .records
        .iter()
        .filter_map(|record| match record {
            slim_core::session::DurableRecord::Fact { fact, .. }
                if fact.namespace == "artifact.v1" =>
            {
                Some(fact.key.clone())
            }
            _ => None,
        })
        .collect()
}

struct DurableProviderHistory {
    messages: Vec<ProviderMessage>,
    parent_entry_id: Option<String>,
    entry_ids: Vec<Option<String>>,
    applied_checkpoint_id: Option<String>,
    /// Checkpoints of the session that were not applied, as notices.
    skipped_checkpoints: Vec<String>,
}

fn durable_provider_history(
    preflight: &SessionPreflight,
) -> Result<DurableProviderHistory, ProviderError> {
    let rebuilt =
        slim_core::session::rebuild_provider_history(&preflight.records).map_err(resume_error)?;
    Ok(DurableProviderHistory {
        messages: rebuilt.messages,
        parent_entry_id: rebuilt.parent_entry_id,
        entry_ids: rebuilt.entry_ids,
        applied_checkpoint_id: rebuilt.applied_checkpoint_id,
        skipped_checkpoints: rebuilt
            .skipped_checkpoints
            .into_iter()
            .map(|skipped| {
                format!(
                    "Compaction checkpoint {} was not applied ({}): the session replays the entries it would have replaced.",
                    skipped.checkpoint_id, skipped.reason
                )
            })
            .collect(),
    })
}

pub(crate) fn resume_messages_from_preflight(
    preflight: &SessionPreflight,
) -> Result<Vec<ProviderMessage>, ProviderError> {
    durable_provider_history(preflight).map(|history| history.messages)
}

fn redact_secret(input: &str, secrets: &[String]) -> String {
    crate::auth::redact_with_secrets(input, secrets)
}

/// API key + every configured MCP env/header value: the set scrubbed from
/// anything persisted to the durable journal.
fn durable_secrets(request: &ProviderRequest, options: &ProviderRunOptions) -> Vec<String> {
    let mut secrets = vec![request.api_key.clone()];
    if let Some(mcp) = options.mcp.as_ref() {
        secrets.extend(mcp.manager().sensitive_values());
    }
    secrets
}

/// Whether the live message is what the journal recorded of it. The journal
/// stores user input through the heuristic credential redactor while the live
/// copy only had its exact secret values replaced, so both sides compare in the
/// redacted form (redaction is idempotent on what the journal already holds).
fn durable_visible_eq(left: &ProviderMessage, right: &ProviderMessage) -> bool {
    // The raw comparison settles the common equal case with one memcmp; the
    // credential redactor, which copies and may parse its input, only runs
    // when the raw texts differ.
    fn strip(message: &ProviderMessage) -> &str {
        slim_core::without_workspace_snapshot(&message.content)
    }
    let blocks_eq = left.content_blocks.len() == right.content_blocks.len()
        && left
            .content_blocks
            .iter()
            .zip(&right.content_blocks)
            .all(|pair| match pair {
                (ProviderContentBlock::Text(left), ProviderContentBlock::Text(right)) => {
                    left == right || crate::auth::redact(left) == crate::auth::redact(right)
                }
                (left, right) => left == right,
            });
    left.role == right.role
        && (strip(left) == strip(right)
            || crate::auth::redact(strip(left)) == crate::auth::redact(strip(right)))
        && left.name == right.name
        && left.tool_call_id == right.tool_call_id
        && left.tool_calls == right.tool_calls
        && blocks_eq
}

fn keep_live_history(live: &[ProviderMessage], durable: &[ProviderMessage]) -> bool {
    !live.is_empty()
        && live.len() == durable.len()
        && live
            .iter()
            .zip(durable)
            .all(|(left, right)| durable_visible_eq(left, right))
}

/// Binds the history cache scope, builds the shared-transport client and runs
/// the loop. The outer error is a client setup failure, which the caller
/// returns before any loop result exists; the inner result is the loop outcome.
/// The loop future is boxed: it is large, and the eight call sites would
/// otherwise each reserve a copy on the caller's (main-thread) stack.
#[allow(clippy::too_many_arguments)]
async fn run_bound_agent_loop<A: ProviderAdapter + Send + Sync + 'static>(
    runtime: &mut Runtime,
    adapter: A,
    bind: impl FnOnce(A, u64) -> A,
    initial_messages: &[ProviderMessage],
    mode: OperatingMode,
    cwd: &Path,
    config: AgentLoopConfig,
    timeouts: ProviderTimeouts,
    compact_only: bool,
) -> Result<Result<AgentLoopResult, ProviderError>, ProviderError> {
    let model = adapter.model().to_owned();
    let adapter = bind_history_scope(adapter, &model, initial_messages, bind);
    let client = HttpProviderClient::with_shared_transport(adapter, timeouts)?;
    let first_seq = 1;
    Ok(if compact_only {
        Box::pin(runtime.compact_messages(&client, initial_messages, mode, cwd, first_seq, config))
            .await
    } else {
        Box::pin(runtime.run_agent_loop_with_messages(
            &client,
            initial_messages,
            mode,
            cwd,
            first_seq,
            config,
        ))
        .await
    })
}

fn bind_history_scope<A>(
    adapter: A,
    model: &str,
    history: &[ProviderMessage],
    bind: impl FnOnce(A, u64) -> A,
) -> A {
    match history_response_cache_scope(history, model) {
        Some(scope) => bind(adapter, scope),
        None => adapter,
    }
}

fn redact_durable_message(mut message: ProviderMessage, secrets: &[String]) -> ProviderMessage {
    message.content = redact_secret(&message.content, secrets);
    message.name = message.name.map(|name| redact_secret(&name, secrets));
    message.tool_call_id = message.tool_call_id.map(|id| redact_secret(&id, secrets));
    for call in &mut message.tool_calls {
        call.id = redact_secret(&call.id, secrets);
        call.name = redact_secret(&call.name, secrets);
        call.arguments = redact_secret(&call.arguments, secrets);
    }
    for block in &mut message.content_blocks {
        if let ProviderContentBlock::Image { data, .. }
        | ProviderContentBlock::Audio { data, .. }
        | ProviderContentBlock::File { data, .. } = block
        {
            if base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .is_ok_and(|bytes| {
                    secrets.iter().any(|secret| {
                        !secret.is_empty()
                            && bytes
                                .windows(secret.len())
                                .any(|window| window == secret.as_bytes())
                    })
                })
            {
                *block = ProviderContentBlock::Text("[REDACTED attachment]".into());
                continue;
            }
        }
        match block {
            ProviderContentBlock::Text(text) => *text = redact_secret(text, secrets),
            ProviderContentBlock::Unsupported { kind } => *kind = redact_secret(kind, secrets),
            ProviderContentBlock::Image { media_type, data }
            | ProviderContentBlock::Audio { media_type, data }
            | ProviderContentBlock::File { media_type, data } => {
                *media_type = redact_secret(media_type, secrets);
                *data = redact_secret(data, secrets);
            }
        }
    }
    message.responses_reasoning.clear();
    message.chat_reasoning = None;
    message
}

#[cfg(test)]
#[test]
fn durable_redaction_covers_attachment_fields() {
    let request = ProviderRequest {
        prompt: String::new(),
        mode: slim_core::OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint: "http://127.0.0.1:1".into(),
        model: "fixture".into(),
        api_key: "provider-secret".into(),
        account_id: None,
        timeout: std::time::Duration::from_secs(1),
    };
    let options = ProviderRunOptions::default();
    let mut message = ProviderMessage::user("provider-secret");
    message.content_blocks = vec![
        ProviderContentBlock::Image {
            media_type: "image/provider-secret".into(),
            data: "provider-secret".into(),
        },
        ProviderContentBlock::Audio {
            media_type: "audio/provider-secret".into(),
            data: "provider-secret".into(),
        },
        ProviderContentBlock::File {
            media_type: "file/provider-secret".into(),
            data: "provider-secret".into(),
        },
        ProviderContentBlock::File {
            media_type: "application/octet-stream".into(),
            data: encode_base64(b"prefix provider-secret suffix"),
        },
    ];
    let redacted = redact_durable_message(message, &durable_secrets(&request, &options));
    let entry = slim_core::session::DurableEntry::from_provider_message(
        "test".into(),
        None,
        "op".into(),
        redacted,
    )
    .unwrap();
    let serialized = serde_json::to_string(&entry).unwrap();
    assert!(!serialized.contains("provider-secret"));
    assert!(!serialized.contains(&encode_base64(b"prefix provider-secret suffix")));
    assert!(serialized.contains("[REDACTED]"));
    assert!(serialized.contains("[REDACTED attachment]"));
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
        tool_process_facts: Vec::new(),
        tool_job_outputs: Vec::new(),
        stop_message: None,
    }
}

struct DurableProviderExecutor {
    request: ProviderRequest,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
    event_sender: Option<SessionEventSender>,
    interaction_route: Option<InteractionRoute>,
    execution: Option<ProviderExecution>,
    journal: std::sync::Arc<std::sync::Mutex<ManualRunJournal>>,
}

impl DurableProviderExecutor {
    fn new(
        request: ProviderRequest,
        options: ProviderRunOptions,
        skill_instructions: Option<SkillInstructions>,
        event_sender: Option<SessionEventSender>,
        interaction_route: Option<InteractionRoute>,
        journal: std::sync::Arc<std::sync::Mutex<ManualRunJournal>>,
    ) -> Self {
        Self {
            request,
            options,
            skill_instructions,
            event_sender,
            interaction_route,
            execution: None,
            journal,
        }
    }
}

impl DurableProviderExecutor {
    async fn execute_async(&mut self) -> Result<ProviderResponse, ProviderError> {
        let options = self.options.clone();
        let mut execution = execute_provider_turn_with_local_lsp(
            self.request.clone(),
            TranscriptCapture::Durable(self.journal.clone()),
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
        let mut pending_tools = std::collections::BTreeSet::new();
        for event in &execution.events {
            match &event.kind {
                EventKind::ToolStarted {
                    batch_id, call_id, ..
                } => {
                    pending_tools.insert((batch_id, call_id));
                }
                EventKind::ToolFinished {
                    batch_id, call_id, ..
                } => {
                    pending_tools.remove(&(batch_id, call_id));
                }
                _ => {}
            }
        }
        let outcome = if !pending_tools.is_empty() {
            DurableOutcome::Unknown
        } else {
            match execution.result.code {
                ExitCode::Success => slim_core::session::DurableOutcome::Success,
                ExitCode::Cancelled => slim_core::session::DurableOutcome::Cancelled,
                _ => slim_core::session::DurableOutcome::Failed,
            }
        };
        let mut response =
            ProviderResponse::with_outcome(execution.result.text.clone(), usage, outcome.clone());
        response.transcript = std::mem::take(&mut execution.turn_transcript);
        response.task_facts = execution
            .task_facts
            .iter()
            .skip(self.options.task_facts.len())
            .cloned()
            .collect();
        if execution.result.stop == "provider_error" {
            // The durable writer prefers a nonempty transcript over content.
            // Keep this explicitly labelled runtime failure after completed tools.
            response.transcript.push(ProviderMessage::assistant(
                format!(
                    "[Run failed]\n{}",
                    execution
                        .result
                        .stop_message
                        .as_deref()
                        .unwrap_or(&execution.result.text)
                ),
                Vec::new(),
            ));
        }
        if execution.result.stop != "provider_error" {
            if let Some(message) = &execution.result.stop_message {
                response.transcript.push(ProviderMessage::assistant(
                    format!("{}\n{message}", run_notice_label(&execution.result)),
                    Vec::new(),
                ));
            }
        }
        // These terminal explanations are added by the CLI after the runtime's
        // incrementally persisted conversation.
        if let Some(message) = response.transcript.last().filter(|_| {
            execution.result.stop == "provider_error" || execution.result.stop_message.is_some()
        }) {
            self.journal
                .lock()
                .map_err(|_| resume_error("durable run lock poisoned"))?
                .record_message(message.clone())
                .map_err(|error| resume_error(error.to_string()))?;
        }
        let terminal = run_telemetry_terminal(&execution, outcome)?;
        self.journal
            .lock()
            .map_err(|_| resume_error("durable run lock poisoned"))?
            .set_run_telemetry_terminal(terminal)
            .map_err(|error| resume_error(error.to_string()))?;
        self.execution = Some(execution);
        Ok(response)
    }
}

fn classify_provider_error(error: &ProviderError) -> DurableErrorClass {
    match error {
        ProviderError::Transport { safe_to_retry, .. } => DurableErrorClass::Transport {
            safe_to_retry: *safe_to_retry,
        },
        ProviderError::Api { .. }
        | ProviderError::TransientRemote { .. }
        | ProviderError::Remote { .. }
        | ProviderError::Http { .. } => DurableErrorClass::Remote,
        ProviderError::InvalidResponse { .. } | ProviderError::MalformedToolCall => {
            DurableErrorClass::Invalid
        }
        ProviderError::Cancelled => DurableErrorClass::Cancelled,
    }
}

const MAX_RUN_TELEMETRY_ID_BYTES: usize = 256;

fn durable_run_telemetry_context(
    request: &ProviderRequest,
    options: &ProviderRunOptions,
) -> Result<RunTelemetryContext, ProviderError> {
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| ProviderError::InvalidResponse {
            message: format!("system clock precedes Unix epoch: {error}"),
        })?
        .as_millis()
        .try_into()
        .map_err(|_| ProviderError::InvalidResponse {
            message: "run telemetry timestamp overflowed".into(),
        })?;
    let experiment_id = validate_run_telemetry_id(options.experiment_id.as_deref(), "experiment")?;
    let task_id = validate_run_telemetry_id(options.task_id.as_deref(), "task")?;
    let executable_identity = executable_identity();
    Ok(RunTelemetryContext {
        experiment_id,
        task_id,
        mode: request.mode,
        provider: provider_kind_name(request.kind).into(),
        model: request.model.clone(),
        build_revision: option_env!("SLIM_BUILD_REVISION")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .into(),
        executable_sha256: executable_identity.sha256.clone(),
        executable_identity_error: executable_identity.error.clone(),
        started_at,
        limits: serde_json::json!({
            "resolved": false,
            "context_window_tokens": options.context_window_tokens,
            "max_output_tokens": options.max_output_tokens,
            "max_mutating_tool_calls": options.max_tool_calls,
            "max_read_tool_calls": options.max_read_tool_calls,
            "max_total_tool_calls": options.max_total_tool_calls,
            "max_turns": options.max_turns,
            "max_result_bytes": options.max_result_bytes,
        }),
    })
}

fn validate_run_telemetry_id(
    value: Option<&str>,
    field: &str,
) -> Result<Option<String>, ProviderError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty()
        || value.len() > MAX_RUN_TELEMETRY_ID_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidResponse {
            message: format!(
                "{field} telemetry id must be 1..={MAX_RUN_TELEMETRY_ID_BYTES} bytes without control characters"
            ),
        });
    }
    Ok(Some(value.to_owned()))
}

fn run_telemetry_terminal(
    execution: &ProviderExecution,
    outcome: DurableOutcome,
) -> Result<RunTelemetryTerminal, ProviderError> {
    let totals = &execution.result.usage;
    let total_input_tokens = totals
        .uncached_input_tokens
        .saturating_add(totals.cache_write_tokens)
        .saturating_add(totals.cache_read_tokens);
    let usage = serde_json::json!({
        "request_count": totals.requests.len(),
        "uncached_input_tokens": totals.uncached_input_tokens,
        "cache_write_tokens": totals.cache_write_tokens,
        "cache_read_tokens": totals.cache_read_tokens,
        "total_input_tokens": total_input_tokens,
        "output_tokens": totals.output_tokens,
        "reasoning_tokens": totals.reasoning_tokens,
        "total_tokens": total_input_tokens.saturating_add(totals.output_tokens),
        "usage_unknown": totals.usage_unknown,
        "system_bytes": totals.system_bytes,
        "tool_schema_bytes": totals.tool_schema_bytes,
        "history_bytes": totals.history_bytes,
        "tool_result_bytes": totals.tool_result_bytes,
        "provider_latency_ms": totals.provider_latency_ms,
        "tool_latency_ms": totals.tool_latency_ms,
        "retry_count": totals.retry_count,
        "cancelled_requests": totals.cancelled_requests,
        "provider_turns": totals.provider_turns,
        "tool_calls_executed": totals.tool_calls_executed,
        "tool_calls_reused": totals.tool_calls_reused,
        "tool_calls_suppressed": totals.tool_calls_suppressed,
        "no_progress_turns": totals.no_progress_turns,
        "no_progress_tokens": totals.no_progress_tokens,
        "duplicate_evidence_bytes_avoided": totals.duplicate_evidence_bytes_avoided,
        "compaction_input_tokens": totals.compaction_input_tokens,
        "compaction_output_tokens": totals.compaction_output_tokens,
        "compaction_tokens_saved": totals.compaction_tokens_saved,
        "post_compaction_reacquisitions": totals.post_compaction_reacquisitions,
        "estimation_error_tokens": totals.estimation_error_tokens,
        "validated_completion": totals.validated_completion,
        "overflowed": totals.overflowed,
    });
    let costs = serde_json::to_value(&execution.result.costs).map_err(|error| {
        ProviderError::InvalidResponse {
            message: format!("serialize durable run costs: {error}"),
        }
    })?;
    Ok(RunTelemetryTerminal {
        stop: execution.result.stop.clone(),
        outcome,
        validated_completion: execution.result.usage.validated_completion,
        validation_source: execution.result.validation_source.clone(),
        usage,
        costs,
        limits: serde_json::json!({
            "resolved": true,
            "context_window_tokens": execution.limits.context_window_tokens,
            "max_output_tokens": execution.limits.max_output_tokens,
            "max_mutating_tool_calls": execution.limits.max_mutating_tool_calls,
            "max_read_tool_calls": execution.limits.max_read_tool_calls,
            "max_total_tool_calls": execution.limits.max_total_tool_calls,
            "max_turns": execution.limits.max_turns,
            "max_result_bytes": execution.limits.max_result_bytes,
        }),
    })
}

fn provider_kind_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAiCompatible => "openai-compatible",
        ProviderKind::OpenAiCodex => "openai-codex",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenCodeGo => "opencode-go",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::ClinePass => "clinepass",
        ProviderKind::CommandCode => "command-code",
        ProviderKind::Xai => "xai",
    }
}

fn run_provider_headless_inner(
    request: ProviderRequest,
    session_path: Option<&Path>,
    options: ProviderRunOptions,
) -> Result<ProviderHeadlessResult, ProviderError> {
    execute_provider_turn(request, session_path, options).map(into_result_reporting_warnings)
}

pub(crate) fn execute_provider_turn(
    request: ProviderRequest,
    session_path: Option<&Path>,
    mut options: ProviderRunOptions,
) -> Result<ProviderExecution, ProviderError> {
    if request.prompt.trim().is_empty() {
        return Ok(empty_provider_execution(input_required_result(&request)));
    }
    if let Some(path) = session_path {
        let workspace = options
            .workspace_root
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .and_then(std::fs::canonicalize)
            .map_err(|error| resume_error(format!("workspace: {error}")))?;
        let cwd = workspace
            .to_str()
            .ok_or_else(|| resume_error("workspace path is not valid Unicode"))?;
        let id = format!("slim-{}-{}", std::process::id(), next_session_suffix());
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| resume_error("system clock is before the Unix epoch"))?
            .as_nanos()
            .to_string();
        let mut repo = JsonlRepo::create(
            path,
            DurableSessionHeader::new(&id, timestamp, cwd, None, None),
        )
        .map_err(|error| ProviderError::InvalidResponse {
            message: format!("session: {error}; use --resume for an existing session"),
        })?;
        let mut parent = None;
        let mut entries = Vec::new();
        for (index, message) in std::mem::take(&mut options.history).into_iter().enumerate() {
            let message = redact_durable_message(message, &durable_secrets(&request, &options));
            let entry_id = format!("{id}-history-{index}");
            entries.push(
                DurableEntry::from_provider_message(
                    entry_id.clone(),
                    parent,
                    format!("{id}-history"),
                    message,
                )
                .map_err(resume_error)?,
            );
            parent = Some(entry_id);
        }
        provider_messages_from_entries(&entries).map_err(resume_error)?;
        repo.append_batch(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, entry)| slim_core::session::DurableRecord::Entry {
                    seq: index as u64,
                    entry,
                })
                .collect(),
        )
        .map_err(|error| resume_error(error.to_string()))?;
        let preflight = SessionPreflight::from_open_repo(&repo);
        drop(repo);
        options.workspace_root = Some(workspace);
        return run_provider_resume_with_preflight_events(request, preflight, options, None);
    }
    block_on_provider(execute_provider_turn_with_local_lsp(
        request, false, options, None, None, None,
    ))
}

pub(crate) enum TranscriptCapture {
    None,
    Memory,
    Durable(std::sync::Arc<std::sync::Mutex<ManualRunJournal>>),
}

impl From<bool> for TranscriptCapture {
    fn from(capture: bool) -> Self {
        if capture {
            Self::Memory
        } else {
            Self::None
        }
    }
}

async fn execute_provider_turn_with_local_lsp(
    request: ProviderRequest,
    capture_transcript: impl Into<TranscriptCapture>,
    mut options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
    event_sender: Option<SessionEventSender>,
    interaction_route: Option<InteractionRoute>,
) -> Result<ProviderExecution, ProviderError> {
    let (local_code_intelligence, lsp_warnings) = attach_local_code_intelligence(&mut options);
    let (local_mcp, mut mcp_warnings) = attach_local_mcp(&mut options);
    mcp_warnings.splice(0..0, lsp_warnings);
    if let Some(manager) = &local_mcp {
        mcp_warnings
            .extend(crate::mcp::await_direct_startup(manager, options.cancellation.as_ref()).await);
    }
    let mut result = execute_provider_turn_async(
        request,
        capture_transcript,
        options,
        skill_instructions,
        event_sender,
        interaction_route,
    )
    .await;
    if let Some(manager) = local_code_intelligence {
        manager.shutdown().await;
    }
    if let Some(manager) = local_mcp {
        manager.disconnect_all().await;
    }
    if let Ok(execution) = result.as_mut() {
        execution.warnings.splice(0..0, mcp_warnings);
    }
    result
}

fn block_on_provider<T: Send>(
    future: impl std::future::Future<Output = Result<T, ProviderError>> + Send,
) -> Result<T, ProviderError> {
    provider_runtime_block_on(future)?
}

/// Stack for threads that poll an agent loop. Its future is large in debug
/// builds; the Windows main thread (1 MiB) and the Rust default (2 MiB) leave
/// little headroom.
pub(crate) const AGENT_LOOP_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Polls on a dedicated thread with [`AGENT_LOOP_STACK_BYTES`] instead of the
/// caller's stack; a panic in the future resumes on the caller.
pub(crate) fn provider_runtime_block_on<T: Send>(
    future: impl std::future::Future<Output = T> + Send,
) -> Result<T, ProviderError> {
    let runtime = shared_provider_runtime()?;
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("slim-provider".into())
            .stack_size(AGENT_LOOP_STACK_BYTES)
            .spawn_scoped(scope, || runtime.block_on(future))
            .map_err(|error| ProviderError::InvalidResponse {
                message: format!("provider thread: {error}"),
            })?;
        worker
            .join()
            .map_err(|panic| std::panic::resume_unwind(panic))
    })
}

/// Process-wide Tokio runtime for synchronous headless entry points.
/// Building one runtime per provider call paid thread-pool spawn on every
/// run; the runtime is thread-safe and `block_on` may be shared.
fn shared_provider_runtime() -> Result<&'static tokio::runtime::Runtime, ProviderError> {
    static PROVIDER_RUNTIME: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    match PROVIDER_RUNTIME
        .get_or_init(|| tokio::runtime::Runtime::new().map_err(|error| error.to_string()))
    {
        Ok(runtime) => Ok(runtime),
        Err(error) => Err(ProviderError::InvalidResponse {
            message: format!("runtime: {error}"),
        }),
    }
}

pub(crate) fn provider_runtime_handle() -> Result<tokio::runtime::Handle, ProviderError> {
    shared_provider_runtime().map(|runtime| runtime.handle().clone())
}

fn attach_local_code_intelligence(
    options: &mut ProviderRunOptions,
) -> (Option<CodeIntelligenceHandle>, Vec<String>) {
    if options.code_intelligence.is_some() {
        return (None, Vec::new());
    }
    let Ok(layered) = crate::config::load_layered() else {
        return (None, Vec::new());
    };
    // load_layered reads the project slim.toml from the process directory.
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let trust = crate::code_intel::project_trust(&layered.lsp, &workspace, options.trust_project);
    let warnings = trust
        .notice
        .map(|notice| {
            vec![format!(
                "{notice}. Pass --trust-project to apply them for this run."
            )]
        })
        .unwrap_or_default();
    let Some(manager) = crate::code_intel::build_code_intelligence(&layered.lsp, trust.trusted)
    else {
        return (None, warnings);
    };
    let handle = CodeIntelligenceHandle::new(manager);
    options.code_intelligence = Some(handle.clone());
    (Some(handle), warnings)
}

/// Connects the process-local MCP manager for a headless run. Configuration
/// problems never abort the run and never vanish: they come back as warnings
/// the caller reports (stderr for headless).
fn attach_local_mcp(
    options: &mut ProviderRunOptions,
) -> (Option<Arc<slim_core::mcp::McpManager>>, Vec<String>) {
    if options.mcp.is_some() {
        return (None, Vec::new());
    }
    let Some(cwd) = options
        .workspace_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
    else {
        return (
            None,
            vec!["MCP disabled: workspace directory unavailable".into()],
        );
    };
    // The project layer is the workspace's slim.toml, which is not the
    // process directory when a session resumes in its recorded workspace.
    let load = match crate::mcp::load_mcp(&cwd, options.trust_project) {
        Ok(load) => load,
        Err(error) => return (None, vec![format!("MCP disabled: config error: {error}")]),
    };
    let warnings =
        crate::mcp::load_diagnostics(&load, "Pass --trust-project to start them for this run.");
    let manager = crate::mcp::build_mcp_manager(&load, &cwd);
    if let Some(manager) = &manager {
        // Per run: enabled, trusted, non-lazy servers connect in the
        // background while the run is prepared.
        manager.start_background_connect();
        options.mcp = Some(McpHandle::new(Arc::clone(manager)));
    }
    (manager, warnings)
}

pub(crate) async fn execute_provider_turn_async(
    request: ProviderRequest,
    capture_transcript: impl Into<TranscriptCapture>,
    options: ProviderRunOptions,
    skill_instructions: Option<SkillInstructions>,
    event_sender: Option<SessionEventSender>,
    interaction_route: Option<slim_core::InteractionRoute>,
) -> Result<ProviderExecution, ProviderError> {
    if request.prompt.trim().is_empty() && !options.compact_only {
        return Ok(empty_provider_execution(input_required_result(&request)));
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
            tool_process_facts: Vec::new(),
            tool_job_outputs: Vec::new(),
            stop_message: None,
        }));
    }

    let open_code_spec =
        match request.kind {
            ProviderKind::OpenCodeGo => Some(open_code_model(&request.model).ok_or_else(|| {
                ProviderError::InvalidResponse {
                    message: format!("unsupported OpenCode Go model: {}", request.model),
                }
            })?),
            ProviderKind::OpenCodeZen => {
                Some(
                    zen_model(&request.model).ok_or_else(|| ProviderError::InvalidResponse {
                        message: format!("unsupported OpenCode Zen model: {}", request.model),
                    })?,
                )
            }
            _ => None,
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
    // Headless runs keep the Command Code cache warm for the next
    // invocation; the lookup itself only reads the on-disk snapshot.
    if request.kind == ProviderKind::CommandCode && catalog_override_absent {
        if let Ok(catalog) = CommandCodeCatalog::production() {
            tokio::spawn(async move {
                let _ = catalog.refresh().await;
            });
        }
    }
    let context_window_tokens = if catalog_override_absent {
        if let Some(model) = open_code_spec {
            require_model_context_window(model.context_window)?
        } else {
            require_model_context_window(known_model_context_window(
                request.kind,
                &request.model,
                live_codex.as_deref(),
            ))?
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
                message: if request.kind == ProviderKind::OpenCodeZen {
                    "OpenCode Zen context or output limit exceeds model metadata"
                } else {
                    "OpenCode Go context or output limit exceeds model metadata"
                }
                .into(),
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
    let max_mutating_tool_calls = resolve_max_mutating_tool_calls(&options)?;
    let max_read_tool_calls = resolve_max_read_tool_calls(&options)?;
    let max_total_tool_calls = resolve_max_total_tool_calls(&options)?;
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
    if let Some(jobs) = options.shell_jobs.clone() {
        runtime.set_session_shell_jobs(jobs);
    } else {
        runtime
            .set_shell_job_limits(options.shell_job_limits.clone())
            .map_err(|message| ProviderError::InvalidResponse { message })?;
    }
    if let Some(bytes) = parse_positive_env_usize("SLIM_READ_PRESENTATION_BYTES")
        .map_err(|message| ProviderError::InvalidResponse { message })?
    {
        runtime
            .set_read_presentation_bytes(bytes)
            .map_err(|message| ProviderError::InvalidResponse { message })?;
    }
    match capture_transcript.into() {
        TranscriptCapture::None => {}
        TranscriptCapture::Memory => runtime.capture_turn_transcript(),
        TranscriptCapture::Durable(journal) => {
            runtime.capture_turn_transcript();
            runtime.app.set_run_journal(journal);
        }
    }
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
    }
    if let Some(handle) = options.manual_retry.clone() {
        runtime.set_manual_retry_handle(handle);
    }
    runtime.register_sensitive_value(&request.api_key);
    runtime.restore_task_facts(&options.task_facts, &cwd)?;
    runtime.restore_artifact_ids(&options.artifact_ids);
    // Per-turn Runtime, application-scoped language-server pool.
    if let Some(code_intelligence) = options.code_intelligence.as_ref() {
        runtime.set_code_intelligence(code_intelligence.manager().clone());
    }
    if let Some(mcp) = options.mcp.as_ref() {
        for value in mcp.manager().sensitive_values() {
            runtime.register_sensitive_value(value);
        }
        runtime.set_mcp_manager(Some(mcp.manager().clone()));
    }
    let provider_timeouts = ProviderTimeouts::production(request.timeout);
    let mut loop_config = AgentLoopConfig {
        context_window_tokens,
        context_reserve_tokens: context_reserve_tokens(
            request.kind,
            max_output_tokens,
            max_output_tokens_is_explicit(options.max_output_tokens),
            options
                .compaction
                .as_ref()
                .map_or(CompactionPolicy::default().reserve_tokens, |handle| {
                    handle.policy().reserve_tokens
                }),
        ),
        ..AgentLoopConfig::default()
    };
    loop_config.max_turns = max_turns;
    loop_config.max_mutating_tool_calls = max_mutating_tool_calls;
    loop_config.max_read_tool_calls = max_read_tool_calls;
    loop_config.max_total_tool_calls = max_total_tool_calls;
    loop_config.max_result_bytes = max_result_bytes;
    let tool_limits = ToolLoopLimits {
        max_mutating_tool_calls: loop_config.max_mutating_tool_calls,
        max_read_tool_calls: loop_config.max_read_tool_calls,
        max_total_tool_calls: loop_config.max_total_tool_calls,
        max_turns: loop_config.max_turns,
        max_output_tokens,
        max_result_bytes: loop_config.max_result_bytes,
        context_window_tokens: loop_config.context_window_tokens,
    };
    let user_text = match skill_user_prefix.as_deref() {
        Some(prefix) => format!("{prefix}{}", request.prompt),
        None => request.prompt.clone(),
    };
    let compact_only = options.compact_only;
    let mut initial_messages = options.history;
    if !compact_only {
        let mut message =
            ProviderMessage::user(user_text).with_content_blocks(options.content_blocks);
        if skill_user_prefix.is_some() {
            // The journal records the prompt without the skill instructions.
            message = message.with_recorded_content(request.prompt.clone());
        }
        initial_messages.push(message);
    }
    let loop_result = match provider {
        ProviderKind::OpenAiCompatible => {
            let mut config =
                ProviderConfig::openai(request.endpoint, request.model, api_key.clone())
                    .with_max_output_tokens(max_output_tokens);
            if let Some(effort) = reasoning_effort.as_deref() {
                config = config.with_reasoning_effort(effort);
            }
            let adapter = OpenAiCompatibleAdapter::new(config)?;
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                OpenAiCompatibleAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
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
            let adapter = OpenAiCodexAdapter::new(config)?.with_fast_mode(options.codex_fast);
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                OpenAiCodexAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
        }
        ProviderKind::Anthropic => {
            let mut config = if request.account_id.is_some() {
                ProviderConfig::anthropic_oauth(request.endpoint, request.model, api_key.clone())
            } else {
                ProviderConfig::anthropic(request.endpoint, request.model, api_key.clone())
            };
            if let Some(effort) = reasoning_effort.as_deref() {
                config = config.with_reasoning_effort(effort);
            }
            let adapter = AnthropicAdapter::new(config.with_max_output_tokens(max_output_tokens))?;
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                AnthropicAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
        }
        ProviderKind::OpenCodeGo => {
            let adapter = OpenCodeGoAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?
            .with_max_output_tokens(max_output_tokens);
            let adapter = match options.provider_session_id.as_deref() {
                Some(id) => adapter.with_session_id(id),
                None => adapter,
            };
            adapter.validate_messages(&initial_messages)?;
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                OpenCodeGoAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
        }
        ProviderKind::OpenCodeZen => {
            let adapter = OpenCodeZenAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?
            .with_max_output_tokens(max_output_tokens);
            let adapter = match options.provider_session_id.as_deref() {
                Some(id) => adapter.with_session_id(id),
                None => adapter,
            };
            adapter.validate_messages(&initial_messages)?;
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                OpenCodeZenAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
        }
        ProviderKind::ClinePass => {
            let adapter = ClinePassAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?
            .with_max_output_tokens(max_output_tokens);
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                ClinePassAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
        }
        ProviderKind::CommandCode => {
            let adapter = CommandCodeAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?
            .with_max_output_tokens(max_output_tokens)
            .with_zero_data_retention(command_code_zero_data_retention());
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                CommandCodeAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
        }
        ProviderKind::Xai => {
            let adapter = XaiAdapter::new(
                &request.endpoint,
                &request.model,
                &api_key,
                reasoning_effort.as_deref(),
            )?
            .with_max_output_tokens(max_output_tokens);
            run_bound_agent_loop(
                &mut runtime,
                adapter,
                XaiAdapter::with_response_cache_scope_id,
                &initial_messages,
                request.mode,
                &cwd,
                loop_config,
                provider_timeouts,
                compact_only,
            )
            .await?
        }
    }
    .map_err(|error| redact_provider_error(error, &api_key));

    let events = runtime.app.drain_events();

    let mut text = String::new();
    let mut tool_text = Vec::new();
    let mut stop_reason = None;
    for event in &events {
        match &event.kind {
            EventKind::AssistantTextDelta { text: delta } => text.push_str(delta),
            EventKind::ToolOutput { name, output, .. }
            | EventKind::ToolJobOutput { name, output, .. } => {
                tool_text.push(format!("tool {name}: {output}"))
            }
            EventKind::AssistantEnded { reason } => stop_reason = Some(reason.clone()),
            _ => {}
        }
    }
    if text.is_empty() {
        text = tool_text.join("\n");
    }
    let has_partial_output = !text.trim().is_empty();
    let mut provider_failure = None;
    let (validated_completion, stop, code, tool_results) = match loop_result {
        Ok(loop_result) => (
            derive_validated_completion(loop_result.stop, &events),
            stop_name(loop_result.stop).to_owned(),
            exit_code_for_stop(loop_result.stop),
            loop_result.tool_results,
        ),
        Err(error) => {
            if matches!(
                &error,
                ProviderError::InvalidResponse { message }
                if message.contains("sensitive material")
            ) {
                return Err(error);
            }
            let (code, stop, message) = provider_failure_details(error);
            if !has_partial_output {
                text.clone_from(&message);
            }
            provider_failure = Some(message);
            (false, stop.to_owned(), code, Vec::new())
        }
    };
    let stop_message = if !matches!(code, ExitCode::Success | ExitCode::Cancelled) {
        let cause = if stop == "provider_error" {
            let failure = provider_failure.as_deref().unwrap_or(&text);
            if events.iter().rev().find_map(|event| match event.kind {
                EventKind::ContextSnapshot { request_kind, .. } => Some(request_kind),
                _ => None,
            }) == Some(slim_core::RequestKind::Compaction)
            {
                format!("Foreground compaction failed: {failure}")
            } else {
                failure.to_owned()
            }
        } else {
            format_run_stop_message(&stop, &tool_results, tool_limits)
        };
        Some(stopped_context(cause, &runtime, &events))
    } else if code == ExitCode::Success {
        pending_task_summary(&runtime)
    } else {
        None
    };
    if stop == "provider_error" && !has_partial_output {
        if let Some(message) = &stop_message {
            text.clone_from(message);
        }
    }
    let usage = UsageTotals::from_events(&events, validated_completion);
    let (input_tokens, output_tokens) = legacy_provider_usage(&usage);
    let costs = cost_summary_for_usage(&usage, resolve_pricing());
    let cost_micros = costs.total_micros;
    text = runtime.redact_sensitive(&text);
    let mut history = runtime.conversation().to_vec();
    if let Some(prefix) = skill_user_prefix.as_deref() {
        for msg in &mut history {
            if msg.role == "user" && msg.content.starts_with(prefix) {
                msg.content = msg.content[prefix.len()..].to_string();
            }
        }
    }
    let tool_summary_lines = summarize_tool_events(&events);
    let tool_process_facts = collect_tool_process_facts(&events);
    let tool_job_outputs = collect_tool_job_outputs(&events);
    // A failed automatic compaction does not fail the run, and its activity
    // label is replaced by the request that follows: the interface keeps it.
    let warnings = events
        .iter()
        .filter_map(|event| event.kind.auto_compaction_failure())
        .map(str::to_owned)
        .collect();
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
            tool_process_facts,
            tool_job_outputs,
            stop_message,
        },
        history: Some(history),
        turn_transcript: runtime.take_turn_transcript(),
        task_facts: runtime.task_facts(),
        events,
        tool_results,
        limits: tool_limits,
        resume_preflight: None,
        warnings,
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
mod skill_prompt_tests;

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

fn collect_tool_process_facts(events: &[SessionEvent]) -> Vec<ToolProcessFact> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolProcessFinished {
                batch_id,
                call_id,
                name,
                process,
            } => Some(ToolProcessFact {
                batch_id: batch_id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                process: process.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn collect_tool_job_outputs(events: &[SessionEvent]) -> Vec<ToolJobOutputFact> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolJobOutput {
                batch_id,
                call_id,
                name,
                output,
            } => Some(ToolJobOutputFact {
                batch_id: batch_id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                output: output.clone(),
            }),
            _ => None,
        })
        .collect()
}

struct FinishedToolSummary {
    batch_id: String,
    name: String,
    success: bool,
    duration_ms: u64,
    reason: String,
    process: Option<slim_core::process::ProcessExecutionFacts>,
}

fn summarize_tool_events(events: &[SessionEvent]) -> Vec<String> {
    // Last output/preview per call, so `✕` rows carry the same short reason
    // the TUI projects for a failed tool. Success rows never carry output
    // text (spec §15.1); empty call ids are never correlated.
    let mut reasons = std::collections::HashMap::<&str, &str>::new();
    let mut processes =
        std::collections::HashMap::<(&str, &str), &slim_core::process::ProcessExecutionFacts>::new(
        );
    for event in events {
        match &event.kind {
            EventKind::ToolOutput {
                call_id, output, ..
            }
            | EventKind::ToolJobOutput {
                call_id, output, ..
            } if !call_id.is_empty() => {
                reasons.insert(call_id.as_str(), output.as_str());
            }
            EventKind::ToolProgress {
                call_id, preview, ..
            } if !call_id.is_empty() => {
                reasons.insert(call_id.as_str(), preview.as_str());
            }
            EventKind::ToolProcessFinished {
                batch_id,
                call_id,
                process,
                ..
            } if !batch_id.is_empty() && !call_id.is_empty() => {
                processes.insert((batch_id.as_str(), call_id.as_str()), process);
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
                Some(FinishedToolSummary {
                    batch_id: batch_id.clone(),
                    name: sanitize_timeline_name(name),
                    success: *success,
                    duration_ms: *duration_ms,
                    reason,
                    process: processes
                        .get(&(batch_id.as_str(), call_id.as_str()))
                        .map(|process| (*process).clone()),
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
                    "✓ {} tools · {}{} · {duration_ms}ms",
                    members.len(),
                    summarize_timeline_names(members.iter().map(|member| member.name.as_str())),
                    process_timeline_suffix(members),
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
            "{status} {}{failure}{reason}{} · {}ms",
            tool.name,
            process_timeline_suffix(std::slice::from_ref(tool)),
            tool.duration_ms,
        ));
        index += 1;
    }
    lines
}

fn process_timeline_suffix(tools: &[FinishedToolSummary]) -> String {
    let statuses = tools
        .iter()
        .filter_map(|tool| tool.process.as_ref())
        .map(process_status_text)
        .collect::<Vec<_>>();
    if statuses.is_empty() {
        String::new()
    } else {
        format!(" · process: {}", statuses.join("; "))
    }
}

fn process_status_text(process: &slim_core::process::ProcessExecutionFacts) -> String {
    let mut parts = vec![format!(
        "exit {}",
        process
            .exit_code
            .map_or_else(|| "n/a".to_owned(), |code| code.to_string())
    )];
    if process.timed_out {
        parts.push("timed out".into());
    }
    if process.cancelled {
        parts.push("cancelled".into());
    }
    if process.capture_may_be_incomplete {
        parts.push("capture may be incomplete".into());
    }
    let discarded = process
        .stdout_discarded_bytes
        .saturating_add(process.stderr_discarded_bytes);
    if discarded > 0 {
        parts.push(format!("discarded {discarded} B"));
    }
    parts.join(" · ")
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
mod durable_checkpoint_tests;

#[cfg(test)]
mod tool_timeline_tests;

#[cfg(test)]
mod validation_derivation_tests;

#[derive(Clone, Copy)]
struct UsagePricing {
    provider: ProviderPricing,
    cache_write_micros_per_million: Option<u64>,
    cache_read_micros_per_million: Option<u64>,
}

fn cost_summary_for_usage(usage: &UsageTotals, pricing: Option<UsagePricing>) -> UsageCostSummary {
    let usage_unknown = usage.usage_unknown;
    let Some(pricing) = pricing else {
        return UsageCostSummary {
            usage_unknown,
            pricing_unknown: !usage.requests.is_empty(),
            ..UsageCostSummary::default()
        };
    };
    let total_micros = if !usage.overflowed
        && !usage_unknown
        && usage.requests.iter().all(|request| !request.usage_unknown)
        && !usage.requests.is_empty()
    {
        sum_request_weighted_costs(usage.requests.iter(), pricing).and_then(weighted_cost_micros)
    } else {
        None
    };
    let failed_attempts_micros = if !usage.overflowed {
        sum_request_weighted_costs(
            usage
                .requests
                .iter()
                .filter(|request| request.failed && !request.cancelled),
            pricing,
        )
        .and_then(weighted_cost_micros)
    } else {
        None
    };
    let compaction_micros = if !usage.overflowed && !usage_unknown {
        sum_request_weighted_costs(
            usage
                .requests
                .iter()
                .filter(|request| request.request_kind == RequestKind::Compaction),
            pricing,
        )
        .and_then(weighted_cost_micros)
    } else {
        None
    };
    let cancelled_estimated_micros = if !usage.overflowed {
        sum_cancelled_estimated_costs(
            usage.requests.iter().filter(|request| request.cancelled),
            pricing,
        )
    } else {
        None
    };
    UsageCostSummary {
        total_micros,
        cost_per_validated_completion_micros: usage
            .validated_completion
            .then_some(total_micros)
            .flatten(),
        failed_attempts_micros,
        compaction_micros,
        cancelled_estimated_micros,
        usage_unknown,
        pricing_unknown: false,
    }
}

fn sum_request_weighted_costs<'a>(
    mut requests: impl Iterator<Item = &'a slim_core::RequestUsage>,
    pricing: UsagePricing,
) -> Option<u128> {
    requests.try_fold(0_u128, |total, request| {
        if request.usage_unknown {
            return None;
        }
        request_weighted_cost(request, pricing).and_then(|cost| total.checked_add(cost))
    })
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
mod usage_cost_tests;

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
    stop_message: Option<&'a str>,
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
    #[serde(skip_serializing_if = "slice_is_empty")]
    tool_process_facts: &'a [ToolProcessFact],
    #[serde(skip_serializing_if = "slice_is_empty")]
    tool_job_outputs: &'a [ToolJobOutputFact],
}

fn slice_is_empty<T>(slice: &[T]) -> bool {
    slice.is_empty()
}

#[derive(Serialize)]
struct ProviderUsageJson<'a> {
    #[serde(flatten)]
    totals: &'a UsageTotals,
    compaction_tokens_saved_estimated: bool,
}

pub fn render_provider_text(result: &ProviderHeadlessResult) -> String {
    let label = run_notice_label(result);
    match result
        .stop_message
        .as_deref()
        .filter(|message| *message != result.text)
    {
        Some(message) if result.text.trim().is_empty() => format!("{label}\n{message}\n"),
        Some(message) => format!("{}\n\n{label}\n{message}\n", result.text),
        None => format!("{}\n", result.text),
    }
}

fn run_notice_label(result: &ProviderHeadlessResult) -> &'static str {
    if result.code == ExitCode::Success {
        "[Run ended]"
    } else {
        "[Run stopped]"
    }
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
            "cost total={} per_validated_completion={} failed_attempts={} compaction={} cancelled_estimated={} usage_unknown={} pricing_unknown={}\n"
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
        result.costs.usage_unknown,
        result.costs.pricing_unknown,
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
    let provider = provider_kind_name(result.provider);
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
        stop_message: result.stop_message.as_deref(),
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
        tool_process_facts: &result.tool_process_facts,
        tool_job_outputs: &result.tool_job_outputs,
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
        require_model_context_window,
    )
}

fn require_model_context_window(value: Option<u64>) -> Result<u64, ProviderError> {
    value.filter(|value| *value > 0).ok_or_else(|| ProviderError::InvalidResponse {
        message: "Model context window is unknown. Set SLIM_CONTEXT_WINDOW_TOKENS to the provider's documented limit (or supply context_window_tokens explicitly).".into(),
    })
}

/// Resolves a Command Code model's context window from the catalog's disk
/// cache first (so live-only models get their real `context_length`), then
/// the static fallback registry. Never performs network I/O.
fn command_code_context_window(model: &str) -> Option<u64> {
    let cached = CommandCodeCatalog::production()
        .ok()
        .map(|catalog| catalog.load_or_fallback())
        .and_then(|snapshot| {
            snapshot
                .models
                .iter()
                .find(|entry| entry.id == model)
                .map(|entry| entry.context_window)
        });
    cached.or_else(|| command_code_model(model).map(|model| model.context_window))
}

fn known_model_context_window(
    kind: ProviderKind,
    model: &str,
    live_codex: Option<&[slim_core::provider::CodexCatalogEntry]>,
) -> Option<u64> {
    match kind {
        ProviderKind::OpenAiCodex => live_codex
            .and_then(|entries| entries.iter().find(|entry| entry.slug == model))
            .map(|entry| entry.context_window)
            .or_else(|| slim_core::provider::codex_model(model).map(|entry| entry.context_window)),
        ProviderKind::ClinePass => clinepass_model(model).map(|model| model.context_window),
        ProviderKind::CommandCode => command_code_context_window(model),
        ProviderKind::Xai => xai_model(model).map(|model| model.context_window),
        ProviderKind::Anthropic => command_code_model(model).map(|model| model.context_window),
        ProviderKind::OpenAiCompatible | ProviderKind::OpenCodeGo | ProviderKind::OpenCodeZen => {
            None
        }
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
            None => return Ok(catalog_default_max_output_tokens(kind, model)),
        }
    };

    if is_explicit
        && known_model_max_output_tokens(kind, model).is_some_and(|limit| value > limit as u64)
    {
        return Err(ProviderError::InvalidResponse {
            message: match kind {
                ProviderKind::OpenCodeGo => {
                    "OpenCode Go context or output limit exceeds model metadata"
                }
                ProviderKind::OpenCodeZen => {
                    "OpenCode Zen context or output limit exceeds model metadata"
                }
                _ => "max output tokens exceeds model metadata",
            }
            .into(),
        });
    }
    u32::try_from(value).map_err(|_| ProviderError::InvalidResponse {
        message: "SLIM_MAX_OUTPUT_TOKENS must fit in a positive 32-bit integer".into(),
    })
}

/// Whether the output limit came from the user (options, `slim.toml` or
/// `SLIM_MAX_OUTPUT_TOKENS`) rather than from the catalog default.
fn max_output_tokens_is_explicit(option: Option<u32>) -> bool {
    option.is_some() || std::env::var_os("SLIM_MAX_OUTPUT_TOKENS").is_some()
}

/// What the context gate keeps free for the answer. An output limit that goes
/// on the wire bounds the answer, so the input plus that limit must fit the
/// window. A catalog default limit the adapter never sends (Codex Responses has
/// no output field) bounds nothing: reserving all of it, 128k of a 272k window,
/// would compact at about half the window for no reason, so it reserves no more
/// than compaction's own reserve. An explicit limit keeps its meaning.
fn context_reserve_tokens(
    kind: ProviderKind,
    max_output_tokens: u32,
    explicit: bool,
    compaction_reserve_tokens: u64,
) -> u64 {
    let output = u64::from(max_output_tokens);
    if explicit || kind != ProviderKind::OpenAiCodex {
        output
    } else {
        output.min(compaction_reserve_tokens)
    }
}

fn catalog_default_max_output_tokens(kind: ProviderKind, model: &str) -> u32 {
    let Some(catalog) = known_model_max_output_tokens(kind, model) else {
        return DEFAULT_MAX_OUTPUT_TOKENS;
    };
    let window = match kind {
        ProviderKind::OpenCodeGo => open_code_model(model)
            .and_then(|model| model.context_window)
            .unwrap_or(AgentLoopConfig::default().context_window_tokens),
        ProviderKind::OpenCodeZen => zen_model(model)
            .and_then(|model| model.context_window)
            .unwrap_or(AgentLoopConfig::default().context_window_tokens),
        _ => known_model_context_window(kind, model, None)
            .unwrap_or(AgentLoopConfig::default().context_window_tokens),
    };
    let usable = window
        .saturating_sub(8_192)
        .min(window / 2)
        .min(u64::from(u32::MAX));
    let usable = u32::try_from(usable).unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    catalog.min(usable.max(DEFAULT_MAX_OUTPUT_TOKENS.min(catalog)))
}

fn known_model_max_output_tokens(kind: ProviderKind, model: &str) -> Option<u32> {
    match kind {
        ProviderKind::OpenAiCodex => codex_model(model).map(|model| model.max_output_tokens),
        ProviderKind::ClinePass => clinepass_model(model).map(|model| model.max_output_tokens),
        ProviderKind::OpenCodeGo => {
            open_code_model(model).and_then(|model| model.max_output_tokens)
        }
        ProviderKind::OpenCodeZen => zen_model(model).and_then(|model| model.max_output_tokens),
        ProviderKind::Xai => xai_model(model).map(|model| model.max_output_tokens),
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
const MAX_TOTAL_TOOL_CALLS_HARD_CAP: usize = 2048;
const MAX_TURNS_HARD_CAP: usize = 1024;

fn clamp_mutating_tool_calls(value: usize) -> usize {
    value.min(MAX_MUTATING_TOOL_CALLS_HARD_CAP)
}

fn clamp_read_tool_calls(value: usize) -> usize {
    value.min(MAX_READ_TOOL_CALLS_HARD_CAP)
}

fn clamp_total_tool_calls(value: usize) -> usize {
    value.min(MAX_TOTAL_TOOL_CALLS_HARD_CAP)
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

pub(crate) fn resolve_max_total_tool_calls(
    options: &ProviderRunOptions,
) -> Result<usize, ProviderError> {
    if let Some(value) = options.max_total_tool_calls {
        return Ok(clamp_total_tool_calls(value));
    }
    parse_positive_env_usize("SLIM_MAX_TOTAL_TOOL_CALLS")
        .map_err(|message| ProviderError::InvalidResponse { message })
        .map(|value| {
            value
                .map(clamp_total_tool_calls)
                .unwrap_or(AgentLoopConfig::DEFAULT_MAX_TOTAL_TOOL_CALLS)
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

/// `SLIM_CMD_ZDR`/`CMD_ZDR` opt into Command Code zero-data retention
/// (`x-cmd-zdr: 1`). Accepts 1/true/yes/on; anything else counts as unset so
/// a typo cannot silently widen retention — the header is only sent when the
/// value is explicitly affirmative.
fn command_code_zero_data_retention() -> bool {
    ["SLIM_CMD_ZDR", "CMD_ZDR"]
        .iter()
        .filter_map(std::env::var_os)
        .any(|value| {
            matches!(
                value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

fn empty_provider_execution(result: ProviderHeadlessResult) -> ProviderExecution {
    ProviderExecution {
        result,
        history: None,
        turn_transcript: Vec::new(),
        task_facts: Vec::new(),
        events: Vec::new(),
        tool_results: Vec::new(),
        limits: ToolLoopLimits {
            max_mutating_tool_calls: AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            max_read_tool_calls: AgentLoopConfig::DEFAULT_MAX_READ_TOOL_CALLS,
            max_total_tool_calls: AgentLoopConfig::DEFAULT_MAX_TOTAL_TOOL_CALLS,
            max_turns: AgentLoopConfig::DEFAULT_MAX_TURNS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            max_result_bytes: AgentLoopConfig::default().max_result_bytes,
            context_window_tokens: AgentLoopConfig::default().context_window_tokens,
        },
        resume_preflight: None,
        warnings: Vec::new(),
    }
}

fn pending_task_summary(runtime: &Runtime) -> Option<String> {
    let pending = runtime
        .todo_items()
        .into_iter()
        .filter(|item| matches!(item.status.as_str(), "pending" | "in_progress" | "blocked"))
        .collect::<Vec<_>>();
    if pending.is_empty() {
        None
    } else {
        let mut message = String::from("Pending tasks:");
        for item in pending.iter().take(8) {
            message.push_str(&format!(
                "\n- [{}] {}",
                item.status,
                runtime
                    .redact_sensitive(&item.title)
                    .chars()
                    .take(160)
                    .collect::<String>()
            ));
        }
        if pending.len() > 8 {
            message.push_str(&format!("\n- {} more saved tasks", pending.len() - 8));
        }
        Some(message)
    }
}

fn stopped_context(mut message: String, runtime: &Runtime, events: &[SessionEvent]) -> String {
    if let Some(error) = runtime.finalization_error() {
        message.push_str(&format!(
            "\nFinal response failed: {}",
            provider_failure_details(error.clone()).2
        ));
    }
    message.push_str("\nTask remains pending verification; this run did not confirm completion.");
    if let Some(pending) = pending_task_summary(runtime) {
        message.push('\n');
        message.push_str(&pending);
    }
    let mut unconfirmed = std::collections::BTreeMap::new();
    for event in events {
        match &event.kind {
            EventKind::ToolStarted {
                batch_id,
                call_id,
                name,
                ..
            } => {
                unconfirmed.insert((batch_id, call_id), name);
            }
            EventKind::ToolFinished {
                batch_id, call_id, ..
            } => {
                unconfirmed.remove(&(batch_id, call_id));
            }
            _ => {}
        }
    }
    if !unconfirmed.is_empty() {
        let names = unconfirmed
            .values()
            .take(8)
            .map(|name| {
                runtime
                    .redact_sensitive(name)
                    .chars()
                    .take(64)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(", ");
        message.push_str(&format!("\nTool execution remains unconfirmed: {names}. Check effects before repeating these operations."));
    }
    if let Some((batch_id, call_id, name, false)) =
        events.iter().rev().find_map(|event| match &event.kind {
            EventKind::ToolFinished {
                batch_id,
                call_id,
                name,
                success,
                ..
            } => Some((batch_id, call_id, name, *success)),
            _ => None,
        })
    {
        if let Some(output) = events.iter().rev().find_map(|event| match &event.kind {
            EventKind::ToolOutput {
                batch_id: batch,
                call_id: call,
                output,
                ..
            }
            | EventKind::ToolJobOutput {
                batch_id: batch,
                call_id: call,
                output,
                ..
            } if batch == batch_id && call == call_id => Some(output),
            _ => None,
        }) {
            message.push_str(&format!(
                "\nLast tool failure ({name}): {}",
                runtime
                    .redact_sensitive(output)
                    .chars()
                    .take(512)
                    .collect::<String>()
            ));
        }
    }
    runtime.redact_sensitive(&message)
}

pub(crate) fn format_run_stop_message(
    stop: &str,
    tool_results: &[slim_core::tools::ToolResult],
    limits: ToolLoopLimits,
) -> String {
    match stop {
        "tool_limit" if limits.max_read_tool_calls == 0 && limits.max_mutating_tool_calls == 0 => {
            "Configured tool budgets are zero. Increase the tool limits to continue with tools."
                .into()
        }
        "tool_limit" => {
            let (read_used, mutating_used) = count_tool_results_by_bucket(tool_results);
            let total_used = read_used + mutating_used;
            let breakdown = summarize_tool_results(tool_results);
            if total_used >= limits.max_total_tool_calls {
                if breakdown.is_empty() {
                    format!(
                        "Total tool budget exhausted ({total_used}/{}). Send a follow-up to continue.",
                        limits.max_total_tool_calls
                    )
                } else {
                    format!(
                        "Total tool budget exhausted ({total_used}/{}): {breakdown}. Send a follow-up to continue.",
                        limits.max_total_tool_calls
                    )
                }
            } else if breakdown.is_empty() {
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
            "Output truncated (initial max_output_tokens={}). Automatic recovery is bounded by retry, turn, model and context limits. Progress is preserved. Increase max_output_tokens within the model limit or request a smaller next step before continuing.",
            limits.max_output_tokens
        ),
        "repeated_failed_tool" => "Repeated failed tool blocked.".into(),
        "provider_filtered" => "Provider response was filtered before completion.".into(),
        "no_progress" => {
            "Stopped: no progress in recent turns. Send a follow-up to continue.".into()
        }
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
        AgentLoopStop::NoProgress => "no_progress",
        AgentLoopStop::Cancelled => "cancelled",
    }
}

fn exit_code_for_stop(stop: AgentLoopStop) -> ExitCode {
    match stop {
        AgentLoopStop::ProviderCompleted => ExitCode::Success,
        AgentLoopStop::ProviderTruncated | AgentLoopStop::ProviderFiltered => ExitCode::Provider,
        AgentLoopStop::TurnLimit
        | AgentLoopStop::RepeatedFailedTool
        | AgentLoopStop::NoProgress => ExitCode::Blocked,
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
        ProviderError::Transport { message, .. } => (
            ExitCode::Provider,
            "provider_error",
            format!("provider transport failed: {message}"),
        ),
        ProviderError::MalformedToolCall => (
            ExitCode::Provider,
            "provider_error",
            "provider returned a malformed tool call".into(),
        ),
        ProviderError::TransientRemote { message }
        | ProviderError::Remote { message }
        | ProviderError::Api { message, .. }
        | ProviderError::Http { message, .. }
        | ProviderError::InvalidResponse { message } => (
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
        ProviderError::Api {
            mut metadata,
            message,
        } => {
            for value in [
                &mut metadata.code,
                &mut metadata.error_type,
                &mut metadata.detail_code,
            ]
            .into_iter()
            .flatten()
            {
                if !secret.is_empty() {
                    *value = value.replace(secret, "[REDACTED]");
                }
            }
            ProviderError::Api {
                metadata,
                message: redact(message),
            }
        }
        ProviderError::Transport {
            safe_to_retry,
            message,
        } => ProviderError::Transport {
            safe_to_retry,
            message: redact(message),
        },
        ProviderError::Remote { message } => ProviderError::Remote {
            message: redact(message),
        },
        ProviderError::TransientRemote { message } => ProviderError::TransientRemote {
            message: redact(message),
        },
        ProviderError::Http {
            status,
            retry_after,
            message,
        } => ProviderError::Http {
            status,
            retry_after,
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
mod stop_message_tests;

#[cfg(test)]
mod run_telemetry_tests;

#[cfg(test)]
mod resolve_budget_tests;

#[cfg(test)]
mod plan_loop_tests;

#[cfg(test)]
mod compaction_resume_tests;

#[cfg(test)]
mod resume_preflight_transport_tests;

#[cfg(test)]
mod live_history_resume_tests;

#[cfg(test)]
mod mcp_attach_tests;

#[cfg(test)]
mod provider_thread_tests;
