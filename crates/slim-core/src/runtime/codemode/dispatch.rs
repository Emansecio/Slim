//! Concurrent dispatch of a cell's tool calls. Snapshot reads, glob,
//! code_intel and MCP calls run in parallel, up to the scheduler's limit
//! ([`MAX_PARALLEL_CALLS`] in Code mode; Auto keeps one at a time); native
//! file mutations on distinct paths run in parallel waves once prior reads
//! finish; `shell`, `then_run` mutations, and same-path mutations wait their
//! turn and run alone. Calls start in request order.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio::task::{JoinError, JoinSet};

use super::*;
use crate::workspace_files::{glob_workspace_files, GlobListing};

pub(super) const MAX_PARALLEL_CALLS: usize = 12;

#[derive(Clone, Debug)]
pub(super) struct ReadManySpec {
    pub raw_path: String,
    pub offset: usize,
    pub max_lines: Option<usize>,
    pub outline: bool,
    pub symbols: Vec<String>,
    pub ranges: Vec<(usize, usize)>,
    pub raw: bool,
}

#[derive(Clone, Debug)]
pub(super) enum OutlineManyInput {
    Paths(Vec<String>),
    Glob {
        subpath: Option<String>,
        pattern: String,
        max_files: usize,
    },
}

#[derive(Clone, Debug)]
pub(super) enum Concurrency {
    Shared {
        coalesce_key: Option<String>,
        read_paths: Vec<String>,
    },
    FileMutation { path_key: String },
    Exclusive,
}

/// A call the program asked for, not yet started.
pub(super) struct Job {
    pub ticket: u32,
    /// As the program named it: `mcp.server.tool` or `slim.<tool>`.
    pub name: String,
    /// Tool-budget slots the call uses, as `tool_call_slots` counts them.
    pub slots: usize,
    pub allow_non_zero: bool,
    pub structured: bool,
    pub raw_arguments: String,
    pub target: Target,
}

pub(super) enum Target {
    Mcp {
        manager: Arc<McpManager>,
        server: String,
        tool: String,
        arguments: Value,
        read_only: bool,
    },
    /// Prepared at request time; run guards admit it when it starts.
    Native {
        prepared: Box<PreparedToolInvocation>,
        /// As the program sent them, for the journal.
        arguments: Value,
        concurrency: Concurrency,
        read_only_override: bool,
    },
    Glob {
        path: Option<String>,
        pattern: Option<String>,
        max_entries: usize,
        workspace: PathBuf,
        fingerprint: String,
        arguments: Value,
    },
    Git {
        op: String,
        path: Option<String>,
        staged: bool,
        limit: usize,
        rev: Option<String>,
        workspace: PathBuf,
        fingerprint: String,
        arguments: Value,
    },
    ReadMany {
        specs: Vec<ReadManySpec>,
        structured: bool,
        workspace: PathBuf,
        read_paths: Vec<String>,
        fingerprint: String,
        arguments: Value,
    },
    OutlineMany {
        input: OutlineManyInput,
        max_symbols: usize,
        workspace: PathBuf,
        fingerprint: String,
        arguments: Value,
    },
    GrepIndex {
        queries: Vec<String>,
        case_insensitive: bool,
        path: Option<String>,
        max_hits: usize,
        workspace: PathBuf,
        fingerprint: String,
        arguments: Value,
    },
    Graph {
        op: String,
        symbol: Option<String>,
        path: Option<String>,
        depth: usize,
        limit: usize,
        workspace: PathBuf,
        fingerprint: String,
        arguments: Value,
    },
    Subagent {
        req: Box<SubagentSpawnRequest>,
        background: bool,
        registry: SubagentRegistry,
        parent_cancel: Option<CancellationToken>,
        steer_inbox: Option<SteerInbox>,
        arguments: Value,
    },
    SubagentControl {
        input: native_subagent::SubagentControlInput,
        registry: SubagentRegistry,
        parent_cancel: Option<CancellationToken>,
        steer_inbox: Option<SteerInbox>,
        arguments: Value,
    },
}

impl Job {
    fn concurrency(&self) -> Concurrency {
        match &self.target {
            Target::Mcp { .. } => Concurrency::Shared {
                coalesce_key: None,
                read_paths: Vec::new(),
            },
            Target::Native { concurrency, .. } => concurrency.clone(),
            Target::Glob { fingerprint, .. }
            | Target::Git { fingerprint, .. }
            | Target::OutlineMany { fingerprint, .. }
            | Target::GrepIndex { fingerprint, .. }
            | Target::Graph { fingerprint, .. } => Concurrency::Shared {
                coalesce_key: Some(fingerprint.clone()),
                read_paths: Vec::new(),
            },
            Target::ReadMany {
                read_paths,
                fingerprint,
                ..
            } => Concurrency::Shared {
                coalesce_key: Some(fingerprint.clone()),
                read_paths: read_paths.clone(),
            },
            Target::Subagent { req, .. } if !req.readonly => Concurrency::Exclusive,
            Target::Subagent { .. } | Target::SubagentControl { .. } => Concurrency::Shared {
                coalesce_key: None,
                read_paths: Vec::new(),
            },
        }
    }

    pub(super) fn is_read_only(&self) -> bool {
        match &self.target {
            Target::Glob { .. }
            | Target::Git { .. }
            | Target::ReadMany { .. }
            | Target::OutlineMany { .. }
            | Target::GrepIndex { .. }
            | Target::Graph { .. } => true,
            Target::Native {
                prepared,
                read_only_override,
                ..
            } => *read_only_override || tool_call_is_read_only(&prepared.name),
            Target::Mcp { read_only, .. } => *read_only,
            Target::Subagent { req, .. } => req.readonly,
            Target::SubagentControl { input, .. } => {
                matches!(input.action.as_str(), "status" | "wait")
            }
        }
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
    pub allow_non_zero: bool,
    pub structured: bool,
    pub is_read_only: bool,
    pub from_cache: bool,
    pub effect_target: Option<String>,
    pub mutation_epoch: u64,
    pub global_mutation_epoch: u64,
    pub daemon_epoch: (u64, u64, u64),
    pub governor_pending: Option<super::governor::PendingCall>,
    pub cache_key: Option<String>,
    concurrency: Concurrency,
}

pub(super) enum Finished {
    Mcp(McpRequestOutcome<Value>),
    Native(Arc<ToolExecutionOutcome>),
    Glob {
        fingerprint: String,
        result: Arc<Result<GlobListing, String>>,
    },
    Git {
        fingerprint: String,
        result: Result<Arc<Value>, String>,
    },
    JsonValue {
        fingerprint: String,
        value: Result<Arc<Value>, String>,
        deps: Vec<PathBuf>,
        cacheable: bool,
    },
    Subagent(Result<String, String>),
}

impl Finished {
    pub(super) fn clone_for_cache(&self) -> Option<Self> {
        match self {
            Self::Native(outcome) => Some(Self::Native(Arc::clone(outcome))),
            Self::Glob {
                fingerprint,
                result,
            } => Some(Self::Glob {
                fingerprint: fingerprint.clone(),
                result: Arc::clone(result),
            }),
            Self::Git {
                fingerprint,
                result,
            } => Some(Self::Git {
                fingerprint: fingerprint.clone(),
                result: result.clone(),
            }),
            Self::JsonValue {
                fingerprint,
                value,
                deps,
                cacheable,
            } if *cacheable => Some(Self::JsonValue {
                fingerprint: fingerprint.clone(),
                value: value.clone(),
                deps: deps.clone(),
                cacheable: true,
            }),
            Self::JsonValue { .. } | Self::Mcp(_) | Self::Subagent(_) => None,
        }
    }
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
    shared_unbounded_running: usize,
    shared_read_paths: HashMap<String, usize>,
    exclusive_running: bool,
    mutating_paths: HashSet<String>,
    in_flight_coalesced: HashMap<String, Vec<Running>>,
    ready_coalesced: VecDeque<Done>,
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
            shared_unbounded_running: 0,
            shared_read_paths: HashMap::new(),
            exclusive_running: false,
            mutating_paths: HashSet::new(),
            in_flight_coalesced: HashMap::new(),
            ready_coalesced: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    pub fn push(&mut self, job: Job) {
        self.queue.push_back(job);
    }

    /// The next queued call that may start now, without head-of-line blocking
    /// for disjoint paths.
    pub fn next_startable(&mut self) -> Option<Job> {
        if self.queue.is_empty() || self.exclusive_running {
            return None;
        }
        let active = self.running.len();
        let mut skipped_mutations = HashSet::<String>::new();
        let mut skipped_shared_paths = HashSet::<String>::new();
        let mut skipped_unbounded_read = false;

        for idx in 0..self.queue.len() {
            match self.queue[idx].concurrency() {
                Concurrency::Exclusive => {
                    if idx == 0
                        && active == 0
                        && self.ready_coalesced.is_empty()
                        && self.in_flight_coalesced.values().all(Vec::is_empty)
                    {
                        return self.queue.pop_front();
                    }
                    return None;
                }
                Concurrency::Shared {
                    coalesce_key,
                    read_paths,
                } => {
                    let can_coalesce = coalesce_key
                        .as_deref()
                        .is_some_and(|key| self.in_flight_coalesced.contains_key(key));
                    if !read_paths.is_empty() {
                        let blocked = read_paths.iter().any(|path| {
                            self.mutating_paths.contains(path) || skipped_mutations.contains(path)
                        });
                        if !blocked && (active < self.limit || can_coalesce) {
                            return self.queue.remove(idx);
                        }
                        skipped_shared_paths.extend(read_paths);
                    } else {
                        let blocked = !self.mutating_paths.is_empty() || !skipped_mutations.is_empty();
                        if !blocked && (active < self.limit || can_coalesce) {
                            return self.queue.remove(idx);
                        }
                        skipped_unbounded_read = true;
                    }
                }
                Concurrency::FileMutation { path_key } => {
                    let runnable = !skipped_unbounded_read
                        && active < self.limit
                        && self.shared_unbounded_running == 0
                        && self.ready_coalesced.is_empty()
                        && !self.mutating_paths.contains(&path_key)
                        && !self.shared_read_paths.contains_key(&path_key)
                        && !skipped_mutations.contains(&path_key)
                        && !skipped_shared_paths.contains(&path_key);
                    if runnable {
                        return self.queue.remove(idx);
                    }
                    skipped_mutations.insert(path_key);
                }
            }
        }
        None
    }

    /// Starts `job` on its own task (or coalesces onto an identical in-flight
    /// snapshot read); `id` keys its journal facts.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &mut self,
        id: String,
        job: Job,
        is_read_only: bool,
        effect_target: Option<String>,
        mutation_epoch: u64,
        global_mutation_epoch: u64,
        daemon_epoch: (u64, u64, u64),
        governor_pending: Option<super::governor::PendingCall>,
    ) {
        let concurrency = job.concurrency();
        let cache_key = match &concurrency {
            Concurrency::Shared { coalesce_key, .. } => coalesce_key.clone(),
            _ => None,
        };
        if let Concurrency::Shared {
            coalesce_key: Some(key),
            ..
        } = &concurrency
        {
            if let Some(followers) = self.in_flight_coalesced.get_mut(key) {
                let prepared = match &job.target {
                    Target::Native { prepared, .. } => Some(Arc::from(prepared.clone())),
                    _ => None,
                };
                followers.push(Running {
                    ticket: job.ticket,
                    id,
                    name: job.name,
                    prepared,
                    started: Instant::now(),
                    allow_non_zero: job.allow_non_zero,
                    structured: job.structured,
                    is_read_only,
                    from_cache: true,
                    effect_target,
                    mutation_epoch,
                    global_mutation_epoch,
                    daemon_epoch,
                    governor_pending,
                    cache_key,
                    concurrency,
                });
                return;
            }
        }
        let (prepared, call): (_, BoxFuture<'static, Finished>) = match job.target {
            Target::Mcp {
                manager,
                server,
                tool,
                arguments,
                ..
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
            Target::Glob {
                path,
                pattern,
                max_entries,
                workspace,
                fingerprint,
                ..
            } => {
                let cell = self.env.cell.clone();
                let work = self.env.run.track_native_work();
                let call = Box::pin(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let _work = work;
                        glob_workspace_files(
                            &workspace,
                            path.as_deref(),
                            pattern.as_deref(),
                            max_entries,
                            Some(&cell),
                        )
                    })
                    .await
                    .unwrap_or_else(|error| Err(format!("glob task failed: {error}")));
                    Finished::Glob {
                        fingerprint,
                        result: Arc::new(result),
                    }
                });
                (None, call)
            }
            Target::Git {
                op,
                path,
                staged,
                limit,
                rev,
                workspace,
                fingerprint,
                ..
            } => {
                let call = Box::pin(async move {
                    let result = run_git_structured(
                        &workspace,
                        &op,
                        path.as_deref(),
                        staged,
                        limit,
                        rev.as_deref(),
                    )
                    .await;
                    Finished::Git {
                        fingerprint,
                        result: result.map(Arc::new),
                    }
                });
                (None, call)
            }
            Target::ReadMany {
                specs,
                structured,
                workspace,
                fingerprint,
                ..
            } => {
                let tools = self.env.tools.clone();
                let cell = self.env.cell.clone();
                let work = self.env.run.track_native_work();
                let call = Box::pin(async move {
                    let (value, deps) = tokio::task::spawn_blocking(move || {
                        let _work = work;
                        run_read_many_batch(&tools, &workspace, &specs, structured, Some(&cell))
                    })
                    .await
                    .unwrap_or_else(|e| (Err(format!("readMany task failed: {e}")), Vec::new()));
                    Finished::JsonValue {
                        fingerprint,
                        cacheable: value.is_ok(),
                        value: value.map(Arc::new),
                        deps,
                    }
                });
                (None, call)
            }
            Target::OutlineMany {
                input,
                max_symbols,
                workspace,
                fingerprint,
                ..
            } => {
                let cell = self.env.cell.clone();
                let work = self.env.run.track_native_work();
                let call = Box::pin(async move {
                    let (value, deps) = tokio::task::spawn_blocking(move || {
                        let _work = work;
                        run_outline_many_batch(&workspace, &input, max_symbols, Some(&cell))
                    })
                    .await
                    .unwrap_or_else(|e| (Err(format!("outlineMany task failed: {e}")), Vec::new()));
                    Finished::JsonValue {
                        fingerprint,
                        cacheable: value.is_ok(),
                        value: value.map(Arc::new),
                        deps,
                    }
                });
                (None, call)
            }
            Target::GrepIndex {
                queries,
                case_insensitive,
                path,
                max_hits,
                workspace,
                fingerprint,
                ..
            } => {
                let cell = self.env.cell.clone();
                let work = self.env.run.track_native_work();
                let call = Box::pin(async move {
                    let value = tokio::task::spawn_blocking(move || {
                        let _work = work;
                        run_grep_index_batch(
                            &workspace,
                            &queries,
                            case_insensitive,
                            path.as_deref(),
                            max_hits,
                            Some(&cell),
                        )
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("grepIndex task failed: {e}")));
                    Finished::JsonValue {
                        fingerprint,
                        cacheable: value.is_ok(),
                        value: value.map(Arc::new),
                        deps: Vec::new(),
                    }
                });
                (None, call)
            }
            Target::Graph {
                op,
                symbol,
                path,
                depth,
                limit,
                workspace,
                fingerprint,
                ..
            } => {
                let work = self.env.run.track_native_work();
                let call = Box::pin(async move {
                    let (value, ready) = tokio::task::spawn_blocking(move || {
                        let _work = work;
                        run_graph_query(
                            &workspace,
                            &op,
                            symbol.as_deref(),
                            path.as_deref(),
                            depth,
                            limit,
                        )
                    })
                    .await
                    .unwrap_or_else(|e| (Err(format!("graph task failed: {e}")), false));
                    Finished::JsonValue {
                        fingerprint,
                        cacheable: ready && value.is_ok(),
                        value: value.map(Arc::new),
                        deps: Vec::new(),
                    }
                });
                (None, call)
            }
            Target::Subagent {
                req,
                background,
                registry,
                parent_cancel,
                steer_inbox,
                ..
            } => {
                let call = Box::pin(async move {
                    Finished::Subagent(
                        native_subagent::execute_subagent_request(
                            &registry,
                            *req,
                            background,
                            parent_cancel.as_ref(),
                            steer_inbox.as_ref(),
                        )
                        .await,
                    )
                });
                (None, call)
            }
            Target::SubagentControl {
                input,
                registry,
                parent_cancel,
                steer_inbox,
                ..
            } => {
                let call = Box::pin(async move {
                    Finished::Subagent(
                        native_subagent::execute_subagent_control_action(
                            &registry,
                            input,
                            parent_cancel,
                            steer_inbox.as_ref(),
                        )
                        .await,
                    )
                });
                (None, call)
            }
        };
        let task = self.tasks.spawn(call);
        match &concurrency {
            Concurrency::Shared {
                coalesce_key,
                read_paths,
            } => {
                if !read_paths.is_empty() {
                    for path in read_paths {
                        *self.shared_read_paths.entry(path.clone()).or_insert(0) += 1;
                    }
                } else {
                    self.shared_unbounded_running += 1;
                }
                if let Some(key) = coalesce_key {
                    self.in_flight_coalesced.insert(key.clone(), Vec::new());
                }
            }
            Concurrency::FileMutation { path_key } => {
                self.mutating_paths.insert(path_key.clone());
            }
            Concurrency::Exclusive => {
                self.exclusive_running = true;
            }
        }
        self.running.insert(
            task.id(),
            Running {
                ticket: job.ticket,
                id,
                name: job.name,
                prepared,
                started: Instant::now(),
                allow_non_zero: job.allow_non_zero,
                structured: job.structured,
                is_read_only,
                from_cache: false,
                effect_target,
                mutation_epoch,
                global_mutation_epoch,
                daemon_epoch,
                governor_pending,
                cache_key,
                concurrency,
            },
        );
    }

    /// Registers an intra-cell read cache hit so it settles on the next poll
    /// without touching the blocking pool.
    #[allow(clippy::too_many_arguments)]
    pub fn push_cached(
        &mut self,
        id: String,
        job: Job,
        is_read_only: bool,
        effect_target: Option<String>,
        prepared: Option<Arc<PreparedToolInvocation>>,
        mutation_epoch: u64,
        global_mutation_epoch: u64,
        daemon_epoch: (u64, u64, u64),
        governor_pending: Option<super::governor::PendingCall>,
        finished: Finished,
    ) {
        let concurrency = job.concurrency();
        let cache_key = match &concurrency {
            Concurrency::Shared { coalesce_key, .. } => coalesce_key.clone(),
            _ => None,
        };
        self.ready_coalesced.push_back(Done {
            running: Running {
                ticket: job.ticket,
                id,
                name: job.name,
                prepared,
                started: Instant::now(),
                allow_non_zero: job.allow_non_zero,
                structured: job.structured,
                is_read_only,
                from_cache: true,
                effect_target,
                mutation_epoch,
                global_mutation_epoch,
                daemon_epoch,
                governor_pending,
                cache_key,
                concurrency,
            },
            finished: Ok(finished),
        });
    }

    /// The next call to finish; `None` when nothing runs.
    pub async fn next_finished(&mut self) -> Option<Done> {
        if let Some(done) = self.ready_coalesced.pop_front() {
            return Some(done);
        }
        let (id, finished) = match self.tasks.join_next_with_id().await? {
            Ok((id, finished)) => (id, Ok(finished)),
            Err(error) => (error.id(), Err(error)),
        };
        let running = self
            .running
            .remove(&id)
            .expect("every call task is registered when spawned");
        match &running.concurrency {
            Concurrency::Shared {
                coalesce_key,
                read_paths,
            } => {
                if !read_paths.is_empty() {
                    for path in read_paths {
                        if let Some(count) = self.shared_read_paths.get_mut(path) {
                            *count = count.saturating_sub(1);
                            if *count == 0 {
                                self.shared_read_paths.remove(path);
                            }
                        }
                    }
                } else {
                    self.shared_unbounded_running =
                        self.shared_unbounded_running.saturating_sub(1);
                }
                if let Some(key) = coalesce_key {
                    if let Some(followers) = self.in_flight_coalesced.remove(key) {
                        for follower in followers {
                            let cloned = clone_coalesced_finished(&follower, &finished);
                            self.ready_coalesced.push_back(Done {
                                running: follower,
                                finished: Ok(cloned),
                            });
                        }
                    }
                }
            }
            Concurrency::FileMutation { path_key } => {
                self.mutating_paths.remove(path_key);
            }
            Concurrency::Exclusive => {
                self.exclusive_running = false;
            }
        }
        Some(Done { running, finished })
    }

    /// Drops the calls that never started.
    pub fn discard_queued(&mut self) {
        self.queue.clear();
    }
}

fn clone_coalesced_finished(
    follower: &Running,
    leader: &Result<Finished, JoinError>,
) -> Finished {
    match leader {
        Ok(finished) => {
            if let Some(cloned) = finished.clone_for_cache() {
                return cloned;
            }
            match finished {
                Finished::JsonValue {
                    fingerprint,
                    value,
                    deps,
                    cacheable,
                } => Finished::JsonValue {
                    fingerprint: fingerprint.clone(),
                    value: value.clone(),
                    deps: deps.clone(),
                    cacheable: *cacheable,
                },
                Finished::Subagent(res) => Finished::Subagent(res.clone()),
                Finished::Mcp(McpRequestOutcome::Completed(Err(err))) => Finished::JsonValue {
                    fingerprint: String::new(),
                    value: Err(err.to_string()),
                    deps: Vec::new(),
                    cacheable: false,
                },
                _ => Finished::JsonValue {
                    fingerprint: String::new(),
                    value: Err("coalesced call failed".to_owned()),
                    deps: Vec::new(),
                    cacheable: false,
                },
            }
        }
        Err(error) => Finished::Native(Arc::new(ToolExecutionOutcome {
            result: ToolResult::fail(
                follower.name.clone(),
                format!("{} task failed: {error}", follower.name),
            ),
            receipt: ToolExecutionReceipt::unobserved(0, 0, 0),
        })),
    }
}

async fn run_git_checked_json(workspace: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(workspace).args(args).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| format!("git {:?} timed out after 10s", args))?
        .map_err(|err| format!("failed to run git: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(if detail.is_empty() {
            format!("git {:?} exited with {:?}", args, output.status.code())
        } else {
            detail
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn run_git_structured(
    workspace: &Path,
    op: &str,
    path: Option<&str>,
    staged: bool,
    limit: usize,
    rev: Option<&str>,
) -> Result<Value, String> {
    match op {
        "status" => {
            let mut args = vec!["status", "--porcelain=v1", "-b"];
            if let Some(p) = path {
                args.push("--");
                args.push(p);
            }
            let raw = run_git_checked_json(workspace, &args).await?;
            let mut branch = String::new();
            let mut entries = Vec::new();
            for line in raw.lines() {
                if let Some(rest) = line.strip_prefix("## ") {
                    branch = rest.to_owned();
                } else if line.len() >= 3 {
                    entries.push(serde_json::json!({
                        "status": line[..2].trim(),
                        "path": line[3..].trim(),
                    }));
                }
            }
            Ok(serde_json::json!({
                "branch": branch,
                "clean": entries.is_empty(),
                "entries": entries,
            }))
        }
        "diff" => {
            let mut args = vec!["diff"];
            if staged {
                args.push("--staged");
            }
            if let Some(p) = path {
                args.push("--");
                args.push(p);
            }
            let raw = run_git_checked_json(workspace, &args).await?;
            let max_bytes = 128 * 1024;
            let truncated = raw.len() > max_bytes;
            let diff = if truncated {
                let mut end = max_bytes;
                while end > 0 && !raw.is_char_boundary(end) {
                    end -= 1;
                }
                raw[..end].to_owned()
            } else {
                raw
            };
            Ok(serde_json::json!({
                "diff": diff,
                "staged": staged,
                "truncated": truncated,
            }))
        }
        "log" => {
            let count = limit.clamp(1, 50).to_string();
            let mut args = vec![
                "log",
                "-n",
                &count,
                "--format=%H%x1f%h%x1f%an%x1f%aI%x1f%s",
            ];
            if let Some(p) = path {
                args.push("--");
                args.push(p);
            }
            let raw = run_git_checked_json(workspace, &args).await?;
            let commits: Vec<Value> = raw
                .lines()
                .filter_map(|line| {
                    let mut cols = line.split('\x1f');
                    Some(serde_json::json!({
                        "hash": cols.next()?,
                        "short_hash": cols.next()?,
                        "author": cols.next()?,
                        "date": cols.next()?,
                        "subject": cols.next()?,
                    }))
                })
                .collect();
            Ok(serde_json::json!({ "commits": commits }))
        }
        "branch" => {
            let raw = run_git_checked_json(
                workspace,
                &["branch", "--format=%(HEAD)%09%(refname:short)"],
            )
            .await?;
            let mut current = String::new();
            let mut branches = Vec::new();
            for line in raw.lines() {
                if let Some((head, name)) = line.split_once('\t') {
                    let branch_name = name.trim();
                    if !branch_name.is_empty() {
                        if head.trim() == "*" {
                            current = branch_name.to_owned();
                        }
                        branches.push(branch_name);
                    }
                }
            }
            Ok(serde_json::json!({
                "current": current,
                "branches": branches,
            }))
        }
        "show" => {
            let target = rev.unwrap_or("HEAD");
            let mut args = vec!["show", "--stat", target];
            if let Some(p) = path {
                args.push("--");
                args.push(p);
            }
            let raw = run_git_checked_json(workspace, &args).await?;
            let max_bytes = 128 * 1024;
            let truncated = raw.len() > max_bytes;
            let output = if truncated {
                let mut end = max_bytes;
                while end > 0 && !raw.is_char_boundary(end) {
                    end -= 1;
                }
                raw[..end].to_owned()
            } else {
                raw
            };
            Ok(serde_json::json!({
                "rev": target,
                "output": output,
                "truncated": truncated,
            }))
        }
        other => Err(format!("unsupported git op `{other}`")),
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
    Finished::Native(Arc::new(outcome))
}

pub(super) fn contains_ascii_ci(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    haystack
        .windows(needle_lower.len())
        .any(|window| window.eq_ignore_ascii_case(needle_lower))
}

fn read_one_spec(
    tools: &ToolRegistry,
    workspace: &Path,
    spec: &ReadManySpec,
    default_structured: bool,
    cell: Option<&CancellationToken>,
) -> Result<(String, Value, Vec<PathBuf>), String> {
    if cell.is_some_and(CancellationToken::is_cancelled) {
        return Err("readMany cancelled".into());
    }
    let mut args = serde_json::json!({ "path": spec.raw_path });
    if spec.offset > 1 {
        args["offset"] = Value::from(spec.offset);
    }
    if let Some(max_lines) = spec.max_lines {
        args["max_lines"] = Value::from(max_lines);
    }
    if spec.outline {
        args["outline"] = Value::Bool(true);
    }
    if !spec.symbols.is_empty() {
        args["symbols"] = Value::from(spec.symbols.clone());
    }
    if !spec.ranges.is_empty() {
        let ranges: Vec<Value> = spec
            .ranges
            .iter()
            .map(|(a, b)| Value::Array(vec![Value::from(*a), Value::from(*b)]))
            .collect();
        args["ranges"] = Value::Array(ranges);
    }
    if spec.raw {
        args["raw"] = Value::Bool(true);
    }
    let prepared = tools.prepare_invocation_value(
        crate::OperatingMode::Code,
        workspace,
        &Ok(workspace.to_path_buf()),
        "read",
        args,
    );
    if let Some(err) = &prepared.error {
        return Err(format!("{} failed: {err}", spec.raw_path));
    }
    let outcome = tools.execute_prepared_with_cancellation_and_progress(&prepared, cell, |_| {});
    if !outcome.result.success {
        return Err(format!("{}: {}", spec.raw_path, outcome.result.output));
    }
    let val = if default_structured {
        if let Some(crate::tools::ToolPresentationSource::Read {
            full,
            first,
            records,
            next_offset,
            ..
        }) = outcome.receipt.presentation.as_ref()
        {
            serde_json::json!({
                "path": spec.raw_path,
                "content": full,
                "first_line": first,
                "total_lines": records.len(),
                "next_offset": next_offset,
                "complete": next_offset.is_none(),
            })
        } else {
            Value::String(outcome.result.output)
        }
    } else {
        Value::String(outcome.result.output)
    };
    Ok((spec.raw_path.clone(), val, prepared.target_paths))
}

fn try_read_one_spec_from_slab(
    tools: &ToolRegistry,
    daemon: &crate::project_index::ProjectIndexDaemon,
    workspace: &Path,
    spec: &ReadManySpec,
    default_structured: bool,
) -> Option<(String, Value, Vec<PathBuf>)> {
    if spec.outline || !spec.symbols.is_empty() || !spec.ranges.is_empty() {
        return None;
    }
    let abs = crate::tools::resolve_workspace_path_from_root(workspace, &spec.raw_path).ok()?;
    let cached = daemon.read_file_cached(&abs).ok()??;
    let total_lines = cached.line_count();
    let first = spec.offset.max(1);
    let max_lines = spec.max_lines.unwrap_or(crate::tools::MAX_READ_LINES_CAP);
    if first > total_lines {
        if first == 1 {
            tools.remember_complete_read_digest(&abs, cached.sha256);
        }
        let val = if default_structured {
            serde_json::json!({
                "path": spec.raw_path,
                "content": "",
                "first_line": first,
                "total_lines": 0,
                "next_offset": Value::Null,
                "complete": true,
            })
        } else {
            Value::String(String::new())
        };
        return Some((spec.raw_path.clone(), val, vec![abs]));
    }
    let end = first.saturating_add(max_lines).saturating_sub(1).min(total_lines);
    let slice = cached.line_slice(first, end);
    if slice.len() > 1024 * 1024 {
        return None;
    }
    let next_offset = (end < total_lines).then_some(end + 1);
    if first == 1 && next_offset.is_none() {
        tools.remember_complete_read_digest(&abs, cached.sha256);
    }
    let returned_lines = end.saturating_sub(first) + 1;
    let val = if default_structured {
        serde_json::json!({
            "path": spec.raw_path,
            "content": slice,
            "first_line": first,
            "total_lines": returned_lines,
            "next_offset": next_offset,
            "complete": next_offset.is_none(),
        })
    } else if let Some(next) = next_offset {
        Value::String(format!(
            "{slice}\n[showing lines {first}-{end}; more content available; pass \"offset\": {next} for the next page]"
        ))
    } else {
        Value::String(slice.to_owned())
    };
    Some((spec.raw_path.clone(), val, vec![abs]))
}

fn run_read_many_batch(
    tools: &ToolRegistry,
    workspace: &Path,
    specs: &[ReadManySpec],
    default_structured: bool,
    cell: Option<&CancellationToken>,
) -> (Result<Value, String>, Vec<PathBuf>) {
    let mut map = serde_json::Map::with_capacity(specs.len());
    let mut deps = Vec::with_capacity(specs.len());
    let daemon = crate::project_index::existing_for_workspace(workspace);
    let mut misses: Vec<(usize, &ReadManySpec)> = Vec::new();
    let mut ordered: Vec<Option<(String, Value, Vec<PathBuf>)>> =
        (0..specs.len()).map(|_| None).collect();

    if let Some(d) = daemon.as_deref() {
        for (idx, spec) in specs.iter().enumerate() {
            if cell.is_some_and(CancellationToken::is_cancelled) {
                return (Err("readMany cancelled".into()), deps);
            }
            if let Some(hit) =
                try_read_one_spec_from_slab(tools, d, workspace, spec, default_structured)
            {
                ordered[idx] = Some(hit);
            } else {
                misses.push((idx, spec));
            }
        }
    } else {
        misses.extend(specs.iter().enumerate());
    }

    if misses.len() < 4 {
        for (idx, spec) in misses {
            match read_one_spec(tools, workspace, spec, default_structured, cell) {
                Ok(row) => ordered[idx] = Some(row),
                Err(err) => return (Err(err), deps),
            }
        }
    } else {
        let next = AtomicUsize::new(0);
        let aborted = std::sync::atomic::AtomicBool::new(false);
        let collected = std::sync::Mutex::new(Vec::with_capacity(misses.len()));
        let workers = misses.len().min(4);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    while !aborted.load(Ordering::Acquire) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(orig_idx, spec)) = misses.get(i) else { break };
                        let res = read_one_spec(tools, workspace, spec, default_structured, cell);
                        if res.is_err() {
                            aborted.store(true, Ordering::Release);
                        }
                        collected
                            .lock()
                            .expect("read_many mutex")
                            .push((orig_idx, res));
                    }
                });
            }
        });
        let rows = collected.into_inner().expect("read_many mutex");
        for (orig_idx, res) in rows {
            match res {
                Ok(row) => ordered[orig_idx] = Some(row),
                Err(err) => return (Err(err), deps),
            }
        }
    }

    for row in ordered.into_iter().flatten() {
        let (key, val, target_paths) = row;
        for p in target_paths {
            if !deps.contains(&p) {
                deps.push(p);
            }
        }
        map.insert(key, val);
    }
    (Ok(Value::Object(map)), deps)
}

fn outline_one_path(
    daemon: Option<&crate::project_index::ProjectIndexDaemon>,
    workspace: &Path,
    abs: &Path,
    max_symbols: usize,
) -> Option<(String, Value)> {
    let rel = crate::tools::display_path(workspace, abs)
        .to_string_lossy()
        .replace('\\', "/");
    let map_sym = |s: &crate::tools::outline::OutlineSymbol| {
        serde_json::json!({
            "kind": s.kind,
            "name": s.name,
            "start_line": s.start_line,
            "end_line": s.end_line,
            "signature": s.signature,
        })
    };
    let syms_json: Vec<Value> = if let Some(cached) =
        daemon.and_then(|d| d.read_file_cached(abs).ok().flatten())
    {
        cached.outline().iter().take(max_symbols).map(map_sym).collect()
    } else if let Ok(text) = std::fs::read_to_string(abs) {
        crate::tools::outline::extract_outline(abs, &text)
            .iter()
            .take(max_symbols)
            .map(map_sym)
            .collect()
    } else {
        return None;
    };
    Some((rel, Value::Array(syms_json)))
}

fn run_outline_many_batch(
    workspace: &Path,
    input: &OutlineManyInput,
    max_symbols: usize,
    cell: Option<&CancellationToken>,
) -> (Result<Value, String>, Vec<PathBuf>) {
    let (paths, track_deps) = match input {
        OutlineManyInput::Paths(raw_paths) => {
            let mut resolved_list = Vec::with_capacity(raw_paths.len());
            for raw in raw_paths {
                match crate::tools::resolve_workspace_path_from_root(workspace, raw) {
                    Ok(p) => resolved_list.push(p),
                    Err(e) => return (Err(e.to_string()), Vec::new()),
                }
            }
            (resolved_list, true)
        }
        OutlineManyInput::Glob {
            subpath,
            pattern,
            max_files,
        } => {
            let listing = match glob_workspace_files(
                workspace,
                subpath.as_deref(),
                Some(pattern.as_str()),
                *max_files,
                cell,
            ) {
                Ok(l) => l,
                Err(e) => return (Err(e), Vec::new()),
            };
            let mut resolved_list = Vec::with_capacity(listing.files.len());
            for rel in listing.files.into_iter().take(*max_files) {
                if let Ok(resolved) = crate::tools::resolve_workspace_path_from_root(workspace, &rel) {
                    resolved_list.push(resolved);
                }
            }
            (resolved_list, false)
        }
    };
    let daemon = crate::project_index::for_workspace(workspace).ok();
    let deps = if track_deps { paths.clone() } else { Vec::new() };
    let mut out = serde_json::Map::with_capacity(paths.len());
    if paths.len() < 4 {
        for abs in &paths {
            if cell.is_some_and(CancellationToken::is_cancelled) {
                return (Err("outlineMany cancelled".into()), deps);
            }
            if let Some((rel, syms)) = outline_one_path(daemon.as_deref(), workspace, abs, max_symbols) {
                out.insert(rel, syms);
            }
        }
        return (Ok(Value::Object(out)), deps);
    }

    let next = AtomicUsize::new(0);
    let collected = std::sync::Mutex::new(Vec::with_capacity(paths.len()));
    let workers = paths.len().min(4);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if cell.is_some_and(CancellationToken::is_cancelled) {
                    break;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                let Some(abs) = paths.get(idx) else { break };
                let row = outline_one_path(daemon.as_deref(), workspace, abs, max_symbols);
                collected.lock().expect("outline_many mutex").push((idx, row));
            });
        }
    });
    if cell.is_some_and(CancellationToken::is_cancelled) {
        return (Err("outlineMany cancelled".into()), deps);
    }
    let mut rows = collected.into_inner().expect("outline_many mutex");
    rows.sort_by_key(|(idx, _)| *idx);
    for (_, row) in rows {
        if let Some((rel, syms)) = row {
            out.insert(rel, syms);
        }
    }
    (Ok(Value::Object(out)), deps)
}

const MAX_GREP_FILE_BYTES: u64 = 10 * 1024 * 1024;

fn load_searchable_text(
    abs: &Path,
    daemon: Option<&crate::project_index::ProjectIndexDaemon>,
    disk_cache: &mut HashMap<PathBuf, Option<Arc<str>>>,
) -> Option<Arc<str>> {
    if let Some(cached) = daemon.and_then(|d| d.read_file_cached(abs).ok().flatten()) {
        return Some(cached.text.clone());
    }
    if let Some(entry) = disk_cache.get(abs) {
        return entry.clone();
    }
    let loaded = std::fs::metadata(abs)
        .ok()
        .filter(|m| m.is_file() && m.len() <= MAX_GREP_FILE_BYTES)
        .and_then(|_| std::fs::read_to_string(abs).ok())
        .map(Arc::<str>::from);
    disk_cache.insert(abs.to_path_buf(), loaded.clone());
    loaded
}

fn scan_text_for_queries(
    text: &str,
    rel: &str,
    queries: &[&(String, Option<Vec<u8>>)],
    max_hits: usize,
    matches: &mut Vec<Value>,
    truncated: &mut bool,
) {
    for (idx, line) in text.lines().enumerate() {
        for (query, needle_lower) in queries {
            let matched = if let Some(lower) = needle_lower {
                contains_ascii_ci(line.as_bytes(), lower)
            } else {
                line.contains(query.as_str())
            };
            if matched {
                matches.push(serde_json::json!({
                    "query": query,
                    "path": rel,
                    "line": idx + 1,
                    "text": line.trim(),
                }));
                if matches.len() >= max_hits {
                    *truncated = true;
                    return;
                }
            }
        }
    }
}

fn run_grep_index_batch(
    workspace: &Path,
    queries: &[String],
    case_insensitive: bool,
    path: Option<&str>,
    max_hits: usize,
    cell: Option<&CancellationToken>,
) -> Result<Value, String> {
    let daemon = crate::project_index::for_workspace(workspace).ok();
    let mut matches = Vec::new();
    let mut truncated = false;
    let scope_prefix = path
        .map(|p| p.trim_start_matches("./").trim_end_matches('/').to_owned())
        .filter(|p| !p.is_empty() && p != ".");
    let scope_slash = scope_prefix.as_ref().map(|p| format!("{p}/"));
    let in_scope = |rel: &str| {
        scope_prefix.as_ref().is_none_or(|prefix| {
            rel == *prefix || scope_slash.as_ref().is_some_and(|s| rel.starts_with(s.as_str()))
        })
    };

    let prepared_queries: Vec<(String, Option<Vec<u8>>)> = queries
        .iter()
        .take(16)
        .map(|q| {
            let lower = case_insensitive.then(|| q.to_ascii_lowercase().into_bytes());
            (q.clone(), lower)
        })
        .collect();
    let mut disk_cache: HashMap<PathBuf, Option<Arc<str>>> = HashMap::new();
    let mut fallback_queries: Vec<&(String, Option<Vec<u8>>)> = Vec::new();
    let mut candidate_masks: std::collections::BTreeMap<PathBuf, u16> =
        std::collections::BTreeMap::new();

    for (q_idx, pq) in prepared_queries.iter().enumerate() {
        if cell.is_some_and(CancellationToken::is_cancelled) {
            return Err("grepIndex cancelled".into());
        }
        let Some(paths) = daemon.as_ref().and_then(|d| d.search_candidates(&pq.0)) else {
            fallback_queries.push(pq);
            continue;
        };
        let bit = 1u16 << q_idx;
        for abs in paths {
            *candidate_masks.entry(abs).or_insert(0) |= bit;
        }
    }

    for (abs, mask) in candidate_masks {
        if cell.is_some_and(CancellationToken::is_cancelled) {
            return Err("grepIndex cancelled".into());
        }
        let rel = crate::tools::display_path(workspace, &abs)
            .to_string_lossy()
            .replace('\\', "/");
        if !in_scope(&rel) {
            continue;
        }
        let Some(text) = load_searchable_text(&abs, daemon.as_deref(), &mut disk_cache) else {
            continue;
        };
        let active: Vec<&(String, Option<Vec<u8>>)> = prepared_queries
            .iter()
            .enumerate()
            .filter(|(idx, _)| (mask & (1u16 << idx)) != 0)
            .map(|(_, pq)| pq)
            .collect();
        scan_text_for_queries(&text, &rel, &active, max_hits, &mut matches, &mut truncated);
        if truncated {
            break;
        }
    }

    if !truncated && !fallback_queries.is_empty() {
        let listing = glob_workspace_files(workspace, path, None, 1000, cell)?;
        let scoped: Vec<(String, PathBuf)> = listing
            .files
            .into_iter()
            .filter(|rel| in_scope(rel))
            .filter_map(|rel| {
                crate::tools::resolve_workspace_path_from_root(workspace, &rel)
                    .ok()
                    .map(|abs| (rel, abs))
            })
            .collect();
        if scoped.len() > 32 {
            let next = AtomicUsize::new(0);
            let done = std::sync::atomic::AtomicBool::new(false);
            let chunks = std::sync::Mutex::new(Vec::<(usize, Vec<Value>)>::new());
            let workers = scoped.len().min(4);
            let daemon_ref = daemon.as_deref();
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| {
                        while !done.load(Ordering::Acquire) {
                            if cell.is_some_and(CancellationToken::is_cancelled) {
                                done.store(true, Ordering::Release);
                                break;
                            }
                            let idx = next.fetch_add(1, Ordering::Relaxed);
                            let Some((rel, abs)) = scoped.get(idx) else { break };
                            let slab_text = daemon_ref
                                .and_then(|d| d.read_file_cached(abs).ok().flatten());
                            let disk_text;
                            let text: &str = if let Some(cached) = slab_text.as_ref() {
                                if cached.text.len() as u64 > MAX_GREP_FILE_BYTES {
                                    continue;
                                }
                                &cached.text
                            } else {
                                let Some(s) = std::fs::metadata(abs)
                                    .ok()
                                    .filter(|m| m.is_file() && m.len() <= MAX_GREP_FILE_BYTES)
                                    .and_then(|_| std::fs::read_to_string(abs).ok())
                                else {
                                    continue;
                                };
                                disk_text = s;
                                disk_text.as_str()
                            };
                            let mut local = Vec::new();
                            let mut local_trunc = false;
                            scan_text_for_queries(
                                text,
                                rel,
                                &fallback_queries,
                                max_hits,
                                &mut local,
                                &mut local_trunc,
                            );
                            if !local.is_empty() {
                                chunks.lock().expect("grep_index mutex").push((idx, local));
                            }
                        }
                    });
                }
            });
            if cell.is_some_and(CancellationToken::is_cancelled) {
                return Err("grepIndex cancelled".into());
            }
            let mut rows = chunks.into_inner().expect("grep_index mutex");
            rows.sort_by_key(|(idx, _)| *idx);
            for (_, local) in rows {
                for hit in local {
                    matches.push(hit);
                    if matches.len() >= max_hits {
                        truncated = true;
                        break;
                    }
                }
                if truncated {
                    break;
                }
            }
        } else {
            for (rel, abs) in scoped {
                if cell.is_some_and(CancellationToken::is_cancelled) {
                    return Err("grepIndex cancelled".into());
                }
                let Some(text) = load_searchable_text(&abs, daemon.as_deref(), &mut disk_cache)
                else {
                    continue;
                };
                scan_text_for_queries(
                    &text,
                    &rel,
                    &fallback_queries,
                    max_hits,
                    &mut matches,
                    &mut truncated,
                );
                if truncated {
                    break;
                }
            }
        }
    }

    Ok(serde_json::json!({
        "matches": matches,
        "truncated": truncated,
    }))
}

fn run_graph_query(
    workspace: &Path,
    op: &str,
    symbol: Option<&str>,
    path: Option<&str>,
    depth: usize,
    limit: usize,
) -> (Result<Value, String>, bool) {
    let daemon = match crate::project_index::for_workspace(workspace) {
        Ok(d) => d,
        Err(e) => return (Err(format!("failed to access workspace index: {e}")), false),
    };
    let ready = daemon.stats().graph_ready;
    let res = match op {
        "definitions" => match symbol {
            Some(sym) => Ok(serde_json::json!({ "definitions": daemon.graph_definitions(sym, limit) })),
            None => Err("graph definitions requires `symbol`".to_owned()),
        },
        "callers" => match symbol {
            Some(sym) => Ok(serde_json::json!({ "callers": daemon.graph_callers(sym, limit) })),
            None => Err("graph callers requires `symbol`".to_owned()),
        },
        "callees" => match (path, symbol) {
            (Some(p), Some(sym)) => Ok(serde_json::json!({ "callees": daemon.graph_callees(p, sym, limit) })),
            _ => Err("graph callees requires `path` and `symbol`".to_owned()),
        },
        "imports" => match path {
            Some(p) => {
                let (imports, importers) = daemon.graph_imports(p);
                Ok(serde_json::json!({ "path": p, "imports": imports, "importers": importers }))
            }
            None => Err("graph imports requires `path`".to_owned()),
        },
        "tests" => match path {
            Some(p) => Ok(serde_json::json!({ "path": p, "tests": daemon.graph_tests_covering(p, limit) })),
            None => Err("graph tests requires `path`".to_owned()),
        },
        "impact" => match symbol.or(path) {
            Some(target) => {
                serde_json::to_value(daemon.graph_impact(target, depth, limit)).map_err(|e| e.to_string())
            }
            None => Err("graph impact requires `symbol` or `path`".to_owned()),
        },
        "symbols" => match path {
            Some(p) => Ok(serde_json::json!({ "path": p, "symbols": daemon.graph_file_symbols(p) })),
            None => Err("graph symbols requires `path`".to_owned()),
        },
        other => Err(format!("unsupported graph op `{other}`")),
    };
    (res, ready)
}
