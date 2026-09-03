use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) const PICKER_NOMINAL_CAPACITY: usize = 14;

pub fn move_selection(selected: usize, total: usize, delta: isize) -> usize {
    if total == 0 {
        return 0;
    }
    if delta.is_negative() {
        selected.saturating_sub(delta.unsigned_abs()).min(total - 1)
    } else {
        selected.saturating_add(delta as usize).min(total - 1)
    }
}

pub fn ensure_visible_start(
    preferred_start: usize,
    selected: usize,
    total: usize,
    capacity: usize,
) -> usize {
    if total == 0 {
        return 0;
    }
    let capacity = capacity.max(1).min(total);
    let max_start = total.saturating_sub(capacity);
    let mut start = preferred_start.min(max_start);
    if selected < start {
        start = selected;
    } else if selected >= start.saturating_add(capacity) {
        start = selected.saturating_add(1).saturating_sub(capacity);
    }
    start.min(max_start)
}

pub fn visible_window(
    total: usize,
    selected: usize,
    capacity: usize,
    preferred_start: usize,
) -> Range<usize> {
    let start = ensure_visible_start(preferred_start, selected, total, capacity);
    start..start.saturating_add(capacity.max(1)).min(total)
}

pub fn truncate_cells(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut output = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(cells) > width - 1 {
            break;
        }
        output.push_str(grapheme);
        used = used.saturating_add(cells);
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::{ensure_visible_start, move_selection, truncate_cells, visible_window};

    #[test]
    fn selected_row_is_kept_inside_the_window() {
        assert_eq!(visible_window(100, 80, 16, 0), 65..81);
        assert_eq!(ensure_visible_start(65, 4, 100, 16), 4);
    }

    #[test]
    fn movement_and_truncation_are_bounded() {
        assert_eq!(move_selection(0, 3, -1), 0);
        assert_eq!(move_selection(2, 3, 1), 2);
        assert_eq!(truncate_cells("abcdef", 4), "abc…");
    }
}
