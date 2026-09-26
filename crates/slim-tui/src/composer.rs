use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextElement {
    Text(String),
    Paste { id: u64, payload: String },
}

pub const MAX_DRAFT_CHARS: usize = 1_048_576;
const MAX_EDIT_HISTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_EDIT_HISTORY_STEPS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
struct DraftSnapshot {
    elements: Vec<TextElement>,
    cursor: usize,
    char_count: usize,
    newline_count: usize,
}

impl DraftSnapshot {
    fn bytes(&self) -> usize {
        self.elements
            .iter()
            .map(|element| match element {
                TextElement::Text(text) => text.len(),
                TextElement::Paste { payload, .. } => payload.len(),
            })
            .sum::<usize>()
            + self.elements.len() * std::mem::size_of::<TextElement>()
            + std::mem::size_of::<Self>()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerError {
    DraftTooLarge,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Composer {
    elements: Vec<TextElement>,
    next_paste_id: u64,
    char_count: usize,
    cursor: usize,
    /// Bumped by every mutation (text or cursor); render memos key on it.
    revision: u64,
    /// '\n' count across `Text` elements — `line_count` stays O(1) on drafts
    /// up to `MAX_DRAFT_CHARS`. Paste payloads display as a single token.
    newline_count: usize,
    undo: VecDeque<DraftSnapshot>,
    redo: VecDeque<DraftSnapshot>,
    history_bytes: usize,
    typing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplaySnapshot {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_cell: usize,
    pub total_lines: usize,
}

impl Composer {
    pub fn insert_text(&mut self, text: impl Into<String>) {
        let _ = self.try_insert_text(text);
    }

    pub fn try_insert_text(&mut self, text: impl Into<String>) -> Result<(), ComposerError> {
        let text = text.into();
        let added_chars = text.chars().count();
        if self
            .char_count
            .checked_add(added_chars)
            .is_none_or(|count| count > MAX_DRAFT_CHARS)
        {
            return Err(ComposerError::DraftTooLarge);
        }
        if text.is_empty() {
            return Ok(());
        }
        let typing = added_chars == 1 && !text.chars().next().unwrap().is_whitespace();
        self.record_edit(typing);
        self.char_count += added_chars;
        self.newline_count += text.bytes().filter(|byte| *byte == b'\n').count();
        // char_count always equals the sum of element lengths, so a cursor at
        // the pre-insert end does not need a second O(elements) scan.
        if self.cursor == self.char_count - added_chars
            && matches!(self.elements.last(), Some(TextElement::Text(_)))
        {
            let Some(TextElement::Text(current)) = self.elements.last_mut() else {
                unreachable!();
            };
            current.push_str(&text);
        } else {
            self.insert_element(TextElement::Text(text));
        }
        self.cursor += added_chars;
        self.bump();
        Ok(())
    }

    pub fn paste(&mut self, payload: impl Into<String>) -> u64 {
        self.try_paste(payload).unwrap_or(self.next_paste_id)
    }

    pub fn try_paste(&mut self, payload: impl Into<String>) -> Result<u64, ComposerError> {
        let payload = payload.into();
        let added_chars = payload.chars().count();
        if self
            .char_count
            .checked_add(added_chars)
            .is_none_or(|count| count > MAX_DRAFT_CHARS)
        {
            return Err(ComposerError::DraftTooLarge);
        }
        self.record_edit(false);
        let id = self.next_paste_id;
        self.next_paste_id += 1;
        self.char_count += added_chars;
        self.insert_element(TextElement::Paste { id, payload });
        self.cursor += added_chars;
        self.bump();
        Ok(id)
    }

    pub fn display(&self) -> String {
        self.display_for_width(80)
    }

    pub fn display_for_width(&self, width: usize) -> String {
        self.elements
            .iter()
            .map(|element| match element {
                TextElement::Text(text) => text.clone(),
                TextElement::Paste { id, payload } => {
                    let chars = payload.chars().count();
                    if !payload.contains('\n') && !payload.contains('\r') && chars <= width {
                        payload.clone()
                    } else {
                        format!("[Pasted Content {id} {chars} chars]")
                    }
                }
            })
            .collect()
    }

    pub fn display_snapshot(&self, width: usize) -> DisplaySnapshot {
        let mut display = String::new();
        let mut atomic_ranges = Vec::new();
        let mut raw_offset = 0usize;
        let mut cursor_byte = None;
        for element in &self.elements {
            let element_len = element_chars(element);
            match element {
                TextElement::Text(text) => {
                    if cursor_byte.is_none()
                        && self.cursor >= raw_offset
                        && self.cursor <= raw_offset + element_len
                    {
                        let local = self.cursor - raw_offset;
                        let byte = char_byte_index(text, local);
                        display.push_str(&text[..byte]);
                        cursor_byte = Some(display.len());
                        display.push_str(&text[byte..]);
                    } else {
                        display.push_str(text);
                    }
                }
                TextElement::Paste { id, payload } => {
                    if cursor_byte.is_none() && self.cursor == raw_offset {
                        cursor_byte = Some(display.len());
                    }
                    let rendered = paste_display(*id, payload, width);
                    let start = display.len();
                    display.push_str(&rendered);
                    if paste_is_chip(payload, width) {
                        atomic_ranges.push((start, display.len()));
                    }
                    if cursor_byte.is_none() && self.cursor == raw_offset + element_len {
                        cursor_byte = Some(display.len());
                    }
                }
            }
            raw_offset += element_len;
        }
        let cursor_byte = cursor_byte.unwrap_or(display.len()).min(display.len());
        let mut raw_offsets = Vec::with_capacity(1 + atomic_ranges.len() * 2);
        raw_offsets.push(cursor_byte);
        raw_offsets.extend(atomic_ranges.iter().flat_map(|(start, end)| [*start, *end]));
        let (display, mapped_offsets) =
            crate::markdown::sanitize_terminal_text_with_offsets(&display, &raw_offsets);
        let cursor_byte = mapped_offsets.first().copied().unwrap_or_default();
        let atomic_ranges = mapped_offsets[1..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|range| (range[0], range[1]))
            .collect::<Vec<_>>();
        let (lines, cursor_line, cursor_cell) =
            wrap_display(&display, width, cursor_byte, &atomic_ranges);
        let total_lines = lines.len().max(1);
        DisplaySnapshot {
            lines,
            cursor_line: cursor_line.min(total_lines - 1),
            cursor_cell,
            total_lines,
        }
    }

    pub fn line_count(&self) -> usize {
        self.newline_count + 1
    }

    /// Monotonic edit counter (text and cursor); render snapshots memoize on it.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn payload(&self) -> String {
        self.elements
            .iter()
            .map(|element| match element {
                TextElement::Text(text) => text.clone(),
                TextElement::Paste { payload, .. } => payload.clone(),
            })
            .collect()
    }

    /// The draft's chars — same sequence as `payload().chars()` — without
    /// materializing the String. Keystroke-driven scans (`move_home`,
    /// `move_end`, slash completion) stay O(1) in allocation.
    pub(crate) fn payload_chars(&self) -> impl DoubleEndedIterator<Item = char> + '_ {
        self.elements.iter().flat_map(|element| match element {
            TextElement::Text(text) => text.chars(),
            TextElement::Paste { payload, .. } => payload.chars(),
        })
    }

    /// Total draft length in chars; cursor positions are char indices.
    pub(crate) fn char_count(&self) -> usize {
        self.char_count
    }

    pub fn is_empty(&self) -> bool {
        self.char_count == 0
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn remove_last_element(&mut self) -> Option<TextElement> {
        if self.elements.is_empty() {
            return None;
        }
        self.record_edit(false);
        let element = self.elements.pop()?;
        let (removed_chars, removed_newlines) = match &element {
            TextElement::Text(text) => (
                text.chars().count(),
                text.bytes().filter(|byte| *byte == b'\n').count(),
            ),
            TextElement::Paste { payload, .. } => (payload.chars().count(), 0),
        };
        self.char_count = self.char_count.saturating_sub(removed_chars);
        self.newline_count = self.newline_count.saturating_sub(removed_newlines);
        self.cursor = self.cursor.min(self.char_count);
        self.bump();
        Some(element)
    }

    /// Removes the last grapheme cluster from the draft (§15.3). A trailing
    /// paste segment is removed as a whole; returns whether anything changed.
    pub fn backspace(&mut self) -> bool {
        if self.span_before_cursor().is_none() {
            return false;
        }
        self.record_edit(false);
        self.backspace_inner()
    }

    fn backspace_inner(&mut self) -> bool {
        let Some((index, start, _end)) = self.span_before_cursor() else {
            return false;
        };
        match &mut self.elements[index] {
            TextElement::Paste { payload, .. } => {
                let removed = payload.chars().count();
                self.elements.remove(index);
                self.char_count -= removed;
                self.cursor = start;
            }
            TextElement::Text(text) => {
                let local = self.cursor - start;
                let cursor_byte = char_byte_index(text, local);
                let Some((boundary, grapheme)) =
                    text[..cursor_byte].grapheme_indices(true).next_back()
                else {
                    return false;
                };
                let removed = grapheme.chars().count();
                self.newline_count = self
                    .newline_count
                    .saturating_sub(grapheme.bytes().filter(|byte| *byte == b'\n').count());
                text.replace_range(boundary..cursor_byte, "");
                self.char_count -= removed;
                self.cursor -= removed;
                if text.is_empty() {
                    self.elements.remove(index);
                }
            }
        }
        self.normalize_text_elements();
        self.bump();
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.span_after_cursor().is_none() {
            return false;
        }
        self.record_edit(false);
        self.delete_inner()
    }

    fn delete_inner(&mut self) -> bool {
        let Some((index, start, _end)) = self.span_after_cursor() else {
            return false;
        };
        match &mut self.elements[index] {
            TextElement::Paste { payload, .. } => {
                let removed = payload.chars().count();
                self.elements.remove(index);
                self.char_count -= removed;
            }
            TextElement::Text(text) => {
                let local = self.cursor - start;
                let cursor_byte = char_byte_index(text, local);
                let Some(grapheme) = text[cursor_byte..].graphemes(true).next() else {
                    return false;
                };
                let removed_bytes = grapheme.len();
                let removed_chars = grapheme.chars().count();
                self.newline_count = self
                    .newline_count
                    .saturating_sub(grapheme.bytes().filter(|byte| *byte == b'\n').count());
                text.replace_range(cursor_byte..cursor_byte + removed_bytes, "");
                self.char_count -= removed_chars;
                if text.is_empty() {
                    self.elements.remove(index);
                }
            }
        }
        self.normalize_text_elements();
        self.bump();
        true
    }

    pub fn move_left(&mut self) -> bool {
        self.typing = false;
        let Some((index, start, _end)) = self.span_before_cursor() else {
            return false;
        };
        self.cursor = match &self.elements[index] {
            TextElement::Paste { .. } => start,
            TextElement::Text(text) => {
                let local = self.cursor - start;
                let byte = char_byte_index(text, local);
                let Some((boundary, _)) = text[..byte].grapheme_indices(true).next_back() else {
                    return false;
                };
                start + text[..boundary].chars().count()
            }
        };
        self.bump();
        true
    }

    pub fn move_right(&mut self) -> bool {
        self.typing = false;
        let Some((index, start, end)) = self.span_after_cursor() else {
            return false;
        };
        self.cursor = match &self.elements[index] {
            TextElement::Paste { .. } => end,
            TextElement::Text(text) => {
                let local = self.cursor - start;
                let byte = char_byte_index(text, local);
                let Some(grapheme) = text[byte..].graphemes(true).next() else {
                    return false;
                };
                self.cursor + grapheme.chars().count()
            }
        };
        self.bump();
        true
    }

    pub fn move_home(&mut self) -> bool {
        self.typing = false;
        let target = self
            .payload_chars()
            .take(self.cursor)
            .enumerate()
            .filter_map(|(index, character)| (character == '\n').then_some(index + 1))
            .last()
            .unwrap_or(0);
        let target = self.snap_out_of_paste(target, false);
        let changed = target != self.cursor;
        self.cursor = target;
        if changed {
            self.bump();
        }
        changed
    }

    pub fn move_end(&mut self) -> bool {
        self.typing = false;
        let target = self
            .payload_chars()
            .enumerate()
            .skip(self.cursor)
            .find_map(|(index, character)| (character == '\n').then_some(index))
            .unwrap_or(self.char_count);
        let target = self.snap_out_of_paste(target, true);
        let changed = target != self.cursor;
        self.cursor = target;
        if changed {
            self.bump();
        }
        changed
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.history_bytes = 0;
        self.typing = false;
        if self.elements.is_empty() {
            return;
        }
        self.elements.clear();
        self.char_count = 0;
        self.newline_count = 0;
        self.cursor = 0;
        self.bump();
    }

    fn snapshot(&self) -> DraftSnapshot {
        DraftSnapshot {
            elements: self.elements.clone(),
            cursor: self.cursor,
            char_count: self.char_count,
            newline_count: self.newline_count,
        }
    }

    fn restore(&mut self, snapshot: DraftSnapshot) {
        self.elements = snapshot.elements;
        self.cursor = snapshot.cursor;
        self.char_count = snapshot.char_count;
        self.newline_count = snapshot.newline_count;
        self.typing = false;
        self.bump();
    }

    fn trim_history(&mut self) {
        while self.history_bytes > MAX_EDIT_HISTORY_BYTES
            || self.undo.len() + self.redo.len() > MAX_EDIT_HISTORY_STEPS
        {
            if let Some(oldest) = self.undo.pop_front().or_else(|| self.redo.pop_front()) {
                self.history_bytes -= oldest.bytes();
            } else {
                break;
            }
        }
    }

    fn record_edit(&mut self, typing: bool) {
        for discarded in self.redo.drain(..) {
            self.history_bytes -= discarded.bytes();
        }
        // Coalesce contiguous typing, so large drafts are not cloned per key.
        if !typing || !self.typing {
            let snapshot = self.snapshot();
            self.history_bytes += snapshot.bytes();
            self.undo.push_back(snapshot);
            self.trim_history();
        }
        self.typing = typing;
    }

    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop_back() else {
            return false;
        };
        self.history_bytes -= previous.bytes();
        let current = self.snapshot();
        self.history_bytes += current.bytes();
        self.redo.push_back(current);
        self.restore(previous);
        self.trim_history();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop_back() else {
            return false;
        };
        self.history_bytes -= next.bytes();
        let current = self.snapshot();
        self.history_bytes += current.bytes();
        self.undo.push_back(current);
        self.restore(next);
        self.trim_history();
        true
    }

    fn word_boundary(&self, forward: bool) -> usize {
        let span = if forward {
            self.span_after_cursor()
        } else {
            self.span_before_cursor()
        };
        let Some((index, start, end)) = span else {
            return self.cursor;
        };
        let TextElement::Text(text) = &self.elements[index] else {
            return if forward { end } else { start };
        };
        let byte = char_byte_index(text, self.cursor - start);
        if forward {
            let mut count = 0;
            for segment in text[byte..].split_word_bounds() {
                count += segment.chars().count();
                if !segment.chars().all(char::is_whitespace) {
                    break;
                }
            }
            self.cursor + count
        } else {
            let mut count = 0;
            for segment in text[..byte].split_word_bounds().rev() {
                count += segment.chars().count();
                if !segment.chars().all(char::is_whitespace) {
                    break;
                }
            }
            self.cursor - count
        }
    }

    pub fn move_word(&mut self, forward: bool) -> bool {
        self.typing = false;
        let target = self.word_boundary(forward);
        if target == self.cursor {
            return false;
        }
        self.cursor = target;
        self.bump();
        true
    }

    pub fn delete_word(&mut self, forward: bool) -> bool {
        let target = self.word_boundary(forward);
        if target == self.cursor {
            return false;
        }
        self.record_edit(false);
        let (index, start, _) = if forward {
            self.span_after_cursor()
        } else {
            self.span_before_cursor()
        }
        .unwrap();
        let first = self.cursor.min(target);
        let last = self.cursor.max(target);
        match &mut self.elements[index] {
            TextElement::Paste { .. } => {
                self.elements.remove(index);
            }
            TextElement::Text(text) => {
                let range =
                    char_byte_index(text, first - start)..char_byte_index(text, last - start);
                self.newline_count -= text[range.clone()].bytes().filter(|b| *b == b'\n').count();
                text.replace_range(range, "");
            }
        }
        self.char_count -= last - first;
        self.cursor = first;
        self.normalize_text_elements();
        self.bump();
        true
    }

    fn insert_element(&mut self, inserted: TextElement) {
        if self.elements.is_empty() || self.cursor == self.char_count - element_chars(&inserted) {
            self.elements.push(inserted);
            self.normalize_text_elements();
            return;
        }
        let mut offset = 0usize;
        for index in 0..self.elements.len() {
            let len = element_chars(&self.elements[index]);
            if self.cursor == offset {
                self.elements.insert(index, inserted);
                self.normalize_text_elements();
                return;
            }
            if self.cursor < offset + len {
                let TextElement::Text(text) = &self.elements[index] else {
                    self.elements.insert(index, inserted);
                    self.normalize_text_elements();
                    return;
                };
                let split = char_byte_index(text, self.cursor - offset);
                let before = text[..split].to_owned();
                let after = text[split..].to_owned();
                self.elements.remove(index);
                let mut replacement = Vec::with_capacity(3);
                if !before.is_empty() {
                    replacement.push(TextElement::Text(before));
                }
                replacement.push(inserted);
                if !after.is_empty() {
                    replacement.push(TextElement::Text(after));
                }
                self.elements.splice(index..index, replacement);
                self.normalize_text_elements();
                return;
            }
            offset += len;
        }
        self.elements.push(inserted);
        self.normalize_text_elements();
    }

    fn normalize_text_elements(&mut self) {
        // Skip the rebuild when nothing needs it: no empty text shard and no
        // adjacent `Text` pair to merge.
        let needs_normalize = self.elements.iter().enumerate().any(|(index, element)| {
            matches!(element, TextElement::Text(text) if text.is_empty())
                || (index > 0
                    && matches!(element, TextElement::Text(_))
                    && matches!(self.elements[index - 1], TextElement::Text(_)))
        });
        if !needs_normalize {
            return;
        }
        let mut normalized = Vec::with_capacity(self.elements.len());
        for element in self.elements.drain(..) {
            match element {
                TextElement::Text(text) if text.is_empty() => {}
                TextElement::Text(text) => {
                    if let Some(TextElement::Text(previous)) = normalized.last_mut() {
                        previous.push_str(&text);
                    } else {
                        normalized.push(TextElement::Text(text));
                    }
                }
                paste => normalized.push(paste),
            }
        }
        self.elements = normalized;
    }

    fn span_before_cursor(&self) -> Option<(usize, usize, usize)> {
        let mut offset = 0usize;
        let mut result = None;
        for (index, element) in self.elements.iter().enumerate() {
            let end = offset + element_chars(element);
            if offset < self.cursor {
                result = Some((index, offset, end));
            }
            if end >= self.cursor {
                break;
            }
            offset = end;
        }
        result
    }

    fn span_after_cursor(&self) -> Option<(usize, usize, usize)> {
        let mut offset = 0usize;
        for (index, element) in self.elements.iter().enumerate() {
            let end = offset + element_chars(element);
            if end > self.cursor {
                return Some((index, offset, end));
            }
            offset = end;
        }
        None
    }

    fn snap_out_of_paste(&self, target: usize, toward_end: bool) -> usize {
        let mut offset = 0usize;
        for element in &self.elements {
            let end = offset + element_chars(element);
            if matches!(element, TextElement::Paste { .. }) && target > offset && target < end {
                return if toward_end { end } else { offset };
            }
            offset = end;
        }
        target
    }
}

fn element_chars(element: &TextElement) -> usize {
    match element {
        TextElement::Text(text) => text.chars().count(),
        TextElement::Paste { payload, .. } => payload.chars().count(),
    }
}

fn char_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn paste_is_chip(payload: &str, width: usize) -> bool {
    payload.contains('\n') || payload.contains('\r') || payload.chars().count() > width
}

fn paste_display(id: u64, payload: &str, width: usize) -> String {
    if !paste_is_chip(payload, width) {
        payload.to_owned()
    } else {
        let chars = payload.chars().count();
        format!("[Pasted Content {id} {chars} chars]")
    }
}

/// Projects the display string into terminal rows without changing the raw
/// prompt. Newlines force a row break; all other breaks happen between
/// grapheme clusters and use their Unicode display width. Tokenized paste
/// ranges are supplied separately so a chip is never split across rows.
fn wrap_display(
    display: &str,
    width: usize,
    cursor_byte: usize,
    atomic_ranges: &[(usize, usize)],
) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let cursor_byte = cursor_byte.min(display.len());
    let mut lines = vec![String::new()];
    let mut used = 0usize;
    let mut cursor = None;
    let mut byte = 0usize;
    let mut atomic_index = 0usize;

    while byte < display.len() {
        if atomic_ranges
            .get(atomic_index)
            .is_some_and(|(start, _)| *start == byte)
        {
            let (start, end) = atomic_ranges[atomic_index];
            append_visual_unit(
                &mut lines,
                &mut used,
                &mut cursor,
                cursor_byte,
                start,
                &display[start..end],
                width,
            );
            byte = end;
            atomic_index += 1;
            continue;
        }

        let end = atomic_ranges
            .get(atomic_index)
            .map_or(display.len(), |(start, _)| *start);
        let chunk = &display[byte..end];
        for (offset, grapheme) in chunk.grapheme_indices(true) {
            append_visual_unit(
                &mut lines,
                &mut used,
                &mut cursor,
                cursor_byte,
                byte + offset,
                grapheme,
                width,
            );
        }
        byte = end;
    }

    // The cursor at a filled row has no valid cell in the renderer's bounded
    // content area. Give it a fresh insertion row instead of covering the
    // final glyph; this also handles an over-wide final grapheme or chip.
    if cursor.is_none() {
        if cursor_byte >= display.len() && !display.is_empty() && used >= width {
            lines.push(String::new());
            used = 0;
        }
        cursor = Some((lines.len().saturating_sub(1), used));
    }

    let (cursor_line, cursor_cell) = cursor.expect("cursor position populated");
    (lines, cursor_line, cursor_cell)
}

fn append_visual_unit(
    lines: &mut Vec<String>,
    used: &mut usize,
    cursor: &mut Option<(usize, usize)>,
    cursor_byte: usize,
    byte: usize,
    unit: &str,
    width: usize,
) {
    let hard_break = matches!(unit, "\n" | "\r\n");
    if hard_break {
        let current_line = lines.len().saturating_sub(1);
        let current_cell = *used;
        if cursor.is_none() && cursor_byte <= byte {
            // At a full row, place the caret at the beginning of the row
            // created by this newline so it remains inside the viewport.
            *cursor = Some(if current_cell >= width {
                (current_line + 1, 0)
            } else {
                (current_line, current_cell)
            });
        }
        lines.push(String::new());
        *used = 0;
        return;
    }

    let unit_width = unicode_width::UnicodeWidthStr::width(unit);
    if *used > 0 && (*used).saturating_add(unit_width) > width {
        lines.push(String::new());
        *used = 0;
    }
    if cursor.is_none() && cursor_byte <= byte {
        *cursor = Some((lines.len().saturating_sub(1), *used));
    }
    lines
        .last_mut()
        .expect("at least one visual row")
        .push_str(unit);
    *used = (*used).saturating_add(unit_width);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo_preserve_cursor_unicode_and_atomic_paste() {
        let mut draft = Composer::default();
        for c in "ação".chars() {
            draft.insert_text(c.to_string());
        }
        assert!(draft.undo());
        assert!(draft.is_empty());
        assert!(draft.redo());
        assert_eq!(draft.payload(), "ação");
        draft.move_left();
        let cursor = draft.cursor();
        draft.paste("linha 1\nlinha 2");
        let after = draft.snapshot();
        assert!(draft.undo());
        assert_eq!(draft.payload(), "ação");
        assert_eq!(draft.cursor(), cursor);
        assert!(draft.redo());
        assert_eq!(draft.snapshot(), after);
        assert!(draft.backspace());
        assert_eq!(draft.payload(), "ação");
        assert!(draft.undo());
        assert_eq!(draft.snapshot(), after);
    }

    #[test]
    fn edit_after_undo_discards_redo_and_clear_discards_all_history() {
        let mut draft = Composer::default();
        draft.insert_text("primeiro");
        draft.paste("segundo");
        assert!(draft.undo());
        draft.insert_text("!");
        assert!(!draft.redo());
        assert_eq!(draft.payload(), "primeiro!");
        draft.clear();
        assert!(!draft.undo());
        assert!(!draft.redo());
        draft.insert_text("novo");
        draft.undo();
        assert!(draft.is_empty());
        draft.clear();
        assert!(
            !draft.redo(),
            "clear must also discard redo on an empty draft"
        );
    }

    #[test]
    fn edit_history_is_bounded_and_rejected_edits_do_not_destroy_redo() {
        let mut draft = Composer::default();
        for _ in 0..100 {
            draft.insert_text("ab ");
        }
        assert!(draft.undo.len() <= MAX_EDIT_HISTORY_STEPS);
        draft.clear();
        draft.insert_text("á".repeat(MAX_DRAFT_CHARS / 2));
        for _ in 0..30 {
            draft.insert_text(" ");
        }
        assert!(draft.history_bytes <= MAX_EDIT_HISTORY_BYTES);
        draft.undo();
        assert_eq!(
            draft.try_insert_text("x".repeat(MAX_DRAFT_CHARS)),
            Err(ComposerError::DraftTooLarge)
        );
        assert!(draft.redo());
    }

    #[test]
    fn word_edits_preserve_punctuation_graphemes_and_paste_boundaries() {
        let mut draft = Composer::default();
        draft.insert_text("ação/path a\u{301}🙂");
        let original = draft.snapshot();
        assert!(draft.delete_word(false));
        assert_eq!(draft.payload(), "ação/path a\u{301}");
        assert!(draft.delete_word(false));
        assert_eq!(draft.payload(), "ação/path ");
        assert!(draft.undo());
        assert!(draft.undo());
        assert_eq!(draft.snapshot(), original);
        draft.move_home();
        assert!(draft.move_word(true));
        assert_eq!(draft.cursor(), 4);
        assert!(draft.delete_word(true));
        assert_eq!(draft.payload(), "açãopath a\u{301}🙂");
        draft.paste("中文\nbloco");
        let pasted = draft.snapshot();
        assert!(draft.delete_word(false));
        assert_eq!(draft.cursor(), 4);
        assert!(draft.undo());
        assert_eq!(draft.snapshot(), pasted);
        assert!(draft.move_word(false));
        assert_eq!(draft.cursor(), 4);
        assert!(draft.delete_word(true));
        assert_eq!(draft.payload(), "açãopath a\u{301}🙂");
    }

    #[test]
    fn word_delete_is_one_undo_step_and_updates_line_count() {
        let mut draft = Composer::default();
        draft.insert_text("one\n\n  ");
        draft.delete_word(false);
        assert_eq!(draft.payload(), "");
        assert_eq!(draft.line_count(), 1);
        draft.undo();
        assert_eq!(draft.payload(), "one\n\n  ");
        assert_eq!(draft.line_count(), 3);
    }

    #[test]
    fn line_count_matches_display_snapshot() {
        let mut composer = Composer::default();
        assert_eq!(composer.line_count(), 1);
        assert_eq!(
            composer.line_count(),
            composer.display_snapshot(80).total_lines
        );

        composer.insert_text("hello");
        assert_eq!(composer.line_count(), 1);
        assert_eq!(
            composer.line_count(),
            composer.display_snapshot(80).total_lines
        );

        composer.insert_text("\nworld\n");
        assert_eq!(composer.line_count(), 3);
        assert_eq!(
            composer.line_count(),
            composer.display_snapshot(80).total_lines
        );

        let _ = composer.try_paste("multiline\npaste\ncontent");
        assert_eq!(
            composer.line_count(),
            composer.display_snapshot(80).total_lines
        );
    }

    #[test]
    fn snapshot_wraps_by_unicode_cells_without_mutating_payload() {
        let mut composer = Composer::default();
        composer.insert_text("ab界cd");

        let snapshot = composer.display_snapshot(4);
        assert_eq!(snapshot.lines, vec!["ab界", "cd"]);
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_cell, 2);
        assert_eq!(snapshot.total_lines, 2);
        assert_eq!(composer.payload(), "ab界cd");
    }

    #[test]
    fn snapshot_never_splits_a_grapheme_and_moves_boundary_cursor_to_next_row() {
        let mut composer = Composer::default();
        composer.insert_text("a\u{301}bc");

        let snapshot = composer.display_snapshot(2);
        assert_eq!(snapshot.lines, vec!["a\u{301}b", "c"]);
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_cell, 1);

        assert!(composer.move_left());
        let snapshot = composer.display_snapshot(2);
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_cell, 0);
    }

    #[test]
    fn snapshot_keeps_tokenized_paste_atomic_across_visual_rows() {
        let mut composer = Composer::default();
        composer.insert_text("x");
        composer.paste("long\npaste");

        let snapshot = composer.display_snapshot(4);
        assert_eq!(snapshot.lines.len(), 3);
        assert_eq!(snapshot.lines[0], "x");
        assert!(snapshot.lines[1].starts_with("[Pasted Content 0 "));
        assert!(snapshot.lines[1].ends_with(" chars]"));
        assert!(snapshot.lines[1].chars().count() > 4);
        assert_eq!(snapshot.lines[2], "");
        assert_eq!(snapshot.cursor_line, 2);
        assert_eq!(snapshot.cursor_cell, 0);
        assert_eq!(composer.payload(), "xlong\npaste");
    }

    #[test]
    fn snapshot_handles_zero_width_and_exact_row_end() {
        let mut composer = Composer::default();
        composer.insert_text("abcd");

        let exact = composer.display_snapshot(2);
        assert_eq!(exact.lines, vec!["ab", "cd", ""]);
        assert_eq!(exact.cursor_line, 2);
        assert_eq!(exact.cursor_cell, 0);

        let zero = composer.display_snapshot(0);
        assert_eq!(zero.lines, vec!["a", "b", "c", "d", ""]);
        assert_eq!(zero.cursor_line, 4);
        assert_eq!(zero.cursor_cell, 0);
    }

    #[test]
    fn snapshot_places_full_row_cursor_before_newline_on_following_row() {
        let mut composer = Composer::default();
        composer.insert_text("ab\ncd");
        assert!(composer.move_home());
        assert!(composer.move_left());

        let snapshot = composer.display_snapshot(2);
        assert_eq!(snapshot.lines, vec!["ab", "cd"]);
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_cell, 0);
    }

    #[test]
    fn snapshot_keeps_chip_whole_and_wraps_following_text() {
        let mut composer = Composer::default();
        composer.paste("long\npaste");
        composer.insert_text("tail");

        let snapshot = composer.display_snapshot(10);
        assert_eq!(snapshot.lines.len(), 2);
        assert!(snapshot.lines[0].starts_with("[Pasted Content 0 "));
        assert_eq!(snapshot.lines[1], "tail");
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_cell, 4);
        assert_eq!(composer.payload(), "long\npastetail");
    }

    #[test]
    fn snapshot_sanitizes_tabs_and_controls_before_wrapping() {
        let mut composer = Composer::default();
        composer.insert_text("a\tbcd");

        let snapshot = composer.display_snapshot(4);
        assert_eq!(snapshot.lines, vec!["a   ", "bcd"]);
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_cell, 3);
        assert_eq!(composer.payload(), "a\tbcd");

        composer.clear();
        composer.insert_text("ab\u{1b}[31mcd");
        let snapshot = composer.display_snapshot(4);
        assert_eq!(snapshot.lines, vec!["abcd", ""]);
        assert_eq!(snapshot.cursor_line, 1);
        assert_eq!(snapshot.cursor_cell, 0);
        assert_eq!(composer.payload(), "ab\u{1b}[31mcd");
    }
}
