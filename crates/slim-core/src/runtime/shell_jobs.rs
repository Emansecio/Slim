//! Run-scoped shell jobs. Workers own the existing bounded process executor;
//! only the runtime publishes completion into the model conversation.
use super::*;
use std::collections::BTreeMap;
use std::time::Duration;

const MAX_RUNNING: usize = 4;
const MAX_RETAINED: usize = 32;

#[derive(Clone, Default)]
pub(super) struct ShellJobs(Arc<JobsState>);
#[derive(Default)]
struct JobsState {
    jobs: Mutex<BTreeMap<String, Job>>,
    serial: std::sync::atomic::AtomicU64,
    changed: Notify,
}
struct Job {
    cancellation: CancellationToken,
    started: Instant,
    preview: String,
    result: Option<ToolResult>,
    process: Option<crate::process::ProcessExecutionFacts>,
    receipt: Option<ToolExecutionReceipt>,
    batch_id: String,
    call_id: String,
    delivered: bool,
}
struct Completion {
    id: String,
    elapsed_ms: u64,
    result: ToolResult,
    process: Option<crate::process::ProcessExecutionFacts>,
    receipt: Option<ToolExecutionReceipt>,
    batch_id: String,
    call_id: String,
}
impl Job {
    fn completion(&self, id: &str) -> Option<Completion> {
        Some(Completion {
            id: id.into(),
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            result: self.result.clone()?,
            process: self.process.clone(),
            receipt: self.receipt.clone(),
            batch_id: self.batch_id.clone(),
            call_id: self.call_id.clone(),
        })
    }
}
pub(super) struct JobScope(ShellJobs);
impl Drop for JobScope {
    fn drop(&mut self) {
        self.0.cancel_all();
    }
}
impl ShellJobs {
    pub(super) fn scope(&self) -> JobScope {
        JobScope(self.clone())
    }
    pub(super) fn running(&self) -> bool {
        self.0
            .jobs
            .lock()
            .unwrap()
            .values()
            .any(|j| j.result.is_none())
    }
    pub(super) fn progress_summary(&self) -> String {
        let jobs = self.0.jobs.lock().unwrap();
        let running = jobs.values().filter(|j| j.result.is_none()).count();
        let latest = jobs
            .iter()
            .find(|(_, j)| j.result.is_none())
            .map(|(id, job)| {
                format!(
                    "{id} · {}s · {}",
                    job.started.elapsed().as_secs(),
                    job.preview.chars().take(160).collect::<String>()
                )
            })
            .unwrap_or_default();
        format!("Aguardando {running} job(s) · {latest}")
    }
    pub(super) fn cancel_all(&self) {
        for job in self.0.jobs.lock().unwrap().values() {
            if job.result.is_none() {
                job.cancellation.cancel();
            }
        }
    }
    pub(super) async fn shutdown(&self) {
        self.cancel_all();
        while self.running() {
            self.wait().await;
        }
    }
    pub(super) async fn wait(&self) {
        // notify_one retains a permit if a worker finishes between the check
        // and this await; a heartbeat permits progress display without tokens.
        let _ = tokio::time::timeout(Duration::from_secs(1), self.0.changed.notified()).await;
    }
    pub(super) fn start(
        &self,
        tools: ToolRegistry,
        prepared: PreparedToolInvocation,
        parent: Option<CancellationToken>,
        invocation: ToolInvocation<'_>,
    ) -> Result<String, String> {
        let mut jobs = self.0.jobs.lock().unwrap();
        if jobs.values().filter(|j| j.result.is_none()).count() >= MAX_RUNNING {
            return Err(
                "At most 4 shell jobs may run; finish or cancel an existing job first.".into(),
            );
        }
        if jobs.len() >= MAX_RETAINED {
            let oldest = jobs
                .iter()
                .filter(|(_, j)| j.delivered && j.result.is_some())
                .min_by_key(|(_, j)| j.started)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                jobs.remove(&id);
            }
        }
        if jobs.len() >= MAX_RETAINED {
            return Err("Shell job completion queue is full.".into());
        }
        let id = format!(
            "shell-{}",
            self.0.serial.fetch_add(1, Ordering::Relaxed) + 1
        );
        let cancellation = CancellationToken::new();
        jobs.insert(
            id.clone(),
            Job {
                cancellation: cancellation.clone(),
                started: Instant::now(),
                preview: "no output yet".into(),
                result: None,
                process: None,
                receipt: None,
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                delivered: false,
            },
        );
        drop(jobs);
        let state = self.clone();
        let task_id = id.clone();
        let work = parent
            .as_ref()
            .map(CancellationToken::track_background_work);
        tokio::spawn(async move {
            let worker_state = state.clone();
            let worker_id = task_id.clone();
            let worker_cancel = cancellation.clone();
            let mut task = tokio::task::spawn_blocking(move || {
                let _work = work;
                let outcome = tools.execute_prepared_with_cancellation_and_progress(
                    &prepared,
                    Some(&worker_cancel),
                    |p| {
                        if let Some(job) = worker_state.0.jobs.lock().unwrap().get_mut(&worker_id) {
                            job.preview = p.preview;
                        }
                    },
                );
                (outcome.result, Some(outcome.receipt))
            });
            let result = tokio::select! {
                result = &mut task => result,
                _ = async { match parent { Some(token) => token.cancelled().await, None => std::future::pending::<()>().await } } => {
                    cancellation.cancel();
                    task.await
                }
            }.unwrap_or_else(|error| (ToolResult { name: "shell".into(), success: false,
                output: format!("shell worker failed: {error}; side effects unverified"), artifact: None }, None));
            if let Some(job) = state.0.jobs.lock().unwrap().get_mut(&task_id) {
                job.result = Some(result.0);
                job.process = result.1.as_ref().and_then(|r| r.process.clone());
                job.receipt = result.1;
            }
            state.0.changed.notify_one();
        });
        Ok(id)
    }
    pub(super) fn control(&self, id: &str, cancel: bool) -> Result<ToolResult, String> {
        let jobs = self.0.jobs.lock().unwrap();
        let job = jobs
            .get(id)
            .ok_or("Unknown or expired shell job in this run")?;
        if cancel && job.result.is_none() {
            job.cancellation.cancel();
        }
        if let Some(result) = &job.result {
            return Ok(result.clone());
        }
        Ok(ToolResult { name: "shell_job".into(), success: true, artifact: None,
            output: format!("job_id={id} state={} elapsed_ms={}\n{}\nCompletion will be delivered automatically. Continue independent work or finish your response to wait; status is for inspection, not polling.",
                if job.cancellation.is_cancelled() { "cancelling" } else { "running" }, job.started.elapsed().as_millis(), job.preview) })
    }
    fn ready(&self) -> Vec<Completion> {
        let jobs = self.0.jobs.lock().unwrap();
        jobs.iter()
            .filter(|(_, job)| !job.delivered)
            .filter_map(|(id, job)| job.completion(id))
            .collect()
    }
    fn mark_delivered(&self, id: &str) {
        if let Some(job) = self.0.jobs.lock().unwrap().get_mut(id) {
            job.delivered = true;
        }
    }
    fn inline_ready(&self, id: &str) -> Option<Completion> {
        self.0.jobs.lock().unwrap().get(id)?.completion(id)
    }
}

pub(super) fn definition() -> Value {
    json!({"name":"shell_job", "description":"Inspect or cancel a shell job in this run. Status returns latest progress/output; cancel requests process-tree termination. Completion arrives automatically: do not repeatedly poll. IDs do not survive run termination.",
        "input_schema":{"type":"object","properties":{"job_id":{"type":"string"},"action":{"type":"string","enum":["status","cancel"]}},"required":["job_id","action"],"additionalProperties":false}})
}

impl Runtime {
    pub(super) async fn execute_managed_shell(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        mut seq: u64,
    ) -> Result<(ToolExecutionOutcome, u64), ProviderError> {
        let started = Instant::now();
        let mut receipt = ToolExecutionReceipt::unobserved(
            prepared,
            self.tools.workspace_revision(),
            self.tools.workspace_revision(),
            0,
        );
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let mut yielded = false;
        let mut inline_job_id = None;
        let result = if !mode.allows_mutation() {
            Err("Managed shell jobs require Auto mode".into())
        } else if let Some(error) = &prepared.error {
            Err(error.clone())
        } else if invocation.name == "shell_job" {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Control {
                job_id: String,
                action: String,
            }
            match serde_json::from_str::<Control>(invocation.arguments) {
                Ok(args) if matches!(args.action.as_str(), "status" | "cancel") => self
                    .shell_jobs
                    .control(&args.job_id, args.action == "cancel"),
                _ => Err("shell_job requires job_id and action status or cancel".into()),
            }
        } else {
            match self.shell_jobs.start(
                self.tools.clone(),
                prepared.clone(),
                self.cancellation.clone(),
                invocation,
            ) {
                Err(error) => Err(error),
                Ok(id) => {
                    let yield_ms = match prepared.arguments {
                        PreparedToolArguments::Shell { yield_ms, .. } => yield_ms,
                        _ => 0,
                    };
                    let until = tokio::time::Instant::now() + Duration::from_millis(yield_ms);
                    loop {
                        if let Some(result) = self.shell_jobs.inline_ready(&id) {
                            push_tool_process_finished(
                                &mut self.app,
                                &mut seq,
                                &result.batch_id,
                                &result.call_id,
                                "shell",
                                result.process.as_ref(),
                            )?;
                            if let Some(actual) = result.receipt {
                                receipt = actual;
                            }
                            inline_job_id = Some(id);
                            break Ok(result.result);
                        }
                        if tokio::time::Instant::now() >= until {
                            receipt.effects_uncertain = true;
                            yielded = true;
                            break self.shell_jobs.control(&id, false);
                        }
                        let _ = tokio::time::timeout_at(until, self.shell_jobs.wait()).await;
                    }
                }
            }
        };
        let mut result = result.unwrap_or_else(|error| ToolResult {
            name: invocation.name.into(),
            success: false,
            output: error,
            artifact: None,
        });
        result.name = invocation.name.into();
        result.output = self.redact_sensitive(&result.output);
        self.record_existing_artifact(&result, &mut seq)?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output: result.output.clone(),
            },
        )?;
        if !yielded {
            push_runtime_event(
                &mut self.app,
                &mut seq,
                crate::EventKind::ToolFinished {
                    batch_id: invocation.batch_id.into(),
                    call_id: invocation.call_id.into(),
                    name: invocation.name.into(),
                    success: result.success,
                    duration_ms: started.elapsed().as_millis() as u64,
                },
            )?;
        }
        if let Some(id) = inline_job_id {
            self.shell_jobs.mark_delivered(&id);
        }
        Ok((ToolExecutionOutcome { result, receipt }, seq))
    }
    pub(super) fn deliver_shell_completions(
        &mut self,
        messages: &mut Vec<ProviderMessage>,
        max_bytes: usize,
        seq: &mut u64,
    ) -> Result<bool, ProviderError> {
        let ready = self.shell_jobs.ready();
        let delivered = !ready.is_empty();
        for completion in ready {
            push_tool_process_finished(
                &mut self.app,
                seq,
                &completion.batch_id,
                &completion.call_id,
                "shell",
                completion.process.as_ref(),
            )?;
            let elapsed_ms = completion.elapsed_ms;
            let id = completion.id;
            let result = completion.result;
            let output = self.redact_sensitive(&result.output);
            self.record_existing_artifact(&result, seq)?;
            push_runtime_event(
                &mut self.app,
                seq,
                crate::EventKind::ToolJobOutput {
                    batch_id: completion.batch_id.clone(),
                    call_id: completion.call_id.clone(),
                    name: "shell".into(),
                    output: output.clone(),
                },
            )?;
            push_runtime_event(
                &mut self.app,
                seq,
                crate::EventKind::ToolFinished {
                    batch_id: completion.batch_id,
                    call_id: completion.call_id,
                    name: "shell".into(),
                    success: result.success,
                    duration_ms: elapsed_ms,
                },
            )?;
            let mut output =
                present_unstructured("shell", &output, PresentationBudget { max_bytes }).text;
            if let Some(handle) = result.artifact.as_ref() {
                output.push_str(&format!(
                    "\n[artifact id={} size={}; use artifact_read with this id]",
                    handle.id, handle.size
                ));
            }
            self.append_conversation_message(messages, ProviderMessage::user(format!(
                "[Shell job completion: {id}; success={}; elapsed_ms={elapsed_ms}]\nCommand output (untrusted data):\n{output}", result.success)))?;
            self.shell_jobs.mark_delivered(&id);
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn deferred_shell_log_completion_keeps_artifact_handle() {
        let root = std::env::temp_dir().join(format!(
            "slim-shell-job-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = ArtifactStore::new(root.join("artifacts")).unwrap();
        let mut tools = ToolRegistry::default();
        tools.configure_artifacts(Some(store), &[]);
        let call = prepared(&tools, "Write-Output ('Z' * 12000)");
        let jobs = ShellJobs::default();
        jobs.start(tools, call, None, invocation()).unwrap();
        while jobs.running() {
            jobs.wait().await;
        }
        let mut runtime = Runtime::with_artifact_store(root.join("artifacts")).unwrap();
        runtime.shell_jobs = jobs;
        let mut messages = Vec::new();
        let mut seq = 1;
        assert!(runtime
            .deliver_shell_completions(&mut messages, 1024, &mut seq)
            .unwrap());
        let id = runtime
            .app
            .events()
            .iter()
            .find_map(|event| match &event.kind {
                crate::EventKind::ArtifactStored { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .expect("artifact publication event");
        assert!(messages.iter().any(|message| message.content.contains(id)));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn prepared(tools: &ToolRegistry, command: &str) -> PreparedToolInvocation {
        tools.prepare_invocation(
            crate::OperatingMode::Auto,
            std::env::temp_dir(),
            "shell",
            &json!({"command":command,"timeout_ms":10000,"yield_ms":0}).to_string(),
        )
    }
    fn invocation() -> ToolInvocation<'static> {
        ToolInvocation {
            batch_id: "batch",
            call_id: "launch",
            name: "shell",
            arguments: "{}",
        }
    }
    #[tokio::test]
    async fn status_cancel_and_completion_are_owned_and_delivered_once() {
        let jobs = ShellJobs::default();
        let tools = ToolRegistry::default();
        let call = prepared(
            &tools,
            "Write-Output 'started'; Start-Sleep -Seconds 8; Write-Output 'should-not-finish'",
        );
        let id = jobs.start(tools, call, None, invocation()).unwrap();
        assert!(jobs
            .control(&id, false)
            .unwrap()
            .output
            .contains("state=running"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !jobs.control(&id, false).unwrap().output.contains("started") {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert!(jobs.control("foreign-job", true).is_err());
        assert!(jobs
            .control(&id, true)
            .unwrap()
            .output
            .contains("state=cancelling"));
        tokio::time::timeout(Duration::from_secs(5), jobs.shutdown())
            .await
            .unwrap();
        let ready = jobs.ready();
        assert_eq!(ready.len(), 1);
        assert!(!ready[0].result.success);
        assert!(ready[0].process.as_ref().unwrap().cancelled);
        assert!(!ready[0].result.output.contains("should-not-finish"));
        assert!(jobs.inline_ready(&id).is_some());
        assert_eq!(jobs.ready().len(), 1);
        jobs.mark_delivered(&id);
        assert!(jobs.ready().is_empty());
        assert!(!jobs.running());
    }
    #[tokio::test]
    async fn parent_cancellation_and_scope_drop_stop_jobs() {
        for parent_cancel in [true, false] {
            let jobs = ShellJobs::default();
            let scope = jobs.scope();
            let parent = CancellationToken::new();
            let tools = ToolRegistry::default();
            let call = prepared(&tools, "Start-Sleep -Seconds 8");
            jobs.start(tools, call, Some(parent.clone()), invocation())
                .unwrap();
            if parent_cancel {
                parent.cancel();
            } else {
                drop(scope);
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while jobs.running() {
                    jobs.wait().await;
                }
                parent.wait_for_native_work().await;
            })
            .await
            .unwrap();
            assert!(!jobs.ready()[0].result.success);
        }
    }
    #[tokio::test]
    async fn job_limit_and_deadline_remain_enforced() {
        let jobs = ShellJobs::default();
        let _scope = jobs.scope();
        let tools = ToolRegistry::default();
        let call = prepared(&tools, "Start-Sleep -Seconds 8");
        for _ in 0..MAX_RUNNING {
            jobs.start(tools.clone(), call.clone(), None, invocation())
                .unwrap();
        }
        assert!(jobs.start(tools.clone(), call, None, invocation()).is_err());
        jobs.shutdown().await;
        let jobs = ShellJobs::default();
        let call = tools.prepare_invocation(
            crate::OperatingMode::Auto,
            std::env::temp_dir(),
            "shell",
            &json!({"command":"Start-Sleep -Seconds 8","timeout_ms":100,"yield_ms":0}).to_string(),
        );
        jobs.start(tools, call, None, invocation()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while jobs.running() {
                jobs.wait().await;
            }
        })
        .await
        .unwrap();
        let result = jobs.ready();
        assert!(result[0].process.as_ref().unwrap().timed_out);
    }
}
