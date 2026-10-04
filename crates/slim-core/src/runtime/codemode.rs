//! CodeMode owns orchestration; MCP retains transport and effect semantics.
mod discovery;
#[cfg(test)]
mod discovery_tests;
mod engine;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::session::{DurableFact, DurableRecord, DurableRepo};
use engine::{Command, MAX_CODE_BYTES, MAX_JSON_BYTES};

const STATE_NAMESPACE: &str = "codemode.state.v1";
const MAX_STATE_BYTES: usize = 64 * 1024;
const CELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_HOST_REQUESTS: usize = 1024;

#[derive(Default)]
pub(super) struct CodeMode {
    values: BTreeMap<String, Value>,
    restored: bool,
    pub remaining_calls: usize,
    pub used_calls: usize,
}

pub(super) fn definition() -> Value {
    json!({
        "name": "codemode",
        "description": "Compose MCP calls in isolated JavaScript. Use await tools.call('mcp.server.tool', arguments): returns structuredContent when present, otherwise the MCP result envelope; isError throws. Discover names/schemas with mcp first, or in code with searchTools(query,{limit?,server?}) -> [{name,server,tool,description}] (BM25), describeTool(name) -> {inputSchema,...}|null and listServers() (synchronous; hidden tools never appear). Calls execute in order, one at a time (no parallel calls) and each consumes the current tool budget. return only the needed summary. store(key, JSON_value) / load(key) persist session values (64 KiB total); store(key,null) deletes. No filesystem, network, Node or imports. Cells have fresh globals, 32 MiB memory and 120s deadline; store writes and completed tool effects survive a later error. Never replay a failed cell blindly.",
        "input_schema": {
            "type": "object",
            "properties": {"code": {"type": "string", "minLength": 1, "maxLength": MAX_CODE_BYTES}},
            "required": ["code"],
            "additionalProperties": false
        }
    })
}

impl Runtime {
    pub(super) async fn execute_codemode(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started = self.begin_tool(invocation, &mut seq)?;
        let used_before = self.codemode.used_calls;
        let outcome = if mode.allows_mutation() {
            self.run_codemode(invocation, &mut seq, CELL_TIMEOUT)
                .await?
        } else {
            Err("codemode is only available in Auto mode".into())
        };
        let mut result = match outcome {
            Ok(output) => ToolResult::ok(invocation.name, output),
            Err(error) => ToolResult::fail(invocation.name, format!(
                "codemode failed after {} MCP call(s): {error}. Stored values and prior tool effects remain; do not replay the cell automatically.",
                self.codemode.used_calls - used_before,
            )),
        };
        self.finish_tool(invocation, &mut result, started, &mut seq, None)?;
        Ok((result, seq))
    }

    async fn run_codemode(
        &mut self,
        invocation: ToolInvocation<'_>,
        seq: &mut u64,
        timeout: Duration,
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
        let run = self.cancellation.clone().unwrap_or_default();
        let mut cell = engine::spawn(args.code, run.clone(), timeout);
        let mut requests = 0usize;
        loop {
            let request = tokio::select! {
                result = &mut cell.worker => return Ok(result.unwrap_or_else(|error| Err(format!("interpreter worker: {error}")))),
                request = cell.requests.recv() => match request {
                    Some(request) => request,
                    None => return Ok((&mut cell.worker).await.unwrap_or_else(|error| Err(format!("interpreter worker: {error}")))),
                },
                _ = run.cancelled() => {
                    cell.cancellation.cancel();
                    let _ = (&mut cell.worker).await;
                    return Ok(Err("cell cancelled".into()));
                },
                _ = tokio::time::sleep_until(cell.deadline.into()) => {
                    cell.cancellation.cancel();
                    let _ = (&mut cell.worker).await;
                    return Ok(Err("cell deadline exceeded".into()));
                }
            };
            requests += 1;
            let response = if run.is_cancelled() || Instant::now() >= cell.deadline {
                cell.cancellation.cancel();
                Err("cell cancelled or deadline exceeded".into())
            } else if requests > MAX_HOST_REQUESTS {
                cell.cancellation.cancel();
                Err("cell host-request limit exceeded".into())
            } else {
                match request.command {
                    Command::Call { name, arguments } => {
                        self.codemode_call(invocation, seq, &cell, &run, &name, arguments)
                            .await?
                    }
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
                    command @ (Command::SearchTools(_)
                    | Command::DescribeTool { .. }
                    | Command::ListServers) => self.codemode_discovery(command, &cell, &run).await,
                }
            };
            if cell.cancellation.is_cancelled() {
                let failure = response.err().unwrap_or_else(|| "cell interrupted".into());
                let _ = request.reply.send(Err(failure.clone()));
                let _ = (&mut cell.worker).await;
                return Ok(Err(failure));
            }
            let _ = request.reply.send(response);
        }
    }

    async fn codemode_call(
        &mut self,
        parent: ToolInvocation<'_>,
        seq: &mut u64,
        cell: &engine::Cell,
        run: &CancellationToken,
        name: &str,
        arguments: Value,
    ) -> Result<Result<Value, String>, ProviderError> {
        if run.is_cancelled() || cell.cancellation.is_cancelled() || Instant::now() >= cell.deadline
        {
            return Ok(Err(
                "cell cancelled or deadline exceeded; MCP call not sent".into(),
            ));
        }
        let Some((server, tool)) = name
            .strip_prefix("mcp.")
            .and_then(|name| name.split_once('.'))
        else {
            return Ok(Err("tool name must be mcp.server.tool".into()));
        };
        if server.is_empty() || tool.is_empty() || !arguments.is_object() {
            return Ok(Err(
                "tool name must be mcp.server.tool and arguments must be an object".into(),
            ));
        }
        if super::stream_normalizer::sensitive_tool_arguments(
            &arguments.to_string(),
            &self.sensitive_values.0,
        ) || self.redact_sensitive(name) != name
        {
            return Ok(Err("tool call contains registered sensitive material; use a configured credential reference".into()));
        }
        if self.codemode.remaining_calls == 0 {
            return Ok(Err(
                "tool budget exhausted; no further MCP call was sent".into()
            ));
        }
        let Some(manager) = self.mcp.clone() else {
            return Ok(Err("no MCP servers configured".into()));
        };
        self.codemode.remaining_calls -= 1;
        self.codemode.used_calls += 1;
        let id = format!(
            "{}:{}:{}",
            parent.batch_id, parent.call_id, self.codemode.used_calls
        );
        self.persist_codemode_fact("codemode.call.v1", &id, json!({
            "parent_call_id": parent.call_id, "name": name, "status": "started", "arguments": arguments,
        }))?;
        let preview = self.redact_sensitive(&format!("{name} — running"));
        push_runtime_event(
            &mut self.app,
            seq,
            crate::EventKind::ToolProgress {
                batch_id: parent.batch_id.into(),
                call_id: parent.call_id.into(),
                name: parent.name.into(),
                preview,
            },
        )?;
        let started = Instant::now();
        let pending = manager.call_cancellable(server, tool, arguments, cell.cancellation.clone());
        tokio::pin!(pending);
        let outcome = tokio::select! {
            biased;
            _ = run.cancelled() => { cell.cancellation.cancel(); pending.await },
            _ = tokio::time::sleep_until(cell.deadline.into()) => { cell.cancellation.cancel(); pending.await },
            outcome = &mut pending => outcome,
        };
        self.refresh_mcp_secrets();
        let (status, response) = match outcome {
            McpRequestOutcome::Completed(Ok(mut value)) => {
                redact_value(&mut value, &self.sensitive_values.0);
                if value.to_string().len() > MAX_JSON_BYTES {
                    ("completed", Err("MCP call completed, but its result exceeds the 1 MiB bridge limit; do not repeat it blindly".into()))
                } else {
                    (
                        if value.get("isError").and_then(Value::as_bool) == Some(true) {
                            "failed"
                        } else {
                            "completed"
                        },
                        Ok(value),
                    )
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
                cell.cancellation.cancel();
                ("uncertain", Err(format!("MCP outcome uncertain: {interruption:?}; cleanup: {cleanup:?}; do not replay")))
            }
        };
        self.persist_codemode_fact("codemode.call.v1", &id, json!({
            "parent_call_id": parent.call_id, "name": name, "status": status,
            "duration_ms": elapsed_millis(started), "result": response.as_ref().ok(), "error": response.as_ref().err(),
        }))?;
        let preview = self.redact_sensitive(&format!("{name} — {status}"));
        push_runtime_event(
            &mut self.app,
            seq,
            crate::EventKind::ToolProgress {
                batch_id: parent.batch_id.into(),
                call_id: parent.call_id.into(),
                name: parent.name.into(),
                preview,
            },
        )?;
        Ok(response)
    }

    fn restore_codemode(&mut self) -> Result<(), ProviderError> {
        if self.codemode.restored {
            return Ok(());
        }
        if let Some(journal) = &self.app.run_journal {
            let journal = journal
                .lock()
                .map_err(|_| journal_error("durable run lock poisoned"))?;
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
            journal
                .lock()
                .map_err(|_| journal_error("durable run lock poisoned"))?
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
