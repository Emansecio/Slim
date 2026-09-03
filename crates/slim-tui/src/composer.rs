use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextElement {
    Text(String),
    Paste { id: u64, payload: String },
}

pub const MAX_DRAFT_CHARS: usize = 1_048_576;

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
        self.char_count += added_chars;
        if self.cursor == self.char_count - added_chars
            && self.cursor == self.elements.iter().map(element_chars).sum::<usize>()
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
        let id = self.next_paste_id;
        self.next_paste_id += 1;
        self.char_count += added_chars;
        self.insert_element(TextElement::Paste { id, payload });
        self.cursor += added_chars;
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
                    display.push_str(&paste_display(*id, payload, width));
                    if cursor_byte.is_none() && self.cursor == raw_offset + element_len {
                        cursor_byte = Some(display.len());
                    }
                }
            }
            raw_offset += element_len;
        }
        let cursor_byte = cursor_byte.unwrap_or(display.len()).min(display.len());
        let before_cursor = &display[..cursor_byte];
        let cursor_line = before_cursor
            .chars()
            .filter(|character| *character == '\n')
            .count();
        let cursor_tail = before_cursor.rsplit('\n').next().unwrap_or_default();
        let cursor_cell = unicode_width::UnicodeWidthStr::width(cursor_tail);
        let lines = display.split('\n').map(str::to_owned).collect::<Vec<_>>();
        let total_lines = lines.len().max(1);
        DisplaySnapshot {
            lines,
            cursor_line: cursor_line.min(total_lines - 1),
            cursor_cell,
            total_lines,
        }
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

    pub fn is_empty(&self) -> bool {
        self.char_count == 0
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn remove_last_element(&mut self) -> Option<TextElement> {
        let element = self.elements.pop()?;
        let removed_chars = match &element {
            TextElement::Text(text) => text.chars().count(),
            TextElement::Paste { payload, .. } => payload.chars().count(),
        };
        self.char_count = self.char_count.saturating_sub(removed_chars);
        self.cursor = self.cursor.min(self.char_count);
        Some(element)
    }

    /// Removes the last grapheme cluster from the draft (§15.3). A trailing
    /// paste segment is removed as a whole; returns whether anything changed.
    pub fn backspace(&mut self) -> bool {
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
                text.replace_range(boundary..cursor_byte, "");
                self.char_count -= removed;
                self.cursor -= removed;
                if text.is_empty() {
                    self.elements.remove(index);
                }
            }
        }
        self.normalize_text_elements();
        true
    }

    pub fn delete(&mut self) -> bool {
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
                text.replace_range(cursor_byte..cursor_byte + removed_bytes, "");
                self.char_count -= removed_chars;
                if text.is_empty() {
                    self.elements.remove(index);
                }
            }
        }
        self.normalize_text_elements();
        true
    }

    pub fn move_left(&mut self) -> bool {
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
        true
    }

    pub fn move_right(&mut self) -> bool {
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
        true
    }

    pub fn move_home(&mut self) -> bool {
        let payload = self.payload();
        let target = payload
            .chars()
            .take(self.cursor)
            .enumerate()
            .filter_map(|(index, character)| (character == '\n').then_some(index + 1))
            .last()
            .unwrap_or(0);
        let target = self.snap_out_of_paste(target, false);
        let changed = target != self.cursor;
        self.cursor = target;
        changed
    }

    pub fn move_end(&mut self) -> bool {
        let payload = self.payload();
        let target = payload
            .chars()
            .enumerate()
            .skip(self.cursor)
            .find_map(|(index, character)| (character == '\n').then_some(index))
            .unwrap_or(self.char_count);
        let target = self.snap_out_of_paste(target, true);
        let changed = target != self.cursor;
        self.cursor = target;
        changed
    }

    pub fn clear(&mut self) {
        self.elements.clear();
        self.char_count = 0;
        self.cursor = 0;
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

fn paste_display(id: u64, payload: &str, width: usize) -> String {
    let chars = payload.chars().count();
    if !payload.contains('\n') && !payload.contains('\r') && chars <= width {
        payload.to_owned()
    } else {
        format!("[Pasted Content {id} {chars} chars]")
    }
}
