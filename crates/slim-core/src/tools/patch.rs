use crate::runtime::CancellationToken;
use std::borrow::Cow;
use std::path::Path;

use super::write::{
    ensure_mutation_size, lock_mutations, patch_file_recovery_context, read_existing_file_observed,
    replace_observed_file, resolved_public_path, CURRENT_FILE_RECOVERY_BYTES, UTF8_BOM,
};
use super::{digest_bytes, DependencyObservation, FastStamp, ToolError, ToolExecutionError};

pub(super) const MAX_PATCH_EDITS: usize = 64;

/// Replaces one unique exact occurrence. LF excerpts may
/// match a uniformly CRLF file; that case preserves CRLF in the replacement.
pub fn apply_exact_patch(
    path: impl AsRef<Path>,
    expected: &str,
    replacement: &str,
) -> Result<(), ToolError> {
    let path = resolved_public_path(path.as_ref())?;
    apply_exact_patches_with_content(
        &path,
        &path,
        &[(expected.to_owned(), replacement.to_owned())],
        None,
        &mut || {},
    )
    .map(|_| ())
    .map_err(|failure| failure.error)
}

pub(crate) struct PatchContent {
    pub(crate) displaced_version_preserved: bool,
    pub(crate) summary: String,
    pub(crate) before_digest: String,
    pub(crate) dependency: DependencyObservation,
    pub(crate) stamp: FastStamp,
    pub(crate) bytes_read: u64,
    pub(crate) text: String,
    pub(crate) edits: Option<Vec<crate::codeintel::CodeIntelTextEdit>>,
    /// Display-only line view of the applied edits, in final-file positions.
    pub(crate) hunks: Vec<crate::ToolEditHunk>,
    pub(crate) hunks_truncated: bool,
}

pub(crate) fn apply_exact_patches_with_content(
    path: impl AsRef<Path>,
    // How messages name the file (workspace-relative for the model).
    display: &Path,
    edits: &[(String, String)],
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<PatchContent, ToolExecutionError> {
    let path = path.as_ref();
    let slot = super::write::mutation_lock_slot(path);
    let _mutation_guard = lock_mutations(&slot, cancellation)?;
    let observed =
        read_existing_file_observed(path, cancellation, on_lock_wait).map_err(|failure| {
            if path.exists() {
                return failure;
            }
            ToolError::InvalidInput {
                message: "file does not exist; patch edits existing files. To create it, use write with expected omitted.".into(),
            }
            .into()
        })?;
    let mut updated = observed.content.clone();
    let mut summary = String::new();
    let mut resolved_edits = Some(Vec::with_capacity(edits.len()));
    let mut sync_bytes = 0usize;
    let mut diff = EditDiffBuilder::default();
    for (edit_index, (expected, replacement)) in edits.iter().enumerate() {
        let mut expected = Cow::Borrowed(expected.as_str());
        let mut replacement = Cow::Borrowed(replacement.as_str());
        let mut count = updated.matches(expected.as_ref()).count();
        // Reconcile explicitly supplied LF excerpts with uniform CRLF,
        // before any write, without relaxing content matching or guessing in mixed files.
        let mut normalized_crlf = count == 0
            && expected.contains('\n')
            && !expected.contains('\r')
            && has_only_crlf_newlines(&updated);
        if normalized_crlf {
            expected = Cow::Owned(expected.replace('\n', "\r\n"));
            replacement = Cow::Owned(replacement.replace("\r\n", "\n").replace('\n', "\r\n"));
            count = updated.matches(expected.as_ref()).count();
        } else if count == 1 && contains_bare_lf(&replacement) && has_only_crlf_newlines(&updated) {
            // New LF lines must inherit the file's CRLF style, whatever the
            // excerpt carried, or later LF excerpts would encounter a mixed
            // file introduced by this patch itself.
            replacement = Cow::Owned(replacement.replace("\r\n", "\n").replace('\n', "\r\n"));
            normalized_crlf = true;
        }
        if count != 1 {
            let context = if count == 0 {
                let mut context = if edits.len() == 1 {
                    format!("{}: file unchanged.", display.display())
                } else {
                    format!("{}:", display.display())
                };
                if expected.contains('\u{FFFD}') {
                    context.push_str(" U+FFFD; copy current text verbatim.");
                } else if !expected.is_ascii() {
                    context.push_str(" non-ASCII; copy current text verbatim.");
                }
                context
            } else {
                let mut previous = 0;
                let mut line = 1;
                let mut first_match = None;
                let lines = updated
                    .match_indices(expected.as_ref())
                    .take(8)
                    .map(|(offset, _)| {
                        line += updated[previous..offset]
                            .bytes()
                            .filter(|byte| *byte == b'\n')
                            .count();
                        previous = offset;
                        first_match.get_or_insert((offset, line));
                        line.to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let omitted = if count > 8 {
                    " (first 8 matches shown)"
                } else {
                    ""
                };
                let mut message = if edits.len() == 1 {
                    format!(
                        "{}: file unchanged. Matches at lines {lines}{omitted}.",
                        display.display()
                    )
                } else {
                    format!("{}: Matches at lines {lines}{omitted}.", display.display())
                };
                if let Some((first_match_offset, first_match_line)) = first_match {
                    if let Some(excerpt) =
                        unique_context_excerpt(&updated, expected.as_ref(), first_match_offset)
                    {
                        message.push_str(&format!(
                            "\nExample context only for the first match at line {first_match_line}; choose the intended occurrence explicitly:\n"
                        ));
                        message.push_str(&excerpt);
                    } else {
                        message.push('\n');
                        message.push_str(&patch_file_recovery_context(&observed.content));
                    }
                } else {
                    message.push('\n');
                    message.push_str(&patch_file_recovery_context(&observed.content));
                }
                message
            };
            let context = if count == 0 {
                let mut budget = RecoveryBudget::new();
                let recovery = whitespace_blind_excerpt(&updated, &expected, &mut budget)
                    .or_else(|| whitespace_insensitive_excerpt(&updated, &expected, &mut budget))
                    .or_else(|| nearest_text_excerpt(&updated, &expected, &mut budget))
                    .or_else(|| search_prefix_hint(&expected))
                    .unwrap_or_else(|| patch_file_recovery_context(&observed.content));
                format!("{context}\n{recovery}")
            } else {
                context
            };
            let mut failure = ToolExecutionError::observed(
                ToolError::MatchCount { count },
                vec![observed.dependency.clone()],
                observed.bytes_read,
            );
            failure.context = Some(if edits.len() == 1 {
                context
            } else {
                // Line numbers in `context` count the proposed content, which
                // already holds the earlier edits of this call.
                let applied = match edit_index {
                    0 => String::new(),
                    1 => " Line numbers below count the proposed content (edit 1 applied)."
                        .to_owned(),
                    later => format!(
                        " Line numbers below count the proposed content (edits 1-{later} applied)."
                    ),
                };
                format!(
                    "Edit {} rejected in proposed content; no edits applied.{applied}\n{context}",
                    edit_index + 1
                )
            });
            return Err(failure);
        }
        let updated_len = updated
            .len()
            .saturating_sub(expected.len())
            .saturating_add(replacement.len());
        ensure_mutation_size(updated_len).map_err(|error| {
            ToolExecutionError::observed(
                error,
                vec![observed.dependency.clone()],
                observed.bytes_read,
            )
        })?;
        let offset = updated
            .find(expected.as_ref())
            .expect("one match was counted");
        let start_line = 1 + updated[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        summary = if expected == replacement {
            format!(
                "unchanged {}:{start_line}; expected equals replacement",
                display.display()
            )
        } else {
            format!(
                "patched {}:{start_line}; replaced {} bytes with {} bytes{}",
                display.display(),
                expected.len(),
                replacement.len(),
                if normalized_crlf {
                    "; LF input normalized to CRLF"
                } else {
                    ""
                }
            )
        };
        record_sync_edit(
            &mut resolved_edits,
            &mut sync_bytes,
            &updated,
            offset..offset + expected.len(),
            &replacement,
            (start_line - 1) as u32,
        );
        diff.record(
            &updated,
            offset..offset + expected.len(),
            &replacement,
            start_line,
        );
        updated = updated.replacen(expected.as_ref(), replacement.as_ref(), 1);
    }
    // An edit that spelled the BOM must not drop it from the file.
    if observed.content.starts_with(UTF8_BOM) && !updated.starts_with(UTF8_BOM) {
        updated.insert(0, UTF8_BOM);
        resolved_edits = None;
    }
    let (hunks, hunks_truncated) = diff.finish();
    if edits.len() > 1 {
        summary = format!(
            "patched {}; {} edits applied atomically",
            display.display(),
            edits.len()
        );
    }
    summary = format!(
        "{summary}; bytes={}; sha256={}; do not re-read",
        updated.len(),
        super::write::content_sha256_prefix(updated.as_bytes())
    );
    let before_digest = digest_bytes(b"slim-written-content-v1", observed.content.as_bytes());
    let mut written = replace_observed_file(path, observed, &updated, cancellation)?;
    if let Some(note) = &written.recovery_note {
        summary.push_str("; ");
        summary.push_str(note);
    }
    if let Some(diagnostic) = &written.syntax_diagnostic {
        summary.push('\n');
        summary.push_str(diagnostic);
    }
    let dependency = written
        .dependency
        .take()
        .expect("an observed replacement always has a dependency");
    Ok(PatchContent {
        displaced_version_preserved: written.recovery_note.is_some(),
        summary,
        before_digest,
        dependency,
        stamp: written.after,
        bytes_read: written.bytes_read,
        text: updated,
        edits: resolved_edits,
        hunks,
        hunks_truncated,
    })
}

/// Total changed lines kept for display across one patch call.
const MAX_EDIT_DIFF_LINES: usize = 400;
const MAX_EDIT_DIFF_LINE_CHARS: usize = 1000;

/// Builds display hunks from edits already resolved by patch. Each edit is
/// widened to whole lines, lines shared at its start and end are dropped, and
/// hunks recorded below a later edit move by that edit's line delta, so every
/// `start_line` names a line of the final file.
#[derive(Default)]
struct EditDiffBuilder {
    hunks: Vec<crate::ToolEditHunk>,
    kept_lines: usize,
    truncated: bool,
}

impl EditDiffBuilder {
    fn record(
        &mut self,
        text: &str,
        range: std::ops::Range<usize>,
        replacement: &str,
        start_line: usize,
    ) {
        let block_start = text[..range.start].rfind('\n').map_or(0, |index| index + 1);
        let block_end = if range.end > range.start && text[..range.end].ends_with('\n') {
            range.end
        } else {
            text[range.end..]
                .find('\n')
                .map_or(text.len(), |index| range.end + index)
        };
        let old_block = &text[block_start..block_end];
        let new_block = format!(
            "{}{replacement}{}",
            &text[block_start..range.start],
            &text[range.end..block_end]
        );
        let old_newlines = old_block.matches('\n').count();
        let delta = new_block.matches('\n').count() as isize - old_newlines as isize;
        let old_end = start_line + old_newlines + usize::from(!old_block.ends_with('\n'));
        for hunk in &mut self.hunks {
            if hunk.start_line >= old_end {
                hunk.start_line = hunk.start_line.saturating_add_signed(delta);
            }
        }
        // A block cut before `\n` keeps the `\r` of a CRLF line.
        let lines = |block: &str| {
            if block.is_empty() {
                return Vec::new();
            }
            block
                .split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
                .collect::<Vec<_>>()
        };
        let mut old = lines(old_block);
        let mut new = lines(&new_block);
        // A trailing newline ends the last line; it does not start another.
        for block in [&mut old, &mut new] {
            if block.len() > 1 && block.last().is_some_and(String::is_empty) {
                block.pop();
            }
        }
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let removed = &old[prefix..old.len() - suffix];
        let added = &new[prefix..new.len() - suffix];
        if removed.is_empty() && added.is_empty() {
            return;
        }
        let mut take = |lines: &[String]| {
            let room = MAX_EDIT_DIFF_LINES.saturating_sub(self.kept_lines);
            if lines.len() > room {
                self.truncated = true;
            }
            let kept = lines
                .iter()
                .take(room)
                .map(|line| bounded_diff_line(line))
                .collect::<Vec<_>>();
            self.kept_lines += kept.len();
            kept
        };
        let removed = take(removed);
        let added = take(added);
        if removed.is_empty() && added.is_empty() {
            return;
        }
        self.hunks.push(crate::ToolEditHunk {
            start_line: start_line + prefix,
            removed,
            added,
        });
    }

    fn finish(mut self) -> (Vec<crate::ToolEditHunk>, bool) {
        self.hunks.sort_by_key(|hunk| hunk.start_line);
        (self.hunks, self.truncated)
    }
}

fn bounded_diff_line(line: &str) -> String {
    let mut chars = line.chars();
    let mut kept = chars
        .by_ref()
        .take(MAX_EDIT_DIFF_LINE_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        kept.push('…');
    }
    kept
}

/// Record positions already resolved by patch, without diffing or indexing the
/// full document again. Keep prefixes for the backend's negotiated codec.
fn record_sync_edit(
    resolved: &mut Option<Vec<crate::codeintel::CodeIntelTextEdit>>,
    retained_bytes: &mut usize,
    text: &str,
    range: std::ops::Range<usize>,
    replacement: &str,
    start_line: u32,
) {
    let Some(edits) = resolved else {
        return;
    };
    let start_prefix = text[..range.start].rsplit('\n').next().unwrap_or("");
    let end_prefix = text[..range.end].rsplit('\n').next().unwrap_or("");
    *retained_bytes = retained_bytes
        .saturating_add(start_prefix.len())
        .saturating_add(end_prefix.len())
        .saturating_add(replacement.len());
    // A batch on huge single lines must not multiply retained memory by the
    // edit count. Reuse the mutation bound and fall back to complete changes.
    if *retained_bytes > super::MAX_MUTATING_FILE_BYTES {
        *resolved = None;
        return;
    }
    let end_line = start_line + text[range].bytes().filter(|b| *b == b'\n').count() as u32;
    edits.push(crate::codeintel::CodeIntelTextEdit {
        start: crate::codeintel::CodeIntelEditPosition {
            line: start_line,
            prefix: start_prefix.to_owned(),
        },
        end: crate::codeintel::CodeIntelEditPosition {
            line: end_line,
            prefix: end_prefix.to_owned(),
        },
        text: replacement.to_owned(),
    });
}

const MAX_UNIQUE_CONTEXT_LINES: usize = 8;

fn unique_context_excerpt(text: &str, expected: &str, first_match: usize) -> Option<String> {
    if expected.is_empty() {
        return None;
    }
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index.saturating_add(1));
        }
    }
    let line_at = |offset: usize| match starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let start_line = line_at(first_match);
    let last_matched = first_match.saturating_add(expected.len().saturating_sub(1));
    let end_line = line_at(last_matched.min(text.len().saturating_sub(1)));
    for extra in 0..=MAX_UNIQUE_CONTEXT_LINES {
        let left = start_line.saturating_sub(extra);
        let right = end_line
            .saturating_add(extra)
            .min(starts.len().saturating_sub(1));
        let from = starts[left];
        let to = starts
            .get(right.saturating_add(1))
            .copied()
            .unwrap_or(text.len());
        let excerpt = &text[from..to];
        if !excerpt.is_empty() && text.matches(excerpt).count() == 1 {
            return Some(excerpt.to_owned());
        }
    }
    None
}

/// Recovery for an excerpt that matches nowhere exactly but whose words, line
/// by line, equal those of exactly one run of lines: that run, verbatim.
/// Indentation, tabs and trailing blanks are what excerpts most often get
/// wrong, and the lines to copy are a far smaller answer than the whole file.
fn whitespace_blind_excerpt(
    text: &str,
    expected: &str,
    budget: &mut RecoveryBudget,
) -> Option<String> {
    let wanted = expected.trim().lines().collect::<Vec<_>>();
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    if wanted.is_empty() || wanted.len() > lines.len() {
        return None;
    }
    let mut found = None;
    for start in 0..=lines.len() - wanted.len() {
        let mut equal = true;
        for (line, want) in lines[start..].iter().zip(&wanted) {
            if !budget.spend(line.len().max(want.len()) + 1) {
                return None;
            }
            if !line.split_whitespace().eq(want.split_whitespace()) {
                equal = false;
                break;
            }
        }
        if equal {
            if found.is_some() {
                return None;
            }
            found = Some(start);
        }
    }
    let index = found?;
    closest_text_note(&lines[index..index + wanted.len()], index)
}

/// Work the whole zero-match recovery chain may do, in bytes and lines
/// compared. A huge file with a long near-match must not stall the call; when
/// it runs out, the excerpt functions give up and the call falls back to the
/// current file text.
const RECOVERY_WORK_BUDGET: usize = 6_000_000;

struct RecoveryBudget(usize);

impl RecoveryBudget {
    fn new() -> Self {
        Self(RECOVERY_WORK_BUDGET)
    }

    /// False when `work` does not fit; the budget is then spent for good.
    fn spend(&mut self, work: usize) -> bool {
        match self.0.checked_sub(work) {
            Some(left) => {
                self.0 = left;
                true
            }
            None => {
                self.0 = 0;
                false
            }
        }
    }
}

fn closest_text_note(run: &[&str], first_index: usize) -> Option<String> {
    let excerpt = run.concat();
    let location = match run.len() {
        1 => format!("line {}", first_index + 1),
        count => format!("lines {}-{}", first_index + 1, first_index + count),
    };
    (excerpt.len() <= CURRENT_FILE_RECOVERY_BYTES).then(|| {
        format!(
            "Closest text is at {location} and differs from expected only in whitespace. Retry patch with it copied exactly; do not read again:\n{}",
            excerpt.trim_end_matches(['\r', '\n'])
        )
    })
}

/// Like `whitespace_blind_excerpt`, but for an excerpt that starts or ends
/// inside a line or breaks lines elsewhere: with all whitespace removed from
/// both, it occurs exactly once. The whole lines it touches are returned.
fn whitespace_insensitive_excerpt(
    text: &str,
    expected: &str,
    budget: &mut RecoveryBudget,
) -> Option<String> {
    // Stripping both, then searching for the excerpt and for a second hit.
    if !budget.spend(text.len().saturating_mul(2).saturating_add(expected.len())) {
        return None;
    }
    let wanted = expected
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if wanted.is_empty() {
        return None;
    }
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let mut stripped = String::with_capacity(text.len());
    // Where each file line begins in the stripped text: a hit is located by
    // line, which is all the answer needs.
    let mut line_starts = Vec::with_capacity(lines.len());
    for line in &lines {
        line_starts.push(stripped.len());
        stripped.extend(line.chars().filter(|character| !character.is_whitespace()));
    }
    let start = stripped.find(&wanted)?;
    let after_first_char = start + stripped[start..].chars().next()?.len_utf8();
    if stripped[after_first_char..].contains(&wanted) {
        return None;
    }
    let end = start + wanted.len();
    let first = line_starts.partition_point(|&line_start| line_start <= start) - 1;
    let last = line_starts.partition_point(|&line_start| line_start < end) - 1;
    closest_text_note(&lines[first..=last], first)
}

const NEAREST_TEXT_BEFORE_LINES: usize = 4;
const NEAREST_TEXT_AFTER_LINES: usize = 5;
const NEAREST_TEXT_ANCHOR_CANDIDATES: usize = 8;

/// Last resort before dumping the file: the longest line of `expected` that
/// occurs verbatim exactly once in the file locates where the excerpt was
/// meant to be. Its surroundings are offered as the nearest text, not as the
/// target.
fn nearest_text_excerpt(text: &str, expected: &str, budget: &mut RecoveryBudget) -> Option<String> {
    let mut candidates = expected
        .lines()
        .map(str::trim)
        .filter(|line| line.chars().filter(|c| c.is_alphanumeric()).count() >= 4)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|line| std::cmp::Reverse(line.len()));
    let anchor = candidates
        .into_iter()
        .take(NEAREST_TEXT_ANCHOR_CANDIDATES)
        .find(|line| budget.spend(text.len()) && text.matches(line).count() == 1)?;
    // Locating the anchor and its line, and splitting the lines.
    if !budget.spend(text.len().saturating_mul(2)) {
        return None;
    }
    let offset = text.find(anchor)?;
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let index = text[..offset].bytes().filter(|byte| *byte == b'\n').count();
    let first = index.saturating_sub(NEAREST_TEXT_BEFORE_LINES);
    let last = (index + NEAREST_TEXT_AFTER_LINES).min(lines.len().saturating_sub(1));
    let excerpt = lines[first..=last].concat();
    (excerpt.len() <= CURRENT_FILE_RECOVERY_BYTES).then(|| {
        format!(
            "Nearest text is around line {}, where one line of expected occurs exactly once (lines {}-{}); expected matched nowhere exactly, so this is not a guaranteed target. Retry patch with an excerpt copied exactly from it; do not read again:\n{}",
            index + 1,
            first + 1,
            last + 1,
            excerpt.trim_end_matches(['\r', '\n'])
        )
    })
}

/// Search output marks lines `N: ` (hit) or `N- ` (context); that marker is
/// not file text, and copying it along is the commonest reason an excerpt
/// matches nowhere.
fn search_prefix_hint(expected: &str) -> Option<String> {
    let has_prefix = |line: &str| {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        let rest = &line[digits..];
        digits > 0
            && (rest == ":" || rest == "-" || rest.starts_with(": ") || rest.starts_with("- "))
    };
    let mut lines = expected
        .lines()
        .filter(|line| !line.trim().is_empty())
        .peekable();
    lines.peek()?;
    lines.all(has_prefix).then(|| {
        "Every line of expected starts with a search marker (`N: ` or `N- `); those markers are not file text. Remove them, copy only the text after each, and retry patch.".to_owned()
    })
}

fn contains_bare_lf(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
}

pub(super) fn has_only_crlf_newlines(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.contains("\r\n")
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte != b'\n' || (index > 0 && bytes[index - 1] == b'\r'))
}

#[cfg(test)]
mod diff_tests {
    use super::apply_exact_patches_with_content;
    use crate::ToolEditHunk;
    use std::path::{Path, PathBuf};

    fn patched(
        name: &str,
        body: &str,
        edits: &[(&str, &str)],
    ) -> (Vec<ToolEditHunk>, bool, String) {
        let dir: PathBuf =
            std::env::temp_dir().join(format!("slim-patch-diff-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("file.txt");
        std::fs::write(&path, body).expect("fixture");
        let path = path.canonicalize().expect("canonical");
        let edits = edits
            .iter()
            .map(|(expected, replacement)| ((*expected).to_owned(), (*replacement).to_owned()))
            .collect::<Vec<_>>();
        let content = apply_exact_patches_with_content(
            &path,
            Path::new("file.txt"),
            &edits,
            None,
            &mut || {},
        )
        .map_err(|failure| failure.error)
        .expect("patch");
        let text = std::fs::read_to_string(&path).expect("patched");
        let _ = std::fs::remove_dir_all(dir);
        (content.hunks, content.hunks_truncated, text)
    }

    fn hunk(start_line: usize, removed: &[&str], added: &[&str]) -> ToolEditHunk {
        ToolEditHunk {
            start_line,
            removed: removed.iter().map(|line| (*line).to_owned()).collect(),
            added: added.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    #[test]
    fn partial_line_edits_show_whole_changed_lines() {
        let (hunks, truncated, _) = patched("partial", "a\nlet x = 1;\nc\n", &[("1;", "2;")]);
        assert_eq!(hunks, vec![hunk(2, &["let x = 1;"], &["let x = 2;"])]);
        assert!(!truncated);
    }

    #[test]
    fn earlier_edits_shift_later_hunks_to_final_file_lines() {
        let (hunks, _, text) = patched(
            "shift",
            "l1\nl2\nl3\nl4\nl5\nl6\n",
            &[("l5", "l5\nx\ny"), ("l2", "L2\nz")],
        );
        assert_eq!(text, "l1\nL2\nz\nl3\nl4\nl5\nx\ny\nl6\n");
        assert_eq!(
            hunks,
            vec![hunk(2, &["l2"], &["L2", "z"]), hunk(7, &[], &["x", "y"])]
        );
    }

    #[test]
    fn crlf_lines_are_shown_without_line_endings() {
        let (hunks, _, text) = patched("crlf", "a\r\nb\r\nc\r\n", &[("b\r\n", "B\r\nB2\r\n")]);
        assert_eq!(text, "a\r\nB\r\nB2\r\nc\r\n");
        assert_eq!(hunks, vec![hunk(2, &["b"], &["B", "B2"])]);
    }

    #[test]
    fn mid_line_crlf_edits_and_deleted_lines_carry_no_line_endings() {
        let (hunks, _, text) = patched("crlf-mid", "a\r\nb = 1,\r\nc\r\n", &[("1,", "1")]);
        assert_eq!(text, "a\r\nb = 1\r\nc\r\n");
        assert_eq!(hunks, vec![hunk(2, &["b = 1,"], &["b = 1"])]);
        let (hunks, _, _) = patched("delete", "a\nb\nc\n", &[("b\n", "")]);
        assert_eq!(hunks, vec![hunk(2, &["b"], &[])]);
    }

    #[test]
    fn unchanged_edits_have_no_hunk_and_large_edits_are_bounded() {
        let (hunks, _, _) = patched("same", "a\nb\n", &[("b", "b")]);
        assert!(hunks.is_empty());
        let many = (0..500)
            .map(|index| format!("n{index}\n"))
            .collect::<String>();
        let (hunks, truncated, _) = patched("many", "a\nb\n", &[("b\n", &many)]);
        assert!(truncated);
        let kept: usize = hunks
            .iter()
            .map(|hunk| hunk.removed.len() + hunk.added.len())
            .sum();
        assert_eq!(kept, super::MAX_EDIT_DIFF_LINES);
    }
}
