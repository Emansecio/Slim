//! The work of a finished turn, folded under one row.
//!
//! Once a run ends well and its last block is the answer, everything the agent
//! did on the way (thoughts, notes between calls, tool rows) matters less than
//! the answer itself. This module decides where that stretch is and what one
//! row says about it. The row is a block of its own, inserted before the
//! stretch, so folding, selection, search and scroll anchors keep working on
//! real blocks; the blocks it folds stay in the transcript untouched.

use crate::api::BlockId;
use crate::block::{Block, BlockKind, BlockLifecycle};

/// A stretch shorter than this is already one row (a lone thought or call),
/// so folding it would only rename the row.
const MIN_FOLDED_BLOCKS: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkState {
    /// The answer that ends the stretch: every block between the row and this
    /// one belongs to the row. The stretch is bounded by identity, not by a
    /// count, so a block removed from inside it later (a queued prompt that
    /// is popped) cannot move its end onto the answer.
    pub until: BlockId,
    /// Wall-clock the work took, when the blocks carry their own timestamps
    /// and the turn has no receipt to say it.
    pub duration_ms: Option<u64>,
    /// The calls by kind (`3 leituras, 1 edição`); empty when none ran.
    pub tally: String,
}

impl WorkState {
    /// The row as plain text, for search, copy and the text projection:
    /// `Trabalhou 12s · 3 leituras, 1 edição`.
    pub fn summary(&self) -> String {
        let mut text = String::from(WORK_LABEL);
        if let Some(duration) = self.duration_ms {
            text.push(' ');
            text.push_str(&crate::receipt::format_duration(duration));
        }
        if !self.tally.is_empty() {
            text.push_str(" · ");
            text.push_str(&self.tally);
        }
        text
    }
}

/// What the folded row calls the turn's work.
pub const WORK_LABEL: &str = "Trabalhou";

fn is_work_block(block: &Block) -> bool {
    matches!(
        block.kind(),
        BlockKind::Assistant(_) | BlockKind::Thinking(_) | BlockKind::Tool(_)
    )
}

/// Blocks of the turn whose prompt is at `user`: up to the next prompt.
fn turn_end(blocks: &[Block], user: usize) -> usize {
    blocks[user + 1..]
        .iter()
        .position(|block| matches!(block.kind(), BlockKind::User(_)))
        .map_or(blocks.len(), |offset| user + 1 + offset)
}

/// The fold for the turn whose prompt sits at `user`: the index the row goes
/// to and its content. `None` unless the turn ended well, its last agent
/// output is a finished answer, and something sits between the turn's first
/// agent output and that answer.
///
/// The fold covers the whole stretch from the first agent output to the
/// answer, whatever sits inside it: answered questions and approvals, system
/// notices, a prompt queued meanwhile. The tally and the duration describe
/// that whole stretch. A turn is left alone when any of its blocks is still
/// running, was cancelled, or failed as prose or as a thought, when a question
/// is still unanswered, when an error closed it, when a receipt sits inside
/// the stretch, or when it already has its row. A failed tool call does not
/// stop the fold: the folded row keeps one row for each failure. The duration
/// is left out when the turn has a receipt, which already says it.
pub fn plan_turn(blocks: &[Block], user: usize) -> Option<(usize, WorkState)> {
    if !matches!(blocks.get(user)?.kind(), BlockKind::User(_)) {
        return None;
    }
    let end = turn_end(blocks, user);
    let turn = &blocks[user + 1..end];
    let unfinished = |block: &Block| match block.kind() {
        BlockKind::Assistant(_) | BlockKind::Thinking(_) => {
            block.lifecycle != BlockLifecycle::Complete
        }
        BlockKind::Tool(_) => matches!(
            block.lifecycle,
            BlockLifecycle::Pending | BlockLifecycle::Streaming | BlockLifecycle::Cancelled
        ),
        BlockKind::InteractionRequest(request) => request.acknowledgement.is_none(),
        BlockKind::Error(_) | BlockKind::Work(_) => true,
        _ => false,
    };
    if turn.iter().any(unfinished) {
        return None;
    }
    let last = turn.iter().rposition(is_work_block)?;
    let BlockKind::Assistant(answer) = turn[last].kind() else {
        return None;
    };
    if answer.trim().is_empty() {
        return None;
    }
    let first = turn.iter().position(is_work_block)?;
    let folded = &turn[first..last];
    if folded.len() < MIN_FOLDED_BLOCKS
        || folded
            .iter()
            .any(|block| matches!(block.kind(), BlockKind::Receipt(_)))
    {
        return None;
    }
    let has_receipt = turn[last..]
        .iter()
        .any(|block| matches!(block.kind(), BlockKind::Receipt(_)));
    let names: Vec<&str> = folded
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::Tool(tool) => Some(tool.name.as_str()),
            _ => None,
        })
        .collect();
    let state = WorkState {
        until: turn[last].id.clone(),
        duration_ms: if has_receipt {
            None
        } else {
            duration_ms(folded)
        },
        tally: crate::view_model::completed_tool_phrase(&names),
    };
    Some((user + 1 + first, state))
}

/// Index just past the last block the row at `row` folds: where its answer is.
/// A row whose answer is gone folds nothing.
pub fn span_end(blocks: &[Block], row: usize) -> usize {
    let Some(BlockKind::Work(work)) = blocks.get(row).map(Block::kind) else {
        return row + 1;
    };
    blocks[row + 1..]
        .iter()
        .position(|block| block.id == work.until)
        .map_or(row + 1, |offset| row + 1 + offset)
}

/// Wall-clock from the first block that started to the last one that ended,
/// for blocks that carry both. A restored transcript has no timestamps and so
/// no duration.
fn duration_ms(blocks: &[Block]) -> Option<u64> {
    let mut started: Option<u64> = None;
    let mut ended: Option<u64> = None;
    for block in blocks {
        if let (Some(start), Some(end)) = (block.started_ms, block.ended_ms) {
            started = Some(started.map_or(start, |known| known.min(start)));
            ended = Some(ended.map_or(end, |known| known.max(end)));
        }
    }
    Some(ended?.saturating_sub(started?))
}

/// Index of the row that folds the block at `index`, when it is folded away:
/// the row is collapsed and `index` is one of its members.
pub fn collapsed_row_over(blocks: &[Block], index: usize) -> Option<usize> {
    // The row stands in the same turn as its members, so the scan stops at
    // the prompt.
    for row in (0..index.min(blocks.len())).rev() {
        match blocks[row].kind() {
            BlockKind::User(_) => return None,
            BlockKind::Work(_) => {
                return (index < span_end(blocks, row)
                    && blocks[row].fold != crate::block::FoldState::Expanded)
                    .then_some(row);
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ToolBatchId, ToolCallId};
    use crate::block::ToolState;

    fn block(id: &str, kind: BlockKind, lifecycle: BlockLifecycle) -> Block {
        Block::new(id, kind, lifecycle)
    }

    fn user() -> Block {
        block(
            "u",
            BlockKind::User("pergunta".into()),
            BlockLifecycle::Complete,
        )
    }

    fn answer(text: &str) -> Block {
        block(
            "a",
            BlockKind::Assistant(text.into()),
            BlockLifecycle::Complete,
        )
    }

    fn thought() -> Block {
        block(
            "t",
            BlockKind::Thinking("hm".into()),
            BlockLifecycle::Complete,
        )
    }

    fn tool(id: &str, name: &str, lifecycle: BlockLifecycle) -> Block {
        block(
            id,
            BlockKind::Tool(ToolState {
                batch_id: ToolBatchId(format!("b-{id}").into()),
                call_id: ToolCallId(format!("c-{id}").into()),
                name: name.into(),
                ..ToolState::default()
            }),
            lifecycle,
        )
    }

    #[test]
    fn a_finished_turn_folds_everything_before_its_answer() {
        let blocks = [
            user(),
            thought(),
            tool("1", "read", BlockLifecycle::Complete),
            tool("2", "shell", BlockLifecycle::Failed),
            answer("pronto"),
        ];
        let (at, work) = plan_turn(&blocks, 0).expect("folds");
        assert_eq!(at, 1);
        assert_eq!(work.until, blocks[4].id);
        assert_eq!(work.tally, "1 leitura, 1 comando");
        assert_eq!(work.summary(), "Trabalhou · 1 leitura, 1 comando");
    }

    #[test]
    fn nothing_folds_without_an_answer_last_or_with_a_lone_block() {
        // The turn ends on a call, or has only one block before the answer.
        assert!(plan_turn(
            &[
                user(),
                thought(),
                tool("1", "read", BlockLifecycle::Complete)
            ],
            0
        )
        .is_none());
        assert!(plan_turn(&[user(), answer("oi")], 0).is_none());
        assert!(plan_turn(&[user(), thought(), answer("oi")], 0).is_none());
        assert!(plan_turn(
            &[
                user(),
                tool("1", "read", BlockLifecycle::Complete),
                answer("oi")
            ],
            0
        )
        .is_none());
        // An empty answer is not an answer.
        assert!(plan_turn(&[user(), thought(), thought(), answer(" ")], 0).is_none());
    }

    #[test]
    fn running_cancelled_or_failed_turns_are_left_as_they_are() {
        for lifecycle in [
            BlockLifecycle::Streaming,
            BlockLifecycle::Cancelled,
            BlockLifecycle::Failed,
        ] {
            let mut last = answer("fim");
            last.lifecycle = lifecycle;
            let blocks = [
                user(),
                thought(),
                tool("1", "read", BlockLifecycle::Complete),
                last,
            ];
            assert!(plan_turn(&blocks, 0).is_none(), "{lifecycle:?}");
        }
        let cancelled = [
            user(),
            thought(),
            tool("1", "read", BlockLifecycle::Cancelled),
            answer("fim"),
        ];
        assert!(plan_turn(&cancelled, 0).is_none());
        let errored = [
            user(),
            thought(),
            tool("1", "read", BlockLifecycle::Complete),
            answer("fim"),
            block(
                "e",
                BlockKind::Error("falhou".into()),
                BlockLifecycle::Failed,
            ),
        ];
        assert!(plan_turn(&errored, 0).is_none());
    }

    fn system(id: &str) -> Block {
        block(
            id,
            BlockKind::System("aviso".into()),
            BlockLifecycle::Complete,
        )
    }

    #[test]
    fn the_fold_covers_the_whole_stretch_whatever_sits_inside_it() {
        let blocks = [
            user(),
            thought(),
            tool("1", "read", BlockLifecycle::Complete),
            system("s"),
            thought(),
            tool("2", "read", BlockLifecycle::Complete),
            answer("fim"),
        ];
        let (at, work) = plan_turn(&blocks, 0).expect("folds the whole stretch");
        assert_eq!(at, 1, "from the first agent output");
        assert_eq!(work.until, blocks[6].id);
        assert_eq!(work.tally, "2 leituras", "the tally counts the whole turn");
        // A notice before any agent output stays above the header.
        let late = [
            user(),
            system("s"),
            thought(),
            tool("1", "read", BlockLifecycle::Complete),
            answer("fim"),
        ];
        assert_eq!(plan_turn(&late, 0).map(|(at, _)| at), Some(2));
    }

    #[test]
    fn a_queued_prompt_inside_the_stretch_neither_blocks_nor_corrupts_the_fold() {
        let queued = block(
            "q",
            BlockKind::QueuedUser("depois".into()),
            BlockLifecycle::Pending,
        );
        let mut blocks = vec![
            user(),
            thought(),
            queued,
            tool("1", "read", BlockLifecycle::Complete),
            answer("fim"),
        ];
        let (at, work) = plan_turn(&blocks, 0).expect("folds around the queued prompt");
        blocks.insert(
            at,
            block("w", BlockKind::Work(work), BlockLifecycle::Complete),
        );
        assert_eq!(span_end(&blocks, 1), 5);
        assert_eq!(
            collapsed_row_over(&blocks, 3),
            Some(1),
            "queued block is folded"
        );
        // The queue pops it later: the stretch still ends at the answer.
        blocks.remove(3);
        assert_eq!(span_end(&blocks, 1), 4);
        assert_eq!(collapsed_row_over(&blocks, 3), Some(1));
        assert_eq!(collapsed_row_over(&blocks, 4), None, "the answer stays out");
    }

    #[test]
    fn a_receipt_inside_the_stretch_leaves_the_turn_alone() {
        let receipt = block(
            "r",
            BlockKind::Receipt(crate::receipt::ReceiptState {
                files: 0,
                added: 0,
                removed: 0,
                stats_partial: false,
                commands: 1,
                failed_commands: 0,
                last_command_ok: Some(true),
                duration_ms: 5_000,
                outcome: crate::receipt::ReceiptOutcome::Completed,
            }),
            BlockLifecycle::Complete,
        );
        let inside = [user(), thought(), receipt, thought(), answer("fim")];
        assert!(plan_turn(&inside, 0).is_none());
    }

    #[test]
    fn a_receipt_after_the_answer_owns_the_duration() {
        let mut first = thought();
        first.started_ms = Some(1_000);
        first.ended_ms = Some(4_000);
        let mut second = tool("1", "read", BlockLifecycle::Complete);
        second.started_ms = Some(4_100);
        second.ended_ms = Some(13_000);
        let receipt = block(
            "r",
            BlockKind::Receipt(crate::receipt::ReceiptState {
                files: 0,
                added: 0,
                removed: 0,
                stats_partial: false,
                commands: 1,
                failed_commands: 0,
                last_command_ok: Some(true),
                duration_ms: 12_000,
                outcome: crate::receipt::ReceiptOutcome::Completed,
            }),
            BlockLifecycle::Complete,
        );
        let blocks = [user(), first, second, answer("fim"), receipt];
        let (_, work) = plan_turn(&blocks, 0).expect("folds");
        assert_eq!(work.duration_ms, None);
        assert_eq!(work.summary(), "Trabalhou · 1 leitura");
    }

    #[test]
    fn only_the_turn_of_the_prompt_is_considered_and_it_is_folded_once() {
        let mut blocks = vec![
            user(),
            thought(),
            tool("1", "read", BlockLifecycle::Complete),
            answer("um"),
            user(),
            thought(),
            tool("2", "read", BlockLifecycle::Complete),
        ];
        assert!(
            plan_turn(&blocks, 4).is_none(),
            "second turn has no answer yet"
        );
        let (at, work) = plan_turn(&blocks, 0).expect("first turn folds");
        blocks.insert(
            at,
            block("w", BlockKind::Work(work), BlockLifecycle::Complete),
        );
        assert!(plan_turn(&blocks, 0).is_none(), "already folded");
    }

    #[test]
    fn the_duration_spans_the_blocks_that_carry_both_timestamps() {
        let mut first = thought();
        first.started_ms = Some(1_000);
        first.ended_ms = Some(4_000);
        let mut second = tool("1", "read", BlockLifecycle::Complete);
        second.started_ms = Some(4_100);
        second.ended_ms = Some(13_000);
        let blocks = [user(), first, second, answer("fim")];
        let (_, work) = plan_turn(&blocks, 0).expect("folds");
        assert_eq!(work.duration_ms, Some(12_000));
        assert_eq!(work.summary(), "Trabalhou 12s · 1 leitura");
    }

    #[test]
    fn a_member_is_folded_away_only_while_its_row_is_collapsed() {
        let state = WorkState {
            until: answer("fim").id,
            duration_ms: None,
            tally: String::new(),
        };
        let mut blocks = vec![
            user(),
            block("w", BlockKind::Work(state), BlockLifecycle::Complete),
            thought(),
            tool("1", "read", BlockLifecycle::Complete),
            answer("fim"),
        ];
        assert_eq!(collapsed_row_over(&blocks, 3), Some(1));
        assert_eq!(collapsed_row_over(&blocks, 4), None, "the answer stays out");
        blocks[1].fold = crate::block::FoldState::Expanded;
        assert_eq!(collapsed_row_over(&blocks, 3), None);
    }
}
