#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectorKind {
    Diff,
    Activity,
    SessionTree,
    Diagnostics,
}

/// Scroll position for an inspector panel.  The bottom-relative variant lets
/// End land at the end without a sentinel offset; Up can then reveal earlier
/// rows immediately regardless of the panel's runtime height.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectorScroll {
    Offset(usize),
    FromEnd(usize),
}

impl Default for InspectorScroll {
    fn default() -> Self {
        Self::Offset(0)
    }
}

impl InspectorScroll {
    pub fn top(&mut self) {
        *self = Self::Offset(0);
    }

    pub fn end(&mut self) {
        *self = Self::FromEnd(0);
    }

    pub fn up(&mut self, amount: usize) {
        match self {
            Self::Offset(offset) => *offset = offset.saturating_sub(amount),
            Self::FromEnd(offset) => *offset = offset.saturating_add(amount),
        }
    }

    pub fn down(&mut self, amount: usize) {
        match self {
            Self::Offset(offset) => *offset = offset.saturating_add(amount),
            Self::FromEnd(offset) => *offset = offset.saturating_sub(amount),
        }
    }

    pub fn up_bounded(&mut self, amount: usize, total: usize, capacity: usize) {
        let capacity = capacity.max(1).min(total.max(1));
        let max_start = total.saturating_sub(capacity);
        let next = self.start(total, capacity).saturating_sub(amount);
        if matches!(self, Self::FromEnd(_)) {
            *self = Self::FromEnd(max_start.saturating_sub(next));
        } else {
            *self = Self::Offset(next);
        }
    }

    pub fn down_bounded(&mut self, amount: usize, total: usize, capacity: usize) {
        let capacity = capacity.max(1).min(total.max(1));
        let max_start = total.saturating_sub(capacity);
        let next = self
            .start(total, capacity)
            .saturating_add(amount)
            .min(max_start);
        if next == max_start {
            *self = Self::FromEnd(0);
        } else {
            *self = Self::Offset(next);
        }
    }

    pub fn start(self, total: usize, capacity: usize) -> usize {
        if total == 0 {
            return 0;
        }
        let capacity = capacity.max(1).min(total);
        let max_start = total.saturating_sub(capacity);
        match self {
            Self::Offset(offset) => offset.min(max_start),
            Self::FromEnd(offset) => max_start.saturating_sub(offset),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InspectorState {
    pub active: Option<InspectorKind>,
    pub scroll: InspectorScroll,
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

fn str_contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let needle_bytes = needle_lower.as_bytes();
        let haystack_bytes = haystack.as_bytes();
        if needle_bytes.len() > haystack_bytes.len() {
            return false;
        }
        return haystack_bytes
            .windows(needle_bytes.len())
            .any(|w| w.eq_ignore_ascii_case(needle_bytes));
    }
    haystack.to_lowercase().contains(needle_lower)
}

pub fn block_matches_query(block: &Block, query_lower: &str) -> bool {
    match block.kind() {
        BlockKind::User(text)
        | BlockKind::Assistant(text)
        | BlockKind::Thinking(text)
        | BlockKind::System(text)
        | BlockKind::Error(text)
        | BlockKind::Activity(text)
        | BlockKind::QueuedUser(text) => str_contains_ignore_case(text, query_lower),
        BlockKind::Tool(tool) => {
            if str_contains_ignore_case(&tool.name, query_lower)
                || str_contains_ignore_case(&tool.arguments_summary, query_lower)
                || str_contains_ignore_case(&tool.preview, query_lower)
                || str_contains_ignore_case(&tool.materialized_output, query_lower)
            {
                true
            } else if query_lower.contains(' ') {
                let combined = format!(
                    "{} {} {} {}",
                    tool.name, tool.arguments_summary, tool.preview, tool.materialized_output
                );
                str_contains_ignore_case(&combined, query_lower)
            } else {
                false
            }
        }
        BlockKind::InteractionRequest(request) => {
            str_contains_ignore_case(&request.display_text(), query_lower)
        }
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
    let query_lower = query.to_lowercase();
    blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            (filter.matches(block) && block_matches_query(block, &query_lower)).then_some(index)
        })
        .collect()
}

pub fn is_mutating_tool(name: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "write", "edit", "patch", "create", "delete", "remove", "move", "rename",
    ];
    let name_bytes = name.as_bytes();
    NEEDLES.iter().any(|needle| {
        let needle_bytes = needle.as_bytes();
        if needle_bytes.len() > name_bytes.len() {
            return false;
        }
        name_bytes
            .windows(needle_bytes.len())
            .any(|w| w.eq_ignore_ascii_case(needle_bytes))
    })
}

impl InspectorState {
    pub fn toggle(&mut self, kind: InspectorKind) {
        if self.active == Some(kind) {
            self.active = None;
            self.scroll = InspectorScroll::default();
        } else {
            self.active = Some(kind);
            self.scroll = InspectorScroll::default();
        }
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
            .filter(|command| str_contains_ignore_case(command, &query))
            .collect()
    }
}

pub fn safe_block_text<T>(
    render: impl FnOnce() -> Result<T, String>,
    fallback: impl FnOnce() -> T,
) -> T {
    render().unwrap_or_else(|_| fallback())
}
use crate::block::{is_failed_tool, Block, BlockKind};
