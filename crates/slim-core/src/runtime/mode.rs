pub fn mode_name(mode: crate::OperatingMode) -> &'static str {
    match mode {
        crate::OperatingMode::Auto => "Auto",
        crate::OperatingMode::ReadOnly => "Read-only",
        crate::OperatingMode::Plan => "Plan",
    }
}

/// Suffix on the latest user message. Durable JSONL stores the prompt only;
/// [`super::without_workspace_snapshot`] strips this with the workspace listing.
pub(super) const CHANNEL_MARKER: &str = "\n\nHarness channel:";

/// Facts that match advertised tools: `ask_question` only when Auto has a route.
pub(super) struct ChannelOverlay<'a> {
    messages: &'a mut [crate::provider::ProviderMessage],
    index: usize,
    original_len: usize,
    applied: bool,
}

impl<'a> ChannelOverlay<'a> {
    pub(super) fn apply(
        messages: &'a mut [crate::provider::ProviderMessage],
        mode: crate::OperatingMode,
        can_ask: bool,
    ) -> Self {
        let Some(index) = messages.iter().rposition(|message| message.role == "user") else {
            return Self {
                messages,
                index: 0,
                original_len: 0,
                applied: false,
            };
        };
        let content = &mut messages[index].content;
        if let Some(at) = content.find(CHANNEL_MARKER) {
            content.truncate(at);
        }
        let original_len = content.len();
        content.push_str(channel_stanza(mode, can_ask));
        Self {
            messages,
            index,
            original_len,
            applied: true,
        }
    }

    pub(super) fn view(&self) -> &[crate::provider::ProviderMessage] {
        self.messages
    }
}

impl Drop for ChannelOverlay<'_> {
    fn drop(&mut self) {
        if self.applied {
            self.messages[self.index]
                .content
                .truncate(self.original_len);
        }
    }
}

pub(super) fn channel_stanza(mode: crate::OperatingMode, can_ask: bool) -> &'static str {
    match (mode, can_ask) {
        (crate::OperatingMode::Auto, true) => {
            "\n\nHarness channel: Auto, interactive. Use ask_question for unauthorized destructive/irreversible/external/production writes, secret exposure, new dependencies, scope expansion, and undiscoverable architecture/safety/data/behavior decisions. Do not reconfirm already authorized work. Shell runs PowerShell, not bash."
        }
        (crate::OperatingMode::Auto, false) => {
            "\n\nHarness channel: Auto, unattended. No interactive pause (ask_question is not available). Refuse unauthorized destructive/irreversible/external/production writes, secret exposure, new dependencies or scope expansion rather than executing them. Continue authorized work without waiting. Shell runs PowerShell, not bash."
        }
        (crate::OperatingMode::Plan, _) => {
            "\n\nHarness channel: Plan. Inspect and report only. Workspace mutations, shell, todo, skill and ask_question are not available."
        }
        (crate::OperatingMode::ReadOnly, true) => {
            "\n\nHarness channel: Read-only, interactive. Inspect without mutating the workspace. Use ask_question for undiscoverable architecture/safety/data/behavior decisions. write, patch, shell, todo and skill are not available."
        }
        (crate::OperatingMode::ReadOnly, false) => {
            "\n\nHarness channel: Read-only. Inspect without mutating the workspace. write, patch, shell, todo, skill and ask_question are not available."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{channel_stanza, CHANNEL_MARKER};
    use crate::OperatingMode;

    #[test]
    fn channel_stanza_matches_advertised_ask_question() {
        let interactive = channel_stanza(OperatingMode::Auto, true);
        let unattended = channel_stanza(OperatingMode::Auto, false);
        assert!(interactive.starts_with(CHANNEL_MARKER));
        assert!(unattended.starts_with(CHANNEL_MARKER));
        assert!(interactive.contains("Auto, interactive"));
        assert!(interactive.contains("Use ask_question"));
        assert!(!interactive.contains("unattended"));
        assert!(unattended.contains("Auto, unattended"));
        assert!(unattended.contains("ask_question is not available"));
        assert!(!unattended.contains("Auto, interactive"));
        assert!(interactive.contains("Shell runs PowerShell"));
        assert!(unattended.contains("Shell runs PowerShell"));
        assert!(!channel_stanza(OperatingMode::Plan, true).contains("PowerShell"));
        assert!(!channel_stanza(OperatingMode::ReadOnly, true).contains("PowerShell"));
        assert!(channel_stanza(OperatingMode::Plan, true).contains("Plan."));
        assert!(!channel_stanza(OperatingMode::Plan, true).contains("Use ask_question"));
        let readonly = channel_stanza(OperatingMode::ReadOnly, true);
        assert!(readonly.contains("Read-only, interactive"));
        assert!(readonly.contains("Use ask_question"));
        assert!(!channel_stanza(OperatingMode::ReadOnly, false).contains("Use ask_question"));
    }
}
