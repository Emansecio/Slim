//! Concurrent dispatch of a cell's tool calls. Snapshot reads, code_intel and
//! MCP calls run in parallel, up to the scheduler's limit
//! ([`MAX_PARALLEL_CALLS`] in Code mode; Auto keeps one at a time); native
//! mutations (write, patch, shell) run alone: they start once every call in
//! flight has finished, and nothing starts while they run. Calls start in
//! request order. Each call runs on its own task, owning what it needs, so
//! the host keeps answering the program while calls are in flight.

use std::collections::{HashMap, VecDeque};

use futures_util::future::BoxFuture;
use tokio::task::{JoinError, JoinSet};

use super::*;

pub(super) const MAX_PARALLEL_CALLS: usize = 8;

/// A call the program asked for, not yet started.
pub(super) struct Job {
    pub ticket: u32,
    /// As the program named it: `mcp.server.tool` or `slim.<tool>`.
    pub name: String,
    /// Tool-budget slots the call uses, as `tool_call_slots` counts them.
    pub slots: usize,
    pub target: Target,
}

pub(super) enum Target {
    Mcp {
        manager: Arc<McpManager>,
        server: String,
        tool: String,
        arguments: Value,
    },
    /// Prepared at request time; run guards admit it when it starts.
    Native {
        prepared: Box<PreparedToolInvocation>,
        /// As the program sent them, for the journal.
        arguments: Value,
        exclusive: bool,
    },
}

impl Job {
    fn exclusive(&self) -> bool {
        matches!(
            self.target,
            Target::Native {
                exclusive: true,
                ..
            }
        )
    }
}

/// A started call, as the host settles it.
pub(super) struct Running {
    pub ticket: u32,
    /// Journal key of the call.
    pub id: String,
    pub name: String,
    pub prepared: Option<Arc<PreparedToolInvocation>>,
    pub started: Instant,
    exclusive: bool,
}

pub(super) enum Finished {
    Mcp(McpRequestOutcome<Value>),
    Native(Box<ToolExecutionOutcome>),
}

pub(super) struct Done {
    pub running: Running,
    pub finished: Result<Finished, JoinError>,
}

/// Handles the call tasks share.
pub(super) struct CallEnv {
    pub tools: ToolRegistry,
    pub code_intel: Option<Arc<dyn CodeIntelligence>>,
    /// The cell's token: cancels native work and, through its MCP handle,
    /// MCP calls.
    pub cell: CancellationToken,
    /// Owns the native work until the blocking task really stops.
    pub run: CancellationToken,
}

pub(super) struct Scheduler {
    env: CallEnv,
    queue: VecDeque<Job>,
    tasks: JoinSet<Finished>,
    running: HashMap<tokio::task::Id, Running>,
    exclusive_running: bool,
    /// Calls in flight at once.
    limit: usize,
}

impl Scheduler {
    pub fn new(env: CallEnv, limit: usize) -> Self {
        Self {
            env,
            queue: VecDeque::new(),
            tasks: JoinSet::new(),
            running: HashMap::new(),
            exclusive_running: false,
            limit: limit.max(1),
        }
    }

    pub fn push(&mut self, job: Job) {
        self.queue.push_back(job);
    }

    /// The next queued call that may start now, in request order: a
    /// mutation waits for the calls in flight, and blocks everything behind
    /// it while it runs.
    pub fn next_startable(&mut self) -> Option<Job> {
        let head = self.queue.front()?;
        let free = !self.exclusive_running
            && self.running.len() < self.limit
            && (!head.exclusive() || self.running.is_empty());
        if free {
            self.queue.pop_front()
        } else {
            None
        }
    }

    /// Starts `job` on its own task; `id` keys its journal facts.
    pub fn spawn(&mut self, id: String, job: Job) {
        let exclusive = job.exclusive();
        let (prepared, call): (_, BoxFuture<'static, Finished>) = match job.target {
            Target::Mcp {
                manager,
                server,
                tool,
                arguments,
            } => {
                let cancellation = self.env.cell.mcp_cancellation();
                let call = Box::pin(async move {
                    Finished::Mcp(
                        manager
                            .call_cancellable(&server, &tool, arguments, cancellation)
                            .await,
                    )
                });
                (None, call)
            }
            Target::Native { prepared, .. } => {
                let prepared = Arc::from(prepared);
                let call = Box::pin(run_native(
                    self.env.tools.clone(),
                    self.env.code_intel.clone(),
                    self.env.cell.clone(),
                    self.env.run.clone(),
                    Arc::clone(&prepared),
                ));
                (Some(prepared), call)
            }
        };
        let task = self.tasks.spawn(call);
        self.exclusive_running = exclusive;
        self.running.insert(
            task.id(),
            Running {
                ticket: job.ticket,
                id,
                name: job.name,
                prepared,
                started: Instant::now(),
                exclusive,
            },
        );
    }

    /// The next call to finish; `None` when nothing runs.
    pub async fn next_finished(&mut self) -> Option<Done> {
        let (id, finished) = match self.tasks.join_next_with_id().await? {
            Ok((id, finished)) => (id, Ok(finished)),
            Err(error) => (error.id(), Err(error)),
        };
        let running = self
            .running
            .remove(&id)
            .expect("every call task is registered when spawned");
        if running.exclusive {
            self.exclusive_running = false;
        }
        Some(Done { running, finished })
    }

    /// Drops the calls that never started.
    pub fn discard_queued(&mut self) {
        self.queue.clear();
    }
}

/// Runs a native tool: code_intel on its async backend, the rest on the
/// blocking pool with the cell's token.
async fn run_native(
    tools: ToolRegistry,
    code_intel: Option<Arc<dyn CodeIntelligence>>,
    cell: CancellationToken,
    run: CancellationToken,
    prepared: Arc<PreparedToolInvocation>,
) -> Finished {
    let started = Instant::now();
    let revision_before = tools.workspace_revision();
    let mut outcome = if prepared.name == "code_intel" {
        let (mut result, presentation) = run_code_intel_request(
            code_intel,
            prepared_code_intel_request(&prepared),
            Some(cell.clone()),
        )
        .await;
        super::super::native_code_intel::admit_code_intel_output(
            &mut result,
            &prepared,
            presentation,
        );
        ToolExecutionOutcome {
            result,
            receipt: ToolExecutionReceipt::unobserved(
                revision_before,
                tools.workspace_revision(),
                elapsed_micros(started),
            ),
        }
    } else {
        let work = run.track_native_work();
        let blocking_tools = tools.clone();
        let blocking_cell = cell.clone();
        tokio::task::spawn_blocking(move || {
            let _work = work;
            blocking_tools.execute_prepared_with_cancellation_and_progress(
                &prepared,
                Some(&blocking_cell),
                |_| {},
            )
        })
        .await
        .unwrap_or_else(|error| ToolExecutionOutcome {
            result: ToolResult::fail("codemode", format!("native tool task failed: {error}")),
            receipt: ToolExecutionReceipt::unobserved(
                revision_before,
                tools.workspace_revision(),
                elapsed_micros(started),
            ),
        })
    };
    if cell.is_cancelled() {
        outcome.result.success = false;
    }
    Finished::Native(Box::new(outcome))
}
