use std::path::Path;

use super::{add_memory_entry, MemoryKind, MemorySource, MAX_ENTRY_CHARS};
use crate::runtime::scratchpad::SessionScratchpad;

fn looks_secret_bearing(cmd: &str) -> bool {
    let upper = cmd.to_ascii_uppercase();
    const SENSITIVE_MARKERS: &[&str] = &[
        "TOKEN=",
        "API_KEY=",
        "SECRET=",
        "PASSWORD=",
        "BEARER ",
        "PRIVATE_KEY=",
        "AUTHORIZATION:",
    ];
    SENSITIVE_MARKERS
        .iter()
        .any(|marker| upper.contains(marker))
}

/// Persists deterministic, zero-LLM project learnings from the completed
/// session's scratchpad. Currently extracts non-secret verified single-line
/// commands as `MemoryKind::Command` with `MemorySource::Auto`.
pub fn persist_scratchpad_learnings(cwd: &Path, scratchpad: &SessionScratchpad) {
    if !cwd.is_dir() {
        return;
    }
    for cmd in &scratchpad.verified_commands {
        let trimmed = cmd.trim();
        if trimmed.is_empty()
            || trimmed.contains('\n')
            || trimmed.contains('\r')
            || trimmed.chars().count() > MAX_ENTRY_CHARS
            || looks_secret_bearing(trimmed)
        {
            continue;
        }
        let _ = add_memory_entry(cwd, MemoryKind::Command, trimmed, MemorySource::Auto);
    }
}
