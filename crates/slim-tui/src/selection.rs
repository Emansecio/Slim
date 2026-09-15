use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use unicode_width::UnicodeWidthStr;

/// Inclusive screen cell. Mouse coordinates and ratatui cells share this space.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScreenPos {
    pub x: u16,
    pub y: u16,
}

impl ScreenPos {
    pub fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }
}

/// Terminal-style range: start cell through the cell under the pointer, wrapping by row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScreenSelection {
    pub anchor: ScreenPos,
    pub head: ScreenPos,
}

impl ScreenSelection {
    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    pub fn ordered(self) -> (ScreenPos, ScreenPos) {
        if (self.anchor.y, self.anchor.x) <= (self.head.y, self.head.x) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

pub fn has_copyable_text(text: &str) -> bool {
    text.chars().any(|character| !character.is_whitespace())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextSpan {
    y: u16,
    x0: u16,
    x1: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GlyphSpan {
    start: u16,
    end: u16,
    has_text: bool,
}

fn bounded_area(buffer: &Buffer, area: Rect) -> Rect {
    buffer.area().intersection(area)
}

fn clamp_pos(pos: ScreenPos, area: Rect) -> ScreenPos {
    let last_x = area.right().saturating_sub(1);
    let last_y = area.bottom().saturating_sub(1);
    if pos.y < area.y {
        // A drag released above the content selects from its first cell, independent of the
        // pointer's horizontal coordinate.
        ScreenPos::new(area.x, area.y)
    } else if pos.y >= area.bottom() {
        // Likewise, a drag released below the content reaches its last cell. This matters when a
        // transcript selection is continued into the composer or another lower panel.
        ScreenPos::new(last_x, last_y)
    } else {
        ScreenPos::new(pos.x.clamp(area.x, last_x), pos.y)
    }
}

fn clamp_selection(selection: ScreenSelection, area: Rect) -> Option<(ScreenPos, ScreenPos)> {
    if selection.is_empty() || area.width == 0 || area.height == 0 {
        return None;
    }

    let anchor = clamp_pos(selection.anchor, area);
    let head = clamp_pos(selection.head, area);
    let (start, end) = ScreenSelection { anchor, head }.ordered();
    Some((start, end))
}

fn symbol_width(symbol: &str) -> u16 {
    UnicodeWidthStr::width(symbol)
        .max(1)
        .min(usize::from(u16::MAX)) as u16
}

fn has_text(symbol: &str) -> bool {
    symbol.chars().any(|character| !character.is_whitespace())
}

/// Return grapheme cells for one row. Ratatui stores the continuation cells of a wide grapheme
/// as spaces, so the scan must advance by the lead cell's display width instead of treating each
/// cell independently.
fn row_glyphs(buffer: &Buffer, area: Rect, y: u16) -> Vec<GlyphSpan> {
    let buffer_area = *buffer.area();
    if buffer_area.width == 0 || area.width == 0 {
        return Vec::new();
    }

    let buffer_last_x = buffer_area.right().saturating_sub(1);
    let area_last_x = area.right().saturating_sub(1);
    let mut glyphs = Vec::new();
    let mut x = buffer_area.x;
    while x <= buffer_last_x {
        let symbol = buffer[(x, y)].symbol();
        let end = x
            .saturating_add(symbol_width(symbol).saturating_sub(1))
            .min(buffer_last_x);
        // A region may begin in the continuation cell of a glyph whose lead is outside it. Such
        // a clipped glyph has no copyable lead in this region, so it is deliberately ignored.
        if x >= area.x && x <= area_last_x && end <= area_last_x {
            glyphs.push(GlyphSpan {
                start: x,
                end,
                has_text: has_text(symbol),
            });
        }
        if end == buffer_last_x {
            break;
        }
        x = end.saturating_add(1);
    }
    glyphs
}

fn row_span_for_selection(
    buffer: &Buffer,
    area: Rect,
    start: ScreenPos,
    end: ScreenPos,
    y: u16,
) -> Option<TextSpan> {
    let area_last_x = area.right().saturating_sub(1);
    let raw_start = if y == start.y { start.x } else { area.x };
    let raw_end = if y == end.y { end.x } else { area_last_x };
    if raw_start > raw_end {
        return None;
    }

    let glyphs = row_glyphs(buffer, area, y);
    let mut selected_end = None;
    let mut span_start = raw_start;
    for glyph in glyphs.iter().copied() {
        if !glyph.has_text || glyph.end < raw_start || glyph.start > raw_end {
            continue;
        }
        // Selecting either half of a wide glyph selects the whole textual glyph. This keeps the
        // copied text and the painted cells in agreement even when the pointer lands on a
        // continuation cell.
        span_start = span_start.min(glyph.start);
        selected_end = Some(selected_end.unwrap_or(glyph.end).max(glyph.end));
    }
    let selected_end = selected_end?;
    Some(TextSpan {
        y,
        x0: span_start,
        // Drop whitespace after the last real glyph in this row. `selected_end` is based only on
        // glyphs intersecting the raw pointer span, so text beyond the pointer is never painted.
        x1: selected_end,
    })
}

/// The shared textual projection used by extraction and highlighting.
fn selected_spans(buffer: &Buffer, selection: ScreenSelection, area: Rect) -> Vec<TextSpan> {
    let area = bounded_area(buffer, area);
    let Some((start, end)) = clamp_selection(selection, area) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    let mut y = start.y;
    loop {
        if let Some(span) = row_span_for_selection(buffer, area, start, end, y) {
            spans.push(span);
        }
        if y == end.y {
            break;
        }
        y = y.saturating_add(1);
    }
    spans
}

fn append_span_text(buffer: &Buffer, span: TextSpan, output: &mut String) {
    let buffer_area = *buffer.area();
    let buffer_last_x = buffer_area.right().saturating_sub(1);
    let mut x = buffer_area.x;
    while x <= buffer_last_x {
        let symbol = buffer[(x, span.y)].symbol();
        let end = x
            .saturating_add(symbol_width(symbol).saturating_sub(1))
            .min(buffer_last_x);
        if x >= span.x0 && x <= span.x1 {
            output.push_str(symbol);
        }
        if end >= span.x1 || end == buffer_last_x {
            break;
        }
        x = end.saturating_add(1);
    }
    // The range's right edge is textual, but a cell may itself carry trailing whitespace. Keep
    // indentation and internal spaces while removing only row-end padding.
    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);
}

/// Visible cell text in reading order, bounded to `area`.
pub fn extract_selected_text(buffer: &Buffer, selection: ScreenSelection, area: Rect) -> String {
    let spans = selected_spans(buffer, selection, area);
    let Some(first) = spans.first().copied() else {
        return String::new();
    };

    let mut output = String::new();
    let mut previous_y = first.y;
    for (index, span) in spans.iter().copied().enumerate() {
        if index > 0 {
            for _ in previous_y.saturating_add(1)..=span.y {
                output.push('\n');
            }
        }
        append_span_text(buffer, span, &mut output);
        previous_y = span.y;
    }
    output
}

/// Highlight the same bounded textual spans returned by [`extract_selected_text`].
pub fn highlight_selection(
    buffer: &mut Buffer,
    selection: ScreenSelection,
    area: Rect,
    background: Color,
) {
    for span in selected_spans(buffer, selection, area) {
        let mut x = span.x0;
        loop {
            buffer[(x, span.y)].set_bg(background);
            if x == span.x1 {
                break;
            }
            x = x.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};

    use super::{
        extract_selected_text, has_copyable_text, highlight_selection, ScreenPos, ScreenSelection,
    };

    fn filled(text: &str) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 3));
        buffer.set_string(0, 0, text, Style::default());
        buffer.set_string(0, 1, "second line here", Style::default());
        buffer
    }

    fn whole_buffer() -> Rect {
        Rect::new(0, 0, 16, 3)
    }

    #[test]
    fn empty_range_extracts_nothing() {
        let buffer = filled("hello world");
        let selection = ScreenSelection {
            anchor: ScreenPos::new(2, 0),
            head: ScreenPos::new(2, 0),
        };
        assert_eq!(
            extract_selected_text(&buffer, selection, whole_buffer()),
            ""
        );
        assert!(!has_copyable_text(""));
    }

    #[test]
    fn single_row_includes_both_ends() {
        let buffer = filled("hello world");
        let selection = ScreenSelection {
            anchor: ScreenPos::new(0, 0),
            head: ScreenPos::new(4, 0),
        };
        assert_eq!(
            extract_selected_text(&buffer, selection, whole_buffer()),
            "hello"
        );
    }

    #[test]
    fn reverse_drag_uses_reading_order() {
        let buffer = filled("hello world");
        let selection = ScreenSelection {
            anchor: ScreenPos::new(10, 0),
            head: ScreenPos::new(6, 0),
        };
        assert_eq!(
            extract_selected_text(&buffer, selection, whole_buffer()),
            "world"
        );
    }

    #[test]
    fn wrapped_range_joins_rows() {
        let buffer = filled("hello world");
        let selection = ScreenSelection {
            anchor: ScreenPos::new(6, 0),
            head: ScreenPos::new(5, 1),
        };
        assert_eq!(
            extract_selected_text(&buffer, selection, whole_buffer()),
            "world\nsecond"
        );
    }

    #[test]
    fn multiline_selection_drops_trailing_empty_rows_and_padding() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 4));
        buffer.set_string(0, 0, "  first", Style::default());
        buffer.set_string(0, 1, "second", Style::default());
        let selection = ScreenSelection {
            anchor: ScreenPos::new(0, 0),
            head: ScreenPos::new(11, 3),
        };

        assert_eq!(
            extract_selected_text(&buffer, selection, *buffer.area()),
            "  first\nsecond"
        );
        highlight_selection(&mut buffer, selection, whole_buffer(), Color::Green);
        for x in 0..12 {
            assert_eq!(buffer[(x, 2)].bg, Color::Reset);
            assert_eq!(buffer[(x, 3)].bg, Color::Reset);
        }
        assert_eq!(buffer[(0, 0)].bg, Color::Green);
        assert_eq!(buffer[(7, 0)].bg, Color::Reset);
        assert_eq!(buffer[(5, 1)].bg, Color::Green);
        assert_eq!(buffer[(6, 1)].bg, Color::Reset);
    }

    #[test]
    fn wide_glyph_continuation_is_not_copied_but_is_highlighted() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        buffer.set_string(0, 0, "界 x", Style::default());
        let selection = ScreenSelection {
            anchor: ScreenPos::new(0, 0),
            head: ScreenPos::new(4, 0),
        };

        assert_eq!(
            extract_selected_text(&buffer, selection, whole_buffer()),
            "界 x"
        );
        highlight_selection(&mut buffer, selection, whole_buffer(), Color::Green);
        for x in 0..4 {
            assert_eq!(buffer[(x, 0)].bg, Color::Green);
        }
        assert_eq!(buffer[(4, 0)].bg, Color::Reset);
    }

    #[test]
    fn wide_glyph_clipped_by_area_edge_is_not_partially_selected() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer.set_string(0, 0, "界", Style::default());
        let selection = ScreenSelection {
            anchor: ScreenPos::new(0, 0),
            head: ScreenPos::new(1, 0),
        };
        let area = Rect::new(0, 0, 1, 1);

        assert_eq!(extract_selected_text(&buffer, selection, area), "");
        highlight_selection(&mut buffer, selection, area, Color::Green);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
    }

    #[test]
    fn selection_area_clamps_and_empty_intersection_is_safe() {
        let buffer = filled("hello world");
        let selection = ScreenSelection {
            anchor: ScreenPos::new(u16::MAX, u16::MAX),
            head: ScreenPos::new(0, 0),
        };
        let area = Rect::new(2, 0, 8, 2);
        assert_eq!(
            extract_selected_text(&buffer, selection, area),
            "llo worl\ncond lin"
        );
        let empty = Rect::new(30, 30, 2, 2);
        assert_eq!(extract_selected_text(&buffer, selection, empty), "");
        let mut buffer = buffer;
        highlight_selection(&mut buffer, selection, empty, Color::Green);
    }

    #[test]
    fn vertical_drag_outside_one_row_area_reaches_the_nearest_corner() {
        let area = Rect::new(4, 7, 8, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(4, 7, "content", Style::default());

        let below = ScreenSelection {
            anchor: ScreenPos::new(4, 7),
            head: ScreenPos::new(0, 23),
        };
        assert_eq!(extract_selected_text(&buffer, below, area), "content");

        let above = ScreenSelection {
            anchor: ScreenPos::new(11, 7),
            head: ScreenPos::new(u16::MAX, 0),
        };
        assert_eq!(extract_selected_text(&buffer, above, area), "content");

        highlight_selection(&mut buffer, below, area, Color::Green);
        for x in 4..11 {
            assert_eq!(buffer[(x, 7)].bg, Color::Green);
        }
        assert_eq!(buffer[(11, 7)].bg, Color::Reset);
    }

    #[test]
    fn non_empty_drag_keeps_the_only_cell_of_a_one_cell_area() {
        let area = Rect::new(2, 2, 1, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(2, 2, "x", Style::default());
        let selection = ScreenSelection {
            anchor: ScreenPos::new(0, 0),
            head: ScreenPos::new(u16::MAX, u16::MAX),
        };

        assert_eq!(extract_selected_text(&buffer, selection, area), "x");
        highlight_selection(&mut buffer, selection, area, Color::Green);
        assert_eq!(buffer[(2, 2)].bg, Color::Green);
    }

    #[test]
    fn leading_and_internal_spaces_are_preserved() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        buffer.set_string(0, 0, "  a  b  ", Style::default());
        let selection = ScreenSelection {
            anchor: ScreenPos::new(0, 0),
            head: ScreenPos::new(7, 0),
        };
        assert_eq!(
            extract_selected_text(&buffer, selection, whole_buffer()),
            "  a  b"
        );
    }

    #[test]
    fn highlighting_and_copy_use_identical_row_bounds() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer.set_string(0, 0, "a   b", Style::default());
        buffer.set_string(0, 1, "   ", Style::default());
        let selection = ScreenSelection {
            anchor: ScreenPos::new(0, 0),
            head: ScreenPos::new(11, 1),
        };
        assert_eq!(
            extract_selected_text(&buffer, selection, whole_buffer()),
            "a   b"
        );
        highlight_selection(&mut buffer, selection, whole_buffer(), Color::Green);
        assert_eq!(buffer[(0, 0)].bg, Color::Green);
        assert_eq!(buffer[(4, 0)].bg, Color::Green);
        assert_eq!(buffer[(5, 0)].bg, Color::Reset);
        for x in 0..12 {
            assert_eq!(buffer[(x, 1)].bg, Color::Reset);
        }
    }
}
