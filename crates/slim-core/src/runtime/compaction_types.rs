use super::*;

pub(super) struct BackgroundCompactionPlan {
    pub(super) selection: CompactionSelection,
    pub(super) request: Option<PreparedProviderRequest>,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) provider_identity: String,
    pub(super) strategy: crate::context::CompactionStrategy,
    pub(super) jev_plan: Option<crate::context::jev_prune::PreparedPrune>,
    pub(super) previous_checkpoint: Option<String>,
    pub(super) context_window_tokens: u64,
    pub(super) reserve_tokens: u64,
    pub(super) serialized_chars: u64,
    pub(super) system_bytes: u64,
    pub(super) history_bytes: u64,
    pub(super) summary_max_bytes: usize,
    pub(super) source_len: usize,
    pub(super) tokens_before: u64,
    pub(super) projected_tokens_after: u64,
    pub(super) request_bytes: u64,
    pub(super) estimated_input_tokens: u64,
    pub(super) projected_savings_tokens: u64,
    pub(super) estimated_cost_tokens: u64,
    pub(super) safety_margin_tokens: u64,
    pub(super) future_turns: u8,
    pub(super) profitable: bool,
}

/// Commit metadata for a local compaction; a prepared summary carries the
/// background attempt's cost, an emergency summary has none.
pub(super) struct LocalCompactionCommit {
    pub(super) prefix_fingerprint: String,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) duration_ms: u64,
    pub(super) tokens_before: u64,
}

pub(super) struct BackgroundCompactionResult {
    pub(super) plan: BackgroundCompactionPlan,
    pub(super) summary: String,
    pub(super) usage: crate::UsageBreakdown,
    pub(super) time_to_first_byte_ms: Option<u64>,
    pub(super) time_to_first_semantic_ms: Option<u64>,
    pub(super) duration_ms: u64,
    pub(super) valid: bool,
    pub(super) usage_known: bool,
    pub(super) cancelled: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CompactionAttemptProgress {
    pub(super) send_started: bool,
    pub(super) headers_received: bool,
    pub(super) first_byte_received: bool,
    pub(super) first_token_received: bool,
    pub(super) time_to_first_byte_ms: Option<u64>,
    pub(super) time_to_first_semantic_ms: Option<u64>,
}

pub(super) struct PendingBackgroundCompaction {
    pub(super) task: tokio::task::JoinHandle<BackgroundCompactionResult>,
    pub(super) cancellation: CancellationToken,
    pub(super) progress: Arc<Mutex<CompactionAttemptProgress>>,
    pub(super) jev_outcome: Arc<Mutex<Option<JevAttemptOutcome>>>,
    pub(super) request_bytes: u64,
    pub(super) estimated_input_tokens: u64,
    pub(super) tokens_before: u64,
    pub(super) started: Instant,
}

pub(super) struct BackgroundCompactionObservers {
    pub(super) progress: Arc<Mutex<CompactionAttemptProgress>>,
    pub(super) jev_outcome: Arc<Mutex<Option<JevAttemptOutcome>>>,
    pub(super) provider_call_journal: Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
}

pub(super) struct JevAttemptOutcome {
    pub(super) error: Option<String>,
    pub(super) stats: crate::context::JevPruneStats,
}

/// Backstop: a background compaction task must never outlive its handle.
/// Normal paths settle via `cancel_pending_background`; this only fires on
/// early `?` returns that would otherwise detach a live HTTP stream.
impl Drop for PendingBackgroundCompaction {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Copy)]
pub(super) struct CompactionUsageEvent {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) input_known: bool,
    pub(super) output_known: bool,
}

#[derive(Default)]
pub(super) struct CompactionSummary {
    pub(super) text: String,
    pub(super) usage: crate::UsageBreakdown,
    pub(super) usage_events: Vec<CompactionUsageEvent>,
    pub(super) breakdown_events: Vec<crate::UsageBreakdown>,
    pub(super) time_to_first_byte_ms: Option<u64>,
    pub(super) time_to_first_semantic_ms: Option<u64>,
    pub(super) breakdown_seen: bool,
    pub(super) stop_reason: Option<String>,
    pub(super) saw_tool_call: bool,
}

impl CompactionSummary {
    /// Terminal/partial counters are only a fallback for a stream without a
    /// provider breakdown.
    fn add_counts(&mut self, input_tokens: u64, output_tokens: u64) {
        if !self.breakdown_seen {
            self.usage.absorb(crate::UsageBreakdown {
                uncached_input_tokens: input_tokens,
                output_tokens,
                ..crate::UsageBreakdown::default()
            });
        }
    }

    pub(super) fn push(&mut self, event: ProviderEvent) {
        match event {
            ProviderEvent::TextDelta(delta) => self.text.push_str(&delta),
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.add_counts(input_tokens, output_tokens);
                self.usage_events.push(CompactionUsageEvent {
                    input_tokens,
                    output_tokens,
                    input_known: true,
                    output_known: true,
                });
            }
            ProviderEvent::UsagePartial {
                input_tokens,
                output_tokens,
                input_complete,
                output_complete,
            } => {
                self.add_counts(input_tokens, output_tokens);
                self.usage_events.push(CompactionUsageEvent {
                    input_tokens,
                    output_tokens,
                    input_known: input_complete,
                    output_known: output_complete,
                });
            }
            ProviderEvent::Stopped { reason } => self.stop_reason = Some(reason),
            ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallInputDelta { .. }
            | ProviderEvent::ToolCallComplete { .. }
            | ProviderEvent::ToolCall { .. } => self.saw_tool_call = true,
            ProviderEvent::UsageBreakdown { usage } => {
                self.breakdown_seen = true;
                self.usage.absorb(usage);
                self.breakdown_events.push(usage);
            }
            ProviderEvent::ResponseCacheHit => {}
            ProviderEvent::Phase {
                phase: ProviderPhase::FirstByte,
                elapsed_ms,
            } => {
                self.time_to_first_byte_ms.get_or_insert(elapsed_ms);
            }
            ProviderEvent::Phase {
                phase: ProviderPhase::FirstSemantic,
                elapsed_ms,
            } => {
                self.time_to_first_semantic_ms.get_or_insert(elapsed_ms);
            }
            ProviderEvent::ReasoningDelta(_)
            | ProviderEvent::ResponsesReasoning(_)
            | ProviderEvent::ChatReasoning(_)
            | ProviderEvent::ReasoningStarted
            | ProviderEvent::ReasoningEnded
            | ProviderEvent::Phase { .. }
            | ProviderEvent::ToolCallProgress { .. }
            | ProviderEvent::ContentBlockStop { .. } => {}
        }
    }

    pub(super) fn usage_known(&self) -> bool {
        let input_known = self.usage_events.iter().any(|event| event.input_known);
        let output_known = self.usage_events.iter().any(|event| event.output_known);
        input_known && output_known && !self.usage.usage_unknown
    }

    pub(super) fn validate(&self, max_bytes: usize) -> Result<(), ProviderError> {
        let raw_reason =
            self.stop_reason
                .as_deref()
                .ok_or_else(|| ProviderError::InvalidResponse {
                    message: "summary provider stream ended without a stop reason".into(),
                })?;
        let normalized = raw_reason.trim().to_ascii_lowercase();
        if self.saw_tool_call
            || matches!(
                normalized.as_str(),
                "tool_calls" | "function_call" | "tool_use"
            )
        {
            return Err(ProviderError::InvalidResponse {
                message: "summary provider attempted to call a tool".into(),
            });
        }
        let stop = classify_provider_stop_reason(raw_reason, &[]).map_err(|error| match error {
            ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
                message: format!("summary provider returned an invalid stop reason: {message}"),
            },
            other => other,
        })?;
        match stop {
            ProviderTurnStop::Normal => {}
            ProviderTurnStop::Truncated => {
                return Err(ProviderError::InvalidResponse {
                    message: "summary provider response was truncated".into(),
                });
            }
            ProviderTurnStop::Filtered => {
                return Err(ProviderError::InvalidResponse {
                    message: "summary provider response was filtered".into(),
                });
            }
        }
        crate::context::validate_checkpoint_content(&self.text, max_bytes).map_err(|message| {
            ProviderError::InvalidResponse {
                message: format!("summary provider returned an invalid checkpoint: {message}"),
            }
        })
    }
}

/// Why this turn compacts before sending its request.
#[derive(Clone, Copy)]
pub(super) struct CompactionTrigger {
    pub(super) preflight_tokens: u64,
    pub(super) manual: bool,
    pub(super) over_hard: bool,
    /// Labels a foreground summary as the recovery of a context overflow.
    pub(super) overflow: bool,
}

pub(super) enum CompactionOutcome {
    /// History was replaced; the request is already prepared from it.
    Applied(Box<PreparedProviderRequest>),
    Skipped,
    Cancelled,
}
