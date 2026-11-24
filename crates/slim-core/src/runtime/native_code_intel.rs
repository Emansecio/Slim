use super::*;

pub(super) fn code_intel_action_name(request: &CodeIntelRequest) -> &'static str {
    match request {
        CodeIntelRequest::Status { .. } => "status",
        CodeIntelRequest::Definition(_) => "definition",
        CodeIntelRequest::References(_) => "references",
        CodeIntelRequest::Hover(_) => "hover",
        CodeIntelRequest::Symbols(_) => "symbol",
        CodeIntelRequest::Diagnostics(_) => "diagnostics",
    }
}

/// Files the agent wrote or patched since the last post-edit diagnostics note,
/// reset when a loop run starts.
#[derive(Default)]
pub(super) struct EditedFiles {
    paths: Vec<PathBuf>,
    /// Post-edit checks in a row that used their whole deadline.
    slow_checks: u8,
}

impl EditedFiles {
    fn record(&mut self, targets: &[PathBuf]) {
        for path in targets {
            if !self.paths.contains(path) {
                self.paths.push(path.clone());
            }
        }
    }

    pub(super) fn remove_paths(&mut self, targets: &[PathBuf]) {
        self.paths.retain(|existing| !targets.contains(existing));
    }
}

pub(super) async fn run_code_intel_request(
    code_intel: Option<Arc<dyn CodeIntelligence>>,
    request: Result<CodeIntelRequest, String>,
    cancellation: Option<CancellationToken>,
) -> (ToolResult, Option<crate::tools::CodeIntelPresentation>) {
    let Some(code_intel) = code_intel else {
        return (
            ToolResult::fail(
                "code_intel",
                "code_intel unavailable: no language-server manager configured",
            ),
            None,
        );
    };
    let request = match request {
        Ok(request) => request,
        Err(message) => {
            return (
                ToolResult::fail("code_intel", format!("code_intel: {message}")),
                None,
            );
        }
    };
    let action = code_intel_action_name(&request);
    let cache_slot = code_intel_cache_key(&request).and_then(|(ws, key)| {
        let daemon = crate::project_index::for_workspace(&ws).ok()?;
        let epoch = daemon.fs_epoch();
        Some((daemon, key, epoch))
    });
    if let Some((daemon, key, _)) = &cache_slot {
        if let Some(hit) = daemon.lookup_code_intel(key) {
            return (hit.result, hit.presentation);
        }
    }
    let result = match request {
        CodeIntelRequest::Status { workspace } => code_intel.status(&workspace).await,
        CodeIntelRequest::Definition(mut query) => {
            query.cancellation = cancellation;
            code_intel.definition(&query).await
        }
        CodeIntelRequest::References(mut query) => {
            query.cancellation = cancellation;
            code_intel.references(&query).await
        }
        CodeIntelRequest::Hover(mut query) => {
            query.cancellation = cancellation;
            code_intel.hover(&query).await
        }
        CodeIntelRequest::Symbols(mut query) => {
            query.cancellation = cancellation;
            code_intel.symbols(&query).await
        }
        CodeIntelRequest::Diagnostics(mut query) => {
            query.cancellation = cancellation;
            code_intel.diagnostics(&query).await
        }
    };
    // Error payloads ("error" key) are failures: governors and turn
    // accounting must not observe them as successful tool calls.
    let success = result.payload.get("error").is_none();
    let cacheable = success
        && !result.meta.stale
        && result.meta.state == crate::codeintel::CodeIntelServerState::Ready
        && result.meta.completeness == crate::codeintel::CodeIntelCompleteness::Complete;
    let presentation = crate::tools::presentation_for_code_intel(action, &result);
    let tool_result = ToolResult::new("code_intel", success, presentation.full.clone());
    if cacheable {
        if let Some((daemon, key, epoch)) = cache_slot {
            daemon.store_code_intel(
                key,
                epoch,
                crate::project_index::CachedCodeIntelOutcome {
                    result: tool_result.clone(),
                    presentation: Some(presentation.clone()),
                },
            );
        }
    }
    (tool_result, Some(presentation))
}

fn code_intel_cache_key(request: &CodeIntelRequest) -> Option<(PathBuf, String)> {
    match request {
        CodeIntelRequest::Status { .. } => None,
        CodeIntelRequest::Definition(q) => Some((
            q.workspace.clone(),
            format!(
                "def|{}|{}:{}|{}|{}|{:?}|{:?}",
                q.path.display(),
                q.line,
                q.column,
                q.max_results,
                q.offset,
                q.symbol,
                q.revision
            ),
        )),
        CodeIntelRequest::References(q) => Some((
            q.workspace.clone(),
            format!(
                "ref|{}|{}:{}|{}|{}|{:?}|{:?}",
                q.path.display(),
                q.line,
                q.column,
                q.max_results,
                q.offset,
                q.symbol,
                q.revision
            ),
        )),
        CodeIntelRequest::Hover(q) => Some((
            q.workspace.clone(),
            format!(
                "hov|{}|{}:{}|{:?}",
                q.path.display(),
                q.line,
                q.column,
                q.symbol
            ),
        )),
        CodeIntelRequest::Symbols(q) => Some((
            q.workspace.clone(),
            format!(
                "sym|{:?}|{:?}|{:?}|{}|{}|{:?}",
                q.path, q.server, q.query, q.max_results, q.offset, q.revision
            ),
        )),
        CodeIntelRequest::Diagnostics(q) => Some((
            q.workspace.clone(),
            format!(
                "diag|{:?}|{:?}|{}|{}",
                q.path, q.server, q.include_info, q.max_results
            ),
        )),
    }
}

pub(super) fn prepared_code_intel_request(
    prepared: &PreparedToolInvocation,
) -> Result<CodeIntelRequest, String> {
    if let Some(error) = &prepared.error {
        return Err(error.clone());
    }
    match &prepared.arguments {
        PreparedToolArguments::CodeIntel(request) => Ok(request.clone()),
        _ => Err("prepared code_intel arguments are unavailable".into()),
    }
}

/// Puts the admission feedback in front of a code_intel result, as native
/// tools get it, and returns the presentation source that carries the same
/// prefix.
pub(super) fn admit_code_intel_output(
    result: &mut ToolResult,
    prepared: &PreparedToolInvocation,
    presentation: Option<crate::tools::CodeIntelPresentation>,
) -> Option<ToolPresentationSource> {
    let prefix = crate::tools::admission_output_prefix(&prepared.admission_notes);
    if let Some(prefix) = &prefix {
        result.output.insert_str(0, prefix);
    }
    presentation.map(|presentation| ToolPresentationSource::CodeIntel {
        prefix: prefix.unwrap_or_default(),
        presentation,
    })
}

/// How long the next request waits for the language server to validate a batch
/// of edits. Bounded so a silent server costs one short pause, not a stall.
pub(super) const POST_EDIT_DIAGNOSTICS_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(1500);
/// The wait once the server used two whole deadlines in a row.
const POST_EDIT_DIAGNOSTICS_SHORT_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(250);

/// Short opportunistic deadline when the cell already validated green and called `finish()`.
const POST_EDIT_DIAGNOSTICS_CONCLUDED_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(100);

impl Runtime {
    /// Runs one code_intel tool call against the installed CodeIntelligence
    /// facade. Only this async path can serve it: LSP queries await
    /// language-server responses.
    pub(super) async fn execute_code_intel(
        &mut self,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started_at = self.begin_tool(invocation, &mut seq)?;
        let request = prepared_code_intel_request(prepared);
        let (mut outcome, semantic_presentation) =
            run_code_intel_request(self.code_intel.clone(), request, self.cancellation.clone())
                .await;
        if self.is_cancelled() {
            outcome.success = false;
        }
        if let Some(source) = admit_code_intel_output(&mut outcome, prepared, semantic_presentation)
        {
            self.presentation_sources.insert(
                (
                    invocation.batch_id.to_owned(),
                    invocation.call_id.to_owned(),
                ),
                source,
            );
        }
        self.finish_tool(invocation, &mut outcome, started_at, &mut seq, None)?;
        Ok((outcome, seq))
    }

    /// Prepares the async LSP `didChange`/`didSave` sync future for a mutation,
    /// recording edited paths immediately. Returns `None` when no LSP sync is needed.
    /// Does not duplicate `ProjectIndexDaemon::notify_local_write`, which `ToolRegistry`
    /// already invokes on write/patch.
    pub(super) fn prepare_code_intel_sync_task(
        &mut self,
        cwd: &Path,
        invocation_name: &str,
        prepared: &PreparedToolInvocation,
        outcome: &ToolExecutionOutcome,
    ) -> Option<futures_util::future::BoxFuture<'static, ()>> {
        if (!outcome.result.success
            && outcome.receipt.mutations.is_empty()
            && !outcome.receipt.effects_uncertain)
            || !matches!(invocation_name, "write" | "patch")
        {
            return None;
        }
        let code_intel = self.code_intel.clone()?;
        if prepared.target_paths.is_empty() {
            return None;
        }
        self.edited.record(&prepared.target_paths);
        let text = if outcome.receipt.effects_uncertain || outcome.receipt.fused_shell.is_some() {
            None
        } else {
            match &prepared.arguments {
                PreparedToolArguments::Write { content, .. } => {
                    Some(crate::codeintel::CodeIntelFileUpdate {
                        text: content.clone(),
                        patch: None,
                    })
                }
                PreparedToolArguments::Patch { .. } => outcome.receipt.synced_text.clone(),
                _ => None,
            }
        };
        let cwd = cwd.to_path_buf();
        let target_paths = prepared.target_paths.clone();
        let cancellation = self.cancellation.clone();
        Some(Box::pin(async move {
            let synchronize = async {
                for (idx, absolute) in target_paths.iter().enumerate() {
                    if idx == 0 {
                        if let Some(update) = text.clone() {
                            code_intel.notify_file_updated(&cwd, absolute, update).await;
                            continue;
                        }
                    }
                    code_intel.notify_file_changed(&cwd, absolute, None).await;
                }
            };
            if let Some(cancellation) = cancellation {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {},
                    _ = synchronize => {},
                }
            } else {
                synchronize.await;
            }
        }))
    }

    /// Best-effort didChange/didSave push after the agent wrote a file. The
    /// warm-only implementation may no-op, but when it does synchronize, the
    /// await establishes protocol order before the next tool call.
    pub(super) async fn notify_code_intel_after_mutation(
        &mut self,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        outcome: &ToolExecutionOutcome,
    ) {
        if let Some(task) =
            self.prepare_code_intel_sync_task(cwd, invocation.name, prepared, outcome)
        {
            task.await;
        }
    }

    /// Errors the language server sees in the files this batch wrote or
    /// patched, with explicit coverage in the next request. When `concluded_green`
    /// is true (`finish()` after a passing build/test), uses a short opportunistic
    /// deadline and only surfaces real errors (never coverage-only notes).
    pub(super) async fn post_edit_diagnostics_note_for(
        &mut self,
        cwd: &Path,
        concluded_green: bool,
    ) -> Option<String> {
        let paths = std::mem::take(&mut self.edited.paths);
        if paths.is_empty() {
            return None;
        }
        let code_intel = self.code_intel.as_ref()?;
        // A server that keeps using the whole deadline gets a short one; every
        // fourth check tries the full one again, so a server that warmed up
        // is trusted again.
        let slow = self.edited.slow_checks;
        let deadline = if concluded_green {
            POST_EDIT_DIAGNOSTICS_CONCLUDED_DEADLINE
        } else if slow >= 2 && slow % 4 != 0 {
            POST_EDIT_DIAGNOSTICS_SHORT_DEADLINE
        } else {
            POST_EDIT_DIAGNOSTICS_DEADLINE
        };
        let started = std::time::Instant::now();
        let report = code_intel
            .diagnostics_after_edits(cwd, &paths, deadline, self.cancellation.clone())
            .await;
        if !concluded_green {
            self.edited.slow_checks = if started.elapsed() >= deadline {
                slow.wrapping_add(1)
            } else {
                0
            };
        }
        let report = report?;
        crate::codeintel::render_edit_diagnostics(&report, !concluded_green)
            .map(|note| self.redact_sensitive(&note))
    }
}
