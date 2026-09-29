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
/// and whether the "unverified" caveat has still to be mentioned. Reset when a
/// loop run starts.
pub(super) struct EditedFiles {
    paths: Vec<PathBuf>,
    unverified_unmentioned: bool,
}

impl Default for EditedFiles {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            unverified_unmentioned: true,
        }
    }
}

impl EditedFiles {
    fn record(&mut self, targets: &[PathBuf]) {
        for path in targets {
            if !self.paths.contains(path) {
                self.paths.push(path.clone());
            }
        }
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
    let presentation = crate::tools::presentation_for_code_intel(action, &result);
    (
        ToolResult::new("code_intel", success, presentation.full.clone()),
        Some(presentation),
    )
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

/// How long the next request waits for the language server to validate a batch
/// of edits. Bounded so a silent server costs one short pause, not a stall.
pub(super) const POST_EDIT_DIAGNOSTICS_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(1500);

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
        if let Some(prefix) = crate::tools::admission_output_prefix(&prepared.admission_notes) {
            outcome.output.insert_str(0, &prefix);
            if let Some(presentation) = semantic_presentation {
                self.presentation_sources.insert(
                    (
                        invocation.batch_id.to_owned(),
                        invocation.call_id.to_owned(),
                    ),
                    ToolPresentationSource::CodeIntel {
                        prefix,
                        presentation,
                    },
                );
            }
        } else if let Some(presentation) = semantic_presentation {
            self.presentation_sources.insert(
                (
                    invocation.batch_id.to_owned(),
                    invocation.call_id.to_owned(),
                ),
                ToolPresentationSource::CodeIntel {
                    prefix: String::new(),
                    presentation,
                },
            );
        }
        self.finish_tool(invocation, &mut outcome, started_at, &mut seq, None)?;
        Ok((outcome, seq))
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
        if (!outcome.result.success
            && outcome.receipt.mutations.is_empty()
            && !outcome.receipt.effects_uncertain)
            || !matches!(invocation.name, "write" | "patch")
        {
            return;
        }
        let Some(code_intel) = self.code_intel.as_ref() else {
            return;
        };
        let Some(absolute) = prepared.target_paths.first().cloned() else {
            return;
        };
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
        let synchronize = async {
            if let Some(update) = text {
                code_intel.notify_file_updated(cwd, &absolute, update).await;
            } else {
                code_intel.notify_file_changed(cwd, &absolute, None).await;
            }
        };
        if let Some(cancellation) = &self.cancellation {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {},
                _ = synchronize => {},
            }
        } else {
            synchronize.await;
        }
    }

    /// Errors the language server sees in the files this batch wrote or
    /// patched, as a short note for the next request. Silent when no server is
    /// warm, when nothing was edited, or when the edits are clean.
    pub(super) async fn post_edit_diagnostics_note(&mut self, cwd: &Path) -> Option<String> {
        let paths = std::mem::take(&mut self.edited.paths);
        if paths.is_empty() {
            return None;
        }
        let code_intel = self.code_intel.as_ref()?;
        let report = code_intel
            .diagnostics_after_edits(
                cwd,
                &paths,
                POST_EDIT_DIAGNOSTICS_DEADLINE,
                self.cancellation.clone(),
            )
            .await?;
        let mention = self.edited.unverified_unmentioned;
        if report
            .files
            .iter()
            .any(|file| file.verification == crate::codeintel::EditVerification::Unverified)
        {
            self.edited.unverified_unmentioned = false;
        }
        crate::codeintel::render_edit_diagnostics(&report, mention)
            .map(|note| self.redact_sensitive(&note))
    }
}
