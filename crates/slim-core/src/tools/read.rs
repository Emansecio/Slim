use std::path::Path;

use super::ToolError;

pub fn read_file(path: impl AsRef<Path>, max_lines: usize) -> Result<String, ToolError> {
    read_file_range(path, 1, max_lines)
}

/// Reads at most `max_lines` lines starting at the 1-based `start_line`.
///
/// When the file has more lines beyond the returned window, a pagination
/// footer is appended so the caller can request the next page with `offset`
/// instead of re-reading the whole file into context.
pub fn read_file_range(
    path: impl AsRef<Path>,
    start_line: usize,
    max_lines: usize,
) -> Result<String, ToolError> {
    let content = std::fs::read_to_string(path.as_ref())?;
    let total_lines = content.lines().count();
    let first = start_line.max(1).saturating_sub(1).min(total_lines);
    let last = first.saturating_add(max_lines).min(total_lines);
    let mut output = String::new();
    for (index, line) in content.lines().skip(first).take(last - first).enumerate() {
        output.push_str(&format!("{}: {line}\n", first + index + 1));
    }
    if last < total_lines {
        output.push_str(&format!(
            "\n[showing lines {}-{} of {total_lines}; pass \"offset\": {} for the next page]",
            first + 1,
            last,
            last + 1
        ));
    }
    Ok(output)
}
