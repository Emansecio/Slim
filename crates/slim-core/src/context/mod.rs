mod artifacts;
mod budget;
mod compact;
pub(crate) use compact::recovery_transcript;

pub use artifacts::{ArtifactHandle, ArtifactStore};
pub use budget::ContextBudget;
pub use compact::{
    apply_compaction_selection, build_bounded_summary_prompt,
    build_bounded_summary_prompt_with_checkpoint, build_summary_prompt,
    build_summary_prompt_with_checkpoint, compact, compact_provider_messages,
    compaction_prefix_fingerprint, estimate_provider_message_tokens,
    estimate_text_tokens_from_chars, has_compactable_history,
    latest_user_instruction_before_boundary, local_emergency_summary, select_compaction_history,
    AdaptiveTokenEstimator, CompactionCommit, CompactionHandle, CompactionPolicy, CompactionReason,
    CompactionResult, CompactionSelection, CompactionStatus, ContextItem, PreparedCompaction,
    COMPACTION_SYSTEM_PROMPT,
};
