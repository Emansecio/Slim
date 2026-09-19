use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::{BlockId, UiEvent};
use crate::app::{AppState, FollowMode, ModelOverlay, ModelRow, ScrollAnchor};
use crate::block::{Block, BlockKind};
use crate::cache::{BoundedCache, WeightedCache};
use crate::composer::{Composer, DisplaySnapshot};
use crate::inspector::{InspectorKind, SearchFilter};
use crate::layout;
use crate::markdown::LogicalLine;
use crate::view_model::{Frame, ViewModel};
use ratatui::style::Style;
use ratatui::text::Line as RatatuiLine;
use unicode_segmentation::UnicodeSegmentation;

/// Retain a visual-row boundary rather than a sliding character suffix. Revisit
/// the last few rows so an appended combining mark/ZWJ can complete a grapheme.
/// Width changes rebuild once; appends only wrap the retained suffix and delta.
#[derive(Default)]
struct ThinkingPreview {
    generation: u64,
    source_len: usize,
    restart: usize,
    logical_column: usize,
    hidden: bool,
    rows: Vec<String>,
}

impl ThinkingPreview {
    fn update(&mut self, text: &str, generation: u64, width: u16) {
        if self.generation == generation && self.source_len == text.len() {
            return;
        }
        if text.len() < self.source_len || !text.is_char_boundary(self.restart) {
            *self = Self::default();
        }
        let prefix = " ".repeat(self.logical_column % 4);
        let source = format!("{prefix}{}", &text[self.restart..]);
        let raw_offsets: Vec<_> = source.char_indices().map(|(offset, _)| offset).collect();
        let (safe, mapped) =
            crate::markdown::sanitize_terminal_text_with_offsets(&source, &raw_offsets);
        let safe = safe[prefix.len()..].trim_end();
        let width = usize::from(width.max(1));
        let mut rows = Vec::new();
        let mut boundaries = vec![(0usize, self.logical_column)];
        let mut row = String::new();
        let mut used = 0usize;
        let mut column = self.logical_column;
        for (offset, grapheme) in safe.grapheme_indices(true) {
            if grapheme == "\n" {
                rows.push(std::mem::take(&mut row));
                used = 0;
                column = 0;
                boundaries.push((offset + 1, column));
                continue;
            }
            let (display, cells) = crate::markdown::normalized_grapheme(grapheme, width);
            if used > 0 && used.saturating_add(cells) > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
                boundaries.push((offset, column));
            }
            row.push_str(display);
            used = used.saturating_add(cells);
            column = column.saturating_add(unicode_width::UnicodeWidthStr::width(grapheme));
        }
        if !safe.is_empty() {
            rows.push(row);
        }
        // Only restart on a real raw boundary. A tab expansion can cross a row
        // boundary; in that case retain the preceding row as well.
        for &(offset, column) in boundaries.iter().take(rows.len().saturating_sub(2)).rev() {
            if let Some(index) = mapped
                .iter()
                .position(|mapped| *mapped == offset + prefix.len())
            {
                let raw = raw_offsets[index].saturating_sub(prefix.len());
                if raw > 0 {
                    self.restart += raw;
                    self.logical_column = column;
                    self.hidden = true;
                    break;
                }
            }
        }
        self.hidden |= rows.len() > 2;
        self.rows = rows.into_iter().rev().take(2).collect();
        self.rows.reverse();
        self.source_len = text.len();
        self.generation = generation;
    }
}

#[derive(Debug)]
pub struct EventCoalescer {
    data_capacity: usize,
    data: Vec<UiEvent>,
    control: Vec<UiEvent>,
    window: Duration,
    clock_origin: Instant,
    window_started: Option<Duration>,
}

impl EventCoalescer {
    pub fn new(data_capacity: usize, window: Duration) -> Self {
        Self {
            data_capacity: data_capacity.max(1),
            data: Vec::new(),
            control: Vec::new(),
            window,
            clock_origin: Instant::now(),
            window_started: None,
        }
    }

    pub fn push_control(&mut self, event: UiEvent) {
        self.control.push(event);
    }

    /// Adds one data event and returns a lossless ready chunk when capacity is
    /// reached. The caller must reduce the returned events before continuing.
    pub fn push_data(&mut self, event: UiEvent) -> Vec<UiEvent> {
        self.push_data_at(event, self.clock_origin.elapsed())
    }

    pub(crate) fn push_data_at(&mut self, event: UiEvent, now: Duration) -> Vec<UiEvent> {
        if let Some(UiEvent::AssistantDelta { text: current }) = self.data.last_mut() {
            if let UiEvent::AssistantDelta { text } = &event {
                current.push_str(text);
                return Vec::new();
            }
        }
        if let Some(UiEvent::ThinkingDelta { text: current }) = self.data.last_mut() {
            if let UiEvent::ThinkingDelta { text } = &event {
                current.push_str(text);
                return Vec::new();
            }
        }
        if let Some(UiEvent::ToolProgress {
            batch_id: current_batch,
            call_id: current_call,
            name: current_name,
            preview: current_preview,
            content_handle: current_handle,
        }) = self.data.last_mut()
        {
            if let UiEvent::ToolProgress {
                batch_id,
                call_id,
                name,
                preview,
                content_handle,
            } = &event
            {
                if current_batch == batch_id && current_call == call_id {
                    current_name.clone_from(name);
                    current_preview.clone_from(preview);
                    if content_handle.is_some() {
                        current_handle.clone_from(content_handle);
                    }
                    return Vec::new();
                }
            }
        }
        if let Some(UiEvent::UsageEstimate {
            request_id: current_request,
            context_tokens: current,
            context_window_tokens: current_window,
        }) = self.data.last_mut()
        {
            if let UiEvent::UsageEstimate {
                request_id,
                context_tokens,
                context_window_tokens,
            } = &event
            {
                if current_request == request_id && current_window == context_window_tokens {
                    *current = (*current).max(*context_tokens);
                    return Vec::new();
                }
            }
        }
        if let Some(UiEvent::UsageEstimateForRun {
            run_id: current_run,
            request_id: current_request,
            context_tokens: current,
            context_window_tokens: current_window,
        }) = self.data.last_mut()
        {
            if let UiEvent::UsageEstimateForRun {
                run_id,
                request_id,
                context_tokens,
                context_window_tokens,
            } = &event
            {
                if current_run == run_id
                    && current_request == request_id
                    && current_window == context_window_tokens
                {
                    *current = (*current).max(*context_tokens);
                    return Vec::new();
                }
            }
        }
        let ready = if self.data.len() >= self.data_capacity {
            self.flush()
        } else {
            Vec::new()
        };
        self.data.push(event);
        self.window_started.get_or_insert(now);
        ready
    }

    pub fn flush(&mut self) -> Vec<UiEvent> {
        let mut events = std::mem::take(&mut self.control);
        events.extend(std::mem::take(&mut self.data));
        self.window_started = None;
        events
    }

    pub fn window_elapsed(&self) -> bool {
        self.window_elapsed_at(self.clock_origin.elapsed())
    }

    pub(crate) fn time_until_flush(&self) -> Option<Duration> {
        self.time_until_flush_at(self.clock_origin.elapsed())
    }

    pub(crate) fn window_elapsed_at(&self, now: Duration) -> bool {
        self.window_started
            .is_some_and(|started| now.saturating_sub(started) >= self.window)
    }

    pub(crate) fn time_until_flush_at(&self, now: Duration) -> Option<Duration> {
        self.window_started
            .map(|started| self.window.saturating_sub(now.saturating_sub(started)))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty() && self.control.is_empty()
    }
}

#[cfg(test)]
mod coalescer_tests {
    use std::time::Duration;

    use super::EventCoalescer;
    use crate::api::{ContentHandle, ToolBatchId, ToolCallId, UiEvent};
    use slim_core::{EventKind, SessionEvent};

    #[test]
    fn fake_monotonic_clock_reaches_window_without_sleeping() {
        let mut coalescer = EventCoalescer::new(16, Duration::from_millis(16));
        assert!(coalescer
            .push_data_at(
                UiEvent::AssistantDelta { text: "a".into() },
                Duration::from_millis(100),
            )
            .is_empty());
        assert!(!coalescer.window_elapsed_at(Duration::from_millis(115)));
        assert!(coalescer.window_elapsed_at(Duration::from_millis(116)));

        assert!(coalescer
            .push_data_at(
                UiEvent::AssistantDelta { text: "b".into() },
                Duration::from_millis(116),
            )
            .is_empty());
        assert!(!coalescer.is_empty());
        assert_eq!(
            coalescer.time_until_flush_at(Duration::from_millis(116)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            coalescer.flush(),
            vec![UiEvent::AssistantDelta { text: "ab".into() }]
        );
        assert!(coalescer.is_empty());
    }

    #[test]
    fn thinking_and_tool_progress_coalesce_only_with_matching_identity() {
        let mut coalescer = EventCoalescer::new(16, Duration::from_millis(16));
        for text in ["reason ", "continued"] {
            coalescer.push_data(UiEvent::ThinkingDelta { text: text.into() });
        }
        let progress = |call: &str, preview: &str| UiEvent::ToolProgress {
            batch_id: crate::api::ToolBatchId("batch".into()),
            call_id: crate::api::ToolCallId(call.into()),
            name: "read".into(),
            preview: preview.into(),
            content_handle: None,
        };
        coalescer.push_data(progress("one", "old"));
        coalescer.push_data(progress("one", "latest"));
        coalescer.push_data(progress("two", "other"));

        let events = coalescer.flush();
        assert_eq!(
            events[0],
            UiEvent::ThinkingDelta {
                text: "reason continued".into()
            }
        );
        assert!(matches!(
            &events[1],
            UiEvent::ToolProgress { call_id, preview, .. }
                if call_id.0.as_ref() == "one" && preview == "latest"
        ));
        assert!(matches!(
            &events[2],
            UiEvent::ToolProgress { call_id, preview, .. }
                if call_id.0.as_ref() == "two" && preview == "other"
        ));
    }

    #[test]
    fn tool_progress_coalescing_preserves_handle_and_replaces_new_one() {
        let batch_id = ToolBatchId("batch-1".into());
        let call_id = ToolCallId("call-1".into());
        let output_handle = ContentHandle("output-handle".into());
        let progress =
            |preview: &str, content_handle: Option<ContentHandle>| UiEvent::ToolProgress {
                batch_id: batch_id.clone(),
                call_id: call_id.clone(),
                name: "shell".into(),
                preview: preview.into(),
                content_handle,
            };
        let process_finished = UiEvent::from_core(SessionEvent::new(
            2,
            EventKind::ToolProcessFinished {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "shell".into(),
                process: slim_core::process::ProcessExecutionFacts {
                    exit_code: Some(7),
                    timed_out: true,
                    cancelled: false,
                    stdout_bytes: 12,
                    stderr_bytes: 8,
                    stdout_discarded_bytes: 3,
                    stderr_discarded_bytes: 4,
                },
            },
        ))
        .expect("process-finished projection");
        assert!(matches!(
            &process_finished,
            UiEvent::ToolProgress {
                batch_id: projected_batch,
                call_id: projected_call,
                content_handle: None,
                preview,
                ..
            } if projected_batch == &batch_id
                && projected_call == &call_id
                && preview == "exit 7 · timed out · discarded 7 B"
        ));

        let mut coalescer = EventCoalescer::new(16, Duration::from_millis(16));
        coalescer.push_data(progress("full output", Some(output_handle.clone())));
        coalescer.push_data(process_finished);
        assert_eq!(
            coalescer.flush(),
            vec![progress(
                "exit 7 · timed out · discarded 7 B",
                Some(output_handle),
            )]
        );

        let old_handle = ContentHandle("old-handle".into());
        let replacement_handle = ContentHandle("replacement-handle".into());
        coalescer.push_data(progress("replacement", Some(old_handle)));
        coalescer.push_data(progress(
            "latest replacement",
            Some(replacement_handle.clone()),
        ));
        assert_eq!(
            coalescer.flush(),
            vec![progress("latest replacement", Some(replacement_handle))]
        );
    }
}

pub fn render(state: &AppState, width: u16, height: u16) -> Frame {
    let regions = layout::plan_with_session_rail(
        width,
        height,
        layout::todo_height(
            state.todo_dock_open,
            state.todo_items.len(),
            state
                .todo_items
                .iter()
                .any(|item| item.status == crate::api::TodoItemStatus::InProgress),
        ),
        state.working || state.activity.is_some(),
        !state.blocks().is_empty()
            && !crate::view_model::is_trivial_cwd(&state.cwd)
            && width >= 80
            && height >= 12,
    );
    ViewModel::derive_with_session_rail(state, regions.session_rail.height > 0, width)
}

/// Measured wrapped height of plain block text at a viewport width (§12.2).
/// Uses the same sanitized grapheme-greedy contract as materialization.
pub fn wrapped_row_count(text: &str, width: usize) -> usize {
    crate::markdown::plain_row_count(text, width.min(u16::MAX as usize) as u16)
}

/// Cells before the user prompt text on each band row (`"  You  "`).
pub(crate) const USER_PROMPT_PREFIX_COLS: u16 = 7;

pub(crate) fn user_prompt_text_width(width: u16) -> u16 {
    width.saturating_sub(USER_PROMPT_PREFIX_COLS).max(1)
}

/// Align reasoning text beneath the header after its marker and disclosure glyph.
pub(crate) fn thinking_body_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
}

fn block_height(block: &Block, width: u16, cache: &mut WrapCache) -> usize {
    let body_width = width.saturating_sub(2).max(1) as usize; // rail/padding column
    let content_rows = match block.kind() {
        BlockKind::User(text) => {
            wrapped_row_count(text, user_prompt_text_width(width) as usize) + 1
        }
        BlockKind::Assistant(text) => {
            let text_width = if body_width > 1 {
                body_width - 1 // reserved streaming-caret cell, stable after completion
            } else {
                body_width
            };
            // The measured projection is shared with the render pass, so the
            // streaming body is parsed once per generation instead of twice.
            2 + cache.markdown_rows(block, text, text_width as u16)
        }
        BlockKind::Thinking(text) => match block.fold {
            crate::block::FoldState::Expanded => {
                1 + cache.cached_body_rows(
                    block,
                    BodyKind::Thinking,
                    width,
                    block.lifecycle != crate::block::BlockLifecycle::Streaming,
                    || wrapped_row_count(text, thinking_body_width(width) as usize),
                )
            }
            _ if block.shows_thinking_preview() => {
                1 + cache
                    .thinking_preview(block, text, thinking_body_width(width))
                    .0
                    .len()
            }
            _ => 1,
        },
        // Tools/system/activity render as one summary row. Error and queued
        // prompt preserve their full multiline payload behind a four-cell role
        // prefix, using the same physical-row contract as materialization.
        BlockKind::Tool(state) => {
            if block.fold == crate::block::FoldState::Expanded
                && !state.materialized_output.is_empty()
            {
                1 + cache.cached_body_rows(
                    block,
                    BodyKind::ToolOutput,
                    width,
                    block.lifecycle != crate::block::BlockLifecycle::Streaming,
                    || {
                        wrapped_row_count(
                            &state.materialized_output,
                            width.saturating_sub(4).max(1) as usize,
                        )
                    },
                )
            } else {
                1
            }
        }
        BlockKind::InteractionRequest(state) => {
            if state.acknowledgement.is_none() {
                0
            } else if matches!(
                state.kind,
                crate::block::InteractionRequestKind::Question { .. }
            ) {
                state
                    .layout_lines(width.saturating_sub(2).max(8) as usize)
                    .len()
            } else {
                crate::markdown::plain_row_count(
                    &state.display_text(),
                    width.saturating_sub(4).max(1),
                )
            }
        }
        BlockKind::System(_) | BlockKind::Activity(_) => 1,
        BlockKind::Error(text) | BlockKind::QueuedUser(text) => {
            crate::markdown::plain_row_count(text, width.saturating_sub(4).max(1))
        }
    };
    content_rows + usize::from(block.turn_boundary_before())
}

/// Visible-range index over prefix sums (§13.2): exact heights measured via
/// the wrap cache, first visible block found in O(log n). Rebuilt per frame —
/// building is O(n) over cached heights, no per-frame wrapping.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScrollMetrics {
    pub viewport_start: u64,
    pub viewport_rows: u64,
    pub total_rows: u64,
    pub bottom_start: u64,
    pub top_anchor: Option<ScrollAnchor>,
    pub up_anchor: Option<ScrollAnchor>,
    pub down_anchor: Option<ScrollAnchor>,
    pub page_up_anchor: Option<ScrollAnchor>,
    pub page_down_anchor: Option<ScrollAnchor>,
    pub last_visible_foldable_anchor: Option<ScrollAnchor>,
}

fn grouped_member_rows(leader: &Block, member_count: usize) -> u64 {
    if leader.fold == crate::block::FoldState::Expanded {
        1u64.saturating_add(member_count as u64)
    } else {
        1
    }
}

fn record_grouped_member_rows(
    block_rows: &mut HashMap<crate::api::BlockId, (u64, u64)>,
    prefix: u64,
    leader: &Block,
    members: &[Block],
    rows: u64,
    member_is_tool: impl Fn(&Block) -> bool,
) -> u64 {
    let mut visible_index = 0usize;
    for member in members {
        let is_tool = member_is_tool(member);
        let (member_prefix, member_rows) = if !is_tool {
            (prefix, 0)
        } else if visible_index == 0 {
            (prefix, rows)
        } else if leader.fold == crate::block::FoldState::Expanded {
            (prefix.saturating_add(1 + visible_index as u64), 1)
        } else {
            (prefix, 1)
        };
        if is_tool {
            visible_index = visible_index.saturating_add(1);
        }
        block_rows
            .entry(member.id.clone())
            .or_insert((member_prefix, member_rows));
    }
    prefix.saturating_add(rows)
}

pub struct HeightIndex<'a> {
    /// The block slice spans resolve against.
    blocks: &'a [Block],
    /// Shareable, borrow-free index content: (prefix, leader index, member
    /// span start..end) plus the per-block row map. A cached `Arc` is reused
    /// as-is while (content, fold, width) are unchanged — no O(n) rebuild.
    memo: Arc<HeightIndexMemo>,
    pub total_rows: u64,
}

/// Borrow-free, immutable index content shared between the built index and
/// the `WrapCache::height_indexes` memo table. Spans are resolved against the
/// block slice on demand via [`HeightIndex::entry`].
pub(crate) struct HeightIndexMemo {
    spans: Vec<(u64, usize, usize, usize)>,
    block_rows: HashMap<crate::api::BlockId, (u64, u64)>,
    total_rows: u64,
}

impl<'a> HeightIndex<'a> {
    /// (prefix_rows_before, leader, members) — consecutive successful tools
    /// from one provider batch are grouped for presentation only (§11.4.1).
    pub fn entry(&self, index: usize) -> (u64, &'a Block, &'a [Block]) {
        let &(prefix, leader, start, end) = &self.memo.spans[index];
        (prefix, &self.blocks[leader], &self.blocks[start..end])
    }

    pub fn len(&self) -> usize {
        self.memo.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.memo.spans.is_empty()
    }

    pub fn build(blocks: &'a [Block], width: u16, cache: &mut WrapCache) -> Self {
        let mut block_rows = HashMap::with_capacity(blocks.len());
        let mut spans = Vec::with_capacity(blocks.len());
        let mut pending_heights = Vec::new();
        let mut prefix = 0u64;
        let mut index = 0usize;
        while index < blocks.len() {
            let block = &blocks[index];
            if crate::block::is_complete_tool(block) {
                if let Some((start, end)) =
                    crate::block::consecutive_complete_tool_span(blocks, index)
                {
                    let tool_count = crate::block::complete_tool_count(blocks, start, end);
                    if tool_count > 1 && start == index {
                        let rows = grouped_member_rows(block, tool_count);
                        let entry_prefix = prefix;
                        prefix = record_grouped_member_rows(
                            &mut block_rows,
                            prefix,
                            block,
                            &blocks[start..end],
                            rows,
                            crate::block::is_complete_tool,
                        );
                        spans.push((entry_prefix, start, start, end));
                        index = end;
                        continue;
                    }
                }
            }
            if crate::block::is_failed_tool(block) {
                if let Some((start, end)) =
                    crate::block::consecutive_identical_failed_tool_span(blocks, index)
                {
                    let tool_count = end.saturating_sub(start);
                    if tool_count > 1 && start == index {
                        let rows = grouped_member_rows(block, tool_count);
                        let entry_prefix = prefix;
                        prefix = record_grouped_member_rows(
                            &mut block_rows,
                            prefix,
                            block,
                            &blocks[start..end],
                            rows,
                            crate::block::is_failed_tool,
                        );
                        spans.push((entry_prefix, start, start, end));
                        index = end;
                        continue;
                    }
                }
            }
            if crate::block::is_complete_thinking(block) {
                if let Some((start, end)) =
                    crate::block::consecutive_complete_thinking_span(blocks, index)
                {
                    let count = end.saturating_sub(start);
                    if count > 1 && start == index {
                        let members = &blocks[start..end];
                        let rows = if block.fold == crate::block::FoldState::Expanded {
                            1u64.saturating_add(
                                members
                                    .iter()
                                    .map(|member| {
                                        cache.thinking_body_height(
                                            member,
                                            width,
                                            &mut pending_heights,
                                        ) as u64
                                    })
                                    .sum(),
                            )
                        } else {
                            1
                        };
                        let entry_prefix = prefix;
                        prefix = record_grouped_member_rows(
                            &mut block_rows,
                            prefix,
                            block,
                            members,
                            rows,
                            crate::block::is_complete_thinking,
                        );
                        spans.push((entry_prefix, start, start, end));
                        index = end;
                        continue;
                    }
                }
            }
            let key = (
                block.cache_identity(),
                block.content_generation(),
                width,
                u8::from(block.fold == crate::block::FoldState::Collapsed),
                block.lifecycle_tag(),
            );
            let height = if let Some(height) = cache.heights.get(&key).copied() {
                height
            } else {
                cache.height_misses = cache.height_misses.saturating_add(1);
                let height = block_height(block, width, cache);
                pending_heights.push((key, height));
                height
            };
            let rows = height as u64;
            let boundary_rows = u64::from(block.turn_boundary_before());
            block_rows.entry(block.id.clone()).or_insert((
                prefix.saturating_add(boundary_rows),
                rows.saturating_sub(boundary_rows).max(1),
            ));
            spans.push((prefix, index, index, index + 1));
            prefix = prefix.saturating_add(rows);
            index += 1;
        }
        // Defer inserts until the scan ends. Eager FIFO insertion can evict
        // the very next key and cascade into an all-miss frame at capacity+1.
        cache.remember_heights(pending_heights);
        Self {
            blocks,
            total_rows: prefix,
            memo: Arc::new(HeightIndexMemo {
                spans,
                block_rows,
                total_rows: prefix,
            }),
        }
    }

    pub fn prefix_for_block(&self, id: &crate::api::BlockId) -> Option<u64> {
        self.memo.block_rows.get(id).map(|(prefix, _)| *prefix)
    }

    pub fn row_for_anchor(&self, anchor: &ScrollAnchor) -> Option<u64> {
        self.memo
            .block_rows
            .get(&anchor.block_id)
            .map(|(prefix, rows)| {
                prefix.saturating_add(anchor.row_offset.min(rows.saturating_sub(1)))
            })
    }

    pub fn anchor_for_row(&self, row: u64) -> Option<ScrollAnchor> {
        if self.memo.spans.is_empty() || row >= self.total_rows {
            return None;
        }
        let (index, row_offset) = self.locate(row);
        Some(ScrollAnchor {
            block_id: self.entry(index).1.id.clone(),
            row_offset,
        })
    }

    pub fn metrics(&self, mode: &FollowMode, viewport_rows: u64) -> ScrollMetrics {
        let has_visible_viewport = viewport_rows > 0;
        let effective_viewport_rows = viewport_rows.max(1);
        let bottom_start = self.total_rows.saturating_sub(effective_viewport_rows);
        let viewport_start = match mode {
            FollowMode::Top => 0,
            FollowMode::Pinned(anchor) => {
                self.row_for_anchor(anchor).unwrap_or(0).min(bottom_start)
            }
            FollowMode::LiveEdge { prompt_id } => prompt_id
                .as_ref()
                .and_then(|id| self.prefix_for_block(id))
                .filter(|prefix| self.total_rows.saturating_sub(*prefix) <= effective_viewport_rows)
                .unwrap_or(bottom_start),
        };
        let page = effective_viewport_rows;
        // Page-fill may start after the conventional full-page bottom. The
        // first upward gesture enters a fully populated historical viewport so
        // later appends cannot fill blank rows in a pinned view.
        let scroll_start = viewport_start.min(bottom_start);
        let up = scroll_start.saturating_sub(1);
        let down = scroll_start.saturating_add(1).min(bottom_start);
        let page_up = scroll_start.saturating_sub(page);
        let page_down = scroll_start.saturating_add(page).min(bottom_start);
        let viewport_end = viewport_start.saturating_add(viewport_rows);
        let last_visible_foldable_anchor = has_visible_viewport
            .then(|| self.last_visible_foldable_anchor(viewport_start, viewport_end))
            .flatten();
        ScrollMetrics {
            viewport_start,
            viewport_rows,
            total_rows: self.total_rows,
            bottom_start,
            top_anchor: self.anchor_for_row(viewport_start),
            up_anchor: self.anchor_for_row(up),
            down_anchor: self.anchor_for_row(down),
            page_up_anchor: self.anchor_for_row(page_up),
            page_down_anchor: self.anchor_for_row(page_down),
            last_visible_foldable_anchor,
        }
    }

    /// Index of the first entry intersecting `row`, plus rows to skip inside it.
    pub fn locate(&self, row: u64) -> (usize, u64) {
        if self.memo.spans.is_empty() {
            return (0, 0);
        }
        let idx = self
            .memo
            .spans
            .partition_point(|(prefix, _, _, _)| *prefix <= row)
            .saturating_sub(1)
            .min(self.memo.spans.len().saturating_sub(1));
        let skipped = row.saturating_sub(self.memo.spans[idx].0);
        (idx, skipped)
    }

    fn last_visible_foldable_anchor(
        &self,
        viewport_start: u64,
        viewport_end: u64,
    ) -> Option<ScrollAnchor> {
        let mut index = self
            .memo
            .spans
            .partition_point(|(prefix, _, _, _)| *prefix < viewport_end);
        while index > 0 {
            index -= 1;
            let (prefix, leader, start, end) = self.memo.spans[index];
            let block = &self.blocks[leader];
            let members = &self.blocks[start..end];
            let Some(&(_, rows)) = self.memo.block_rows.get(&block.id) else {
                continue;
            };
            if prefix.saturating_add(rows) <= viewport_start {
                break;
            }
            let foldable_tool = matches!(block.kind(), BlockKind::Tool(state)
                if members.len() > 1 || state.content_handle.is_some());
            if matches!(block.kind(), BlockKind::Thinking(_)) || foldable_tool {
                return Some(ScrollAnchor {
                    block_id: block.id.clone(),
                    row_offset: 0,
                });
            }
        }
        None
    }
}

/// Which heavy body renderer produced a cached line block. Kept as a plain
/// byte so the cache key stays cheap to hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyKind {
    User,
    Assistant,
    Thinking,
    ToolOutput,
    Interaction,
    Error,
    QueuedUser,
}

impl BodyKind {
    fn tag(self) -> u8 {
        match self {
            Self::User => 0,
            Self::Assistant => 1,
            Self::Thinking => 2,
            Self::ToolOutput => 3,
            Self::Interaction => 4,
            Self::Error => 5,
            Self::QueuedUser => 6,
        }
    }
}

/// The inspector rows are styled at paint time, while keyboard navigation only
/// needs their palette-independent row count. Keep the palette in the single
/// inspector memo key so a NoColor measurement can never poison a later paint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct InspectorPaletteKey([Style; 7]);

impl InspectorPaletteKey {
    pub(crate) fn new(styles: [Style; 7]) -> Self {
        Self(styles)
    }
}

type InspectorMemoBaseKey = (Option<InspectorKind>, u64, u64, u64, u16);
type InspectorMemoKey = (InspectorMemoBaseKey, InspectorPaletteKey);

fn inspector_memo_base_key(
    state: &AppState,
    kind: Option<InspectorKind>,
    width: u16,
) -> InspectorMemoBaseKey {
    (
        kind,
        state.revisions.content,
        state.revisions.status,
        state.clock.elapsed_ms / 1_000,
        width,
    )
}

/// Bounded caches for the render pipeline (§12.3/§12.4): heights keyed by
/// block generation, width and layout kind (folded/full/grouped body).
/// Eviction only drops derivations.
pub struct WrapCache {
    pub(crate) painted_selection: Option<crate::selection::PaintedSelection>,
    pub(crate) selection_scroll_anchor: Option<ScrollAnchor>,
    thinking_previews: Vec<((u64, u16), ThinkingPreview)>,
    finished_previews: WeightedCache<(u64, u64, u16), (Vec<String>, bool)>,
    /// Content hit regions from the last painted frame: transcript, inspector.
    pub(crate) selection_regions: [Option<ratatui::layout::Rect>; 2],
    heights: BoundedCache<HeightKey, usize>,
    height_misses: u64,
    /// Wrapped body lines keyed by (identity, generation, width, body kind).
    /// Byte-weighted so a few large bodies cannot exhaust memory. Stable blocks
    /// are re-wrapped every frame today; this makes them render from the last
    /// wrap until their content changes.
    bodies: WeightedCache<(u64, u64, u16, u8), Vec<RatatuiLine<'static>>>,
    /// Bodies that bypass the LRU (streaming or oversized): a few most-recent
    /// derivations kept so height measurement and paint share one wrap per
    /// content generation instead of re-wrapping every frame.
    scratch_bodies: Vec<(BodyKey, Arc<Vec<RatatuiLine<'static>>>)>,
    /// Shared markdown projections keyed by (identity, generation, body width):
    /// one pulldown-cmark parse feeds both row counting and body rendering.
    #[allow(clippy::type_complexity)]
    projections: Vec<((u64, u64, u16), Arc<Vec<LogicalLine>>)>,
    /// Built height indexes keyed by (content rev, fold rev, width): scroll
    /// gestures and churn-free frames re-render without an O(n) rebuild.
    height_indexes: HashMap<(u64, u64, u16), Arc<HeightIndexMemo>>,
    /// Transcript search results keyed by (query, filter, content rev).
    #[allow(clippy::type_complexity)]
    search_memo: Option<((String, SearchFilter, u64), Arc<[usize]>)>,
    /// Foldable selected-block probe keyed by (content, fold, scroll mode).
    selected_memo: Option<((u64, u64, FollowMode), Option<BlockId>)>,
    /// Last collapsed tool-group leader keyed by (content, fold).
    tool_leader_memo: Option<((u64, u64), Option<BlockId>)>,
    /// Composer display snapshot keyed by (composer revision, width).
    composer_memo: Option<((u64, u16), DisplaySnapshot)>,
    /// Slash completion matches keyed by (query, skill list revision).
    #[allow(clippy::type_complexity)]
    slash_memo: Option<((String, u64), Arc<Vec<String>>)>,
    /// Model overlay flattened rows keyed by (filter, collapsed, catalog rev).
    #[allow(clippy::type_complexity)]
    model_rows_memo: Option<((String, [bool; 5], u64), Arc<Vec<ModelRow>>)>,
    /// Inspector panel lines keyed by (kind, content, status, second, width,
    /// palette). The row count is retained independently of the palette so a
    /// keyboard-only probe can reuse it without rebuilding styled lines.
    #[allow(clippy::type_complexity)]
    inspector_memo: Option<(InspectorMemoKey, Arc<Vec<RatatuiLine<'static>>>)>,
    /// Command palette matches keyed by the query — the open palette no
    /// longer re-filters the static command list per frame.
    palette_memo: Option<(String, Arc<Vec<&'static str>>)>,
    streaming_blocks_memo: Option<(u64, bool)>,
    /// Fully-rendered block lines (header + body + boundary) keyed by
    /// [`BlockLinesKey`]. Stable blocks paint into the frame buffer by
    /// reference; only changed or animated blocks re-materialize.
    block_line_memos: WeightedCache<BlockLinesKey, Arc<Vec<RatatuiLine<'static>>>>,
    /// Footer/status rows keyed by their explicit inputs — the candidate
    /// strings are built once per state change, not per frame.
    footer_memo: Option<(FooterKey, Arc<Vec<String>>)>,
    body_hits: u64,
    body_misses: u64,
    body_bypasses: u64,
    body_oversized_skips: u64,
}

// (identity, content generation, width, layout tag, lifecycle). Lifecycle is
// assigned directly without `touch_content`, so it cannot ride on the
// generation counter — a collapsed thinking block leaving `Streaming` must
// miss the cached preview height.
type HeightKey = (u64, u64, u16, u8, u8);
type BodyKey = (u64, u64, u16, u8);
/// (leader identity, member-state fold, flags, width) — see
/// [`WrapCache::get_block_lines`]. Animation is painted as a one-cell patch
/// after a cache hit, so the clock never invalidates stable block lines.
type BlockLinesKey = (u64, u64, u8, u16);

/// Footer inputs that decide the rendered rows. Values, not just revisions,
/// so the memo can never go stale when a field changes without a revision
/// bump (lifecycle/fold-style direct writes exist elsewhere in the state).
#[derive(Clone, Eq, PartialEq)]
struct FooterKey {
    working: bool,
    mode: slim_core::OperatingMode,
    pinned: bool,
    unseen: u32,
    authenticated: bool,
    activity: Option<crate::app::ActivityPhase>,
    retry: Option<crate::app::RetryState>,
    cancellation: Option<crate::app::CancellationState>,
    last_execution: Option<crate::app::ExecutionSummary>,
    retry_second: u64,
    context_tokens: u64,
    context_window_tokens: u64,
    content_rev: u64,
    status_rev: u64,
    width: u16,
    rows: u16,
    activity_visible: bool,
}

const SCRATCH_BODY_SLOTS: usize = 4;
const SCRATCH_PROJECTION_SLOTS: usize = 4;
const MAX_HEIGHT_INDEX_MEMOS: usize = 4;
const MAX_BLOCK_LINE_MEMOS: usize = 256;
const MAX_BLOCK_LINE_MEMO_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHED_BLOCK_LINES_BYTES: usize = 256 * 1024;

/// Single-slot memo: recomputes only when `key` changes. Every consumer is a
/// pure derivation of state, so a kept value can never be semantically stale.
fn memoized<K: PartialEq, V>(slot: &mut Option<(K, V)>, key: K, produce: impl FnOnce() -> V) -> &V {
    if slot.as_ref().is_none_or(|(stored, _)| *stored != key) {
        *slot = Some((key, produce()));
    }
    &slot.as_ref().expect("memo slot populated").1
}

impl WrapCache {
    pub(crate) fn thinking_preview(
        &mut self,
        block: &Block,
        text: &str,
        width: u16,
    ) -> (Vec<String>, bool) {
        let finished_key = (block.cache_identity(), block.content_generation(), width);
        let stable = block.lifecycle != crate::block::BlockLifecycle::Streaming;
        if stable {
            if let Some(preview) = self.finished_previews.get(&finished_key) {
                return preview.clone();
            }
        }
        let key = (block.cache_identity(), width);
        if !self
            .thinking_previews
            .iter()
            .any(|(stored, _)| *stored == key)
        {
            if self.thinking_previews.len() == 4 {
                self.thinking_previews.remove(0);
            }
            self.thinking_previews.push((
                key,
                ThinkingPreview {
                    generation: u64::MAX,
                    ..Default::default()
                },
            ));
        }
        let (_, preview) = self
            .thinking_previews
            .iter_mut()
            .find(|(stored, _)| *stored == key)
            .expect("preview inserted");
        preview.update(text, block.content_generation(), width);
        let result = (preview.rows.clone(), preview.hidden);
        if stable {
            let bytes = result
                .0
                .iter()
                .map(|row| row.len() + std::mem::size_of::<String>())
                .sum();
            self.finished_previews
                .insert(finished_key, result.clone(), bytes);
        }
        result
    }

    /// Height index for the current frame: shares the memoized index content
    /// when (content, fold, width) are unchanged, built and memoized otherwise.
    pub(crate) fn height_index<'a>(
        &mut self,
        blocks: &'a [Block],
        content_rev: u64,
        fold_rev: u64,
        width: u16,
    ) -> HeightIndex<'a> {
        let key = (content_rev, fold_rev, width);
        if let Some(memo) = self.height_indexes.get(&key) {
            return HeightIndex {
                blocks,
                memo: Arc::clone(memo),
                total_rows: memo.total_rows,
            };
        }
        let index = HeightIndex::build(blocks, width, self);
        self.height_indexes
            .retain(|(c, f, _), _| *c == content_rev && *f == fold_rev);
        if self.height_indexes.len() >= MAX_HEIGHT_INDEX_MEMOS {
            self.height_indexes.clear();
        }
        self.height_indexes.insert(key, Arc::clone(&index.memo));
        index
    }

    pub(crate) fn has_streaming_block(&mut self, blocks: &[Block], content_rev: u64) -> bool {
        *memoized(&mut self.streaming_blocks_memo, content_rev, || {
            blocks
                .iter()
                .any(|block| block.lifecycle == crate::block::BlockLifecycle::Streaming)
        })
    }

    /// Markdown row count sharing the parse with the render pass: the
    /// projection produced here is reused by `wrapped_body` callers within
    /// the same content generation (the streaming tail's hot path).
    pub(crate) fn markdown_rows(&mut self, block: &Block, text: &str, width: u16) -> usize {
        let projection = self.markdown_projection(block, text, width);
        crate::markdown::projected_row_count(&projection, width)
    }

    /// The shared parse result for a markdown body, deduplicated across the
    /// height probe and the render pass for the same (block, generation,
    /// width).
    pub(crate) fn markdown_projection(
        &mut self,
        block: &Block,
        text: &str,
        width: u16,
    ) -> Arc<Vec<LogicalLine>> {
        let key = (block.cache_identity(), block.content_generation(), width);
        if let Some((_, projection)) = self.projections.iter().find(|(stored, _)| *stored == key) {
            return projection.clone();
        }
        let projection = Arc::new(crate::markdown::project_markdown(text, width));
        if self.projections.len() >= SCRATCH_PROJECTION_SLOTS {
            self.projections.remove(0);
        }
        self.projections.push((key, projection.clone()));
        projection
    }

    /// Row count reusing whichever body store applies: a hit returns the
    /// stored line count, a miss falls back to `count` — never produces a
    /// body just to measure it.
    pub(crate) fn cached_body_rows(
        &mut self,
        block: &Block,
        kind: BodyKind,
        width: u16,
        cacheable: bool,
        count: impl FnOnce() -> usize,
    ) -> usize {
        let key = (
            block.cache_identity(),
            block.content_generation(),
            width,
            kind.tag(),
        );
        if cacheable {
            if let Some(body) = self.bodies.get(&key) {
                return body.len();
            }
        } else if let Some((_, body)) = self
            .scratch_bodies
            .iter()
            .find(|(stored, _)| *stored == key)
        {
            return body.len();
        }
        count()
    }

    /// Transcript search results, recomputed only when the query, filter or
    /// block set changes — the open search bar no longer rescans every frame.
    pub(crate) fn search_matches(
        &mut self,
        blocks: &[Block],
        query: &str,
        filter: SearchFilter,
        content_rev: u64,
    ) -> Arc<[usize]> {
        memoized(
            &mut self.search_memo,
            (query.to_owned(), filter, content_rev),
            || crate::inspector::search_match_indices_filtered(blocks, query, filter).into(),
        )
        .clone()
    }

    /// Foldable block under the stable scroll anchor; the O(n) probe is
    /// skipped while (content, fold, scroll mode) are unchanged.
    pub(crate) fn selected_block(&mut self, state: &AppState) -> Option<BlockId> {
        memoized(
            &mut self.selected_memo,
            (
                state.revisions.content,
                state.revisions.fold,
                state.scroll.mode.clone(),
            ),
            || state.selected_block_id().cloned(),
        )
        .clone()
    }

    /// The most recent collapsed tool-group leader while a run is active;
    /// memoized over (content, fold) so working frames stop rescanning.
    pub(crate) fn tool_group_leader(
        &mut self,
        blocks: &[Block],
        content_rev: u64,
        fold_rev: u64,
    ) -> Option<BlockId> {
        memoized(&mut self.tool_leader_memo, (content_rev, fold_rev), || {
            crate::runtime::last_collapsed_tool_group_leader(blocks)
        })
        .clone()
    }

    /// Composer display snapshot keyed by composer revision and width; the
    /// O(draft) rebuild only runs after an actual edit or resize.
    pub(crate) fn composer_snapshot(
        &mut self,
        composer: &Composer,
        width: u16,
    ) -> &DisplaySnapshot {
        memoized(
            &mut self.composer_memo,
            (composer.revision(), width),
            || composer.display_snapshot(width as usize),
        )
    }

    /// Slash completion matches; the filter runs only when the query or the
    /// skill list changes, not per frame while the popup is open.
    pub(crate) fn slash_matches(&mut self, state: &AppState, query: &str) -> Arc<Vec<String>> {
        memoized(
            &mut self.slash_memo,
            (query.to_owned(), state.skills_revision()),
            || Arc::new(crate::reducer::slash_matches_with_skills(state, query)),
        )
        .clone()
    }

    /// Flattened model-overlay rows keyed by (filter, collapsed, catalogs).
    /// Mirrors `ModelOverlay::for_current`: the catalogs stay explicit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn model_rows(
        &mut self,
        overlay: &ModelOverlay,
        opencode: &[crate::api::OpenCodeModelView],
        clinepass: &[crate::api::OpenCodeModelView],
        command_code: &[crate::api::OpenCodeModelView],
        zen: &[crate::api::OpenCodeModelView],
        catalog_rev: u64,
    ) -> Arc<Vec<ModelRow>> {
        memoized(
            &mut self.model_rows_memo,
            (overlay.filter.clone(), overlay.collapsed, catalog_rev),
            || Arc::new(overlay.rows(opencode, clinepass, command_code, zen)),
        )
        .clone()
    }

    /// Inspector panel lines keyed by (kind, content, status, wall second,
    /// width, palette) — the open inspector no longer rescans the transcript
    /// per frame. `None` renders the run summary.
    pub(crate) fn inspector_lines(
        &mut self,
        state: &AppState,
        kind: Option<InspectorKind>,
        palette: &crate::runtime::Palette,
        width: u16,
        palette_key: InspectorPaletteKey,
    ) -> Arc<Vec<RatatuiLine<'static>>> {
        let base = inspector_memo_base_key(state, kind, width);
        let key = (base, palette_key);
        if let Some((stored, lines)) = &self.inspector_memo {
            if *stored == key {
                return lines.clone();
            }
        }
        let lines = Arc::new(match kind {
            Some(kind) => crate::runtime::inspector_lines(state, kind, palette, width),
            None => crate::runtime::run_inspector_lines(state, palette, width),
        });
        // Color changes cannot change wrapping or row count; replacing the
        // styled projection keeps the same semantic key available to probes.
        self.inspector_memo = Some((key, lines.clone()));
        lines
    }

    /// Returns the current inspector projection and its palette-independent
    /// row count. Existing semantic input reuses the stored count even when a
    /// renderer palette changed, so keyboard navigation never rebuilds rows.
    pub(crate) fn inspector_line_metrics(
        &mut self,
        state: &AppState,
        kind: Option<InspectorKind>,
        palette: &crate::runtime::Palette,
        width: u16,
        palette_key: InspectorPaletteKey,
    ) -> Arc<Vec<RatatuiLine<'static>>> {
        let base = inspector_memo_base_key(state, kind, width);
        if let Some((stored, lines)) = &self.inspector_memo {
            if stored.0 == base {
                return lines.clone();
            }
        }
        let lines = Arc::new(match kind {
            Some(kind) => crate::runtime::inspector_lines(state, kind, palette, width),
            None => crate::runtime::run_inspector_lines(state, palette, width),
        });
        self.inspector_memo = Some(((base, palette_key), lines.clone()));
        lines
    }

    /// Command palette matches keyed by query; the filter over the static
    /// command list runs once per edit, not per frame.
    pub(crate) fn palette_matches(&mut self, query: &str) -> Arc<Vec<&'static str>> {
        memoized(&mut self.palette_memo, query.to_owned(), || {
            Arc::new(crate::reducer::palette_matches(query))
        })
        .clone()
    }

    /// Lookup for the per-block rendered-lines memo. The produce step needs
    /// `&mut WrapCache`, so get/store are split instead of a `memoized` call.
    pub(crate) fn get_block_lines(
        &mut self,
        key: &BlockLinesKey,
    ) -> Option<Arc<Vec<RatatuiLine<'static>>>> {
        self.block_line_memos.get(key).cloned()
    }

    /// Stores produced block lines under `key`. Oversized entries bypass the
    /// cache but are still returned — the frame always paints.
    pub(crate) fn store_block_lines(
        &mut self,
        key: BlockLinesKey,
        lines: Vec<RatatuiLine<'static>>,
    ) -> Arc<Vec<RatatuiLine<'static>>> {
        let bytes = cached_lines_bytes(&lines);
        let lines = Arc::new(lines);
        if bytes <= MAX_CACHED_BLOCK_LINES_BYTES {
            self.block_line_memos.insert(key, lines.clone(), bytes);
        }
        lines
    }

    /// Footer rows keyed by every input `footer_lines` reads — revisions plus
    /// the raw values, so nothing can serve stale rows.
    pub(crate) fn footer_lines(
        &mut self,
        state: &AppState,
        width: u16,
        rows: u16,
        activity_visible: bool,
    ) -> Arc<Vec<String>> {
        let key = FooterKey {
            working: state.working,
            mode: state.mode,
            pinned: state.scroll.is_pinned(),
            unseen: state.scroll.unseen,
            authenticated: state.authenticated,
            activity: state
                .activity
                .as_ref()
                .map(|activity| activity.phase.clone()),
            retry: state.retry.clone(),
            cancellation: state.cancellation,
            last_execution: state.last_execution.clone(),
            retry_second: if !activity_visible && state.retry.is_some() {
                state.clock.elapsed_ms / 1_000
            } else {
                0
            },
            context_tokens: state.context_tokens,
            context_window_tokens: state.context_window_tokens,
            content_rev: state.revisions.content,
            status_rev: state.revisions.status,
            width,
            rows,
            activity_visible,
        };
        memoized(&mut self.footer_memo, key, || {
            Arc::new(crate::view_model::footer_lines(
                state,
                width as usize,
                rows,
                activity_visible,
            ))
        })
        .clone()
    }

    pub(crate) fn remember_heights(&mut self, pending: Vec<(HeightKey, usize)>) {
        for (key, height) in pending {
            self.heights.insert(key, height);
        }
    }

    // Tag 2 measures only a grouped thinking member's body. It must not alias
    // standalone block heights (tags 0/1), which include headings/boundaries.
    pub(crate) fn thinking_body_height(
        &mut self,
        block: &Block,
        width: u16,
        pending: &mut Vec<(HeightKey, usize)>,
    ) -> usize {
        let key = (
            block.cache_identity(),
            block.content_generation(),
            width,
            2,
            block.lifecycle_tag(),
        );
        if let Some(height) = self.heights.get(&key) {
            return *height;
        }
        self.height_misses = self.height_misses.saturating_add(1);
        let height = match block.kind() {
            BlockKind::Thinking(text) => {
                wrapped_row_count(text, thinking_body_width(width) as usize)
            }
            _ => 1,
        };
        pending.push((key, height));
        height
    }

    pub fn height_misses(&self) -> u64 {
        self.height_misses
    }

    pub fn body_hits(&self) -> u64 {
        self.body_hits
    }

    pub fn body_misses(&self) -> u64 {
        self.body_misses
    }

    pub fn body_bypasses(&self) -> u64 {
        self.body_bypasses
    }

    pub fn body_oversized_skips(&self) -> u64 {
        self.body_oversized_skips
    }

    pub fn body_evictions(&self) -> u64 {
        self.bodies.evictions()
    }

    pub fn body_retained_bytes(&self) -> usize {
        self.bodies.retained_bytes()
    }

    /// Looks up the cached wrapped body for a stable block, or computes and
    /// stores it via `produce` on a miss. Non-cacheable blocks (streaming or
    /// oversized) reuse a small scratch slot per content generation so the
    /// streaming tail wraps once per frame rather than once per use.
    pub fn wrapped_body<F, G>(
        &mut self,
        block: &crate::block::Block,
        kind: BodyKind,
        width: u16,
        cacheable: bool,
        produce: F,
        weight: G,
    ) -> Vec<RatatuiLine<'static>>
    where
        F: FnOnce() -> Vec<RatatuiLine<'static>>,
        G: Fn(&[RatatuiLine<'static>]) -> usize,
    {
        let key = (
            block.cache_identity(),
            block.content_generation(),
            width,
            kind.tag(),
        );
        if !cacheable {
            if let Some((_, body)) = self
                .scratch_bodies
                .iter()
                .find(|(stored, _)| *stored == key)
            {
                self.body_hits = self.body_hits.saturating_add(1);
                return body.as_ref().clone();
            }
            self.body_bypasses = self.body_bypasses.saturating_add(1);
            let body = Arc::new(produce());
            if self.scratch_bodies.len() >= SCRATCH_BODY_SLOTS {
                self.scratch_bodies.remove(0);
            }
            self.scratch_bodies.push((key, body));
            return self
                .scratch_bodies
                .last()
                .expect("scratch body inserted")
                .1
                .as_ref()
                .clone();
        }
        if let Some(body) = self.bodies.get(&key) {
            self.body_hits = self.body_hits.saturating_add(1);
            return body.clone();
        }
        self.body_misses = self.body_misses.saturating_add(1);
        let body = produce();
        let bytes = weight(&body);
        if bytes <= MAX_CACHED_BODY_BYTES {
            self.bodies.insert(key, body.clone(), bytes);
        } else {
            self.body_oversized_skips = self.body_oversized_skips.saturating_add(1);
        }
        body
    }
}

const MAX_BODY_CACHE_ENTRIES: usize = 4_096;
const MAX_BODY_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHED_BODY_BYTES: usize = 128 * 1024;

/// Approximate retained heap size of a wrapped body: struct sizes plus the
/// heap bytes of every owned span string. Good enough for the global budget;
/// a bare `size_of::<Vec<_>>()` would undercount the dominant allocation.
pub(crate) fn cached_lines_bytes(lines: &[RatatuiLine<'static>]) -> usize {
    lines.iter().fold(0usize, |total, line| {
        total
            .saturating_add(std::mem::size_of::<RatatuiLine<'static>>())
            .saturating_add(line.spans.iter().fold(0usize, |sum, span| {
                sum.saturating_add(std::mem::size_of::<ratatui::text::Span<'static>>())
                    .saturating_add(span.content.len())
            }))
    })
}

impl Default for WrapCache {
    fn default() -> Self {
        Self {
            painted_selection: None,
            selection_scroll_anchor: None,
            thinking_previews: Vec::new(),
            finished_previews: WeightedCache::new(256, 1024 * 1024),
            selection_regions: [None, None],
            heights: BoundedCache::new(16_384),
            height_misses: 0,
            bodies: WeightedCache::new(MAX_BODY_CACHE_ENTRIES, MAX_BODY_CACHE_BYTES),
            scratch_bodies: Vec::new(),
            projections: Vec::new(),
            height_indexes: HashMap::new(),
            search_memo: None,
            selected_memo: None,
            tool_leader_memo: None,
            composer_memo: None,
            slash_memo: None,
            model_rows_memo: None,
            palette_memo: None,
            streaming_blocks_memo: None,
            block_line_memos: WeightedCache::new(MAX_BLOCK_LINE_MEMOS, MAX_BLOCK_LINE_MEMO_BYTES),
            footer_memo: None,
            inspector_memo: None,
            body_hits: 0,
            body_misses: 0,
            body_bypasses: 0,
            body_oversized_skips: 0,
        }
    }
}

#[cfg(test)]
mod height_cache_tests {
    use super::*;
    use crate::block::{Block, BlockKind, BlockLifecycle};

    #[test]
    fn thinking_preview_appends_match_full_wrap_without_a_moving_origin() {
        for width in [1, 7, 19, 96] {
            let mut cache = WrapCache::default();
            let mut block = Block::new(
                "preview",
                BlockKind::Thinking(String::new()),
                BlockLifecycle::Streaming,
            );
            for part in [
                "prefix ",
                "a".repeat(600).as_str(),
                " 日",
                "本語\n",
                "👩",
                "\u{200d}",
                "💻",
                "a",
                "\u{301}",
                "\tX\n",
                "\u{1b}[",
                "31mcolor\u{1b}[0m",
                "z".repeat(400).as_str(),
            ] {
                block.append_text(part);
                let BlockKind::Thinking(text) = block.kind() else {
                    unreachable!()
                };
                let (actual, _) = cache.thinking_preview(&block, text, width);
                let safe = crate::markdown::sanitize_terminal_text(text);
                let expected = crate::markdown::render_plain(safe.trim_end(), width);
                let start = expected.len().saturating_sub(2);
                assert_eq!(actual, expected[start..], "width={width} part={part:?}");
            }
            let preview = &cache.thinking_previews.last().unwrap().1;
            assert!(preview.restart > 600, "must retain a bounded visual suffix");
            let BlockKind::Thinking(text) = block.kind() else {
                unreachable!()
            };
            let resized = cache.thinking_preview(&block, text, 13).0;
            let expected = crate::markdown::render_plain(text.trim_end(), 13);
            assert_eq!(resized, expected[expected.len().saturating_sub(2)..]);
        }
    }

    #[test]
    fn finished_previews_survive_more_than_four_visible_blocks() {
        let blocks: Vec<_> = (0..5)
            .map(|id| {
                Block::new(
                    format!("preview-{id}"),
                    BlockKind::Thinking("conteúdo estável 日本語 ".repeat(2_000)),
                    BlockLifecycle::Complete,
                )
            })
            .collect();
        let mut cache = WrapCache::default();
        for block in &blocks {
            let BlockKind::Thinking(text) = block.kind() else {
                unreachable!()
            };
            cache.thinking_preview(block, text, 76);
        }
        let retained: Vec<_> = cache
            .thinking_previews
            .iter()
            .map(|(key, _)| *key)
            .collect();
        for block in &blocks {
            let BlockKind::Thinking(text) = block.kind() else {
                unreachable!()
            };
            let expected = crate::markdown::render_plain(text.trim_end(), 76);
            assert_eq!(
                cache.thinking_preview(block, text, 76).0,
                expected[expected.len() - 2..]
            );
        }
        assert_eq!(
            retained,
            cache
                .thinking_previews
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>(),
            "warm finished previews must not evict/rebuild incremental slots"
        );
    }

    #[test]
    fn preview_stays_aligned_for_small_unicode_and_control_fragments() {
        for width in [7, 53] {
            let mut cache = WrapCache::default();
            let mut block = Block::new(
                "fragments",
                BlockKind::Thinking(String::new()),
                BlockLifecycle::Streaming,
            );
            let corpus = "abc 日本語 👩‍💻 a\u{301}\txyz\n\u{1b}[31mtexto\u{1b}[0m ".repeat(12);
            for character in corpus.chars() {
                block.append_text(&character.to_string());
                let BlockKind::Thinking(text) = block.kind() else {
                    unreachable!()
                };
                let safe = crate::markdown::sanitize_terminal_text(text);
                let expected = crate::markdown::render_plain(safe.trim_end(), width);
                let actual = cache.thinking_preview(&block, text, width).0;
                assert_eq!(
                    actual,
                    expected[expected.len().saturating_sub(2)..],
                    "width={width} at={}",
                    text.len()
                );
            }
        }
    }

    #[test]
    fn grouped_thinking_heights_reuse_and_invalidate_per_member() {
        let mut blocks: Vec<_> = (0..3)
            .map(|index| {
                Block::new(
                    format!("t{index}"),
                    BlockKind::Thinking("ação 日本語\n👩‍💻".into()),
                    BlockLifecycle::Complete,
                )
            })
            .collect();
        blocks[0].fold = crate::block::FoldState::Expanded;
        let mut cache = WrapCache::default();
        let expected = |blocks: &[Block], width: u16| {
            1 + blocks
                .iter()
                .map(|block| match block.kind() {
                    BlockKind::Thinking(text) => {
                        wrapped_row_count(text, super::thinking_body_width(width) as usize) as u64
                    }
                    _ => unreachable!(),
                })
                .sum::<u64>()
        };
        for width in [12, 12, 20, 12] {
            assert_eq!(
                HeightIndex::build(&blocks, width, &mut cache).total_rows,
                expected(&blocks, width)
            );
        }
        assert_eq!(cache.height_misses(), 6);
        blocks[1].append_text("\nnew row");
        assert_eq!(
            HeightIndex::build(&blocks, 12, &mut cache).total_rows,
            expected(&blocks, 12)
        );
        assert_eq!(cache.height_misses(), 7);
        blocks[0].fold = crate::block::FoldState::Collapsed;
        assert_eq!(HeightIndex::build(&blocks, 12, &mut cache).total_rows, 1);
        blocks[0].fold = crate::block::FoldState::Expanded;
        assert_eq!(
            HeightIndex::build(&blocks, 12, &mut cache).total_rows,
            expected(&blocks, 12)
        );
        assert_eq!(cache.height_misses(), 7);
        // A standalone heading must not reuse the grouped body-only height.
        assert_eq!(
            HeightIndex::build(&blocks[..1], 12, &mut cache).total_rows,
            block_height(&blocks[0], 12, &mut cache) as u64
        );
        let replacement = blocks[1].clone();
        blocks[1] = replacement;
        HeightIndex::build(&blocks, 12, &mut cache);
        assert_eq!(cache.height_misses(), 9);
    }

    #[test]
    fn grouped_thinking_eviction_preserves_exact_heights() {
        let mut blocks: Vec<_> = (0..5)
            .map(|index| {
                Block::new(
                    format!("t{index}"),
                    BlockKind::Thinking("line\n".into()),
                    BlockLifecycle::Complete,
                )
            })
            .collect();
        blocks[0].fold = crate::block::FoldState::Expanded;
        let mut cache = WrapCache {
            heights: BoundedCache::new(4),
            ..WrapCache::default()
        };
        for _ in 0..3 {
            assert_eq!(HeightIndex::build(&blocks, 80, &mut cache).total_rows, 11);
            assert_eq!(cache.heights.len(), 4);
        }
        assert_eq!(
            cache.height_misses(),
            7,
            "deferred insertion avoids a cascade of misses"
        );
    }

    #[test]
    fn retains_both_active_widths_without_recurring_misses() {
        let blocks: Vec<_> = (0..8_192)
            .map(|index| {
                Block::new(
                    format!("block-{index}"),
                    BlockKind::Assistant("one line".into()),
                    BlockLifecycle::Complete,
                )
            })
            .collect();
        let mut cache = WrapCache::default();

        HeightIndex::build(&blocks, 80, &mut cache);
        assert_eq!(cache.height_misses(), 8_192);
        HeightIndex::build(&blocks, 79, &mut cache);
        assert_eq!(cache.height_misses(), 16_384);

        HeightIndex::build(&blocks, 80, &mut cache);
        HeightIndex::build(&blocks, 79, &mut cache);
        assert_eq!(
            cache.height_misses(),
            16_384,
            "both active viewport widths should remain cached"
        );
    }

    #[test]
    fn metrics_finds_last_visible_foldable_across_viewports() {
        let mut cache = WrapCache::default();
        let mut blocks: Vec<Block> = (0..200)
            .map(|index| {
                Block::new(
                    format!("a{index}"),
                    BlockKind::Assistant("line".into()),
                    BlockLifecycle::Complete,
                )
            })
            .collect();
        for index in [60usize, 198] {
            blocks[index] = Block::new(
                format!("fold-{index}"),
                BlockKind::Thinking("t".into()),
                BlockLifecycle::Complete,
            );
        }
        let index = HeightIndex::build(&blocks, 80, &mut cache);
        let last_foldable = |block: &Block| {
            index
                .metrics(
                    &FollowMode::Pinned(ScrollAnchor {
                        block_id: block.id.clone(),
                        row_offset: 0,
                    }),
                    10,
                )
                .last_visible_foldable_anchor
                .map(|anchor| anchor.block_id)
        };
        assert_eq!(last_foldable(&blocks[0]), None);
        assert_eq!(last_foldable(&blocks[59]), Some(blocks[60].id.clone()));
        assert_eq!(last_foldable(&blocks[60]), Some(blocks[60].id.clone()));
        assert_eq!(last_foldable(&blocks[61]), None);
        assert_eq!(last_foldable(&blocks[199]), Some(blocks[198].id.clone()));
    }

    #[test]
    fn streaming_probe_memoizes_on_content_revision() {
        let mut cache = WrapCache::default();
        let mut blocks = vec![Block::new(
            "a",
            BlockKind::Assistant("x".into()),
            BlockLifecycle::Complete,
        )];
        assert!(!cache.has_streaming_block(&blocks, 1));
        blocks[0].lifecycle = BlockLifecycle::Streaming;
        assert!(!cache.has_streaming_block(&blocks, 1));
        assert!(cache.has_streaming_block(&blocks, 2));
    }

    #[test]
    fn height_cache_is_16384_and_body_cache_is_byte_weighted() {
        let cache = WrapCache::default();
        assert_eq!(cache.heights.capacity(), 16_384);
        assert_eq!(cache.bodies.capacity(), 4_096);
        assert_eq!(cache.body_retained_bytes(), 0);
    }

    #[test]
    fn body_cache_evicts_by_bytes_and_counts_bypasses() {
        let mut cache = WrapCache::default();
        let block = || {
            Block::new(
                String::from("stable"),
                BlockKind::Assistant("hello".into()),
                BlockLifecycle::Complete,
            )
        };
        // Streaming content bypasses the cache and never stores a derivation.
        let mut streaming = block();
        streaming.lifecycle = BlockLifecycle::Streaming;
        let produced = cache.wrapped_body(
            &streaming,
            BodyKind::Assistant,
            80,
            false,
            || vec![RatatuiLine::from("streaming")],
            cached_lines_bytes,
        );
        assert_eq!(produced.len(), 1);
        assert_eq!(cache.body_bypasses(), 1);
        assert_eq!(cache.body_hits(), 0);
        assert!(cache.bodies.is_empty());

        // A stable block caches its body and hits on the second lookup.
        let stable = block();
        let first = cache.wrapped_body(
            &stable,
            BodyKind::Assistant,
            80,
            true,
            || vec![RatatuiLine::from("stable")],
            cached_lines_bytes,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(cache.body_misses(), 1);
        let second = cache.wrapped_body(
            &stable,
            BodyKind::Assistant,
            80,
            true,
            || vec![RatatuiLine::from("should not be produced")],
            cached_lines_bytes,
        );
        assert_eq!(second[0], RatatuiLine::from("stable"));
        assert_eq!(cache.body_hits(), 1);
        assert!(cache.body_retained_bytes() > 0);
    }
}
