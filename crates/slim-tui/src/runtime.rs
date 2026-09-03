use std::io;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use crossterm::event::{read, Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use slim_core::runtime::mode_name;

use crate::api::{
    BlockId, LoginProvider, ModelAlias, ReasoningEffort, TodoItemStatus, UiChannels, UiCommand,
};
use crate::app::{
    AppState, EffortOverlay, LoginOverlay, LoginStage, ModelOverlay, ModelRow, SlashSuggestions,
    INFO_TOAST_TTL_MS,
};
use crate::block::{Block, BlockKind, BlockLifecycle, FoldState};
use crate::fullscreen::FullscreenBackend;
use crate::inspector::{is_mutating_tool, search_match_indices_filtered, InspectorKind};
use crate::layout::{plan_with_session_rail_and_composer, todo_height, Rect};
use crate::markdown::{render_markdown, render_plain, sanitize_terminal_text, MarkdownStyles};
use crate::picker::{truncate_cells, visible_window, PICKER_NOMINAL_CAPACITY};
use crate::reducer::{reduce, slash_matches_with_skills, Action, Effect, ScrollIntent};
use crate::render::{
    cached_lines_bytes, thinking_preview_tail, BodyKind, EventCoalescer, HeightIndex,
    ScrollMetrics, WrapCache,
};
use crate::runtime_wait::{
    next_visual_deadline, runtime_clock, wait_for_runtime_signal, WaitOutcome,
};
use crate::theme::{
    detect_capabilities, glyph, resolve_theme, to_terminal_color, Capabilities, ColorDepth,
};
use crate::view_model::{
    activity_elapsed, activity_label, display_cwd, format_context, is_trivial_cwd,
    run_status_label, session_rail_projection, truncate_display_width,
};

/// Composer prompt is ASCII on purpose (G264): `›` (U+203A) is East-Asian
/// Ambiguous and Windows consoles advance two cells while Ratatui counts one,
/// which parks the caret on the last typed letter.
const COMPOSER_PROMPT: &str = "> ";
const COMPOSER_OVERFLOW_HINT: &str = "<";
const CONTROL_BATCH_LIMIT: usize = 32;
const STREAM_BATCH_LIMIT: usize = 1_024;
const MAX_WORKSPACE_WIDTH: u16 = 144;
const DOCKED_INSPECTOR_MIN_WIDTH: u16 = 100;
const DEFAULT_INSPECTOR_MIN_WIDTH: u16 = 140;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaneDrain {
    Open,
    Exhausted,
    Closed,
}

fn receive_batch(
    receiver: &Receiver<crate::api::UiEvent>,
    limit: usize,
) -> (Vec<crate::api::UiEvent>, LaneDrain) {
    let mut events = Vec::with_capacity(limit);
    for _ in 0..limit {
        match receiver.try_recv() {
            Ok(event) => events.push(event),
            Err(TryRecvError::Empty) => return (events, LaneDrain::Open),
            Err(TryRecvError::Disconnected) => return (events, LaneDrain::Closed),
        }
    }
    (events, LaneDrain::Exhausted)
}

pub fn run_app(channels: UiChannels) -> io::Result<()> {
    let capabilities = detect_capabilities();
    let mut backend = FullscreenBackend::start(capabilities)?;
    let result = run_loop(&mut backend, &channels);
    let _ = channels.commands.send(UiCommand::Shutdown);
    let shutdown = backend.shutdown();
    result.and(shutdown)
}

fn run_loop(backend: &mut FullscreenBackend, channels: &UiChannels) -> io::Result<()> {
    let mut state = AppState::new();
    let capabilities = detect_capabilities();
    let mut dirty = true;
    let started = Instant::now();
    let mut last_motion_frame = 0;
    let mut last_status_second = 0;
    let mut control_closed = false;
    let mut stream_closed = false;
    let mut render_cache = WrapCache::default();
    let mut coalescer = EventCoalescer::new(1024, Duration::from_millis(16));
    let mut visible_stream_started = false;
    loop {
        let clock = runtime_clock(started.elapsed());
        let effects = reduce(&mut state, Action::SyncClock(clock));
        run_effects(channels, &mut state, effects)?;

        let (control_events, control_drain) = if control_closed {
            (Vec::new(), LaneDrain::Closed)
        } else {
            receive_batch(&channels.events, CONTROL_BATCH_LIMIT)
        };
        let (stream_events, stream_drain) = if stream_closed {
            (Vec::new(), LaneDrain::Closed)
        } else {
            receive_batch(&channels.events_data, STREAM_BATCH_LIMIT)
        };
        channels.lane_space.notify();
        let data_barrier = control_requires_data_barrier(&control_events);
        let mut stream_events = Some(stream_events);
        if data_barrier {
            reduce_stream_events(
                channels,
                &mut state,
                &mut coalescer,
                &mut visible_stream_started,
                &mut dirty,
                stream_events.take().expect("stream batch"),
                true,
            )?;
        }
        if !control_events.is_empty() && !coalescer.is_empty() {
            for event in coalescer.flush() {
                let effects = reduce(&mut state, Action::UiEventReceived(event));
                run_effects(channels, &mut state, effects)?;
                dirty = true;
            }
        }
        channels.lane_space.notify();
        for event in control_events {
            update_visible_stream_state(&event, &mut visible_stream_started);
            let effects = reduce(&mut state, Action::UiEventReceived(event));
            run_effects(channels, &mut state, effects)?;
            dirty = true;
        }
        control_closed = control_drain == LaneDrain::Closed;

        if let Some(stream_events) = stream_events {
            reduce_stream_events(
                channels,
                &mut state,
                &mut coalescer,
                &mut visible_stream_started,
                &mut dirty,
                stream_events,
                false,
            )?;
        }
        stream_closed = stream_drain == LaneDrain::Closed;
        if stream_closed || coalescer.window_elapsed() {
            for event in coalescer.flush() {
                let effects = reduce(&mut state, Action::UiEventReceived(event));
                run_effects(channels, &mut state, effects)?;
                dirty = true;
            }
        }
        if control_drain == LaneDrain::Exhausted || stream_drain == LaneDrain::Exhausted {
            channels.wake.notify();
        }
        if state.shutdown {
            return Ok(());
        }
        if dirty {
            draw_state(backend, &state, capabilities, &mut render_cache)?;
            dirty = false;
        }
        if control_closed && stream_closed {
            return Ok(());
        }

        let size = backend.terminal().size()?;
        let regions = plan_regions(&state, size.width, size.height);
        let motion_visible = motion_needed(&state, capabilities);
        let status_visible = regions.activity_rail.height > 0;
        let next_toast_expiry_ms = state
            .visible_notifications()
            .map(|notification| notification.created_ms.saturating_add(INFO_TOAST_TTL_MS))
            .min();
        let visual_deadline = next_visual_deadline(
            started.elapsed(),
            last_motion_frame,
            last_status_second,
            motion_visible,
            status_visible,
            next_toast_expiry_ms,
        );
        let deadline = [visual_deadline, coalescer.time_until_flush()]
            .into_iter()
            .flatten()
            .min();
        match wait_for_runtime_signal(&channels.wake, deadline)? {
            WaitOutcome::Wake => {}
            WaitOutcome::Input => {
                let event = read()?;
                if let Some(action) =
                    terminal_action(event, &state, (size.width, size.height), &mut render_cache)
                {
                    let effects = reduce(&mut state, action);
                    run_effects(channels, &mut state, effects)?;
                    dirty = true;
                }
            }
            WaitOutcome::Deadline => {
                let clock = runtime_clock(started.elapsed());
                let motion_due = motion_visible && clock.frame > last_motion_frame;
                let status_second = clock.elapsed_ms / 1_000;
                let status_due = status_visible && status_second > last_status_second;
                if motion_due || status_due {
                    if motion_due {
                        last_motion_frame = clock.frame;
                    }
                    if status_due {
                        last_status_second = status_second;
                    }
                    let action = if motion_due {
                        Action::Tick(clock)
                    } else {
                        Action::StatusTick(clock)
                    };
                    let effects = reduce(&mut state, action);
                    run_effects(channels, &mut state, effects)?;
                    dirty = true;
                } else if next_toast_expiry_ms.is_some() {
                    let effects = reduce(&mut state, Action::SyncClock(clock));
                    run_effects(channels, &mut state, effects)?;
                    dirty = true;
                }
            }
        }
    }
}

fn control_requires_data_barrier(events: &[crate::api::UiEvent]) -> bool {
    events.iter().any(|event| {
        !event.is_control() || matches!(event, crate::api::UiEvent::RunCancelled { .. })
    })
}

fn update_visible_stream_state(event: &crate::api::UiEvent, visible_stream_started: &mut bool) {
    if matches!(
        event,
        crate::api::UiEvent::RunStarted { .. }
            | crate::api::UiEvent::AssistantEnded
            | crate::api::UiEvent::RunCompleted { .. }
            | crate::api::UiEvent::RunStopped { .. }
            | crate::api::UiEvent::RunCancelled { .. }
            | crate::api::UiEvent::RunFailed { .. }
    ) {
        *visible_stream_started = false;
    }
}

fn reduce_stream_events(
    channels: &UiChannels,
    state: &mut AppState,
    coalescer: &mut EventCoalescer,
    visible_stream_started: &mut bool,
    dirty: &mut bool,
    events: Vec<crate::api::UiEvent>,
    force_flush: bool,
) -> io::Result<()> {
    for event in events {
        update_visible_stream_state(&event, visible_stream_started);
        let first_visible = !*visible_stream_started
            && matches!(
                &event,
                crate::api::UiEvent::AssistantDelta { .. }
                    | crate::api::UiEvent::ThinkingDelta { .. }
            );
        for ready in coalescer.push_data(event) {
            let effects = reduce(state, Action::UiEventReceived(ready));
            run_effects(channels, state, effects)?;
            *dirty = true;
        }
        if first_visible {
            *visible_stream_started = true;
            for ready in coalescer.flush() {
                let effects = reduce(state, Action::UiEventReceived(ready));
                run_effects(channels, state, effects)?;
                *dirty = true;
            }
        }
    }
    if force_flush {
        for event in coalescer.flush() {
            let effects = reduce(state, Action::UiEventReceived(event));
            run_effects(channels, state, effects)?;
            *dirty = true;
        }
    }
    Ok(())
}

fn run_effects(
    channels: &UiChannels,
    state: &mut AppState,
    effects: Vec<Effect>,
) -> io::Result<()> {
    for effect in effects {
        match effect {
            Effect::Send(command) => send(&channels.commands, command)?,
            Effect::CopyToClipboard(text) => {
                let success = crate::clipboard::copy_text(&text).is_ok();
                let follow_up = reduce(state, Action::ClipboardCompleted { success });
                run_effects(channels, state, follow_up)?;
            }
            Effect::RequestRender => {}
        }
    }
    Ok(())
}

fn send(commands: &std::sync::mpsc::Sender<UiCommand>, command: UiCommand) -> io::Result<()> {
    commands
        .send(command)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI runtime disconnected"))
}

fn navigation_captured(state: &AppState) -> bool {
    state.login_overlay.is_some()
        || state.model_overlay.is_some()
        || state.effort_overlay.is_some()
        || state.palette_query.is_some()
        || state.search.is_some()
        || state.slash_suggestions.is_some()
        || state.pending_interaction().is_some()
}

/// Maps terminal events onto reducer Actions. Overlays, slash, palette and a
/// pending structured question keep arrows; otherwise they become scroll.
pub fn terminal_action(
    event: Event,
    state: &AppState,
    size: (u16, u16),
    cache: &mut WrapCache,
) -> Option<Action> {
    match event {
        Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
            if !navigation_captured(state) {
                if key.code == KeyCode::Enter
                    && key.modifiers == KeyModifiers::NONE
                    && state.composer.is_empty()
                    && state.scroll.is_live_edge()
                {
                    let metrics = measure_scrollback(state, size.0, size.1, cache);
                    if let Some(anchor) = metrics.last_visible_foldable_anchor {
                        return Some(Action::ToggleBlock(anchor.block_id));
                    }
                }
                let intent = match key.code {
                    KeyCode::Up => Some(ScrollIntent::Up),
                    KeyCode::Down => Some(ScrollIntent::Down),
                    KeyCode::PageUp => Some(ScrollIntent::PageUp),
                    KeyCode::PageDown => Some(ScrollIntent::PageDown),
                    // Home/End edit the cursor while typing at the live edge,
                    // but navigate while pinned: a pinned view is an explicit
                    // reading position, and the footer promises `End latest`.
                    // The draft is preserved; only the viewport moves.
                    KeyCode::Home if state.composer.is_empty() || state.scroll.is_pinned() => {
                        Some(ScrollIntent::Top)
                    }
                    KeyCode::End if state.composer.is_empty() || state.scroll.is_pinned() => {
                        Some(ScrollIntent::LiveEdge)
                    }
                    _ => None,
                };
                if let Some(intent) = intent {
                    return Some(Action::Scroll {
                        intent,
                        metrics: measure_scrollback(state, size.0, size.1, cache),
                    });
                }
            }
            Some(Action::Key(key))
        }
        Event::Paste(payload) => Some(Action::Paste(payload)),
        Event::Resize(_, _) => Some(Action::Resize),
        Event::Mouse(mouse) => mouse_action(mouse, state, size, cache),
        _ => None,
    }
}

fn mouse_action(
    mouse: MouseEvent,
    state: &AppState,
    size: (u16, u16),
    cache: &mut WrapCache,
) -> Option<Action> {
    let intent = match mouse.kind {
        MouseEventKind::ScrollUp => ScrollIntent::Up,
        MouseEventKind::ScrollDown => ScrollIntent::Down,
        _ => return None,
    };
    Some(Action::Scroll {
        intent,
        metrics: measure_scrollback(state, size.0, size.1, cache),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkspaceRegions {
    transcript: ratatui::layout::Rect,
    inspector: Option<ratatui::layout::Rect>,
}

fn workspace_regions(state: &AppState, scrollback: ratatui::layout::Rect) -> WorkspaceRegions {
    let has_transcript = !state.blocks().is_empty();
    let explicit_inspector =
        state.inspector.active.is_some() && scrollback.width >= DOCKED_INSPECTOR_MIN_WIDTH;
    let default_inspector = has_transcript && scrollback.width >= DEFAULT_INSPECTOR_MIN_WIDTH;
    let inspector_visible = explicit_inspector || default_inspector;
    let max_width = if inspector_visible {
        MAX_WORKSPACE_WIDTH
    } else {
        scrollback.width
    };
    let workspace = centered(
        scrollback,
        scrollback.width.min(max_width),
        scrollback.height,
    );
    if !inspector_visible {
        return WorkspaceRegions {
            transcript: workspace,
            inspector: None,
        };
    }
    let inspector_width = (((u32::from(workspace.width) * 38) / 100) as u16)
        .clamp(34, 52)
        .min(workspace.width.saturating_sub(40));
    if inspector_width == 0 {
        return WorkspaceRegions {
            transcript: workspace,
            inspector: None,
        };
    }
    let transcript_width = workspace.width - inspector_width;
    WorkspaceRegions {
        transcript: ratatui::layout::Rect {
            width: transcript_width,
            ..workspace
        },
        inspector: Some(ratatui::layout::Rect {
            x: workspace.x + transcript_width,
            width: inspector_width,
            ..workspace
        }),
    }
}

pub fn measure_scrollback(
    state: &AppState,
    width: u16,
    height: u16,
    cache: &mut WrapCache,
) -> ScrollMetrics {
    let regions = plan_regions(state, width, height);
    let workspace = workspace_regions(state, to_ratatui(regions.scrollback));
    let notices = toast_row_count(state, workspace.transcript.height);
    let viewport = u64::from(workspace.transcript.height.saturating_sub(notices));
    let mut content_width = workspace.transcript.width;
    let mut index = HeightIndex::build(state.blocks(), content_width, cache);
    if viewport > 0 && index.total_rows > viewport {
        content_width = content_width.saturating_sub(1);
        index = HeightIndex::build(state.blocks(), content_width, cache);
    }
    index.metrics(&state.scroll.mode, viewport)
}

struct Palette {
    background: Style,
    surface: Style,
    surface_alt: Style,
    composer_bg: Style,
    user_prompt_bg: Style,
    text: Style,
    muted: Style,
    secondary: Style,
    accent: Style,
    accent_bold: Style,
    thinking: Style,
    heading: Style,
    h1: Style,
    link: Style,
    quote: Style,
    tool: Style,
    code_block: Style,
    code_rail: Style,
    diff_add: Style,
    diff_remove: Style,
    diff_add_bg: Style,
    diff_remove_bg: Style,
    warning: Style,
    error: Style,
    success: Style,
    border: Style,
    border_focus: Style,
    scrollbar_track: Style,
    scrollbar_thumb: Style,
}

impl Palette {
    fn of(capabilities: Capabilities) -> Self {
        let theme = resolve_theme(capabilities);
        let color = |rgb: (u8, u8, u8)| {
            if capabilities.color_depth == ColorDepth::None {
                Color::Reset
            } else {
                to_terminal_color(capabilities.color_depth, rgb)
            }
        };
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
            thinking: base.fg(color(theme.thinking_accent)),
            heading: base
                .fg(color(theme.heading_accent))
                .add_modifier(Modifier::BOLD),
            h1: base
                .fg(color(theme.assistant_accent))
                .add_modifier(Modifier::BOLD),
            link: base.fg(color(theme.link_accent)),
            quote: base.fg(color(theme.secondary_text)),
            tool: base.fg(color(theme.tool_accent)),
            code_block: base.fg(color(theme.foreground)).bg(color(theme.code_bg)),
            code_rail: base.fg(color(theme.code_rail)),
            diff_add: base.fg(color(theme.diff_add)),
            diff_remove: base.fg(color(theme.diff_remove)),
            diff_add_bg: base.bg(color(theme.diff_add_bg)),
            diff_remove_bg: base.bg(color(theme.diff_remove_bg)),
            warning: base.fg(color(theme.warning)),
            error: base.fg(color(theme.error)),
            success: base.fg(color(theme.success)),
            border: base.fg(color(theme.border)),
            border_focus: base.fg(color(theme.border_focus)),
            scrollbar_track: base.fg(color(theme.scrollbar_track)),
            scrollbar_thumb: base.fg(color(theme.scrollbar_thumb)),
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

pub fn render_frame(
    frame: &mut ratatui::Frame,
    state: &AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) {
    let palette = Palette::of(capabilities);
    let area = frame.area();
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.background),
        area,
    );
    let regions = plan_regions(state, area.width, area.height);
    let session_visible = regions.session_rail.height > 0;
    let scrollback = to_ratatui(regions.scrollback);
    let workspace = workspace_regions(state, scrollback);

    if session_visible {
        render_session_rail(frame, to_ratatui(regions.session_rail), state, &palette);
    }
    render_scrollback(
        frame,
        workspace.transcript,
        state,
        &palette,
        cache,
        capabilities,
    );
    if regions.todo.height > 0 {
        render_todo_dock(
            frame,
            to_ratatui(regions.todo),
            state,
            &palette,
            capabilities,
        );
    }
    render_divider(
        frame,
        horizontal_inset(content_column(to_ratatui(regions.todo_divider))),
        palette.border,
    );
    if regions.activity_rail.height > 0 {
        render_activity_rail(
            frame,
            horizontal_inset(content_column(to_ratatui(regions.activity_rail))),
            state,
            &palette,
            capabilities,
        );
    }
    let composer_area = horizontal_inset(content_column(to_ratatui(regions.composer)));
    render_composer(frame, composer_area, state, &palette);
    if let Some(suggestions) = &state.slash_suggestions {
        let matches = slash_matches_with_skills(state, &suggestions.query);
        render_slash_popup(frame, composer_area, suggestions, &matches, &palette);
    }
    render_operational_bar(
        frame,
        horizontal_inset(content_column(to_ratatui(regions.operational))),
        state,
        &palette,
        session_visible,
        regions.activity_rail.height > 0,
    );
    if let Some(inspector) = workspace.inspector {
        render_inspector_panel(frame, inspector, state, state.inspector.active, &palette);
    } else if let Some(kind) = state.inspector.active {
        render_inspector_overlay(frame, scrollback, state, kind, &palette);
    }
    if state.search.is_some() {
        render_search_bar(frame, workspace.transcript, state, &palette);
    }
    if let Some(overlay) = &state.model_overlay {
        render_model_overlay(
            frame,
            overlay,
            &state.open_code_models,
            &state.cline_pass_models,
            &state.command_code_models,
            &palette,
        );
    }
    if let Some(overlay) = &state.effort_overlay {
        render_effort_overlay(frame, overlay, &palette);
    }
    if let Some(overlay) = &state.login_overlay {
        render_login_overlay(frame, overlay, &palette);
    }
    if let Some(query) = &state.palette_query {
        render_palette(
            frame,
            query,
            state.palette_selected,
            state.palette_viewport_start,
            &palette,
        );
    }
}

fn plan_regions(state: &AppState, width: u16, height: u16) -> crate::layout::LayoutRegions {
    let todo_rows = todo_height(
        state.todo_dock_open,
        state.todo_items.len(),
        state
            .todo_items
            .iter()
            .any(|item| item.status == TodoItemStatus::InProgress),
    );
    let show_session = !state.blocks().is_empty() && width >= 80 && height >= 12;
    let composer_lines = state
        .composer
        .display_snapshot(width.saturating_sub(6).max(1) as usize)
        .total_lines
        .saturating_add(attachment_rows(&state.attachment_labels));
    plan_with_session_rail_and_composer(
        width,
        height,
        todo_rows,
        state.working || state.activity.is_some(),
        show_session,
        composer_lines,
    )
}

fn welcome_visible(state: &AppState) -> bool {
    state.blocks().is_empty() && state.visible_notifications().next().is_none() && !state.working
}

fn toast_row_count(state: &AppState, area_height: u16) -> u16 {
    state.visible_toast_tail(3).len().min(area_height as usize) as u16
}

fn motion_needed(state: &AppState, capabilities: Capabilities) -> bool {
    if capabilities.reduced_motion || welcome_visible(state) {
        return false;
    }
    state.working
        || state
            .blocks()
            .iter()
            .any(|block| block.lifecycle == BlockLifecycle::Streaming)
}

fn render_session_rail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.surface),
        area,
    );
    let content = horizontal_inset(content_column(area));
    let projection = session_rail_projection(state, content.width as usize, state.working);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(projection.identity, palette.accent_bold),
            Span::raw(" ".repeat(projection.gap)),
            Span::styled(projection.status, palette.secondary),
        ]))
        .style(palette.surface),
        content,
    );
}

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
    let notice_count = toast_row_count(state, area.height);
    let viewport = u64::from(area.height.saturating_sub(notice_count));
    let mut content_width = area.width;
    let mut index = HeightIndex::build(state.blocks(), content_width, cache);
    let scrollbar = viewport > 0 && index.total_rows > viewport;
    if scrollbar {
        content_width = area.width.saturating_sub(1);
        index = HeightIndex::build(state.blocks(), content_width, cache);
    }
    let metrics = index.metrics(&state.scroll.mode, viewport);
    let bottom = metrics.bottom_start;
    let start_row = metrics.viewport_start;
    let (mut idx, mut skip_rows) = index.locate(start_row);
    let capacity = viewport as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(capacity);
    let mut rows = 0usize;
    let selected = state.selected_block_id().cloned();
    let search_matches = state
        .search
        .as_ref()
        .map(|search| search_match_indices_filtered(state.blocks(), &search.query, search.filter))
        .unwrap_or_default();
    let selected_search_id = state.search.as_ref().and_then(|search| {
        search_matches
            .get(search.selected)
            .and_then(|index| state.blocks().get(*index))
            .map(|block| block.id.clone())
    });
    let live_collapsed_group = if state.working {
        last_collapsed_tool_group_leader(state.blocks())
    } else {
        None
    };
    while idx < index.entries.len() && rows < capacity {
        let (_, block, members) = index.entries[idx];
        idx += 1;
        let is_selected = selected.as_ref() == Some(&block.id);
        let show_enter_hint = is_selected
            || live_collapsed_group
                .as_ref()
                .is_some_and(|leader| leader == &block.id);
        let ctx = BlockRender {
            palette,
            width: content_width,
            capabilities,
            selected: is_selected,
            frame: state.clock.frame,
        };
        let mut block_lines = if members.len() > 1 {
            if crate::block::is_failed_tool(block) {
                grouped_failed_tool_lines(block, members, show_enter_hint, &ctx)
            } else if crate::block::is_complete_thinking(block) {
                grouped_thinking_lines(block, members, &ctx)
            } else {
                grouped_tool_lines(block, members, show_enter_hint, &ctx)
            }
        } else {
            safe_block_lines(block, &ctx, cache)
        };
        let search_match = search_matches
            .iter()
            .filter_map(|index| state.blocks().get(*index))
            .any(|matched| {
                matched.id == block.id || members.iter().any(|member| member.id == matched.id)
            });
        if search_match {
            let selected_match = selected_search_id
                .as_ref()
                .is_some_and(|id| id == &block.id || members.iter().any(|member| &member.id == id));
            let highlight = if selected_match {
                palette.surface_alt.patch(palette.accent_bold)
            } else {
                palette.surface_alt
            };
            for line in &mut block_lines {
                for span in &mut line.spans {
                    span.style = span.style.patch(highlight);
                }
            }
        }
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
    let text_area = ratatui::layout::Rect {
        x: area.x,
        y: area.y,
        width: content_width,
        height: area.height,
    };
    // Rows arrive pre-wrapped to `content_width` by the same contract the
    // HeightIndex measures. No widget-level wrap: ratatui's WordWrapper
    // renders whitespace-only rows twice, desynchronising heights and
    // clipping the tail of the transcript at the live edge.
    frame.render_widget(Paragraph::new(lines).style(palette.text), text_area);
    if scrollbar {
        let track = viewport.max(1);
        let thumb = ((viewport * viewport) / index.total_rows.max(1)).clamp(1, track);
        let max_top = track.saturating_sub(thumb);
        let thumb_top = start_row
            .saturating_mul(max_top)
            .checked_div(bottom)
            .unwrap_or(0)
            .min(max_top);
        let bar: Vec<Line> = (0..track)
            .map(|row| {
                let (glyph, style) = if row >= thumb_top && row < thumb_top.saturating_add(thumb)
                {
                    ("┃", palette.scrollbar_thumb)
                } else {
                    ("│", palette.scrollbar_track)
                };
                Line::from(Span::styled(glyph, style))
            })
            .collect();
        frame.render_widget(
            Paragraph::new(bar),
            ratatui::layout::Rect {
                x: area.x + area.width.saturating_sub(1),
                y: area.y,
                width: 1,
                height: track as u16,
            },
        );
    }
    if notice_count > 0 {
        let notices: Vec<Line> = state
            .visible_toast_tail(notice_count as usize)
            .into_iter()
            .map(|notice| {
                let safe = sanitize_terminal_text(notice);
                let truncated =
                    crate::view_model::truncate_display_width(&safe, area.width as usize);
                Line::from(Span::styled(truncated, palette.muted))
            })
            .collect();
        let toast_area = ratatui::layout::Rect {
            x: area.x,
            y: area.y + area.height - notice_count,
            width: area.width,
            height: notice_count,
        };
        frame.render_widget(Paragraph::new(notices).style(palette.surface), toast_area);
    }
}

fn render_todo_dock(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    capabilities: Capabilities,
) {
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.surface_alt),
        area,
    );
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
        header.push(Span::styled(
            format!(" {} ", glyph(capabilities, '\u{25cc}', '~')),
            palette.warning,
        ));
        header.push(Span::styled(
            sanitize_terminal_text(&active.title),
            palette.text,
        ));
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
                let fallback = match item.status {
                    TodoItemStatus::Completed => '+',
                    TodoItemStatus::InProgress => '~',
                    TodoItemStatus::Pending => 'o',
                    TodoItemStatus::Blocked | TodoItemStatus::Cancelled => 'x',
                };
                Line::from(vec![
                    Span::styled(
                        format!("{} ", glyph(capabilities, item.status.glyph(), fallback)),
                        style,
                    ),
                    Span::styled(sanitize_terminal_text(&item.title), palette.muted),
                ])
            })
            .collect();
        rows.extend(rest);
    }
    frame.render_widget(
        Paragraph::new(rows).style(palette.surface_alt),
        horizontal_inset(content_column(area)),
    );
}

fn render_activity_rail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    capabilities: Capabilities,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let spin = spinner_glyph(state.clock.frame, capabilities);
    let label = activity_label(state);
    let elapsed = activity_elapsed(state);
    let elapsed_text = if elapsed == 0 {
        String::new()
    } else {
        format!(" · {elapsed}s")
    };
    let mut spans = vec![
        Span::styled(format!("{spin} "), palette.warning),
        Span::styled(label, palette.text),
        Span::styled(elapsed_text, palette.muted),
    ];
    if area.width >= 72 {
        spans.push(Span::styled(
            format!(
                " · turn {}/{} · reads {}/{} · edits {}/{}",
                state.turns_used,
                state.max_turns,
                state.tools_used_read,
                state.max_read_tool_calls,
                state.tools_used_mutating,
                state.max_mutating_tool_calls
            ),
            palette.muted,
        ));
    }
    let used: usize = spans.iter().map(|span| span.width()).sum();
    let cancel = "Ctrl+C stop";
    if used + 1 + UnicodeWidthStr::width(cancel) <= area.width as usize {
        let pad = (area.width as usize).saturating_sub(used + UnicodeWidthStr::width(cancel));
        spans.push(Span::raw(" ".repeat(pad.max(1))));
        spans.push(Span::styled(cancel, palette.muted));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(palette.surface),
        area,
    );
}

fn render_welcome(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    _capabilities: Capabilities,
) {
    let (status, hint, connected) = if state.authenticated {
        (
            format!(
                "Connected · {}",
                state
                    .auth_provider
                    .map(LoginProvider::label)
                    .unwrap_or("provider")
            ),
            "Describe a task to begin",
            true,
        )
    } else {
        ("Not connected".into(), "Run /login to connect", false)
    };
    let dot = if connected { '\u{25cf}' } else { '\u{25cb}' };
    let dot_style = if connected {
        palette.success
    } else {
        palette.muted
    };
    let title_style = palette
        .text
        .patch(Style::default().add_modifier(Modifier::BOLD));
    let cwd = (!is_trivial_cwd(&state.cwd)).then(|| display_cwd(&state.cwd));
    let mut block_width = 58u16;
    if let Some(cwd) = &cwd {
        let cwd_width = UnicodeWidthStr::width(cwd.as_str()) as u16;
        block_width = block_width.max(cwd_width.min(72));
    }
    let block_width = block_width.min(area.width);
    let shortcuts = if connected {
        "Ctrl+P commands · Shift+Tab mode · /model"
    } else {
        "Ctrl+P commands · /login"
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("SLIM", title_style),
            Span::styled(concat!(" v", env!("CARGO_PKG_VERSION")), palette.muted),
        ]),
        Line::default(),
        Line::from(vec![
            Span::styled(format!("{dot}  "), dot_style),
            Span::styled(status, palette.muted),
        ]),
        Line::from(Span::styled(hint, palette.accent)),
    ];
    // Detail rows (cwd, shortcuts) are the first to go on short viewports.
    if area.height >= 8 {
        lines.push(Line::default());
        if let Some(cwd) = cwd {
            lines.push(Line::from(Span::styled(
                truncate_display_width(&cwd, block_width as usize),
                palette.muted,
            )));
        }
        lines.push(Line::from(Span::styled(shortcuts, palette.muted)));
    } else {
        lines.retain(|line| !line.spans.is_empty());
    }
    if area.height <= 5 {
        lines = vec![
            Line::from(Span::styled("SLIM", title_style)),
            Line::from(vec![
                Span::styled(format!("{dot}  "), dot_style),
                Span::styled(
                    if connected {
                        "Connected"
                    } else {
                        "Not connected"
                    },
                    palette.muted,
                ),
            ]),
        ];
    }
    let centered_area = centered(area, block_width, lines.len() as u16);
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        centered_area,
    );
}

fn last_collapsed_tool_group_leader(blocks: &[Block]) -> Option<BlockId> {
    let mut index = blocks.len();
    while index > 0 {
        index -= 1;
        let block = &blocks[index];
        if !matches!(block.kind(), BlockKind::Tool(_)) {
            continue;
        }
        let span = if crate::block::is_complete_tool(block) {
            crate::block::consecutive_complete_tool_span(blocks, index)
                .map(|(start, end)| (start, crate::block::complete_tool_count(blocks, start, end)))
        } else if crate::block::is_failed_tool(block) {
            crate::block::consecutive_identical_failed_tool_span(blocks, index)
                .map(|(start, end)| (start, end.saturating_sub(start)))
        } else {
            None
        };
        let Some((start, group_len)) = span else {
            continue;
        };
        if group_len > 1 && blocks[start].fold != FoldState::Expanded {
            return Some(blocks[start].id.clone());
        }
        index = start;
    }
    None
}

fn grouped_tool_lines(
    leader: &Block,
    members: &[Block],
    show_enter_hint: bool,
    ctx: &BlockRender<'_>,
) -> Vec<Line<'static>> {
    let duration_ms: Option<u64> = members
        .iter()
        .try_fold(0u64, |acc, block| match block.kind() {
            BlockKind::Tool(tool) => tool
                .duration_ms
                .map(|duration| acc.saturating_add(duration)),
            _ => Some(acc),
        });
    let duration =
        duration_ms.map_or_else(String::new, |value| format!(" · {}", duration_label(value)));
    let marker = if ctx.selected { "> " } else { "  " };
    let complete = glyph(ctx.capabilities, '\u{2713}', '+');
    let names = grouped_tool_name_summary(members);
    let tool_count = members
        .iter()
        .filter(|block| crate::block::is_complete_tool(block))
        .count();
    let detailed = format!("{tool_count} tools · {names}{duration}");
    let compact = format!("{tool_count} tools{duration}");
    let detail = if UnicodeWidthStr::width(marker)
        + UnicodeWidthStr::width(" ")
        + UnicodeWidthStr::width(complete.to_string().as_str())
        + UnicodeWidthStr::width(detailed.as_str())
        <= ctx.width as usize
    {
        detailed
    } else {
        compact
    };
    let hint = "Enter details";
    let marker_width = UnicodeWidthStr::width(marker);
    let glyph_text = format!("{complete} ");
    let header_width =
        UnicodeWidthStr::width(glyph_text.as_str()) + UnicodeWidthStr::width(detail.as_str());
    let hint_width = UnicodeWidthStr::width(hint);
    let content_width = ctx.width as usize;
    let mut header_spans = vec![
        Span::styled(marker.to_owned(), ctx.palette.muted),
        Span::styled(glyph_text, ctx.palette.success),
        Span::styled(detail, ctx.palette.muted),
    ];
    if leader.fold != FoldState::Expanded
        && show_enter_hint
        && marker_width + header_width + 1 + hint_width <= content_width
    {
        let gap = content_width.saturating_sub(marker_width + header_width + hint_width);
        header_spans.push(Span::raw(" ".repeat(gap)));
        header_spans.push(Span::styled(hint.to_owned(), ctx.palette.muted));
    }
    let mut lines = vec![Line::from(header_spans)];
    if leader.fold == FoldState::Expanded {
        for member in members {
            lines.extend(tool_member_lines(
                member,
                false,
                ctx.palette,
                ctx.capabilities,
                ctx.width,
                ctx.frame,
                true,
            ));
        }
    }
    lines
}

fn grouped_thinking_lines(
    leader: &Block,
    members: &[Block],
    ctx: &BlockRender<'_>,
) -> Vec<Line<'static>> {
    let count = members
        .iter()
        .filter(|block| crate::block::is_complete_thinking(block))
        .count()
        .max(members.len());
    let marker = if ctx.selected { "> " } else { "  " };
    let thinking_glyph = glyph(ctx.capabilities, '\u{25cc}', '~');
    let label = if count > 1 {
        format!("{marker}{thinking_glyph} Thought ×{count}")
    } else {
        format!("{marker}{thinking_glyph} Thought")
    };
    let mut lines = vec![Line::from(Span::styled(label, ctx.palette.muted))];
    if leader.fold == FoldState::Expanded {
        let body_width = ctx.width.saturating_sub(2).max(1);
        for member in members {
            let BlockKind::Thinking(text) = member.kind() else {
                continue;
            };
            for row in render_plain(text, body_width) {
                lines.push(Line::from(Span::styled(
                    format!("  {row}"),
                    ctx.palette.thinking,
                )));
            }
        }
    }
    lines
}

fn grouped_failed_tool_lines(
    leader: &Block,
    members: &[Block],
    show_enter_hint: bool,
    ctx: &BlockRender<'_>,
) -> Vec<Line<'static>> {
    let count = members
        .iter()
        .filter(|block| crate::block::is_failed_tool(block))
        .count();
    let (name, preview) = match leader.kind() {
        BlockKind::Tool(tool) => (
            sanitize_terminal_text(&tool.name),
            sanitize_terminal_text(&tool.preview),
        ),
        _ => (String::new(), String::new()),
    };
    let reason = if preview.is_empty() {
        format!("{name} ×{count} · failed")
    } else {
        format!("{name} ×{count} · {preview}")
    };
    let marker = if ctx.selected { "> " } else { "  " };
    let failed = glyph(ctx.capabilities, '\u{2715}', 'x');
    let glyph_text = format!("{failed} ");
    let hint = "Enter details";
    let marker_width = UnicodeWidthStr::width(marker);
    let header_width =
        UnicodeWidthStr::width(glyph_text.as_str()) + UnicodeWidthStr::width(reason.as_str());
    let hint_width = UnicodeWidthStr::width(hint);
    let content_width = ctx.width as usize;
    let mut header_spans = vec![
        Span::styled(marker.to_owned(), ctx.palette.muted),
        Span::styled(glyph_text, ctx.palette.error),
        Span::styled(reason, ctx.palette.secondary),
    ];
    if leader.fold != FoldState::Expanded
        && show_enter_hint
        && marker_width + header_width + 1 + hint_width <= content_width
    {
        let gap = content_width.saturating_sub(marker_width + header_width + hint_width);
        header_spans.push(Span::raw(" ".repeat(gap)));
        header_spans.push(Span::styled(hint.to_owned(), ctx.palette.muted));
    }
    let mut lines = vec![Line::from(header_spans)];
    if leader.fold == FoldState::Expanded {
        for member in members {
            lines.extend(tool_member_lines(
                member,
                false,
                ctx.palette,
                ctx.capabilities,
                ctx.width,
                ctx.frame,
                true,
            ));
        }
    }
    lines
}

fn grouped_tool_name_summary(members: &[Block]) -> String {
    let mut names: Vec<(String, usize)> = Vec::new();
    for member in members {
        let BlockKind::Tool(tool) = member.kind() else {
            continue;
        };
        let name = sanitize_terminal_text(&tool.name);
        if let Some((_, count)) = names.iter_mut().find(|(seen, _)| seen == &name) {
            *count = count.saturating_add(1);
        } else {
            names.push((name, 1));
        }
    }

    let hidden = names.len().saturating_sub(3);
    let mut summary = names
        .iter()
        .take(3)
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name} ×{count}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>();
    if hidden > 0 {
        summary.push(format!("+{hidden}"));
    }
    summary.join(", ")
}

fn tool_member_lines(
    block: &Block,
    selected: bool,
    palette: &Palette,
    capabilities: Capabilities,
    width: u16,
    frame: u64,
    include_call_id: bool,
) -> Vec<Line<'static>> {
    let BlockKind::Tool(state) = block.kind() else {
        return Vec::new();
    };
    let (glyph_text, glyph_style, name_style, status) = match block.lifecycle {
        BlockLifecycle::Pending => (
            format!("{} ", glyph(capabilities, '\u{25cb}', 'o')),
            palette.muted,
            palette.tool,
            None,
        ),
        BlockLifecycle::Streaming => (
            format!("{} ", spinner_glyph(frame, capabilities)),
            palette.warning,
            palette.tool,
            None,
        ),
        BlockLifecycle::Complete => (
            format!("{} ", glyph(capabilities, '\u{2713}', '+')),
            palette.success,
            palette.muted,
            None,
        ),
        BlockLifecycle::Failed => (
            format!("{} ", glyph(capabilities, '\u{2715}', 'x')),
            palette.error,
            palette.secondary,
            Some("failed"),
        ),
        BlockLifecycle::Cancelled => (
            format!("{} ", glyph(capabilities, '\u{25a0}', 'x')),
            palette.warning,
            palette.muted,
            Some("cancelled"),
        ),
    };
    let marker = if selected { "> " } else { "  " };
    let name = sanitize_terminal_text(&state.name);
    let args = sanitize_terminal_text(&state.arguments_summary);
    let preview = sanitize_terminal_text(state.preview.lines().next().unwrap_or_default());
    let call = sanitize_terminal_text(state.call_id.0.as_ref());
    let show_args = block.lifecycle == BlockLifecycle::Streaming
        || block.fold == FoldState::Expanded
        || include_call_id;
    let show_preview = show_args || block.lifecycle == BlockLifecycle::Failed;
    let preview_shown = show_preview && !preview.is_empty();
    let mut parts = vec![ToolDetailPart::fixed(name)];
    if show_args {
        for (index, value) in args
            .split(" · ")
            .filter(|value| !value.is_empty())
            .enumerate()
        {
            parts.push(if index == 0 {
                ToolDetailPart::flexible(value.to_owned(), 0, 1)
            } else {
                ToolDetailPart::fixed(value.to_owned())
            });
        }
        if include_call_id && !call.is_empty() {
            parts.push(ToolDetailPart::flexible(call, 0, 1));
        }
    }
    if show_preview {
        for (index, value) in preview
            .split(" · ")
            .filter(|value| !value.is_empty())
            .enumerate()
        {
            parts.push(if index == 0 {
                ToolDetailPart::flexible(value.to_owned(), 1, 1)
            } else {
                ToolDetailPart::fixed(value.to_owned())
            });
        }
    }
    if let Some(value) = state.duration_ms {
        parts.push(ToolDetailPart::fixed(duration_label(value)));
    }
    if let Some(value) = status.filter(|_| !preview_shown) {
        parts.push(ToolDetailPart::fixed(value.to_owned()));
    }
    let occupied = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(glyph_text.as_str());
    let detail = fit_tool_detail(parts, (width as usize).saturating_sub(occupied));
    let mut lines = vec![Line::from(vec![
        Span::styled(marker.to_owned(), palette.muted),
        Span::styled(glyph_text, glyph_style),
        Span::styled(detail, name_style),
    ])];
    if block.fold == FoldState::Expanded && !state.materialized_output.is_empty() {
        let body_width = width.saturating_sub(4).max(1);
        for row in render_plain(&state.materialized_output, body_width) {
            lines.push(Line::from(Span::styled(
                format!("    {row}"),
                palette.muted,
            )));
        }
    }
    lines
}

struct ToolDetailPart {
    text: String,
    shrink_priority: Option<u8>,
    min_width: usize,
}

impl ToolDetailPart {
    fn fixed(text: String) -> Self {
        Self {
            text,
            shrink_priority: None,
            min_width: 0,
        }
    }

    fn flexible(text: String, priority: u8, min_width: usize) -> Self {
        Self {
            text,
            shrink_priority: Some(priority),
            min_width,
        }
    }
}

fn fit_tool_detail(mut parts: Vec<ToolDetailPart>, width: usize) -> String {
    for priority in [0, 1] {
        while tool_detail_width(&parts) > width {
            let Some(index) = parts
                .iter()
                .position(|part| part.shrink_priority == Some(priority))
            else {
                break;
            };
            let excess = tool_detail_width(&parts).saturating_sub(width);
            let current = UnicodeWidthStr::width(parts[index].text.as_str());
            let target = current.saturating_sub(excess);
            if target >= parts[index].min_width {
                parts[index].text = truncate_cells(&parts[index].text, target);
                break;
            }
            parts.remove(index);
        }
    }
    truncate_cells(
        &parts
            .into_iter()
            .map(|part| part.text)
            .collect::<Vec<_>>()
            .join(" · "),
        width,
    )
}

fn tool_detail_width(parts: &[ToolDetailPart]) -> usize {
    let separators = parts.len().saturating_sub(1) * UnicodeWidthStr::width(" · ");
    parts
        .iter()
        .map(|part| UnicodeWidthStr::width(part.text.as_str()))
        .sum::<usize>()
        .saturating_add(separators)
}

fn duration_label(value: u64) -> String {
    if value == 0 {
        "<1ms".into()
    } else if value < 1000 {
        format!("{value}ms")
    } else if value < 60_000 {
        format!("{:.1}s", value as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", value / 60_000, (value % 60_000) / 1000)
    }
}

/// Immutable per-frame render inputs threaded through the block renderer.
/// Bundled so `block_lines` stays under clippy's argument-count lint.
struct BlockRender<'a> {
    palette: &'a Palette,
    width: u16,
    capabilities: Capabilities,
    selected: bool,
    frame: u64,
}

fn block_lines(block: &Block, ctx: &BlockRender<'_>, cache: &mut WrapCache) -> Vec<Line<'static>> {
    let mut lines = match block.kind() {
        BlockKind::User(text) => {
            let label = "  You";
            let pad = (ctx.width as usize).saturating_sub(UnicodeWidthStr::width(label));
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    label,
                    ctx.palette.secondary.patch(ctx.palette.user_prompt_bg),
                ),
                Span::styled(" ".repeat(pad), ctx.palette.user_prompt_bg),
            ])];
            lines.extend(indented_body(
                text,
                ctx.palette.text,
                ctx.palette.user_prompt_bg,
                ctx.width,
            ));
            lines.push(Line::default());
            lines
        }
        BlockKind::Assistant(text) => {
            let mut lines = vec![Line::from(Span::styled("  Slim", ctx.palette.accent_bold))];
            lines.extend(assistant_body(
                text,
                ctx,
                block.lifecycle == BlockLifecycle::Streaming,
                cache,
                block,
            ));
            lines
        }
        BlockKind::Thinking(text) => {
            let marker = if ctx.selected { "> " } else { "  " };
            let streaming = block.lifecycle == BlockLifecycle::Streaming;
            let thinking_glyph = if streaming {
                spinner_glyph(ctx.frame, ctx.capabilities)
            } else {
                glyph(ctx.capabilities, '\u{25cc}', '~')
            };
            let label = if streaming { "Thinking" } else { "Thought" };
            let header = format!("{marker}{thinking_glyph} {label}");
            let mut lines = vec![Line::from(Span::styled(header, ctx.palette.muted))];
            if block.fold == FoldState::Expanded {
                let body_width = ctx.width.saturating_sub(2).max(1);
                for row in render_plain(text, body_width) {
                    lines.push(Line::from(Span::styled(
                        format!("  {row}"),
                        ctx.palette.thinking,
                    )));
                }
            } else if streaming {
                let preview_width = ctx.width.saturating_sub(4).max(1);
                let (preview, truncated) = thinking_preview_tail(text);
                let rows = render_plain(preview, preview_width);
                let hidden = truncated || rows.len() > 2;
                let start = rows.len().saturating_sub(2);
                for (index, row) in rows.into_iter().skip(start).enumerate() {
                    let prefix = if hidden && index == 0 { "  … " } else { "  " };
                    lines.push(Line::from(Span::styled(
                        format!("{prefix}{row}"),
                        ctx.palette.thinking,
                    )));
                }
            }
            lines
        }
        BlockKind::Tool(_) => tool_member_lines(
            block,
            ctx.selected,
            ctx.palette,
            ctx.capabilities,
            ctx.width,
            ctx.frame,
            false,
        ),
        BlockKind::InteractionRequest(state) => {
            render_plain(&state.display_text(), ctx.width.saturating_sub(2).max(1))
                .into_iter()
                .map(|row| Line::from(Span::styled(row, ctx.palette.text)))
                .collect()
        }
        BlockKind::System(text) => vec![Line::from(Span::styled(
            format!("  system · {}", sanitize_terminal_text(text)),
            ctx.palette.muted,
        ))],
        BlockKind::Error(text) => render_plain(text, ctx.width.saturating_sub(4).max(1))
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let prefix = if index == 0 {
                    format!("  {} ", glyph(ctx.capabilities, '\u{2715}', 'x'))
                } else {
                    "    ".into()
                };
                Line::from(vec![
                    Span::styled(prefix, ctx.palette.error),
                    Span::styled(row, ctx.palette.secondary),
                ])
            })
            .collect(),
        BlockKind::Activity(text) => vec![Line::from(Span::styled(
            format!("  · {}", sanitize_terminal_text(text)),
            ctx.palette.muted,
        ))],
        BlockKind::QueuedUser(text) => render_plain(text, ctx.width.saturating_sub(4).max(1))
            .into_iter()
            .map(|row| Line::from(Span::styled(format!("  … {row}"), ctx.palette.muted)))
            .collect(),
    };
    if block.turn_boundary_before() {
        lines.insert(0, Line::default());
    }
    lines
}

fn assistant_body(
    text: &str,
    ctx: &BlockRender<'_>,
    streaming: bool,
    cache: &mut WrapCache,
    block: &Block,
) -> Vec<Line<'static>> {
    let body_width = ctx.width.saturating_sub(2).max(1);
    let text_width = body_width.saturating_sub(1).max(1);
    let styles = MarkdownStyles {
        text: ctx.palette.text,
        h1: ctx.palette.h1,
        h2: ctx.palette.heading,
        h3: ctx.palette.thinking,
        link: ctx.palette.link,
        code: ctx.palette.tool,
        code_block: ctx.palette.code_block,
        code_rail: ctx.palette.code_rail,
        diff_add: ctx.palette.diff_add,
        diff_remove: ctx.palette.diff_remove,
        diff_add_bg: ctx.palette.diff_add_bg,
        diff_remove_bg: ctx.palette.diff_remove_bg,
        quote: ctx.palette.quote,
    };
    // Only frozen content is cacheable: streaming text changes every frame and
    // must bypass the cache. The caret and indent stay dynamic, applied after.
    let cacheable = !streaming;
    let mut lines = cache.wrapped_body(
        block,
        BodyKind::Assistant,
        ctx.width,
        cacheable,
        || render_markdown(text, text_width, styles),
        cached_lines_bytes,
    );
    let caret = if streaming && !ctx.capabilities.reduced_motion {
        "▌"
    } else {
        " "
    };
    if let Some(last) = lines.last_mut() {
        last.spans.push(Span::styled(caret, ctx.palette.accent));
    } else {
        lines.push(Line::from(Span::styled(caret, ctx.palette.accent)));
    }
    lines
        .into_iter()
        .map(|mut line| {
            let mut spans = vec![Span::styled("  ", ctx.palette.surface)];
            spans.append(&mut line.spans);
            Line::from(spans)
        })
        .collect()
}

fn indented_body(text: &str, body: Style, bg: Style, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    render_plain(text, width.saturating_sub(2).max(1) as u16)
        .into_iter()
        .map(|line| {
            let content_width = 2 + UnicodeWidthStr::width(line.as_str());
            let pad = width.saturating_sub(content_width);
            Line::from(vec![
                Span::styled("  ", bg),
                Span::styled(line, body.patch(bg)),
                Span::styled(" ".repeat(pad), bg),
            ])
        })
        .collect()
}

fn safe_block_lines(
    block: &Block,
    ctx: &BlockRender<'_>,
    cache: &mut WrapCache,
) -> Vec<Line<'static>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_lines(block, ctx, cache)
    }))
    .unwrap_or_else(|_| {
        vec![Line::from(Span::styled(
            "  ⚠ block unavailable",
            ctx.palette.muted,
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

fn composer_label(state: &AppState, area_width: u16, total_lines: usize) -> String {
    let model = ModelAlias::parse(&state.model).map_or_else(
        || sanitize_terminal_text(&state.model),
        |alias| alias.label().into(),
    );
    let effort = state.effort.id();
    let mode = mode_name(state.mode);
    let lines = (total_lines > 1).then(|| format!("{total_lines} lines"));
    let images = (!state.attachment_labels.is_empty()).then(|| {
        format!(
            "{} image{}",
            state.attachment_labels.len(),
            if state.attachment_labels.len() == 1 {
                ""
            } else {
                "s"
            }
        )
    });
    let metadata = [images, lines]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let prefix = if metadata.is_empty() {
        String::new()
    } else {
        format!("{metadata} · ")
    };
    // Model/effort are meaningless before login; show only the mode then.
    let (with_model, with_effort) = if state.authenticated {
        (
            format!(" {prefix}{model} ({effort}) · {mode} "),
            format!(" {prefix}({effort}) · {mode} "),
        )
    } else {
        (String::new(), String::new())
    };
    let candidates = [
        with_model,
        with_effort,
        format!(" {prefix}{mode} "),
        if metadata.is_empty() {
            String::new()
        } else {
            format!(" {metadata} ")
        },
    ];
    let available = area_width.saturating_sub(2) as usize;
    candidates
        .into_iter()
        .filter(|candidate| !candidate.is_empty())
        .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= available)
        .unwrap_or_default()
}

fn render_composer(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
) {
    if area.height == 0 {
        return;
    }
    let focused = state.login_overlay.is_none()
        && state.model_overlay.is_none()
        && state.effort_overlay.is_none()
        && state.palette_query.is_none()
        && state.search.is_none()
        && state.inspector.active.is_none();
    let glyph_style = if focused {
        palette.accent
    } else {
        palette.muted
    };
    let prompt_width = UnicodeWidthStr::width(COMPOSER_PROMPT);
    let text_budget = (area.width.saturating_sub(2) as usize)
        .saturating_sub(prompt_width)
        .max(1);
    let snapshot = state.composer.display_snapshot(text_budget);
    let content_area = if area.height >= 3 {
        let label = composer_label(state, area.width, snapshot.total_lines);
        let border = if focused {
            palette.border_focus
        } else {
            palette.border
        };
        let block = RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border)
            .title_bottom(Line::from(Span::styled(label, palette.muted)))
            .title_alignment(Alignment::Right);
        let inner = block.inner(area);
        frame.render_widget(block.style(palette.composer_bg), area);
        inner
    } else {
        frame.render_widget(RatatuiBlock::default().style(palette.composer_bg), area);
        area
    };
    if content_area.width == 0 || content_area.height == 0 {
        return;
    }
    let budget = content_area.width as usize;
    let text_budget = budget.saturating_sub(prompt_width);
    let attachment_rows = attachment_rows(&state.attachment_labels);
    let total_rows = attachment_rows + snapshot.total_lines;
    let cursor_row = attachment_rows + snapshot.cursor_line;
    let visible_rows = content_area.height as usize;
    let max_start = total_rows.saturating_sub(visible_rows);
    let start = cursor_row
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_start);
    let end = start.saturating_add(visible_rows).min(total_rows);
    let mut cursor_x = prompt_width;
    let mut rendered = Vec::with_capacity(end.saturating_sub(start));
    for row_index in start..end {
        if row_index < attachment_rows {
            let label = if row_index + 1 == attachment_rows
                && state.attachment_labels.len() > attachment_rows
            {
                format!(
                    "image · +{} more",
                    state.attachment_labels.len() - row_index
                )
            } else {
                format!("image · {}", state.attachment_labels[row_index])
            };
            rendered.push(Line::from(vec![
                Span::styled("  ", palette.composer_bg),
                Span::styled(
                    truncate_cells(&sanitize_terminal_text(&label), text_budget),
                    palette.secondary,
                ),
            ]));
            continue;
        }
        let line_index = row_index - attachment_rows;
        let line = sanitize_terminal_text(&snapshot.lines[line_index]);
        let is_cursor_line = line_index == snapshot.cursor_line;
        let (hint, visible, local_cursor) = if is_cursor_line {
            composer_cursor_window(&line, snapshot.cursor_cell, text_budget)
        } else {
            ("", take_head_cells(&line, text_budget), 0)
        };
        if is_cursor_line {
            cursor_x = prompt_width + UnicodeWidthStr::width(hint) + local_cursor;
        }
        rendered.push(Line::from(vec![
            Span::styled(
                if is_cursor_line {
                    COMPOSER_PROMPT
                } else {
                    "  "
                },
                if is_cursor_line {
                    glyph_style
                } else {
                    palette.muted
                },
            ),
            Span::styled(hint.to_owned(), palette.muted),
            Span::styled(visible, palette.text.patch(palette.composer_bg)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(rendered).style(palette.composer_bg),
        content_area,
    );
    if focused {
        let cursor_y = cursor_row.saturating_sub(start) as u16;
        frame.set_cursor_position((
            content_area.x + (cursor_x as u16).min(content_area.width.saturating_sub(1)),
            content_area.y + cursor_y.min(content_area.height.saturating_sub(1)),
        ));
    }
}

fn attachment_rows(labels: &[String]) -> usize {
    labels.len().min(2)
}

fn composer_cursor_window(
    text: &str,
    cursor_cell: usize,
    budget: usize,
) -> (&'static str, String, usize) {
    let width = UnicodeWidthStr::width(text);
    if width <= budget {
        return ("", text.to_owned(), cursor_cell.min(width));
    }
    if cursor_cell < budget {
        return ("", take_head_cells(text, budget), cursor_cell);
    }
    let hint_width = UnicodeWidthStr::width(COMPOSER_OVERFLOW_HINT);
    let reserve_cursor = usize::from(cursor_cell >= width);
    let visible_budget = budget.saturating_sub(hint_width + reserve_cursor);
    let start_cell = cursor_cell.saturating_sub(visible_budget);
    (
        COMPOSER_OVERFLOW_HINT,
        take_cell_window(text, start_cell, visible_budget),
        cursor_cell.saturating_sub(start_cell),
    )
}

fn take_head_cells(text: &str, budget: usize) -> String {
    take_cell_window(text, 0, budget)
}

fn take_cell_window(text: &str, start_cell: usize, budget: usize) -> String {
    let mut position = 0usize;
    let mut used = 0usize;
    let mut visible = String::new();
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if position + width <= start_cell {
            position += width;
            continue;
        }
        if used + width > budget {
            break;
        }
        visible.push_str(grapheme);
        used += width;
        position += width;
    }
    visible
}

fn render_operational_bar(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    session_visible: bool,
    activity_visible: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let overflow_in = if state.input_tokens_overflowed {
        "+"
    } else {
        ""
    };
    let overflow_out = if state.output_tokens_overflowed {
        "+"
    } else {
        ""
    };
    let full_totals = format!(
        "↑{}{overflow_in} ↓{}{overflow_out}",
        state.input_tokens, state.output_tokens
    );
    let compact_totals = format!(
        "↑{}{overflow_in} ↓{}{overflow_out}",
        crate::view_model::format_token_count(state.input_tokens),
        crate::view_model::format_token_count(state.output_tokens)
    );
    let arrows = format!("↑{overflow_in} ↓{overflow_out}");
    let usage_visible = state.input_tokens > 0
        || state.output_tokens > 0
        || state.input_tokens_overflowed
        || state.output_tokens_overflowed;
    let context_full = format_context(state, false);
    let context_compact = format_context(state, true);
    let context_visible = context_full != "ctx --";
    let right_variants = match (session_visible, context_visible, usage_visible) {
        (true, _, true) => vec![full_totals, compact_totals, arrows],
        (true, _, false) => vec![String::new()],
        (false, true, true) => vec![
            format!("{context_full} · {full_totals}"),
            format!("{context_compact} · {compact_totals}"),
            format!("{context_compact} {compact_totals}"),
            context_compact,
            compact_totals,
            arrows,
        ],
        (false, true, false) => vec![context_full, context_compact],
        (false, false, true) => vec![full_totals, compact_totals, arrows],
        (false, false, false) => vec![String::new()],
    };
    // `None` = shortcut chips ("Key label · Key label"), styled per chip below.
    let (left_variants, left_style): (Vec<String>, Option<Style>) =
        if state.working && activity_visible {
            (
                vec![
                    "Shift+Tab mode · Ctrl+C cancel · Ctrl+P commands".into(),
                    "Ctrl+C cancel".into(),
                    "^C".into(),
                ],
                None,
            )
        } else if state.working {
            (
                vec![
                    "Working… Esc cancel · Ctrl+C cancel".into(),
                    "Working… Esc/^C cancel".into(),
                    "Working… ^C".into(),
                ],
                Some(palette.warning),
            )
        } else if state.scroll.is_pinned() {
        let unseen = state.scroll.unseen;
        let mut variants = Vec::new();
        if unseen > 0 {
            variants.push(format!("{unseen} new · End latest"));
            variants.push(format!("{unseen} new · End"));
        } else {
            variants.push("End latest".into());
        }
        variants.push("End".into());
        (variants, Some(palette.secondary))
    } else if !state.authenticated {
        (
            vec!["signed out · /login".into(), "/login".into()],
            Some(palette.muted),
        )
    } else {
        (
            vec![
                "Shift+Tab mode · Ctrl+C exit · Ctrl+P commands".into(),
                "⇧Tab mode · ^C exit · ^P commands".into(),
                "⇧Tab mode · ^C exit".into(),
                "^P · ^C".into(),
            ],
            None,
        )
    };
    let width = |text: &str| UnicodeWidthStr::width(text);
    let available = area.width as usize;
    let Some(minimum_left) = left_variants.last() else {
        return;
    };
    let right = right_variants
        .iter()
        .find(|candidate| width(minimum_left) + 1 + width(candidate) <= available)
        .or_else(|| right_variants.last())
        .cloned()
        .unwrap_or_default();
    let left_budget = available.saturating_sub(width(&right) + 1);
    let left = left_variants
        .iter()
        .find(|candidate| width(candidate) <= left_budget)
        .unwrap_or(minimum_left);
    let pad = available.saturating_sub(width(left) + width(&right)).max(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    match left_style {
        Some(style) => spans.push(Span::styled(left.clone(), style)),
        None => {
            for (index, chip) in left.split(" · ").enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" · ", palette.muted));
                }
                match chip.split_once(' ') {
                    Some((key, label)) => {
                        spans.push(Span::styled(key.to_owned(), palette.secondary));
                        spans.push(Span::styled(format!(" {label}"), palette.muted));
                    }
                    None => spans.push(Span::styled(chip.to_owned(), palette.secondary)),
                }
            }
        }
    }
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, palette.muted));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(palette.surface),
        area,
    );
}

fn render_search_bar(
    frame: &mut ratatui::Frame,
    scrollback: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
) {
    let Some(search) = &state.search else {
        return;
    };
    if scrollback.width < 8 || scrollback.height == 0 {
        return;
    }
    let matches = search_match_indices_filtered(state.blocks(), &search.query, search.filter);
    let position = if matches.is_empty() {
        "0/0".to_owned()
    } else {
        format!(
            "{}/{}",
            search.selected.min(matches.len() - 1) + 1,
            matches.len()
        )
    };
    let scope = format!(" [{}] ", search.filter.label());
    let tail = format!("  {position} · Enter next · Tab filter · Esc ");
    let chrome = UnicodeWidthStr::width(" Find: ")
        + UnicodeWidthStr::width(scope.as_str())
        + UnicodeWidthStr::width(tail.as_str());
    let text = format!(
        " Find: {}{scope}{tail}",
        sanitize_terminal_text(&search.query)
    );
    let width = (UnicodeWidthStr::width(text.as_str()) as u16)
        .saturating_add(1)
        .min(scrollback.width.saturating_sub(2))
        .max(6);
    let area = ratatui::layout::Rect {
        x: scrollback.x + 1,
        y: scrollback.y,
        width,
        height: 1,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Find: ", palette.secondary),
            Span::styled(
                crate::view_model::truncate_display_width(
                    &sanitize_terminal_text(&search.query),
                    area.width.saturating_sub(chrome as u16) as usize,
                ),
                palette.text,
            ),
            Span::styled(format!("{scope}{tail}"), palette.muted),
        ]))
        .style(palette.surface_alt),
        area,
    );
}

fn render_inspector_overlay(
    frame: &mut ratatui::Frame,
    scrollback: ratatui::layout::Rect,
    state: &AppState,
    kind: InspectorKind,
    palette: &Palette,
) {
    if scrollback.width < 12 || scrollback.height < 4 {
        return;
    }
    let width = ((u32::from(frame.area().width) * 9) / 10) as u16;
    let height = ((u32::from(frame.area().height) * 7) / 10) as u16;
    let area = centered(frame.area(), width.max(12), height.max(4));
    render_inspector_panel(frame, area, state, Some(kind), palette);
}

fn render_inspector_panel(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    kind: Option<InspectorKind>,
    palette: &Palette,
) {
    if area.width < 12 || area.height < 4 {
        return;
    }
    frame.render_widget(Clear, area);
    let title = kind.map_or("Run", inspector_title);
    let hint = if kind.is_some() {
        " Esc close "
    } else {
        " Ctrl+P commands "
    };
    let border_style = if kind.is_some() {
        palette.border_focus
    } else {
        palette.border
    };
    let block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(Line::from(vec![
            Span::styled(format!(" {title} "), palette.accent_bold),
            Span::styled(hint, palette.muted),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block.style(palette.surface_alt), area);
    let lines = kind.map_or_else(
        || run_inspector_lines(state, palette, inner.width),
        |kind| inspector_lines(state, kind, palette, inner.width),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .style(palette.surface_alt)
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn inspector_title(kind: InspectorKind) -> &'static str {
    match kind {
        InspectorKind::Diff => "Changes",
        InspectorKind::Activity => "Activity",
        InspectorKind::SessionTree => "Session",
        InspectorKind::Diagnostics => "Diagnostics",
    }
}

fn run_inspector_lines(state: &AppState, palette: &Palette, width: u16) -> Vec<Line<'static>> {
    let truncate = |text: &str| {
        crate::view_model::truncate_display_width(
            &sanitize_terminal_text(text),
            width.saturating_sub(11) as usize,
        )
    };
    let row = |label: &str, value: String, style: Style| {
        Line::from(vec![
            Span::styled(format!(" {label:<9}"), palette.secondary),
            Span::styled(truncate(&value), style),
        ])
    };
    let status = run_status_label(state);
    let status_style = if state.working {
        palette.warning
    } else if state.authenticated {
        palette.success
    } else {
        palette.muted
    };
    let active = state.working || state.activity.is_some();
    let phase = if active {
        activity_label(state)
    } else {
        "Idle".into()
    };
    let elapsed = if active {
        format!("{}s", activity_elapsed(state))
    } else {
        "--".into()
    };
    let mut completed_tools = 0usize;
    let mut failed_tools = 0usize;
    let mut active_tools = 0usize;
    let mut errors = 0usize;
    for block in state.blocks() {
        if block.lifecycle == BlockLifecycle::Failed || matches!(block.kind(), BlockKind::Error(_))
        {
            errors = errors.saturating_add(1);
        }
        if !matches!(block.kind(), BlockKind::Tool(_)) {
            continue;
        }
        match block.lifecycle {
            BlockLifecycle::Complete => completed_tools = completed_tools.saturating_add(1),
            BlockLifecycle::Failed | BlockLifecycle::Cancelled => {
                failed_tools = failed_tools.saturating_add(1);
            }
            BlockLifecycle::Pending | BlockLifecycle::Streaming => {
                active_tools = active_tools.saturating_add(1);
            }
        }
    }
    let tools = if active_tools > 0 {
        format!("{completed_tools} ok · {failed_tools} failed · {active_tools} active")
    } else {
        format!("{completed_tools} ok · {failed_tools} failed")
    };
    let error_style = if errors > 0 {
        palette.warning
    } else {
        palette.muted
    };
    let mut lines = vec![
        row("Status", status.into(), status_style),
        row("Phase", phase, palette.text),
        row("Elapsed", elapsed, palette.muted),
        Line::default(),
        row("Tools", tools, palette.text),
        row("Errors", errors.to_string(), error_style),
        row(
            "Turns",
            format!("{}/{}", state.turns_used, state.max_turns),
            palette.text,
        ),
        row("Context", format_context(state, true), palette.text),
        Line::default(),
        Line::from(Span::styled(" Ctrl+J  activity", palette.muted)),
        Line::from(Span::styled(" Ctrl+D  changes", palette.muted)),
        Line::from(Span::styled(" Ctrl+R  session", palette.muted)),
        Line::from(Span::styled(" Ctrl+G  diagnostics", palette.muted)),
    ];
    if state.working {
        lines.push(Line::from(Span::styled(
            " Ctrl+C  cancel run",
            palette.warning,
        )));
    }
    lines
}

fn inspector_lines(
    state: &AppState,
    kind: InspectorKind,
    palette: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let truncate = |text: &str| {
        crate::view_model::truncate_display_width(
            &sanitize_terminal_text(text),
            width.saturating_sub(2) as usize,
        )
    };
    let mut lines = Vec::new();
    match kind {
        InspectorKind::Diff => {
            for block in state.blocks() {
                let BlockKind::Tool(tool) = block.kind() else {
                    continue;
                };
                if !is_mutating_tool(&tool.name) {
                    continue;
                }
                let (status, status_style) = match block.lifecycle {
                    BlockLifecycle::Complete => ("✓", palette.success),
                    BlockLifecycle::Failed => ("×", palette.error),
                    BlockLifecycle::Cancelled => ("×", palette.warning),
                    BlockLifecycle::Pending | BlockLifecycle::Streaming => ("·", palette.tool),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {status} "), status_style),
                    Span::styled(truncate(&tool.name), palette.text),
                ]));
                if !tool.arguments_summary.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("   {}", truncate(&tool.arguments_summary)),
                        palette.muted,
                    )));
                }
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(
                    " No file changes reported",
                    palette.muted,
                )));
            }
        }
        InspectorKind::Activity => {
            if state.activity.is_some() {
                lines.push(Line::from(Span::styled(
                    format!(" · {}", truncate(&activity_label(state))),
                    palette.warning,
                )));
                lines.push(Line::default());
            }
            for block in state.blocks() {
                let BlockKind::Tool(tool) = block.kind() else {
                    continue;
                };
                let duration = tool
                    .duration_ms
                    .map(|duration| format!(" · {}", duration_label(duration)))
                    .unwrap_or_default();
                let (status, status_style) = match block.lifecycle {
                    BlockLifecycle::Complete => ("✓", palette.success),
                    BlockLifecycle::Failed => ("✕", palette.error),
                    BlockLifecycle::Cancelled => ("■", palette.warning),
                    BlockLifecycle::Pending | BlockLifecycle::Streaming => ("◌", palette.tool),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {status} "), status_style),
                    Span::styled(truncate(&tool.name), palette.text),
                    Span::styled(duration, palette.muted),
                ]));
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(" No activity yet", palette.muted)));
            }
        }
        InspectorKind::SessionTree => {
            for (index, block) in state.blocks().iter().enumerate() {
                let (label, text) = match block.kind() {
                    BlockKind::User(text) => ("You", text.as_str()),
                    BlockKind::Assistant(text) => ("Slim", text.as_str()),
                    BlockKind::Thinking(text) => ("Thought", text.as_str()),
                    BlockKind::Tool(tool) => ("Tool", tool.name.as_str()),
                    BlockKind::InteractionRequest(_) => ("Input", "request"),
                    BlockKind::System(text) => ("System", text.as_str()),
                    BlockKind::Error(text) => ("Error", text.as_str()),
                    BlockKind::Activity(text) => ("Activity", text.as_str()),
                    BlockKind::QueuedUser(text) => ("Queued", text.as_str()),
                };
                let summary = text.lines().next().unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!(" {:>2} ", index + 1), palette.muted),
                    Span::styled(format!("{label:<8}"), palette.secondary),
                    Span::styled(truncate(summary), palette.text),
                ]));
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(" Empty session", palette.muted)));
            }
        }
        InspectorKind::Diagnostics => {
            let provider = state
                .auth_provider
                .map(|provider| format!("{provider:?}"))
                .unwrap_or_else(|| "none".into());
            let rows = [
                ("provider", provider),
                ("model", state.model.clone()),
                ("mode", mode_name(state.mode).to_owned()),
                ("blocks", state.blocks().len().to_string()),
                (
                    "tools",
                    format!(
                        "read {}/{} · mutate {}/{}",
                        state.tools_used_read,
                        state.max_read_tool_calls,
                        state.tools_used_mutating,
                        state.max_mutating_tool_calls
                    ),
                ),
                ("turns", format!("{}/{}", state.turns_used, state.max_turns)),
                ("context", format_context(state, false)),
                ("latency", format_provider_timings(state)),
            ];
            for (label, value) in rows {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {label:<9}"), palette.secondary),
                    Span::styled(truncate(&value), palette.text),
                ]));
            }
        }
    }
    lines
}

fn format_provider_timings(state: &AppState) -> String {
    let value = |timing: Option<u64>| timing.map_or_else(|| "--".into(), |ms| format!("{ms}ms"));
    format!(
        "hdr {} · byte {} · sem {}",
        value(state.provider_timings.headers_ms),
        value(state.provider_timings.first_byte_ms),
        value(state.provider_timings.first_semantic_ms)
    )
}

fn render_model_overlay(
    frame: &mut ratatui::Frame,
    overlay: &ModelOverlay,
    opencode: &[crate::api::OpenCodeModelView],
    clinepass: &[crate::api::OpenCodeModelView],
    command_code: &[crate::api::OpenCodeModelView],
    palette: &Palette,
) {
    let rows = overlay.rows(opencode, clinepass, command_code);
    let frame_area = frame.area();
    let width = ((u32::from(frame_area.width) * 9) / 10) as u16;
    let width = width.clamp(24, 72).min(frame_area.width);
    let reserved = usize::from(!overlay.filter.is_empty()) + 1;
    let content_height = u16::try_from(rows.len() + reserved + 2).unwrap_or(u16::MAX);
    let height = content_height.clamp(6, frame_area.height.saturating_sub(2).max(6));
    let area = centered(frame_area, width, height);
    let group_title = |index: usize| match index {
        0 => "OpenAI Codex",
        1 => "OpenCode Go",
        2 => "ClinePass",
        _ => "Command Code",
    };
    let mut lines: Vec<Line> = Vec::new();
    if !overlay.filter.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" Filter: {}", sanitize_terminal_text(&overlay.filter)),
            palette.muted,
        )));
    }
    let capacity = (area.height.saturating_sub(2) as usize).saturating_sub(reserved);
    let window = visible_window(
        rows.len(),
        overlay.selected,
        capacity,
        overlay.viewport_start,
    );
    let row_budget = area.width.saturating_sub(4) as usize;
    let include_id = area.width >= 60;
    for index in window {
        let row = &rows[index];
        let selected = index == overlay.selected;
        let marker = if selected { "> " } else { "  " };
        let content = match row {
            ModelRow::Header(group) => {
                let glyph = if overlay.collapsed[*group] {
                    "▸"
                } else {
                    "▾"
                };
                format!("{glyph} {}", group_title(*group))
            }
            ModelRow::Alias(alias) => {
                if include_id {
                    format!("{}  ({})", alias.label(), alias.id())
                } else {
                    alias.label().to_owned()
                }
            }
            ModelRow::Catalog(idx) => opencode
                .get(*idx)
                .map(|model| {
                    if include_id {
                        format!("{}  ({})", model.name, model.id)
                    } else {
                        model.name.clone()
                    }
                })
                .unwrap_or_default(),
            ModelRow::ClinePass(idx) => clinepass
                .get(*idx)
                .map(|model| {
                    if include_id {
                        format!("{}  ({})", model.name, model.id)
                    } else {
                        model.name.clone()
                    }
                })
                .unwrap_or_default(),
            ModelRow::CommandCode(idx) => command_code
                .get(*idx)
                .map(|model| {
                    if include_id {
                        format!("{}  ({})", model.name, model.id)
                    } else {
                        model.name.clone()
                    }
                })
                .unwrap_or_default(),
        };
        let line = format!("{marker}{}", truncate_cells(&content, row_budget));
        let style = if selected {
            palette.accent
        } else {
            palette.text
        };
        lines.push(Line::from(Span::styled(
            sanitize_terminal_text(&line),
            style,
        )));
    }
    while lines.len() < reserved.saturating_add(capacity).saturating_sub(1) {
        lines.push(Line::default());
    }
    let position = if rows.is_empty() {
        "0/0".to_owned()
    } else {
        format!("{}/{}", overlay.selected.saturating_add(1), rows.len())
    };
    let footer = truncate_cells(
        &format!(" {position} · Space fold · Enter select · Esc cancel"),
        area.width.saturating_sub(2) as usize,
    );
    lines.push(Line::from(Span::styled(footer, palette.muted)));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Select model ", palette)),
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
            let marker = if index == overlay.selected { ">" } else { " " };
            format!("{marker} {:<6}  {}", effort.label(), effort.description())
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(format!("\n{rows}\n\n Enter select · Esc back"))
            .block(modal_block(" Select reasoning effort ", palette)),
        area,
    );
}

fn render_login_overlay(frame: &mut ratatui::Frame, overlay: &LoginOverlay, palette: &Palette) {
    if let LoginStage::ApiKey(key) = &overlay.stage {
        let title = match overlay.provider() {
            LoginProvider::OpenCodeGo => " OpenCode Go API key ",
            LoginProvider::ClinePass => " ClinePass API key ",
            LoginProvider::CommandCode => " Command Code API key ",
            LoginProvider::Anthropic => " Anthropic API key ",
            LoginProvider::OpenAiCodex => " OpenAI Codex API key ",
        };
        let mask = if key.is_empty() {
            String::new()
        } else {
            "•".repeat(key.char_len().max(4))
        };
        let area = centered(frame.area(), 58, 8);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(format!("\n {mask}\n\n Enter save · Esc back"))
                .block(modal_block(title, palette)),
            area,
        );
        return;
    }
    let providers = [
        LoginProvider::Anthropic,
        LoginProvider::OpenAiCodex,
        LoginProvider::OpenCodeGo,
        LoginProvider::ClinePass,
        LoginProvider::CommandCode,
    ];
    let mut text = String::from("\n");
    for (index, provider) in providers.iter().enumerate() {
        let marker = if overlay.selected == index { ">" } else { " " };
        text.push_str(&format!("{marker} {}\n\n", provider.label()));
    }
    if let Some(code) = &overlay.user_code {
        text.push_str(&format!(
            " Code: {}\n\n Esc cancel · Ctrl+C cancel",
            sanitize_terminal_text(code.expose())
        ));
    } else if let Some(progress) = &overlay.progress {
        text.push_str(&format!(
            " {}\n\n Esc cancel · Ctrl+C cancel",
            sanitize_terminal_text(progress)
        ));
    } else if let Some(url) = &overlay.auth_url {
        text.push_str(&format!(
            " Open: {}\n\n Esc cancel · Ctrl+C cancel",
            sanitize_terminal_text(url.expose())
        ));
    } else {
        text.push_str(" Enter connect · Esc cancel");
    }
    let frame_area = frame.area();
    let content_rows = text.lines().count() as u16;
    let height = content_rows
        .saturating_add(2)
        .clamp(8, frame_area.height.saturating_sub(2).max(8));
    let area = centered(frame_area, 58, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).block(modal_block(" Connect provider ", palette)),
        area,
    );
}

/// Key hint pinned to the bottom of the slash popup (§17.2): the same
/// vocabulary the model overlay and search bar already use.
const SLASH_POPUP_HINT: &str = "Tab complete · Enter run · Esc";

fn render_slash_popup(
    frame: &mut ratatui::Frame,
    composer_area: ratatui::layout::Rect,
    suggestions: &SlashSuggestions,
    matches: &[String],
    palette: &Palette,
) {
    if matches.is_empty() || composer_area.y < 3 {
        return;
    }
    // One row is reserved for the key hint below the commands.
    let capacity =
        usize::from(composer_area.y.saturating_sub(3).max(1)).min(PICKER_NOMINAL_CAPACITY);
    let visible = visible_window(matches.len(), suggestions.selected, capacity, 0);
    let mut rows: Vec<Line> = matches[visible.clone()]
        .iter()
        .enumerate()
        .map(|(offset, command)| {
            let selected = visible.start + offset == suggestions.selected;
            let marker = if selected { "> " } else { "  " };
            let style = if selected {
                palette.accent
            } else {
                palette.text
            };
            Line::from(Span::styled(format!("{marker}{command}"), style))
        })
        .collect();
    let width = 32.min(composer_area.width);
    rows.push(Line::from(Span::styled(
        truncate_cells(SLASH_POPUP_HINT, width.saturating_sub(2) as usize),
        palette.muted,
    )));
    let height = (rows.len() as u16 + 2).min(composer_area.y);
    let area = ratatui::layout::Rect {
        x: composer_area.x,
        y: composer_area.y - height,
        width,
        height,
    };
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

fn render_palette(
    frame: &mut ratatui::Frame,
    query: &str,
    selected: usize,
    _viewport_start: usize,
    palette: &Palette,
) {
    let matches = crate::reducer::palette_matches(query);
    let rows = grouped_command_lines(&matches, Some(selected), palette);
    let height = (rows.len() as u16 + 4).clamp(6, 16);
    let area = centered(frame.area(), 42, height);
    let query_row = if query.is_empty() {
        Span::styled(" Type to filter…", palette.muted)
    } else {
        Span::styled(format!(" {}", sanitize_terminal_text(query)), palette.text)
    };
    let mut lines = vec![Line::from(query_row)];
    lines.extend(rows);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Commands ", palette)),
        area,
    );
}

fn spinner_glyph(frame: u64, capabilities: Capabilities) -> char {
    const SPINNER: [char; 4] = ['\u{25d2}', '\u{25d3}', '\u{25d1}', '\u{25d0}'];
    if capabilities.reduced_motion {
        glyph(capabilities, '\u{25cc}', '~')
    } else if capabilities.color_depth == ColorDepth::None {
        '~'
    } else {
        SPINNER[(frame % SPINNER.len() as u64) as usize]
    }
}

fn palette_command_line(command: &str, selected_here: bool, palette: &Palette) -> Line<'static> {
    const MODAL_INNER_WIDTH: usize = 40; // modal width 42 minus borders
    let marker = if selected_here { "> " } else { "  " };
    let style = if selected_here {
        palette.accent
    } else {
        palette.text
    };
    let head = format!("{marker}{command}");
    let detail = crate::reducer::palette_description(command);
    if detail.is_empty() {
        return Line::from(Span::styled(head, style));
    }
    let budget = MODAL_INNER_WIDTH.saturating_sub(UnicodeWidthStr::width(head.as_str()) + 2);
    Line::from(vec![
        Span::styled(head, style),
        Span::raw("  "),
        Span::styled(truncate_cells(detail, budget), palette.muted),
    ])
}

fn grouped_command_lines(
    commands: &[&str],
    selected: Option<usize>,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let selected_command = selected.and_then(|index| commands.get(index)).copied();
    for (group, members) in crate::reducer::COMMAND_GROUPS {
        let visible: Vec<&str> = members
            .iter()
            .copied()
            .filter(|command| commands.contains(command))
            .collect();
        if visible.is_empty() {
            continue;
        }
        lines.push(Line::from(Span::styled((*group).to_owned(), palette.muted)));
        for command in visible {
            lines.push(palette_command_line(
                command,
                selected_command == Some(command),
                palette,
            ));
        }
    }
    let ungrouped = commands
        .iter()
        .copied()
        .filter(|command| {
            !crate::reducer::COMMAND_GROUPS
                .iter()
                .any(|(_, members)| members.contains(command))
        })
        .collect::<Vec<_>>();
    if !ungrouped.is_empty() {
        lines.push(Line::from(Span::styled("skills", palette.muted)));
        for command in ungrouped {
            lines.push(palette_command_line(
                command,
                selected_command == Some(command),
                palette,
            ));
        }
    }
    lines
}

fn modal_block<'a>(title: &'a str, palette: &'a Palette) -> RatatuiBlock<'a> {
    RatatuiBlock::default()
        .title(title)
        .title_style(palette.accent_bold)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
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

fn content_column(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    centered(area, area.width.min(MAX_WORKSPACE_WIDTH), area.height)
}

fn horizontal_inset(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    if area.width <= 2 {
        return area;
    }
    ratatui::layout::Rect {
        x: area.x + 1,
        width: area.width - 2,
        ..area
    }
}

#[cfg(test)]
mod tests {
    use super::{
        control_requires_data_barrier, measure_scrollback, receive_batch, terminal_action,
        toast_row_count, update_visible_stream_state, LaneDrain, CONTROL_BATCH_LIMIT,
        STREAM_BATCH_LIMIT,
    };
    use crate::api::{ToolBatchId, ToolCallId, UiEvent};
    use crate::app::{AppState, FrameClock, SlashSuggestions};
    use crate::reducer::{reduce, Action, ScrollIntent};
    use crate::render::WrapCache;
    use crate::runtime::render_frame;
    use crate::theme::{Capabilities, ColorDepth};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::mpsc;

    fn caps() -> Capabilities {
        Capabilities {
            color_depth: ColorDepth::TrueColor,
            mouse: false,
            clipboard: false,
            images: false,
            reduced_motion: false,
        }
    }

    #[test]
    fn slash_popup_keeps_a_late_selection_visible_in_a_small_viewport() {
        let mut state = AppState::new();
        state.set_skill_names_for_test((0..20).map(|index| format!("skill-{index:02}")).collect());
        state.slash_suggestions = Some(SlashSuggestions {
            query: "skill".into(),
            selected: 15,
        });

        let frame = render_to_string(&state, 32, 10);

        assert!(frame.contains("/skill-15"), "{frame}");
        assert!(!frame.contains("/skill-00"), "{frame}");
    }

    fn render_to_string(state: &AppState, width: u16, height: u16) -> String {
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
    fn migrated_causal_suffix_and_cancel_wait_for_queued_stream_data() {
        let tool_ended = UiEvent::ToolEnded {
            batch_id: ToolBatchId("batch-1".into()),
            call_id: ToolCallId("call-1".into()),
            name: "read".into(),
            success: false,
            duration_ms: 1,
        };
        assert!(control_requires_data_barrier(&[tool_ended]));
        assert!(control_requires_data_barrier(&[UiEvent::RunCancelled {
            run_id: 1,
        }]));
        assert!(!control_requires_data_barrier(&[
            UiEvent::AuthStateChanged {
                provider: None,
                authenticated: false,
            },
        ]));
        let mut visible_stream_started = true;
        update_visible_stream_state(
            &UiEvent::RunStarted {
                run_id: 2,
                max_mutating_tool_calls: 1,
                max_read_tool_calls: 1,
                max_turns: 1,
            },
            &mut visible_stream_started,
        );
        assert!(!visible_stream_started);
    }

    #[test]
    fn question_arrows_are_routed_to_reducer_instead_of_scrollback() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::QuestionRequired {
            request_id: crate::api::InteractionRequestId("question-1".into()),
            question: "Which crate?".into(),
            options: vec![
                slim_core::QuestionOption {
                    label: "core".into(),
                    description: String::new(),
                },
                slim_core::QuestionOption {
                    label: "tui".into(),
                    description: String::new(),
                },
            ],
            persisted: false,
        });
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        let mut cache = WrapCache::default();
        let action = terminal_action(
            crossterm::event::Event::Key(key),
            &state,
            (80, 24),
            &mut cache,
        )
        .expect("action");
        assert!(
            matches!(
                action,
                Action::Key(event) if event.code == crossterm::event::KeyCode::Down
            ),
            "pending question must capture arrows: {action:?}"
        );
        reduce(&mut state, action);
        assert_eq!(
            state
                .pending_interaction()
                .map(|interaction| interaction.selected_question_option),
            Some(1)
        );
    }

    #[test]
    fn toasts_occupy_one_row_each_without_joining() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::UserMessageAdded {
            text: "prompt".into(),
        });
        state.notifications.push("first notice".into());
        state.notifications.push("second notice".into());
        let frame = render_to_string(&state, 80, 24);
        assert!(
            !frame.contains("first notice· second notice")
                && !frame.contains("first notice · second notice"),
            "toasts must not join on one row:\n{frame}"
        );
        let lines: Vec<&str> = frame.lines().collect();
        let first = lines
            .iter()
            .position(|line| line.contains("first notice"))
            .expect("first toast");
        let second = lines
            .iter()
            .position(|line| line.contains("second notice"))
            .expect("second toast");
        assert_ne!(first, second, "each toast occupies its own row:\n{frame}");
        assert_eq!(toast_row_count(&state, 24), 2);
    }

    #[test]
    fn wide_activity_rail_includes_turn_and_this_turn_tool_budgets() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::run_started_with_budget(1, 32, 96, 128));
        state.apply_event(UiEvent::UsageEstimateForRun {
            run_id: 1,
            request_id: 1,
            context_tokens: 100,
            context_window_tokens: 128_000,
        });
        let wide = render_to_string(&state, 80, 24);
        assert!(wide.contains("turn 1/128"), "{wide}");
        assert!(wide.contains("reads 0/96"), "{wide}");
        assert!(wide.contains("edits 0/32"), "{wide}");
        let narrow = render_to_string(&state, 71, 24);
        assert!(!narrow.contains("turn 1/128"), "{narrow}");
    }

    #[test]
    fn idle_down_becomes_scroll_without_an_overlay() {
        let state = AppState::new();
        let mut cache = WrapCache::default();
        let action = terminal_action(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            )),
            &state,
            (80, 24),
            &mut cache,
        )
        .expect("action");
        assert!(matches!(
            action,
            Action::Scroll {
                intent: ScrollIntent::Down,
                ..
            }
        ));
        let _ = measure_scrollback(&state, 80, 24, &mut cache);
        let _ = FrameClock::default();
    }

    fn pinned_state_with_draft() -> AppState {
        let mut state = AppState::new();
        state.apply_event(UiEvent::UserMessageAdded {
            text: "something to read back".into(),
        });
        let anchor = crate::app::ScrollAnchor {
            block_id: state.blocks().last().expect("block").id.clone(),
            row_offset: 0,
        };
        state.scroll.mode = crate::app::FollowMode::Pinned(anchor);
        state.composer.insert_text("half-typed draft");
        state
    }

    fn key_action(state: &AppState, code: crossterm::event::KeyCode) -> Action {
        let mut cache = WrapCache::default();
        terminal_action(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )),
            state,
            (80, 24),
            &mut cache,
        )
        .expect("action")
    }

    #[test]
    fn end_returns_to_live_edge_while_pinned_with_nonempty_draft() {
        let state = pinned_state_with_draft();
        assert!(state.scroll.is_pinned());
        let action = key_action(&state, crossterm::event::KeyCode::End);
        assert!(
            matches!(
                action,
                Action::Scroll {
                    intent: ScrollIntent::LiveEdge,
                    ..
                }
            ),
            "pinned End must honor the footer promise: {action:?}"
        );
    }

    #[test]
    fn home_returns_to_top_while_pinned_with_nonempty_draft() {
        let state = pinned_state_with_draft();
        let action = key_action(&state, crossterm::event::KeyCode::Home);
        assert!(
            matches!(
                action,
                Action::Scroll {
                    intent: ScrollIntent::Top,
                    ..
                }
            ),
            "pinned Home must navigate: {action:?}"
        );
    }

    #[test]
    fn end_moves_cursor_while_typing_at_live_edge() {
        let mut state = AppState::new();
        state.composer.insert_text("half-typed draft");
        assert!(state.scroll.is_live_edge());
        let action = key_action(&state, crossterm::event::KeyCode::End);
        assert!(
            matches!(action, Action::Key(_)),
            "live-edge End must keep editing the cursor: {action:?}"
        );
    }

    #[test]
    fn control_lane_yields_after_its_batch_budget() {
        let (sender, receiver) = mpsc::channel();
        for index in 0..=CONTROL_BATCH_LIMIT {
            sender
                .send(UiEvent::Notification {
                    message: index.to_string(),
                })
                .expect("enqueue control event");
        }

        let (batch, state) = receive_batch(&receiver, CONTROL_BATCH_LIMIT);

        assert_eq!(batch.len(), CONTROL_BATCH_LIMIT);
        assert_eq!(state, LaneDrain::Exhausted);
        assert!(receiver.try_recv().is_ok(), "next event must remain queued");
    }

    #[test]
    fn stream_lane_yields_after_its_batch_budget() {
        let (sender, receiver) = mpsc::channel();
        for index in 0..=STREAM_BATCH_LIMIT {
            sender
                .send(UiEvent::AssistantDelta {
                    text: index.to_string(),
                })
                .expect("enqueue stream event");
        }

        let (batch, state) = receive_batch(&receiver, STREAM_BATCH_LIMIT);

        assert_eq!(batch.len(), STREAM_BATCH_LIMIT);
        assert_eq!(state, LaneDrain::Exhausted);
        assert!(receiver.try_recv().is_ok(), "next event must remain queued");
    }
}
