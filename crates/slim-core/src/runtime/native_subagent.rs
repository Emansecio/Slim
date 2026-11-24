//! Ad-hoc parallel subagents inside a normal conversation (depth 1, up to 6 concurrent).
//!
//! Subagents inherit the parent conversation's model/provider client and operating mode
//! (unless `readonly: true` is requested), stream live steps and text into a shared
//! [`SubagentRegistry`], and support real-time pause/resume, immediate `steer` or
//! queued `queue` interventions (from both the user UI and the parent agent), and
//! individual cancellation.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{watch, Semaphore};

use super::*;
use crate::events::EventKind;
use crate::model::SessionEventSender;
use crate::provider::{HttpProviderClient, ProviderAdapter, ProviderMessage};
use crate::tools::ToolResult;

/// Maximum concurrent active subagents per conversation.
pub const MAX_CONCURRENT_SUBAGENTS: usize = 6;
const DEFAULT_SUBAGENT_MAX_TURNS: usize = AgentLoopConfig::DEFAULT_MAX_TURNS;
const MAX_SUBAGENT_STEPS_STORED: usize = 48;
const MAX_LIVE_TEXT_BYTES: usize = 16 * 1024;
const MAX_RESULT_CHARS: usize = 12_000;
const MIN_SUBAGENT_RESULT_CHARS: usize = 500;
const MAX_SUBAGENT_RESULT_CHARS: usize = 64_000;
static WORKTREE_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentModelTier {
    #[default]
    Same,
    Fast,
    Deep,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentIsolation {
    #[default]
    Shared,
    Worktree,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentOutputFormat {
    #[default]
    Markdown,
    Json,
    DiffOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentResumeOutcome {
    Unpaused,
    Steered,
    AlreadyActive,
    Relaunched,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Cancelled,
    Failed,
}

impl SubagentStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Paused)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentDelivery {
    /// Immediate intervention at the next turn/step boundary (auto-resumes if paused).
    Steer,
    /// Queued follow-up executed right after the current turn completes (auto-resumes if paused).
    Queue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentStep {
    pub id: String,
    pub name: String,
    pub target: String,
    pub running: bool,
    pub is_error: bool,
    pub duration_ms: u64,
    pub preview: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentInfo {
    pub id: String,
    pub role: String,
    pub prompt_summary: String,
    pub mode: crate::OperatingMode,
    pub status: SubagentStatus,
    pub activity: String,
    pub steps: Vec<SubagentStep>,
    pub live_text: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub worktree_path: Option<String>,
    pub pending_steers: usize,
    pub queued_messages: usize,
}

/// Cooperative pause gate checked before provider turns and tool batches.
#[derive(Clone, Debug)]
pub struct PauseGate {
    tx: Arc<watch::Sender<bool>>,
    // Keep at least one live receiver so `tx.send(...)` always updates state even before any waiter subscribes.
    _keep_alive: watch::Receiver<bool>,
}

impl Default for PauseGate {
    fn default() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            _keep_alive: rx,
        }
    }
}

impl PauseGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pause(&self) {
        let _ = self.tx.send(true);
    }

    pub fn resume(&self) {
        let _ = self.tx.send(false);
    }

    pub fn is_paused(&self) -> bool {
        *self.tx.borrow()
    }

    /// Blocks while paused until resumed (`true`) or cancelled (`false`).
    pub async fn wait_if_paused(&self, cancel: &CancellationToken) -> bool {
        if cancel.is_cancelled() {
            return false;
        }
        if !self.is_paused() {
            return true;
        }
        let mut rx = self.tx.subscribe();
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            if !*rx.borrow_and_update() {
                return true;
            }
            tokio::select! {
                _ = cancel.cancelled() => return false,
                changed = rx.changed() => {
                    if changed.is_err() {
                        return !cancel.is_cancelled();
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubagentSpawnRequest {
    pub role: String,
    pub prompt: String,
    pub readonly: bool,
    pub max_turns: usize,
    pub model_tier: SubagentModelTier,
    pub isolation: SubagentIsolation,
    pub output_format: SubagentOutputFormat,
    pub max_result_chars: Option<usize>,
    pub fork_context: bool,
    pub fork_messages: Option<Vec<ProviderMessage>>,
    pub fork_channel_anchors: Vec<(usize, crate::OperatingMode, String)>,
    pub max_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
}

impl Default for SubagentSpawnRequest {
    fn default() -> Self {
        Self {
            role: String::new(),
            prompt: String::new(),
            readonly: false,
            max_turns: DEFAULT_SUBAGENT_MAX_TURNS,
            model_tier: SubagentModelTier::Same,
            isolation: SubagentIsolation::Shared,
            output_format: SubagentOutputFormat::Markdown,
            max_result_chars: None,
            fork_context: false,
            fork_messages: None,
            fork_channel_anchors: Vec::new(),
            max_tokens: None,
            timeout_ms: None,
        }
    }
}

#[derive(Clone)]
struct SubagentSavedConfig {
    role: String,
    readonly: bool,
    max_turns: usize,
    model_tier: SubagentModelTier,
    isolation: SubagentIsolation,
    output_format: SubagentOutputFormat,
    max_result_chars: usize,
    max_tokens: Option<u64>,
    timeout_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct WorktreeHandle {
    pub repo_toplevel: PathBuf,
    pub wt_root: PathBuf,
    pub wt_cwd: PathBuf,
    pub base_head: String,
}

pub(super) struct SubagentRunArgs {
    pub id: String,
    pub role: String,
    pub prompt: String,
    pub readonly: bool,
    pub max_turns: usize,
    pub model_tier: SubagentModelTier,
    pub isolation: SubagentIsolation,
    pub output_format: SubagentOutputFormat,
    pub max_result_chars: usize,
    pub fork_messages: Option<Vec<ProviderMessage>>,
    pub fork_channel_anchors: Vec<(usize, crate::OperatingMode, String)>,
    pub max_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub resume_messages: Option<Vec<ProviderMessage>>,
    pub existing_worktree: Option<WorktreeHandle>,
    pub cancel: CancellationToken,
    pub pause: PauseGate,
    pub steer: SteerInbox,
    pub registry: SubagentRegistry,
}

/// A batch needs a copy of the parent's history only when a call can ask
/// for it: a `subagent` call, or a program that may spawn one, that names
/// `fork_context`.
pub(super) fn needs_fork_context(calls: &[ProviderToolCall]) -> bool {
    calls.iter().any(|call| {
        matches!(call.name.as_str(), "subagent" | "codemode" | "batch_code")
            && call.arguments.contains("fork_context")
    })
}

pub(super) fn sanitize_fork_context(messages: &[ProviderMessage]) -> Vec<ProviderMessage> {
    let mut out = messages.to_vec();
    while let Some(last) = out.last() {
        if last.role == "assistant" && !last.tool_calls.is_empty() {
            out.pop();
        } else {
            break;
        }
    }
    out
}

fn effective_max_result_chars(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(MAX_RESULT_CHARS)
        .clamp(MIN_SUBAGENT_RESULT_CHARS, MAX_SUBAGENT_RESULT_CHARS)
}

fn take_last_chars(text: &str, count: usize) -> &str {
    if count == 0 {
        return "";
    }
    let total = text.chars().count();
    if count >= total {
        return text;
    }
    let skip = total - count;
    let byte_start = text
        .char_indices()
        .nth(skip)
        .map_or(text.len(), |(idx, _)| idx);
    &text[byte_start..]
}

fn apply_result_limit_and_spill(
    text: &str,
    max_chars: usize,
    subagent_id: &str,
    store: Option<&ArtifactStore>,
) -> String {
    let limit = max_chars.clamp(MIN_SUBAGENT_RESULT_CHARS, MAX_SUBAGENT_RESULT_CHARS);
    let total_chars = text.chars().count();
    if total_chars <= limit {
        return text.to_owned();
    }
    let artifact = store.and_then(|s| s.put(&format!("subagent-{subagent_id}"), text.as_bytes()).ok());
    let footer = match artifact {
        Some(handle) => format!(
            "\n\n[Saída completa salva no artefato {} ({total_chars} chars). Use artifact_read para inspecionar trechos específicos.]",
            handle.id
        ),
        None => format!("\n\n[Saída truncada: {total_chars} chars]"),
    };
    let sep = "\n\n... [trecho intermediário omitido] ...\n\n";
    let body_budget = limit.saturating_sub(footer.chars().count() + sep.chars().count());
    let head_len = (body_budget * 3) / 5;
    let tail_len = body_budget.saturating_sub(head_len);
    let head: String = text.chars().take(head_len).collect();
    let tail = take_last_chars(text, tail_len);
    format!("{head}{sep}{tail}{footer}")
}

async fn run_git_checked(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(args).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| format!("git {:?} expirou após 10s", args))?
        .map_err(|err| format!("falha ao executar git: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(if detail.is_empty() {
            format!("git {:?} falhou com status {:?}", args, output.status.code())
        } else {
            detail
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn create_subagent_worktree(
    repo_cwd: &Path,
    subagent_id: &str,
) -> Result<WorktreeHandle, String> {
    let toplevel_raw = run_git_checked(repo_cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| "isolation: 'worktree' requer um repositório Git válido.".to_owned())?;
    let repo_toplevel =
        std::fs::canonicalize(&toplevel_raw).unwrap_or_else(|_| PathBuf::from(&toplevel_raw));
    let canon_cwd = std::fs::canonicalize(repo_cwd).unwrap_or_else(|_| repo_cwd.to_path_buf());
    let base_head = run_git_checked(&repo_toplevel, &["rev-parse", "HEAD"])
        .await
        .map_err(|_| {
            "isolation: 'worktree' requer ao menos um commit (HEAD válido) no repositório."
                .to_owned()
        })?;
    let seq = WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let wt_root = std::env::temp_dir().join(format!(
        "slim-wt-{}-{subagent_id}-{seq}-{ts}",
        std::process::id()
    ));
    let wt_root_str = wt_root.to_string_lossy().into_owned();
    run_git_checked(
        &repo_toplevel,
        &["worktree", "add", "--detach", &wt_root_str, "HEAD"],
    )
    .await
    .map_err(|err| format!("Falha ao criar worktree isolado: {err}"))?;
    let rel = canon_cwd
        .strip_prefix(&repo_toplevel)
        .unwrap_or(Path::new(""));
    let wt_cwd = if rel.as_os_str().is_empty() {
        wt_root.clone()
    } else {
        wt_root.join(rel)
    };
    Ok(WorktreeHandle {
        repo_toplevel,
        wt_root,
        wt_cwd,
        base_head,
    })
}

async fn finalize_subagent_worktree(wt: &WorktreeHandle, remove_if_clean: bool) -> (bool, Option<String>) {
    if !wt.wt_root.exists() {
        return (false, None);
    }
    let status = run_git_checked(&wt.wt_root, &["status", "--porcelain"])
        .await
        .unwrap_or_default();
    let head = run_git_checked(&wt.wt_root, &["rev-parse", "HEAD"])
        .await
        .unwrap_or_else(|_| wt.base_head.clone());
    if status.trim().is_empty() && head == wt.base_head {
        if remove_if_clean {
            let wt_str = wt.wt_root.to_string_lossy().into_owned();
            let _ =
                run_git_checked(&wt.repo_toplevel, &["worktree", "remove", "--force", &wt_str])
                    .await;
            let _ = run_git_checked(&wt.repo_toplevel, &["worktree", "prune"]).await;
        }
        return (false, None);
    }
    let mut parts = Vec::new();
    if head != wt.base_head {
        let range = format!("{}..HEAD", wt.base_head);
        if let Ok(commits) = run_git_checked(&wt.wt_root, &["log", "--oneline", "-n", "10", &range]).await {
            if !commits.is_empty() {
                parts.push(format!("Commits:\n{commits}"));
            }
        }
    }
    if let Ok(short) = run_git_checked(&wt.wt_root, &["status", "--short"]).await {
        if !short.is_empty() {
            parts.push(format!("Status:\n{short}"));
        }
    }
    if let Ok(stat) = run_git_checked(&wt.wt_root, &["diff", "--stat"]).await {
        if !stat.is_empty() {
            parts.push(format!("Diff:\n{stat}"));
        }
    }
    let joined = parts.join("\n");
    let clipped: String = joined.chars().take(1_500).collect();
    (true, Some(clipped))
}

pub(super) trait SubagentSpawner: Send + Sync {
    fn run(
        &self,
        args: SubagentRunArgs,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>>;
}

pub(super) struct ConcreteSubagentSpawner<A: ProviderAdapter + Send + Sync + 'static> {
    pub client: HttpProviderClient<A>,
    pub mode: crate::OperatingMode,
    pub cwd: PathBuf,
    pub base_config: AgentLoopConfig,
    pub artifact_store: Option<ArtifactStore>,
    pub sensitive_values: SensitiveValues,
    pub code_intel: Option<Arc<dyn CodeIntelligence>>,
    pub mcp: Option<Arc<McpManager>>,
    pub compaction_policy: crate::context::CompactionPolicy,
    pub initial_workspace_snapshot: Option<String>,
    pub skill_discovery: Option<(PathBuf, Option<crate::skills::DiscoveryResult>)>,
    pub parent_tools: Option<Arc<[Value]>>,
}

impl<A: ProviderAdapter + Send + Sync + 'static> SubagentSpawner for ConcreteSubagentSpawner<A> {
    fn run(
        &self,
        args: SubagentRunArgs,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>> {
        let client = self.client.clone();
        let mode = if args.readonly {
            crate::OperatingMode::ReadOnly
        } else {
            self.mode
        };
        let mut cwd = self.cwd.clone();
        let base_window = self.base_config.context_window_tokens;
        let mut config = self.base_config.clone();
        config.max_turns = if args.max_turns == 0 {
            DEFAULT_SUBAGENT_MAX_TURNS
        } else {
            args.max_turns
        };
        if let Some(ms) = args.timeout_ms {
            config.time_budget = Some(Duration::from_millis(ms.clamp(5_000, 1_800_000)));
        }
        if let Some(toks) = args.max_tokens {
            config.max_total_tokens = Some(toks.max(1_000));
        }
        let artifact_store = self.artifact_store.clone();
        let sensitive_values = self.sensitive_values.clone();
        let code_intel = self.code_intel.clone();
        let mcp = self.mcp.clone();
        let compaction_policy = self.compaction_policy.clone();
        let initial_workspace_snapshot = self.initial_workspace_snapshot.clone();
        let skill_discovery = self.skill_discovery.clone();
        let parent_tools = self.parent_tools.clone();

        Box::pin(async move {
            let mut active_worktree = args.existing_worktree;
            if let Some(wt) = active_worktree.as_ref() {
                if !wt.wt_cwd.exists() {
                    return Err(format!(
                        "Worktree preservado do subagente não existe mais em: {}",
                        wt.wt_root.display()
                    ));
                }
                cwd = wt.wt_cwd.clone();
            } else if args.isolation == SubagentIsolation::Worktree && !args.readonly {
                let wt = create_subagent_worktree(&cwd, &args.id).await?;
                args.registry.set_worktree(&args.id, wt.clone());
                cwd = wt.wt_cwd.clone();
                active_worktree = Some(wt);
            }

            let mut runtime = Runtime::new();
            runtime.subagent_depth = 1;
            runtime.artifact_store = artifact_store.clone();
            runtime.sensitive_values = sensitive_values;
            runtime.code_intel = code_intel;
            runtime.mcp = mcp;
            runtime.preseeded_workspace_snapshot = initial_workspace_snapshot.clone();
            runtime.preseeded_skill_discovery = skill_discovery;
            runtime.subagent_readonly = args.readonly;
            if args.fork_messages.is_some() {
                runtime.frozen_tools_override = parent_tools;
                config.stable_tool_envelope = true;
            }

            // Default is strictly `SubagentModelTier::Same`, even when `readonly: true`.
            match args.model_tier {
                SubagentModelTier::Fast => {
                    if let Some((fast_id, fast_window)) = client.adapter().fast_compaction_model() {
                        runtime.model_override = Some(fast_id.to_owned());
                        config.context_window_tokens = config.context_window_tokens.min(fast_window);
                        args.registry.set_model(&args.id, fast_id);
                    } else {
                        args.registry.set_model(&args.id, client.adapter().model());
                    }
                }
                SubagentModelTier::Deep => {
                    runtime.deep_reasoning = true;
                    args.registry.set_model(&args.id, client.adapter().model());
                }
                SubagentModelTier::Same => {
                    args.registry.set_model(&args.id, client.adapter().model());
                }
            }

            let compaction_handle = crate::context::CompactionHandle::new(compaction_policy);
            if let Some(snapshot) = initial_workspace_snapshot {
                compaction_handle.set_initial_workspace_snapshot(snapshot);
            }
            runtime.set_compaction_handle(compaction_handle);
            runtime.set_cancellation_token(args.cancel.clone());
            runtime.set_steer_inbox(args.steer.clone());
            runtime.set_pause_gate(args.pause.clone());

            let (ev_tx, ev_rx) = SessionEventSender::bounded(256, args.cancel.clone());
            runtime.app.set_event_sender(ev_tx);

            // Spawn live event consumer forwarding child events into SubagentRegistry.
            let reg_ev = args.registry.clone();
            let id_ev = args.id.clone();
            let ev_task = tokio::task::spawn_blocking(move || {
                let mut last_text_flush = Instant::now();
                let mut dirty_text = false;
                loop {
                    let recv = if dirty_text {
                        let wait =
                            Duration::from_millis(60).saturating_sub(last_text_flush.elapsed());
                        if wait.is_zero() {
                            dirty_text = false;
                            last_text_flush = Instant::now();
                            reg_ev.emit_change();
                            continue;
                        }
                        ev_rx.recv_timeout(wait)
                    } else {
                        ev_rx
                            .recv()
                            .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
                    };
                    match recv {
                        Ok(event) => {
                            let urgent = reg_ev.apply_child_event(&id_ev, &event.kind);
                            if urgent {
                                dirty_text = false;
                                last_text_flush = Instant::now();
                                reg_ev.emit_change();
                            } else {
                                dirty_text = true;
                                if last_text_flush.elapsed() >= Duration::from_millis(60) {
                                    dirty_text = false;
                                    last_text_flush = Instant::now();
                                    reg_ev.emit_change();
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if dirty_text {
                                dirty_text = false;
                                last_text_flush = Instant::now();
                                reg_ev.emit_change();
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                if dirty_text {
                    reg_ev.emit_change();
                }
            });

            let mut messages = if let Some(mut resumed) = args.resume_messages {
                resumed.push(ProviderMessage::user(args.prompt));
                resumed
            } else {
                let format_hint = match args.output_format {
                    SubagentOutputFormat::Markdown => "",
                    SubagentOutputFormat::Json => "\n\nOutput contract: return ONLY valid JSON in your final response (no prose outside JSON).",
                    SubagentOutputFormat::DiffOnly => "\n\nOutput contract: return ONLY unified diffs (`git diff` style) or, if no files changed, a concise `path:line-range` findings list.",
                };
                let tree_hint = if active_worktree.is_some() {
                    "You work in your own git worktree."
                } else {
                    "You share this working tree with the main agent and other subagents: stay within your subtask and never run destructive git (reset, stash, checkout of files you did not change)."
                };
                let system_hint = format!(
                    "[Subagent role: \"{}\"]\n\
                     You are an ad-hoc subagent working on a delegated subtask. {tree_hint}\n\
                     Only your final response returns to the main agent, cut to {} characters: put every finding in it, concise and high-signal, citing exact file paths and line numbers instead of pasting large raw excerpts.{format_hint}",
                    args.role, args.max_result_chars
                );
                let user_msg =
                    ProviderMessage::user(format!("{system_hint}\n\n---\n\n{}", args.prompt));
                if let Some(mut forked) = args.fork_messages.filter(|m| !m.is_empty()) {
                    if let Some(handle) = runtime.compaction_handle.as_ref() {
                        handle.seed_channel_anchors(&forked, args.fork_channel_anchors);
                    }
                    forked.push(user_msg);
                    forked
                } else {
                    vec![user_msg]
                }
            };

            let mut next_seq = 1u64;
            let mut final_text = String::new();
            let mut retried_fast_fallback = false;
            let mut loop_outcome: Result<String, String>;

            loop {
                if !args.pause.wait_if_paused(&args.cancel).await {
                    loop_outcome = Err("Subagente cancelado.".to_owned());
                    break;
                }
                let res = runtime
                    .run_agent_loop_with_messages(
                        &client,
                        &messages,
                        mode,
                        &cwd,
                        next_seq,
                        config.clone(),
                    )
                    .await;

                match res {
                    Ok(done) => {
                        next_seq = done.next_seq;
                        if done.stop == AgentLoopStop::Cancelled || args.cancel.is_cancelled() {
                            loop_outcome = Err("Subagente cancelado.".to_owned());
                            break;
                        }
                        if let Some(reply) = last_assistant_text(runtime.conversation()) {
                            final_text = reply;
                        }
                        let next = loop {
                            let late_steers = args.steer.drain();
                            if !late_steers.is_empty() {
                                break Some((late_steers, "Processando orientação recebida…"));
                            }
                            if let Some(queued) = args.registry.pop_queued_message(&args.id) {
                                break Some((vec![queued], "Processando próxima instrução da fila…"));
                            }
                            // Nothing waiting: close under the registry lock, so a message
                            // accepted after the drains above is never silently dropped.
                            if args.registry.close_if_idle(&args.id) {
                                break None;
                            }
                        };
                        let Some((pending, activity)) = next else {
                            loop_outcome = Ok(final_text);
                            break;
                        };
                        messages = runtime.conversation().to_vec();
                        messages.extend(pending.into_iter().map(ProviderMessage::user));
                        args.registry.append_live_separator(&args.id);
                        args.registry.set_activity(&args.id, activity);
                        continue;
                    }
                    Err(ProviderError::Cancelled) => {
                        loop_outcome = Err("Subagente cancelado.".to_owned());
                        break;
                    }
                    Err(err)
                        if runtime.model_override.is_some()
                            && !retried_fast_fallback
                            && !args.cancel.is_cancelled()
                            && super::compaction_foreground::is_fast_model_rejection(&err) =>
                    {
                        retried_fast_fallback = true;
                        runtime.model_override = None;
                        config.context_window_tokens = base_window;
                        args.registry.set_model(&args.id, client.adapter().model());
                        args.registry.set_activity(
                            &args.id,
                            "Modelo rápido indisponível; usando modelo ativo…",
                        );
                        continue;
                    }
                    Err(err) => {
                        loop_outcome = Err(provider_retry_reason(&err));
                        break;
                    }
                }
            }

            let conv = runtime.conversation().to_vec();
            if !conv.is_empty() {
                args.registry.save_conversation(&args.id, conv);
            }
            drop(runtime);
            let _ = ev_task.await;

            if let Some(wt) = active_worktree.as_ref() {
                let (kept, summary) = finalize_subagent_worktree(wt, true).await;
                if !kept {
                    args.registry.clear_worktree(&args.id);
                } else if let (Ok(ref mut text), Some(summary)) = (&mut loop_outcome, summary) {
                    if !text.trim().is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&format!(
                        "[Worktree isolado preservado em: {}]\n{summary}",
                        wt.wt_root.display()
                    ));
                }
            }

            if let Ok(ref mut text) = loop_outcome {
                *text = apply_result_limit_and_spill(
                    text,
                    args.max_result_chars,
                    &args.id,
                    artifact_store.as_ref(),
                );
            }

            loop_outcome
        })
    }
}

fn last_assistant_text(messages: &[ProviderMessage]) -> Option<String> {
    messages.iter().rev().find_map(|msg| {
        if msg.role == "assistant" && !msg.content.trim().is_empty() {
            Some(msg.content.trim().to_owned())
        } else {
            None
        }
    })
}

struct SubagentEntry {
    info: SubagentInfo,
    started_at: Instant,
    finished_at: Option<Instant>,
    paused_at: Option<Instant>,
    paused_duration: Duration,
    has_slot: bool,
    collected: bool,
    /// The run saw nothing left to process and is ending: no more messages.
    closing: bool,
    /// The parent was already told this finished subagent awaits collection.
    announced: bool,
    cancel: CancellationToken,
    pause: PauseGate,
    steer: SteerInbox,
    followup_queue: VecDeque<String>,
    done_tx: watch::Sender<bool>,
    _done_rx: watch::Receiver<bool>,
    saved_conversation: Vec<ProviderMessage>,
    saved_config: SubagentSavedConfig,
    worktree: Option<WorktreeHandle>,
}

impl SubagentEntry {
    fn snapshot(&self) -> SubagentInfo {
        let mut info = self.info.clone();
        let active_end = self.finished_at.unwrap_or_else(Instant::now);
        let wall = active_end.saturating_duration_since(self.started_at);
        let current_pause = self
            .paused_at
            .map_or(Duration::ZERO, |at| active_end.saturating_duration_since(at));
        let total_paused = self.paused_duration.saturating_add(current_pause);
        info.elapsed_ms = wall.saturating_sub(total_paused).as_millis() as u64;
        info.pending_steers = self.steer.len();
        info.queued_messages = self.followup_queue.len();
        info
    }
}

type ChangeCallback = Arc<dyn Fn(Vec<SubagentInfo>) + Send + Sync>;

struct RegistryInner {
    entries: Vec<SubagentEntry>,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    on_change: Option<ChangeCallback>,
    default_mode: crate::OperatingMode,
}

/// Session-scoped live registry of ad-hoc subagents (up to 6 concurrent).
/// How a parent's wait on one subagent ended (see `SubagentRegistry::wait_all`).
#[derive(Clone, Debug)]
pub enum SubagentWait {
    Done(Box<SubagentInfo>),
    /// The registry no longer knows the subagent.
    Gone,
    /// The user wrote to the parent first: the subagent is still running in the background.
    Released,
}

/// Tool result for a subagent the parent stopped waiting for because the user wrote to it.
fn released_wait_text(id: &str) -> String {
    format!(
        "O usuário enviou uma mensagem durante a espera: o subagente {id} segue trabalhando em segundo plano. Atenda a mensagem e depois use subagent_control (status ou wait) para acompanhar ou recolher o resultado."
    )
}

/// Tool result for one finished wait; `Err` carries the failure text.
fn subagent_wait_result(id: &str, waited: &SubagentWait) -> Result<String, String> {
    match waited {
        SubagentWait::Done(info) => {
            let (ok, text) = format_subagent_outcome(info);
            if ok { Ok(text) } else { Err(text) }
        }
        SubagentWait::Gone => Err(format!("Subagente {id} desapareceu.")),
        SubagentWait::Released => Ok(released_wait_text(id)),
    }
}

#[derive(Clone)]
pub struct SubagentRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    next_id: Arc<AtomicU64>,
    slots: Arc<Semaphore>,
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for SubagentRegistry {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}
impl Eq for SubagentRegistry {}

impl std::fmt::Debug for SubagentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRegistry").finish()
    }
}

fn unpause_subagent_entry(e: &mut SubagentEntry, slots_free: bool, running_activity: &str) {
    if let Some(paused_at) = e.paused_at.take() {
        e.paused_duration = e
            .paused_duration
            .saturating_add(paused_at.elapsed());
    }
    e.pause.resume();
    if e.has_slot || slots_free {
        e.info.status = SubagentStatus::Running;
        e.info.activity = running_activity.to_owned();
    } else {
        e.info.status = SubagentStatus::Queued;
        e.info.activity = "Aguardando vaga…".to_owned();
    }
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                entries: Vec::new(),
                spawner: None,
                on_change: None,
                default_mode: crate::OperatingMode::Auto,
            })),
            next_id: Arc::new(AtomicU64::new(1)),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBAGENTS)),
        }
    }

    pub fn set_on_change(&self, cb: ChangeCallback) {
        lock_mutex(&self.inner).on_change = Some(cb);
    }

    pub(super) fn set_spawner(
        &self,
        spawner: Arc<dyn SubagentSpawner>,
        mode: crate::OperatingMode,
    ) {
        let mut guard = lock_mutex(&self.inner);
        guard.spawner = Some(spawner);
        guard.default_mode = mode;
    }

    pub fn list(&self) -> Vec<SubagentInfo> {
        let guard = lock_mutex(&self.inner);
        guard.entries.iter().map(SubagentEntry::snapshot).collect()
    }

    pub fn get(&self, id: &str) -> Option<SubagentInfo> {
        let guard = lock_mutex(&self.inner);
        guard
            .entries
            .iter()
            .find(|e| e.info.id == id)
            .map(SubagentEntry::snapshot)
    }

    fn emit_change(&self) {
        let (cb, snap) = {
            let guard = lock_mutex(&self.inner);
            let Some(cb) = guard.on_change.clone() else {
                return;
            };
            let snap: Vec<SubagentInfo> =
                guard.entries.iter().map(SubagentEntry::snapshot).collect();
            (cb, snap)
        };
        cb(snap);
    }

    /// Spawns a new subagent running on a tokio task (bounded by `MAX_CONCURRENT_SUBAGENTS`).
    pub fn spawn(
        &self,
        req: SubagentSpawnRequest,
        parent_cancel: Option<CancellationToken>,
    ) -> Result<String, String> {
        let (spawner, mode) = {
            let guard = lock_mutex(&self.inner);
            let Some(spawner) = guard.spawner.clone() else {
                return Err("Subagentes indisponíveis neste contexto de execução.".to_owned());
            };
            let mode = if req.readonly {
                crate::OperatingMode::ReadOnly
            } else {
                guard.default_mode
            };
            (spawner, mode)
        };

        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("sub-{n}");
        let role = {
            let trimmed = req.role.trim();
            if trimmed.is_empty() {
                format!("Subagente {n}")
            } else {
                trimmed.chars().take(64).collect()
            }
        };
        let prompt_summary: String = req
            .prompt
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(180)
            .collect();
        let max_turns = if req.max_turns == 0 {
            DEFAULT_SUBAGENT_MAX_TURNS
        } else {
            req.max_turns
        };
        let max_result_chars = effective_max_result_chars(req.max_result_chars);
        let fork_messages = if req.fork_context {
            req.fork_messages
        } else {
            None
        };
        let saved_config = SubagentSavedConfig {
            role: role.clone(),
            readonly: req.readonly,
            max_turns,
            model_tier: req.model_tier,
            isolation: req.isolation,
            output_format: req.output_format,
            max_result_chars,
            max_tokens: req.max_tokens,
            timeout_ms: req.timeout_ms,
        };

        let cancel = CancellationToken::new();
        let pause = PauseGate::new();
        let steer = SteerInbox::default();
        let (done_tx, done_rx) = watch::channel(false);

        let entry = SubagentEntry {
            info: SubagentInfo {
                id: id.clone(),
                role: role.clone(),
                prompt_summary,
                mode,
                status: SubagentStatus::Queued,
                activity: "Aguardando vaga…".to_owned(),
                steps: Vec::new(),
                live_text: String::new(),
                result: None,
                error: None,
                elapsed_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                model: None,
                worktree_path: None,
                pending_steers: 0,
                queued_messages: 0,
            },
            started_at: Instant::now(),
            finished_at: None,
            paused_at: None,
            paused_duration: Duration::ZERO,
            has_slot: false,
            collected: false,
            closing: false,
            announced: false,
            cancel: cancel.clone(),
            pause: pause.clone(),
            steer: steer.clone(),
            followup_queue: VecDeque::new(),
            done_tx: done_tx.clone(),
            _done_rx: done_rx.clone(),
            saved_conversation: Vec::new(),
            saved_config,
            worktree: None,
        };

        {
            let mut guard = lock_mutex(&self.inner);
            guard.entries.push(entry);
        }
        self.emit_change();

        let run_args = SubagentRunArgs {
            id: id.clone(),
            role,
            prompt: req.prompt,
            readonly: req.readonly,
            max_turns,
            model_tier: req.model_tier,
            isolation: req.isolation,
            output_format: req.output_format,
            max_result_chars,
            fork_messages,
            fork_channel_anchors: req.fork_channel_anchors,
            max_tokens: req.max_tokens,
            timeout_ms: req.timeout_ms,
            resume_messages: None,
            existing_worktree: None,
            cancel: cancel.clone(),
            pause: pause.clone(),
            steer,
            registry: self.clone(),
        };

        self.spawn_entry_task(
            id.clone(),
            spawner,
            run_args,
            cancel,
            pause,
            done_rx,
            parent_cancel,
        );

        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_entry_task(
        &self,
        task_id: String,
        spawner: Arc<dyn SubagentSpawner>,
        run_args: SubagentRunArgs,
        cancel: CancellationToken,
        pause: PauseGate,
        done_rx: watch::Receiver<bool>,
        parent_cancel: Option<CancellationToken>,
    ) {
        // Cascade cancellation from parent to child, without child cancelling parent.
        if let Some(parent) = parent_cancel {
            let child_c = cancel.clone();
            let mut done_c = done_rx;
            tokio::spawn(async move {
                tokio::select! {
                    _ = parent.cancelled() => child_c.cancel(),
                    _ = done_c.wait_for(|&done| done) => {}
                }
            });
        }

        let reg = self.clone();
        let slots = Arc::clone(&self.slots);

        tokio::spawn(async move {
            // A paused subagent never holds a concurrency slot (up to 6 parallel):
            // it waits out the pause first, and gives the slot back when the pause
            // arrived while it was queued for one.
            let permit = loop {
                if !pause.wait_if_paused(&cancel).await {
                    reg.finish_entry(&task_id, SubagentStatus::Cancelled, None, Some("Cancelado".into()));
                    return;
                }
                let permit = tokio::select! {
                    _ = cancel.cancelled() => None,
                    p = Arc::clone(&slots).acquire_owned() => p.ok(),
                };
                let Some(permit) = permit.filter(|_| !cancel.is_cancelled()) else {
                    reg.finish_entry(&task_id, SubagentStatus::Cancelled, None, Some("Cancelado".into()));
                    return;
                };
                if !pause.is_paused() {
                    break permit;
                }
            };
            reg.mark_running(&task_id);

            let outcome = spawner.run(run_args).await;
            drop(permit);
            match outcome {
                Ok(text) => {
                    if cancel.is_cancelled() {
                        reg.finish_entry(&task_id, SubagentStatus::Cancelled, None, Some("Cancelado".into()));
                    } else {
                        reg.finish_entry(&task_id, SubagentStatus::Completed, Some(text), None);
                    }
                }
                Err(err) => {
                    let status = if cancel.is_cancelled() {
                        SubagentStatus::Cancelled
                    } else {
                        SubagentStatus::Failed
                    };
                    reg.finish_entry(&task_id, status, None, Some(err));
                }
            }
        });
    }

    fn set_model(&self, id: &str, model: &str) {
        {
            let mut guard = lock_mutex(&self.inner);
            if let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) {
                e.info.model = Some(model.to_owned());
            }
        }
        self.emit_change();
    }

    fn set_worktree(&self, id: &str, wt: WorktreeHandle) {
        {
            let mut guard = lock_mutex(&self.inner);
            if let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) {
                e.info.worktree_path = Some(wt.wt_root.to_string_lossy().into_owned());
                e.worktree = Some(wt);
            }
        }
        self.emit_change();
    }

    fn clear_worktree(&self, id: &str) {
        {
            let mut guard = lock_mutex(&self.inner);
            if let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) {
                e.info.worktree_path = None;
                e.worktree = None;
            }
        }
        self.emit_change();
    }

    fn save_conversation(&self, id: &str, conv: Vec<ProviderMessage>) {
        let mut guard = lock_mutex(&self.inner);
        if let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) {
            e.saved_conversation = conv;
        }
    }

    fn mark_running(&self, id: &str) {
        {
            let mut guard = lock_mutex(&self.inner);
            if let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) {
                e.has_slot = true;
                if !e.info.status.is_terminal() {
                    e.info.status = if e.pause.is_paused() {
                        SubagentStatus::Paused
                    } else {
                        SubagentStatus::Running
                    };
                    if e.info.activity == "Aguardando vaga…" {
                        e.info.activity = "Iniciando…".to_owned();
                    }
                }
            }
        }
        self.emit_change();
    }

    fn append_live_separator(&self, id: &str) {
        {
            let mut guard = lock_mutex(&self.inner);
            if let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) {
                if !e.info.live_text.trim().is_empty() {
                    e.info.live_text.push_str("\n\n---\n\n");
                }
            }
        }
        self.emit_change();
    }

    pub fn reset(&self) {
        {
            let mut guard = lock_mutex(&self.inner);
            for e in &mut guard.entries {
                if !e.info.status.is_terminal() {
                    e.pause.resume();
                    e.cancel.cancel();
                }
                let _ = e.done_tx.send(true);
            }
            guard.entries.clear();
        }
        self.emit_change();
    }

    fn mark_collected(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut guard = lock_mutex(&self.inner);
        for e in &mut guard.entries {
            if ids.iter().any(|id| id == &e.info.id) && e.info.status.is_terminal() {
                e.collected = true;
            }
        }
    }

    fn set_activity(&self, id: &str, activity: &str) {
        {
            let mut guard = lock_mutex(&self.inner);
            if let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) {
                e.info.activity = activity.to_owned();
            }
        }
        self.emit_change();
    }

    fn pop_queued_message(&self, id: &str) -> Option<String> {
        let msg = {
            let mut guard = lock_mutex(&self.inner);
            let e = guard.entries.iter_mut().find(|e| e.info.id == id)?;
            let next = e.followup_queue.pop_front();
            e.info.queued_messages = e.followup_queue.len();
            next
        };
        if msg.is_some() {
            self.emit_change();
        }
        msg
    }

    fn finish_entry(
        &self,
        id: &str,
        status: SubagentStatus,
        result: Option<String>,
        error: Option<String>,
    ) {
        let done_tx = {
            let mut guard = lock_mutex(&self.inner);
            let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) else {
                return;
            };
            let now = Instant::now();
            if let Some(paused_at) = e.paused_at.take() {
                e.paused_duration = e
                    .paused_duration
                    .saturating_add(now.saturating_duration_since(paused_at));
            }
            e.info.status = status;
            e.info.activity = match status {
                SubagentStatus::Completed => "Concluído".to_owned(),
                SubagentStatus::Cancelled => "Cancelado".to_owned(),
                SubagentStatus::Failed => "Falhou".to_owned(),
                _ => e.info.activity.clone(),
            };
            for step in &mut e.info.steps {
                step.running = false;
            }
            if let Some(text) = result {
                let clipped: String = text
                    .chars()
                    .take(e.saved_config.max_result_chars)
                    .collect();
                e.info.result = Some(clipped);
            }
            if let Some(err) = error {
                e.info.error = Some(err);
            }
            e.finished_at = Some(now);
            e.done_tx.clone()
        };
        let _ = done_tx.send(true);
        self.emit_change();
    }

    /// Updates live status/steps/text from a child `EventKind`. Returns `true` when
    /// an immediate UI notification is warranted (status/step transition).
    fn apply_child_event(&self, id: &str, kind: &EventKind) -> bool {
        let mut guard = lock_mutex(&self.inner);
        let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) else {
            return false;
        };
        match kind {
            EventKind::ThinkingStarted => {
                e.info.activity = "Pensando…".to_owned();
                true
            }
            EventKind::ProviderPhase { phase, detail, .. } => {
                e.info.activity = match (phase, detail.as_deref()) {
                    (crate::ProviderPhase::Connecting, _) => "Conectando ao modelo…".to_owned(),
                    (crate::ProviderPhase::PreparingTool, Some(d)) if !d.is_empty() => {
                        format!("Preparando {d}")
                    }
                    (crate::ProviderPhase::PreparingTool, _) => "Preparando ferramenta…".to_owned(),
                    (crate::ProviderPhase::FirstSemantic, _) => "Respondendo…".to_owned(),
                    (_, Some(d)) if !d.is_empty() => d.to_owned(),
                    _ => e.info.activity.clone(),
                };
                true
            }
            EventKind::ToolStarted {
                call_id,
                name,
                arguments,
                ..
            } => {
                let target = short_tool_target(name, arguments);
                e.info.activity = if target.is_empty() {
                    format!("Executando {name}")
                } else {
                    format!("{name} · {target}")
                };
                if e.info.steps.len() >= MAX_SUBAGENT_STEPS_STORED {
                    e.info.steps.remove(0);
                }
                e.info.steps.push(SubagentStep {
                    id: call_id.clone(),
                    name: name.clone(),
                    target,
                    running: true,
                    is_error: false,
                    duration_ms: 0,
                    preview: String::new(),
                });
                true
            }
            EventKind::ToolProgress {
                call_id, preview, ..
            } => {
                let line = preview.trim();
                if !line.is_empty() {
                    if let Some(step) = e.info.steps.iter_mut().rev().find(|s| s.id == *call_id) {
                        step.preview = line.chars().take(160).collect();
                    }
                    e.info.activity = line.chars().take(120).collect();
                }
                false
            }
            EventKind::ToolOutput {
                call_id, output, ..
            } => {
                if let Some(step) = e.info.steps.iter_mut().rev().find(|s| s.id == *call_id) {
                    let first = output.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                    step.preview = first.chars().take(160).collect();
                }
                false
            }
            EventKind::ToolFinished {
                call_id,
                success,
                duration_ms,
                ..
            } => {
                if let Some(step) = e.info.steps.iter_mut().rev().find(|s| s.id == *call_id) {
                    step.running = false;
                    step.is_error = !*success;
                    step.duration_ms = *duration_ms;
                }
                e.info.pending_steers = usize::from(!e.steer.is_empty());
                true
            }
            EventKind::AssistantTextDelta { text } => {
                e.info.live_text.push_str(text);
                if e.info.live_text.len() > MAX_LIVE_TEXT_BYTES {
                    let keep = e.info.live_text.len() - MAX_LIVE_TEXT_BYTES;
                    let mut cut = keep;
                    while !e.info.live_text.is_char_boundary(cut) && cut < e.info.live_text.len() {
                        cut += 1;
                    }
                    e.info.live_text.drain(..cut);
                }
                if e.info.activity == "Iniciando…" || e.info.activity == "Pensando…" {
                    e.info.activity = "Escrevendo resposta…".to_owned();
                }
                false
            }
            EventKind::Usage {
                input_tokens,
                output_tokens,
            } => {
                e.info.input_tokens = e.info.input_tokens.saturating_add(*input_tokens);
                e.info.output_tokens = e.info.output_tokens.saturating_add(*output_tokens);
                true
            }
            EventKind::UsageBreakdown { usage } => {
                e.info.cache_read_tokens = e
                    .info
                    .cache_read_tokens
                    .saturating_add(usage.cache_read_tokens);
                e.info.cache_write_tokens = e
                    .info
                    .cache_write_tokens
                    .saturating_add(usage.cache_write_tokens);
                true
            }
            _ => false,
        }
    }

    pub fn pause(&self, id: &str) -> Result<(), String> {
        {
            let mut guard = lock_mutex(&self.inner);
            let e = guard
                .entries
                .iter_mut()
                .find(|e| e.info.id == id)
                .ok_or_else(|| format!("Subagente \"{id}\" não encontrado."))?;
            if e.info.status.is_terminal() {
                return Err(format!("Subagente \"{id}\" já terminou."));
            }
            e.pause.pause();
            if e.paused_at.is_none() {
                e.paused_at = Some(Instant::now());
            }
            e.info.status = SubagentStatus::Paused;
            e.info.activity = "Pausado".to_owned();
        }
        self.emit_change();
        Ok(())
    }

    pub fn resume(&self, id: &str) -> Result<(), String> {
        self.resume_with_message(id, None, None).map(|_| ())
    }

    pub fn resume_with_message(
        &self,
        id: &str,
        message: Option<String>,
        parent_cancel: Option<CancellationToken>,
    ) -> Result<SubagentResumeOutcome, String> {
        let trimmed = message
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty());
        let slots_free = self.slots.available_permits() > 0;
        let relaunch = {
            let mut guard = lock_mutex(&self.inner);
            let spawner_opt = guard.spawner.clone();
            let e = guard
                .entries
                .iter_mut()
                .find(|e| e.info.id == id)
                .ok_or_else(|| format!("Subagente \"{id}\" não encontrado."))?;
            if !e.info.status.is_terminal() {
                if let Some(msg) = trimmed {
                    if e.closing {
                        return Err(format!("Subagente \"{id}\" está concluindo; repita o resume com 'message' em instantes."));
                    }
                    if !e.steer.push(msg) {
                        return Err("Caixa de intervenção do subagente está cheia.".to_owned());
                    }
                    e.info.pending_steers = e.steer.len();
                    if e.pause.is_paused() {
                        unpause_subagent_entry(e, slots_free, "Retomando com nova instrução…");
                    } else {
                        let outcome = SubagentResumeOutcome::Steered;
                        drop(guard);
                        self.emit_change();
                        return Ok(outcome);
                    }
                } else if e.info.status == SubagentStatus::Paused {
                    unpause_subagent_entry(e, slots_free, "Retomando…");
                } else {
                    return Ok(SubagentResumeOutcome::AlreadyActive);
                }
                drop(guard);
                self.emit_change();
                return Ok(SubagentResumeOutcome::Unpaused);
            }

            let Some(msg) = trimmed else {
                return Err(format!(
                    "Subagente \"{id}\" já terminou. Passe 'message' em resume para continuar a partir do histórico salvo."
                ));
            };
            let Some(spawner) = spawner_opt else {
                return Err("Subagentes indisponíveis neste contexto de execução.".to_owned());
            };
            let cancel = CancellationToken::new();
            let pause = PauseGate::new();
            let steer = SteerInbox::default();
            let (done_tx, done_rx) = watch::channel(false);

            e.started_at = Instant::now();
            e.finished_at = None;
            e.paused_at = None;
            e.paused_duration = Duration::ZERO;
            e.has_slot = false;
            e.collected = false;
            e.closing = false;
            e.announced = false;
            e.cancel = cancel.clone();
            e.pause = pause.clone();
            e.steer = steer.clone();
            e.done_tx = done_tx;
            e._done_rx = done_rx.clone();
            e.info.status = SubagentStatus::Queued;
            e.info.activity = "Retomando com nova instrução…".to_owned();
            e.info.error = None;
            e.info.result = None;
            if !e.info.live_text.trim().is_empty() {
                e.info.live_text.push_str("\n\n---\n\n");
            }
            let saved = e.saved_config.clone();
            let run_args = SubagentRunArgs {
                id: id.to_owned(),
                role: saved.role,
                prompt: msg,
                readonly: saved.readonly,
                max_turns: saved.max_turns,
                model_tier: saved.model_tier,
                isolation: saved.isolation,
                output_format: saved.output_format,
                max_result_chars: saved.max_result_chars,
                fork_messages: None,
                fork_channel_anchors: Vec::new(),
                max_tokens: saved.max_tokens,
                timeout_ms: saved.timeout_ms,
                resume_messages: Some(e.saved_conversation.clone()),
                existing_worktree: e.worktree.clone(),
                cancel: cancel.clone(),
                pause: pause.clone(),
                steer,
                registry: self.clone(),
            };
            (spawner, run_args, cancel, pause, done_rx)
        };
        self.emit_change();
        let (spawner, run_args, cancel, pause, done_rx) = relaunch;
        self.spawn_entry_task(
            id.to_owned(),
            spawner,
            run_args,
            cancel,
            pause,
            done_rx,
            parent_cancel,
        );
        Ok(SubagentResumeOutcome::Relaunched)
    }

    /// Sends an intervention message to a subagent without needing to pause it.
    /// If the subagent IS paused, automatically resumes it with the new instruction.
    pub fn send_message(
        &self,
        id: &str,
        message: String,
        delivery: SubagentDelivery,
    ) -> Result<(), String> {
        let trimmed = message.trim().to_owned();
        if trimmed.is_empty() {
            return Err("Mensagem vazia para o subagente.".to_owned());
        }
        let slots_free = self.slots.available_permits() > 0;
        {
            let mut guard = lock_mutex(&self.inner);
            let e = guard
                .entries
                .iter_mut()
                .find(|e| e.info.id == id)
                .ok_or_else(|| format!("Subagente \"{id}\" não encontrado."))?;
            if e.info.status.is_terminal() {
                return Err(format!("Subagente \"{id}\" já terminou."));
            }
            if e.closing {
                return Err(format!("Subagente \"{id}\" está concluindo; continue com resume e 'message'."));
            }
            match delivery {
                SubagentDelivery::Steer => {
                    if !e.steer.push(trimmed) {
                        return Err("Caixa de intervenção do subagente está cheia.".to_owned());
                    }
                    e.info.pending_steers = e.steer.len();
                }
                SubagentDelivery::Queue => {
                    if e.followup_queue.len() >= 8 {
                        return Err("Fila do subagente está cheia.".to_owned());
                    }
                    e.followup_queue.push_back(trimmed);
                    e.info.queued_messages = e.followup_queue.len();
                }
            }
            // Auto-resume when intervening on a paused subagent!
            if e.pause.is_paused() {
                if let Some(paused_at) = e.paused_at.take() {
                    e.paused_duration = e
                        .paused_duration
                        .saturating_add(paused_at.elapsed());
                }
                e.pause.resume();
                if e.has_slot || slots_free {
                    e.info.status = SubagentStatus::Running;
                    e.info.activity = "Retomando com nova instrução…".to_owned();
                } else {
                    e.info.status = SubagentStatus::Queued;
                    e.info.activity = "Aguardando vaga…".to_owned();
                }
            }
        }
        self.emit_change();
        Ok(())
    }

    pub fn cancel(&self, id: &str) -> Result<(), String> {
        {
            let mut guard = lock_mutex(&self.inner);
            let e = guard
                .entries
                .iter_mut()
                .find(|e| e.info.id == id)
                .ok_or_else(|| format!("Subagente \"{id}\" não encontrado."))?;
            if e.info.status.is_terminal() {
                return Ok(());
            }
            e.pause.resume();
            e.cancel.cancel();
            e.info.activity = "Cancelando…".to_owned();
        }
        self.emit_change();
        Ok(())
    }

    pub fn cancel_all(&self) {
        let mut any = false;
        {
            let mut guard = lock_mutex(&self.inner);
            for e in &mut guard.entries {
                if !e.info.status.is_terminal() {
                    e.pause.resume();
                    e.cancel.cancel();
                    any = true;
                }
            }
        }
        if any {
            self.emit_change();
        }
    }

    /// Waits for every subagent in `ids` at once, in the given order. A user message arriving in
    /// `inbox` releases the parent: whoever has not finished keeps running and comes back as
    /// `Released`, so the parent can answer the message instead of sitting blocked.
    pub async fn wait_all(
        &self,
        ids: &[String],
        parent_cancel: Option<&CancellationToken>,
        inbox: Option<&SteerInbox>,
    ) -> Vec<SubagentWait> {
        let mut join_set = tokio::task::JoinSet::new();
        for (pos, id) in ids.iter().enumerate() {
            let (reg, id, cancel) = (self.clone(), id.clone(), parent_cancel.cloned());
            join_set.spawn(async move { (pos, reg.wait_one(&id, cancel.as_ref()).await) });
        }
        let message = async {
            match inbox {
                Some(inbox) => inbox.arrived().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(message);
        let mut waited: Vec<Option<SubagentWait>> = vec![None; ids.len()];
        let mut released = false;
        loop {
            tokio::select! {
                // A subagent that finished in the same instant still reports its result.
                biased;
                joined = join_set.join_next() => match joined {
                    Some(Ok((pos, info))) => {
                        waited[pos] = Some(info.map_or(SubagentWait::Gone, |info| SubagentWait::Done(Box::new(info))));
                    }
                    Some(Err(_)) => {}
                    None => break,
                },
                _ = &mut message => {
                    released = true;
                    break;
                }
            }
        }
        // Dropping the set only stops the waiters; the subagents themselves keep running.
        let pending = if released { SubagentWait::Released } else { SubagentWait::Gone };
        waited.into_iter().map(|w| w.unwrap_or_else(|| pending.clone())).collect()
    }

    /// Waits until the specified subagent completes (or parent cancellation triggers).
    pub async fn wait_one(
        &self,
        id: &str,
        parent_cancel: Option<&CancellationToken>,
    ) -> Option<SubagentInfo> {
        let mut rx = {
            let guard = lock_mutex(&self.inner);
            let e = guard.entries.iter().find(|e| e.info.id == id)?;
            if e.info.status.is_terminal() {
                return Some(e.snapshot());
            }
            e.done_tx.subscribe()
        };
        match parent_cancel {
            Some(cancel) => {
                tokio::select! {
                    _ = cancel.cancelled() => self.get(id),
                    _ = rx.wait_for(|&done| done) => self.get(id),
                }
            }
            None => {
                let _ = rx.wait_for(|&done| done).await;
                self.get(id)
            }
        }
    }

    fn uncollected_or_active_ids(&self) -> Vec<String> {
        let guard = lock_mutex(&self.inner);
        guard
            .entries
            .iter()
            .filter(|e| e.info.status.is_active() || !e.collected)
            .map(|e| e.info.id.clone())
            .collect()
    }

    /// Marks the subagent as closing when no steer or queued message waits for
    /// it. `false`: something arrived after the caller's last drain.
    fn close_if_idle(&self, id: &str) -> bool {
        let mut guard = lock_mutex(&self.inner);
        let Some(e) = guard.entries.iter_mut().find(|e| e.info.id == id) else {
            return true;
        };
        e.closing = e.steer.is_empty() && e.followup_queue.is_empty();
        e.closing
    }

    /// Finished subagents the parent has neither collected nor been told
    /// about, as `id (role)`; each is reported once.
    pub(super) fn take_unannounced_terminal(&self) -> Vec<String> {
        let mut guard = lock_mutex(&self.inner);
        guard
            .entries
            .iter_mut()
            .filter(|e| e.info.status.is_terminal() && !e.collected && !e.announced)
            .map(|e| {
                e.announced = true;
                format!("{} ({})", e.info.id, e.info.role)
            })
            .collect()
    }

    fn is_uncollected_terminal(&self, id: &str) -> bool {
        let guard = lock_mutex(&self.inner);
        guard
            .entries
            .iter()
            .find(|e| e.info.id == id)
            .is_some_and(|e| e.info.status.is_terminal() && !e.collected)
    }
}

fn short_tool_target(name: &str, arguments: &str) -> String {
    let Ok(val) = serde_json::from_str::<Value>(arguments) else {
        return String::new();
    };
    let pick = match name {
        "read" | "write" | "patch" | "list" => val.get("path").and_then(Value::as_str),
        "search" => val
            .get("query")
            .or_else(|| val.get("pattern"))
            .and_then(Value::as_str),
        "shell" => val.get("command").and_then(Value::as_str),
        "code_intel" => val
            .get("symbol")
            .or_else(|| val.get("path"))
            .and_then(Value::as_str),
        _ => val
            .get("path")
            .or_else(|| val.get("command"))
            .or_else(|| val.get("name"))
            .and_then(Value::as_str),
    };
    pick.map(|s| s.lines().next().unwrap_or("").trim().chars().take(64).collect())
        .unwrap_or_default()
}

pub(super) fn subagent_definition() -> Value {
    json!({
        "name": "subagent",
        "description": "Spawn an isolated ad-hoc subagent using the conversation's model and permissions by default (up to 6 run in parallel when called in the same turn or with background: true). Supports explicit model_tier routing, git worktree isolation, context forking, output formatting/caps, and token/time budgets.",
        "input_schema": {
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "description": "Short 2-5 word role title shown live in the UI (e.g. 'Auth audit', 'Inspect parser')."
                },
                "prompt": {
                    "type": "string",
                    "description": "Self-contained brief (without fork_context the subagent sees nothing else): goal, what is already known (paths, what was ruled out), scope limits, done criteria and the output to return."
                },
                "readonly": {
                    "type": "boolean",
                    "description": "Optional. If true, restricts this subagent to read-only inspection; defaults to false (inherits conversation permissions)."
                },
                "background": {
                    "type": "boolean",
                    "description": "Optional. If true, returns the subagent id immediately so you can continue working and manage it with subagent_control; defaults to false (waits for completion, still parallel with sibling subagent calls in the same turn)."
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional turn budget for this subagent (uncapped by default)."
                },
                "model_tier": {
                    "type": "string",
                    "enum": ["same", "fast", "deep"],
                    "description": "Optional model tier: 'same' (default, keeps the conversation's active model even when readonly=true), 'fast' (fast sibling model with automatic fallback), or 'deep' (raises reasoning effort on supported adapters)."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["shared", "worktree"],
                    "description": "Optional filesystem isolation: 'shared' (default) or 'worktree' (creates an isolated detached git worktree; auto-removed if clean or preserved with diff summary if modified)."
                },
                "output_format": {
                    "type": "string",
                    "enum": ["markdown", "json", "diff_only"],
                    "description": "Optional output contract: 'markdown' (default), 'json' (valid JSON only), or 'diff_only' (unified diff or terse path:line findings)."
                },
                "max_result_chars": {
                    "type": "integer",
                    "minimum": 500,
                    "maximum": 64000,
                    "description": "Optional character cap on the returned result (500..=64000, default 12000). Oversized outputs spill full text to artifact storage."
                },
                "fork_context": {
                    "type": "boolean",
                    "description": "Optional. If true, seeds the subagent with the sanitized conversation prefix before the active tool call so it inherits context and hits the warm prompt cache. Defaults to false."
                },
                "max_tokens": {
                    "type": "integer",
                    "minimum": 1000,
                    "description": "Optional cumulative token budget (minimum 1000) before asking the subagent for a concise final answer."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 5000,
                    "maximum": 1800000,
                    "description": "Optional wall-clock timeout in milliseconds (5000..=1800000) before asking the subagent for a concise final answer."
                }
            },
            "required": ["role", "prompt"],
            "additionalProperties": false
        }
    })
}

pub(super) fn subagent_control_definition() -> Value {
    json!({
        "name": "subagent_control",
        "description": "Inspect or intervene in subagents in real time: check status, wait for completion, pause, resume (both paused subagents and completed/failed subagents via id + message reusing saved transcript), send a message ('steer' or 'queue'), or cancel.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "wait", "pause", "resume", "message", "cancel"],
                    "description": "Action to perform. 'resume' unpauses an active subagent or continues an already-finished one when 'message' is provided."
                },
                "id": {
                    "type": "string",
                    "description": "Subagent id (e.g. 'sub-1'). Required for pause, resume, message, cancel; optional for status/wait (defaults to active or uncollected subagents)."
                },
                "message": {
                    "type": "string",
                    "description": "Message to send for action 'message', or follow-up prompt when resuming a finished subagent with action 'resume'."
                },
                "delivery": {
                    "type": "string",
                    "enum": ["steer", "queue"],
                    "description": "How to deliver the message: 'steer' (immediate at next step boundary, default) or 'queue' (run after current turn finishes). Auto-resumes if paused."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional timeout in milliseconds for action 'wait'. Returns finished results and current status of still-running subagents without cancelling them."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }
    })
}

#[derive(Deserialize)]
pub(super) struct SubagentInput {
    role: String,
    prompt: String,
    #[serde(default)]
    pub(super) readonly: bool,
    #[serde(default)]
    pub(super) background: bool,
    #[serde(default)]
    max_turns: Option<usize>,
    #[serde(default)]
    model_tier: SubagentModelTier,
    #[serde(default)]
    isolation: SubagentIsolation,
    #[serde(default)]
    output_format: SubagentOutputFormat,
    #[serde(default)]
    max_result_chars: Option<usize>,
    #[serde(default)]
    fork_context: bool,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl SubagentInput {
    pub(super) fn into_spawn_request(
        self,
        active_fork: Option<&[ProviderMessage]>,
    ) -> SubagentSpawnRequest {
        self.into_spawn_request_with_anchors(active_fork, &[])
    }

    pub(super) fn into_spawn_request_with_anchors(
        self,
        active_fork: Option<&[ProviderMessage]>,
        anchors: &[(usize, crate::OperatingMode, String)],
    ) -> SubagentSpawnRequest {
        SubagentSpawnRequest {
            role: self.role,
            prompt: self.prompt,
            readonly: self.readonly,
            max_turns: self.max_turns.unwrap_or(DEFAULT_SUBAGENT_MAX_TURNS),
            model_tier: self.model_tier,
            isolation: self.isolation,
            output_format: self.output_format,
            max_result_chars: self.max_result_chars,
            fork_context: self.fork_context,
            fork_messages: self
                .fork_context
                .then(|| active_fork.map(<[ProviderMessage]>::to_vec))
                .flatten(),
            fork_channel_anchors: if self.fork_context {
                anchors.to_vec()
            } else {
                Vec::new()
            },
            max_tokens: self.max_tokens,
            timeout_ms: self.timeout_ms,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct SubagentControlInput {
    pub(super) action: String,
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) message: Option<String>,
    #[serde(default)]
    pub(super) delivery: Option<String>,
    #[serde(default)]
    pub(super) timeout_ms: Option<u64>,
}

pub(super) async fn execute_subagent_request(
    registry: &SubagentRegistry,
    req: SubagentSpawnRequest,
    background: bool,
    parent_cancel: Option<&CancellationToken>,
    steer_inbox: Option<&SteerInbox>,
) -> Result<String, String> {
    let id = registry.spawn(req, parent_cancel.cloned())?;
    if background {
        return Ok(format!(
            "Subagente {id} iniciado em segundo plano. Use subagent_control para acompanhar, intervir ou aguardar."
        ));
    }
    let ids = [id];
    let waited = registry
        .wait_all(&ids, parent_cancel, steer_inbox)
        .await;
    if matches!(waited[0], SubagentWait::Done(_)) {
        registry.mark_collected(&ids);
    }
    subagent_wait_result(&ids[0], &waited[0])
}

pub(super) async fn execute_subagent_control_action(
    registry: &SubagentRegistry,
    parsed: SubagentControlInput,
    parent_cancel: Option<CancellationToken>,
    steer_inbox: Option<&SteerInbox>,
) -> Result<String, String> {
    match parsed.action.as_str() {
        "status" => {
            let list = match parsed.id.as_deref() {
                Some(id) => registry.get(id).into_iter().collect::<Vec<_>>(),
                None => registry.list(),
            };
            if list.is_empty() {
                return Ok("Nenhum subagente encontrado.".to_owned());
            }
            let lines: Vec<String> = list
                .iter()
                .map(|s| {
                    let tok_note = if s.input_tokens > 0 || s.output_tokens > 0 {
                        if s.cache_read_tokens > 0 {
                            format!(
                                " · in={} out={} cache_read={}",
                                s.input_tokens, s.output_tokens, s.cache_read_tokens
                            )
                        } else {
                            format!(" · in={} out={}", s.input_tokens, s.output_tokens)
                        }
                    } else {
                        String::new()
                    };
                    let ready_note = if registry.is_uncollected_terminal(&s.id) {
                        " · resultado pronto (use wait)"
                    } else {
                        ""
                    };
                    let model_note = s
                        .model
                        .as_deref()
                        .map(|m| format!(" · model={m}"))
                        .unwrap_or_default();
                    let wt_note = s
                        .worktree_path
                        .as_deref()
                        .map(|wt| format!(" · worktree={wt}"))
                        .unwrap_or_default();
                    format!(
                        "• {} ({}) · {:?} · {} passos · {} ms{}{}{}{} · {}",
                        s.id,
                        s.role,
                        s.status,
                        s.steps.len(),
                        s.elapsed_ms,
                        tok_note,
                        model_note,
                        wt_note,
                        ready_note,
                        s.activity
                    )
                })
                .collect();
            Ok(lines.join("\n"))
        }
        "wait" => {
            let explicit_id = parsed.id.is_some();
            let ids: Vec<String> = match parsed.id {
                Some(id) => vec![id],
                None => registry.uncollected_or_active_ids(),
            };
            if ids.is_empty() {
                return Ok("Nenhum subagente ativo ou pendente de coleta.".to_owned());
            }
            let (waited, timed_out) = match parsed.timeout_ms.filter(|&ms| ms > 0) {
                Some(ms) => match tokio::time::timeout(
                    Duration::from_millis(ms),
                    registry.wait_all(&ids, parent_cancel.as_ref(), steer_inbox),
                )
                .await
                {
                    Ok(w) => (w, false),
                    Err(_) => {
                        let snap = ids
                            .iter()
                            .map(|id| match registry.get(id) {
                                Some(info) if info.status.is_terminal() => {
                                    SubagentWait::Done(Box::new(info))
                                }
                                Some(_) => SubagentWait::Released,
                                None => SubagentWait::Gone,
                            })
                            .collect();
                        (snap, true)
                    }
                },
                None => (
                    registry
                        .wait_all(&ids, parent_cancel.as_ref(), steer_inbox)
                        .await,
                    false,
                ),
            };
            let done_ids: Vec<String> = ids
                .iter()
                .zip(&waited)
                .filter_map(|(id, w)| matches!(w, SubagentWait::Done(_)).then(|| id.clone()))
                .collect();
            registry.mark_collected(&done_ids);
            if explicit_id && waited.len() == 1 && matches!(waited[0], SubagentWait::Gone) {
                return Err(format!("Subagente \"{}\" não encontrado.", ids[0]));
            }
            let parts: Vec<String> = ids
                .iter()
                .zip(&waited)
                .filter(|(_, w)| !matches!(w, SubagentWait::Gone))
                .map(|(id, w)| match w {
                    SubagentWait::Released if timed_out => match registry.get(id) {
                        Some(info) => format!(
                            "[subagent {} · {}] ainda ativo após timeout ({:?} · {} passos · {} ms · {}); segue rodando em segundo plano.",
                            info.id,
                            info.role,
                            info.status,
                            info.steps.len(),
                            info.elapsed_ms,
                            info.activity
                        ),
                        None => format!("Subagente {id} desapareceu."),
                    },
                    _ => subagent_wait_result(id, w).unwrap_or_else(|text| text),
                })
                .collect();
            Ok(parts.join("\n\n"))
        }
        "pause" => match parsed.id.as_deref() {
            Some(id) => registry.pause(id).map(|()| format!("Subagente {id} pausado.")),
            None => Err("Informe o id do subagente para pausar.".to_owned()),
        },
        "resume" => match parsed.id.as_deref() {
            Some(id) => match registry.resume_with_message(id, parsed.message, parent_cancel)? {
                SubagentResumeOutcome::Unpaused => Ok(format!("Subagente {id} retomado.")),
                SubagentResumeOutcome::Steered => Ok(format!(
                    "Subagente {id} já ativo; instrução entregue via steer."
                )),
                SubagentResumeOutcome::AlreadyActive => {
                    Ok(format!("Subagente {id} já está ativo."))
                }
                SubagentResumeOutcome::Relaunched => Ok(format!(
                    "Subagente {id} retomado em segundo plano com o histórico salvo. Use subagent_control(action='wait', id='{id}') para aguardar o resultado."
                )),
            },
            None => Err("Informe o id do subagente para retomar.".to_owned()),
        },
        "message" => {
            let delivery = match parsed.delivery.as_deref() {
                Some("queue") => SubagentDelivery::Queue,
                _ => SubagentDelivery::Steer,
            };
            match (parsed.id.as_deref(), parsed.message) {
                (Some(id), Some(msg)) => {
                    registry.send_message(id, msg, delivery)?;
                    let kind = if delivery == SubagentDelivery::Queue {
                        "colocada na fila de"
                    } else {
                        "enviada (steer) para"
                    };
                    Ok(format!("Mensagem {kind} {id}."))
                }
                _ => Err("Informe id e message para enviar uma mensagem ao subagente.".to_owned()),
            }
        }
        "cancel" => match parsed.id.as_deref() {
            Some(id) => registry
                .cancel(id)
                .map(|()| format!("Subagente {id} cancelado.")),
            None => Err("Informe o id do subagente para cancelar.".to_owned()),
        },
        other => Err(format!("Ação desconhecida: {other}")),
    }
}

fn format_subagent_outcome(info: &SubagentInfo) -> (bool, String) {
    match info.status {
        SubagentStatus::Completed => {
            let body = info
                .result
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("(subagente concluiu sem texto de saída)");
            (
                true,
                format!(
                    "[subagent {} · {} · {} passos · {} ms]\n{}",
                    info.id,
                    info.role,
                    info.steps.len(),
                    info.elapsed_ms,
                    body
                ),
            )
        }
        SubagentStatus::Cancelled => (
            false,
            format!(
                "[subagent {} · {}] cancelado ({})",
                info.id,
                info.role,
                info.error.as_deref().unwrap_or("cancelado")
            ),
        ),
        SubagentStatus::Failed => (
            false,
            format!(
                "[subagent {} · {}] falhou: {}",
                info.id,
                info.role,
                info.error.as_deref().unwrap_or("erro desconhecido")
            ),
        ),
        other => (
            true,
            format!(
                "[subagent {} · {}] status={other:?} · {}",
                info.id, info.role, info.activity
            ),
        ),
    }
}

impl Runtime {
    pub fn set_subagents(&mut self, registry: SubagentRegistry) {
        self.subagents = Some(registry);
    }

    pub fn set_pause_gate(&mut self, gate: PauseGate) {
        self.pause_gate = Some(gate);
    }

    pub(super) async fn execute_subagent(
        &mut self,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut following = next_seq;
        let begun = self.begin_tool(invocation, &mut following)?;
        let Some(registry) = self.subagents.clone() else {
            let mut result = ToolResult::fail(
                invocation.name,
                "Subagentes não estão habilitados nesta execução.",
            );
            self.finish_tool(invocation, &mut result, begun, &mut following, None)?;
            return Ok((result, following));
        };
        if self.subagent_depth > 0 {
            let mut result = ToolResult::fail(
                invocation.name,
                "Subagentes não podem criar sub-subagentes (profundidade máxima = 1).",
            );
            self.finish_tool(invocation, &mut result, begun, &mut following, None)?;
            return Ok((result, following));
        }
        let parsed: SubagentInput = match serde_json::from_str(invocation.arguments) {
            Ok(v) => v,
            Err(err) => {
                let mut result =
                    ToolResult::fail(invocation.name, format!("Argumentos inválidos: {err}"));
                self.finish_tool(invocation, &mut result, begun, &mut following, None)?;
                return Ok((result, following));
            }
        };
        let background = parsed.background;
        let req = parsed.into_spawn_request_with_anchors(
            self.active_fork_context.as_deref(),
            &self.active_fork_channel_anchors,
        );
        let parent_cancel = self.cancellation.clone();
        let mut result = match execute_subagent_request(
            &registry,
            req,
            background,
            parent_cancel.as_ref(),
            self.steer_inbox.as_ref(),
        )
        .await
        {
            Ok(text) => ToolResult::ok(invocation.name, text),
            Err(text) => ToolResult::fail(invocation.name, text),
        };
        self.finish_tool(invocation, &mut result, begun, &mut following, None)?;
        Ok((result, following))
    }

    /// Runs a cluster of 2+ `subagent` calls from the same tool batch in true parallel!
    pub(super) async fn execute_subagent_cluster(
        &mut self,
        batch_id: &str,
        calls: &[ProviderToolCall],
        indices: &[usize],
        next_seq: &mut u64,
    ) -> Result<Vec<(usize, ToolResult)>, ProviderError> {
        self.ensure_not_cancelled()?;
        let registry = self.subagents.clone();
        let parent_cancel = self.cancellation.clone();
        let depth = self.subagent_depth;
        let active_fork = self.active_fork_context.clone();
        let active_anchors = self.active_fork_channel_anchors.clone();

        let mut begun_list = Vec::with_capacity(indices.len());
        let mut spawned: Vec<(usize, Result<(String, bool), String>)> =
            Vec::with_capacity(indices.len());

        // 1. Begin every tool call and spawn all subagents immediately so all run concurrently.
        for &idx in indices {
            let call = &calls[idx];
            let inv = ToolInvocation::provider(batch_id, call);
            let begun = self.begin_tool(inv, next_seq)?;
            begun_list.push((idx, begun));

            if depth > 0 {
                spawned.push((
                    idx,
                    Err("Subagentes não podem criar sub-subagentes (profundidade máxima = 1)."
                        .to_owned()),
                ));
                continue;
            }
            let Some(reg) = registry.as_ref() else {
                spawned.push((
                    idx,
                    Err("Subagentes não estão habilitados nesta execução.".to_owned()),
                ));
                continue;
            };
            match serde_json::from_str::<SubagentInput>(&call.arguments) {
                Ok(parsed) => {
                    let background = parsed.background;
                    let req = parsed.into_spawn_request_with_anchors(
                        active_fork.as_deref(),
                        &active_anchors,
                    );
                    match reg.spawn(req, parent_cancel.clone()) {
                        Ok(id) => spawned.push((idx, Ok((id, background)))),
                        Err(err) => spawned.push((idx, Err(err))),
                    }
                }
                Err(err) => spawned.push((idx, Err(format!("Argumentos inválidos: {err}")))),
            }
        }

        // 2. Await all foreground subagents concurrently (a user message releases the wait).
        let foreground: Vec<(usize, String)> = spawned
            .iter()
            .enumerate()
            .filter_map(|(pos, (_, outcome))| match outcome {
                Ok((id, false)) => Some((pos, id.clone())),
                _ => None,
            })
            .collect();
        let mut waited: Vec<SubagentWait> = vec![SubagentWait::Gone; spawned.len()];
        if let Some(reg) = registry.as_ref() {
            let ids: Vec<String> = foreground.iter().map(|(_, id)| id.clone()).collect();
            let done = reg
                .wait_all(&ids, parent_cancel.as_ref(), self.steer_inbox.as_ref())
                .await;
            let mut done_ids = Vec::new();
            for ((pos, id), w) in foreground.iter().zip(done) {
                if matches!(w, SubagentWait::Done(_)) {
                    done_ids.push(id.clone());
                }
                waited[*pos] = w;
            }
            reg.mark_collected(&done_ids);
        }

        // 3. Emit finish events in deterministic batch order.
        let mut results = Vec::with_capacity(indices.len());
        for ((pos, (idx, outcome)), (_, begun)) in
            spawned.into_iter().enumerate().zip(begun_list.into_iter())
        {
            let call = &calls[idx];
            let inv = ToolInvocation::provider(batch_id, call);
            let mut result = match outcome {
                Err(err) => ToolResult::fail(inv.name, err),
                Ok((id, true)) => ToolResult::ok(
                    inv.name,
                    format!("Subagente {id} iniciado em segundo plano. Use subagent_control para acompanhar, intervir ou aguardar."),
                ),
                Ok((id, false)) => match subagent_wait_result(&id, &waited[pos]) {
                    Ok(text) => ToolResult::ok(inv.name, text),
                    Err(text) => ToolResult::fail(inv.name, text),
                },
            };
            self.finish_tool(inv, &mut result, begun, next_seq, None)?;
            results.push((idx, result));
        }

        Ok(results)
    }

    pub(super) async fn execute_subagent_control(
        &mut self,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut following = next_seq;
        let begun = self.begin_tool(invocation, &mut following)?;
        let Some(registry) = self.subagents.clone() else {
            let mut result = ToolResult::fail(
                invocation.name,
                "Subagentes não estão habilitados nesta execução.",
            );
            self.finish_tool(invocation, &mut result, begun, &mut following, None)?;
            return Ok((result, following));
        };
        let parent_cancel = self.cancellation.clone();
        let mut result = match serde_json::from_str::<SubagentControlInput>(invocation.arguments) {
            Ok(parsed) => match execute_subagent_control_action(
                &registry,
                parsed,
                parent_cancel,
                self.steer_inbox.as_ref(),
            )
            .await
            {
                Ok(text) => ToolResult::ok(invocation.name, text),
                Err(err) => ToolResult::fail(invocation.name, err),
            },
            Err(err) => ToolResult::fail(invocation.name, format!("Argumentos inválidos: {err}")),
        };
        self.finish_tool(invocation, &mut result, begun, &mut following, None)?;
        Ok((result, following))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct ScriptedSpawner {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl SubagentSpawner for ScriptedSpawner {
        fn run(
            &self,
            args: SubagentRunArgs,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>> {
            let active = Arc::clone(&self.active);
            let peak = Arc::clone(&self.peak);
            let delay = self.delay;
            Box::pin(async move {
                let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                let mut parts = vec![format!("prompt:{}", args.prompt)];
                loop {
                    if !args.pause.wait_if_paused(&args.cancel).await {
                        active.fetch_sub(1, Ordering::SeqCst);
                        return Err("cancelled".into());
                    }
                    tokio::select! {
                        _ = args.cancel.cancelled() => {
                            active.fetch_sub(1, Ordering::SeqCst);
                            return Err("cancelled".into());
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                    if !args.pause.wait_if_paused(&args.cancel).await {
                        active.fetch_sub(1, Ordering::SeqCst);
                        return Err("cancelled".into());
                    }
                    for s in args.steer.drain() {
                        parts.push(format!("steer:{s}"));
                    }
                    if let Some(q) = args.registry.pop_queued_message(&args.id) {
                        parts.push(format!("queue:{q}"));
                        continue;
                    }
                    break;
                }
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(parts.join("|"))
            })
        }
    }

    #[tokio::test]
    async fn a_user_message_releases_the_wait_and_the_subagents_keep_running() {
        let reg = SubagentRegistry::new();
        reg.set_spawner(
            Arc::new(ScriptedSpawner {
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(150),
            }),
            crate::OperatingMode::Auto,
        );
        let ids: Vec<String> = (0..2)
            .map(|i| {
                let req = SubagentSpawnRequest {
                    role: format!("Worker {i}"),
                    prompt: format!("task-{i}"),
                    readonly: false,
                    max_turns: 5,
                    ..SubagentSpawnRequest::default()
                };
                reg.spawn(req, None).unwrap()
            })
            .collect();

        // The user writes while the parent waits: the wait ends, nobody is cancelled.
        let inbox = SteerInbox::default();
        tokio::spawn({
            let inbox = inbox.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                inbox.push("e agora?".into());
            }
        });
        let waited = reg.wait_all(&ids, None, Some(&inbox)).await;
        assert!(waited.iter().all(|w| matches!(w, SubagentWait::Released)), "{waited:?}");
        assert!(ids.iter().all(|id| reg.get(id).unwrap().status.is_active()));
        let text = subagent_wait_result(&ids[0], &waited[0]).unwrap();
        assert!(text.contains(&ids[0]) && text.contains("subagent_control"), "{text}");
        // The message is still there for the loop to deliver.
        assert_eq!(inbox.drain(), vec!["e agora?".to_owned()]);

        // Waiting again (inbox empty) collects every result, in the order asked.
        let waited = reg.wait_all(&ids, None, Some(&inbox)).await;
        for (i, w) in waited.iter().enumerate() {
            let SubagentWait::Done(info) = w else { panic!("{w:?}") };
            assert_eq!(info.status, SubagentStatus::Completed);
            assert_eq!(info.id, ids[i]);
            assert!(subagent_wait_result(&ids[i], w).unwrap().contains(&format!("task-{i}")));
        }
        // Without an inbox nothing releases the wait; an unknown id is reported as gone.
        let gone = reg.wait_all(&["nope".to_owned()], None, None).await;
        assert!(matches!(gone[0], SubagentWait::Gone));
        assert!(subagent_wait_result("nope", &gone[0]).is_err());
    }

    #[tokio::test]
    async fn subagent_registry_runs_up_to_six_in_parallel_and_supports_pause_steer_queue_cancel() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let reg = SubagentRegistry::new();
        reg.set_spawner(
            Arc::new(ScriptedSpawner {
                active: Arc::clone(&active),
                peak: Arc::clone(&peak),
                delay: Duration::from_millis(60),
            }),
            crate::OperatingMode::Auto,
        );

        let mut ids = Vec::new();
        for i in 0..7 {
            let id = reg
                .spawn(
                    SubagentSpawnRequest {
                        role: format!("Worker {i}"),
                        prompt: format!("task-{i}"),
                        readonly: false,
                        max_turns: 5,
                        ..SubagentSpawnRequest::default()
                    },
                    None,
                )
                .unwrap();
            ids.push(id);
        }

        // Pause #0, send steer message (auto-resumes!) + queue message; cancel #1.
        reg.pause(&ids[0]).unwrap();
        assert_eq!(reg.get(&ids[0]).unwrap().status, SubagentStatus::Paused);
        reg.send_message(&ids[0], "foco".into(), SubagentDelivery::Steer)
            .unwrap();
        assert_eq!(reg.get(&ids[0]).unwrap().status, SubagentStatus::Running);
        reg.send_message(&ids[0], "depois".into(), SubagentDelivery::Queue)
            .unwrap();
        reg.cancel(&ids[1]).unwrap();

        let mut done = Vec::new();
        for id in &ids {
            done.push(reg.wait_one(id, None).await.unwrap());
        }
        assert!(peak.load(Ordering::SeqCst) <= MAX_CONCURRENT_SUBAGENTS);
        assert!(peak.load(Ordering::SeqCst) >= 2, "deve rodar em paralelo");
        assert_eq!(done[0].status, SubagentStatus::Completed);
        let r0 = done[0].result.as_deref().unwrap();
        assert!(r0.contains("steer:foco") && r0.contains("queue:depois"), "r0={r0}");
        assert_eq!(done[1].status, SubagentStatus::Cancelled);
        for d in &done[2..] {
            assert_eq!(d.status, SubagentStatus::Completed);
        }
    }

    #[tokio::test]
    async fn wait_one_does_not_lose_immediate_completion_and_reset_clears_entries() {
        let reg = SubagentRegistry::new();
        reg.set_spawner(
            Arc::new(ScriptedSpawner {
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(0),
            }),
            crate::OperatingMode::Auto,
        );
        let id = reg
            .spawn(
                SubagentSpawnRequest {
                    role: "Fast".into(),
                    prompt: "quick".into(),
                    readonly: true,
                    max_turns: 2,
                    ..SubagentSpawnRequest::default()
                },
                None,
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        let info = tokio::time::timeout(Duration::from_millis(200), reg.wait_one(&id, None))
            .await
            .expect("must not deadlock on immediate completion")
            .unwrap();
        assert_eq!(info.status, SubagentStatus::Completed);
        assert!(!reg.uncollected_or_active_ids().is_empty());
        reg.mark_collected(std::slice::from_ref(&id));
        assert!(reg.uncollected_or_active_ids().is_empty());
        reg.reset();
        assert!(reg.list().is_empty());
    }

    #[tokio::test]
    async fn paused_queued_subagent_does_not_starve_slot_and_resumes_to_queued() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let reg = SubagentRegistry::new();
        reg.set_spawner(
            Arc::new(ScriptedSpawner {
                active: Arc::clone(&active),
                peak: Arc::clone(&peak),
                delay: Duration::from_millis(80),
            }),
            crate::OperatingMode::Auto,
        );
        for i in 0..MAX_CONCURRENT_SUBAGENTS {
            reg.spawn(
                SubagentSpawnRequest {
                    role: format!("Busy {i}"),
                    prompt: format!("busy-{i}"),
                    readonly: true,
                    max_turns: 2,
                    ..SubagentSpawnRequest::default()
                },
                None,
            )
            .unwrap();
        }
        // Let the 6 workers acquire all slots.
        tokio::time::sleep(Duration::from_millis(15)).await;
        let queued_id = reg
            .spawn(
                SubagentSpawnRequest {
                    role: "Queued".into(),
                    prompt: "q".into(),
                    readonly: true,
                    max_turns: 2,
                    ..SubagentSpawnRequest::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(reg.get(&queued_id).unwrap().status, SubagentStatus::Queued);
        reg.pause(&queued_id).unwrap();
        assert_eq!(reg.get(&queued_id).unwrap().status, SubagentStatus::Paused);
        // Resuming while all 6 slots are still busy restores Queued rather than faking Running.
        reg.resume(&queued_id).unwrap();
        assert_eq!(reg.get(&queued_id).unwrap().status, SubagentStatus::Queued);
        let done = reg.wait_one(&queued_id, None).await.unwrap();
        assert_eq!(done.status, SubagentStatus::Completed);
    }

    fn scripted_registry(delay_ms: u64) -> SubagentRegistry {
        let reg = SubagentRegistry::new();
        reg.set_spawner(
            Arc::new(ScriptedSpawner {
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(delay_ms),
            }),
            crate::OperatingMode::Auto,
        );
        reg
    }

    fn spawn_scripted(reg: &SubagentRegistry, prompt: &str) -> String {
        reg.spawn(
            SubagentSpawnRequest {
                role: prompt.into(),
                prompt: prompt.into(),
                readonly: true,
                max_turns: 2,
                ..SubagentSpawnRequest::default()
            },
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn paused_queued_subagents_give_their_slots_back() {
        let reg = scripted_registry(40);
        for i in 0..MAX_CONCURRENT_SUBAGENTS {
            spawn_scripted(&reg, &format!("busy-{i}"));
        }
        // Six more reach the wait for a slot, and are paused while they wait.
        let paused: Vec<String> = (0..MAX_CONCURRENT_SUBAGENTS)
            .map(|i| spawn_scripted(&reg, &format!("paused-{i}")))
            .collect();
        tokio::time::sleep(Duration::from_millis(10)).await;
        for id in &paused {
            reg.pause(id).unwrap();
        }
        let last = spawn_scripted(&reg, "last");
        // The busy ones end: the paused ones must not keep the slots they free.
        let done = tokio::time::timeout(Duration::from_millis(1500), reg.wait_one(&last, None))
            .await
            .expect("a paused subagent held a slot");
        assert_eq!(done.unwrap().status, SubagentStatus::Completed);
        reg.cancel_all();
    }

    #[tokio::test]
    async fn closing_subagent_refuses_late_messages() {
        let reg = scripted_registry(200);
        let id = spawn_scripted(&reg, "work");
        tokio::time::sleep(Duration::from_millis(10)).await;
        reg.send_message(&id, "one".into(), SubagentDelivery::Queue).unwrap();
        assert!(!reg.close_if_idle(&id), "a queued message keeps the run open");
        assert_eq!(reg.pop_queued_message(&id).as_deref(), Some("one"));
        assert!(reg.close_if_idle(&id));
        // Accepted now, it would never be read: the run already saw nothing left.
        assert!(reg.send_message(&id, "late".into(), SubagentDelivery::Steer).is_err());
        assert!(reg.resume_with_message(&id, Some("late".into()), None).is_err());
        reg.cancel_all();
    }

    #[tokio::test]
    async fn unannounced_terminal_subagents_are_reported_once() {
        let reg = scripted_registry(5);
        let id = spawn_scripted(&reg, "bg");
        assert!(reg.take_unannounced_terminal().is_empty(), "still working");
        for _ in 0..200 {
            if reg.get(&id).unwrap().status.is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(reg.take_unannounced_terminal(), vec![format!("{id} (bg)")]);
        assert!(reg.take_unannounced_terminal().is_empty());
    }

    #[test]
    fn needs_fork_context_only_when_a_call_names_it() {
        let call = |name: &str, arguments: &str| ProviderToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.into(),
        };
        assert!(needs_fork_context(&[call("subagent", r#"{"role":"A","prompt":"p","fork_context":true}"#)]));
        assert!(needs_fork_context(&[call(
            "codemode",
            r#"{"code":"await slim.subagent({role:'A',prompt:'p',fork_context:true})"}"#
        )]));
        assert!(!needs_fork_context(&[call("subagent", r#"{"role":"A","prompt":"p"}"#)]));
        assert!(!needs_fork_context(&[call("codemode", r#"{"code":"return await slim.read({path:'a'})"}"#)]));
        assert!(!needs_fork_context(&[call("read", r#"{"path":"fork_context"}"#)]));
    }

    #[tokio::test]
    async fn subagent_control_wait_collects_background_handles_unknown_id_and_timeout() {
        let mut rt = Runtime::new();
        let reg = SubagentRegistry::new();
        reg.set_spawner(
            Arc::new(ScriptedSpawner {
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(20),
            }),
            crate::OperatingMode::Auto,
        );
        rt.set_subagents(reg.clone());

        let fast_id = reg
            .spawn(
                SubagentSpawnRequest {
                    role: "BgDone".into(),
                    prompt: "bg-fast".into(),
                    readonly: true,
                    max_turns: 2,
                    ..SubagentSpawnRequest::default()
                },
                None,
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(reg.get(&fast_id).unwrap().status, SubagentStatus::Completed);

        let inv = |arguments: &'static str| ToolInvocation {
            batch_id: "b1",
            call_id: "c1",
            name: "subagent_control",
            arguments,
        };

        // Unknown id returns failure rather than empty Ok.
        let (missing, seq) = rt
            .execute_subagent_control(inv(r#"{"action":"wait","id":"nope"}"#), 1)
            .await
            .unwrap();
        assert!(!missing.success);

        // Wait with id=None collects the already-completed uncollected background result.
        let (collected, seq) = rt
            .execute_subagent_control(inv(r#"{"action":"wait"}"#), seq)
            .await
            .unwrap();
        assert!(collected.success);
        assert!(collected.output.contains("bg-fast"));

        // Second wait sees nothing left pending.
        let (empty, seq) = rt
            .execute_subagent_control(inv(r#"{"action":"wait"}"#), seq)
            .await
            .unwrap();
        assert!(empty.success);
        assert!(empty.output.contains("Nenhum subagente ativo ou pendente"));

        // Timeout reports active subagent without cancelling it.
        reg.set_spawner(
            Arc::new(ScriptedSpawner {
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(120),
            }),
            crate::OperatingMode::Auto,
        );
        let slow_id = reg
            .spawn(
                SubagentSpawnRequest {
                    role: "Slow".into(),
                    prompt: "slow".into(),
                    readonly: true,
                    max_turns: 2,
                    ..SubagentSpawnRequest::default()
                },
                None,
            )
            .unwrap();
        let slow_args = serde_json::json!({"action":"wait","id":slow_id,"timeout_ms":20}).to_string();
        let (timed, _) = rt
            .execute_subagent_control(
                ToolInvocation {
                    batch_id: "b1",
                    call_id: "c2",
                    name: "subagent_control",
                    arguments: &slow_args,
                },
                seq,
            )
            .await
            .unwrap();
        assert!(timed.success);
        assert!(timed.output.contains("timeout"));
        assert!(reg.get(&slow_id).unwrap().status.is_active());
        let done = reg.wait_one(&slow_id, None).await.unwrap();
        assert_eq!(done.status, SubagentStatus::Completed);
    }

    #[test]
    fn readonly_defaults_to_same_model_tier_and_only_explicit_fast_routes_fast() {
        let ro_default: SubagentInput =
            serde_json::from_str(r#"{"role":"Audit","prompt":"check","readonly":true}"#).unwrap();
        assert_eq!(ro_default.model_tier, SubagentModelTier::Same);
        let ro_req = ro_default.into_spawn_request(None);
        assert!(ro_req.readonly);
        assert_eq!(ro_req.model_tier, SubagentModelTier::Same);

        let ro_fast: SubagentInput = serde_json::from_str(
            r#"{"role":"Audit","prompt":"check","readonly":true,"model_tier":"fast"}"#,
        )
        .unwrap();
        assert_eq!(ro_fast.model_tier, SubagentModelTier::Fast);

        let deep: SubagentInput = serde_json::from_str(
            r#"{"role":"Plan","prompt":"design","model_tier":"deep","output_format":"json","isolation":"worktree","fork_context":true,"max_result_chars":2000,"max_tokens":4000,"timeout_ms":15000}"#,
        )
        .unwrap();
        assert_eq!(deep.model_tier, SubagentModelTier::Deep);
        assert_eq!(deep.output_format, SubagentOutputFormat::Json);
        assert_eq!(deep.isolation, SubagentIsolation::Worktree);
        assert!(deep.fork_context);
        assert_eq!(deep.max_result_chars, Some(2000));
        assert_eq!(deep.max_tokens, Some(4000));
        assert_eq!(deep.timeout_ms, Some(15_000));
    }

    #[test]
    fn sanitize_fork_context_strips_unsettled_trailing_tool_call() {
        let messages = vec![
            ProviderMessage::user("initial task"),
            ProviderMessage::assistant(
                "",
                vec![crate::provider::ProviderToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                }],
            ),
            ProviderMessage::tool("read", "c1", "contents"),
            ProviderMessage::assistant(
                "spawning subagent",
                vec![crate::provider::ProviderToolCall {
                    id: "c2".into(),
                    name: "subagent".into(),
                    arguments: r#"{"role":"worker","prompt":"go"}"#.into(),
                }],
            ),
        ];
        let sanitized = sanitize_fork_context(&messages);
        assert_eq!(sanitized.len(), 3);
        assert_eq!(sanitized.last().unwrap().role, "tool");
    }

    #[test]
    fn apply_result_limit_and_spill_persists_large_output_to_artifact_store() {
        let dir = std::env::temp_dir().join(format!("slim-subagent-spill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ArtifactStore::new(&dir).unwrap();
        let big = format!("HEAD-{}-TAIL", "x".repeat(2_000));
        let limited = apply_result_limit_and_spill(&big, 600, "sub-9", Some(&store));
        assert!(limited.chars().count() <= 600, "len={}", limited.chars().count());
        assert!(limited.contains("HEAD-"));
        assert!(limited.contains("-TAIL"));
        assert!(limited.contains("artifact_read"));
        let entry = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .expect("artifact file");
        let saved = std::fs::read_to_string(entry.path()).unwrap();
        assert_eq!(saved, big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_with_message_continues_completed_subagent_from_saved_conversation() {
        struct ResumeSpawner;
        impl SubagentSpawner for ResumeSpawner {
            fn run(
                &self,
                args: SubagentRunArgs,
            ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>> {
                Box::pin(async move {
                    let prev_count = args.resume_messages.as_ref().map_or(0, Vec::len);
                    let mut conv = args.resume_messages.unwrap_or_default();
                    conv.push(ProviderMessage::user(args.prompt.clone()));
                    let reply = format!("prev={prev_count}|prompt={}", args.prompt);
                    conv.push(ProviderMessage::assistant(reply.clone(), Vec::new()));
                    args.registry.save_conversation(&args.id, conv);
                    Ok(reply)
                })
            }
        }

        let reg = SubagentRegistry::new();
        reg.set_spawner(Arc::new(ResumeSpawner), crate::OperatingMode::Auto);
        let id = reg
            .spawn(
                SubagentSpawnRequest {
                    role: "Resumable".into(),
                    prompt: "step1".into(),
                    ..SubagentSpawnRequest::default()
                },
                None,
            )
            .unwrap();
        let first = reg.wait_one(&id, None).await.unwrap();
        assert_eq!(first.status, SubagentStatus::Completed);
        assert_eq!(first.result.as_deref(), Some("prev=0|prompt=step1"));

        // Resuming a terminal subagent without a follow-up message fails cleanly.
        assert!(reg.resume_with_message(&id, None, None).is_err());

        // Resuming with a message relaunches using saved conversation (2 prior messages).
        let outcome = reg
            .resume_with_message(&id, Some("step2".into()), None)
            .unwrap();
        assert_eq!(outcome, SubagentResumeOutcome::Relaunched);
        assert_eq!(reg.get(&id).unwrap().result, None, "the old result leaves the screen");
        let second = reg.wait_one(&id, None).await.unwrap();
        assert_eq!(second.status, SubagentStatus::Completed);
        assert_eq!(second.result.as_deref(), Some("prev=2|prompt=step2"));
    }

    #[tokio::test]
    async fn worktree_isolation_creates_isolates_and_cleans_empty_or_preserves_dirty_worktree() {
        let repo = std::env::temp_dir().join(format!("slim-wt-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        run_git_checked(&repo, &["init"]).await.unwrap();
        run_git_checked(&repo, &["config", "user.name", "test"])
            .await
            .unwrap();
        run_git_checked(&repo, &["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        run_git_checked(&repo, &["add", "."]).await.unwrap();
        run_git_checked(&repo, &["commit", "-m", "init"])
            .await
            .unwrap();

        let clean = create_subagent_worktree(&repo, "clean").await.unwrap();
        assert!(clean.wt_cwd.join("a.txt").exists());
        let (kept_clean, summary_clean) = finalize_subagent_worktree(&clean, true).await;
        assert!(!kept_clean);
        assert!(summary_clean.is_none());
        assert!(!clean.wt_root.exists());

        let dirty = create_subagent_worktree(&repo, "dirty").await.unwrap();
        std::fs::write(dirty.wt_cwd.join("b.txt"), "new change\n").unwrap();
        assert!(!repo.join("b.txt").exists());
        let (kept_dirty, summary_dirty) = finalize_subagent_worktree(&dirty, true).await;
        assert!(kept_dirty);
        assert!(dirty.wt_root.exists());
        assert!(summary_dirty.unwrap().contains("b.txt"));

        let wt_str = dirty.wt_root.to_string_lossy().into_owned();
        let _ = run_git_checked(&dirty.repo_toplevel, &["worktree", "remove", "--force", &wt_str])
            .await;
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn subagent_turns_are_uncapped_by_default_and_preserve_large_values() {
        assert_eq!(SubagentSpawnRequest::default().max_turns, usize::MAX);
        let default_req: SubagentInput =
            serde_json::from_str(r#"{"role":"Audit","prompt":"inspect"}"#).unwrap();
        assert_eq!(default_req.into_spawn_request(None).max_turns, usize::MAX);

        let large_req: SubagentInput =
            serde_json::from_str(r#"{"role":"Audit","prompt":"inspect","max_turns":5000}"#)
                .unwrap();
        assert_eq!(large_req.into_spawn_request(None).max_turns, 5000);
    }

    #[test]
    fn fork_runtime_preserves_frozen_tools_even_in_readonly_mode() {
        let mut runtime = Runtime::new();
        let frozen: Arc<[Value]> = Arc::from(vec![
            serde_json::json!({"name": "read", "description": "r", "parameters": {}}),
            serde_json::json!({"name": "edit", "description": "e", "parameters": {}}),
        ]);
        runtime.frozen_tools_override = Some(Arc::clone(&frozen));
        runtime.subagent_readonly = true;

        let resolved = runtime.workspace_tool_definitions_for(
            crate::OperatingMode::ReadOnly,
            std::path::Path::new("/tmp"),
            true,
        );
        let names: Vec<_> = resolved.iter().filter_map(|v| v["name"].as_str()).collect();
        assert_eq!(names, vec!["read", "edit"]);
    }
}
