use std::path::Path;

use super::{load_project_memory, MemoryEntry, MAX_RENDERED_MEMORY_BYTES};

/// Renders durable project memories into a deterministic, byte-bounded bullet
/// list suitable for appending to the session's initial workspace context so
/// provider prompt caches stay stable across turns.
pub fn render_for_prompt(cwd: &Path) -> Option<String> {
    let mem = load_project_memory(cwd);
    if mem.entries.is_empty() {
        return None;
    }
    let mut entries: Vec<&MemoryEntry> = mem.entries.iter().collect();
    entries.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut out = String::new();
    for entry in entries {
        let single_line: String = entry.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if single_line.is_empty() {
            continue;
        }
        let line = format!("- [{}] {}\n", entry.kind.as_str(), single_line);
        if out.len() + line.len() > MAX_RENDERED_MEMORY_BYTES {
            break;
        }
        out.push_str(&line);
    }

    let trimmed = out.trim_end_matches('\n');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
