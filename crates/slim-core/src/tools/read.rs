use std::io::{BufRead, BufReader};
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
    let file = std::fs::File::open(path.as_ref())?;
    let mut reader = BufReader::new(file);
    let requested_first = start_line.max(1).saturating_sub(1);
    let requested_last = requested_first.saturating_add(max_lines);
    let mut output = String::new();
    let mut line = String::new();
    let mut total_lines = 0;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        if (requested_first..requested_last).contains(&total_lines) {
            output.push_str(&format!("{}: {line}\n", total_lines + 1));
        }
        total_lines += 1;
    }

    let first = requested_first.min(total_lines);
    let last = requested_last.min(total_lines);
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
