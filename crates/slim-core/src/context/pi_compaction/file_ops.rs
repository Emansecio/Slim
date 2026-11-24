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

    /// Extracts file operations across a full conversation span, verifying
    /// tool outcomes so failed writes/patches do not pollute `<modified-files>`,
    /// and also capturing file mutations reported inside `codemode` runs.
    pub fn extract_from_history(&mut self, messages: &[ProviderMessage]) {
        let mut tool_outputs: HashMap<&str, &str> = HashMap::new();
        for message in messages {
            if message.role == "tool" {
                if let Some(id) = message.tool_call_id.as_deref() {
                    let body = message
                        .recorded_content
                        .as_deref()
                        .unwrap_or(&message.content);
                    tool_outputs.insert(id, body);
                }
            }
        }
        for message in messages {
            if message.role != "assistant" {
                continue;
            }
            for call in &message.tool_calls {
                let output = tool_outputs.get(call.id.as_str()).copied();
                if matches!(call.name.as_str(), "codemode" | "batch_code") {
                    let failed = output.is_some_and(|out| {
                        out.starts_with("codemode failed after ")
                            || out.starts_with("batch_code failed after ")
                            || out.starts_with("codemode is not available ")
                    });
                    if !failed {
                        if let Some(out) = output {
                            self.extract_codemode_output(out);
                        }
                        if let Ok(serde_json::Value::Object(args)) =
                            serde_json::from_str::<serde_json::Value>(&call.arguments)
                        {
                            if let Some(code) = args.get("code").and_then(serde_json::Value::as_str) {
                                self.extract_codemode_code_ops(code);
                            }
                        }
                    }
                    continue;
                }
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
                let failed = output.is_some_and(tool_output_looks_failed);
                match call.name.as_str() {
                    "read" if !failed => self.add_read(path),
                    "write" if !failed => self.add_written(path),
                    "patch" if !failed => self.add_edited(path),
                    _ => {}
                }
            }
        }
    }

    fn extract_codemode_output(&mut self, output: &str) {
        for line in output.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("written ") {
                let path = rest.split(';').next().unwrap_or("").trim();
                if !path.is_empty() && !path.contains(' ') {
                    self.add_written(path);
                }
            } else if let Some(rest) = trimmed.strip_prefix("patched ") {
                let path = rest.split(';').next().unwrap_or("").trim();
                if !path.is_empty() && !path.contains(' ') {
                    self.add_edited(path);
                }
            }
        }
    }

    fn extract_codemode_code_ops(&mut self, code: &str) {
        #[derive(Clone, Copy)]
        enum Kind {
            Read,
            ReadMany,
            Write,
            Edit,
        }
        let needles = [
            ("slim.read(", Kind::Read),
            ("slim.readMany(", Kind::ReadMany),
            ("slim.write(", Kind::Write),
            ("slim.patch(", Kind::Edit),
            ("slim.patchMany(", Kind::Edit),
        ];
        for (needle, kind) in needles {
            let mut idx = 0;
            while let Some(pos) = code[idx..].find(needle) {
                let after = &code[idx + pos + needle.len()..];
                let window = &after[..after.floor_char_boundary(512)];
                match kind {
                    Kind::ReadMany => {
                        let scan = window.find(']').map_or(window, |end| &window[..end]);
                        for lit in scan_quoted_literals(scan) {
                            if looks_like_file_path(lit) {
                                self.add_read(lit);
                            }
                        }
                    }
                    Kind::Read | Kind::Write | Kind::Edit => {
                        for lit in scan_path_field_literals(window) {
                            match kind {
                                Kind::Read => self.add_read(lit),
                                Kind::Write => self.add_written(lit),
                                Kind::Edit => self.add_edited(lit),
                                Kind::ReadMany => {}
                            }
                        }
                    }
                }
                idx += pos + needle.len();
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

fn looks_like_file_path(cand: &str) -> bool {
    !cand.is_empty()
        && cand.len() <= 240
        && !cand.contains(['\n', '\r', ' '])
        && (cand.contains('/') || cand.contains('.'))
}

fn take_quoted_literal(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' || bytes[i] == b'\n' {
            return None;
        }
        if bytes[i] == quote {
            return Some((&s[1..i], &s[i + 1..]));
        }
        i += 1;
    }
    None
}

fn scan_quoted_literals(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some((lit, rest)) = take_quoted_literal(&s[i..]) {
            out.push(lit);
            i = s.len() - rest.len();
        } else {
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

fn scan_path_field_literals(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut idx = 0usize;
    while let Some(pos) = s[idx..].find("path") {
        let key_start = idx + pos;
        let prev_ok = key_start == 0
            || !bytes[key_start - 1].is_ascii_alphanumeric() && bytes[key_start - 1] != b'_';
        let mut p = key_start + 4;
        if bytes.get(p).is_some_and(|b| matches!(b, b'\'' | b'"')) {
            p += 1;
        }
        while p < bytes.len() && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        if prev_ok && bytes.get(p) == Some(&b':') {
            p += 1;
            while p < bytes.len() && bytes[p].is_ascii_whitespace() {
                p += 1;
            }
            if let Some((lit, rest)) = take_quoted_literal(&s[p..]) {
                if looks_like_file_path(lit) {
                    out.push(lit);
                }
                idx = s.len() - rest.len();
                continue;
            }
        }
        idx = key_start + 4;
    }
    out
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

fn tool_output_looks_failed(output: &str) -> bool {
    output.starts_with("error:")
        || output.starts_with("Error:")
        || output.contains("Current file is below")
        || output.contains("Current file edges are below")
        || output.contains("Suggested unique expected:")
        || output.contains("Closest text is at line")
        || output.contains("Nearest text is around line")
        || output.contains("Example context only for the first match at line ")
}

/// Strips trailing/embedded `<read-files>` and `<modified-files>` XML blocks
/// from a prior summary before feeding it back as `<previous-summary>`, so the
/// summarizer does not echo duplicated file blocks.
pub fn strip_file_operation_blocks(summary: &str) -> String {
    let mut out = summary.to_owned();
    for (open, close) in [
        ("<read-files>", "</read-files>"),
        ("<modified-files>", "</modified-files>"),
    ] {
        while let Some(start) = out.find(open) {
            let Some(rel_end) = out[start..].find(close) else {
                break;
            };
            let end = start + rel_end + close.len();
            out.replace_range(start..end, "");
        }
    }
    out.trim().to_owned()
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

    #[test]
    fn extract_from_history_captures_codemode_code_paths_even_with_json_summary_output() {
        let mut ops = FileOperations::new();
        let code = r#"
            const [a, b] = await Promise.all([
                slim.read({ path: 'src/lib.rs' }),
                slim.readMany(['src/alpha.rs', 'src/beta.rs']),
            ]);
            await slim.write({ path: 'src/new.rs', content: 'x', expected: null });
            await slim.patch({ path: 'src/lib.rs', edits: [] });
            return { ok: true };
        "#;
        let call = ProviderToolCall {
            id: "cm-1".into(),
            name: "codemode".into(),
            arguments: serde_json::json!({ "code": code }).to_string(),
        };
        let messages = vec![
            ProviderMessage::assistant("", vec![call]),
            ProviderMessage::tool("codemode", "cm-1", r#"{"ok":true}"#),
        ];
        ops.extract_from_history(&messages);
        let lists = compute_file_lists(&ops);
        assert_eq!(lists.read_files, vec!["src/alpha.rs", "src/beta.rs"]);
        assert_eq!(lists.modified_files, vec!["src/lib.rs", "src/new.rs"]);
    }
}
