//! Lifecycle of one internal call: admission when the program asks for it,
//! start when the scheduler admits it (budget, run guards, task, journal,
//! step event) and settlement when it finishes.

use super::*;
use dispatch::{Concurrency, Done, Finished, Job, Running, Target};
use engine::Payload;

/// Bytes of a failed native output kept in the journal.
pub(super) const MAX_JOURNALED_ERROR_BYTES: usize = 1024;
/// Output a step event carries for display; the program gets all of it.
const MAX_STEP_OUTPUT_BYTES: usize = 16 * 1024;
/// Characters of an unawaited failure kept in the result's note.
const MAX_NOTE_ERROR_CHARS: usize = 200;
/// Threshold past which `codemode.call.v1` `started.arguments` is compacted.
const MAX_JOURNALED_ARGUMENT_BYTES: usize = 1024;
/// Maximum entries kept in the per-cell snapshot read cache.
const MAX_CELL_READ_CACHE: usize = 32;

impl Runtime {
    /// Checks a requested call and resolves what it runs. Nothing is sent
    /// and no budget is used until the call starts.
    pub(super) fn admit_call(
        &self,
        scope: &CellScope<'_>,
        ticket: u32,
        name: String,
        mut arguments: Value,
        raw_arguments: String,
    ) -> Result<Job, String> {
        if !arguments.is_object() {
            return Err("tool arguments must be an object".into());
        }
        if super::stream_normalizer::sensitive_tool_arguments(
            &raw_arguments,
            &self.sensitive_values.0,
        ) || self.redact_sensitive(&name) != name
        {
            return Err("tool call contains registered sensitive material; use a configured credential reference".into());
        }
        let mut allow_non_zero = false;
        let mut structured = false;
        let mut read_only_override = false;
        let (target, slots) = if let Some(native) = name.strip_prefix("slim.") {
            if native == "shell" {
                allow_non_zero = take_optional_bool(&mut arguments, "allow_non_zero")?;
                read_only_override = take_optional_bool(&mut arguments, "readonly")?;
            } else if matches!(native, "read" | "readMany" | "search" | "list") {
                structured = take_optional_bool(&mut arguments, "structured")?;
            }
            let slots = if matches!(native, "readMany" | "outlineMany" | "grepIndex" | "graph") {
                1
            } else {
                super::tool_schedule::tool_call_slots_value(native, &arguments)
            };
            (
                self.native_target(scope, native, arguments, read_only_override, structured)?,
                slots,
            )
        } else if let Some((server, tool)) = name
            .strip_prefix("mcp.")
            .and_then(|name| name.split_once('.'))
            .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
        {
            let Some(manager) = self.mcp.clone() else {
                return Err("no MCP servers configured".into());
            };
            let read_only = manager.is_tool_read_only(server, tool);
            let target = Target::Mcp {
                manager,
                server: server.to_owned(),
                tool: tool.to_owned(),
                arguments,
                read_only,
            };
            (target, 1)
        } else if scope.natives.is_empty() {
            return Err("tool name must be mcp.server.tool".into());
        } else {
            return Err("tool name must be slim.<tool> or mcp.server.tool".into());
        };
        Ok(Job {
            ticket,
            name,
            slots,
            allow_non_zero,
            structured,
            raw_arguments,
            target,
        })
    }

    fn native_target(
        &self,
        scope: &CellScope<'_>,
        name: &str,
        mut arguments: Value,
        read_only_override: bool,
        structured: bool,
    ) -> Result<Target, String> {
        if scope.natives.is_empty() {
            return Err("slim.<tool> functions are only available in Code mode".into());
        }
        if matches!(name, "readMany" | "outlineMany" | "grepIndex" | "graph") {
            if scope.native("read").is_none() {
                return Err(format!("slim.{name} requires read access in this cell"));
            }
            return match name {
                "readMany" => prepare_read_many_target(scope, arguments, structured),
                "outlineMany" => prepare_outline_many_target(scope, arguments),
                "grepIndex" => prepare_grep_index_target(scope, arguments),
                "graph" => prepare_graph_target(scope, arguments),
                _ => unreachable!(),
            };
        }
        if !scope.allow_mutation && matches!(name, "write" | "patch" | "shell") {
            return Err(format!(
                "slim.{name} is not available in Read-only or Plan mode"
            ));
        }
        if scope.native(name).is_none() {
            return Err(format!(
                "unknown slim tool: {}; available: {}",
                name.chars().take(64).collect::<String>(),
                native_names(scope.natives).join(", ")
            ));
        }
        if name == "glob" {
            return prepare_glob_target(scope, arguments);
        }
        if name == "git" {
            return prepare_git_target(scope, arguments);
        }
        if name == "subagent" {
            if self.subagent_depth > 0 {
                return Err("subagents cannot spawn sub-subagents (max depth = 1)".into());
            }
            let registry = self
                .subagents
                .clone()
                .ok_or_else(|| "subagents are not enabled in this session".to_owned())?;
            let parsed: super::native_subagent::SubagentInput =
                serde_json::from_value(arguments.clone())
                    .map_err(|err| format!("invalid slim.subagent arguments: {err}"))?;
            let background = parsed.background;
            let req = parsed.into_spawn_request_with_anchors(
                self.active_fork_context.as_deref(),
                &self.active_fork_channel_anchors,
            );
            return Ok(Target::Subagent {
                req: Box::new(req),
                background,
                registry,
                parent_cancel: self.cancellation.clone(),
                steer_inbox: self.steer_inbox.clone(),
                arguments,
            });
        }
        if name == "subagent_control" {
            if self.subagent_depth > 0 {
                return Err("subagents cannot control parent subagents (max depth = 1)".into());
            }
            let registry = self
                .subagents
                .clone()
                .ok_or_else(|| "subagents are not enabled in this session".to_owned())?;
            let input: super::native_subagent::SubagentControlInput =
                serde_json::from_value(arguments.clone())
                    .map_err(|err| format!("invalid slim.subagent_control arguments: {err}"))?;
            return Ok(Target::SubagentControl {
                input,
                registry,
                parent_cancel: self.cancellation.clone(),
                steer_inbox: self.steer_inbox.clone(),
                arguments,
            });
        }
        refuse_unsupported(name, &arguments)?;
        if name == "shell" {
            cap_shell_timeout(&mut arguments, scope.deadline)?;
        }
        let mut prepared = self.tools.prepare_invocation_value(
            scope.mode,
            scope.cwd,
            &scope.workspace,
            name,
            arguments.clone(),
        );
        if self.tuning_closed && prepared.error.is_none() && prepared.is_tuning_patch() {
            prepared.reject(TUNING_CLOSED_ERROR);
        }
        let concurrency = if read_only_override {
            Concurrency::Shared {
                coalesce_key: None,
                read_paths: Vec::new(),
            }
        } else if super::tool_schedule::tool_call_is_parallel_snapshot_read(&self.tools, name) {
            let read_paths = (name == "read" && prepared.target_paths.len() == 1)
                .then(|| super::tool_schedule::mutation_path_key(&prepared))
                .flatten()
                .into_iter()
                .collect();
            Concurrency::Shared {
                coalesce_key: (prepared.error.is_none() && prepared.reusable_evidence())
                    .then(|| prepared.canonical_fingerprint.clone()),
                read_paths,
            }
        } else if super::tool_schedule::is_file_mutation(&prepared)
            && !super::tool_schedule::is_serial_barrier(&prepared)
            && prepared.target_paths.len() <= 1
        {
            super::tool_schedule::mutation_path_key(&prepared)
                .map(|path_key| Concurrency::FileMutation { path_key })
                .unwrap_or(Concurrency::Exclusive)
        } else {
            Concurrency::Exclusive
        };
        Ok(Target::Native {
            concurrency,
            prepared: Box::new(prepared),
            arguments,
            read_only_override,
        })
    }

    /// Starts every queued call the scheduler admits: budget and run guards,
    /// causal governor preflight, task (or intra-cell/session cache hit), then the
    /// unsynced `started` fact and step event.
    pub(super) fn start_ready_calls(
        &mut self,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
        state: &mut CellState,
        seq: &mut u64,
        governor: &mut CausalGovernor,
    ) -> Result<(), ProviderError> {
        let display_root = scope.workspace.as_ref().map_or(scope.cwd, PathBuf::as_path);
        let daemon_epochs = crate::project_index::existing_for_workspace(display_root)
            .or_else(|| crate::project_index::for_workspace(display_root).ok())
            .map_or((0, 0), |d| (d.fs_epoch(), d.git_dirty_epoch()));
        let ws_base_rev = self.tools.workspace_revision();

        while let Some(mut job) = state.calls.next_startable() {
            if cell.cancellation.is_cancelled() {
                cell.reject(
                    job.ticket,
                    "cell cancelled or deadline exceeded; call not sent",
                );
                continue;
            }
            let is_read_only = job.is_read_only();
            let is_mutating = !is_read_only;
            let over_total = self.codemode.remaining_calls < job.slots;
            let over_mutating = is_mutating
                && self
                    .codemode
                    .remaining_mutating_calls
                    .is_some_and(|remaining| remaining < job.slots);
            if over_total || over_mutating {
                cell.reject(job.ticket, "tool budget exhausted; the call was not sent");
                continue;
            }
            self.codemode.remaining_calls -= job.slots;
            if is_mutating {
                if let Some(remaining) = self.codemode.remaining_mutating_calls.as_mut() {
                    *remaining -= job.slots;
                }
                self.codemode.cell_had_non_readonly_effect = true;
                state.read_cache.clear();
                match &job.target {
                    Target::Native { prepared, .. }
                        if matches!(prepared.name.as_str(), "write" | "patch")
                            && !prepared.target_paths.is_empty() =>
                    {
                        self.codemode
                            .session_read_cache
                            .invalidate_paths(&prepared.target_paths);
                    }
                    _ => self.codemode.session_read_cache.clear(),
                }
                state.mutation_epoch = state.mutation_epoch.saturating_add(1);
            }
            self.codemode.used_calls += job.slots;
            let id = format!(
                "{}:{}:{}",
                scope.parent.batch_id, scope.parent.call_id, self.codemode.used_calls
            );
            let effect_target = job_effect_target(&job.target, display_root);
            let mut mcp_fingerprint: Option<(String, String)> = None;
            let mut governor_pending = None;
            let mut cache_hit: Option<Finished> = None;
            let raw_args = std::mem::take(&mut job.raw_arguments);
            let (compact_arguments, serialized_arguments) = match &mut job.target {
                Target::Mcp {
                    server,
                    tool,
                    arguments,
                    read_only,
                    ..
                } => {
                    if !*read_only {
                        state.in_flight_mcp = state.in_flight_mcp.saturating_add(1);
                    }
                    let fingerprint = crate::tools::hash_fields(&[
                        b"slim-codemode-mcp-v1",
                        server.as_bytes(),
                        tool.as_bytes(),
                        raw_args.as_bytes(),
                    ]);
                    mcp_fingerprint = Some((format!("mcp.{server}.{tool}"), fingerprint));
                    let compact = compact_journal_arguments(&job.name, arguments, &raw_args);
                    (compact, raw_args)
                }
                Target::Glob {
                    fingerprint,
                    arguments,
                    ..
                } => {
                    let rev = (ws_base_rev, daemon_epochs.0);
                    if let Some(cached) = state
                        .read_cache
                        .get(fingerprint)
                        .and_then(Finished::clone_for_cache)
                        .or_else(|| self.codemode.session_read_cache.get(fingerprint, rev))
                    {
                        cache_hit = Some(cached);
                    }
                    let compact = compact_journal_arguments(&job.name, arguments, &raw_args);
                    (compact, raw_args)
                }
                Target::Git {
                    fingerprint,
                    arguments,
                    ..
                } => {
                    let rev = (ws_base_rev, daemon_epochs.1);
                    if let Some(cached) = state
                        .read_cache
                        .get(fingerprint)
                        .and_then(Finished::clone_for_cache)
                        .or_else(|| self.codemode.session_read_cache.get(fingerprint, rev))
                    {
                        cache_hit = Some(cached);
                    }
                    let compact = compact_journal_arguments(&job.name, arguments, &raw_args);
                    (compact, raw_args)
                }
                Target::ReadMany {
                    fingerprint,
                    arguments,
                    ..
                }
                | Target::OutlineMany {
                    fingerprint,
                    arguments,
                    ..
                }
                | Target::GrepIndex {
                    fingerprint,
                    arguments,
                    ..
                }
                | Target::Graph {
                    fingerprint,
                    arguments,
                    ..
                } => {
                    let rev = (ws_base_rev, daemon_epochs.0);
                    if let Some(cached) = state
                        .read_cache
                        .get(fingerprint)
                        .and_then(Finished::clone_for_cache)
                        .or_else(|| self.codemode.session_read_cache.get(fingerprint, rev))
                    {
                        cache_hit = Some(cached);
                    }
                    let compact = compact_journal_arguments(&job.name, arguments, &raw_args);
                    (compact, raw_args)
                }
                Target::Subagent { arguments, .. } | Target::SubagentControl { arguments, .. } => {
                    let compact = compact_journal_arguments(&job.name, arguments, &raw_args);
                    (compact, raw_args)
                }
                Target::Native {
                    prepared,
                    arguments,
                    concurrency,
                    ..
                } => {
                    if let Some(tx) = state.atomic_tx.as_mut() {
                        if matches!(prepared.name.as_str(), "write" | "patch") {
                            for path in &prepared.target_paths {
                                if !tx.backups.iter().any(|(existing, _)| existing == path) {
                                    tx.backups.push((path.clone(), std::fs::read(path).ok()));
                                }
                            }
                        }
                    }
                    self.run_guards.admit_codemode(&id, prepared);
                    if prepared.error.is_some() {
                        if let Concurrency::Shared { coalesce_key, .. } = concurrency {
                            *coalesce_key = None;
                        }
                        if prepared.name != "code_intel" {
                            let outcome = self
                                .tools
                                .execute_prepared_with_cancellation_and_progress(
                                    prepared,
                                    None,
                                    |_| {},
                                );
                            cache_hit = Some(Finished::Native(Arc::new(outcome)));
                        }
                    } else if is_read_only {
                        if let Concurrency::Shared {
                            coalesce_key: Some(key),
                            ..
                        } = concurrency
                        {
                            let rev = (ws_base_rev, daemon_epochs.0);
                            if let Some(cached) = state
                                .read_cache
                                .get(key)
                                .and_then(Finished::clone_for_cache)
                                .or_else(|| self.codemode.session_read_cache.get(key, rev))
                            {
                                cache_hit = Some(cached);
                            }
                        }
                    }
                    let (pending, observations) =
                        governor.observe_before_identified(prepared, scope.parent.batch_id, &id);
                    self.emit_governor_observations(observations, seq)?;
                    governor_pending = Some(pending);
                    let compact = compact_journal_arguments(&job.name, arguments, &raw_args);
                    (compact, raw_args)
                }
            };
            let started = json!({
                "parent_call_id": scope.parent.call_id,
                "name": job.name,
                "status": "started",
                "arguments": compact_arguments,
            });
            let step = crate::EventKind::ToolStepStarted {
                batch_id: scope.parent.batch_id.into(),
                call_id: scope.parent.call_id.into(),
                step_id: id.clone(),
                name: job.name.clone(),
                arguments: self.redact_sensitive(&serialized_arguments),
            };
            if let Some(finished) = cache_hit {
                let prepared = match &job.target {
                    Target::Native { prepared, .. } => Some(Arc::from(prepared.clone())),
                    _ => None,
                };
                state.calls.push_cached(
                    id.clone(),
                    job,
                    is_read_only,
                    effect_target,
                    prepared,
                    state.mutation_epoch,
                    daemon_epochs,
                    governor_pending,
                    finished,
                );
            } else {
                state.calls.spawn(
                    id.clone(),
                    job,
                    is_read_only,
                    effect_target,
                    state.mutation_epoch,
                    daemon_epochs,
                    governor_pending,
                );
            }
            self.persist_codemode_fact_unsynced(CALL_NAMESPACE, &id, started)?;
            push_runtime_event(&mut self.app, seq, step)?;
            if let Some((tool_name, fingerprint)) = mcp_fingerprint {
                let observations = governor.observe_external_call(
                    scope.parent.batch_id,
                    &id,
                    &tool_name,
                    fingerprint,
                );
                self.emit_governor_observations(observations, seq)?;
            }
        }
        Ok(())
    }

    /// Settles a finished call: redaction, the bridge limit, run guards,
    /// causal governor, language-server sync, the program's promise, then
    /// the artifact, the terminal fact and step event.
    pub(super) async fn finish_call(
        &mut self,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
        state: &mut CellState,
        seq: &mut u64,
        governor: &mut CausalGovernor,
        done: Done,
    ) -> Result<(), ProviderError> {
        let Done {
            mut running,
            finished,
        } = done;
        let mut fact = json!({
            "parent_call_id": scope.parent.call_id, "name": running.name,
            "duration_ms": elapsed_millis(running.started),
        });
        let mut artifact = None;
        let mut diff = None;
        let (status, response, step_preview) = match finished {
            Ok(Finished::Mcp(outcome)) => {
                let (status, response, result_bytes, step_preview) =
                    self.mcp_response(outcome, &running, cell, state);
                match &response {
                    Ok(_) => {
                        fact["result"] = json!({
                            "result_bytes": result_bytes,
                            "is_error": status == "failed",
                        });
                        if status == "failed" {
                            fact["error"] = json!(prefix(&step_preview, MAX_JOURNALED_ERROR_BYTES));
                            fact["error_bytes"] = json!(result_bytes);
                        }
                    }
                    Err(error) => {
                        fact["error"] = json!(prefix(error, MAX_JOURNALED_ERROR_BYTES));
                        fact["error_bytes"] = json!(error.len());
                    }
                }
                (status, response, step_preview)
            }
            Ok(Finished::Glob {
                fingerprint,
                result,
            }) => {
                let (status, response, output_bytes, step_preview) = self.glob_response(
                    scope,
                    &running,
                    state,
                    &fingerprint,
                    result,
                    governor,
                    seq,
                )?;
                match &response {
                    Ok(_) => {
                        fact["result"] = json!({ "output_bytes": output_bytes });
                    }
                    Err(error) => {
                        fact["error"] = json!(prefix(error, MAX_JOURNALED_ERROR_BYTES));
                        fact["error_bytes"] = json!(error.len());
                    }
                }
                (status, response, step_preview)
            }
            Ok(Finished::Git {
                fingerprint,
                result,
            }) => {
                let (status, response, output_bytes, step_preview) =
                    self.git_response(&running, state, &fingerprint, result);
                match &response {
                    Ok(_) => {
                        fact["result"] = json!({ "output_bytes": output_bytes });
                    }
                    Err(error) => {
                        fact["error"] = json!(prefix(error, MAX_JOURNALED_ERROR_BYTES));
                        fact["error_bytes"] = json!(error.len());
                    }
                }
                (status, response, step_preview)
            }
            Ok(Finished::JsonValue {
                fingerprint,
                value,
                deps,
                cacheable,
            }) => {
                let (status, response, output_bytes, step_preview) = match value.as_ref() {
                    Ok(v) => {
                        if !running.from_cache && cacheable {
                            let cached = Finished::JsonValue {
                                fingerprint: fingerprint.clone(),
                                value: Arc::clone(&value),
                                deps,
                                cacheable: true,
                            };
                            self.cache_read_result(state, &running, &fingerprint, &cached);
                        }
                        let mut redacted = v.clone();
                        redact_value(&mut redacted, &self.sensitive_values.0);
                        match probe_json_size_and_preview(&redacted) {
                            None => {
                                let msg = format!(
                                    "{} output exceeds the 1 MiB bridge limit; narrow the request",
                                    running.name
                                );
                                let preview = prefix(&msg, MAX_STEP_OUTPUT_BYTES).to_owned();
                                ("failed", Err(msg.clone()), msg.len(), preview)
                            }
                            Some((raw_len, _)) => {
                                let preview = format!("<{} bytes JSON>", raw_len);
                                ("completed", Ok(Payload::JsonValue(redacted)), raw_len, preview)
                            }
                        }
                    }
                    Err(error) => {
                        let error = self.redact_sensitive(error);
                        let bytes = error.len();
                        let preview = prefix(&error, MAX_STEP_OUTPUT_BYTES).to_owned();
                        ("failed", Err(error), bytes, preview)
                    }
                };
                match &response {
                    Ok(_) => {
                        fact["result"] = json!({ "output_bytes": output_bytes });
                    }
                    Err(error) => {
                        fact["error"] = json!(prefix(error, MAX_JOURNALED_ERROR_BYTES));
                        fact["error_bytes"] = json!(error.len());
                    }
                }
                (status, response, step_preview)
            }
            Ok(Finished::Subagent(result)) => {
                let (status, response, output_bytes, step_preview) =
                    self.subagent_response(&running, state, result);
                match &response {
                    Ok(_) => {
                        fact["result"] = json!({ "output_bytes": output_bytes });
                    }
                    Err(error) => {
                        fact["error"] = json!(prefix(error, MAX_JOURNALED_ERROR_BYTES));
                        fact["error_bytes"] = json!(error.len());
                    }
                }
                (status, response, step_preview)
            }
            Ok(Finished::Native(outcome)) => {
                let (status, response, output_bytes, step_preview, result, edit_diff) = self
                    .native_response(scope, &mut running, state, outcome, governor, seq)
                    .await?;
                diff = edit_diff;
                match &response {
                    Ok(_) => {
                        fact["result"] = json!({"output_bytes": output_bytes});
                    }
                    Err(error) => {
                        fact["error"] = json!(prefix(error, MAX_JOURNALED_ERROR_BYTES));
                        fact["error_bytes"] = json!(error.len());
                    }
                }
                artifact = Some(result);
                (status, response, step_preview)
            }
            Err(error) => {
                let error = format!("{} task failed: {error}", running.name);
                fact["error"] = json!(prefix(&error, MAX_JOURNALED_ERROR_BYTES));
                fact["error_bytes"] = json!(error.len());
                let preview = prefix(&error, MAX_STEP_OUTPUT_BYTES).to_owned();
                ("failed", Err(error), preview)
            }
        };
        fact["status"] = json!(status);
        if state.returned && (status != "completed" || response.is_err()) {
            let error = response.as_ref().err().map_or(status, String::as_str);
            state.unawaited_failures.push(format!(
                "{} ({})",
                running.name,
                error.chars().take(MAX_NOTE_ERROR_CHARS).collect::<String>()
            ));
        }
        let step = crate::EventKind::ToolStepFinished {
            batch_id: scope.parent.batch_id.into(),
            call_id: scope.parent.call_id.into(),
            step_id: running.id.clone(),
            name: running.name.clone(),
            success: status == "completed" && response.is_ok(),
            duration_ms: elapsed_millis(running.started),
            output: step_preview,
            diff,
        };
        cell.settle(running.ticket, response);
        if let Some(result) = &artifact {
            self.record_existing_artifact(result, seq)?;
        }
        self.persist_codemode_fact_unsynced(CALL_NAMESPACE, &running.id, fact)?;
        push_runtime_event(&mut self.app, seq, step)
    }

    fn cache_read_result(
        &mut self,
        state: &mut CellState,
        running: &Running,
        key: &str,
        value: &Finished,
    ) {
        if !running.is_read_only
            || state.in_flight_mcp != 0
            || running.mutation_epoch != state.mutation_epoch
        {
            return;
        }
        if state.read_cache.len() < MAX_CELL_READ_CACHE && !state.read_cache.contains_key(key) {
            if let Some(cloned) = value.clone_for_cache() {
                state.read_cache.insert(key.to_owned(), cloned);
            }
        }
        if let Some(cloned) = value.clone_for_cache() {
            let deps = match value {
                Finished::Native(outcome) => outcome
                    .receipt
                    .dependencies
                    .iter()
                    .map(|dep| dep.path.clone())
                    .collect(),
                Finished::JsonValue { deps, .. } => deps.clone(),
                _ => Vec::new(),
            };
            let daemon_epoch = if matches!(value, Finished::Git { .. }) {
                running.daemon_epoch.1
            } else {
                running.daemon_epoch.0
            };
            self.codemode.session_read_cache.insert(
                key.to_owned(),
                (self.tools.workspace_revision(), daemon_epoch),
                deps,
                cloned,
            );
        }
    }

    fn mcp_response(
        &mut self,
        outcome: McpRequestOutcome<Value>,
        running: &Running,
        cell: &engine::Cell,
        state: &mut CellState,
    ) -> (&'static str, Result<Payload, String>, usize, String) {
        if !running.is_read_only {
            // An MCP call may have mutated external/workspace state while running
            // alongside reads; invalidate any cached reads and advance the epoch.
            state.in_flight_mcp = state.in_flight_mcp.saturating_sub(1);
            state.read_cache.clear();
            self.codemode.session_read_cache.clear();
            state.mutation_epoch = state.mutation_epoch.saturating_add(1);
        }
        self.refresh_mcp_secrets();
        match outcome {
            McpRequestOutcome::Completed(Ok(mut value)) => {
                redact_value(&mut value, &self.sensitive_values.0);
                let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
                let serialized = value.to_string();
                let bytes = serialized.len();
                if bytes > MAX_JSON_BYTES {
                    let msg = "MCP call completed, but its result exceeds the 1 MiB bridge limit; do not repeat it blindly".to_owned();
                    let preview = prefix(&msg, MAX_STEP_OUTPUT_BYTES).to_owned();
                    ("completed", Err(msg), bytes, preview)
                } else {
                    let preview = prefix(&serialized, MAX_STEP_OUTPUT_BYTES).to_owned();
                    let status = if is_error { "failed" } else { "completed" };
                    if !is_error {
                        record_applied_effect(state, running);
                    }
                    (status, Ok(Payload::McpJson(serialized)), bytes, preview)
                }
            }
            McpRequestOutcome::Completed(Err(error)) => {
                let msg = error.to_string();
                let preview = prefix(&msg, MAX_STEP_OUTPUT_BYTES).to_owned();
                ("failed", Err(msg), 0, preview)
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                let msg = format!("MCP call not sent: {interruption:?}; cleanup: {cleanup:?}");
                let preview = prefix(&msg, MAX_STEP_OUTPUT_BYTES).to_owned();
                ("not_sent", Err(msg), 0, preview)
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                let error = format!(
                    "MCP outcome uncertain: {interruption:?}; cleanup: {cleanup:?}; do not replay"
                );
                state.uncertain.push(error.clone());
                cell.cancellation.cancel();
                let preview = prefix(&error, MAX_STEP_OUTPUT_BYTES).to_owned();
                ("uncertain", Err(error), 0, preview)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn glob_response(
        &mut self,
        scope: &CellScope<'_>,
        running: &Running,
        state: &mut CellState,
        fingerprint: &str,
        result: Arc<Result<crate::workspace_files::GlobListing, String>>,
        governor: &mut CausalGovernor,
        seq: &mut u64,
    ) -> Result<(&'static str, Result<Payload, String>, usize, String), ProviderError> {
        let (status, response, output_bytes, step_preview, tool_result) = match result.as_ref() {
            Ok(listing) => {
                if !running.from_cache {
                    let cached = Finished::Glob {
                        fingerprint: fingerprint.to_owned(),
                        result: Arc::clone(&result),
                    };
                    self.cache_read_result(state, running, fingerprint, &cached);
                }
                let mut value = json!({
                    "files": listing.files,
                    "truncated": listing.truncated,
                    "timed_out": listing.timed_out,
                });
                redact_value(&mut value, &self.sensitive_values.0);
                if let Some((output_bytes, preview)) = probe_json_size_and_preview(&value) {
                    let raw = value.to_string();
                    let tool_result = ToolResult::ok("glob", raw);
                    (
                        "completed",
                        Ok(Payload::JsonValue(value)),
                        output_bytes,
                        preview,
                        tool_result,
                    )
                } else {
                    let msg = "slim.glob output exceeds the 1 MiB bridge limit; narrow the pattern or path".to_owned();
                    let preview = prefix(&msg, MAX_STEP_OUTPUT_BYTES).to_owned();
                    (
                        "failed",
                        Err(msg.clone()),
                        msg.len(),
                        preview,
                        ToolResult::fail("glob", msg),
                    )
                }
            }
            Err(error) => {
                let error = self.redact_sensitive(error);
                let output_bytes = error.len();
                let preview = prefix(&error, MAX_STEP_OUTPUT_BYTES).to_owned();
                let tool_result = ToolResult::fail("glob", error.clone());
                ("failed", Err(error), output_bytes, preview, tool_result)
            }
        };
        let observations =
            governor.observe_codemode_glob(scope.parent.batch_id, &running.id, fingerprint, &tool_result);
        self.emit_governor_observations(observations, seq)?;
        Ok((status, response, output_bytes, step_preview))
    }

    fn git_response(
        &mut self,
        running: &Running,
        state: &mut CellState,
        fingerprint: &str,
        result: Arc<Result<Value, String>>,
    ) -> (&'static str, Result<Payload, String>, usize, String) {
        match result.as_ref() {
            Ok(value) => {
                if !running.from_cache {
                    let cached = Finished::Git {
                        fingerprint: fingerprint.to_owned(),
                        result: Arc::clone(&result),
                    };
                    self.cache_read_result(state, running, fingerprint, &cached);
                }
                let mut redacted = value.clone();
                redact_value(&mut redacted, &self.sensitive_values.0);
                if let Some((output_bytes, preview)) = probe_json_size_and_preview(&redacted) {
                    ("completed", Ok(Payload::JsonValue(redacted)), output_bytes, preview)
                } else {
                    let msg = "slim.git output exceeds the 1 MiB bridge limit; narrow the path or limit".to_owned();
                    let preview = prefix(&msg, MAX_STEP_OUTPUT_BYTES).to_owned();
                    ("failed", Err(msg.clone()), msg.len(), preview)
                }
            }
            Err(error) => {
                let error = self.redact_sensitive(error);
                let output_bytes = error.len();
                let preview = prefix(&error, MAX_STEP_OUTPUT_BYTES).to_owned();
                ("failed", Err(error), output_bytes, preview)
            }
        }
    }

    fn subagent_response(
        &mut self,
        running: &Running,
        state: &mut CellState,
        result: Result<String, String>,
    ) -> (&'static str, Result<Payload, String>, usize, String) {
        match result {
            Ok(text) => {
                let text = self.redact_sensitive(&text);
                let bytes = text.len();
                if bytes > MAX_JSON_BYTES {
                    let msg = format!(
                        "{} output exceeds the 1 MiB bridge limit; lower max_result_chars",
                        running.name
                    );
                    let preview = prefix(&msg, MAX_STEP_OUTPUT_BYTES).to_owned();
                    ("failed", Err(msg.clone()), msg.len(), preview)
                } else {
                    record_applied_effect(state, running);
                    let preview = prefix(&text, MAX_STEP_OUTPUT_BYTES).to_owned();
                    ("completed", Ok(Payload::Text(text)), bytes, preview)
                }
            }
            Err(error) => {
                let error = self.redact_sensitive(&error);
                let bytes = error.len();
                let preview = prefix(&error, MAX_STEP_OUTPUT_BYTES).to_owned();
                ("failed", Err(error), bytes, preview)
            }
        }
    }

    /// The program's view of a native outcome, the result whose artifact
    /// the caller records and the redacted edit diff for display.
    async fn native_response(
        &mut self,
        scope: &CellScope<'_>,
        running: &mut Running,
        state: &mut CellState,
        outcome: Arc<ToolExecutionOutcome>,
        governor: &mut CausalGovernor,
        seq: &mut u64,
    ) -> Result<
        (
            &'static str,
            Result<Payload, String>,
            usize,
            String,
            ToolResult,
            Option<crate::ToolEditDiff>,
        ),
        ProviderError,
    > {
        let mut outcome = Arc::unwrap_or_clone(outcome);
        let mut output = self.redact_sensitive(&outcome.result.output);
        outcome.result.output = output.clone();
        let allow_non_zero_exit = running.allow_non_zero
            && !outcome.result.success
            && outcome
                .receipt
                .process
                .as_ref()
                .is_some_and(|process| {
                    process.exit_code.is_some() && !process.timed_out && !process.cancelled
                })
            && !outcome.receipt.effects_uncertain;
        let succeeded = outcome.result.success || allow_non_zero_exit;
        // The program, not the model, receives the output: nothing counts
        // as already read, and a hold is not remembered as a provider call.
        // Intentional `allow_non_zero: true` shell exits count as accepted so
        // `RunGuards` does not hold a deliberate follow-up call.
        self.run_guards.was_held(&running.id);
        if let Some(note) = self.run_guards.settle(&running.id, succeeded, false) {
            state.guard_notes.push(note);
        }
        let artifact_ref = Self::artifact_reference(&outcome.result, scope.cwd);
        if let Some(reference) = &artifact_ref {
            output.push('\n');
            output.push_str(reference);
        }
        let display_root = scope.workspace.as_ref().map_or(scope.cwd, PathBuf::as_path);
        if let Some(prepared) = &running.prepared {
            let native = ToolInvocation {
                batch_id: scope.parent.batch_id,
                call_id: &running.id,
                name: &prepared.name,
                arguments: "",
            };
            self.notify_code_intel_after_mutation(scope.cwd, native, prepared, &outcome)
                .await;
            if matches!(prepared.name.as_str(), "write" | "patch")
                && (outcome.result.success
                    || outcome
                        .receipt
                        .mutations
                        .iter()
                        .any(|mutation| mutation.changed()))
            {
                self.codemode.cell_mutated_workspace = true;
                self.codemode.cell_validation_green = false;
            }
            if outcome.result.success {
                let shell_cmd = match &prepared.arguments {
                    PreparedToolArguments::Shell { command, .. } => Some(command.as_str()),
                    _ => None,
                };
                self.scratchpad.record_codemode_step(
                    &prepared.name,
                    &prepared.target_paths,
                    display_root,
                    shell_cmd,
                );
            }
        }
        if let Some(pending) = running.governor_pending.take() {
            let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
            if observations.iter().any(|obs| {
                matches!(
                    obs,
                    GovernorObservation::Progress {
                        kind: crate::CausalProgressKind::ValidationGreen,
                        ..
                    }
                )
            }) {
                self.codemode.cell_validation_green = true;
            }
            self.emit_governor_observations(observations, seq)?;
        }
        if outcome.result.success && !running.from_cache {
            if let Some(cache_key) = running.cache_key.clone() {
                let cached = Finished::Native(Arc::new(outcome.clone()));
                self.cache_read_result(state, running, &cache_key, &cached);
            }
        }
        let status = if succeeded {
            record_applied_effect(state, running);
            "completed"
        } else {
            "failed"
        };
        let path_hint = running
            .prepared
            .as_ref()
            .and_then(|p| p.target_paths.first());
        let output_bytes = output.len();
        let (response, reported_bytes, step_preview) = if running.structured && outcome.result.success {
            if let Some(val) =
                self.structured_native_value(&outcome.receipt.presentation, path_hint, display_root)
            {
                if probe_json_size_and_preview(&val).is_none() {
                    let message = format!(
                        "{} finished ({status}), but its structured output exceeds the 1 MiB bridge limit; narrow the call",
                        running.name
                    );
                    let preview = prefix(&message, MAX_STEP_OUTPUT_BYTES).to_owned();
                    let bytes = message.len();
                    (Err(message), bytes, preview)
                } else {
                    let preview = prefix(&output, MAX_STEP_OUTPUT_BYTES).to_owned();
                    (Ok(Payload::JsonValue(val)), output_bytes, preview)
                }
            } else if output_bytes > MAX_JSON_BYTES {
                let mut message = format!(
                    "{} finished ({status}), but its output exceeds the 1 MiB bridge limit; narrow the call",
                    running.name
                );
                if let Some(reference) = artifact_ref {
                    message.push_str(" or inspect via artifact_read:\n");
                    message.push_str(&reference);
                }
                let preview = prefix(&message, MAX_STEP_OUTPUT_BYTES).to_owned();
                let bytes = message.len();
                (Err(message), bytes, preview)
            } else {
                let preview = prefix(&output, MAX_STEP_OUTPUT_BYTES).to_owned();
                (Ok(Payload::Text(output)), output_bytes, preview)
            }
        } else if output_bytes > MAX_JSON_BYTES {
            let mut message = format!(
                "{} finished ({status}), but its output exceeds the 1 MiB bridge limit; narrow the call",
                running.name
            );
            if let Some(reference) = artifact_ref {
                message.push_str(" or inspect via artifact_read:\n");
                message.push_str(&reference);
            }
            let preview = prefix(&message, MAX_STEP_OUTPUT_BYTES).to_owned();
            let bytes = message.len();
            (Err(message), bytes, preview)
        } else if succeeded {
            let preview = prefix(&output, MAX_STEP_OUTPUT_BYTES).to_owned();
            (Ok(Payload::Text(output)), output_bytes, preview)
        } else {
            let preview = prefix(&output, MAX_STEP_OUTPUT_BYTES).to_owned();
            (Err(output), output_bytes, preview)
        };
        let diff = outcome
            .result
            .success
            .then(|| self.redacted_edit_diff(outcome.receipt.edit_diff.as_ref()))
            .flatten();
        Ok((status, response, reported_bytes, step_preview, outcome.result, diff))
    }

    fn structured_native_value(
        &self,
        presentation: &Option<crate::tools::ToolPresentationSource>,
        path_hint: Option<&PathBuf>,
        cwd: &Path,
    ) -> Option<Value> {
        let mut value = match presentation.as_ref()? {
            crate::tools::ToolPresentationSource::Read {
                full,
                first,
                records,
                next_offset,
                ..
            } => {
                let path = path_hint
                    .map(|p| crate::tools::display_path(cwd, p).to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                json!({
                    "path": path,
                    "content": full,
                    "first_line": first,
                    "total_lines": records.len(),
                    "next_offset": next_offset,
                    "complete": next_offset.is_none(),
                })
            }
            crate::tools::ToolPresentationSource::Search {
                page, display_root, ..
            } => {
                let hits: Vec<Value> = page
                    .hits
                    .iter()
                    .map(|matched| {
                        let path = crate::tools::display_path(display_root, &matched.hit.path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        let context: Vec<Value> = matched
                            .context
                            .iter()
                            .map(|arc| json!({ "line": arc.0, "text": arc.1 }))
                            .collect();
                        json!({
                            "path": path,
                            "line": matched.hit.line,
                            "text": matched.hit.text,
                            "context": context,
                        })
                    })
                    .collect();
                json!({
                    "hits": hits,
                    "total": page.total,
                    "next_cursor": page.next_cursor,
                    "capped": page.capped,
                })
            }
            crate::tools::ToolPresentationSource::List {
                page, display_root, ..
            } => {
                let entries: Vec<String> = (0..page.entries.len())
                    .map(|index| page.label(index, display_root))
                    .collect();
                json!({
                    "entries": entries,
                    "total": page.total,
                    "next_cursor": page.next_cursor,
                })
            }
            _ => return None,
        };
        redact_value(&mut value, &self.sensitive_values.0);
        Some(value)
    }
}

fn record_applied_effect(state: &mut CellState, running: &Running) {
    if !running.is_read_only {
        state.applied_effects.push(json!({
            "call": running.name,
            "target": running.effect_target,
            "status": "completed",
        }));
    }
}

fn job_effect_target(target: &Target, cwd: &Path) -> Option<String> {
    match target {
        Target::Native { prepared, .. } => prepared
            .target_paths
            .first()
            .map(|p| crate::tools::display_path(cwd, p).to_string_lossy().replace('\\', "/"))
            .or_else(|| {
                if prepared.name == "shell" {
                    if let PreparedToolArguments::Shell { command, .. } = &prepared.arguments {
                        return Some(prefix(command.trim(), 120).to_owned());
                    }
                }
                None
            }),
        Target::Mcp { server, tool, .. } => Some(format!("mcp.{server}.{tool}")),
        Target::Subagent { req, .. } => Some(req.role.clone()),
        Target::SubagentControl { input, .. } => input.id.clone().or_else(|| Some(input.action.clone())),
        Target::Glob { path, .. } | Target::Git { path, .. } => path.clone(),
        Target::ReadMany { specs, .. } => Some(format!("{} file(s)", specs.len())),
        Target::OutlineMany { input, .. } => match input {
            dispatch::OutlineManyInput::Paths(paths) => Some(format!("{} file(s)", paths.len())),
            dispatch::OutlineManyInput::Glob { pattern, .. } => Some(pattern.clone()),
        },
        Target::GrepIndex { queries, .. } => Some(format!("{} query(ies)", queries.len())),
        Target::Graph { op, .. } => Some(op.clone()),
    }
}

fn take_optional_bool(arguments: &mut Value, key: &str) -> Result<bool, String> {
    let Some(object) = arguments.as_object_mut() else {
        return Ok(false);
    };
    match object.remove(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(flag),
        Some(_) => Err(format!("`{key}` must be a boolean")),
    }
}

fn prepare_git_target(scope: &CellScope<'_>, arguments: Value) -> Result<Target, String> {
    let Some(object) = arguments.as_object() else {
        return Err("tool arguments must be an object".into());
    };
    for key in object.keys() {
        if !matches!(key.as_str(), "op" | "path" | "staged" | "limit" | "rev") {
            return Err(format!(
                "git has unknown argument `{key}`; valid fields: op, path, staged, limit, rev"
            ));
        }
    }
    let Some(op) = object.get("op").and_then(Value::as_str) else {
        return Err("git `op` must be one of status, diff, log, branch, show".into());
    };
    if !matches!(op, "status" | "diff" | "log" | "branch" | "show") {
        return Err(format!(
            "unsupported git op `{op}`; expected one of status, diff, log, branch, show"
        ));
    }
    let path = match object.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if text.trim().is_empty()
                || text.starts_with('-')
                || text.starts_with('/')
                || text.split(['/', '\\']).any(|seg| seg == "..")
            {
                return Err("git `path` must be a non-empty relative path inside workspace".into());
            }
            Some(text.clone())
        }
        Some(_) => return Err("git `path` must be a string".into()),
    };
    let staged = match object.get("staged") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("git `staged` must be a boolean".into()),
    };
    let limit = match object.get("limit") {
        None | Some(Value::Null) => 20usize,
        Some(value) => {
            let Some(n) = value.as_u64().and_then(|v| usize::try_from(v).ok()) else {
                return Err("git `limit` must be an integer between 1 and 50".into());
            };
            if !(1..=50).contains(&n) {
                return Err("git `limit` must be an integer between 1 and 50".into());
            }
            n
        }
    };
    let rev = match object.get("rev") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if text.trim().is_empty()
                || text.len() > 256
                || text.starts_with('-')
                || text.chars().any(char::is_whitespace)
            {
                return Err("git `rev` must be a valid revision identifier".into());
            }
            Some(text.clone())
        }
        Some(_) => return Err("git `rev` must be a string".into()),
    };
    let workspace = scope.workspace.clone()?;
    let fingerprint = crate::tools::hash_fields(&[
        b"slim-git-v1",
        op.as_bytes(),
        path.as_deref().unwrap_or("").as_bytes(),
        &[u8::from(staged)],
        &limit.to_le_bytes(),
        rev.as_deref().unwrap_or("").as_bytes(),
    ]);
    Ok(Target::Git {
        op: op.to_owned(),
        path,
        staged,
        limit,
        rev,
        workspace,
        fingerprint,
        arguments,
    })
}

fn prepare_glob_target(scope: &CellScope<'_>, arguments: Value) -> Result<Target, String> {
    let Some(object) = arguments.as_object() else {
        return Err("tool arguments must be an object".into());
    };
    for key in object.keys() {
        if !matches!(key.as_str(), "pattern" | "path" | "max_entries") {
            return Err(format!(
                "glob has unknown argument `{key}`; valid fields: pattern, path, max_entries"
            ));
        }
    }
    let pattern = match object.get("pattern") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err("glob `pattern` must be a string".into()),
    };
    let path = match object.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err("glob `path` must be a string".into()),
    };
    let max_entries = match object.get("max_entries") {
        None | Some(Value::Null) => crate::tools::DEFAULT_MAX_ENTRIES,
        Some(value) => {
            let Some(n) = value.as_u64() else {
                return Err("glob `max_entries` must be an integer between 1 and 1000".into());
            };
            let Ok(n) = usize::try_from(n) else {
                return Err("glob `max_entries` must be an integer between 1 and 1000".into());
            };
            if !(1..=1000).contains(&n) {
                return Err("glob `max_entries` must be an integer between 1 and 1000".into());
            }
            n
        }
    };
    let workspace = scope.workspace.clone()?;
    if matches!(path.as_deref(), Some("")) {
        return Err("path must not be empty".into());
    }
    if let Some(pat) = &pattern {
        if pat.trim().is_empty() || pat.len() > 512 {
            return Err("glob pattern must contain 1-512 bytes".into());
        }
        if pat.starts_with('!') {
            return Err("glob pattern must be an include pattern, not negated with `!`".into());
        }
        if pat.starts_with('/') || pat.split(['/', '\\']).any(|segment| segment == "..") {
            return Err("glob pattern must stay relative inside the workspace".into());
        }
    }
    let normalized_path = path
        .as_deref()
        .map(|p| p.trim_start_matches("./").trim_end_matches('/'))
        .filter(|p| !p.is_empty())
        .unwrap_or(".");
    let fingerprint = crate::tools::hash_fields(&[
        b"slim-glob-v1",
        normalized_path.as_bytes(),
        pattern.as_deref().unwrap_or("*").as_bytes(),
        &max_entries.to_le_bytes(),
    ]);
    Ok(Target::Glob {
        path,
        pattern,
        max_entries,
        workspace,
        fingerprint,
        arguments,
    })
}

fn compact_journal_arguments(name: &str, arguments: &Value, serialized: &str) -> Value {
    if serialized.len() <= MAX_JOURNALED_ARGUMENT_BYTES {
        return arguments.clone();
    }
    let Some(map) = arguments.as_object() else {
        return json!({ "bytes": serialized.len() });
    };
    let mut compact = serde_json::Map::new();
    for key in ["path", "command", "query", "pattern", "glob", "action"] {
        if let Some(val) = map.get(key) {
            if let Some(text) = val.as_str() {
                compact.insert(
                    key.to_owned(),
                    Value::String(prefix(text, 256).to_owned()),
                );
            } else {
                compact.insert(key.to_owned(), val.clone());
            }
        }
    }
    if name == "slim.patch" {
        if let Some((added, removed)) = crate::tools::edit_line_stats("patch", &serialized) {
            compact.insert("added_lines".into(), json!(added));
            compact.insert("removed_lines".into(), json!(removed));
        }
    }
    for key in ["content", "expected", "replacement", "edits"] {
        if let Some(val) = map.get(key) {
            let bytes = val.as_str().map_or_else(|| val.to_string().len(), str::len);
            compact.insert(key.to_owned(), json!(format!("<{bytes} bytes>")));
        }
    }
    compact.insert("elided".into(), Value::Bool(true));
    Value::Object(compact)
}

/// Refuses the arguments a program cannot use (see `build_native_definitions`).
fn refuse_unsupported(name: &str, arguments: &Value) -> Result<(), String> {
    let (scope, unsupported): (&Value, &[&str]) = match name {
        "shell" => (arguments, &UNSUPPORTED_SHELL_ARGUMENTS),
        "write" | "patch" => (&arguments["then_run"], &UNSUPPORTED_THEN_RUN_ARGUMENTS),
        _ => return Ok(()),
    };
    match unsupported.iter().find(|argument| scope.get(**argument).is_some()) {
        Some(argument) if name == "shell" => Err(format!(
            "shell `{argument}` is not available in codemode: the command runs in the foreground until it exits or times out"
        )),
        Some(argument) => Err(format!(
            "{name} `then_run.{argument}` is not available in codemode: return the answer from the program"
        )),
        None => Ok(()),
    }
}

/// Caps a shell call's `timeout_ms` to the time the cell has left. A
/// malformed value is left for the tool's own validation.
pub(super) fn cap_shell_timeout(arguments: &mut Value, deadline: Instant) -> Result<(), String> {
    let left = u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    if left == 0 {
        return Err("cell deadline exceeded; shell not started".into());
    }
    let requested = match arguments.get("timeout_ms") {
        None => crate::tools::DEFAULT_SHELL_TIMEOUT_MS,
        Some(value) => match value.as_u64() {
            Some(requested) => requested,
            None => return Ok(()),
        },
    };
    if requested > left {
        arguments["timeout_ms"] = json!(left);
    }
    Ok(())
}

/// At most `limit` bytes of `text`, on a character boundary.
pub(super) fn prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

struct BoundedJsonProbe {
    total: usize,
    limit: usize,
    preview: Vec<u8>,
    preview_limit: usize,
}

impl std::io::Write for BoundedJsonProbe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let next = self.total.saturating_add(buf.len());
        if next > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "json bridge limit exceeded",
            ));
        }
        self.total = next;
        let room = self.preview_limit.saturating_sub(self.preview.len());
        if room > 0 {
            self.preview.extend_from_slice(&buf[..buf.len().min(room)]);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn probe_json_size_and_preview(value: &Value) -> Option<(usize, String)> {
    let mut probe = BoundedJsonProbe {
        total: 0,
        limit: MAX_JSON_BYTES,
        preview: Vec::with_capacity(256),
        preview_limit: MAX_STEP_OUTPUT_BYTES,
    };
    serde_json::to_writer(&mut probe, value).ok()?;
    let mut preview_end = probe.preview.len();
    while preview_end > 0 && std::str::from_utf8(&probe.preview[..preview_end]).is_err() {
        preview_end -= 1;
    }
    let preview = String::from_utf8_lossy(&probe.preview[..preview_end]).into_owned();
    Some((probe.total, preview))
}

fn canonical_json_hash(tag: &[u8], extra: &[u8], value: &Value) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    crate::tools::hash_fields(&[tag, extra, &encoded])
}

fn prepare_read_many_target(
    scope: &CellScope<'_>,
    arguments: Value,
    top_structured: bool,
) -> Result<Target, String> {
    let workspace = scope.workspace.clone()?;
    let Some(obj) = arguments.as_object() else {
        return Err("readMany arguments must be an object".into());
    };
    let Some(paths_arr) = obj.get("paths").and_then(Value::as_array) else {
        return Err("readMany requires a `paths` array".into());
    };
    if paths_arr.len() > 64 {
        return Err("readMany accepts at most 64 files per call".into());
    }
    let opts = obj.get("opts").and_then(Value::as_object);
    let default_offset = opts
        .and_then(|o| o.get("offset"))
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(1);
    let default_max_lines = opts
        .and_then(|o| o.get("max_lines"))
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok());
    let default_outline = opts
        .and_then(|o| o.get("outline"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let default_raw = opts
        .and_then(|o| o.get("raw"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let structured = top_structured
        || opts
            .and_then(|o| o.get("structured"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

    let mut specs = Vec::with_capacity(paths_arr.len());
    for entry in paths_arr {
        if let Some(p) = entry.as_str() {
            specs.push(dispatch::ReadManySpec {
                raw_path: p.to_owned(),
                offset: default_offset,
                max_lines: default_max_lines,
                outline: default_outline,
                symbols: Vec::new(),
                ranges: Vec::new(),
                raw: default_raw,
            });
        } else if let Some(item) = entry.as_object() {
            let Some(raw_path) = item.get("path").and_then(Value::as_str) else {
                return Err("each readMany object item requires `path`".into());
            };
            let offset = item
                .get("offset")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(default_offset);
            let max_lines = item
                .get("max_lines")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .or(default_max_lines);
            let outline = item
                .get("outline")
                .and_then(Value::as_bool)
                .unwrap_or(default_outline);
            let raw = item
                .get("raw")
                .and_then(Value::as_bool)
                .unwrap_or(default_raw);
            let symbols = item
                .get("symbols")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let ranges = item
                .get("ranges")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|pair| {
                            let a = pair.as_array()?;
                            let s = usize::try_from(a.first()?.as_u64()?).ok()?;
                            let e = usize::try_from(a.get(1)?.as_u64()?).ok()?;
                            Some((s, e))
                        })
                        .collect()
                })
                .unwrap_or_default();
            specs.push(dispatch::ReadManySpec {
                raw_path: raw_path.to_owned(),
                offset,
                max_lines,
                outline,
                symbols,
                ranges,
                raw,
            });
        } else {
            return Err("readMany `paths` entries must be strings or objects".into());
        }
    }

    let mut read_paths = Vec::with_capacity(specs.len());
    let mut all_resolved = !specs.is_empty();
    for spec in &specs {
        if let Ok(resolved) =
            crate::tools::resolve_workspace_path_from_root(&workspace, &spec.raw_path)
        {
            read_paths.push(super::tool_schedule::schedule_key_for_path(&resolved));
        } else {
            all_resolved = false;
            break;
        }
    }
    if !all_resolved {
        read_paths.clear();
    }
    let fingerprint = canonical_json_hash(b"slim-read-many-v1", &[u8::from(structured)], &arguments);

    Ok(Target::ReadMany {
        specs,
        structured,
        workspace,
        read_paths,
        fingerprint,
        arguments,
    })
}

fn prepare_outline_many_target(
    scope: &CellScope<'_>,
    arguments: Value,
) -> Result<Target, String> {
    let workspace = scope.workspace.clone()?;
    let Some(obj) = arguments.as_object() else {
        return Err("outlineMany arguments must be an object".into());
    };
    let max_files = obj
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(40)
        .clamp(1, 64);
    let max_symbols = obj
        .get("max_symbols")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(64)
        .clamp(1, 200);

    let input = if let Some(arr) = obj.get("paths").and_then(Value::as_array) {
        let mut raw_paths = Vec::with_capacity(arr.len().min(max_files));
        for raw in arr.iter().take(max_files) {
            let Some(p) = raw.as_str() else {
                return Err("outlineMany `paths` entries must be strings".into());
            };
            raw_paths.push(p.to_owned());
        }
        dispatch::OutlineManyInput::Paths(raw_paths)
    } else if let Some(glob_pat) = obj.get("glob").and_then(Value::as_str) {
        dispatch::OutlineManyInput::Glob {
            subpath: obj.get("path").and_then(Value::as_str).map(str::to_owned),
            pattern: glob_pat.to_owned(),
            max_files,
        }
    } else {
        return Err("outlineMany requires `paths` or `glob`".into());
    };
    let fingerprint = canonical_json_hash(b"slim-outline-many-v1", &[], &arguments);

    Ok(Target::OutlineMany {
        input,
        max_symbols,
        workspace,
        fingerprint,
        arguments,
    })
}

fn prepare_grep_index_target(
    scope: &CellScope<'_>,
    arguments: Value,
) -> Result<Target, String> {
    let workspace = scope.workspace.clone()?;
    let Some(obj) = arguments.as_object() else {
        return Err("grepIndex arguments must be an object".into());
    };
    let mut queries = Vec::new();
    if let Some(arr) = obj.get("queries").and_then(Value::as_array) {
        for item in arr.iter().take(16) {
            if let Some(s) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                queries.push(s.to_owned());
            }
        }
    } else if let Some(q) = obj.get("query").and_then(Value::as_str) {
        let trimmed = q.trim();
        if !trimmed.is_empty() {
            queries.push(trimmed.to_owned());
        }
    }
    if queries.is_empty() {
        return Err("grepIndex requires `queries` (array of non-empty strings)".into());
    }
    let case_insensitive = obj
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let max_hits = obj
        .get("max_hits")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let fingerprint = canonical_json_hash(b"slim-grep-index-v1", &[], &arguments);

    Ok(Target::GrepIndex {
        queries,
        case_insensitive,
        path,
        max_hits,
        workspace,
        fingerprint,
        arguments,
    })
}

fn prepare_graph_target(scope: &CellScope<'_>, arguments: Value) -> Result<Target, String> {
    let workspace = scope.workspace.clone()?;
    let Some(obj) = arguments.as_object() else {
        return Err("graph arguments must be an object".into());
    };
    let Some(op) = obj.get("op").and_then(Value::as_str) else {
        return Err("graph requires `op` (definitions, callers, callees, imports, tests, impact, symbols)".into());
    };
    if !matches!(
        op,
        "definitions" | "callers" | "callees" | "imports" | "tests" | "impact" | "symbols"
    ) {
        return Err(format!("unsupported graph op `{op}`"));
    }
    let symbol = obj.get("symbol").and_then(Value::as_str).map(str::to_owned);
    let path = obj.get("path").and_then(Value::as_str).map(str::to_owned);
    let depth = obj
        .get("depth")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(2)
        .clamp(1, 2);
    let limit = obj
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let fingerprint = canonical_json_hash(b"slim-graph-v1", &[], &arguments);

    Ok(Target::Graph {
        op: op.to_owned(),
        symbol,
        path,
        depth,
        limit,
        workspace,
        fingerprint,
        arguments,
    })
}
