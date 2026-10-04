//! Native port of Pi's default context compaction.
//!
//! Ported from `packages/coding-agent/src/core/compaction/` of Pi, Copyright
//! (c) 2025 Mario Zechner, MIT License (https://github.com/earendil-works/pi):
//! the trigger and token estimate, the cut point (with split turns), iterative
//! summaries, serialization, prompts and file-operation tracking. Everything
//! here is pure over [`ProviderMessage`](crate::provider::ProviderMessage)
//! histories; sending the summary requests and persisting the result belong to
//! the runtime.
//!
//! Where Slim deviates, the module says so: provider messages replace session
//! entries, Slim's `read`/`write`/`patch` tools replace Pi's
//! `read`/`write`/`edit`, a checkpoint's summary is bounded by what the
//! durable session accepts, and the live-history invariants of the tool
//! protocol hold on top of Pi's cut.

mod cut;
mod estimate;
mod file_ops;
mod prompts;
mod serialize;
mod summary;

pub use cut::{
    find_cut_point, find_turn_start_index, find_valid_cut_points, prepare_compaction,
    CompactionPreparation, CutPointResult,
};
pub(crate) use estimate::ESTIMATED_ATTACHMENT_CHARS;
pub use estimate::{
    calculate_context_tokens, estimate_context_tokens, estimate_system_and_tools_tokens,
    estimate_tokens, should_compact, usable_anchor, ContextUsage, ContextUsageEstimate,
    UsageAnchor,
};
pub use file_ops::{
    compute_file_lists, fit_summary_for_persistence, format_file_operations,
    parse_file_operation_blocks, FileLists, FileOperations, FittedSummary,
};
pub use prompts::{
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT, TURN_PREFIX_SUMMARIZATION_PROMPT, UPDATE_SUMMARIZATION_PROMPT,
};
pub use serialize::serialize_conversation;
pub use summary::{
    apply_compaction, build_history_summary_request, build_turn_prefix_summary_request,
    compaction_summary_content, compaction_summary_message, compaction_summary_text,
    merge_split_turn_summary, rewrite_stale_duplicate_pointers, SummaryRequest, SummaryRequests,
    DUPLICATE_POINTER_PREFIX, NO_PRIOR_HISTORY,
};

/// Compaction settings, with Pi's defaults.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Tokens kept free below the context window. It is both the trigger
    /// margin and the budget the summary's output cap derives from.
    pub reserve_tokens: u64,
    /// Tokens of the newest messages kept verbatim.
    pub keep_recent_tokens: u64,
}

pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
};

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_pi() {
        let settings = CompactionSettings::default();
        assert!(settings.enabled);
        assert_eq!(settings.reserve_tokens, 16_384);
        assert_eq!(settings.keep_recent_tokens, 20_000);
        assert_eq!(settings, DEFAULT_COMPACTION_SETTINGS);
    }
}
