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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchState {
    pub query: String,
    pub selected: usize,
    pub filter: SearchFilter,
}

/// Scope of transcript search (DESIGN §17.2): `Tab` with the search open
/// cycles `all → errors → tools`. Errors are `Error` blocks plus failed tool
/// calls; tools are every tool block regardless of lifecycle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchFilter {
    #[default]
    All,
    Errors,
    Tools,
}

impl SearchFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Errors,
            Self::Errors => Self::Tools,
            Self::Tools => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Errors => "errors",
            Self::Tools => "tools",
        }
    }

    fn matches(self, block: &Block) -> bool {
        match self {
            Self::All => true,
            Self::Errors => matches!(block.kind(), BlockKind::Error(_)) || is_failed_tool(block),
            Self::Tools => matches!(block.kind(), BlockKind::Tool(_)),
        }
    }
}

pub fn block_search_text(block: &Block) -> String {
    match block.kind() {
        BlockKind::User(text)
        | BlockKind::Assistant(text)
        | BlockKind::Thinking(text)
        | BlockKind::System(text)
        | BlockKind::Error(text)
        | BlockKind::Activity(text)
        | BlockKind::QueuedUser(text) => text.clone(),
        BlockKind::Tool(tool) => format!(
            "{} {} {} {}",
            tool.name, tool.arguments_summary, tool.preview, tool.materialized_output
        ),
        BlockKind::InteractionRequest(request) => request.display_text(),
    }
}

pub fn search_match_indices(blocks: &[Block], query: &str) -> Vec<usize> {
    search_match_indices_filtered(blocks, query, SearchFilter::All)
}

pub fn search_match_indices_filtered(
    blocks: &[Block],
    query: &str,
    filter: SearchFilter,
) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            (filter.matches(block) && block_search_text(block).to_lowercase().contains(&query))
                .then_some(index)
        })
        .collect()
}

pub fn is_mutating_tool(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "write", "edit", "patch", "create", "delete", "remove", "move", "rename",
    ]
    .iter()
    .any(|needle| name.contains(needle))
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
use crate::block::{Block, BlockKind, is_failed_tool};
