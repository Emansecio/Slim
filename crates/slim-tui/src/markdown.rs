use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
pub(crate) struct MarkdownStyles {
    pub text: Style,
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub link: Style,
    pub code: Style,
    pub code_block: Style,
    pub code_rail: Style,
    pub diff_add: Style,
    pub diff_remove: Style,
    pub diff_add_bg: Style,
    pub diff_remove_bg: Style,
    pub quote: Style,
}

#[derive(Clone, Copy)]
enum Tone {
    Text,
    H1,
    H2,
    H3,
    Link,
    Code,
    CodeBlock,
    Quote,
}

#[derive(Clone)]
struct Piece {
    text: String,
    tone: Tone,
    modifiers: Modifier,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LineKind {
    #[default]
    Text,
    Code,
    DiffAdd,
    DiffRemove,
    DiffHeader,
    DiffContext,
}

impl LineKind {
    fn is_code(self) -> bool {
        self != Self::Text
    }
}

#[derive(Default)]
pub(crate) struct LogicalLine {
    pieces: Vec<Piece>,
    kind: LineKind,
}

impl LogicalLine {
    fn push(&mut self, text: impl AsRef<str>, tone: Tone, modifiers: Modifier) {
        let text = text.as_ref();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.pieces.last_mut() {
            if std::mem::discriminant(&last.tone) == std::mem::discriminant(&tone)
                && last.modifiers == modifiers
            {
                last.text.push_str(text);
                return;
            }
        }
        self.pieces.push(Piece {
            text: text.to_owned(),
            tone,
            modifiers,
        });
    }

    fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }
}

#[derive(Clone, Copy)]
enum BlockTone {
    Text,
    Heading(HeadingLevel),
    Quote,
    Code { diff: bool },
}

struct Projection {
    lines: Vec<LogicalLine>,
    current: LogicalLine,
    block_tone: BlockTone,
    strong_depth: usize,
    emphasis_depth: usize,
    link_depth: usize,
    code_depth: usize,
    list_stack: Vec<Option<u64>>,
    item_depth: usize,
    width: usize,
    table: Option<TableBuilder>,
}

#[derive(Default)]
struct TableBuilder {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
}

impl Default for Projection {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            current: LogicalLine::default(),
            block_tone: BlockTone::Text,
            strong_depth: 0,
            emphasis_depth: 0,
            link_depth: 0,
            code_depth: 0,
            list_stack: Vec::new(),
            item_depth: 0,
            width: 1,
            table: None,
        }
    }
}

impl Projection {
    fn tone(&self) -> Tone {
        if self.code_depth > 0 {
            Tone::Code
        } else if self.link_depth > 0 {
            Tone::Link
        } else {
            match self.block_tone {
                BlockTone::Heading(HeadingLevel::H1) => Tone::H1,
                BlockTone::Heading(HeadingLevel::H2) => Tone::H2,
                BlockTone::Heading(HeadingLevel::H3) => Tone::H3,
                BlockTone::Heading(_) => Tone::H2,
                BlockTone::Quote => Tone::Quote,
                BlockTone::Code { .. } => Tone::CodeBlock,
                BlockTone::Text => Tone::Text,
            }
        }
    }

    fn modifiers(&self) -> Modifier {
        let mut modifiers = Modifier::empty();
        if self.strong_depth > 0 {
            modifiers.insert(Modifier::BOLD);
        }
        if self.emphasis_depth > 0 {
            modifiers.insert(Modifier::ITALIC);
        }
        modifiers
    }

    fn push_text(&mut self, text: &str) {
        let tone = self.tone();
        let modifiers = self.modifiers();
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            self.current.push(part, tone, modifiers);
            if parts.peek().is_some() {
                self.finish_line(true);
            }
        }
    }

    fn finish_line(&mut self, keep_empty: bool) {
        if keep_empty || !self.current.is_empty() {
            self.current.kind = match self.block_tone {
                BlockTone::Code { diff: false } => LineKind::Code,
                BlockTone::Code { diff: true } => classify_diff_line(&self.current),
                BlockTone::Text | BlockTone::Heading(_) | BlockTone::Quote => LineKind::Text,
            };
            self.lines.push(std::mem::take(&mut self.current));
        }
    }

    fn start_item(&mut self) {
        self.item_depth += 1;
        let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
        let marker = self
            .list_stack
            .last_mut()
            .and_then(Option::as_mut)
            .map_or_else(
                || "• ".to_owned(),
                |next| {
                    let marker = format!("{next}. ");
                    *next = next.saturating_add(1);
                    marker
                },
            );
        self.current
            .push(format!("{indent}{marker}"), Tone::Quote, Modifier::empty());
    }

    fn at_root(&self) -> bool {
        self.item_depth == 0 && self.list_stack.is_empty()
    }

    fn gap_before_block(&mut self) {
        if !self.at_root() {
            return;
        }
        self.finish_line(false);
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(LogicalLine::default());
        }
    }

    fn push_in_scope(&mut self, text: &str) {
        if let Some(table) = &mut self.table {
            table.current_cell.push_str(text);
            return;
        }
        self.push_text(text);
    }

    fn flush_table_cell(&mut self) {
        if let Some(table) = &mut self.table {
            table
                .current_row
                .push(std::mem::take(&mut table.current_cell));
        }
    }

    fn flush_table_row(&mut self) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        if !table.current_cell.is_empty() {
            table
                .current_row
                .push(std::mem::take(&mut table.current_cell));
        }
        if !table.current_row.is_empty() {
            table.rows.push(std::mem::take(&mut table.current_row));
        }
    }

    fn emit_table(&mut self) {
        self.flush_table_row();
        let Some(table) = self.table.take() else {
            return;
        };
        let rows = table.rows;
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let mut widths = vec![0usize; cols];
        for row in &rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
        let separator = 2usize;
        let total = widths.iter().sum::<usize>() + separator.saturating_mul(cols.saturating_sub(1));
        if total > self.width {
            self.emit_table_fallback(&rows, cols);
            return;
        }
        for (row_index, row) in rows.iter().enumerate() {
            let mut line = String::new();
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    line.push_str("  ");
                }
                let cell = row.get(index).map(String::as_str).unwrap_or("");
                line.push_str(&pad_cells(cell, *width));
            }
            let tone = if row_index == 0 { Tone::H3 } else { Tone::Text };
            self.current.push(line, tone, Modifier::empty());
            self.finish_line(false);
            if row_index == 0 {
                let mut rule = String::new();
                for (index, width) in widths.iter().enumerate() {
                    if index > 0 {
                        rule.push_str("  ");
                    }
                    rule.push_str(&"─".repeat((*width).max(1)));
                }
                self.current.push(rule, Tone::Quote, Modifier::empty());
                self.finish_line(false);
            }
        }
    }

    fn emit_table_fallback(&mut self, rows: &[Vec<String>], cols: usize) {
        let headers = rows.first();
        let labels = (0..cols)
            .map(|index| {
                headers
                    .and_then(|row| row.get(index))
                    .filter(|header| !header.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("Column {}", index + 1))
            })
            .collect::<Vec<_>>();

        let body_rows = rows.iter().skip(1);
        if body_rows.clone().next().is_none() {
            for label in &labels {
                self.push_table_field(label, "");
            }
            return;
        }

        for row in body_rows {
            for (index, label) in labels.iter().enumerate() {
                let value = row.get(index).map(String::as_str).unwrap_or("");
                self.push_table_field(label, value);
            }
        }
    }

    fn push_table_field(&mut self, header: &str, value: &str) {
        self.current
            .push(format!("{header}: "), Tone::H3, Modifier::empty());
        self.current.push(value, Tone::Text, Modifier::empty());
        self.finish_line(false);
    }
}

fn pad_cells(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(cells) > width {
            break;
        }
        out.push_str(grapheme);
        used = used.saturating_add(cells);
    }
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

#[cfg(test)]
pub(crate) fn render_markdown(
    source: &str,
    width: u16,
    styles: MarkdownStyles,
) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    wrap(&project(source, width), width, styles)
}

/// Projects markdown into logical lines without styling so the same parse can
/// feed both row counting and rendering (§12.2/§12.4 shared derivation).
pub(crate) fn project_markdown(source: &str, width: u16) -> Vec<LogicalLine> {
    project(source, width.max(1) as usize)
}

pub(crate) fn projected_row_count(lines: &[LogicalLine], width: u16) -> usize {
    let width = width.max(1) as usize;
    lines
        .iter()
        .map(|line| wrapped_line_count(line, width))
        .sum::<usize>()
        .max(1)
}

pub(crate) fn render_projected(
    lines: &[LogicalLine],
    width: u16,
    styles: MarkdownStyles,
) -> Vec<Line<'static>> {
    wrap(lines, width.max(1) as usize, styles)
}

#[cfg(test)]
pub(crate) fn markdown_row_count(source: &str, width: u16) -> usize {
    projected_row_count(&project(source, width.max(1) as usize), width)
}

pub(crate) fn render_plain(source: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let safe = sanitize_terminal_text_cow(source);
    let mut rows = Vec::new();
    for line in safe.split('\n') {
        let mut row = String::new();
        let mut used = 0usize;
        for grapheme in line.graphemes(true) {
            let (display, cells) = normalized_grapheme(grapheme, width);
            if starts_new_row(used, cells, width) {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(display);
            used = used.saturating_add(cells);
        }
        rows.push(row);
    }
    rows
}

pub(crate) fn plain_row_count(source: &str, width: u16) -> usize {
    let width = width.max(1) as usize;
    if is_safe_ascii(source) {
        return source
            .split('\n')
            .map(|line| line.len().div_ceil(width).max(1))
            .sum();
    }
    let safe = sanitize_terminal_text_cow(source);
    safe.split('\n')
        .map(|line| {
            let mut rows = 1usize;
            let mut used = 0usize;
            for grapheme in line.graphemes(true) {
                let (_, cells) = normalized_grapheme(grapheme, width);
                if starts_new_row(used, cells, width) {
                    rows += 1;
                    used = 0;
                }
                used = used.saturating_add(cells);
            }
            rows
        })
        .sum::<usize>()
        .max(1)
}

fn project(source: &str, width: usize) -> Vec<LogicalLine> {
    let safe = sanitize_terminal_text_cow(source);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let mut projection = Projection {
        width: width.max(1),
        ..Projection::default()
    };
    for event in Parser::new_ext(&safe, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                projection.gap_before_block();
                projection.block_tone = BlockTone::Heading(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                projection.finish_line(false);
                projection.block_tone = BlockTone::Text;
            }
            Event::Start(Tag::Paragraph) => {
                projection.gap_before_block();
            }
            Event::End(TagEnd::Paragraph) => projection.finish_line(false),
            Event::Start(Tag::BlockQuote(_)) => {
                projection.gap_before_block();
                projection.block_tone = BlockTone::Quote;
                projection
                    .current
                    .push("│ ", Tone::Quote, Modifier::empty());
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                projection.finish_line(false);
                projection.block_tone = BlockTone::Text;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                projection.gap_before_block();
                let diff = matches!(
                    kind,
                    CodeBlockKind::Fenced(ref label)
                        if matches!(label.trim().to_ascii_lowercase().as_str(), "diff" | "patch")
                );
                projection.block_tone = BlockTone::Code { diff };
            }
            Event::End(TagEnd::CodeBlock) => {
                projection.finish_line(false);
                projection.block_tone = BlockTone::Text;
            }
            Event::Start(Tag::List(start)) => {
                if projection.item_depth > 0 {
                    projection.finish_line(false);
                } else {
                    projection.gap_before_block();
                }
                projection.list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                projection.list_stack.pop();
            }
            Event::Start(Tag::Item) => projection.start_item(),
            Event::End(TagEnd::Item) => {
                projection.finish_line(false);
                projection.item_depth = projection.item_depth.saturating_sub(1);
            }
            Event::Start(Tag::Table(_)) => {
                projection.gap_before_block();
                projection.table = Some(TableBuilder::default());
            }
            Event::End(TagEnd::Table) => projection.emit_table(),
            Event::Start(Tag::TableHead | Tag::TableRow | Tag::TableCell) => {}
            Event::End(TagEnd::TableCell) => projection.flush_table_cell(),
            Event::End(TagEnd::TableRow | TagEnd::TableHead) => projection.flush_table_row(),
            Event::Start(Tag::Strong) => projection.strong_depth += 1,
            Event::End(TagEnd::Strong) => {
                projection.strong_depth = projection.strong_depth.saturating_sub(1);
            }
            Event::Start(Tag::Emphasis) => projection.emphasis_depth += 1,
            Event::End(TagEnd::Emphasis) => {
                projection.emphasis_depth = projection.emphasis_depth.saturating_sub(1);
            }
            Event::Start(Tag::Link { .. }) => projection.link_depth += 1,
            Event::End(TagEnd::Link) => {
                projection.link_depth = projection.link_depth.saturating_sub(1);
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                projection.push_in_scope(&text);
            }
            Event::Code(text) => {
                if projection.table.is_some() {
                    projection.push_in_scope(&text);
                } else {
                    projection.code_depth += 1;
                    projection.push_text(&text);
                    projection.code_depth = projection.code_depth.saturating_sub(1);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if projection.table.is_some() {
                    projection.push_in_scope(" ");
                } else {
                    projection.finish_line(true);
                }
            }
            Event::Rule => {
                projection.gap_before_block();
                projection
                    .current
                    .push("────", Tone::Quote, Modifier::empty());
                projection.finish_line(false);
            }
            Event::TaskListMarker(checked) => projection.current.push(
                if checked { "[✓] " } else { "[ ] " },
                Tone::Quote,
                Modifier::empty(),
            ),
            Event::InlineMath(text) | Event::DisplayMath(text) => projection.push_in_scope(&text),
            Event::Start(_) | Event::End(_) | Event::FootnoteReference(_) => {}
        }
    }
    projection.finish_line(false);
    projection.lines
}

fn wrapped_line_count(line: &LogicalLine, width: usize) -> usize {
    if is_horizontal_rule(line) {
        return 1;
    }
    let width = if line.kind.is_code() {
        width.saturating_sub(2).max(1)
    } else {
        width
    };
    let count = if line.kind.is_code() {
        count_hard_wrapped(&line.pieces, width)
    } else {
        count_prose_lines(&line.pieces, width)
    };
    #[cfg(test)]
    {
        let reference = wrap_pieces(line, width).len();
        assert_eq!(
            count, reference,
            "wrapped_line_count mismatch: count={count} reference={reference}, width={width}"
        );
    }
    count
}

fn count_hard_wrapped(pieces: &[Piece], width: usize) -> usize {
    let mut rows = 1usize;
    let mut used = 0usize;
    for piece in pieces {
        for grapheme in piece.text.graphemes(true) {
            let (_, cells) = normalized_grapheme(grapheme, width);
            if starts_new_row(used, cells, width) {
                rows += 1;
                used = 0;
            }
            used = used.saturating_add(cells);
        }
    }
    rows
}

struct MetricToken<'a> {
    slices: Vec<(&'a str, usize)>,
    width: usize,
    whitespace: bool,
}

fn count_prose_lines(pieces: &[Piece], width: usize) -> usize {
    let mut tokens: Vec<MetricToken> = Vec::new();
    for piece in pieces {
        for grapheme in piece.text.graphemes(true) {
            let (display, cells) = normalized_grapheme(grapheme, width);
            let whitespace = display.chars().all(char::is_whitespace);
            if tokens.last().is_none_or(|t| t.whitespace != whitespace) {
                tokens.push(MetricToken {
                    slices: Vec::new(),
                    width: 0,
                    whitespace,
                });
            }
            let token = tokens.last_mut().expect("token inserted");
            token.slices.push((display, cells));
            token.width = token.width.saturating_add(cells);
        }
    }

    if tokens.is_empty() {
        return 1;
    }

    let mut rows = 1usize;
    let mut used = 0usize;
    let mut row_has_word = false;
    let mut pending_space: Option<&MetricToken> = None;

    for token in &tokens {
        if token.whitespace {
            if used == 0 && rows == 1 && !row_has_word {
                for &(_, cells) in &token.slices {
                    if starts_new_row(used, cells, width) {
                        rows += 1;
                        used = 0;
                    }
                    used = used.saturating_add(cells);
                }
            } else {
                pending_space = Some(token);
            }
            continue;
        }

        let pending_width = pending_space.map_or(0, |s| s.width);
        if row_has_word
            && used
                .saturating_add(pending_width)
                .saturating_add(token.width)
                > width
        {
            rows += 1;
            used = 0;
            pending_space = None;
        }

        if let Some(space) = pending_space.take() {
            for &(_, cells) in &space.slices {
                if starts_new_row(used, cells, width) {
                    rows += 1;
                    used = 0;
                }
                used = used.saturating_add(cells);
            }
        }
        for &(_, cells) in &token.slices {
            if starts_new_row(used, cells, width) {
                rows += 1;
                used = 0;
            }
            used = used.saturating_add(cells);
        }
        row_has_word = true;
    }
    rows
}

fn wrap(logical: &[LogicalLine], width: usize, styles: MarkdownStyles) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for line in logical {
        if is_horizontal_rule(line) {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.max(1)),
                styles.quote,
            )));
            continue;
        }
        let kind = line.kind;
        let content_width = if kind.is_code() {
            width.saturating_sub(2).max(1)
        } else {
            width
        };
        for row in wrap_pieces(line, content_width) {
            let mut spans = Vec::new();
            for piece in row {
                let style = style_for(piece.tone, piece.modifiers, styles);
                push_span(&mut spans, &piece.text, style);
            }
            lines.push(decorate_code_row(spans, kind, width, styles));
        }
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

struct WrapToken {
    pieces: Vec<Piece>,
    width: usize,
    whitespace: bool,
}

fn wrap_pieces(line: &LogicalLine, width: usize) -> Vec<Vec<Piece>> {
    if line.kind.is_code() {
        return hard_wrap_pieces(&line.pieces, width);
    }
    let tokens = wrap_tokens(&line.pieces, width);
    if tokens.is_empty() {
        return vec![Vec::new()];
    }

    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut used = 0usize;
    let mut row_has_word = false;
    let mut pending_space: Option<WrapToken> = None;
    for token in tokens {
        if token.whitespace {
            if used == 0 && rows.is_empty() {
                append_hard_wrapped(&token.pieces, width, &mut rows, &mut row, &mut used);
            } else {
                pending_space = Some(token);
            }
            continue;
        }

        let pending_width = pending_space.as_ref().map_or(0, |space| space.width);
        if row_has_word
            && used
                .saturating_add(pending_width)
                .saturating_add(token.width)
                > width
        {
            rows.push(std::mem::take(&mut row));
            used = 0;
            pending_space = None;
        }

        if let Some(space) = pending_space.take() {
            append_hard_wrapped(&space.pieces, width, &mut rows, &mut row, &mut used);
        }
        append_hard_wrapped(&token.pieces, width, &mut rows, &mut row, &mut used);
        row_has_word = true;
    }
    rows.push(row);
    rows
}

fn hard_wrap_pieces(pieces: &[Piece], width: usize) -> Vec<Vec<Piece>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut used = 0usize;
    append_hard_wrapped(pieces, width, &mut rows, &mut row, &mut used);
    rows.push(row);
    rows
}

fn append_hard_wrapped(
    pieces: &[Piece],
    width: usize,
    rows: &mut Vec<Vec<Piece>>,
    row: &mut Vec<Piece>,
    used: &mut usize,
) {
    for piece in pieces {
        for grapheme in piece.text.graphemes(true) {
            let (display, cells) = normalized_grapheme(grapheme, width);
            if starts_new_row(*used, cells, width) {
                rows.push(std::mem::take(row));
                *used = 0;
            }
            push_piece(row, display, piece.tone, piece.modifiers);
            *used = used.saturating_add(cells);
        }
    }
}

fn wrap_tokens(pieces: &[Piece], width: usize) -> Vec<WrapToken> {
    let mut tokens: Vec<WrapToken> = Vec::new();
    for piece in pieces {
        for grapheme in piece.text.graphemes(true) {
            let (display, cells) = normalized_grapheme(grapheme, width);
            let whitespace = display.chars().all(char::is_whitespace);
            if tokens
                .last()
                .is_none_or(|token| token.whitespace != whitespace)
            {
                tokens.push(WrapToken {
                    pieces: Vec::new(),
                    width: 0,
                    whitespace,
                });
            }
            let token = tokens.last_mut().expect("token was inserted");
            push_piece(&mut token.pieces, display, piece.tone, piece.modifiers);
            token.width = token.width.saturating_add(cells);
        }
    }
    tokens
}

fn push_piece(pieces: &mut Vec<Piece>, text: &str, tone: Tone, modifiers: Modifier) {
    if let Some(last) = pieces.last_mut() {
        if std::mem::discriminant(&last.tone) == std::mem::discriminant(&tone)
            && last.modifiers == modifiers
        {
            last.text.push_str(text);
            return;
        }
    }
    pieces.push(Piece {
        text: text.to_owned(),
        tone,
        modifiers,
    });
}

fn is_horizontal_rule(line: &LogicalLine) -> bool {
    !line.pieces.is_empty()
        && line
            .pieces
            .iter()
            .all(|piece| !piece.text.is_empty() && piece.text.chars().all(|glyph| glyph == '─'))
}

fn normalized_grapheme(grapheme: &str, width: usize) -> (&str, usize) {
    let cells = grapheme.width();
    if cells > width {
        ("�", 1)
    } else {
        (grapheme, cells)
    }
}

fn starts_new_row(used: usize, cells: usize, width: usize) -> bool {
    used > 0 && used.saturating_add(cells) > width
}

fn push_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(text);
            return;
        }
    }
    spans.push(Span::styled(text.to_owned(), style));
}

fn style_for(tone: Tone, modifiers: Modifier, styles: MarkdownStyles) -> Style {
    let style = match tone {
        Tone::Text => styles.text,
        Tone::H1 => styles.h1,
        Tone::H2 => styles.h2,
        Tone::H3 => styles.h3,
        Tone::Link => styles.link,
        Tone::Code => styles.code,
        Tone::CodeBlock => styles.code_block,
        Tone::Quote => styles.quote,
    };
    style.add_modifier(modifiers)
}

fn classify_diff_line(line: &LogicalLine) -> LineKind {
    let first = line
        .pieces
        .iter()
        .flat_map(|piece| piece.text.chars())
        .next();
    match first {
        Some('+') => LineKind::DiffAdd,
        Some('-') => LineKind::DiffRemove,
        Some('@') => LineKind::DiffHeader,
        _ => LineKind::DiffContext,
    }
}

fn decorate_code_row(
    mut spans: Vec<Span<'static>>,
    kind: LineKind,
    width: usize,
    styles: MarkdownStyles,
) -> Line<'static> {
    if !kind.is_code() {
        return Line::from(spans);
    }
    let background = match kind {
        LineKind::DiffAdd => styles.diff_add_bg,
        LineKind::DiffRemove => styles.diff_remove_bg,
        LineKind::Code | LineKind::DiffHeader | LineKind::DiffContext => styles.code_block,
        LineKind::Text => Style::default(),
    };
    for span in &mut spans {
        span.style = span.style.patch(background);
    }
    if matches!(kind, LineKind::DiffAdd | LineKind::DiffRemove) {
        style_diff_marker(
            &mut spans,
            if kind == LineKind::DiffAdd {
                styles.diff_add.patch(background)
            } else {
                styles.diff_remove.patch(background)
            },
        );
    }
    let used = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let mut decorated = Vec::with_capacity(spans.len() + 2);
    decorated.push(Span::styled("│ ", styles.code_rail.patch(background)));
    decorated.extend(spans);
    decorated.push(Span::styled(
        " ".repeat(width.saturating_sub(2).saturating_sub(used)),
        background,
    ));
    Line::from(decorated)
}

fn style_diff_marker(spans: &mut Vec<Span<'static>>, marker_style: Style) {
    let Some(first) = spans.first_mut() else {
        return;
    };
    let Some(marker) = first.content.chars().next() else {
        return;
    };
    if !matches!(marker, '+' | '-') {
        return;
    }
    let remainder = first.content[marker.len_utf8()..].to_owned();
    let remainder_style = first.style;
    first.content = marker.to_string().into();
    first.style = marker_style;
    if !remainder.is_empty() {
        spans.insert(1, Span::styled(remainder, remainder_style));
    }
}

fn is_safe_ascii(source: &str) -> bool {
    source
        .bytes()
        .all(|byte| byte == b'\n' || (b' '..=b'~').contains(&byte))
}

pub(crate) fn sanitize_terminal_text_cow(source: &str) -> std::borrow::Cow<'_, str> {
    if is_safe_ascii(source) {
        return std::borrow::Cow::Borrowed(source);
    }
    std::borrow::Cow::Owned(sanitize_terminal_text(source))
}

pub(crate) fn sanitize_terminal_text(source: &str) -> String {
    sanitize_terminal_text_with_offsets(source, &[]).0
}

/// Sanitizes terminal text while translating raw UTF-8 byte boundaries into
/// offsets in the visible projection. Offsets need not be sorted and are
/// clamped to the source length. The parser is shared with
/// [`sanitize_terminal_text`], so control stripping and tab expansion cannot
/// diverge between the composer and the rest of the TUI.
pub(crate) fn sanitize_terminal_text_with_offsets(
    source: &str,
    offsets: &[usize],
) -> (String, Vec<usize>) {
    if is_safe_ascii(source) {
        return (
            source.to_owned(),
            offsets
                .iter()
                .map(|offset| (*offset).min(source.len()))
                .collect(),
        );
    }

    #[derive(Clone, Copy)]
    enum State {
        Ground,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }

    let mut state = State::Ground;
    let mut stripped = String::with_capacity(source.len());
    let mut has_tab = false;
    let mut stripped_offsets = vec![0; offsets.len()];
    let ordered_offsets = offset_order(offsets);
    let mut next_offset = 0usize;
    for (byte, character) in source.char_indices() {
        while next_offset < ordered_offsets.len()
            && offsets[ordered_offsets[next_offset]].min(source.len()) <= byte
        {
            stripped_offsets[ordered_offsets[next_offset]] = stripped.len();
            next_offset += 1;
        }
        state = match state {
            State::Ground => match character {
                '\u{1b}' => State::Escape,
                '\u{9b}' => State::Csi,
                '\u{9d}' => State::Osc,
                '\n' => {
                    stripped.push(character);
                    State::Ground
                }
                '\t' => {
                    stripped.push(character);
                    has_tab = true;
                    State::Ground
                }
                _ if character.is_control() => State::Ground,
                _ => {
                    stripped.push(character);
                    State::Ground
                }
            },
            State::Escape => match character {
                '[' => State::Csi,
                ']' => State::Osc,
                _ => State::Ground,
            },
            State::Csi => {
                if ('@'..='~').contains(&character) {
                    State::Ground
                } else {
                    State::Csi
                }
            }
            State::Osc => match character {
                '\u{7}' | '\u{9c}' => State::Ground,
                '\u{1b}' => State::OscEscape,
                _ => State::Osc,
            },
            State::OscEscape => {
                if character == '\\' {
                    State::Ground
                } else {
                    State::Osc
                }
            }
        };
    }

    while next_offset < ordered_offsets.len() {
        stripped_offsets[ordered_offsets[next_offset]] = stripped.len();
        next_offset += 1;
    }

    if !has_tab {
        return (stripped, stripped_offsets);
    }

    let mut safe = String::with_capacity(stripped.len());
    let mut column = 0usize;
    let mut safe_offsets = vec![0; stripped_offsets.len()];
    let ordered_offsets = offset_order(&stripped_offsets);
    let mut next_offset = 0usize;
    for (byte, grapheme) in stripped.grapheme_indices(true) {
        while next_offset < ordered_offsets.len()
            && stripped_offsets[ordered_offsets[next_offset]] <= byte
        {
            safe_offsets[ordered_offsets[next_offset]] = safe.len();
            next_offset += 1;
        }
        match grapheme {
            "\n" => {
                safe.push('\n');
                column = 0;
            }
            "\t" => {
                let spaces = 4 - column % 4;
                safe.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            _ => {
                safe.push_str(grapheme);
                column += grapheme.width();
            }
        }
    }

    while next_offset < ordered_offsets.len() {
        safe_offsets[ordered_offsets[next_offset]] = safe.len();
        next_offset += 1;
    }
    (safe, safe_offsets)
}

fn offset_order(offsets: &[usize]) -> Vec<usize> {
    let mut order = (0..offsets.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|index| offsets[*index]);
    order
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;
    use ratatui::text::Line;

    use super::{
        markdown_row_count, plain_row_count, render_markdown, render_plain, sanitize_terminal_text,
        sanitize_terminal_text_with_offsets, MarkdownStyles,
    };

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn styles() -> MarkdownStyles {
        MarkdownStyles {
            text: Style::default(),
            h1: Style::default(),
            h2: Style::default(),
            h3: Style::default(),
            link: Style::default(),
            code: Style::default(),
            code_block: Style::default(),
            code_rail: Style::default(),
            diff_add: Style::default(),
            diff_remove: Style::default(),
            diff_add_bg: Style::default(),
            diff_remove_bg: Style::default(),
            quote: Style::default(),
        }
    }

    #[test]
    fn osc_clipboard_payload_is_removed() {
        assert_eq!(
            sanitize_terminal_text("safe\u{1b}]52;c2VjcmV0\u{7}tail"),
            "safetail"
        );
    }

    #[test]
    fn csi_style_sequence_is_removed() {
        assert_eq!(sanitize_terminal_text("a\u{1b}[31mred\u{1b}[0mz"), "aredz");
    }

    #[test]
    fn greedy_wrap_and_measure_agree_at_wide_boundary() {
        let rendered = render_markdown("abc界abc", 4, styles());
        assert_eq!(markdown_row_count("abc界abc", 4), rendered.len());
    }

    #[test]
    fn prose_wraps_at_a_word_boundary_without_leading_space() {
        let rendered = render_markdown("alpha beta gamma", 10, styles());
        let lines = rendered.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(lines, ["alpha beta", "gamma"]);
        assert_eq!(markdown_row_count("alpha beta gamma", 10), lines.len());
    }

    #[test]
    fn long_unbreakable_token_still_hard_wraps() {
        let rendered = render_markdown("abcdefghijkl", 5, styles());
        let lines = rendered.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(lines, ["abcde", "fghij", "kl"]);
        assert_eq!(markdown_row_count("abcdefghijkl", 5), lines.len());
    }

    #[test]
    fn glyph_wider_than_viewport_degrades_without_overrun() {
        let rendered = render_markdown("界", 1, styles());
        assert!(rendered.iter().all(|line| line.width() <= 1));
    }

    #[test]
    fn marker_only_heading_does_not_restore_source_markers() {
        let rendered = render_markdown("###", 20, styles());
        assert!(rendered.iter().all(|line| !line_text(line).contains('#')));
    }

    #[test]
    fn loose_list_paragraphs_keep_a_line_boundary() {
        let rendered = render_markdown("- first\n\n  second", 40, styles());
        let text = rendered
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("firstsecond"));
        assert!(rendered.len() >= 2);
    }

    #[test]
    fn root_blocks_insert_a_blank_gap() {
        let rendered = render_markdown("# Title\n\nBody paragraph.", 40, styles());
        let lines = rendered.iter().map(line_text).collect::<Vec<_>>();
        let title = lines.iter().position(|line| line.contains("Title"));
        let body = lines.iter().position(|line| line.contains("Body"));
        assert_eq!(title, Some(0));
        assert_eq!(body, Some(2));
        assert!(lines[1].trim().is_empty());
        assert_eq!(markdown_row_count("# Title\n\nBody paragraph.", 40), 3);
    }

    #[test]
    fn gfm_table_aligns_columns_without_pipe_markers() {
        let markdown = "| Pasta | Tamanho |\n| --- | --- |\n| target | 55 GB |\n| .git | 80 MB |";
        let rendered = render_markdown(markdown, 40, styles());
        let lines = rendered.iter().map(line_text).collect::<Vec<_>>();
        let joined = lines.join("\n");
        assert!(!joined.contains('|'), "{joined}");
        let header = lines
            .iter()
            .find(|line| line.contains("Pasta"))
            .expect("header");
        let row = lines
            .iter()
            .find(|line| line.contains("target"))
            .expect("body");
        assert_eq!(header.find("Pasta"), row.find("target"));
        assert_eq!(header.find("Tamanho"), row.find("55 GB"));
        assert_eq!(markdown_row_count(markdown, 40), rendered.len());
    }

    #[test]
    fn narrow_table_fallback_keeps_cell_tails_and_row_count() {
        let markdown = "| Field | Value |\n| --- | --- |\n| alpha | first value with tail-alpha |\n| beta | second value with tail-beta |";
        let rendered = render_markdown(markdown, 18, styles());
        let lines = rendered.iter().map(line_text).collect::<Vec<_>>();
        let joined = lines.join("\n");
        assert!(joined.contains("Field: alpha"), "{joined}");
        assert!(joined.contains("Field: beta"), "{joined}");
        assert!(joined.contains("tail-alpha"), "{joined}");
        assert!(joined.contains("tail-beta"), "{joined}");
        assert!(rendered.iter().all(|line| line.width() <= 18));
        assert_eq!(markdown_row_count(markdown, 18), rendered.len());
    }

    #[test]
    fn tabs_expand_before_materialization() {
        let rendered = render_markdown("`a\tb`", 20, styles());
        assert!(rendered.iter().all(|line| !line_text(line).contains('\t')));
    }

    #[test]
    fn plain_wrap_and_measure_share_physical_rows() {
        let source = "ab界c👨‍👩‍👧‍👦def\nsecond";
        for width in 1..=8 {
            assert_eq!(
                render_plain(source, width).len(),
                plain_row_count(source, width)
            );
        }
    }

    #[test]
    fn tab_stop_uses_displayed_grapheme_width() {
        let family = "👨‍👩‍👧‍👦";
        assert_eq!(unicode_width::UnicodeWidthStr::width(family), 2);
        assert_eq!(
            sanitize_terminal_text(&format!("{family}\tX")),
            format!("{family}  X")
        );
    }

    #[test]
    fn c1_string_terminator_ends_osc() {
        assert_eq!(
            sanitize_terminal_text("safe\u{9d}52;secret\u{9c}tail"),
            "safetail"
        );
    }

    #[test]
    fn sanitized_offsets_follow_tabs_and_removed_controls() {
        let source = "a\tb\u{1b}[31mcd";
        let offsets = [source.len(), 0, 2];
        let (safe, mapped) = sanitize_terminal_text_with_offsets(source, &offsets);
        assert_eq!(safe, "a   bcd");
        assert_eq!(mapped[0], safe.len());
        assert_eq!(mapped[1], 0);
        assert_eq!(&safe[..mapped[2]], "a   ");
    }
}
