//! Run-scoped shell jobs. Workers own the existing bounded process executor;
//! only the runtime publishes completion into the model conversation.
use super::*;
use std::collections::BTreeMap;
use std::sync::MutexGuard;
use std::time::Duration;

const MAX_RUNNING: usize = 4;
const MAX_RETAINED: usize = 32;
const JOB_PREFIX: &str = "shell-";
const CONTROL_HINT: &str = "Completion will be delivered automatically. Continue independent work or finish your response to wait; status is for inspection, not polling.";

/// Jobs are keyed by their serial number so iteration (delivery order) is
/// numeric; the `shell-N` spelling only exists at the boundary.
fn job_name(key: u64) -> String {
    format!("{JOB_PREFIX}{key}")
}
/// Accepts exactly the spelling `job_name` produces.
fn job_key(name: &str) -> Option<u64> {
    let key: u64 = name.strip_prefix(JOB_PREFIX)?.parse().ok()?;
    (job_name(key) == name).then_some(key)
}

#[derive(Clone, Default)]
pub(super) struct ShellJobs(Arc<JobsState>);
#[derive(Default)]
struct JobsState {
    jobs: Mutex<BTreeMap<u64, Job>>,
    serial: std::sync::atomic::AtomicU64,
    changed: Notify,
}
struct Job {
    cancellation: CancellationToken,
    started: Instant,
    preview: String,
    batch_id: String,
    call_id: String,
    delivered: bool,
    done: Option<Done>,
}
/// Outcome of a finished job. `elapsed_ms` is fixed when the worker reports,
/// so a late delivery still states how long the command really ran.
struct Done {
    result: ToolResult,
    receipt: Option<ToolExecutionReceipt>,
    elapsed_ms: u64,
}
struct Completion {
    id: u64,
    elapsed_ms: u64,
    result: ToolResult,
    process: Option<crate::process::ProcessExecutionFacts>,
    receipt: Option<ToolExecutionReceipt>,
    batch_id: String,
    call_id: String,
}
impl Job {
    fn completion(&self, id: u64) -> Option<Completion> {
        let done = self.done.as_ref()?;
        Some(Completion {
            id,
            elapsed_ms: done.elapsed_ms,
            result: done.result.clone(),
            process: done.receipt.as_ref().and_then(|r| r.process.clone()),
            receipt: done.receipt.clone(),
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
    fn jobs(&self) -> MutexGuard<'_, BTreeMap<u64, Job>> {
        crate::tools::lock_mutex(&self.0.jobs)
    }
    pub(super) fn scope(&self) -> JobScope {
        JobScope(self.clone())
    }
    pub(super) fn running(&self) -> bool {
        self.jobs().values().any(|j| j.done.is_none())
    }
    pub(super) fn progress_summary(&self) -> String {
        let mut running = 0;
        let mut latest = None;
        for (key, job) in self.jobs().iter().filter(|(_, j)| j.done.is_none()) {
            running += 1;
            latest.get_or_insert_with(|| {
                format!(
                    "{} · {}s · {}",
                    job_name(*key),
                    job.started.elapsed().as_secs(),
                    job.preview.chars().take(160).collect::<String>()
                )
            });
        }
        format!(
            "Aguardando {running} job(s) · {}",
            latest.unwrap_or_default()
        )
    }
    pub(super) fn cancel_all(&self) {
        for job in self.jobs().values() {
            if job.done.is_none() {
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
        let (key, cancellation) = self.register(invocation)?;
        // A parent cancelled before the worker exists must not race the
        // worker's first poll: the job starts already cancelled.
        if parent.as_ref().is_some_and(CancellationToken::is_cancelled) {
            cancellation.cancel();
        }
        self.spawn_worker(key, tools, prepared, parent, cancellation);
        Ok(job_name(key))
    }
    /// Enforces the running/retained limits, evicting the oldest delivered
    /// job when the table is full, and inserts the new job.
    fn register(&self, invocation: ToolInvocation<'_>) -> Result<(u64, CancellationToken), String> {
        let mut jobs = self.jobs();
        if jobs.values().filter(|j| j.done.is_none()).count() >= MAX_RUNNING {
            return Err(format!(
                "At most {MAX_RUNNING} shell jobs may run; finish or cancel an existing job first."
            ));
        }
        if jobs.len() >= MAX_RETAINED {
            let oldest = jobs
                .iter()
                .filter(|(_, j)| j.delivered && j.done.is_some())
                .min_by_key(|(_, j)| j.started)
                .map(|(key, _)| *key);
            if let Some(key) = oldest {
                jobs.remove(&key);
            }
        }
        if jobs.len() >= MAX_RETAINED {
            return Err("Shell job completion queue is full.".into());
        }
        let key = self.0.serial.fetch_add(1, Ordering::Relaxed) + 1;
        let cancellation = CancellationToken::new();
        jobs.insert(
            key,
            Job {
                cancellation: cancellation.clone(),
                started: Instant::now(),
                preview: "no output yet".into(),
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                delivered: false,
                done: None,
            },
        );
        Ok((key, cancellation))
    }
    fn spawn_worker(
        &self,
        key: u64,
        tools: ToolRegistry,
        prepared: PreparedToolInvocation,
        parent: Option<CancellationToken>,
        cancellation: CancellationToken,
    ) {
        let state = self.clone();
        let work = parent
            .as_ref()
            .map(CancellationToken::track_background_work);
        tokio::spawn(async move {
            let worker_state = state.clone();
            let worker_cancel = cancellation.clone();
            let mut task = tokio::task::spawn_blocking(move || {
                let _work = work;
                if worker_cancel.is_cancelled() {
                    return (
                        ToolResult::fail(
                            "shell",
                            "shell job cancelled before it started; command not executed",
                        ),
                        None,
                    );
                }
                let outcome = tools.execute_prepared_with_cancellation_and_progress(
                    &prepared,
                    Some(&worker_cancel),
                    |p| {
                        if let Some(job) = worker_state.jobs().get_mut(&key) {
                            job.preview = p.preview;
                        }
                    },
                );
                (outcome.result, Some(outcome.receipt))
            });
            let (result, receipt) = tokio::select! {
                result = &mut task => result,
                _ = CancellationToken::cancelled_or_pending(parent) => {
                    cancellation.cancel();
                    task.await
                }
            }
            .unwrap_or_else(|error| {
                (
                    ToolResult::fail(
                        "shell",
                        format!("shell worker failed: {error}; side effects unverified"),
                    ),
                    None,
                )
            });
            if let Some(job) = state.jobs().get_mut(&key) {
                job.done = Some(Done {
                    result,
                    receipt,
                    elapsed_ms: job.started.elapsed().as_millis() as u64,
                });
            }
            state.0.changed.notify_one();
        });
    }
    pub(super) fn control(&self, id: &str, cancel: bool) -> Result<ToolResult, String> {
        let jobs = self.jobs();
        let job = job_key(id)
            .and_then(|key| jobs.get(&key))
            .ok_or("Unknown or expired shell job in this run")?;
        if cancel && job.done.is_none() {
            job.cancellation.cancel();
        }
        if let Some(done) = &job.done {
            return Ok(done.result.clone());
        }
        Ok(ToolResult::ok(
            "shell_job",
            format!(
                "job_id={id} state={} elapsed_ms={}\n{}\n{CONTROL_HINT}",
                if job.cancellation.is_cancelled() {
                    "cancelling"
                } else {
                    "running"
                },
                job.started.elapsed().as_millis(),
                job.preview
            ),
        ))
    }
    fn ready(&self) -> Vec<Completion> {
        self.jobs()
            .iter()
            .filter(|(_, job)| !job.delivered)
            .filter_map(|(key, job)| job.completion(*key))
            .collect()
    }
    fn mark_delivered(&self, id: &str) {
        if let Some(key) = job_key(id) {
            if let Some(job) = self.jobs().get_mut(&key) {
                job.delivered = true;
            }
        }
    }
    fn inline_ready(&self, id: &str) -> Option<Completion> {
        let key = job_key(id)?;
        self.jobs().get(&key)?.completion(key)
    }
}

pub(super) fn definition() -> Value {
    json!({"name":"shell_job", "description":"Inspect or cancel a shell job in this run. Status returns latest progress/output; cancel requests process-tree termination. Completion arrives automatically: do not repeatedly poll. IDs do not survive run termination.",
        "input_schema":{"type":"object","properties":{"job_id":{"type":"string"},"action":{"type":"string","enum":["status","cancel"]}},"required":["job_id","action"],"additionalProperties":false}})
}

/// How a managed shell call ended up before its result is published.
enum Managed {
    /// Result available without waiting on a job: refusal, control or start error.
    Immediate(Result<ToolResult, String>),
    /// The job finished inside its yield window; it is delivered inline.
    Finished(Box<Completion>),
    /// The yield window elapsed; the job keeps running and completes later.
    Yielded(Result<ToolResult, String>),
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
        let managed = if !mode.allows_mutation() {
            Managed::Immediate(Err("Managed shell jobs require Auto mode".into()))
        } else if let Some(error) = &prepared.error {
            Managed::Immediate(Err(error.clone()))
        } else if invocation.name == "shell_job" {
            Managed::Immediate(self.control_from_args(invocation.arguments))
        } else {
            self.run_inline(invocation, prepared, &mut seq).await?
        };
        let mut inline_job_id = None;
        let (result, yielded) = match managed {
            Managed::Immediate(result) => (result, false),
            Managed::Finished(done) => {
                let Completion {
                    id,
                    result,
                    receipt: actual,
                    ..
                } = *done;
                if let Some(actual) = actual {
                    receipt = actual;
                }
                inline_job_id = Some(job_name(id));
                (Ok(result), false)
            }
            Managed::Yielded(result) => {
                receipt.effects_uncertain = true;
                (result, true)
            }
        };
        let mut result = result.unwrap_or_else(|error| ToolResult::fail(invocation.name, error));
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
    fn control_from_args(&self, arguments: &str) -> Result<ToolResult, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Control {
            job_id: String,
            action: Action,
        }
        #[derive(serde::Deserialize, PartialEq)]
        #[serde(rename_all = "lowercase")]
        enum Action {
            Status,
            Cancel,
        }
        match serde_json::from_str::<Control>(arguments) {
            Ok(args) => self
                .shell_jobs
                .control(&args.job_id, args.action == Action::Cancel),
            Err(_) => Err("shell_job requires job_id and action status or cancel".into()),
        }
    }
    /// Starts the job and waits for it up to its `yield_ms`.
    async fn run_inline(
        &mut self,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        seq: &mut u64,
    ) -> Result<Managed, ProviderError> {
        let id = match self.shell_jobs.start(
            self.tools.clone(),
            prepared.clone(),
            self.cancellation.clone(),
            invocation,
        ) {
            Ok(id) => id,
            Err(error) => return Ok(Managed::Immediate(Err(error))),
        };
        let yield_ms = match prepared.arguments {
            PreparedToolArguments::Shell { yield_ms, .. } => yield_ms,
            _ => 0,
        };
        let until = tokio::time::Instant::now() + Duration::from_millis(yield_ms);
        loop {
            if let Some(done) = self.shell_jobs.inline_ready(&id) {
                push_tool_result_facts(
                    &mut self.app,
                    seq,
                    &done.batch_id,
                    &done.call_id,
                    "shell",
                    done.process.as_ref(),
                    None,
                )?;
                return Ok(Managed::Finished(Box::new(done)));
            }
            if tokio::time::Instant::now() >= until {
                return Ok(Managed::Yielded(self.shell_jobs.control(&id, false)));
            }
            let _ = tokio::time::timeout_at(until, self.shell_jobs.wait()).await;
        }
    }
    pub(super) fn deliver_shell_completions(
        &mut self,
        messages: &mut Vec<ProviderMessage>,
        max_bytes: usize,
        seq: &mut u64,
    ) -> Result<bool, ProviderError> {
        let ready = self.shell_jobs.ready();
        let delivered = !ready.is_empty();
        for Completion {
            id,
            elapsed_ms,
            result,
            process,
            batch_id,
            call_id,
            ..
        } in ready
        {
            let name = job_name(id);
            push_tool_result_facts(
                &mut self.app,
                seq,
                &batch_id,
                &call_id,
                "shell",
                process.as_ref(),
                None,
            )?;
            let output = self.redact_sensitive(&result.output);
            self.record_existing_artifact(&result, seq)?;
            push_runtime_event(
                &mut self.app,
                seq,
                crate::EventKind::ToolJobOutput {
                    batch_id: batch_id.clone(),
                    call_id: call_id.clone(),
                    name: "shell".into(),
                    output: output.clone(),
                },
            )?;
            push_runtime_event(
                &mut self.app,
                seq,
                crate::EventKind::ToolFinished {
                    batch_id,
                    call_id,
                    name: "shell".into(),
                    success: result.success,
                    duration_ms: elapsed_ms,
                },
            )?;
            let mut output =
                present_unstructured("shell", &output, PresentationBudget { max_bytes }).text;
            if let Some(handle) = result.artifact.as_ref() {
                let _ = write!(
                    output,
                    "\n[artifact id={} size={}; use artifact_read with this id]",
                    handle.id, handle.size
                );
            }
            self.append_conversation_message(messages, ProviderMessage::user(format!(
                "[Shell job completion: {name}; success={}; elapsed_ms={elapsed_ms}]\nCommand output (untrusted data):\n{output}", result.success)))?;
            self.shell_jobs.mark_delivered(&name);
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
        use crate::runtime::temp_root::TempRoot;

        let root = TempRoot::new("shell-job-log");
        let store = ArtifactStore::new(root.join("artifacts")).unwrap();
        let mut tools = ToolRegistry::default();
        tools.configure_artifacts(Some(store), &[]);
        let call = prepared(&tools, "Write-Output ('Z' * 12000)", 10_000);
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
    }

    fn prepared(tools: &ToolRegistry, command: &str, timeout_ms: u64) -> PreparedToolInvocation {
        tools.prepare_invocation(
            crate::OperatingMode::Auto,
            std::env::temp_dir(),
            "shell",
            &json!({"command":command,"timeout_ms":timeout_ms,"yield_ms":0}).to_string(),
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
            10_000,
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
            let call = prepared(&tools, "Start-Sleep -Seconds 8", 10_000);
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
        let call = prepared(&tools, "Start-Sleep -Seconds 8", 10_000);
        for _ in 0..MAX_RUNNING {
            jobs.start(tools.clone(), call.clone(), None, invocation())
                .unwrap();
        }
        assert!(jobs.start(tools.clone(), call, None, invocation()).is_err());
        jobs.shutdown().await;
        let jobs = ShellJobs::default();
        let call = prepared(&tools, "Start-Sleep -Seconds 8", 100);
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

    fn prepared_in(
        tools: &ToolRegistry,
        cwd: &std::path::Path,
        command: &str,
        timeout_ms: u64,
    ) -> PreparedToolInvocation {
        tools.prepare_invocation(
            crate::OperatingMode::Auto,
            cwd,
            "shell",
            &json!({"command":command,"timeout_ms":timeout_ms,"yield_ms":0}).to_string(),
        )
    }
    fn finished_job() -> Job {
        Job {
            cancellation: CancellationToken::new(),
            started: Instant::now(),
            preview: String::new(),
            batch_id: "batch".into(),
            call_id: "call".into(),
            delivered: false,
            done: Some(Done {
                result: ToolResult::ok("shell", "done"),
                receipt: None,
                elapsed_ms: 0,
            }),
        }
    }
    #[tokio::test]
    async fn completion_reports_run_time_not_delivery_delay() {
        let jobs = ShellJobs::default();
        let tools = ToolRegistry::default();
        let call = prepared(&tools, "Write-Output done", 10_000);
        let launched = Instant::now();
        jobs.start(tools, call, None, invocation()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while jobs.running() {
                jobs.wait().await;
            }
        })
        .await
        .unwrap();
        let observed_ms = launched.elapsed().as_millis() as u64;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let ready = jobs.ready();
        assert_eq!(ready.len(), 1);
        assert!(
            ready[0].elapsed_ms <= observed_ms,
            "elapsed_ms={} must not include the {}ms delivery delay (run finished within {observed_ms}ms)",
            ready[0].elapsed_ms,
            1200
        );
    }
    #[test]
    fn ready_delivers_in_numeric_order() {
        let jobs = ShellJobs::default();
        {
            let mut map = jobs.0.jobs.lock().unwrap();
            for n in 1..=11u64 {
                map.insert(n, finished_job());
            }
        }
        let ids: Vec<String> = jobs.ready().into_iter().map(|c| job_name(c.id)).collect();
        let expected: Vec<String> = (1..=11).map(|n| format!("shell-{n}")).collect();
        assert_eq!(ids, expected);
    }
    #[tokio::test]
    async fn already_cancelled_parent_never_starts_the_command() {
        use crate::runtime::temp_root::TempRoot;

        let root = TempRoot::new("shell-job-precancel");
        let tools = ToolRegistry::default();
        let call = prepared_in(
            &tools,
            &root,
            "Set-Content -Path sentinel.txt -Value x",
            10_000,
        );
        let jobs = ShellJobs::default();
        let parent = CancellationToken::new();
        parent.cancel();
        jobs.start(tools, call, Some(parent.clone()), invocation())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while jobs.running() {
                jobs.wait().await;
            }
            parent.wait_for_native_work().await;
        })
        .await
        .unwrap();
        let ready = jobs.ready();
        assert_eq!(ready.len(), 1);
        assert!(!ready[0].result.success);
        assert!(
            ready[0].process.is_none(),
            "the command must not be spawned"
        );
        assert!(!root.join("sentinel.txt").exists());
    }

    /// Table with `count` finished jobs, oldest first, keyed 1..=count.
    fn jobs_with_finished(count: u64) -> ShellJobs {
        let jobs = ShellJobs::default();
        let base = Instant::now();
        {
            let mut map = jobs.jobs();
            for n in 1..=count {
                let mut job = finished_job();
                job.started = base + Duration::from_millis(n);
                map.insert(n, job);
            }
        }
        jobs.0.serial.store(count, Ordering::Relaxed);
        jobs
    }
    #[test]
    fn retained_limit_evicts_oldest_delivered_job_then_reports_full_queue() {
        let jobs = jobs_with_finished(MAX_RETAINED as u64);
        // Nothing delivered yet: no job may be evicted, the queue is full.
        assert_eq!(
            jobs.register(invocation()).err().as_deref(),
            Some("Shell job completion queue is full.")
        );
        assert_eq!(jobs.jobs().len(), MAX_RETAINED);
        // Delivered jobs are evictable, oldest first; undelivered ones stay.
        jobs.mark_delivered("shell-9");
        jobs.mark_delivered("shell-5");
        let (key, _) = jobs.register(invocation()).unwrap();
        assert_eq!(key, MAX_RETAINED as u64 + 1);
        let table = jobs.jobs();
        assert_eq!(table.len(), MAX_RETAINED);
        assert!(!table.contains_key(&5), "oldest delivered job is evicted");
        assert!(table.contains_key(&9) && table.contains_key(&key));
        drop(table);
        // The next registration evicts the remaining delivered job; after
        // that nothing is evictable and the queue is full again.
        jobs.register(invocation()).unwrap();
        assert!(!jobs.jobs().contains_key(&9));
        assert_eq!(jobs.jobs().len(), MAX_RETAINED);
        assert_eq!(
            jobs.register(invocation()).err().as_deref(),
            Some("Shell job completion queue is full.")
        );
    }
    #[test]
    fn running_limit_message_keeps_its_text() {
        let jobs = ShellJobs::default();
        {
            let mut map = jobs.jobs();
            for n in 1..=MAX_RUNNING as u64 {
                let mut job = finished_job();
                job.done = None;
                map.insert(n, job);
            }
        }
        assert_eq!(
            jobs.register(invocation()).err().as_deref(),
            Some("At most 4 shell jobs may run; finish or cancel an existing job first.")
        );
    }
    #[test]
    fn control_on_a_finished_job_returns_its_result_for_status_and_cancel() {
        let jobs = jobs_with_finished(7);
        for cancel in [false, true] {
            let result = jobs.control("shell-7", cancel).unwrap();
            assert!(result.success);
            assert_eq!(result.output, "done");
        }
        assert!(!jobs.jobs()[&7].cancellation.is_cancelled());
        // Only the exact `shell-N` spelling names a job.
        for id in ["7", "shell-07", "shell-+7", "shell-8", "shell-", "shell-7 "] {
            assert!(jobs.control(id, false).is_err(), "{id} must be unknown");
        }
    }
}
