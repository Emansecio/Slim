//! Structured per-session working memory (`SessionScratchpad`).
//!
//! Tracks touched files, verified check commands, pinned user/agent facts, and
//! open TODO items so compaction and context inspection keep high-signal state
//! with minimal token overhead.

use std::fmt::Write as _;

const MAX_ITEMS: usize = 24;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionScratchpad {
    pub touched_files: Vec<String>,
    pub verified_commands: Vec<String>,
    pub pinned_facts: Vec<String>,
    pub open_todos: Vec<String>,
}

impl SessionScratchpad {
    #[allow(dead_code)]
    pub fn pin_fact(&mut self, fact: impl Into<String>) {
        let fact = fact.into().trim().to_owned();
        if fact.is_empty() || self.pinned_facts.contains(&fact) {
            return;
        }
        if self.pinned_facts.len() >= MAX_ITEMS {
            self.pinned_facts.remove(0);
        }
        self.pinned_facts.push(fact);
    }

    #[allow(dead_code)]
    pub fn unpin_fact(&mut self, index: usize) -> bool {
        if index < self.pinned_facts.len() {
            self.pinned_facts.remove(index);
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.touched_files.clear();
        self.verified_commands.clear();
        self.pinned_facts.clear();
        self.open_todos.clear();
    }

    /// Updates scratchpad state from a finished tool invocation.
    pub fn record_tool_outcome(
        &mut self,
        tool_name: &str,
        arguments: &str,
        success: bool,
        _output: &str,
    ) {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(arguments) else {
            return;
        };
        match tool_name {
            "write" | "patch" if success => {
                if let Some(path) = val.get("path").and_then(|v| v.as_str()) {
                    push_unique_bounded(&mut self.touched_files, path.to_owned(), MAX_ITEMS);
                }
            }
            "shell" if success => {
                if let Some(cmd) = val.get("command").and_then(|v| v.as_str()) {
                    let trimmed = cmd.trim();
                    if is_check_command(trimmed) {
                        let compact = if trimmed.len() > 96 {
                            format!("{}...", &trimmed[..93])
                        } else {
                            trimmed.to_owned()
                        };
                        push_unique_bounded(&mut self.verified_commands, compact, 12);
                    }
                }
            }
            "todo" if success => {
                if let Some(todos) = val.get("todos").and_then(|v| v.as_array()) {
                    self.open_todos.clear();
                    for item in todos {
                        let status = item
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending");
                        if status == "completed" {
                            continue;
                        }
                        if let Some(content) = item.get("content").and_then(|v| v.as_str()) {
                            let line = format!("[{status}] {}", content.trim());
                            push_unique_bounded(&mut self.open_todos, line, MAX_ITEMS);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn record_codemode_step(
        &mut self,
        tool_name: &str,
        target_paths: &[std::path::PathBuf],
        cwd: &std::path::Path,
        command: Option<&str>,
    ) {
        match tool_name {
            "write" | "patch" => {
                for abs in target_paths {
                    let rel = crate::tools::display_path(cwd, abs)
                        .to_string_lossy()
                        .replace('\\', "/");
                    push_unique_bounded(&mut self.touched_files, rel, MAX_ITEMS);
                }
            }
            "shell" => {
                if let Some(cmd) = command.map(str::trim).filter(|c| is_check_command(c)) {
                    let compact = if cmd.len() > 96 {
                        format!("{}...", &cmd[..93])
                    } else {
                        cmd.to_owned()
                    };
                    push_unique_bounded(&mut self.verified_commands, compact, 12);
                }
            }
            _ => {}
        }
    }

    pub fn render_compact(&self, max_bytes: usize) -> Option<String> {
        if self.touched_files.is_empty()
            && self.verified_commands.is_empty()
            && self.pinned_facts.is_empty()
            && self.open_todos.is_empty()
        {
            return None;
        }
        let mut out = String::new();
        if !self.pinned_facts.is_empty() {
            out.push_str("Pinned facts:\n");
            for f in &self.pinned_facts {
                let _ = writeln!(out, "- {f}");
            }
        }
        if !self.touched_files.is_empty() {
            let _ = writeln!(out, "Touched files: {}", self.touched_files.join(", "));
        }
        if !self.verified_commands.is_empty() {
            let _ = writeln!(
                out,
                "Verified checks: {}",
                self.verified_commands.join(" ; ")
            );
        }
        if !self.open_todos.is_empty() {
            out.push_str("Open tasks:\n");
            for t in &self.open_todos {
                let _ = writeln!(out, "- {t}");
            }
        }
        if out.len() > max_bytes {
            let mut end = max_bytes;
            while end > 0 && !out.is_char_boundary(end) {
                end -= 1;
            }
            out.truncate(end);
        }
        Some(out.trim_end().to_owned())
    }
}

fn push_unique_bounded(list: &mut Vec<String>, item: String, max: usize) {
    if item.is_empty() {
        return;
    }
    if let Some(pos) = list.iter().position(|existing| existing == &item) {
        list.remove(pos);
    }
    if list.len() >= max {
        list.remove(0);
    }
    list.push(item);
}

fn is_check_command(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    lower.contains("test")
        || lower.contains("check")
        || lower.contains("clippy")
        || lower.contains("lint")
        || lower.contains("pytest")
        || lower.contains("tsc")
        || lower.contains("build")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_files_verified_checks_todos_and_pins() {
        let mut pad = SessionScratchpad::default();
        pad.record_tool_outcome("patch", r#"{"path":"src/main.rs","edits":[]}"#, true, "patched src/main.rs");
        pad.record_tool_outcome("shell", r#"{"command":"cargo test -p slim-core"}"#, true, "exit_code=0");
        pad.record_tool_outcome(
            "todo",
            r#"{"todos":[{"content":"done","status":"completed"},{"content":"next step","status":"in_progress"}]}"#,
            true,
            "ok",
        );
        pad.pin_fact("Always run targeted tests");
        let rendered = pad.render_compact(2048).expect("rendered");
        assert!(rendered.contains("Pinned facts:\n- Always run targeted tests"));
        assert!(rendered.contains("Touched files: src/main.rs"));
        assert!(rendered.contains("Verified checks: cargo test -p slim-core"));
        assert!(rendered.contains("Open tasks:\n- [in_progress] next step"));
        assert!(!rendered.contains("done"));
        assert!(pad.unpin_fact(0));
        assert!(!pad.unpin_fact(0));
        pad.clear();
        assert!(pad.render_compact(2048).is_none());
    }
}
