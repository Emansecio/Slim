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
    /// request still fits the window (and stays under the hard threshold).
    pub(super) fn plan_tool_presentations<A: ProviderAdapter>(
        &self,
        ctx: &LoopCtx<'_, A>,
        st: &LoopState<'_>,
        tools: &[Value],
        batch: &PresentationBatch<'_>,
    ) -> Vec<ToolPresentation> {
        let config = st.config;
        let compaction_policy = self.compaction_policy();
        // The loop treats the hard threshold as inclusive (`>=`). Keep the
        // projected request strictly below the same configured line so a
        // newly completed tool batch does not immediately trigger a second
        // compaction that would add summary overhead to the batch.
        let hard_threshold = (config.context_compaction_enabled && compaction_policy.enabled)
            .then(|| compaction_policy.hard_threshold_tokens(config.context_window_tokens));
        let plan = self.presentation_plan(batch, st.messages, &config, ctx.cwd);
        let fit = RequestFit {
            client: ctx.client,
            tools,
            mode: ctx.mode,
            hard_threshold,
            reserve_tokens: config.context_reserve_tokens,
            window_tokens: config.context_window_tokens,
        };
        largest_fitting_scale(
            |scale| self.present_batch(&plan, scale),
            |presentations| self.request_fits(&fit, &plan, presentations),
        )
    }

    /// The scale-independent facts of a batch, computed once.
    fn presentation_plan<'a>(
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
        let targets = results
            .iter()
            .zip(&sources)
            .map(|(result, source)| {
                let cap = if result.name == "read" {
                    self.read_presentation_bytes
                } else {
                    config.max_result_bytes
                };
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
        PresentationPlan {
            base_messages,
            calls,
            results,
            duplicates: names.iter().map(|name| duplicate_pointer(name)).collect(),
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
        }
    }

    /// Every call of the batch presented at `scale` per mille of its target.
    fn present_batch(&self, plan: &PresentationPlan<'_>, scale: usize) -> Vec<ToolPresentation> {
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
        if result.success && already_in_context(&result.output) {
            return ToolPresentation::complete(duplicate.clone());
        }
        let suffix = plan.suffixes[index].as_ref();
        let allowance = plan.targets[index].saturating_mul(scale).div_ceil(1000);
        let body_budget = allowance.saturating_sub(suffix.map_or(0, String::len));
        let mut presentation = plan.sources[index]
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
        if result.success && already_in_context(&presentation.text) {
            ToolPresentation::complete(duplicate.clone())
        } else {
            presentation
        }
    }

    /// Whether the request with these presentations appended stays inside
    /// the window and under the hard threshold.
    fn request_fits<A: ProviderAdapter>(
        &self,
        fit: &RequestFit<'_, A>,
        plan: &PresentationPlan<'_>,
        presentations: &[ToolPresentation],
    ) -> bool {
        let client = fit.client;
        let mut candidate = plan.base_messages.to_vec();
        for ((name, id), presentation) in plan.names.iter().zip(&plan.ids).zip(presentations) {
            candidate.push(ProviderMessage::tool(
                name.clone(),
                id.clone(),
                presentation.text.clone(),
            ));
        }
        // The loop's preflight uses the conservative structural estimate
        // before preparing the wire request, and that estimate never
        // under-counts the serialized body. When the adapter bounds its
        // request envelope the structural estimate alone is the decision
        // input — the exact path the loop already takes — so transport
        // metadata is only built for adapters without the bound.
        let overlay = self.overlay_channel(&mut candidate, fit.mode);
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
                let Ok(request) =
                    self.prepare_loop_request(client, &mut candidate, fit.tools, fit.mode)
                else {
                    return false;
                };
                self.token_estimator.estimate(
                    crate::provider::provider_kind_name(client.adapter().kind()),
                    client.adapter().model(),
                    request.serialized_chars,
                )
            }
        };
        let under_hard_threshold = fit
            .hard_threshold
            .is_none_or(|threshold| budget_estimated < threshold);
        under_hard_threshold
            && budget_estimated.saturating_add(fit.reserve_tokens) <= fit.window_tokens
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
struct PresentationPlan<'a> {
    base_messages: &'a [ProviderMessage],
    calls: &'a [ProviderToolCall],
    results: &'a [ToolResult],
    names: Vec<String>,
    ids: Vec<String>,
    duplicates: Vec<String>,
    targets: Vec<usize>,
    suffixes: Vec<Option<String>>,
    sources: Vec<Option<&'a ToolPresentationSource>>,
}

/// The request envelope a presented batch must fit.
struct RequestFit<'a, A: ProviderAdapter> {
    client: &'a HttpProviderClient<A>,
    tools: &'a [Value],
    mode: crate::OperatingMode,
    hard_threshold: Option<u64>,
    reserve_tokens: u64,
    window_tokens: u64,
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
