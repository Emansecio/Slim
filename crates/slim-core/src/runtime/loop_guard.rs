use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct LoopGuard {
    failed_calls: HashSet<String>,
    last_failed_shell: Option<String>,
    last_failed_mutation: Option<String>,
}

fn canonical_arguments(tool: &str, arguments: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.trim().to_string();
    };
    if tool == "shell" {
        if let Some(command) = value.get_mut("command") {
            if let Some(text) = command.as_str() {
                *command = serde_json::Value::String(text.trim().to_string());
            }
        }
    }
    value.to_string()
}

impl LoopGuard {
    pub fn accept(&mut self, tool: &str, arguments: &str, error: &str) -> bool {
        let arguments = canonical_arguments(tool, arguments);
        self.accept_key(tool, &arguments, error)
    }

    pub(crate) fn accept_canonical(&mut self, tool: &str, fingerprint: &str, error: &str) -> bool {
        self.accept_key(tool, fingerprint, error)
    }

    fn accept_key(&mut self, tool: &str, arguments: &str, error: &str) -> bool {
        if tool == "shell" {
            self.last_failed_mutation = None;
            if self.last_failed_shell.as_deref() == Some(arguments) {
                return false;
            }
            self.last_failed_shell = Some(arguments.to_owned());
            true
        } else if matches!(tool, "write" | "patch") {
            self.last_failed_shell = None;
            let key = format!("{tool}\n{arguments}");
            if self.last_failed_mutation.as_ref() == Some(&key) {
                return false;
            }
            self.last_failed_mutation = Some(key);
            true
        } else {
            self.last_failed_shell = None;
            self.last_failed_mutation = None;
            let fingerprint = format!("{tool}\n{arguments}\n{error}");
            self.failed_calls.insert(fingerprint)
        }
    }

    pub fn record_success(&mut self, tool: &str) {
        if matches!(tool, "shell" | "write" | "patch" | "ask_question") {
            self.last_failed_shell = None;
            self.last_failed_mutation = None;
            self.failed_calls.clear();
        }
    }
}
