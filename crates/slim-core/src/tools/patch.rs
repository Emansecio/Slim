use crate::runtime::CancellationToken;
use std::borrow::Cow;
use std::path::Path;

use super::write::{
    ensure_mutation_size, lock_mutations, patch_file_recovery_context, read_existing_file_observed,
    replace_observed_file, resolved_public_path,
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
        path,
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
}

pub(crate) fn apply_exact_patches_with_content(
    path: impl AsRef<Path>,
    edits: &[(String, String)],
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<PatchContent, ToolExecutionError> {
    let path = path.as_ref();
    let slot = super::write::mutation_lock_slot(path);
    let _mutation_guard = lock_mutations(&slot, cancellation)?;
    let observed = read_existing_file_observed(path, cancellation, on_lock_wait)?;
    let mut updated = observed.content.clone();
    let mut summary = String::new();
    let mut resolved_edits = Some(Vec::with_capacity(edits.len()));
    let mut sync_bytes = 0usize;
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
        } else if count == 1
            && !expected.contains(['\r', '\n'])
            && replacement.contains('\n')
            && !replacement.contains('\r')
            && has_only_crlf_newlines(&updated)
        {
            // A one-line match carries no newline convention. New LF lines
            // must inherit the file's CRLF style, or later LF excerpts would
            // encounter a mixed file introduced by this patch itself.
            replacement = Cow::Owned(replacement.replace('\n', "\r\n"));
            normalized_crlf = true;
        }
        if count != 1 {
            let context = if count == 0 {
                let mut context = format!("{}: file unchanged. Use a unique exact excerpt, including its whitespace and line endings.", path.display());
                if expected.contains('\u{FFFD}') {
                    context.push_str(" The excerpt contains U+FFFD replacement characters: non-ASCII text was corrupted before reaching patch. Read the file and copy the bytes verbatim.");
                } else if !expected.is_ascii() {
                    context.push_str(" The excerpt contains non-ASCII text; it must match the file byte-for-byte—copy it verbatim from a read or search hit.");
                }
                context
            } else {
                let mut previous = 0;
                let mut line = 1;
                let lines = updated
                    .match_indices(expected.as_ref())
                    .take(8)
                    .map(|(offset, _)| {
                        line += updated[previous..offset]
                            .bytes()
                            .filter(|byte| *byte == b'\n')
                            .count();
                        previous = offset;
                        line.to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let omitted = if count > 8 {
                    " (first 8 matches shown)"
                } else {
                    ""
                };
                let mut message = format!("{}: file unchanged. Matches start at lines {lines}{omitted}; include surrounding unchanged text in expected to select one occurrence.", path.display());
                if let Some(excerpt) = suggested_unique_excerpt(&updated, expected.as_ref()) {
                    message.push_str("\nSuggested unique expected:\n");
                    message.push_str(&excerpt);
                } else {
                    message.push('\n');
                    message.push_str(&patch_file_recovery_context(&observed.content));
                }
                message
            };
            let context = if count == 0 {
                format!(
                    "{context}\n{}",
                    patch_file_recovery_context(&observed.content)
                )
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
                format!("Edit {} rejected in proposed content after earlier edits; entire batch left the original file unchanged. {context}", edit_index + 1)
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
                path.display()
            )
        } else {
            format!(
                "patched {}:{start_line}; replaced {} bytes with {} bytes{}",
                path.display(),
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
        updated = updated.replacen(expected.as_ref(), replacement.as_ref(), 1);
    }
    if edits.len() > 1 {
        summary = format!(
            "patched {}; {} edits applied atomically",
            path.display(),
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
    })
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

fn suggested_unique_excerpt(text: &str, expected: &str) -> Option<String> {
    if expected.is_empty() {
        return None;
    }
    let first = text.find(expected)?;
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
    let start_line = line_at(first);
    let last_matched = first.saturating_add(expected.len().saturating_sub(1));
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

pub(super) fn has_only_crlf_newlines(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.contains("\r\n")
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte != b'\n' || (index > 0 && bytes[index - 1] == b'\r'))
}
