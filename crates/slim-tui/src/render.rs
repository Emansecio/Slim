use std::time::{Duration, Instant};

use crate::api::UiEvent;
use crate::app::AppState;
use crate::block::{Block, BlockKind};
use crate::cache::BoundedCache;
use crate::layout;
use crate::view_model::{Frame, ViewModel};

#[derive(Debug)]
pub struct EventCoalescer {
    data_capacity: usize,
    data: Vec<UiEvent>,
    control: Vec<UiEvent>,
    window: Duration,
    window_started: Option<Instant>,
}

impl EventCoalescer {
    pub fn new(data_capacity: usize, window: Duration) -> Self {
        Self {
            data_capacity,
            data: Vec::new(),
            control: Vec::new(),
            window,
            window_started: None,
        }
    }

    pub fn push_control(&mut self, event: UiEvent) {
        self.control.push(event);
    }

    pub fn push_data(&mut self, event: UiEvent) {
        if let Some(UiEvent::AssistantDelta { text: current }) = self.data.last_mut() {
            if let UiEvent::AssistantDelta { text } = &event {
                current.push_str(text);
                return;
            }
        }
        if self.data.len() < self.data_capacity {
            self.data.push(event);
            self.window_started.get_or_insert_with(Instant::now);
        }
    }

    pub fn flush(&mut self) -> Vec<UiEvent> {
        let mut events = std::mem::take(&mut self.control);
        events.extend(std::mem::take(&mut self.data));
        self.window_started = None;
        events
    }

    pub fn window_elapsed(&self) -> bool {
        self.window_started
            .is_some_and(|started| started.elapsed() >= self.window)
    }
}

pub fn render(state: &AppState, width: u16, height: u16) -> Frame {
    let _regions = layout::plan(
        width,
        height,
        layout::todo_height(state.todo_dock_open, state.todo_items.len()),
        state.working,
    );
    ViewModel::derive(state)
}

/// Measured wrapped height of one block's text at a viewport width (§12.2):
/// conservative display-width wrap, one row minimum per source line.
pub fn wrapped_row_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    use unicode_width::UnicodeWidthStr;
    text.split('\n')
        .map(|line| {
            let w = line.width();
            w.div_ceil(width).max(1)
        })
        .sum()
}

fn block_height(block: &Block, width: u16) -> usize {
    let width = width.max(4) as usize - 2; // rail/padding column
    match &block.kind {
        BlockKind::User(text) | BlockKind::Assistant(text) => 1 + wrapped_row_count(text, width),
        BlockKind::Thinking(text) => match block.fold {
            crate::block::FoldState::Collapsed => 1,
            _ => 1 + wrapped_row_count(text, width),
        },
        // Tools render as one row each (aggregation happens on runs).
        BlockKind::Tool(_) | BlockKind::System(_) | BlockKind::Error(_) => 1,
        BlockKind::Activity(_) | BlockKind::QueuedUser(_) => 1,
    }
}

/// Visible-range index over prefix sums (§13.2): exact heights measured via
/// the wrap cache, first visible block found in O(log n). Rebuilt per frame —
/// building is O(n) over cached heights, no per-frame wrapping.
pub struct HeightIndex<'a> {
    /// (prefix_rows_before, block, group_size) — tools completed with the same
    /// name are grouped into one rendered row (§11.4.1, presentation only).
    pub entries: Vec<(u64, &'a Block, usize)>,
    pub total_rows: u64,
}

impl<'a> HeightIndex<'a> {
    pub fn build(blocks: &'a [Block], width: u16, cache: &mut WrapCache) -> Self {
        let mut entries = Vec::with_capacity(blocks.len());
        let mut prefix = 0u64;
        let mut iter = blocks.iter().peekable();
        while let Some(block) = iter.next() {
            if let (BlockKind::Tool(state), crate::block::BlockLifecycle::Complete) =
                (&block.kind, block.lifecycle)
            {
                let name = state.name.clone();
                let mut group = 1usize;
                while matches!(iter.peek(), Some(b)
                    if b.lifecycle == crate::block::BlockLifecycle::Complete
                    && matches!(&b.kind, BlockKind::Tool(t) if t.name == name))
                {
                    iter.next();
                    group += 1;
                }
                entries.push((prefix, block, group));
                prefix += 1;
                continue;
            }
            let key = (block.id.0.to_string(), width, block.fold == crate::block::FoldState::Collapsed);
            let height = cache.heights.get(&key).copied().unwrap_or_else(|| {
                let h = block_height(block, width);
                cache.heights.insert(key, h);
                h
            });
            entries.push((prefix, block, 1));
            prefix += height as u64;
        }
        Self {
            entries,
            total_rows: prefix,
        }
    }

    /// Index of the first entry intersecting `row`, plus rows to skip inside it.
    pub fn locate(&self, row: u64) -> (usize, u64) {
        let idx = self
            .entries
            .partition_point(|(prefix, _, _)| *prefix <= row)
            .saturating_sub(1)
            .min(self.entries.len().saturating_sub(1));
        let skipped = row.saturating_sub(self.entries[idx].0);
        (idx, skipped)
    }
}

/// Bounded caches for the render pipeline (§12.3/§12.4): heights keyed by
/// (block id, width, collapsed). Eviction only drops derivations.
pub struct WrapCache {
    pub heights: BoundedCache<(String, u16, bool), usize>,
}

impl Default for WrapCache {
    fn default() -> Self {
        Self {
            heights: BoundedCache::new(4_096),
        }
    }
}
