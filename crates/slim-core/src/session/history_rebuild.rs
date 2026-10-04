//! The provider-visible history of a durable session: its entries with the
//! compaction checkpoints applied.
//!
//! Resume and branch compaction share this rebuild, so a checkpoint is applied
//! the same way wherever the history is needed. A checkpoint is applied when it
//! chains onto the previous applied one, its anchor entry exists and is not a
//! tool result, its summary is within bounds, and the fingerprint of the
//! history before the anchor matches. One that fails any check is skipped, and
//! the entries it would have replaced stay in the history; each skip is
//! reported so it is not silent.

use super::{CompactionCheckpoint, DurableRecord, MAX_COMPACTION_SUMMARY_BYTES};
use crate::context::{
    compaction_prefix_fingerprint, compaction_summary_message, legacy_prefix_fingerprint,
    rewrite_stale_duplicate_pointers,
};
use crate::provider::ProviderMessage;

/// A checkpoint the rebuild did not apply, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedCheckpoint {
    pub checkpoint_id: String,
    pub reason: &'static str,
}

/// The rebuilt history of a durable session.
#[derive(Clone, Debug)]
pub struct RebuiltHistory {
    /// What the model sees: the latest applied checkpoint's summary message
    /// followed by the entries from its anchor on, or every entry when no
    /// checkpoint applies.
    pub messages: Vec<ProviderMessage>,
    /// The last entry of the session.
    pub parent_entry_id: Option<String>,
    /// The entry behind each message; `None` for a summary message.
    pub entry_ids: Vec<Option<String>>,
    pub applied_checkpoint_id: Option<String>,
    pub skipped_checkpoints: Vec<SkippedCheckpoint>,
}

/// The history the writer before the Pi port kept after a checkpoint, which
/// its later checkpoints were fingerprinted over: the first user message, a
/// `[Compacted context]` message, optionally the latest later user instruction,
/// then the entries from the anchor on.
struct LegacyView {
    history: Vec<ProviderMessage>,
    ids: Vec<Option<String>>,
}

/// Rebuilds the provider history of `records`.
pub fn rebuild_provider_history(records: &[DurableRecord]) -> Result<RebuiltHistory, &'static str> {
    let raw = super::provider_messages_from_records(records.iter())?;
    let raw_ids: Vec<Option<String>> = records
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Entry { entry, .. } => Some(Some(entry.entry_id.clone())),
            _ => None,
        })
        .collect();
    let parent_entry_id = raw_ids.last().cloned().flatten();
    let mut history = raw;
    let mut entry_ids = raw_ids;
    let mut applied: Option<&CompactionCheckpoint> = None;
    // Candidates for the old writer's view after the applied checkpoints,
    // kept only while the checkpoints so far could be the old writer's.
    let mut legacy: Vec<LegacyView> = Vec::new();
    let mut skipped = Vec::new();
    for checkpoint in records.iter().filter_map(|record| match record {
        DurableRecord::Compaction { checkpoint, .. } => Some(checkpoint),
        _ => None,
    }) {
        let mut skip = |reason| {
            skipped.push(SkippedCheckpoint {
                checkpoint_id: checkpoint.checkpoint_id.clone(),
                reason,
            });
        };
        if checkpoint.previous_checkpoint_id.as_deref()
            != applied.map(|previous| previous.checkpoint_id.as_str())
        {
            skip("it does not follow the previous applied checkpoint");
            continue;
        }
        let Some(anchor_index) = entry_ids
            .iter()
            .position(|id| id.as_deref() == Some(checkpoint.first_kept_entry_id.as_str()))
        else {
            skip("its first kept entry is missing");
            continue;
        };
        if history[anchor_index].role == "tool"
            || checkpoint.summary.trim().is_empty()
            || checkpoint.summary.len() > MAX_COMPACTION_SUMMARY_BYTES
        {
            skip("its anchor or summary is invalid");
            continue;
        }
        let in_pi_layout = prefix_fingerprint_matches(&history[..anchor_index], checkpoint);
        // The first checkpoint of a session fingerprints the plain entries,
        // which both writers share; a later one the old writer made
        // fingerprints its own view.
        let legacy_source = if in_pi_layout {
            None
        } else {
            let found = legacy
                .iter()
                .position(|view| legacy_prefix_matches(view, checkpoint));
            let Some(found) = found else {
                skip("the entries before its anchor changed");
                continue;
            };
            Some(legacy.swap_remove(found))
        };
        let next_legacy = match (&legacy_source, applied.is_none()) {
            (Some(view), _) => legacy_views_after(&view.history, &view.ids, checkpoint),
            (None, true) => legacy_views_after(&history, &entry_ids, checkpoint),
            (None, false) => Vec::new(),
        };
        let mut restored = vec![compaction_summary_message(&checkpoint.summary)];
        let mut restored_ids = vec![None];
        restored.extend(history[anchor_index..].iter().cloned());
        rewrite_stale_duplicate_pointers(&mut restored[1..]);
        restored_ids.extend(entry_ids[anchor_index..].iter().cloned());
        history = restored;
        entry_ids = restored_ids;
        legacy = next_legacy;
        applied = Some(checkpoint);
    }
    Ok(RebuiltHistory {
        messages: history,
        parent_entry_id,
        entry_ids,
        applied_checkpoint_id: applied.map(|last| last.checkpoint_id.clone()),
        skipped_checkpoints: skipped,
    })
}

/// Whether the fingerprint of `prefix` is the one `checkpoint` stored: the
/// current form, or the raw-byte form the writer before the Pi port stored.
fn prefix_fingerprint_matches(
    prefix: &[ProviderMessage],
    checkpoint: &CompactionCheckpoint,
) -> bool {
    compaction_prefix_fingerprint(prefix) == checkpoint.prefix_fingerprint
        || legacy_prefix_fingerprint(prefix) == checkpoint.prefix_fingerprint
}

/// Whether `checkpoint` was fingerprinted over this old-writer view.
fn legacy_prefix_matches(view: &LegacyView, checkpoint: &CompactionCheckpoint) -> bool {
    view.ids
        .iter()
        .position(|id| id.as_deref() == Some(checkpoint.first_kept_entry_id.as_str()))
        .is_some_and(|anchor| {
            view.history[anchor].role != "tool"
                && prefix_fingerprint_matches(&view.history[..anchor], checkpoint)
        })
}

/// The old writer's views after applying `checkpoint` to `history`. It pinned
/// the latest user instruction that lay before the anchor when no user message
/// followed it in the history it then held, which a later replay cannot tell
/// apart from the other case, so both views are candidates.
fn legacy_views_after(
    history: &[ProviderMessage],
    ids: &[Option<String>],
    checkpoint: &CompactionCheckpoint,
) -> Vec<LegacyView> {
    let Some(anchor) = ids
        .iter()
        .position(|id| id.as_deref() == Some(checkpoint.first_kept_entry_id.as_str()))
    else {
        return Vec::new();
    };
    let Some(root_index) = history.iter().position(|message| message.role == "user") else {
        return Vec::new();
    };
    let head = || {
        (
            vec![
                history[root_index].clone(),
                ProviderMessage::user(format!(
                    "[Compacted context]\n{}",
                    checkpoint.summary.trim()
                )),
            ],
            vec![ids[root_index].clone(), None],
        )
    };
    let view = |pinned: Option<usize>| {
        let (mut messages, mut view_ids) = head();
        if let Some(index) = pinned {
            messages.push(history[index].clone());
            view_ids.push(ids[index].clone());
        }
        messages.extend(history[anchor..].iter().cloned());
        view_ids.extend(ids[anchor..].iter().cloned());
        LegacyView {
            history: messages,
            ids: view_ids,
        }
    };
    let pinned = (root_index + 1..anchor).rev().find(|&index| {
        history[index].role == "user"
            && !history[index].content.starts_with("[Compacted context]\n")
    });
    let mut views = vec![view(None)];
    if pinned.is_some() {
        views.push(view(pinned));
    }
    views
}
