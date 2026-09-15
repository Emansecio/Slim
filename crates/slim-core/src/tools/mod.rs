mod code_intel;
mod execution;
mod list;
mod patch;
mod read;
mod search;
mod shell;
mod write;

use crate::context::ArtifactHandle;
use crate::process::{
    ExecutableResolver, ProcessExecutionFacts, ProcessOutputBudget, ProcessRunner,
};
use crate::runtime::CancellationToken;
use crate::OperatingMode;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

pub(crate) use execution::{
    canonical_workspace, digest_bytes, path_identity, present_unstructured, CodeIntelPresentation,
    DependencyKind, DependencyObservation, FastStamp, MutationObservation, PreparedToolArguments,
    PreparedToolInvocation, ToolExecutionError, ToolExecutionOutcome, ToolExecutionReceipt,
    ToolPresentationSource,
};

pub(crate) use code_intel::presentation_for_code_intel;
pub use code_intel::{
    code_intel_definition, parse_code_intel_request, render_code_intel, CodeIntelRequest,
    CODE_INTEL_ACTIONS,
};
pub use list::{list_directory, DEFAULT_MAX_ENTRIES, MAX_ENTRIES_CAP};
pub use patch::apply_exact_patch;
pub use read::{read_file, read_file_range, DEFAULT_MAX_READ_LINES, MAX_READ_LINES_CAP};
pub use search::{
    format_search_page, search_bounded, search_literal, SearchHit, SearchOptions, SearchPage,
    DEFAULT_MAX_HITS, MAX_HITS_CAP,
};
pub(crate) use search::{MAX_SEARCH_PATTERNS, SKIP_DIR_NAMES};
pub use shell::{
    run_shell, run_shell_timeout, run_shell_timeout_cancellable,
    run_shell_timeout_cancellable_with_progress, ShellProgress, TimedShellOutput,
};
pub use write::{write_file, FilePrecondition, MAX_MUTATING_FILE_BYTES};

const ARGUMENT_SUMMARY_LIMIT: usize = 48;
const MAX_EVIDENCE_CACHE: usize = 64;
const PREFERRED_ARGUMENT_KEYS: &[&str] = &[
    "path", "command", "query", "pattern", "glob", "url", "file", "target", "name", "id",
];
const MAX_SHELL_TIMEOUT_MS: u64 = 120_000;

/// One-line tool argument for the TUI: preferred keys, never raw JSON dumps.
pub fn summarize_tool_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return bound_argument_summary(trimmed);
    };
    match value {
        Value::Object(map) if map.is_empty() => String::new(),
        Value::Array(items) if items.is_empty() => String::new(),
        Value::Object(map) => {
            if let Some(summary) = summarize_todos(&map) {
                return bound_argument_summary(&summary);
            }
            for key in PREFERRED_ARGUMENT_KEYS {
                if let Some(text) = map.get(*key).and_then(json_scalar) {
                    return bound_argument_summary(&format!("{key}={text}"));
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

pub fn summarize_tool_arguments_for(name: &str, arguments: &str) -> String {
    if name == "mcp" {
        let Ok(value) = serde_json::from_str::<Value>(arguments) else {
            return summarize_tool_arguments(arguments);
        };
        return match (
            value.get("server").and_then(Value::as_str),
            value.get("tool").and_then(Value::as_str),
        ) {
            (Some(server), Some(tool)) => bound_argument_summary(&format!("{server}.{tool}")),
            (Some(server), None) => bound_argument_summary(server),
            _ => summarize_tool_arguments(arguments),
        };
    }
    if name != "shell" {
        return summarize_tool_arguments(arguments);
    }
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return summarize_tool_arguments(arguments);
    };
    let Some(command) = value.get("command").and_then(Value::as_str) else {
        return summarize_tool_arguments(arguments);
    };
    let timeout_ms = match value.get("timeout_ms") {
        None => 30_000,
        Some(value) => {
            let Some(timeout_ms) = value
                .as_u64()
                .filter(|timeout_ms| (1..=MAX_SHELL_TIMEOUT_MS).contains(timeout_ms))
            else {
                return summarize_tool_arguments(arguments);
            };
            timeout_ms
        }
    };
    let limit = if timeout_ms.is_multiple_of(1_000) {
        format!("{}s", timeout_ms / 1_000)
    } else {
        format!("{timeout_ms}ms")
    };
    let suffix = format!(" · limit {limit}");
    let available = ARGUMENT_SUMMARY_LIMIT.saturating_sub(suffix.chars().count());
    let invocation = if let Some(args) = value.get("args").and_then(Value::as_array) {
        let args = args
            .iter()
            .filter_map(Value::as_str)
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("program={command} {args}")
    } else {
        format!("command={command}")
    };
    let command = bound_argument_summary_exact(&invocation, available);
    format!("{command}{suffix}")
}

fn bound_argument_summary_exact(text: &str, limit: usize) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut chars = first.chars();
    let mut out = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() && limit > 0 {
        out.pop();
        out.push('…');
    }
    out
}

fn summarize_todos(map: &serde_json::Map<String, Value>) -> Option<String> {
    let todos = map.get("todos")?.as_array()?;
    let chosen = todos
        .iter()
        .find(|item| item.get("status").and_then(Value::as_str) == Some("in_progress"))
        .or_else(|| todos.first())?;
    for key in ["content", "title", "id"] {
        if let Some(text) = chosen
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            return Some(text.to_owned());
        }
    }
    None
}

fn json_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn bound_argument_summary(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut chars = first.chars();
    let mut out: String = chars.by_ref().take(ARGUMENT_SUMMARY_LIMIT).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

#[derive(Debug, Eq, PartialEq)]
pub enum ToolError {
    Io { message: String },
    Cancelled,
    StaleRead { path: String },
    PreconditionRequired { path: String },
    MatchCount { count: usize },
    InvalidInput { message: String },
}

impl From<std::io::Error> for ToolError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ToolEffectClass {
    SnapshotRead,
    WorkspaceMutation,
    Validation,
    Interaction,
    InternalState,
    PotentiallyVolatile,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ToolCacheability {
    None,
    Evidence,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ToolVolatility {
    Stable,
    ObservedWorkspace,
    ExternalInput,
    Internal,
    Volatile,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ToolDependencyScope {
    TargetFile,
    ImmediateDirectory,
    ObservedWorkspace,
    Interaction,
    Internal,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ToolReplayPolicy {
    Never,
    EquivalentEvidence,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ToolOperationalSpec {
    pub(crate) effect_class: ToolEffectClass,
    pub(crate) cacheability: ToolCacheability,
    pub(crate) volatility: ToolVolatility,
    pub(crate) dependency_scope: ToolDependencyScope,
    pub(crate) replay_policy: ToolReplayPolicy,
}

impl ToolOperationalSpec {
    fn mutates_workspace(self) -> bool {
        matches!(
            self.effect_class,
            ToolEffectClass::WorkspaceMutation | ToolEffectClass::PotentiallyVolatile
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolSpec {
    name: &'static str,
    effect_class: ToolEffectClass,
    cacheability: ToolCacheability,
    volatility: ToolVolatility,
    dependency_scope: ToolDependencyScope,
    replay_policy: ToolReplayPolicy,
}

impl ToolSpec {
    fn operational(self) -> ToolOperationalSpec {
        ToolOperationalSpec {
            effect_class: self.effect_class,
            cacheability: self.cacheability,
            volatility: self.volatility,
            dependency_scope: self.dependency_scope,
            replay_policy: self.replay_policy,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolRegistry {
    specs: &'static [ToolSpec],
    services: Arc<ToolServices>,
}

#[derive(Clone, Debug)]
struct CachedEvidence {
    fingerprint: String,
    outcome: Arc<ToolExecutionOutcome>,
}

#[derive(Debug)]
struct ToolServices {
    process_runner: ProcessRunner,
    read: read::ReadService,
    list: list::ListService,
    search: search::SearchService,
    workspace_revision: AtomicU64,
    evidence_cache: Mutex<VecDeque<CachedEvidence>>,
}

impl ToolServices {
    fn new(process_runner: ProcessRunner) -> Self {
        Self {
            search: search::SearchService::default(),
            process_runner,
            read: read::ReadService::default(),
            list: list::ListService::default(),
            workspace_revision: AtomicU64::new(0),
            evidence_cache: Mutex::new(VecDeque::new()),
        }
    }
}

fn lock_mutex<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn evidence_reusable(prepared: &PreparedToolInvocation) -> bool {
    prepared.reusable_evidence()
}

fn reused_evidence(mut outcome: ToolExecutionOutcome) -> ToolExecutionOutcome {
    outcome.receipt.execution_us = 0;
    outcome.receipt.finalization_us = 0;
    outcome
}

const ADMISSION_OUTPUT_PREFIX_BYTES: usize = 512;

fn with_admission_feedback(
    mut outcome: ToolExecutionOutcome,
    admission_notes: &[String],
) -> ToolExecutionOutcome {
    let Some(prefix) = admission_output_prefix(admission_notes) else {
        return outcome;
    };
    if let Some(source) = outcome.receipt.presentation.take() {
        outcome.receipt.presentation = Some(source.with_prefix(prefix.clone()));
    }
    outcome.result.output.insert_str(0, &prefix);
    outcome
}

pub(crate) fn admission_output_prefix(admission_notes: &[String]) -> Option<String> {
    if admission_notes.is_empty() {
        return None;
    }
    let mut body = String::new();
    for note in admission_notes {
        let note = note.replace(['\r', '\n'], " ");
        if note.is_empty() {
            continue;
        }
        if !body.is_empty() {
            body.push_str("; ");
        }
        body.push_str(&note);
    }
    if body.is_empty() {
        return None;
    }
    let marker = "[admission: ";
    let suffix = "]\n";
    let available = ADMISSION_OUTPUT_PREFIX_BYTES
        .saturating_sub(marker.len())
        .saturating_sub(suffix.len());
    let mut bounded = String::new();
    let mut consumed = 0usize;
    let mut truncated = false;
    for character in body.chars() {
        let width = character.len_utf8();
        if consumed.saturating_add(width) > available {
            truncated = true;
            break;
        }
        bounded.push(character);
        consumed = consumed.saturating_add(width);
    }
    if truncated {
        while bounded.len() > available.saturating_sub("…".len()) {
            bounded.pop();
        }
        bounded.push('…');
    }
    Some(format!("{marker}{bounded}{suffix}"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResult {
    pub name: String,
    pub success: bool,
    pub output: String,
    pub artifact: Option<ArtifactHandle>,
}

/// Internal allowance for one model-facing tool projection.
///
/// The execution layer retains the complete result and optional artifact. This
/// value is only the per-call allowance selected by the aggregate planner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PresentationBudget {
    pub(crate) max_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolPresentation {
    pub(crate) text: String,
    /// Number of complete logical records included in the presentation.
    pub(crate) delivered_records: usize,
    /// True when the source was delivered without an omitted continuation.
    pub(crate) complete: bool,
    /// True when the first record itself did not fit the allowance.
    pub(crate) oversized_record: bool,
}

impl ToolPresentation {
    pub(crate) fn complete(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            delivered_records: 0,
            complete: true,
            oversized_record: false,
        }
    }

    pub(crate) fn bounded(
        text: impl Into<String>,
        delivered_records: usize,
        oversized_record: bool,
    ) -> Self {
        Self {
            text: text.into(),
            delivered_records,
            complete: false,
            oversized_record,
        }
    }

    pub(crate) fn omitted(message: impl Into<String>) -> Self {
        Self {
            text: message.into(),
            delivered_records: 0,
            complete: false,
            oversized_record: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecutionProgress {
    pub preview: String,
}

struct ExecutedTool {
    output: String,
    success: bool,
    dependencies: Vec<DependencyObservation>,
    mutations: Vec<MutationObservation>,
    bytes_read: u64,
    synced_text: Option<crate::codeintel::CodeIntelFileUpdate>,
    process: Option<ProcessExecutionFacts>,
    presentation: Option<ToolPresentationSource>,
}

impl ExecutedTool {
    fn output(output: String, success: bool) -> Self {
        Self {
            output,
            success,
            dependencies: Vec::new(),
            mutations: Vec::new(),
            bytes_read: 0,
            synced_text: None,
            process: None,
            presentation: None,
        }
    }

    fn with_process(mut self, process: Option<ProcessExecutionFacts>) -> Self {
        self.process = process;
        self
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            specs: &[
                ToolSpec {
                    name: "read",
                    effect_class: ToolEffectClass::SnapshotRead,
                    cacheability: ToolCacheability::Evidence,
                    volatility: ToolVolatility::Stable,
                    dependency_scope: ToolDependencyScope::TargetFile,
                    replay_policy: ToolReplayPolicy::EquivalentEvidence,
                },
                ToolSpec {
                    name: "list",
                    effect_class: ToolEffectClass::SnapshotRead,
                    cacheability: ToolCacheability::Evidence,
                    volatility: ToolVolatility::Stable,
                    dependency_scope: ToolDependencyScope::ImmediateDirectory,
                    replay_policy: ToolReplayPolicy::EquivalentEvidence,
                },
                ToolSpec {
                    name: "search",
                    effect_class: ToolEffectClass::SnapshotRead,
                    cacheability: ToolCacheability::Evidence,
                    volatility: ToolVolatility::ObservedWorkspace,
                    dependency_scope: ToolDependencyScope::ObservedWorkspace,
                    replay_policy: ToolReplayPolicy::EquivalentEvidence,
                },
                ToolSpec {
                    name: "write",
                    effect_class: ToolEffectClass::WorkspaceMutation,
                    cacheability: ToolCacheability::None,
                    volatility: ToolVolatility::Stable,
                    dependency_scope: ToolDependencyScope::TargetFile,
                    replay_policy: ToolReplayPolicy::Never,
                },
                ToolSpec {
                    name: "patch",
                    effect_class: ToolEffectClass::WorkspaceMutation,
                    cacheability: ToolCacheability::None,
                    volatility: ToolVolatility::Stable,
                    dependency_scope: ToolDependencyScope::TargetFile,
                    replay_policy: ToolReplayPolicy::Never,
                },
                ToolSpec {
                    name: "shell",
                    effect_class: ToolEffectClass::PotentiallyVolatile,
                    cacheability: ToolCacheability::None,
                    volatility: ToolVolatility::Volatile,
                    dependency_scope: ToolDependencyScope::Unknown,
                    replay_policy: ToolReplayPolicy::Never,
                },
                ToolSpec {
                    name: "code_intel",
                    effect_class: ToolEffectClass::PotentiallyVolatile,
                    cacheability: ToolCacheability::None,
                    volatility: ToolVolatility::Volatile,
                    dependency_scope: ToolDependencyScope::Unknown,
                    replay_policy: ToolReplayPolicy::Never,
                },
            ],
            services: Arc::new(ToolServices::new(ProcessRunner::new(
                ExecutableResolver::default(),
            ))),
        }
    }
}

static MODE_DEFINITIONS_AUTO: LazyLock<Arc<[Value]>> =
    LazyLock::new(|| mode_definitions_base(OperatingMode::Auto));
static MODE_DEFINITIONS_READ_ONLY: LazyLock<Arc<[Value]>> =
    LazyLock::new(|| mode_definitions_base(OperatingMode::ReadOnly));
static MODE_DEFINITIONS_PLAN: LazyLock<Arc<[Value]>> =
    LazyLock::new(|| mode_definitions_base(OperatingMode::Plan));

fn mode_definitions_base(mode: OperatingMode) -> Arc<[Value]> {
    ToolRegistry::default()
        .names_for_mode(mode)
        .into_iter()
        .map(tool_definition)
        .collect()
}

impl ToolRegistry {
    pub fn with_process_runner(process_runner: ProcessRunner) -> Self {
        Self {
            services: Arc::new(ToolServices::new(process_runner)),
            ..Self::default()
        }
    }

    pub(crate) fn process_runner(&self) -> &ProcessRunner {
        &self.services.process_runner
    }

    pub(crate) fn prepare_invocation(
        &self,
        mode: OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
    ) -> PreparedToolInvocation {
        PreparedToolInvocation::new(
            mode,
            cwd.as_ref(),
            name,
            arguments,
            self.operational_spec(name),
        )
    }

    pub(crate) fn prepare_invocations(
        &self,
        mode: OperatingMode,
        cwd: impl AsRef<Path>,
        calls: &[(String, String)],
    ) -> Vec<PreparedToolInvocation> {
        let cwd = cwd.as_ref();
        let started = Instant::now();
        let workspace = canonical_workspace(cwd);
        let workspace_preparation_us =
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        calls
            .iter()
            .map(|(name, arguments)| {
                PreparedToolInvocation::from_workspace(
                    mode,
                    cwd,
                    &workspace,
                    workspace_preparation_us,
                    name,
                    arguments,
                    self.operational_spec(name),
                )
            })
            .collect()
    }

    pub(crate) fn workspace_revision(&self) -> u64 {
        self.services.workspace_revision.load(Ordering::Acquire)
    }

    pub fn names_for_mode(&self, mode: OperatingMode) -> Vec<&'static str> {
        self.specs
            .iter()
            .filter(|spec| mode == OperatingMode::Auto || !spec.operational().mutates_workspace())
            .map(|spec| spec.name)
            .collect()
    }

    pub(crate) fn operational_spec(&self, name: &str) -> Option<ToolOperationalSpec> {
        self.specs
            .iter()
            .find(|spec| spec.name == name)
            .copied()
            .map(ToolSpec::operational)
    }

    pub(crate) fn mutates_workspace(&self, name: &str) -> Option<bool> {
        self.operational_spec(name)
            .map(ToolOperationalSpec::mutates_workspace)
    }

    pub(crate) fn definitions_for_mode_shared(&self, mode: OperatingMode) -> Arc<[Value]> {
        match mode {
            OperatingMode::Auto => Arc::clone(&MODE_DEFINITIONS_AUTO),
            OperatingMode::ReadOnly => Arc::clone(&MODE_DEFINITIONS_READ_ONLY),
            OperatingMode::Plan => Arc::clone(&MODE_DEFINITIONS_PLAN),
        }
    }

    pub fn definitions_for_mode(&self, mode: OperatingMode) -> Vec<Value> {
        self.definitions_for_mode_shared(mode).as_ref().to_vec()
    }

    pub fn execute(
        &self,
        mode: OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
    ) -> ToolResult {
        self.execute_with_cancellation(mode, cwd, name, arguments, None)
    }

    pub fn execute_with_cancellation(
        &self,
        mode: OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
        cancellation: Option<&CancellationToken>,
    ) -> ToolResult {
        self.execute_with_cancellation_and_progress(
            mode,
            cwd,
            name,
            arguments,
            cancellation,
            |_| {},
        )
    }

    pub fn execute_with_cancellation_and_progress(
        &self,
        mode: OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
        cancellation: Option<&CancellationToken>,
        on_progress: impl FnMut(ToolExecutionProgress),
    ) -> ToolResult {
        let prepared = self.prepare_invocation(mode, cwd, name, arguments);
        self.execute_prepared_with_cancellation_and_progress(&prepared, cancellation, on_progress)
            .result
    }

    pub(crate) fn execute_prepared_with_cancellation_and_progress(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
        mut on_progress: impl FnMut(ToolExecutionProgress),
    ) -> ToolExecutionOutcome {
        if let Some(outcome) = self.lookup_cached_evidence(prepared) {
            return with_admission_feedback(outcome, &prepared.admission_notes);
        }
        let revision_before = self.workspace_revision();
        let started = Instant::now();
        let result = if !self
            .names_for_mode(prepared.mode)
            .contains(&prepared.name.as_str())
        {
            Err(ToolExecutionError::from(ToolError::InvalidInput {
                message: format!(
                    "tool unavailable in {} mode",
                    crate::runtime::mode_name(prepared.mode)
                ),
            }))
        } else if let Some(error) = &prepared.error {
            Err(ToolExecutionError::from(ToolError::InvalidInput {
                message: error.clone(),
            }))
        } else {
            match prepared.name.as_str() {
                "read" => self.execute_read(prepared, cancellation),
                "list" => self.execute_list(prepared, cancellation),
                "search" => self.execute_search(prepared, cancellation),
                "write" => self.execute_write(prepared, cancellation, &mut on_progress),
                "patch" => self.execute_patch(prepared, cancellation, &mut on_progress),
                "shell" => self.execute_shell(prepared, cancellation, &mut on_progress),
                // code_intel is executed by the agent loop (async, server-backed).
                "code_intel" => Err(ToolExecutionError::from(ToolError::InvalidInput {
                    message:
                        "code_intel runs through the agent loop; not available on this sync path"
                            .into(),
                })),
                name => Err(ToolExecutionError::from(ToolError::InvalidInput {
                    message: format!("unknown tool: {name}"),
                })),
            }
        };
        let execution_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let finalization_started = Instant::now();
        let effects_uncertain = result
            .as_ref()
            .err()
            .is_some_and(|failure| failure.effects_uncertain);
        let (result, dependencies, mutations, bytes_read, synced_text, process, presentation) =
            match result {
                Ok(executed) => (
                    ToolResult {
                        name: prepared.name.clone(),
                        success: executed.success,
                        output: executed.output,
                        artifact: None,
                    },
                    executed.dependencies,
                    executed.mutations,
                    executed.bytes_read,
                    executed.synced_text,
                    executed.process,
                    executed.presentation,
                ),
                Err(failure) => {
                    let mut output = tool_error_message(failure.error);
                    if let Some(context) = failure.context {
                        output.push('\n');
                        output.push_str(&context);
                    }
                    (
                        ToolResult {
                            name: prepared.name.clone(),
                            success: false,
                            output,
                            artifact: None,
                        },
                        failure.dependencies,
                        failure.mutations,
                        failure.bytes_read,
                        None,
                        None,
                        None,
                    )
                }
            };
        let changed = effects_uncertain
            || (result.success && mutations.iter().any(MutationObservation::changed));
        let revision_after = if changed {
            self.services
                .workspace_revision
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1)
        } else {
            self.services.workspace_revision.load(Ordering::Acquire)
        };
        let mut modified_paths: Vec<_> = mutations
            .iter()
            .map(|mutation| mutation.path.clone())
            .collect();
        if effects_uncertain {
            modified_paths.extend(prepared.target_paths.iter().cloned());
        }
        let finalization_us =
            u64::try_from(finalization_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let outcome = ToolExecutionOutcome {
            result,
            receipt: ToolExecutionReceipt {
                effects_uncertain,
                dependencies,
                mutations,
                modified_paths,
                revision_before,
                revision_after,
                bytes_read,
                preparation_us: prepared.preparation_us,
                execution_us,
                finalization_us,
                synced_text,
                process,
                presentation,
            },
        };
        if (outcome.result.success || effects_uncertain)
            && !outcome.receipt.modified_paths.is_empty()
        {
            self.invalidate_cached_evidence_for_paths(&outcome.receipt.modified_paths);
        }
        self.store_cached_evidence(prepared, &outcome);
        with_admission_feedback(outcome, &prepared.admission_notes)
    }

    fn lookup_cached_evidence(
        &self,
        prepared: &PreparedToolInvocation,
    ) -> Option<ToolExecutionOutcome> {
        if !evidence_reusable(prepared) {
            return None;
        }
        if prepared
            .spec
            .is_some_and(|spec| spec.dependency_scope == ToolDependencyScope::ObservedWorkspace)
        {
            return None;
        }
        // Retain the candidate, then release the shared cache before filesystem
        // validation and copying the output. Independent reads must not serialize
        // their I/O behind this mutex. This is still a fresh validation per hit.
        let outcome = {
            let cache = lock_mutex(&self.services.evidence_cache);
            Arc::clone(
                &cache
                    .iter()
                    .find(|entry| entry.fingerprint == prepared.canonical_fingerprint)?
                    .outcome,
            )
        };
        if !outcome
            .receipt
            .dependencies
            .iter()
            .all(DependencyObservation::stamp_matches)
        {
            return None;
        }
        Some(reused_evidence((*outcome).clone()))
    }

    fn store_cached_evidence(
        &self,
        prepared: &PreparedToolInvocation,
        outcome: &ToolExecutionOutcome,
    ) {
        if !outcome.result.success || !evidence_reusable(prepared) {
            return;
        }
        if prepared
            .spec
            .is_some_and(|spec| spec.dependency_scope == ToolDependencyScope::ObservedWorkspace)
        {
            return;
        }
        let entry = CachedEvidence {
            fingerprint: prepared.canonical_fingerprint.clone(),
            outcome: Arc::new(outcome.clone()),
        };
        let mut cache = lock_mutex(&self.services.evidence_cache);
        cache.retain(|entry| entry.fingerprint != prepared.canonical_fingerprint);
        if cache.len() >= MAX_EVIDENCE_CACHE {
            cache.pop_front();
        }
        cache.push_back(entry);
    }

    fn invalidate_cached_evidence_for_paths(&self, paths: &[PathBuf]) {
        let identities = paths
            .iter()
            .map(|path| path_identity(path))
            .collect::<Vec<_>>();
        if identities.is_empty() {
            return;
        }
        let mut cache = lock_mutex(&self.services.evidence_cache);
        cache.retain(|entry| {
            !entry.outcome.receipt.dependencies.iter().any(|dependency| {
                identities
                    .iter()
                    .any(|identity| identity == &path_identity(&dependency.path))
            })
        });
    }

    fn execute_read(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Read { offset, max_lines } = &prepared.arguments else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match read".into(),
            }
            .into());
        };
        let path = prepared_path(prepared)?;
        let page = self.services.read.read_file_range_resolved(
            &path,
            *offset,
            *max_lines,
            false,
            cancellation,
        )?;
        let presentation = Some(ToolPresentationSource::Read {
            prefix: String::new(),
            full: page.output.clone(),
            first: page.first_line,
            records: page.records.clone(),
            next_offset: page.next_offset,
        });
        Ok(ExecutedTool {
            success: true,
            output: page.output,
            dependencies: vec![page.dependency],
            mutations: Vec::new(),
            bytes_read: page.bytes_read,
            synced_text: None,
            process: None,
            presentation,
        })
    }

    fn execute_list(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::List {
            offset,
            max_entries,
            cursor,
        } = &prepared.arguments
        else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match list".into(),
            }
            .into());
        };
        let path = prepared_path(prepared)?;
        let page = self.services.list.page(
            &path,
            *offset,
            *max_entries,
            cursor.as_deref(),
            cancellation,
        )?;
        let presentation = Some(ToolPresentationSource::List {
            prefix: String::new(),
            page: page.clone(),
            display_root: prepared.canonical_workspace.clone(),
        });
        let page_len = page.entries.len();
        let mut output = page
            .entries
            .iter()
            .map(|entry| {
                search::display_path(&prepared.canonical_workspace, entry)
                    .display()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(cursor) = page.next_cursor {
            let last = page.first.saturating_add(page_len).saturating_sub(1);
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!(
                "\n[showing entries {}-{last} of {}; pass \"cursor\": \"{cursor}\" for the next page]",
                page.first, page.total
            ));
        }
        Ok(ExecutedTool {
            success: true,
            output,
            dependencies: vec![page.dependency],
            mutations: Vec::new(),
            bytes_read: page.bytes_read,
            synced_text: None,
            process: None,
            presentation,
        })
    }

    fn execute_search(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Search {
            context_lines,
            patterns,
            offset,
            max_hits,
            cursor,
        } = &prepared.arguments
        else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match search".into(),
            }
            .into());
        };
        let path = prepared_path(prepared)?;
        let page = self.services.search.page(
            &path,
            patterns.clone(),
            search::SearchPageOptions {
                offset: *offset,
                max_hits: *max_hits,
                context_lines: *context_lines,
            },
            cursor.as_deref(),
            cancellation,
        )?;
        let output = search::format_search_batch_page(&page, &prepared.canonical_workspace);
        let presentation = Some(ToolPresentationSource::Search {
            prefix: String::new(),
            page: page.clone(),
            display_root: prepared.canonical_workspace.clone(),
        });
        Ok(ExecutedTool {
            success: true,
            output,
            dependencies: vec![page.dependency],
            mutations: Vec::new(),
            bytes_read: page.bytes_read,
            synced_text: None,
            process: None,
            presentation,
        })
    }

    fn execute_write(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
        on_progress: &mut impl FnMut(ToolExecutionProgress),
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Write { content, expected } = &prepared.arguments else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match write".into(),
            }
            .into());
        };
        let path = prepared_path(prepared)?;
        if expected.is_none()
            && fs::metadata(&path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            self.services.read.invalidate(&path);
            self.invalidate_cached_evidence_for_paths(std::slice::from_ref(&path));
        }
        let precondition = expected
            .clone()
            .map(FilePrecondition::ExactText)
            .or_else(|| {
                self.services
                    .read
                    .complete_digest(&path)
                    .map(FilePrecondition::ObservedDigest)
            });
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolError::Cancelled.into());
        }
        let written = match write::write_file_with_receipt(
            &path,
            content,
            precondition,
            cancellation,
            &mut || {
                on_progress(ToolExecutionProgress {
                    preview: "Waiting for file lock".into(),
                })
            },
        ) {
            Ok(written) => written,
            Err(failure) => {
                if matches!(
                    &failure.error,
                    ToolError::StaleRead { .. } | ToolError::PreconditionRequired { .. }
                ) {
                    self.services.read.invalidate(&path);
                    self.invalidate_cached_evidence_for_paths(std::slice::from_ref(&path));
                }
                return Err(failure);
            }
        };
        if written.before.is_some() {
            self.services
                .read
                .remember_complete_digest(&path, written.written_digest);
        } else {
            self.services.read.invalidate(&path);
        }
        let display = path
            .strip_prefix(&prepared.canonical_workspace)
            .unwrap_or(&path);
        Ok(ExecutedTool {
            success: true,
            output: format!(
                "written {}; bytes={}; sha256={}; exists=true; do not re-read{}",
                display.display(),
                written.after.len,
                written.written_sha256_12,
                written
                    .recovery_note
                    .as_ref()
                    .map(|note| format!("; {note}"))
                    .unwrap_or_default()
            ),
            dependencies: written.dependency.into_iter().collect(),
            mutations: vec![MutationObservation {
                path,
                before_content_digest: if written.recovery_note.is_some() {
                    None
                } else {
                    written
                        .before
                        .as_deref()
                        .map(|value| digest_bytes(b"slim-written-content-v1", value.as_bytes()))
                },
                after: written.after,
            }],
            bytes_read: written.bytes_read,
            synced_text: None,
            process: None,
            presentation: None,
        })
    }

    fn execute_patch(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
        on_progress: &mut impl FnMut(ToolExecutionProgress),
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Patch { edits } = &prepared.arguments else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match patch".into(),
            }
            .into());
        };
        let path = prepared_path(prepared)?;
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolError::Cancelled.into());
        }
        let content =
            patch::apply_exact_patches_with_content(&path, edits, cancellation, &mut || {
                on_progress(ToolExecutionProgress {
                    preview: "Waiting for file lock".into(),
                })
            })?;
        self.services
            .read
            .remember_complete_digest(&path, write::content_sha256(content.text.as_bytes()));
        let before_digest = content.before_digest;
        Ok(ExecutedTool {
            success: true,
            output: content.summary,
            dependencies: vec![content.dependency],
            mutations: vec![MutationObservation {
                path,
                before_content_digest: (!content.displaced_version_preserved)
                    .then(|| before_digest.clone()),
                after: content.stamp,
            }],
            bytes_read: content.bytes_read,
            synced_text: Some(crate::codeintel::CodeIntelFileUpdate {
                text: content.text,
                patch: content.edits.map(|edits| crate::codeintel::CodeIntelPatch {
                    before_digest,
                    edits,
                }),
            }),
            process: None,
            presentation: None,
        })
    }

    fn execute_shell(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
        on_progress: &mut impl FnMut(ToolExecutionProgress),
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Shell {
            command,
            args,
            timeout_ms,
        } = &prepared.arguments
        else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match shell".into(),
            }
            .into());
        };
        let result = shell::run_shell_timeout_cancellable_with_progress_and_runner_with_budget(
            &self.services.process_runner,
            &prepared.canonical_workspace,
            match args {
                Some(args) => shell::ShellInvocation::Program {
                    executable: command,
                    args,
                },
                None => shell::ShellInvocation::Script(command),
            },
            std::time::Duration::from_millis(*timeout_ms),
            cancellation,
            ProcessOutputBudget::per_stream(SHELL_STREAM_CAP_BYTES),
            |progress| {
                let line = if progress.last_line.is_empty() {
                    "no output yet"
                } else {
                    &progress.last_line
                };
                on_progress(ToolExecutionProgress {
                    preview: format!(
                        "{line} · out {} B · err {} B",
                        progress.stdout_bytes, progress.stderr_bytes
                    ),
                });
            },
        )?;
        let success = result.output.status.success() && !result.timed_out && !result.cancelled;
        let mut output = format!(
            "{}\nstdout:\n{}stderr:\n{}",
            format_shell_status_header(
                result.output.status.code(),
                result.timed_out,
                result.cancelled,
            ),
            cap_shell_stream(&result.output.stdout, result.stdout_discarded_bytes),
            cap_shell_stream(&result.output.stderr, result.stderr_discarded_bytes),
        );
        if result.output.status.code().is_some_and(|code| code != 0)
            && !result.timed_out
            && !result.cancelled
            && result
                .output
                .stderr
                .iter()
                .all(|byte| byte.is_ascii_whitespace())
        {
            output.push_str(
                "\n[note: nonzero exit with empty stderr — empty stderr does not imply success; \
                 if this was a grep/diff/--check-style command, its nonzero status may mean \
                 \"no match\"/\"diffs exist\"; otherwise this remains a failed process; \
                 judge by stdout and documented status semantics]",
            );
        }
        let process = Some(result.execution_facts());
        Ok(ExecutedTool::output(output, success).with_process(process))
    }
}

fn prepared_path(prepared: &PreparedToolInvocation) -> Result<PathBuf, ToolError> {
    let target = prepared
        .target_paths
        .first()
        .ok_or_else(|| ToolError::InvalidInput {
            message: "missing required string field `path`".into(),
        })?;
    if target.starts_with(&prepared.canonical_workspace) {
        return Ok(target.clone());
    }
    let relative = target
        .strip_prefix(&prepared.canonical_workspace)
        .map_err(|_| ToolError::InvalidInput {
            message: "path escapes the workspace".into(),
        })?;
    if relative.as_os_str().is_empty() {
        let resolved = fs::canonicalize(&prepared.canonical_workspace).map_err(|error| {
            ToolError::InvalidInput {
                message: format!("workspace root cannot be resolved: {error}"),
            }
        })?;
        return ensure_workspace_containment(&prepared.canonical_workspace, resolved)
            .map_err(|message| ToolError::InvalidInput { message });
    }
    resolve_workspace_path_from_root(&prepared.canonical_workspace, &path_identity(relative))
        .map_err(|message| ToolError::InvalidInput { message })
}

fn format_shell_status_header(code: Option<i32>, timed_out: bool, cancelled: bool) -> String {
    let exit = match code {
        Some(0) => "exit 0".into(),
        Some(value) => format!("exit {value}"),
        None => "exit n/a".into(),
    };
    let mut header = exit;
    if timed_out {
        header.push_str(" · timed out");
    }
    if cancelled {
        header.push_str(" · cancelled");
    }
    header
}

/// Maximum bytes of one stdout/stderr stream placed into model context.
/// Larger streams are truncated with an explicit marker that also accounts for
/// bytes discarded by the bounded raw shell capture.
const SHELL_STREAM_CAP_BYTES: usize = 8 * 1024;

fn cap_shell_stream(raw: &[u8], previously_discarded_bytes: usize) -> String {
    if raw.len() <= SHELL_STREAM_CAP_BYTES && previously_discarded_bytes == 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let retained_bytes = raw.len().min(SHELL_STREAM_CAP_BYTES);
    let head_bytes = retained_bytes / 2 + retained_bytes % 2;
    let tail_bytes = retained_bytes - head_bytes;
    let tail_start = raw.len() - tail_bytes;
    let discarded_bytes = previously_discarded_bytes.saturating_add(raw.len() - retained_bytes);
    format!(
        "{}\n[truncated {discarded_bytes} bytes; kept first {head_bytes} and last {tail_bytes} bytes of this stream]\n{}",
        String::from_utf8_lossy(&raw[..head_bytes]),
        String::from_utf8_lossy(&raw[tail_start..]),
    )
}

fn tool_definition(name: &str) -> Value {
    let (description, properties, required) = match name {
        "read" => (
            "Read unchanged UTF-8 text. Omit `offset` for the first page (line 1): when both `max_lines` and its `lines` alias are omitted, a file whose metadata length is at most 1 MiB minus 128 bytes gets up to 4096 lines; otherwise the omitted limit is 200 lines. Pages starting later default to 200 lines. Pass either `max_lines` or `lines` (1..4096) for an explicit page size. Example: `{\"path\":\"src/lib.rs\",\"max_lines\":20}`. Prefer search+patch for a local edit; a complete read authorizes write without repeating expected. A successful overwrite or patch of that path does too.",
            json!({"path": {"type": "string", "description": "Workspace-relative path."}, "max_lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES_CAP, "description": "Canonical page-size field; omit to use the default for this offset."}, "lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES_CAP, "description": "Alias for `max_lines`."}, "offset": {"type": "integer", "minimum": 1, "description": "First line (1-based); omit for line 1."}}),
            json!(["path"]),
        ),
        "list" => (
            "List directory pages. Inspect .slim runtime/config files only when relevant.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative; default root."},
                "max_entries": {"type": "integer", "minimum": 1, "maximum": MAX_ENTRIES_CAP},
                "offset": {"type": "integer", "minimum": 1},
                "cursor": {"type": "string", "description": "Resume cursor; omit/empty to start."}
            }),
            json!([]),
        ),
        "search" => (
            "Literal UTF-8 search: query or patterns, exclusively. Patterns share a scan; hits group under a [path] header, `N:` marks a hit line and `N-` context. A unique hit line is enough for patch.expected—copy only the text after `N: `, never line numbers, [path] headers or [pattern] labels. Raise context_lines when that line is not unique; values above 3 saturate at 3 and report an admission note. Omit or pass 0 to locate; join context lines with \\n and omit the N:/N- prefixes. A hit that contains [truncated] is not expected—narrow path or query. Cursors preserve historical snapshots; repeat path/query/context_lines.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path."},
                "query": {"type": "string"},
                "patterns": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 32},
                "context_lines": {"type": "integer", "minimum": 0, "default": 0, "description": "Lines before/after each hit in the same scan when the hit line is not unique. Omit or 0 to locate; values above 3 saturate at 3 with an admission note. Does not count as a full-file read for write."},
                "max_hits": {"type": "integer", "minimum": 1, "maximum": MAX_HITS_CAP},
                "offset": {"type": "integer", "minimum": 1},
                "cursor": {"type": "string", "description": "Previous page cursor."}
            }),
            json!([]),
        ),
        "code_intel" => {
            let definition = code_intel_definition();
            let name = definition.get("name").and_then(Value::as_str).unwrap_or(name);
            return json!({
                "name": name,
                "description": definition.get("description").cloned().unwrap_or_default(),
                "input_schema": definition.get("input_schema").cloned().unwrap_or_default()
            });
        }
        "write" => (
            "Write a whole UTF-8 file; create parents. Prefer patch for a local edit. Create: omit expected. Overwrite: pass expected as the current full file, or omit it after a complete read or a successful overwrite/patch of that path. A failed overwrite includes the current file when it fits; retry with that expected or patch—do not re-read. A successful write needs no confirmation read; a successful overwrite authorizes the next write to omit expected. Create does not. Independent writes to different paths may share one turn. Do not send a large expected for a local change. Changes since expected/read reject the write.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path."},
                "content": {"type": "string"},
                "expected": {
                    "type": ["string", "null"],
                    "description": "Current full-file text for an optimistic overwrite. Omit or null to create, or after a complete read or a successful overwrite/patch of that path. LF matches uniform CRLF."
                }
            }),
            json!(["path", "content"]),
        ),
        "patch" => (
            "Preferred for local code edits. Atomic ordered edits. Use either the `edits` array or the legacy top-level `expected` + `replacement` pair, never both. Each expected is a unique raw file substring—copy search hit/context TEXT only, never line numbers (`N:`/`N-`), [path] headers, [pattern] labels, or [truncated] markers. A unique context_lines=0 line is enough. Search with context_lines is enough—no complete read. A failed patch with no match includes the current file when it fits; retry with a unique excerpt—do not re-read. A successful patch needs no confirmation read and authorizes a later write to omit expected. Independent patches to different paths may share one turn. LF matches uniform CRLF. Failure leaves the file unchanged.",
            json!({"path": {"type": "string", "description": "Workspace-relative path."}, "edits": {"type": "array", "minItems": 1, "maxItems": patch::MAX_PATCH_EDITS, "items": {"type": "object", "properties": {"expected": {"type": "string", "minLength": 1, "description": "Unique raw file substring. Copy the text after `N: `/`N- ` in search output, or verbatim from a read; join context lines with \\n."}, "replacement": {"type": "string"}}, "required": ["expected", "replacement"], "additionalProperties": false}}, "expected": {"type": "string", "minLength": 1, "description": "Legacy unique raw file substring."}, "replacement": {"type": "string", "description": "Legacy replacement text."}}),
            json!(["path"]),
        ),
        "shell" => (
            "Run in workspace. Script form (`args` omitted or null) always executes through PowerShell without a profile, preserving native exit codes; pass the command text in `command`, never a JSON tool payload. Program form (`args` supplied, including an empty array) runs the executable directly with literal arguments; `.bat` and `.cmd` keep their own interpreter semantics. Direct example: `{\"command\":\"git\",\"args\":[\"status\",\"--short\"]}`. Exit status describes the process, not every suboperation or task verification. Use workspace metadata before git status/diff; they need a Git repository unless using explicit --no-index. Do not use git status/diff as code validation. Use file tools for code edits. `timeout_ms` must be 1..120000; invalid values are rejected. Raise timeout only for deliberate builds/tests.",
            json!({"command": {"type": "string"}, "args": {"type": ["array", "null"], "items": {"type": "string"}}, "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_MS, "default": 30000}}),
            json!(["command"]),
        ),
        _ => ("Slim tool", json!({}), json!([])),
    };
    let mut input_schema = json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    });
    if name == "patch" {
        input_schema["oneOf"] = json!([
            {
                "required": ["edits"],
                "not": {"anyOf": [{"required": ["expected"]}, {"required": ["replacement"]}]}
            },
            {
                "required": ["expected", "replacement"],
                "not": {"required": ["edits"]}
            }
        ]);
    }
    json!({
        "name": name,
        "description": description,
        "input_schema": input_schema
    })
}

pub(crate) fn resolve_workspace_path(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    if raw.as_os_str().is_empty() {
        return Err("path must not be empty".into());
    }
    if raw.is_absolute() {
        return Err("absolute paths are not allowed; use a workspace-relative path".into());
    }

    let root = fs::canonicalize(cwd)
        .map_err(|error| format!("workspace root cannot be resolved: {error}"))?;
    resolve_workspace_path_from_root(&root, path)
}

pub(crate) fn resolve_workspace_path_from_root(root: &Path, path: &str) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    if raw.as_os_str().is_empty() {
        return Err("path must not be empty".into());
    }
    if raw.is_absolute() {
        return Err("absolute paths are not allowed; use a workspace-relative path".into());
    }
    let mut relative = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => relative.push(part),
            Component::ParentDir => {
                if !relative.pop() {
                    return Err("path escapes the workspace".into());
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err("absolute paths are not allowed; use a workspace-relative path".into());
            }
        }
    }

    let candidate = root.join(relative);
    match fs::canonicalize(&candidate) {
        Ok(resolved) => ensure_workspace_containment(root, resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            resolve_missing_workspace_path(root, &candidate)
        }
        Err(error) => Err(format!("path cannot be resolved: {error}")),
    }
}

fn resolve_missing_workspace_path(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let mut ancestor = candidate;
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| "path escapes the workspace".to_owned())?;
                missing.push(name.to_os_string());
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| "path escapes the workspace".to_owned())?;
            }
            Err(error) => return Err(format!("path cannot be resolved: {error}")),
        }
    }

    let mut resolved =
        fs::canonicalize(ancestor).map_err(|error| format!("path cannot be resolved: {error}"))?;
    if !resolved.starts_with(root) {
        return Err("path escapes the workspace".into());
    }
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn ensure_workspace_containment(root: &Path, resolved: PathBuf) -> Result<PathBuf, String> {
    if resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err("path escapes the workspace".into())
    }
}

fn tool_error_message(error: ToolError) -> String {
    match error {
        ToolError::Io { message } => format!("io error: {message}"),
        ToolError::Cancelled => "tool cancelled before side effect".into(),
        ToolError::StaleRead { path } => format!("stale read: {path}; the precondition differs from current bytes. Retry write with expected set to the current file below, or patch an exact current excerpt. No write applied."),
        ToolError::PreconditionRequired { path } => format!("precondition required: {path}; pass expected as the current full file below, or use patch with a unique excerpt. No write applied."),
        ToolError::MatchCount { count } => format!("expected exactly one match, got {count}"),
        ToolError::InvalidInput { message } => message,
    }
}

#[cfg(test)]
mod timeout_bound_tests {
    use super::{
        admission_output_prefix, summarize_tool_arguments_for, tool_definition,
        ADMISSION_OUTPUT_PREFIX_BYTES, MAX_SHELL_TIMEOUT_MS,
    };
    use serde_json::json;

    #[test]
    fn shell_timeout_is_bounded_and_advertised() {
        assert_eq!(
            tool_definition("shell")["input_schema"]["properties"]["timeout_ms"]["maximum"],
            MAX_SHELL_TIMEOUT_MS
        );
        assert!(
            summarize_tool_arguments_for("shell", r#"{"command":"echo","timeout_ms":1}"#)
                .contains("limit 1ms")
        );
        assert!(
            summarize_tool_arguments_for("shell", r#"{"command":"echo","timeout_ms":120001}"#)
                .contains("command=echo")
        );
        assert!(!summarize_tool_arguments_for(
            "shell",
            r#"{"command":"echo","timeout_ms":120001}"#
        )
        .contains("limit 120s"));
    }

    #[test]
    fn admission_feedback_is_bounded_in_bytes() {
        let notes = vec!["ação ".repeat(256)];
        let prefix = admission_output_prefix(&notes).expect("feedback");
        assert!(prefix.len() <= ADMISSION_OUTPUT_PREFIX_BYTES);
        assert!(prefix.starts_with("[admission: "));
        assert!(prefix.ends_with("]\n"));
    }

    #[test]
    fn shell_consumer_budget_preserves_rendered_bytes_and_discard_counts() {
        let bytes = (0..9 * 1024 * 1024 + 3)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        // This composition exercises the real renderer, including invalid
        // UTF-8 at the cut. The manual runner test checks actual pipe capture.
        let retain = |source: &[u8], budget: usize| {
            let size = source.len().min(budget);
            let head = size.div_ceil(2);
            let tail = size - head;
            let mut retained = source[..head].to_vec();
            retained.extend_from_slice(&source[source.len() - tail..]);
            (retained, source.len() - size)
        };
        for size in [
            0, 1, 4095, 4096, 4097, 8191, 8192, 8193, 16383, 16384, 16385, 1_048_576, 8_388_607,
            8_388_608, 8_388_609, 9_437_187,
        ] {
            let source = &bytes[..size];
            let (old, old_discarded) = retain(source, 8 * 1024 * 1024);
            let (native, native_discarded) = retain(source, super::SHELL_STREAM_CAP_BYTES);
            assert_eq!(
                super::cap_shell_stream(&old, old_discarded),
                super::cap_shell_stream(&native, native_discarded),
                "presentation differs at {size} source bytes",
            );
        }
    }

    #[test]
    fn search_and_patch_schemas_teach_raw_expected_without_a_safety_read() {
        let search = tool_definition("search");
        let context = search["input_schema"]["properties"]["context_lines"]["description"]
            .as_str()
            .expect("context_lines description");
        assert!(context.contains("hit line is not unique"), "{context}");
        assert!(
            !context.to_ascii_lowercase().contains("use read"),
            "{context}"
        );
        assert_eq!(
            search["input_schema"]["properties"]["context_lines"]["default"],
            0
        );
        assert!(
            search["input_schema"]["properties"]["context_lines"]
                .get("maximum")
                .is_none(),
            "context_lines schema should describe saturation instead of rejecting values"
        );
        let search_description = search["description"].as_str().expect("search description");
        assert!(
            search_description.contains("copy only the text after `N: `"),
            "{search_description}"
        );
        assert!(
            !search_description.to_ascii_lowercase().contains("use read"),
            "{search_description}"
        );

        let patch = tool_definition("patch");
        let expected = patch["input_schema"]["properties"]["edits"]["items"]["properties"]
            ["expected"]["description"]
            .as_str()
            .expect("patch.expected description");
        assert!(expected.contains("raw file substring"), "{expected}");
        assert!(expected.contains("`N: `"), "{expected}");
        let patch_description = patch["description"].as_str().expect("patch description");
        assert!(
            patch_description.contains("never line numbers"),
            "{patch_description}"
        );
        assert!(
            patch_description.contains("[truncated]"),
            "{patch_description}"
        );
        assert!(
            patch_description.contains("no complete read"),
            "{patch_description}"
        );
        assert_eq!(patch["input_schema"]["required"], json!(["path"]));
        assert_eq!(
            patch["input_schema"]["oneOf"]
                .as_array()
                .map(|branches| branches.len()),
            Some(2)
        );
    }
}

#[cfg(test)]
mod evidence_cache_tests {
    use super::*;
    use std::fs;

    #[test]
    fn replacing_a_file_with_equal_size_and_mtime_invalidates_read_evidence() {
        let root = std::env::temp_dir().join(format!(
            "slim-evidence-replacement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("data.txt");
        fs::write(&path, "alpha\n").expect("write");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let registry = ToolRegistry::default();
        let read = || {
            registry.execute(
                OperatingMode::ReadOnly,
                &root,
                "read",
                r#"{"path":"data.txt"}"#,
            )
        };
        assert_eq!(read().output, "alpha\n");
        fs::rename(&path, root.join("old.txt")).expect("retain original inode");
        fs::write(&path, "bravo\n").expect("replace");
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        let result = read();
        assert!(result.success, "{}", result.output);
        assert_eq!(result.output, "bravo\n");
        fs::remove_file(&path).unwrap();
        assert!(
            !read().success,
            "removed files must not replay cached evidence"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn matching_read_reuses_cached_evidence_until_stamp_changes() {
        let root = std::env::temp_dir().join(format!(
            "slim-evidence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("dup.txt");
        fs::write(&path, "alpha\n").expect("write");
        let registry = ToolRegistry::default();
        let prepared = registry.prepare_invocation(
            OperatingMode::Auto,
            &root,
            "read",
            r#"{"path":"dup.txt","max_lines":10}"#,
        );
        let first =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        assert!(first.result.success);
        assert!(first.receipt.execution_us > 0);
        let second =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        assert_eq!(second.result.output, first.result.output);
        assert_eq!(second.receipt.execution_us, 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&path, "beta\n").expect("rewrite");
        let third =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        assert!(third.result.success);
        assert_ne!(third.result.output, first.result.output);
        assert!(third.receipt.execution_us > 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn write_does_not_invalidate_evidence_for_an_unrelated_path() {
        let root = std::env::temp_dir().join(format!(
            "slim-evidence-unrelated-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join("keep.txt"), "stable\n").expect("keep");
        fs::write(root.join("other.txt"), "before\n").expect("other");
        let registry = ToolRegistry::default();
        let keep = registry.prepare_invocation(
            OperatingMode::Auto,
            &root,
            "read",
            r#"{"path":"keep.txt","max_lines":10}"#,
        );
        let first = registry.execute_prepared_with_cancellation_and_progress(&keep, None, |_| {});
        assert!(first.result.success);
        assert!(first.receipt.execution_us > 0);
        let write = registry.prepare_invocation(
            OperatingMode::Auto,
            &root,
            "write",
            r#"{"path":"other.txt","content":"after\n","expected":"before\n"}"#,
        );
        let written =
            registry.execute_prepared_with_cancellation_and_progress(&write, None, |_| {});
        assert!(written.result.success);
        let second = registry.execute_prepared_with_cancellation_and_progress(&keep, None, |_| {});
        assert_eq!(second.result.output, first.result.output);
        assert_eq!(second.receipt.execution_us, 0);
        let _ = fs::remove_dir_all(root);
    }
}
