use super::*;

#[derive(Default)]
pub(super) struct SensitiveValues(pub(super) Vec<String>);

pub(super) fn redact_task_value(value: &mut Value, sensitive_values: &[String]) {
    match value {
        Value::String(text) => *text = redact_values(sensitive_values, text),
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| redact_task_value(value, sensitive_values)),
        Value::Object(values) => values
            .values_mut()
            .for_each(|value| redact_task_value(value, sensitive_values)),
        _ => {}
    }
}

pub(crate) fn redact_values(sensitive_values: &[String], input: &str) -> String {
    // An empty value matches everywhere and would splice the marker between
    // every character.
    let active = || sensitive_values.iter().filter(|value| !value.is_empty());
    if !active().any(|value| input.contains(value.as_str())) {
        return input.to_owned();
    }
    active().fold(input.to_owned(), |redacted, value| {
        if redacted.contains(value.as_str()) {
            redacted.replace(value, "[REDACTED]")
        } else {
            redacted
        }
    })
}

impl Runtime {
    /// Registers an exact value that must not cross a runtime boundary.
    ///
    /// Runtime diagnostics intentionally omit the storage so they can never
    /// print the secret itself.
    pub fn register_sensitive_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() && !self.sensitive_values.0.iter().any(|item| item == &value) {
            self.sensitive_values.0.push(value);
            self.sensitive_values
                .0
                .sort_by_key(|value| std::cmp::Reverse(value.len()));
        }
    }

    /// Registers the secrets the MCP manager holds now. OAuth tokens signed in
    /// or refreshed after the run started (before an MCP request is sent) are
    /// unknown to the snapshot taken at run start.
    pub(super) fn refresh_mcp_secrets(&mut self) {
        let Some(manager) = self.mcp.clone() else {
            return;
        };
        for value in manager.sensitive_values() {
            self.register_sensitive_value(value);
        }
    }

    /// Replaces every exact registered sensitive value in `input`.
    pub fn redact_sensitive(&self, input: &str) -> String {
        redact_values(&self.sensitive_values.0, input)
    }

    /// The display diff quotes file lines, so it is redacted like output.
    pub(super) fn redacted_edit_diff(
        &self,
        diff: Option<&crate::ToolEditDiff>,
    ) -> Option<crate::ToolEditDiff> {
        let redact_lines = |lines: &[String]| {
            lines
                .iter()
                .map(|line| self.redact_sensitive(line))
                .collect::<Vec<_>>()
        };
        diff.map(|diff| crate::ToolEditDiff {
            path: self.redact_sensitive(&diff.path),
            hunks: diff
                .hunks
                .iter()
                .map(|hunk| crate::ToolEditHunk {
                    start_line: hunk.start_line,
                    removed: redact_lines(&hunk.removed),
                    added: redact_lines(&hunk.added),
                })
                .collect(),
            truncated: diff.truncated,
        })
    }

    pub(super) fn redact_provider_error(&self, error: ProviderError) -> ProviderError {
        crate::provider::redact_provider_error_values(error, &self.sensitive_values.0)
    }

    pub(super) fn redact_message(&self, mut message: ProviderMessage) -> ProviderMessage {
        if self.sensitive_values.0.is_empty() {
            return message;
        }
        message.content = self.redact_sensitive(&message.content);
        message.recorded_content = message
            .recorded_content
            .map(|recorded| Arc::from(self.redact_sensitive(&recorded)));
        message.name = message.name.map(|value| self.redact_sensitive(&value));
        message.tool_call_id = message
            .tool_call_id
            .map(|value| self.redact_sensitive(&value));
        for call in &mut message.tool_calls {
            call.id = self.redact_sensitive(&call.id);
            call.name = self.redact_sensitive(&call.name);
            call.arguments = self.redact_sensitive(&call.arguments);
        }
        for block in &mut message.content_blocks {
            match block {
                crate::provider::ProviderContentBlock::Text(text)
                | crate::provider::ProviderContentBlock::Unsupported { kind: text } => {
                    *text = self.redact_sensitive(text);
                }
                crate::provider::ProviderContentBlock::Image { media_type, data }
                | crate::provider::ProviderContentBlock::Audio { media_type, data }
                | crate::provider::ProviderContentBlock::File { media_type, data } => {
                    *media_type = self.redact_sensitive(media_type);
                    *data = self.redact_sensitive(data);
                }
            }
        }
        message
    }

    pub(super) fn redact_messages(&self, messages: &[ProviderMessage]) -> Vec<ProviderMessage> {
        if self.sensitive_values.0.is_empty() {
            return messages.to_vec();
        }
        messages
            .iter()
            .cloned()
            .map(|message| self.redact_message(message))
            .collect()
    }
}
