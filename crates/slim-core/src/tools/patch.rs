use std::path::Path;

use super::write::{
    ensure_mutation_size, lock_mutations, read_existing_file_observed, replace_observed_file,
    resolved_public_path,
};
use super::{digest_bytes, DependencyObservation, FastStamp, ToolError, ToolExecutionError};

pub fn apply_exact_patch(
    path: impl AsRef<Path>,
    expected: &str,
    replacement: &str,
) -> Result<(), ToolError> {
    let path = resolved_public_path(path.as_ref())?;
    apply_exact_patch_with_content(path, expected, replacement)
        .map(|_| ())
        .map_err(|failure| failure.error)
}

pub(crate) struct PatchContent {
    pub(crate) before_digest: String,
    pub(crate) dependency: DependencyObservation,
    pub(crate) stamp: FastStamp,
    pub(crate) bytes_read: u64,
    pub(crate) text: String,
}

pub(crate) fn apply_exact_patch_with_content(
    path: impl AsRef<Path>,
    expected: &str,
    replacement: &str,
) -> Result<PatchContent, ToolExecutionError> {
    let _mutation_guard = lock_mutations();
    let path = path.as_ref();
    let observed = read_existing_file_observed(path)?;
    let count = observed.content.matches(expected).count();
    if count != 1 {
        return Err(ToolExecutionError::observed(
            ToolError::MatchCount { count },
            vec![observed.dependency],
            observed.bytes_read,
        ));
    }
    let updated_len = observed
        .content
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
    let before_digest = digest_bytes(b"slim-written-content-v1", observed.content.as_bytes());
    let updated = observed.content.replacen(expected, replacement, 1);
    let mut written = replace_observed_file(path, observed, &updated)?;
    let dependency = written
        .dependency
        .take()
        .expect("an observed replacement always has a dependency");
    Ok(PatchContent {
        before_digest,
        dependency,
        stamp: written.after,
        bytes_read: written.bytes_read,
        text: updated,
    })
}
