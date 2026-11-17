//! Lifecycle of one internal call: admission when the program asks for it,
//! start when the scheduler admits it (budget, run guards, task, journal,
//! step event) and settlement when it finishes.

use super::*;
use dispatch::{Done, Finished, Job, Running, Target};

/// Bytes of a failed native output kept in the journal.
pub(super) const MAX_JOURNALED_ERROR_BYTES: usize = 1024;
/// Output a step event carries for display; the program gets all of it.
const MAX_STEP_OUTPUT_BYTES: usize = 16 * 1024;
/// Characters of an unawaited failure kept in the result's note.
const MAX_NOTE_ERROR_CHARS: usize = 200;

impl Runtime {
    /// Checks a requested call and resolves what it runs. Nothing is sent
    /// and no budget is used until the call starts.
    pub(super) fn admit_call(
        &self,
        scope: &CellScope<'_>,
        ticket: u32,
        name: String,
        arguments: Value,
    ) -> Result<Job, String> {
        if !arguments.is_object() {
            return Err("tool arguments must be an object".into());
        }
        if super::stream_normalizer::sensitive_tool_arguments(
            &arguments.to_string(),
            &self.sensitive_values.0,
        ) || self.redact_sensitive(&name) != name
        {
            return Err("tool call contains registered sensitive material; use a configured credential reference".into());
        }
        let (target, slots) = if let Some(native) = name.strip_prefix("slim.") {
            // The same slots as the provider call: an edit with then_run
            // also runs a command.
            let slots = tool_call_slots(&ProviderToolCall {
                id: String::new(),
                name: native.to_owned(),
                arguments: arguments.to_string(),
            });
            (self.native_target(scope, native, arguments)?, slots)
        } else if let Some((server, tool)) = name
            .strip_prefix("mcp.")
            .and_then(|name| name.split_once('.'))
            .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
        {
            let Some(manager) = self.mcp.clone() else {
                return Err("no MCP servers configured".into());
            };
            let target = Target::Mcp {
                manager,
                server: server.to_owned(),
                tool: tool.to_owned(),
                arguments,
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
            target,
        })
    }

    fn native_target(
        &self,
        scope: &CellScope<'_>,
        name: &str,
        mut arguments: Value,
    ) -> Result<Target, String> {
        if scope.natives.is_empty() {
            return Err("slim.<tool> functions are only available in Code mode".into());
        }
        if scope.native(name).is_none() {
            return Err(format!(
                "unknown slim tool: {}; available: {}",
                name.chars().take(64).collect::<String>(),
                native_names(scope.natives).join(", ")
            ));
        }
        refuse_unsupported(name, &arguments)?;
        if name == "shell" {
            cap_shell_timeout(&mut arguments, scope.deadline)?;
        }
        let mut prepared =
            self.tools
                .prepare_invocation(scope.mode, scope.cwd, name, &arguments.to_string());
        if self.tuning_closed && prepared.error.is_none() && prepared.is_tuning_patch() {
            prepared.reject(TUNING_CLOSED_ERROR);
        }
        Ok(Target::Native {
            exclusive: !super::tool_schedule::tool_call_is_parallel_snapshot_read(
                &self.tools,
                name,
            ),
            prepared: Box::new(prepared),
            arguments,
        })
    }

    /// Starts every queued call the scheduler admits: budget and run guards,
    /// then its task, then the `started` fact and step event, so a journal
    /// failure never spends budget on a call that does not run.
    pub(super) fn start_ready_calls(
        &mut self,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
        state: &mut CellState,
        seq: &mut u64,
    ) -> Result<(), ProviderError> {
        while let Some(mut job) = state.calls.next_startable() {
            if cell.cancellation.is_cancelled() {
                cell.settle(
                    job.ticket,
                    Err("cell cancelled or deadline exceeded; call not sent".into()),
                );
                continue;
            }
            if self.codemode.remaining_calls < job.slots {
                cell.settle(
                    job.ticket,
                    Err("tool budget exhausted; the call was not sent".into()),
                );
                continue;
            }
            self.codemode.remaining_calls -= job.slots;
            self.codemode.used_calls += job.slots;
            let id = format!(
                "{}:{}:{}",
                scope.parent.batch_id, scope.parent.call_id, self.codemode.used_calls
            );
            let arguments = match &mut job.target {
                Target::Mcp { arguments, .. } => arguments.clone(),
                Target::Native {
                    prepared,
                    arguments,
                    ..
                } => {
                    // The same holds as a provider call: an oversized read,
                    // a repeated failing command, a second full rewrite.
                    self.run_guards.admit(&id, prepared);
                    arguments.clone()
                }
            };
            let started = json!({
                "parent_call_id": scope.parent.call_id, "name": job.name, "status": "started", "arguments": arguments,
            });
            let step = crate::EventKind::ToolStepStarted {
                batch_id: scope.parent.batch_id.into(),
                call_id: scope.parent.call_id.into(),
                step_id: id.clone(),
                name: job.name.clone(),
                arguments: self.redact_sensitive(&arguments.to_string()),
            };
            state.calls.spawn(id.clone(), job);
            self.persist_codemode_fact(CALL_NAMESPACE, &id, started)?;
            push_runtime_event(&mut self.app, seq, step)?;
        }
        Ok(())
    }

    /// Settles a finished call: redaction, the bridge limit, run guards and
    /// language-server sync, the program's promise, then the artifact, the
    /// terminal fact and step event. The promise settles even when the
    /// journal fails.
    pub(super) async fn finish_call(
        &mut self,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
        state: &mut CellState,
        seq: &mut u64,
        done: Done,
    ) -> Result<(), ProviderError> {
        let Done { running, finished } = done;
        let mut fact = json!({
            "parent_call_id": scope.parent.call_id, "name": running.name,
            "duration_ms": elapsed_millis(running.started),
        });
        let mut artifact = None;
        let mut diff = None;
        let (status, response) = match finished {
            Ok(Finished::Mcp(outcome)) => {
                let (status, response) = self.mcp_response(outcome, cell, state);
                fact["result"] = response.as_ref().ok().cloned().unwrap_or(Value::Null);
                fact["error"] = response
                    .as_ref()
                    .err()
                    .map_or(Value::Null, |error| json!(error));
                (status, response)
            }
            Ok(Finished::Native(outcome)) => {
                let (status, response, result, edit_diff) =
                    self.native_response(scope, &running, *outcome).await;
                diff = edit_diff;
                match &response {
                    Ok(output) => {
                        fact["result"] =
                            json!({"output_bytes": output.as_str().map_or(0, str::len)});
                    }
                    Err(error) => {
                        fact["error"] = json!(prefix(error, MAX_JOURNALED_ERROR_BYTES));
                        fact["error_bytes"] = json!(error.len());
                    }
                }
                artifact = Some(result);
                (status, response)
            }
            Err(error) => {
                let error = format!("{} task failed: {error}", running.name);
                fact["error"] = json!(error);
                ("failed", Err(error))
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
            output: step_output(&response),
            diff,
        };
        cell.settle(running.ticket, response);
        if let Some(result) = &artifact {
            self.record_existing_artifact(result, seq)?;
        }
        self.persist_codemode_fact(CALL_NAMESPACE, &running.id, fact)?;
        push_runtime_event(&mut self.app, seq, step)
    }

    fn mcp_response(
        &mut self,
        outcome: McpRequestOutcome<Value>,
        cell: &engine::Cell,
        state: &mut CellState,
    ) -> (&'static str, Result<Value, String>) {
        self.refresh_mcp_secrets();
        match outcome {
            McpRequestOutcome::Completed(Ok(mut value)) => {
                redact_value(&mut value, &self.sensitive_values.0);
                if value.to_string().len() > MAX_JSON_BYTES {
                    ("completed", Err("MCP call completed, but its result exceeds the 1 MiB bridge limit; do not repeat it blindly".into()))
                } else if value.get("isError").and_then(Value::as_bool) == Some(true) {
                    ("failed", Ok(value))
                } else {
                    ("completed", Ok(value))
                }
            }
            McpRequestOutcome::Completed(Err(error)) => ("failed", Err(error.to_string())),
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => (
                "not_sent",
                Err(format!(
                    "MCP call not sent: {interruption:?}; cleanup: {cleanup:?}"
                )),
            ),
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                let error = format!(
                    "MCP outcome uncertain: {interruption:?}; cleanup: {cleanup:?}; do not replay"
                );
                state.uncertain.push(error.clone());
                cell.cancellation.cancel();
                ("uncertain", Err(error))
            }
        }
    }

    /// The program's view of a native outcome, the result whose artifact
    /// the caller records and the redacted edit diff for display.
    async fn native_response(
        &mut self,
        scope: &CellScope<'_>,
        running: &Running,
        outcome: ToolExecutionOutcome,
    ) -> (
        &'static str,
        Result<Value, String>,
        ToolResult,
        Option<crate::ToolEditDiff>,
    ) {
        let mut output = self.redact_sensitive(&outcome.result.output);
        // The program, not the model, receives the output: nothing counts
        // as already read, and a hold is not remembered as a provider call.
        self.run_guards.was_held(&running.id);
        if let Some(note) = self
            .run_guards
            .settle(&running.id, outcome.result.success, false)
        {
            output.push('\n');
            output.push_str(&note);
        }
        if let Some(reference) = Self::artifact_reference(&outcome.result, scope.cwd) {
            output.push('\n');
            output.push_str(&reference);
        }
        if let Some(prepared) = &running.prepared {
            let native = ToolInvocation {
                batch_id: scope.parent.batch_id,
                call_id: &running.id,
                name: &prepared.name,
                arguments: "",
            };
            self.notify_code_intel_after_mutation(scope.cwd, native, prepared, &outcome)
                .await;
        }
        let status = if outcome.result.success {
            "completed"
        } else {
            "failed"
        };
        let response = if output.len() > MAX_JSON_BYTES {
            Err(format!(
                "{} finished ({status}), but its output exceeds the 1 MiB bridge limit; narrow the call",
                running.name
            ))
        } else if outcome.result.success {
            Ok(Value::String(output))
        } else {
            Err(output)
        };
        let diff = outcome
            .result
            .success
            .then(|| self.redacted_edit_diff(outcome.receipt.edit_diff.as_ref()))
            .flatten();
        (status, response, outcome.result, diff)
    }
}

/// What a step shows: the result text (an MCP result as compact JSON) or
/// the error, bounded. Both are already redacted.
fn step_output(response: &Result<Value, String>) -> String {
    let text = match response {
        Ok(Value::String(text)) | Err(text) => std::borrow::Cow::Borrowed(text.as_str()),
        Ok(value) => std::borrow::Cow::Owned(value.to_string()),
    };
    prefix(&text, MAX_STEP_OUTPUT_BYTES).to_owned()
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
