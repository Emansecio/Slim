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

use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::session::{DurableFact, DurableRecord, DurableRepo};
use dispatch::{Done, Scheduler};
use engine::{Command, Request, MAX_CODE_BYTES, MAX_JSON_BYTES};

const STATE_NAMESPACE: &str = "codemode.state.v1";
const CALL_NAMESPACE: &str = "codemode.call.v1";
const MAX_STATE_BYTES: usize = 64 * 1024;
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

const AUTO_DESCRIPTION: &str = "Compose MCP calls in isolated JavaScript. Use await tools.call('mcp.server.tool', arguments): returns structuredContent when present, otherwise the MCP result envelope; isError throws. Discover names/schemas with mcp first, or in code with searchTools(query,{limit?,server?}) -> [{name,server,tool,description}] (BM25), describeTool(name) -> {inputSchema,...}|null and listServers() (synchronous; hidden tools never appear). Calls execute in order, one at a time (no parallel calls) and each consumes the current tool budget. return only the needed summary. store(key, JSON_value) / load(key) persist session values (64 KiB total); store(key,null) deletes. No filesystem, network, Node or imports. Cells have fresh globals, 32 MiB memory and 120s deadline; store writes and completed tool effects survive a later error. Never replay a failed cell blindly.";

const CODE_DESCRIPTION: &str = "Act by running a short async JavaScript program; this is your only tool for the workspace. Only the JSON value you return, plus captured console output, comes back (up to 1 MiB each; a long output is previewed with an artifact id): keep intermediate results in the program and return just what you need. Workspace tools are the typed functions of `slim` below: each resolves to the tool's text output and rejects with an Error on failure. Promise.all declares the calls independent: reads, searches, code_intel and MCP calls then run in parallel (up to 8), MCP calls included, so await in sequence whatever must happen in order; only write, patch and shell are serialized by the host, alone and in request order. Calls you do not await still run before the cell ends, and their failures are noted after the result. MCP tools are not listed: find them with searchTools/describeTool/listServers (synchronous), then await tools.call('mcp.server.tool', args) (structuredContent when present, isError rejects). Every call consumes the tool budget (an edit with then_run counts twice). No filesystem, network, Node or imports beyond these. Cells have fresh globals, 32 MiB memory, 120 s of JavaScript CPU time (waits for calls excluded) and a 3600 s wall deadline; shell timeout_ms is capped to the time left. store(key, value)/load(key) keep JSON across cells (64 KiB, null deletes). Completed effects survive a later error: never replay a failed cell blindly.";

const CODE_HELPERS: &str = "declare const tools: { call(name: string, args?: object): Promise<unknown> };
declare function searchTools(query: string, options?: { limit?: number; server?: string }): { name: string; server: string; tool: string; description: string }[];
declare function describeTool(name: string): { name: string; description?: string; inputSchema: unknown; outputSchema?: unknown; signature: string } | null;
declare function listServers(): unknown[];
declare function store(key: string, value: unknown): void;
declare function load(key: string): unknown;
declare const console: { log(...values: unknown[]): void; info(...values: unknown[]): void; warn(...values: unknown[]): void; error(...values: unknown[]): void };";

/// `slim` definitions without and with code_intel, built once.
static NATIVE_DEFINITIONS: LazyLock<[Vec<Value>; 2]> = LazyLock::new(|| {
    [
        build_native_definitions(false),
        build_native_definitions(true),
    ]
});

#[derive(Default)]
pub(super) struct CodeMode {
    values: BTreeMap<String, Value>,
    restored: bool,
    pub remaining_calls: usize,
    pub used_calls: usize,
}

pub(super) fn definition(mode: crate::OperatingMode, code_intel_enabled: bool) -> Value {
    let description = if mode == crate::OperatingMode::Code {
        format!(
            "{CODE_DESCRIPTION}\n```ts\n{}\n{CODE_HELPERS}\n```",
            typescript::declarations("slim", native_definitions(code_intel_enabled))
        )
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

/// The native tools a Code-mode program reaches as `slim.<name>`.
fn native_definitions(code_intel_enabled: bool) -> &'static [Value] {
    &NATIVE_DEFINITIONS[usize::from(code_intel_enabled)]
}

/// As Auto advertises them, without what a program cannot use.
fn build_native_definitions(code_intel_enabled: bool) -> Vec<Value> {
    let mut definitions = ToolRegistry::default()
        .definitions_for_mode_shared(crate::OperatingMode::Code)
        .as_ref()
        .to_vec();
    crate::tools::compact_provider_definitions(&mut definitions);
    definitions.retain(|definition| code_intel_enabled || definition["name"] != "code_intel");
    for definition in &mut definitions {
        let name = definition["name"].as_str().unwrap_or_default().to_owned();
        let (schema, unsupported): (&mut Value, &[&str]) = match name.as_str() {
            "shell" => (
                &mut definition["input_schema"],
                &UNSUPPORTED_SHELL_ARGUMENTS,
            ),
            "write" | "patch" => (
                &mut definition["input_schema"]["properties"]["then_run"],
                &UNSUPPORTED_THEN_RUN_ARGUMENTS,
            ),
            _ => continue,
        };
        if let Some(properties) = schema["properties"].as_object_mut() {
            for argument in unsupported {
                properties.remove(*argument);
            }
        }
    }
    definitions
}

/// What every host step of one cell reads.
struct CellScope<'a> {
    parent: ToolInvocation<'a>,
    mode: crate::OperatingMode,
    cwd: &'a Path,
    /// Native definitions reachable as `slim.<name>`; empty outside Code.
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
}

impl CellState {
    /// The note on calls the program did not await, when one failed.
    fn unawaited_note(&self) -> Option<String> {
        (!self.unawaited_failures.is_empty()).then(|| {
            format!(
                "calls not awaited by the program failed: {}",
                self.unawaited_failures.join("; ")
            )
        })
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
    pub(super) async fn execute_codemode(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started = self.begin_tool(invocation, &mut seq)?;
        let used_before = self.codemode.used_calls;
        let wall = if mode == crate::OperatingMode::Code {
            CODE_CELL_TIMEOUT
        } else {
            CELL_TIMEOUT
        };
        let outcome = if mode.allows_mutation() {
            self.run_codemode(mode, cwd, invocation, &mut seq, wall)
                .await?
        } else {
            Err("codemode is only available in Auto and Code modes".into())
        };
        let mut result = match outcome {
            Ok(output) => ToolResult::ok(invocation.name, output),
            Err(error) => ToolResult::fail(invocation.name, format!(
                "codemode failed after {} tool call(s); stored values and prior tool effects remain, do not replay the cell automatically.\n{error}",
                self.codemode.used_calls - used_before,
            )),
        };
        self.finish_tool(invocation, &mut result, started, &mut seq, None)?;
        Ok((result, seq))
    }

    /// Runs one cell within the `wall` deadline and the JavaScript CPU limit.
    async fn run_codemode(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        seq: &mut u64,
        wall: Duration,
    ) -> Result<Result<String, String>, ProviderError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            code: String,
        }
        let args: Arguments = match serde_json::from_str(invocation.arguments) {
            Ok(args) => args,
            Err(error) => return Ok(Err(format!("invalid codemode arguments: {error}"))),
        };
        if args.code.trim().is_empty() || args.code.len() > MAX_CODE_BYTES {
            return Ok(Err("code must contain 1-65536 bytes".into()));
        }
        self.restore_codemode()?;
        let natives: &'static [Value] = if mode == crate::OperatingMode::Code {
            native_definitions(
                self.code_intel
                    .as_ref()
                    .is_some_and(|backend| backend.supports_workspace(cwd)),
            )
        } else {
            &[]
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
            cwd,
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
                // Auto keeps its contract: one call at a time.
                if mode == crate::OperatingMode::Code {
                    dispatch::MAX_PARALLEL_CALLS
                } else {
                    1
                },
            ),
            requests: 0,
            stop: None,
            uncertain: Vec::new(),
            returned: false,
            unawaited_failures: Vec::new(),
        };
        let ended = self
            .drive_cell(&scope, &run, &mut cell, &mut state, seq)
            .await;
        // A stopped cell starts nothing more; calls already sent finish and
        // are journaled. A journal failure keeps the first error and still
        // collects the others; dropping the cell then stops the interpreter.
        state.calls.discard_queued();
        let mut journal_error = None;
        while let Some(done) = state.calls.next_finished().await {
            if let Err(error) = self.finish_call(&scope, &cell, &mut state, seq, done).await {
                journal_error.get_or_insert(error);
            }
        }
        let ended = ended?;
        if let Some(error) = journal_error {
            return Err(error);
        }
        let note = state.unawaited_note();
        Ok(match ended {
            CellEnd::Finished(output) => output.finish(note.as_deref()),
            CellEnd::Stopped(mut reason, mut output) => {
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
            self.start_ready_calls(scope, cell, state, seq)?;
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
                Event::Finished(done) => self.finish_call(scope, cell, state, seq, done).await,
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
            } => {
                if count_request(cell, state) {
                    cell.settle(ticket, Err(OVER_LIMIT.into()));
                    return Ok(());
                }
                match self.admit_call(scope, ticket, name, arguments) {
                    Ok(job) => state.calls.push(job),
                    Err(error) => cell.settle(ticket, Err(error)),
                }
            }
            Request::Helper { command, reply } => {
                let response = if count_request(cell, state) {
                    Err(OVER_LIMIT.into())
                } else {
                    self.answer_helper(scope, cell, command).await?
                };
                let _ = reply.send(response);
            }
        }
        Ok(())
    }

    async fn answer_helper(
        &mut self,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
        command: Command,
    ) -> Result<Result<Value, String>, ProviderError> {
        Ok(match command {
            Command::Store { key, mut value } => {
                redact_value(&mut value, &self.sensitive_values.0);
                let key = self.redact_sensitive(&key);
                match updated_state(&self.codemode.values, &key, &value) {
                    Ok(values) => {
                        self.persist_codemode_fact(STATE_NAMESPACE, &key, value)?;
                        self.codemode.values = values;
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
            command => self.codemode_discovery(command, scope, cell).await,
        })
    }

    fn restore_codemode(&mut self) -> Result<(), ProviderError> {
        if self.codemode.restored {
            return Ok(());
        }
        if let Some(journal) = &self.app.run_journal {
            let journal = lock_journal(journal)?;
            for record in journal.repo().records() {
                if let DurableRecord::Fact { fact, .. } = record {
                    if fact.namespace == STATE_NAMESPACE {
                        let mut value = fact.value.clone();
                        redact_value(&mut value, &self.sensitive_values.0);
                        self.codemode.values = updated_state(
                            &self.codemode.values,
                            &self.redact_sensitive(&fact.key),
                            &value,
                        )
                        .map_err(journal_error)?;
                    }
                }
            }
        }
        self.codemode.restored = true;
        Ok(())
    }

    fn persist_codemode_fact(
        &self,
        namespace: &str,
        key: &str,
        mut value: Value,
    ) -> Result<(), ProviderError> {
        if let Some(journal) = &self.app.run_journal {
            redact_value(&mut value, &self.sensitive_values.0);
            lock_journal(journal)?
                .record_fact(DurableFact {
                    namespace: namespace.into(),
                    key: self.redact_sensitive(key),
                    value,
                })
                .map_err(journal_error)?;
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
            Some(Request::Call { ticket, .. }) => cell.settle(ticket, Err(STOPPED.into())),
            Some(Request::Helper { reply, .. }) => {
                let _ = reply.send(Err(STOPPED.into()));
            }
            Some(Request::Returned) => {}
            None => return joined((&mut cell.worker).await),
        }
    }
}

fn redact_value(value: &mut Value, secrets: &[String]) {
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

fn updated_state(
    values: &BTreeMap<String, Value>,
    key: &str,
    value: &Value,
) -> Result<BTreeMap<String, Value>, String> {
    if key.is_empty() || key.len() > 128 {
        return Err("store key must contain 1-128 bytes".into());
    }
    let mut next = values.clone();
    if value.is_null() {
        next.remove(key);
    } else {
        next.insert(key.into(), value.clone());
    }
    if serde_json::to_vec(&next)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_STATE_BYTES
    {
        return Err(
            "stored session values exceed 64 KiB; remove values with store(key,null)".into(),
        );
    }
    Ok(next)
}
