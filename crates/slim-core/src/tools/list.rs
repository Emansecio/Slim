use std::path::{Path, PathBuf};

use super::ToolError;

pub fn list_directory(path: impl AsRef<Path>) -> Result<Vec<PathBuf>, ToolError> {
    let mut entries = std::fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}
