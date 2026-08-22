use std::io;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use crossterm::event::{poll, read, Event, MouseEvent, MouseEventKind};
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;

use slim_core::runtime::mode_name;

use crate::api::{LoginProvider, ModelAlias, ReasoningEffort, UiChannels, UiCommand};
use crate::app::{AppState, EffortOverlay, LoginOverlay, ModelOverlay, SlashSuggestions};
use crate::block::{Block, BlockKind, BlockLifecycle, FoldState};
use crate::fullscreen::FullscreenBackend;
use crate::layout::{plan, Rect};
use crate::reducer::{reduce, Action, Effect, ScrollIntent};
use crate::render::{EventCoalescer, HeightIndex, WrapCache};
use crate::theme::{
    detect_capabilities, resolve_theme, to_terminal_color, Capabilities, ColorDepth,
};

pub fn run_app(channels: UiChannels) -> io::Result<()> {
    let capabilities = detect_capabilities();
    let mut backend = FullscreenBackend::start(capabilities)?;
    let result = run_loop(&mut backend, &channels);
    let _ = channels.commands.send(UiCommand::Shutdown);
    let shutdown = backend.shutdown();
    result.and(shutdown)
}

/// Normative flow (§4.1/§9.2): terminal and bridge events become Actions, the
/// reducer is the only mutation route, and the runtime only executes Effects.
fn run_loop(backend: &mut FullscreenBackend, channels: &UiChannels) -> io::Result<()> {
    let mut state = AppState::new();
    let capabilities = detect_capabilities();
    let mut dirty = true;
    // Motion clock (§10.3): the loop ticks only while something animates —
    // 8-12 fps for the working spinner, ≤ 2 fps for the welcome's single
    // ambient glyph; nothing under reduced motion.
    let working_tick = Duration::from_millis(if capabilities.reduced_motion { u64::MAX } else { 120 });
    let idle_tick = Duration::from_millis(if capabilities.reduced_motion { u64::MAX } else { 500 });
    let mut last_tick = Instant::now();
    let mut spinner_frame: u64 = 0;
    let mut render_cache = WrapCache::default();
    loop {
        // Control lane drains first, capped at 32 events before one look at
        // data (deterministic fairness, spec §10.1).
        let mut control_budget = 32;
        while control_budget > 0 {
            match channels.events.try_recv() {
                Ok(event) => {
                    let effects = reduce(&mut state, Action::UiEventReceived(event));
                    run_effects(channels, effects)?;
                    dirty = true;
                    control_budget -= 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        // Data lane passes through the coalescer (§10.2): consecutive deltas
        // concatenate, everything else keeps order.
        let mut coalescer = EventCoalescer::new(1024, Duration::from_millis(16));
        loop {
            match channels.events_data.try_recv() {
                Ok(event) => coalescer.push_data(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        for event in coalescer.flush() {
            let effects = reduce(&mut state, Action::UiEventReceived(event));
            run_effects(channels, effects)?;
            dirty = true;
        }
        if state.shutdown {
            return Ok(());
        }
        if dirty {
            draw_state(backend, &state, capabilities, &mut render_cache)?;
            dirty = false;
        }
        if !poll(Duration::from_millis(16))? {
            let interval = if state.working { working_tick } else { idle_tick };
            let animating = state.working || welcome_visible(&state);
            if animating && last_tick.elapsed() >= interval {
                spinner_frame += 1;
                let effects = reduce(&mut state, Action::Tick(spinner_frame));
                run_effects(channels, effects)?;
                dirty = true;
                last_tick = Instant::now();
            }
            continue;
        }
        let event = read()?;
        if let Some(action) = terminal_action(event) {
            let effects = reduce(&mut state, action);
            run_effects(channels, effects)?;
            dirty = true;
        }
    }
}

fn run_effects(
    channels: &UiChannels,
    effects: Vec<Effect>,
) -> io::Result<()> {
    for effect in effects {
        match effect {
            Effect::Send(command) => send(&channels.commands, command)?,
            Effect::RequestRender => {}
        }
    }
    Ok(())
}

fn terminal_action(event: Event) -> Option<Action> {
    match event {
        Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
            Some(Action::Key(key))
        }
        Event::Paste(payload) => Some(Action::Paste(payload)),
        Event::Resize(_, _) => Some(Action::Resize),
        Event::Mouse(mouse) => mouse_action(mouse),
        _ => None,
    }
}

fn mouse_action(mouse: MouseEvent) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::ScrollUp => Some(Action::Scroll(ScrollIntent::Up)),
        MouseEventKind::ScrollDown => Some(Action::Scroll(ScrollIntent::Down)),
        _ => None,
    }
}

fn send(commands: &std::sync::mpsc::Sender<UiCommand>, command: UiCommand) -> io::Result<()> {
    commands
        .send(command)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI runtime disconnected"))
}

/// Resolved styles for one frame, derived from the semantic theme (§21.1):
/// components never reference literal colors.
struct Palette {
    background: Style,
    surface: Style,
    surface_alt: Style,
    composer_bg: Style,
    user_prompt_bg: Style,    text: Style,
    muted: Style,
    secondary: Style,
    accent: Style,
    accent_bold: Style,
    user_rail: Style,
    assistant_rail: Style,
    thinking: Style,
    heading: Style,
    code_rail: Style,
    tool: Style,
    warning: Style,
    error: Style,
    success: Style,
    border: Style,
}

impl Palette {
    fn of(capabilities: Capabilities) -> Self {
        let theme = resolve_theme(capabilities);
        let color = |rgb: (u8, u8, u8)| to_terminal_color(capabilities.color_depth, rgb);
        let base = Style::default();
        Palette {
            background: base.bg(color(theme.background)),
            surface: base.bg(color(theme.surface)),
            surface_alt: base.bg(color(theme.surface_alt)),
            composer_bg: base.bg(color(theme.composer_bg)),
            user_prompt_bg: base.bg(color(theme.user_prompt_bg)),
            text: base.fg(color(theme.foreground)),
            muted: base.fg(color(theme.muted)),
            secondary: base.fg(color(theme.secondary_text)),
            accent: base.fg(color(theme.accent)),
            accent_bold: base.fg(color(theme.accent)).add_modifier(Modifier::BOLD),
            user_rail: base.fg(color(theme.user_accent)),
            assistant_rail: base.fg(color(theme.assistant_accent)),
            thinking: base.fg(color(theme.thinking_accent)),
            heading: base.fg(color(theme.heading_accent)).add_modifier(Modifier::BOLD),
            code_rail: base.fg(color(theme.code_rail)),
            tool: base.fg(color(theme.tool_accent)),
            warning: base.fg(color(theme.warning)),
            error: base.fg(color(theme.error)),
            success: base.fg(color(theme.success)),
            border: base.fg(color(theme.border)),
        }
    }
}

fn draw_state(
    backend: &mut FullscreenBackend,
    state: &AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) -> io::Result<()> {
    backend
        .terminal()
        .draw(|frame| render_frame(frame, state, capabilities, cache))
        .map(|_| ())
}

/// Pure frame composition shared by the fullscreen backend and buffer-based
/// golden tests.
pub fn render_frame(
    frame: &mut ratatui::Frame,
    state: &AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) {
    let palette = Palette::of(capabilities);
    {
            let area = frame.area();
            // Stratified surfaces (§1.2/§21.3): background fills everything,
            // each region paints its own level on top.
            frame.render_widget(
                ratatui::widgets::Block::default().style(palette.background),
                area,
            );
            let todo_rows = crate::layout::todo_height(
                state.todo_dock_open,
                state.todo_items.len(),
            );
            let regions = plan(area.width, area.height, todo_rows, state.working);

            if regions.activity_rail.height > 0 {
                render_activity_rail(frame, to_ratatui(regions.activity_rail), state, &palette);
            }
            render_scrollback(
                frame,
                to_ratatui(regions.scrollback),
                state,
                &palette,
                cache,
                capabilities,
            );

            if regions.todo.height > 0 {
                render_todo_dock(frame, to_ratatui(regions.todo), state, &palette);
            }
            render_divider(frame, to_ratatui(regions.todo_divider), palette.border);
            render_composer(frame, to_ratatui(regions.composer), state, &palette);
            if let Some(suggestions) = &state.slash_suggestions {
                render_slash_popup(frame, to_ratatui(regions.composer), suggestions, &palette);
            }
            // W5: the op-bar divider rule is gone — the composer label row and
            // the surface stratification separate the two regions quietly.
            render_operational_bar(frame, to_ratatui(regions.operational), state, &palette);

            if let Some(overlay) = &state.model_overlay {
                render_model_overlay(frame, overlay, &palette);
            }
            if let Some(overlay) = &state.effort_overlay {
                render_effort_overlay(frame, overlay, &palette);
            }
            if let Some(overlay) = &state.login_overlay {
                render_login_overlay(frame, overlay, &palette);
            }
            if let Some(query) = &state.palette_query {
                render_palette(frame, query, &palette);
            }
    }
}

/// Empty-state predicate shared by the renderer and the motion clock: the
/// welcome screen is on exactly when there is nothing else to show.
fn welcome_visible(state: &AppState) -> bool {
    state.blocks.is_empty() && state.notifications.is_empty() && !state.working
}

/// Scrollback renders edge-to-edge, virtualized through the HeightIndex
/// (§13.2): only blocks intersecting the viewport are materialized; live edge
/// pins to the newest rows, pinned keeps `offset_from_end` rows above bottom.
fn render_scrollback(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    cache: &mut WrapCache,
    capabilities: Capabilities,
) {
    if welcome_visible(state) {
        frame.render_widget(
            ratatui::widgets::Block::default().style(palette.surface),
            area,
        );
        render_welcome(frame, area, state, palette, capabilities);
        return;
    }
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.surface),
        area,
    );
    let index = HeightIndex::build(&state.blocks, area.width, cache);
    // Toasts reserve their rows instead of overwriting transcript (§15.8).
    let notice_count = state.notifications.len().min(3).min(area.height as usize) as u16;
    let viewport = area.height.saturating_sub(notice_count).max(1) as u64;
    let bottom = index.total_rows.saturating_sub(viewport);
    let start_row = if state.scroll.pinned {
        bottom.saturating_sub(state.scroll.offset_from_end as u64)
    } else {
        bottom
    };
    let (mut idx, mut skip_rows) = index.locate(start_row);
    let capacity = viewport as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(capacity);
    let mut rows = 0usize;
    while idx < index.entries.len() && rows < capacity {
        let (_, block, group) = index.entries[idx];
        idx += 1;
        if group > 1 {
            // Aggregated completed tools (presentation-only merge, §11.4.1).
            let BlockKind::Tool(first) = &block.kind else { continue };
            lines.push(Line::from(Span::styled(
                format!("  ✓ {} ×{group}", first.name),
                palette.success,
            )));
            rows += 1;
            skip_rows = 0;
            continue;
        }
        let mut block_lines = safe_block_lines(block, palette, area.width);
        if skip_rows > 0 {
            let skip = (skip_rows as usize).min(block_lines.len());
            block_lines.drain(..skip);
            skip_rows = 0;
        }
        let take = capacity - rows;
        if block_lines.len() > take {
            block_lines.truncate(take);
        }
        rows += block_lines.len();
        lines.extend(block_lines);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(palette.text)
            .wrap(Wrap { trim: false }),
        area,
    );
    if notice_count > 0 {
        let notices: Vec<String> = state
            .notifications
            .iter()
            .rev()
            .take(notice_count as usize)
            .rev()
            .cloned()
            .collect();
        let toast_area = ratatui::layout::Rect {
            x: area.x,
            y: area.y + area.height - notice_count,
            width: area.width,
            height: notice_count,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                notices.join("· "),
                palette.muted,
            )))
            .style(palette.surface),
            toast_area,
        );
    }
}

/// Todo dock projection (§14.3/§15.5): header row with progress + active item,
/// then item rows (compact shows the rest, expanded up to six rows). The dock
/// is a projection of `todo_items`, never a parallel authority.
fn render_todo_dock(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
) {
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.surface_alt),
        area,
    );
    use crate::api::TodoItemStatus;
    let total = state.todo_items.len();
    let done = state
        .todo_items
        .iter()
        .filter(|item| item.status == TodoItemStatus::Completed)
        .count();
    let active = state
        .todo_items
        .iter()
        .find(|item| item.status == TodoItemStatus::InProgress);
    let mut rows = Vec::new();
    let mut header = vec![
        Span::styled("TODO", palette.secondary),
        Span::styled(format!(" {done}/{total}"), palette.muted),
    ];
    if let Some(active) = active {
        header.push(Span::styled("  ◌ ", palette.warning));
        header.push(Span::styled(active.title.clone(), palette.text));
    }
    rows.push(Line::from(header));
    if area.height > 1 {
        let rest: Vec<Line> = state
            .todo_items
            .iter()
            .filter(|item| item.status != TodoItemStatus::InProgress)
            .take(area.height as usize - 1)
            .map(|item| {
                let style = match item.status {
                    TodoItemStatus::Completed => palette.success,
                    TodoItemStatus::InProgress => palette.warning,
                    TodoItemStatus::Pending => palette.muted,
                    TodoItemStatus::Blocked | TodoItemStatus::Cancelled => palette.error,
                };
                Line::from(vec![
                    Span::styled(format!("{} ", item.status.glyph()), style),
                    Span::styled(item.title.clone(), palette.muted),
                ])
            })
            .collect();
        rows.extend(rest);
    }
    frame.render_widget(Paragraph::new(rows).style(palette.surface_alt), area);
}

/// Activity rail (§15.4): transient working signal with cancel hint; spinner
/// glyph advances with the motion clock and freezes under reduced motion.
fn render_activity_rail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
) {
    const SPINNER: [char; 4] = ['\u{25d2}', '\u{25d3}', '\u{25d1}', '\u{25d0}'];
    let glyph = if capabilities_reduced_motion() {
        '\u{25cc}'
    } else {
        SPINNER[(state.spinner_frame % 4) as usize]
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), palette.warning),
        Span::styled("working", palette.text),
    ];
    spans.push(Span::raw("    "));
    spans.push(Span::styled("Ctrl+C stop", palette.muted));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(palette.surface_alt),
        area,
    );
}

fn capabilities_reduced_motion() -> bool {
    std::env::var_os("SLIM_REDUCED_MOTION").is_some_and(|v| v == "1")
}

fn render_welcome(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    capabilities: Capabilities,
) {
    let (connection, hint) = welcome_copy(state);
    let connection_style = if state.authenticated {
        palette.accent
    } else {
        palette.secondary
    };

    // Compact fallback: small viewports keep the plain centered composition.
    if area.width < 36 || area.height < 14 {
        let lines = vec![
            Line::from(Span::styled("SLIM", palette.accent_bold)),
            Line::default(),
            Line::from(Span::styled(
                "Native coding agent for this workspace",
                palette.text,
            )),
            Line::default(),
            Line::from(Span::styled(connection, connection_style)),
            Line::from(Span::styled(hint, palette.muted)),
        ];
        let centered_area = centered(area, 58, lines.len() as u16);
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            centered_area,
        );
        return;
    }

    // Full identity (§1.2): braille dot-matrix wordmark in the accent color,
    // one pulsing ambient glyph beside the tagline (spec §2: single glyph,
    // ≤ 2 fps, text and alignment stable), status, command hints, version.
    let ascii_pulse = capabilities.color_depth == ColorDepth::None;
    let glyph = if capabilities.reduced_motion {
        '\u{25cc}' // dotted circle: frozen, same width as the animated glyphs
    } else {
        crate::welcome::pulse(state.spinner_frame, ascii_pulse)
    };
    let mut lines = Vec::with_capacity(12);
    for row in crate::welcome::wordmark() {
        lines.push(Line::from(Span::styled(row, palette.accent_bold)));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(format!("{glyph} "), palette.accent),
        Span::styled("Native coding agent for this workspace", palette.text),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(connection, connection_style)));
    lines.push(Line::from(Span::styled(hint, palette.muted)));

    let width = lines
        .iter()
        .map(|line| line.width())
        .max()
        .unwrap_or(1)
        .min(area.width.max(1) as usize) as u16;
    let height = lines.len() as u16;
    let centered_area = centered(area, width.max(20), height.min(area.height.max(1)));
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        centered_area,
    );
}

fn welcome_copy(state: &AppState) -> (String, &'static str) {
    if state.authenticated {
        (
            format!(
                "Connected · {}",
                state.auth_provider.map_or("provider", LoginProvider::label)
            ),
            "/models to choose GPT-5.6 and reasoning effort",
        )
    } else {
        (
            "No provider connected".into(),
            "Type /login to connect · /models to choose GPT-5.6",
        )
    }
}

fn block_lines(block: &Block, palette: &Palette, width: u16) -> Vec<Line<'static>> {
    match &block.kind {
        BlockKind::User(text) => {
            let mut lines = vec![Line::from(Span::styled("  you", palette.secondary))];
            lines.extend(indented_body(
                text,
                palette.user_rail,
                palette.text,
                palette.user_prompt_bg,
                width,
            ));
            lines
        }
        BlockKind::Assistant(text) => {
            let mut lines = vec![Line::from(Span::styled("  Slim", palette.accent_bold))];
            lines.extend(assistant_body(text, palette, width));
            lines
        }
        BlockKind::Thinking(text) => match block.fold {
            FoldState::Collapsed => {
                let preview = text.lines().next().unwrap_or_default();
                vec![Line::from(Span::styled(
                    format!("  ◌ thinking · {preview}"),
                    palette.thinking,
                ))]
            }
            _ => {
                let mut lines = vec![Line::from(Span::styled("  ◌ thinking", palette.thinking))];
                lines.extend(indented_body(
                    text,
                    palette.thinking,
                    palette.thinking,
                    palette.surface,
                    width,
                ));
                lines
            }
        },
        BlockKind::Tool(state) => match block.lifecycle {
            BlockLifecycle::Streaming => {
                let preview = state.preview.lines().next().unwrap_or_default();
                vec![Line::from(vec![
                    Span::styled("  ◌ ", palette.warning),
                    Span::styled(state.name.clone(), palette.tool),
                    Span::styled(
                        if preview.is_empty() {
                            String::new()
                        } else {
                            format!(" · {preview}")
                        },
                        palette.muted,
                    ),
                ])]
            }
            _ => vec![Line::from(vec![
                Span::styled("  ✕ ", palette.error),
                Span::styled(state.name.clone(), palette.error),
                Span::styled(
                    if state.preview.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", state.preview.lines().next().unwrap_or_default())
                    },
                    palette.muted,
                ),
            ])],
        },
        BlockKind::System(text) => vec![Line::from(Span::styled(
            format!("  system · {text}"),
            palette.muted,
        ))],
        BlockKind::Error(text) => vec![Line::from(Span::styled(
            format!("  ✕ {text}"),
            palette.error,
        ))],
        BlockKind::Activity(text) => vec![Line::from(Span::styled(
            format!("  · {text}"),
            palette.muted,
        ))],
        BlockKind::QueuedUser(text) => vec![Line::from(Span::styled(
            format!("  … {text}"),
            palette.secondary,
        ))],
    }
}

/// Markdown-light assistant body (gate C7): headings in `heading_accent`,
/// fenced code in `code_rail`; prose stays plain per spec §19.5 (no
/// whole-paragraph coloring).
fn assistant_body(text: &str, palette: &Palette, width: u16) -> Vec<Line<'static>> {
    let mut in_code = false;
    let mut out = Vec::new();
    for (index, line) in text.split("
").enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push(Line::from(vec![
                Span::styled(if index == 0 { "│ " } else { "  " }, palette.code_rail),
                Span::styled(line.to_owned(), palette.code_rail),
            ]));
            continue;
        }
        let style = if in_code {
            palette.code_rail
        } else if trimmed.starts_with('#') {
            palette.heading
        } else {
            palette.text
        };
        let rail_style = if index == 0 { palette.assistant_rail } else { palette.surface };
        let prefix = if index == 0 { "│ " } else { "  " };
        let pad = (width as usize).saturating_sub(2 + line.chars().count());
        out.push(Line::from(vec![
            Span::styled(prefix, rail_style),
            Span::styled(line.to_owned(), style),
            Span::styled(" ".repeat(pad), palette.surface),
        ]));
    }
    out
}

/// Message body starts after the rail; wrapped/source continuation lines align
/// with the text column, never with the rail (§15.2.1). `bg` fills the full
/// useful width so user prompts show `user_prompt_bg` as an elevated band.
fn indented_body(
    text: &str,
    rail: Style,
    body: Style,
    bg: Style,
    width: u16,
) -> Vec<Line<'static>> {
    let width = width as usize;
    text.split('\n')
        .enumerate()
        .map(|(index, line)| {
            let content_width = 2 + line.chars().count();
            let pad = width.saturating_sub(content_width);
            if index == 0 {
                Line::from(vec![
                    Span::styled("│ ", rail),
                    Span::styled(line.to_owned(), body.patch(bg)),
                    Span::styled(" ".repeat(pad), bg),
                ])
            } else {
                Line::from(vec![
                    Span::styled("  ", bg),
                    Span::styled(line.to_owned(), body.patch(bg)),
                    Span::styled(" ".repeat(pad), bg),
                ])
            }
        })
        .collect()
}

/// Render fault isolation per block (§24.1): a panicking block renderer
/// degrades to a bounded fallback row; the rest of the frame survives.
fn safe_block_lines(
    block: &Block,
    palette: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_lines(block, palette, width)
    }))
    .unwrap_or_else(|_| {
        vec![Line::from(Span::styled(
            "  ⚠ bloco indisponível",
            palette.muted,
        ))]
    })
}

fn render_divider(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, style: Style) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            style,
        ))),
        area,
    );
}

/// Composer (§15.3, refined 2026-08-21 W6, Grok-style): full rounded box with
/// a permanently neutral border, model/effort embedded in the bottom border's
/// right end; focus lives in the `›` glyph only. One compact row when the
/// viewport is short.
fn render_composer(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
) {
    if area.height == 0 {
        return;
    }
    let payload = state.composer.payload();
    let total_lines = payload.lines().count().max(1);
    let last_line = payload.lines().last().unwrap_or_default().to_owned();
    let focused = state.login_overlay.is_none()
        && state.model_overlay.is_none()
        && state.effort_overlay.is_none();
    let glyph_style = if focused {
        palette.accent
    } else {
        palette.muted
    };
    let content_area = if area.height >= 3 {
        let model = ModelAlias::parse(&state.model)
            .map_or_else(|| state.model.clone(), |alias| alias.label().into());
        let label = format!("{model} · {} ", state.effort.id());
        let block = RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(palette.border)
            .title_bottom(Line::from(Span::styled(label, palette.muted)))
            .title_alignment(Alignment::Right);
        let inner = block.inner(area);
        frame.render_widget(block.style(palette.composer_bg), area);
        inner
    } else {
        frame.render_widget(
            RatatuiBlock::default().style(palette.composer_bg),
            area,
        );
        area
    };
    let mut spans = vec![
        Span::styled("› ", glyph_style),
        Span::styled(last_line.clone(), palette.text.patch(palette.composer_bg)),
    ];
    if total_lines > 1 {
        spans.push(Span::styled(
            format!("  {total_lines}/{total_lines}"),
            palette.muted,
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(palette.composer_bg),
        content_area,
    );
    if focused {
        let offset = last_line.graphemes(true).count() as u16 + 2;
        frame.set_cursor_position((
            content_area.x + offset.min(content_area.width.saturating_sub(1)),
            content_area.y,
        ));
    }
}

/// Operational bar stays on surface_alt; left group carries identity, mode and
/// state; right group carries the context meter and token counters (§15.1,
/// revised 2026-08-21: model/effort live only on the composer label, the
/// context meter moved here from the old top rail). Pinned scroll shows the
/// unseen counter with the End hint (§13.3).
fn render_operational_bar(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
) {
    let mut left = vec![
        Span::styled("SLIM", palette.accent),
        Span::styled(format!("  {}", mode_name(state.mode)), palette.text),
    ];
    if state.working {
        left.push(Span::styled(" · ", palette.muted));
        left.push(Span::styled("◌ working", palette.warning));
    } else if !state.authenticated {
        left.push(Span::styled(" · signed out · /login", palette.muted));
    }
    if state.scroll.pinned {
        let unseen = state.scroll.unseen;
        let hint = if unseen > 0 {
            format!(" · {unseen} new · End latest")
        } else {
            " · End latest".into()
        };
        left.push(Span::styled(hint, palette.secondary));
    }
    // Context meter + usage (moved from the removed top context rail).
    const CONTEXT_TOKENS: u64 = 128_000;
    let used = state.input_tokens.saturating_add(state.output_tokens);
    let pct = used.saturating_mul(100) / CONTEXT_TOKENS;
    let right = vec![
        Span::styled(
            format!("ctx {pct}% \u{b7} {}k/128k", (used + 500) / 1_000),
            palette.muted,
        ),
        Span::styled(" · ", palette.muted),
        Span::styled(
            format!("\u{2191}{} \u{2193}{}", state.input_tokens, state.output_tokens),
            palette.muted,
        ),
    ];
    let span_width = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();
    let pad = (area.width as usize)
        .saturating_sub(span_width(&left) + span_width(&right))
        .max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(palette.surface_alt),
        area,
    );
}

fn render_model_overlay(frame: &mut ratatui::Frame, overlay: &ModelOverlay, palette: &Palette) {
    let area = centered(frame.area(), 48, 11);
    let rows = ModelAlias::ALL
        .iter()
        .enumerate()
        .map(|(index, alias)| {
            let marker = if index == overlay.selected {
                "›"
            } else {
                " "
            };
            format!("{marker} {}  ({})", alias.label(), alias.id())
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(format!("\n{rows}\n\nEnter select · Esc cancel"))
            .block(modal_block(" Select model ", palette)),
        area,
    );
}

fn render_effort_overlay(frame: &mut ratatui::Frame, overlay: &EffortOverlay, palette: &Palette) {
    let levels = ReasoningEffort::supported(overlay.model);
    let area = centered(frame.area(), 72, (levels.len() * 2 + 4) as u16);
    let rows = levels
        .iter()
        .enumerate()
        .map(|(index, effort)| {
            let marker = if index == overlay.selected {
                "›"
            } else {
                " "
            };
            format!("{marker} {:<6}  {}", effort.label(), effort.description())
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(format!("\n{rows}\n\nEnter select · Esc back"))
            .block(modal_block(" Select reasoning effort ", palette)),
        area,
    );
}

fn render_login_overlay(frame: &mut ratatui::Frame, overlay: &LoginOverlay, palette: &Palette) {
    let area = centered(frame.area(), 58, 11);
    let first = if overlay.selected == 0 { "›" } else { " " };
    let second = if overlay.selected == 1 { "›" } else { " " };
    let mut text =
        format!("\n{first} Anthropic — Claude Pro/Max\n\n{second} OpenAI Codex — ChatGPT Plus/Pro");
    if let Some(code) = &overlay.user_code {
        text.push_str(&format!("\n\nCode: {}", code.expose()));
    } else if let Some(progress) = &overlay.progress {
        text.push_str(&format!("\n\n{progress}"));
    } else if let Some(url) = &overlay.auth_url {
        text.push_str(&format!("\n\nOpen: {}", url.expose()));
    } else {
        text.push_str("\n\nEnter connect · Esc cancel");
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).block(modal_block(" Connect provider ", palette)),
        area,
    );
}

/// Command palette (gate C7): centered modal filtering the slash commands;
/// Slash autocomplete (W7): rounded popup hugging above the composer, listing
/// commands matching the token under edit; `›` marks the selection.
fn render_slash_popup(
    frame: &mut ratatui::Frame,
    composer_area: ratatui::layout::Rect,
    suggestions: &SlashSuggestions,
    palette: &Palette,
) {
    let matches = crate::reducer::slash_matches(&suggestions.query);
    if matches.is_empty() || composer_area.y < 3 {
        return;
    }
    let width = 32.min(frame.area().width);
    let height = (matches.len() as u16 + 2).min(composer_area.y);
    let area = ratatui::layout::Rect {
        x: 0,
        y: composer_area.y - height,
        width,
        height,
    };
    let rows: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(index, command)| {
            if index == suggestions.selected {
                Line::from(Span::styled(format!("› {command}"), palette.accent))
            } else {
                Line::from(Span::styled(format!("  {command}"), palette.muted))
            }
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(rows).block(
            RatatuiBlock::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(palette.border)
                .style(palette.surface_alt),
        ),
        area,
    );
}

/// Enter submits the top match, Esc dismisses.
fn render_palette(frame: &mut ratatui::Frame, query: &str, palette: &Palette) {
    const COMMANDS: [&str; 4] = ["/login", "/logout", "/model", "/mode"];
    let matches: Vec<&str> = COMMANDS
        .iter()
        .copied()
        .filter(|command| command.starts_with(query))
        .collect();
    let area = centered(frame.area(), 48, 9);
    let body = if matches.is_empty() {
        "nenhum comando".to_string()
    } else {
        matches.join("
")
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(format!("
{body}

Enter executar · Esc cancelar"))
            .block(modal_block(&format!(" Comando: {query} "), palette)),
        area,
    );
}

/// Overlays are the only surfaces allowed a box (§21.4); they use the neutral
/// border token and the accent for the title.
fn modal_block<'a>(title: &'a str, palette: &'a Palette) -> RatatuiBlock<'a> {
    RatatuiBlock::default()
        .title(title)
        .title_style(palette.accent_bold)
        .borders(Borders::ALL)
        .border_style(palette.border)
}

fn centered(
    area: ratatui::layout::Rect,
    requested_width: u16,
    requested_height: u16,
) -> ratatui::layout::Rect {
    let width = requested_width.min(area.width);
    let height = requested_height.min(area.height);
    ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn to_ratatui(rect: Rect) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

#[cfg(test)]
mod tests {
    use super::welcome_copy;
    use crate::api::LoginProvider;
    use crate::app::AppState;
    use crate::render::WrapCache;
    use crate::runtime::render_frame;
    use crate::theme::{Capabilities, ColorDepth};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn caps() -> Capabilities {
        Capabilities {
            color_depth: ColorDepth::TrueColor,
            mouse: false,
            clipboard: false,
            images: false,
            reduced_motion: false,
        }
    }

    fn render_welcome_to_string(state: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn welcome_reflects_active_oauth_provider() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.auth_provider = Some(LoginProvider::OpenAiCodex);

        let (connection, hint) = welcome_copy(&state);

        assert!(connection.contains("OpenAI Codex"));
        assert!(!connection.contains("No provider connected"));
        assert!(hint.contains("reasoning effort"));
    }

    #[test]
    fn full_welcome_shows_braille_wordmark_status_and_hints() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.auth_provider = Some(LoginProvider::OpenAiCodex);
        let frame = render_welcome_to_string(&state, 80, 24);
        assert!(
            frame.contains("Connected · OpenAI Codex"),
            "status line present"
        );
        // Dot-matrix wordmark: at least one braille glyph on screen.
        assert!(
            frame.chars().any(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch)),
            "braille wordmark rendered"
        );
        // The single ambient glyph sits beside the tagline.
        assert!(frame.contains("Native coding agent for this workspace"));
        // Trimmed 2026-08-21: no hint/version rows beyond the status pair.
        assert!(!frame.contains("Ctrl+P commands"), "no shortcut hint row");
        assert!(!frame.contains("v0.1.0"), "no version row");
    }

    #[test]
    fn compact_welcome_falls_back_to_plain_wordmark_on_small_viewports() {
        let state = AppState::new();
        let frame = render_welcome_to_string(&state, 40, 10);
        assert!(frame.contains("SLIM"), "plain wordmark");
        assert!(
            !frame.chars().any(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch)),
            "no braille art on compact fallback"
        );
    }
}
