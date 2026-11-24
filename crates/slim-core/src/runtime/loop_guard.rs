use crate::tools::hash_fields;
use std::collections::HashSet;

const MAX_CODEMODE_SAME_ERROR_STREAK: usize = 3;

/// Detects a model repeating a failed call.
///
/// `shell`, `write` and `patch` are serial: only an immediate identical
/// repetition is blocked (`last_serial`). Any other tool is blocked on a
/// repeated (arguments, error) pair, remembered as a digest. Zero-effect
/// `codemode` failures also stop after repeated identical error lines even
/// if the program text was slightly rewritten.
#[derive(Debug, Default)]
pub struct LoopGuard {
    failed_calls: HashSet<String>,
    /// `tool\narguments` of the previous failed serial call.
    last_serial: Option<String>,
    /// `(normalized_error, count)` of consecutive zero-effect `codemode` failures.
    codemode_error_streak: Option<(String, usize)>,
}

fn codemode_error_signature(error: &str) -> String {
    let body = error
        .strip_prefix("codemode failed after ")
        .or_else(|| error.strip_prefix("batch_code failed after "))
        .and_then(|rest| rest.split_once('\n'))
        .map_or(error, |(_, rest)| rest);
    body.lines().next().unwrap_or(body).trim().to_owned()
}

fn canonical_arguments(tool: &str, arguments: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.trim().to_string();
    };
    if tool == "shell" {
        if let Some(serde_json::Value::String(command)) = value.get_mut("command") {
            *command = command.trim().to_string();
        }
    }
    value.to_string()
}

impl LoopGuard {
    pub fn accept(&mut self, tool: &str, arguments: &str, error: &str) -> bool {
        let arguments = canonical_arguments(tool, arguments);
        self.accept_key(tool, &arguments, error)
    }

    /// Like [`accept`](Self::accept) for arguments that are already canonical,
    /// such as a prepared call fingerprint.
    pub(crate) fn accept_key(&mut self, tool: &str, arguments: &str, error: &str) -> bool {
        if matches!(tool, "shell" | "write" | "patch") {
            self.codemode_error_streak = None;
            let key = format!("{tool}\n{arguments}");
            if self.last_serial.as_deref() == Some(key.as_str()) {
                return false;
            }
            self.last_serial = Some(key);
            true
        } else {
            self.last_serial = None;
            if matches!(tool, "codemode" | "batch_code") {
                let signature = codemode_error_signature(error);
                let count = match &self.codemode_error_streak {
                    Some((prev, count)) if prev == &signature => count + 1,
                    _ => 1,
                };
                self.codemode_error_streak = Some((signature, count));
                if count >= MAX_CODEMODE_SAME_ERROR_STREAK {
                    return false;
                }
            } else {
                self.codemode_error_streak = None;
            }
            self.failed_calls.insert(hash_fields(&[
                tool.as_bytes(),
                arguments.as_bytes(),
                error.as_bytes(),
            ]))
        }
    }

    pub fn record_success(&mut self, tool: &str) {
        if matches!(tool, "codemode" | "batch_code") {
            self.codemode_error_streak = None;
        }
        if matches!(tool, "shell" | "write" | "patch" | "ask_question") {
            self.last_serial = None;
            self.failed_calls.clear();
            self.codemode_error_streak = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LoopGuard;

    #[test]
    fn serial_tools_only_block_immediate_identical_repetition() {
        let mut guard = LoopGuard::default();
        assert!(guard.accept("shell", r#"{"command":"cargo test"}"#, "e1"));
        assert!(guard.accept("write", r#"{"path":"a"}"#, "e"));
        assert!(
            guard.accept("shell", r#"{"command":"cargo test"}"#, "e2"),
            "an intervening mutation reopens the shell command"
        );
        assert!(guard.accept("patch", r#"{"path":"a"}"#, "e"));
        assert!(
            guard.accept("write", r#"{"path":"a"}"#, "e"),
            "the same arguments under another tool are a different call"
        );
        assert!(!guard.accept("write", r#"{"path":"a"}"#, "e"));
        assert!(guard.accept("read", "{}", "missing"));
        assert!(guard.accept("write", r#"{"path":"a"}"#, "e"));
    }

    #[test]
    fn shell_command_whitespace_is_canonical() {
        let mut guard = LoopGuard::default();
        assert!(guard.accept("shell", r#"{"command":" cargo test "}"#, "e"));
        assert!(!guard.accept("shell", r#"{"command":"cargo test"}"#, "e"));
        assert!(guard.accept("shell", r#"{"command":42}"#, "e"));
        assert!(guard.accept("shell", "not json ", "e"));
        assert!(!guard.accept("shell", " not json", "e"));
    }

    #[test]
    fn failed_calls_keep_a_bounded_digest_not_the_error_text() {
        let mut guard = LoopGuard::default();
        let error = "x".repeat(100_000);
        assert!(guard.accept("read", "{}", &error));
        assert!(!guard.accept("read", "{}", &error));
        assert!(guard.accept("read", "{}", "another error"));
        assert!(guard.failed_calls.iter().all(|entry| entry.len() == 64));
    }

    #[test]
    fn codemode_same_error_streak_blocks_on_third_consecutive_failure() {
        let mut guard = LoopGuard::default();
        let err = "codemode failed after 0 tool call(s); stored values and prior tool effects remain, do not replay the cell automatically.\nSyntaxError: unexpected token";
        assert!(guard.accept("codemode", r#"{"code":"a"}"#, err));
        assert!(guard.accept("codemode", r#"{"code":"b"}"#, err));
        assert!(!guard.accept("codemode", r#"{"code":"c"}"#, err));
        guard.record_success("codemode");
        assert!(guard.accept("codemode", r#"{"code":"d"}"#, err));
        let b1 = "batch_code failed after 1 tool call(s); ...\nTypeError: boom";
        let b2 = "batch_code failed after 2 tool call(s); ...\nTypeError: boom";
        let b3 = "batch_code failed after 3 tool call(s); ...\nTypeError: boom";
        assert!(guard.accept("batch_code", r#"{"code":"x"}"#, b1));
        assert!(guard.accept("batch_code", r#"{"code":"y"}"#, b2));
        assert!(!guard.accept("batch_code", r#"{"code":"z"}"#, b3));
    }
}
