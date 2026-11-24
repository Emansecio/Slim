use super::governor::PendingCall;
use super::*;

/// An executed batch, as `record_batch_results` consumes it.
pub(super) struct BatchRecord<'a> {
    pub(super) calls: &'a [ProviderToolCall],
    pub(super) results: &'a [ToolResult],
    pub(super) presentations: &'a [ToolPresentation],
    /// Calls whose evidence had been compacted away, consumed as reported.
    pub(super) reacquisitions: &'a mut std::collections::HashSet<String>,
    /// Journal position where the batch's events begin.
    pub(super) event_start: usize,
    /// Mid-run elision is skipped when an active stateful `previous_response_id`
    /// delta chain is preserving the remote prefix.
    pub(super) allow_elision: bool,
}

/// What recording a batch concluded.
pub(super) struct BatchOutcome {
    /// A repeated failed call must stop the run.
    pub(super) repeated_failure: bool,
    /// Earlier tool outputs were elided, so a usage figure recorded before
    /// the batch no longer describes the history.
    pub(super) elided: bool,
}

/// What every phase of one batch reads; borrowed for the whole batch.
struct BatchCtx<'a> {
    mode: crate::OperatingMode,
    cwd: &'a Path,
    batch_id: &'a str,
    calls: &'a [ProviderToolCall],
    /// Prepared once per batch. Shared, not copied, with the pool futures,
    /// which must own their input.
    prepared: &'a [Arc<PreparedToolInvocation>],
}

/// Progress of one batch: a result slot per call, in call order.
struct BatchState {
    results: Vec<Option<ToolResult>>,
    next_seq: u64,
}

impl BatchState {
    fn store(&mut self, indices: &[usize], results: impl IntoIterator<Item = ToolResult>) {
        for (&index, result) in indices.iter().zip(results) {
            self.results[index] = Some(result);
        }
    }
}

/// The two kinds of parallel wave. They share admission and the pool but
/// differ in how a journal failure is handled while outcomes stream in.
#[derive(Clone, Copy)]
enum SegmentKind {
    /// Snapshot reads: a journal failure aborts the wave at once.
    ReadOnly,
    /// Independent file mutations: side effects are already underway, so a
    /// journal failure cancels the run but every outcome is still recorded.
    Mutation,
}

impl SegmentKind {
    fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Mutation => "mutation",
        }
    }

    fn fails_fast(self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

/// Handles the pool futures of one segment share.
struct PoolEnv {
    tools: ToolRegistry,
    code_intel: Option<Arc<dyn CodeIntelligence>>,
    cancellation: Option<CancellationToken>,
}

/// One call handed to the pool, owning what its future needs.
struct PoolJob {
    /// Position inside the segment.
    slot: usize,
    /// Index inside the batch, reported by the start notice.
    index: usize,
    name: String,
    /// Redacted, as announced by `ToolStarted`.
    arguments: String,
    prepared: Arc<PreparedToolInvocation>,
    speculative: Option<SpeculativeReadEntry>,
}

struct PoolRun {
    /// One slot per segment position; evidence aliases stay `None`.
    completed: Vec<Option<ToolExecutionOutcome>>,
    /// First journal failure of a `Mutation` wave, reported once every
    /// outcome is recorded. Always `None` for `ReadOnly`, which returns it.
    first_error: Option<ProviderError>,
}

/// Announces the start, runs the call and stamps its duration.
async fn run_pool_job(
    env: &PoolEnv,
    kind: SegmentKind,
    job: PoolJob,
    started_tx: tokio::sync::mpsc::Sender<ToolStartedNotice>,
) -> (usize, PoolOutcome) {
    let PoolJob {
        slot,
        index,
        name,
        arguments,
        prepared,
        speculative,
    } = job;
    let _ = started_tx
        .send(ToolStartedNotice { index, arguments })
        .await;
    let started_at = Instant::now();
    let revision_before = env.tools.workspace_revision();
    let current_fs_epoch = prepared
        .target_paths
        .first()
        .and_then(|p| crate::project_index::existing_for_path(p))
        .map_or(0, |d| d.fs_epoch());
    let mut outcome = if name == "code_intel" {
        if let Some(entry) = speculative {
            let valid_pre = prepared.error.is_none()
                && entry.revision_before == revision_before
                && (entry.fs_epoch_before == 0 || current_fs_epoch == 0 || entry.fs_epoch_before == current_fs_epoch)
                && (entry.fingerprint.is_empty() || entry.fingerprint == prepared.canonical_fingerprint);
            if valid_pre {
                if let Ok(Some(SpeculativeOutcome::CodeIntel {
                    fingerprint,
                    mut result,
                    presentation,
                })) = entry.handle.await
                {
                    if fingerprint == prepared.canonical_fingerprint
                        && env.tools.workspace_revision() == revision_before
                        && result.success
                    {
                        let admitted = super::native_code_intel::admit_code_intel_output(
                            &mut result,
                            &prepared,
                            presentation,
                        );
                        ToolExecutionOutcome {
                            result,
                            receipt: ToolExecutionReceipt {
                                presentation: admitted,
                                ..ToolExecutionReceipt::unobserved(
                                    revision_before,
                                    env.tools.workspace_revision(),
                                    elapsed_micros(started_at),
                                )
                            },
                        }
                    } else {
                        code_intel_outcome(env, &prepared, started_at, revision_before).await
                    }
                } else {
                    code_intel_outcome(env, &prepared, started_at, revision_before).await
                }
            } else {
                entry.handle.abort();
                code_intel_outcome(env, &prepared, started_at, revision_before).await
            }
        } else {
            code_intel_outcome(env, &prepared, started_at, revision_before).await
        }
    } else if let Some(entry) = speculative {
        if prepared.error.is_none()
            && (entry.fingerprint.is_empty() || entry.fingerprint == prepared.canonical_fingerprint)
            && entry.revision_before == revision_before
            && env.tools.workspace_revision() == entry.revision_before
            && (entry.fs_epoch_before == 0 || current_fs_epoch == 0 || entry.fs_epoch_before == current_fs_epoch)
        {
            if let Ok(Some(SpeculativeOutcome::Native {
                fingerprint: fp,
                outcome: spec_outcome,
            })) = entry.handle.await
            {
                if fp == prepared.canonical_fingerprint
                    && env.tools.workspace_revision() == revision_before
                    && spec_outcome.receipt.revision_before == revision_before
                    && spec_outcome.result.success
                {
                    spec_outcome
                } else {
                    native_outcome(env, kind, &name, &prepared, started_at, revision_before).await
                }
            } else {
                native_outcome(env, kind, &name, &prepared, started_at, revision_before).await
            }
        } else {
            entry.handle.abort();
            native_outcome(env, kind, &name, &prepared, started_at, revision_before).await
        }
    } else {
        native_outcome(env, kind, &name, &prepared, started_at, revision_before).await
    };
    if env
        .cancellation
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        outcome.result.success = false;
    }
    (
        slot,
        PoolOutcome {
            outcome,
            duration_ms: elapsed_millis(started_at),
        },
    )
}

/// Code-intel runs outside the registry's synchronous executor.
async fn code_intel_outcome(
    env: &PoolEnv,
    prepared: &PreparedToolInvocation,
    started_at: Instant,
    revision_before: u64,
) -> ToolExecutionOutcome {
    let request = prepared_code_intel_request(prepared);
    let (mut result, semantic_presentation) =
        run_code_intel_request(env.code_intel.clone(), request, env.cancellation.clone()).await;
    // Keep the same admission feedback contract as native tools, including
    // batch calls, cache aliases, and the model-facing prompt copy.
    let presentation = super::native_code_intel::admit_code_intel_output(
        &mut result,
        prepared,
        semantic_presentation,
    );
    ToolExecutionOutcome {
        result,
        receipt: ToolExecutionReceipt {
            presentation,
            ..ToolExecutionReceipt::unobserved(
                revision_before,
                env.tools.workspace_revision(),
                elapsed_micros(started_at),
            )
        },
    }
}

/// Runs a native tool on the blocking pool. The cancellation token tracks the
/// work until the blocking task really stops; a task that fails to join
/// becomes a failed result instead of aborting the segment.
async fn native_outcome(
    env: &PoolEnv,
    kind: SegmentKind,
    name: &str,
    prepared: &Arc<PreparedToolInvocation>,
    started_at: Instant,
    revision_before: u64,
) -> ToolExecutionOutcome {
    let native_work = env
        .cancellation
        .as_ref()
        .map(CancellationToken::track_native_work);
    let tools = env.tools.clone();
    let blocking_prepared = Arc::clone(prepared);
    let cancellation = env.cancellation.clone();
    tokio::task::spawn_blocking(move || {
        let _native_work = native_work;
        tools.execute_prepared_with_cancellation_and_progress(
            &blocking_prepared,
            cancellation.as_ref(),
            |_| {},
        )
    })
    .await
    .unwrap_or_else(|error| ToolExecutionOutcome {
        result: ToolResult::fail(
            name.to_owned(),
            format!("{} tool task failed: {error}", kind.label()),
        ),
        receipt: ToolExecutionReceipt::unobserved(
            revision_before,
            env.tools.workspace_revision(),
            elapsed_micros(started_at),
        ),
    })
}

/// The leader's outcome, restated for an alias: no execution cost and the
/// alias's own admission prefix.
fn evidence_alias_outcome(
    leader: &ToolExecutionOutcome,
    leader_prepared: &PreparedToolInvocation,
    alias_prepared: &PreparedToolInvocation,
) -> ToolExecutionOutcome {
    let mut reused = leader.clone();
    reused.receipt.execution_us = 0;
    reused.receipt.finalization_us = 0;
    if let Some(presentation) = reused.receipt.presentation.take() {
        let from = crate::tools::admission_output_prefix(&leader_prepared.admission_notes)
            .unwrap_or_default();
        let to = crate::tools::admission_output_prefix(&alias_prepared.admission_notes)
            .unwrap_or_default();
        reused.receipt.presentation = Some(presentation.replace_prefix(from, to));
    }
    replace_admission_prefix(
        &mut reused.result.output,
        &leader_prepared.admission_notes,
        &alias_prepared.admission_notes,
    );
    reused
}

impl Runtime {
    /// Appends one tool message per executed call, reports reused evidence
    /// and folds each outcome into the loop guard.
    pub(super) async fn record_batch_results(
        &mut self,
        batch: BatchRecord<'_>,
        guard: &mut LoopGuard,
        messages: &mut Vec<ProviderMessage>,
        next_seq: &mut u64,
    ) -> Result<BatchOutcome, ProviderError> {
        let BatchRecord {
            calls,
            results,
            presentations,
            reacquisitions,
            event_start,
            allow_elision,
        } = batch;
        let mut repeated_failure_in_batch = false;
        let mut mutation_succeeded = false;
        let batch_advisory = self.run_guards.observe_batch_health(calls, results);
        let call_count = calls.len();
        for (call_index, ((call, result), presentation)) in
            calls.iter().zip(results.iter()).zip(presentations.iter()).enumerate()
        {
            let is_mutation = matches!(call.name.as_str(), "write" | "patch");
            let codemode_effects = matches!(call.name.as_str(), "codemode" | "batch_code")
                .then(|| self.codemode.effects_for(&call.id))
                .unwrap_or_default();
            let codemode_mutated = codemode_effects.mutated_workspace;
            let mutation_changed = is_mutation
                && self.app.events()[event_start..].iter().any(|event| {
                    matches!(&event.kind, crate::EventKind::CausalProgressObserved {
                        call_id, kind: crate::CausalProgressKind::WorkspaceChanged, ..
                    } if call_id.as_ref() == call.id)
                });
            let ok = result.success || mutation_changed;
            self.scratchpad
                .record_tool_outcome(&call.name, &call.arguments, ok, &result.output);
            let full_output = presentation.text.clone();
            // Cut to fit the request, the model did not receive all of it.
            let shown = full_output == result.output;
            let held = self.run_guards.was_held(&call.id);
            let tool_name = self.redact_sensitive(&call.name);
            let pointer = duplicate_pointer(&tool_name);
            let already_in_context = |text: &str| {
                pointer.len() < text.len()
                    && tool_output_already_in_context(messages, &tool_name, text)
            };
            let duplicate_in_active_context = result.success
                && result.media.is_empty()
                && (already_in_context(&full_output)
                    || (full_output != result.output && already_in_context(&result.output)));
            let full_output_bytes = if full_output == pointer {
                result.output.len() as u64
            } else {
                full_output.len() as u64
            };
            if ok {
                guard.record_success(&call.name);
                mutation_succeeded |= is_mutation || codemode_mutated;
                if codemode_effects.had_non_readonly_effect {
                    guard.record_success("write");
                }
                if matches!(
                    call.name.as_str(),
                    "shell" | "write" | "patch" | "ask_question"
                ) {
                    repeated_failure_in_batch = false;
                }
            }
            let mut output = if duplicate_in_active_context {
                pointer
            } else {
                full_output
            };
            if let Some(note) = self.run_guards.settle(&call.id, result.success, shown) {
                output.push('\n');
                output.push_str(&note);
            }
            if duplicate_in_active_context {
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ToolEvidenceReused {
                        original_bytes: full_output_bytes,
                        emitted_bytes: output.len() as u64,
                        post_compaction: false,
                    },
                )?;
            }
            if reacquisitions.remove(&call.id) && !duplicate_in_active_context {
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ToolEvidenceReused {
                        original_bytes: 0,
                        emitted_bytes: 0,
                        post_compaction: true,
                    },
                )?;
            }
            if call_index + 1 == call_count {
                if let Some(advisory) = &batch_advisory {
                    if !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str(advisory);
                }
            }
            self.append_conversation_message(
                messages,
                ProviderMessage::tool(tool_name, self.redact_sensitive(&call.id), output)
                    .with_content_blocks(result.media.clone()),
            )?;
            // A volatile operation may change state even when it fails.
            // Use the existing causal boundary instead of treating its exit
            // status as proof that the workspace stayed unchanged.
            let then_run_id = format!("{}:then_run", call.id);
            let volatile_boundary = if matches!(call.name.as_str(), "codemode" | "batch_code") {
                codemode_effects.had_effect
            } else {
                self.app.events()[event_start..].iter().any(|event| {
                    matches!(&event.kind, crate::EventKind::CausalBoundaryObserved {
                        call_id, kind: crate::CausalBoundaryKind::PotentiallyVolatile, ..
                    } if call_id.as_ref() == call.id || call_id.as_ref() == then_run_id)
                })
            };
            let repeated_failure = if volatile_boundary {
                *guard = LoopGuard::default();
                repeated_failure_in_batch = false;
                false
            } else if ok || held {
                // A hold is the guard's answer, not a failure of the call:
                // the retry it invites must not count as a repetition.
                false
            } else {
                // Reuse the governor's prepared identity and dependency state.
                // Validation shells also use the observed workspace revision.
                let causal_identity =
                    self.app.events()[event_start..]
                        .iter()
                        .find_map(|event| match &event.kind {
                            crate::EventKind::CausalProgressObserved {
                                call_id,
                                call_fingerprint,
                                ..
                            }
                            | crate::EventKind::CausalAnomalyDetected {
                                call_id,
                                call_fingerprint,
                                ..
                            } if call_id.as_ref() == call.id => Some(call_fingerprint.as_ref()),
                            crate::EventKind::CausalBoundaryObserved {
                                call_id,
                                call_fingerprint,
                                ..
                            } if call_id.as_ref() == call.id => Some(call_fingerprint.as_ref()),
                            _ => None,
                        });
                let accepted = match causal_identity {
                    Some(fingerprint) => guard.accept_key(&call.name, fingerprint, &result.output),
                    None => guard.accept(&call.name, &call.arguments, &result.output),
                };
                !accepted
            };
            repeated_failure_in_batch |= repeated_failure;
        }
        let mut elided = false;
        if mutation_succeeded && allow_elision {
            let write_penalty = self.cache_write_penalty.max(1);
            let elision =
                elide_superseded_tool_outputs_with_penalty(messages, write_penalty);
            if elision.elided > 0 {
                elided = true;
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ToolEvidenceElided {
                        count: u64::from(elision.elided),
                        original_bytes: elision.original_bytes,
                        emitted_bytes: elision.emitted_bytes,
                    },
                )?;
            }
        }
        Ok(BatchOutcome {
            repeated_failure: repeated_failure_in_batch,
            elided,
        })
    }

    pub(super) async fn prepare_provider_tool_invocations(
        &self,
        mode: crate::OperatingMode,
        cwd: &Path,
        calls: &[ProviderToolCall],
    ) -> Result<Vec<PreparedToolInvocation>, ProviderError> {
        let tools = self.tools.clone();
        let cwd = cwd.to_path_buf();
        // Identical (name, arguments) pairs prepare identically within a batch
        // and are aliased to one execution: prepare each once, expand by clone.
        let mut index_of: std::collections::HashMap<(&str, &str), usize> =
            std::collections::HashMap::with_capacity(calls.len());
        let mut distinct = Vec::with_capacity(calls.len());
        let mut remap = Vec::with_capacity(calls.len());
        for call in calls {
            if let Some(&found) = index_of.get(&(call.name.as_str(), call.arguments.as_str())) {
                remap.push(found);
            } else {
                index_of.insert(
                    (call.name.as_str(), call.arguments.as_str()),
                    distinct.len(),
                );
                distinct.push((call.name.clone(), call.arguments.clone()));
                remap.push(distinct.len() - 1);
            }
        }
        let prepared =
            tokio::task::spawn_blocking(move || tools.prepare_invocations(mode, cwd, &distinct))
                .await
                .map_err(|error| ProviderError::InvalidResponse {
                    message: format!("tool preparation task failed: {error}"),
                })?;
        let shell_may_run =
            self.shell_jobs.running() || calls.iter().any(|call| call.name == "shell");
        Ok(remap
            .into_iter()
            .map(|index| {
                let mut prepared = prepared[index].clone();
                // Parallel read and mutation waves execute `prepared` without
                // the per-call dispatch below, so the refusal travels with it.
                if let Some(refusal) = super::mode::code_mode_direct_refusal(mode, &prepared.name) {
                    prepared.reject(&refusal);
                }
                if self.tuning_closed && prepared.error.is_none() && prepared.is_tuning_patch() {
                    prepared.reject(TUNING_CLOSED_ERROR);
                }
                if shell_may_run {
                    if let Some(spec) = prepared.spec.as_mut() {
                        spec.cacheability = crate::tools::ToolCacheability::None;
                        spec.replay_policy = crate::tools::ToolReplayPolicy::Never;
                    }
                }
                prepared
            })
            .collect())
    }

    pub(super) async fn execute_provider_tool_batch(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        batch_id: &str,
        calls: &[ProviderToolCall],
        next_seq: u64,
        governor: &mut CausalGovernor,
    ) -> Result<(Vec<ToolResult>, u64), ProviderError> {
        // Even callers without a host token own their native work until it
        // stops. Dropping a blocking-task future is not a termination barrier.
        let previous = self.cancellation.clone();
        let cancellation = previous.clone().unwrap_or_default();
        self.cancellation = Some(cancellation.clone());
        let started = std::time::Instant::now();
        let mut result = self
            .execute_provider_tool_batch_inner(mode, cwd, batch_id, calls, next_seq, governor)
            .await;
        if result.is_ok() && !calls.is_empty() {
            let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            if let Err(error) =
                super::events::persist_tool_batch(&self.app.run_journal, batch_id, calls, wall_ms)
            {
                result = Err(error);
            }
        }
        if result.is_err() || cancellation.is_cancelled() {
            cancellation.cancel();
            self.clear_speculative_reads();
            cancellation.wait_for_native_work().await;
        } else {
            self.clear_speculative_reads();
            // Managed jobs outlive a batch, but remain owned by the run.
            cancellation.wait_for_foreground_work().await;
        }
        self.cancellation = previous;
        result
    }

    pub(super) async fn execute_provider_tool_batch_inner(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        batch_id: &str,
        calls: &[ProviderToolCall],
        mut next_seq: u64,
        governor: &mut CausalGovernor,
    ) -> Result<(Vec<ToolResult>, u64), ProviderError> {
        if calls.is_empty() || self.is_cancelled() {
            return Ok((Vec::new(), next_seq));
        }
        self.presentation_sources.clear();
        let prepared = self
            .prepare_and_announce(mode, cwd, batch_id, calls, &mut next_seq)
            .await?;
        let ctx = BatchCtx {
            mode,
            cwd,
            batch_id,
            calls,
            prepared: &prepared,
        };
        let mut st = BatchState {
            results: vec![None; calls.len()],
            next_seq,
        };
        // Independent snapshot reads run first so a read of an unrelated file
        // does not wait on a mutation; results stay in call order.
        let phase1 = phase1_snapshot_indices(&self.tools, ctx.prepared);
        self.execute_read_only_tool_segment(&ctx, &mut st, governor, &phase1)
            .await?;
        let mut index = 0;
        while index < calls.len() {
            if self.is_cancelled() {
                break;
            }
            if st.results[index].is_some() {
                index += 1;
                continue;
            }
            // Multiple `subagent` calls emitted together in one turn run concurrently (up to 6),
            // even when phase-1 snapshot reads or a `todo` update sit between them in the batch.
            if ctx.calls[index].name == "subagent" {
                let mut cluster = vec![index];
                let mut probe = index + 1;
                while probe < ctx.calls.len() {
                    if st.results[probe].is_some() {
                        probe += 1;
                        continue;
                    }
                    if ctx.calls[probe].name == "subagent" {
                        cluster.push(probe);
                    } else if ctx.calls[probe].name != "todo" {
                        break;
                    }
                    probe += 1;
                }
                if cluster.len() >= 2 {
                    for &ci in &cluster {
                        push_runtime_event(
                            &mut self.app,
                            &mut st.next_seq,
                            crate::EventKind::ToolAdmitted {
                                batch_id: ctx.batch_id.into(),
                                call_id: ctx.calls[ci].id.clone(),
                                name: ctx.calls[ci].name.clone(),
                            },
                        )?;
                    }
                    let outcomes = self
                        .execute_subagent_cluster(ctx.batch_id, ctx.calls, &cluster, &mut st.next_seq)
                        .await?;
                    for (ci, res) in outcomes {
                        st.results[ci] = Some(res);
                    }
                    index += 1;
                    continue;
                }
            }
            if ctx.prepared[index].error.is_none()
                && super::native_mcp_direct::is_direct_mcp_call(&ctx.calls[index].name)
                && self
                    .mcp
                    .as_ref()
                    .is_some_and(|manager| manager.is_direct_tool_read_only(&ctx.calls[index].name))
            {
                let mut cluster = vec![index];
                let mut probe = index + 1;
                while probe < ctx.calls.len() {
                    if st.results[probe].is_some() {
                        probe += 1;
                        continue;
                    }
                    let probe_name = &ctx.calls[probe].name;
                    if ctx.prepared[probe].error.is_none()
                        && super::native_mcp_direct::is_direct_mcp_call(probe_name)
                        && self
                            .mcp
                            .as_ref()
                            .is_some_and(|manager| manager.is_direct_tool_read_only(probe_name))
                    {
                        cluster.push(probe);
                        probe += 1;
                    } else {
                        break;
                    }
                }
                if cluster.len() >= 2 {
                    for &ci in &cluster {
                        push_runtime_event(
                            &mut self.app,
                            &mut st.next_seq,
                            crate::EventKind::ToolAdmitted {
                                batch_id: ctx.batch_id.into(),
                                call_id: ctx.calls[ci].id.clone(),
                                name: ctx.calls[ci].name.clone(),
                            },
                        )?;
                    }
                    let outcomes = self
                        .execute_direct_mcp_cluster(
                            ctx.mode,
                            ctx.batch_id,
                            ctx.calls,
                            &cluster,
                            &mut st.next_seq,
                        )
                        .await?;
                    for (ci, res) in outcomes {
                        st.results[ci] = Some(res);
                    }
                    index += 1;
                    continue;
                }
            }
            let cluster = independent_mutation_cluster(ctx.prepared, &st.results, index);
            let mutation_wave = if !cluster.is_empty() {
                cluster
            } else if is_file_mutation(&ctx.prepared[index])
                && !is_serial_barrier(&ctx.prepared[index])
            {
                vec![index]
            } else {
                Vec::new()
            };
            if !mutation_wave.is_empty() {
                let disjoint_reads = disjoint_ready_reads_for_mutations(
                    &self.tools,
                    ctx.prepared,
                    &st.results,
                    &mutation_wave,
                );
                if mutation_wave.len() >= 2 || !disjoint_reads.is_empty() {
                    self.execute_combined_mutation_and_read_segment(
                        &ctx,
                        &mut st,
                        governor,
                        &mutation_wave,
                        &disjoint_reads,
                    )
                    .await?;
                    self.anticipate_ready_snapshots(&ctx, &mut st, governor)
                        .await?;
                    continue;
                }
            }
            if !self.run_single_call(&ctx, &mut st, governor, index).await? {
                break;
            }
            let finished = &ctx.prepared[index];
            index += 1;
            if is_file_mutation(finished) || is_serial_barrier(finished) {
                self.anticipate_ready_snapshots(&ctx, &mut st, governor)
                    .await?;
            }
        }
        if !self.is_cancelled() {
            let observations = governor.finish_turn();
            self.emit_governor_observations(observations, &mut st.next_seq)?;
        }
        let results = st.results.into_iter().flatten().collect();
        Ok((results, st.next_seq))
    }

    /// Preparation is pure: prepares the whole batch once and announces it.
    async fn prepare_and_announce(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        batch_id: &str,
        calls: &[ProviderToolCall],
        next_seq: &mut u64,
    ) -> Result<Vec<Arc<PreparedToolInvocation>>, ProviderError> {
        let mut prepared = self
            .prepare_provider_tool_invocations(mode, cwd, calls)
            .await?;
        self.run_guards.begin_batch();
        for (call, prepared) in calls.iter().zip(prepared.iter_mut()) {
            self.run_guards.admit(&call.id, prepared);
        }
        for call in calls {
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::ToolPrepared {
                    batch_id: batch_id.into(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                },
            )?;
        }
        Ok(prepared.into_iter().map(Arc::new).collect())
    }

    /// Runs, as one parallel wave, the snapshot reads whose blocking
    /// mutations have completed. A wave of one gains nothing over the serial
    /// loop, which reaches that call next.
    async fn anticipate_ready_snapshots(
        &mut self,
        ctx: &BatchCtx<'_>,
        st: &mut BatchState,
        governor: &mut CausalGovernor,
    ) -> Result<(), ProviderError> {
        if self.is_cancelled() {
            return Ok(());
        }
        let ready = phase1_snapshot_indices_ready(&self.tools, ctx.prepared, 0, &st.results);
        if ready.len() >= 2 {
            self.execute_read_only_tool_segment(ctx, st, governor, &ready)
                .await?;
        }
        Ok(())
    }

    /// Runs one call on its own. `Ok(false)` means the run was cancelled
    /// while the call was executing.
    async fn run_single_call(
        &mut self,
        ctx: &BatchCtx<'_>,
        st: &mut BatchState,
        governor: &mut CausalGovernor,
        index: usize,
    ) -> Result<bool, ProviderError> {
        let call = &ctx.calls[index];
        let prepared = &*ctx.prepared[index];
        let mut pending = None;
        if !(matches!(call.name.as_str(), "codemode" | "batch_code") && prepared.error.is_none()) {
            let (call_pending, observations) =
                governor.observe_before_identified(prepared, ctx.batch_id, &call.id);
            self.emit_governor_observations(observations, &mut st.next_seq)?;
            pending = Some(call_pending);
        }
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ToolAdmitted {
                batch_id: ctx.batch_id.into(),
                call_id: call.id.clone(),
                name: call.name.clone(),
            },
        )?;
        self.shell_ends_batch = st.results[index + 1..].iter().all(Option::is_some);
        let execution = self
            .execute_provider_tool_call(
                ctx.mode,
                ctx.cwd,
                ToolInvocation::provider(ctx.batch_id, call),
                prepared,
                governor,
                st.next_seq,
            )
            .await;
        let (outcome, following_seq) = match execution {
            Ok(completed) => completed,
            Err(ProviderError::Cancelled) if self.is_cancelled() => return Ok(false),
            Err(error) => return Err(error),
        };
        st.next_seq = following_seq;
        if let Some(pending) = pending {
            let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
            self.emit_governor_observations(observations, &mut st.next_seq)?;
        }
        if let Some(presentation) = outcome.receipt.presentation.clone() {
            self.presentation_sources
                .insert((ctx.batch_id.to_owned(), call.id.clone()), presentation);
        }
        st.results[index] = Some(outcome.result);
        Ok(true)
    }

    /// Runs snapshot reads in parallel. Identical reusable evidence executes
    /// once; its aliases are answered from the leader. A journal failure
    /// aborts the wave immediately.
    async fn execute_read_only_tool_segment(
        &mut self,
        ctx: &BatchCtx<'_>,
        st: &mut BatchState,
        governor: &mut CausalGovernor,
        indices: &[usize],
    ) -> Result<(), ProviderError> {
        if indices.is_empty() {
            return Ok(());
        }
        let alias_of = evidence_reuse_aliases(indices.iter().map(|&index| &*ctx.prepared[index]));
        let Some(pending_calls) =
            self.admit_segment(ctx, governor, indices, &alias_of, &mut st.next_seq)?
        else {
            return Ok(());
        };
        let PoolRun { mut completed, .. } = self
            .drive_pool(
                ctx,
                indices,
                &alias_of,
                SegmentKind::ReadOnly,
                &mut st.next_seq,
            )
            .await?;
        self.emit_evidence_aliases(ctx, indices, &alias_of, &mut completed, &mut st.next_seq)?;
        let completed = completed
            .into_iter()
            .map(|outcome| outcome.expect("every read-only tool future yields one result"))
            .collect::<Vec<_>>();
        for (&index, outcome) in indices.iter().zip(&completed) {
            if let Some(presentation) = outcome.receipt.presentation.clone() {
                self.presentation_sources.insert(
                    (ctx.batch_id.to_owned(), ctx.calls[index].id.clone()),
                    presentation,
                );
            }
        }
        // Every call is observed before any observation is emitted.
        let observations = pending_calls
            .into_iter()
            .zip(&completed)
            .map(|(pending, outcome)| {
                governor.observe_after(pending, &outcome.result, &outcome.receipt)
            })
            .collect::<Vec<_>>();
        for call_observations in observations {
            self.emit_governor_observations(call_observations, &mut st.next_seq)?;
        }
        st.store(indices, completed.into_iter().map(|outcome| outcome.result));
        Ok(())
    }

    /// Runs file mutations on distinct files (plus any disjoint ready snapshot
    /// reads that touch no mutated path) in parallel. A journal failure cancels
    /// the run, but side effects are already underway: every outcome is still
    /// recorded and the first failure is returned last.
    async fn execute_combined_mutation_and_read_segment(
        &mut self,
        ctx: &BatchCtx<'_>,
        st: &mut BatchState,
        governor: &mut CausalGovernor,
        mutation_indices: &[usize],
        disjoint_read_indices: &[usize],
    ) -> Result<(), ProviderError> {
        let mut indices = Vec::with_capacity(mutation_indices.len() + disjoint_read_indices.len());
        indices.extend_from_slice(mutation_indices);
        indices.extend_from_slice(disjoint_read_indices);
        let alias_of = evidence_reuse_aliases(indices.iter().map(|&index| &*ctx.prepared[index]));
        let Some(pending_calls) =
            self.admit_segment(ctx, governor, &indices, &alias_of, &mut st.next_seq)?
        else {
            return Ok(());
        };
        let PoolRun {
            mut completed,
            first_error,
        } = self
            .drive_pool(
                ctx,
                &indices,
                &alias_of,
                SegmentKind::Mutation,
                &mut st.next_seq,
            )
            .await?;
        if first_error.is_none() {
            self.emit_evidence_aliases(ctx, &indices, &alias_of, &mut completed, &mut st.next_seq)?;
        }
        let completed = completed
            .into_iter()
            .enumerate()
            .filter_map(|(slot, outcome)| {
                if outcome.is_none() && first_error.is_some() && alias_of[slot] != slot {
                    None
                } else {
                    Some(outcome.expect("every wave call yields one result"))
                }
            })
            .collect::<Vec<_>>();
        for (&index, outcome) in indices.iter().zip(&completed) {
            if let Some(presentation) = outcome.receipt.presentation.clone() {
                self.presentation_sources.insert(
                    (ctx.batch_id.to_owned(), ctx.calls[index].id.clone()),
                    presentation,
                );
            }
            if mutation_indices.contains(&index) {
                self.notify_code_intel_after_mutation(
                    ctx.cwd,
                    ToolInvocation::provider(ctx.batch_id, &ctx.calls[index]),
                    &ctx.prepared[index],
                    outcome,
                )
                .await;
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        for (pending, outcome) in pending_calls.into_iter().zip(&completed) {
            let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
            self.emit_governor_observations(observations, &mut st.next_seq)?;
        }
        st.store(&indices, completed.into_iter().map(|outcome| outcome.result));
        Ok(())
    }

    /// Governor preflight and `ToolAdmitted` for every call of a segment, in
    /// call order. `None` when the run was cancelled after the preflight.
    fn admit_segment(
        &mut self,
        ctx: &BatchCtx<'_>,
        governor: &mut CausalGovernor,
        indices: &[usize],
        alias_of: &[usize],
        next_seq: &mut u64,
    ) -> Result<Option<Vec<PendingCall>>, ProviderError> {
        let preflights = indices
            .iter()
            .map(|&index| {
                governor.observe_before_identified(
                    &ctx.prepared[index],
                    ctx.batch_id,
                    &ctx.calls[index].id,
                )
            })
            .collect::<Vec<_>>();
        if self.is_cancelled() {
            return Ok(None);
        }
        let mut pending_calls = Vec::with_capacity(indices.len());
        for ((slot, &index), (pending, observations)) in indices.iter().enumerate().zip(preflights)
        {
            let call = &ctx.calls[index];
            self.emit_governor_observations(observations, next_seq)?;
            pending_calls.push(pending);
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::ToolAdmitted {
                    batch_id: ctx.batch_id.into(),
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                },
            )?;
            // Evidence aliases have no executor future. Keep a lifecycle
            // block for those synthetic results; real leaders announce
            // ToolStarted from their pool future.
            if alias_of[slot] != slot {
                let arguments = self.redact_sensitive(&call.arguments);
                push_runtime_event(
                    &mut self.app,
                    next_seq,
                    crate::EventKind::ToolStarted {
                        batch_id: ctx.batch_id.into(),
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments,
                    },
                )?;
            }
        }
        Ok(Some(pending_calls))
    }

    /// Executes every non-alias call of a segment on the blocking pool, up to
    /// `BATCH_CONCURRENCY` at a time, and records lifecycle events as
    /// outcomes stream in.
    async fn drive_pool(
        &mut self,
        ctx: &BatchCtx<'_>,
        indices: &[usize],
        alias_of: &[usize],
        kind: SegmentKind,
        next_seq: &mut u64,
    ) -> Result<PoolRun, ProviderError> {
        let env = PoolEnv {
            tools: self.tools.clone(),
            code_intel: self.code_intel.clone(),
            cancellation: self.cancellation.clone(),
        };
        for (slot, &index) in indices.iter().enumerate() {
            if alias_of[slot] != slot {
                if let Some(dup) = self.speculative_reads.remove(&ctx.calls[index].id) {
                    dup.handle.abort();
                }
            }
        }
        let jobs = indices
            .iter()
            .enumerate()
            .filter(|&(slot, _)| alias_of[slot] == slot)
            .map(|(slot, &index)| PoolJob {
                slot,
                index,
                name: ctx.calls[index].name.clone(),
                arguments: self.redact_sensitive(&ctx.calls[index].arguments),
                prepared: Arc::clone(&ctx.prepared[index]),
                speculative: self.speculative_reads.remove(&ctx.calls[index].id),
            })
            .collect::<Vec<_>>();
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::channel::<ToolStartedNotice>(indices.len().max(1));
        let mut outcomes = futures_util::stream::iter(
            jobs.into_iter()
                .map(|job| run_pool_job(&env, kind, job, started_tx.clone())),
        )
        .buffer_unordered(BATCH_CONCURRENCY);
        let mut completed = std::iter::repeat_with(|| None)
            .take(indices.len())
            .collect::<Vec<Option<ToolExecutionOutcome>>>();
        let mut first_error = None;
        loop {
            let next = tokio::select! {
                Some(notice) = started_rx.recv() => {
                    push_tool_started_notice(
                        &mut self.app,
                        next_seq,
                        ctx.batch_id,
                        ctx.calls,
                        notice,
                    )?;
                    continue;
                }
                next = outcomes.next() => next,
            };
            drain_tool_started_notices(
                &mut self.app,
                next_seq,
                ctx.batch_id,
                ctx.calls,
                &mut started_rx,
            )?;
            let Some((slot, mut pool)) = next else {
                break;
            };
            pool.outcome.result.output = self.redact_sensitive(&pool.outcome.result.output);
            if let Err(error) = self.emit_pool_completion(
                ctx.batch_id,
                &ctx.calls[indices[slot]].id,
                &pool,
                next_seq,
                kind.fails_fast(),
            ) {
                if kind.fails_fast() {
                    return Err(error);
                }
                if let Some(token) = &env.cancellation {
                    token.cancel();
                }
                first_error.get_or_insert(error);
            }
            completed[slot] = Some(pool.outcome);
        }
        drain_tool_started_notices(
            &mut self.app,
            next_seq,
            ctx.batch_id,
            ctx.calls,
            &mut started_rx,
        )?;
        Ok(PoolRun {
            completed,
            first_error,
        })
    }

    /// `ToolOutput`, the receipt's structured facts and `ToolFinished` for
    /// one pool outcome. With `fail_fast` the first journal failure stops the
    /// sequence; otherwise all three are attempted and the first failure is
    /// returned.
    fn emit_pool_completion(
        &mut self,
        batch_id: &str,
        call_id: &str,
        pool: &PoolOutcome,
        next_seq: &mut u64,
        fail_fast: bool,
    ) -> Result<(), ProviderError> {
        let outcome = &pool.outcome;
        let output = push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ToolOutput {
                batch_id: batch_id.into(),
                call_id: call_id.into(),
                name: outcome.result.name.clone(),
                output: outcome.result.output.clone(),
            },
        );
        if fail_fast && output.is_err() {
            return output;
        }
        let edit_diff = self.redacted_edit_diff(outcome.receipt.edit_diff.as_ref());
        let facts = push_tool_result_facts(
            &mut self.app,
            next_seq,
            batch_id,
            call_id,
            &outcome.result.name,
            outcome.receipt.process.as_ref(),
            edit_diff,
        );
        if fail_fast && facts.is_err() {
            return facts;
        }
        let finished = push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ToolFinished {
                batch_id: batch_id.into(),
                call_id: call_id.into(),
                name: outcome.result.name.clone(),
                success: outcome.result.success,
                duration_ms: pool.duration_ms,
            },
        );
        output.and(facts).and(finished)
    }

    /// Answers each evidence alias from its leader's outcome: output, then
    /// finish, with zero duration.
    fn emit_evidence_aliases(
        &mut self,
        ctx: &BatchCtx<'_>,
        indices: &[usize],
        alias_of: &[usize],
        completed: &mut [Option<ToolExecutionOutcome>],
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        for (slot, &index) in indices.iter().enumerate() {
            let leader = alias_of[slot];
            if leader == slot {
                continue;
            }
            let reused = evidence_alias_outcome(
                completed[leader]
                    .as_ref()
                    .expect("evidence leader completed"),
                &ctx.prepared[indices[leader]],
                &ctx.prepared[index],
            );
            let call = &ctx.calls[index];
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::ToolOutput {
                    batch_id: ctx.batch_id.into(),
                    call_id: call.id.clone(),
                    name: reused.result.name.clone(),
                    output: reused.result.output.clone(),
                },
            )?;
            push_runtime_event(
                &mut self.app,
                next_seq,
                crate::EventKind::ToolFinished {
                    batch_id: ctx.batch_id.into(),
                    call_id: call.id.clone(),
                    name: reused.result.name.clone(),
                    success: reused.result.success,
                    duration_ms: 0,
                },
            )?;
            completed[slot] = Some(reused);
        }
        Ok(())
    }

    pub(super) fn emit_governor_observations(
        &mut self,
        observations: Vec<GovernorObservation>,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        for observation in observations {
            push_runtime_event(&mut self.app, next_seq, observation.into())?;
        }
        Ok(())
    }

    pub(super) async fn execute_provider_tool_call(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        governor: &mut CausalGovernor,
        next_seq: u64,
    ) -> Result<(ToolExecutionOutcome, u64), ProviderError> {
        let started_at = Instant::now();
        let revision_before = self.tools.workspace_revision();
        let mcp_enabled = self
            .mcp
            .as_ref()
            .is_some_and(|manager| manager.has_enabled_servers());
        let envelope_refusal = if invocation.name == "ask_question" && !self.can_ask(mode) {
            Some("ask_question is not available in this mode".to_owned())
        } else if invocation.name == "shell_job" && !mode.allows_mutation() {
            Some("shell_job is only available in Auto or Code mode".to_owned())
        } else if invocation.name == "batch_code" && !super::codemode::batch_code_enabled() {
            Some("batch_code is disabled in this session".to_owned())
        } else if invocation.name == "codemode" && !mode.allows_mutation() {
            Some("codemode is not available in Read-only or Plan mode".to_owned())
        } else {
            None
        };
        if let Some(refusal) = envelope_refusal
            .or_else(|| super::mode::code_mode_direct_refusal(mode, invocation.name))
            .or_else(|| super::mode::auto_mode_codemode_refusal(mode, invocation.name, mcp_enabled))
        {
            self.ensure_not_cancelled()?;
            let mut following = next_seq;
            let begun = self.begin_tool(invocation, &mut following)?;
            let mut result = ToolResult::fail(invocation.name, refusal);
            self.finish_tool(invocation, &mut result, begun, &mut following, None)?;
            return Ok((
                ToolExecutionOutcome {
                    result,
                    receipt: ToolExecutionReceipt::unobserved(
                        revision_before,
                        self.tools.workspace_revision(),
                        elapsed_micros(started_at),
                    ),
                },
                following,
            ));
        }
        if invocation.name == "shell_job" || (invocation.name == "shell" && mode.allows_mutation())
        {
            return self
                .execute_managed_shell(mode, invocation, prepared, next_seq)
                .await;
        }
        let (result, following) = if invocation.name == "ask_question" {
            self.execute_ask_question(invocation, next_seq).await?
        } else if invocation.name == "artifact_read" {
            self.execute_artifact_read(invocation, next_seq)?
        } else if invocation.name == "history_recall" {
            self.execute_history_recall(invocation, next_seq)?
        } else if invocation.name == "todo" {
            self.execute_todo(mode, invocation, next_seq)?
        } else if invocation.name == "skill" {
            self.execute_skill(mode, cwd, invocation, next_seq).await?
        } else if invocation.name == "code_intel" {
            self.execute_code_intel(invocation, prepared, next_seq)
                .await?
        } else if invocation.name == "mcp"
            || super::native_mcp_direct::is_direct_mcp_call(invocation.name)
        {
            self.execute_mcp(mode, invocation, next_seq).await?
        } else if matches!(invocation.name, "codemode" | "batch_code") {
            self.execute_codemode_with_governor(mode, cwd, invocation, next_seq, Some(governor))
                .await?
        } else if invocation.name == "subagent" {
            self.execute_subagent(invocation, next_seq).await?
        } else if invocation.name == "subagent_control" {
            self.execute_subagent_control(invocation, next_seq).await?
        } else {
            let (outcome, following) = self
                .execute_tool_call_async(invocation, prepared, next_seq)
                .await?;
            // Ordered LSP sync after successful workspace mutations so a
            // following semantic query cannot overtake didChange/didSave.
            self.notify_code_intel_after_mutation(cwd, invocation, prepared, &outcome)
                .await;
            return Ok((outcome, following));
        };
        Ok((
            ToolExecutionOutcome {
                result,
                receipt: ToolExecutionReceipt::unobserved(
                    revision_before,
                    self.tools.workspace_revision(),
                    elapsed_micros(started_at),
                ),
            },
            following,
        ))
    }

    pub fn execute_tool(
        &mut self,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.tools
            .configure_artifacts(self.artifact_store.clone(), &self.sensitive_values.0);
        let batch_id = format!("slim-batch-direct-{next_seq}");
        let call_id = format!("slim-call-direct-{next_seq}");
        if name == "artifact_read" {
            return self.execute_artifact_read(
                ToolInvocation {
                    batch_id: &batch_id,
                    call_id: &call_id,
                    name,
                    arguments,
                },
                next_seq,
            );
        }
        if name == "history_recall" {
            return self.execute_history_recall(
                ToolInvocation {
                    batch_id: &batch_id,
                    call_id: &call_id,
                    name,
                    arguments,
                },
                next_seq,
            );
        }
        let cwd = cwd.as_ref();
        let prepared = self.tools.prepare_invocation(mode, cwd, name, arguments);
        self.execute_tool_call(
            ToolInvocation {
                batch_id: &batch_id,
                call_id: &call_id,
                name,
                arguments,
            },
            &prepared,
            next_seq,
        )
    }

    pub(super) async fn execute_tool_call_async(
        &mut self,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        next_seq: u64,
    ) -> Result<(ToolExecutionOutcome, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut following_seq = next_seq;
        let started_at = self.begin_tool(invocation, &mut following_seq)?;
        if let Some(spec) = self.speculative_reads.remove(invocation.call_id) {
            let revision_now = self.tools.workspace_revision();
            let current_fs_epoch = prepared
                .target_paths
                .first()
                .and_then(|p| crate::project_index::existing_for_path(p))
                .map_or(0, |d| d.fs_epoch());
            if prepared.error.is_none()
                && spec.revision_before == revision_now
                && (spec.fs_epoch_before == 0 || current_fs_epoch == 0 || spec.fs_epoch_before == current_fs_epoch)
                && (spec.fingerprint.is_empty() || spec.fingerprint == prepared.canonical_fingerprint)
            {
                if let Ok(Some(SpeculativeOutcome::Native {
                    fingerprint: fp,
                    mut outcome,
                })) = spec.handle.await
                {
                    if fp == prepared.canonical_fingerprint
                        && self.tools.workspace_revision() == revision_now
                        && outcome.receipt.revision_before == revision_now
                        && outcome.result.success
                    {
                        self.finish_tool(
                            invocation,
                            &mut outcome.result,
                            started_at,
                            &mut following_seq,
                            Some(&outcome.receipt),
                        )?;
                        return Ok((outcome, following_seq));
                    }
                }
            } else {
                spec.handle.abort();
            }
        }
        let tools = self.tools.clone();
        let fallback_tools = tools.clone();
        let revision_before = tools.workspace_revision();
        let prepared = prepared.clone();
        let cancellation = self.cancellation.clone();
        let result_name = invocation.name.to_owned();
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let native_work = cancellation
            .as_ref()
            .map(CancellationToken::track_native_work);
        let task = tokio::task::spawn_blocking(move || {
            let _native_work = native_work;
            tools.execute_prepared_with_cancellation_and_progress(
                &prepared,
                cancellation.as_ref(),
                |progress| {
                    let _ = progress_tx.send(progress);
                },
            )
        });
        tokio::pin!(task);
        let mut progress_error = None;
        let mut progress_open = true;
        let mut outcome = loop {
            tokio::select! {
                joined = &mut task => {
                    break joined.unwrap_or_else(|error| ToolExecutionOutcome {
                        result: ToolResult::fail(result_name.clone(), format!("native tool task failed: {error}")),
                        receipt: ToolExecutionReceipt::unobserved(
                            revision_before,
                            fallback_tools.workspace_revision(),
                            elapsed_micros(started_at),
                        ),
                    });
                }
                progress = progress_rx.recv(), if progress_open => {
                    let Some(progress) = progress else {
                        progress_open = false;
                        continue;
                    };
                    if progress_error.is_none() {
                        if let Err(error) = push_tool_progress(
                            &mut self.app,
                            &mut following_seq,
                            &self.sensitive_values.0,
                            invocation,
                            &progress.preview,
                        ) {
                            // Only here, while the task still runs, does a
                            // failed push stop the call.
                            if let Some(cancellation) = &self.cancellation {
                                cancellation.cancel();
                            }
                            progress_error = Some(error);
                        }
                    }
                }
            }
        };
        while let Ok(progress) = progress_rx.try_recv() {
            if progress_error.is_some() {
                break;
            }
            if let Err(error) = push_tool_progress(
                &mut self.app,
                &mut following_seq,
                &self.sensitive_values.0,
                invocation,
                &progress.preview,
            ) {
                progress_error = Some(error);
            }
        }
        if let Some(error) = progress_error {
            return Err(error);
        }
        if self.is_cancelled() {
            mark_cancelled_tool_outcome(&mut outcome);
        }
        if let Some(fused_shell) = outcome.receipt.fused_shell.as_mut() {
            fused_shell.1.output = self.redact_sensitive(&fused_shell.1.output);
        }
        self.finish_tool(
            invocation,
            &mut outcome.result,
            started_at,
            &mut following_seq,
            Some(&outcome.receipt),
        )?;
        Ok((outcome, following_seq))
    }

    pub(super) fn execute_tool_call(
        &mut self,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut following_seq = next_seq;
        let started_at = self.begin_tool(invocation, &mut following_seq)?;
        let mut progress_error = None;
        // Disjoint field borrows: the progress sink writes `app` while the
        // registry executes, so no handle swap or value clone is needed.
        let mut outcome = self.tools.execute_prepared_with_cancellation_and_progress(
            prepared,
            self.cancellation.as_ref(),
            |progress| {
                if progress_error.is_some() {
                    return;
                }
                if let Err(error) = push_tool_progress(
                    &mut self.app,
                    &mut following_seq,
                    &self.sensitive_values.0,
                    invocation,
                    &progress.preview,
                ) {
                    progress_error = Some(error);
                }
            },
        );
        if let Some(error) = progress_error {
            return Err(error);
        }
        // Tool cancellation is cooperative: the process may already have
        // produced a side effect, but the ledger must still close the
        // ToolStarted phase with output and a terminal ToolFinished event.
        if self.is_cancelled() {
            outcome.result.success = false;
        }
        self.finish_tool(
            invocation,
            &mut outcome.result,
            started_at,
            &mut following_seq,
            Some(&outcome.receipt),
        )?;
        Ok((outcome.result, following_seq))
    }

    /// Opens one tool lifecycle: `ToolStarted` with redacted arguments.
    /// Returns the start instant for `finish_tool`'s duration.
    pub(super) fn begin_tool(
        &mut self,
        invocation: ToolInvocation<'_>,
        seq: &mut u64,
    ) -> Result<Instant, ProviderError> {
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        Ok(Instant::now())
    }

    /// Closes one tool lifecycle. Redacts `result.output` in place so the
    /// event, the returned result and any artifact written from it agree,
    /// then emits `ToolOutput`, the receipt's structured facts (native
    /// tools only) and `ToolFinished`.
    pub(super) fn finish_tool(
        &mut self,
        invocation: ToolInvocation<'_>,
        result: &mut ToolResult,
        started_at: Instant,
        seq: &mut u64,
        receipt: Option<&ToolExecutionReceipt>,
    ) -> Result<(), ProviderError> {
        let duration_ms = elapsed_millis(started_at);
        result.output = self.redact_sensitive(&result.output);
        self.record_existing_artifact(result, seq)?;
        push_runtime_event(
            &mut self.app,
            seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output: result.output.clone(),
            },
        )?;
        if let Some(receipt) = receipt {
            let edit_diff = self.redacted_edit_diff(receipt.edit_diff.as_ref());
            push_tool_result_facts(
                &mut self.app,
                seq,
                invocation.batch_id,
                invocation.call_id,
                &result.name,
                receipt.process.as_ref(),
                edit_diff,
            )?;
        }
        push_runtime_event(
            &mut self.app,
            seq,
            crate::EventKind::ToolFinished {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                success: result.success,
                duration_ms,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::native_subagent::{SubagentRegistry, SubagentRunArgs, SubagentSpawner};
    use super::super::temp_root::TempRoot;
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct ParallelProbeSpawner {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl SubagentSpawner for ParallelProbeSpawner {
        fn run(
            &self,
            args: SubagentRunArgs,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>> {
            let active = Arc::clone(&self.active);
            let peak = Arc::clone(&self.peak);
            Box::pin(async move {
                let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(60)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(format!("done:{}", args.prompt))
            })
        }
    }

    #[tokio::test]
    async fn subagent_calls_separated_by_snapshot_read_still_run_in_parallel() {
        let root = TempRoot::new("subagent-interleaved-read");
        std::fs::write(root.join("note.txt"), "hello\n").unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let reg = SubagentRegistry::new();
        reg.set_spawner(
            Arc::new(ParallelProbeSpawner {
                active: Arc::clone(&active),
                peak: Arc::clone(&peak),
            }),
            crate::OperatingMode::Auto,
        );
        let mut rt = Runtime::new();
        rt.set_subagents(reg);

        let calls = vec![
            ProviderToolCall {
                id: "s1".into(),
                name: "subagent".into(),
                arguments: r#"{"role":"A","prompt":"task-a"}"#.into(),
            },
            ProviderToolCall {
                id: "r1".into(),
                name: "read".into(),
                arguments: r#"{"path":"note.txt"}"#.into(),
            },
            ProviderToolCall {
                id: "s2".into(),
                name: "subagent".into(),
                arguments: r#"{"role":"B","prompt":"task-b"}"#.into(),
            },
            ProviderToolCall {
                id: "t1".into(),
                name: "todo".into(),
                arguments: r#"{"todos":[{"title":"split the work","status":"in_progress"}]}"#.into(),
            },
            ProviderToolCall {
                id: "s3".into(),
                name: "subagent".into(),
                arguments: r#"{"role":"C","prompt":"task-c"}"#.into(),
            },
        ];
        let mut governor = CausalGovernor::default();
        let (results, _) = rt
            .execute_provider_tool_batch(
                crate::OperatingMode::Auto,
                &root,
                "batch-1",
                &calls,
                1,
                &mut governor,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 5);
        // The todo has no capability bridge in this runtime; only its place in the batch matters.
        assert!(
            results.iter().all(|r| r.success || r.name == "todo"),
            "{results:?}"
        );
        assert!(results[0].output.contains("task-a"));
        assert!(results[1].output.contains("hello"));
        assert!(results[2].output.contains("task-b"));
        assert!(results[4].output.contains("task-c"));
        assert!(
            peak.load(Ordering::SeqCst) >= 3,
            "subagents across a phase-1 read and a todo update must overlap"
        );
    }

    #[tokio::test]
    async fn speculative_read_is_reused_when_valid_and_discarded_on_revision_change() {
        let root = TempRoot::new("speculative-read-reuse");
        std::fs::write(root.join("fast.txt"), "from-disk\n").unwrap();

        let mut rt = Runtime::new();
        let prep = rt.tools.prepare_invocation(
            crate::OperatingMode::ReadOnly,
            &root,
            "read",
            r#"{"path":"fast.txt"}"#,
        );
        let fp = prep.canonical_fingerprint.clone();
        let rev = rt.tools.workspace_revision();

        // Inject a pre-resolved speculative outcome with a distinctive marker so we can
        // prove the pool consumed the speculative handle instead of hitting disk again.
        let mut fake_outcome =
            rt.tools
                .execute_prepared_with_cancellation_and_progress(&prep, None, |_| {});
        fake_outcome.result.output = "speculative-hit\n".into();
        let handle = tokio::spawn(async move {
            Some(SpeculativeOutcome::Native {
                fingerprint: fp.clone(),
                outcome: fake_outcome,
            })
        });
        rt.speculative_reads.insert(
            "call-hit".into(),
            SpeculativeReadEntry {
                fingerprint: prep.canonical_fingerprint.clone(),
                revision_before: rev,
                fs_epoch_before: 0,
                git_dirty_epoch_before: 0,
                handle,
            },
        );

        let calls = vec![ProviderToolCall {
            id: "call-hit".into(),
            name: "read".into(),
            arguments: r#"{"path":"fast.txt"}"#.into(),
        }];
        let mut governor = CausalGovernor::default();
        let (results, _) = rt
            .execute_provider_tool_batch(
                crate::OperatingMode::ReadOnly,
                &root,
                "b1",
                &calls,
                1,
                &mut governor,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].output, "speculative-hit\n");
        assert!(rt.speculative_reads.is_empty());

        // Stale revision must ignore speculative outcome and execute fresh read.
        let fp2 = prep.canonical_fingerprint.clone();
        let mut stale_outcome =
            rt.tools
                .execute_prepared_with_cancellation_and_progress(&prep, None, |_| {});
        stale_outcome.result.output = "stale-hit\n".into();
        stale_outcome.receipt.revision_before = rev.saturating_add(99);
        let stale_handle = tokio::spawn(async move {
            Some(SpeculativeOutcome::Native {
                fingerprint: fp2,
                outcome: stale_outcome,
            })
        });
        rt.speculative_reads.insert(
            "call-miss".into(),
            SpeculativeReadEntry {
                fingerprint: prep.canonical_fingerprint,
                revision_before: rev.saturating_add(99),
                fs_epoch_before: 0,
                git_dirty_epoch_before: 0,
                handle: stale_handle,
            },
        );
        let calls2 = vec![ProviderToolCall {
            id: "call-miss".into(),
            name: "read".into(),
            arguments: r#"{"path":"fast.txt"}"#.into(),
        }];
        let (results2, _) = rt
            .execute_provider_tool_batch(
                crate::OperatingMode::ReadOnly,
                &root,
                "b2",
                &calls2,
                10,
                &mut governor,
            )
            .await
            .unwrap();
        assert_eq!(results2[0].output, "from-disk\n");
    }
}
