mod code_intel;
mod execution;
mod list;
pub mod outline;
mod patch;
mod read;
mod schema;
mod search;
mod shell;
mod validation_cache;
mod write;

pub(crate) use shell::normalize_shell_text;

use crate::context::{ArtifactHandle, ArtifactStore};
use crate::process::{
    ExecutableResolver, ProcessExecutionFacts, ProcessOutputBudget, ProcessRunner,
};
use crate::runtime::CancellationToken;
use crate::OperatingMode;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

pub(crate) use execution::{
    canonical_workspace, dependency_key, digest_bytes, hash_fields, path_identity,
    present_unstructured, CodeIntelPresentation, DEFAULT_SHELL_YIELD_MS, DependencyKind, DependencyObservation, FastStamp,
    MutationObservation, PreparedToolArguments, PreparedToolInvocation, ToolExecutionError,
    ToolExecutionOutcome, ToolExecutionReceipt, ToolPresentationSource,
};

pub(crate) use code_intel::presentation_for_code_intel;
pub use code_intel::{
    code_intel_definition, parse_code_intel_request, render_code_intel, CodeIntelRequest,
    CODE_INTEL_ACTIONS,
};
pub use list::{list_directory, DEFAULT_MAX_ENTRIES, MAX_ENTRIES_CAP};
pub use patch::apply_exact_patch;
pub(crate) use read::page_lines as read_page_lines;
pub use read::{read_file, read_file_range, DEFAULT_MAX_READ_LINES, MAX_READ_LINES_CAP};
pub(crate) use schema::compact_provider_definitions;
pub use search::{
    format_search_page, search_bounded, search_literal, SearchHit, SearchOptions, SearchPage,
    DEFAULT_MAX_HITS, MAX_HITS_CAP,
};
pub(crate) use search::{display_path, is_macos_metadata_file, SKIP_DIR_NAMES};
pub use shell::{
    run_shell, run_shell_timeout, run_shell_timeout_cancellable,
    run_shell_timeout_cancellable_with_progress, ShellProgress, TimedShellOutput,
};
pub use write::{write_file, FilePrecondition, MAX_MUTATING_FILE_BYTES};
pub(crate) use write::link_or_rename_into_gap;

// Bounds the stored summary; tool rows still fit it to the terminal width,
// so this only needs to cover a wide row's command or path.
const ARGUMENT_SUMMARY_LIMIT: usize = 120;
const MAX_EVIDENCE_CACHE: usize = 2_048;
const OBSERVED_WORKSPACE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);
const PREFERRED_ARGUMENT_KEYS: &[&str] = &[
    "path", "command", "query", "pattern", "glob", "url", "file", "target", "name", "id",
];
pub(crate) const MAX_SHELL_TIMEOUT_MS: u64 = 3_600_000;
pub(crate) const DEFAULT_SHELL_TIMEOUT_MS: u64 = 600_000;

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
    if matches!(name, "codemode" | "batch_code") {
        // The row target is the program's first statement, past comments.
        if let Some(code) = serde_json::from_str::<Value>(arguments)
            .ok()
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str)
        {
            return bound_argument_summary(skip_js_leading_comments(code));
        }
        return summarize_tool_arguments(arguments);
    }
    if name == "search" {
        // The row target is the searched text, not the scope (TUI spec §11.4).
        return summarize_search_arguments(arguments)
            .unwrap_or_else(|| summarize_tool_arguments(arguments));
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
        None => DEFAULT_SHELL_TIMEOUT_MS,
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

/// Line-level size of a `patch` call as `(added, removed)`, from its arguments.
/// Lines shared at the start and end of each edit's `expected`/`replacement`
/// are not counted, matching what a line diff of that edit reports. Other
/// tools, malformed arguments and edits without both strings yield `None`.
pub fn edit_line_stats(name: &str, arguments: &str) -> Option<(usize, usize)> {
    if name != "patch" {
        return None;
    }
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    if let (Some(added), Some(removed)) = (
        value.get("added_lines").and_then(Value::as_u64),
        value.get("removed_lines").and_then(Value::as_u64),
    ) {
        return Some((added as usize, removed as usize));
    }
    let single;
    let edits: &[Value] = match value.get("edits") {
        Some(Value::Array(edits)) => edits,
        // Admission normalizes a single edit object to a one-item array.
        Some(edit @ Value::Object(_)) => {
            single = [edit.clone()];
            &single
        }
        _ => {
            single = [value.clone()];
            &single
        }
    };
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut counted = false;
    let mut count_edits = |slice: &[Value]| {
        for edit in slice {
            let (Some(expected), Some(replacement)) = (
                edit.get("expected").and_then(Value::as_str),
                edit.get("replacement").and_then(Value::as_str),
            ) else {
                continue;
            };
            let old: Vec<&str> = expected.lines().collect();
            let new: Vec<&str> = replacement.lines().collect();
            let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(new[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            removed += old.len() - prefix - suffix;
            added += new.len() - prefix - suffix;
            counted = true;
        }
    };
    count_edits(edits);
    if let Some(files) = value.get("files").and_then(Value::as_array) {
        for file_entry in files {
            match file_entry.get("edits") {
                Some(Value::Array(file_edits)) => count_edits(file_edits),
                Some(edit @ Value::Object(_)) => count_edits(std::slice::from_ref(edit)),
                _ => {}
            }
        }
    }
    counted.then_some((added, removed))
}

/// Advances past leading whitespace, `//` line comments and `/* ... */` block
/// comments so the row target names the first statement.
fn skip_js_leading_comments(mut rest: &str) -> &str {
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("//") {
            match after.find('\n') {
                Some(end) => rest = &after[end + 1..],
                None => return "",
            }
        } else if let Some(after) = rest.strip_prefix("/*") {
            match after.find("*/") {
                Some(end) => rest = &after[end + 2..],
                None => return "",
            }
        } else {
            return rest;
        }
    }
}

/// First non-blank line bounded to `limit` characters; `…` marks both a cut
/// line and dropped later lines.
fn bound_argument_summary_exact(text: &str, limit: usize) -> String {
    let mut lines = text
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty());
    let first = lines.next().unwrap_or("");
    let more_lines = lines.any(|line| !line.is_empty());
    let mut chars = first.chars();
    let mut out = chars.by_ref().take(limit).collect::<String>();
    let cut = chars.next().is_some();
    if (cut || more_lines) && limit > 0 {
        if cut {
            out.pop();
        }
        out.push('…');
    }
    out
}

fn summarize_search_arguments(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    let text = match value.get("query").and_then(Value::as_str) {
        Some(query) if !query.is_empty() => query.to_owned(),
        _ => {
            let patterns = value
                .get("patterns")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .filter(|pattern| !pattern.is_empty())
                .collect::<Vec<_>>();
            if patterns.is_empty() {
                return None;
            }
            patterns.join(" | ")
        }
    };
    Some(bound_argument_summary(&format!("query={text}")))
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
    bound_argument_summary_exact(text, ARGUMENT_SUMMARY_LIMIT)
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
    artifact_store: Option<ArtifactStore>,
    pub(crate) sensitive_values: Arc<[String]>,
    pub(crate) process_observer: Option<crate::process::ProcessObserver>,
}

#[derive(Clone, Debug)]
struct CachedEvidence {
    fingerprint: String,
    workspace_revision: Option<u64>,
    fs_epoch: Option<u64>,
    observed_snapshot_hash: Option<u64>,
    cached_at: Instant,
    last_used: u64,
    dep_identities: Vec<String>,
    outcome: Arc<ToolExecutionOutcome>,
}

/// Computes a fast FNV-1a snapshot of direct entries (names, lengths, and mtime nanos)
/// inside observed directories (capped at 64 entries) so `ObservedWorkspace` cache
/// lookups detect external `fs::write` edits immediately without waiting for async
/// FSEvents callbacks.
fn observed_workspace_snapshot_hash(workspace: &Path, outcome: &ToolExecutionOutcome) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut dirs = Vec::with_capacity(outcome.receipt.dependencies.len() + 1);
    dirs.push(workspace);
    for dep in &outcome.receipt.dependencies {
        if dep.stamp.kind == DependencyKind::Directory && dep.path != workspace {
            dirs.push(&dep.path);
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut count = 0_usize;
    for dir in dirs {
        dir.hash(&mut hasher);
        let dir_meta = std::fs::metadata(dir).ok()?;
        let dir_mtime = dir_meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());
        dir_mtime.hash(&mut hasher);
        let rd = std::fs::read_dir(dir).ok()?;
        let mut entries: Vec<(std::ffi::OsString, u64, Option<u128>)> = Vec::new();
        for item in rd {
            count += 1;
            if count > 64 {
                return None;
            }
            let entry = item.ok()?;
            let meta = entry.metadata().ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos());
            entries.push((entry.file_name(), meta.len(), mtime));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.hash(&mut hasher);
    }
    Some(hasher.finish())
}

#[derive(Debug, Default)]
struct EvidenceLruCache {
    entries: HashMap<String, CachedEvidence>,
    by_path: HashMap<String, HashSet<String>>,
    clock: u64,
}

impl EvidenceLruCache {
    fn get_touch(&mut self, fingerprint: &str) -> Option<&CachedEvidence> {
        if !self.entries.contains_key(fingerprint) {
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        if let Some(entry) = self.entries.get_mut(fingerprint) {
            entry.last_used = clock;
            return Some(entry);
        }
        None
    }

    fn insert(&mut self, mut entry: CachedEvidence) {
        self.remove(&entry.fingerprint);
        while self.entries.len() >= MAX_EVIDENCE_CACHE {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.clock = self.clock.saturating_add(1);
        entry.last_used = self.clock;
        let fp = entry.fingerprint.clone();
        for id in &entry.dep_identities {
            self.by_path.entry(id.clone()).or_default().insert(fp.clone());
        }
        self.entries.insert(fp, entry);
    }

    fn remove(&mut self, fingerprint: &str) {
        if let Some(removed) = self.entries.remove(fingerprint) {
            for id in &removed.dep_identities {
                if let Some(set) = self.by_path.get_mut(id) {
                    set.remove(fingerprint);
                    if set.is_empty() {
                        self.by_path.remove(id);
                    }
                }
            }
        }
    }

    fn invalidate_observed_workspace(&mut self) {
        let keys: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.workspace_revision.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.remove(&key);
        }
    }

    fn invalidate_for_identities(&mut self, identities: &[String]) {
        let mut to_remove: Vec<String> = Vec::new();
        for id in identities {
            if let Some(keys) = self.by_path.get(id) {
                for key in keys {
                    to_remove.push(key.clone());
                }
            }
        }
        for key in to_remove {
            self.remove(&key);
        }
        self.invalidate_observed_workspace();
    }
}

#[derive(Debug)]
struct ToolServices {
    process_runner: ProcessRunner,
    read: read::ReadService,
    list: list::ListService,
    search: search::SearchService,
    workspace_revision: AtomicU64,
    evidence_cache: Mutex<EvidenceLruCache>,
}

impl ToolServices {
    fn new(process_runner: ProcessRunner) -> Self {
        Self {
            search: search::SearchService::default(),
            process_runner,
            read: read::ReadService::default(),
            list: list::ListService::default(),
            workspace_revision: AtomicU64::new(0),
            evidence_cache: Mutex::new(EvidenceLruCache::default()),
        }
    }
}

pub(crate) use crate::sync::lock_mutex;

pub(super) fn check_cancellation(cancellation: Option<&CancellationToken>) -> Result<(), ToolError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

/// Process-unique id of a paged snapshot (`list`, `search`): `prefix-pid-seq`.
pub(super) fn new_snapshot_id(prefix: &str) -> String {
    let sequence = NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:x}-{sequence:x}", std::process::id())
}

/// Splits a `<snapshot id>:<hex position>` cursor; the position is 1-based.
pub(super) fn parse_cursor(cursor: &str) -> Option<(&str, usize)> {
    let (id, offset) = cursor.rsplit_once(':')?;
    let offset = usize::from_str_radix(offset, 16).ok()?;
    (offset > 0 && !id.is_empty()).then_some((id, offset))
}

fn mismatched_arguments(tool: &str) -> ToolExecutionError {
    ToolError::InvalidInput {
        message: format!("prepared arguments do not match {tool}"),
    }
    .into()
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
    /// Media the model sees next to `output` (MCP image content). Only
    /// provider wires that accept images in tool results receive it.
    pub media: Vec<crate::provider::ProviderContentBlock>,
}

impl ToolResult {
    pub(crate) fn new(name: impl Into<String>, success: bool, output: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            success,
            output: output.into(),
            artifact: None,
            media: Vec::new(),
        }
    }

    pub(crate) fn ok(name: impl Into<String>, output: impl Into<String>) -> Self {
        Self::new(name, true, output)
    }

    pub(crate) fn fail(name: impl Into<String>, output: impl Into<String>) -> Self {
        Self::new(name, false, output)
    }
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
    artifact: Option<ArtifactHandle>,
    dependencies: Vec<DependencyObservation>,
    mutations: Vec<MutationObservation>,
    bytes_read: u64,
    synced_text: Option<crate::codeintel::CodeIntelFileUpdate>,
    edit_diff: Option<crate::ToolEditDiff>,
    process: Option<ProcessExecutionFacts>,
    presentation: Option<ToolPresentationSource>,
    rollback_snapshots: Vec<(PathBuf, String)>,
}

impl ExecutedTool {
    fn output(output: String, success: bool) -> Self {
        Self {
            output,
            success,
            artifact: None,
            dependencies: Vec::new(),
            mutations: Vec::new(),
            bytes_read: 0,
            synced_text: None,
            edit_diff: None,
            process: None,
            presentation: None,
            rollback_snapshots: Vec::new(),
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
            artifact_store: None,
            sensitive_values: Arc::from([]),
            process_observer: None,
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
    pub(crate) fn job_artifacts(&self) -> Option<ArtifactStore> {
        self.artifact_store.clone()
    }

    pub(crate) fn configure_artifacts(
        &mut self,
        store: Option<ArtifactStore>,
        sensitive_values: &[String],
    ) {
        self.artifact_store = store;
        self.sensitive_values = Arc::from(sensitive_values);
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

    pub(crate) fn remember_complete_read_digest(&self, path: &Path, sha256: [u8; 32]) {
        self.services.read.remember_complete_digest(path, sha256);
    }

    pub(crate) fn prepare_invocation_value(
        &self,
        mode: OperatingMode,
        cwd: &Path,
        workspace: &Result<PathBuf, String>,
        name: &str,
        arguments: Value,
    ) -> PreparedToolInvocation {
        PreparedToolInvocation::from_workspace_value(
            mode,
            cwd,
            workspace,
            name,
            Ok(arguments),
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
        let workspace = canonical_workspace(cwd);
        calls
            .iter()
            .map(|(name, arguments)| {
                PreparedToolInvocation::from_workspace(
                    mode,
                    cwd,
                    &workspace,
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
            .filter(|spec| mode.allows_mutation() || !spec.operational().mutates_workspace())
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
            // Code mode reaches the same native tools, from inside `codemode`.
            OperatingMode::Auto | OperatingMode::Code => Arc::clone(&MODE_DEFINITIONS_AUTO),
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
        let mut result = if !self
            .names_for_mode(prepared.mode)
            .contains(&prepared.name.as_str())
        {
            Err(ToolExecutionError::from(ToolError::InvalidInput {
                message: if prepared.spec.is_none() {
                    format!(
                        "unknown tool: {}; registered native tools allowed in {} mode: {}",
                        prepared.name.chars().take(64).collect::<String>(),
                        crate::runtime::mode_name(prepared.mode),
                        self.names_for_mode(prepared.mode).join(", ")
                    )
                } else {
                    format!(
                        "tool unavailable in {} mode",
                        crate::runtime::mode_name(prepared.mode)
                    )
                },
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
        let mut fused_shell = None;
        let mut fused_effects_uncertain = false;
        let then_run = match &prepared.arguments {
            PreparedToolArguments::Write { then_run, .. }
            | PreparedToolArguments::Patch { then_run, .. } => then_run.as_deref(),
            _ => None,
        }
        // A batch with rejected edits is reported, not checked.
        .filter(|_| !matches!(&result, Ok(edited) if !edited.success));
        if let (Ok(edited), Some(shell_call)) = (&mut result, then_run) {
            on_progress(ToolExecutionProgress {
                preview: "Edit applied; running then_run".into(),
            });
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                edited.success = false;
                edited
                    .output
                    .push_str("\nthen_run: not executed (cancelled after edit)");
            } else {
                let shell = self
                    .execute_shell(shell_call, cancellation, &mut on_progress)
                    .unwrap_or_else(|failure| {
                        fused_effects_uncertain = failure.effects_uncertain;
                        let mut output = tool_error_message(failure.error);
                        if let Some(context) = failure.context {
                            output.push('\n');
                            output.push_str(&context);
                        }
                        ExecutedTool::output(output, false)
                    });
                for path in &prepared.target_paths {
                    self.services.read.invalidate(path);
                }
                self.invalidate_cached_evidence_for_paths(&prepared.target_paths);
                if shell.success {
                    edited.output.push_str(THEN_RUN_PASSED);
                } else if !edited.rollback_snapshots.is_empty() {
                    match patch::restore_patched_files_atomic(&edited.rollback_snapshots) {
                        Ok(()) => {
                            for (path, _) in &edited.rollback_snapshots {
                                self.services.read.invalidate(path);
                            }
                            edited.synced_text = None;
                            edited.edit_diff = None;
                            edited.mutations.clear();
                            self.services
                                .workspace_revision
                                .fetch_add(1, Ordering::AcqRel);
                            self.invalidate_observed_workspace_evidence(
                                &prepared.canonical_workspace,
                            );
                            edited.output.push_str(
                                "\nthen_run: failed\n[rollback] reverted patched file(s) after then_run failed\n",
                            );
                        }
                        Err(err) => {
                            fused_effects_uncertain = true;
                            edited.output.push_str(&format!(
                                "\nthen_run: failed\n[rollback error] {err}\n"
                            ));
                        }
                    }
                } else {
                    edited
                        .output
                        .push_str("\nthen_run: failed; edit remains applied\n");
                }
                edited.output.push_str(&shell.output);
                edited.success = shell.success;
                edited.process = shell.process;
                edited.artifact = shell.artifact.clone();
                fused_shell = Some(Box::new((
                    shell_call.clone(),
                    ToolResult {
                        name: "shell".into(),
                        success: shell.success,
                        output: shell.output,
                        artifact: shell.artifact,
                        media: Vec::new(),
                    },
                )));
            }
        }
        let execution_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let finalization_started = Instant::now();
        let effects_uncertain = fused_effects_uncertain
            || result
                .as_ref()
                .err()
                .is_some_and(|failure| failure.effects_uncertain);
        let (
            result,
            dependencies,
            mutations,
            bytes_read,
            synced_text,
            edit_diff,
            process,
            presentation,
        ) = match result {
            Ok(executed) => (
                ToolResult {
                    name: prepared.name.clone(),
                    success: executed.success,
                    output: executed.output,
                    artifact: executed.artifact,
                    media: Vec::new(),
                },
                executed.dependencies,
                executed.mutations,
                executed.bytes_read,
                executed.synced_text,
                executed.edit_diff,
                executed.process,
                executed.presentation,
            ),
            Err(failure) => {
                let mut output = workspace_relative_text(
                    &tool_error_message(failure.error),
                    &prepared.canonical_workspace,
                );
                if let Some(context) = failure.context {
                    output.push('\n');
                    output.push_str(&context);
                }
                // A file tool aimed at a file that is not there: the name
                // often exists in another directory. Only that failure: a
                // write that creates a file (no `expected`) or one that failed
                // for another reason (size, encoding) is not a wrong directory.
                let names_existing_file = match &prepared.arguments {
                    PreparedToolArguments::Read { .. } | PreparedToolArguments::Patch { .. } => {
                        true
                    }
                    PreparedToolArguments::Write { expected, .. } => expected.is_some(),
                    _ => false,
                };
                let same_name = prepared
                    .target_paths
                    .first()
                    .filter(|path| {
                        names_existing_file && !path.exists() && output.contains("does not exist")
                    })
                    .and_then(|path| {
                        crate::workspace_files::same_name_files_note(
                            &prepared.canonical_workspace,
                            path,
                            cancellation,
                        )
                    });
                if let Some(note) = same_name {
                    output.push('\n');
                    output.push_str(&note);
                }
                (
                    ToolResult::fail(prepared.name.clone(), output),
                    failure.dependencies,
                    failure.mutations,
                    failure.bytes_read,
                    None,
                    None,
                    None,
                    None,
                )
            }
        };
        let changed = effects_uncertain || mutations.iter().any(MutationObservation::changed);
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
                execution_us,
                finalization_us,
                synced_text,
                edit_diff,
                process,
                fused_shell,
                presentation,
            },
        };
        if changed {
            self.invalidate_observed_workspace_evidence(&prepared.canonical_workspace);
        }
        if (outcome.result.success
            || effects_uncertain
            || outcome
                .receipt
                .mutations
                .iter()
                .any(MutationObservation::changed))
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
        let observed_workspace = prepared
            .spec
            .is_some_and(|spec| spec.dependency_scope == ToolDependencyScope::ObservedWorkspace);
        let current_revision = observed_workspace.then(|| self.workspace_revision());
        let daemon = observed_workspace
            .then(|| crate::project_index::for_workspace(&prepared.canonical_workspace).ok())
            .flatten();
        let current_fs_epoch = daemon.as_ref().map(|d| d.fs_epoch());
        let watcher_active = daemon.as_ref().is_some_and(|d| d.watcher_active());
        // Retain the candidate, then release the shared cache before filesystem
        // validation and copying the output. Independent reads must not serialize
        // their I/O behind this mutex. This is still a fresh validation per hit.
        let (outcome, observed_snapshot_hash) = {
            let mut cache = lock_mutex(&self.services.evidence_cache);
            let entry = cache.get_touch(&prepared.canonical_fingerprint)?;
            let valid = match current_revision {
                Some(rev) => {
                    entry.workspace_revision == Some(rev)
                        && entry.fs_epoch == current_fs_epoch
                        && (watcher_active
                            || entry.cached_at.elapsed() < OBSERVED_WORKSPACE_CACHE_TTL)
                }
                None => entry.workspace_revision.is_none(),
            };
            if !valid {
                return None;
            }
            (Arc::clone(&entry.outcome), entry.observed_snapshot_hash)
        };
        if !outcome.receipt.dependencies.iter().all(|dep| {
            if current_revision.is_some()
                && dep.stamp.kind == DependencyKind::Directory
                && dep.stamp.modified_nanos.is_none()
            {
                dep.path.exists()
            } else {
                dep.stamp_matches()
            }
        }) {
            return None;
        }
        if current_revision.is_some() {
            let expected = observed_snapshot_hash?;
            let current = observed_workspace_snapshot_hash(&prepared.canonical_workspace, &outcome)?;
            if expected != current {
                return None;
            }
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
        let observed_workspace = prepared
            .spec
            .is_some_and(|spec| spec.dependency_scope == ToolDependencyScope::ObservedWorkspace);
        let workspace_revision = observed_workspace.then(|| self.workspace_revision());
        let fs_epoch = observed_workspace
            .then(|| {
                crate::project_index::for_workspace(&prepared.canonical_workspace)
                    .ok()
                    .map(|d| d.fs_epoch())
            })
            .flatten();
        let dep_identities = outcome
            .receipt
            .dependencies
            .iter()
            .map(|dep| path_identity(&dep.path))
            .collect();
        let observed_snapshot_hash = if observed_workspace {
            observed_workspace_snapshot_hash(&prepared.canonical_workspace, outcome)
        } else {
            None
        };
        let entry = CachedEvidence {
            fingerprint: prepared.canonical_fingerprint.clone(),
            workspace_revision,
            fs_epoch,
            observed_snapshot_hash,
            cached_at: Instant::now(),
            last_used: 0,
            dep_identities,
            outcome: Arc::new(outcome.clone()),
        };
        let mut cache = lock_mutex(&self.services.evidence_cache);
        cache.insert(entry);
    }

    fn invalidate_observed_workspace_evidence(&self, canonical_workspace: &Path) {
        crate::workspace_files::invalidate_workspace_index(canonical_workspace);
        let mut cache = lock_mutex(&self.services.evidence_cache);
        cache.invalidate_observed_workspace();
    }

    fn invalidate_cached_evidence_for_paths(&self, paths: &[PathBuf]) {
        for path in paths {
            if let Some(daemon) = crate::project_index::existing_for_path(path) {
                daemon.notify_local_write(path, None);
            }
        }
        let identities = paths
            .iter()
            .map(|path| path_identity(path))
            .collect::<Vec<_>>();
        if identities.is_empty() {
            return;
        }
        let mut cache = lock_mutex(&self.services.evidence_cache);
        cache.invalidate_for_identities(&identities);
    }

    pub(crate) fn notify_external_paths_changed(
        &self,
        workspace: &Path,
        paths: &[(PathBuf, Option<&[u8]>)],
    ) {
        if paths.is_empty() {
            return;
        }
        let canonical_ws =
            canonical_workspace(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let daemon = crate::project_index::existing_for_workspace(&canonical_ws)
            .or_else(|| crate::project_index::for_workspace(&canonical_ws).ok());
        let mut identities = Vec::with_capacity(paths.len());
        for (path, bytes) in paths {
            self.services.read.invalidate(path);
            let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| {
                path.parent()
                    .and_then(|parent| std::fs::canonicalize(parent).ok())
                    .zip(path.file_name())
                    .map_or_else(|| path.clone(), |(parent, name)| parent.join(name))
            });
            if let Some(d) = daemon
                .as_ref()
                .cloned()
                .or_else(|| crate::project_index::existing_for_path(&canonical_path))
            {
                if let Some(data) = bytes {
                    d.notify_local_write(&canonical_path, Some(data));
                } else {
                    d.notify_local_delete(&canonical_path);
                }
            }
            identities.push(path_identity(&canonical_path));
        }
        self.services
            .workspace_revision
            .fetch_add(1, Ordering::AcqRel);
        self.invalidate_observed_workspace_evidence(&canonical_ws);
        let mut cache = lock_mutex(&self.services.evidence_cache);
        cache.invalidate_for_identities(&identities);
    }

    fn execute_read(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Read {
            offset,
            max_lines,
            outline,
            raw,
            ranges,
            symbols,
        } = &prepared.arguments
        else {
            return Err(mismatched_arguments("read"));
        };
        let path = prepared_path(prepared)?;
        let map_read_err = |mut failure: ToolExecutionError| {
            failure.context = failure.context.map(|context| {
                workspace_relative_text(&context, &prepared.canonical_workspace)
            });
            failure
        };
        if *outline {
            if let Ok(daemon) = crate::project_index::for_workspace(&prepared.canonical_workspace) {
                if let Ok(Some(cached)) = daemon.read_file_cached(&path) {
                    if let Ok(file) = fs::File::open(&path) {
                        if let Ok(metadata) = file.metadata() {
                            if !metadata.is_dir() {
                                let dependency = DependencyObservation {
                                    path: path.clone(),
                                    stamp: FastStamp::from_file(
                                        DependencyKind::File,
                                        &file,
                                        &metadata,
                                        None,
                                    ),
                                };
                                let syms = cached.outline();
                                let output = outline::format_outline(&syms, cached.line_count());
                                return Ok(ExecutedTool {
                                    dependencies: vec![dependency],
                                    bytes_read: cached.text.len() as u64,
                                    presentation: None,
                                    ..ExecutedTool::output(output, true)
                                });
                            }
                        }
                    }
                }
            }
            let (content, dependency, bytes_read) = self
                .services
                .read
                .read_full_utf8_for_outline(&path, cancellation)
                .map_err(map_read_err)?;
            let syms = outline::extract_outline(&path, &content);
            let total_lines = content.lines().count();
            let output = outline::format_outline(&syms, total_lines);
            return Ok(ExecutedTool {
                dependencies: vec![dependency],
                bytes_read,
                presentation: None,
                ..ExecutedTool::output(output, true)
            });
        }
        if !symbols.is_empty() {
            let cached_file = crate::project_index::for_workspace(&prepared.canonical_workspace)
                .ok()
                .and_then(|d| d.read_file_cached(&path).ok().flatten());
            let (content_arc, extracted_arc, dependency, bytes_read) =
                if let Some(cached) = cached_file {
                    if let Ok(file) = fs::File::open(&path) {
                        if let Ok(metadata) = file.metadata() {
                            if !metadata.is_dir() {
                                let dep = DependencyObservation {
                                    path: path.clone(),
                                    stamp: FastStamp::from_file(
                                        DependencyKind::File,
                                        &file,
                                        &metadata,
                                        None,
                                    ),
                                };
                                let len = cached.text.len() as u64;
                                (Arc::clone(&cached.text), cached.outline(), dep, len)
                            } else {
                                let (content, dep, bytes_read) = self
                                    .services
                                    .read
                                    .read_full_utf8_for_outline(&path, cancellation)
                                    .map_err(map_read_err)?;
                                let extracted = Arc::from(outline::extract_outline(&path, &content));
                                (Arc::<str>::from(content), extracted, dep, bytes_read)
                            }
                        } else {
                            let (content, dep, bytes_read) = self
                                .services
                                .read
                                .read_full_utf8_for_outline(&path, cancellation)
                                .map_err(map_read_err)?;
                            let extracted = Arc::from(outline::extract_outline(&path, &content));
                            (Arc::<str>::from(content), extracted, dep, bytes_read)
                        }
                    } else {
                        let (content, dep, bytes_read) = self
                            .services
                            .read
                            .read_full_utf8_for_outline(&path, cancellation)
                            .map_err(map_read_err)?;
                        let extracted = Arc::from(outline::extract_outline(&path, &content));
                        (Arc::<str>::from(content), extracted, dep, bytes_read)
                    }
                } else {
                    let (content, dep, bytes_read) = self
                        .services
                        .read
                        .read_full_utf8_for_outline(&path, cancellation)
                        .map_err(map_read_err)?;
                    let extracted = Arc::from(outline::extract_outline(&path, &content));
                    (Arc::<str>::from(content), extracted, dep, bytes_read)
                };
            let content = content_arc.as_ref();
            let extracted = extracted_arc.as_ref();
            let sym_ranges = outline::find_symbol_ranges(extracted, symbols);
            let mut combined = sym_ranges.clone();
            combined.extend(ranges.iter().copied());
            let merged = outline::merge_line_ranges(combined);
            if merged.is_empty() {
                let sample: Vec<&str> = extracted.iter().take(20).map(|s| s.name.as_str()).collect();
                let hint = if sample.is_empty() {
                    "no symbols extracted from this file".to_owned()
                } else {
                    format!("available symbols: {}", sample.join(", "))
                };
                return Err(ToolExecutionError::observed(
                    ToolError::InvalidInput {
                        message: format!(
                            "no matching symbols ({}) found; {hint}",
                            symbols.join(", ")
                        ),
                    },
                    vec![dependency],
                    bytes_read,
                ));
            }
            let file_lines: Vec<&str> = content.split_inclusive('\n').collect();
            let mut out = String::new();
            for (idx, (start, end)) in merged.iter().enumerate() {
                if *start > file_lines.len() {
                    continue;
                }
                let actual_end = (*end).min(file_lines.len());
                if idx > 0 {
                    out.push('\n');
                }
                let slice: String = file_lines[*start - 1..actual_end].concat();
                out.push_str(&format!("[lines {start}-{actual_end}]\n{slice}"));
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            return Ok(ExecutedTool {
                dependencies: vec![dependency],
                bytes_read,
                presentation: None,
                ..ExecutedTool::output(out, true)
            });
        }
        if !ranges.is_empty() {
            let mut out = String::new();
            let mut deps = Vec::new();
            let mut total_bytes_read = 0u64;
            for (idx, &(start, end)) in ranges.iter().enumerate() {
                let count = end
                    .saturating_sub(start)
                    .saturating_add(1)
                    .min(MAX_READ_LINES_CAP);
                let page = self
                    .services
                    .read
                    .read_file_range_no_digest(&path, start, count, cancellation)
                    .map_err(map_read_err)?;
                total_bytes_read = total_bytes_read.saturating_add(page.bytes_read);
                if idx == 0 {
                    deps.push(page.dependency);
                } else {
                    out.push('\n');
                }
                let line_count = page.records.len();
                if line_count == 0 {
                    out.push_str(&format!("[lines {start}-{end}: empty/beyond EOF]\n"));
                } else {
                    let actual_end = page.first_line.saturating_add(line_count).saturating_sub(1);
                    out.push_str(&format!(
                        "[lines {}-{actual_end}]\n{}",
                        page.first_line, page.output
                    ));
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            return Ok(ExecutedTool {
                dependencies: deps,
                bytes_read: total_bytes_read,
                presentation: None,
                ..ExecutedTool::output(out, true)
            });
        }
        let in_code_mode = prepared.mode == OperatingMode::Code;
        if !in_code_mode
            && !*raw
            && *offset <= 1
            && max_lines.is_none()
            && ranges.is_empty()
            && symbols.is_empty()
        {
            if let Some((overview, dependency, bytes_read)) = self
                .services
                .read
                .smart_unbounded_overview(&path, cancellation)
                .map_err(map_read_err)?
            {
                return Ok(ExecutedTool {
                    dependencies: vec![dependency],
                    bytes_read,
                    presentation: None,
                    ..ExecutedTool::output(overview, true)
                });
            }
        }
        let effective_max_lines = if (in_code_mode || *raw) && max_lines.is_none() && *offset <= 1 {
            Some(MAX_READ_LINES_CAP)
        } else {
            *max_lines
        };
        let page = self
            .services
            .read
            .read_file_range_resolved(&path, *offset, effective_max_lines, false, cancellation)
            .map_err(map_read_err)?;
        let model_output = self
            .services
            .read
            .maybe_compact_delta(
                &path,
                page.first_line,
                &page.records,
                page.output.len(),
                *raw || in_code_mode,
            )
            .unwrap_or_else(|| page.output.clone());
        let presentation = Some(ToolPresentationSource::Read {
            prefix: String::new(),
            full: page.output,
            first: page.first_line,
            records: page.records,
            next_offset: page.next_offset,
        });
        Ok(ExecutedTool {
            dependencies: vec![page.dependency],
            bytes_read: page.bytes_read,
            presentation,
            ..ExecutedTool::output(model_output, true)
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
            return Err(mismatched_arguments("list"));
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
        let mut output = (0..page_len)
            .map(|index| page.label(index, &prepared.canonical_workspace))
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
            dependencies: vec![page.dependency],
            bytes_read: page.bytes_read,
            presentation,
            ..ExecutedTool::output(output, true)
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
            glob,
        } = &prepared.arguments
        else {
            return Err(mismatched_arguments("search"));
        };
        let path = prepared_path(prepared)?;
        let page = self.services.search.page_in(
            &path,
            patterns.clone(),
            search::SearchPageOptions {
                offset: *offset,
                max_hits: *max_hits,
                context_lines: *context_lines,
            },
            cursor.as_deref(),
            cancellation,
            glob.as_deref(),
        )?;
        let output = search::format_search_batch_page(&page, &prepared.canonical_workspace);
        let presentation = Some(ToolPresentationSource::Search {
            prefix: String::new(),
            page: page.clone(),
            display_root: prepared.canonical_workspace.clone(),
        });
        Ok(ExecutedTool {
            dependencies: vec![page.dependency],
            bytes_read: page.bytes_read,
            presentation,
            ..ExecutedTool::output(output, true)
        })
    }

    fn execute_write(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
        on_progress: &mut impl FnMut(ToolExecutionProgress),
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Write {
            content, expected, ..
        } = &prepared.arguments
        else {
            return Err(mismatched_arguments("write"));
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
        let display = workspace_display(&path, &prepared.canonical_workspace);
        Ok(ExecutedTool {
            success: true,
            output: format!(
                "written {}; bytes={}; sha256={}; exists=true; do not re-read{}{}",
                display.display(),
                written.after.len,
                written.written_sha256_12,
                written
                    .recovery_note
                    .as_ref()
                    .map(|note| {
                        format!(
                            "; {}",
                            workspace_relative_text(note, &prepared.canonical_workspace)
                        )
                    })
                    .unwrap_or_default(),
                written
                    .syntax_diagnostic
                    .as_ref()
                    .map(|diagnostic| format!("\n{diagnostic}"))
                    .unwrap_or_default()
            ),
            artifact: None,
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
            edit_diff: None,
            process: None,
            presentation: None,
            rollback_snapshots: Vec::new(),
        })
    }

    fn execute_patch(
        &self,
        prepared: &PreparedToolInvocation,
        cancellation: Option<&CancellationToken>,
        on_progress: &mut impl FnMut(ToolExecutionProgress),
    ) -> Result<ExecutedTool, ToolExecutionError> {
        let PreparedToolArguments::Patch {
            edits,
            extra_files,
            rollback_on_check_fail,
            then_run,
        } = &prepared.arguments
        else {
            return Err(mismatched_arguments("patch"));
        };
        let path = prepared_path(prepared)?;
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolError::Cancelled.into());
        }
        let capture_originals = *rollback_on_check_fail && then_run.is_some();
        if extra_files.is_empty() {
            let mut content = patch::apply_exact_patches_with_content_and_original(
                &path,
                workspace_display(&path, &prepared.canonical_workspace),
                edits,
                capture_originals,
                cancellation,
                &mut || {
                    on_progress(ToolExecutionProgress {
                        preview: "Waiting for file lock".into(),
                    })
                },
            )?;
            self.services
                .read
                .remember_complete_digest(&path, content.written_digest);
            let before_digest = content.before_digest;
            let edit_diff = (!content.hunks.is_empty()).then(|| crate::ToolEditDiff {
                path: workspace_display(&path, &prepared.canonical_workspace)
                    .display()
                    .to_string(),
                hunks: content.hunks,
                truncated: content.hunks_truncated,
            });
            let rollback_snapshots = content
                .original_content
                .take()
                .map(|orig| vec![(path.clone(), orig)])
                .unwrap_or_default();
            if content.partial && then_run.is_some() {
                content.summary.push_str("\nthen_run: not executed (edits were rejected)");
            }
            return Ok(ExecutedTool {
                edit_diff,
                success: !content.partial,
                // The summary may carry a displaced-version note with its path.
                output: workspace_relative_text(&content.summary, &prepared.canonical_workspace),
                artifact: None,
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
                rollback_snapshots,
            });
        }

        let mut targets = Vec::with_capacity(1 + extra_files.len());
        targets.push((
            path.clone(),
            workspace_display(&path, &prepared.canonical_workspace).to_path_buf(),
            edits.clone(),
        ));
        for (extra_path, extra_edits) in extra_files {
            targets.push((
                extra_path.clone(),
                workspace_display(extra_path, &prepared.canonical_workspace).to_path_buf(),
                extra_edits.clone(),
            ));
        }
        let mut outcomes = patch::apply_atomic_multi_file_patches(
            &targets,
            capture_originals,
            cancellation,
            &mut || {
                on_progress(ToolExecutionProgress {
                    preview: "Waiting for file lock".into(),
                })
            },
        )?;
        let mut dependencies = Vec::with_capacity(outcomes.len());
        let mut mutations = Vec::with_capacity(outcomes.len());
        let mut bytes_read = 0u64;
        let mut summaries = Vec::with_capacity(outcomes.len());
        let mut rollback_snapshots = if capture_originals {
            Vec::with_capacity(outcomes.len())
        } else {
            Vec::new()
        };
        for item in &mut outcomes {
            self.services
                .read
                .remember_complete_digest(&item.path, item.content.written_digest);
            dependencies.push(item.content.dependency.clone());
            mutations.push(MutationObservation {
                path: item.path.clone(),
                before_content_digest: (!item.content.displaced_version_preserved)
                    .then(|| item.content.before_digest.clone()),
                after: item.content.stamp.clone(),
            });
            bytes_read = bytes_read.saturating_add(item.content.bytes_read);
            summaries.push(workspace_relative_text(
                &item.content.summary,
                &prepared.canonical_workspace,
            ));
            if let Some(orig) = item.content.original_content.take() {
                rollback_snapshots.push((item.path.clone(), orig));
            }
        }
        let mut first = outcomes.remove(0);
        let edit_diff = (!first.content.hunks.is_empty()).then(|| crate::ToolEditDiff {
            path: workspace_display(&first.path, &prepared.canonical_workspace)
                .display()
                .to_string(),
            hunks: std::mem::take(&mut first.content.hunks),
            truncated: first.content.hunks_truncated,
        });
        let synced_text = Some(crate::codeintel::CodeIntelFileUpdate {
            text: std::mem::take(&mut first.content.text),
            patch: first.content.edits.take().map(|edits| crate::codeintel::CodeIntelPatch {
                before_digest: std::mem::take(&mut first.content.before_digest),
                edits,
            }),
        });
        Ok(ExecutedTool {
            edit_diff,
            success: true,
            output: format!(
                "atomic multi-file patch ({} files):\n{}",
                summaries.len(),
                summaries.join("\n")
            ),
            artifact: None,
            dependencies,
            mutations,
            bytes_read,
            synced_text,
            process: None,
            presentation: None,
            rollback_snapshots,
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
            stdin,
            no_cache,
            ..
        } = &prepared.arguments
        else {
            return Err(mismatched_arguments("shell"));
        };
        let can_use_validation_cache = !*no_cache && !*stdin && self.process_observer.is_none();
        if can_use_validation_cache {
            if let Some(hit) = validation_cache::try_lookup(
                &prepared.canonical_workspace,
                command,
                args.as_deref(),
            ) {
                return Ok(ExecutedTool::output(hit.output, true));
            }
        }
        let started_at = Instant::now();
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
            ProcessOutputBudget::per_stream(if let Some(observer) = &self.process_observer {
                observer.capture_bytes
            } else if self.artifact_store.is_some() {
                SHELL_LOG_CAPTURE_CAP_BYTES
            } else if prepared.mode == OperatingMode::Code {
                CODE_SHELL_STREAM_CAP_BYTES
            } else {
                SHELL_STREAM_CAP_BYTES
            }),
            (
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
                self.process_observer.clone(),
            ),
        )?;
        let success = !result.interrupted
            && result.output.status.success()
            && !result.timed_out
            && !result.cancelled;
        let safe_output = self
            .process_observer
            .as_ref()
            .map(|observer| (observer.redacted_output)());
        let status_header = format_shell_status_header(
            result.output.status.code(),
            result.timed_out,
            result.cancelled,
        );
        let effective_command = match args {
            Some(a) if !a.is_empty() => format!("{command} {}", a.join(" ")),
            _ => command.clone(),
        };
        let stream_cap = if prepared.mode == OperatingMode::Code {
            CODE_SHELL_STREAM_CAP_BYTES
        } else {
            SHELL_STREAM_CAP_BYTES
        };
        let mut output = if safe_output.is_none()
            && (result.output.stdout.len() > stream_cap
                || result.output.stderr.len() > stream_cap)
        {
            let full_out = normalize_shell_text(&String::from_utf8_lossy(&result.output.stdout));
            let full_err = normalize_shell_text(&String::from_utf8_lossy(&result.output.stderr));
            let mut combined = String::with_capacity(full_out.len() + full_err.len() + 64);
            combined.push_str(&status_header);
            combined.push_str("\nstdout:\n");
            combined.push_str(&full_out);
            if !full_out.is_empty() && !full_out.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str("stderr:\n");
            combined.push_str(&full_err);
            if let Some(folded) =
                crate::runtime::shell_fold::fold_shell_output(&effective_command, &combined, success)
            {
                if folded.len() <= stream_cap * 2 {
                    folded
                } else {
                    let mut stdout = clean_shell_stream_with_cap(
                        &result.output.stdout,
                        result.stdout_discarded_bytes,
                        stream_cap,
                    );
                    if !stdout.is_empty() && !stdout.ends_with('\n') {
                        stdout.push('\n');
                    }
                    format!(
                        "{status_header}\nstdout:\n{stdout}stderr:\n{}",
                        clean_shell_stream_with_cap(
                            &result.output.stderr,
                            result.stderr_discarded_bytes,
                            stream_cap,
                        ),
                    )
                }
            } else {
                let mut stdout = clean_shell_stream_with_cap(
                    &result.output.stdout,
                    result.stdout_discarded_bytes,
                    stream_cap,
                );
                if !stdout.is_empty() && !stdout.ends_with('\n') {
                    stdout.push('\n');
                }
                format!(
                    "{status_header}\nstdout:\n{stdout}stderr:\n{}",
                    clean_shell_stream_with_cap(
                        &result.output.stderr,
                        result.stderr_discarded_bytes,
                        stream_cap,
                    ),
                )
            }
        } else {
            let mut stdout = safe_output.clone().unwrap_or_else(|| {
                clean_shell_stream_with_cap(
                    &result.output.stdout,
                    result.stdout_discarded_bytes,
                    stream_cap,
                )
            });
            // The stderr label must start its own line even when stdout does not
            // end with a newline.
            if !stdout.is_empty() && !stdout.ends_with('\n') {
                stdout.push('\n');
            }
            format!(
                "{status_header}\nstdout:\n{stdout}stderr:\n{}",
                if safe_output.is_some() {
                    String::new()
                } else {
                    clean_shell_stream_with_cap(
                        &result.output.stderr,
                        result.stderr_discarded_bytes,
                        stream_cap,
                    )
                },
            )
        };
        if result.interrupted {
            output.push_str(if result.interrupt_escalated {
                "\ninterrupt=forced"
            } else {
                "\ninterrupt=graceful"
            });
        }
        if result.capture_may_be_incomplete {
            output.push_str(if result.timed_out || result.cancelled || result.interrupted {
                "\n[note: pipe capture may be incomplete after interruption]"
            } else {
                "\n[note: the command exited but a process it started kept its output open; that process was stopped and later output is not captured. For a long-lived process use background=true]"
            });
        }
        if args.is_none()
            && shell::script_shell_is_windows_powershell(&self.services.process_runner)
        {
            if let Some(note) = windows_powershell_syntax_note(
                !result.output.status.success(),
                &result.output.stdout,
                &result.output.stderr,
            ) {
                output.push_str(note);
            }
        }
        if result.timed_out {
            output.push_str(
                "\n[note: the command hit its timeout_ms and was stopped; for long work run it \
                 with background=true and follow it with shell_job, or raise timeout_ms]",
            );
        }
        if result.output.status.code().is_some_and(|code| code != 0)
            && !result.timed_out
            && !result.cancelled
            && normalize_shell_text(&String::from_utf8_lossy(&result.output.stderr))
                .chars()
                .all(|character| character.is_ascii_whitespace())
        {
            output.push_str(
                "\n[note: nonzero exit with empty stderr — empty stderr does not imply success; \
                 if this was a grep/diff/--check-style command, its nonzero status may mean \
                 \"no match\"/\"diffs exist\"; otherwise this remains a failed process; \
                 judge by stdout and documented status semantics]",
            );
        }
        let artifact = if let Some(store) = &self.artifact_store {
            if result.output.stdout.len() > stream_cap
                || result.output.stderr.len() > stream_cap
                || result.stdout_discarded_bytes > 0
                || result.stderr_discarded_bytes > 0
            {
                let complete = result.stdout_discarded_bytes == 0
                    && result.stderr_discarded_bytes == 0
                    && !result.capture_may_be_incomplete;
                let valid_utf8 = std::str::from_utf8(&result.output.stdout).is_ok()
                    && std::str::from_utf8(&result.output.stderr).is_ok();
                let log = format!(
                    "{}\ncapture_complete={complete}; utf8_exact={valid_utf8}; stdout_discarded_bytes={}; stderr_discarded_bytes={}\nstdout:\n{}\nstderr:\n{}",
                    format_shell_status_header(
                        result.output.status.code(),
                        result.timed_out,
                        result.cancelled,
                    ),
                    result.stdout_discarded_bytes,
                    result.stderr_discarded_bytes,
                    captured_shell_stream_text(
                        &result.output.stdout,
                        result.stdout_discarded_bytes,
                    ),
                    captured_shell_stream_text(
                        &result.output.stderr,
                        result.stderr_discarded_bytes,
                    ),
                );
                let redacted = safe_output
                    .map(|safe| {
                        format!(
                            "{}\n[redacted preview; use shell_job output for full log]\n{safe}",
                            format_shell_status_header(
                                result.output.status.code(),
                                result.timed_out,
                                result.cancelled
                            )
                        )
                    })
                    .unwrap_or_else(|| crate::runtime::redact_values(&self.sensitive_values, &log));
                match store.put("shell-log", redacted.as_bytes()) {
                    Ok(handle) => Some(handle),
                    Err(error) => {
                        output.push_str(&format!(
                            "\n[shell log artifact unavailable: {:?}]",
                            error.kind()
                        ));
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        if can_use_validation_cache
            && success
            && result.output.status.code() == Some(0)
            && !result.capture_may_be_incomplete
            && result.stdout_discarded_bytes == 0
            && result.stderr_discarded_bytes == 0
        {
            let redacted = crate::runtime::redact_values(&self.sensitive_values, &output);
            validation_cache::record_success(
                &prepared.canonical_workspace,
                command,
                args.as_deref(),
                &redacted,
                started_at.elapsed().as_millis() as u64,
            );
        }
        let process = Some(result.execution_facts());
        let mut executed = ExecutedTool::output(output, success).with_process(process);
        executed.artifact = artifact;
        Ok(executed)
    }
}

/// Windows PowerShell rejects the whole script at parse time, before the
/// UTF-8 setup runs, so its own message arrives localized and mis-encoded. The
/// error id and category are neither. Models write PowerShell 7 syntax here;
/// one note covers every parse error, the statement-separator one in detail.
/// Only for a script that itself failed to parse: the process failed, nothing
/// reached stdout, and the first error record on stderr is the top-level
/// parser's (`ParserError: (:) [], ParentContainsErrorRecordException`). A parse
/// error that a running script printed (`Invoke-Expression` reports
/// `ParserError: (:) [Invoke-Expression], ParseException`) is not that record.
/// Known limit: a nested `powershell` whose own script fails to parse prints
/// the same record and is not told apart.
fn windows_powershell_syntax_note(
    failed: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Option<&'static str> {
    if !failed || !stdout.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let stderr = normalize_shell_text(&String::from_utf8_lossy(stderr));
    let first_record = stderr
        .lines()
        .map(str::trim_start)
        .find(|line| line.starts_with("+ CategoryInfo"))?;
    if !first_record.contains(": ParserError: (:) [], ParentContainsErrorRecordException") {
        return None;
    }
    Some(
        if stderr.contains("FullyQualifiedErrorId : InvalidEndOfLine") {
            "\n[note: nothing ran — Windows PowerShell 5.1 has no `&&`/`||`; \
             write `a; if ($?) { b }` for `a && b` and `a; if (-not $?) { b }` for `a || b`. \
             `??`, `?.` and `?:` are unavailable too]"
        } else {
            "\n[note: this host is Windows PowerShell 5.1; PowerShell 7 syntax \
             (`&&`, `||`, `??`, `?.`, `?:`) is unavailable]"
        },
    )
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
/// bytes discarded by the bounded raw shell capture. Two capped streams plus
/// header, markers, an admission note and an artifact reference stay under the
/// default 16 KiB per-result allowance, so presentation never cuts them again.
const SHELL_STREAM_CAP_BYTES: usize = 7 * 1024;
/// Larger per-stream cap in Code mode where output goes into JS RAM (not LLM context).
const CODE_SHELL_STREAM_CAP_BYTES: usize = 256 * 1024;
const SHELL_LOG_CAPTURE_CAP_BYTES: usize = 8 * 1024 * 1024;

fn captured_shell_stream_text(raw: &[u8], discarded_bytes: usize) -> String {
    if discarded_bytes == 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let middle = raw.len().div_ceil(2);
    let (head, tail, boundary_bytes) = utf8_edges(&raw[..middle], &raw[middle..]);
    let omitted = discarded_bytes.saturating_add(boundary_bytes);
    format!(
        "{}\n[{omitted} bytes omitted by capture limit]\n{}",
        String::from_utf8_lossy(head),
        String::from_utf8_lossy(tail),
    )
}

fn utf8_edges<'a>(head: &'a [u8], tail: &'a [u8]) -> (&'a [u8], &'a [u8], usize) {
    let original_len = head.len() + tail.len();
    let head = match std::str::from_utf8(head) {
        Err(error) if error.error_len().is_none() => &head[..error.valid_up_to()],
        _ => head,
    };
    let tail = (0..=tail.len().min(3))
        .find_map(|skip| {
            std::str::from_utf8(&tail[skip..])
                .ok()
                .map(|_| &tail[skip..])
        })
        .unwrap_or(tail);
    let omitted = original_len - head.len() - tail.len();
    (head, tail, omitted)
}

const CLEANED_BASIS: &str = " after terminal cleanup";

/// One stream as placed into model context: terminal noise is removed first
/// so it does not consume the cap. Clean text takes the exact byte path of
/// `cap_shell_stream`; otherwise the cap applies to the cleaned text, and the
/// truncation marker says its byte counts refer to that cleaned text
/// (`previously_discarded_bytes` still counts raw capture-limit bytes).
///
/// After a capture-limit discard `raw` is the capture head joined to the
/// capture tail, with the real gap at the midpoint: the halves are cleaned
/// separately so nothing (an unterminated escape, a repeat run, a partial
/// character) crosses the gap, and the marker stays where the gap is.
#[cfg(test)]
fn clean_shell_stream(raw: &[u8], previously_discarded_bytes: usize) -> String {
    clean_shell_stream_with_cap(raw, previously_discarded_bytes, SHELL_STREAM_CAP_BYTES)
}

fn clean_shell_stream_with_cap(
    raw: &[u8],
    previously_discarded_bytes: usize,
    cap_bytes: usize,
) -> String {
    if previously_discarded_bytes == 0 {
        let text = String::from_utf8_lossy(raw);
        let cleaned = normalize_shell_text(&text);
        if cleaned == text {
            return cap_shell_bytes(raw, 0, "", cap_bytes);
        }
        return cap_shell_bytes(cleaned.as_bytes(), 0, CLEANED_BASIS, cap_bytes);
    }
    let (head, tail) = raw.split_at(raw.len().div_ceil(2));
    let (head, tail, boundary_bytes) = utf8_edges(head, tail);
    let (head, tail) = (String::from_utf8_lossy(head), String::from_utf8_lossy(tail));
    let (cleaned_head, cleaned_tail) = (normalize_shell_text(&head), normalize_shell_text(&tail));
    if cleaned_head == head && cleaned_tail == tail {
        return cap_shell_bytes(raw, previously_discarded_bytes, "", cap_bytes);
    }
    cap_shell_gap(
        &cleaned_head,
        &cleaned_tail,
        previously_discarded_bytes.saturating_add(boundary_bytes),
        cap_bytes,
    )
}

/// Caps a cleaned capture head and tail to `cap_bytes` in total,
/// trimming at the gap (end of head, start of tail) and reporting the gap's
/// `omitted_bytes` plus what the trim removed.
fn cap_shell_gap(head: &str, tail: &str, omitted_bytes: usize, cap_bytes: usize) -> String {
    let head_share = cap_bytes.div_ceil(2);
    let tail_share = cap_bytes - head_share;
    let head_limit = if tail.len() < tail_share {
        cap_bytes - tail.len()
    } else {
        head_share
    };
    let tail_limit = if head.len() < head_share {
        cap_bytes - head.len()
    } else {
        tail_share
    };
    let mut head_end = head.len().min(head_limit);
    while !head.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = tail.len() - tail.len().min(tail_limit);
    while !tail.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = omitted_bytes
        .saturating_add(head.len() - head_end)
        .saturating_add(tail_start);
    format!(
        "{}\n[truncated {omitted} bytes; kept first {head_end} and last {} bytes of this stream{CLEANED_BASIS}]\n{}",
        &head[..head_end],
        tail.len() - tail_start,
        &tail[tail_start..],
    )
}

#[cfg(test)]
fn cap_shell_stream(raw: &[u8], previously_discarded_bytes: usize) -> String {
    cap_shell_bytes(raw, previously_discarded_bytes, "", SHELL_STREAM_CAP_BYTES)
}

fn cap_shell_bytes(
    raw: &[u8],
    previously_discarded_bytes: usize,
    basis: &str,
    cap_bytes: usize,
) -> String {
    if raw.len() <= cap_bytes && previously_discarded_bytes == 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let retained_bytes = raw.len().min(cap_bytes);
    let head_bytes = retained_bytes / 2 + retained_bytes % 2;
    let tail_bytes = retained_bytes - head_bytes;
    let tail_start = raw.len() - tail_bytes;
    let (head, tail, boundary_bytes) = utf8_edges(&raw[..head_bytes], &raw[tail_start..]);
    let discarded_bytes = previously_discarded_bytes
        .saturating_add(raw.len() - retained_bytes)
        .saturating_add(boundary_bytes);
    format!(
        "{}\n[truncated {discarded_bytes} bytes; kept first {} and last {} bytes of this stream{basis}]\n{}",
        String::from_utf8_lossy(head),
        head.len(),
        tail.len(),
        String::from_utf8_lossy(tail),
    )
}

macro_rules! shell_tool_description_tail {
    () => {
        "Inspect workspace metadata before Git. Exit status alone does not validate the task. Use file tools for edits. In the agent loop, returns a job after yield_ms; continue independent work. Completion is delivered automatically; use shell_job to list, inspect, read output, wait, interrupt or cancel; do not poll. background=true yields immediately. stdin=true pipes the job's stdin; feed it with shell_job input (eof closes it). TUI jobs survive responses within the session; headless waits. Run cancellation stops jobs launched by that run; do not detach child processes. Avoid concurrent edits to files used by a running job. With the default yield_ms, a shell that is the last pending call of its batch is awaited up to 30s; start servers and watchers with background=true."
    };
}
/// `shell` tool description: the script form names the interpreter
/// `tools::shell` actually launches on this platform.
#[cfg(windows)]
const SHELL_TOOL_DESCRIPTION: &str = concat!(
    "Run in workspace. Without args (or null), command is PowerShell script; with args (even []), an executable with literal arguments. .bat/.cmd keep their interpreter. For native checks, prefer command + args. In scripts, capture $LASTEXITCODE immediately after the native command, before filtering or printing, and explicitly exit with that saved code. ",
    shell_tool_description_tail!()
);
#[cfg(not(windows))]
const SHELL_TOOL_DESCRIPTION: &str = concat!(
    "Run in workspace. Without args (or null), command is a bash script (`bash -c`, no rc files); with args (even []), an executable with literal arguments. For native checks, prefer command + args. A script exits with its last command's status: chain with `&&` or `set -e` when an earlier failure matters, and `set -o pipefail` before piping a native command into a filter. ",
    shell_tool_description_tail!()
);

/// Marks a write/patch result whose fused `then_run` exited successfully;
/// the agent loop keys `then_run.final` on it.
pub(crate) const THEN_RUN_PASSED: &str = "\nthen_run: passed\n";

fn tool_definition(name: &str) -> Value {
    let then_run_schema = json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "minLength": 1},
            "args": {"type": ["array", "null"], "items": {"type": "string"}},
            "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_MS, "default": DEFAULT_SHELL_TIMEOUT_MS},
            "final": {"type": "string", "minLength": 1, "description": "Your closing answer to the user. If the command exits 0, the task ends with this text and no further turn; on failure it is ignored and you see the output."}
        },
        "required": ["command"],
        "additionalProperties": false
    });
    let (description, properties, required) = match name {
        "read" => (
            "Read exact UTF-8 text, at most 1000 lines per call; follow the returned offset for more. Pass outline:true for symbol line spans, symbols:[...] or ranges:[[start,end]] to read targeted slices, or raw:true to force full text when edited recently. Complete reads authorize overwrite; prefer search+patch for local edits. For calculations, use shell and return aggregates.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path."},
                "max_lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES_CAP, "description": "Page size; omit for automatic sizing."},
                "lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES_CAP, "description": "Alias for `max_lines`."},
                "offset": {"type": "integer", "minimum": 1, "description": "First line (1-based); omit for line 1."},
                "outline": {"type": "boolean", "description": "Return compact structural symbol outline with line ranges."},
                "ranges": {"type": "array", "maxItems": 8, "items": {"type": "array", "minItems": 2, "maxItems": 2, "items": {"type": "integer", "minimum": 1}}, "description": "Inclusive 1-based [start, end] line ranges."},
                "symbols": {"type": "array", "maxItems": 8, "items": {"type": "string"}, "description": "Extract declarations matching these symbol names."},
                "raw": {"type": "boolean", "description": "Force full text instead of a compact diff against an earlier read in context."}
            }),
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
            "Search literal UTF-8 text using query OR patterns. N: marks hits; N- marks context. For patch.expected, copy only the text after `N: ` or `N- `, without headers. Never patch [truncated] text. With a cursor, repeat path/query/context_lines.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path."},
                "query": {"type": "string"},
                "patterns": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 32},
                "glob": {"type": "string", "description": "Only files matching this gitignore-style glob under path, e.g. `*.rs` or `src/**/*.ts`; `!` excludes."},
                "context_lines": {"type": "integer", "minimum": 0, "default": 0, "description": "Context when the hit line is not unique; capped at 3 with an admission note. Not a complete read for overwrite."},
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
            "Write a complete UTF-8 file, creating parents. See expected for overwrite requirements; stale content is rejected. Prefer patch for local edits. Failure may return recovery text. Success needs no confirmation read; independent paths may share a turn. When a check or test command exists, attach it as then_run on the last edit instead of a separate shell call: it runs only after the write succeeds, waits, and reports both results; a failed command leaves the edit applied. Add then_run.final when a pass completes the task.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path."},
                "content": {"type": "string"},
                "expected": {
                    "type": ["string", "null"],
                    "description": "Current full-file text. Omit/null to create, or after a complete read, overwrite or patch of this path (not after creating it). LF matches uniform CRLF."
                },
                "then_run": then_run_schema
            }),
            json!(["path", "content"]),
        ),
        "patch" => (
            "Apply ordered edits to one file: edits OR top-level expected/replacement. Match unique raw text; never line numbers, headers or [truncated] markers. Unique search hits need no complete read. LF matches uniform CRLF. A batch keeps the edits that match and reports the rejected ones with recovery text: resend only those; then_run is skipped. Multi-file patches stay atomic. Success authorizes overwrite; no confirmation read. Independent paths may share a turn. When a check or test command exists, attach it as then_run on the last edit instead of a separate shell call: it runs only after the patch succeeds, waits, and reports both results; a failed command leaves the edit applied. Add then_run.final when a pass completes the task.",
            json!({
                "path": {"type": "string", "description": "Workspace-relative path."},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": patch::MAX_PATCH_EDITS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "expected": {"type": "string", "minLength": 1, "description": "Unique raw file substring from read or search (strip `N: `/`N- ` prefixes)."},
                            "replacement": {"type": "string"},
                            "replace_all": {"type": "boolean", "default": false, "description": "Replace every match of expected instead of requiring a unique one."}
                        },
                        "required": ["expected", "replacement"],
                        "additionalProperties": false
                    }
                },
                "files": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 16,
                    "description": "Additional workspace files to patch atomically in the same transaction.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Workspace-relative path."},
                            "edits": {
                                "type": "array",
                                "minItems": 1,
                                "maxItems": patch::MAX_PATCH_EDITS,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "expected": {"type": "string", "minLength": 1},
                                        "replacement": {"type": "string"},
                                        "replace_all": {"type": "boolean", "default": false}
                                    },
                                    "required": ["expected", "replacement"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["path", "edits"],
                        "additionalProperties": false
                    }
                },
                "rollback_on_check_fail": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true with then_run, revert all files patched by this call if the command exits non-zero."
                },
                "expected": {"type": "string", "minLength": 1, "description": "Legacy unique raw file substring."},
                "replacement": {"type": "string", "description": "Legacy replacement text."},
                "then_run": then_run_schema
            }),
            json!(["path"]),
        ),
        "shell" => (
            SHELL_TOOL_DESCRIPTION,
            json!({"command": {"type": "string"}, "args": {"type": ["array", "null"], "items": {"type": "string"}}, "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_MS, "default": DEFAULT_SHELL_TIMEOUT_MS}, "yield_ms": {"type": "integer", "minimum": 0, "maximum": 10000, "default": 1000}, "background": {"type": "boolean", "default": false}, "stdin": {"type": "boolean", "default": false}, "no_cache": {"type": "boolean", "default": false}}),
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
    if name == "search" {
        // Native admission accepts both fields only when they describe the
        // same literal term. Do not exclude that spelling with oneOf.
        input_schema["anyOf"] = json!([
            {"required": ["query"]},
            {"required": ["patterns"]}
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

/// Spells an absolute path that lies inside `root` as a workspace-relative
/// one. Only the spelling changes: the result must still pass
/// [`resolve_workspace_path_from_root`], which owns containment. `None` when
/// the path is relative, outside the workspace, or cannot be placed.
pub(crate) fn workspace_relative_spelling(root: &Path, path: &str) -> Option<String> {
    let raw = Path::new(path);
    if !raw.is_absolute() {
        return None;
    }
    let relative = match raw.strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => {
            // Canonicalize the deepest existing ancestor (so `C:\x` and
            // `\\?\C:\x` agree) and keep the not-yet-existing tail, which a
            // `write` may legitimately name.
            let mut ancestor = raw;
            let mut tail = Vec::<OsString>::new();
            loop {
                match fs::canonicalize(ancestor) {
                    Ok(resolved) => {
                        let mut relative = resolved.strip_prefix(root).ok()?.to_path_buf();
                        relative.extend(tail.iter().rev());
                        break relative;
                    }
                    Err(_) => {
                        tail.push(ancestor.file_name()?.to_os_string());
                        ancestor = ancestor.parent()?;
                    }
                }
            }
        }
    };
    let spelled = relative.to_str()?;
    Some(if spelled.is_empty() {
        ".".into()
    } else {
        spelled.into()
    })
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

/// Names workspace paths relative to the workspace in text Slim generates
/// (receipts, notes, error headers). Never apply it to file content.
fn workspace_relative_text(text: &str, workspace: &Path) -> String {
    let prefix = format!("{}{}", workspace.display(), std::path::MAIN_SEPARATOR);
    text.replace(&prefix, "")
}

fn workspace_display<'a>(path: &'a Path, workspace: &Path) -> &'a Path {
    path.strip_prefix(workspace).unwrap_or(path)
}

fn tool_error_message(error: ToolError) -> String {
    match error {
        ToolError::Io { message } => format!("io error: {message}"),
        ToolError::Cancelled => "tool cancelled before side effect".into(),
        ToolError::StaleRead { path } => {
            format!(
                "stale read: {path}; precondition differs from current bytes; no write applied."
            )
        }
        ToolError::PreconditionRequired { path } => {
            format!("precondition required: {path}; no write applied.")
        }
        ToolError::MatchCount { count } => format!("expected one match; got {count}"),
        ToolError::InvalidInput { message } => message,
    }
}

#[cfg(test)]
mod argument_summary_tests {
    use super::{summarize_tool_arguments_for, ARGUMENT_SUMMARY_LIMIT};
    use serde_json::json;

    #[test]
    fn long_summaries_stay_within_the_limit() {
        let arguments = json!({"path": "p".repeat(300)}).to_string();
        let summary = summarize_tool_arguments_for("read", &arguments);
        assert_eq!(summary.chars().count(), ARGUMENT_SUMMARY_LIMIT);
        assert!(summary.ends_with('…'), "{summary}");
    }

    #[test]
    fn later_lines_of_a_multiline_value_are_marked() {
        assert_eq!(
            summarize_tool_arguments_for("shell", r#"{"command":"cd crates\ncargo test"}"#),
            "command=cd crates… · limit 600s"
        );
        assert_eq!(
            summarize_tool_arguments_for("read", r#"{"path":"src/lib.rs\n"}"#),
            "path=src/lib.rs"
        );
    }

    #[test]
    fn codemode_summary_is_the_first_statement_past_comments() {
        let code = "\n// gather the files\n/* multi\nline */\nconst files = await slim.list({});\nreturn files;";
        assert_eq!(
            summarize_tool_arguments_for("codemode", &json!({ "code": code }).to_string()),
            "const files = await slim.list({});…"
        );
        assert_eq!(
            summarize_tool_arguments_for("codemode", &json!({"code": "return 1"}).to_string()),
            "return 1"
        );
        assert_eq!(summarize_tool_arguments_for("codemode", "{}"), "");
        assert_eq!(
            summarize_tool_arguments_for("codemode", &json!({"code": "// only"}).to_string()),
            ""
        );
        assert_eq!(
            summarize_tool_arguments_for("codemode", &json!({"code": "/* unclosed"}).to_string()),
            ""
        );
    }

    #[test]
    fn search_summary_names_the_searched_text() {
        assert_eq!(
            summarize_tool_arguments_for("search", r#"{"path":"crates","query":"fn run"}"#),
            "query=fn run"
        );
        assert_eq!(
            summarize_tool_arguments_for("search", r#"{"patterns":["alpha","beta"]}"#),
            "query=alpha | beta"
        );
        assert_eq!(
            summarize_tool_arguments_for("search", r#"{"path":"crates"}"#),
            "path=crates"
        );
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
        assert!(summarize_tool_arguments_for(
            "shell",
            r#"{"command":"echo","timeout_ms":3600001}"#
        )
        .contains("command=echo"));
        assert!(!summarize_tool_arguments_for(
            "shell",
            r#"{"command":"echo","timeout_ms":3600001}"#
        )
        .contains("limit 3600s"));
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
    fn captured_log_marks_the_missing_middle() {
        assert_eq!(
            super::captured_shell_stream_text(b"HEADTAIL", 123),
            "HEAD\n[123 bytes omitted by capture limit]\nTAIL"
        );
    }

    #[test]
    fn shell_head_tail_cuts_do_not_invent_invalid_utf8() {
        // Each `é` straddles one cut: the head ends and the tail starts mid-char.
        let half = super::SHELL_STREAM_CAP_BYTES / 2;
        let raw = format!(
            "{}é{}é{}",
            "A".repeat(half - 1),
            "B".repeat(4095),
            "C".repeat(half - 1)
        );
        let preview = super::cap_shell_stream(raw.as_bytes(), 0);
        assert!(!preview.contains('\u{fffd}'));
        assert!(preview.contains("truncated 4099 bytes"));

        let captured = b"AAA\xc3\xa9BBB";
        let log = super::captured_shell_stream_text(captured, 10);
        assert_eq!(log, "AAA\n[12 bytes omitted by capture limit]\nBBB");
    }

    #[test]
    fn clean_shell_stream_keeps_clean_text_on_the_exact_cap_path() {
        let raw = format!("{}\n", "ok line".repeat(2000));
        assert_eq!(
            super::clean_shell_stream(raw.as_bytes(), 5),
            super::cap_shell_stream(raw.as_bytes(), 5)
        );
        assert_eq!(super::clean_shell_stream(b"fine\n", 0), "fine\n");
    }

    #[test]
    fn terminal_noise_no_longer_pushes_an_error_line_out_of_the_cap() {
        // The error sits between repeated lines and ANSI-colored progress
        // frames, all far larger than the cap.
        let noise = "downloading crate foo v1.0.0\n".repeat(600);
        let mut progress = String::new();
        for step in 0..400 {
            progress.push_str(&format!("\u{1b}[32m{step:>3}%\u{1b}[0m\r"));
        }
        let raw = format!(
            "{noise}{progress}\nerror[E0308]: mismatched types\n{noise}{progress}\nfinished\n"
        );
        assert!(raw.len() > 3 * super::SHELL_STREAM_CAP_BYTES);
        let old = super::cap_shell_stream(raw.as_bytes(), 0);
        assert!(!old.contains("mismatched types"), "{old}");

        let cleaned = super::clean_shell_stream(raw.as_bytes(), 0);
        assert!(!cleaned.contains("[truncated"), "{cleaned}");
        assert!(cleaned.contains("error[E0308]: mismatched types\n"));
        assert_eq!(cleaned.matches("399%\n").count(), 2, "{cleaned}");
        assert!(!cleaned.contains("398%"));
        assert_eq!(
            cleaned
                .matches("[previous line repeated 599 more times]")
                .count(),
            2
        );
        assert!(cleaned.ends_with("finished\n"));
        assert!(!cleaned.contains('\u{1b}') && !cleaned.contains('\r'));
    }

    /// Capture head and tail as `process.rs` joins them: equal halves, the
    /// real gap at the midpoint.
    fn joined_capture(head: &[u8], tail: &[u8]) -> Vec<u8> {
        assert_eq!(head.len(), tail.len());
        [head, tail].concat()
    }

    #[test]
    fn cleaned_capture_gap_keeps_the_marker_at_the_gap_and_repeat_runs_apart() {
        let line = "downloading crate foo v1.0.0\n";
        let raw = line.repeat(100);
        let (head, tail) = raw.as_bytes().split_at(raw.len() / 2);
        let out = super::clean_shell_stream(&joined_capture(head, tail), 5_000_000);
        let half = format!("{line}[previous line repeated 49 more times]\n");
        assert_eq!(
            out,
            format!(
                "{half}\n[truncated 5000000 bytes; kept first {0} and last {0} bytes of this stream after terminal cleanup]\n{half}",
                half.len()
            )
        );
    }

    #[test]
    fn cleaned_capture_gap_does_not_let_head_escapes_or_split_chars_leak_across() {
        // Head ends in an unterminated OSC; the tail's first line must survive.
        let head = b"first\n\x1b]0;unterminated title".to_vec();
        let mut tail = b"next line\n".to_vec();
        tail.resize(head.len(), b'.');
        let out = super::clean_shell_stream(&joined_capture(&head, &tail), 777);
        assert_eq!(
            out,
            format!(
                "first\n\n[truncated 777 bytes; kept first 6 and last {0} bytes of this stream after terminal cleanup]\n{1}",
                tail.len(),
                String::from_utf8(tail).unwrap()
            )
        );

        // `é` split by the gap: both partial bytes are dropped and counted.
        let out =
            super::clean_shell_stream(&joined_capture(b"\x1b[31mred\xc3", b"\xa9blue!!!!"), 100);
        assert_eq!(
            out,
            "red\n[truncated 102 bytes; kept first 3 and last 8 bytes of this stream after terminal cleanup]\nblue!!!!"
        );
    }

    #[test]
    fn cleaned_capture_gap_trims_at_the_gap_when_halves_exceed_the_cap() {
        let half = |label: &str| {
            let mut text = String::from("\u{1b}[0m");
            let mut index = 0;
            while text.len() < 6000 {
                text.push_str(&format!("{label} {index}\n"));
                index += 1;
            }
            text
        };
        let (head, tail) = (half("head"), half("tail"));
        let size = head.len().max(tail.len());
        let head = format!("{head}{}", "h".repeat(size - head.len()));
        let tail = format!("{tail}{}", "t".repeat(size - tail.len()));
        let (cleaned_head, cleaned_tail) = (
            super::normalize_shell_text(&head),
            super::normalize_shell_text(&tail),
        );
        let out = super::clean_shell_stream(&joined_capture(head.as_bytes(), tail.as_bytes()), 42);
        let share = super::SHELL_STREAM_CAP_BYTES / 2;
        let (kept_head, kept_tail) = out.split_once("\n[truncated ").expect("marker");
        assert_eq!(kept_head, &cleaned_head[..share]);
        let (marker, kept_tail) = kept_tail.split_once("]\n").expect("marker end");
        assert_eq!(kept_tail, &cleaned_tail[cleaned_tail.len() - share..]);
        let omitted = 42 + (cleaned_head.len() - share) + (cleaned_tail.len() - share);
        assert_eq!(
            marker,
            format!(
                "{omitted} bytes; kept first {share} and last {share} bytes of this stream after terminal cleanup"
            )
        );
    }

    #[test]
    fn cleaned_truncation_marker_counts_cleaned_bytes() {
        let mut raw = String::from("\u{1b}[1m");
        for index in 0..2000 {
            raw.push_str(&format!("line {index}\r\n"));
        }
        let cleaned = super::normalize_shell_text(&raw);
        let capped = super::clean_shell_stream(raw.as_bytes(), 0);
        let cap = super::SHELL_STREAM_CAP_BYTES;
        assert!(cleaned.len() > cap && raw.len() > cleaned.len());
        assert!(
            capped.contains(&format!(
                "[truncated {} bytes; kept first {} and last {} bytes of this stream after terminal cleanup]",
                cleaned.len() - cap,
                cap / 2,
                cap / 2
            )),
            "{capped}"
        );
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
mod edit_line_stats_tests {
    use super::edit_line_stats;

    #[test]
    fn counts_only_changed_lines_across_edits() {
        let arguments = serde_json::json!({
            "path": "src/lib.rs",
            "edits": [
                {"expected": "fn a() {\n    old();\n}", "replacement": "fn a() {\n    new();\n    more();\n}"},
                {"expected": "x", "replacement": "y"}
            ]
        })
        .to_string();
        assert_eq!(edit_line_stats("patch", &arguments), Some((3, 2)));
    }

    #[test]
    fn top_level_edit_and_pure_deletion_are_counted() {
        let arguments =
            serde_json::json!({"path": "a", "expected": "keep\ndrop\n", "replacement": "keep\n"})
                .to_string();
        assert_eq!(edit_line_stats("patch", &arguments), Some((0, 1)));
    }

    #[test]
    fn single_edit_object_is_counted_like_admission_normalizes_it() {
        let arguments = serde_json::json!({
            "path": "a",
            "edits": {"expected": "one\n", "replacement": "one\ntwo\n"}
        })
        .to_string();
        assert_eq!(edit_line_stats("patch", &arguments), Some((1, 0)));
    }

    #[test]
    fn other_tools_and_malformed_arguments_have_no_stats() {
        assert_eq!(
            edit_line_stats("write", r#"{"path":"a","content":"x"}"#),
            None
        );
        assert_eq!(edit_line_stats("patch", "not json"), None);
        assert_eq!(edit_line_stats("patch", r#"{"path":"a"}"#), None);
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

    #[test]
    fn read_outline_ranges_symbols_work_and_do_not_authorize_overwrite() {
        let root = std::env::temp_dir().join(format!(
            "slim-tools-read-ext-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let src = "pub struct Alpha {\n    pub x: usize,\n}\n\npub fn beta() -> usize {\n    1\n}\n\npub fn gamma() -> usize {\n    2\n}\n";
        fs::write(root.join("lib.rs"), src).unwrap();

        let registry = ToolRegistry::default();
        let outline = registry.execute(
            OperatingMode::ReadOnly,
            &root,
            "read",
            r#"{"path":"lib.rs","outline":true}"#,
        );
        assert!(outline.success, "{}", outline.output);
        assert!(outline.output.contains("L1-L3 struct pub struct Alpha"));
        assert!(outline.output.contains("L5-L7 fn pub fn beta()"));

        let syms = registry.execute(
            OperatingMode::ReadOnly,
            &root,
            "read",
            r#"{"path":"lib.rs","symbols":["beta"]}"#,
        );
        assert!(syms.success, "{}", syms.output);
        assert!(syms.output.contains("[lines 5-7]\npub fn beta() -> usize {\n    1\n}"));

        let ranges = registry.execute(
            OperatingMode::ReadOnly,
            &root,
            "read",
            r#"{"path":"lib.rs","ranges":[[1,3],[9,11]]}"#,
        );
        assert!(ranges.success, "{}", ranges.output);
        assert!(ranges.output.contains("[lines 1-3]"));
        assert!(ranges.output.contains("[lines 9-11]"));

        // Neither outline nor ranges should authorize implicit write overwrite.
        let overwrite = registry.execute(
            OperatingMode::Auto,
            &root,
            "write",
            r#"{"path":"lib.rs","content":"replaced"}"#,
        );
        assert!(!overwrite.success);
        assert!(overwrite.output.contains("precondition required"), "{}", overwrite.output);

        let conflict = registry.execute(
            OperatingMode::ReadOnly,
            &root,
            "read",
            r#"{"path":"lib.rs","outline":true,"ranges":[[1,2]]}"#,
        );
        assert!(!conflict.success);
        assert!(conflict.output.contains("cannot be combined"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_and_search_reuse_cached_evidence_until_mutation() {
        let root = std::env::temp_dir().join(format!(
            "slim-evidence-ws-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "needle\n").unwrap();

        let registry = ToolRegistry::default();
        let list_prep = registry.prepare_invocation(
            OperatingMode::Auto,
            &root,
            "list",
            r#"{"path":"."}"#,
        );
        let search_prep = registry.prepare_invocation(
            OperatingMode::Auto,
            &root,
            "search",
            r#"{"query":"needle"}"#,
        );

        let l1 = registry.execute_prepared_with_cancellation_and_progress(&list_prep, None, |_| {});
        assert!(l1.result.success && l1.receipt.execution_us > 0);
        let l2 = registry.execute_prepared_with_cancellation_and_progress(&list_prep, None, |_| {});
        assert_eq!(l2.result.output, l1.result.output);
        assert_eq!(l2.receipt.execution_us, 0);

        let s1 = registry.execute_prepared_with_cancellation_and_progress(&search_prep, None, |_| {});
        assert!(s1.result.success && s1.receipt.execution_us > 0);
        let s2 = registry.execute_prepared_with_cancellation_and_progress(&search_prep, None, |_| {});
        assert_eq!(s2.result.output, s1.result.output);
        assert_eq!(s2.receipt.execution_us, 0);

        // A mutation bumps workspace_revision and invalidates ObservedWorkspace caches.
        let w = registry.execute(
            OperatingMode::Auto,
            &root,
            "write",
            r#"{"path":"b.txt","content":"needle\n"}"#,
        );
        assert!(w.success, "{}", w.output);

        let l3 = registry.execute_prepared_with_cancellation_and_progress(&list_prep, None, |_| {});
        assert!(l3.receipt.execution_us > 0);
        assert!(l3.result.output.contains("b.txt"));
        let s3 = registry.execute_prepared_with_cancellation_and_progress(&search_prep, None, |_| {});
        assert!(s3.receipt.execution_us > 0);
        assert!(s3.result.output.contains("b.txt"));

        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(all(test, windows))]
mod action_fusion_tests {
    use super::*;

    #[test]
    fn then_run_only_follows_successful_mutation_and_preserves_a_failed_shell_status() {
        let root = std::env::temp_dir().join(format!(
            "slim-fusion-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), "before").unwrap();
        let registry = ToolRegistry::default();
        let call = |expected: &str| {
            json!({
                "path": "file.txt", "expected": expected, "replacement": "after",
                "then_run": {"command": "Set-Content -LiteralPath marker.txt -Value ran"}
            })
            .to_string()
        };
        let rejected = registry.execute(OperatingMode::Auto, &root, "patch", &call("stale"));
        assert!(!rejected.success);
        assert!(!root.join("marker.txt").exists());
        let applied = registry.execute(OperatingMode::Auto, &root, "patch", &call("before"));
        assert!(applied.success, "{}", applied.output);
        assert!(applied.output.contains("then_run: passed"));
        assert!(root.join("marker.txt").exists());

        let failed = registry.prepare_invocation(
            OperatingMode::Auto,
            &root,
            "write",
            &json!({
                "path": "created.txt", "content": "saved", "then_run": {"command": "exit 7"}
            })
            .to_string(),
        );
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&failed, None, |_| {});
        assert!(!outcome.result.success);
        assert_eq!(
            fs::read_to_string(root.join("created.txt")).unwrap(),
            "saved"
        );
        assert!(outcome
            .result
            .output
            .contains("then_run: failed; edit remains applied"));
        assert_eq!(outcome.receipt.process.unwrap().exit_code, Some(7));
        assert!(outcome.receipt.revision_after > outcome.receipt.revision_before);

        let token = CancellationToken::new();
        let cancelled = registry.prepare_invocation(
            OperatingMode::Auto,
            &root,
            "write",
            &json!({
                "path": "cancelled.txt", "content": "saved",
                "then_run": {"command": "Set-Content -LiteralPath cancelled_marker.txt -Value ran"}
            })
            .to_string(),
        );
        let outcome = registry.execute_prepared_with_cancellation_and_progress(
            &cancelled,
            Some(&token),
            |progress| {
                if progress.preview == "Edit applied; running then_run" {
                    token.cancel();
                }
            },
        );
        assert!(!outcome.result.success);
        assert!(outcome.result.output.contains("then_run: not executed"));
        assert_eq!(
            fs::read_to_string(root.join("cancelled.txt")).unwrap(),
            "saved"
        );
        assert!(!root.join("cancelled_marker.txt").exists());

        let rewritten = registry.execute(
            OperatingMode::Auto,
            &root,
            "patch",
            &json!({
                "path": "file.txt", "expected": "after", "replacement": "edited",
                "then_run": {"command": "Set-Content -LiteralPath file.txt -Value shell"}
            })
            .to_string(),
        );
        assert!(rewritten.success, "{}", rewritten.output);
        assert!(fs::read_to_string(root.join("file.txt"))
            .unwrap()
            .contains("shell"));
        let implicit = registry.execute(
            OperatingMode::Auto,
            &root,
            "write",
            &json!({"path": "file.txt", "content": "next"}).to_string(),
        );
        assert!(!implicit.success);
        assert!(
            implicit.output.contains("precondition required"),
            "{}",
            implicit.output
        );
        assert!(
            registry
                .execute(OperatingMode::Auto, &root, "read", r#"{"path":"file.txt"}"#)
                .success
        );
        assert!(
            registry
                .execute(
                    OperatingMode::Auto,
                    &root,
                    "write",
                    &json!({"path": "file.txt", "content": "next"}).to_string(),
                )
                .success
        );
        for invalid in [
            json!({"command": ""}),
            json!({"command": "exit 0", "timeout_ms": 0}),
            json!({"command": "exit 0", "yield_ms": 1}),
            json!({"command": "exit 0", "unknown": true}),
        ] {
            let result = registry.execute(
                OperatingMode::Auto,
                &root,
                "write",
                &json!({"path": "invalid.txt", "content": "saved", "then_run": invalid})
                    .to_string(),
            );
            assert!(!result.success);
            assert!(!root.join("invalid.txt").exists());
        }
        let read_only = registry.execute(
            OperatingMode::ReadOnly,
            &root,
            "write",
            &json!({"path": "read_only.txt", "content": "saved", "then_run": {"command": "exit 0"}}).to_string(),
        );
        assert!(!read_only.success);
        assert!(!root.join("read_only.txt").exists());
        let _ = fs::remove_dir_all(root);
    }
}
