use crate::runtime::CancellationToken;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::write::{
    ensure_mutation_size, lock_mutations, patch_file_recovery_context, read_existing_file_observed,
    replace_observed_file, resolved_public_path, ObservedText, CURRENT_FILE_RECOVERY_BYTES,
    UTF8_BOM,
};
use super::{
    path_identity, digest_bytes, DependencyObservation, FastStamp, ToolError, ToolExecutionError,
};

pub(super) const MAX_PATCH_EDITS: usize = 64;

/// The edit keeps every character outside numeric literals and hex colours,
/// and changes at least one of them: a retune, not a structural change.
pub(super) fn tuning_only_edit(expected: &str, replacement: &str) -> bool {
    let (before, old) = numeric_skeleton(expected);
    let (after, new) = numeric_skeleton(replacement);
    before == after && old != new
}

/// `text` with each numeric literal replaced by a NUL marker, and the
/// literals in order. A literal starts at a digit, `.digit` or `#hex` that
/// does not continue an identifier, and spans suffixes (`0xFF`, `1e-3`,
/// `10px`, `1.5f`).
fn numeric_skeleton(text: &str) -> (String, Vec<&str>) {
    fn word(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }
    let bytes = text.as_bytes();
    let digit_at = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_digit);
    let mut skeleton = String::with_capacity(text.len());
    let mut literals = Vec::new();
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        let starts = (index == 0 || !word(bytes[index - 1]))
            && match bytes[index] {
                b'0'..=b'9' => true,
                b'.' => digit_at(index + 1),
                b'#' => bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit),
                _ => false,
            };
        if !starts {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while let Some(&byte) = bytes.get(end) {
            let exponent_sign = matches!(byte, b'+' | b'-')
                && matches!(bytes[end - 1], b'e' | b'E')
                && bytes[index] != b'#'
                && digit_at(end + 1);
            if word(byte) || (byte == b'.' && digit_at(end + 1)) || exponent_sign {
                end += 1;
            } else {
                break;
            }
        }
        skeleton.push_str(&text[copied..index]);
        skeleton.push('\0');
        literals.push(&text[index..end]);
        copied = end;
        index = end;
    }
    skeleton.push_str(&text[copied..]);
    (skeleton, literals)
}

/// Whether `needle` occurs in `text` zero, once or more than once (`seen`, saturated
/// at 2), and where it first occurs. Stops after two matches, so the common
/// unique-match path never scans the rest of the file.
struct Matches {
    seen: usize,
    first: Option<usize>,
}

fn first_two_matches(text: &str, needle: &str) -> Matches {
    let mut offsets = text.match_indices(needle).map(|(offset, _)| offset);
    let first = offsets.next();
    let second = first.and_then(|_| offsets.next());
    Matches {
        seen: usize::from(first.is_some()) + usize::from(second.is_some()),
        first,
    }
}

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
        &[(expected.to_owned(), replacement.to_owned(), false)],
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
    /// SHA-256 of `text`, the bytes now on disk.
    pub(crate) written_digest: [u8; 32],
    pub(crate) text: String,
    pub(crate) original_content: Option<String>,
    pub(crate) edits: Option<Vec<crate::codeintel::CodeIntelTextEdit>>,
    /// Display-only line view of the applied edits, in final-file positions.
    pub(crate) hunks: Vec<crate::ToolEditHunk>,
    pub(crate) hunks_truncated: bool,
    /// Some edits of a single-file batch were rejected; the others are on disk.
    pub(crate) partial: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToleranceKind {
    Exact,
    LineNumberPrefixes,
    Indentation,
}

pub(crate) struct MultiFilePatchOutcome {
    pub(crate) path: PathBuf,
    pub(crate) content: PatchContent,
}

struct StagedFilePatch {
    path: PathBuf,
    observed: ObservedText,
    updated: String,
    before_digest: String,
    summary: String,
    resolved_edits: Option<Vec<crate::codeintel::CodeIntelTextEdit>>,
    hunks: Vec<crate::ToolEditHunk>,
    hunks_truncated: bool,
    original_backup: Option<String>,
}

struct InMemoryEditsResult {
    updated: String,
    before_digest: String,
    summary: String,
    resolved_edits: Option<Vec<crate::codeintel::CodeIntelTextEdit>>,
    hunks: Vec<crate::ToolEditHunk>,
    hunks_truncated: bool,
    /// The edits that did not match, when the others were kept.
    rejected: Option<String>,
}

/// One patch edit: expected text, replacement, and whether every match of the
/// expected text is replaced instead of exactly one.
pub(crate) type Edit = (String, String, bool);

pub(crate) fn apply_exact_patches_with_content(
    path: impl AsRef<Path>,
    // How messages name the file (workspace-relative for the model).
    display: &Path,
    edits: &[Edit],
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<PatchContent, ToolExecutionError> {
    apply_exact_patches_with_content_and_original(
        path,
        display,
        edits,
        false,
        cancellation,
        on_lock_wait,
    )
}

pub(crate) fn apply_exact_patches_with_content_and_original(
    path: impl AsRef<Path>,
    display: &Path,
    edits: &[Edit],
    capture_original: bool,
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<PatchContent, ToolExecutionError> {
    let path = path.as_ref();
    let slot = super::write::mutation_lock_slot(path);
    let _mutation_guard = lock_mutations(&slot, cancellation)?;
    let observed = read_patch_observed(path, None, cancellation, on_lock_wait)?;
    let original_backup = capture_original.then(|| observed.content.clone());
    let applied = apply_edits_in_memory(display, &observed, edits, true)?;
    finalize_patch_write(path, observed, applied, original_backup, cancellation)
}

pub(crate) fn apply_atomic_multi_file_patches(
    targets: &[(PathBuf, PathBuf, Vec<Edit>)],
    capture_originals: bool,
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<Vec<MultiFilePatchOutcome>, ToolExecutionError> {
    if targets.is_empty() {
        return Err(ToolError::InvalidInput {
            message: "patch requires at least one target file".into(),
        }
        .into());
    }
    let mut order: Vec<usize> = (0..targets.len()).collect();
    order.sort_by_key(|&idx| path_identity(&targets[idx].0));
    let slots = order
        .iter()
        .map(|&idx| super::write::mutation_lock_slot(&targets[idx].0))
        .collect::<Vec<_>>();
    let mut _guards = Vec::with_capacity(slots.len());
    for slot in &slots {
        _guards.push(lock_mutations(slot, cancellation)?);
    }

    // Acquire OS file locks in canonical path order to stay deadlock-free.
    let mut observed_by_idx: Vec<Option<ObservedText>> = (0..targets.len()).map(|_| None).collect();
    for &idx in &order {
        let (path, display, _) = &targets[idx];
        let observed = read_patch_observed(path, Some(display), cancellation, on_lock_wait)?;
        observed_by_idx[idx] = Some(observed);
    }

    // Apply all edits purely in memory before touching disk.
    let mut staged = Vec::with_capacity(targets.len());
    for (idx, (path, display, edits)) in targets.iter().enumerate() {
        let observed = observed_by_idx[idx].take().expect("observed present");
        let original_backup = Some(observed.content.clone());
        let applied = apply_edits_in_memory(display, &observed, edits, false)?;
        staged.push(StagedFilePatch {
            path: path.clone(),
            observed,
            updated: applied.updated,
            before_digest: applied.before_digest,
            summary: applied.summary,
            resolved_edits: applied.resolved_edits,
            hunks: applied.hunks,
            hunks_truncated: applied.hunks_truncated,
            original_backup,
        });
    }

    // All files validated in memory. Commit to disk, tracking backups in case a publish fails mid-way.
    let mut committed: Vec<(PathBuf, String)> = Vec::with_capacity(staged.len());
    let mut outcomes = Vec::with_capacity(staged.len());
    for item in staged {
        let StagedFilePatch {
            path,
            observed,
            updated,
            before_digest,
            summary,
            resolved_edits,
            hunks,
            hunks_truncated,
            original_backup,
        } = item;
        let orig_for_publish_rollback = original_backup.clone().expect("backup recorded");
        let applied = InMemoryEditsResult {
            updated,
            before_digest,
            summary,
            resolved_edits,
            hunks,
            hunks_truncated,
            rejected: None,
        };
        match finalize_patch_write(
            &path,
            observed,
            applied,
            capture_originals.then(|| orig_for_publish_rollback.clone()),
            cancellation,
        ) {
            Ok(content) => {
                committed.push((path.clone(), orig_for_publish_rollback));
                outcomes.push(MultiFilePatchOutcome { path, content });
            }
            Err(mut failure) => {
                for (prev_path, prev_original) in committed.into_iter().rev() {
                    if restore_file_under_lock(&prev_path, &prev_original).is_err() {
                        failure.effects_uncertain = true;
                    }
                }
                return Err(failure);
            }
        }
    }
    Ok(outcomes)
}

pub(crate) fn restore_patched_files_atomic(
    snapshots: &[(PathBuf, String)],
) -> Result<(), String> {
    let mut order: Vec<usize> = (0..snapshots.len()).collect();
    order.sort_by_key(|&idx| path_identity(&snapshots[idx].0));
    let slots = order
        .iter()
        .map(|&idx| super::write::mutation_lock_slot(&snapshots[idx].0))
        .collect::<Vec<_>>();
    let mut _guards = Vec::with_capacity(slots.len());
    for slot in &slots {
        _guards.push(
            lock_mutations(slot, None)
                .map_err(|err| format!("rollback lock failed: {}", super::tool_error_message(err)))?,
        );
    }
    for &idx in &order {
        let (path, original) = &snapshots[idx];
        restore_file_under_lock(path, original)?;
    }
    Ok(())
}

fn restore_file_under_lock(path: &Path, original: &str) -> Result<(), String> {
    let observed = read_existing_file_observed(path, None, &mut || {})
        .map_err(|err| format!("{}: {}", path.display(), super::tool_error_message(err.error)))?;
    if observed.content == original {
        return Ok(());
    }
    replace_observed_file(path, observed, original, None)
        .map(|_| ())
        .map_err(|err| format!("{}: {}", path.display(), super::tool_error_message(err.error)))
}

fn read_patch_observed(
    path: &Path,
    display_on_missing: Option<&Path>,
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<ObservedText, ToolExecutionError> {
    read_existing_file_observed(path, cancellation, on_lock_wait).map_err(|failure| {
        if path.exists() {
            return failure;
        }
        let message = match display_on_missing {
            Some(display) => format!(
                "{}: file does not exist; patch edits existing files. To create it, use write with expected omitted.",
                display.display()
            ),
            None => "file does not exist; patch edits existing files. To create it, use write with expected omitted.".into(),
        };
        ToolError::InvalidInput { message }.into()
    })
}

fn finalize_patch_write(
    path: &Path,
    observed: ObservedText,
    applied: InMemoryEditsResult,
    original_content: Option<String>,
    cancellation: Option<&CancellationToken>,
) -> Result<PatchContent, ToolExecutionError> {
    let InMemoryEditsResult {
        updated,
        before_digest,
        mut summary,
        resolved_edits,
        hunks,
        hunks_truncated,
        rejected,
    } = applied;
    let mut written = replace_observed_file(path, observed, &updated, cancellation)?;
    // The receipt already hashed the final content: reuse it for the summary.
    summary = format!(
        "{summary}; bytes={}; sha256={}; do not re-read",
        updated.len(),
        written.written_sha256_12
    );
    if let Some(note) = &written.recovery_note {
        summary.push_str("; ");
        summary.push_str(note);
    }
    if let Some(diagnostic) = &written.syntax_diagnostic {
        summary.push('\n');
        summary.push_str(diagnostic);
    }
    if let Some(rejected) = &rejected {
        summary.push('\n');
        summary.push_str(rejected);
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
        written_digest: written.written_digest,
        text: updated,
        original_content,
        edits: resolved_edits,
        hunks,
        hunks_truncated,
        partial: rejected.is_some(),
    })
}

/// `partial` keeps the edits of a batch that matched and reports the others;
/// without it one rejected edit rejects the batch.
fn apply_edits_in_memory(
    display: &Path,
    observed: &ObservedText,
    edits: &[Edit],
    partial: bool,
) -> Result<InMemoryEditsResult, ToolExecutionError> {
    // Edit number, match count and recovery text of each rejected edit.
    let mut rejected: Vec<(usize, usize, String)> = Vec::new();
    let mut updated = observed.content.clone();
    let before_digest = digest_bytes(b"slim-written-content-v1", observed.content.as_bytes());
    let mut summary = String::new();
    let mut resolved_edits = Some(Vec::with_capacity(edits.len()));
    let mut sync_bytes = 0usize;
    let mut diff = EditDiffBuilder::default();
    let mut used_prefix_strip = false;
    let mut used_indent_norm = false;
    for (edit_index, (expected, replacement, replace_all)) in edits.iter().enumerate() {
        let mut expected = Cow::Borrowed(expected.as_str());
        let mut replacement = Cow::Borrowed(replacement.as_str());
        let mut found = first_two_matches(&updated, expected.as_ref());
        // Reconcile explicitly supplied LF excerpts with uniform CRLF,
        // before any write, without relaxing content matching or guessing in mixed files.
        let mut normalized_crlf = found.seen == 0
            && expected.contains('\n')
            && !expected.contains('\r')
            && has_only_crlf_newlines(&updated);
        if normalized_crlf {
            expected = Cow::Owned(expected.replace('\n', "\r\n"));
            replacement = Cow::Owned(super::write::with_crlf_line_endings(&replacement));
            found = first_two_matches(&updated, expected.as_ref());
        } else if found.seen == 1 && contains_bare_lf(&replacement) && has_only_crlf_newlines(&updated) {
            // New LF lines must inherit the file's CRLF style, whatever the
            // excerpt carried, or later LF excerpts would encounter a mixed
            // file introduced by this patch itself.
            replacement = Cow::Owned(super::write::with_crlf_line_endings(&replacement));
            normalized_crlf = true;
        }

        let mut tolerance = ToleranceKind::Exact;
        let mut match_range: Option<std::ops::Range<usize>> =
            (found.seen == 1).then(|| {
                let offset = found.first.expect("one match was counted");
                offset..offset + expected.len()
            });

        // Stage A: Deterministic strip of copied `N: ` / `N- ` / `N| ` / `N\t` prefixes
        // when stripping yields an exact unique match in the file.
        if found.seen == 0 {
            if let Some(stripped_expected) = strip_line_number_prefixes(expected.as_ref()) {
                let stripped_matches = first_two_matches(&updated, &stripped_expected);
                if stripped_matches.seen == 1 {
                    let offset = stripped_matches.first.expect("unique stripped match");
                    if let Some(stripped_rep) = strip_line_number_prefixes(replacement.as_ref()) {
                        replacement = Cow::Owned(stripped_rep);
                    }
                    if contains_bare_lf(&replacement) && has_only_crlf_newlines(&updated) {
                        replacement = Cow::Owned(super::write::with_crlf_line_endings(&replacement));
                        normalized_crlf = true;
                    }
                    expected = Cow::Owned(stripped_expected);
                    found = stripped_matches;
                    match_range = Some(offset..offset + expected.len());
                    tolerance = ToleranceKind::LineNumberPrefixes;
                    used_prefix_strip = true;
                }
            }
        }

        // Stage B: Strictly unique whole-line indentation / trailing-whitespace fallback
        // (internal whitespace must still match byte-for-byte).
        if found.seen == 0 {
            if let Some((span, reindented_replacement)) =
                find_unique_trimmed_line_match(&updated, expected.as_ref(), replacement.as_ref())
            {
                found = Matches {
                    seen: 1,
                    first: Some(span.start),
                };
                match_range = Some(span);
                replacement = Cow::Owned(reindented_replacement);
                tolerance = ToleranceKind::Indentation;
                used_indent_norm = true;
            }
        }

        // `replace_all` takes every exact match; a unique match needs no flag.
        let replace_every = *replace_all && found.seen >= 2;
        if found.seen != 1 && !replace_every {
            // Only the failure path pays for the full count.
            let count = updated.matches(expected.as_ref()).count();
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
            if partial && edits.len() > 1 {
                rejected.push((edit_index + 1, count, context));
                continue;
            }
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
        // Back to front, so the offsets of the matches still to replace stay valid.
        let ranges: Vec<std::ops::Range<usize>> = if replace_every {
            let mut all: Vec<_> = updated
                .match_indices(expected.as_ref())
                .map(|(offset, matched)| offset..offset + matched.len())
                .collect();
            all.reverse();
            all
        } else {
            vec![match_range.expect("one match was counted")]
        };
        let replaced = ranges.len();
        for range in ranges {
            let matched_len = range.end - range.start;
            let updated_len = updated
                .len()
                .saturating_sub(matched_len)
                .saturating_add(replacement.len());
            ensure_mutation_size(updated_len).map_err(|error| {
                ToolExecutionError::observed(
                    error,
                    vec![observed.dependency.clone()],
                    observed.bytes_read,
                )
            })?;
            let offset = range.start;
            let start_line = 1 + updated[..offset]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count();
            summary = if updated[range.clone()] == *replacement {
                format!(
                    "unchanged {}:{start_line}; expected equals replacement",
                    display.display()
                )
            } else {
                format!(
                    "patched {}:{start_line}; replaced {} bytes with {} bytes{}{}",
                    display.display(),
                    matched_len,
                    replacement.len(),
                    if normalized_crlf {
                        "; LF input normalized to CRLF"
                    } else {
                        ""
                    },
                    match tolerance {
                        ToleranceKind::Exact => "",
                        ToleranceKind::LineNumberPrefixes => "; line-number prefixes stripped",
                        ToleranceKind::Indentation => "; indentation normalized",
                    }
                )
            };
            record_sync_edit(
                &mut resolved_edits,
                &mut sync_bytes,
                &updated,
                range.clone(),
                &replacement,
                (start_line - 1) as u32,
            );
            diff.record(
                &updated,
                range.clone(),
                &replacement,
                start_line,
            );
            updated.replace_range(range, replacement.as_ref());
        }
        if replace_every {
            summary = format!(
                "patched {}; replaced {replaced} matches of the expected text",
                display.display()
            );
        }
    }
    let rejected_note = match rejected.first() {
        None => None,
        Some(&(_, count, _)) => {
            let detail = rejected
                .iter()
                .map(|(index, _, context)| {
                    // The first edit is tried against the file on disk.
                    let tried = if *index > 1 {
                        " Line numbers below count the content as tried."
                    } else {
                        ""
                    };
                    format!("Edit {index} rejected.{tried}\n{context}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            if rejected.len() == edits.len() {
                let mut failure = ToolExecutionError::observed(
                    ToolError::MatchCount { count },
                    vec![observed.dependency.clone()],
                    observed.bytes_read,
                );
                failure.context = Some(format!("No edits applied.\n{detail}"));
                return Err(failure);
            }
            Some(detail)
        }
    };
    // An edit that spelled the BOM must not drop it from the file.
    if observed.content.starts_with(UTF8_BOM) && !updated.starts_with(UTF8_BOM) {
        updated.insert(0, UTF8_BOM);
        resolved_edits = None;
    }
    let (hunks, hunks_truncated) = diff.finish();
    if edits.len() > 1 {
        summary = format!(
            "patched {}; {}{}{}",
            display.display(),
            if rejected.is_empty() {
                format!("{} edits applied atomically", edits.len())
            } else {
                format!("{} of {} edits applied", edits.len() - rejected.len(), edits.len())
            },
            if used_prefix_strip {
                "; line-number prefixes stripped"
            } else {
                ""
            },
            if used_indent_norm {
                "; indentation normalized"
            } else {
                ""
            }
        );
    }
    Ok(InMemoryEditsResult {
        updated,
        before_digest,
        summary,
        resolved_edits,
        hunks,
        hunks_truncated,
        rejected: rejected_note,
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

fn strip_line_number_prefixes(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut stripped_any = false;
    for segment in text.split_inclusive('\n') {
        let (body, ending) = if let Some(body) = segment.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = segment.strip_suffix('\n') {
            (body, "\n")
        } else {
            (segment, "")
        };
        if body.trim().is_empty() {
            out.push_str(body);
            out.push_str(ending);
            continue;
        }
        let ws_len = body.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
        let after_ws = &body[ws_len..];
        let digits = after_ws.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let rest = &after_ws[digits..];
        let content = if let Some(after_tab) = rest.strip_prefix('\t') {
            after_tab
        } else if let Some(sep) = rest.as_bytes().first().copied().filter(|b| matches!(b, b':' | b'|' | b'-')) {
            let after_sep = &rest[1..];
            let _ = sep;
            after_sep.strip_prefix(' ').unwrap_or(after_sep)
        } else {
            return None;
        };
        stripped_any = true;
        out.push_str(content);
        out.push_str(ending);
    }
    stripped_any.then_some(out)
}

fn common_leading_whitespace<'a>(lines: &[&'a str]) -> &'a str {
    let mut non_empty = lines.iter().copied().filter(|line| !line.trim().is_empty());
    let Some(first) = non_empty.next() else {
        return "";
    };
    let first_ws_len = first
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count();
    let mut prefix = &first[..first_ws_len];
    for line in non_empty {
        let common_bytes = prefix
            .bytes()
            .zip(line.bytes())
            .take_while(|(a, b)| a == b && (*a == b' ' || *a == b'\t'))
            .count();
        prefix = &prefix[..common_bytes];
        if prefix.is_empty() {
            break;
        }
    }
    prefix
}

fn reindent_replacement(
    replacement: &str,
    exp_indent: &str,
    file_indent: &str,
    crlf: bool,
) -> String {
    if replacement.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(replacement.len() + 32);
    for segment in replacement.split_inclusive('\n') {
        let (body, ending) = if let Some(body) = segment.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = segment.strip_suffix('\n') {
            (body, "\n")
        } else {
            (segment, "")
        };
        if body.trim().is_empty() {
            out.push_str(body);
        } else if let Some(rest) = body.strip_prefix(exp_indent) {
            out.push_str(file_indent);
            out.push_str(rest);
        } else if exp_indent.is_empty() {
            out.push_str(file_indent);
            out.push_str(body);
        } else {
            out.push_str(body);
        }
        if !ending.is_empty() {
            out.push_str(if crlf { "\r\n" } else { "\n" });
        }
    }
    out
}

fn find_unique_trimmed_line_match(
    updated: &str,
    expected: &str,
    replacement: &str,
) -> Option<(std::ops::Range<usize>, String)> {
    let exp_has_trailing_nl = expected.ends_with('\n');
    let exp_body = expected
        .strip_suffix("\r\n")
        .or_else(|| expected.strip_suffix('\n'))
        .unwrap_or(expected);
    if exp_body.is_empty() {
        return None;
    }
    let exp_lines: Vec<&str> = exp_body
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if exp_lines.is_empty() || !exp_lines.iter().any(|line| !line.trim().is_empty()) {
        return None;
    }
    let exp_trimmed: Vec<&str> = exp_lines.iter().map(|line| line.trim()).collect();

    struct LineSpan<'a> {
        body: &'a str,
        trimmed: &'a str,
        start: usize,
        end_without_nl: usize,
        end_with_nl: usize,
    }

    let mut file_lines = Vec::new();
    let mut offset = 0usize;
    for segment in updated.split_inclusive('\n') {
        let len = segment.len();
        let body = segment
            .strip_suffix("\r\n")
            .or_else(|| segment.strip_suffix('\n'))
            .unwrap_or(segment);
        file_lines.push(LineSpan {
            body,
            trimmed: body.trim(),
            start: offset,
            end_without_nl: offset + body.len(),
            end_with_nl: offset + len,
        });
        offset += len;
    }
    if exp_lines.len() > file_lines.len() {
        return None;
    }

    let mut unique_start: Option<usize> = None;
    for start in 0..=file_lines.len() - exp_lines.len() {
        if exp_has_trailing_nl {
            let last = &file_lines[start + exp_lines.len() - 1];
            if last.end_with_nl == last.end_without_nl {
                continue;
            }
        }
        let matches = file_lines[start..start + exp_lines.len()]
            .iter()
            .zip(&exp_trimmed)
            .all(|(file_line, exp_trim)| file_line.trimmed == *exp_trim);
        if matches {
            if unique_start.is_some() {
                return None;
            }
            unique_start = Some(start);
        }
    }

    let start = unique_start?;
    let matched_slice = &file_lines[start..start + exp_lines.len()];
    let match_start = matched_slice[0].start;
    let last = &matched_slice[matched_slice.len() - 1];
    let match_end = if exp_has_trailing_nl {
        last.end_with_nl
    } else {
        last.end_without_nl
    };

    let matched_bodies: Vec<&str> = matched_slice.iter().map(|line| line.body).collect();
    let exp_indent = common_leading_whitespace(&exp_lines);
    let file_indent = common_leading_whitespace(&matched_bodies);
    let crlf = has_only_crlf_newlines(updated);
    let reindented = reindent_replacement(replacement, exp_indent, file_indent, crlf);
    Some((match_start..match_end, reindented))
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
            .map(|(expected, replacement)| ((*expected).to_owned(), (*replacement).to_owned(), false))
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

#[cfg(test)]
mod tuning_tests {
    use super::tuning_only_edit;

    #[test]
    fn numeric_and_colour_retunes_are_tuning() {
        assert!(tuning_only_edit("k = 0.0085;", "k = 0.0042;"));
        assert!(tuning_only_edit("color: #ff8800;", "color: #ffaa00;"));
        assert!(tuning_only_edit("glow(1e-3, 10px)", "glow(2.5e-3, 12px)"));
        assert!(tuning_only_edit("let n = 0xFF;", "let n = 0x7F;"));
        assert!(tuning_only_edit("w = .5", "w = .75"));
    }

    #[test]
    fn structural_or_identical_edits_are_not_tuning() {
        assert!(!tuning_only_edit("k = 0.5;", "k = 0.5;"));
        assert!(!tuning_only_edit("max(lf, 0)", "abs(lf)"));
        assert!(!tuning_only_edit("vec3(1.0)", "vec4(1.0)"));
        assert!(!tuning_only_edit("x = 1.0.max(y)", "x = 1.0.min(y)"));
        assert!(!tuning_only_edit("a = 1", "a  = 1"));
        assert!(!tuning_only_edit("x - 0.5", "x + 0.5"));
    }
}

#[cfg(test)]
mod tolerant_and_atomic_tests {
    use super::{
        apply_atomic_multi_file_patches, apply_exact_patches_with_content,
        strip_line_number_prefixes,
    };
    use crate::tools::ToolRegistry;
    use crate::OperatingMode;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slim-patch-tol-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn stage_a_strips_read_and_search_line_prefixes() {
        let stripped = strip_line_number_prefixes("10: fn a() {\n11-     let x = 1;\n12|\treturn x;\n13\t}").unwrap();
        assert_eq!(stripped, "fn a() {\n    let x = 1;\n\treturn x;\n}");
        assert!(strip_line_number_prefixes("10: ok\nplain line").is_none());

        let dir = temp_dir("stage-a");
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {\n    let x = 1;\n    return x;\n}\n").unwrap();
        let outcome = apply_exact_patches_with_content(
            &path,
            Path::new("a.rs"),
            &[(
                "2:     let x = 1;\n3-     return x;".into(),
                "    let x = 2;\n    return x;".into(),
                false,
            )],
            None,
            &mut || {},
        )
        .unwrap();
        assert!(outcome.summary.contains("line-number prefixes stripped"), "{}", outcome.summary);
        assert!(outcome.text.contains("let x = 2;"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_all_replaces_every_match_and_plain_edit_still_rejects_duplicates() {
        let dir = temp_dir("replace-all");
        let path = dir.join("r.rs");
        std::fs::write(&path, "let a = old;\nlet b = old;\nlet c = old;\n").unwrap();
        let plain = apply_exact_patches_with_content(
            &path,
            Path::new("r.rs"),
            &[("old".into(), "new".into(), false)],
            None,
            &mut || {},
        );
        assert!(plain.is_err(), "a plain edit still needs a unique match");
        let all = apply_exact_patches_with_content(
            &path,
            Path::new("r.rs"),
            &[("old".into(), "old_new".into(), true)],
            None,
            &mut || {},
        )
        .unwrap();
        assert_eq!(all.text, "let a = old_new;\nlet b = old_new;\nlet c = old_new;\n");
        assert!(all.summary.contains("replaced 3 matches"), "{}", all.summary);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stage_b_normalizes_unique_indentation_and_rejects_ambiguous() {
        let dir = temp_dir("stage-b");
        let path = dir.join("b.rs");
        let original = "fn one() {\n\tlet v = 10;\n\tlet w = 20;\n}\nfn two() {\n\tlet z = 1;\n}\nfn three() {\n\tlet z = 1;\n}\n";
        std::fs::write(&path, original).unwrap();

        let ok = apply_exact_patches_with_content(
            &path,
            Path::new("b.rs"),
            &[(
                "    let v = 10;\n    let w = 20;".into(),
                "    if true {\n        let v = 99;\n    }".into(),
                false,
            )],
            None,
            &mut || {},
        )
        .unwrap();
        assert!(ok.summary.contains("indentation normalized"), "{}", ok.summary);
        assert!(ok.text.contains("\tif true {\n\t    let v = 99;\n\t}"));

        // Two matches for `let z = 1;` -> Stage B must refuse.
        let err = apply_exact_patches_with_content(
            &path,
            Path::new("b.rs"),
            &[("    let z = 1;".into(), "    let z = 2;".into(), false)],
            None,
            &mut || {},
        )
        .err()
        .expect("ambiguous indent match must fail");
        assert!(matches!(err.error, crate::tools::ToolError::MatchCount { count: 0 }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multi_file_patch_is_atomic_and_supports_rollback_on_check_fail() {
        let dir = temp_dir("multi-atomic");
        let f1 = dir.join("one.rs");
        let f2 = dir.join("two.rs");
        std::fs::write(&f1, "pub fn one() -> i32 { 1 }\n").unwrap();
        std::fs::write(&f2, "pub fn two() -> i32 { 2 }\n").unwrap();

        // File 2 fails -> file 1 MUST remain untouched on disk!
        let err = apply_atomic_multi_file_patches(
            &[
                (f1.clone(), PathBuf::from("one.rs"), vec![("1".into(), "10".into(), false)]),
                (f2.clone(), PathBuf::from("two.rs"), vec![("missing".into(), "20".into(), false)]),
            ],
            false,
            None,
            &mut || {},
        )
        .err()
        .expect("missing edit in file 2 should abort entire transaction");
        let _ = err;
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "pub fn one() -> i32 { 1 }\n");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "pub fn two() -> i32 { 2 }\n");

        // Multi-file patch via ToolRegistry with rollback_on_check_fail: true when then_run fails.
        let registry = ToolRegistry::default();
        let res = registry.execute(
            OperatingMode::Auto,
            &dir,
            "patch",
            &json!({
                "path": "one.rs",
                "edits": [{"expected": "{ 1 }", "replacement": "{ 11 }"}],
                "files": [
                    {"path": "two.rs", "edits": [{"expected": "{ 2 }", "replacement": "{ 22 }"}]}
                ],
                "rollback_on_check_fail": true,
                "then_run": {"command": "exit 1"}
            })
            .to_string(),
        );
        assert!(!res.success, "{}", res.output);
        assert!(res.output.contains("[rollback] reverted patched file(s)"), "{}", res.output);
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "pub fn one() -> i32 { 1 }\n");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "pub fn two() -> i32 { 2 }\n");

        // Multi-file patch via standalone `files` without top-level `path`/`edits` succeeds.
        let ok = registry.execute(
            OperatingMode::Auto,
            &dir,
            "patch",
            &json!({
                "files": [
                    {"path": "one.rs", "edits": [{"expected": "{ 1 }", "replacement": "{ 101 }"}]},
                    {"path": "two.rs", "edits": [{"expected": "{ 2 }", "replacement": "{ 202 }"}]}
                ]
            })
            .to_string(),
        );
        assert!(ok.success, "{}", ok.output);
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "pub fn one() -> i32 { 101 }\n");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "pub fn two() -> i32 { 202 }\n");
        let _ = std::fs::remove_dir_all(dir);
    }
}
