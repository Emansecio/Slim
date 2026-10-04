//! Cut-point selection and compaction preparation.
//!
//! Port of Pi's `findValidCutPoints`, `findTurnStartIndex`, `findCutPoint` and
//! `prepareCompaction` (`packages/coding-agent/src/core/compaction/
//! compaction.ts`), Copyright (c) 2025 Mario Zechner, MIT License
//! (https://github.com/earendil-works/pi).
//!
//! Pi walks session entries; Slim walks provider messages, one message per
//! entry. Pi's metadata entries and projection edits have no counterpart.
//! Slim protocol invariants hold on top of Pi's algorithm:
//! - a tool result is never separated from the assistant call it answers
//!   ([`keep_tool_pairs_together`] pulls the cut back to the call);
//! - a live reasoning/tool continuation at the end of the history is never
//!   summarized, because the provider needs its opaque state
//!   ([`live_continuation_start`] clamps the cut back to the assistant call).
//!   Pi's rule alone is not enough: the runtime appends user messages (steers,
//!   notes) after a tool batch, and those are valid cut points, so a large
//!   batch could put the cut after its own call;
//! - system and developer messages are not conversation: they are never
//!   summarized (see [`apply_compaction`](super::apply_compaction) for how they
//!   are kept).

use super::estimate::{estimate_context_tokens, estimate_tokens, ContextUsage};
use super::file_ops::{parse_file_operation_blocks, FileOperations};
use super::summary::last_compaction_summary_index;
use super::{compaction_summary_text, CompactionSettings};
use crate::provider::ProviderMessage;
use std::collections::HashMap;

/// Result of [`find_cut_point`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CutPointResult {
    /// Index of the first message to keep.
    pub first_kept_index: usize,
    /// Index of the user message that starts the turn being split, or `None`
    /// when the cut does not split a turn.
    pub turn_start_index: Option<usize>,
    /// Whether the cut falls in the middle of a turn (the first kept message
    /// does not start one).
    pub is_split_turn: bool,
}

/// User and assistant messages are valid cut points. A tool result never is:
/// it must follow its call, so a cut at an assistant message keeps the results
/// that come after it.
fn is_cut_point(message: &ProviderMessage) -> bool {
    matches!(message.role.as_str(), "user" | "assistant")
}

/// A user message starts a turn.
fn is_turn_start(message: &ProviderMessage) -> bool {
    message.role == "user"
}

fn is_conversation(message: &ProviderMessage) -> bool {
    !matches!(message.role.as_str(), "system" | "developer")
}

/// Indices in `start..end` where a cut may fall.
pub fn find_valid_cut_points(messages: &[ProviderMessage], start: usize, end: usize) -> Vec<usize> {
    let end = end.min(messages.len());
    (start..end)
        .filter(|&index| is_cut_point(&messages[index]))
        .collect()
}

/// The user message that starts the turn containing `index`, searching back
/// to `start`.
pub fn find_turn_start_index(
    messages: &[ProviderMessage],
    index: usize,
    start: usize,
) -> Option<usize> {
    (start..=index.min(messages.len().saturating_sub(1)))
        .rev()
        .find(|&candidate| is_turn_start(&messages[candidate]))
}

/// Pi's choice of cut index: walk back from the newest message accumulating
/// estimated tokens; once `keep_recent_tokens` is reached, cut at the first
/// valid cut point at or after that message (the last one when none follows).
/// Without reaching the budget the first cut point is used, keeping
/// everything. `cut_points` must not be empty.
fn choose_cut_index(
    messages: &[ProviderMessage],
    start: usize,
    end: usize,
    keep_recent_tokens: u64,
    cut_points: &[usize],
) -> usize {
    let mut accumulated = 0u64;
    for index in (start..end).rev() {
        let tokens = estimate_tokens(&messages[index]);
        if tokens == 0 {
            continue;
        }
        accumulated = accumulated.saturating_add(tokens);
        if accumulated >= keep_recent_tokens {
            let at_or_after = cut_points.partition_point(|&candidate| candidate < index);
            return cut_points
                .get(at_or_after)
                .or_else(|| cut_points.last())
                .copied()
                .unwrap_or(cut_points[0]);
        }
    }
    cut_points[0]
}

/// Whether the cut at `cut_index` starts a turn, and otherwise which turn it
/// splits.
fn resolve_cut(messages: &[ProviderMessage], start: usize, cut_index: usize) -> CutPointResult {
    let starts_turn = is_turn_start(&messages[cut_index]);
    let turn_start_index = if starts_turn {
        None
    } else {
        find_turn_start_index(messages, cut_index, start)
    };
    CutPointResult {
        first_kept_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

/// Finds the cut that keeps about `keep_recent_tokens` of the newest messages
/// in `start..end`, exactly as Pi does. With no valid cut point the result is
/// `start` (nothing to cut).
pub fn find_cut_point(
    messages: &[ProviderMessage],
    start: usize,
    end: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let end = end.min(messages.len());
    let cut_points = find_valid_cut_points(messages, start, end);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_index: start,
            turn_start_index: None,
            is_split_turn: false,
        };
    }
    let cut_index = choose_cut_index(messages, start, end, keep_recent_tokens, &cut_points);
    resolve_cut(messages, start, cut_index)
}

/// Index of the assistant message that started the trailing live reasoning
/// continuation: the last assistant message in `start..end`, when it carries
/// tool calls and opaque reasoning, which a text summary cannot rebuild. Its
/// results and the user messages the runtime appended after them (steers,
/// notes) belong to the same continuation. `None` when the history ends in a
/// finished assistant answer or the trailing tool batch has no opaque state.
fn live_continuation_start(
    messages: &[ProviderMessage],
    start: usize,
    end: usize,
) -> Option<usize> {
    (start..end)
        .rev()
        .find(|&index| messages[index].role == "assistant")
        .filter(|&index| !messages[index].tool_calls.is_empty())
        .filter(|&index| {
            !messages[index].responses_reasoning.is_empty()
                || messages[index].chat_reasoning.is_some()
        })
}

/// Moves `cut_index` back to the assistant message of any kept tool result
/// whose call would otherwise be summarized.
fn keep_tool_pairs_together(
    messages: &[ProviderMessage],
    start: usize,
    end: usize,
    mut cut_index: usize,
) -> usize {
    let mut call_index: HashMap<&str, usize> = HashMap::new();
    for (index, message) in messages.iter().enumerate().take(end).skip(start) {
        if message.role == "assistant" {
            for call in &message.tool_calls {
                call_index.insert(call.id.as_str(), index);
            }
        }
    }
    loop {
        let earliest_call = messages[cut_index..end]
            .iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| message.tool_call_id.as_deref())
            .filter_map(|id| call_index.get(id).copied())
            .filter(|&index| index < cut_index)
            .min();
        match earliest_call {
            Some(index) => cut_index = index,
            None => return cut_index,
        }
    }
}

/// Everything needed to produce a compaction: what to summarize, what to keep
/// and the state iterative summarization starts from.
#[derive(Clone, Debug)]
pub struct CompactionPreparation {
    /// Index (in the messages passed to [`prepare_compaction`]) of the first
    /// message to keep.
    pub first_kept_index: usize,
    /// Conversation messages summarized by the history call.
    pub messages_to_summarize: Vec<ProviderMessage>,
    /// Messages of the split turn's prefix, summarized by the second call.
    pub turn_prefix_messages: Vec<ProviderMessage>,
    /// Whether the cut falls in the middle of a turn.
    pub is_split_turn: bool,
    pub tokens_before: u64,
    /// Summary of the previous compaction, for the update prompt.
    pub previous_summary: Option<String>,
    /// File operations of the previous compaction and of the summarized
    /// messages.
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

/// Plans a compaction of `messages` (the live history, including the summary
/// message of a previous compaction, if any).
///
/// The summarized span starts after the previous summary message, so messages
/// the previous compaction kept are summarized together with newer ones.
/// Returns `None` when there is nothing to compact: the history ends with a
/// summary, no message falls outside the recent window, or no cut point
/// exists.
pub fn prepare_compaction(
    messages: &[ProviderMessage],
    settings: &CompactionSettings,
    usage: ContextUsage,
) -> Option<CompactionPreparation> {
    let end = messages.len();
    let summary_index = last_compaction_summary_index(messages);
    if summary_index == end.checked_sub(1) && summary_index.is_some() {
        return None;
    }
    let boundary_start = summary_index.map_or(0, |index| index + 1);
    let previous_summary = summary_index
        .and_then(|index| compaction_summary_text(&messages[index]))
        .map(str::to_owned);

    let tokens_before = estimate_context_tokens(messages, usage).tokens;
    let cut_points = find_valid_cut_points(messages, boundary_start, end);
    if cut_points.is_empty() {
        return None;
    }

    let mut cut_index = choose_cut_index(
        messages,
        boundary_start,
        end,
        settings.keep_recent_tokens,
        &cut_points,
    );
    if let Some(continuation) = live_continuation_start(messages, boundary_start, end) {
        cut_index = cut_index.min(continuation);
    }
    cut_index = keep_tool_pairs_together(messages, boundary_start, end, cut_index);
    let cut = resolve_cut(messages, boundary_start, cut_index);

    let history_end = match (cut.is_split_turn, cut.turn_start_index) {
        (true, Some(turn_start)) => turn_start,
        _ => cut.first_kept_index,
    };
    let conversation = |range: std::ops::Range<usize>| -> Vec<ProviderMessage> {
        messages[range]
            .iter()
            .filter(|message| is_conversation(message))
            .cloned()
            .collect()
    };
    let messages_to_summarize = conversation(boundary_start..history_end);
    let turn_prefix_messages = match cut.turn_start_index {
        Some(turn_start) if cut.is_split_turn => conversation(turn_start..cut.first_kept_index),
        _ => Vec::new(),
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let mut file_ops = FileOperations::new();
    if let Some(summary) = &previous_summary {
        file_ops.add_previous(&parse_file_operation_blocks(summary));
    }
    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        file_ops.extract_from_message(message);
    }

    Some(CompactionPreparation {
        first_kept_index: cut.first_kept_index,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: settings.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::estimate::UsageAnchor;
    use super::super::file_ops::{compute_file_lists, FileLists};
    use super::super::{compaction_summary_message, DEFAULT_COMPACTION_SETTINGS};
    use super::*;
    use crate::provider::ProviderToolCall;

    fn user(text: &str) -> ProviderMessage {
        ProviderMessage::user(text)
    }

    fn assistant(text: &str) -> ProviderMessage {
        ProviderMessage::assistant(text, Vec::new())
    }

    fn call(id: &str, name: &str, arguments: &str) -> ProviderToolCall {
        ProviderToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn settings(keep_recent_tokens: u64) -> CompactionSettings {
        CompactionSettings {
            keep_recent_tokens,
            ..DEFAULT_COMPACTION_SETTINGS
        }
    }

    fn text_of(messages: &[ProviderMessage]) -> String {
        messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Pi: compaction.test.ts "findCutPoint"
    #[test]
    fn cut_point_lands_on_a_user_or_assistant_message() {
        let mut messages = Vec::new();
        for index in 0..10 {
            messages.push(user(&format!("User {index}")));
            messages.push(assistant(&format!("Assistant {index}")));
        }
        let result = find_cut_point(&messages, 0, messages.len(), 4);
        assert!(matches!(
            messages[result.first_kept_index].role.as_str(),
            "user" | "assistant"
        ));
        assert!(result.first_kept_index > 0);
    }

    #[test]
    fn no_cut_points_means_start() {
        let messages = [ProviderMessage::tool("read", "c1", "orphan result")];
        let result = find_cut_point(&messages, 0, messages.len(), 1000);
        assert_eq!(
            result,
            CutPointResult {
                first_kept_index: 0,
                turn_start_index: None,
                is_split_turn: false
            }
        );
    }

    #[test]
    fn a_lone_assistant_is_its_own_cut_point() {
        let messages = [assistant("a")];
        assert_eq!(find_cut_point(&messages, 0, 1, 1000).first_kept_index, 0);
    }

    #[test]
    fn keeps_everything_when_all_messages_fit_the_budget() {
        let messages = [user("1"), assistant("a"), user("2"), assistant("b")];
        let result = find_cut_point(&messages, 0, messages.len(), 50_000);
        assert_eq!(result.first_kept_index, 0);
        assert!(!result.is_split_turn);
    }

    #[test]
    fn cutting_at_an_assistant_splits_the_turn() {
        let messages = [
            user("Turn 1"),
            assistant("A1"),
            user("Turn 2"),
            assistant("A2-1"),
            assistant("A2-2"),
            assistant("A2-3"),
        ];
        // A2-3 (1) + A2-2 (2) + A2-1 (3) reaches 3 tokens at index 3.
        let result = find_cut_point(&messages, 0, messages.len(), 3);
        assert_eq!(
            result,
            CutPointResult {
                first_kept_index: 3,
                turn_start_index: Some(2),
                is_split_turn: true
            }
        );
    }

    #[test]
    fn a_large_message_is_budgeted_whole() {
        let messages = [
            user("hi"),
            assistant("hello"),
            user(&"x".repeat(4000)),
            assistant("ok"),
        ];

        let tiny = find_cut_point(&messages, 0, messages.len(), 1);
        assert_eq!(tiny.first_kept_index, 3);
        assert!(tiny.is_split_turn);
        assert_eq!(tiny.turn_start_index, Some(2));

        let fits = find_cut_point(&messages, 0, messages.len(), 2);
        assert_eq!(fits.first_kept_index, 2);
        assert!(!fits.is_split_turn);
        assert_eq!(fits.turn_start_index, None);
    }

    // Pi regression test for #9740.
    #[test]
    fn falls_back_to_the_latest_cut_point_before_oversized_trailing_tool_results() {
        let messages = [
            user("old history"),
            assistant("old answer"),
            user("read the large file"),
            ProviderMessage::assistant("", vec![call("call-1", "read", r#"{"path":"big.txt"}"#)]),
            ProviderMessage::tool("read", "call-1", "x".repeat(8000)),
        ];

        let result = find_cut_point(&messages, 0, messages.len(), 1000);
        assert_eq!(
            result,
            CutPointResult {
                first_kept_index: 3,
                turn_start_index: Some(2),
                is_split_turn: true
            }
        );

        let preparation = prepare_compaction(&messages, &settings(1000), ContextUsage::default())
            .expect("preparation");
        assert_eq!(preparation.first_kept_index, 3);
        assert_eq!(preparation.messages_to_summarize, messages[..2].to_vec());
        assert_eq!(preparation.turn_prefix_messages, vec![messages[2].clone()]);
        assert!(preparation.is_split_turn);
    }

    #[test]
    fn turn_start_search_stops_at_the_start_index() {
        let messages = [user("a"), assistant("b"), assistant("c")];
        assert_eq!(find_turn_start_index(&messages, 2, 0), Some(0));
        assert_eq!(find_turn_start_index(&messages, 2, 1), None);
    }

    // Pi: compaction.test.ts "prepareCompaction"
    #[test]
    fn system_messages_are_not_conversation_history() {
        let mut system = user("current prompt");
        system.role = "system".into();
        let messages = [system, user("one long turn"), assistant("assistant suffix")];
        let preparation = prepare_compaction(&messages, &settings(1), ContextUsage::default())
            .expect("preparation");

        assert_eq!(preparation.first_kept_index, 2);
        assert!(preparation.is_split_turn);
        assert!(preparation.messages_to_summarize.is_empty());
        assert_eq!(preparation.turn_prefix_messages, vec![messages[1].clone()]);
    }

    #[test]
    fn nothing_to_compact_when_everything_is_recent() {
        let messages = [user("1"), assistant("a")];
        assert!(prepare_compaction(
            &messages,
            &DEFAULT_COMPACTION_SETTINGS,
            ContextUsage::default()
        )
        .is_none());
        assert!(
            prepare_compaction(&[], &DEFAULT_COMPACTION_SETTINGS, ContextUsage::default())
                .is_none()
        );
    }

    #[test]
    fn nothing_to_compact_right_after_a_compaction() {
        let messages = [user("old"), compaction_summary_message("First summary")];
        assert!(prepare_compaction(&messages, &settings(1), ContextUsage::default()).is_none());
    }

    // Pi: compaction.test.ts "prepareCompaction with previous compaction"
    fn after_first_compaction() -> Vec<ProviderMessage> {
        // Live history after a compaction that kept everything from "user msg 2".
        vec![
            compaction_summary_message("First summary"),
            user(&"user msg 2 - kept by compaction1 ".repeat(12)),
            assistant(&"assistant msg 2 ".repeat(12)),
            user(&"user msg 3 - kept by compaction1 ".repeat(12)),
            assistant(&"assistant msg 3 ".repeat(12)),
            user(&"user msg 4 (new after compaction1) ".repeat(12)),
            assistant(&"assistant msg 4 ".repeat(12)),
        ]
    }

    #[test]
    fn skips_repeated_compactions_when_kept_messages_still_fit() {
        let messages = after_first_compaction();
        assert!(prepare_compaction(
            &messages,
            &DEFAULT_COMPACTION_SETTINGS,
            ContextUsage::default()
        )
        .is_none());
    }

    #[test]
    fn resummarizes_previously_kept_messages_when_the_window_moves_past_them() {
        let messages = after_first_compaction();
        let preparation = prepare_compaction(&messages, &settings(100), ContextUsage::default())
            .expect("preparation");

        let summarized = text_of(&preparation.messages_to_summarize);
        assert!(summarized.contains("user msg 2 - kept by compaction1"));
        assert!(summarized.contains("user msg 3 - kept by compaction1"));
        assert!(!summarized.contains("First summary"));
        assert_eq!(
            preparation.previous_summary.as_deref(),
            Some("First summary")
        );
        assert_eq!(preparation.first_kept_index, 5);
        assert!(!preparation.is_split_turn);
    }

    #[test]
    fn tokens_before_uses_the_usage_anchor_and_ignores_a_stale_one() {
        let messages = after_first_compaction();
        let anchored = prepare_compaction(
            &messages,
            &settings(100),
            ContextUsage {
                anchor: Some(UsageAnchor {
                    message_index: 4,
                    context_tokens: 7_000,
                }),
                fixed_tokens: 0,
            },
        )
        .unwrap();
        assert_eq!(
            anchored.tokens_before,
            7_000 + estimate_tokens(&messages[5]) + estimate_tokens(&messages[6])
        );

        let stale = prepare_compaction(
            &messages,
            &settings(100),
            ContextUsage {
                anchor: Some(UsageAnchor {
                    message_index: 0,
                    context_tokens: 7_000,
                }),
                fixed_tokens: 0,
            },
        )
        .unwrap();
        assert_eq!(
            stale.tokens_before,
            messages.iter().map(estimate_tokens).sum::<u64>()
        );
    }

    #[test]
    fn file_operations_cover_the_summarized_span_the_prefix_and_the_previous_summary() {
        let previous = format!(
            "First summary{}",
            super::super::format_file_operations(
                &["prev-read.rs".to_owned(), "kept.rs".to_owned()],
                &["prev-mod.rs".to_owned()]
            )
        );
        let messages = vec![
            compaction_summary_message(&previous),
            user("task"),
            ProviderMessage::assistant(
                "",
                vec![
                    call("c1", "read", r#"{"path":"a.rs"}"#),
                    call("c2", "patch", r#"{"path":"kept.rs","edits":[]}"#),
                ],
            ),
            ProviderMessage::tool("read", "c1", "x".repeat(40)),
            ProviderMessage::tool("patch", "c2", "ok"),
            ProviderMessage::assistant("", vec![call("c3", "write", r#"{"path":"p.rs"}"#)]),
            ProviderMessage::tool("write", "c3", "ok"),
            ProviderMessage::assistant("", vec![call("c4", "read", r#"{"path":"kept-tail.rs"}"#)]),
            ProviderMessage::tool("read", "c4", "tail"),
        ];
        // Budget so that the last assistant call (and its result) is kept.
        let preparation = prepare_compaction(&messages, &settings(8), ContextUsage::default())
            .expect("preparation");
        assert_eq!(preparation.first_kept_index, 7);
        assert!(preparation.is_split_turn);
        assert_eq!(
            compute_file_lists(&preparation.file_ops),
            FileLists {
                read_files: vec!["a.rs".into(), "prev-read.rs".into()],
                modified_files: vec!["kept.rs".into(), "p.rs".into(), "prev-mod.rs".into()],
            }
        );
    }

    #[test]
    fn a_call_in_the_kept_tail_does_not_count() {
        let messages = [
            user("task"),
            ProviderMessage::assistant("", vec![call("c1", "read", r#"{"path":"old.rs"}"#)]),
            ProviderMessage::tool("read", "c1", "x".repeat(400)),
            ProviderMessage::assistant("", vec![call("c2", "read", r#"{"path":"new.rs"}"#)]),
            ProviderMessage::tool("read", "c2", "tail"),
        ];
        let preparation = prepare_compaction(&messages, &settings(4), ContextUsage::default())
            .expect("preparation");
        assert_eq!(preparation.first_kept_index, 3);
        assert_eq!(
            compute_file_lists(&preparation.file_ops).read_files,
            vec!["old.rs".to_owned()]
        );
    }

    // Slim protocol invariants.
    fn reasoning_assistant(calls: Vec<ProviderToolCall>) -> ProviderMessage {
        let mut message = ProviderMessage::assistant("", calls);
        message.chat_reasoning = Some(crate::provider::ChatReasoning {
            scope_id: 1,
            model: "m".into(),
            content: "opaque".into(),
            details: Vec::new(),
        });
        message
    }

    #[test]
    fn a_live_reasoning_continuation_is_never_summarized() {
        let history = |results: Vec<ProviderMessage>| {
            let mut messages = vec![
                user("old request"),
                assistant("old answer"),
                user("current request"),
                reasoning_assistant(vec![
                    call("c1", "read", r#"{"path":"a.rs"}"#),
                    call("c2", "read", r#"{"path":"b.rs"}"#),
                ]),
            ];
            messages.extend(results);
            messages
        };
        let one_large = history(vec![
            ProviderMessage::tool("read", "c1", "ok"),
            ProviderMessage::tool("read", "c2", "x".repeat(8000)),
        ]);
        let all_small = history(vec![
            ProviderMessage::tool("read", "c1", "ok"),
            ProviderMessage::tool("read", "c2", "ok"),
        ]);
        for messages in [one_large, all_small] {
            for keep in [1u64, 5, 50, 5_000] {
                let Some(preparation) =
                    prepare_compaction(&messages, &settings(keep), ContextUsage::default())
                else {
                    continue;
                };
                assert!(
                    preparation.first_kept_index <= 3,
                    "keep={keep} cut at {}",
                    preparation.first_kept_index
                );
                assert!(preparation
                    .messages_to_summarize
                    .iter()
                    .chain(&preparation.turn_prefix_messages)
                    .all(|message| message.chat_reasoning.is_none()));
            }
        }
        let messages = history(vec![
            ProviderMessage::tool("read", "c1", "ok"),
            ProviderMessage::tool("read", "c2", "x".repeat(8000)),
        ]);
        let preparation = prepare_compaction(&messages, &settings(1), ContextUsage::default())
            .expect("preparation");
        assert_eq!(preparation.first_kept_index, 3);
        assert_eq!(preparation.turn_prefix_messages, vec![messages[2].clone()]);
    }

    #[test]
    fn a_steer_after_a_large_tool_batch_does_not_pull_the_cut_past_its_call() {
        // The runtime appends user messages (steers, notes) after a batch;
        // they are valid cut points, and results above `keep_recent_tokens`
        // would put the cut on them, summarizing the live call.
        let messages = [
            user("old request"),
            assistant("old answer"),
            user("current request"),
            reasoning_assistant(vec![
                call("c1", "read", r#"{"path":"a.rs"}"#),
                call("c2", "read", r#"{"path":"b.rs"}"#),
            ]),
            ProviderMessage::tool("read", "c1", "x".repeat(8000)),
            ProviderMessage::tool("read", "c2", "y".repeat(8000)),
            user("TODO_PROGRESS_REVIEW"),
        ];
        for keep in [1u64, 100, 1_000, 3_000] {
            let Some(preparation) =
                prepare_compaction(&messages, &settings(keep), ContextUsage::default())
            else {
                continue;
            };
            assert!(
                preparation.first_kept_index <= 3,
                "keep={keep} cut at {}",
                preparation.first_kept_index
            );
            assert!(preparation
                .messages_to_summarize
                .iter()
                .chain(&preparation.turn_prefix_messages)
                .all(|message| message.tool_calls.is_empty() && message.role != "tool"));
        }
        let preparation = prepare_compaction(&messages, &settings(1), ContextUsage::default())
            .expect("preparation");
        assert_eq!(preparation.first_kept_index, 3);
    }

    #[test]
    fn a_trailing_batch_without_reasoning_is_summarized_once_a_user_message_follows() {
        // No opaque reasoning needs the call: like Pi, the cut may land on
        // the user message after the batch and summarize the call and results.
        let messages = [
            user("old request"),
            assistant("old answer"),
            user("current request"),
            ProviderMessage::assistant(
                "",
                vec![
                    call("c1", "read", r#"{"path":"a.rs"}"#),
                    call("c2", "read", r#"{"path":"b.rs"}"#),
                ],
            ),
            ProviderMessage::tool("read", "c1", "x".repeat(8000)),
            ProviderMessage::tool("read", "c2", "y".repeat(8000)),
            user("continue"),
        ];
        let preparation = prepare_compaction(&messages, &settings(1), ContextUsage::default())
            .expect("preparation");
        assert_eq!(preparation.first_kept_index, 6);
        let summarized = preparation
            .messages_to_summarize
            .iter()
            .chain(&preparation.turn_prefix_messages)
            .collect::<Vec<_>>();
        assert!(summarized
            .iter()
            .any(|message| !message.tool_calls.is_empty()));
        assert_eq!(
            summarized
                .iter()
                .filter(|message| message.role == "tool")
                .count(),
            2
        );
    }

    #[test]
    fn a_tool_result_stays_with_its_call() {
        // The result is detached from its call by an unrelated user message;
        // cutting after the call would orphan the result.
        let messages = [
            user("start"),
            ProviderMessage::assistant("", vec![call("c1", "read", r#"{"path":"a.rs"}"#)]),
            user("interjection"),
            ProviderMessage::tool("read", "c1", "x".repeat(4000)),
            assistant("tail"),
        ];
        let preparation = prepare_compaction(&messages, &settings(1003), ContextUsage::default())
            .expect("preparation");
        let kept = &messages[preparation.first_kept_index..];
        for result in kept.iter().filter(|message| message.role == "tool") {
            let id = result.tool_call_id.as_deref().unwrap();
            assert!(
                kept.iter()
                    .any(|message| message.tool_calls.iter().any(|call| call.id == id)),
                "tool result {id} lost its call"
            );
        }
        assert_eq!(preparation.first_kept_index, 1);
    }

    #[test]
    fn cut_points_never_include_tool_results_in_a_long_tool_history() {
        let mut messages = vec![user("task")];
        for index in 0..60 {
            let id = format!("call-{index}");
            messages.push(ProviderMessage::assistant(
                "",
                vec![call(&id, "read", &format!(r#"{{"path":"f{index}.rs"}}"#))],
            ));
            messages.push(ProviderMessage::tool("read", id, "y".repeat(2000)));
        }
        messages.push(assistant("done"));

        for keep in [1u64, 400, 1_500, 5_000, 40_000] {
            let preparation =
                prepare_compaction(&messages, &settings(keep), ContextUsage::default());
            let Some(preparation) = preparation else {
                continue;
            };
            assert_ne!(messages[preparation.first_kept_index].role, "tool");
            let summarized =
                preparation.messages_to_summarize.len() + preparation.turn_prefix_messages.len();
            assert_eq!(summarized, preparation.first_kept_index);
            let summarized_calls = preparation
                .messages_to_summarize
                .iter()
                .chain(&preparation.turn_prefix_messages)
                .flat_map(|message| message.tool_calls.iter())
                .count();
            let summarized_results = preparation
                .messages_to_summarize
                .iter()
                .chain(&preparation.turn_prefix_messages)
                .filter(|message| message.role == "tool")
                .count();
            assert_eq!(summarized_calls, summarized_results);
        }
    }
}
