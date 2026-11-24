//! CodeMode owns orchestration; MCP retains transport and effect semantics.
//!
//! In Auto mode `codemode` composes MCP calls. In Code mode it is the only
//! way to act: the program also reaches the native tools as typed functions
//! on `slim`, and only its `return` value goes back to the model.
mod calls;
mod discovery;
#[cfg(test)]
mod discovery_tests;
mod dispatch;
mod engine;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod tests;
mod typescript;
#[cfg(test)]
mod typescript_tests;

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use super::*;
use crate::session::{DurableFact, DurableRecord, DurableRepo};
use dispatch::{Done, Scheduler};
use engine::{Command, Request, MAX_CODE_BYTES, MAX_JSON_BYTES};

const STATE_NAMESPACE: &str = "codemode.state.v1";
const CALL_NAMESPACE: &str = "codemode.call.v1";
const MAX_STATE_BYTES: usize = 512 * 1024;
/// Wall deadline of an Auto cell.
const CELL_TIMEOUT: Duration = Duration::from_secs(120);
/// Wall deadline of a Code cell: the longest shell call it may run.
const CODE_CELL_TIMEOUT: Duration = Duration::from_millis(crate::tools::MAX_SHELL_TIMEOUT_MS);
/// JavaScript execution time of a cell, waits for the host excluded.
const JS_CPU_LIMIT: Duration = Duration::from_secs(120);
const MAX_HOST_REQUESTS: usize = 1024;
/// Shell arguments a program cannot use: the cell's shell runs in the
/// foreground and its output is the call's result.
const UNSUPPORTED_SHELL_ARGUMENTS: [&str; 3] = ["background", "yield_ms", "stdin"];
/// `then_run` arguments a program cannot use: the program returns its own
/// answer.
const UNSUPPORTED_THEN_RUN_ARGUMENTS: [&str; 1] = ["final"];

const AUTO_DESCRIPTION: &str = "Compose MCP calls in isolated JavaScript. Use await tools.call('mcp.server.tool', arguments): returns structuredContent when present, otherwise the MCP result envelope; isError throws. Discover names/schemas with mcp first, or in code with searchTools(query,{limit?,server?}) -> [{name,server,tool,description}] (BM25; without server searches ready catalogs only), describeTool(name) -> {inputSchema,...}|null and listServers() (synchronous; hidden tools never appear). Calls execute in order, one at a time (no parallel calls) and each consumes the current tool budget. return only the needed summary. store(key, JSON_value) / load(key) persist session values (512 KiB total); store(key,null) deletes. No filesystem, network, Node or imports. Cells have fresh globals, 32 MiB memory and 120s deadline; store writes and completed tool effects survive a later error. Never replay a failed cell blindly.";

const CODE_DESCRIPTION_BASE: &str = "Act by running a short async JavaScript program; this is your only workspace tool. Only the returned JSON value plus console output comes back, and only about 16 KiB of it inline (the rest, up to 1 MiB each, is stored as an artifact you must read back): filter intermediate data in code. `slim.*` rejects on error; `structured: true` returns the object form instead of text. `slim.atomic(fn)` rolls back its `write`/`patch` on throw. Cells have 32 MiB memory and 120s of JavaScript CPU.";

pub(crate) fn prewarm_worker_cell() {
    engine::prewarm_worker_cell();
}

pub(super) fn batch_code_enabled() -> bool {
    !std::env::var_os("SLIM_HYBRID_BATCH_CODE")
        .is_some_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellProfile {
    McpSerial,
    FullCode,
    HybridBatch { allow_mutation: bool },
}

const CODE_DESCRIPTION_CODE_INTEL: &str = " `slim.code_intel` requires `path`+`line`+`column` for `definition`/`references`/`hover`, and `path` or `query` for `symbol`.";

const CODE_DESCRIPTION_CONCURRENCY_LOCAL: &str = " `Promise.all` runs reads, `glob`, `git`, `shell({readonly:true})`, and `readMany` in parallel (up to 12) overlapping with disjoint file mutations; `write`/`patch` on distinct files run in parallel waves. Unawaited calls finish before exit. Call `finish(answer)` before returning when checks pass. `store(k, v)`/`load(k)` keep 512 KiB JSON across cells. Completed effects survive a later error (except inside `slim.atomic`): never replay a failed cell blindly.";

const CODE_DESCRIPTION_CONCURRENCY_CODE_INTEL: &str = " `Promise.all` runs reads, `glob`, `git`, `code_intel`, `shell({readonly:true})`, and `readMany` in parallel (up to 12) overlapping with disjoint file mutations; `write`/`patch` on distinct files run in parallel waves. Unawaited calls finish before exit. Call `finish(answer)` before returning when checks pass. `store(k, v)`/`load(k)` keep 512 KiB JSON across cells. Completed effects survive a later error (except inside `slim.atomic`): never replay a failed cell blindly.";

const CODE_DESCRIPTION_MCP: &str = " For MCP (read-only calls also parallel in `Promise.all`): `listServers()`, `searchTools(q, {limit?, server?})`, `describeTool(name)`, then `await tools.call('mcp.server.tool', args)`.";

const CODE_DESCRIPTION_SUBAGENTS: &str = " Delegate subtasks cleanly inside code via `slim.subagent(...)` and `slim.subagent_control(...)`.";

const CODE_HELPERS_BASE: &str = "declare function finish(answer: string): void;
declare function store(key: string, value: unknown): void;
declare function load(key: string): unknown;
declare function lines(text: string): string[];
declare function grepText(text: string, pattern: string | RegExp, opts?: { context?: number; ignoreCase?: boolean }): { line: number; text: string; context: { line: number; text: string }[] }[];
declare function diffText(before: string, after: string, opts?: { context?: number }): string;
declare function parseJsonSafe(text: string, fallback?: unknown): unknown;
declare function chunk<T>(arr: T[], size: number): T[][];
declare function unique<T>(arr: T[], keyFn?: (item: T) => unknown): T[];
declare const console: { log(...values: unknown[]): void; info(...values: unknown[]): void; warn(...values: unknown[]): void; error(...values: unknown[]): void };";

const CODE_HELPERS_MCP: &str = "declare const tools: { call(name: string, args?: object): Promise<unknown> };
declare function searchTools(query: string, options?: { limit?: number; server?: string }): { name: string; server: string; tool: string; description: string }[];
declare function describeTool(name: string): { name: string; description?: string; inputSchema: unknown; outputSchema?: unknown; signature: string } | null;
declare function listServers(): unknown[];";

/// `slim` definitions indexed by `(code_intel << 1) | subagents`, built once.
static NATIVE_DEFINITIONS: LazyLock<[Vec<Value>; 4]> = LazyLock::new(|| {
    [
        build_native_definitions(false, false, true),
        build_native_definitions(false, true, true),
        build_native_definitions(true, false, true),
        build_native_definitions(true, true, true),
    ]
});

/// Read-only `slim` definitions for `batch_code` under `Plan`/`ReadOnly` indexed by `code_intel`.
static NATIVE_READONLY_DEFINITIONS: LazyLock<[Vec<Value>; 2]> = LazyLock::new(|| {
    [
        build_native_definitions(false, false, false),
        build_native_definitions(true, false, false),
    ]
});

/// Per-call effect summary kept across a batch so multiple `codemode` calls in
/// one turn never overwrite each other's mutation/effect flags.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CellEffectSummary {
    pub mutated_workspace: bool,
    pub had_non_readonly_effect: bool,
    pub had_effect: bool,
    pub had_validation_green: bool,
}

const MAX_SESSION_READ_CACHE: usize = 256;

struct CachedSessionRead {
    rev: (u64, u64),
    last_used: u64,
    deps: Vec<PathBuf>,
    finished: dispatch::Finished,
}

#[derive(Default)]
struct SessionReadCache {
    entries: HashMap<String, CachedSessionRead>,
    clock: u64,
}

impl SessionReadCache {
    fn get(&mut self, key: &str, rev: (u64, u64)) -> Option<dispatch::Finished> {
        let matches = self.entries.get(key).is_some_and(|e| e.rev == rev);
        if !matches {
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        let entry = self.entries.get_mut(key)?;
        entry.last_used = clock;
        entry.finished.clone_for_cache()
    }

    fn insert(
        &mut self,
        key: String,
        rev: (u64, u64),
        deps: Vec<PathBuf>,
        finished: dispatch::Finished,
    ) {
        if !self.entries.contains_key(&key) {
            while self.entries.len() >= MAX_SESSION_READ_CACHE {
                let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                self.entries.remove(&oldest);
            }
        }
        self.clock = self.clock.saturating_add(1);
        self.entries.insert(
            key,
            CachedSessionRead {
                rev,
                last_used: self.clock,
                deps,
                finished,
            },
        );
    }

    fn invalidate_paths(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            self.entries.clear();
            return;
        }
        self.entries.retain(|_, entry| {
            !entry.deps.is_empty() && !entry.deps.iter().any(|d| paths.contains(d))
        });
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Default)]
pub(super) struct CodeMode {
    values: BTreeMap<String, Value>,
    values_bytes_estimate: usize,
    restored: bool,
    pub remaining_calls: usize,
    pub remaining_mutating_calls: Option<usize>,
    pub used_calls: usize,
    pub(super) store_mutated: bool,
    pub(super) last_cell_had_effect: bool,
    pub(super) pending_final_answer: Option<String>,
    pub(super) cell_mutated_workspace: bool,
    pub(super) cell_had_non_readonly_effect: bool,
    pub(super) cell_validation_green: bool,
    pub(super) cell_effects: HashMap<String, CellEffectSummary>,
    session_read_cache: SessionReadCache,
    pub(super) last_applied_effects: Vec<Value>,
}

impl CodeMode {
    pub(super) fn effects_for(&self, call_id: &str) -> CellEffectSummary {
        self.cell_effects
            .get(call_id)
            .copied()
            .unwrap_or(CellEffectSummary {
                mutated_workspace: self.cell_mutated_workspace,
                had_non_readonly_effect: self.cell_had_non_readonly_effect,
                had_effect: self.last_cell_had_effect,
                had_validation_green: self.cell_validation_green,
            })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn definition(mode: crate::OperatingMode, code_intel_enabled: bool) -> Value {
    definition_for(mode, code_intel_enabled, true)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn definition_for(
    mode: crate::OperatingMode,
    code_intel_enabled: bool,
    mcp_enabled: bool,
) -> Value {
    definition_for_with_subagents(mode, code_intel_enabled, mcp_enabled, false)
}

pub(super) fn definition_for_with_subagents(
    mode: crate::OperatingMode,
    code_intel_enabled: bool,
    mcp_enabled: bool,
    subagents_enabled: bool,
) -> Value {
    let description = if mode == crate::OperatingMode::Code {
        let mut prose = String::from(CODE_DESCRIPTION_BASE);
        if code_intel_enabled {
            prose.push_str(CODE_DESCRIPTION_CODE_INTEL);
            prose.push_str(CODE_DESCRIPTION_CONCURRENCY_CODE_INTEL);
        } else {
            prose.push_str(CODE_DESCRIPTION_CONCURRENCY_LOCAL);
        }
        if mcp_enabled {
            prose.push_str(CODE_DESCRIPTION_MCP);
        }
        if subagents_enabled {
            prose.push_str(CODE_DESCRIPTION_SUBAGENTS);
        }
        let decls = typescript::declarations(
            "slim",
            native_definitions_for(code_intel_enabled, subagents_enabled),
        );
        let helpers_block =
            "readMany(paths: (string | { path: string; offset?: number; max_lines?: number; outline?: boolean; symbols?: string[]; ranges?: number[][]; raw?: boolean })[], opts?: { offset?: number; max_lines?: number; outline?: boolean; raw?: boolean; structured?: boolean }): Promise<Record<string, unknown>>; // at most 64 paths\noutlineMany(pathsOrReq: string[] | { paths?: string[]; glob?: string; path?: string; limit?: number; max_symbols?: number }, opts?: object): Promise<Record<string, unknown[]>>;\ngrepIndex(queries: string | string[] | { queries: string[]; case_insensitive?: boolean; path?: string; max_hits?: number }, opts?: object): Promise<{ matches: unknown[]; truncated: boolean }>;\ngraph(req: { op: \"definitions\" | \"callers\" | \"callees\" | \"imports\" | \"tests\" | \"impact\" | \"symbols\"; symbol?: string; path?: string; depth?: number; limit?: number }): Promise<Record<string, unknown>>;\npatchMany(specs: object[]): Promise<string[]>;\natomic<T>(fn: () => Promise<T>): Promise<T>;";
        if mcp_enabled {
            format!(
                "{prose}\n```ts\n{decls}\n// Also on slim: {helpers_block}\n{CODE_HELPERS_MCP}\n{CODE_HELPERS_BASE}\n```"
            )
        } else {
            format!(
                "{prose}\n```ts\n{decls}\n// Also on slim: {helpers_block}\n{CODE_HELPERS_BASE}\n```"
            )
        }
    } else {
        AUTO_DESCRIPTION.to_owned()
    };
    json!({
        "name": "codemode",
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": {"code": {"type": "string", "minLength": 1, "maxLength": MAX_CODE_BYTES}},
            "required": ["code"],
            "additionalProperties": false
        }
    })
}

/// Compact Auto/Plan hybrid batching tool (~420 bytes): lets the model combine
/// multiple native workspace reads/graph queries/disjoint edits in 1 turn
/// without repeating native schemas or breaking `"codemode"` contracts.
pub(super) fn batch_code_definition(code_intel_enabled: bool) -> Value {
    let ci = if code_intel_enabled {
        ", slim.code_intel"
    } else {
        ""
    };
    let description = format!(
        "Run a parallel async JavaScript batch over workspace tools when multi-file inspection or disjoint edits would otherwise need several turns. Available: slim.readMany(paths, opts?), slim.outlineMany(pathsOrReq), slim.grepIndex(queries, opts?), slim.graph({{op:'definitions'|'callers'|'callees'|'imports'|'tests'|'impact'|'symbols', symbol?, path?, depth?, limit?}}), slim.read, slim.list, slim.search, slim.glob, slim.git{ci} (plus slim.write, slim.patch, slim.patchMany, slim.atomic, slim.shell in writable modes). Sync helpers: lines, grepText, diffText, parseJsonSafe, chunk, unique, store/load. Filter in JS and return only the compact summary."
    );
    json!({
        "name": "batch_code",
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": {"code": {"type": "string", "minLength": 1, "maxLength": MAX_CODE_BYTES}},
            "required": ["code"],
            "additionalProperties": false
        }
    })
}

/// The native tools a Code-mode program reaches as `slim.<name>`.
fn native_definitions_for(code_intel_enabled: bool, subagents_enabled: bool) -> &'static [Value] {
    let idx = (usize::from(code_intel_enabled) << 1) | usize::from(subagents_enabled);
    &NATIVE_DEFINITIONS[idx]
}

fn native_readonly_definitions_for(code_intel_enabled: bool) -> &'static [Value] {
    &NATIVE_READONLY_DEFINITIONS[usize::from(code_intel_enabled)]
}

#[cfg(test)]
fn native_definitions(code_intel_enabled: bool) -> &'static [Value] {
    native_definitions_for(code_intel_enabled, false)
}

/// As Auto advertises them, without what a program cannot use.
fn build_native_definitions(
    code_intel_enabled: bool,
    subagents_enabled: bool,
    allow_mutation: bool,
) -> Vec<Value> {
    let mode = if allow_mutation {
        crate::OperatingMode::Code
    } else {
        crate::OperatingMode::ReadOnly
    };
    let mut definitions = ToolRegistry::default()
        .definitions_for_mode_shared(mode)
        .as_ref()
        .to_vec();
    crate::tools::compact_provider_definitions(&mut definitions);
    definitions.retain(|definition| code_intel_enabled || definition["name"] != "code_intel");
    for definition in &mut definitions {
        let name = definition["name"].as_str().unwrap_or_default().to_owned();
        match name.as_str() {
            "read" => {
                if let Some(properties) =
                    definition["input_schema"]["properties"].as_object_mut()
                {
                    properties.insert("structured".into(), json!({"type": "boolean"}));
                }
                definition["return_type"] = json!(
                    "string | { path: string; content: string; lines: string[]; first_line: number; total_lines: number; next_offset: number | null; complete: boolean }"
                );
            }
            "shell" => {
                if let Some(properties) =
                    definition["input_schema"]["properties"].as_object_mut()
                {
                    for argument in UNSUPPORTED_SHELL_ARGUMENTS {
                        properties.remove(argument);
                    }
                    properties.insert("allow_non_zero".into(), json!({"type": "boolean"}));
                    properties.insert("readonly".into(), json!({"type": "boolean"}));
                }
                definition["description"] = Value::String(
                    "Run in workspace; rejects on non-zero exit unless allow_non_zero: true. Pass readonly: true for read-only commands to run in parallel."
                        .into(),
                );
            }
            "write" | "patch" => {
                if let Some(properties) = definition["input_schema"]["properties"]["then_run"]
                    ["properties"]
                    .as_object_mut()
                {
                    for argument in UNSUPPORTED_THEN_RUN_ARGUMENTS {
                        properties.remove(argument);
                    }
                }
            }
            "search" => {
                if let Some(properties) =
                    definition["input_schema"]["properties"].as_object_mut()
                {
                    properties.insert("structured".into(), json!({"type": "boolean"}));
                }
                definition["return_type"] = json!(
                    "string | { hits: { path: string; line: number; text: string; context: { line: number; text: string }[] }[]; total: number; next_cursor: string | null; capped: boolean }"
                );
            }
            "list" => {
                if let Some(properties) =
                    definition["input_schema"]["properties"].as_object_mut()
                {
                    properties.insert("structured".into(), json!({"type": "boolean"}));
                }
                definition["return_type"] = json!(
                    "string | { entries: string[]; total: number; next_cursor: string | null }"
                );
            }
            _ => {}
        }
    }
    definitions.push(json!({
        "name": "glob",
        "description": "Find workspace files matching a glob pattern.",
        "input_schema": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "max_entries": {"type": "integer", "minimum": 1, "maximum": 1000}
            },
            "additionalProperties": false
        },
        "return_type": "{ files: string[]; truncated: boolean; timed_out: boolean }"
    }));
    definitions.push(json!({
        "name": "git",
        "description": "Read-only structured git status, diff, log, branch, or show.",
        "input_schema": {
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["status", "diff", "log", "branch", "show"]},
                "path": {"type": "string"},
                "staged": {"type": "boolean"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50},
                "rev": {"type": "string"}
            },
            "required": ["op"],
            "additionalProperties": false
        },
        "return_type": "Record<string, unknown>"
    }));
    if subagents_enabled {
        definitions.push(super::native_subagent::subagent_definition());
        definitions.push(super::native_subagent::subagent_control_definition());
    }
    definitions
}

/// What every host step of one cell reads.
struct CellScope<'a> {
    parent: ToolInvocation<'a>,
    mode: crate::OperatingMode,
    allow_mutation: bool,
    cwd: &'a Path,
    workspace: Result<PathBuf, String>,
    /// Native definitions reachable as `slim.<name>`; empty outside Code / HybridBatch.
    natives: &'static [Value],
    deadline: Instant,
}

impl CellScope<'_> {
    fn native(&self, name: &str) -> Option<&Value> {
        self.natives
            .iter()
            .find(|definition| definition["name"] == name)
    }
}

fn native_names(natives: &[Value]) -> Vec<String> {
    natives
        .iter()
        .filter_map(|definition| definition["name"].as_str().map(str::to_owned))
        .collect()
}

#[derive(Default)]
pub(super) struct AtomicTx {
    pub effect_start: usize,
    pub backups: Vec<(PathBuf, Option<Vec<u8>>)>,
}

/// Host-side progress of one cell.
struct CellState {
    calls: Scheduler,
    requests: usize,
    /// Why the host stopped the cell, when it did.
    stop: Option<String>,
    /// MCP outcomes that may have taken effect without a result.
    uncertain: Vec<String>,
    /// The program returned; calls still running were not awaited.
    returned: bool,
    /// Failures of calls that settled after the program returned.
    unawaited_failures: Vec<String>,
    /// Advisories from `RunGuards` (e.g. rewrite warning) surfaced at cell end.
    guard_notes: Vec<String>,
    /// Intra-cell cache for snapshot reads; cleared on any non-readonly call.
    read_cache: HashMap<String, dispatch::Finished>,
    /// Incremented whenever a non-readonly call starts or an MCP call settles.
    mutation_epoch: u64,
    /// Number of MCP calls currently in flight; while > 0, reads must not
    /// populate `read_cache` because an overlapping MCP call may mutate disk.
    in_flight_mcp: usize,
    /// Non-readonly side effects applied by this cell in order.
    applied_effects: Vec<Value>,
    /// Active `slim.atomic(...)` transaction backups.
    atomic_tx: Option<AtomicTx>,
}

impl Runtime {
    async fn rollback_atomic(
        &mut self,
        backups: Vec<(PathBuf, Option<Vec<u8>>)>,
        cwd: &Path,
    ) -> Vec<String> {
        let mut rolled_back = Vec::new();
        let mut restored_paths = Vec::with_capacity(backups.len());
        let mut notifications: Vec<(PathBuf, Option<&[u8]>)> = Vec::with_capacity(backups.len());
        let reversed: Vec<(PathBuf, Option<Vec<u8>>)> = backups.into_iter().rev().collect();
        for (path, original) in &reversed {
            match original {
                Some(bytes) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(path, bytes);
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
            let label = path
                .strip_prefix(cwd)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if !rolled_back.iter().any(|existing| existing == &label) {
                rolled_back.push(label);
            }
            if !restored_paths.contains(path) {
                restored_paths.push(path.clone());
                notifications.push((path.clone(), original.as_deref()));
            }
        }
        self.tools.notify_external_paths_changed(cwd, &notifications);
        self.edited.remove_paths(&restored_paths);
        if let Some(code_intel) = self.code_intel.clone() {
            let sync = async {
                for path in &restored_paths {
                    code_intel.notify_file_changed(cwd, path, None).await;
                }
            };
            if let Some(cancel) = &self.cancellation {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {},
                    _ = sync => {},
                }
            } else {
                sync.await;
            }
        }
        rolled_back
    }
}

impl CellState {
    /// Notes attached to the cell result (guard advisories + unawaited failures).
    fn cell_notes(&self) -> Option<String> {
        let mut notes = self.guard_notes.clone();
        if !self.unawaited_failures.is_empty() {
            notes.push(format!(
                "calls not awaited by the program failed: {}",
                self.unawaited_failures.join("; ")
            ));
        }
        (!notes.is_empty()).then(|| notes.join("\n"))
    }
}

/// Counts one host request; past the limit the cell stops.
fn count_request(cell: &engine::Cell, state: &mut CellState) -> bool {
    state.requests += 1;
    let over_limit = state.requests > MAX_HOST_REQUESTS;
    if over_limit {
        state
            .stop
            .get_or_insert_with(|| "cell host-request limit exceeded".into());
        cell.cancellation.cancel();
    }
    over_limit
}

/// How the host loop ended.
enum CellEnd {
    /// The program ran to its end, with every call it started settled.
    Finished(engine::Output),
    /// The run, the deadline or the host stopped it.
    Stopped(String, engine::Output),
}

impl Runtime {
    pub(super) async fn execute_codemode_with_governor(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
        governor: Option<&mut CausalGovernor>,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started = self.begin_tool(invocation, &mut seq)?;
        let used_before = self.codemode.used_calls;
        let prior_final_answer = self.codemode.pending_final_answer.take();
        self.codemode.store_mutated = false;
        self.codemode.cell_mutated_workspace = false;
        self.codemode.cell_had_non_readonly_effect = false;
        self.codemode.cell_validation_green = false;
        self.codemode.last_applied_effects.clear();
        let is_batch_code = invocation.name == "batch_code";
        let wall = if mode == crate::OperatingMode::Code || is_batch_code {
            CODE_CELL_TIMEOUT
        } else {
            CELL_TIMEOUT
        };
        let profile = if is_batch_code {
            if !batch_code_enabled() {
                None
            } else if mode == crate::OperatingMode::Code {
                Some(CellProfile::FullCode)
            } else {
                Some(CellProfile::HybridBatch {
                    allow_mutation: mode.allows_mutation(),
                })
            }
        } else if mode.allows_mutation() {
            Some(if mode == crate::OperatingMode::Code {
                CellProfile::FullCode
            } else {
                CellProfile::McpSerial
            })
        } else {
            None
        };
        let outcome = if let Some(profile) = profile {
            self.run_codemode_with_governor(mode, profile, cwd, invocation, &mut seq, wall, governor)
                .await?
        } else if is_batch_code {
            Err("batch_code is disabled in this session".into())
        } else {
            Err("codemode is only available in Auto and Code modes".into())
        };
        if outcome.is_err() {
            self.codemode.pending_final_answer = None;
        } else if self.codemode.pending_final_answer.is_none() {
            self.codemode.pending_final_answer = prior_final_answer;
        }
        self.codemode.last_cell_had_effect =
            self.codemode.cell_had_non_readonly_effect || self.codemode.store_mutated;
        self.codemode.cell_effects.insert(
            invocation.call_id.to_owned(),
            CellEffectSummary {
                mutated_workspace: self.codemode.cell_mutated_workspace,
                had_non_readonly_effect: self.codemode.cell_had_non_readonly_effect,
                had_effect: self.codemode.last_cell_had_effect,
                had_validation_green: self.codemode.cell_validation_green,
            },
        );
        let mut result = match outcome {
            Ok(output) => ToolResult::ok(invocation.name, output),
            Err(error) => {
                let auto_hint = if mode == crate::OperatingMode::Auto
                    && !is_batch_code
                    && (error.contains("'slim'")
                        || error.contains("slim is not defined")
                        || error.contains("'finish'")
                        || error.contains("finish is not defined"))
                {
                    "\nHint: in Auto mode, slim.<tool>() and finish() are not available; call native workspace tools (read, list, search, write, patch, shell) directly and use codemode only for MCP calls via tools.call(...)."
                } else {
                    ""
                };
                let effects_note = if self.codemode.last_applied_effects.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\napplied_effects: {}",
                        serde_json::to_string(&self.codemode.last_applied_effects)
                            .unwrap_or_else(|_| "[]".into())
                    )
                };
                ToolResult::fail(invocation.name, format!(
                    "{} failed after {} tool call(s); stored values and prior tool effects remain, do not replay the cell automatically.\n{error}{effects_note}{auto_hint}",
                    invocation.name,
                    self.codemode.used_calls - used_before,
                ))
            }
        };
        self.finish_tool(invocation, &mut result, started, &mut seq, None)?;
        Ok((result, seq))
    }

    /// Runs one cell within the `wall` deadline and the JavaScript CPU limit.
    #[cfg_attr(not(test), allow(dead_code))]
    async fn run_codemode(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        seq: &mut u64,
        wall: Duration,
    ) -> Result<Result<String, String>, ProviderError> {
        let profile = if invocation.name == "batch_code" {
            CellProfile::HybridBatch {
                allow_mutation: mode.allows_mutation(),
            }
        } else if mode == crate::OperatingMode::Code {
            CellProfile::FullCode
        } else {
            CellProfile::McpSerial
        };
        self.run_codemode_with_governor(mode, profile, cwd, invocation, seq, wall, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_codemode_with_governor(
        &mut self,
        mode: crate::OperatingMode,
        profile: CellProfile,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        seq: &mut u64,
        wall: Duration,
        governor: Option<&mut CausalGovernor>,
    ) -> Result<Result<String, String>, ProviderError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            code: String,
        }
        let args: Arguments = match serde_json::from_str(invocation.arguments) {
            Ok(args) => args,
            Err(error) => return Ok(Err(format!("invalid {} arguments: {error}", invocation.name))),
        };
        if args.code.trim().is_empty() || args.code.len() > MAX_CODE_BYTES {
            return Ok(Err("code must contain 1-65536 bytes".into()));
        }
        self.restore_codemode()?;
        let mut fallback_governor;
        let governor = match governor {
            Some(governor) => governor,
            None => {
                fallback_governor = CausalGovernor::default();
                &mut fallback_governor
            }
        };
        let code_intel_supported = self
            .code_intel
            .as_ref()
            .is_some_and(|backend| backend.supports_workspace(cwd));
        let (natives, allow_mutation, parallel_limit): (&'static [Value], bool, usize) =
            match profile {
                CellProfile::FullCode => {
                    let subagents_enabled = self.subagent_depth == 0 && self.subagents.is_some();
                    (
                        native_definitions_for(code_intel_supported, subagents_enabled),
                        true,
                        dispatch::MAX_PARALLEL_CALLS,
                    )
                }
                CellProfile::HybridBatch { allow_mutation } => {
                    let defs = if allow_mutation {
                        native_definitions_for(code_intel_supported, false)
                    } else {
                        native_readonly_definitions_for(code_intel_supported)
                    };
                    (defs, allow_mutation, dispatch::MAX_PARALLEL_CALLS)
                }
                CellProfile::McpSerial => (&[], mode.allows_mutation(), 1),
            };
        let run = self.cancellation.clone().unwrap_or_default();
        let limits = engine::Limits {
            wall,
            cpu: JS_CPU_LIMIT,
        };
        let mut cell = engine::spawn(args.code, run.clone(), limits, native_names(natives));
        let scope = CellScope {
            parent: invocation,
            mode,
            allow_mutation,
            cwd,
            workspace: crate::tools::canonical_workspace(cwd),
            natives,
            deadline: cell.deadline,
        };
        let mut state = CellState {
            calls: Scheduler::new(
                dispatch::CallEnv {
                    tools: self.tools.clone(),
                    code_intel: self.code_intel.clone(),
                    cell: cell.cancellation.clone(),
                    run: run.clone(),
                },
                parallel_limit,
            ),
            requests: 0,
            stop: None,
            uncertain: Vec::new(),
            returned: false,
            unawaited_failures: Vec::new(),
            guard_notes: Vec::new(),
            read_cache: HashMap::new(),
            mutation_epoch: 0,
            in_flight_mcp: 0,
            applied_effects: Vec::new(),
            atomic_tx: None,
        };
        let ended = self
            .drive_cell(&scope, &run, &mut cell, &mut state, seq, governor)
            .await;
        // A stopped cell starts nothing more; calls already sent finish and
        // are journaled. A journal failure keeps the first error and still
        // collects the others; dropping the cell then stops the interpreter.
        state.calls.discard_queued();
        let mut journal_error = None;
        while let Some(done) = state.calls.next_finished().await {
            if let Err(error) = self
                .finish_call(&scope, &cell, &mut state, seq, governor, done)
                .await
            {
                journal_error.get_or_insert(error);
            }
        }
        let ended = ended?;
        if let Some(error) = journal_error {
            if let Some(tx) = state.atomic_tx.take() {
                self.rollback_atomic(tx.backups, scope.cwd).await;
                state.read_cache.clear();
                self.codemode.session_read_cache.clear();
            }
            self.codemode.last_applied_effects = state.applied_effects;
            return Err(error);
        }
        let cell_succeeded = matches!(ended, CellEnd::Finished(ref out) if out.result.is_ok() && !out.has_unhandled_rejections())
            && state.unawaited_failures.is_empty();
        if !cell_succeeded {
            if let Some(tx) = state.atomic_tx.take() {
                self.rollback_atomic(tx.backups, scope.cwd).await;
                state.applied_effects.truncate(tx.effect_start);
                self.codemode.cell_mutated_workspace = state.applied_effects.iter().any(|e| {
                    matches!(
                        e.get("call").and_then(Value::as_str),
                        Some("slim.write" | "slim.patch")
                    )
                });
                self.codemode.cell_validation_green = false;
                state.read_cache.clear();
                self.codemode.session_read_cache.clear();
            }
        }
        self.codemode.last_applied_effects = state.applied_effects.clone();
        if self.codemode.store_mutated {
            governor.note_internal_state_change();
        }
        let note = state.cell_notes();
        let unawaited_failed = !state.unawaited_failures.is_empty();
        Ok(match ended {
            CellEnd::Finished(output) => {
                if output.result.is_err() || output.has_unhandled_rejections() || unawaited_failed {
                    self.codemode.pending_final_answer = None;
                }
                output.finish(note.as_deref())
            }
            CellEnd::Stopped(mut reason, mut output) => {
                self.codemode.pending_final_answer = None;
                for uncertain in &state.uncertain {
                    reason.push_str("; ");
                    reason.push_str(uncertain);
                }
                output.result = Err(reason);
                output.finish(note.as_deref())
            }
        })
    }

    /// The host loop: answers requests, starts the calls the scheduler
    /// admits and settles the ones that finish, until the program ends or
    /// the cell is stopped.
    async fn drive_cell(
        &mut self,
        scope: &CellScope<'_>,
        run: &CancellationToken,
        cell: &mut engine::Cell,
        state: &mut CellState,
        seq: &mut u64,
        governor: &mut CausalGovernor,
    ) -> Result<CellEnd, ProviderError> {
        enum Event {
            Stopped,
            Worker(Result<engine::Output, tokio::task::JoinError>),
            Request(Option<Request>),
            Finished(Done),
        }
        loop {
            // On an error the calls already sent are left to finish: the
            // caller drains them before the cell is dropped and stopped.
            self.start_ready_calls(scope, cell, state, seq, governor)?;
            let cancellation = cell.cancellation.clone();
            let event = tokio::select! {
                biased;
                () = cancellation.cancelled() => Event::Stopped,
                result = &mut cell.worker => Event::Worker(result),
                request = cell.requests.recv() => Event::Request(request),
                Some(done) = state.calls.next_finished() => Event::Finished(done),
            };
            let step = match event {
                Event::Stopped => {
                    let reason = state.stop.take().unwrap_or_else(|| {
                        if run.is_cancelled() {
                            "cell cancelled".into()
                        } else if Instant::now() >= cell.deadline {
                            "cell deadline exceeded".into()
                        } else {
                            "cell stopped after an uncertain tool outcome".into()
                        }
                    });
                    return Ok(CellEnd::Stopped(reason, join_worker(cell).await));
                }
                Event::Worker(result) => return Ok(CellEnd::Finished(joined(result))),
                Event::Request(None) => {
                    return Ok(CellEnd::Finished(joined((&mut cell.worker).await)))
                }
                Event::Request(Some(request)) => {
                    self.answer_request(scope, cell, state, request).await
                }
                Event::Finished(done) => {
                    self.finish_call(scope, cell, state, seq, governor, done)
                        .await
                }
            };
            step?;
        }
    }

    async fn answer_request(
        &mut self,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
        state: &mut CellState,
        request: Request,
    ) -> Result<(), ProviderError> {
        const OVER_LIMIT: &str = "cell host-request limit exceeded";
        match request {
            Request::Returned => state.returned = true,
            Request::Call {
                ticket,
                name,
                arguments,
                raw_arguments,
            } => {
                if count_request(cell, state) {
                    cell.reject(ticket, OVER_LIMIT);
                    return Ok(());
                }
                match self.admit_call(scope, ticket, name, arguments, raw_arguments) {
                    Ok(job) => state.calls.push(job),
                    Err(error) => cell.reject(ticket, error),
                }
            }
            Request::Helper { command } => {
                let response = if count_request(cell, state) {
                    Err(OVER_LIMIT.into())
                } else {
                    self.answer_helper(scope, cell, state, command).await?
                };
                cell.reply_helper(response);
            }
        }
        Ok(())
    }

    async fn answer_helper(
        &mut self,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
        state: &mut CellState,
        command: Command,
    ) -> Result<Result<Value, String>, ProviderError> {
        Ok(match command {
            Command::Store { key, mut value } => {
                redact_value(&mut value, &self.sensitive_values.0);
                let key = self.redact_sensitive(&key);
                match apply_store_incremental(
                    &mut self.codemode.values,
                    &mut self.codemode.values_bytes_estimate,
                    &key,
                    &value,
                ) {
                    Ok(()) => {
                        self.persist_codemode_fact(STATE_NAMESPACE, &key, value)?;
                        self.codemode.store_mutated = true;
                        Ok(Value::Null)
                    }
                    Err(error) => Err(error),
                }
            }
            Command::Load { key } => Ok(self
                .codemode
                .values
                .get(&self.redact_sensitive(&key))
                .cloned()
                .unwrap_or(Value::Null)),
            Command::Finish { answer } => {
                if scope.mode != crate::OperatingMode::Code {
                    return Ok(Err("finish() is only available in Code mode".into()));
                }
                if answer.trim().is_empty() || answer.len() > MAX_JSON_BYTES {
                    return Ok(Err(
                        "finish() answer must contain 1-1048576 non-whitespace bytes".into(),
                    ));
                }
                let answer = self.redact_sensitive(&answer);
                self.codemode.pending_final_answer = Some(answer);
                Ok(Value::Null)
            }
            Command::BeginAtomic => {
                if !scope.allow_mutation || scope.natives.is_empty() {
                    return Ok(Err("slim.atomic() requires a writable workspace cell".into()));
                }
                if state.atomic_tx.is_some() {
                    return Ok(Err("nested slim.atomic() transactions are not supported".into()));
                }
                state.atomic_tx = Some(AtomicTx {
                    effect_start: state.applied_effects.len(),
                    backups: Vec::new(),
                });
                Ok(Value::Null)
            }
            Command::CommitAtomic => {
                state.atomic_tx = None;
                Ok(Value::Null)
            }
            Command::RollbackAtomic => {
                if let Some(tx) = state.atomic_tx.take() {
                    let rolled_back = self.rollback_atomic(tx.backups, scope.cwd).await;
                    state.applied_effects.truncate(tx.effect_start);
                    self.codemode.cell_mutated_workspace = state.applied_effects.iter().any(|e| {
                        matches!(
                            e.get("call").and_then(Value::as_str),
                            Some("slim.write" | "slim.patch")
                        )
                    });
                    self.codemode.cell_validation_green = false;
                    state.read_cache.clear();
                    self.codemode.session_read_cache.clear();
                    state.mutation_epoch = state.mutation_epoch.saturating_add(1);
                    Ok(json!({ "rolled_back": rolled_back }))
                } else {
                    Ok(json!({ "rolled_back": [] }))
                }
            }
            command => self.codemode_discovery(command, scope, cell).await,
        })
    }

    fn restore_codemode(&mut self) -> Result<(), ProviderError> {
        if self.codemode.restored {
            return Ok(());
        }
        if let Some(journal) = &self.app.run_journal {
            let mut values = self.codemode.values.clone();
            {
                let journal = lock_journal(journal)?;
                for record in journal.repo().records() {
                    if let DurableRecord::Fact { fact, .. } = record {
                        if fact.namespace == STATE_NAMESPACE {
                            let key = self.redact_sensitive(&fact.key);
                            if key.is_empty() || key.len() > 128 {
                                return Err(journal_error("store key must contain 1-128 bytes"));
                            }
                            if fact.value.is_null() {
                                values.remove(&key);
                            } else {
                                let mut value = fact.value.clone();
                                redact_value(&mut value, &self.sensitive_values.0);
                                values.insert(key, value);
                            }
                        }
                    }
                }
            }
            let restored_len = serde_json::to_vec(&values)
                .map_err(journal_error)?
                .len();
            if restored_len > MAX_STATE_BYTES {
                return Err(journal_error(
                    "stored session values exceed 512 KiB; remove values with store(key,null)",
                ));
            }
            self.codemode.values = values;
            self.codemode.values_bytes_estimate = restored_len;
            self.codemode.restored = true;
        }
        Ok(())
    }

    fn persist_codemode_fact(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), ProviderError> {
        self.persist_codemode_fact_with_sync(namespace, key, value, true)
    }

    fn persist_codemode_fact_unsynced(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), ProviderError> {
        self.persist_codemode_fact_with_sync(namespace, key, value, false)
    }

    fn persist_codemode_fact_with_sync(
        &self,
        namespace: &str,
        key: &str,
        mut value: Value,
        synced: bool,
    ) -> Result<(), ProviderError> {
        if let Some(journal) = &self.app.run_journal {
            redact_value(&mut value, &self.sensitive_values.0);
            let fact = DurableFact {
                namespace: namespace.into(),
                key: self.redact_sensitive(key),
                value,
            };
            let write_fact = || {
                let mut journal = lock_journal(journal)?;
                let result = if synced {
                    journal.record_fact(fact)
                } else {
                    journal.record_fact_unsynced(fact)
                };
                result.map_err(journal_error)
            };
            if synced {
                super::events::journal_io(write_fact)?;
            } else {
                write_fact()?;
            }
        }
        Ok(())
    }
}

fn joined(result: Result<engine::Output, tokio::task::JoinError>) -> engine::Output {
    result.unwrap_or_else(|error| engine::Output::failed(format!("interpreter worker: {error}")))
}

/// Waits for a stopped cell's worker, refusing what it still asks for so a
/// full request channel cannot hold it.
async fn join_worker(cell: &mut engine::Cell) -> engine::Output {
    const STOPPED: &str = "cell cancelled or deadline exceeded; call not sent";
    loop {
        let request = tokio::select! {
            result = &mut cell.worker => return joined(result),
            request = cell.requests.recv() => request,
        };
        match request {
            Some(Request::Call { ticket, .. }) => cell.reject(ticket, STOPPED),
            Some(Request::Helper { .. }) => cell.reply_helper(Err(STOPPED.into())),
            Some(Request::Returned) => {}
            None => return joined((&mut cell.worker).await),
        }
    }
}

fn redact_value(value: &mut Value, secrets: &[String]) {
    if !secrets.iter().any(|secret| !secret.is_empty()) {
        return;
    }
    match value {
        Value::String(text) => *text = redact_values(secrets, text),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_value(item, secrets)),
        Value::Object(items) => {
            *items = std::mem::take(items)
                .into_iter()
                .map(|(key, mut value)| {
                    redact_value(&mut value, secrets);
                    (redact_values(secrets, &key), value)
                })
                .collect();
        }
        _ => {}
    }
}

fn apply_store_incremental(
    values: &mut BTreeMap<String, Value>,
    estimate_bytes: &mut usize,
    key: &str,
    value: &Value,
) -> Result<(), String> {
    if key.is_empty() || key.len() > 128 {
        return Err("store key must contain 1-128 bytes".into());
    }
    let old_entry = values.get(key).cloned();
    let old_entry_bytes = old_entry
        .as_ref()
        .and_then(|v| serde_json::to_vec(v).ok())
        .map_or(0, |vec| key.len() + 4 + vec.len());
    if value.is_null() {
        if values.remove(key).is_some() {
            *estimate_bytes = estimate_bytes.saturating_sub(old_entry_bytes).max(2);
        }
        return Ok(());
    }
    let new_val_bytes = serde_json::to_vec(value)
        .map_err(|error| error.to_string())?
        .len();
    let new_entry_bytes = key.len() + 4 + new_val_bytes;
    let next_est = (*estimate_bytes)
        .max(2)
        .saturating_sub(old_entry_bytes)
        .saturating_add(new_entry_bytes);
    values.insert(key.to_owned(), value.clone());
    if next_est.saturating_add(64) > MAX_STATE_BYTES {
        let exact = serde_json::to_vec(values)
            .map_err(|error| error.to_string())?
            .len();
        if exact > MAX_STATE_BYTES {
            match old_entry {
                Some(prev) => {
                    values.insert(key.to_owned(), prev);
                }
                None => {
                    values.remove(key);
                }
            }
            return Err(
                "stored session values exceed 512 KiB; remove values with store(key,null)".into(),
            );
        }
        *estimate_bytes = exact;
    } else {
        *estimate_bytes = next_est;
    }
    Ok(())
}
