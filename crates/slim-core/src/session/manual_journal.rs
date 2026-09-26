use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::time::Instant;

use crate::context::ArtifactStore;
use crate::provider::{
    normalized_provider_code, ProviderCallOutcome, ProviderCallTelemetry, ProviderMessage,
    ProviderToolCall,
};
use crate::EventKind;

use super::manual_drive::{
    persist_manual_failure_with_facts, persist_manual_prefix, persist_manual_terminal_with_facts,
    run_telemetry_terminal_fact,
};
use super::schema_v2::DurableFact;
use super::{
    DurableEntry, DurableOutcome, DurableRecord, DurableRepo, JsonlRepo, ManualRunSpec,
    ProviderResponse,
};

/// A manual run whose conversation is durable before the next external effect.
/// Uses ordinary v2 entries, so a crash leaves a visible, incomplete call group.
pub struct ManualRunJournal {
    repo: JsonlRepo,
    spec: ManualRunSpec,
    parent: String,
    batch_id: Option<String>,
    pending: BTreeMap<String, (String, bool)>,
    call_ids: BTreeMap<String, String>,
    event_call_ids: BTreeMap<(String, String), String>,
    /// Durable raw tool entry keyed by the provider-visible call ID. The
    /// model-facing projection is recorded as a separate fact after the raw
    /// result has been appended, so replay can select it without replacing
    /// the authoritative capture.
    tool_entries: BTreeMap<String, String>,
    presentation_fact_keys: BTreeSet<String>,
    process_fact_keys: BTreeSet<String>,
    job_output_fact_keys: BTreeSet<String>,
    run_started: Instant,
    provider_call_ordinal: u32,
    run_telemetry_terminal: Option<super::RunTelemetryTerminal>,
    uncertain: bool,
    entries_written: usize,
    failure: Option<String>,
    artifact_store: Option<ArtifactStore>,
    max_result_bytes: usize,
}

impl fmt::Debug for ManualRunJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManualRunJournal")
            .field("entries_written", &self.entries_written)
            .field("pending", &self.pending.len())
            .field("failed", &self.failure.is_some())
            .finish_non_exhaustive()
    }
}

impl ManualRunJournal {
    pub fn start(mut repo: JsonlRepo, spec: ManualRunSpec) -> io::Result<Self> {
        let run_started = Instant::now();
        persist_manual_prefix::<_, io::Error>(&mut repo, &spec).map_err(io::Error::other)?;
        Ok(Self {
            parent: spec.input_entry_id.clone(),
            repo,
            spec,
            batch_id: None,
            pending: BTreeMap::new(),
            call_ids: BTreeMap::new(),
            event_call_ids: BTreeMap::new(),
            tool_entries: BTreeMap::new(),
            presentation_fact_keys: BTreeSet::new(),
            process_fact_keys: BTreeSet::new(),
            job_output_fact_keys: BTreeSet::new(),
            run_started,
            provider_call_ordinal: 0,
            run_telemetry_terminal: None,
            uncertain: false,
            entries_written: 0,
            failure: None,
            artifact_store: None,
            max_result_bytes: usize::MAX,
        })
    }

    pub fn repo(&self) -> &JsonlRepo {
        &self.repo
    }

    pub fn repo_mut(&mut self) -> &mut JsonlRepo {
        &mut self.repo
    }

    pub(crate) fn configure_output(&mut self, store: Option<ArtifactStore>, max_bytes: usize) {
        self.artifact_store = store;
        self.max_result_bytes = max_bytes;
    }

    pub fn begin_tools(
        &mut self,
        batch_id: &str,
        assistant: ProviderMessage,
        original_calls: &[ProviderToolCall],
    ) -> io::Result<()> {
        self.check()?;
        if !self.pending.is_empty() {
            return Err(io::Error::other(
                "previous durable tool batch is incomplete",
            ));
        }
        let mut pending = BTreeMap::new();
        if assistant.tool_calls.len() != original_calls.len() {
            return Err(io::Error::other("durable tool batch identity mismatch"));
        }
        for call in &assistant.tool_calls {
            if call.id.trim().is_empty()
                || call.name.trim().is_empty()
                || pending
                    .insert(call.id.clone(), (call.name.clone(), false))
                    .is_some()
            {
                return Err(io::Error::other("invalid or duplicate durable tool call"));
            }
        }
        let call_ids: BTreeMap<String, String> = original_calls
            .iter()
            .zip(&assistant.tool_calls)
            .map(|(original, redacted)| (original.id.clone(), redacted.id.clone()))
            .collect();
        self.append(assistant)?;
        self.batch_id = Some(batch_id.to_owned());
        self.pending = pending;
        for (original, redacted) in &call_ids {
            self.event_call_ids
                .insert((batch_id.to_owned(), original.clone()), redacted.clone());
        }
        self.call_ids = call_ids;
        Ok(())
    }

    pub fn record_event(&mut self, event: &EventKind) -> io::Result<()> {
        self.check()?;
        match event {
            EventKind::ToolStarted {
                batch_id, call_id, ..
            } if self.batch_id.as_ref() == Some(batch_id) => {
                let call_id = self.call_ids.get(call_id).unwrap_or(call_id);
                let (_, started) = self
                    .pending
                    .get_mut(call_id)
                    .ok_or_else(|| io::Error::other("durable tool start has no call"))?;
                *started = true;
            }
            EventKind::ToolOutput {
                batch_id,
                call_id,
                name,
                output,
            } => {
                let call_id = self.call_ids.get(call_id).unwrap_or(call_id).clone();
                if self.batch_id.as_ref() != Some(batch_id) || !self.pending.contains_key(&call_id)
                {
                    return Err(io::Error::other("durable tool result has no pending call"));
                }
                let mut artifact_error = None;
                let durable_output = output.clone();
                if output.len() > self.max_result_bytes {
                    if let Some(store) = self.artifact_store.as_ref() {
                        if let Err(error) = store.put(&format!("tool-{name}"), output.as_bytes()) {
                            artifact_error = Some(error);
                        }
                    }
                }
                // Durable tool entries keep the authoritative raw capture.
                // Model-facing projection is selected later by the aggregate
                // planner, so a cursor or head/tail preview can never become
                // the replay source of truth.
                let output = durable_output;
                let entry_id = self.append(ProviderMessage::tool(name, &call_id, output))?;
                self.tool_entries.insert(call_id.clone(), entry_id);
                self.pending.remove(&call_id);
                if let Some(error) = artifact_error {
                    return Err(io::Error::other(format!("artifact materialization failed after tool execution: {error}; the captured result was preserved in the durable session")));
                }
            }
            EventKind::ToolFinished {
                batch_id,
                call_id,
                name,
                success,
                duration_ms,
                ..
            } => {
                // Observability-only: tool outcome as a namespaced fact. Facts
                // are keyed readers (task.v1, capability.v1, ...) ignore, the
                // reducer only indexes them, and resume rebuilds exclusively
                // from entries -- so this record cannot change replay. It
                // resolves the redacted durable identity exactly like
                // ToolStarted and never fails an otherwise healthy run.
                let durable_id = self
                    .event_call_ids
                    .get(&(batch_id.clone(), call_id.clone()))
                    .cloned()
                    .unwrap_or_else(|| call_id.clone());
                // Observability-only fact: skip the per-call data sync. The
                // next synced entry append flushes this line; on crash only
                // the unreplayable trailing fact may be lost.
                let seq = self.repo.next_seq()?;
                let result = self.repo.append_unsynced(DurableRecord::Fact {
                    seq,
                    fact: DurableFact {
                        namespace: "tool.v1".into(),
                        key: durable_id,
                        value: serde_json::json!({
                            "name": name,
                            "success": success,
                            "duration_ms": duration_ms,
                        }),
                    },
                });
                self.remember_failure(result)?;
            }
            EventKind::ToolProcessFinished {
                batch_id,
                call_id,
                name,
                process,
                ..
            } => {
                // Process facts are observability-only and intentionally live
                // in their own namespace. Keep the durable call identity
                // aligned with ToolStarted/ToolFinished while ensuring a
                // repeated event cannot append the same fact twice.
                let durable_id = self
                    .event_call_ids
                    .get(&(batch_id.clone(), call_id.clone()))
                    .cloned()
                    .unwrap_or_else(|| call_id.clone());
                if !self.process_fact_keys.insert(durable_id.clone()) {
                    return Ok(());
                }
                let value = serde_json::to_value(process).map_err(|error| {
                    io::Error::other(format!("failed to serialize process facts: {error}"))
                })?;
                let seq = self.repo.next_seq()?;
                let result = self.repo.append_unsynced(DurableRecord::Fact {
                    seq,
                    fact: DurableFact {
                        namespace: "tool.process.v1".into(),
                        key: durable_id,
                        value: serde_json::json!({
                            "name": name,
                            "process": value,
                        }),
                    },
                });
                self.remember_failure(result)?;
            }
            EventKind::ToolJobOutput {
                batch_id,
                call_id,
                name,
                output,
            } => {
                let durable_id = self
                    .event_call_ids
                    .get(&(batch_id.clone(), call_id.clone()))
                    .cloned()
                    .unwrap_or_else(|| call_id.clone());
                if !self.job_output_fact_keys.insert(durable_id.clone()) {
                    return Err(io::Error::other("duplicate durable shell job output"));
                }
                let seq = self.repo.next_seq()?;
                let result = self.repo.append(DurableRecord::Fact {
                    seq,
                    fact: DurableFact {
                        namespace: "tool.job_output.v1".into(),
                        key: durable_id,
                        value: serde_json::json!({"name": name, "output": output}),
                    },
                });
                self.remember_failure(result)?;
            }
            EventKind::ArtifactStored { id, size } => {
                let seq = self.repo.next_seq()?;
                let result = self.repo.append(DurableRecord::Fact {
                    seq,
                    fact: DurableFact {
                        namespace: "artifact.v1".into(),
                        key: id.clone(),
                        value: serde_json::json!({"size": size}),
                    },
                });
                self.remember_failure(result)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Persist bounded metadata for one actual provider HTTP call. The caller
    /// invokes this only after `HttpProviderClient` has started a network
    /// request; cache hits never reach this boundary.
    pub(crate) fn record_provider_call(
        &mut self,
        telemetry: &ProviderCallTelemetry,
    ) -> io::Result<()> {
        self.check()?;
        let ordinal = self
            .provider_call_ordinal
            .checked_add(1)
            .ok_or_else(|| io::Error::other("provider call ordinal exhausted"))?;
        self.provider_call_ordinal = ordinal;
        let id = format!("{}-provider-call-{ordinal}", self.spec.operation_id);
        let outcome = match telemetry.outcome {
            ProviderCallOutcome::Success => "success",
            ProviderCallOutcome::Failed => "failed",
            ProviderCallOutcome::Cancelled => "cancelled",
        };
        let code = telemetry.code.as_deref().and_then(normalized_provider_code);
        let seq = self.repo.next_seq()?;
        let result = self.repo.append(DurableRecord::Fact {
            seq,
            fact: DurableFact {
                namespace: "provider.call.v1".into(),
                key: id.clone(),
                value: serde_json::json!({
                    "id": id,
                    "ordinal": ordinal,
                    "operation_id": &self.spec.operation_id,
                    "attempt_id": &self.spec.attempt_id,
                    "provider": &telemetry.provider,
                    "model": &telemetry.model,
                    "duration_ms": telemetry.duration_ms,
                    "headers_ms": telemetry.headers_ms,
                    "first_semantic_ms": telemetry.first_semantic_ms,
                    "outcome": outcome,
                    "http_status": telemetry.status,
                    "code": code,
                    "retry_after_ms": telemetry.retry_after_ms,
                }),
            },
        });
        self.remember_failure(result.map_err(|error| {
            io::Error::other(format!(
                "provider call fact could not be persisted: {error}"
            ))
        }))
    }

    /// Tool calls/results were already saved at their execution boundaries.
    /// Ordinary assistant/user messages retain their original conversation order.
    pub fn record_message(&mut self, message: ProviderMessage) -> io::Result<()> {
        self.check()?;
        if message.role == "tool" {
            self.record_tool_presentation(message)?;
            return Ok(());
        }
        if !message.tool_calls.is_empty() {
            return Ok(());
        }
        self.close_pending()?;
        self.append(message).map(|_| ())
    }

    pub fn set_run_telemetry_terminal(
        &mut self,
        terminal: super::RunTelemetryTerminal,
    ) -> io::Result<()> {
        self.check()?;
        if self.spec.run_telemetry.is_none() {
            return Err(io::Error::other(
                "durable run telemetry terminal has no start context",
            ));
        }
        if self.run_telemetry_terminal.is_some() {
            return Err(io::Error::other(
                "durable run telemetry terminal was already recorded",
            ));
        }
        self.run_telemetry_terminal = Some(terminal);
        Ok(())
    }

    pub fn finish(&mut self, mut response: ProviderResponse) -> io::Result<()> {
        self.check()?;
        self.close_pending()?;
        if self.entries_written == 0 {
            self.append(ProviderMessage::assistant(
                response.content.clone(),
                Vec::new(),
            ))?;
        }
        if self.uncertain {
            response.outcome = DurableOutcome::Unknown;
        }
        let telemetry_fact = self.take_run_telemetry_fact(
            response.outcome.clone(),
            match &response.outcome {
                DurableOutcome::Success => "completed",
                DurableOutcome::Failed => "failed",
                DurableOutcome::Cancelled => "cancelled",
                DurableOutcome::Unknown => "unknown_tool_effect",
            },
        );
        let seq = self
            .repo
            .records()
            .last()
            .expect("manual prefix exists")
            .seq();
        let result = persist_manual_terminal_with_facts::<_, io::Error>(
            &mut self.repo,
            self.spec.clone(),
            seq,
            Vec::new(),
            response,
            telemetry_fact.into_iter().collect(),
        )
        .map_err(io::Error::other);
        self.remember_failure(result)
    }

    pub fn fail_attempt(&mut self, error: super::DurableErrorClass) -> io::Result<()> {
        self.check()?;
        let (outcome, stop) = match &error {
            super::DurableErrorClass::Cancelled => (DurableOutcome::Cancelled, "cancelled"),
            _ => (DurableOutcome::Failed, "provider_error"),
        };
        let telemetry_fact = self.take_run_telemetry_fact(outcome, stop);
        let seq = self
            .repo
            .records()
            .last()
            .expect("manual prefix exists")
            .seq();
        let result = persist_manual_failure_with_facts::<_, io::Error>(
            &mut self.repo,
            &self.spec,
            seq,
            error,
            telemetry_fact.into_iter().collect(),
        )
        .map_err(io::Error::other);
        self.remember_failure(result)
    }

    fn take_run_telemetry_fact(
        &mut self,
        outcome: DurableOutcome,
        fallback_stop: &str,
    ) -> Option<DurableFact> {
        let context = self.spec.run_telemetry.as_ref()?;
        let mut terminal =
            self.run_telemetry_terminal
                .take()
                .unwrap_or_else(|| super::RunTelemetryTerminal {
                    stop: fallback_stop.into(),
                    outcome: outcome.clone(),
                    validated_completion: false,
                    validation_source: None,
                    usage: serde_json::Value::Null,
                    costs: serde_json::Value::Null,
                    limits: context.limits.clone(),
                });
        terminal.outcome = outcome;
        let duration_ms = self
            .run_started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        run_telemetry_terminal_fact(&self.spec, &terminal, duration_ms)
    }

    fn close_pending(&mut self) -> io::Result<()> {
        for (id, (name, started)) in self.pending.clone() {
            let output = if started {
                self.uncertain = true;
                "[Unconfirmed result] Tool started, but no result was captured. Effects are unknown. Inspect prior effects; do not replay automatically."
            } else {
                "[Not executed] This call did not start before the run stopped. No tool effect was performed."
            };
            self.append(ProviderMessage::tool(name, &id, output))?;
            self.pending.remove(&id);
        }
        Ok(())
    }

    fn record_tool_presentation(&mut self, message: ProviderMessage) -> io::Result<()> {
        let name = message
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| io::Error::other("durable tool presentation has no name"))?;
        let call_id = message
            .tool_call_id
            .as_deref()
            .filter(|call_id| !call_id.trim().is_empty())
            .ok_or_else(|| io::Error::other("durable tool presentation has no call ID"))?;
        let entry_id = self
            .tool_entries
            .get(call_id)
            .cloned()
            .ok_or_else(|| io::Error::other("durable tool presentation has no raw result"))?;
        if !self.presentation_fact_keys.insert(entry_id.clone()) {
            return Ok(());
        }
        let seq = self.repo.next_seq()?;
        let fact = DurableFact {
            namespace: "tool.presentation.v1".into(),
            key: entry_id,
            value: serde_json::json!({
                "name": name,
                "call_id": call_id,
                "output": message.content,
            }),
        };
        let result = self.repo.append_unsynced(DurableRecord::Fact { seq, fact });
        self.remember_failure(result)
    }

    fn append(&mut self, message: ProviderMessage) -> io::Result<String> {
        self.check()?;
        let seq = self.repo.next_seq()?;
        let entry_id = format!("{}-message-{seq}", self.spec.operation_id);
        let entry = DurableEntry::from_provider_message(
            entry_id.clone(),
            Some(self.parent.clone()),
            self.spec.operation_id.clone(),
            message,
        )
        .map_err(io::Error::other)?;
        let result = self.repo.append(DurableRecord::Entry { seq, entry });
        self.remember_failure(result)?;
        self.parent = entry_id;
        self.entries_written += 1;
        Ok(self.parent.clone())
    }

    fn check(&self) -> io::Result<()> {
        match &self.failure {
            Some(message) => Err(io::Error::other(message.clone())),
            None => Ok(()),
        }
    }

    fn remember_failure(&mut self, result: io::Result<()>) -> io::Result<()> {
        if let Err(error) = result {
            let message = format!("durable run persistence failed: {error}; prior tool effects may already have occurred. Inspect the session and workspace before repeating any action");
            self.failure = Some(message.clone());
            return Err(io::Error::other(message));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        preflight_session, provider_messages_from_entries, provider_messages_from_records,
        restore_records, DurableErrorClass, DurableOperationKind, DurableSessionHeader,
        RunTelemetryContext, RunTelemetryTerminal,
    };
    use crate::OperatingMode;
    use std::fs;

    fn fixture() -> (std::path::PathBuf, ManualRunJournal) {
        let root = std::env::temp_dir().join(format!(
            "slim-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let repo = JsonlRepo::create(
            root.join("session.jsonl"),
            DurableSessionHeader::new("journal", "now", root.to_str().unwrap(), None, None),
        )
        .unwrap();
        let journal = ManualRunJournal::start(
            repo,
            ManualRunSpec::new("op", "attempt", "input", "final", "inspect", 0),
        )
        .unwrap();
        (root, journal)
    }

    fn call(id: &str) -> ProviderToolCall {
        ProviderToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{}".into(),
        }
    }

    fn output(id: &str, text: &str) -> EventKind {
        EventKind::ToolOutput {
            batch_id: "batch".into(),
            call_id: id.into(),
            name: "read".into(),
            output: text.into(),
        }
    }

    #[test]
    fn artifact_id_is_durable_for_session_resume() {
        let (root, mut journal) = fixture();
        journal
            .record_event(&EventKind::ArtifactStored {
                id: "shell-log-deadbeef".into(),
                size: 42,
            })
            .unwrap();
        let preflight = preflight_session(root.join("session.jsonl")).unwrap();
        assert!(preflight.records.iter().any(|record| matches!(
            record,
            DurableRecord::Fact { fact, .. }
                if fact.namespace == "artifact.v1"
                    && fact.key == "shell-log-deadbeef"
                    && fact.value["size"] == 42
        )));
        drop(journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn background_job_output_survives_later_tool_batches_without_second_tool_entry() {
        let (root, mut journal) = fixture();
        let original = vec![ProviderToolCall {
            id: "private-launch-id".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }];
        let mut redacted = original.clone();
        redacted[0].id = "redacted-launch-id".into();
        journal
            .begin_tools(
                "launch-batch",
                ProviderMessage::assistant("", redacted),
                &original,
            )
            .unwrap();
        journal
            .record_event(&EventKind::ToolOutput {
                batch_id: "launch-batch".into(),
                call_id: original[0].id.clone(),
                name: "shell".into(),
                output: "job_id=shell-1 state=running".into(),
            })
            .unwrap();
        let next = vec![call("other-call")];
        journal
            .begin_tools(
                "next-batch",
                ProviderMessage::assistant("", next.clone()),
                &next,
            )
            .unwrap();
        journal
            .record_event(&EventKind::ToolOutput {
                batch_id: "next-batch".into(),
                call_id: "other-call".into(),
                name: "read".into(),
                output: "other result".into(),
            })
            .unwrap();
        journal
            .record_event(&EventKind::ToolJobOutput {
                batch_id: "launch-batch".into(),
                call_id: original[0].id.clone(),
                name: "shell".into(),
                output: "final marker".into(),
            })
            .unwrap();
        journal
            .record_event(&EventKind::ToolFinished {
                batch_id: "launch-batch".into(),
                call_id: original[0].id.clone(),
                name: "shell".into(),
                success: true,
                duration_ms: 17,
            })
            .unwrap();
        journal
            .record_message(ProviderMessage::user(
                "[Shell job completion: shell-1] final marker",
            ))
            .unwrap();
        journal.finish(ProviderResponse::new("done", None)).unwrap();
        drop(journal);
        let report = preflight_session(root.join("session.jsonl")).unwrap();
        let output_facts = report
            .records
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. } if fact.namespace == "tool.job_output.v1" => {
                    Some(fact)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(output_facts.len(), 1);
        assert_eq!(output_facts[0].key, "redacted-launch-id");
        assert_eq!(output_facts[0].value["output"], "final marker");
        let raw = fs::read_to_string(root.join("session.jsonl")).unwrap();
        assert!(!raw.contains("private-launch-id"));
        let tool_entries = report.records.iter().filter(|record| matches!(record,
            DurableRecord::Entry { entry, .. } if entry.role == super::super::DurableEntryRole::Tool && entry.tool_call_id.as_deref() == Some("redacted-launch-id")
        )).count();
        assert_eq!(tool_entries, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_call_facts_capture_ordered_outcomes_without_payloads() {
        let (root, mut journal) = fixture();
        journal
            .record_provider_call(&ProviderCallTelemetry {
                provider: "openai-compatible".into(),
                model: "fixture-model".into(),
                duration_ms: 12,
                headers_ms: Some(4),
                first_semantic_ms: None,
                outcome: ProviderCallOutcome::Failed,
                status: Some(429),
                code: Some("header-secret".into()),
                retry_after_ms: Some(2500),
            })
            .unwrap();
        journal
            .record_provider_call(&ProviderCallTelemetry {
                provider: "openai-compatible".into(),
                model: "fixture-model".into(),
                duration_ms: 8,
                headers_ms: Some(2),
                first_semantic_ms: Some(5),
                outcome: ProviderCallOutcome::Success,
                status: Some(200),
                code: None,
                retry_after_ms: None,
            })
            .unwrap();
        drop(journal);
        let report = preflight_session(root.join("session.jsonl")).unwrap();
        let facts = report
            .records
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. } if fact.namespace == "provider.call.v1" => {
                    Some(fact)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].value["ordinal"], 1);
        assert_eq!(facts[0].value["attempt_id"], "attempt");
        assert_eq!(facts[0].value["outcome"], "failed");
        assert_eq!(facts[0].value["http_status"], 429);
        assert_eq!(facts[0].value["retry_after_ms"], 2500);
        assert_eq!(facts[0].value["code"], "provider_error");
        assert_eq!(facts[1].value["ordinal"], 2);
        assert_eq!(facts[1].value["outcome"], "success");
        assert_eq!(facts[1].value["http_status"], 200);
        assert_eq!(facts[0].value["headers_ms"], 4);
        assert!(facts[0].value["first_semantic_ms"].is_null());
        assert_eq!(facts[1].value["headers_ms"], 2);
        assert_eq!(facts[1].value["first_semantic_ms"], 5);
        let raw = fs::read_to_string(root.join("session.jsonl")).unwrap();
        assert!(!raw.contains("prompt-marker"));
        assert!(!raw.contains("body-marker"));
        assert!(!raw.contains("header-secret"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_preserves_out_of_order_results_and_redacted_call_identity_once() {
        let (root, mut journal) = fixture();
        journal.configure_output(Some(ArtifactStore::new(root.join("artifacts")).unwrap()), 8);
        let calls = vec![call("secret-id"), call("b")];
        let mut redacted = calls.clone();
        redacted[0].id = "[REDACTED]".into();
        journal
            .begin_tools(
                "batch",
                ProviderMessage::assistant("checking", redacted),
                &calls,
            )
            .unwrap();
        journal.record_event(&output("b", "second result")).unwrap();
        journal
            .record_event(&output("secret-id", "first result"))
            .unwrap();
        journal
            .record_message(ProviderMessage::tool("read", "b", "second preview"))
            .unwrap();
        journal
            .record_message(ProviderMessage::assistant("done", Vec::new()))
            .unwrap();
        journal.finish(ProviderResponse::new("done", None)).unwrap();
        drop(journal);
        let report = preflight_session(root.join("session.jsonl")).unwrap();
        let messages =
            provider_messages_from_entries(report.records.iter().filter_map(
                |record| match record {
                    DurableRecord::Entry { entry, .. } => Some(entry),
                    _ => None,
                },
            ))
            .unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.role == "tool")
                .count(),
            2
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("b"));
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("[REDACTED]"));
        // The raw durable entry remains authoritative, while the linked
        // presentation fact restores exactly what the model saw.
        assert_eq!(messages[2].content, "second result");
        let projected = provider_messages_from_records(report.records.iter()).unwrap();
        assert_eq!(projected[2].content, "second preview");
        assert_eq!(projected[3].content, "first result");
        assert_eq!(
            report
                .records
                .iter()
                .filter(|record| matches!(record, DurableRecord::Fact { fact, .. } if fact.namespace == "tool.presentation.v1"))
                .count(),
            1
        );
        assert_eq!(fs::read_dir(root.join("artifacts")).unwrap().count(), 2);
        assert!(!fs::read_to_string(root.join("session.jsonl"))
            .unwrap()
            .contains("secret-id"));
        assert_eq!(report.summary.pending_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn journal_io_failure_before_or_after_effect_cannot_be_finalized_as_success() {
        for after_effect in [false, true] {
            let (root, mut journal) = fixture();
            let calls = vec![call("a")];
            if after_effect {
                journal
                    .begin_tools(
                        "batch",
                        ProviderMessage::assistant("", calls.clone()),
                        &calls,
                    )
                    .unwrap();
                fs::write(root.join("effect.txt"), "once").unwrap();
            }
            // A real competing file lock makes WriteFile fail; no impossible
            // repository state or mocked successful effect is injected.
            let file = fs::File::open(root.join("session.jsonl")).unwrap();
            file.lock().unwrap();
            let result = if after_effect {
                journal.record_event(&output("a", "receipt"))
            } else {
                journal.begin_tools(
                    "batch",
                    ProviderMessage::assistant("", calls.clone()),
                    &calls,
                )
            };
            file.unlock().unwrap();
            drop(file);
            assert!(result.is_err());
            let error = journal
                .finish(ProviderResponse::new("done", None))
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("prior tool effects may already have occurred"));
            assert_eq!(root.join("effect.txt").exists(), after_effect);
            drop(journal);
            let report = preflight_session(root.join("session.jsonl")).unwrap();
            assert_eq!(report.summary.pending_count(), 1);
            assert_eq!(report.summary.terminal_count(), 0);
            fs::remove_dir_all(root).unwrap();
        }
    }

    fn finished(id: &str, success: bool) -> EventKind {
        EventKind::ToolFinished {
            batch_id: "batch".into(),
            call_id: id.into(),
            name: "read".into(),
            success,
            duration_ms: 7,
        }
    }

    fn process(id: &str, exit_code: Option<i32>) -> EventKind {
        EventKind::ToolProcessFinished {
            batch_id: "batch".into(),
            call_id: id.into(),
            name: "shell".into(),
            process: crate::process::ProcessExecutionFacts {
                exit_code,
                timed_out: false,
                cancelled: false,
                capture_may_be_incomplete: false,
                stdout_bytes: 4,
                stderr_bytes: 2,
                stdout_discarded_bytes: 1,
                stderr_discarded_bytes: 0,
            },
        }
    }

    fn run_telemetry(task_id: &str) -> RunTelemetryContext {
        RunTelemetryContext {
            experiment_id: Some("benchmark".into()),
            task_id: Some(task_id.into()),
            mode: OperatingMode::Auto,
            provider: "openai-compatible".into(),
            model: "fixture-main".into(),
            build_revision: "test-build".into(),
            executable_sha256: Some("a".repeat(64)),
            executable_identity_error: None,
            started_at: 1_700_000_000_000,
            limits: serde_json::json!({"configured": true}),
        }
    }

    fn run_terminal() -> RunTelemetryTerminal {
        RunTelemetryTerminal {
            stop: "provider_completed".into(),
            outcome: DurableOutcome::Success,
            validated_completion: true,
            validation_source: Some("derived_runtime".into()),
            usage: serde_json::json!({"provider_turns": 1}),
            costs: serde_json::json!({"total_micros": 7}),
            limits: serde_json::json!({"configured": false, "max_turns": 8}),
        }
    }

    #[test]
    fn run_telemetry_survives_two_resumed_operations() {
        let root = std::env::temp_dir().join(format!(
            "slim-journal-telemetry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("session.jsonl");
        let repo = JsonlRepo::create(
            &path,
            DurableSessionHeader::new("telemetry", "now", root.to_str().unwrap(), None, None),
        )
        .unwrap();

        let mut first = ManualRunJournal::start(
            repo,
            ManualRunSpec::new("op-a", "attempt-a", "input-a", "final-a", "one", 0)
                .with_run_telemetry(run_telemetry("task-a")),
        )
        .unwrap();
        first
            .record_message(ProviderMessage::assistant("done-a", Vec::new()))
            .unwrap();
        first.set_run_telemetry_terminal(run_terminal()).unwrap();
        first
            .finish(ProviderResponse::with_outcome(
                "done-a",
                None,
                DurableOutcome::Success,
            ))
            .unwrap();
        drop(first);

        let repo = JsonlRepo::open(&path).unwrap();
        let first_seq = repo.next_seq().unwrap();
        let mut second = ManualRunJournal::start(
            repo,
            ManualRunSpec::new("op-b", "attempt-b", "input-b", "final-b", "two", first_seq)
                .with_run_telemetry(run_telemetry("task-b")),
        )
        .unwrap();
        second
            .record_message(ProviderMessage::assistant("done-b", Vec::new()))
            .unwrap();
        second.set_run_telemetry_terminal(run_terminal()).unwrap();
        second
            .finish(ProviderResponse::with_outcome(
                "done-b",
                None,
                DurableOutcome::Success,
            ))
            .unwrap();
        drop(second);

        let report = preflight_session(&path).unwrap();

        let first_start = report
            .records
            .iter()
            .position(|record| {
                matches!(record, DurableRecord::Fact { fact, .. }
                if fact.namespace == "run.telemetry.v1"
                    && fact.key == "op-a"
                    && fact.value["phase"] == "started")
            })
            .unwrap();
        let first_finished = report
            .records
            .iter()
            .position(|record| {
                matches!(record, DurableRecord::Operation { operation, .. }
                if operation.operation_id == "op-a"
                    && matches!(&operation.kind, DurableOperationKind::Finished { .. }))
            })
            .unwrap();
        let first_terminal_telemetry = report
            .records
            .iter()
            .position(|record| {
                matches!(record, DurableRecord::Fact { fact, .. }
                if fact.namespace == "run.telemetry.v1"
                    && fact.key == "op-a"
                    && fact.value["phase"] == "terminal")
            })
            .unwrap();
        assert!(first_start < first_finished);
        assert!(first_finished < first_terminal_telemetry);

        let raw_run_facts = report
            .records
            .iter()
            .filter(|record| {
                matches!(record, DurableRecord::Fact { fact, .. }
                if fact.namespace == "run.telemetry.v1")
            })
            .count();
        assert_eq!(raw_run_facts, 4);

        let reduced = restore_records(&report.records).unwrap();
        let terminal = reduced.fact_value("run.telemetry.v1", "op-a").unwrap();
        assert_eq!(terminal["phase"], "terminal");
        assert_eq!(terminal["mode"], "auto");
        assert_eq!(terminal["provider"], "openai-compatible");
        assert_eq!(terminal["model"], "fixture-main");
        assert_eq!(terminal["build_revision"], "test-build");
        assert_eq!(terminal["executable_sha256"].as_str().unwrap().len(), 64);
        assert_eq!(
            terminal["executable_identity_error"],
            serde_json::Value::Null
        );
        assert_eq!(terminal["stop"], "provider_completed");
        assert_eq!(terminal["outcome"], "success");
        assert_eq!(terminal["validated_completion"], true);
        assert_eq!(terminal["validation_source"], "derived_runtime");
        assert_eq!(terminal["usage"]["provider_turns"], 1);
        assert_eq!(terminal["costs"]["total_micros"], 7);
        assert_eq!(terminal["limits"]["max_turns"], 8);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_decision_failure_keeps_started_mode_and_writes_terminal_telemetry() {
        let root = std::env::temp_dir().join(format!(
            "slim-journal-telemetry-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("session.jsonl");
        let repo = JsonlRepo::create(
            &path,
            DurableSessionHeader::new(
                "telemetry-failure",
                "now",
                root.to_str().unwrap(),
                None,
                None,
            ),
        )
        .unwrap();
        let mut journal = ManualRunJournal::start(
            repo,
            ManualRunSpec::new("op-fail", "attempt-fail", "input", "final", "run", 0)
                .with_run_telemetry(run_telemetry("task-fail")),
        )
        .unwrap();
        journal.fail_attempt(DurableErrorClass::Invalid).unwrap();
        drop(journal);

        let report = preflight_session(&path).unwrap();
        let telemetry = report
            .records
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. }
                    if fact.namespace == "run.telemetry.v1" && fact.key == "op-fail" =>
                {
                    Some(fact)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(telemetry.len(), 2);
        assert_eq!(telemetry[0].value["phase"], "started");
        assert_eq!(telemetry[0].value["mode"], "auto");
        assert_eq!(telemetry[0].value["outcome"], "running");
        assert_eq!(telemetry[1].value["phase"], "terminal");
        assert_eq!(telemetry[1].value["stop"], "provider_error");
        assert_eq!(telemetry[1].value["outcome"], "failed");
        assert_eq!(telemetry[1].value["validated_completion"], false);
        assert!(telemetry[1].value["duration_ms"].is_u64());
        let failed_attempt = report
            .records
            .iter()
            .position(|record| matches!(record, DurableRecord::Operation { operation, .. }
                if operation.operation_id == "op-fail"
                    && matches!(&operation.kind, DurableOperationKind::ProviderAttemptFailed { .. })))
            .unwrap();
        let terminal_telemetry = report
            .records
            .iter()
            .position(|record| {
                matches!(record, DurableRecord::Fact { fact, .. }
                if fact.namespace == "run.telemetry.v1"
                    && fact.key == "op-fail"
                    && fact.value["phase"] == "terminal")
            })
            .unwrap();
        assert!(failed_attempt < terminal_telemetry);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_persists_tool_outcome_without_changing_resume() {
        let (root, mut journal) = fixture();
        let calls = vec![call("a"), call("b")];
        journal
            .begin_tools(
                "batch",
                ProviderMessage::assistant("", calls.clone()),
                &calls,
            )
            .unwrap();
        journal.record_event(&output("a", "ok-a")).unwrap();
        journal.record_event(&finished("a", true)).unwrap();
        journal.record_event(&output("b", "boom")).unwrap();
        journal.record_event(&finished("b", false)).unwrap();
        journal
            .record_message(ProviderMessage::assistant("done", Vec::new()))
            .unwrap();
        journal.finish(ProviderResponse::new("done", None)).unwrap();
        drop(journal);
        let report = preflight_session(root.join("session.jsonl")).unwrap();
        let outcomes: Vec<(String, bool, u64)> = report
            .records
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. } if fact.namespace == "tool.v1" => Some((
                    fact.key.clone(),
                    fact.value.get("success").and_then(|value| value.as_bool()),
                    fact.value
                        .get("duration_ms")
                        .and_then(|value| value.as_u64()),
                )),
                _ => None,
            })
            .filter_map(|(key, success, duration_ms)| Some((key, success?, duration_ms?)))
            .collect();
        assert_eq!(
            outcomes,
            vec![("a".to_string(), true, 7), ("b".to_string(), false, 7),]
        );
        // Resume still observes only conversation entries.
        let messages =
            provider_messages_from_entries(report.records.iter().filter_map(
                |record| match record {
                    DurableRecord::Entry { entry, .. } => Some(entry),
                    _ => None,
                },
            ))
            .unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.role == "tool")
                .count(),
            2
        );
        assert_eq!(report.summary.pending_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_persists_process_facts_once_in_their_own_namespace() {
        let (root, mut journal) = fixture();
        let calls = vec![call("a")];
        journal
            .begin_tools(
                "batch",
                ProviderMessage::assistant("", calls.clone()),
                &calls,
            )
            .unwrap();
        journal.record_event(&output("a", "ok")).unwrap();
        journal.record_event(&process("a", Some(0))).unwrap();
        journal.record_event(&process("a", Some(9))).unwrap();
        journal.record_event(&finished("a", true)).unwrap();
        journal
            .record_message(ProviderMessage::assistant("done", Vec::new()))
            .unwrap();
        journal.finish(ProviderResponse::new("done", None)).unwrap();
        drop(journal);

        let report = preflight_session(root.join("session.jsonl")).unwrap();
        let process_facts = report
            .records
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. } if fact.namespace == "tool.process.v1" => {
                    Some(fact)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(process_facts.len(), 1);
        assert_eq!(process_facts[0].key, "a");
        assert_eq!(process_facts[0].value["name"], "shell");
        assert_eq!(process_facts[0].value["process"]["exit_code"], 0);
        assert_eq!(
            report
                .records
                .iter()
                .filter(|record| matches!(record, DurableRecord::Fact { fact, .. } if fact.namespace == "tool.v1"))
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_facts_keep_repeated_call_ids_distinct_across_batches() {
        let (root, mut journal) = fixture();
        let calls = vec![call("same")];
        journal
            .begin_tools(
                "batch-1",
                ProviderMessage::assistant("first", calls.clone()),
                &calls,
            )
            .unwrap();
        journal
            .record_event(&EventKind::ToolOutput {
                batch_id: "batch-1".into(),
                call_id: "same".into(),
                name: "read".into(),
                output: "raw-first".into(),
            })
            .unwrap();
        journal
            .record_message(ProviderMessage::tool("read", "same", "shown-first"))
            .unwrap();

        journal
            .begin_tools(
                "batch-2",
                ProviderMessage::assistant("second", calls.clone()),
                &calls,
            )
            .unwrap();
        journal
            .record_event(&EventKind::ToolOutput {
                batch_id: "batch-2".into(),
                call_id: "same".into(),
                name: "read".into(),
                output: "raw-second".into(),
            })
            .unwrap();
        journal
            .record_message(ProviderMessage::tool("read", "same", "shown-second"))
            .unwrap();
        journal
            .record_message(ProviderMessage::assistant("done", Vec::new()))
            .unwrap();
        journal.finish(ProviderResponse::new("done", None)).unwrap();
        drop(journal);

        let report = preflight_session(root.join("session.jsonl")).unwrap();
        let raw =
            provider_messages_from_entries(report.records.iter().filter_map(
                |record| match record {
                    DurableRecord::Entry { entry, .. } => Some(entry),
                    _ => None,
                },
            ))
            .unwrap();
        let projected = provider_messages_from_records(report.records.iter()).unwrap();
        let raw_tools = raw
            .iter()
            .filter(|message| message.role == "tool")
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();
        let projected_tools = projected
            .iter()
            .filter(|message| message.role == "tool")
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(raw_tools, vec!["raw-first", "raw-second"]);
        assert_eq!(projected_tools, vec!["shown-first", "shown-second"]);
        assert_eq!(
            report
                .records
                .iter()
                .filter(|record| matches!(record, DurableRecord::Fact { fact, .. } if fact.namespace == "tool.presentation.v1"))
                .count(),
            2
        );
        fs::remove_dir_all(root).unwrap();
    }
}
