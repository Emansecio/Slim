use super::agent_loop::LoopState;
use super::overflow::is_context_overflow_message;
use super::*;

/// Retries after an output-limit truncation and after rejected tool arguments.
pub(super) const MAX_TRUNCATION_RECOVERIES: u32 = 2;

pub(super) const MAX_ARGUMENT_REPAIRS: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProviderTurnStop {
    Normal,
    Truncated,
    Filtered,
}

impl ProviderTurnStop {
    pub(super) fn error_message(self) -> &'static str {
        match self {
            Self::Normal => "provider turn did not stop with a non-normal reason",
            Self::Truncated => "provider response was truncated",
            Self::Filtered => "provider response was filtered",
        }
    }

    pub(super) fn ensure_normal(self) -> Result<(), ProviderError> {
        if self == Self::Normal {
            Ok(())
        } else {
            Err(ProviderError::InvalidResponse {
                message: self.error_message().into(),
            })
        }
    }
}

pub(super) fn classify_provider_stop_reason(
    raw_stop_reason: &str,
    sensitive_values: &[String],
) -> Result<ProviderTurnStop, ProviderError> {
    let stop_reason = raw_stop_reason.trim().to_ascii_lowercase();
    match stop_reason.as_str() {
        "stop" | "tool_calls" | "function_call" | "end_turn" | "tool_use" | "stop_sequence"
        | "completed" | "complete" => Ok(ProviderTurnStop::Normal),
        reason
            if reason == "length"
                || reason == "incomplete"
                // Anthropic: the context window, not `max_tokens`, ended the response.
                || reason == "model_context_window_exceeded"
                || reason.contains("max_tokens")
                || reason.contains("max_output_tokens")
                || reason.contains("max_completion_tokens")
                || reason.contains("truncat") =>
        {
            Ok(ProviderTurnStop::Truncated)
        }
        reason if reason.contains("filter") || matches!(reason, "safety" | "refusal") => {
            Ok(ProviderTurnStop::Filtered)
        }
        _ => {
            let reason: String = redact_values(sensitive_values, raw_stop_reason.trim())
                .chars()
                .take(128)
                .collect();
            Err(ProviderError::InvalidResponse {
                message: format!("unsupported stop reason: {reason}"),
            })
        }
    }
}

/// Recovery counters for one agent-loop run; each bounds one recovery kind.
///
/// | recovery                  | counter scope       | resets                         | limit                        |
/// |---------------------------|---------------------|--------------------------------|------------------------------|
/// | any automatic recovery    | episode             | progress                       | `MAX_AUTOMATIC_RECOVERIES`   |
/// | retry-after wait          | episode             | progress                       | provider backoff cap         |
/// | truncated response        | episode             | progress                       | `MAX_TRUNCATION_RECOVERIES`  |
/// | tool argument repair      | episode             | progress                       | `MAX_ARGUMENT_REPAIRS`       |
/// | context overflow          | episode             | progress                       | one                          |
/// | provider retry            | consecutive         | next successful provider turn  | `MAX_PROVIDER_RECOVERIES`    |
/// | empty response            | consecutive         | next successful provider turn  | one                          |
/// | foreground compaction     | run                 | never                          | `MAX_PROVIDER_RECOVERIES`    |
/// | raised output limit       | run                 | never                          | bounded by the context window|
/// | context reserve           | run                 | never                          | follows the raised limit     |
///
/// An episode ends when the governor observes real progress
/// (`renew_after_progress`); the run-wide counters never renew.
pub(super) struct RecoveryBudget {
    pub(super) initial_context_reserve: u64,
    /// The single overflow recovery of this run was spent (a limit).
    pub(super) overflow_retry_used: bool,
    /// The next preflight compacts because of that overflow (consumed there).
    pub(super) overflow_compaction_pending: bool,
    pub(super) provider_recoveries: u32,
    pub(super) truncation_recoveries: u32,
    pub(super) recovery_output_limit: Option<u64>,
    pub(super) compaction_recoveries: u32,
    pub(super) argument_repairs: u32,
    pub(super) provider_recovery_wait: std::time::Duration,
    pub(super) automatic_recoveries: u32,
    pub(super) provider_attempts: u32,
    pub(super) empty_recovery_used: bool,
}

impl RecoveryBudget {
    pub(super) fn new(initial_context_reserve: u64) -> Self {
        Self {
            initial_context_reserve,
            overflow_retry_used: false,
            overflow_compaction_pending: false,
            provider_recoveries: 0,
            truncation_recoveries: 0,
            recovery_output_limit: None,
            compaction_recoveries: 0,
            argument_repairs: 0,
            provider_recovery_wait: std::time::Duration::ZERO,
            automatic_recoveries: 0,
            provider_attempts: 0,
            empty_recovery_used: false,
        }
    }

    /// Opens a new recovery episode after the run proved progress: the
    /// episode-scoped counters in the table above restart. Consecutive limits,
    /// the empty-response one-shot, foreground compaction retries, the raised
    /// output limit and the context reserve keep bounding the whole run.
    pub(super) fn renew_after_progress(&mut self) {
        self.automatic_recoveries = 0;
        self.provider_recovery_wait = std::time::Duration::ZERO;
        self.truncation_recoveries = 0;
        self.argument_repairs = 0;
        self.overflow_retry_used = false;
    }

    /// Another retry fits under the given consecutive counter's limit and the
    /// episode-wide automatic limit.
    pub(super) fn can_retry(&self, consecutive: u32) -> bool {
        consecutive < MAX_PROVIDER_RECOVERIES
            && self.automatic_recoveries < MAX_AUTOMATIC_RECOVERIES
    }
}

/// True for governor observations that show the task advanced. A distinct
/// failure or a changed dependency is activity, not progress, and repeated
/// evidence never reaches this point because the governor does not report it.
pub(super) fn proves_task_progress(kind: crate::CausalProgressKind) -> bool {
    matches!(
        kind,
        crate::CausalProgressKind::WorkspaceChanged
            | crate::CausalProgressKind::ValidationGreen
            | crate::CausalProgressKind::NewEvidence
            | crate::CausalProgressKind::DiagnosticsChanged
            | crate::CausalProgressKind::ExternalInput
    )
}

/// What the loop does after a provider attempt.
pub(super) enum ProviderAttempt {
    Completed(ProviderTurnResult),
    /// Resend the current turn; it does not consume the turn budget.
    Retry,
    /// A correction request that counts as a new model turn.
    NextTurn,
}

pub(super) fn is_output_limit_rejection(error: &ProviderError) -> bool {
    let (Some(400 | 422), Some(message)) = (error.status(), error.message()) else {
        return false;
    };
    let lower = message.to_ascii_lowercase();
    ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .iter()
        .any(|key| lower.contains(key))
}

/// Pi's overflow patterns over the error text, plus the provider's own error
/// code. Pi matches the text of any failed response; Slim also knows the HTTP
/// status, and a request the provider rejected can only overflow when it was
/// rejected as a client error: a server failure, an authentication or
/// permission failure, a timeout and rate limiting (401, 403, 408, 429) are
/// never an overflow, whatever their wording.
pub(super) fn is_context_overflow_error(error: &ProviderError) -> bool {
    if error.status().is_some_and(|status| {
        !(400..500).contains(&status) || matches!(status, 401 | 403 | 408 | 429)
    }) {
        return false;
    }
    if let ProviderError::Api { metadata, .. } = error {
        if matches!(
            metadata.classification_code(),
            Some(
                "context_length_exceeded"
                    | "context_window_exceeded"
                    | "model_context_window_exceeded"
                    | "prompt_too_long"
            )
        ) {
            return true;
        }
    }
    matches!(
        error,
        ProviderError::Api { .. }
            | ProviderError::TransientRemote { .. }
            | ProviderError::Remote { .. }
            | ProviderError::Http { .. }
            | ProviderError::InvalidResponse { .. }
    ) && error.message().is_some_and(is_context_overflow_message)
}

/// Pi's text for an overflow that survived its one compact-and-retry.
const OVERFLOW_RECOVERY_FAILED_SUFFIX: &str = "; context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.";

// Retry only the current request. Tool calls already emitted in this request
// are not repeated. Partial assistant text is preserved and the model is told
// to continue. Auth, spend-cap, cancellation, malformed calls and empty
// completions stay terminal. Post-send transport timeouts are uncertain at the
// HTTP layer (`safe_to_retry: false`) but are safe to reissue here when no tool
// effects exist.
pub(super) fn recoverable_provider_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport { .. } => true,
        ProviderError::Http { status, .. } => is_transient_http_status(*status),
        ProviderError::InvalidResponse { message } => matches!(
            message.as_str(),
            STREAM_ENDED_EARLY_MESSAGE | NO_STOP_REASON_MESSAGE
        ),
        other => other.is_explicit_transient(),
    }
}

pub(super) const MAX_AUTOMATIC_RECOVERIES: u32 = 6;

/// Failure texts that double as identities: created where the failure is
/// detected and compared by the recovery policy.
pub(super) const NO_STOP_REASON_MESSAGE: &str = "provider stream ended without a stop reason";

pub(super) const EMPTY_RESPONSE_MESSAGE: &str =
    "provider completed without assistant text or tool calls";

pub(super) fn is_empty_provider_response(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::InvalidResponse { message }
            if message == EMPTY_RESPONSE_MESSAGE
    )
}

pub(super) fn annotate_provider_recovery_error(
    error: ProviderError,
    scope: &str,
    attempts: u32,
    automatic_recoveries: u32,
    consecutive_recoveries: u32,
    reason: &str,
) -> ProviderError {
    let suffix = format!(
        "; automatic recovery stopped ({reason}): {scope} attempts={attempts}; automatic recoveries={automatic_recoveries}/{MAX_AUTOMATIC_RECOVERIES}; consecutive {scope} recoveries={consecutive_recoveries}/{MAX_PROVIDER_RECOVERIES}; work remains pending"
    );
    error.with_suffix(&suffix)
}

pub(super) fn request_emitted_tools(app: &AppHandle, event_start: usize) -> bool {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .any(|event| {
            matches!(
                event.kind,
                crate::EventKind::ProviderToolCall { .. }
                    | crate::EventKind::ToolCall { .. }
                    | crate::EventKind::ToolOutput { .. }
                    | crate::EventKind::ToolStarted { .. }
            )
        })
}

pub(super) const MAX_PROVIDER_RECOVERIES: u32 = 2;

pub(super) const MAX_PROVIDER_RECOVERY_WAIT: std::time::Duration =
    std::time::Duration::from_secs(60);

pub(super) fn provider_recovery_backoff(
    base: std::time::Duration,
    attempt: u32,
) -> std::time::Duration {
    let shift = attempt.saturating_sub(1).min(4);
    base.saturating_mul(1u32 << shift)
}

pub(super) fn requested_provider_recovery_delay(
    error: &ProviderError,
    attempt: u32,
    base: std::time::Duration,
) -> std::time::Duration {
    let backoff = provider_recovery_backoff(base, attempt);
    match error {
        ProviderError::Api { metadata, .. } => {
            backoff.max(metadata.retry_after.unwrap_or_default())
        }
        ProviderError::Http {
            retry_after: Some(delay),
            ..
        } => backoff.max(*delay),
        _ => backoff,
    }
}

pub(super) fn provider_recovery_delay(
    error: &ProviderError,
    attempt: u32,
    waited: std::time::Duration,
    base: std::time::Duration,
) -> Result<std::time::Duration, ProviderError> {
    let requested = requested_provider_recovery_delay(error, attempt, base);
    let remaining = MAX_PROVIDER_RECOVERY_WAIT.saturating_sub(waited);
    if requested > remaining {
        return Err(retry_wait_budget_exceeded(error, requested, remaining));
    }
    Ok(requested)
}

pub(super) fn provider_retry_reason(error: &ProviderError) -> String {
    match error {
        ProviderError::Transport { message, .. }
        | ProviderError::TransientRemote { message }
        | ProviderError::Remote { message }
        | ProviderError::Http { message, .. }
        | ProviderError::Api { message, .. }
        | ProviderError::InvalidResponse { message } => message.clone(),
        ProviderError::MalformedToolCall => "malformed tool call".into(),
        ProviderError::Cancelled => "cancelled".into(),
    }
}

pub(super) fn retry_wait_budget_exceeded(
    error: &ProviderError,
    requested: std::time::Duration,
    remaining: std::time::Duration,
) -> ProviderError {
    let suffix = format!(
        "; automatic retry requires a wait of {} ms, exceeding the remaining wait budget of {} ms; no early retry was sent; work remains pending",
        requested.as_millis(),
        remaining.as_millis()
    );
    match error {
        // Not recoverable, so it never waits; kept unannotated.
        ProviderError::Remote { .. } => error.clone(),
        other => other.clone().with_suffix(&suffix),
    }
}

pub(super) fn has_causal_provider_output(app: &AppHandle, event_start: usize) -> bool {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .any(|event| {
            matches!(
                &event.kind,
                crate::EventKind::AssistantTextDelta { .. }
                    | crate::EventKind::ReasoningDelta { .. }
                    | crate::EventKind::ProviderToolCall { .. }
                    | crate::EventKind::ToolCall { .. }
                    | crate::EventKind::ToolOutput { .. }
            )
        })
}

/// What the loop knows about the provider attempt being settled.
pub(super) struct AttemptCtx {
    pub(super) event_start: usize,
}

/// The phase and attempt counters a scheduled retry announces.
pub(super) struct RetryNotice {
    pub(super) phase: ProviderPhase,
    pub(super) detail: String,
    pub(super) attempt: u32,
    pub(super) limit: u32,
    pub(super) wait: std::time::Duration,
}

/// Why an unrecovered provider error was not retried automatically.
pub(super) fn terminal_recovery_reason(
    error: &ProviderError,
    recovery: &RecoveryBudget,
    tools_emitted: bool,
) -> &'static str {
    if is_empty_provider_response(error) && recovery.empty_recovery_used {
        "repeated empty provider response"
    } else if tools_emitted {
        "tool effects were emitted; request was not replayed"
    } else if recovery.automatic_recoveries >= MAX_AUTOMATIC_RECOVERIES {
        "global automatic recovery limit reached"
    } else if recovery.provider_recoveries >= MAX_PROVIDER_RECOVERIES {
        "consecutive provider recovery limit reached"
    } else {
        "automatic recovery stopped"
    }
}

impl Runtime {
    /// Applies the recovery policy to one provider attempt. Transport retries
    /// resend the current turn; a correction request is a new model turn.
    /// The recoveries are tried in a fixed order; the first whose guard holds
    /// settles the attempt.
    pub(super) async fn settle_provider_attempt(
        &mut self,
        result: Result<ProviderTurnResult, ProviderError>,
        st: &mut LoopState<'_>,
        attempt: &AttemptCtx,
    ) -> Result<ProviderAttempt, ProviderError> {
        let error = match result {
            Ok(turn) => {
                st.recovery.provider_recoveries = 0;
                st.recovery.empty_recovery_used = false;
                return Ok(ProviderAttempt::Completed(turn));
            }
            Err(error) => error,
        };
        let event_start = attempt.event_start;
        let recovery = &st.recovery;
        if is_empty_provider_response(&error)
            && !recovery.empty_recovery_used
            && recovery.can_retry(recovery.provider_recoveries)
            && !request_emitted_tools(&self.app, event_start)
        {
            return self.settle_empty_response(&error, st);
        }
        if recovery.recovery_output_limit.is_some()
            && recovery.truncation_recoveries < MAX_TRUNCATION_RECOVERIES
            && is_output_limit_rejection(&error)
            && !has_causal_provider_output(&self.app, event_start)
        {
            return self.settle_output_limit_rejection(st);
        }
        if matches!(error, ProviderError::MalformedToolCall)
            && self.pending_argument_repair.is_some()
        {
            return self.settle_argument_repair(st, attempt).await;
        }
        let overflow = is_context_overflow_error(&error)
            && !has_causal_provider_output(&self.app, event_start);
        // Pi compacts for an overflow only when compaction is enabled; with it
        // off the next request would be the rejected one again.
        if overflow
            && !recovery.overflow_retry_used
            && st.config.context_compaction_enabled
            && self.compaction_policy().enabled
            && self.compaction_handle.is_some()
            && self.has_compactable_history(st.messages)
        {
            return self.settle_context_overflow(st);
        }
        let error = if overflow && recovery.overflow_retry_used {
            error.with_suffix(OVERFLOW_RECOVERY_FAILED_SUFFIX)
        } else {
            error
        };
        if recovery.can_retry(recovery.provider_recoveries)
            && recoverable_provider_error(&error)
            && !request_emitted_tools(&self.app, event_start)
        {
            return self.settle_transport_retry(&error, st, attempt).await;
        }
        self.settle_unrecovered(error, st, attempt).await
    }

    fn settle_empty_response(
        &mut self,
        error: &ProviderError,
        st: &mut LoopState<'_>,
    ) -> Result<ProviderAttempt, ProviderError> {
        st.recovery.empty_recovery_used = true;
        st.recovery.automatic_recoveries += 1;
        st.recovery.provider_recoveries += 1;
        self.append_conversation_message(st.messages, ProviderMessage::user(
            "The provider returned an empty response. Produce an effective answer or make the needed tool call; do not return an empty response.",
        ))?;
        self.uncommitted_event_start = None;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ThinkingEnded,
        )?;
        self.announce_retry(
            &mut st.next_seq,
            error,
            RetryNotice {
                phase: ProviderPhase::Connecting,
                detail: format!(
                    "Recovering empty provider response ({automatic_recoveries}/{MAX_AUTOMATIC_RECOVERIES})",
                    automatic_recoveries = st.recovery.automatic_recoveries,
                ),
                attempt: st.recovery.automatic_recoveries,
                limit: MAX_AUTOMATIC_RECOVERIES,
                wait: std::time::Duration::ZERO,
            },
        )?;
        Ok(ProviderAttempt::Retry)
    }

    fn settle_output_limit_rejection(
        &mut self,
        st: &mut LoopState<'_>,
    ) -> Result<ProviderAttempt, ProviderError> {
        // Unknown gateways may accept a smaller ceiling than our
        // fallback. Use the last recovery at the original limit.
        st.recovery.truncation_recoveries = MAX_TRUNCATION_RECOVERIES;
        st.recovery.recovery_output_limit = None;
        st.config.context_reserve_tokens = st.recovery.initial_context_reserve;
        self.uncommitted_event_start = None;
        push_runtime_event(&mut self.app, &mut st.next_seq, crate::EventKind::ProviderPhase {
            phase: ProviderPhase::Connecting,
            elapsed_ms: 0,
            detail: Some(format!("Provider rejected the larger output budget; continuing at the original limit (recovery {MAX_TRUNCATION_RECOVERIES}/{MAX_TRUNCATION_RECOVERIES})")),
        })?;
        Ok(ProviderAttempt::Retry)
    }

    async fn settle_argument_repair(
        &mut self,
        st: &mut LoopState<'_>,
        attempt: &AttemptCtx,
    ) -> Result<ProviderAttempt, ProviderError> {
        let note = self.pending_argument_repair.take().unwrap_or_default();
        let partial = assistant_text_since(&self.app, attempt.event_start);
        if !partial.is_empty() {
            self.append_conversation_message(
                st.messages,
                ProviderMessage::assistant(partial, Vec::new()),
            )?;
        }
        self.append_conversation_message(st.messages, ProviderMessage::user(format!(
            "[Tool argument validation]\nThe entire previous tool batch was rejected before execution. No tools from that batch ran. Correct the arguments as JSON objects before requesting tools again.\n{note}"
        )))?;
        self.uncommitted_event_start = None;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::AssistantEnded {
                reason: "tool_arguments_rejected".into(),
            },
        )?;
        if st.recovery.argument_repairs >= MAX_ARGUMENT_REPAIRS
            || st.turn + 1 >= st.config.max_turns
        {
            return Err(ProviderError::InvalidResponse { message: format!(
                "tool arguments remain invalid after {argument_repairs} repair retries or the configured turn limit; rejected batch was not executed; task remains pending: {note}",
                argument_repairs = st.recovery.argument_repairs,
            ) });
        }
        st.recovery.argument_repairs += 1;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ThinkingEnded,
        )?;
        push_runtime_event(&mut self.app, &mut st.next_seq, crate::EventKind::ProviderPhase {
            phase: ProviderPhase::Connecting, elapsed_ms: 0,
            detail: Some(format!("Repairing tool arguments ({argument_repairs}/{MAX_ARGUMENT_REPAIRS}); rejected batch was not executed", argument_repairs = st.recovery.argument_repairs)),
        })?;
        Ok(ProviderAttempt::NextTurn)
    }

    /// One compact-and-retry per episode: the failed request left no assistant
    /// message in the history, so the next request compacts it and is resent.
    fn settle_context_overflow(
        &mut self,
        st: &mut LoopState<'_>,
    ) -> Result<ProviderAttempt, ProviderError> {
        st.recovery.overflow_retry_used = true;
        st.recovery.overflow_compaction_pending = true;
        st.next_seq = self.observed_next_seq(st.next_seq);
        Ok(ProviderAttempt::Retry)
    }

    async fn settle_transport_retry(
        &mut self,
        error: &ProviderError,
        st: &mut LoopState<'_>,
        attempt: &AttemptCtx,
    ) -> Result<ProviderAttempt, ProviderError> {
        let event_start = attempt.event_start;
        let delay = match provider_recovery_delay(
            error,
            st.recovery.provider_recoveries + 1,
            st.recovery.provider_recovery_wait,
            st.config.provider_recovery_backoff,
        ) {
            Ok(delay) => delay,
            Err(blocked) => {
                if self
                    .wait_for_manual_retry(
                        error,
                        event_start,
                        st.config.provider_recovery_backoff,
                        &mut st.next_seq,
                    )
                    .await?
                {
                    return Ok(ProviderAttempt::Retry);
                }
                return Err(annotate_provider_recovery_error(
                    blocked,
                    "provider",
                    st.recovery.provider_attempts,
                    st.recovery.automatic_recoveries,
                    st.recovery.provider_recoveries,
                    "retry wait budget exhausted",
                ));
            }
        };
        st.recovery.provider_recovery_wait += delay;
        st.recovery.provider_recoveries += 1;
        st.recovery.automatic_recoveries += 1;
        let partial = assistant_text_since(&self.app, event_start);
        if !partial.is_empty() {
            self.append_conversation_message(
                st.messages,
                ProviderMessage::assistant(format!("[Interrupted turn]\n{partial}"), Vec::new()),
            )?;
            self.append_conversation_message(st.messages, ProviderMessage::user(
                "The provider failed while generating the previous response. Continue from the preserved partial response and existing tool results. Do not repeat completed actions or claim that the interrupted response completed the task."
            ))?;
            push_runtime_event(
                &mut self.app,
                &mut st.next_seq,
                crate::EventKind::AssistantEnded {
                    reason: "interrupted".into(),
                },
            )?;
        }
        self.uncommitted_event_start = None;
        push_runtime_event(
            &mut self.app,
            &mut st.next_seq,
            crate::EventKind::ThinkingEnded,
        )?;
        self.announce_retry(
            &mut st.next_seq,
            error,
            RetryNotice {
                phase: ProviderPhase::Connecting,
                detail: format!(
                    "Retrying provider ({provider_recoveries}/{MAX_PROVIDER_RECOVERIES}); waiting {} ms",
                    delay.as_millis(),
                    provider_recoveries = st.recovery.provider_recoveries,
                ),
                attempt: st.recovery.provider_recoveries,
                limit: MAX_PROVIDER_RECOVERIES,
                wait: delay,
            },
        )?;
        self.sleep_or_cancel(delay).await;
        Ok(ProviderAttempt::Retry)
    }

    /// No automatic recovery applies: offers the manual retry, otherwise
    /// fails with the error annotated with why nothing was retried.
    async fn settle_unrecovered(
        &mut self,
        error: ProviderError,
        st: &mut LoopState<'_>,
        attempt: &AttemptCtx,
    ) -> Result<ProviderAttempt, ProviderError> {
        let recovery_reason = terminal_recovery_reason(
            &error,
            &st.recovery,
            request_emitted_tools(&self.app, attempt.event_start),
        );
        if self
            .wait_for_manual_retry(
                &error,
                attempt.event_start,
                st.config.provider_recovery_backoff,
                &mut st.next_seq,
            )
            .await?
        {
            return Ok(ProviderAttempt::Retry);
        }
        Err(
            if recoverable_provider_error(&error) || is_empty_provider_response(&error) {
                annotate_provider_recovery_error(
                    error,
                    "provider",
                    st.recovery.provider_attempts,
                    st.recovery.automatic_recoveries,
                    st.recovery.provider_recoveries,
                    recovery_reason,
                )
            } else {
                error
            },
        )
    }

    /// Announces a scheduled retry: the phase, then the attempt with its
    /// redacted reason.
    pub(super) fn announce_retry(
        &mut self,
        next_seq: &mut u64,
        error: &ProviderError,
        notice: RetryNotice,
    ) -> Result<(), ProviderError> {
        let reason = self.redact_sensitive(&provider_retry_reason(error));
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ProviderPhase {
                phase: notice.phase,
                elapsed_ms: 0,
                detail: Some(notice.detail),
            },
        )?;
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::RetryScheduled {
                attempt: notice.attempt,
                limit: notice.limit,
                wait_ms: duration_millis(notice.wait),
                reason: Some(reason),
            },
        )
    }

    /// Waits out a retry delay, cut short by cancellation; reports whether the
    /// run is cancelled afterwards.
    pub(super) async fn sleep_or_cancel(&self, delay: std::time::Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {},
            _ = CancellationToken::cancelled_or_pending(self.cancellation.clone()) => {},
        }
        self.is_cancelled()
    }
}
