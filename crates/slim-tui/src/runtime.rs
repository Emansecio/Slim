use std::io;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    read, Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use slim_core::runtime::mode_name;

use crate::api::{
    BlockId, LoginProvider, McpServerView, McpStatusView, ReasoningEffort, TodoItemStatus,
    UiChannels, UiCommand,
};
use crate::app::{
    ActivityPhase, AppState, EffortOverlay, LoginOverlay, LoginStage, McpOverlay, ModelOverlay,
    ModelRow, SlashSuggestions, INFO_TOAST_TTL_MS,
};
use crate::block::{
    question_option_marker, wrap_words, Block, BlockKind, BlockLifecycle, FoldState,
    InteractionRequestKind, InteractionRequestState,
};
use crate::fullscreen::FullscreenBackend;
use crate::inspector::{is_mutating_tool, InspectorKind};
use crate::layout::{plan_with_session_rail_and_composer, todo_height, Rect};
use crate::markdown::{render_plain, sanitize_terminal_text, MarkdownStyles};
use crate::picker::{truncate_cells, visible_window, PICKER_NOMINAL_CAPACITY};
use crate::reducer::{reduce, Action, Effect, ScrollIntent};
use crate::render::{
    cached_lines_bytes, thinking_body_width, thinking_preview_tail, user_prompt_text_width,
    BodyKind, EventCoalescer, InspectorPaletteKey, ScrollMetrics, WrapCache,
};
use crate::runtime_wait::{
    next_visual_deadline, runtime_clock, wait_for_runtime_signal, WaitOutcome,
};
use crate::theme::{
    detect_capabilities, glyph, resolve_theme, to_terminal_color, Capabilities, ColorDepth,
    MENU_SELECTION_BG,
};
use crate::view_model::{
    activity_elapsed, activity_label, assistant_label, budget_near_limit, completed_tool_phrase,
    display_cwd, format_context, is_trivial_cwd, run_status_label, session_rail_projection,
    truncate_display_width,
};

/// Composer prompt is ASCII on purpose (G264): `›` (U+203A) is East-Asian
/// Ambiguous and Windows consoles advance two cells while Ratatui counts one,
/// which parks the caret on the last typed letter.
const COMPOSER_PROMPT: &str = "> ";
const COMPOSER_OVERFLOW_HINT: &str = "<";
const CONTROL_BATCH_LIMIT: usize = 32;
const STREAM_BATCH_LIMIT: usize = 1_024;
const DOCKED_INSPECTOR_MIN_WIDTH: u16 = 100;
const INFO_TOAST_HIGHLIGHT_MS: u64 = 249;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaneDrain {
    Open,
    Exhausted,
    Closed,
}

fn receive_batch(
    receiver: &Receiver<crate::api::UiEvent>,
    limit: usize,
    prefetched: &mut Vec<crate::api::UiEvent>,
) -> (Vec<crate::api::UiEvent>, LaneDrain) {
    // `prefetched` holds events popped while probing before the wait; they go
    // first so per-lane FIFO order is preserved.
    let mut events = std::mem::take(prefetched);
    while events.len() < limit {
        match receiver.try_recv() {
            Ok(event) => events.push(event),
            Err(TryRecvError::Empty) => return (events, LaneDrain::Open),
            Err(TryRecvError::Disconnected) => return (events, LaneDrain::Closed),
        }
    }
    (events, LaneDrain::Exhausted)
}

/// Pulls one already-queued event into `prefetched`; used to probe a lane
/// without sleeping between the last drain and the wait.
fn prefetch_event(
    receiver: &Receiver<crate::api::UiEvent>,
    prefetched: &mut Vec<crate::api::UiEvent>,
) -> bool {
    match receiver.try_recv() {
        Ok(event) => {
            prefetched.push(event);
            true
        }
        Err(_) => false,
    }
}

/// Clipboard work runs off the UI thread (§20): image pulls can take hundreds
/// of milliseconds between OLE access, DIB→PNG encode and temp-file writes.
enum ClipboardOutcome {
    Pulled(Result<crate::clipboard::ClipboardContent, String>),
    Copied(bool),
}

pub fn run_app(channels: UiChannels) -> io::Result<()> {
    let capabilities = detect_capabilities();
    let mut backend = FullscreenBackend::start(capabilities)?;
    let result = run_loop(&mut backend, &channels, capabilities);
    let _ = channels.commands.send(UiCommand::Shutdown);
    let shutdown = backend.shutdown();
    result.and(shutdown)
}

fn run_loop(
    backend: &mut FullscreenBackend,
    channels: &UiChannels,
    capabilities: Capabilities,
) -> io::Result<()> {
    let mut state = AppState::new();
    let mut dirty = true;
    let started = Instant::now();
    let mut last_motion_frame = 0;
    let mut last_status_second = 0;
    let mut control_closed = false;
    let mut stream_closed = false;
    let mut render_cache = WrapCache::default();
    let mut coalescer = EventCoalescer::new(1024, Duration::from_millis(16));
    let mut visible_stream_started = false;
    let (clipboard_tx, clipboard_rx) = std::sync::mpsc::channel::<ClipboardOutcome>();
    let mut control_prefetched = Vec::new();
    let mut stream_prefetched = Vec::new();
    // Terminal size is only re-queried on resize events; the query is an OS
    // call on Windows.
    let mut size = backend.terminal().size()?;
    loop {
        let clock = runtime_clock(started.elapsed());
        let effects = reduce(&mut state, Action::SyncClock(clock));
        run_effects(channels, &clipboard_tx, effects)?;

        // Clipboard jobs report back through the wake signal like lane events.
        while let Ok(outcome) = clipboard_rx.try_recv() {
            let action = match outcome {
                ClipboardOutcome::Pulled(Ok(content)) => Action::ClipboardPull {
                    image: content.image_path,
                    text: content.text,
                },
                ClipboardOutcome::Pulled(Err(message)) => Action::ClipboardImageFailed {
                    message: format!("Clipboard image: {message}"),
                },
                ClipboardOutcome::Copied(success) => Action::ClipboardCompleted { success },
            };
            let effects = reduce(&mut state, action);
            run_effects(channels, &clipboard_tx, effects)?;
            dirty = true;
        }

        let (control_events, control_drain) = if control_closed {
            (Vec::new(), LaneDrain::Closed)
        } else {
            receive_batch(
                &channels.events,
                CONTROL_BATCH_LIMIT,
                &mut control_prefetched,
            )
        };
        let (stream_events, stream_drain) = if stream_closed {
            (Vec::new(), LaneDrain::Closed)
        } else {
            receive_batch(
                &channels.events_data,
                STREAM_BATCH_LIMIT,
                &mut stream_prefetched,
            )
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
                &clipboard_tx,
                stream_events.take().expect("stream batch"),
                true,
            )?;
        }
        if !control_events.is_empty() && !coalescer.is_empty() {
            for event in coalescer.flush() {
                let effects = reduce(&mut state, Action::UiEventReceived(event));
                run_effects(channels, &clipboard_tx, effects)?;
                dirty = true;
            }
        }
        channels.lane_space.notify();
        for event in control_events {
            update_visible_stream_state(&event, &mut visible_stream_started);
            let effects = reduce(&mut state, Action::UiEventReceived(event));
            run_effects(channels, &clipboard_tx, effects)?;
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
                &clipboard_tx,
                stream_events,
                false,
            )?;
        }
        stream_closed = stream_drain == LaneDrain::Closed;
        if stream_closed || coalescer.window_elapsed() {
            for event in coalescer.flush() {
                let effects = reduce(&mut state, Action::UiEventReceived(event));
                run_effects(channels, &clipboard_tx, effects)?;
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
            // Extract from the painted frame. After Terminal::draw swaps
            // buffers, current_buffer_mut is the reset back buffer.
            state.selection_text = draw_state(backend, &state, capabilities, &mut render_cache)?;
            // Event-driven draws already paint the current animation/status
            // clock. Do not immediately request the same frame via a deadline.
            last_motion_frame = state.clock.frame;
            last_status_second = state.clock.elapsed_ms / 1_000;
            dirty = false;
        }
        if control_closed && stream_closed {
            return Ok(());
        }

        let regions = plan_regions(&state, size.width, size.height, &mut render_cache);
        let motion_visible = regions.activity_rail.height > 0
            && motion_needed(&state, capabilities, &mut render_cache);
        let status_visible = regions.activity_rail.height > 0
            && !navigation_captured(&state)
            && state.inspector.active.is_none();
        let toast_count = toast_row_count(&state, regions.scrollback.height);
        let elapsed = started.elapsed();
        let next_toast_deadline_ms = next_toast_visual_deadline_ms(
            &state,
            runtime_clock(elapsed).elapsed_ms,
            toast_count,
            !capabilities.reduced_motion,
        );
        let visual_deadline = next_visual_deadline(
            elapsed,
            last_motion_frame,
            last_status_second,
            motion_visible,
            status_visible,
            next_toast_deadline_ms,
        );
        let deadline = [visual_deadline, coalescer.time_until_flush()]
            .into_iter()
            .flatten()
            .min();
        // Arming before the probes closes the race between the last drain and
        // the wait: producers only signal while a waiter is armed, so the
        // prefetch/`poll(0)` checks are the single place that decides.
        channels.wake.arm();
        let outcome = if prefetch_event(&channels.events, &mut control_prefetched)
            | prefetch_event(&channels.events_data, &mut stream_prefetched)
        {
            WaitOutcome::Wake
        } else if crossterm::event::poll(Duration::ZERO)? {
            WaitOutcome::Input
        } else {
            wait_for_runtime_signal(&channels.wake, deadline)?
        };
        channels.wake.disarm();
        match outcome {
            WaitOutcome::Wake => {}
            WaitOutcome::Input => {
                // Drain the reader's buffered events: crossterm can read ahead,
                // which would otherwise strand input until the next OS signal.
                let mut handled = 0usize;
                loop {
                    let event = read()?;
                    if let Event::Resize(width, height) = event {
                        size = ratatui::layout::Size { width, height };
                    }
                    // Input is drained in batches. A drag and copy can arrive
                    // before the next normal draw, so materialize the latest
                    // bounded selection before the reducer consumes its text.
                    if dirty && state.selection.is_some() && is_selection_copy_event(&event) {
                        state.selection_text =
                            draw_state(backend, &state, capabilities, &mut render_cache)?;
                        last_motion_frame = state.clock.frame;
                        last_status_second = state.clock.elapsed_ms / 1_000;
                        dirty = false;
                    }
                    if let Some(action) =
                        terminal_action(event, &state, (size.width, size.height), &mut render_cache)
                    {
                        let effects = reduce(&mut state, action);
                        run_effects(channels, &clipboard_tx, effects)?;
                        dirty = true;
                    }
                    handled += 1;
                    if handled >= 64 || !crossterm::event::poll(Duration::ZERO)? {
                        break;
                    }
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
                    run_effects(channels, &clipboard_tx, effects)?;
                    dirty = true;
                } else if next_toast_deadline_ms
                    .is_some_and(|deadline_ms| clock.elapsed_ms >= deadline_ms)
                {
                    let effects = reduce(&mut state, Action::SyncClock(clock));
                    run_effects(channels, &clipboard_tx, effects)?;
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

#[allow(clippy::too_many_arguments)]
fn reduce_stream_events(
    channels: &UiChannels,
    state: &mut AppState,
    coalescer: &mut EventCoalescer,
    visible_stream_started: &mut bool,
    dirty: &mut bool,
    clipboard_tx: &std::sync::mpsc::Sender<ClipboardOutcome>,
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
            run_effects(channels, clipboard_tx, effects)?;
            *dirty = true;
        }
        if first_visible {
            *visible_stream_started = true;
            for ready in coalescer.flush() {
                let effects = reduce(state, Action::UiEventReceived(ready));
                run_effects(channels, clipboard_tx, effects)?;
                *dirty = true;
            }
        }
    }
    if force_flush {
        for event in coalescer.flush() {
            let effects = reduce(state, Action::UiEventReceived(event));
            run_effects(channels, clipboard_tx, effects)?;
            *dirty = true;
        }
    }
    Ok(())
}

/// Executes one action's effects. Clipboard IO is dispatched to a worker
/// thread; the outcome lands on the `clipboard` channel and wakes the loop
/// like any lane event, so a slow image pull no longer blocks the UI.
fn run_effects(
    channels: &UiChannels,
    clipboard: &std::sync::mpsc::Sender<ClipboardOutcome>,
    effects: Vec<Effect>,
) -> io::Result<()> {
    for effect in effects {
        match effect {
            Effect::Send(command) => send(&channels.commands, command)?,
            Effect::CopyToClipboard(text) => {
                let sink = clipboard.clone();
                let wake = channels.wake.clone();
                std::thread::spawn(move || {
                    let success = crate::clipboard::copy_text(&text).is_ok();
                    let _ = sink.send(ClipboardOutcome::Copied(success));
                    wake.notify();
                });
            }
            Effect::PasteFromClipboard => {
                let sink = clipboard.clone();
                let wake = channels.wake.clone();
                std::thread::spawn(move || {
                    let outcome = ClipboardOutcome::Pulled(crate::clipboard::pull());
                    let _ = sink.send(outcome);
                    wake.notify();
                });
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
        || state.mcp_overlay.is_some()
        || state.palette_query.is_some()
        || state.search.is_some()
        || state.slash_suggestions.is_some()
        || state.inspector.active.is_some()
        || state.pending_interaction().is_some()
}

/// Maps terminal events onto reducer Actions. Overlays, slash, palette,
/// inspectors and a pending structured question keep arrows; otherwise they
/// become transcript scroll.
pub fn terminal_action(
    event: Event,
    state: &AppState,
    size: (u16, u16),
    cache: &mut WrapCache,
) -> Option<Action> {
    match event {
        Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
            if inspector_has_keyboard_focus(state) {
                if let Some(intent) = inspector_scroll_intent(key) {
                    if let Some(area) = inspector_panel_area_for_size(state, size, cache) {
                        let palette = Palette::of(Capabilities {
                            color_depth: ColorDepth::None,
                            mouse: false,
                            clipboard: false,
                            images: false,
                            reduced_motion: false,
                        });
                        let metrics = inspector_panel_metrics(
                            state,
                            state.inspector.active,
                            area,
                            &palette,
                            cache,
                            false,
                        );
                        return Some(Action::InspectorScroll {
                            intent,
                            total_rows: metrics.total_rows,
                            capacity: metrics.capacity,
                        });
                    }
                }
            }
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
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            // A visible modal owns the pointer just as it owns the keyboard;
            // never move the transcript behind a menu or prompt.
            if mouse_navigation_captured(state) {
                return None;
            }
            let intent = if mouse.kind == MouseEventKind::ScrollUp {
                ScrollIntent::Up
            } else {
                ScrollIntent::Down
            };
            if let Some(area) = inspector_panel_area_for_size(state, size, cache)
                .filter(|area| area_contains(*area, mouse.column, mouse.row))
            {
                let palette = Palette::of(Capabilities {
                    color_depth: ColorDepth::None,
                    mouse: false,
                    clipboard: false,
                    images: false,
                    reduced_motion: false,
                });
                let metrics = inspector_panel_metrics(
                    state,
                    state.inspector.active,
                    area,
                    &palette,
                    cache,
                    false,
                );
                return Some(Action::InspectorScroll {
                    intent,
                    total_rows: metrics.total_rows,
                    capacity: metrics.capacity,
                });
            }
            Some(Action::Scroll {
                intent,
                metrics: measure_scrollback(state, size.0, size.1, cache),
            })
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let area = (!mouse_navigation_captured(state))
                .then(|| {
                    cache
                        .selection_regions
                        .iter()
                        .rev()
                        .flatten()
                        .copied()
                        .find(|area| area_contains(*area, mouse.column, mouse.row))
                })
                .flatten();
            Some(Action::StartScreenSelection {
                x: mouse.column,
                y: mouse.row,
                area,
            })
        }
        MouseEventKind::Drag(MouseButton::Left) => Some(Action::UpdateScreenSelection {
            x: mouse.column,
            y: mouse.row,
        }),
        MouseEventKind::Up(MouseButton::Left) => Some(Action::FinishScreenSelection),
        MouseEventKind::Down(MouseButton::Right) => Some(Action::MouseSecondary),
        MouseEventKind::Down(MouseButton::Middle) => Some(Action::RequestClipboardPaste),
        _ => None,
    }
}

fn is_selection_copy_event(event: &Event) -> bool {
    matches!(event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Right))
        || matches!(event, Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press
            && key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
}

fn mouse_navigation_captured(state: &AppState) -> bool {
    state.login_overlay.is_some()
        || state.model_overlay.is_some()
        || state.effort_overlay.is_some()
        || state.mcp_overlay.is_some()
        || state.palette_query.is_some()
        || state.search.is_some()
        || state.slash_suggestions.is_some()
        || state.pending_interaction().is_some()
}

fn area_contains(area: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn selection_area(state: &AppState, cache: &WrapCache) -> Option<ratatui::layout::Rect> {
    let area = state.selection_area?;
    (!mouse_navigation_captured(state) && cache.selection_regions.contains(&Some(area)))
        .then_some(area)
}

fn extract_visible_selection(
    frame: &mut ratatui::Frame,
    state: &AppState,
    cache: &WrapCache,
) -> String {
    state
        .selection
        .zip(selection_area(state, cache))
        .map(|(selection, area)| {
            crate::selection::extract_selected_text(frame.buffer_mut(), selection, area)
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkspaceRegions {
    transcript: ratatui::layout::Rect,
    inspector: Option<ratatui::layout::Rect>,
}

fn inspector_has_keyboard_focus(state: &AppState) -> bool {
    state.inspector.active.is_some()
        && state.login_overlay.is_none()
        && state.model_overlay.is_none()
        && state.effort_overlay.is_none()
        && state.mcp_overlay.is_none()
        && state.palette_query.is_none()
        && state.search.is_none()
        && state.slash_suggestions.is_none()
        && state.pending_interaction().is_none()
}

fn inspector_scroll_intent(key: crossterm::event::KeyEvent) -> Option<ScrollIntent> {
    if key.modifiers != KeyModifiers::NONE {
        return None;
    }
    match key.code {
        KeyCode::Up => Some(ScrollIntent::Up),
        KeyCode::Down => Some(ScrollIntent::Down),
        KeyCode::PageUp => Some(ScrollIntent::PageUp),
        KeyCode::PageDown => Some(ScrollIntent::PageDown),
        KeyCode::Home => Some(ScrollIntent::Top),
        KeyCode::End => Some(ScrollIntent::LiveEdge),
        _ => None,
    }
}

fn inspector_overlay_area(frame_area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let width = ((u32::from(frame_area.width) * 9) / 10) as u16;
    let height = ((u32::from(frame_area.height) * 7) / 10) as u16;
    centered(frame_area, width.max(12), height.max(4))
}

fn inspector_panel_area_for_size(
    state: &AppState,
    size: (u16, u16),
    cache: &mut WrapCache,
) -> Option<ratatui::layout::Rect> {
    state.inspector.active?;
    let frame_area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: size.0,
        height: size.1,
    };
    let regions = plan_regions(state, size.0, size.1, cache);
    let scrollback = to_ratatui(regions.scrollback);
    let workspace = workspace_regions(state, scrollback);
    Some(
        workspace
            .inspector
            .unwrap_or_else(|| inspector_overlay_area(frame_area)),
    )
}

struct InspectorPanelMetrics {
    inner: ratatui::layout::Rect,
    lines: Arc<Vec<Line<'static>>>,
    total_rows: usize,
    capacity: usize,
    start: usize,
    end: usize,
}

fn inspector_inner_area(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn inspector_palette_key(palette: &Palette) -> InspectorPaletteKey {
    InspectorPaletteKey::new([
        palette.muted,
        palette.secondary,
        palette.text,
        palette.warning,
        palette.success,
        palette.error,
        palette.tool,
    ])
}

fn inspector_panel_metrics(
    state: &AppState,
    kind: Option<InspectorKind>,
    area: ratatui::layout::Rect,
    palette: &Palette,
    cache: &mut WrapCache,
    paint: bool,
) -> InspectorPanelMetrics {
    let inner = inspector_inner_area(area);
    let palette_key = inspector_palette_key(palette);
    let lines = if paint {
        cache.inspector_lines(state, kind, palette, inner.width, palette_key)
    } else {
        cache.inspector_line_metrics(state, kind, palette, inner.width, palette_key)
    };
    let total_rows = lines.len();
    let overflow = total_rows > usize::from(inner.height);
    let capacity = usize::from(inner.height)
        .saturating_sub(usize::from(overflow))
        .max(1);
    let start = state.inspector.scroll.start(total_rows, capacity);
    let end = start.saturating_add(capacity).min(total_rows);
    InspectorPanelMetrics {
        inner,
        lines,
        total_rows,
        capacity,
        start,
        end,
    }
}

fn workspace_regions(state: &AppState, scrollback: ratatui::layout::Rect) -> WorkspaceRegions {
    let inspector_visible =
        state.inspector.active.is_some() && scrollback.width >= DOCKED_INSPECTOR_MIN_WIDTH;
    if !inspector_visible {
        return WorkspaceRegions {
            transcript: scrollback,
            inspector: None,
        };
    }
    let inspector_width = (((u32::from(scrollback.width) * 38) / 100) as u16)
        .clamp(34, 52)
        .min(scrollback.width.saturating_sub(40));
    if inspector_width == 0 {
        return WorkspaceRegions {
            transcript: scrollback,
            inspector: None,
        };
    }
    let transcript_width = scrollback.width - inspector_width;
    WorkspaceRegions {
        transcript: ratatui::layout::Rect {
            width: transcript_width,
            ..scrollback
        },
        inspector: Some(ratatui::layout::Rect {
            x: scrollback.x + transcript_width,
            width: inspector_width,
            ..scrollback
        }),
    }
}

fn scrollbar_is_guaranteed(blocks: &[Block], viewport: u64) -> bool {
    if viewport == 0 {
        return false;
    }
    let vp = viewport as usize;
    if blocks.len() > vp {
        return true;
    }
    let mut min_rows = 0usize;
    for block in blocks {
        let base = match block.kind() {
            BlockKind::User(_) => 2,
            BlockKind::Assistant(_) => 3,
            _ => 1,
        };
        min_rows = min_rows.saturating_add(base + usize::from(block.turn_boundary_before()));
        if min_rows > vp {
            return true;
        }
    }
    false
}

pub fn measure_scrollback(
    state: &AppState,
    width: u16,
    height: u16,
    cache: &mut WrapCache,
) -> ScrollMetrics {
    let regions = plan_regions(state, width, height, cache);
    let workspace = workspace_regions(state, to_ratatui(regions.scrollback));
    let notices = toast_row_count(state, workspace.transcript.height);
    let viewport = u64::from(workspace.transcript.height.saturating_sub(notices));
    let mut content_width = workspace.transcript.width;
    let scrollbar_guaranteed = scrollbar_is_guaranteed(state.blocks(), viewport);
    let content_rev = state.revisions.content;
    let fold_rev = state.revisions.fold;
    let index = if scrollbar_guaranteed {
        content_width = content_width.saturating_sub(1);
        cache.height_index(state.blocks(), content_rev, fold_rev, content_width)
    } else {
        let mut idx = cache.height_index(state.blocks(), content_rev, fold_rev, content_width);
        if viewport > 0 && idx.total_rows > viewport {
            content_width = content_width.saturating_sub(1);
            idx = cache.height_index(state.blocks(), content_rev, fold_rev, content_width);
        }
        idx
    };
    index.metrics(&state.scroll.mode, viewport)
}

pub(crate) struct Palette {
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
    selection: Color,
    menu_selected: Style,
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
        let menu_selected_bg = if capabilities.color_depth == ColorDepth::Ansi16 {
            // #2A2A2A and the modal #181818 both quantize to Black in the
            // legacy 16-color palette; DarkGray preserves the focus contrast.
            Color::DarkGray
        } else {
            color(MENU_SELECTION_BG)
        };
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
            selection: color(theme.selection),
            menu_selected: base.bg(menu_selected_bg),
        }
    }
}

fn menu_line(
    mut spans: Vec<Span<'static>>,
    selected: bool,
    width: usize,
    palette: &Palette,
) -> Line<'static> {
    if selected {
        let occupied: usize = spans.iter().map(Span::width).sum();
        if occupied < width {
            spans.push(Span::styled(
                " ".repeat(width - occupied),
                palette.menu_selected,
            ));
        }
        Line::from(spans).style(palette.menu_selected)
    } else {
        Line::from(spans)
    }
}

fn draw_state(
    backend: &mut FullscreenBackend,
    state: &AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) -> io::Result<String> {
    let mut selected = String::new();
    backend.terminal().draw(|frame| {
        render_frame(frame, state, capabilities, cache);
        selected = extract_visible_selection(frame, state, cache);
    })?;
    Ok(selected)
}

pub fn render_frame(
    frame: &mut ratatui::Frame,
    state: &AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) {
    cache.selection_regions = [None, None];
    let palette = Palette::of(capabilities);
    let area = frame.area();
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.background),
        area,
    );
    let regions = plan_regions(state, area.width, area.height, cache);
    let session_visible = regions.session_rail.height > 0;
    let scrollback = to_ratatui(regions.scrollback);
    let workspace = workspace_regions(state, scrollback);
    let band = workspace_band(&workspace);

    if session_visible {
        render_session_rail(
            frame,
            chrome_area(to_ratatui(regions.session_rail), band),
            state,
            &palette,
        );
    }
    let motion_capabilities = Capabilities {
        reduced_motion: capabilities.reduced_motion
            || regions.activity_rail.height == 0
            || !motion_needed(state, capabilities, cache),
        ..capabilities
    };
    let search_matches = state.search.as_ref().map_or_else(Arc::default, |search| {
        cache.search_matches(
            state.blocks(),
            &search.query,
            search.filter,
            state.revisions.content,
        )
    });
    let thinking_header_visible = render_scrollback(
        frame,
        workspace.transcript,
        state,
        &palette,
        cache,
        motion_capabilities,
        !capabilities.reduced_motion,
        &search_matches,
    );
    if regions.todo.height > 0 {
        render_todo_dock(
            frame,
            chrome_area(to_ratatui(regions.todo), band),
            state,
            &palette,
            capabilities,
        );
    }
    render_divider(
        frame,
        chrome_area(to_ratatui(regions.todo_divider), band),
        palette.border,
    );
    if regions.activity_rail.height > 0 {
        render_activity_rail(
            frame,
            chrome_area(to_ratatui(regions.activity_rail), band),
            state,
            &palette,
            Capabilities {
                reduced_motion: motion_capabilities.reduced_motion || thinking_header_visible,
                ..capabilities
            },
            thinking_header_visible,
        );
    }
    let composer_area = chrome_area(to_ratatui(regions.composer), band);
    render_composer(frame, composer_area, state, &palette, cache);
    if let Some(suggestions) = &state.slash_suggestions {
        let matches = cache.slash_matches(state, &suggestions.query);
        render_slash_popup(frame, composer_area, suggestions, &matches, &palette);
    }
    render_interaction_overlay(frame, composer_area, state, &palette, capabilities);
    render_operational_bar(
        frame,
        chrome_area(to_ratatui(regions.operational), band),
        state,
        &palette,
        session_visible,
        regions.activity_rail.height > 0,
        cache,
    );
    if let Some(inspector) = workspace.inspector {
        render_inspector_panel(
            frame,
            inspector,
            state,
            state.inspector.active,
            &palette,
            cache,
        );
    } else if let Some(kind) = state.inspector.active {
        // A floating inspector owns the content surface; do not select behind it.
        cache.selection_regions[0] = None;
        render_inspector_overlay(frame, scrollback, state, kind, &palette, cache);
    }
    if state.search.is_some() {
        render_search_bar(
            frame,
            workspace.transcript,
            state,
            &palette,
            &search_matches,
        );
    }
    if let Some(overlay) = &state.model_overlay {
        let rows = cache.model_rows(
            overlay,
            &state.open_code_models,
            &state.cline_pass_models,
            &state.command_code_models,
            &state.zen_models,
            state.catalog_revision,
        );
        render_model_overlay(
            frame,
            overlay,
            &state.model,
            &state.open_code_models,
            &state.cline_pass_models,
            &state.command_code_models,
            &state.zen_models,
            &rows,
            &palette,
        );
    }
    if let Some(overlay) = &state.mcp_overlay {
        render_mcp_overlay(frame, overlay, &state.mcp_servers, &palette);
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
            cache,
        );
    }
    if let Some((selection, area)) = state.selection.zip(selection_area(state, cache)) {
        crate::selection::highlight_selection(
            frame.buffer_mut(),
            selection,
            area,
            palette.selection,
        );
    }
}

/// Width available to draft text after the shared chrome inset, composer
/// border and ASCII prompt. This is the same budget used by `render_composer`
/// and keeps layout height/paint wrapping in lockstep across resize probes.
fn composer_text_budget_for_viewport(viewport_width: u16, viewport_height: u16) -> u16 {
    let chrome_width = if viewport_width > 2 {
        viewport_width - 2
    } else {
        viewport_width
    };
    let boxed = viewport_height >= 8;
    let content_width = if boxed {
        chrome_width.saturating_sub(2)
    } else {
        chrome_width
    };
    content_width
        .saturating_sub(UnicodeWidthStr::width(COMPOSER_PROMPT) as u16)
        .max(1)
}

fn composer_text_budget_for_area(area_width: u16, boxed: bool) -> usize {
    let content_width = if boxed {
        area_width.saturating_sub(2)
    } else {
        area_width
    };
    content_width
        .saturating_sub(UnicodeWidthStr::width(COMPOSER_PROMPT) as u16)
        .max(1) as usize
}

fn plan_regions(
    state: &AppState,
    width: u16,
    height: u16,
    cache: &mut WrapCache,
) -> crate::layout::LayoutRegions {
    let todo_rows = todo_height(
        state.todo_dock_open,
        state.todo_items.len(),
        state
            .todo_items
            .iter()
            .any(|item| item.status == TodoItemStatus::InProgress),
    );
    let show_session =
        !state.blocks().is_empty() && !is_trivial_cwd(&state.cwd) && width >= 80 && height >= 12;
    let snapshot_width = composer_text_budget_for_viewport(width, height);
    let composer_lines = cache
        .composer_snapshot(&state.composer, snapshot_width)
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
    if area_height == 0 || state.mcp_overlay.is_some() {
        return 0;
    }
    state.visible_toast_tail(3).len().min(area_height as usize) as u16
}

fn next_toast_visual_deadline_ms(
    state: &AppState,
    now_ms: u64,
    notice_count: u16,
    highlight_enabled: bool,
) -> Option<u64> {
    if notice_count == 0 {
        return None;
    }
    state
        .visible_toast_tail(usize::from(notice_count))
        .into_iter()
        .map(|notification| {
            let expiry_ms = notification.created_ms.saturating_add(INFO_TOAST_TTL_MS);
            if highlight_enabled {
                let highlight_ms = notification
                    .created_ms
                    .saturating_add(INFO_TOAST_HIGHLIGHT_MS);
                if now_ms < highlight_ms {
                    return highlight_ms;
                }
            }
            expiry_ms
        })
        .min()
}

fn motion_needed(state: &AppState, capabilities: Capabilities, cache: &mut WrapCache) -> bool {
    if capabilities.reduced_motion
        || capabilities.color_depth == ColorDepth::None
        || welcome_visible(state)
        || navigation_captured(state)
        || state.inspector.active.is_some()
    {
        return false;
    }
    state.working || cache.has_streaming_block(state.blocks(), state.revisions.content)
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
    let projection = session_rail_projection(state, area.width as usize, state.working);
    let mut spans = session_rail_spans(&projection.identity, palette);
    if projection.gap > 0 {
        spans.push(Span::raw(" ".repeat(projection.gap)));
    }
    if !projection.status.is_empty() {
        spans.push(Span::styled(projection.status, palette.secondary));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(palette.surface),
        area,
    );
}

fn session_rail_spans(identity: &str, palette: &Palette) -> Vec<Span<'static>> {
    if let Some(path) = identity.strip_prefix("SLIM · ") {
        vec![
            Span::styled("SLIM", palette.secondary),
            Span::styled(" · ", palette.muted),
            Span::styled(path.to_owned(), palette.muted),
        ]
    } else if identity == "SLIM" {
        vec![Span::styled("SLIM", palette.secondary)]
    } else {
        vec![Span::styled(identity.to_owned(), palette.muted)]
    }
}

// The transcript painter keeps its independent layout, motion, notice, and
// search inputs explicit so callers cannot accidentally conflate their state.
#[allow(clippy::too_many_arguments)]
fn render_scrollback(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    cache: &mut WrapCache,
    capabilities: Capabilities,
    notice_highlight_enabled: bool,
    search_matches: &[usize],
) -> bool {
    if welcome_visible(state) {
        frame.render_widget(
            ratatui::widgets::Block::default().style(palette.surface),
            area,
        );
        render_welcome(frame, area, state, palette, capabilities);
        return false;
    }
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.surface),
        area,
    );
    let notice_count = toast_row_count(state, area.height);
    let viewport = u64::from(area.height.saturating_sub(notice_count));
    let mut content_width = area.width;
    let scrollbar_guaranteed = scrollbar_is_guaranteed(state.blocks(), viewport);
    let content_rev = state.revisions.content;
    let fold_rev = state.revisions.fold;
    let (index, scrollbar) = if scrollbar_guaranteed {
        content_width = area.width.saturating_sub(1);
        (
            cache.height_index(state.blocks(), content_rev, fold_rev, content_width),
            true,
        )
    } else {
        let mut idx = cache.height_index(state.blocks(), content_rev, fold_rev, content_width);
        let bar = viewport > 0 && idx.total_rows > viewport;
        if bar {
            content_width = area.width.saturating_sub(1);
            idx = cache.height_index(state.blocks(), content_rev, fold_rev, content_width);
        }
        (idx, bar)
    };
    let metrics = index.metrics(&state.scroll.mode, viewport);
    let bottom = metrics.bottom_start;
    let start_row = metrics.viewport_start;
    let (mut idx, mut skip_rows) = index.locate(start_row);
    let capacity = viewport as usize;
    let mut rows = 0usize;
    // Geometric visibility is kept separate from animation capability.  A
    // visible streaming header suppresses the ActivityRail duplicate even
    // when reduced-motion or no-color leaves its glyph static.
    let mut streaming_header_visible = false;
    let selected = cache.selected_block(state);
    let matched_ids: std::collections::HashSet<&crate::api::BlockId> = if search_matches.is_empty()
    {
        std::collections::HashSet::new()
    } else {
        search_matches
            .iter()
            .filter_map(|index| state.blocks().get(*index).map(|block| &block.id))
            .collect()
    };
    let selected_search_id = state.search.as_ref().and_then(|search| {
        search_matches
            .get(search.selected)
            .and_then(|index| state.blocks().get(*index))
            .map(|block| &block.id)
    });
    let live_collapsed_group = if state.working {
        cache.tool_group_leader(state.blocks(), content_rev, fold_rev)
    } else {
        None
    };
    // A short conversation grows upward from the composer, like the welcome.
    // Full histories retain their anchored/page-fill scroll semantics.
    let leading_space = viewport.saturating_sub(index.total_rows).saturating_sub(1) as u16;
    let text_area = ratatui::layout::Rect {
        x: area.x,
        y: area.y + leading_space,
        width: content_width,
        height: area.height.saturating_sub(leading_space),
    };
    let buf = frame.buffer_mut();
    buf.set_style(text_area, palette.text);
    let mut y = text_area.y;
    while idx < index.len() && rows < capacity && y < text_area.bottom() {
        let (_, block, members) = index.entry(idx);
        idx += 1;
        let is_selected = selected.as_ref() == Some(&block.id);
        let show_enter_hint = is_selected
            || live_collapsed_group
                .as_ref()
                .is_some_and(|leader| leader == &block.id);
        let thinking_header_index = usize::from(
            members.len() == 1
                && matches!(block.kind(), BlockKind::Thinking(_))
                && block.turn_boundary_before(),
        );
        let skipped = usize::try_from(skip_rows).unwrap_or(usize::MAX);
        let thinking_header_candidate = matches!(block.kind(), BlockKind::Thinking(_))
            && block.lifecycle == BlockLifecycle::Streaming
            && skipped <= thinking_header_index;
        let ctx = BlockRender {
            palette,
            width: content_width,
            capabilities,
            selected: is_selected,
            frame: state.clock.frame,
            animate_thinking: !streaming_header_visible
                && !capabilities.reduced_motion
                && capabilities.color_depth != ColorDepth::None
                && thinking_header_candidate,
        };
        // Key on everything the produced lines depend on: each member's
        // generation/lifecycle/fold folded together, selection and the enter
        // hint on the leader, and the wrap width.  The animated Thinking
        // glyph is patched into the visible header after these static lines
        // are painted, so the clock is intentionally absent from this key.
        let mut member_state = 0u64;
        for member in members {
            member_state = member_state
                .wrapping_mul(31)
                .wrapping_add(member.content_generation())
                .wrapping_mul(31)
                .wrapping_add(u64::from(member.lifecycle_tag()))
                .wrapping_mul(31)
                .wrapping_add(u64::from(member.fold_tag()));
        }
        let key = (
            block.cache_identity(),
            member_state,
            u8::from(is_selected) | u8::from(show_enter_hint) << 1,
            content_width,
        );
        let block_lines = cache.get_block_lines(&key).unwrap_or_else(|| {
            // Cache only the stable projection.  In particular, never retain
            // a frame-specific spinner glyph in the body/header memo.
            let static_ctx = BlockRender {
                palette: ctx.palette,
                width: ctx.width,
                capabilities: ctx.capabilities,
                selected: ctx.selected,
                frame: ctx.frame,
                animate_thinking: false,
            };
            let built = if members.len() > 1 {
                if crate::block::is_failed_tool(block) {
                    grouped_failed_tool_lines(block, members, show_enter_hint, &static_ctx, cache)
                } else if crate::block::is_complete_thinking(block) {
                    grouped_thinking_lines(block, members, &static_ctx, cache)
                } else {
                    grouped_tool_lines(block, members, show_enter_hint, &static_ctx, cache)
                }
            } else {
                safe_block_lines(block, &static_ctx, cache)
            };
            cache.store_block_lines(key, built)
        });
        let search_match = !matched_ids.is_empty()
            && (matched_ids.contains(&block.id)
                || members
                    .iter()
                    .any(|member| matched_ids.contains(&member.id)));
        let skip = (skip_rows as usize).min(block_lines.len());
        skip_rows = 0;
        let written = block_lines.len().saturating_sub(skip).min(capacity - rows);
        let block_start_y = y;
        // Lines are pre-wrapped to `content_width` by the same contract the
        // HeightIndex measures; writing straight into the buffer avoids
        // materializing an owned Vec<Line> per frame.
        for line in &block_lines[skip..skip + written] {
            buf.set_line(text_area.x, y, line, content_width);
            y += 1;
        }
        if search_match && written > 0 {
            let selected_match = selected_search_id
                .is_some_and(|id| id == &block.id || members.iter().any(|member| &member.id == id));
            let highlight = if selected_match {
                palette.surface_alt.patch(palette.accent_bold)
            } else {
                palette.surface_alt
            };
            buf.set_style(
                ratatui::layout::Rect {
                    x: text_area.x,
                    y: y - written as u16,
                    width: content_width,
                    height: written as u16,
                },
                highlight,
            );
        }
        // Thinking lines are cached without the clock.  Patch exactly the
        // indicator cell on the visible streaming header, preserving the
        // cached cell's foreground/background/modifiers (including selection
        // and search overlays).
        let header_written = skip <= thinking_header_index
            && skip.saturating_add(written) > thinking_header_index
            && thinking_header_candidate;
        if header_written {
            streaming_header_visible = true;
        }
        if ctx.animate_thinking && header_written {
            let glyph_x = text_area.x.saturating_add(2);
            let glyph_y =
                block_start_y.saturating_add((thinking_header_index.saturating_sub(skip)) as u16);
            if glyph_x < text_area.right() && glyph_y < text_area.bottom() {
                buf[(glyph_x, glyph_y)].set_char(spinner_glyph(ctx.frame, ctx.capabilities));
            }
        }
        rows += written;
    }
    // Only painted transcript rows, excluding its gutter, scrollbar and toasts.
    let selection_rect = ratatui::layout::Rect {
        x: text_area.x.saturating_add(2),
        y: text_area.y,
        width: content_width.saturating_sub(2),
        height: y.saturating_sub(text_area.y).min(viewport as u16),
    };
    if selection_rect.width > 0 && selection_rect.height > 0 {
        cache.selection_regions[0] = Some(selection_rect);
    }
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
                let (glyph, style) = if row >= thumb_top && row < thumb_top.saturating_add(thumb) {
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
                let available_width = area.width.saturating_sub(2) as usize;
                let truncated = crate::view_model::truncate_display_width(&safe, available_width);
                let style = if notice_highlight_enabled
                    && state.clock.elapsed_ms.saturating_sub(notice.created_ms)
                        < INFO_TOAST_HIGHLIGHT_MS
                {
                    palette.muted.add_modifier(Modifier::BOLD)
                } else {
                    palette.muted
                };
                Line::from(vec![
                    Span::styled("  ", palette.surface),
                    Span::styled(truncated, style),
                ])
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
    streaming_header_visible
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
        // The activity rail or visible thinking header owns the one animated
        // indicator for a frame; the TODO dock stays a stable status marker.
        let active_glyph = glyph(capabilities, '\u{25cc}', '~');
        header.push(Span::styled(format!(" {active_glyph} "), palette.warning));
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
    frame.render_widget(Paragraph::new(rows).style(palette.surface_alt), area);
}

fn render_activity_rail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    capabilities: Capabilities,
    thinking_header_visible: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let elapsed = activity_elapsed(state);
    let header_owns_thinking = thinking_header_visible
        && matches!(
            state.activity.as_ref().map(|activity| &activity.phase),
            Some(ActivityPhase::Thinking)
        );
    let mut spans = if header_owns_thinking {
        Vec::new()
    } else {
        vec![
            Span::styled(
                format!("{} ", spinner_glyph(state.clock.frame, capabilities)),
                palette.accent,
            ),
            Span::styled(activity_label(state), palette.text),
        ]
    };
    if elapsed > 0 {
        let separator = if header_owns_thinking { " " } else { " · " };
        spans.push(Span::styled(
            format!("{separator}{elapsed}s"),
            palette.muted,
        ));
    }
    if area.width >= 72 {
        for (name, used, limit) in [
            ("turn", state.turns_used, state.max_turns),
            ("reads", state.tools_used_read, state.max_read_tool_calls),
            (
                "edits",
                state.tools_used_mutating,
                state.max_mutating_tool_calls,
            ),
        ] {
            if !budget_near_limit(used, limit) {
                continue;
            }
            let counter = format!(" · {name} {used}/{limit}");
            let occupied: usize = spans.iter().map(|span| span.width()).sum();
            if occupied + UnicodeWidthStr::width(counter.as_str()) <= area.width as usize {
                spans.push(Span::styled(counter, palette.warning));
            }
        }
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
    let inset = if area.width > 4 { 2 } else { 1 };
    let content_area = if area.width > inset * 2 {
        ratatui::layout::Rect {
            x: area.x + inset,
            width: area.width - (inset * 2),
            ..area
        }
    } else {
        area
    };
    let block_width = content_area.width;
    let mut lines = vec![
        Line::from(vec![
            Span::styled("SLIM", title_style),
            Span::styled(concat!(" v", env!("CARGO_PKG_VERSION")), palette.muted),
        ]),
        Line::from(vec![
            Span::styled(format!("{dot}  "), dot_style),
            Span::styled(status, palette.muted),
        ]),
        Line::from(Span::styled(
            hint,
            if connected {
                palette.secondary
            } else {
                palette.accent
            },
        )),
    ];
    // Workspace path stays on the welcome card; shortcuts live in the footer.
    if area.height >= 8 {
        if let Some(cwd) = cwd {
            lines.push(Line::from(Span::styled(
                truncate_display_width(&cwd, block_width as usize),
                palette.muted,
            )));
        }
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
    let welcome_height = (lines.len() as u16).min(content_area.height);
    let gap = u16::from(content_area.height > welcome_height);
    let welcome_area = ratatui::layout::Rect {
        y: content_area.y + content_area.height.saturating_sub(welcome_height + gap),
        height: welcome_height,
        ..content_area
    };
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Left),
        welcome_area,
    );
}

pub(crate) fn last_collapsed_tool_group_leader(blocks: &[Block]) -> Option<BlockId> {
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
    cache: &mut WrapCache,
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
    let names: Vec<&str> = members
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::Tool(tool) if crate::block::is_complete_tool(block) => {
                Some(tool.name.as_str())
            }
            _ => None,
        })
        .collect();
    let tool_count = names.len();
    let phrase = completed_tool_phrase(&names);
    let detailed = if phrase.is_empty() {
        format!("{tool_count} tools{duration}")
    } else {
        format!("{phrase}{duration}")
    };
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
        Span::styled(glyph_text, ctx.palette.muted),
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
                cache,
            ));
        }
    }
    lines
}

fn grouped_thinking_lines(
    leader: &Block,
    members: &[Block],
    ctx: &BlockRender<'_>,
    cache: &mut WrapCache,
) -> Vec<Line<'static>> {
    let count = members
        .iter()
        .filter(|block| crate::block::is_complete_thinking(block))
        .count()
        .max(members.len());
    let mut lines = vec![thinking_header(leader, count, ctx)];
    if leader.fold == FoldState::Expanded {
        let body_width = thinking_body_width(ctx.width);
        let thinking_style = ctx.palette.thinking;
        for member in members {
            let BlockKind::Thinking(text) = member.kind() else {
                continue;
            };
            lines.extend(cache.wrapped_body(
                member,
                BodyKind::Thinking,
                ctx.width,
                member.lifecycle != BlockLifecycle::Streaming,
                || {
                    render_plain(text, body_width)
                        .into_iter()
                        .map(|row| Line::from(Span::styled(format!("    {row}"), thinking_style)))
                        .collect()
                },
                cached_lines_bytes,
            ));
        }
    }
    lines
}

fn grouped_failed_tool_lines(
    leader: &Block,
    members: &[Block],
    show_enter_hint: bool,
    ctx: &BlockRender<'_>,
    cache: &mut WrapCache,
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
    let reason_text = short_failure_reason(&preview);
    let label = if count > 1 {
        format!("{name} ×{count}")
    } else {
        name
    };
    let mut parts = vec![ToolDetailPart::fixed(label)];
    if reason_text.is_empty() {
        parts.push(ToolDetailPart::fixed("failed".into()));
    } else {
        parts.push(ToolDetailPart::flexible(reason_text, 1, 1));
    }
    let marker = if ctx.selected { "> " } else { "  " };
    let failed = glyph(ctx.capabilities, '\u{2715}', 'x');
    let glyph_text = format!("{failed} ");
    let hint = "Enter details";
    let marker_width = UnicodeWidthStr::width(marker);
    let content_width = ctx.width as usize;
    let occupied = marker_width + UnicodeWidthStr::width(glyph_text.as_str());
    let reason = fit_tool_detail(parts, content_width.saturating_sub(occupied));
    let header_width =
        UnicodeWidthStr::width(glyph_text.as_str()) + UnicodeWidthStr::width(reason.as_str());
    let hint_width = UnicodeWidthStr::width(hint);
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
                cache,
            ));
        }
    }
    lines
}

fn short_failure_reason(preview: &str) -> String {
    let line = sanitize_terminal_text(preview.lines().next().unwrap_or_default());
    let mut kept = Vec::new();
    for part in line.split(" · ").filter(|part| !part.is_empty()) {
        if is_tool_telemetry_segment(part) {
            continue;
        }
        kept.push(part);
    }
    if kept.is_empty() {
        line.split(" · ")
            .find(|part| !part.is_empty())
            .unwrap_or("")
            .to_owned()
    } else {
        kept.join(" · ")
    }
}

fn is_tool_telemetry_segment(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    lower.contains('=')
        || lower.starts_with("out ")
        || lower.starts_with("err ")
        || lower.starts_with("stdout")
        || lower.starts_with("stderr")
}

#[allow(clippy::too_many_arguments)]
fn tool_member_lines(
    block: &Block,
    selected: bool,
    palette: &Palette,
    capabilities: Capabilities,
    width: u16,
    _frame: u64,
    include_call_id: bool,
    cache: &mut WrapCache,
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
            format!("{} ", glyph(capabilities, '\u{25cb}', '~')),
            palette.accent,
            palette.text,
            None,
        ),
        BlockLifecycle::Complete if state.historical => {
            ("- ".into(), palette.muted, palette.muted, Some("history"))
        }
        BlockLifecycle::Complete => (
            format!("{} ", glyph(capabilities, '\u{2713}', '+')),
            palette.muted,
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
    let show_args = !state.historical
        && (block.lifecycle == BlockLifecycle::Streaming
            || block.fold == FoldState::Expanded
            || include_call_id);
    let collapsed_failure = matches!(block.lifecycle, BlockLifecycle::Failed)
        && block.fold != FoldState::Expanded
        && !include_call_id;
    let show_preview = !state.historical
        && (show_args || block.lifecycle == BlockLifecycle::Failed)
        && !collapsed_failure;
    let failure_reason = collapsed_failure
        .then(|| short_failure_reason(&preview))
        .filter(|reason| !reason.is_empty());
    let preview_shown = (show_preview && !preview.is_empty()) || failure_reason.is_some();
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
    if let Some(reason) = failure_reason {
        parts.push(ToolDetailPart::flexible(reason, 1, 1));
    } else if show_preview {
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
        let output_style = palette.secondary;
        let materialized = &state.materialized_output;
        lines.extend(cache.wrapped_body(
            block,
            BodyKind::ToolOutput,
            width,
            block.lifecycle != BlockLifecycle::Streaming,
            || {
                render_plain(materialized, body_width)
                    .into_iter()
                    .map(|row| Line::from(Span::styled(format!("    {row}"), output_style)))
                    .collect()
            },
            cached_lines_bytes,
        ));
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

fn fill_to_width(mut spans: Vec<Span<'static>>, width: usize, fill: Style) -> Line<'static> {
    let used: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(spans)
}

fn user_band_lines(text: &str, ctx: &BlockRender<'_>) -> Vec<Line<'static>> {
    let width = ctx.width.max(1) as usize;
    let band = ctx.palette.user_prompt_bg;
    let hang = " ".repeat(crate::render::USER_PROMPT_PREFIX_COLS as usize);
    let rows = render_plain(text, user_prompt_text_width(ctx.width));
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = if index == 0 {
                vec![
                    Span::styled("  ", band),
                    Span::styled("You", ctx.palette.secondary.patch(band)),
                    Span::styled("  ", band),
                ]
            } else {
                vec![Span::styled(hang.clone(), band)]
            };
            spans.push(Span::styled(row, ctx.palette.text.patch(band)));
            fill_to_width(spans, width, band)
        })
        .collect();
    if lines.is_empty() {
        lines.push(fill_to_width(
            vec![
                Span::styled("  ", band),
                Span::styled("You", ctx.palette.secondary.patch(band)),
            ],
            width,
            band,
        ));
    }
    lines.push(Line::default());
    lines
}

fn question_block_lines(
    state: &InteractionRequestState,
    ctx: &BlockRender<'_>,
) -> Vec<Line<'static>> {
    let width = ctx.width.saturating_sub(2).max(8) as usize;
    state
        .layout_lines(width)
        .into_iter()
        .map(|row| {
            if row.is_empty() {
                return Line::default();
            }
            let selected = row.starts_with("[x] ");
            let prompt = row.starts_with("? ");
            let status = row.trim_start().starts_with('·');
            if selected {
                return Line::from(Span::styled(pad_cells(&row, width), ctx.palette.text));
            }
            if prompt {
                return Line::from(vec![
                    Span::styled("? ".to_owned(), ctx.palette.accent),
                    Span::styled(row[2..].to_owned(), ctx.palette.text),
                ]);
            }
            let style = if status {
                ctx.palette.muted
            } else {
                ctx.palette.secondary
            };
            Line::from(Span::styled(row, style))
        })
        .collect()
}

/// Immutable per-frame render inputs threaded through the block renderer.
/// Bundled so `block_lines` stays under clippy's argument-count lint.
struct BlockRender<'a> {
    palette: &'a Palette,
    width: u16,
    capabilities: Capabilities,
    selected: bool,
    frame: u64,
    animate_thinking: bool,
}

fn thinking_header(block: &Block, count: usize, ctx: &BlockRender<'_>) -> Line<'static> {
    let streaming = block.lifecycle == BlockLifecycle::Streaming;
    let expanded = block.fold == FoldState::Expanded;
    let indicator = if streaming {
        if ctx.animate_thinking {
            spinner_glyph(ctx.frame, ctx.capabilities)
        } else {
            glyph(ctx.capabilities, '\u{25cb}', '~')
        }
    } else if expanded {
        glyph(ctx.capabilities, '\u{25be}', 'v')
    } else {
        glyph(ctx.capabilities, '\u{25b8}', '>')
    };
    let label = match block.lifecycle {
        BlockLifecycle::Streaming => "Thinking".into(),
        BlockLifecycle::Cancelled => "Thought · interrupted".into(),
        BlockLifecycle::Failed => "Thought · failed".into(),
        _ if count > 1 => format!("Thought ×{count}"),
        _ => "Thought".into(),
    };
    let label = truncate_cells(&label, ctx.width.saturating_sub(4) as usize);
    let mut spans = vec![
        Span::styled(if ctx.selected { "> " } else { "  " }, ctx.palette.muted),
        Span::styled(
            format!("{indicator} "),
            if streaming {
                ctx.palette.accent
            } else {
                ctx.palette.muted
            },
        ),
        Span::styled(
            label.clone(),
            if streaming {
                ctx.palette.secondary
            } else {
                ctx.palette.muted
            },
        ),
    ];
    if ctx.selected {
        let hint = if expanded {
            "Enter collapse"
        } else {
            "Enter expand"
        };
        let used = 4 + UnicodeWidthStr::width(label.as_str());
        if used + 2 + hint.len() <= ctx.width as usize {
            spans.push(Span::raw(
                " ".repeat(ctx.width as usize - used - hint.len()),
            ));
            spans.push(Span::styled(hint, ctx.palette.secondary));
        }
    }
    Line::from(spans)
}

fn block_lines(block: &Block, ctx: &BlockRender<'_>, cache: &mut WrapCache) -> Vec<Line<'static>> {
    let mut lines = match block.kind() {
        BlockKind::User(text) => user_band_lines(text, ctx),
        BlockKind::Assistant(text) => {
            let label_style = if matches!(
                block.lifecycle,
                BlockLifecycle::Cancelled | BlockLifecycle::Failed
            ) {
                ctx.palette.secondary
            } else {
                ctx.palette.accent_bold
            };
            let mut lines = vec![
                Line::default(),
                Line::from(Span::styled(
                    format!("  {}", assistant_label(block.lifecycle)),
                    label_style,
                )),
            ];
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
            let streaming = block.lifecycle == BlockLifecycle::Streaming;
            let mut lines = vec![thinking_header(block, 1, ctx)];
            if block.fold == FoldState::Expanded {
                let body_width = thinking_body_width(ctx.width);
                let thinking_style = ctx.palette.thinking;
                // Expanded bodies are wrapped once per generation and shared
                // with the height probe instead of re-wrapping every frame.
                lines.extend(cache.wrapped_body(
                    block,
                    BodyKind::Thinking,
                    ctx.width,
                    !streaming,
                    || {
                        render_plain(text, body_width)
                            .into_iter()
                            .map(|row| {
                                Line::from(Span::styled(format!("    {row}"), thinking_style))
                            })
                            .collect()
                    },
                    cached_lines_bytes,
                ));
            } else if streaming {
                let preview_width = thinking_body_width(ctx.width);
                let (preview, truncated) = thinking_preview_tail(text);
                let rows = render_plain(preview, preview_width);
                let hidden = truncated || rows.len() > 2;
                let start = rows.len().saturating_sub(2);
                for (index, row) in rows.into_iter().skip(start).enumerate() {
                    let prefix = if hidden && index == 0 {
                        "  … "
                    } else {
                        "    "
                    };
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
            cache,
        ),
        BlockKind::InteractionRequest(state) => {
            if state.acknowledgement.is_none() {
                Vec::new()
            } else {
                question_block_lines(state, ctx)
            }
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
    // One markdown parse per block generation feeds both the height probe and
    // this render: stable bodies wrap once into the LRU, the streaming tail
    // wraps once into the scratch slot. The caret/indent stay dynamic.
    let projection = cache.markdown_projection(block, text, text_width);
    let mut lines = cache.wrapped_body(
        block,
        BodyKind::Assistant,
        ctx.width,
        !streaming,
        || crate::markdown::render_projected(&projection, text_width, styles),
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

fn question_composer_hint(state: &AppState) -> Option<&'static str> {
    let interaction = state.pending_interaction()?;
    let InteractionRequestKind::Question { options, .. } = &interaction.kind else {
        return None;
    };
    if interaction.response_pending {
        Some("waiting")
    } else if options.is_empty() || interaction.custom_question_answer {
        Some("answer · Enter")
    } else {
        None
    }
}

fn composer_is_focused(state: &AppState) -> bool {
    state.login_overlay.is_none()
        && state.model_overlay.is_none()
        && state.effort_overlay.is_none()
        && state.mcp_overlay.is_none()
        && state.palette_query.is_none()
        && state.search.is_none()
        && state.inspector.active.is_none()
        && !interaction_blocks_composer(state)
}

fn interaction_blocks_composer(state: &AppState) -> bool {
    let Some(interaction) = state.pending_interaction() else {
        return false;
    };
    if interaction.response_pending {
        return true;
    }
    match &interaction.kind {
        InteractionRequestKind::Approval { .. } => true,
        InteractionRequestKind::Question { options, .. } => {
            !options.is_empty() && !interaction.custom_question_answer
        }
        InteractionRequestKind::Input { .. } => false,
    }
}

fn pad_cells(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let used = UnicodeWidthStr::width(text);
    if used > width {
        return truncate_cells(text, width);
    }
    let mut padded = text.to_owned();
    padded.push_str(&" ".repeat(width - used));
    padded
}

fn interaction_status_line(interaction: &InteractionRequestState) -> String {
    let persistence = if interaction.persisted {
        "persisted"
    } else {
        "ephemeral"
    };
    let status = match &interaction.acknowledgement {
        Some(acknowledgement) if acknowledgement.message.is_empty() => {
            if acknowledgement.accepted {
                "accepted"
            } else {
                "rejected"
            }
        }
        Some(acknowledgement) => acknowledgement.message.as_str(),
        None if interaction.response_pending => "response sent · awaiting acknowledgement",
        None => "waiting for response",
    };
    format!("{persistence} · {status}")
}

fn interaction_overlay_chrome(
    interaction: &InteractionRequestState,
) -> (&'static str, &'static str) {
    match &interaction.kind {
        InteractionRequestKind::Question { options, .. } => {
            let hint = if interaction.response_pending {
                "waiting"
            } else if options.is_empty() || interaction.custom_question_answer {
                "answer · Enter"
            } else {
                "↑↓ Enter"
            };
            (" Question ", hint)
        }
        InteractionRequestKind::Approval { .. } => {
            let hint = if interaction.response_pending {
                "waiting"
            } else {
                "Y approve · N reject"
            };
            (" Approve ", hint)
        }
        InteractionRequestKind::Input { .. } => {
            let hint = if interaction.response_pending {
                "waiting"
            } else {
                "Enter answer"
            };
            (" Input ", hint)
        }
    }
}

fn overlay_option_row(
    text: &str,
    inner_width: usize,
    selected: bool,
    palette: &Palette,
    label: bool,
) -> Line<'static> {
    let padded = pad_cells(&sanitize_terminal_text(text), inner_width);
    let style = if selected && label {
        palette.text
    } else if label {
        palette.secondary
    } else {
        palette.muted
    };
    Line::from(Span::styled(format!(" {padded}"), style))
}

fn overlay_option_lines(
    label: &str,
    description: &str,
    selected: bool,
    inner_width: usize,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let prefix = question_option_marker(selected);
    let hang_width = UnicodeWidthStr::width(prefix).min(inner_width.saturating_sub(1));
    let body_width = inner_width.saturating_sub(hang_width).max(1);
    let hang = " ".repeat(hang_width);
    let mut lines = wrap_words(label, body_width)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let text = if index == 0 {
                format!("{prefix}{row}")
            } else {
                format!("{hang}{row}")
            };
            overlay_option_row(&text, inner_width, selected, palette, true)
        })
        .collect::<Vec<_>>();
    if !description.is_empty() {
        lines.extend(wrap_words(description, body_width).into_iter().map(|row| {
            overlay_option_row(
                &format!("{hang}{row}"),
                inner_width,
                selected,
                palette,
                false,
            )
        }));
    }
    lines
}

fn interaction_overlay_lines(
    interaction: &InteractionRequestState,
    inner_width: usize,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    let push_wrapped = |lines: &mut Vec<Line<'static>>, text: &str, style: Style| {
        for row in wrap_words(text, inner_width) {
            lines.push(Line::from(Span::styled(
                format!(" {}", pad_cells(&sanitize_terminal_text(&row), inner_width)),
                style,
            )));
        }
    };
    match &interaction.kind {
        InteractionRequestKind::Question { question, options } => {
            push_wrapped(&mut lines, question, palette.text);
            if !options.is_empty() {
                lines.push(Line::default());
                for (option_index, option) in options.iter().enumerate() {
                    let selected = !interaction.custom_question_answer
                        && interaction.selected_question_option == option_index;
                    lines.extend(overlay_option_lines(
                        &option.label,
                        &option.description,
                        selected,
                        inner_width,
                        palette,
                    ));
                }
                let other_selected = !interaction.custom_question_answer
                    && interaction.selected_question_option == options.len();
                lines.extend(overlay_option_lines(
                    "Outro...",
                    "",
                    other_selected,
                    inner_width,
                    palette,
                ));
            }
        }
        InteractionRequestKind::Approval { summary } => {
            push_wrapped(&mut lines, summary, palette.text);
            lines.push(Line::default());
            push_wrapped(
                &mut lines,
                &interaction_status_line(interaction),
                palette.muted,
            );
        }
        InteractionRequestKind::Input { prompt, options } => {
            push_wrapped(&mut lines, prompt, palette.text);
            if !options.is_empty() {
                push_wrapped(
                    &mut lines,
                    &format!("options: {}", options.join(" · ")),
                    palette.muted,
                );
            }
            lines.push(Line::default());
            push_wrapped(
                &mut lines,
                &interaction_status_line(interaction),
                palette.muted,
            );
        }
    }
    if interaction.response_pending {
        lines.push(Line::default());
        push_wrapped(&mut lines, "sent", palette.muted);
    } else if matches!(&interaction.kind, InteractionRequestKind::Question { .. })
        && interaction.custom_question_answer
    {
        push_wrapped(&mut lines, "type answer below", palette.muted);
    }
    lines.push(Line::default());
    lines
}

fn line_has_text(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .any(|span| !span.content.as_ref().trim().is_empty())
}

/// On short screens, keep the focused choice visible and spend remaining rows
/// on its description. Keyboard selection drives this viewport as in pickers.
fn compact_question_lines(
    interaction: &InteractionRequestState,
    inner_width: usize,
    capacity: usize,
    palette: &Palette,
) -> Option<Vec<Line<'static>>> {
    let InteractionRequestKind::Question { question, options } = &interaction.kind else {
        return None;
    };
    if options.is_empty() || capacity < 2 {
        return None;
    }
    let question = sanitize_terminal_text(question);
    let mut lines = wrap_words(&question, inner_width)
        .into_iter()
        .take(capacity.saturating_sub(1))
        .map(|row| overlay_option_row(&row, inner_width, false, palette, true))
        .collect::<Vec<_>>();
    let selected = interaction.selected_question_option.min(options.len());
    let window = visible_window(
        options.len() + 1,
        selected,
        capacity.saturating_sub(lines.len()),
        0,
    );
    for index in window {
        let label = options
            .get(index)
            .map_or("Outro...", |option| option.label.as_str());
        let selected_here = !interaction.custom_question_answer && index == selected;
        let marker = if selected_here { "[x]" } else { "[ ]" };
        let label = truncate_cells(
            &sanitize_terminal_text(label),
            inner_width.saturating_sub(4),
        );
        lines.push(overlay_option_row(
            &format!("{marker} {label}"),
            inner_width,
            selected_here,
            palette,
            true,
        ));
    }
    if let Some(option) = options.get(selected) {
        let description = sanitize_terminal_text(&option.description);
        for row in wrap_words(&description, inner_width.saturating_sub(4).max(1))
            .into_iter()
            .filter(|row| !row.is_empty())
            .take(capacity.saturating_sub(lines.len()))
        {
            lines.push(overlay_option_row(
                &format!("    {row}"),
                inner_width,
                true,
                palette,
                false,
            ));
        }
    }
    Some(lines)
}

fn question_card_width(frame_width: u16) -> u16 {
    const INSET: u16 = 2;
    const MAX_CARD: u16 = 72;
    frame_width
        .saturating_sub(INSET.saturating_mul(2))
        .clamp(1, MAX_CARD)
}

fn render_interaction_overlay(
    frame: &mut ratatui::Frame,
    composer_area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    _capabilities: Capabilities,
) {
    let Some(interaction) = state.pending_interaction() else {
        return;
    };
    if interaction.acknowledgement.is_some() {
        return;
    }
    let frame_area = frame.area();
    if frame_area.width < 24 || composer_area.y < 4 {
        return;
    }
    let width = question_card_width(composer_area.width.saturating_add(4)).min(composer_area.width);
    let inner_width = width.saturating_sub(4).max(8) as usize;
    let (title, hint) = interaction_overlay_chrome(interaction);
    let mut lines = interaction_overlay_lines(interaction, inner_width, palette);
    let max_height = composer_area.y.min(frame_area.height).max(4);
    let bordered = |content: usize| (content as u16).saturating_add(2);
    if bordered(lines.len()) > max_height {
        lines.retain(line_has_text);
    }
    if bordered(lines.len()) > max_height {
        if let Some(compact) = compact_question_lines(
            interaction,
            inner_width,
            max_height.saturating_sub(2) as usize,
            palette,
        ) {
            lines = compact;
        }
    }
    let mut height = bordered(lines.len()).clamp(4, max_height);
    let inner_rows = height.saturating_sub(2) as usize;
    if lines.len() > inner_rows {
        if inner_rows > 1 {
            lines.truncate(inner_rows.saturating_sub(1));
            lines.push(Line::from(Span::styled(
                format!(" {}", pad_cells("…", inner_width)),
                palette.muted,
            )));
        } else {
            lines.truncate(inner_rows);
        }
        height = bordered(lines.len()).clamp(4, max_height);
    }
    let hint_budget = (width as usize).saturating_sub(UnicodeWidthStr::width(title) + 4);
    let hint = truncate_cells(hint, hint_budget.max(6));
    let area = ratatui::layout::Rect {
        x: composer_area.x,
        y: composer_area.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            RatatuiBlock::default()
                .title(Line::from(Span::styled(title, palette.accent_bold)))
                .title(Line::from(Span::styled(format!(" {hint} "), palette.muted)).right_aligned())
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(palette.border_focus)
                .style(palette.surface_alt),
        ),
        area,
    );
}

fn composer_label(state: &AppState, area_width: u16, total_lines: usize) -> String {
    let lines = (total_lines > 1).then(|| format!("{total_lines} lines"));
    let question = question_composer_hint(state).map(str::to_owned);
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
    let metadata = [question, images, lines]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    if metadata.is_empty() {
        String::new()
    } else {
        format!(
            " {} ",
            truncate_cells(&metadata, area_width.saturating_sub(4) as usize)
        )
    }
}

fn render_composer(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    cache: &mut WrapCache,
) {
    if area.height == 0 {
        return;
    }
    let focused = composer_is_focused(state);
    let glyph_style = if focused {
        palette.accent
    } else {
        palette.muted
    };
    let prompt = COMPOSER_PROMPT;
    let prompt_width = UnicodeWidthStr::width(prompt);
    let boxed = area.height >= 3;
    // Keyed by composer revision + width: unchanged drafts skip the O(draft)
    // snapshot rebuild every frame.
    let snapshot_width = composer_text_budget_for_area(area.width, boxed);
    let snapshot = cache.composer_snapshot(&state.composer, snapshot_width as u16);
    let content_area = if boxed {
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
            .title_alignment(Alignment::Left);
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
    let text_budget = composer_text_budget_for_area(content_area.width, false);
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
                if is_cursor_line { prompt } else { "  " },
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
    _session_visible: bool,
    activity_visible: bool,
    cache: &mut WrapCache,
) {
    let lines = cache.footer_lines(state, area.width, area.height, activity_visible);
    let buf = frame.buffer_mut();
    buf.set_style(area, palette.surface);
    for (index, text) in lines.iter().enumerate() {
        buf.set_line(
            area.x,
            area.y + index as u16,
            &footer_line(index, area.height, text, state, activity_visible, palette),
            area.width,
        );
    }
}

/// Style footer values independently from their shortcut descriptions.  The
/// projection already owns width fitting; keeping the separator as its own
/// muted span avoids changing the measured text while making the active mode,
/// metadata, phase, or unread count easy to scan.
fn footer_line(
    index: usize,
    rows: u16,
    text: &str,
    state: &AppState,
    activity_visible: bool,
    palette: &Palette,
) -> Line<'static> {
    let segments = text.split(" · ").collect::<Vec<_>>();
    let segment_count = segments.len();
    let mut spans = Vec::with_capacity(segments.len().saturating_mul(2));
    for (segment_index, segment) in segments.into_iter().enumerate() {
        let unread_value = state.scroll.is_pinned()
            && state.scroll.unseen > 0
            && (segment.contains(" new") || segment.contains('↑'));
        let active = if rows == 1 {
            // A one-row footer has no dedicated metadata row. Keep the first
            // value prominent, except when it is a shortcut-only projection.
            if state.working {
                (!activity_visible && segment_index == 0) || unread_value
            } else if state.scroll.is_pinned() {
                unread_value
            } else {
                segment_index == 0
            }
        } else if rows == 2 {
            if index == 0 {
                // The common two-row projection combines mode and metadata.
                true
            } else {
                (!activity_visible && state.working && segment_index == 0)
                    || (!state.authenticated && segment_index == 0)
                    || unread_value
            }
        } else if index == 0 {
            segment_index == 0
        } else if index == 1 {
            // Three-row callers retain the explicit metadata row.
            true
        } else {
            (!activity_visible && state.working && segment_index == 0)
                || (!state.authenticated && segment_index == 0)
                || unread_value
        };
        spans.push(Span::styled(
            segment.to_owned(),
            if active {
                palette.accent
            } else {
                palette.muted
            },
        ));
        if segment_index + 1 < segment_count {
            spans.push(Span::styled(" · ", palette.muted));
        }
    }
    Line::from(spans)
}

fn render_search_bar(
    frame: &mut ratatui::Frame,
    scrollback: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    matches: &[usize],
) {
    let Some(search) = &state.search else {
        return;
    };
    if scrollback.width < 8 || scrollback.height == 0 {
        return;
    }
    let position = if matches.is_empty() {
        "0/0".to_owned()
    } else {
        format!(
            "{}/{}",
            search.selected.min(matches.len() - 1) + 1,
            matches.len()
        )
    };
    let safe_query = crate::markdown::sanitize_terminal_text_cow(&search.query);
    let max_width = scrollback.width.saturating_sub(2).max(6);
    let prefix = " Find: ";
    let scope = format!(" [{}] ", search.filter.label());
    let suffixes = [
        format!("{scope} {position} · Enter next · Tab filter · Esc "),
        format!("{scope}{position} · Enter · Esc "),
        format!(" {position} "),
        format!(" {position}"),
    ];
    let prefix_width = UnicodeWidthStr::width(prefix);
    let query_width = UnicodeWidthStr::width(safe_query.as_ref());
    let full_width = prefix_width
        .saturating_add(query_width)
        .saturating_add(UnicodeWidthStr::width(suffixes[0].as_str()))
        .saturating_add(1) as u16;
    let width = full_width.min(max_width).max(6);
    let available = width.saturating_sub(1) as usize;
    let suffix = suffixes
        .iter()
        .find(|suffix| {
            prefix_width
                .saturating_add(query_width)
                .saturating_add(UnicodeWidthStr::width(suffix.as_str()))
                <= available
        })
        .or_else(|| {
            suffixes.iter().rev().find(|suffix| {
                prefix_width.saturating_add(UnicodeWidthStr::width(suffix.as_str())) <= available
            })
        })
        .map(String::as_str)
        .unwrap_or("");
    let query_budget = available
        .saturating_sub(prefix_width)
        .saturating_sub(UnicodeWidthStr::width(suffix));
    let query = if safe_query.is_empty() {
        crate::view_model::truncate_display_width("Type to find", query_budget)
    } else {
        truncate_search_query(safe_query.as_ref(), query_budget)
    };
    let area = ratatui::layout::Rect {
        x: scrollback.x + 1,
        y: scrollback.y,
        width,
        height: 1,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prefix, palette.secondary),
            Span::styled(
                query,
                if safe_query.is_empty() {
                    palette.muted
                } else {
                    palette.text
                },
            ),
            Span::styled(suffix, palette.muted),
        ]))
        .style(palette.surface_alt),
        area,
    );
}

fn truncate_search_query(query: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(query) <= max_width {
        return query.to_owned();
    }
    if max_width <= 1 {
        return "…".into();
    }
    let mut tail = String::new();
    let mut used = 0usize;
    for grapheme in query.graphemes(true).rev() {
        let width = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(width) > max_width.saturating_sub(1) {
            break;
        }
        tail.insert_str(0, grapheme);
        used = used.saturating_add(width);
    }
    format!("…{tail}")
}

fn render_inspector_overlay(
    frame: &mut ratatui::Frame,
    scrollback: ratatui::layout::Rect,
    state: &AppState,
    kind: InspectorKind,
    palette: &Palette,
    cache: &mut WrapCache,
) {
    if scrollback.width < 12 || scrollback.height < 4 {
        return;
    }
    let area = inspector_overlay_area(frame.area());
    render_inspector_panel(frame, area, state, Some(kind), palette, cache);
}

fn render_inspector_panel(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    kind: Option<InspectorKind>,
    palette: &Palette,
    cache: &mut WrapCache,
) {
    if area.width < 12 || area.height < 4 {
        return;
    }
    frame.render_widget(Clear, area);
    let title = kind.map_or("Run", inspector_title);
    let hint = if kind.is_some() {
        " ↑↓ scroll · Esc close "
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
    frame.render_widget(block.style(palette.surface_alt), area);
    let metrics = inspector_panel_metrics(state, kind, area, palette, cache, true);
    let content_area = ratatui::layout::Rect {
        height: (metrics.end - metrics.start).min(usize::from(metrics.inner.height)) as u16,
        ..metrics.inner
    };
    if content_area.width > 0 && content_area.height > 0 {
        cache.selection_regions[1] = Some(content_area);
    }
    let mut visible = metrics.lines[metrics.start..metrics.end].to_vec();
    if metrics.total_rows > metrics.capacity {
        let footer = format!(
            " ↑↓ scroll · {}-{}/{} · Home/End",
            metrics.start.saturating_add(1),
            metrics.end,
            metrics.total_rows
        );
        visible.push(Line::from(Span::styled(
            truncate_cells(&footer, metrics.inner.width.saturating_sub(1) as usize),
            palette.muted,
        )));
    }
    frame.render_widget(
        Paragraph::new(visible)
            .style(palette.surface_alt)
            .wrap(Wrap { trim: false }),
        metrics.inner,
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

pub(crate) fn run_inspector_lines(
    state: &AppState,
    palette: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
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

pub(crate) fn inspector_lines(
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
                let (status, status_style, name, name_style) = if tool.historical {
                    (
                        "-",
                        palette.muted,
                        format!("{} · history", tool.name),
                        palette.muted,
                    )
                } else {
                    let (status, status_style) = match block.lifecycle {
                        BlockLifecycle::Complete => ("✓", palette.success),
                        BlockLifecycle::Failed => ("×", palette.error),
                        BlockLifecycle::Cancelled => ("×", palette.warning),
                        BlockLifecycle::Pending | BlockLifecycle::Streaming => ("·", palette.tool),
                    };
                    (status, status_style, tool.name.clone(), palette.text)
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {status} "), status_style),
                    Span::styled(truncate(&name), name_style),
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
                let (status, status_style, name, name_style) = if tool.historical {
                    (
                        "-",
                        palette.muted,
                        format!("{} · history", tool.name),
                        palette.muted,
                    )
                } else {
                    let (status, status_style) = match block.lifecycle {
                        BlockLifecycle::Complete => ("✓", palette.success),
                        BlockLifecycle::Failed => ("✕", palette.error),
                        BlockLifecycle::Cancelled => ("■", palette.warning),
                        BlockLifecycle::Pending | BlockLifecycle::Streaming => ("◌", palette.tool),
                    };
                    (status, status_style, tool.name.clone(), palette.text)
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {status} "), status_style),
                    Span::styled(truncate(&name), name_style),
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

#[allow(clippy::too_many_arguments)]
fn render_model_overlay(
    frame: &mut ratatui::Frame,
    overlay: &ModelOverlay,
    active_model: &str,
    opencode: &[crate::api::OpenCodeModelView],
    clinepass: &[crate::api::OpenCodeModelView],
    command_code: &[crate::api::OpenCodeModelView],
    zen: &[crate::api::OpenCodeModelView],
    rows: &[ModelRow],
    palette: &Palette,
) {
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
        3 => "Command Code",
        _ => "OpenCode Zen",
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
        let active = model_row_matches(row, active_model, opencode, clinepass, command_code, zen);
        let active_marker = if active { " ●" } else { "" };
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
            ModelRow::Zen(idx) => zen
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
        let content_budget = row_budget.saturating_sub(UnicodeWidthStr::width(active_marker));
        let line = format!("{marker}{}", truncate_cells(&content, content_budget));
        let style = if selected {
            palette.accent_bold
        } else {
            palette.text
        };
        let mut spans = vec![Span::styled(sanitize_terminal_text(&line), style)];
        if active {
            spans.push(Span::styled(active_marker, palette.success));
        }
        lines.push(menu_line(
            spans,
            selected,
            usize::from(area.width.saturating_sub(2)),
            palette,
        ));
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

fn model_row_matches(
    row: &ModelRow,
    active_model: &str,
    opencode: &[crate::api::OpenCodeModelView],
    clinepass: &[crate::api::OpenCodeModelView],
    command_code: &[crate::api::OpenCodeModelView],
    zen: &[crate::api::OpenCodeModelView],
) -> bool {
    match row {
        ModelRow::Header(_) => false,
        ModelRow::Alias(alias) => alias.id() == active_model,
        ModelRow::Catalog(index) => opencode
            .get(*index)
            .is_some_and(|model| model.id == active_model),
        ModelRow::ClinePass(index) => clinepass
            .get(*index)
            .is_some_and(|model| model.id == active_model),
        ModelRow::CommandCode(index) => command_code
            .get(*index)
            .is_some_and(|model| model.id == active_model),
        ModelRow::Zen(index) => zen
            .get(*index)
            .is_some_and(|model| model.id == active_model),
    }
}

fn render_mcp_overlay(
    frame: &mut ratatui::Frame,
    overlay: &McpOverlay,
    servers: &[McpServerView],
    palette: &Palette,
) {
    let frame_area = frame.area();
    let width = ((u32::from(frame_area.width) * 9) / 10) as u16;
    let width = width
        .clamp(40, 96)
        .min(frame_area.width.saturating_sub(2).max(1));
    let row_budget = width.saturating_sub(4) as usize;
    let position = if servers.is_empty() {
        "0/0".to_owned()
    } else {
        format!("{}/{}", overlay.selected.saturating_add(1), servers.len())
    };
    let (footer, footer_style) = match &overlay.confirm_remove {
        Some(name) => (
            format!("remove {name}? y/Enter confirms · n/Esc cancels"),
            palette.warning,
        ),
        None => (
            format!("{position} · Enter test · r reconnect · x disconnect · d remove · Esc"),
            palette.muted,
        ),
    };
    let footer_rows = wrap_words(&footer, row_budget.max(1));
    let max_inner = frame_area.height.saturating_sub(4) as usize;
    let list_budget = max_inner.saturating_sub(footer_rows.len().max(1)).max(1);
    let mut entries: Vec<Vec<Line>> = Vec::new();
    if servers.is_empty() {
        entries.push(vec![Line::from(Span::styled(
            " no servers configured — /mcp add <name> <command>",
            palette.muted,
        ))]);
    } else {
        for (index, server) in servers.iter().enumerate() {
            entries.push(mcp_entry_lines(
                server,
                index == overlay.selected,
                row_budget,
                palette,
            ));
        }
    }
    let shortest = entries.iter().map(Vec::len).min().unwrap_or(1).max(1);
    let capacity = (list_budget / shortest).max(1).min(entries.len());
    let window = visible_window(
        entries.len(),
        overlay.selected.min(entries.len().saturating_sub(1)),
        capacity,
        overlay.viewport_start,
    );
    let mut lines: Vec<Line> = Vec::new();
    for index in window {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(entries[index].clone());
    }
    while lines.len() > list_budget {
        lines.pop();
    }
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    for row in footer_rows {
        lines.push(Line::from(Span::styled(format!(" {row}"), footer_style)));
    }
    let height = u16::try_from(lines.len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(8, frame_area.height.saturating_sub(2).max(8));
    let area = centered(frame_area, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" MCP servers ", palette)),
        area,
    );
}

fn mcp_entry_lines<'a>(
    server: &McpServerView,
    selected: bool,
    row_budget: usize,
    palette: &'a Palette,
) -> Vec<Line<'a>> {
    let marker = if selected { "> " } else { "  " };
    let (glyph, state_style, state_label, detail) = match server.status {
        McpStatusView::Ready => (
            '\u{25CF}',
            palette.success,
            server
                .tools
                .map(|count| format!("{count} tools"))
                .unwrap_or_else(|| "ready".to_owned()),
            None,
        ),
        McpStatusView::Connecting => ('\u{25CC}', palette.warning, "connecting".to_owned(), None),
        McpStatusView::Disconnected => ('\u{25CC}', palette.muted, "disconnected".to_owned(), None),
        McpStatusView::Disabled => ('\u{25CB}', palette.muted, "disabled".to_owned(), None),
        McpStatusView::Failed => (
            '\u{2715}',
            palette.error,
            "failed".to_owned(),
            server.error.as_deref(),
        ),
    };
    let title = format!(
        "{marker}{glyph} {} · {} · {state_label}",
        server.name, server.transport
    );
    let title_style = if selected {
        palette.accent_bold
    } else {
        palette.text
    };
    let mut lines = vec![Line::from(Span::styled(
        sanitize_terminal_text(&truncate_cells(&title, row_budget)),
        title_style,
    ))];
    let indent = "    ";
    let body_width = row_budget.saturating_sub(indent.len()).max(1);
    for row in wrap_words(&sanitize_terminal_text(&server.target), body_width) {
        lines.push(Line::from(Span::styled(
            format!("{indent}{row}"),
            palette.muted,
        )));
    }
    if let Some(error) = detail {
        for row in wrap_words(&sanitize_terminal_text(error), body_width) {
            lines.push(Line::from(Span::styled(
                format!("{indent}{row}"),
                state_style,
            )));
        }
    }
    lines
}

fn render_effort_overlay(frame: &mut ratatui::Frame, overlay: &EffortOverlay, palette: &Palette) {
    let levels = ReasoningEffort::supported(overlay.model);
    let area = centered(frame.area(), 72, (levels.len() * 2 + 6) as u16);
    let mut lines = vec![Line::default()];
    for (index, effort) in levels.iter().enumerate() {
        let selected = index == overlay.selected;
        let marker = if selected { "> " } else { "  " };
        let style = if selected {
            palette.accent_bold
        } else {
            palette.text
        };
        lines.push(menu_line(
            vec![Span::styled(
                format!("{marker}{:<6}  {}", effort.label(), effort.description()),
                style,
            )],
            selected,
            usize::from(area.width.saturating_sub(2)),
            palette,
        ));
        if index + 1 < levels.len() {
            lines.push(Line::default());
        }
    }
    lines.extend([
        Line::default(),
        Line::from(Span::styled(
            format!(
                " Speed: {} · Tab toggle",
                if overlay.fast {
                    "Fast (higher usage)"
                } else {
                    "Normal"
                }
            ),
            palette.muted,
        )),
        Line::from(Span::styled(" Enter select · Esc back", palette.muted)),
    ]);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Select reasoning effort ", palette)),
        area,
    );
}

fn render_login_overlay(frame: &mut ratatui::Frame, overlay: &LoginOverlay, palette: &Palette) {
    if let LoginStage::ApiKey(key) = &overlay.stage {
        let title = match overlay.provider() {
            LoginProvider::OpenCodeGo => " OpenCode Go API key ",
            LoginProvider::OpenCodeZen => " OpenCode Zen API key ",
            LoginProvider::ClinePass => " ClinePass API key ",
            LoginProvider::CommandCode => " Command Code API key ",
            LoginProvider::Anthropic => " Anthropic API key ",
            LoginProvider::OpenAiCodex => " OpenAI Codex API key ",
            LoginProvider::Xai => " xAI API key ",
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
        LoginProvider::Xai,
        LoginProvider::OpenCodeZen,
    ];
    let frame_area = frame.area();
    let status = if let Some(code) = &overlay.user_code {
        Some(format!(" Code: {}", sanitize_terminal_text(code.expose())))
    } else if let Some(progress) = &overlay.progress {
        Some(format!(" {}", sanitize_terminal_text(progress)))
    } else {
        overlay
            .auth_url
            .as_ref()
            .map(|url| format!(" Open: {}", sanitize_terminal_text(url.expose())))
    };
    let hint = if status.is_some() {
        " Esc cancel · Ctrl+C cancel"
    } else {
        " Enter connect · Esc cancel"
    };
    // Keep one compact row per provider and reserve the final row for the
    // essential action hint.  The selected provider is windowed into the
    // available rows, so End remains visible even on a short terminal.
    let status_rows = usize::from(status.is_some());
    let requested_inner = providers.len().saturating_add(status_rows + 1);
    let height = u16::try_from(requested_inner.saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(8, frame_area.height.saturating_sub(2).max(8));
    let area = centered(frame_area, 58, height);
    let inner_rows = usize::from(area.height.saturating_sub(2));
    let provider_capacity = inner_rows.saturating_sub(status_rows + 1).max(1);
    let selected = overlay.selected.min(providers.len().saturating_sub(1));
    let window = visible_window(providers.len(), selected, provider_capacity, 0);
    let mut lines: Vec<Line> = providers[window.clone()]
        .iter()
        .enumerate()
        .map(|(offset, provider)| {
            let index = window.start + offset;
            let selected_here = index == selected;
            let marker = if selected_here { "> " } else { "  " };
            let style = if selected_here {
                palette.accent_bold
            } else {
                palette.text
            };
            menu_line(
                vec![Span::styled(format!("{marker}{}", provider.label()), style)],
                selected_here,
                usize::from(area.width.saturating_sub(2)),
                palette,
            )
        })
        .collect();
    if let Some(status) = status {
        lines.push(Line::from(Span::styled(
            truncate_cells(&status, area.width.saturating_sub(4) as usize),
            palette.muted,
        )));
    }
    lines.push(Line::from(Span::styled(hint, palette.muted)));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Connect provider ", palette)),
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
                palette.accent_bold
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
    viewport_start: usize,
    palette: &Palette,
    cache: &mut WrapCache,
) {
    let matches = cache.palette_matches(query);
    let rows = grouped_command_lines(&matches, Some(selected), palette);
    // The viewport is measured in rendered rows, not command indices: group
    // headings consume the same vertical space as a command and therefore
    // must participate in keeping the focused command visible.
    let requested_height = u16::try_from(rows.len().saturating_add(3)).unwrap_or(u16::MAX);
    let height = requested_height.clamp(6, frame.area().height.saturating_sub(2).max(6));
    let area = centered(frame.area(), 42, height);
    let capacity = usize::from(area.height.saturating_sub(3)).max(1);
    let selected_row = rows
        .iter()
        .position(|row| row.command_index == Some(selected));
    let preferred_row = rows
        .iter()
        .position(|row| row.command_index == Some(viewport_start))
        .unwrap_or(0);
    let start = ensure_palette_row_visible(
        preferred_row,
        selected_row.unwrap_or(preferred_row),
        rows.len(),
        capacity,
    );
    let end = start.saturating_add(capacity).min(rows.len());
    let query_row = if query.is_empty() {
        Span::styled(" Type to filter…", palette.muted)
    } else {
        Span::styled(
            format!(
                " {}",
                truncate_cells(
                    &sanitize_terminal_text(query),
                    area.width.saturating_sub(2) as usize,
                )
            ),
            palette.text,
        )
    };
    let mut lines = vec![Line::from(query_row)];
    lines.extend(rows[start..end].iter().map(|row| row.line.clone()));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Commands ", palette)),
        area,
    );
}

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn spinner_glyph(frame: u64, capabilities: Capabilities) -> char {
    if capabilities.reduced_motion {
        glyph(capabilities, '\u{25cb}', '~')
    } else if capabilities.color_depth == ColorDepth::None {
        '~'
    } else {
        SPINNER_FRAMES[(frame as usize) % SPINNER_FRAMES.len()]
    }
}

fn palette_command_line(command: &str, selected_here: bool, palette: &Palette) -> Line<'static> {
    const MODAL_INNER_WIDTH: usize = 40; // modal width 42 minus borders
    let marker = if selected_here { "> " } else { "  " };
    let style = if selected_here {
        palette.accent_bold
    } else {
        palette.text
    };
    let head = format!("{marker}{command}");
    let detail = crate::reducer::palette_description(command);
    if detail.is_empty() {
        return menu_line(
            vec![Span::styled(head, style)],
            selected_here,
            MODAL_INNER_WIDTH,
            palette,
        );
    }
    let budget = MODAL_INNER_WIDTH.saturating_sub(UnicodeWidthStr::width(head.as_str()) + 2);
    menu_line(
        vec![
            Span::styled(head, style),
            Span::raw("  "),
            Span::styled(truncate_cells(detail, budget), palette.muted),
        ],
        selected_here,
        MODAL_INNER_WIDTH,
        palette,
    )
}

struct PaletteRow {
    command_index: Option<usize>,
    line: Line<'static>,
}

fn grouped_command_lines(
    commands: &[&str],
    selected: Option<usize>,
    palette: &Palette,
) -> Vec<PaletteRow> {
    let mut lines = Vec::new();
    for (group, members) in crate::reducer::COMMAND_GROUPS {
        let visible: Vec<(usize, &str)> = commands
            .iter()
            .enumerate()
            .filter_map(|(index, command)| members.contains(command).then_some((index, *command)))
            .collect();
        if visible.is_empty() {
            continue;
        }
        lines.push(PaletteRow {
            command_index: None,
            line: Line::from(Span::styled((*group).to_owned(), palette.muted)),
        });
        for (index, command) in visible {
            lines.push(PaletteRow {
                command_index: Some(index),
                line: palette_command_line(command, selected == Some(index), palette),
            });
        }
    }
    let ungrouped = commands
        .iter()
        .enumerate()
        .map(|(index, command)| (index, *command))
        .filter(|(_, command)| {
            !crate::reducer::COMMAND_GROUPS
                .iter()
                .any(|(_, members)| members.contains(command))
        })
        .collect::<Vec<_>>();
    if !ungrouped.is_empty() {
        lines.push(PaletteRow {
            command_index: None,
            line: Line::from(Span::styled("skills", palette.muted)),
        });
        for (index, command) in ungrouped {
            lines.push(PaletteRow {
                command_index: Some(index),
                line: palette_command_line(command, selected == Some(index), palette),
            });
        }
    }
    lines
}

fn ensure_palette_row_visible(
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

fn modal_block<'a>(title: &'a str, palette: &'a Palette) -> RatatuiBlock<'a> {
    RatatuiBlock::default()
        .title(title)
        .title_style(palette.accent_bold)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(palette.border)
        .style(palette.surface_alt)
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

fn workspace_band(workspace: &WorkspaceRegions) -> ratatui::layout::Rect {
    match workspace.inspector {
        Some(inspector) => ratatui::layout::Rect {
            x: workspace.transcript.x,
            y: workspace.transcript.y,
            width: workspace.transcript.width.saturating_add(inspector.width),
            height: workspace.transcript.height,
        },
        None => workspace.transcript,
    }
}

fn align_to_band(
    area: ratatui::layout::Rect,
    band: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let x = area.x.max(band.x);
    let end = (area.x + area.width).min(band.x + band.width);
    ratatui::layout::Rect {
        x,
        width: end.saturating_sub(x),
        ..area
    }
}

fn chrome_area(area: ratatui::layout::Rect, band: ratatui::layout::Rect) -> ratatui::layout::Rect {
    horizontal_inset(align_to_band(area, band))
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
        control_requires_data_barrier, footer_line, measure_scrollback, menu_line, receive_batch,
        terminal_action, toast_row_count, update_visible_stream_state, LaneDrain, Palette,
        CONTROL_BATCH_LIMIT, STREAM_BATCH_LIMIT,
    };
    use crate::api::{
        McpServerView, McpStatusView, TodoItemStatus, TodoItemView, ToolBatchId, ToolCallId,
        UiEvent,
    };
    use crate::app::{
        ActivityPhase, ActivityState, AppState, FrameClock, McpOverlay, SlashSuggestions,
    };
    use crate::reducer::{reduce, Action, ScrollIntent};
    use crate::render::WrapCache;
    use crate::runtime::render_frame;
    use crate::selection::{ScreenPos, ScreenSelection};
    use crate::theme::{Capabilities, ColorDepth};
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Span;
    use ratatui::widgets::Paragraph;
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

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn state_with_transcript() -> AppState {
        let mut state = AppState::new();
        state.apply_event(UiEvent::UserMessageAdded {
            text: "hello from the transcript".into(),
        });
        state
    }

    #[test]
    fn selected_menu_line_fills_the_row_and_keeps_no_color_fallbacks() {
        let palette = Palette::of(caps());
        assert_ne!(palette.menu_selected.bg, palette.surface_alt.bg);
        assert_eq!(palette.menu_selected.bg, Some(Color::Rgb(0x2A, 0x2A, 0x2A)));
        let ansi = Palette::of(Capabilities {
            color_depth: ColorDepth::Ansi16,
            ..caps()
        });
        assert_ne!(ansi.menu_selected.bg, ansi.surface_alt.bg);
        assert_eq!(ansi.menu_selected.bg, Some(Color::DarkGray));
        let ansi256 = Palette::of(Capabilities {
            color_depth: ColorDepth::Ansi256,
            ..caps()
        });
        assert_ne!(ansi256.menu_selected.bg, ansi256.surface_alt.bg);

        let mut terminal = Terminal::new(TestBackend::new(20, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(vec![menu_line(
                        vec![Span::styled("> Option", palette.accent_bold)],
                        true,
                        20,
                        &palette,
                    )]),
                    frame.area(),
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        for x in 0..20 {
            assert_eq!(buffer[(x, 0)].bg, Color::Rgb(0x2A, 0x2A, 0x2A));
        }

        let no_color = Palette::of(Capabilities {
            color_depth: ColorDepth::None,
            ..caps()
        });
        assert_eq!(no_color.menu_selected.bg, Some(Color::Reset));
        let fallback = menu_line(
            vec![Span::styled("> Option", no_color.accent_bold)],
            true,
            20,
            &no_color,
        );
        assert_eq!(fallback.spans[0].content.chars().next(), Some('>'));
        assert!(fallback.spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn wide_terminal_does_not_open_the_run_inspector_by_default() {
        let state = state_with_transcript();
        let frame = render_to_string(&state, 160, 24);
        assert!(
            !frame.contains("Ctrl+J  activity"),
            "wide transcript must not dock the Run inspector:\n{frame}"
        );
        assert!(
            !frame.contains("Ctrl+D  changes"),
            "wide transcript must not dock inspector shortcuts:\n{frame}"
        );
    }

    #[test]
    fn explicit_inspector_still_docks_on_a_wide_terminal() {
        let mut state = state_with_transcript();
        state.inspector.active = Some(crate::inspector::InspectorKind::Activity);
        let frame = render_to_string(&state, 160, 24);
        assert!(
            frame.contains("Activity"),
            "Ctrl+J must still dock the activity inspector:\n{frame}"
        );
    }

    fn complete_tool(state: &mut AppState, batch: &str, call: &str, name: &str, duration_ms: u64) {
        state.apply_event(UiEvent::ToolStarted {
            batch_id: ToolBatchId(batch.into()),
            call_id: ToolCallId(call.into()),
            name: name.into(),
            arguments_summary: String::new(),
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id: ToolBatchId(batch.into()),
            call_id: ToolCallId(call.into()),
            name: name.into(),
            success: true,
            duration_ms,
        });
    }

    #[test]
    fn tools_sit_flush_against_assistant_and_thinking() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::UserMessageAdded {
            text: "question".into(),
        });
        state.apply_event(UiEvent::run_started(1));
        state.apply_event(UiEvent::ThinkingStarted);
        state.apply_event(UiEvent::ThinkingDelta {
            text: "plan".into(),
        });
        state.apply_event(UiEvent::ThinkingEnded);
        complete_tool(&mut state, "b1", "c1", "list", 3);
        complete_tool(&mut state, "b1", "c2", "read", 4);
        state.apply_event(UiEvent::AssistantDelta {
            text: "Vou explorar o projeto.\n\n".into(),
        });
        state.apply_event(UiEvent::AssistantEnded);
        complete_tool(&mut state, "b2", "c3", "list", 5);
        complete_tool(&mut state, "b2", "c4", "read", 7);
        state.apply_event(UiEvent::ThinkingStarted);
        state.apply_event(UiEvent::ThinkingDelta {
            text: "still thinking".into(),
        });

        let frame = render_to_string(&state, 80, 24);
        let rows: Vec<&str> = frame.lines().collect();
        let explore = rows
            .iter()
            .position(|row| row.contains("Vou explorar"))
            .unwrap_or_else(|| panic!("assistant body missing\n{frame}"));
        let second_tools = rows
            .iter()
            .rposition(|row| row.contains("Read, list"))
            .unwrap_or_else(|| panic!("second tools missing\n{frame}"));
        let thinking = rows
            .iter()
            .position(|row| row.contains("Thinking"))
            .unwrap_or_else(|| panic!("thinking missing\n{frame}"));
        assert_eq!(
            second_tools,
            explore + 1,
            "assistant body must sit flush against the next tool row\n{frame}"
        );
        assert_eq!(
            thinking,
            second_tools + 1,
            "thinking must sit flush against the tool row\n{frame}"
        );
    }

    #[test]
    fn left_drag_selects_and_right_click_copies_or_pastes() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::AssistantDelta {
            text: "hello world".into(),
        });
        let mut cache = WrapCache::default();
        let size = (80, 24);
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
            .unwrap();
        let area = cache.selection_regions[0].unwrap();
        let row = area.bottom() - 1;
        let start = terminal_action(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, row),
            &state,
            size,
            &mut cache,
        );
        reduce(&mut state, start.expect("start"));
        let drag = terminal_action(
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 6, row),
            &state,
            size,
            &mut cache,
        );
        reduce(&mut state, drag.expect("drag"));
        assert_eq!(
            state.selection,
            Some(ScreenSelection {
                anchor: ScreenPos::new(2, row),
                head: ScreenPos::new(6, row),
            })
        );
        let mut copied = String::new();
        terminal
            .draw(|frame| {
                render_frame(frame, &state, caps(), &mut cache);
                copied = super::extract_visible_selection(frame, &state, &cache);
            })
            .unwrap();
        assert_eq!(copied, "hello");
        state.selection_text = copied;
        let right = terminal_action(
            mouse_event(MouseEventKind::Down(MouseButton::Right), 6, 1),
            &state,
            size,
            &mut cache,
        );
        assert_eq!(right, Some(Action::MouseSecondary));
        assert!(reduce(&mut state, right.unwrap())
            .contains(&crate::reducer::Effect::CopyToClipboard("hello".into())));
        let middle = terminal_action(
            mouse_event(MouseEventKind::Down(MouseButton::Middle), 6, 1),
            &state,
            size,
            &mut cache,
        );
        assert_eq!(middle, Some(Action::RequestClipboardPaste));
    }

    #[test]
    fn selection_stays_in_transcript_when_dragged_over_composer() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::AssistantDelta {
            text: "alpha\nbeta".into(),
        });
        state.apply_event(UiEvent::AssistantEnded);
        state.composer.insert_text("PRIVATE DRAFT");
        let mut cache = WrapCache::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..24)
            .find(|y| {
                (0..80)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("alpha")
            })
            .unwrap();
        for event in [
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, row),
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 70, 23),
            mouse_event(MouseEventKind::Up(MouseButton::Left), 70, 23),
        ] {
            let action = terminal_action(event, &state, (80, 24), &mut cache).unwrap();
            reduce(&mut state, action);
        }
        assert!(
            state.selection_text.is_empty(),
            "drag has not been painted yet"
        );
        let copy_event = mouse_event(MouseEventKind::Down(MouseButton::Right), 70, 23);
        assert!(super::is_selection_copy_event(&copy_event));
        let mut copied = String::new();
        terminal
            .draw(|frame| {
                render_frame(frame, &state, caps(), &mut cache);
                copied = super::extract_visible_selection(frame, &state, &cache);
            })
            .unwrap();
        assert!(
            copied.contains("alpha") && copied.contains("beta"),
            "{copied:?}"
        );
        assert!(
            !copied.contains("PRIVATE DRAFT") && !copied.contains("Ctrl+C"),
            "{copied:?}"
        );
        let selected_bg = Palette::of(caps()).selection;
        let area = state.selection_area.unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..24 {
            for x in 0..80 {
                if buffer[(x, y)].bg == selected_bg {
                    assert!(super::area_contains(area, x, y));
                    assert!(x < 10, "padding at {x},{y} was highlighted");
                }
            }
        }
        state.selection_text = copied.clone();
        let action = terminal_action(copy_event, &state, (80, 24), &mut cache).unwrap();
        assert!(
            reduce(&mut state, action).contains(&crate::reducer::Effect::CopyToClipboard(copied))
        );
        let action = terminal_action(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 21),
            &state,
            (80, 24),
            &mut cache,
        )
        .unwrap();
        reduce(&mut state, action);
        assert!(
            state.selection.is_none(),
            "composer does not start transcript selection"
        );
    }

    #[test]
    fn selection_inside_inspector_excludes_border_and_transcript() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::AssistantDelta {
            text: "TRANSCRIPT ONLY".into(),
        });
        state.activity = None;
        state.inspector.active = Some(crate::inspector::InspectorKind::Activity);
        let mut cache = WrapCache::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
            .unwrap();
        let area = cache.selection_regions[1].unwrap();
        for event in [
            mouse_event(MouseEventKind::Down(MouseButton::Left), area.x, area.y),
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 0, 23),
        ] {
            let action = terminal_action(event, &state, (120, 24), &mut cache).unwrap();
            reduce(&mut state, action);
        }
        let mut copied = String::new();
        terminal
            .draw(|frame| {
                render_frame(frame, &state, caps(), &mut cache);
                copied = super::extract_visible_selection(frame, &state, &cache);
            })
            .unwrap();
        assert!(copied.contains("No activity yet"), "{copied:?}");
        assert!(
            !copied.contains("TRANSCRIPT ONLY")
                && !copied.contains("scroll")
                && !copied.contains('│'),
            "{copied:?}"
        );
    }

    #[test]
    fn wheel_still_scrolls_when_mouse_is_captured() {
        let state = AppState::new();
        let mut cache = WrapCache::default();
        let action = terminal_action(
            mouse_event(MouseEventKind::ScrollUp, 0, 0),
            &state,
            (80, 24),
            &mut cache,
        );
        assert!(matches!(
            action,
            Some(Action::Scroll {
                intent: ScrollIntent::Up,
                ..
            })
        ));
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
    fn toast_deadline_tracks_highlight_then_expiry_and_skips_hidden_rows() {
        let mut state = AppState::new();
        state.push_notification("notice".into());
        let count = toast_row_count(&state, 24);
        assert_eq!(count, 1);
        assert_eq!(
            super::next_toast_visual_deadline_ms(&state, 0, count, true),
            Some(super::INFO_TOAST_HIGHLIGHT_MS)
        );
        assert_eq!(
            super::next_toast_visual_deadline_ms(
                &state,
                super::INFO_TOAST_HIGHLIGHT_MS,
                count,
                true,
            ),
            Some(crate::app::INFO_TOAST_TTL_MS)
        );
        assert_eq!(
            super::next_toast_visual_deadline_ms(&state, 0, count, false),
            Some(crate::app::INFO_TOAST_TTL_MS)
        );

        state.mcp_overlay = Some(McpOverlay::default());
        let hidden_count = toast_row_count(&state, 24);
        assert_eq!(hidden_count, 0);
        assert_eq!(
            super::next_toast_visual_deadline_ms(&state, 0, hidden_count, true),
            None
        );
    }

    #[test]
    fn toast_is_bold_only_during_initial_highlight_and_reduced_motion_is_stable() {
        let mut state = AppState::new();
        state.push_notification("notice".into());
        let render = |state: &AppState, capabilities| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
            terminal
                .draw(|frame| {
                    super::render_frame(frame, state, capabilities, &mut WrapCache::default())
                })
                .expect("draw");
            terminal.backend().buffer().clone()
        };
        let notice_modifier = |buffer: &ratatui::buffer::Buffer| {
            for y in 0..buffer.area.height {
                let row = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>();
                if let Some(x) = row.find("notice") {
                    return buffer[(x as u16, y)].modifier;
                }
            }
            panic!("notice cell")
        };

        let highlighted = render(&state, caps());
        assert!(notice_modifier(&highlighted).contains(Modifier::BOLD));

        state.clock.elapsed_ms = super::INFO_TOAST_HIGHLIGHT_MS;
        let stable = render(&state, caps());
        assert!(!notice_modifier(&stable).contains(Modifier::BOLD));
        state.clock.elapsed_ms += 1_000;
        assert_eq!(stable, render(&state, caps()));

        let reduced = Capabilities {
            reduced_motion: true,
            ..caps()
        };
        state.clock.elapsed_ms = 0;
        let reduced_initial = render(&state, reduced);
        assert!(!notice_modifier(&reduced_initial).contains(Modifier::BOLD));
        state.clock.elapsed_ms = super::INFO_TOAST_HIGHLIGHT_MS - 1;
        assert_eq!(reduced_initial, render(&state, reduced));
    }

    fn mcp_server_view(
        name: &str,
        target: &str,
        status: McpStatusView,
        error: Option<&str>,
    ) -> McpServerView {
        McpServerView {
            name: name.into(),
            transport: "stdio",
            target: target.into(),
            status,
            tools: None,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn mcp_overlay_keeps_error_and_hints_on_their_own_rows() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.mcp_overlay = Some(McpOverlay {
            selected: 1,
            ..McpOverlay::default()
        });
        state.mcp_servers = vec![
            mcp_server_view(
                "burp-hunt",
                r"C:\Users\User\AppData\Local\Programs\Python\Python312\python.exe -B proxy.py",
                McpStatusView::Disconnected,
                None,
            ),
            mcp_server_view(
                "chrome-devtools",
                "npx -y chrome-devtools-mcp@latest",
                McpStatusView::Failed,
                Some("%1 não é um aplicativo Win32 válido. (os error 193)"),
            ),
        ];
        let frame = render_to_string(&state, 80, 24);
        assert!(
            frame.contains("chrome-devtools") && frame.contains("Enter test"),
            "{frame}"
        );
        assert!(
            frame.contains("os error 193") && frame.contains("python.exe"),
            "target and error must remain readable:\n{frame}"
        );
        assert!(
            !frame.contains("npx -y chrome-devtools-mcp@latest %1") && !frame.contains("Entertest"),
            "error and hints must not be jammed onto one truncated row:\n{frame}"
        );
    }

    #[test]
    fn mcp_overlay_hides_status_toast() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.notifications.push(
            "mcp chrome-devtools: %1 não é um aplicativo Win32 válido. (os error 193)".into(),
        );
        state.mcp_overlay = Some(McpOverlay::default());
        state.mcp_servers = vec![mcp_server_view(
            "chrome-devtools",
            "npx -y chrome-devtools-mcp@latest",
            McpStatusView::Failed,
            Some("%1 não é um aplicativo Win32 válido. (os error 193)"),
        )];
        let frame = render_to_string(&state, 80, 24);
        assert!(
            !frame.contains("mcp chrome-devtools:"),
            "toast must not sit on the overlay:\n{frame}"
        );
        assert_eq!(toast_row_count(&state, 24), 0);
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
        assert!(!wide.contains("turn 1/128"), "{wide}");
        assert!(!wide.contains("reads 0/96"), "{wide}");
        assert!(!wide.contains("edits 0/32"), "{wide}");
        assert!(
            !wide.contains("Ctrl+C stop"),
            "cancel is already in the footer: {wide}"
        );
        assert!(wide.contains("Ctrl+C cancel"), "{wide}");
        state.turns_used = 103;
        state.tools_used_read = 77;
        state.tools_used_mutating = 2;
        let counters = render_to_string(&state, 80, 24);
        assert!(counters.contains("turn 103/128"), "{counters}");
        assert!(counters.contains("reads 77/96"), "{counters}");
        assert!(!counters.contains("edits 2/32"), "{counters}");
        let narrow = render_to_string(&state, 71, 24);
        assert!(!narrow.contains("turn 103/128"), "{narrow}");
    }

    #[test]
    fn footer_model_keeps_identity_and_effort_with_cell_safe_elision() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.model = "model-宇宙-with-a-very-long-provider-qualified-name".into();
        for width in [38, 58, 78, 118] {
            let label = crate::view_model::model_metadata(&state, width as usize - 2);
            assert!(label.contains("model-"), "{width}: {label}");
            assert!(
                label.contains("(high)"),
                "effort stays beside the model: {width}: {label}"
            );
            assert!(
                !label.contains("Auto") || width < 78,
                "metadata leaves mode to its own footer row: {width}: {label}"
            );
            assert!(unicode_width::UnicodeWidthStr::width(label.as_str()) <= width as usize - 2);
            assert_eq!(label.contains('…'), width < 78, "{width}: {label}");
        }
        state.authenticated = false;
        assert!(super::composer_label(&state, 38, 1).is_empty());
        assert!(crate::view_model::model_metadata(&state, 38).is_empty());
    }

    #[test]
    fn footer_values_are_accented_while_shortcut_descriptions_stay_muted() {
        let mut state = AppState::new();
        state.authenticated = true;
        let palette = super::Palette::of(caps());
        let line = footer_line(
            0,
            2,
            "Auto · GPT-5.6 Sol (high) · ctx ~7%",
            &state,
            true,
            &palette,
        );
        assert_eq!(line.spans[0].style, palette.accent);
        assert_eq!(line.spans[1].style, palette.muted);
        assert_eq!(line.spans[2].style, palette.accent);
        assert_eq!(line.spans[3].style, palette.muted);
        assert_eq!(line.spans[4].style, palette.accent);
    }

    #[test]
    fn footer_highlights_hidden_phase_and_unread_value_without_highlighting_controls() {
        let mut state = AppState::new();
        state.working = true;
        state.scroll.mode = crate::app::FollowMode::Top;
        state.scroll.unseen = 4;
        let palette = super::Palette::of(caps());
        let line = footer_line(
            1,
            2,
            "Thinking · Esc stop · 4 new · End latest",
            &state,
            false,
            &palette,
        );
        assert_eq!(line.spans[0].style, palette.accent);
        assert_eq!(line.spans[2].style, palette.muted);
        assert_eq!(line.spans[4].style, palette.accent);
        assert_eq!(line.spans[6].style, palette.muted);

        let visible_controls =
            footer_line(0, 1, "Esc stop · Ctrl+C cancel", &state, true, &palette);
        assert!(visible_controls
            .spans
            .iter()
            .all(|span| span.style == palette.muted));
    }

    #[test]
    fn todo_active_marker_is_stable_when_the_primary_indicator_animates() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.working = true;
        state.todo_dock_open = true;
        state.todo_items = vec![TodoItemView {
            title: "inspect state".into(),
            status: TodoItemStatus::InProgress,
        }];

        let render = |state: &AppState| {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
            terminal
                .draw(|frame| super::render_frame(frame, state, caps(), &mut WrapCache::default()))
                .expect("draw");
            terminal.backend().buffer().clone()
        };
        let first = render(&state);
        state.clock.frame = 1;
        let second = render(&state);

        let todo_marker = |buffer: &ratatui::buffer::Buffer| {
            let row = (0..buffer.area.height)
                .find(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains("TODO")
                })
                .expect("TODO row");
            let marker_x = (0..buffer.area.width)
                .find(|x| matches!(buffer[(*x, row)].symbol(), "◌" | "~"))
                .expect("TODO active marker");
            buffer[(marker_x, row)].symbol().to_owned()
        };
        assert_eq!(todo_marker(&first), todo_marker(&second));
        assert_ne!(
            first, second,
            "the activity rail/thinking indicator should remain the animated owner"
        );
    }

    #[test]
    fn thinking_pulse_moves_to_visible_header_and_freezes_with_motion_disabled() {
        use ratatui::backend::TestBackend;
        let mut state = AppState::new();
        state.authenticated = true;
        state.apply_event(UiEvent::run_started(1));
        state.apply_event(UiEvent::UserMessageAdded {
            text: "history\n".repeat(80),
        });
        state.apply_event(UiEvent::ThinkingStarted);
        state.apply_event(UiEvent::ThinkingDelta {
            text: "Inspect the current state".into(),
        });
        let render = |state: &AppState, capabilities| {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| {
                    super::render_frame(frame, state, capabilities, &mut WrapCache::default())
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        for at_top in [false, true] {
            if at_top {
                state.scroll.mode = crate::app::FollowMode::Top;
            }
            state.clock.frame = 0;
            let first = render(&state, caps());
            state.clock.frame = 6;
            let next = render(&state, caps());
            let changed = first
                .content
                .iter()
                .zip(&next.content)
                .enumerate()
                .filter_map(|(i, (a, b))| (a != b).then_some(i))
                .collect::<Vec<_>>();
            assert_eq!(
                changed.len(),
                1,
                "only one pulse across transcript and rail"
            );
            let thinking_rows = (0..24)
                .filter(|y| {
                    (0..80)
                        .map(|x| first[(x, *y)].symbol())
                        .collect::<String>()
                        .contains("Thinking")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                thinking_rows.len(),
                1,
                "Thinking belongs to the visible header or the ActivityRail, never both"
            );
            assert_eq!(
                changed[0] / 80,
                thinking_rows[0] as usize,
                "pulse belongs to the visible reasoning header, or rail if offscreen"
            );
            for capabilities in [
                Capabilities {
                    reduced_motion: true,
                    ..caps()
                },
                Capabilities {
                    color_depth: ColorDepth::None,
                    ..caps()
                },
            ] {
                let still = render(&state, capabilities);
                state.clock.frame = 12;
                assert_eq!(still, render(&state, capabilities));
                state.clock.frame = 6;
            }
        }
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        let finished = render(&state, caps());
        state.clock.frame = 18;
        assert_eq!(finished, render(&state, caps()));
    }

    #[test]
    fn thinking_header_patch_handles_boundary_anchor_offsets_without_body_work() {
        use crate::app::{FollowMode, ScrollAnchor};
        use crate::block::{Block, BlockKind, BlockLifecycle, FoldState};

        let mut state = AppState::new();
        state.authenticated = true;
        // Keep the pinned thinking block below the viewport so the anchor
        // produces a non-zero `skip_rows` value inside its boundary-prefixed
        // line block.
        for index in 0..24 {
            assert!(state.append_block(Block::new(
                format!("history-{index}"),
                BlockKind::Assistant("history row".into()),
                BlockLifecycle::Complete,
            )));
        }
        let mut thinking = Block::new(
            "streaming-thinking",
            BlockKind::Thinking("first body\nsecond body".into()),
            BlockLifecycle::Streaming,
        );
        thinking.fold = FoldState::Expanded;
        thinking.set_turn_boundary_before(true);
        assert!(thinking.turn_boundary_before());
        let thinking_id = thinking.id.clone();
        assert!(state.append_block(thinking));
        for index in 0..24 {
            assert!(state.append_block(Block::new(
                format!("tail-{index}"),
                BlockKind::Assistant("tail row".into()),
                BlockLifecycle::Complete,
            )));
        }
        state.working = true;

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
        let mut cache = WrapCache::default();
        for (row_offset, header_expected) in [(0, true), (1, false)] {
            state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
                block_id: thinking_id.clone(),
                row_offset,
            });
            state.clock.frame = 0;
            terminal
                .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
                .expect("initial draw");
            let first = terminal.backend().buffer().clone();
            let body_counters = (
                cache.body_hits(),
                cache.body_misses(),
                cache.body_bypasses(),
                cache.body_oversized_skips(),
            );

            state.clock.frame = 6;
            terminal
                .draw(|frame| render_frame(frame, &state, caps(), &mut cache))
                .expect("animated draw");
            let second = terminal.backend().buffer().clone();
            let changed = first
                .content
                .iter()
                .zip(&second.content)
                .enumerate()
                .filter_map(|(index, (before, after))| (before != after).then_some(index))
                .collect::<Vec<_>>();
            assert_eq!(
                body_counters,
                (
                    cache.body_hits(),
                    cache.body_misses(),
                    cache.body_bypasses(),
                    cache.body_oversized_skips(),
                ),
                "clock-only redraw must not re-render the Thinking body"
            );
            let header_row = (0..12).find(|row| {
                (0..80)
                    .map(|column| first[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("Thinking")
            });
            if header_expected {
                let header_row = header_row.expect("visible Thinking header");
                assert_eq!(changed, vec![header_row as usize * 80 + 2]);
            } else {
                assert!(
                    header_row.is_none(),
                    "row_offset=1 should leave the boundary-prefixed header offscreen"
                );
                assert_eq!(changed.len(), 1, "offscreen Thinking pulses the rail only");
            }
        }
    }

    #[test]
    fn residual_thinking_header_keeps_real_activity_label_without_second_spinner() {
        use crate::block::{Block, BlockKind, BlockLifecycle};

        let mut state = AppState::new();
        state.authenticated = true;
        state.working = true;
        let mut thinking = Block::new(
            "streaming-thinking",
            BlockKind::Thinking("plan".into()),
            BlockLifecycle::Streaming,
        );
        thinking.set_turn_boundary_before(true);
        assert!(state.append_block(thinking));
        state.activity = Some(ActivityState {
            phase: ActivityPhase::RunningTool("read".into()),
            started_ms: 0,
        });
        state.clock.elapsed_ms = 2_000;

        let render = |state: &AppState| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
            terminal
                .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
                .expect("draw");
            terminal.backend().buffer().clone()
        };
        let first = render(&state);
        let rail_row = (0..24)
            .find(|row| {
                (0..80)
                    .map(|column| first[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("Reading")
            })
            .expect("real activity rail label");
        let rail_before = (0..80)
            .map(|column| first[(column, rail_row)].clone())
            .collect::<Vec<_>>();
        let header_rows = (0..24)
            .filter(|row| {
                (0..80)
                    .map(|column| first[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("Thinking")
            })
            .count();
        assert_eq!(
            header_rows, 1,
            "the residual header must not duplicate in rail"
        );

        state.clock.frame = 6;
        let second = render(&state);
        let rail_after = (0..80)
            .map(|column| second[(column, rail_row)].clone())
            .collect::<Vec<_>>();
        assert_eq!(rail_before, rail_after, "rail glyph must stay static");
    }

    #[test]
    fn activity_pulse_changes_one_cell_and_stops_with_reduced_motion() {
        use ratatui::backend::TestBackend;
        let mut state = AppState::new();
        state.authenticated = true;
        state.apply_event(UiEvent::run_started(1));
        state.apply_event(UiEvent::ToolStarted {
            batch_id: ToolBatchId("pulse".into()),
            call_id: ToolCallId("pulse".into()),
            name: "read".into(),
            arguments_summary: "path=src/main.rs".into(),
        });
        let render = |state: &AppState, capabilities| {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 30)).unwrap();
            terminal
                .draw(|frame| {
                    super::render_frame(frame, state, capabilities, &mut WrapCache::default())
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let first = render(&state, caps());
        state.clock.frame = 6;
        let next = render(&state, caps());
        let changed = first
            .content
            .iter()
            .zip(&next.content)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 1, "only the activity glyph may animate");
        let reduced = crate::theme::Capabilities {
            reduced_motion: true,
            ..caps()
        };
        let still = render(&state, reduced);
        state.clock.frame = 12;
        assert_eq!(still, render(&state, reduced));
        let mut motion_cache = WrapCache::default();
        assert!(!super::motion_needed(&state, reduced, &mut motion_cache));
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        assert!(!super::motion_needed(&state, caps(), &mut motion_cache));
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

        let (batch, state) = receive_batch(&receiver, CONTROL_BATCH_LIMIT, &mut Vec::new());

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

        let (batch, state) = receive_batch(&receiver, STREAM_BATCH_LIMIT, &mut Vec::new());

        assert_eq!(batch.len(), STREAM_BATCH_LIMIT);
        assert_eq!(state, LaneDrain::Exhausted);
        assert!(receiver.try_recv().is_ok(), "next event must remain queued");
    }

    #[test]
    fn spinner_glyph_cycles_braille_and_honors_fallbacks() {
        let full = caps();
        assert_eq!(super::spinner_glyph(0, full), '⠋');
        assert_eq!(super::spinner_glyph(1, full), '⠙');
        assert_eq!(super::spinner_glyph(9, full), '⠏');
        assert_eq!(super::spinner_glyph(10, full), '⠋');

        let reduced = Capabilities {
            reduced_motion: true,
            ..full
        };
        assert_eq!(super::spinner_glyph(0, reduced), '\u{25cb}');
        assert_eq!(super::spinner_glyph(1, reduced), '\u{25cb}');

        let no_color = Capabilities {
            color_depth: ColorDepth::None,
            ..full
        };
        assert_eq!(super::spinner_glyph(0, no_color), '~');
    }
}
