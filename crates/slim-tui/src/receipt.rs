//! What a turn changed, derived from the blocks that follow the last prompt.
//!
//! The receipt closes a run that touched the project: files edited, lines
//! added and removed, commands run and how the last one ended. Everything
//! comes from data the transcript already holds, so nothing is accumulated
//! per event and a restored session simply has no receipt.

use crate::block::{Block, BlockKind, BlockLifecycle, ToolState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptOutcome {
    Completed,
    Interrupted,
}

/// One file the turn changed. `exact` is false when a line count is missing
/// or estimated: a `write` carries no diff, and a diff can be truncated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub added: u64,
    pub removed: u64,
    pub exact: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptState {
    pub files: usize,
    pub added: u64,
    pub removed: u64,
    /// Some file has no exact line counts, so the totals are a lower bound.
    pub stats_partial: bool,
    pub commands: usize,
    pub failed_commands: usize,
    /// Whether the last command of the turn succeeded, if any ran.
    pub last_command_ok: Option<bool>,
    pub duration_ms: u64,
    pub outcome: ReceiptOutcome,
}

impl ReceiptState {
    /// A turn that changed nothing and ran nothing has no receipt.
    pub fn is_worth_showing(&self) -> bool {
        self.files > 0 || self.commands > 0
    }

    /// The counts of the receipt as separate segments, in reading order:
    /// files, lines, commands. Segments with nothing to say are absent.
    pub fn segments(&self) -> Vec<ReceiptSegment> {
        let mut segments = Vec::new();
        if self.files > 0 {
            segments.push(ReceiptSegment::Text(if self.files == 1 {
                "1 arquivo".into()
            } else {
                format!("{} arquivos", self.files)
            }));
            if self.added > 0 || self.removed > 0 {
                segments.push(ReceiptSegment::Lines {
                    added: self.added,
                    removed: self.removed,
                    partial: self.stats_partial,
                });
            }
        }
        if self.commands > 0 {
            let mut text = if self.commands == 1 {
                "1 comando".to_owned()
            } else {
                format!("{} comandos", self.commands)
            };
            match self.failed_commands {
                0 => {}
                1 => text.push_str(", 1 falhou"),
                failed => text.push_str(&format!(", {failed} falharam")),
            }
            segments.push(ReceiptSegment::Text(text));
        }
        segments.push(ReceiptSegment::Text(format_duration(self.duration_ms)));
        segments
    }

    /// One plain line, for search, copy and the text projection.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.outcome == ReceiptOutcome::Interrupted {
            parts.push("interrompido".into());
        }
        parts.extend(self.segments().into_iter().map(|segment| match segment {
            ReceiptSegment::Text(text) => text,
            ReceiptSegment::Lines {
                added,
                removed,
                partial,
            } => format!("{}+{added} -{removed}", if partial { "~" } else { "" }),
        }));
        parts.join(" · ")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiptSegment {
    Text(String),
    /// Added and removed lines, drawn as `+A -R` in the diff colors. `partial`
    /// marks a lower bound (`~+A -R`).
    Lines {
        added: u64,
        removed: u64,
        partial: bool,
    },
}

fn format_duration(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=59_999 => format!("{}s", ms / 1_000),
        _ => format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1_000),
    }
}

/// Blocks of the current turn: everything after the last user prompt.
fn turn_blocks(blocks: &[Block]) -> &[Block] {
    let start = blocks
        .iter()
        .rposition(|block| matches!(block.kind(), BlockKind::User(_)))
        .map_or(0, |index| index + 1);
    &blocks[start..]
}

/// Files changed by the successful `patch` and `write` calls in `blocks`, in
/// order of first change, one entry per path.
pub fn file_changes(blocks: &[Block]) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = Vec::new();
    for block in blocks {
        let BlockKind::Tool(tool) = block.kind() else {
            continue;
        };
        if block.lifecycle != BlockLifecycle::Complete || tool.historical {
            continue;
        }
        let Some(change) = change_of(tool) else {
            continue;
        };
        match changes.iter_mut().find(|known| known.path == change.path) {
            Some(known) => {
                known.added += change.added;
                known.removed += change.removed;
                known.exact &= change.exact;
            }
            None => changes.push(change),
        }
    }
    changes
}

fn change_of(tool: &ToolState) -> Option<FileChange> {
    match tool.name.as_str() {
        "patch" => {
            if let Some(diff) = &tool.edit_diff {
                return Some(FileChange {
                    path: diff.path.clone(),
                    added: diff.hunks.iter().map(|hunk| hunk.added.len() as u64).sum(),
                    removed: diff
                        .hunks
                        .iter()
                        .map(|hunk| hunk.removed.len() as u64)
                        .sum(),
                    exact: !diff.truncated,
                });
            }
            // No diff arrived: fall back to the estimate in the call summary.
            let (added, removed) = estimated_stats(&tool.arguments_summary).unwrap_or((0, 0));
            Some(FileChange {
                path: path_of(&tool.arguments_summary)?,
                added,
                removed,
                exact: false,
            })
        }
        "write" => Some(FileChange {
            path: path_of(&tool.arguments_summary)?,
            added: 0,
            removed: 0,
            exact: false,
        }),
        _ => None,
    }
}

fn path_of(arguments_summary: &str) -> Option<String> {
    arguments_summary
        .split(" · ")
        .find_map(|segment| segment.trim().strip_prefix("path="))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

/// `+A -R` segment the projection appends to a patch summary.
fn estimated_stats(arguments_summary: &str) -> Option<(u64, u64)> {
    arguments_summary.split(" · ").find_map(|segment| {
        let (added, removed) = segment.trim().strip_prefix('+')?.split_once(" -")?;
        Some((added.parse().ok()?, removed.parse().ok()?))
    })
}

/// The receipt of the turn that just ended.
pub fn turn_receipt(blocks: &[Block], duration_ms: u64, outcome: ReceiptOutcome) -> ReceiptState {
    let turn = turn_blocks(blocks);
    let changes = file_changes(turn);
    let mut commands = 0;
    let mut failed_commands = 0;
    let mut last_command_ok = None;
    for block in turn {
        let BlockKind::Tool(tool) = block.kind() else {
            continue;
        };
        if tool.name != "shell" || tool.historical {
            continue;
        }
        match block.lifecycle {
            BlockLifecycle::Complete => {
                commands += 1;
                last_command_ok = Some(true);
            }
            BlockLifecycle::Failed => {
                commands += 1;
                failed_commands += 1;
                last_command_ok = Some(false);
            }
            _ => {}
        }
    }
    ReceiptState {
        files: changes.len(),
        added: changes.iter().map(|change| change.added).sum(),
        removed: changes.iter().map(|change| change.removed).sum(),
        stats_partial: changes.iter().any(|change| !change.exact),
        commands,
        failed_commands,
        last_command_ok,
        duration_ms,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ToolBatchId, ToolCallId};
    use slim_core::{ToolEditDiff, ToolEditHunk};

    fn tool(
        name: &str,
        summary: &str,
        lifecycle: BlockLifecycle,
        diff: Option<ToolEditDiff>,
    ) -> Block {
        Block::new(
            format!("{name}-{summary}"),
            BlockKind::Tool(ToolState {
                batch_id: ToolBatchId("b".into()),
                call_id: ToolCallId(format!("{name}-{summary}").into()),
                name: name.into(),
                arguments_summary: summary.into(),
                edit_diff: diff,
                ..ToolState::default()
            }),
            lifecycle,
        )
    }

    fn diff(path: &str, added: usize, removed: usize, truncated: bool) -> ToolEditDiff {
        ToolEditDiff {
            path: path.into(),
            hunks: vec![ToolEditHunk {
                start_line: 1,
                removed: vec!["old".into(); removed],
                added: vec!["new".into(); added],
            }],
            truncated,
        }
    }

    fn user() -> Block {
        Block::new(
            "u",
            BlockKind::User("pergunta".into()),
            BlockLifecycle::Complete,
        )
    }

    #[test]
    fn totals_come_from_exact_diffs_and_count_a_path_once() {
        let blocks = [
            user(),
            tool(
                "patch",
                "path=src/a.rs · +9 -9",
                BlockLifecycle::Complete,
                Some(diff("src/a.rs", 2, 1, false)),
            ),
            tool(
                "patch",
                "path=src/a.rs",
                BlockLifecycle::Complete,
                Some(diff("src/a.rs", 3, 0, false)),
            ),
            tool(
                "patch",
                "path=src/b.rs",
                BlockLifecycle::Complete,
                Some(diff("src/b.rs", 0, 4, false)),
            ),
        ];
        let receipt = turn_receipt(&blocks, 6_000, ReceiptOutcome::Completed);
        assert_eq!((receipt.files, receipt.added, receipt.removed), (2, 5, 5));
        assert!(!receipt.stats_partial);
        assert!(receipt.is_worth_showing());
    }

    #[test]
    fn a_write_counts_as_a_file_without_inventing_line_counts() {
        let blocks = [
            user(),
            tool("write", "path=new.txt", BlockLifecycle::Complete, None),
            tool(
                "patch",
                "path=a.rs",
                BlockLifecycle::Complete,
                Some(diff("a.rs", 1, 1, false)),
            ),
        ];
        let receipt = turn_receipt(&blocks, 0, ReceiptOutcome::Completed);
        assert_eq!((receipt.files, receipt.added, receipt.removed), (2, 1, 1));
        assert!(receipt.stats_partial, "the write has no counts");
    }

    #[test]
    fn a_truncated_diff_or_a_missing_one_is_marked_partial() {
        let truncated = [
            user(),
            tool(
                "patch",
                "path=a.rs",
                BlockLifecycle::Complete,
                Some(diff("a.rs", 400, 0, true)),
            ),
        ];
        assert!(turn_receipt(&truncated, 0, ReceiptOutcome::Completed).stats_partial);
        // No diff arrived: the estimate in the summary stands in, flagged partial.
        let estimated = [
            user(),
            tool("patch", "path=a.rs · +7 -2", BlockLifecycle::Complete, None),
        ];
        let receipt = turn_receipt(&estimated, 0, ReceiptOutcome::Completed);
        assert_eq!((receipt.added, receipt.removed), (7, 2));
        assert!(receipt.stats_partial);
    }

    #[test]
    fn failed_edits_are_not_changes_and_commands_track_the_last_outcome() {
        let blocks = [
            user(),
            tool(
                "patch",
                "path=a.rs",
                BlockLifecycle::Failed,
                Some(diff("a.rs", 5, 5, false)),
            ),
            tool("shell", "command=cargo test", BlockLifecycle::Failed, None),
            tool(
                "shell",
                "command=cargo test",
                BlockLifecycle::Complete,
                None,
            ),
            tool("read", "path=x", BlockLifecycle::Complete, None),
        ];
        let receipt = turn_receipt(&blocks, 0, ReceiptOutcome::Completed);
        assert_eq!(receipt.files, 0);
        assert_eq!((receipt.commands, receipt.failed_commands), (2, 1));
        assert_eq!(receipt.last_command_ok, Some(true));
        let last_failed = [
            user(),
            tool("shell", "command=x", BlockLifecycle::Failed, None),
        ];
        assert_eq!(
            turn_receipt(&last_failed, 0, ReceiptOutcome::Completed).last_command_ok,
            Some(false)
        );
    }

    #[test]
    fn only_the_current_turn_counts_and_reads_alone_earn_no_receipt() {
        let blocks = [
            user(),
            tool(
                "patch",
                "path=old.rs",
                BlockLifecycle::Complete,
                Some(diff("old.rs", 9, 9, false)),
            ),
            user(),
            tool("read", "path=x", BlockLifecycle::Complete, None),
            tool("search", "pattern=y", BlockLifecycle::Complete, None),
        ];
        let receipt = turn_receipt(&blocks, 1_000, ReceiptOutcome::Completed);
        assert!(
            !receipt.is_worth_showing(),
            "the earlier turn's edit is not this turn's"
        );
    }
}
