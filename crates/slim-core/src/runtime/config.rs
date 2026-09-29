use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentLoopConfig {
    /// Run ceiling. Sequential 1-tool/turn work stops here, not on tool caps.
    pub max_turns: usize,
    /// Per-turn mutating native action slots; then_run reserves a second slot.
    pub max_mutating_tool_calls: usize,
    /// Per-turn read batch cap (read/list/search). Not a run total.
    pub max_read_tool_calls: usize,
    /// Cumulative native action slots reserved across the run; then_run reserves a second slot.
    pub max_total_tool_calls: usize,
    pub max_result_bytes: usize,
    pub context_window_tokens: u64,
    pub context_reserve_tokens: u64,
    pub context_compaction_enabled: bool,
    /// First automatic provider-recovery delay; it doubles per attempt (at most 16x).
    /// A provider's retry-after still wins when it is longer.
    pub provider_recovery_backoff: std::time::Duration,
}

impl AgentLoopConfig {
    pub const DEFAULT_MAX_TURNS: usize = 128;
    pub const DEFAULT_MAX_MUTATING_TOOL_CALLS: usize = 32;
    pub const DEFAULT_MAX_READ_TOOL_CALLS: usize = 96;
    pub const DEFAULT_MAX_TOTAL_TOOL_CALLS: usize = 256;
    pub const DEFAULT_PROVIDER_RECOVERY_BACKOFF: std::time::Duration =
        std::time::Duration::from_millis(500);
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_turns: Self::DEFAULT_MAX_TURNS,
            max_mutating_tool_calls: Self::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            max_read_tool_calls: Self::DEFAULT_MAX_READ_TOOL_CALLS,
            max_total_tool_calls: Self::DEFAULT_MAX_TOTAL_TOOL_CALLS,
            max_result_bytes: 16 * 1024,
            context_window_tokens: 32_000,
            context_reserve_tokens: 4_096,
            context_compaction_enabled: true,
            provider_recovery_backoff: Self::DEFAULT_PROVIDER_RECOVERY_BACKOFF,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentLoopStop {
    ProviderCompleted,
    ProviderTruncated,
    ProviderFiltered,
    TurnLimit,
    ToolLimit,
    RepeatedFailedTool,
    /// The causal ledger asked to stop (repeated evidence / stagnant turns):
    /// continuing would only burn turns without progress.
    NoProgress,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLoopResult {
    pub next_seq: u64,
    pub turns: usize,
    pub stop: AgentLoopStop,
    pub tool_results: Vec<ToolResult>,
    pub usage: UsageTotals,
}

pub(super) struct ProviderTurnResult {
    pub(super) next_seq: u64,
    pub(super) blocks_tools: bool,
    pub(super) stop: ProviderTurnStop,
    pub(super) responses_reasoning: Vec<crate::provider::ResponsesReasoning>,
    pub(super) chat_reasoning: Option<crate::provider::ChatReasoning>,
}

impl ProviderTurnResult {
    /// The assistant message for this turn; moves the reasoning state into it.
    pub(super) fn take_assistant_message(
        &mut self,
        text: String,
        calls: Vec<ProviderToolCall>,
    ) -> ProviderMessage {
        let mut assistant = ProviderMessage::assistant(text, calls);
        assistant.responses_reasoning = std::mem::take(&mut self.responses_reasoning);
        assistant.chat_reasoning = self.chat_reasoning.take();
        assistant
    }
}
