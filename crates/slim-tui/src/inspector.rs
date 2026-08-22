#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectorKind {
    Diff,
    Activity,
    SessionTree,
    Diagnostics,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InspectorState {
    pub active: Option<InspectorKind>,
}

impl InspectorState {
    pub fn toggle(&mut self, kind: InspectorKind) {
        self.active = (self.active != Some(kind)).then_some(kind);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandPalette {
    pub query: String,
}

impl CommandPalette {
    pub fn matches<'a>(&self, commands: &'a [&'a str]) -> Vec<&'a str> {
        let query = self.query.to_ascii_lowercase();
        commands
            .iter()
            .copied()
            .filter(|command| command.to_ascii_lowercase().contains(&query))
            .collect()
    }
}

pub fn safe_block_text<T>(
    render: impl FnOnce() -> Result<T, String>,
    fallback: impl FnOnce() -> T,
) -> T {
    render().unwrap_or_else(|_| fallback())
}
