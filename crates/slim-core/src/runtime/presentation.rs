use super::agent_loop::{LoopCtx, LoopState};
use super::*;

impl Runtime {
    pub(super) async fn materialize_results(
        &mut self,
        results: &mut [ToolResult],
        max_result_bytes: usize,
        force: Option<&[bool]>,
        mut next_seq: u64,
    ) -> Result<u64, ProviderError> {
        let Some(store) = self.artifact_store.clone() else {
            return Ok(next_seq);
        };
        let jobs = results
            .iter_mut()
            .enumerate()
            .filter(|(index, result)| {
                result.artifact.is_none()
                    && (result.output.len() > max_result_bytes
                        || force
                            .and_then(|forced| forced.get(*index))
                            .copied()
                            .unwrap_or(false))
            })
            .map(|(index, result)| {
                let store = store.clone();
                let label = format!("tool-{}", result.name);
                let output = std::mem::take(&mut result.output);
                async move {
                    tokio::task::spawn_blocking(move || {
                        let handle = store.put(&label, output.as_bytes())?;
                        Ok::<_, std::io::Error>((index, output, handle))
                    })
                    .await
                    .map_err(|error| ProviderError::InvalidResponse {
                        message: format!("artifact worker: {error}"),
                    })?
                    .map_err(|error| ProviderError::InvalidResponse {
                        message: format!("artifact: {error}"),
                    })
                }
            })
            .collect::<Vec<_>>();
        let stored = futures_util::stream::iter(jobs)
            .buffered(BATCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        for stored in stored {
            let (index, output, handle) = stored?;
            let result = &mut results[index];
            result.output = output;
            result.artifact = Some(handle.clone());
            push_runtime_event(
                &mut self.app,
                &mut next_seq,
                crate::EventKind::ArtifactStored {
                    id: handle.id,
                    size: handle.size,
                },
            )?;
        }
        Ok(next_seq)
    }

    /// Chooses how much of each result of one tool batch enters the request:
    /// the largest common scale of the per-call targets whose projected
    /// request still fits the window. A batch that crosses the compaction
    /// line is not squeezed to stay under it: the next turn's compaction
    /// keeps the newest `keep_recent_tokens` verbatim, so the batch survives
    /// and starving it would only lose data.
    pub(super) fn plan_tool_presentations<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        st: &LoopState<'_>,
        tools: &[Value],
        batch: &PresentationBatch<'_>,
    ) -> Vec<ToolPresentation> {
        let config = st.config;
        let plan = self.presentation_plan(batch, st.messages, &config, ctx.cwd);
        let fit = RequestFit {
            client: ctx.client,
            tools,
            mode: ctx.mode,
            reserve_tokens: config.context_reserve_tokens,
            window_tokens: config.context_window_tokens,
            channel: &st.channel,
        };
        largest_fitting_scale(
            |scale| self.present_batch(&plan, scale),
            |presentations| self.request_fits(&fit, &plan, presentations),
        )
    }

    /// The scale-independent facts of a batch, computed once.
    pub(super) fn presentation_plan<'a>(
        &'a self,
        batch: &PresentationBatch<'a>,
        base_messages: &'a [ProviderMessage],
        config: &AgentLoopConfig,
        cwd: &Path,
    ) -> PresentationPlan<'a> {
        let count = batch.calls.len().min(batch.results.len());
        let calls = &batch.calls[..count];
        let results = &batch.results[..count];
        let sources = calls
            .iter()
            .map(|call| {
                self.presentation_sources
                    .get(&(batch.id.to_owned(), call.id.clone()))
            })
            .collect::<Vec<_>>();
        let (read_cap, result_cap, batch_cap) = effective_presentation_caps(
            config,
            self.read_presentation_bytes,
            base_messages,
        );
        let effective_outputs: Vec<Option<String>> = calls
            .iter()
            .zip(results)
            .map(|(call, result)| {
                if result.name == "shell" {
                    let cmd = serde_json::from_str::<Value>(&call.arguments)
                        .ok()
                        .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_owned));
                    cmd.as_deref().and_then(|command| {
                        super::shell_fold::fold_shell_output(command, &result.output, result.success)
                    })
                } else if matches!(result.name.as_str(), "codemode" | "batch_code")
                    && result.success
                {
                    let (body, console_tail) = match result.output.split_once("\nconsole:\n") {
                        Some((body, tail)) => (body, Some(tail)),
                        None => (result.output.as_str(), None),
                    };
                    let body_distilled = super::distill::distill_json_for_model(body);
                    let console_distilled =
                        console_tail.and_then(super::distill::distill_generic_log);
                    if body_distilled.is_some() || console_distilled.is_some() {
                        let out_body = body_distilled.as_ref().map_or(body, |d| d.text.as_str());
                        match (console_distilled.as_deref(), console_tail) {
                            (Some(tail), _) | (None, Some(tail)) => {
                                Some(format!("{out_body}\nconsole:\n{tail}"))
                            }
                            (None, None) => Some(out_body.to_owned()),
                        }
                    } else {
                        None
                    }
                } else if result.name == "read"
                    && result.success
                    && result.media.is_empty()
                    && result.output.len() >= 1024
                {
                    differential_read_output(base_messages, call, &result.output)
                } else {
                    None
                }
            })
            .collect();
        let targets: Vec<usize> = results
            .iter()
            .zip(&sources)
            .zip(&effective_outputs)
            .map(|((result, source), effective)| {
                let cap = if result.name == "read" {
                    read_cap
                } else {
                    result_cap
                };
                if let Some(effective) = effective {
                    return effective.len().min(cap);
                }
                // `result.output` is post-redaction while the presentation
                // source holds the raw projection; a few bytes of divergence
                // (e.g. "[REDACTED]" shorter than the secret) must not divert
                // the whole result to an artifact, so size by the real need.
                let projected = source.map(|source| source.full_len()).unwrap_or(0);
                result.output.len().max(projected).min(cap)
            })
            .collect();
        let names = calls
            .iter()
            .map(|call| self.redact_sensitive(&call.name))
            .collect::<Vec<_>>();
        let duplicates: Vec<String> = names.iter().map(|name| duplicate_pointer(name)).collect();
        // What each result needs of the batch cap: a result already in context
        // (or repeated inside the batch) collapses to its pointer in
        // `present_call`, so it needs no more than that.
        let needs: Vec<usize> = (0..count)
            .map(|index| {
                let result = &results[index];
                let repeated = result.success
                    && result.media.is_empty()
                    && duplicates[index].len() < result.output.len()
                    && (tool_output_already_in_context(
                        base_messages,
                        &names[index],
                        &result.output,
                    ) || results[..index].iter().zip(&names).any(|(earlier, name)| {
                        earlier.success
                            && earlier.media.is_empty()
                            && *name == names[index]
                            && earlier.output == result.output
                    }));
                if repeated {
                    duplicates[index].len()
                } else {
                    targets[index]
                }
            })
            .collect();
        PresentationPlan {
            batch_limits: batch_limits(&needs, batch_cap),
            base_messages,
            calls,
            results,
            duplicates,
            ids: calls
                .iter()
                .map(|call| self.redact_sensitive(&call.id))
                .collect(),
            names,
            targets,
            suffixes: results
                .iter()
                .map(|result| Self::artifact_reference(result, cwd))
                .collect(),
            sources,
            effective_outputs,
        }
    }

    /// Every call of the batch presented at `scale` per mille of its target.
    pub(super) fn present_batch(
        &self,
        plan: &PresentationPlan<'_>,
        scale: usize,
    ) -> Vec<ToolPresentation> {
        // Duplicate detection must see the same context the assembly loop
        // will: base history plus this batch's already chosen tool
        // messages. Otherwise a repeated result inside one batch is
        // budgeted as another full copy and can shrink the first
        // occurrence even when one copy plus the notice would fit.
        let mut batch_messages = Vec::with_capacity(plan.calls.len());
        (0..plan.calls.len())
            .map(|index| {
                let presentation = self.present_call(plan, index, scale, &batch_messages);
                // Mirror `append_conversation_message`: the retained wire
                // message is the redacted presentation, and it is what the
                // next duplicate check compares against.
                batch_messages.push(self.redact_message(ProviderMessage::tool(
                    plan.names[index].clone(),
                    plan.ids[index].clone(),
                    presentation.text.clone(),
                )));
                presentation
            })
            .collect()
    }

    fn present_call(
        &self,
        plan: &PresentationPlan<'_>,
        index: usize,
        scale: usize,
        batch_messages: &[ProviderMessage],
    ) -> ToolPresentation {
        let result = &plan.results[index];
        let name = &plan.names[index];
        let duplicate = &plan.duplicates[index];
        let already_in_context = |text: &str| {
            duplicate.len() < text.len()
                && (tool_output_already_in_context(plan.base_messages, name, text)
                    || tool_output_already_in_context(batch_messages, name, text))
        };
        // A retained complete result needs only an identity
        // pointer. Account for that before allocating page space,
        // otherwise a tight budget can hide the duplicate behind
        // a zero-record projection and defeat evidence reuse.
        if result.success && result.media.is_empty() && already_in_context(&result.output) {
            return ToolPresentation::complete(duplicate.clone());
        }
        let suffix = plan.suffixes[index].as_ref();
        let allowance = plan.targets[index]
            .saturating_mul(scale)
            .div_ceil(1000)
            .min(plan.batch_limits[index]);
        let body_budget = allowance.saturating_sub(suffix.map_or(0, String::len));
        let mut presentation = if let Some(effective) = plan.effective_outputs[index].as_deref() {
            let mut p = present_unstructured(
                &result.name,
                effective,
                PresentationBudget {
                    max_bytes: body_budget,
                },
            );
            if !p.complete && p.text.len() >= effective.len() {
                p = ToolPresentation::complete(effective.to_owned());
            }
            p
        } else {
            let mut p = plan.sources[index]
                .map(|source| {
                    source.present(PresentationBudget {
                        max_bytes: body_budget,
                    })
                })
                .unwrap_or_else(|| {
                    present_unstructured(
                        &result.name,
                        &result.output,
                        PresentationBudget {
                            max_bytes: body_budget,
                        },
                    )
                });
            // A cut result is never longer than the result it cuts: the notice a
            // small result would gain costs more than the result is worth.
            if !p.complete && p.text.len() >= result.output.len() {
                p = ToolPresentation::complete(result.output.clone());
            }
            p
        };
        if let Some(suffix) = suffix {
            if presentation.text.len().saturating_add(suffix.len()) <= allowance {
                presentation.text.push_str(suffix);
            } else {
                // Keep the recovery handle even when the aggregate
                // budget cannot carry both the selected records and
                // metadata. The explicit over-budget notice is a
                // safer contract than an unreachable artifact.
                presentation.text.push('\n');
                presentation.text.push_str(suffix);
            }
        }
        // The assembly loop also collapses a projection that is
        // already verbatim in context (e.g. an identical
        // truncation); mirror it so the budgeted size matches
        // the emitted one.
        if result.success && result.media.is_empty() && already_in_context(&presentation.text) {
            ToolPresentation::complete(duplicate.clone())
        } else {
            presentation
        }
    }

    /// Whether the request with these presentations appended stays inside
    /// the window.
    fn request_fits<A: ProviderAdapter>(
        &self,
        fit: &RequestFit<'_, A>,
        plan: &PresentationPlan<'_>,
        presentations: &[ToolPresentation],
    ) -> bool {
        let client = fit.client;
        let mut candidate = plan.base_messages.to_vec();
        for (((name, id), presentation), result) in plan
            .names
            .iter()
            .zip(&plan.ids)
            .zip(presentations)
            .zip(plan.results)
        {
            candidate.push(
                ProviderMessage::tool(name.clone(), id.clone(), presentation.text.clone())
                    .with_content_blocks(result.media.clone()),
            );
        }
        // The loop's preflight uses the conservative structural estimate
        // before preparing the wire request, and that estimate never
        // under-counts the serialized body. When the adapter bounds its
        // request envelope the structural estimate alone is the decision
        // input — the exact path the loop already takes — so transport
        // metadata is only built for adapters without the bound.
        let overlay = self.overlay_channel(&mut candidate, fit.mode, fit.channel);
        let structural_chars =
            estimate_unprepared_request_chars(client.adapter(), overlay.view(), fit.tools, None);
        drop(overlay);
        let budget_estimated = match structural_chars {
            Some(chars) => self.token_estimator.estimate(
                crate::provider::provider_kind_name(client.adapter().kind()),
                client.adapter().model(),
                chars,
            ),
            None => {
                let Ok(request) = self.prepare_loop_request(
                    client,
                    &mut candidate,
                    fit.tools,
                    fit.mode,
                    fit.channel,
                ) else {
                    return false;
                };
                self.token_estimator.estimate(
                    crate::provider::provider_kind_name(client.adapter().kind()),
                    client.adapter().model(),
                    request
                        .serialized_chars
                        .saturating_sub(image_payload_discount_chars(&candidate)),
                )
            }
        };
        budget_estimated.saturating_add(fit.reserve_tokens) <= fit.window_tokens
    }
}

/// The tool calls of one executed batch and their results.
pub(super) struct PresentationBatch<'a> {
    pub(super) id: &'a str,
    pub(super) calls: &'a [ProviderToolCall],
    pub(super) results: &'a [ToolResult],
}

/// The per-call facts of a batch that do not depend on the presentation
/// scale; the names and ids are redacted.
pub(super) struct PresentationPlan<'a> {
    /// The most of its target each result may present so that the batch stays
    /// within its shared budget (`usize::MAX`: no limit).
    batch_limits: Vec<usize>,
    base_messages: &'a [ProviderMessage],
    calls: &'a [ProviderToolCall],
    results: &'a [ToolResult],
    names: Vec<String>,
    ids: Vec<String>,
    duplicates: Vec<String>,
    targets: Vec<usize>,
    suffixes: Vec<Option<String>>,
    sources: Vec<Option<&'a ToolPresentationSource>>,
    effective_outputs: Vec<Option<String>>,
}

/// Scales `(read_cap, result_cap, batch_cap)` proportionally to context window
/// size and remaining headroom before compaction.
pub(super) fn effective_presentation_caps(
    config: &AgentLoopConfig,
    base_read_bytes: usize,
    base_messages: &[ProviderMessage],
) -> (usize, usize, usize) {
    let default_read = base_read_bytes;
    let default_result = config.max_result_bytes;
    let default_batch = AgentLoopConfig::DEFAULT_MAX_BATCH_RESULT_BYTES
        .max(default_result)
        .max(default_read);
    if config.context_window_tokens == 0 {
        return (default_read, default_result, default_batch);
    }
    let window = config.context_window_tokens;
    let window_bytes = usize::try_from(window.saturating_mul(3)).unwrap_or(usize::MAX);
    let (mut read_cap, mut result_cap, mut batch_cap) =
        (default_read, default_result, default_batch);
    if window < 32_000 {
        read_cap = read_cap.min((window_bytes / 6).max(4 * 1024));
        result_cap = result_cap.min((window_bytes / 6).max(4 * 1024));
        batch_cap = batch_cap.min((window_bytes / 4).max(8 * 1024));
    }
    let used_tokens: u64 = base_messages
        .iter()
        .map(crate::context::estimate_tokens)
        .sum();
    if used_tokens.saturating_mul(100) > window.saturating_mul(75) {
        read_cap = (read_cap / 2).max(4 * 1024).min(default_read);
        result_cap = (result_cap / 2).max(4 * 1024).min(default_result);
        batch_cap = (batch_cap / 2).max(8 * 1024).min(default_batch);
    } else if window >= 200_000 && used_tokens.saturating_mul(100) < window.saturating_mul(60) {
        if base_read_bytes >= 64 * 1024 {
            read_cap = read_cap.max(96 * 1024).min(window_bytes / 6);
        }
        batch_cap = batch_cap.max(128 * 1024).min(window_bytes / 4);
    }
    (read_cap, result_cap, batch_cap)
}

#[derive(Clone, Eq, PartialEq)]
struct ReadCallKey {
    path: String,
    offset: u64,
    max_lines: Option<u64>,
    raw: bool,
    special_mode: bool,
}

fn parse_read_call_key(arguments: &str) -> Option<ReadCallKey> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let path = value
        .get("path")
        .and_then(Value::as_str)?
        .trim()
        .trim_start_matches("./")
        .replace('\\', "/");
    if path.is_empty() {
        return None;
    }
    let offset = value.get("offset").and_then(Value::as_u64).unwrap_or(1);
    let max_lines = value
        .get("max_lines")
        .or_else(|| value.get("lines"))
        .and_then(Value::as_u64);
    let raw = value.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let special_mode = value.get("outline").and_then(Value::as_bool).unwrap_or(false)
        || value.get("ranges").is_some_and(|v| !v.is_null())
        || value.get("symbols").is_some_and(|v| !v.is_null());
    Some(ReadCallKey {
        path,
        offset,
        max_lines,
        raw,
        special_mode,
    })
}

fn is_raw_read_content(content: &str) -> bool {
    !content.is_empty()
        && !content.starts_with(super::history_elision::DUPLICATE_POINTER_PREFIX)
        && !content.starts_with("[diff vs earlier read")
        && !content.starts_with("[pruned old ")
        && !content.starts_with("[superseded ")
}

fn differential_read_output(
    base_messages: &[ProviderMessage],
    call: &ProviderToolCall,
    new_output: &str,
) -> Option<String> {
    let target = parse_read_call_key(&call.arguments)?;
    if target.raw || target.special_mode {
        return None;
    }
    let mut earlier_id: Option<&str> = None;
    for msg in base_messages.iter().rev() {
        if msg.role != "assistant" {
            continue;
        }
        for prev in msg.tool_calls.iter().rev() {
            if prev.name == "read" {
                if let Some(prev_key) = parse_read_call_key(&prev.arguments) {
                    if !prev_key.special_mode
                        && prev_key.path == target.path
                        && prev_key.offset == target.offset
                        && prev_key.max_lines == target.max_lines
                    {
                        earlier_id = Some(prev.id.as_str());
                        break;
                    }
                }
            }
        }
        if earlier_id.is_some() {
            break;
        }
    }
    let earlier_id = earlier_id?;
    let earlier_output = base_messages.iter().rev().find_map(|msg| {
        (msg.role == "tool"
            && msg.name.as_deref() == Some("read")
            && msg.tool_call_id.as_deref() == Some(earlier_id))
        .then_some(msg.content.as_str())
    })?;
    if !is_raw_read_content(earlier_output) || earlier_output == new_output {
        return None;
    }
    let diff = compute_compact_diff(earlier_output, new_output)?;
    let wrapped = format!(
        "[diff vs earlier read of {} in context; pass raw:true for full text]\n{}",
        target.path, diff
    );
    if wrapped.len() < new_output.len().saturating_mul(45) / 100
        && new_output.len().saturating_sub(wrapped.len()) >= 512
    {
        Some(wrapped)
    } else {
        None
    }
}

fn compute_compact_diff(old: &str, new: &str) -> Option<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let min_len = old_lines.len().min(new_lines.len());
    let mut prefix = 0;
    while prefix < min_len && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (min_len - prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mid_old = &old_lines[prefix..old_lines.len() - suffix];
    let mid_new = &new_lines[prefix..new_lines.len() - suffix];
    if mid_old.is_empty() && mid_new.is_empty() {
        return None;
    }
    if mid_old.len().saturating_mul(mid_new.len()) > 250_000 {
        return None;
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Op<'a> {
        Same(&'a str),
        Del(&'a str),
        Add(&'a str),
    }

    let n = mid_old.len();
    let m = mid_new.len();
    let cols = m + 1;
    let mut dp = vec![0u32; (n + 1) * cols];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * cols + j] = if mid_old[i] == mid_new[j] {
                dp[(i + 1) * cols + (j + 1)] + 1
            } else {
                dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if mid_old[i] == mid_new[j] {
            ops.push(Op::Same(mid_old[i]));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
            ops.push(Op::Del(mid_old[i]));
            i += 1;
        } else {
            ops.push(Op::Add(mid_new[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(mid_old[i]));
        i += 1;
    }
    while j < m {
        ops.push(Op::Add(mid_new[j]));
        j += 1;
    }

    let mut all_ops = Vec::with_capacity(prefix + ops.len() + suffix);
    all_ops.extend(old_lines[..prefix].iter().map(|l| Op::Same(l)));
    all_ops.extend(ops);
    all_ops.extend(
        old_lines[old_lines.len() - suffix..]
            .iter()
            .map(|l| Op::Same(l)),
    );

    const CTX: usize = 2;
    let changed_indices: Vec<usize> = all_ops
        .iter()
        .enumerate()
        .filter_map(|(idx, op)| (!matches!(op, Op::Same(_))).then_some(idx))
        .collect();
    if changed_indices.is_empty() {
        return None;
    }

    let mut groups: Vec<(usize, usize)> = Vec::new();
    for idx in changed_indices {
        let start = idx.saturating_sub(CTX);
        let end = (idx + CTX + 1).min(all_ops.len());
        if let Some(last) = groups.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        groups.push((start, end));
    }

    let mut old_pos = vec![0usize; all_ops.len() + 1];
    let mut new_pos = vec![0usize; all_ops.len() + 1];
    for (idx, op) in all_ops.iter().enumerate() {
        old_pos[idx + 1] = old_pos[idx] + usize::from(!matches!(op, Op::Add(_)));
        new_pos[idx + 1] = new_pos[idx] + usize::from(!matches!(op, Op::Del(_)));
    }

    let mut out = String::new();
    for (g_idx, (start, end)) in groups.into_iter().enumerate() {
        if g_idx > 0 {
            out.push('\n');
        }
        let o_start = old_pos[start] + 1;
        let o_count = old_pos[end] - old_pos[start];
        let n_start = new_pos[start] + 1;
        let n_count = new_pos[end] - new_pos[start];
        let _ = writeln!(out, "@@ -{o_start},{o_count} +{n_start},{n_count} @@");
        for op in &all_ops[start..end] {
            match op {
                Op::Same(l) => {
                    out.push(' ');
                    out.push_str(l);
                }
                Op::Del(l) => {
                    out.push('-');
                    out.push_str(l);
                }
                Op::Add(l) => {
                    out.push('+');
                    out.push_str(l);
                }
            }
            out.push('\n');
        }
    }
    Some(out.trim_end_matches('\n').to_owned())
}

/// How much of its need each result of one batch may present so that the batch
/// stays within `cap` bytes. A result no larger than an equal share of the cap
/// is never reduced; the larger ones share what is left in proportion to their
/// needs.
fn batch_limits(needs: &[usize], cap: usize) -> Vec<usize> {
    let total = needs
        .iter()
        .fold(0_usize, |sum, need| sum.saturating_add(*need));
    if total <= cap {
        return vec![usize::MAX; needs.len()];
    }
    let share = cap / needs.len().max(1);
    let large: usize = needs
        .iter()
        .filter(|need| **need > share)
        .fold(0_usize, |sum, need| sum.saturating_add(*need));
    let remaining = cap.saturating_sub(total - large);
    needs
        .iter()
        .map(|need| {
            if *need <= share {
                usize::MAX
            } else {
                usize::try_from(*need as u128 * remaining as u128 / large as u128)
                    .unwrap_or(usize::MAX)
            }
        })
        .collect()
}

/// The request envelope a presented batch must fit.
struct RequestFit<'a, A: ProviderAdapter> {
    client: &'a HttpProviderClient<A>,
    tools: &'a [Value],
    mode: crate::OperatingMode,
    reserve_tokens: u64,
    window_tokens: u64,
    channel: &'a mode::ChannelFrame,
}

/// The value built at the largest scale (per mille, 0..=999) that still
/// fits, or the full one when it fits; scale 0 is the floor even when it
/// does not fit either.
pub(super) fn largest_fitting_scale<T>(build: impl Fn(usize) -> T, fits: impl Fn(&T) -> bool) -> T {
    let full = build(1000);
    if fits(&full) {
        return full;
    }
    let mut low = 0usize;
    let mut high = 999usize;
    let mut best = build(0);
    if fits(&best) {
        while low <= high {
            let middle = low.saturating_add(high).div_ceil(2);
            let candidate = build(middle);
            if fits(&candidate) {
                best = candidate;
                low = middle.saturating_add(1);
            } else {
                high = middle.saturating_sub(1);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differential_read_emits_compact_diff_on_small_edit_and_respects_raw_flag() {
        let mut lines: Vec<String> = (1..=80).map(|i| format!("line number {i:03} with stable content")).collect();
        let old_text = lines.join("\n");
        lines[40] = "line number 041 with UPDATED content".to_owned();
        let new_text = lines.join("\n");
        assert!(new_text.len() >= 1024);

        let assistant = ProviderMessage::assistant(
            "",
            vec![ProviderToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: r#"{"path":"src/lib.rs"}"#.into(),
            }],
        );
        let tool = ProviderMessage::tool("read", "c1", old_text);
        let base = vec![ProviderMessage::user("check"), assistant, tool];

        let runtime = Runtime::new();
        let calls = vec![ProviderToolCall {
            id: "c2".into(),
            name: "read".into(),
            arguments: r#"{"path":"./src/lib.rs","offset":1}"#.into(),
        }];
        let results = vec![ToolResult::ok("read", new_text.clone())];
        let batch = PresentationBatch {
            id: "b1",
            calls: &calls,
            results: &results,
        };
        let config = AgentLoopConfig::default();
        let plan = runtime.presentation_plan(&batch, &base, &config, Path::new("."));
        let presented = runtime.present_batch(&plan, 1000);
        assert_eq!(presented.len(), 1);
        assert!(presented[0].text.starts_with("[diff vs earlier read of src/lib.rs"));
        assert!(presented[0].text.contains("-line number 041 with stable content"));
        assert!(presented[0].text.contains("+line number 041 with UPDATED content"));
        assert!(presented[0].text.len() < new_text.len() / 2);

        // `raw: true` forces full text.
        let raw_calls = vec![ProviderToolCall {
            id: "c3".into(),
            name: "read".into(),
            arguments: r#"{"path":"src/lib.rs","raw":true}"#.into(),
        }];
        let raw_batch = PresentationBatch {
            id: "b2",
            calls: &raw_calls,
            results: &results,
        };
        let raw_plan = runtime.presentation_plan(&raw_batch, &base, &config, Path::new("."));
        let raw_presented = runtime.present_batch(&raw_plan, 1000);
        assert_eq!(raw_presented[0].text, new_text);
    }

    #[test]
    fn shell_fold_compresses_verbose_cargo_output_in_presentation() {
        let mut raw = String::from("exit_code=0\nstdout:\n");
        for i in 0..40 {
            raw.push_str(&format!("   Compiling dep-{i} v0.1.0\n"));
        }
        for i in 0..30 {
            raw.push_str(&format!("test t_{i} ... ok\n"));
        }
        raw.push_str("test result: ok. 30 passed; 0 failed\n");

        let runtime = Runtime::new();
        let calls = vec![ProviderToolCall {
            id: "s1".into(),
            name: "shell".into(),
            arguments: r#"{"command":"cargo test -p slim-core"}"#.into(),
        }];
        let results = vec![ToolResult::ok("shell", raw.clone())];
        let batch = PresentationBatch {
            id: "b1",
            calls: &calls,
            results: &results,
        };
        let config = AgentLoopConfig::default();
        let plan = runtime.presentation_plan(&batch, &[], &config, Path::new("."));
        let presented = runtime.present_batch(&plan, 1000);
        assert!(presented[0].text.starts_with("exit_code=0\n[cargo summary:"));
        assert!(presented[0].text.len() < raw.len() / 2);
    }

    #[test]
    fn effective_presentation_caps_adapts_to_small_tight_and_large_windows() {
        let mut cfg = AgentLoopConfig {
            context_window_tokens: 16_000,
            max_result_bytes: 16 * 1024,
            ..AgentLoopConfig::default()
        };
        let (read_small, res_small, batch_small) =
            effective_presentation_caps(&cfg, 64 * 1024, &[]);
        assert!(read_small < 64 * 1024);
        assert!(batch_small <= 16_000);
        assert!(res_small >= 4 * 1024);

        cfg.context_window_tokens = 200_000;
        let (read_large, _, batch_large) = effective_presentation_caps(&cfg, 64 * 1024, &[]);
        assert!(read_large >= 96 * 1024);
        assert!(batch_large >= 128 * 1024);
    }

    #[test]
    fn codemode_output_is_distilled_and_oversized_preserves_console_tail() {
        let rows: Vec<Value> = (0..30)
            .map(|i| serde_json::json!({ "id": i, "path": format!("src/file_{i}.rs"), "line": i + 1 }))
            .collect();
        let json_out = format!("{}\nconsole:\nkept tail line", Value::Array(rows));
        let runtime = Runtime::new();
        let calls = vec![ProviderToolCall {
            id: "cm1".into(),
            name: "codemode".into(),
            arguments: r#"{"code":"return [];"}"#.into(),
        }];
        let results = vec![ToolResult::ok("codemode", json_out.clone())];
        let batch = PresentationBatch {
            id: "b1",
            calls: &calls,
            results: &results,
        };
        let config = AgentLoopConfig::default();
        let plan = runtime.presentation_plan(&batch, &[], &config, Path::new("."));
        let presented = runtime.present_batch(&plan, 1000);
        assert!(presented[0].text.len() < json_out.len());
        assert!(presented[0].text.contains("console:\nkept tail line"));

        let oversized = format!(
            "{}\nconsole:\nIMPORTANT_CONSOLE_MARKER",
            "x".repeat(20_000)
        );
        let cut = crate::tools::present_unstructured(
            "codemode",
            &oversized,
            PresentationBudget { max_bytes: 1024 },
        );
        assert!(!cut.complete);
        assert!(cut.text.contains("IMPORTANT_CONSOLE_MARKER"));
    }
}
