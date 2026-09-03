use std::sync::atomic::{AtomicU64, Ordering};

use super::api::{
    BlockId, ContentHandle, ContentRequestId, InteractionRequestId, PageCursor, ToolBatchId,
    ToolCallId,
};

static NEXT_CACHE_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn fresh_cache_identity() -> u64 {
    NEXT_CACHE_IDENTITY.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolState {
    pub batch_id: ToolBatchId,
    pub call_id: ToolCallId,
    pub name: String,
    pub arguments_summary: String,
    pub preview: String,
    pub duration_ms: Option<u64>,
    pub content_handle: Option<ContentHandle>,
    pub materialized_output: String,
    pub next_cursor: Option<PageCursor>,
    pub pending_page: Option<PendingContentPage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingContentPage {
    pub request_id: ContentRequestId,
    pub cursor: Option<PageCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractionRequestKind {
    Input {
        prompt: String,
        options: Vec<String>,
    },
    Approval {
        summary: String,
    },
    Question {
        question: String,
        options: Vec<slim_core::QuestionOption>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionAcknowledgement {
    pub accepted: bool,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionRequestState {
    pub request_id: InteractionRequestId,
    pub kind: InteractionRequestKind,
    pub persisted: bool,
    pub response_pending: bool,
    pub acknowledgement: Option<InteractionAcknowledgement>,
    pub selected_question_option: usize,
    pub custom_question_answer: bool,
}

impl InteractionRequestState {
    pub fn same_request(&self, other: &Self) -> bool {
        self.request_id == other.request_id
            && self.kind == other.kind
            && self.persisted == other.persisted
    }

    pub fn display_text(&self) -> String {
        if let InteractionRequestKind::Question { question, options } = &self.kind {
            let mut lines = vec![question.clone()];
            if options.is_empty() {
                lines.push("Type your answer and press Enter".into());
            } else {
                for (index, option) in options.iter().enumerate() {
                    let marker = if self.selected_question_option == index {
                        ">"
                    } else {
                        " "
                    };
                    let description = if option.description.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", option.description)
                    };
                    lines.push(format!(
                        "{marker} {}. {}{description}",
                        index + 1,
                        option.label
                    ));
                }
                let marker = if self.selected_question_option == options.len() {
                    ">"
                } else {
                    " "
                };
                lines.push(format!("{marker} Outro..."));
                if self.custom_question_answer {
                    lines.push("Type your answer and press Enter".into());
                }
            }
            if self.response_pending {
                lines.push("Response sent · awaiting acknowledgement".into());
            } else if let Some(acknowledgement) = &self.acknowledgement {
                lines.push(if acknowledgement.message.is_empty() {
                    if acknowledgement.accepted {
                        "Accepted".into()
                    } else {
                        "Rejected".into()
                    }
                } else {
                    acknowledgement.message.clone()
                });
            }
            return lines.join("\n");
        }
        let (request, hint) = match &self.kind {
            InteractionRequestKind::Input { prompt, options } => {
                let hint = if options.is_empty() {
                    "Enter answer".into()
                } else {
                    format!("options: {}", options.join(" · "))
                };
                (prompt.clone(), hint)
            }
            InteractionRequestKind::Approval { summary } => {
                (summary.clone(), "Y approve · N reject".into())
            }
            InteractionRequestKind::Question { .. } => (String::new(), String::new()),
        };
        let persistence = if self.persisted {
            "persisted"
        } else {
            "ephemeral"
        };
        let status = match &self.acknowledgement {
            Some(acknowledgement) if acknowledgement.message.is_empty() => {
                if acknowledgement.accepted {
                    "accepted".into()
                } else {
                    "rejected".into()
                }
            }
            Some(acknowledgement) => acknowledgement.message.clone(),
            None if self.response_pending => "response sent · awaiting acknowledgement".into(),
            None => "waiting for response".into(),
        };
        format!("{request}\n{hint}\n{persistence} · {status}")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockKind {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(ToolState),
    InteractionRequest(InteractionRequestState),
    System(String),
    Error(String),
    Activity(String),
    QueuedUser(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockLifecycle {
    Pending,
    Streaming,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FoldState {
    Auto,
    Collapsed,
    Expanded,
}

pub fn is_complete_tool(block: &Block) -> bool {
    block.lifecycle == BlockLifecycle::Complete && matches!(block.kind(), BlockKind::Tool(_))
}

pub fn is_collapsed_complete_thinking(block: &Block) -> bool {
    block.lifecycle == BlockLifecycle::Complete
        && matches!(block.kind(), BlockKind::Thinking(_))
        && block.fold != FoldState::Expanded
}

fn is_tool_group_bridge(block: &Block) -> bool {
    is_collapsed_complete_thinking(block)
}

/// Presentation-only span of consecutive successful tools (DESIGN §11.4.1).
/// Collapsed complete thinking between those tools is a bridge, not a split,
/// so a think→tools→think→tools streak collapses to one group.
pub fn consecutive_complete_tool_span(blocks: &[Block], index: usize) -> Option<(usize, usize)> {
    if index >= blocks.len() || !is_complete_tool(&blocks[index]) {
        return None;
    }
    let mut start = index;
    while start > 0
        && (is_complete_tool(&blocks[start - 1]) || is_tool_group_bridge(&blocks[start - 1]))
    {
        start -= 1;
    }
    while start < index && is_tool_group_bridge(&blocks[start]) {
        start += 1;
    }
    let mut end = index + 1;
    while end < blocks.len()
        && (is_complete_tool(&blocks[end]) || is_tool_group_bridge(&blocks[end]))
    {
        end += 1;
    }
    while end > start && is_tool_group_bridge(&blocks[end - 1]) {
        end -= 1;
    }
    Some((start, end))
}

pub fn complete_tool_count(blocks: &[Block], start: usize, end: usize) -> usize {
    blocks
        .get(start..end)
        .map(|span| span.iter().filter(|block| is_complete_tool(block)).count())
        .unwrap_or(0)
}

pub fn is_failed_tool(block: &Block) -> bool {
    block.lifecycle == BlockLifecycle::Failed && matches!(block.kind(), BlockKind::Tool(_))
}

fn failed_tool_signature(block: &Block) -> Option<(&str, &str)> {
    match block.kind() {
        BlockKind::Tool(tool) if block.lifecycle == BlockLifecycle::Failed => {
            Some((tool.name.as_str(), tool.preview.as_str()))
        }
        _ => None,
    }
}

/// Consecutive failed tools that share name and reason, for one error row.
pub fn is_complete_thinking(block: &Block) -> bool {
    block.lifecycle == BlockLifecycle::Complete && matches!(block.kind(), BlockKind::Thinking(_))
}

pub fn consecutive_complete_thinking_span(
    blocks: &[Block],
    index: usize,
) -> Option<(usize, usize)> {
    if index >= blocks.len() || !is_complete_thinking(&blocks[index]) {
        return None;
    }
    let mut start = index;
    while start > 0 && is_complete_thinking(&blocks[start - 1]) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < blocks.len() && is_complete_thinking(&blocks[end]) {
        end += 1;
    }
    Some((start, end))
}

pub fn consecutive_identical_failed_tool_span(
    blocks: &[Block],
    index: usize,
) -> Option<(usize, usize)> {
    let signature = failed_tool_signature(blocks.get(index)?)?;
    let mut start = index;
    while start > 0 && failed_tool_signature(&blocks[start - 1]) == Some(signature) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < blocks.len() && failed_tool_signature(&blocks[end]) == Some(signature) {
        end += 1;
    }
    Some((start, end))
}

#[derive(Debug)]
pub struct Block {
    pub id: BlockId,
    kind: BlockKind,
    pub lifecycle: BlockLifecycle,
    pub fold: FoldState,
    turn_boundary_before: bool,
    content_generation: u64,
    cache_identity: u64,
}

impl Clone for Block {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            kind: self.kind.clone(),
            lifecycle: self.lifecycle,
            fold: self.fold,
            turn_boundary_before: self.turn_boundary_before,
            content_generation: self.content_generation,
            cache_identity: fresh_cache_identity(),
        }
    }
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.lifecycle == other.lifecycle
            && self.fold == other.fold
            && self.turn_boundary_before == other.turn_boundary_before
            && self.content_generation == other.content_generation
    }
}

impl Eq for Block {}

impl Block {
    pub fn new(id: impl Into<String>, kind: BlockKind, lifecycle: BlockLifecycle) -> Self {
        Self {
            id: BlockId(id.into().into()),
            kind,
            lifecycle,
            fold: FoldState::Auto,
            turn_boundary_before: false,
            content_generation: 0,
            cache_identity: fresh_cache_identity(),
        }
    }

    pub fn kind(&self) -> &BlockKind {
        &self.kind
    }

    pub(crate) fn tool_state_mut(&mut self) -> Option<&mut ToolState> {
        match &mut self.kind {
            BlockKind::Tool(state) => Some(state),
            _ => None,
        }
    }

    pub(crate) fn interaction_state_mut(&mut self) -> Option<&mut InteractionRequestState> {
        match &mut self.kind {
            BlockKind::InteractionRequest(state) => Some(state),
            _ => None,
        }
    }

    pub(crate) fn mark_interaction_response_pending(&mut self) -> bool {
        let Some(state) = self.interaction_state_mut() else {
            return false;
        };
        if state.response_pending || state.acknowledgement.is_some() {
            return false;
        }
        state.response_pending = true;
        self.touch_content();
        true
    }

    pub(crate) fn move_question_selection(&mut self, forward: bool) -> bool {
        let Some(state) = self.interaction_state_mut() else {
            return false;
        };
        let InteractionRequestKind::Question { options, .. } = &state.kind else {
            return false;
        };
        if state.custom_question_answer || options.is_empty() {
            return false;
        }
        let count = options.len() + 1;
        state.selected_question_option = if forward {
            (state.selected_question_option + 1) % count
        } else {
            state
                .selected_question_option
                .checked_sub(1)
                .unwrap_or(count - 1)
        };
        self.touch_content();
        true
    }

    pub(crate) fn select_question_option(&mut self, index: usize) -> bool {
        let Some(state) = self.interaction_state_mut() else {
            return false;
        };
        let InteractionRequestKind::Question { options, .. } = &state.kind else {
            return false;
        };
        if state.custom_question_answer || index >= options.len() {
            return false;
        }
        state.selected_question_option = index;
        self.touch_content();
        true
    }

    pub(crate) fn activate_custom_question_answer(&mut self) -> bool {
        let Some(state) = self.interaction_state_mut() else {
            return false;
        };
        if !matches!(state.kind, InteractionRequestKind::Question { .. })
            || state.custom_question_answer
        {
            return false;
        }
        state.custom_question_answer = true;
        self.touch_content();
        true
    }

    pub(crate) fn acknowledge_interaction(&mut self, accepted: bool, message: String) -> bool {
        let Some(state) = self.interaction_state_mut() else {
            return false;
        };
        if state.acknowledgement.is_some() {
            return false;
        }
        state.response_pending = false;
        state.acknowledgement = Some(InteractionAcknowledgement { accepted, message });
        self.lifecycle = if accepted {
            BlockLifecycle::Complete
        } else {
            BlockLifecycle::Failed
        };
        self.touch_content();
        true
    }

    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    pub(crate) fn turn_boundary_before(&self) -> bool {
        self.turn_boundary_before
    }

    pub(crate) fn set_turn_boundary_before(&mut self, value: bool) {
        if self.turn_boundary_before != value {
            self.turn_boundary_before = value;
            self.touch_content();
        }
    }

    pub(crate) fn cache_identity(&self) -> u64 {
        self.cache_identity
    }

    pub fn append_text(&mut self, text: &str) {
        match &mut self.kind {
            BlockKind::Assistant(current) | BlockKind::Thinking(current) => current.push_str(text),
            _ => return,
        }
        self.touch_content();
    }

    pub fn set_tool_preview(&mut self, preview: String) -> bool {
        let BlockKind::Tool(state) = &mut self.kind else {
            return false;
        };
        state.preview = preview;
        self.touch_content();
        true
    }

    pub fn append_tool_page(&mut self, text: &str, next_cursor: Option<PageCursor>) -> bool {
        const MAX_MATERIALIZED_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
        let BlockKind::Tool(state) = &mut self.kind else {
            return false;
        };
        let Some(next_len) = state.materialized_output.len().checked_add(text.len()) else {
            return false;
        };
        if next_len > MAX_MATERIALIZED_OUTPUT_BYTES {
            return false;
        }
        state.materialized_output.push_str(text);
        state.next_cursor = next_cursor;
        state.pending_page = None;
        self.touch_content();
        true
    }

    fn touch_content(&mut self) {
        self.content_generation = self.content_generation.saturating_add(1);
    }
}
