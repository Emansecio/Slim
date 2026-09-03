mod code_intel;
mod execution;
mod list;
mod patch;
mod read;
mod search;
mod shell;
mod write;

use crate::context::ArtifactHandle;
use crate::process::{ExecutableResolver, ProcessRunner};
use crate::runtime::CancellationToken;
use crate::OperatingMode;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(crate) use execution::{
    canonical_workspace, digest_bytes, path_identity, DependencyKind, DependencyObservation,
    FastStamp, MutationObservation, PreparedToolArguments, PreparedToolInvocation,
    ToolExecutionError, ToolExecutionOutcome, ToolExecutionReceipt,
};

pub use code_intel::{
    code_intel_definition, parse_code_intel_request, render_code_intel, CodeIntelRequest,
    CODE_INTEL_ACTIONS,
};
pub use list::{list_directory, DEFAULT_MAX_ENTRIES, MAX_ENTRIES_CAP};
pub use patch::apply_exact_patch;
pub use read::{read_file, read_file_range, DEFAULT_MAX_READ_LINES, MAX_READ_LINES_CAP};
pub(crate) use search::MAX_SEARCH_PATTERNS;
pub use search::{
    format_search_page, search_bounded, search_literal, SearchHit, SearchOptions, SearchPage,
    DEFAULT_MAX_HITS, MAX_HITS_CAP,
};
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

fn bounded_shell_timeout_ms(timeout_ms: u64) -> u64 {
    timeout_ms.clamp(1, MAX_SHELL_TIMEOUT_MS)
}

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
    if name != "shell" {
        return summarize_tool_arguments(arguments);
    }
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return summarize_tool_arguments(arguments);
    };
    let Some(command) = value.get("command").and_then(Value::as_str) else {
        return summarize_tool_arguments(arguments);
    };
    let timeout_ms = bounded_shell_timeout_ms(
        value
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000),
    );
    let limit = if timeout_ms.is_multiple_of(1_000) {
        format!("{}s", timeout_ms / 1_000)
    } else {
        format!("{timeout_ms}ms")
    };
    let suffix = format!(" · limit {limit}");
    let available = ARGUMENT_SUMMARY_LIMIT.saturating_sub(suffix.chars().count());
    let command = bound_argument_summary_exact(&format!("command={command}"), available);
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
    outcome: ToolExecutionOutcome,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResult {
    pub name: String,
    pub success: bool,
    pub output: String,
    pub artifact: Option<ArtifactHandle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecutionProgress {
    pub preview: String,
}

struct ExecutedTool {
    output: String,
    dependencies: Vec<DependencyObservation>,
    mutations: Vec<MutationObservation>,
    bytes_read: u64,
    synced_text: Option<String>,
}

impl ExecutedTool {
    fn output(output: String) -> Self {
        Self {
            output,
            dependencies: Vec::new(),
            mutations: Vec::new(),
            bytes_read: 0,
            synced_text: None,
        }
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

    pub fn definitions_for_mode(&self, mode: OperatingMode) -> Vec<Value> {
        self.names_for_mode(mode)
            .into_iter()
            .map(tool_definition)
            .collect()
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
            return outcome;
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
                "write" => self.execute_write(prepared, cancellation),
                "patch" => self.execute_patch(prepared, cancellation),
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
        let (result, dependencies, mutations, bytes_read, synced_text) = match result {
            Ok(executed) => (
                ToolResult {
                    name: prepared.name.clone(),
                    success: true,
                    output: executed.output,
                    artifact: None,
                },
                executed.dependencies,
                executed.mutations,
                executed.bytes_read,
                executed.synced_text,
            ),
            Err(failure) => (
                ToolResult {
                    name: prepared.name.clone(),
                    success: false,
                    output: tool_error_message(failure.error),
                    artifact: None,
                },
                failure.dependencies,
                failure.mutations,
                failure.bytes_read,
                None,
            ),
        };
        let changed = result.success && mutations.iter().any(MutationObservation::changed);
        let revision_after = if changed {
            self.services
                .workspace_revision
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1)
        } else {
            self.services.workspace_revision.load(Ordering::Acquire)
        };
        let modified_paths = mutations
            .iter()
            .map(|mutation| mutation.path.clone())
            .collect();
        let finalization_us =
            u64::try_from(finalization_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let outcome = ToolExecutionOutcome {
            result,
            receipt: ToolExecutionReceipt {
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
            },
        };
        if outcome.result.success && !outcome.receipt.modified_paths.is_empty() {
            self.invalidate_cached_evidence_for_paths(&outcome.receipt.modified_paths);
        }
        self.store_cached_evidence(prepared, &outcome);
        outcome
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
        let cache = lock_mutex(&self.services.evidence_cache);
        let entry = cache
            .iter()
            .find(|entry| entry.fingerprint == prepared.canonical_fingerprint)?;
        if !entry
            .outcome
            .receipt
            .dependencies
            .iter()
            .all(DependencyObservation::stamp_matches)
        {
            return None;
        }
        Some(reused_evidence(entry.outcome.clone()))
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
        let mut cache = lock_mutex(&self.services.evidence_cache);
        cache.retain(|entry| entry.fingerprint != prepared.canonical_fingerprint);
        if cache.len() >= MAX_EVIDENCE_CACHE {
            cache.pop_front();
        }
        cache.push_back(CachedEvidence {
            fingerprint: prepared.canonical_fingerprint.clone(),
            outcome: outcome.clone(),
        });
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
            cancellation,
        )?;
        Ok(ExecutedTool {
            output: page.output,
            dependencies: vec![page.dependency],
            mutations: Vec::new(),
            bytes_read: page.bytes_read,
            synced_text: None,
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
        let page_len = page.entries.len();
        let mut output = page
            .entries
            .iter()
            .map(|entry| entry.display().to_string())
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
            output,
            dependencies: vec![page.dependency],
            mutations: Vec::new(),
            bytes_read: page.bytes_read,
            synced_text: None,
        })
    }

    fn execute_search(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Search {
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
            *offset,
            *max_hits,
            cursor.as_deref(),
            cancellation,
        )?;
        let output = search::format_search_batch_page(&page, &prepared.canonical_workspace);
        Ok(ExecutedTool {
            output,
            dependencies: vec![page.dependency],
            mutations: Vec::new(),
            bytes_read: page.bytes_read,
            synced_text: None,
        })
    }

    fn execute_write(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Write { content, expected } = &prepared.arguments else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match write".into(),
            }
            .into());
        };
        let path = prepared_path(prepared)?;
        let precondition = expected.clone().map(FilePrecondition::ExactText);
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolError::Cancelled.into());
        }
        let written = write::write_file_with_receipt(&path, content, precondition)?;
        self.services.read.invalidate(&path);
        Ok(ExecutedTool {
            output: "written".into(),
            dependencies: written.dependency.into_iter().collect(),
            mutations: vec![MutationObservation {
                path,
                before_content_digest: written
                    .before
                    .as_deref()
                    .map(|value| digest_bytes(b"slim-written-content-v1", value.as_bytes())),
                after: written.after,
            }],
            bytes_read: written.bytes_read,
            synced_text: None,
        })
    }

    fn execute_patch(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Patch {
            expected,
            replacement,
        } = &prepared.arguments
        else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match patch".into(),
            }
            .into());
        };
        let path = prepared_path(prepared)?;
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolError::Cancelled.into());
        }
        let content = patch::apply_exact_patch_with_content(&path, expected, replacement)?;
        self.services.read.invalidate(&path);
        let before_digest = content.before_digest;
        Ok(ExecutedTool {
            output: "patched".into(),
            dependencies: vec![content.dependency],
            mutations: vec![MutationObservation {
                path,
                before_content_digest: Some(before_digest),
                after: content.stamp,
            }],
            bytes_read: content.bytes_read,
            synced_text: Some(content.text),
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
            timeout_ms,
        } = &prepared.arguments
        else {
            return Err(ToolError::InvalidInput {
                message: "prepared arguments do not match shell".into(),
            }
            .into());
        };
        let result = shell::run_shell_timeout_cancellable_with_progress_and_runner(
            &self.services.process_runner,
            &prepared.canonical_workspace,
            command,
            std::time::Duration::from_millis(*timeout_ms),
            cancellation,
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
        Ok(ExecutedTool::output(format!(
            "{}\nstdout:\n{}stderr:\n{}",
            format_shell_status_header(
                result.output.status.code(),
                result.timed_out,
                result.cancelled,
            ),
            cap_shell_stream(&result.output.stdout, result.stdout_discarded_bytes),
            cap_shell_stream(&result.output.stderr, result.stderr_discarded_bytes),
        )))
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
            "Read a UTF-8 text file",
            json!({"path": {"type": "string", "description": "Workspace-relative path."}, "max_lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES_CAP}, "offset": {"type": "integer", "minimum": 1}}),
            json!(["path"]),
        ),
        "list" => (
            "List directory entries with bounded pagination",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path. Omit, leave empty, or use \".\" for the workspace root."},
                "max_entries": {"type": "integer", "minimum": 1, "maximum": MAX_ENTRIES_CAP},
                "offset": {"type": "integer", "minimum": 1},
                "cursor": {"type": "string", "description": "Opaque cursor from the previous page. Omit or leave empty to start a new list."}
            }),
            json!([]),
        ),
        "search" => (
            "Search UTF-8 files for literal text; pass exactly one of query or patterns. Multiple patterns share one scan and each hit identifies its pattern. Results use snapshot cursors.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path."},
                "query": {"type": "string"},
                "patterns": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 32},
                "max_hits": {"type": "integer", "minimum": 1, "maximum": MAX_HITS_CAP},
                "offset": {"type": "integer", "minimum": 1},
                "cursor": {"type": "string", "description": "Opaque cursor returned by the previous page."}
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
            "Write a UTF-8 text file",
            json!({"path": {"type": "string", "description": "Workspace-relative path."}, "content": {"type": "string"}, "expected": {"type": "string"}}),
            json!(["path", "content"]),
        ),
        "patch" => (
            "Replace one exact text occurrence in a file",
            json!({"path": {"type": "string", "description": "Workspace-relative path."}, "expected": {"type": "string"}, "replacement": {"type": "string"}}),
            json!(["path", "expected", "replacement"]),
        ),
        "shell" => (
            "Run a shell command in the workspace. Default timeout is 30000 ms; use longer values only for deliberate builds or tests.",
            json!({"command": {"type": "string"}, "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_MS, "default": 30000, "description": "Maximum runtime in milliseconds."}}),
            json!(["command"]),
        ),
        _ => ("Slim tool", json!({}), json!([])),
    };
    json!({
        "name": name,
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }
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
        ToolError::StaleRead { path } => format!("stale read: {path}"),
        ToolError::PreconditionRequired { path } => format!("precondition required: {path}"),
        ToolError::MatchCount { count } => format!("expected exactly one match, got {count}"),
        ToolError::InvalidInput { message } => message,
    }
}

#[cfg(test)]
mod timeout_bound_tests {
    use super::{bounded_shell_timeout_ms, tool_definition, MAX_SHELL_TIMEOUT_MS};

    #[test]
    fn shell_timeout_is_clamped_and_advertised() {
        assert_eq!(bounded_shell_timeout_ms(1), 1);
        assert_eq!(
            bounded_shell_timeout_ms(MAX_SHELL_TIMEOUT_MS),
            MAX_SHELL_TIMEOUT_MS
        );
        assert_eq!(
            bounded_shell_timeout_ms(MAX_SHELL_TIMEOUT_MS + 1),
            MAX_SHELL_TIMEOUT_MS
        );
        assert_eq!(bounded_shell_timeout_ms(u64::MAX), MAX_SHELL_TIMEOUT_MS);
        assert_eq!(
            tool_definition("shell")["input_schema"]["properties"]["timeout_ms"]["maximum"],
            MAX_SHELL_TIMEOUT_MS
        );
    }
}

#[cfg(test)]
mod evidence_cache_tests {
    use super::*;
    use std::fs;

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
