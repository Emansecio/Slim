use std::path::Path;

use super::{write_file, FilePrecondition, ToolError};

pub fn apply_exact_patch(
    path: impl AsRef<Path>,
    expected: &str,
    replacement: &str,
) -> Result<(), ToolError> {
    let path = path.as_ref();
    let current = std::fs::read_to_string(path)?;
    let count = current.matches(expected).count();
    if count != 1 {
        return Err(ToolError::MatchCount { count });
    }
    let updated = current.replacen(expected, replacement, 1);
    write_file(path, &updated, Some(FilePrecondition::ExactText(current)))
}
