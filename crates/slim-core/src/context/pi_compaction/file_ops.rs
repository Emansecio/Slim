//! File-operation tracking for compaction summaries.
//!
//! Port of Pi's `FileOperations`, `extractFileOpsFromMessage`,
//! `computeFileLists` and `formatFileOperations`
//! (`packages/coding-agent/src/core/compaction/utils.ts`), Copyright (c) 2025
//! Mario Zechner, MIT License (https://github.com/earendil-works/pi). Slim has
//! no `edit` tool: its `patch` tool plays that role, and `write` stays a
//! write. Both end up in the modified list, as in Pi.

use crate::provider::ProviderMessage;
use crate::session::{
    MAX_COMPACTION_FILES_BYTES, MAX_COMPACTION_FILES_PER_CLASS, MAX_COMPACTION_PATH_BYTES,
};
use std::collections::HashMap;

/// Files read and files modified, each sorted, as stored in a checkpoint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileLists {
    /// Files that were read and never modified.
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Paths touched by tool calls, remembering when each was last touched so a
/// bounded list can keep the most recent ones.
#[derive(Clone, Debug, Default)]
pub struct FileOperations {
    read: HashMap<String, u64>,
    written: HashMap<String, u64>,
    edited: HashMap<String, u64>,
    next_stamp: u64,
}

impl FileOperations {
    pub fn new() -> Self {
        Self::default()
    }

    fn stamp(&mut self) -> u64 {
        self.next_stamp += 1;
        self.next_stamp
    }

    pub fn add_read(&mut self, path: &str) {
        let stamp = self.stamp();
        self.read.insert(path.to_owned(), stamp);
    }

    pub fn add_written(&mut self, path: &str) {
        let stamp = self.stamp();
        self.written.insert(path.to_owned(), stamp);
    }

    pub fn add_edited(&mut self, path: &str) {
        let stamp = self.stamp();
        self.edited.insert(path.to_owned(), stamp);
    }

    /// Seeds the operations with the lists of the previous compaction, as Pi
    /// does: its read files stay read and its modified files stay modified.
    pub fn add_previous(&mut self, previous: &FileLists) {
        for path in &previous.read_files {
            self.add_read(path);
        }
        for path in &previous.modified_files {
            self.add_edited(path);
        }
    }

    /// Records the file operations of the tool calls in an assistant message.
    /// A call counts only when its arguments are a JSON object with a
    /// non-empty string `path`.
    pub fn extract_from_message(&mut self, message: &ProviderMessage) {
        if message.role != "assistant" {
            return;
        }
        for call in &message.tool_calls {
            let Ok(serde_json::Value::Object(arguments)) =
                serde_json::from_str::<serde_json::Value>(&call.arguments)
            else {
                continue;
            };
            let Some(path) = arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .filter(|path| !path.is_empty())
            else {
                continue;
            };
            match call.name.as_str() {
                "read" => self.add_read(path),
                "write" => self.add_written(path),
                "patch" => self.add_edited(path),
                _ => {}
            }
        }
    }

    /// Files read only and files modified, each with its last-touch stamp.
    /// Modified wins over read.
    fn classified(&self) -> (HashMap<&str, u64>, HashMap<&str, u64>) {
        let mut modified: HashMap<&str, u64> = HashMap::new();
        for (path, stamp) in self.written.iter().chain(self.edited.iter()) {
            let entry = modified.entry(path.as_str()).or_insert(*stamp);
            *entry = (*entry).max(*stamp);
        }
        let read_only = self
            .read
            .iter()
            .filter(|(path, _)| !modified.contains_key(path.as_str()))
            .map(|(path, stamp)| (path.as_str(), *stamp))
            .collect();
        (read_only, modified)
    }
}

/// Final file lists: read files that were not modified, and modified files,
/// each sorted.
pub fn compute_file_lists(file_ops: &FileOperations) -> FileLists {
    let (read_only, modified) = file_ops.classified();
    let sorted = |paths: HashMap<&str, u64>| {
        let mut paths = paths
            .into_keys()
            .map(str::to_owned)
            .collect::<Vec<String>>();
        paths.sort();
        paths
    };
    FileLists {
        read_files: sorted(read_only),
        modified_files: sorted(modified),
    }
}

/// The `<read-files>` and `<modified-files>` blocks appended to a summary.
/// Empty when there is nothing to list.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

/// Recovers the file lists a previous summary carries in the trailing blocks
/// written by [`format_file_operations`]. A summary without those blocks has
/// no lists.
pub fn parse_file_operation_blocks(summary: &str) -> FileLists {
    fn take_block<'a>(text: &'a str, tag: &str) -> (&'a str, Vec<String>) {
        let closing = format!("\n</{tag}>");
        let opening = format!("\n\n<{tag}>\n");
        let Some(without_close) = text.strip_suffix(closing.as_str()) else {
            return (text, Vec::new());
        };
        let Some(open_at) = without_close.rfind(opening.as_str()) else {
            return (text, Vec::new());
        };
        let body = &without_close[open_at + opening.len()..];
        (
            &without_close[..open_at],
            body.lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    let (rest, modified_files) = take_block(summary, "modified-files");
    let (_, read_files) = take_block(rest, "read-files");
    FileLists {
        read_files,
        modified_files,
    }
}

/// A summary ready to be persisted, with the file lists it carries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FittedSummary {
    /// Summary text followed by the file-operation blocks.
    pub summary: String,
    pub files: FileLists,
}

/// Appends the file-operation blocks to the summarizer's `text` so the result
/// stays within what a durable checkpoint accepts (`max_bytes` for the whole
/// summary, and the session reducer's limits for the file lists).
///
/// The lists are bounded first, deterministically and keeping the most
/// recently touched files: paths over the per-path limit are dropped, then at
/// most the per-class count and the total path bytes are kept, then as many as
/// still fit `max_bytes`. When `text` alone exceeds `max_bytes` the compaction
/// fails instead of persisting a record the reducer would reject.
pub fn fit_summary_for_persistence(
    text: &str,
    file_ops: &FileOperations,
    max_bytes: usize,
) -> Result<FittedSummary, String> {
    if text.len() > max_bytes {
        return Err(format!(
            "compaction summary is {} bytes, over the {max_bytes}-byte persistence limit; it was not shortened",
            text.len()
        ));
    }

    let (read_only, modified) = file_ops.classified();
    let mut candidates: Vec<(u64, bool, &str)> = read_only
        .into_iter()
        .map(|(path, stamp)| (stamp, false, path))
        .chain(
            modified
                .into_iter()
                .map(|(path, stamp)| (stamp, true, path)),
        )
        .filter(|(_, _, path)| !path.is_empty() && path.len() <= MAX_COMPACTION_PATH_BYTES)
        .collect();
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(right.2)));

    let mut counts = [0usize; 2];
    let mut path_bytes = 0usize;
    let mut selected: Vec<(bool, &str)> = Vec::new();
    for (_, is_modified, path) in candidates {
        let class = usize::from(is_modified);
        if counts[class] >= MAX_COMPACTION_FILES_PER_CLASS
            || path_bytes + path.len() > MAX_COMPACTION_FILES_BYTES
        {
            continue;
        }
        counts[class] += 1;
        path_bytes += path.len();
        selected.push((is_modified, path));
    }

    let render = |count: usize| -> (String, FileLists) {
        let mut files = FileLists::default();
        for (is_modified, path) in &selected[..count] {
            if *is_modified {
                files.modified_files.push((*path).to_owned());
            } else {
                files.read_files.push((*path).to_owned());
            }
        }
        files.read_files.sort();
        files.modified_files.sort();
        let blocks = format_file_operations(&files.read_files, &files.modified_files);
        (blocks, files)
    };

    // The rendered size grows with the number of kept files: find the largest
    // prefix of the newest-first selection that still fits.
    let budget = max_bytes - text.len();
    let (mut low, mut high) = (0usize, selected.len());
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if render(middle).0.len() <= budget {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let (blocks, files) = render(low);
    Ok(FittedSummary {
        summary: format!("{text}{blocks}"),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderToolCall;

    fn call(name: &str, arguments: &str) -> ProviderToolCall {
        ProviderToolCall {
            id: format!("{name}-id"),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    // Pi: compaction-nested-calls.test.ts, with direct calls (Slim keeps no
    // nested-call records on tool results).
    #[test]
    fn lists_read_files_and_modified_files() {
        let assistant = ProviderMessage::assistant(
            "",
            vec![
                call("read", r#"{"path":"a.ts"}"#),
                call("patch", r#"{"path":"b.ts","edits":[]}"#),
                call("write", r#"{"content":"x"}"#),
            ],
        );
        let mut ops = FileOperations::new();
        ops.extract_from_message(&assistant);
        assert_eq!(
            compute_file_lists(&ops),
            FileLists {
                read_files: strings(&["a.ts"]),
                modified_files: strings(&["b.ts"]),
            }
        );
    }

    #[test]
    fn written_and_edited_are_both_modified_and_win_over_read() {
        let mut ops = FileOperations::new();
        ops.add_read("z.rs");
        ops.add_read("m.rs");
        ops.add_read("a.rs");
        ops.add_written("m.rs");
        ops.add_edited("b.rs");
        ops.add_written("a.rs");
        ops.add_edited("a.rs");
        assert_eq!(
            compute_file_lists(&ops),
            FileLists {
                read_files: strings(&["z.rs"]),
                modified_files: strings(&["a.rs", "b.rs", "m.rs"]),
            }
        );
    }

    #[test]
    fn ignores_other_tools_other_roles_and_unusable_arguments() {
        let mut tool = ProviderMessage::tool("read", "c1", "body");
        tool.tool_calls = vec![call("read", r#"{"path":"from-tool.rs"}"#)];
        let messages = [
            ProviderMessage::assistant(
                "",
                vec![
                    call("search", r#"{"path":"src"}"#),
                    call("read", r#"{"path":""}"#),
                    call("read", r#"{"path":3}"#),
                    call("read", "not json"),
                    call("read", r#"["a.rs"]"#),
                ],
            ),
            tool,
            ProviderMessage::user("read c.rs"),
        ];
        let mut ops = FileOperations::new();
        for message in &messages {
            ops.extract_from_message(message);
        }
        assert_eq!(compute_file_lists(&ops), FileLists::default());
    }

    #[test]
    fn previous_lists_seed_the_operations() {
        let mut ops = FileOperations::new();
        ops.add_previous(&FileLists {
            read_files: strings(&["old-read.rs", "both.rs"]),
            modified_files: strings(&["old-mod.rs"]),
        });
        ops.add_written("both.rs");
        ops.add_read("new.rs");
        assert_eq!(
            compute_file_lists(&ops),
            FileLists {
                read_files: strings(&["new.rs", "old-read.rs"]),
                modified_files: strings(&["both.rs", "old-mod.rs"]),
            }
        );
    }

    #[test]
    fn formats_blocks_exactly() {
        assert_eq!(format_file_operations(&[], &[]), "");
        assert_eq!(
            format_file_operations(&strings(&["a", "b"]), &[]),
            "\n\n<read-files>\na\nb\n</read-files>"
        );
        assert_eq!(
            format_file_operations(&[], &strings(&["c"])),
            "\n\n<modified-files>\nc\n</modified-files>"
        );
        assert_eq!(
            format_file_operations(&strings(&["a"]), &strings(&["c", "d"])),
            "\n\n<read-files>\na\n</read-files>\n\n<modified-files>\nc\nd\n</modified-files>"
        );
    }

    #[test]
    fn blocks_round_trip_through_the_parser() {
        let read = strings(&["a.rs", "dir/b.rs"]);
        let modified = strings(&["c.rs"]);
        for (read, modified) in [
            (read.clone(), modified.clone()),
            (read, Vec::new()),
            (Vec::new(), modified),
            (Vec::new(), Vec::new()),
        ] {
            let summary = format!("## Goal\nx{}", format_file_operations(&read, &modified));
            assert_eq!(
                parse_file_operation_blocks(&summary),
                FileLists {
                    read_files: read,
                    modified_files: modified
                }
            );
        }
        assert_eq!(
            parse_file_operation_blocks("## Goal\nno blocks"),
            FileLists::default()
        );
        assert_eq!(
            parse_file_operation_blocks("text <read-files>\nx\n</read-files> inline"),
            FileLists::default()
        );
    }

    #[test]
    fn fit_appends_blocks_when_everything_fits() {
        let mut ops = FileOperations::new();
        ops.add_read("a.rs");
        ops.add_edited("b.rs");
        let fitted = fit_summary_for_persistence("summary", &ops, 64 * 1024).unwrap();
        assert_eq!(
            fitted.summary,
            "summary\n\n<read-files>\na.rs\n</read-files>\n\n<modified-files>\nb.rs\n</modified-files>"
        );
        assert_eq!(fitted.files, compute_file_lists(&ops));
    }

    #[test]
    fn fit_keeps_the_most_recent_files_that_fit_the_byte_budget() {
        let mut ops = FileOperations::new();
        for index in 0..10 {
            ops.add_read(&format!("old/read-{index}.rs"));
        }
        ops.add_edited("newest.rs");
        let text = "t".repeat(100);
        let all = fit_summary_for_persistence(&text, &ops, 64 * 1024).unwrap();
        assert_eq!(all.files.read_files.len(), 10);

        let budget = text.len()
            + format_file_operations(
                &strings(&["old/read-9.rs", "old/read-8.rs"]),
                &strings(&["newest.rs"]),
            )
            .len();
        let fitted = fit_summary_for_persistence(&text, &ops, budget).unwrap();
        assert!(fitted.summary.len() <= budget);
        assert_eq!(fitted.files.modified_files, strings(&["newest.rs"]));
        assert_eq!(
            fitted.files.read_files,
            strings(&["old/read-8.rs", "old/read-9.rs"])
        );
        assert!(fitted.summary.starts_with(&text));
    }

    #[test]
    fn fit_drops_every_file_when_only_the_text_fits() {
        let mut ops = FileOperations::new();
        ops.add_read("a.rs");
        let fitted = fit_summary_for_persistence("abc", &ops, 3).unwrap();
        assert_eq!(fitted.summary, "abc");
        assert_eq!(fitted.files, FileLists::default());
    }

    #[test]
    fn fit_fails_when_the_text_alone_is_too_long() {
        let error = fit_summary_for_persistence("abcd", &FileOperations::new(), 3).unwrap_err();
        assert!(error.contains("4 bytes"), "{error}");
        assert!(error.contains("3-byte"), "{error}");
    }

    #[test]
    fn fit_applies_the_reducer_limits_to_the_lists() {
        let mut ops = FileOperations::new();
        for index in 0..(MAX_COMPACTION_FILES_PER_CLASS + 40) {
            ops.add_read(&format!("r/{index:04}.rs"));
        }
        for index in 0..(MAX_COMPACTION_FILES_PER_CLASS + 40) {
            ops.add_edited(&format!("m/{index:04}.rs"));
        }
        ops.add_read(&"x".repeat(MAX_COMPACTION_PATH_BYTES + 1));
        let fitted = fit_summary_for_persistence("s", &ops, 1024 * 1024).unwrap();
        assert_eq!(
            fitted.files.read_files.len(),
            MAX_COMPACTION_FILES_PER_CLASS
        );
        assert_eq!(
            fitted.files.modified_files.len(),
            MAX_COMPACTION_FILES_PER_CLASS
        );
        assert!(fitted
            .files
            .read_files
            .iter()
            .all(|path| path.len() <= MAX_COMPACTION_PATH_BYTES));
        // The newest entries survive, the oldest are cut.
        assert!(fitted
            .files
            .modified_files
            .contains(&format!("m/{:04}.rs", MAX_COMPACTION_FILES_PER_CLASS + 39)));
        assert!(!fitted
            .files
            .modified_files
            .contains(&"m/0000.rs".to_owned()));
    }

    #[test]
    fn fit_bounds_total_path_bytes() {
        let mut ops = FileOperations::new();
        let long = "p".repeat(MAX_COMPACTION_PATH_BYTES);
        for index in 0..40 {
            ops.add_read(&format!("{index:02}{}", &long[2..]));
        }
        let fitted = fit_summary_for_persistence("s", &ops, 1024 * 1024).unwrap();
        let total: usize = fitted.files.read_files.iter().map(String::len).sum();
        assert!(total <= MAX_COMPACTION_FILES_BYTES);
        assert_eq!(
            fitted.files.read_files.len(),
            MAX_COMPACTION_FILES_BYTES / MAX_COMPACTION_PATH_BYTES
        );
    }
}
