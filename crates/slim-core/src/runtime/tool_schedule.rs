use super::*;
use std::borrow::Borrow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolCallBucket {
    Read,
    Mutating,
}

pub(super) fn tool_call_bucket(name: &str) -> ToolCallBucket {
    match name {
        "read" | "list" | "search" | "code_intel" => ToolCallBucket::Read,
        _ => ToolCallBucket::Mutating,
    }
}

pub(super) fn tool_call_slots(call: &ProviderToolCall) -> usize {
    if matches!(call.name.as_str(), "write" | "patch")
        && serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .is_some_and(|arguments| arguments.get("then_run").is_some_and(Value::is_object))
    {
        2
    } else {
        1
    }
}

pub(super) fn mark_cancelled_tool_outcome(outcome: &mut ToolExecutionOutcome) {
    outcome.result.success = false;
    if let Some(fused_shell) = outcome.receipt.fused_shell.as_mut() {
        fused_shell.1.success = false;
    }
}

pub fn tool_call_is_read_only(name: &str) -> bool {
    matches!(tool_call_bucket(name), ToolCallBucket::Read)
}

pub(super) fn tool_call_is_parallel_snapshot_read(tools: &ToolRegistry, name: &str) -> bool {
    name == "code_intel"
        || tools
            .operational_spec(name)
            .is_some_and(|spec| spec.effect_class == ToolEffectClass::SnapshotRead)
}

pub(super) fn is_serial_barrier(prepared: &PreparedToolInvocation) -> bool {
    matches!(prepared.name.as_str(), "shell" | "skill" | "mcp")
        || matches!(
            &prepared.arguments,
            PreparedToolArguments::Write {
                then_run: Some(_),
                ..
            } | PreparedToolArguments::Patch {
                then_run: Some(_),
                ..
            }
        )
}

pub(super) fn is_file_mutation(prepared: &PreparedToolInvocation) -> bool {
    matches!(prepared.name.as_str(), "write" | "patch") && !prepared.target_paths.is_empty()
}

/// Scheduling identity of a path text. Windows file names are case-insensitive,
/// so two spellings of a not-yet-existing file (`New.txt`, `new.txt`) must not
/// be scheduled as independent files. Scheduling only: never use it as a path.
pub(super) fn schedule_key(identity: &str) -> String {
    #[cfg(windows)]
    {
        identity.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        identity.to_owned()
    }
}

/// `schedule_key` of a path, kept as a path so `starts_with` stays per component.
fn schedule_path(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    {
        Cow::Owned(PathBuf::from(schedule_key(&crate::tools::path_identity(
            path,
        ))))
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

pub(super) fn mutation_path_key(prepared: &PreparedToolInvocation) -> Option<String> {
    prepared
        .target_paths
        .first()
        .map(|path| schedule_key(&crate::tools::path_identity(path)))
}

pub(super) fn snapshot_depends_on_prior_mutation(
    prior: &PreparedToolInvocation,
    snapshot: &PreparedToolInvocation,
) -> bool {
    // A file mutation always has a target path, so the filter only rejects
    // calls that are not file mutations.
    let Some(mutated) = prior
        .target_paths
        .first()
        .filter(|_| is_file_mutation(prior))
        .map(|path| schedule_path(path))
    else {
        return false;
    };
    match snapshot.name.as_str() {
        "search" => true,
        // Semantic results span the whole workspace: a patch to b.rs changes
        // the references of a symbol in a.rs. Path equality cannot prove
        // independence, so any prior file mutation blocks anticipation.
        "code_intel" => true,
        "list" => snapshot
            .target_paths
            .first()
            .map(|dir| schedule_path(dir))
            .is_some_and(|dir| mutated == dir || mutated.starts_with(&dir)),
        _ => {
            snapshot.target_paths.is_empty()
                || snapshot
                    .target_paths
                    .iter()
                    .any(|path| schedule_path(path) == mutated)
        }
    }
}

pub(super) fn phase1_snapshot_indices<P: Borrow<PreparedToolInvocation>>(
    tools: &ToolRegistry,
    prepared: &[P],
) -> Vec<usize> {
    phase1_snapshot_indices_ready(tools, prepared, 0, &vec![None; prepared.len()])
}

/// Selects snapshot reads that are ready at the current scheduler state.
///
/// A call is completed once its slot in `results` is filled. A file mutation
/// is a dependency barrier only while it is still pending: once completed
/// (successfully or otherwise), a read whose dependency was that mutation may
/// join the next parallel wave. Serial barriers remain fail-closed: a pending
/// shell/skill/MCP call blocks every later snapshot until it has completed.
pub(super) fn phase1_snapshot_indices_ready<P: Borrow<PreparedToolInvocation>>(
    tools: &ToolRegistry,
    prepared: &[P],
    from: usize,
    results: &[Option<ToolResult>],
) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut barrier = false;
    // Pending file mutations before the current call, over the whole batch
    // (also before `from`): the only calls a snapshot can depend on.
    let mut pending_mutations: Vec<&PreparedToolInvocation> = Vec::new();
    for (index, call) in prepared.iter().enumerate() {
        let call: &PreparedToolInvocation = call.borrow();
        let is_completed = results.get(index).is_some_and(Option::is_some);
        if index >= from {
            if is_serial_barrier(call) && !is_completed {
                barrier = true;
            }
            if !is_completed
                && !barrier
                && tool_call_is_parallel_snapshot_read(tools, &call.name)
                && !pending_mutations
                    .iter()
                    .any(|prior| snapshot_depends_on_prior_mutation(prior, call))
            {
                indices.push(index);
            }
        }
        if !is_completed && is_file_mutation(call) {
            pending_mutations.push(call);
        }
    }
    indices
}

/// Indices, starting at `start`, of the file mutations that can run as one
/// parallel wave: consecutive not-yet-completed mutations, none of them a
/// serial barrier, each on a distinct target file. Completed calls in between
/// are skipped. The cluster stops at the first call that does not qualify
/// or repeats a file. Empty when `start` itself does not qualify; a cluster
/// of one is not worth a wave and runs as a single call.
pub(super) fn independent_mutation_cluster<P: Borrow<PreparedToolInvocation>>(
    prepared: &[P],
    results: &[Option<ToolResult>],
    start: usize,
) -> Vec<usize> {
    let file_key = |index: usize| {
        let call: &PreparedToolInvocation = prepared[index].borrow();
        (is_file_mutation(call) && !is_serial_barrier(call))
            .then(|| mutation_path_key(call))
            .flatten()
    };
    let Some(first) = file_key(start) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::from([first]);
    let mut cluster = vec![start];
    cluster.extend(
        (start + 1..prepared.len())
            .filter(|&index| results[index].is_none())
            .map_while(|index| seen.insert(file_key(index)?).then_some(index)),
    );
    cluster
}

/// Maps each call to the first earlier call with identical reusable
/// evidence (itself when it leads or is not reusable).
pub(super) fn evidence_reuse_aliases<'a>(
    prepared_calls: impl IntoIterator<Item = &'a PreparedToolInvocation>,
) -> Vec<usize> {
    let mut alias_of = Vec::new();
    let mut leaders = std::collections::HashMap::<&str, usize>::new();
    for (index, prepared) in prepared_calls.into_iter().enumerate() {
        alias_of.push(index);
        if !prepared.reusable_evidence() {
            continue;
        }
        match leaders.entry(&prepared.canonical_fingerprint) {
            std::collections::hash_map::Entry::Occupied(leader) => alias_of[index] = *leader.get(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(index);
            }
        }
    }
    alias_of
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ToolBudgetCut {
    pub(super) suppressed: usize,
    pub(super) hit_run_total: bool,
    /// Both per-turn buckets are zero: no emitted call can ever execute, so
    /// retrying next turn would only spend provider requests for nothing.
    pub(super) buckets_disabled: bool,
}

/// Keeps the executable prefix in `calls` and returns the suppressed tail.
pub(super) fn split_calls_for_budget(
    calls: &mut Vec<ProviderToolCall>,
    config: AgentLoopConfig,
    already_reserved: usize,
) -> (ToolBudgetCut, Vec<ProviderToolCall>) {
    let mut read_used = 0usize;
    let mut mutating_used = 0usize;
    let mut total_used = 0usize;
    let mut hit_run_total = false;
    let remaining_total = config.max_total_tool_calls.saturating_sub(already_reserved);
    let accepted = calls
        .iter()
        .take_while(|call| {
            let slots = tool_call_slots(call);
            if total_used.saturating_add(slots) > remaining_total {
                hit_run_total = true;
                return false;
            }
            let (used, limit) = match tool_call_bucket(&call.name) {
                ToolCallBucket::Read => (&mut read_used, config.max_read_tool_calls),
                ToolCallBucket::Mutating => (&mut mutating_used, config.max_mutating_tool_calls),
            };
            if used.saturating_add(slots) > limit {
                return false;
            }
            *used = used.saturating_add(slots);
            total_used = total_used.saturating_add(slots);
            true
        })
        .count();
    let suppressed = calls.split_off(accepted);
    (
        ToolBudgetCut {
            suppressed: suppressed.len(),
            hit_run_total,
            buckets_disabled: config.max_read_tool_calls == 0
                && config.max_mutating_tool_calls == 0,
        },
        suppressed,
    )
}

/// Names the calls a per-turn cap dropped so the model can reissue exactly
/// those; arguments are shortened, and the message is redacted on append.
pub(super) fn suppressed_calls_steer(suppressed: &[ProviderToolCall]) -> String {
    const MAX_ARGUMENT_CHARS: usize = 160;
    let mut steer = format!(
        "{} tool call(s) this turn were not executed (per-turn cap):",
        suppressed.len()
    );
    for call in suppressed {
        let mut arguments: String = call.arguments.chars().take(MAX_ARGUMENT_CHARS).collect();
        if arguments.len() < call.arguments.len() {
            arguments.push_str("...");
        }
        let _ = write!(steer, "\n- {} {arguments}", call.name);
    }
    steer.push_str("\nRetry only these calls next turn if they are still needed; do not repeat calls that already returned results.");
    steer
}

pub(super) fn should_stop_after_tool_budget_cut(
    cut: ToolBudgetCut,
    turn: usize,
    max_turns: usize,
) -> bool {
    cut.suppressed > 0 && (cut.hit_run_total || cut.buckets_disabled || turn + 1 >= max_turns)
}

/// Closing-turn prompts, sent with `tools=[]`; a failure never replaces the original `stop`.
pub(super) const BUDGET_FINALIZE_PROMPT: &str = "Budget exhausted. Respond now as the final answer with what is already known: outcome, changed files/behavior, validation result, remaining risks. Do not call tools.";

pub(super) const NO_PROGRESS_FINALIZE_PROMPT: &str = "Execution stopped because repeated tool work produced no new evidence or workspace progress. Give a brief final answer: what is known, what remains incomplete, and what new information or changed state would allow progress. Do not call tools or claim completion without evidence.";

/// Between-turns steer cap: nudges are bounded so they can never inflate context.
pub(super) const MAX_BUDGET_STEERS: usize = 2;

pub(super) const BATCH_CONCURRENCY: usize = 8;

pub(super) struct PoolOutcome {
    pub(super) outcome: ToolExecutionOutcome,
    pub(super) duration_ms: u64,
}

/// Notification sent by a pool future immediately before it dispatches tool
/// work. The outer runtime owns the event journal and assigns the monotonic
/// sequence when it receives this notice.
pub(super) struct ToolStartedNotice {
    pub(super) index: usize,
    pub(super) arguments: String,
}

#[derive(Clone, Copy)]
pub(super) struct ToolInvocation<'a> {
    pub(super) batch_id: &'a str,
    pub(super) call_id: &'a str,
    pub(super) name: &'a str,
    pub(super) arguments: &'a str,
}

impl<'a> ToolInvocation<'a> {
    pub(super) fn provider(batch_id: &'a str, call: &'a ProviderToolCall) -> Self {
        Self {
            batch_id,
            call_id: &call.id,
            name: &call.name,
            arguments: &call.arguments,
        }
    }
}
