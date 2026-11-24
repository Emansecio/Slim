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
    /// Changed lines of a successful patch, rendered before the output.
    pub edit_diff: Option<slim_core::ToolEditDiff>,
    /// Redacted program source of a `codemode` call, shown when expanded.
    pub program_source: Option<String>,
}

impl ToolState {
    pub(crate) fn diff_rows(&self) -> Vec<(DiffRowKind, String)> {
        let Some(diff) = &self.edit_diff else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for hunk in &diff.hunks {
            rows.push((
                DiffRowKind::Header,
                format!("@@ {}:{}", diff.path, hunk.start_line),
            ));
            rows.extend(
                hunk.removed
                    .iter()
                    .map(|line| (DiffRowKind::Removed, format!("- {line}"))),
            );
            rows.extend(
                hunk.added
                    .iter()
                    .map(|line| (DiffRowKind::Added, format!("+ {line}"))),
            );
        }
        if diff.truncated {
            rows.push((DiffRowKind::Header, "@@ diff truncated".into()));
        }
        rows
    }

    /// Diff lines for the expanded body: one `@@ path:line` header per hunk,
    /// then `- ` removed and `+ ` added lines.
    pub fn diff_lines(&self) -> Vec<String> {
        self.diff_rows().into_iter().map(|(_, text)| text).collect()
    }

    pub fn has_expanded_body(&self) -> bool {
        self.program_source.is_some()
            || self.edit_diff.is_some()
            || !self.materialized_output.is_empty()
    }

    /// The row an open call keeps under its details while its output is not
    /// all here: a page in flight, or another page it has not asked for yet.
    pub fn page_note(&self) -> Option<ToolPageNote> {
        if self.pending_page.is_some() {
            Some(ToolPageNote::Loading)
        } else if self.next_cursor.is_some() {
            Some(ToolPageNote::MorePages)
        } else {
            None
        }
    }

    /// What Enter does to this call right now, if anything.
    pub fn enter_hint(&self, fold: FoldState) -> Option<&'static str> {
        match fold {
            FoldState::Expanded => match self.page_note() {
                Some(ToolPageNote::Loading) => None,
                Some(ToolPageNote::MorePages) => Some("Enter mais"),
                None if self.has_expanded_body() => Some("Enter recolher"),
                None => None,
            },
            FoldState::Collapsed | FoldState::Auto
                if self.has_expanded_body() || self.content_handle.is_some() =>
            {
                Some("Enter detalhes")
            }
            FoldState::Collapsed | FoldState::Auto => None,
        }
    }

    /// Everything the expanded body shows: program source (for `codemode`),
    /// diff, then materialized output. Layout and rendering measure this same text.
    pub fn expanded_body(&self) -> String {
        let mut sections = Vec::new();
        if let Some(source) = &self.program_source {
            sections.push(format!("Program:\n{source}"));
        }
        let diff = self.diff_lines().join("\n");
        if !diff.is_empty() {
            sections.push(diff);
        }
        if !self.materialized_output.is_empty() {
            sections.push(self.materialized_output.clone());
        }
        sections.join("\n")
    }
}

/// The footer of an open call whose output is still arriving.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPageNote {
    Loading,
    MorePages,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffRowKind {
    Header,
    Added,
    Removed,
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
                    "Enter responder".into()
                } else {
                    format!("opções: {}", options.join(" · "))
                };
                (prompt.clone(), hint)
            }
            InteractionRequestKind::Approval { summary } => {
                (summary.clone(), "Y aprovar · N rejeitar".into())
            }
            InteractionRequestKind::Question { .. } => (String::new(), String::new()),
        };
        let status = match &self.acknowledgement {
            Some(acknowledgement) if acknowledgement.message.is_empty() => {
                if acknowledgement.accepted {
                    "confirmada".into()
                } else {
                    "rejeitada".into()
                }
            }
            Some(acknowledgement) => acknowledgement.message.clone(),
            None if self.response_pending => "resposta enviada · aguardando confirmação".into(),
            None => "aguardando resposta".into(),
        };
        format!("{request}\n{hint}\n{status}")
    }

    pub fn layout_lines(&self, width: usize) -> Vec<String> {
        let InteractionRequestKind::Question { question, options } = &self.kind else {
            return self.display_text().lines().map(str::to_owned).collect();
        };
        if let Some((mut lines, answer)) = self.question_record_rows(width) {
            lines.extend(answer);
            return lines;
        }
        let width = width.max(8);
        let mut lines = wrap_hanging("? ", question, width);
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
            lines.extend(wrap_hanging(
                "  · ",
                "enviada · aguardando confirmação",
                width,
            ));
        } else if self.custom_question_answer {
            lines.extend(wrap_hanging("  · ", "digite a resposta abaixo", width));
        }
        lines
    }

    /// Transcript record of an acknowledged question, on the transcript grid:
    /// the question rows behind the `?` marker, then the answer rows behind
    /// `→` (markers on column 2, text on column 4).
    pub(crate) fn question_record_rows(&self, width: usize) -> Option<(Vec<String>, Vec<String>)> {
        let InteractionRequestKind::Question { question, .. } = &self.kind else {
            return None;
        };
        let acknowledgement = self.acknowledgement.as_ref()?;
        let width = width.max(8);
        let answer = self
            .answered
            .as_deref()
            .or_else(|| {
                Some(acknowledgement.message.as_str()).filter(|message| !message.is_empty())
            })
            .unwrap_or(if acknowledgement.accepted {
                "confirmada"
            } else {
                "rejeitada"
            });
        Some((
            wrap_hanging(QUESTION_RECORD_MARKER, question, width),
            wrap_hanging(QUESTION_ANSWER_MARKER, answer, width),
        ))
    }
}

pub(crate) const QUESTION_RECORD_MARKER: &str = "  ? ";
pub(crate) const QUESTION_ANSWER_MARKER: &str = "  → ";

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
        ">   "
    } else {
        "    "
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
    /// What the turn that just ended changed (files, lines, commands).
    Receipt(crate::receipt::ReceiptState),
    /// The row that folds the work of a finished turn: it stands before the
    /// blocks it folds and hides them while collapsed.
    Work(crate::work::WorkState),
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

fn is_tool_group_bridge(block: &Block) -> bool {
    is_complete_thinking(block)
}

/// How long a call that just settled eases from its brighter tone to rest.
/// The call joins its group in the frame it settles; the group's header
/// carries this emphasis, so no row appears only to disappear.
pub const TOOL_SETTLE_MS: u64 = 249;

/// A live call that has finished, successfully or not. Restored calls have no
/// trustworthy outcome and never join a group.
pub fn is_settled_tool(block: &Block) -> bool {
    matches!(
        block.lifecycle,
        BlockLifecycle::Complete | BlockLifecycle::Failed
    ) && matches!(block.kind(), BlockKind::Tool(tool) if !tool.historical)
}

fn consecutive_tool_span(
    blocks: &[Block],
    index: usize,
    is_member: impl Fn(&Block) -> bool,
) -> Option<(usize, usize)> {
    if index >= blocks.len() || !is_member(&blocks[index]) {
        return None;
    }
    let continues = |block: &Block| is_member(block) || is_tool_group_bridge(block);
    let mut start = index;
    while start > 0 && continues(&blocks[start - 1]) {
        start -= 1;
    }
    while start < index && is_tool_group_bridge(&blocks[start]) {
        start += 1;
    }
    let mut end = index + 1;
    while end < blocks.len() && continues(&blocks[end]) {
        end += 1;
    }
    while end > start && is_tool_group_bridge(&blocks[end - 1]) {
        end -= 1;
    }
    Some((start, end))
}

/// Presentation-only span of consecutive settled tools (DESIGN §11.4.1).
/// A failure joins its neighbors instead of splitting the work; the group
/// keeps it visible as a row of its own (see [`group_failure_rows`]).
/// Complete thinking between those tools is a bridge, not a split,
/// so a think→tools→think→tools streak collapses to one group.
pub fn consecutive_settled_tool_span(blocks: &[Block], index: usize) -> Option<(usize, usize)> {
    consecutive_tool_span(blocks, index, is_settled_tool)
}

/// Whether a settled span reads as one group: more than one call, at least
/// one of them successful. Failures alone keep their own rows (or the
/// identical-failure row).
pub fn is_tool_group(blocks: &[Block], start: usize, end: usize) -> bool {
    let Some(span) = blocks.get(start..end) else {
        return false;
    };
    span.iter().filter(|block| is_settled_tool(block)).count() > 1
        && span.iter().any(is_complete_tool)
}

/// One failure row under a collapsed group: the first member of a run of
/// failures it stands for, how many there were, and whether a later call
/// inside the group succeeded where they failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupFailureRow {
    pub member: usize,
    pub repeats: usize,
    pub recovered: bool,
    /// The calls were refused before running (see [`rejection_reason`]).
    pub rejection: bool,
    /// Every call the row stands for had the same arguments, so the first
    /// one's target names them all.
    pub same_call: bool,
}

/// What a collapsed group shows for its failures: the rows it keeps, and how
/// many refused calls a later success made moot, which the header counts
/// instead of listing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GroupFailures {
    pub rows: Vec<GroupFailureRow>,
    pub folded_rejections: usize,
}

/// Failure texts of a call the tool refused before anything ran: a malformed
/// argument or an edit precondition. The model corrects these itself.
const REJECTION_MARKERS: [&str; 3] = [
    "has unknown argument",
    "no write applied",
    "expected one match",
];

/// The reason a failed call was refused before running, as its row reads it,
/// or None for a failure that ran (a command that exited non-zero, say).
pub fn rejection_reason(block: &Block) -> Option<&str> {
    let BlockKind::Tool(tool) = block.kind() else {
        return None;
    };
    if block.lifecycle != BlockLifecycle::Failed {
        return None;
    }
    let line = tool.preview.lines().next().unwrap_or_default();
    let line = line
        .find("[note:")
        .map_or(line, |start| &line[..start])
        .trim_end_matches([' ', '·']);
    REJECTION_MARKERS
        .iter()
        .any(|marker| line.contains(marker))
        .then_some(line)
}

/// Failures a collapsed group still shows. Identical consecutive failures
/// share one row; a call retried with the same arguments that then succeeded
/// is marked `recovered` instead of disappearing. Refused calls share a row by
/// reason whatever their arguments, and leave the rows once a later call of
/// the same tool (on the same file, for edits) succeeded.
pub fn group_failures(members: &[Block]) -> GroupFailures {
    let call = |block: &Block| match block.kind() {
        BlockKind::Tool(tool) => Some((tool.name.clone(), tool.arguments_summary.clone())),
        _ => None,
    };
    let mut rows: Vec<GroupFailureRow> = Vec::new();
    for (index, member) in members.iter().enumerate() {
        if !is_failed_tool(member) {
            continue;
        }
        if let Some(reason) = rejection_reason(member) {
            let recovered = rejection_recovered(members, index);
            let same = rows.iter_mut().find(|row| {
                row.rejection
                    && tool_name(&members[row.member]) == tool_name(member)
                    && rejection_reason(&members[row.member]) == Some(reason)
            });
            if let Some(row) = same {
                row.repeats += 1;
                row.recovered &= recovered;
                row.same_call &= call(&members[row.member]) == call(member);
                continue;
            }
            rows.push(GroupFailureRow {
                member: index,
                repeats: 1,
                recovered,
                rejection: true,
                same_call: true,
            });
            continue;
        }
        if let Some(last) = rows.iter_mut().rev().find(|row| !row.rejection) {
            let previous = &members[last.member];
            if failed_tool_signature(previous) == failed_tool_signature(member)
                && call(previous) == call(member)
                && members[last.member..index]
                    .iter()
                    .all(|between| is_failed_tool(between) || !is_settled_tool(between))
            {
                last.repeats += 1;
                continue;
            }
        }
        let recovered = members[index + 1..]
            .iter()
            .any(|later| is_complete_tool(later) && call(later) == call(member));
        rows.push(GroupFailureRow {
            member: index,
            repeats: 1,
            recovered,
            rejection: false,
            same_call: true,
        });
    }
    let folded_rejections = rows
        .iter()
        .filter(|row| row.rejection && row.recovered)
        .map(|row| row.repeats)
        .sum();
    rows.retain(|row| !(row.rejection && row.recovered));
    GroupFailures {
        rows,
        folded_rejections,
    }
}

/// The rows of [`group_failures`], for row accounting.
pub fn group_failure_rows(members: &[Block]) -> Vec<GroupFailureRow> {
    group_failures(members).rows
}

fn tool_name(block: &Block) -> Option<&str> {
    match block.kind() {
        BlockKind::Tool(tool) => Some(tool.name.as_str()),
        _ => None,
    }
}

/// Whether a later call made a refused one moot: an edit that later landed on
/// the same file (by either edit tool), or any later success of the same tool
/// when the refused call names no file.
fn rejection_recovered(members: &[Block], index: usize) -> bool {
    let BlockKind::Tool(failed) = members[index].kind() else {
        return false;
    };
    let edits = |name: &str| matches!(name, "write" | "patch");
    let target = |summary: &str| {
        summary
            .split(" · ")
            .map(str::trim)
            .find(|part| !part.is_empty())
            .map(str::to_owned)
    };
    let failed_target = target(&failed.arguments_summary);
    members[index + 1..].iter().any(|later| {
        let BlockKind::Tool(tool) = later.kind() else {
            return false;
        };
        if !is_complete_tool(later) {
            return false;
        }
        match &failed_target {
            Some(path) if edits(&failed.name) => {
                edits(&tool.name) && target(&tool.arguments_summary).as_ref() == Some(path)
            }
            _ => tool.name == failed.name,
        }
    })
}

/// The `● Slim` header that opens the agent's side of a turn. It belongs to
/// the first thing the agent emits after a user message (thinking, a tool
/// call or prose), so everything the agent does in the turn reads under one
/// owner. Later blocks of the turn stay in the flow; a queued draft does not
/// open another group. Cancellation or failure of any assistant segment of the
/// turn is reported on this header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponseHeader {
    pub cancelled: bool,
    pub failed: bool,
    /// Blank row above the label. A user prompt already ends with its own
    /// breathing row, so the header sits directly under it and adds none.
    pub lead_blank: bool,
}

impl ResponseHeader {
    /// The header a prompt shows while it waits for the agent's first output:
    /// the label row, with no blank row above.
    pub const PENDING: Self = Self {
        cancelled: false,
        failed: false,
        lead_blank: false,
    };

    /// The label row, which carries the rule that opens the turn, and the
    /// blank row a notice before it asks for.
    pub fn rows(self) -> usize {
        1 + usize::from(self.lead_blank)
    }

    pub fn cache_tag(self) -> u64 {
        1 + u64::from(self.cancelled) + 2 * u64::from(self.failed) + 4 * u64::from(self.lead_blank)
    }
}

/// Blocks the agent produces inside a turn. Interaction requests render
/// nothing until answered, and system, error and activity rows are notices.
fn is_agent_output(block: &Block) -> bool {
    matches!(
        block.kind(),
        BlockKind::Assistant(_) | BlockKind::Thinking(_) | BlockKind::Tool(_) | BlockKind::Work(_)
    )
}

/// Header for the block at `index`, or `None` when the block is not the first
/// agent output of its turn. Without a preceding user message (a bare
/// transcript) only assistant prose opens a header, as before.
pub fn response_header(blocks: &[Block], index: usize) -> Option<ResponseHeader> {
    let block = blocks.get(index)?;
    if !is_agent_output(block) {
        return None;
    }
    let is_prose = matches!(block.kind(), BlockKind::Assistant(_));
    let mut earlier_output = false;
    let mut earlier_prose = false;
    let mut opened_by_user = false;
    // Stop at the first fact that settles the answer, so a long run of tool
    // calls costs O(1) per block instead of a scan back to the prompt.
    for earlier in blocks[..index].iter().rev() {
        match earlier.kind() {
            BlockKind::User(_) => {
                opened_by_user = true;
                break;
            }
            BlockKind::Assistant(_) => {
                earlier_output = true;
                earlier_prose = true;
                break;
            }
            _ if is_agent_output(earlier) => {
                earlier_output = true;
                if !is_prose {
                    break;
                }
            }
            _ => {}
        }
    }
    let opens = if opened_by_user {
        !earlier_output
    } else {
        is_prose && !earlier_prose
    };
    if !opens {
        return None;
    }
    let mut cancelled = false;
    let mut failed = false;
    for later in &blocks[index..] {
        match later.kind() {
            BlockKind::User(_) => break,
            BlockKind::Assistant(_) => match later.lifecycle {
                BlockLifecycle::Cancelled => cancelled = true,
                BlockLifecycle::Failed => failed = true,
                _ => {}
            },
            _ => {}
        }
    }
    let lead_blank = index == 0 || !matches!(blocks[index - 1].kind(), BlockKind::User(_));
    Some(ResponseHeader {
        cancelled,
        failed,
        lead_blank,
    })
}

/// Prose the agent wrote on the way to something else in the same turn: a
/// thought, a call or more prose follows it before the next prompt. The last
/// prose of a turn is its answer. A receipt or a command the user ran (`!`)
/// closes the agent's side, so nothing after them makes the answer a note.
pub fn is_narration(blocks: &[Block], index: usize) -> bool {
    let Some(block) = blocks.get(index) else {
        return false;
    };
    if !matches!(block.kind(), BlockKind::Assistant(_)) {
        return false;
    }
    for later in &blocks[index + 1..] {
        match later.kind() {
            BlockKind::User(_) | BlockKind::Receipt(_) => return false,
            BlockKind::Tool(tool) => return !tool.arguments_summary.starts_with("! "),
            BlockKind::Thinking(_) => return true,
            BlockKind::Assistant(text) if !text.trim().is_empty() => return true,
            _ => {}
        }
    }
    false
}

/// One breathing row where the agent switches between prose and work inside
/// a turn: before tools/thinking that follow assistant text, and before
/// prose that follows them. Derived from neighbors at layout time, so it
/// needs no per-block state; the block that opens the turn has its own header.
pub fn transition_gap(blocks: &[Block], index: usize) -> bool {
    let Some(block) = blocks.get(index) else {
        return false;
    };
    if index == 0 || block.turn_boundary_before() {
        return false;
    }
    let is_work =
        |block: &Block| matches!(block.kind(), BlockKind::Tool(_) | BlockKind::Thinking(_));
    let previous = &blocks[index - 1];
    match block.kind() {
        // The receipt closes a turn: always set apart from what precedes it.
        BlockKind::Receipt(_) => true,
        // A note folded away with the rest of the work still counts as work
        // above the answer: the folded row and the answer do not touch.
        BlockKind::Assistant(_) => {
            (is_work(previous)
                || (matches!(previous.kind(), BlockKind::Assistant(_))
                    && crate::work::collapsed_row_over(blocks, index - 1).is_some()))
                && response_header(blocks, index).is_none()
        }
        // Work after a receipt is not part of the closed turn (a `!command`).
        _ if is_work(block) => {
            matches!(previous.kind(), BlockKind::Receipt(_))
                || matches!(previous.kind(), BlockKind::Assistant(text) if !text.trim().is_empty())
        }
        _ => false,
    }
}

/// Rows that precede the block at `index`: the turn header, or the
/// prose/work breathing row. Measured and drawn from the same derivation.
pub fn leading_rows(blocks: &[Block], index: usize) -> usize {
    response_header(blocks, index).map_or(0, ResponseHeader::rows)
        + usize::from(transition_gap(blocks, index))
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

/// Presentation-only group of adjacent pending user messages; transcript
/// output between messages keeps its position and breaks the group.
pub fn consecutive_queued_user_span(blocks: &[Block], index: usize) -> Option<(usize, usize)> {
    if !matches!(blocks.get(index)?.kind(), BlockKind::QueuedUser(_)) {
        return None;
    }
    let mut start = index;
    while start > 0 && matches!(blocks[start - 1].kind(), BlockKind::QueuedUser(_)) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < blocks.len() && matches!(blocks[end].kind(), BlockKind::QueuedUser(_)) {
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
    /// The tool-group header folds independently of this member's details.
    pub group_expanded: bool,
    /// Monotonic presentation timestamps; historical blocks may not have them.
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    /// A prompt whose run has not produced any agent output yet: the user
    /// block also draws the `● Slim` header the first agent block will take over.
    awaiting_agent: bool,
    pub reasoning_classification: Option<slim_core::events::ReasoningClassification>,
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
            group_expanded: self.group_expanded,
            started_ms: self.started_ms,
            ended_ms: self.ended_ms,
            awaiting_agent: self.awaiting_agent,
            reasoning_classification: self.reasoning_classification,
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
            && self.group_expanded == other.group_expanded
            && self.started_ms == other.started_ms
            && self.ended_ms == other.ended_ms
            && self.awaiting_agent == other.awaiting_agent
            && self.reasoning_classification == other.reasoning_classification
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
            group_expanded: false,
            started_ms: None,
            ended_ms: None,
            awaiting_agent: false,
            reasoning_classification: None,
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
        let fold = match self.fold {
            FoldState::Auto => 0,
            FoldState::Collapsed => 1,
            FoldState::Expanded => 2,
        };
        fold | (u8::from(self.group_expanded) << 2)
    }

    pub(crate) fn awaiting_agent(&self) -> bool {
        self.awaiting_agent
    }

    /// Returns whether the flag changed (and the block must be laid out again).
    pub(crate) fn set_awaiting_agent(&mut self, value: bool) -> bool {
        if self.awaiting_agent == value {
            return false;
        }
        self.awaiting_agent = value;
        self.touch_content();
        true
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

    pub fn set_tool_diff(&mut self, diff: slim_core::ToolEditDiff) -> bool {
        let BlockKind::Tool(state) = &mut self.kind else {
            return false;
        };
        state.edit_diff = Some(diff);
        self.touch_content();
        true
    }

    pub fn set_tool_source(&mut self, source: String) -> bool {
        let BlockKind::Tool(state) = &mut self.kind else {
            return false;
        };
        state.program_source = Some(source);
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

    /// A call whose details changed shape without new output: a page was
    /// asked for, so its footer and hint read differently.
    pub(crate) fn touch_tool_content(&mut self) {
        if matches!(self.kind, BlockKind::Tool(_)) {
            self.touch_content();
        }
    }

    fn touch_content(&mut self) {
        self.content_generation = self.content_generation.saturating_add(1);
    }
}

#[cfg(test)]
mod turn_layout_tests {
    use super::{
        leading_rows, response_header, transition_gap, Block, BlockKind, BlockLifecycle,
        ResponseHeader, ToolState,
    };

    fn user() -> Block {
        Block::new(
            "u",
            BlockKind::User("pergunta".into()),
            BlockLifecycle::Complete,
        )
    }

    fn thinking() -> Block {
        Block::new(
            "t",
            BlockKind::Thinking("plano".into()),
            BlockLifecycle::Complete,
        )
    }

    fn tool() -> Block {
        Block::new(
            "w",
            BlockKind::Tool(ToolState::default()),
            BlockLifecycle::Complete,
        )
    }

    fn prose(lifecycle: BlockLifecycle) -> Block {
        Block::new("a", BlockKind::Assistant("resposta".into()), lifecycle)
    }

    fn system() -> Block {
        Block::new(
            "s",
            BlockKind::System("aviso".into()),
            BlockLifecycle::Complete,
        )
    }

    fn receipt() -> Block {
        Block::new(
            "r",
            BlockKind::Receipt(crate::receipt::ReceiptState {
                files: 1,
                added: 2,
                removed: 1,
                stats_partial: false,
                commands: 0,
                last_command_ok: None,
                outcome: crate::receipt::ReceiptOutcome::Completed,
            }),
            BlockLifecycle::Complete,
        )
    }

    #[test]
    fn work_after_a_receipt_is_set_apart_from_the_closed_turn() {
        // A `!command` run after a turn ended: the receipt closed that turn.
        let blocks = [user(), tool(), receipt(), tool()];
        assert!(transition_gap(&blocks, 2), "the receipt keeps its own gap");
        assert!(transition_gap(&blocks, 3), "work after it gets a gap too");
        // A prompt starts a turn: no gap, its own boundary.
        let mut next_prompt = user();
        next_prompt.set_turn_boundary_before(true);
        let blocks = [user(), tool(), receipt(), next_prompt, tool()];
        assert!(!transition_gap(&blocks, 4), "work right after a prompt");
    }

    fn headers(blocks: &[Block]) -> Vec<Option<ResponseHeader>> {
        (0..blocks.len())
            .map(|index| response_header(blocks, index))
            .collect()
    }

    #[test]
    fn the_first_agent_output_after_the_prompt_opens_the_header() {
        for opener in [thinking(), tool(), prose(BlockLifecycle::Complete)] {
            let blocks = [
                user(),
                opener,
                tool(),
                prose(BlockLifecycle::Complete),
                thinking(),
            ];
            let found = headers(&blocks);
            assert!(found[0].is_none(), "the prompt is not agent output");
            assert_eq!(
                found[1],
                Some(ResponseHeader {
                    cancelled: false,
                    failed: false,
                    lead_blank: false,
                })
            );
            assert!(found[2..].iter().all(Option::is_none), "{found:?}");
        }
    }

    #[test]
    fn the_header_adds_a_blank_row_only_when_the_prompt_is_not_directly_above() {
        let below_prompt = [user(), tool()];
        let header = response_header(&below_prompt, 1).expect("header");
        assert!(!header.lead_blank);
        assert_eq!(leading_rows(&below_prompt, 1), header.rows());

        let after_notice = [user(), system(), tool()];
        let header = response_header(&after_notice, 2).expect("header");
        assert!(header.lead_blank);
        assert_eq!(header.rows(), 2);
        assert_eq!(leading_rows(&after_notice, 2), 2);
        assert!(
            response_header(&after_notice, 1).is_none(),
            "notices do not open a turn"
        );
    }

    #[test]
    fn prose_after_the_opening_work_breathes_while_the_opener_does_not() {
        let blocks = [
            user(),
            tool(),
            prose(BlockLifecycle::Complete),
            tool(),
            prose(BlockLifecycle::Complete),
        ];
        let opener = response_header(&blocks, 1).expect("header");
        assert_eq!(leading_rows(&blocks, 1), opener.rows(), "header only");
        assert!(!transition_gap(&blocks, 1));
        assert_eq!(leading_rows(&blocks, 2), 1, "breathing row before prose");
        assert!(transition_gap(&blocks, 2));
        assert!(transition_gap(&blocks, 3), "work after prose");
        assert!(transition_gap(&blocks, 4), "prose after work");

        let text_first = [user(), prose(BlockLifecycle::Complete), tool()];
        let opener = response_header(&text_first, 1).expect("header");
        assert_eq!(
            leading_rows(&text_first, 1),
            opener.rows(),
            "header only, no extra gap"
        );
        assert!(!transition_gap(&text_first, 1));
        assert_eq!(leading_rows(&text_first, 2), 1, "work after prose");
    }

    #[test]
    fn a_new_prompt_starts_a_new_turn_with_its_own_header() {
        let blocks = [
            user(),
            prose(BlockLifecycle::Complete),
            user(),
            thinking(),
            prose(BlockLifecycle::Complete),
        ];
        let found = headers(&blocks);
        assert!(found[1].is_some());
        assert!(found[3].is_some());
        assert!(found[4].is_none());
    }

    #[test]
    fn interruption_or_failure_of_any_prose_segment_lands_on_the_turn_header() {
        let cancelled = [
            user(),
            tool(),
            prose(BlockLifecycle::Complete),
            prose(BlockLifecycle::Cancelled),
        ];
        let header = response_header(&cancelled, 1).expect("header");
        assert!(header.cancelled && !header.failed);

        let failed = [user(), thinking(), prose(BlockLifecycle::Failed)];
        let header = response_header(&failed, 1).expect("header");
        assert!(header.failed && !header.cancelled);

        let next_turn = [
            user(),
            prose(BlockLifecycle::Cancelled),
            user(),
            prose(BlockLifecycle::Complete),
        ];
        let header = response_header(&next_turn, 3).expect("header");
        assert!(!header.cancelled, "the previous turn's state does not leak");
        assert_ne!(
            response_header(&next_turn, 1).map(ResponseHeader::cache_tag),
            Some(header.cache_tag())
        );
    }

    #[test]
    fn without_a_prompt_only_prose_carries_the_header() {
        let blocks = [thinking(), prose(BlockLifecycle::Complete), tool()];
        let found = headers(&blocks);
        assert!(found[0].is_none(), "bare work has no owner to name");
        assert!(found[1].expect("prose header").lead_blank);
        assert!(found[2].is_none());
        assert!(!transition_gap(&blocks, 1), "the header replaces the gap");
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
        assert!(joined.contains(">   core"), "{joined}");
        assert!(joined.contains("Runtime and protocol"), "{joined}");
        let core = lines
            .iter()
            .position(|line| line.contains(">   core"))
            .unwrap();
        assert!(
            !lines[core].contains("Runtime"),
            "description must not crowd the label: {}",
            lines[core]
        );
        assert!(lines[core + 1].contains("Runtime and protocol"), "{joined}");
    }

    #[test]
    fn codemode_expanded_body_shows_program_source_before_output() {
        let state = super::ToolState {
            name: "codemode".into(),
            program_source: Some("const x = await slim.list();\nreturn x;".into()),
            materialized_output: "\"ok\"".into(),
            ..super::ToolState::default()
        };
        assert!(state.has_expanded_body());
        assert_eq!(
            state.expanded_body(),
            "Program:\nconst x = await slim.list();\nreturn x;\n\"ok\""
        );
    }
}
