use super::*;

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

    /// Pi's checks of a summarization response: a terminal stop, no tool
    /// call, not cut short, not filtered, and some text.
    pub(super) fn validate(&self) -> Result<(), ProviderError> {
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
                    message: SUMMARY_TRUNCATED.into(),
                });
            }
            ProviderTurnStop::Filtered => {
                return Err(ProviderError::InvalidResponse {
                    message: "summary provider response was filtered".into(),
                });
            }
        }
        if self.text.trim().is_empty() {
            return Err(ProviderError::InvalidResponse {
                message: "summary provider returned an empty summary".into(),
            });
        }
        Ok(())
    }
}

/// The failure of a summary that ran into its output limit.
pub(super) const SUMMARY_TRUNCATED: &str = "summary provider response was truncated (its output cap derives from compaction.reserve_tokens: 80% for the history, 50% for a split turn; raise compaction.reserve_tokens)";

/// Whether `error` is a summary cut short by its output limit.
pub(super) fn is_truncated_summary(error: &ProviderError) -> bool {
    matches!(error, ProviderError::InvalidResponse { message } if message == SUMMARY_TRUNCATED)
}

/// Why this turn compacts before sending its request, and what its context
/// estimate may rely on.
#[derive(Clone, Copy)]
pub(super) struct CompactionTrigger {
    pub(super) reason: CompactionReason,
    /// The turn cannot go on without the compaction: a manual or overflow
    /// request, or a request that does not fit the context gate. Otherwise
    /// the trigger is Pi's soft line, and a failed compaction is reported and
    /// the request is sent as it is.
    pub(super) required: bool,
    pub(super) usage: ContextUsage,
}

pub(super) enum CompactionOutcome {
    /// History was replaced; the request is already prepared from it.
    Applied(Box<PreparedProviderRequest>),
    Skipped,
    Cancelled,
}
