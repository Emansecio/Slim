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
    } = job;
    let _ = started_tx
        .send(ToolStartedNotice { index, arguments })
        .await;
    let started_at = Instant::now();
    let revision_before = env.tools.workspace_revision();
    let mut outcome = if name == "code_intel" {
        code_intel_outcome(env, &prepared, started_at, revision_before).await
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
    let prefix = crate::tools::admission_output_prefix(&prepared.admission_notes);
    if let Some(prefix) = &prefix {
        result.output.insert_str(0, prefix);
    }
    let presentation =
        semantic_presentation.map(|presentation| ToolPresentationSource::CodeIntel {
            prefix: prefix.unwrap_or_default(),
            presentation,
        });
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
    /// and folds each outcome into the loop guard. Returns whether a repeated
    /// failed call must stop the run.
    pub(super) async fn record_batch_results(
        &mut self,
        batch: BatchRecord<'_>,
        guard: &mut LoopGuard,
        pending_background: &mut Option<PendingBackgroundCompaction>,
        messages: &mut Vec<ProviderMessage>,
        next_seq: &mut u64,
    ) -> Result<bool, ProviderError> {
        let BatchRecord {
            calls,
            results,
            presentations,
            reacquisitions,
            event_start,
        } = batch;
        let mut repeated_failure_in_batch = false;
        let mut mutation_succeeded = false;
        for ((call, result), presentation) in
            calls.iter().zip(results.iter()).zip(presentations.iter())
        {
            let is_mutation = matches!(call.name.as_str(), "write" | "patch");
            let mutation_changed = is_mutation
                && self.app.events()[event_start..].iter().any(|event| {
                    matches!(&event.kind, crate::EventKind::CausalProgressObserved {
                        call_id, kind: crate::CausalProgressKind::WorkspaceChanged, ..
                    } if call_id.as_ref() == call.id)
                });
            let ok = result.success || mutation_changed;
            let full_output = presentation.text.clone();
            let tool_name = self.redact_sensitive(&call.name);
            let pointer = duplicate_pointer(&tool_name);
            let already_in_context = |text: &str| {
                pointer.len() < text.len()
                    && tool_output_already_in_context(messages, &tool_name, text)
            };
            let duplicate_in_active_context = result.success
                && (already_in_context(&full_output)
                    || (full_output != result.output && already_in_context(&result.output)));
            let full_output_bytes = if full_output == pointer {
                result.output.len() as u64
            } else {
                full_output.len() as u64
            };
            if ok {
                guard.record_success(&call.name);
                mutation_succeeded |= is_mutation;
                if matches!(
                    call.name.as_str(),
                    "shell" | "write" | "patch" | "ask_question"
                ) {
                    repeated_failure_in_batch = false;
                }
            }
            let output = if duplicate_in_active_context {
                pointer
            } else {
                full_output
            };
            if duplicate_in_active_context {
                self.push_or_cancel_background(
                    pending_background,
                    next_seq,
                    crate::EventKind::ToolEvidenceReused {
                        original_bytes: full_output_bytes,
                        emitted_bytes: output.len() as u64,
                        post_compaction: false,
                    },
                    "tool_evidence_reused",
                )
                .await?;
            }
            if reacquisitions.remove(&call.id) && !duplicate_in_active_context {
                self.push_or_cancel_background(
                    pending_background,
                    next_seq,
                    crate::EventKind::ToolEvidenceReused {
                        original_bytes: 0,
                        emitted_bytes: 0,
                        post_compaction: true,
                    },
                    "tool_evidence_reused",
                )
                .await?;
            }
            self.append_conversation_message(
                messages,
                ProviderMessage::tool(tool_name, self.redact_sensitive(&call.id), output),
            )?;
            // A volatile operation may change state even when it fails.
            // Use the existing causal boundary instead of treating its exit
            // status as proof that the workspace stayed unchanged.
            let then_run_id = format!("{}:then_run", call.id);
            let volatile_boundary = self.app.events()[event_start..].iter().any(|event| {
                matches!(&event.kind, crate::EventKind::CausalBoundaryObserved {
                    call_id, kind: crate::CausalBoundaryKind::PotentiallyVolatile, ..
                } if call_id.as_ref() == call.id || call_id.as_ref() == then_run_id)
            });
            let repeated_failure = if volatile_boundary {
                *guard = LoopGuard::default();
                repeated_failure_in_batch = false;
                false
            } else if ok {
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
        if mutation_succeeded {
            let elision = elide_superseded_tool_outputs(messages);
            if elision.elided > 0 {
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
        Ok(repeated_failure_in_batch)
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
        let result = self
            .execute_provider_tool_batch_inner(mode, cwd, batch_id, calls, next_seq, governor)
            .await;
        if result.is_err() || cancellation.is_cancelled() {
            cancellation.cancel();
            cancellation.wait_for_native_work().await;
        } else {
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
            let cluster = independent_mutation_cluster(ctx.prepared, &st.results, index);
            if cluster.len() >= 2 {
                self.execute_independent_mutation_segment(&ctx, &mut st, governor, &cluster)
                    .await?;
                self.anticipate_ready_snapshots(&ctx, &mut st, governor)
                    .await?;
                continue;
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
        let prepared = self
            .prepare_provider_tool_invocations(mode, cwd, calls)
            .await?;
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
        let (pending, observations) =
            governor.observe_before_identified(prepared, ctx.batch_id, &call.id);
        self.emit_governor_observations(observations, &mut st.next_seq)?;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ToolAdmitted {
                batch_id: ctx.batch_id.into(),
                call_id: call.id.clone(),
                name: call.name.clone(),
            },
        )?;
        let execution = self
            .execute_provider_tool_call(
                ctx.mode,
                ctx.cwd,
                ToolInvocation::provider(ctx.batch_id, call),
                prepared,
                st.next_seq,
            )
            .await;
        let (outcome, following_seq) = match execution {
            Ok(completed) => completed,
            Err(ProviderError::Cancelled) if self.is_cancelled() => return Ok(false),
            Err(error) => return Err(error),
        };
        st.next_seq = following_seq;
        let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
        self.emit_governor_observations(observations, &mut st.next_seq)?;
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

    /// Runs file mutations on distinct files in parallel. A journal failure
    /// cancels the run, but the side effects are already underway: every
    /// outcome is still recorded and the first failure is returned last.
    async fn execute_independent_mutation_segment(
        &mut self,
        ctx: &BatchCtx<'_>,
        st: &mut BatchState,
        governor: &mut CausalGovernor,
        indices: &[usize],
    ) -> Result<(), ProviderError> {
        let no_aliases = (0..indices.len()).collect::<Vec<_>>();
        let Some(pending_calls) =
            self.admit_segment(ctx, governor, indices, &no_aliases, &mut st.next_seq)?
        else {
            return Ok(());
        };
        let PoolRun {
            completed,
            first_error,
        } = self
            .drive_pool(
                ctx,
                indices,
                &no_aliases,
                SegmentKind::Mutation,
                &mut st.next_seq,
            )
            .await?;
        let completed = completed
            .into_iter()
            .map(|outcome| outcome.expect("every independent mutation yields one result"))
            .collect::<Vec<_>>();
        for (&index, outcome) in indices.iter().zip(&completed) {
            self.notify_code_intel_after_mutation(
                ctx.cwd,
                ToolInvocation::provider(ctx.batch_id, &ctx.calls[index]),
                &ctx.prepared[index],
                outcome,
            )
            .await;
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        for (pending, outcome) in pending_calls.into_iter().zip(&completed) {
            let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
            self.emit_governor_observations(observations, &mut st.next_seq)?;
        }
        st.store(indices, completed.into_iter().map(|outcome| outcome.result));
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
        next_seq: u64,
    ) -> Result<(ToolExecutionOutcome, u64), ProviderError> {
        let started_at = Instant::now();
        let revision_before = self.tools.workspace_revision();
        if invocation.name == "shell_job" || (invocation.name == "shell" && mode.allows_mutation())
        {
            return self
                .execute_managed_shell(mode, invocation, prepared, next_seq)
                .await;
        }
        let special = if invocation.name == "ask_question" {
            Some(self.execute_ask_question(invocation, next_seq).await?)
        } else if invocation.name == "artifact_read" {
            Some(self.execute_artifact_read(invocation, next_seq)?)
        } else if invocation.name == "todo" {
            Some(self.execute_todo(mode, invocation, next_seq)?)
        } else if invocation.name == "skill" {
            Some(self.execute_skill(mode, cwd, invocation, next_seq).await?)
        } else if invocation.name == "code_intel" {
            Some(
                self.execute_code_intel(invocation, prepared, next_seq)
                    .await?,
            )
        } else if invocation.name == "mcp" {
            Some(self.execute_mcp(mode, invocation, next_seq).await?)
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
        let (result, following) = special.expect("special tool branch returns a result");
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
                        let preview = self.redact_sensitive(&progress.preview);
                        if let Err(error) = push_runtime_transient_event(
                            &mut self.app,
                            &mut following_seq,
                            crate::EventKind::ToolProgress {
                                batch_id: invocation.batch_id.into(),
                                call_id: invocation.call_id.into(),
                                name: invocation.name.into(),
                                preview,
                            },
                        ) {
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
            let preview = self.redact_sensitive(&progress.preview);
            if let Err(error) = push_runtime_transient_event(
                &mut self.app,
                &mut following_seq,
                crate::EventKind::ToolProgress {
                    batch_id: invocation.batch_id.into(),
                    call_id: invocation.call_id.into(),
                    name: invocation.name.into(),
                    preview,
                },
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
                let preview = redact_values(&self.sensitive_values.0, &progress.preview);
                if let Err(error) = push_runtime_transient_event(
                    &mut self.app,
                    &mut following_seq,
                    crate::EventKind::ToolProgress {
                        batch_id: invocation.batch_id.into(),
                        call_id: invocation.call_id.into(),
                        name: invocation.name.into(),
                        preview,
                    },
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
