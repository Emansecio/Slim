use std::sync::atomic::{AtomicU64, Ordering};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
    pub historical: bool,
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
    pub answered: Option<String>,
}

impl InteractionRequestState {
    pub fn same_request(&self, other: &Self) -> bool {
        self.request_id == other.request_id
            && self.kind == other.kind
            && self.persisted == other.persisted
    }

    pub fn display_text(&self) -> String {
        if matches!(self.kind, InteractionRequestKind::Question { .. }) {
            return self.layout_lines(80).join("\n");
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

    pub fn layout_lines(&self, width: usize) -> Vec<String> {
        let InteractionRequestKind::Question { question, options } = &self.kind else {
            return self.display_text().lines().map(str::to_owned).collect();
        };
        let width = width.max(8);
        let mut lines = wrap_hanging("? ", question, width);
        let collapsed = self.acknowledgement.is_some();
        if collapsed {
            let answer = self
                .answered
                .as_deref()
                .or_else(|| {
                    self.acknowledgement
                        .as_ref()
                        .map(|acknowledgement| acknowledgement.message.as_str())
                        .filter(|message| !message.is_empty())
                })
                .unwrap_or(
                    if self
                        .acknowledgement
                        .as_ref()
                        .is_some_and(|ack| ack.accepted)
                    {
                        "accepted"
                    } else {
                        "rejected"
                    },
                );
            lines.extend(wrap_hanging("  · ", answer, width));
            return lines;
        }
        if !options.is_empty() {
            lines.push(String::new());
            for (index, option) in options.iter().enumerate() {
                let selected =
                    !self.custom_question_answer && self.selected_question_option == index;
                let prefix = question_option_marker(selected).to_owned();
                lines.extend(wrap_hanging(&prefix, &option.label, width));
                if !option.description.is_empty() {
                    let hang = " ".repeat(UnicodeWidthStr::width(prefix.as_str()).min(width));
                    lines.extend(wrap_hanging(&hang, &option.description, width));
                }
            }
            let other_selected =
                !self.custom_question_answer && self.selected_question_option == options.len();
            lines.push(format!(
                "{}Outro...",
                question_option_marker(other_selected)
            ));
        }
        if self.response_pending {
            lines.extend(wrap_hanging("  · ", "sent", width));
        } else if self.custom_question_answer {
            lines.extend(wrap_hanging("  · ", "type answer below", width));
        }
        lines
    }
}

pub(crate) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        if word_width > width {
            if used > 0 {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            rows.extend(hard_wrap_token(word, width));
            continue;
        }
        if used == 0 {
            row.push_str(word);
            used = word_width;
            continue;
        }
        if used.saturating_add(1).saturating_add(word_width) <= width {
            row.push(' ');
            row.push_str(word);
            used = used.saturating_add(1).saturating_add(word_width);
        } else {
            rows.push(std::mem::take(&mut row));
            row.push_str(word);
            used = word_width;
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn hard_wrap_token(token: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    for grapheme in token.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme).max(1);
        if used.saturating_add(cells) > width && used > 0 {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push_str(grapheme);
        used = used.saturating_add(cells);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

pub(crate) fn question_option_marker(selected: bool) -> &'static str {
    if selected {
        "[x] "
    } else {
        "[ ] "
    }
}

fn wrap_hanging(prefix: &str, body: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let hang_width = UnicodeWidthStr::width(prefix).min(width.saturating_sub(1));
    let hang = " ".repeat(hang_width);
    wrap_words(body, width.saturating_sub(hang_width).max(1))
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                format!("{prefix}{line}")
            } else {
                format!("{hang}{line}")
            }
        })
        .collect()
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
    block.lifecycle == BlockLifecycle::Complete
        && matches!(block.kind(), BlockKind::Tool(tool) if !tool.historical)
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

    pub(crate) fn record_question_answer(&mut self, answer: String) -> bool {
        let Some(state) = self.interaction_state_mut() else {
            return false;
        };
        if !matches!(state.kind, InteractionRequestKind::Question { .. }) {
            return false;
        }
        state.answered = Some(answer);
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

    /// Compact key for caches: `lifecycle` is assigned directly (bypassing
    /// `touch_content`), so presentation caches must key on it separately.
    pub(crate) fn lifecycle_tag(&self) -> u8 {
        match self.lifecycle {
            BlockLifecycle::Pending => 0,
            BlockLifecycle::Streaming => 1,
            BlockLifecycle::Complete => 2,
            BlockLifecycle::Failed => 3,
            BlockLifecycle::Cancelled => 4,
        }
    }

    /// Compact key for caches: `fold` is likewise assigned directly.
    pub(crate) fn fold_tag(&self) -> u8 {
        match self.fold {
            FoldState::Auto => 0,
            FoldState::Collapsed => 1,
            FoldState::Expanded => 2,
        }
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

#[cfg(test)]
mod tests {
    use super::{wrap_words, InteractionRequestKind, InteractionRequestState};
    use crate::api::InteractionRequestId;

    fn question_state(
        question: &str,
        options: Vec<slim_core::QuestionOption>,
    ) -> InteractionRequestState {
        InteractionRequestState {
            request_id: InteractionRequestId("q".into()),
            kind: InteractionRequestKind::Question {
                question: question.into(),
                options,
            },
            persisted: false,
            response_pending: false,
            acknowledgement: None,
            selected_question_option: 0,
            custom_question_answer: false,
            answered: None,
        }
    }

    #[test]
    fn wrap_words_breaks_on_word_boundaries() {
        let rows = wrap_words("Quer que eu execute o download completo agora?", 24);
        assert!(rows.iter().any(|row| row.contains("download")), "{rows:?}");
        assert!(
            rows.iter()
                .all(|row| !row.trim_end().ends_with("downlo") && !row.starts_with("ad ")),
            "{rows:?}"
        );
    }

    #[test]
    fn question_layout_keeps_description_under_the_label() {
        let state = question_state(
            "Which crate should change?",
            vec![
                slim_core::QuestionOption {
                    label: "core".into(),
                    description: "Runtime and protocol".into(),
                },
                slim_core::QuestionOption {
                    label: "tui".into(),
                    description: "Interface only".into(),
                },
            ],
        );
        let lines = state.layout_lines(36);
        let joined = lines.join("\n");
        assert!(joined.contains("? Which crate should change?"), "{joined}");
        assert!(joined.contains("[x] core"), "{joined}");
        assert!(joined.contains("Runtime and protocol"), "{joined}");
        let core = lines
            .iter()
            .position(|line| line.contains("[x] core"))
            .unwrap();
        assert!(
            !lines[core].contains("Runtime"),
            "description must not crowd the label: {}",
            lines[core]
        );
        assert!(lines[core + 1].contains("Runtime and protocol"), "{joined}");
    }
}
