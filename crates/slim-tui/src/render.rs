use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::UiEvent;
use crate::app::{AppState, FollowMode, ScrollAnchor};
use crate::block::{Block, BlockKind};
use crate::cache::{BoundedCache, WeightedCache};
use crate::layout;
use crate::view_model::{Frame, ViewModel};
use ratatui::text::Line as RatatuiLine;
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const THINKING_PREVIEW_GRAPHEMES: usize = 256;

pub(crate) fn thinking_preview_tail(text: &str) -> (&str, bool) {
    let text = text.trim_end();
    let Some((start, _)) = text
        .grapheme_indices(true)
        .rev()
        .nth(THINKING_PREVIEW_GRAPHEMES - 1)
    else {
        return (text, false);
    };
    (&text[start..], start > 0)
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
                    current_handle.clone_from(content_handle);
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

    use super::{thinking_preview_tail, EventCoalescer, THINKING_PREVIEW_GRAPHEMES};
    use crate::api::UiEvent;

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
        let suffix = "👩‍💻".repeat(THINKING_PREVIEW_GRAPHEMES);
        let text = format!("hidden{suffix}");
        let (preview, hidden) = thinking_preview_tail(&text);
        assert!(hidden);
        assert_eq!(preview, suffix);

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
        !state.blocks().is_empty() && width >= 80 && height >= 12,
    );
    ViewModel::derive_with_session_rail(state, regions.session_rail.height > 0, width)
}

/// Measured wrapped height of plain block text at a viewport width (§12.2).
/// Uses the same sanitized grapheme-greedy contract as materialization.
pub fn wrapped_row_count(text: &str, width: usize) -> usize {
    crate::markdown::plain_row_count(text, width.min(u16::MAX as usize) as u16)
}

fn block_height(block: &Block, width: u16) -> usize {
    let body_width = width.saturating_sub(2).max(1) as usize; // rail/padding column
    let content_rows = match block.kind() {
        BlockKind::User(text) => 1 + wrapped_row_count(text, body_width) + 1,
        BlockKind::Assistant(text) => {
            let text_width = if body_width > 1 {
                body_width - 1 // reserved streaming-caret cell, stable after completion
            } else {
                body_width
            };
            1 + crate::markdown::markdown_row_count(text, text_width as u16)
        }
        BlockKind::Thinking(text) => match block.fold {
            crate::block::FoldState::Expanded => 1 + wrapped_row_count(text, body_width),
            _ if block.lifecycle == crate::block::BlockLifecycle::Streaming => {
                let (preview, _) = thinking_preview_tail(text);
                let rows = if preview.is_empty() {
                    0
                } else {
                    crate::markdown::plain_row_count(preview, width.saturating_sub(4).max(1)).min(2)
                };
                1 + rows
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
                1 + wrapped_row_count(&state.materialized_output, body_width)
            } else {
                1
            }
        }
        BlockKind::InteractionRequest(state) => {
            crate::markdown::plain_row_count(&state.display_text(), width.saturating_sub(4).max(1))
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

fn push_grouped_tool_entry<'a>(
    entries: &mut Vec<(u64, &'a Block, &'a [Block])>,
    block_rows: &mut HashMap<crate::api::BlockId, (u64, u64)>,
    prefix: u64,
    leader: &'a Block,
    members: &'a [Block],
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
    entries.push((prefix, leader, members));
    prefix.saturating_add(rows)
}

pub struct HeightIndex<'a> {
    /// (prefix_rows_before, leader, members) — consecutive successful tools
    /// from one provider batch are grouped for presentation only (§11.4.1).
    pub entries: Vec<(u64, &'a Block, &'a [Block])>,
    pub total_rows: u64,
    block_rows: HashMap<crate::api::BlockId, (u64, u64)>,
}

impl<'a> HeightIndex<'a> {
    pub fn build(blocks: &'a [Block], width: u16, cache: &mut WrapCache) -> Self {
        let mut entries = Vec::with_capacity(blocks.len());
        let mut block_rows = HashMap::with_capacity(blocks.len());
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
                        prefix = push_grouped_tool_entry(
                            &mut entries,
                            &mut block_rows,
                            prefix,
                            block,
                            &blocks[start..end],
                            rows,
                            crate::block::is_complete_tool,
                        );
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
                        prefix = push_grouped_tool_entry(
                            &mut entries,
                            &mut block_rows,
                            prefix,
                            block,
                            &blocks[start..end],
                            rows,
                            crate::block::is_failed_tool,
                        );
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
                            let body_width = width.saturating_sub(2).max(1) as usize;
                            1u64.saturating_add(
                                members
                                    .iter()
                                    .map(|member| match member.kind() {
                                        BlockKind::Thinking(text) => {
                                            wrapped_row_count(text, body_width).max(1) as u64
                                        }
                                        _ => 1,
                                    })
                                    .sum(),
                            )
                        } else {
                            1
                        };
                        prefix = push_grouped_tool_entry(
                            &mut entries,
                            &mut block_rows,
                            prefix,
                            block,
                            members,
                            rows,
                            crate::block::is_complete_thinking,
                        );
                        index = end;
                        continue;
                    }
                }
            }
            let key = (
                block.cache_identity(),
                block.content_generation(),
                width,
                block.fold == crate::block::FoldState::Collapsed,
            );
            let height = if let Some(height) = cache.heights.get(&key).copied() {
                height
            } else {
                cache.height_misses = cache.height_misses.saturating_add(1);
                let height = block_height(block, width);
                pending_heights.push((key, height));
                height
            };
            let rows = height as u64;
            let boundary_rows = u64::from(block.turn_boundary_before());
            block_rows.entry(block.id.clone()).or_insert((
                prefix.saturating_add(boundary_rows),
                rows.saturating_sub(boundary_rows).max(1),
            ));
            entries.push((prefix, block, &blocks[index..=index]));
            prefix = prefix.saturating_add(rows);
            index += 1;
        }
        // Defer inserts until the scan ends. Eager FIFO insertion can evict
        // the very next key and cascade into an all-miss frame at capacity+1.
        for (key, height) in pending_heights {
            cache.heights.insert(key, height);
        }
        Self {
            entries,
            total_rows: prefix,
            block_rows,
        }
    }

    pub fn prefix_for_block(&self, id: &crate::api::BlockId) -> Option<u64> {
        self.block_rows.get(id).map(|(prefix, _)| *prefix)
    }

    pub fn row_for_anchor(&self, anchor: &ScrollAnchor) -> Option<u64> {
        self.block_rows.get(&anchor.block_id).map(|(prefix, rows)| {
            prefix.saturating_add(anchor.row_offset.min(rows.saturating_sub(1)))
        })
    }

    pub fn anchor_for_row(&self, row: u64) -> Option<ScrollAnchor> {
        if self.entries.is_empty() || row >= self.total_rows {
            return None;
        }
        let (index, row_offset) = self.locate(row);
        Some(ScrollAnchor {
            block_id: self.entries[index].1.id.clone(),
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
            .then(|| {
                self.entries
                    .iter()
                    .rev()
                    .find_map(|(prefix, block, members)| {
                        let rows = self.block_rows.get(&block.id)?.1;
                        let foldable_tool = matches!(block.kind(), BlockKind::Tool(state)
                        if members.len() > 1 || state.content_handle.is_some());
                        (*prefix < viewport_end
                            && prefix.saturating_add(rows) > viewport_start
                            && (matches!(block.kind(), BlockKind::Thinking(_)) || foldable_tool))
                            .then(|| ScrollAnchor {
                                block_id: block.id.clone(),
                                row_offset: 0,
                            })
                    })
            })
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
        if self.entries.is_empty() {
            return (0, 0);
        }
        let idx = self
            .entries
            .partition_point(|(prefix, _, _)| *prefix <= row)
            .saturating_sub(1)
            .min(self.entries.len().saturating_sub(1));
        let skipped = row.saturating_sub(self.entries[idx].0);
        (idx, skipped)
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

/// Bounded caches for the render pipeline (§12.3/§12.4): heights keyed by
/// block generation, width and fold state. Eviction only drops derivations.
pub struct WrapCache {
    heights: BoundedCache<(u64, u64, u16, bool), usize>,
    height_misses: u64,
    /// Wrapped body lines keyed by (identity, generation, width, body kind).
    /// Byte-weighted so a few large bodies cannot exhaust memory. Stable blocks
    /// are re-wrapped every frame today; this makes them render from the last
    /// wrap until their content changes.
    bodies: WeightedCache<(u64, u64, u16, u8), Vec<RatatuiLine<'static>>>,
    body_hits: u64,
    body_misses: u64,
    body_bypasses: u64,
    body_oversized_skips: u64,
}

impl WrapCache {
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

    /// Drops every cached wrapped body. Call this when theme or render
    /// configuration changes so stale color/style derivations are never reused.
    pub fn invalidate_render_configuration(&mut self) {
        self.bodies.clear();
    }

    /// Looks up the cached wrapped body for a stable block, or computes and
    /// stores it via `produce` on a miss. Streaming blocks (content changes
    /// per frame) bypass the cache by returning `produce` directly.
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
        if !cacheable {
            self.body_bypasses = self.body_bypasses.saturating_add(1);
            return produce();
        }
        let key = (
            block.cache_identity(),
            block.content_generation(),
            width,
            kind.tag(),
        );
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
            heights: BoundedCache::new(16_384),
            height_misses: 0,
            bodies: WeightedCache::new(MAX_BODY_CACHE_ENTRIES, MAX_BODY_CACHE_BYTES),
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
