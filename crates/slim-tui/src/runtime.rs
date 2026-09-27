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
    BlockId, LoginProvider, McpServerView, McpStatusView, TodoItemStatus, UiChannels, UiCommand,
};
use crate::app::{
    ActivityPhase, AppState, EffortOverlay, LoginOverlay, LoginStage, McpOverlay, ModelOverlay,
    ModelRow, NotificationPriority, SlashSuggestions, INFO_TOAST_TTL_MS,
};
use crate::block::{
    question_option_marker, wrap_words, Block, BlockKind, BlockLifecycle, FoldState,
    InteractionRequestKind, InteractionRequestState,
};
use crate::fullscreen::FullscreenBackend;
use crate::input::{DecodedEvent, PasteStreamDecoder};
use crate::inspector::{is_mutating_tool, InspectorKind};
use crate::layout::{plan_with_session_rail_and_composer, todo_height, Rect};
use crate::markdown::{render_plain, render_prose, sanitize_terminal_text, MarkdownStyles};
use crate::picker::{truncate_cells, visible_window, PICKER_NOMINAL_CAPACITY};
use crate::reducer::{reduce, Action, Effect, ScrollIntent};
use crate::render::{
    cached_lines_bytes, thinking_body_width, user_prompt_text_width, BodyKind, EventCoalescer,
    InspectorPaletteKey, ScrollMetrics, WrapCache,
};
use crate::runtime_wait::{
    next_visual_deadline, runtime_clock, wait_for_runtime_signal, WaitOutcome,
};
use crate::theme::{
    detect_capabilities, glyph, resolve_theme, to_terminal_color, Capabilities, ColorDepth,
    MENU_SELECTION_BG,
};
use crate::view_model::{
    activity_elapsed, activity_label, activity_phase_label, assistant_label, budget_near_limit,
    completed_tool_phrase, display_cwd, format_context, is_trivial_cwd, run_status_label,
    session_rail_projection, tool_target, tool_title, truncate_display_width,
};

/// Composer prompt is ASCII on purpose (G264): `›` (U+203A) is East-Asian
/// Ambiguous and Windows consoles advance two cells while Ratatui counts one,
/// which parks the caret on the last typed letter.
const COMPOSER_PROMPT: &str = "> ";
const COMPOSER_OVERFLOW_HINT: &str = "<";
const CONTROL_BATCH_LIMIT: usize = 32;
// One event may already be removed from the bounded lane into the prefetch
// slot when a control event establishes a causal barrier.
const STREAM_BATCH_LIMIT: usize = crate::api::STREAM_EVENT_CAPACITY + 1;
const DOCKED_INSPECTOR_MIN_WIDTH: u16 = 100;
const INFO_TOAST_HIGHLIGHT_MS: u64 = crate::block::TOOL_GROUP_HOLD_MS;

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
    run_app_with_initial_prompt(channels, None)
}

pub fn run_app_with_initial_prompt(
    channels: UiChannels,
    initial_prompt: Option<String>,
) -> io::Result<()> {
    let capabilities = detect_capabilities();
    let mut backend = FullscreenBackend::start(capabilities)?;
    let result = run_loop(&mut backend, &channels, capabilities, initial_prompt);
    let _ = channels.commands.send(UiCommand::Shutdown);
    let shutdown = backend.shutdown();
    result.and(shutdown)
}

fn run_loop(
    backend: &mut FullscreenBackend,
    channels: &UiChannels,
    capabilities: Capabilities,
    initial_prompt: Option<String>,
) -> io::Result<()> {
    let mut state = AppState::new();
    let mut queue = crate::prompt_queue::PersistentQueue::with_initial_prompt(initial_prompt);
    let mut dirty = true;
    let started = Instant::now();
    let mut last_motion_frame = 0;
    let mut last_status_second = 0;
    let mut control_closed = false;
    let mut stream_closed = false;
    let mut render_cache = WrapCache::default();
    let mut paste_decoder = PasteStreamDecoder::default();
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
        let effects = queue.reduce(&mut state, Action::SyncClock(clock))?;
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
            let effects = queue.reduce(&mut state, action)?;
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
                &mut queue,
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
                let effects = queue.reduce(&mut state, Action::UiEventReceived(event))?;
                run_effects(channels, &clipboard_tx, effects)?;
                dirty = true;
            }
        }
        channels.lane_space.notify();
        for event in control_events {
            update_visible_stream_state(&event, &mut visible_stream_started);
            let effects = queue.reduce(&mut state, Action::UiEventReceived(event))?;
            run_effects(channels, &clipboard_tx, effects)?;
            dirty = true;
        }
        control_closed = control_drain == LaneDrain::Closed;

        if let Some(stream_events) = stream_events {
            reduce_stream_events(
                &mut queue,
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
                let effects = queue.reduce(&mut state, Action::UiEventReceived(event))?;
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
            if let Some(accessible) =
                approval_content_accessible(&state, (size.width, size.height), &mut render_cache)
            {
                if state.approval_content_accessible != accessible {
                    let effects = queue
                        .reduce(&mut state, Action::SetApprovalContentAccessible(accessible))?;
                    run_effects(channels, &clipboard_tx, effects)?;
                }
            }
            // Extract from the painted frame. After Terminal::draw swaps
            // buffers, current_buffer_mut is the reset back buffer.
            state.selection_text =
                draw_state(backend, &mut state, capabilities, &mut render_cache)?;
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
        // A hidden ActivityRail still needs one-second semantic redraws for
        // retry countdowns, cancellation labels and elapsed footer metadata.
        // This remains a status tick (not an animation loop) and is disabled
        // while an unrelated modal owns the frame. Activity's visible ages
        // continue at one status tick per second during an active run.
        let status_visible = status_clock_visible(&state, regions.activity_rail.height);
        let toast_count = toast_row_count(&state, regions.scrollback.height);
        let elapsed = started.elapsed();
        let next_toast_deadline_ms = next_toast_visual_deadline_ms(
            &state,
            runtime_clock(elapsed).elapsed_ms,
            toast_count,
            !capabilities.reduced_motion,
        );
        let next_transition_deadline_ms = next_transition_visual_deadline_ms(
            &state,
            runtime_clock(elapsed).elapsed_ms,
            capabilities,
        );
        let next_expiry_ms = [next_toast_deadline_ms, next_transition_deadline_ms]
            .into_iter()
            .flatten()
            .min();
        let visual_deadline = next_visual_deadline(
            elapsed,
            last_motion_frame,
            last_status_second,
            motion_visible,
            status_visible,
            next_expiry_ms,
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
                let mut input_pending;
                loop {
                    let event = read()?;
                    input_pending = crossterm::event::poll(Duration::ZERO)?;
                    if let Event::Resize(width, height) = event {
                        size = ratatui::layout::Size { width, height };
                    }
                    // Input is drained in batches. A drag and copy can arrive
                    // before the next normal draw, so materialize the latest
                    // bounded selection before the reducer consumes its text.
                    if dirty && state.selection.is_some() && is_selection_copy_event(&event) {
                        state.selection_text =
                            draw_state(backend, &mut state, capabilities, &mut render_cache)?;
                        last_motion_frame = state.clock.frame;
                        last_status_second = state.clock.elapsed_ms / 1_000;
                        dirty = false;
                    }
                    for decoded in paste_decoder.feed(event, input_pending) {
                        let action = match decoded {
                            DecodedEvent::Paste(payload) => Some(Action::Paste(payload)),
                            DecodedEvent::Event(event) => terminal_action(
                                event,
                                &state,
                                (size.width, size.height),
                                &mut render_cache,
                            ),
                        };
                        if let Some(action) = action {
                            let effects = queue.reduce(&mut state, action)?;
                            run_effects(channels, &clipboard_tx, effects)?;
                            dirty = true;
                        }
                    }
                    handled += 1;
                    if handled >= 64 || !input_pending {
                        break;
                    }
                }
                // Queue exhausted: a held marker prefix was literal input and
                // the burst heuristic resets for the next drain.
                if !input_pending {
                    for decoded in paste_decoder.end_of_input() {
                        let DecodedEvent::Event(event) = decoded else {
                            continue;
                        };
                        if let Some(action) = terminal_action(
                            event,
                            &state,
                            (size.width, size.height),
                            &mut render_cache,
                        ) {
                            let effects = queue.reduce(&mut state, action)?;
                            run_effects(channels, &clipboard_tx, effects)?;
                            dirty = true;
                        }
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
                    let effects = queue.reduce(&mut state, action)?;
                    run_effects(channels, &clipboard_tx, effects)?;
                    dirty = true;
                } else if next_expiry_ms.is_some_and(|deadline_ms| clock.elapsed_ms >= deadline_ms)
                {
                    let effects = queue.reduce(&mut state, Action::SyncClock(clock))?;
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
    queue: &mut crate::prompt_queue::PersistentQueue,
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
            let effects = queue.reduce(state, Action::UiEventReceived(ready))?;
            run_effects(channels, clipboard_tx, effects)?;
            *dirty = true;
        }
        if first_visible {
            *visible_stream_started = true;
            for ready in coalescer.flush() {
                let effects = queue.reduce(state, Action::UiEventReceived(ready))?;
                run_effects(channels, clipboard_tx, effects)?;
                *dirty = true;
            }
        }
    }
    if force_flush {
        for event in coalescer.flush() {
            let effects = queue.reduce(state, Action::UiEventReceived(event))?;
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
        || state.todo_focused
}

fn status_clock_visible(state: &AppState, activity_rows: u16) -> bool {
    (state.working && state.todo_dock_open && !state.todo_items.is_empty())
        || (!navigation_captured(state)
            && (activity_rows > 0 || state.working || state.retry.is_some()))
        || (state.working
            && state.inspector.active == Some(InspectorKind::Activity)
            && inspector_has_keyboard_focus(state))
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
    // A pending copy must not become cancel/quit/paste when its pre-input
    // draw (or a resize in the same input batch) invalidated the selection.
    if is_selection_copy_event(&event)
        && cache
            .painted_selection
            .as_ref()
            .is_some_and(|snapshot| !snapshot.valid)
    {
        return None;
    }
    match event {
        Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
            // Re-check the painted approval card at dispatch time.  Resize
            // and Y/N can arrive in one input batch, before the dirty-frame
            // pass has synchronized the reducer's accessibility flag.
            if approval_content_accessible(state, size, cache) == Some(false)
                && key.modifiers == KeyModifiers::NONE
                && matches!(key.code, KeyCode::Char('y' | 'Y' | 'n' | 'N'))
            {
                return Some(Action::SetApprovalContentAccessible(false));
            }
            if let Some((_, total_rows, capacity)) = approval_overlay_metrics(state, size, cache) {
                if let Some(intent) = inspector_scroll_intent(key) {
                    return Some(Action::ScrollApproval {
                        intent,
                        total_rows,
                        capacity,
                    });
                }
            }
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
        Event::Resize(_, _) => {
            if state.selection.is_some() {
                if let Some(snapshot) = &mut cache.painted_selection {
                    snapshot.valid = false;
                }
            }
            Some(Action::Resize)
        }
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
            if let Some((area, total_rows, capacity)) = approval_overlay_metrics(state, size, cache)
            {
                // A pending approval owns the pointer. Scroll only when the
                // wheel is over its card; clicks elsewhere are ignored.
                if area_contains(area, mouse.column, mouse.row) {
                    let intent = if mouse.kind == MouseEventKind::ScrollUp {
                        ScrollIntent::Up
                    } else {
                        ScrollIntent::Down
                    };
                    return Some(Action::ScrollApproval {
                        intent,
                        total_rows,
                        capacity,
                    });
                }
                return None;
            }
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
            if area.is_some() && area == cache.selection_regions[0] {
                if let Some(anchor) = cache.selection_scroll_anchor.clone() {
                    return Some(Action::StartPinnedScreenSelection {
                        x: mouse.column,
                        y: mouse.row,
                        area,
                        anchor,
                    });
                }
            }
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
    if mouse_navigation_captured(state) {
        return None;
    }
    // Appending rows below the selected text may grow the painted region.
    // Preserve the original range; the painted-cell comparison below also
    // handles a scrollbar appearing outside the selected cells.
    cache
        .selection_regions
        .iter()
        .flatten()
        .find(|current| current.x == area.x && current.y == area.y)
        .map(|current| area.intersection(*current))
}

fn extract_visible_selection(
    frame: &mut ratatui::Frame,
    state: &AppState,
    cache: &WrapCache,
) -> String {
    let _ = frame;
    cache
        .painted_selection
        .as_ref()
        .filter(|snapshot| snapshot.valid && Some(snapshot.selection) == state.selection)
        .map(|snapshot| snapshot.text.clone())
        .unwrap_or_default()
}

fn validate_painted_selection(
    frame: &mut ratatui::Frame,
    state: &AppState,
    cache: &mut WrapCache,
) -> bool {
    let Some(selection) = state.selection else {
        cache.painted_selection = None;
        return false;
    };
    let Some(area) = selection_area(state, cache) else {
        if let Some(snapshot) = &mut cache.painted_selection {
            snapshot.valid = false;
        }
        return false;
    };
    let mut current = crate::selection::capture_selection(frame.buffer_mut(), selection, area);
    if let Some(previous) = &cache.painted_selection {
        if previous.selection == selection && (!previous.valid || !state.selection_text.is_empty())
        {
            current.valid = previous.valid && previous.cells == current.cells;
        }
    }
    let valid = current.valid;
    cache.painted_selection = Some(current);
    valid
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
    let workspace = scrollback;
    let inspector_visible =
        state.inspector.active.is_some() && workspace.width >= DOCKED_INSPECTOR_MIN_WIDTH;
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

fn scrollbar_is_guaranteed(blocks: &[Block], viewport: u64) -> bool {
    if viewport == 0 {
        return false;
    }
    let vp = viewport as usize;
    if blocks.len() > vp {
        return true;
    }
    let mut min_rows = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        let base = match block.kind() {
            BlockKind::User(_) => 2,
            BlockKind::Assistant(_)
                if crate::block::assistant_chrome(blocks, index)
                    .is_some_and(|chrome| chrome.shows_header()) =>
            {
                3
            }
            BlockKind::Assistant(_) => 1,
            _ => 1,
        };
        min_rows = min_rows.saturating_add(
            base + usize::from(block.turn_boundary_before())
                + usize::from(crate::block::transition_gap(blocks, index)),
        );
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
    cache.set_tool_presentation(state.clock.elapsed_ms, cache.reduced_motion_presentation());
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
    inline_code: Style,
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
    /// Applied over everything behind a centered modal; keeps backgrounds.
    backdrop: Style,
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
            // H1 is set apart from H2 by underline rather than hue.
            h1: base
                .fg(color(theme.heading_accent))
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            link: base.fg(color(theme.link_accent)),
            quote: base.fg(color(theme.secondary_text)),
            tool: base.fg(color(theme.tool_accent)),
            // The raised background collapses to Black in ANSI16/no-color, so
            // those depths keep marking inline code by foreground instead.
            inline_code: if matches!(
                capabilities.color_depth,
                ColorDepth::Ansi16 | ColorDepth::None
            ) {
                base.fg(color(theme.tool_accent))
            } else {
                base.fg(color(theme.foreground)).bg(color(theme.code_bg))
            },
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
            backdrop: if capabilities.color_depth == ColorDepth::None {
                base.add_modifier(Modifier::DIM)
            } else {
                base.fg(color(theme.backdrop))
            },
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
    state: &mut AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) -> io::Result<String> {
    let mut selected = String::new();
    backend.draw_synchronized(|frame| {
        render_frame(frame, state, capabilities, cache);
        selected = extract_visible_selection(frame, state, cache);
    })?;
    if state.selection.is_some()
        && cache
            .painted_selection
            .as_ref()
            .is_none_or(|snapshot| !snapshot.valid)
    {
        let _ = reduce(state, Action::ClearScreenSelection);
    }
    Ok(selected)
}

pub fn render_frame(
    frame: &mut ratatui::Frame,
    state: &AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) {
    cache.set_tool_presentation(state.clock.elapsed_ms, capabilities.reduced_motion);
    cache.selection_regions = [None, None];
    cache.selection_scroll_anchor = None;
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
    let cursor_target = input_cursor_target(state);

    if session_visible {
        render_session_rail(
            frame,
            chrome_area(to_ratatui(regions.session_rail), band),
            state,
            &palette,
        );
    }
    // The rail may suppress its spinner when it is hidden, idle, or otherwise
    // not useful.  Keep that decision separate from the user's motion
    // preference: completed blocks still receive the short transition
    // emphasis whenever reduced motion is not requested.
    let rail_motion_capabilities = Capabilities {
        reduced_motion: capabilities.reduced_motion
            || regions.activity_rail.height == 0
            || !motion_needed(state, capabilities, cache),
        ..capabilities
    };
    let spinner_motion_enabled = !rail_motion_capabilities.reduced_motion;
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
        capabilities,
        spinner_motion_enabled,
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
                reduced_motion: rail_motion_capabilities.reduced_motion || thinking_header_visible,
                ..capabilities
            },
            thinking_header_visible,
        );
    }
    let composer_area = chrome_area(to_ratatui(regions.composer), band);
    render_composer(
        frame,
        composer_area,
        state,
        &palette,
        cache,
        cursor_target == Some(InputCursorTarget::Composer),
    );
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
        capabilities,
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
            capabilities,
            cache,
        );
    } else if let Some(kind) = state.inspector.active {
        // A floating inspector owns the content surface; do not select behind it.
        cache.selection_regions[0] = None;
        render_inspector_overlay(
            frame,
            scrollback,
            state,
            kind,
            &palette,
            capabilities,
            cache,
        );
    }
    if state.search.is_some() {
        render_search_bar(
            frame,
            workspace.transcript,
            state,
            &palette,
            &search_matches,
            cursor_target == Some(InputCursorTarget::Search),
        );
    }
    if state.model_overlay.is_some()
        || state.mcp_overlay.is_some()
        || state.effort_overlay.is_some()
        || state.login_overlay.is_some()
        || state.palette_query.is_some()
    {
        dim_backdrop(frame, &palette);
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
            cursor_target == Some(InputCursorTarget::ModelFilter),
        );
    }
    if let Some(overlay) = &state.mcp_overlay {
        render_mcp_overlay(frame, overlay, &state.mcp_servers, &palette);
    }
    if let Some(overlay) = &state.effort_overlay {
        render_effort_overlay(frame, overlay, &palette);
    }
    if let Some(overlay) = &state.login_overlay {
        render_login_overlay(
            frame,
            overlay,
            &palette,
            cursor_target == Some(InputCursorTarget::LoginApiKey),
        );
    }
    if let Some(query) = &state.palette_query {
        render_palette(
            frame,
            query,
            state.palette_selected,
            state.palette_viewport_start,
            &palette,
            cache,
            cursor_target == Some(InputCursorTarget::Palette),
        );
    }
    if validate_painted_selection(frame, state, cache) {
        let (selection, area) = state
            .selection
            .zip(selection_area(state, cache))
            .expect("validated selection");
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

fn next_transition_visual_deadline_ms(
    state: &AppState,
    now_ms: u64,
    capabilities: Capabilities,
) -> Option<u64> {
    if capabilities.reduced_motion {
        return None;
    }
    let mut deadline = None;
    let mut consider = |at_ms: Option<u64>| {
        let Some(at_ms) = at_ms else {
            return;
        };
        let expires = at_ms.saturating_add(INFO_TOAST_HIGHLIGHT_MS);
        if expires > now_ms {
            deadline = Some(deadline.map_or(expires, |current: u64| current.min(expires)));
        }
    };
    consider(state.confirmed_setting.as_ref().map(|(_, at_ms)| *at_ms));
    consider(state.activity.as_ref().map(|activity| activity.started_ms));
    consider(
        state
            .cancellation
            .as_ref()
            .map(|cancellation| cancellation.requested_ms),
    );
    consider(state.retry.as_ref().map(|retry| retry.scheduled_ms));
    consider(
        state
            .last_execution
            .as_ref()
            .map(|execution| execution.ended_ms),
    );
    for block in state.blocks() {
        consider(block.started_ms);
        consider(block.ended_ms);
    }
    deadline
}

fn motion_needed(state: &AppState, capabilities: Capabilities, cache: &mut WrapCache) -> bool {
    if capabilities.reduced_motion
        || capabilities.color_depth == ColorDepth::None
        || welcome_visible(state)
        || navigation_captured(state)
        || state.inspector.active.is_some()
        || state.cancellation.is_some()
        || state.retry.is_some()
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
    spinner_motion_enabled: bool,
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
    cache.selection_scroll_anchor = metrics.top_anchor;
    let (mut idx, mut skip_rows) = index.locate(start_row);
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
        cache.tool_group_leader(
            state.blocks(),
            content_rev,
            fold_rev,
            state.clock.elapsed_ms,
            capabilities.reduced_motion,
        )
    } else {
        None
    };
    // A short conversation grows upward from the composer, like the welcome.
    // Full histories retain their anchored/page-fill scroll semantics.
    let leading_space = if state.selection.is_some() && state.scroll.is_pinned() {
        state
            .selection_area
            .filter(|selected| selected.x == area.x.saturating_add(2))
            .map(|selected| {
                selected
                    .y
                    .saturating_sub(area.y)
                    .min(area.height.saturating_sub(1))
            })
            .unwrap_or(0)
    } else {
        viewport.saturating_sub(index.total_rows).saturating_sub(1) as u16
    };
    let text_area = ratatui::layout::Rect {
        x: area.x,
        y: area.y + leading_space,
        width: content_width,
        height: area.height.saturating_sub(leading_space),
    };
    let capacity = viewport.saturating_sub(u64::from(leading_space)) as usize;
    let buf = frame.buffer_mut();
    buf.set_style(text_area, palette.text);
    let mut y = text_area.y;
    while idx < index.len() && rows < capacity && y < text_area.bottom() {
        let block_index = index.entry_start(idx);
        let (_, block, members) = index.entry(idx);
        idx += 1;
        let assistant_chrome = crate::block::assistant_chrome(state.blocks(), block_index);
        let is_selected = selected.as_ref() == Some(&block.id);
        let show_enter_hint = is_selected
            || live_collapsed_group
                .as_ref()
                .is_some_and(|leader| leader == &block.id);
        let transition_gap = crate::block::transition_gap(state.blocks(), block_index);
        let thinking_header_index = usize::from(
            members.len() == 1
                && matches!(block.kind(), BlockKind::Thinking(_))
                && (block.turn_boundary_before() || transition_gap),
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
            now_ms: state.clock.elapsed_ms,
            animate_thinking: spinner_motion_enabled
                && !streaming_header_visible
                && !capabilities.reduced_motion
                && capabilities.color_depth != ColorDepth::None
                && thinking_header_candidate,
            assistant_chrome,
        };
        // Key on everything the produced lines depend on: each member's
        // generation/lifecycle/fold/timing folded together, selection and the
        // enter hint on the leader, and the wrap width.  The animated Thinking
        // glyph is patched into the visible header after these static lines
        // are painted.  Only a streaming Thinking member contributes the
        // current second, so ordinary clock ticks never invalidate the rest
        // of the transcript cache.
        let mut member_state = 0u64;
        for member in members {
            member_state = member_state
                .wrapping_mul(31)
                .wrapping_add(member.content_generation())
                .wrapping_mul(31)
                .wrapping_add(u64::from(member.lifecycle_tag()))
                .wrapping_mul(31)
                .wrapping_add(u64::from(member.fold_tag()))
                .wrapping_mul(31)
                .wrapping_add(member.started_ms.unwrap_or(u64::MAX))
                .wrapping_mul(31)
                .wrapping_add(member.ended_ms.unwrap_or(u64::MAX));
            if matches!(member.kind(), BlockKind::Thinking(_))
                && member.lifecycle == BlockLifecycle::Streaming
            {
                member_state = member_state
                    .wrapping_mul(31)
                    .wrapping_add(state.clock.elapsed_ms / 1_000);
            }
            // Transition emphasis is a short-lived visual state. Key the
            // memo on its boolean edge so the style expires without adding
            // the full frame clock to every block cache entry.
            let recent = recent_transition(
                member.ended_ms.or(member.started_ms),
                state.clock.elapsed_ms,
                capabilities,
            );
            member_state = member_state
                .wrapping_mul(31)
                .wrapping_add(u64::from(recent));
        }
        if let Some(chrome) = assistant_chrome {
            member_state = member_state
                .wrapping_mul(31)
                .wrapping_add(chrome.cache_tag());
        }
        let key = (
            block.cache_identity(),
            member_state,
            u8::from(is_selected) | u8::from(show_enter_hint) << 1 | u8::from(transition_gap) << 2,
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
                now_ms: ctx.now_ms,
                animate_thinking: false,
                assistant_chrome: ctx.assistant_chrome,
            };
            let mut built = if members.len() > 1 {
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
            if transition_gap {
                built.insert(0, Line::default());
            }
            cache.store_block_lines(key, built)
        });
        let search_match = !matched_ids.is_empty()
            && (matched_ids.contains(&block.id)
                || members
                    .iter()
                    .any(|member| matched_ids.contains(&member.id)));
        let skip = (skip_rows as usize).min(block_lines.len());
        skip_rows = 0;
        let written = block_lines
            .len()
            .saturating_sub(skip)
            .min(capacity.saturating_sub(rows))
            .min(usize::from(text_area.bottom().saturating_sub(y)));
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
        // One cell per row, written straight into the buffer: building a
        // `Vec<Line>` for a Paragraph costs ~3 allocations per visible row
        // on every frame the scrollbar is painted.
        let bar_x = area.x + area.width.saturating_sub(1);
        for row in 0..track {
            let (glyph, style) = if row >= thumb_top && row < thumb_top.saturating_add(thumb) {
                ("┃", palette.scrollbar_thumb)
            } else {
                ("│", palette.scrollbar_track)
            };
            frame
                .buffer_mut()
                .set_string(bar_x, area.y + row as u16, glyph, style);
        }
    }
    if notice_count > 0 {
        let history_count = state.notification_history().len().saturating_sub(1);
        let available_width = area.width.saturating_sub(2) as usize;
        let notices: Vec<Line> = state
            .visible_toast_tail(notice_count as usize)
            .into_iter()
            .map(|notice| {
                let label = notification_toast_label(notice, history_count, available_width);
                let base_style = match notice.priority {
                    NotificationPriority::Error => palette.error,
                    NotificationPriority::Warning => palette.warning,
                    NotificationPriority::Info => palette.muted,
                };
                let style = if notice_highlight_enabled
                    && state.clock.elapsed_ms.saturating_sub(notice.created_ms)
                        < INFO_TOAST_HIGHLIGHT_MS
                {
                    base_style.add_modifier(Modifier::BOLD)
                } else {
                    base_style
                };
                Line::from(vec![
                    Span::styled("  ", palette.surface),
                    Span::styled(label, style),
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

fn notification_toast_label(
    notice: &crate::app::Notification,
    history_count: usize,
    max_width: usize,
) -> String {
    let mut label = sanitize_terminal_text(notice.as_str());
    if notice.repeat_count > 1 {
        label.push_str(&format!(" · {}x", notice.repeat_count));
    }
    if history_count > 0 {
        label.push_str(&format!(
            " · +{} histórico{}",
            history_count,
            if history_count == 1 { "" } else { "s" }
        ));
    }
    truncate_with_marker(&label, max_width)
}

/// Truncate one-line chrome while leaving an explicit marker that more content
/// is available through the Diagnostics history.  The marker is counted in
/// terminal cells, so wide glyphs cannot spill into the border.
fn truncate_with_marker(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".into();
    }
    format!("{}…", truncate_cells(text, max_width.saturating_sub(1)))
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
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let summary = crate::todo::summary(state);
    if area.height == 1 {
        frame.render_widget(
            Paragraph::new(truncate_cells(&crate::todo::compact_summary(state), width))
                .style(palette.text),
            area,
        );
        return;
    }
    let indices = crate::todo::ordered_indices(state);
    let capacity = area.height.saturating_sub(2).max(1) as usize;
    let selected = indices
        .iter()
        .position(|index| *index == state.todo_selected)
        .unwrap_or(0);
    let window = visible_window(
        indices.len(),
        if state.todo_focused { selected } else { 0 },
        capacity,
        0,
    );
    let start = window.start;
    let end = window.end;
    let mut rows = vec![Line::from(Span::styled(
        truncate_cells(&summary, width),
        palette.text,
    ))];
    for index in &indices[start..end] {
        let item = &state.todo_items[*index];
        let selected = state.todo_focused && *index == state.todo_selected;
        let (marker, fallback, style) = match item.status {
            TodoItemStatus::InProgress => ('◌', '~', palette.warning),
            TodoItemStatus::Blocked => ('✕', 'x', palette.error),
            TodoItemStatus::Pending => ('○', 'o', palette.text),
            TodoItemStatus::Completed => ('✓', '+', palette.muted),
            TodoItemStatus::Cancelled => ('·', '-', palette.muted),
        };
        let offset = if selected { state.todo_title_offset } else { 0 };
        let label = truncate_cells(
            &crate::todo::visible_title(&crate::todo::item_text(item), offset),
            width.saturating_sub(3),
        );
        rows.push(Line::from(vec![
            Span::styled(if selected { "> " } else { "  " }, palette.text),
            Span::styled(glyph(capabilities, marker, fallback).to_string(), style),
            Span::styled(
                label,
                if selected {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                },
            ),
        ]));
    }
    if rows.len() < area.height as usize {
        let hidden = indices.len().saturating_sub(end - start);
        let mut hint = if state.todo_focused {
            format!(
                "{}/{} · ↑↓ tarefas · ←→ título · Tab voltar · Esc recolher",
                selected + 1,
                indices.len()
            )
        } else {
            "Alt+T navegar · Ctrl+T recolher".into()
        };
        if hidden > 0 {
            hint = format!("+{hidden} fora da vista · {hint}");
        }
        if let Some(updated) = state.todo_updated_ms {
            // Relative age is visible only while running, when the status clock ticks.
            if state.working {
                hint.push_str(&format!(
                    " · atualizado há {}s",
                    state.clock.elapsed_ms.saturating_sub(updated) / 1000
                ));
            }
        }
        rows.push(Line::from(Span::styled(
            truncate_cells(&hint, width),
            palette.secondary,
        )));
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
    let cancellation = state.cancellation.as_ref();
    let retry = state.retry.as_ref();
    let semantic_style = if cancellation.is_some() || retry.is_some() {
        palette.warning
    } else {
        palette.text
    };
    let semantic_style = if recent_transition(
        cancellation.map(|value| value.requested_ms),
        state.clock.elapsed_ms,
        capabilities,
    ) || recent_transition(
        retry.map(|value| value.scheduled_ms),
        state.clock.elapsed_ms,
        capabilities,
    ) || recent_transition(
        state.activity.as_ref().map(|value| value.started_ms),
        state.clock.elapsed_ms,
        capabilities,
    ) {
        semantic_style.add_modifier(Modifier::BOLD)
    } else {
        semantic_style
    };
    let mut spans = if header_owns_thinking && cancellation.is_none() && retry.is_none() {
        Vec::new()
    } else {
        let indicator = if cancellation.is_some() {
            glyph(capabilities, '\u{21bb}', '!')
        } else if retry.is_some() {
            glyph(capabilities, '\u{21bb}', '~')
        } else {
            spinner_glyph(state.clock.frame, capabilities)
        };
        vec![
            Span::styled(format!("{indicator} "), semantic_style),
            Span::styled(activity_label(state), semantic_style),
        ]
    };
    if header_owns_thinking && spans.is_empty() {
        // The visible Thinking header already shows this phase's time; the
        // rail adds the run total only when it says something new.
        let total = state
            .run_started_ms
            .map(|started| state.clock.elapsed_ms.saturating_sub(started) / 1_000)
            .filter(|total| *total > elapsed);
        if let Some(total) = total {
            spans.push(Span::styled(format!("execução {total}s"), palette.muted));
        }
    } else if elapsed > 0 {
        spans.push(Span::styled(format!(" · {elapsed}s"), palette.muted));
    }
    if let Some(age) = last_useful_update_age_ms(state) {
        if age >= 5_000 && cancellation.is_none() && retry.is_none() {
            let stale_label = if tool_activity_is_active(state) {
                "sem progresso"
            } else {
                "sem novo conteúdo"
            };
            let stale = format!(" · {stale_label} há {}", duration_label(age));
            let occupied: usize = spans.iter().map(|span| span.width()).sum();
            if occupied + UnicodeWidthStr::width(stale.as_str()) <= area.width as usize {
                spans.push(Span::styled(stale, palette.muted));
            }
        }
    }
    if area.width >= 72 {
        for (name, used, limit) in [
            ("turnos", state.turns_used, state.max_turns),
            ("leituras", state.tools_used_read, state.max_read_tool_calls),
            (
                "edições",
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
    // Segments open with ` · `; with the leading label omitted the rail must
    // not start on a bare separator.
    if let Some(first) = spans.first_mut() {
        if let Some(rest) = first.content.strip_prefix(" · ") {
            first.content = rest.to_owned().into();
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(palette.surface),
        area,
    );
}

fn last_useful_update_age_ms(state: &AppState) -> Option<u64> {
    let latest = [state.last_provider_content_ms, state.last_tool_progress_ms]
        .into_iter()
        .flatten()
        .max()?;
    Some(state.clock.elapsed_ms.saturating_sub(latest))
}

fn tool_activity_is_active(state: &AppState) -> bool {
    matches!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(
            ActivityPhase::PreparingTool(_)
                | ActivityPhase::QueuedTool(_)
                | ActivityPhase::RunningTool(_)
        )
    )
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
                "Conectado · {}",
                state
                    .auth_provider
                    .map(LoginProvider::label)
                    .unwrap_or("provider")
            ),
            "Descreva uma tarefa para começar",
            true,
        )
    } else {
        ("Não conectado".into(), "Use /login para conectar", false)
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
                        "Conectado"
                    } else {
                        "Não conectado"
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

pub(crate) fn last_collapsed_tool_group_leader(
    blocks: &[Block],
    now_ms: u64,
    reduced_motion: bool,
) -> Option<BlockId> {
    let mut index = blocks.len();
    while index > 0 {
        index -= 1;
        let block = &blocks[index];
        if !matches!(block.kind(), BlockKind::Tool(_)) {
            continue;
        }
        let span = if crate::block::is_presented_complete_tool(block, now_ms, reduced_motion) {
            crate::block::consecutive_presented_tool_span(blocks, index, now_ms, reduced_motion)
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
    let accumulated_duration_ms: Option<u64> =
        members
            .iter()
            .try_fold(0u64, |acc, block| match block.kind() {
                BlockKind::Tool(tool) => tool
                    .duration_ms
                    .map(|duration| acc.saturating_add(duration)),
                _ => Some(acc),
            });
    let duration = batch_duration_ms(members)
        .map(|value| format!(" · {}", duration_label(value)))
        .or_else(|| {
            accumulated_duration_ms.map(|value| format!(" · acumulado {}", duration_label(value)))
        })
        .unwrap_or_default();
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
        format!("{tool_count} ferramentas{duration}")
    } else {
        format!("{phrase}{duration}")
    };
    let compact = format!("{tool_count} ferramentas{duration}");
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
    let hint = "Enter detalhes";
    let marker_width = UnicodeWidthStr::width(marker);
    let glyph_text = format!("{complete} ");
    let recent_completion = !ctx.capabilities.reduced_motion
        && members.iter().any(|member| {
            matches!(member.lifecycle, BlockLifecycle::Complete)
                && member
                    .ended_ms
                    .is_some_and(|ended| ctx.now_ms.saturating_sub(ended) < INFO_TOAST_HIGHLIGHT_MS)
        });
    let completion_style = if recent_completion {
        ctx.palette.success.add_modifier(Modifier::BOLD)
    } else {
        ctx.palette.muted
    };
    let header_width =
        UnicodeWidthStr::width(glyph_text.as_str()) + UnicodeWidthStr::width(detail.as_str());
    let hint_width = UnicodeWidthStr::width(hint);
    let content_width = ctx.width as usize;
    let mut header_spans = vec![
        Span::styled(marker.to_owned(), ctx.palette.muted),
        Span::styled(glyph_text, completion_style),
        Span::styled(
            detail,
            if recent_completion {
                ctx.palette.secondary.add_modifier(Modifier::BOLD)
            } else {
                ctx.palette.muted
            },
        ),
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
                ctx.now_ms,
                true,
                cache,
            ));
        }
    }
    lines
}

/// Returns wall-clock duration for a grouped batch when every non-historical
/// tool member has both lifecycle timestamps.  Parallel calls therefore do
/// not inflate the headline with the sum of their individual durations.
fn batch_duration_ms(members: &[Block]) -> Option<u64> {
    let mut started = None;
    let mut ended = None;
    let mut saw_tool = false;
    for block in members {
        let BlockKind::Tool(tool) = block.kind() else {
            continue;
        };
        if tool.historical {
            continue;
        }
        saw_tool = true;
        let block_started = block.started_ms?;
        let block_ended = block.ended_ms?;
        started = Some(started.map_or(block_started, |value: u64| value.min(block_started)));
        ended = Some(ended.map_or(block_ended, |value: u64| value.max(block_ended)));
    }
    if saw_tool {
        Some(ended?.saturating_sub(started?))
    } else {
        None
    }
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
        let thinking_style = ctx.palette.thinking.add_modifier(Modifier::ITALIC);
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
                    render_prose(text, body_width)
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
    let (name, preview, target) = match leader.kind() {
        BlockKind::Tool(tool) => (
            sanitize_terminal_text(&tool.name),
            sanitize_terminal_text(&tool.preview),
            first_tool_segment(&sanitize_terminal_text(&tool.arguments_summary))
                .map(|segment| (tool_target(&segment), segment))
                .filter(|(target, segment)| target != segment)
                .map(|(target, _)| target),
        ),
        _ => (String::new(), String::new(), None),
    };
    let reason_text = short_failure_reason(&preview);
    let mut parts = vec![ToolDetailPart::fixed(tool_title(
        &name,
        BlockLifecycle::Failed,
    ))];
    if let Some(target) = target {
        parts.push(ToolDetailPart::flexible(target, 0, 1).attached());
    }
    if count > 1 {
        parts.push(ToolDetailPart::fixed(format!("×{count}")).attached());
    }
    if reason_text.is_empty() {
        parts.push(ToolDetailPart::fixed("falhou".into()));
    } else {
        parts.push(ToolDetailPart::flexible(reason_text, 1, 1));
    }
    let marker = if ctx.selected { "> " } else { "  " };
    let failed = glyph(ctx.capabilities, '\u{2715}', 'x');
    let glyph_text = format!("{failed} ");
    let hint = "Enter detalhes";
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
                ctx.now_ms,
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
    now_ms: u64,
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
            ("- ".into(), palette.muted, palette.muted, Some("histórico"))
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
            Some("falhou"),
        ),
        BlockLifecycle::Cancelled => (
            format!("{} ", glyph(capabilities, '\u{25a0}', 'x')),
            palette.warning,
            palette.muted,
            Some("cancelada"),
        ),
    };
    let recent_completion = !capabilities.reduced_motion
        && block.lifecycle == BlockLifecycle::Complete
        && block
            .ended_ms
            .is_some_and(|ended| now_ms.saturating_sub(ended) < INFO_TOAST_HIGHLIGHT_MS);
    let glyph_style = if recent_completion {
        glyph_style.add_modifier(Modifier::BOLD)
    } else {
        glyph_style
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
    // A collapsed completed call still carries useful evidence. Keep one
    // short argument segment (the target) and one preview segment (the
    // observable result) on its summary row; the full payload remains behind
    // Enter details.
    let show_compact_context = !state.historical
        && !show_args
        && !collapsed_failure
        && block.fold != FoldState::Expanded
        && matches!(
            block.lifecycle,
            BlockLifecycle::Complete | BlockLifecycle::Cancelled
        );
    // Collapsed rows (settled or failed) keep the call's target and its
    // edit size; the full argument list stays behind Enter details.
    // A failure keeps only a recognized target: unknown `key=value`
    // arguments read as telemetry next to the failure reason.
    let compact_target = (show_compact_context || collapsed_failure)
        .then(|| first_tool_segment(&args))
        .flatten()
        .map(|segment| (tool_target(&segment), segment))
        .filter(|(target, segment)| !collapsed_failure || target != segment)
        .map(|(target, _)| target);
    let compact_stats = (show_compact_context || collapsed_failure)
        .then(|| {
            args.split(" · ")
                .find(|segment| edit_stats_segment(segment).is_some())
                .map(str::to_owned)
        })
        .flatten();
    let compact_result = show_compact_context
        .then(|| first_tool_segment(&preview))
        .flatten()
        .and_then(|segment| compact_tool_result(&name, segment));
    let failure_reason = collapsed_failure
        .then(|| short_failure_reason(&preview))
        .filter(|reason| !reason.is_empty());
    let preview_shown = (show_preview && !preview.is_empty())
        || failure_reason.is_some()
        || compact_result.is_some();
    // Restored calls keep their raw name: a past-tense verb would imply an
    // outcome that history does not record.
    let title = if state.historical {
        name
    } else {
        tool_title(&name, block.lifecycle)
    };
    let mut parts = vec![ToolDetailPart::fixed(title.clone())];
    if show_args {
        for (index, value) in args
            .split(" · ")
            .filter(|value| !value.is_empty())
            .enumerate()
        {
            parts.push(if index == 0 {
                ToolDetailPart::flexible(tool_target(value), 0, 1).attached()
            } else {
                ToolDetailPart::fixed(value.to_owned())
            });
        }
        if include_call_id && !call.is_empty() {
            parts.push(ToolDetailPart::flexible(call, 0, 1));
        }
    } else {
        if let Some(target) = compact_target {
            parts.push(ToolDetailPart::flexible(target, 0, 1).attached());
        }
        if let Some(stats) = compact_stats {
            parts.push(ToolDetailPart::fixed(stats));
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
    } else if let Some(result) = compact_result {
        parts.push(ToolDetailPart::flexible(result, 1, 1));
    }
    if let Some(value) = state.duration_ms {
        parts.push(ToolDetailPart::fixed(duration_label(value)));
    }
    if let Some(value) = status.filter(|_| !preview_shown) {
        parts.push(ToolDetailPart::fixed(value.to_owned()));
    }
    let occupied = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(glyph_text.as_str());
    let detail = fit_tool_detail(parts, (width as usize).saturating_sub(occupied));
    let mut header_spans = vec![
        Span::styled(marker.to_owned(), palette.muted),
        Span::styled(glyph_text, glyph_style),
    ];
    header_spans.extend(tool_detail_spans(&detail, &title, name_style, palette));
    let mut lines = vec![Line::from(header_spans)];
    if block.fold == FoldState::Expanded && state.has_expanded_body() {
        let body_width = width.saturating_sub(4).max(1);
        lines.extend(cache.wrapped_body(
            block,
            BodyKind::ToolOutput,
            width,
            block.lifecycle != BlockLifecycle::Streaming,
            || {
                // Same text the layout measures; the diff lines come first
                // and take their style from their marker.
                let diff_lines = state.diff_lines().len();
                state
                    .expanded_body()
                    .split('\n')
                    .enumerate()
                    .flat_map(|(index, source)| {
                        let style = if index >= diff_lines {
                            palette.secondary
                        } else if source.starts_with("+ ") {
                            palette.diff_add
                        } else if source.starts_with("- ") {
                            palette.diff_remove
                        } else {
                            palette.muted
                        };
                        render_plain(source, body_width)
                            .into_iter()
                            .map(move |row| Line::from(Span::styled(format!("    {row}"), style)))
                    })
                    .collect()
            },
            cached_lines_bytes,
        ));
    }
    lines
}

fn first_tool_segment(value: &str) -> Option<String> {
    value
        .split(" · ")
        .map(str::trim)
        .find(|part| !part.is_empty())
        .map(str::to_owned)
}

/// `+N -M` line stats that the event projection appends to a patch summary.
fn edit_stats_segment(segment: &str) -> Option<(&str, &str)> {
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    let (added, removed) = segment.trim().strip_prefix('+')?.split_once(" -")?;
    (digits(added) && digits(removed)).then_some((added, removed))
}

/// Collapsed result segment worth a row: a clean exit and the textual
/// patch/write receipts add nothing that the title and stats do not say.
fn compact_tool_result(name: &str, segment: String) -> Option<String> {
    (!matches!(name, "patch" | "write") && segment != "exit 0").then_some(segment)
}

fn tool_detail_spans(
    detail: &str,
    title: &str,
    action_style: Style,
    palette: &Palette,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, segment) in detail.split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", palette.muted));
        }
        // The title and its attached target share the first segment.
        if let Some(target) = (index == 0)
            .then(|| segment.strip_prefix(title))
            .flatten()
            .and_then(|rest| rest.strip_prefix(' '))
        {
            spans.push(Span::styled(title.to_owned(), action_style));
            spans.push(Span::styled(format!(" {target}"), palette.secondary));
            continue;
        }
        if index > 0 {
            if let Some((added, removed)) = edit_stats_segment(segment) {
                spans.push(Span::styled(format!("+{added}"), palette.diff_add));
                spans.push(Span::styled(" ", palette.muted));
                spans.push(Span::styled(format!("-{removed}"), palette.diff_remove));
                continue;
            }
        }
        let style = if index == 0 {
            action_style
        } else if is_duration_segment(segment) {
            palette.muted
        } else {
            palette.secondary
        };
        spans.push(Span::styled(segment.to_owned(), style));
    }
    spans
}

fn is_duration_segment(segment: &str) -> bool {
    let trimmed = segment.trim();
    let numeric = |value: &str| {
        !value.is_empty()
            && value
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
    };
    trimmed.strip_suffix("ms").is_some_and(numeric)
        || trimmed
            .strip_suffix('s')
            .is_some_and(|value| numeric(value) || value.strip_suffix('m').is_some_and(numeric))
}

struct ToolDetailPart {
    text: String,
    shrink_priority: Option<u8>,
    min_width: usize,
    /// Joined to the previous part by a space instead of ` · `, so a verb
    /// and its target read as one phrase.
    attached: bool,
}

impl ToolDetailPart {
    fn fixed(text: String) -> Self {
        Self {
            text,
            shrink_priority: None,
            min_width: 0,
            attached: false,
        }
    }

    fn flexible(text: String, priority: u8, min_width: usize) -> Self {
        Self {
            text,
            shrink_priority: Some(priority),
            min_width,
            attached: false,
        }
    }

    fn attached(mut self) -> Self {
        self.attached = true;
        self
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
    let mut joined = String::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            joined.push_str(if part.attached { " " } else { " · " });
        }
        joined.push_str(&part.text);
    }
    truncate_cells(&joined, width)
}

fn tool_detail_width(parts: &[ToolDetailPart]) -> usize {
    let separators = parts
        .iter()
        .skip(1)
        .map(|part| {
            if part.attached {
                1
            } else {
                UnicodeWidthStr::width(" · ")
            }
        })
        .sum::<usize>();
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

fn recent_transition(at_ms: Option<u64>, now_ms: u64, capabilities: Capabilities) -> bool {
    !capabilities.reduced_motion
        && at_ms.is_some_and(|at| now_ms.saturating_sub(at) < INFO_TOAST_HIGHLIGHT_MS)
}

fn block_transition_recent(block: &Block, ctx: &BlockRender<'_>) -> bool {
    recent_transition(
        block.ended_ms.or(block.started_ms),
        ctx.now_ms,
        ctx.capabilities,
    )
}

fn user_message_lines(text: &str, ctx: &BlockRender<'_>, recent: bool) -> Vec<Line<'static>> {
    let surface = ctx.palette.user_prompt_bg;
    let marker_style = if recent {
        ctx.palette.accent_bold
    } else {
        ctx.palette.accent.add_modifier(Modifier::DIM)
    };
    // Pad every band row to the content width: `set_line` paints only the
    // spans it is given, and the band reads as one surface only when filled.
    let band = |mut spans: Vec<Span<'static>>| {
        let used: usize = spans.iter().map(Span::width).sum();
        let fill = usize::from(ctx.width).saturating_sub(used);
        if fill > 0 {
            spans.push(Span::styled(" ".repeat(fill), surface));
        }
        Line::from(spans)
    };
    let mut lines = vec![band(vec![
        Span::styled("  ", surface),
        Span::styled(
            glyph(ctx.capabilities, '●', '*').to_string(),
            marker_style.patch(surface),
        ),
        Span::styled(" Você", ctx.palette.secondary.patch(surface)),
    ])];
    let indent = " ".repeat(crate::render::USER_PROMPT_PREFIX_COLS as usize);
    lines.extend(
        render_prose(text, user_prompt_text_width(ctx.width))
            .into_iter()
            .map(|row| {
                band(vec![Span::styled(
                    format!("{indent}{row}"),
                    ctx.palette.text.patch(surface),
                )])
            }),
    );
    lines.push(Line::default());
    lines
}

fn question_block_lines(
    state: &InteractionRequestState,
    ctx: &BlockRender<'_>,
    recent: bool,
) -> Vec<Line<'static>> {
    let width = ctx.width.saturating_sub(2).max(8) as usize;
    state
        .layout_lines(width)
        .into_iter()
        .map(|row| {
            if row.is_empty() {
                return Line::default();
            }
            let selected_marker = question_option_marker(true);
            let unselected_marker = question_option_marker(false);
            let selected = row.starts_with(selected_marker);
            let option = selected || row.starts_with(unselected_marker);
            let prompt = row.starts_with("? ");
            let status = row.trim_start().starts_with('·');
            if option {
                let style = if recent {
                    ctx.palette.text.add_modifier(Modifier::BOLD)
                } else {
                    ctx.palette.text
                };
                return Line::from(Span::styled(pad_cells(&row, width), style));
            }
            if prompt {
                return Line::from(vec![
                    Span::styled(
                        "? ".to_owned(),
                        if recent {
                            ctx.palette.accent.add_modifier(Modifier::BOLD)
                        } else {
                            ctx.palette.accent
                        },
                    ),
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
    now_ms: u64,
    animate_thinking: bool,
    assistant_chrome: Option<crate::block::AssistantChrome>,
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
    let phase_label = match block.lifecycle {
        BlockLifecycle::Streaming => "Pensando".into(),
        BlockLifecycle::Cancelled => "Pensamento · interrompido".into(),
        BlockLifecycle::Failed => "Pensamento · falhou".into(),
        _ if count > 1 => format!("Pensamento ×{count}"),
        _ => "Pensamento".into(),
    };
    let label = thinking_duration_ms(block, ctx.now_ms)
        .filter(|_| count <= 1)
        .filter(|duration| *duration > 0)
        .map_or(phase_label.clone(), |duration| {
            format!("{phase_label} · {}", duration_label(duration))
        });
    let label = truncate_cells(&label, ctx.width.saturating_sub(4) as usize);
    let recent_end = !streaming && block_transition_recent(block, ctx);
    let label_style = if streaming {
        ctx.palette.secondary
    } else if recent_end {
        ctx.palette.secondary.add_modifier(Modifier::BOLD)
    } else {
        ctx.palette.muted
    };
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
        Span::styled(label.clone(), label_style),
    ];
    if ctx.selected {
        let hint = if expanded {
            "Enter recolher"
        } else {
            "Enter expandir"
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

fn thinking_duration_ms(block: &Block, now_ms: u64) -> Option<u64> {
    let started = block.started_ms?;
    let ended = block
        .ended_ms
        .or_else(|| (block.lifecycle == BlockLifecycle::Streaming).then_some(now_ms))?;
    Some(ended.saturating_sub(started))
}

fn block_lines(block: &Block, ctx: &BlockRender<'_>, cache: &mut WrapCache) -> Vec<Line<'static>> {
    let mut lines = match block.kind() {
        BlockKind::User(text) => user_message_lines(text, ctx, block_transition_recent(block, ctx)),
        BlockKind::Assistant(text) => {
            let mut lines = Vec::new();
            if ctx
                .assistant_chrome
                .is_none_or(|chrome| chrome.shows_header())
            {
                let cancelled = matches!(
                    ctx.assistant_chrome,
                    Some(crate::block::AssistantChrome::Header {
                        cancelled: true,
                        ..
                    })
                );
                let failed = matches!(
                    ctx.assistant_chrome,
                    Some(crate::block::AssistantChrome::Header { failed: true, .. })
                );
                let lifecycle = if cancelled {
                    BlockLifecycle::Cancelled
                } else {
                    block.lifecycle
                };
                let label_style = if cancelled
                    || failed
                    || matches!(
                        block.lifecycle,
                        BlockLifecycle::Cancelled | BlockLifecycle::Failed
                    ) {
                    ctx.palette.secondary
                } else {
                    ctx.palette.accent_bold
                };
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    format!("  {}", assistant_label(lifecycle)),
                    label_style,
                )));
            }
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
                // Reasoning reads apart from the answer by italic.
                let thinking_style = ctx.palette.thinking.add_modifier(Modifier::ITALIC);
                // Expanded bodies are wrapped once per generation and shared
                // with the height probe instead of re-wrapping every frame.
                lines.extend(cache.wrapped_body(
                    block,
                    BodyKind::Thinking,
                    ctx.width,
                    !streaming,
                    || {
                        render_prose(text, body_width)
                            .into_iter()
                            .map(|row| {
                                Line::from(Span::styled(format!("    {row}"), thinking_style))
                            })
                            .collect()
                    },
                    cached_lines_bytes,
                ));
            } else if block.shows_thinking_preview() {
                let preview_width = thinking_body_width(ctx.width);
                let (rows, truncated) = cache.thinking_preview(block, text, preview_width);
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
                        ctx.palette.thinking.add_modifier(Modifier::ITALIC),
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
            ctx.now_ms,
            false,
            cache,
        ),
        BlockKind::InteractionRequest(state) => {
            if state.acknowledgement.is_none() {
                Vec::new()
            } else {
                question_block_lines(state, ctx, block_transition_recent(block, ctx))
            }
        }
        BlockKind::System(text) => vec![Line::from(Span::styled(
            format!("  sistema · {}", sanitize_terminal_text(text)),
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
        BlockKind::QueuedUser(text) => {
            let style = if block_transition_recent(block, ctx) {
                ctx.palette.warning.add_modifier(Modifier::BOLD)
            } else {
                ctx.palette.muted
            };
            render_plain(text, ctx.width.saturating_sub(4).max(1))
                .into_iter()
                .map(|row| Line::from(Span::styled(format!("  … {row}"), style)))
                .collect()
        }
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
        code: ctx.palette.inline_code,
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
    if interaction.response_pending {
        return Some("aguardando confirmação");
    }
    match &interaction.kind {
        InteractionRequestKind::Approval { .. } if !state.approval_content_accessible => {
            Some("aprovação · amplie o terminal para ler")
        }
        InteractionRequestKind::Approval { .. } => Some("aprovação · Y/N"),
        InteractionRequestKind::Input { .. } => Some("resposta · Enter"),
        InteractionRequestKind::Question { options, .. } => {
            if options.is_empty() || interaction.custom_question_answer {
                Some("resposta · Enter")
            } else {
                Some("escolha · ↑↓ Enter")
            }
        }
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

/// Returns whether the frame owns a text caret.  List and choice overlays
/// intentionally keep the native cursor hidden; only their filter fields (and
/// the API-key field) capture text input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputCursorTarget {
    Composer,
    LoginApiKey,
    Search,
    ModelFilter,
    Palette,
}

fn input_cursor_target(state: &AppState) -> Option<InputCursorTarget> {
    // Match the visual/input priority: the last painted capturing overlay
    // wins, while a non-text modal suppresses every cursor beneath it.
    if state.palette_query.is_some() {
        return Some(InputCursorTarget::Palette);
    }
    if let Some(login) = &state.login_overlay {
        return matches!(&login.stage, LoginStage::ApiKey(_))
            .then_some(InputCursorTarget::LoginApiKey)
            .filter(|_| !login.in_progress);
    }
    if state.effort_overlay.is_some() || state.mcp_overlay.is_some() {
        return None;
    }
    if state.model_overlay.is_some() {
        return Some(InputCursorTarget::ModelFilter);
    }
    if state.search.is_some() {
        return Some(InputCursorTarget::Search);
    }
    composer_is_focused(state).then_some(InputCursorTarget::Composer)
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
    let status = match &interaction.acknowledgement {
        Some(acknowledgement) if acknowledgement.message.is_empty() => {
            if acknowledgement.accepted {
                "aceito"
            } else {
                "rejeitado"
            }
        }
        Some(acknowledgement) => acknowledgement.message.as_str(),
        None if interaction.response_pending => "resposta enviada · aguardando confirmação",
        None => "aguardando resposta",
    };
    status.to_owned()
}

fn interaction_overlay_chrome(
    interaction: &InteractionRequestState,
) -> (&'static str, &'static str) {
    match &interaction.kind {
        InteractionRequestKind::Question { options, .. } => {
            let hint = if interaction.response_pending {
                "aguardando"
            } else if options.is_empty() || interaction.custom_question_answer {
                "resposta · Enter"
            } else {
                "↑↓ · Enter"
            };
            (" Pergunta ", hint)
        }
        InteractionRequestKind::Approval { .. } => {
            let hint = if interaction.response_pending {
                "aguardando"
            } else {
                "Y aprovar · N rejeitar"
            };
            (" Aprovação ", hint)
        }
        InteractionRequestKind::Input { .. } => {
            let hint = if interaction.response_pending {
                "aguardando"
            } else {
                "Enter responder"
            };
            (" Entrada ", hint)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InteractionOverlayGeometry {
    x: u16,
    bottom_y: u16,
    width: u16,
    inner_width: usize,
    max_height: u16,
}

fn interaction_overlay_geometry(
    frame_area: ratatui::layout::Rect,
    composer_area: ratatui::layout::Rect,
) -> Option<InteractionOverlayGeometry> {
    // A card shorter than four rows cannot expose its border, content and
    // controls reliably.  Report it as unavailable to approval gating so a
    // tiny resize never leaves an apparently actionable Y/N prompt whose
    // summary is clipped out of the terminal.
    if frame_area.width < 24 || frame_area.height < 4 || composer_area.y < 4 {
        return None;
    }
    let width = question_card_width(composer_area.width.saturating_add(4)).min(composer_area.width);
    let inner_width = width.saturating_sub(4).max(8) as usize;
    let max_height = composer_area.y.min(frame_area.height);
    if max_height < 4 {
        return None;
    }
    Some(InteractionOverlayGeometry {
        x: composer_area.x,
        bottom_y: composer_area.y,
        width,
        inner_width,
        max_height,
    })
}

fn approval_overlay_geometry_for_size(
    state: &AppState,
    size: (u16, u16),
    cache: &mut WrapCache,
) -> Option<InteractionOverlayGeometry> {
    let interaction = state.pending_interaction()?;
    if !matches!(interaction.kind, InteractionRequestKind::Approval { .. })
        || interaction.acknowledgement.is_some()
    {
        return None;
    }
    let regions = plan_regions(state, size.0, size.1, cache);
    let workspace = workspace_regions(state, to_ratatui(regions.scrollback));
    let band = workspace_band(&workspace);
    let composer_area = chrome_area(to_ratatui(regions.composer), band);
    let frame_area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: size.0,
        height: size.1,
    };
    interaction_overlay_geometry(frame_area, composer_area)
}

fn approval_overlay_metrics(
    state: &AppState,
    size: (u16, u16),
    cache: &mut WrapCache,
) -> Option<(ratatui::layout::Rect, usize, usize)> {
    let geometry = approval_overlay_geometry_for_size(state, size, cache)?;
    let palette = Palette::of(Capabilities {
        color_depth: ColorDepth::None,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    });
    let interaction = state.pending_interaction()?;
    let mut lines = interaction_overlay_lines(interaction, geometry.inner_width, &palette);
    let bordered = |content: usize| (content as u16).saturating_add(2);
    if bordered(lines.len()) > geometry.max_height {
        lines.retain(line_has_text);
    }
    let height = bordered(lines.len()).clamp(4, geometry.max_height);
    let inner_rows = height.saturating_sub(2) as usize;
    if inner_rows == 0 || lines.is_empty() {
        return Some((
            ratatui::layout::Rect {
                x: geometry.x,
                y: geometry.bottom_y.saturating_sub(height),
                width: geometry.width,
                height,
            },
            lines.len(),
            0,
        ));
    }
    // One row is reserved for the range hint whenever the approval needs to
    // scroll.  The content itself is never replaced by an ellipsis.
    let capacity = if lines.len() > inner_rows {
        inner_rows.saturating_sub(1).max(1)
    } else {
        inner_rows
    };
    Some((
        ratatui::layout::Rect {
            x: geometry.x,
            y: geometry.bottom_y.saturating_sub(height),
            width: geometry.width,
            height,
        },
        lines.len(),
        capacity,
    ))
}

fn approval_content_accessible(
    state: &AppState,
    size: (u16, u16),
    cache: &mut WrapCache,
) -> Option<bool> {
    let interaction = state.pending_interaction()?;
    if !matches!(interaction.kind, InteractionRequestKind::Approval { .. })
        || interaction.acknowledgement.is_some()
    {
        return None;
    }
    let Some((_, total_rows, capacity)) = approval_overlay_metrics(state, size, cache) else {
        return Some(false);
    };
    Some(total_rows > 0 && capacity > 0)
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
                    &format!("opções: {}", options.join(" · ")),
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
        push_wrapped(&mut lines, "enviado", palette.muted);
    } else if matches!(&interaction.kind, InteractionRequestKind::Question { .. })
        && interaction.custom_question_answer
    {
        push_wrapped(&mut lines, "digite a resposta abaixo", palette.muted);
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
        let marker = question_option_marker(selected_here);
        let label = truncate_cells(
            &sanitize_terminal_text(label),
            inner_width.saturating_sub(UnicodeWidthStr::width(marker)),
        );
        lines.push(overlay_option_row(
            &format!("{marker}{label}"),
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
    capabilities: Capabilities,
) {
    let Some(interaction) = state.pending_interaction() else {
        return;
    };
    if interaction.acknowledgement.is_some() {
        return;
    }
    let frame_area = frame.area();
    let Some(geometry) = interaction_overlay_geometry(frame_area, composer_area) else {
        return;
    };
    let width = geometry.width;
    let inner_width = geometry.inner_width;
    let (title, hint) = interaction_overlay_chrome(interaction);
    let interaction_recent = !capabilities.reduced_motion
        && state.blocks().iter().rev().any(|block| {
            block.lifecycle == BlockLifecycle::Pending
                && matches!(block.kind(), BlockKind::InteractionRequest(_))
                && recent_transition(block.started_ms, state.clock.elapsed_ms, capabilities)
        });
    let mut lines = interaction_overlay_lines(interaction, inner_width, palette);
    let max_height = geometry.max_height;
    let bordered = |content: usize| (content as u16).saturating_add(2);
    let approval = matches!(interaction.kind, InteractionRequestKind::Approval { .. });
    if bordered(lines.len()) > max_height {
        lines.retain(line_has_text);
    }
    if !approval && bordered(lines.len()) > max_height {
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
    if approval {
        let total_rows = lines.len();
        if total_rows > inner_rows {
            let capacity = inner_rows.saturating_sub(1).max(1);
            let start = state.approval_scroll.start(total_rows, capacity);
            let end = start.saturating_add(capacity).min(total_rows);
            let mut visible = lines[start..end].to_vec();
            visible.push(Line::from(Span::styled(
                format!(" ↑↓ rolar · {}-{}/{}", start + 1, end, total_rows),
                palette.muted,
            )));
            lines = visible;
        }
    } else if lines.len() > inner_rows {
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
    let border_style = if interaction_recent {
        palette.border_focus.add_modifier(Modifier::BOLD)
    } else {
        palette.border_focus
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            RatatuiBlock::default()
                .title(Line::from(Span::styled(title, palette.accent_bold)))
                .title(Line::from(Span::styled(format!(" {hint} "), palette.muted)).right_aligned())
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(border_style)
                .style(palette.surface_alt),
        ),
        area,
    );
}

fn composer_label(state: &AppState, area_width: u16, total_lines: usize) -> String {
    let lines = (total_lines > 1).then(|| format!("{total_lines} linhas"));
    let question = question_composer_hint(state).map(str::to_owned);
    let queue = if state.queue_paused {
        let pending = state.queued_prompts.len();
        Some(format!(
            "fila pausada · {pending} pendente{} · /queue resume",
            if pending == 1 { "" } else { "s" }
        ))
    } else if state.working {
        let pending = state.queued_prompts.len();
        if state.queue_paused {
            Some(format!(
                "fila pausada · {pending} pendente{}",
                if pending == 1 { "" } else { "s" }
            ))
        } else if pending > 0 {
            Some(format!(
                "fila · {pending} pendente{}",
                if pending == 1 { "" } else { "s" }
            ))
        } else if !state.composer.is_empty() {
            Some("Enter enfileira".to_owned())
        } else {
            None
        }
    } else {
        None
    };
    let images = (!state.attachment_labels.is_empty()).then(|| {
        format!(
            "{} imagem{}",
            state.attachment_labels.len(),
            if state.attachment_labels.len() == 1 {
                ""
            } else {
                "s"
            }
        )
    });
    let metadata = [question, queue, images, lines]
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
    cursor_focused: bool,
) {
    if area.height == 0 {
        return;
    }
    let focused = cursor_focused;
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
    let placeholder = composer_placeholder(state, focused);
    let mut rendered = Vec::with_capacity(end.saturating_sub(start));
    for row_index in start..end {
        if row_index < attachment_rows {
            let label = if row_index + 1 == attachment_rows
                && state.attachment_labels.len() > attachment_rows
            {
                format!(
                    "imagem · +{} mais",
                    state.attachment_labels.len() - row_index
                )
            } else {
                format!("imagem · {}", state.attachment_labels[row_index])
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
        if let Some(placeholder) = placeholder.filter(|_| is_cursor_line && line.is_empty()) {
            rendered.push(Line::from(vec![
                Span::styled(prompt, glyph_style),
                Span::styled(truncate_cells(placeholder, text_budget), palette.muted),
            ]));
            continue;
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

/// Muted hint on an empty, focused composer once the conversation started.
/// The welcome keeps its own contextual hint, and pending questions or
/// attachments already describe what Enter does.
fn composer_placeholder(state: &AppState, focused: bool) -> Option<&'static str> {
    if !focused
        || !state.composer.is_empty()
        || !state.attachment_labels.is_empty()
        || state.blocks().is_empty()
        || question_composer_hint(state).is_some()
    {
        return None;
    }
    Some(if state.working {
        "Escreva para enfileirar a próxima mensagem"
    } else {
        "Responda ou descreva a próxima tarefa · / comandos"
    })
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
    capabilities: Capabilities,
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
            &footer_line(
                index,
                area.height,
                text,
                state,
                activity_visible,
                capabilities,
                palette,
            ),
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
    capabilities: Capabilities,
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
            footer_segment_style(segment, active, unread_value, state, capabilities, palette),
        ));
        if segment_index + 1 < segment_count {
            spans.push(Span::styled(" · ", palette.muted));
        }
    }
    Line::from(spans)
}

fn footer_segment_style(
    segment: &str,
    active: bool,
    unread_value: bool,
    state: &AppState,
    capabilities: Capabilities,
    palette: &Palette,
) -> Style {
    let lower = segment.to_ascii_lowercase();
    if lower.starts_with("execução ") {
        if let Some(execution) = &state.last_execution {
            let outcome_style = match execution.outcome {
                crate::app::RunOutcomeKind::Completed => palette.secondary,
                crate::app::RunOutcomeKind::Interrupted => palette.warning,
                crate::app::RunOutcomeKind::Failed => palette.error,
            };
            return if recent_transition(
                Some(execution.ended_ms),
                state.clock.elapsed_ms,
                capabilities,
            ) {
                outcome_style.add_modifier(Modifier::BOLD)
            } else {
                outcome_style
            };
        }
    }
    if lower.contains("erro")
        || lower.contains("failed")
        || lower.contains("falha")
        || lower.contains("falhou")
    {
        return palette.error;
    }
    if lower.contains("retry")
        || lower.contains("tentativa")
        || lower.contains("warning")
        || lower.contains("limite")
    {
        return palette.warning;
    }
    if unread_value {
        return palette.warning;
    }
    if confirmed_setting_highlight(segment, state, capabilities) {
        return palette.secondary.add_modifier(Modifier::BOLD);
    }
    // Key hints are intentionally quiet even when the footer has only one
    // row.  The mode/model/effort values remain readable in the secondary
    // tone, while context and counts stay neutral.
    if footer_segment_is_shortcut(&lower) {
        return palette.muted;
    }
    if lower.contains("ctx") || lower.contains("context") {
        return palette.muted;
    }
    if active {
        palette.secondary.add_modifier(Modifier::BOLD)
    } else {
        palette.secondary
    }
}

fn confirmed_setting_highlight(
    segment: &str,
    state: &AppState,
    capabilities: Capabilities,
) -> bool {
    if capabilities.reduced_motion {
        return false;
    }
    let Some((setting, at_ms)) = state.confirmed_setting.as_ref() else {
        return false;
    };
    if state.clock.elapsed_ms.saturating_sub(*at_ms) >= INFO_TOAST_HIGHLIGHT_MS {
        return false;
    }
    let segment = segment.to_ascii_lowercase();
    match setting {
        crate::app::ConfirmedSetting::Model => {
            let model = crate::api::ModelAlias::parse(&state.model).map_or_else(
                || state.model.to_ascii_lowercase(),
                |alias| alias.label().to_ascii_lowercase(),
            );
            segment.contains(&model)
        }
        crate::app::ConfirmedSetting::Effort => {
            segment.contains(&format!("({})", state.effort.id().to_ascii_lowercase()))
        }
        crate::app::ConfirmedSetting::Mode => segment == mode_name(state.mode).to_ascii_lowercase(),
    }
}

fn footer_segment_is_shortcut(lower: &str) -> bool {
    [
        "ctrl+", "shift+", "esc", "enter", "space", "pageup", "pagedown", "home", "end", "↑", "↓",
        "←", "→", "f1", "tab",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Places a caret inside a painted rectangle while keeping tiny terminal
/// sizes inside the frame.  `offset_x`/`offset_y` are measured in display
/// cells from the rectangle's origin.
fn set_cursor_in_rect(
    frame: &mut ratatui::Frame,
    rect: ratatui::layout::Rect,
    offset_x: usize,
    offset_y: usize,
) {
    let rect = rect.intersection(frame.area());
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let x = rect
        .x
        .saturating_add(u16::try_from(offset_x).unwrap_or(u16::MAX))
        .min(rect.right().saturating_sub(1));
    let y = rect
        .y
        .saturating_add(u16::try_from(offset_y).unwrap_or(u16::MAX))
        .min(rect.bottom().saturating_sub(1));
    frame.set_cursor_position((x, y));
}

fn render_search_bar(
    frame: &mut ratatui::Frame,
    scrollback: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    matches: &[usize],
    cursor_focused: bool,
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
    let prefix = " Buscar: ";
    let scope = format!(" [{}] ", search.filter.label());
    let suffixes = [
        format!("{scope} {position} · Enter próxima · Tab filtro · Esc "),
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
        crate::view_model::truncate_display_width("Digite para buscar", query_budget)
    } else {
        truncate_search_query(safe_query.as_ref(), query_budget)
    };
    let query_cursor_width = if safe_query.is_empty() {
        0
    } else {
        UnicodeWidthStr::width(query.as_str())
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
    if cursor_focused {
        set_cursor_in_rect(
            frame,
            area,
            prefix_width.saturating_add(query_cursor_width),
            0,
        );
    }
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
    capabilities: Capabilities,
    cache: &mut WrapCache,
) {
    if scrollback.width < 12 || scrollback.height < 4 {
        return;
    }
    let area = inspector_overlay_area(frame.area());
    render_inspector_panel(frame, area, state, Some(kind), palette, capabilities, cache);
}

fn render_inspector_panel(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    kind: Option<InspectorKind>,
    palette: &Palette,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) {
    if area.width < 12 || area.height < 4 {
        return;
    }
    frame.render_widget(Clear, area);
    let title = kind.map_or("Execução", inspector_title);
    let hint = if kind.is_some() {
        " ↑↓ rolar · Esc fechar "
    } else {
        " Ctrl+P comandos "
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
    if !capabilities.reduced_motion
        && state.last_execution.as_ref().is_some_and(|execution| {
            recent_transition(
                Some(execution.ended_ms),
                state.clock.elapsed_ms,
                capabilities,
            )
        })
    {
        if let Some(line) = visible.iter_mut().find(|line| {
            line.spans
                .first()
                .is_some_and(|span| span.content.as_ref().trim_start().starts_with("Última"))
        }) {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
        }
    }
    if metrics.total_rows > metrics.capacity {
        let footer = format!(
            " ↑↓ rolar · {}-{}/{} · Home/End",
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
        InspectorKind::Diff => "Operações de alteração",
        InspectorKind::Activity => "Atividade",
        InspectorKind::SessionTree => "Sessão",
        InspectorKind::Diagnostics => "Diagnósticos",
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
        "Ocioso".into()
    };
    let elapsed = if active {
        format!("{}s", activity_elapsed(state))
    } else {
        "--".into()
    };
    let content_clock =
        semantic_clock_label(state.clock.elapsed_ms, state.last_provider_content_ms);
    let tool_clock = semantic_clock_label(state.clock.elapsed_ms, state.last_tool_progress_ms);
    let reasoning = reasoning_classification_label(state.reasoning_classification);
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
        format!(
            "{completed_tools} concluída(s) · {failed_tools} falha(s) · {active_tools} ativa(s)"
        )
    } else {
        format!("{completed_tools} concluída(s) · {failed_tools} falha(s)")
    };
    let error_style = if errors > 0 {
        palette.warning
    } else {
        palette.muted
    };
    let mut lines = vec![
        row("Estado", status.into(), status_style),
        row("Fase", phase, palette.text),
        row("Decorrido", elapsed, palette.muted),
        row("Conteúdo", content_clock, palette.muted),
        row("Progresso", tool_clock, palette.muted),
        row("Raciocínio", reasoning.into(), palette.muted),
        Line::default(),
        row("Ferramentas", tools, palette.text),
        row("Erros", errors.to_string(), error_style),
        row(
            "Turnos",
            format!("{}/{}", state.turns_used, state.max_turns),
            palette.text,
        ),
        row("Contexto", format_context(state, true), palette.text),
        Line::default(),
        Line::from(Span::styled(" Ctrl+J  atividade", palette.muted)),
        Line::from(Span::styled(" Ctrl+D  alterações", palette.muted)),
        Line::from(Span::styled(" Ctrl+R  sessão", palette.muted)),
        Line::from(Span::styled(" Ctrl+G  diagnósticos", palette.muted)),
    ];
    if let Some(execution) = &state.last_execution {
        let pending = if execution.pending_count > 0 {
            format!(
                " · {} pendente{}",
                execution.pending_count,
                if execution.pending_count == 1 {
                    ""
                } else {
                    "s"
                }
            )
        } else {
            String::new()
        };
        lines.insert(
            3,
            row(
                "Última",
                format!(
                    "{} · {}{pending}",
                    execution_outcome_label(execution.outcome),
                    duration_label(execution.duration_ms)
                ),
                palette.muted,
            ),
        );
    }
    if let Some(reason) = state
        .retry
        .as_ref()
        .and_then(|retry| retry.reason.as_deref())
    {
        lines.push(row("Motivo", reason.to_owned(), palette.muted));
    }
    if state.working {
        lines.push(Line::from(Span::styled(
            " Ctrl+C  cancelar execução",
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
                        format!("{} · histórico", tool.name),
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
                    " Nenhuma operação de alteração reportada",
                    palette.muted,
                )));
            }
        }
        InspectorKind::Activity => {
            if state.activity.is_some()
                || state.last_provider_content_ms.is_some()
                || state.last_tool_progress_ms.is_some()
                || state.reasoning_classification.is_some()
            {
                lines.push(Line::from(Span::styled(" Relógios", palette.secondary)));
                lines.push(Line::from(vec![
                    Span::styled(" Conteúdo ", palette.secondary),
                    Span::styled(
                        semantic_clock_label(
                            state.clock.elapsed_ms,
                            state.last_provider_content_ms,
                        ),
                        palette.muted,
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(" Progresso ", palette.secondary),
                    Span::styled(
                        semantic_clock_label(state.clock.elapsed_ms, state.last_tool_progress_ms),
                        palette.muted,
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(" Raciocínio ", palette.secondary),
                    Span::styled(
                        truncate(reasoning_classification_label(
                            state.reasoning_classification,
                        )),
                        palette.muted,
                    ),
                ]));
                lines.push(Line::default());
            }
            if state.activity.is_some() {
                lines.push(Line::from(Span::styled(
                    format!(" · {}", truncate(&activity_label(state))),
                    palette.warning,
                )));
                lines.push(Line::default());
            }
            if !state.activity_timeline.is_empty() {
                lines.push(Line::from(Span::styled(" Cronologia", palette.secondary)));
                let origin = state
                    .run_started_ms
                    .or_else(|| state.last_execution.as_ref().map(|run| run.started_ms))
                    .unwrap_or(0);
                for entry in state.activity_timeline.iter().rev().take(12).rev() {
                    let label = activity_phase_label(&entry.phase);
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(
                                " +{} ",
                                duration_label(entry.timestamp_ms.saturating_sub(origin))
                            ),
                            palette.muted,
                        ),
                        Span::styled(truncate(&label), palette.text),
                    ]));
                }
                lines.push(Line::default());
            }
            if !state.execution_history.is_empty() {
                lines.push(Line::from(Span::styled(" Execuções", palette.secondary)));
                for execution in state.execution_history.iter().rev().take(8).rev() {
                    let outcome_style = match execution.outcome {
                        crate::app::RunOutcomeKind::Completed => palette.success,
                        crate::app::RunOutcomeKind::Interrupted => palette.warning,
                        crate::app::RunOutcomeKind::Failed => palette.error,
                    };
                    let pending = if execution.pending_count > 0 {
                        format!(
                            " · {} pendente{}",
                            execution.pending_count,
                            if execution.pending_count == 1 {
                                ""
                            } else {
                                "s"
                            }
                        )
                    } else {
                        String::new()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" #{} ", execution.run_id), palette.muted),
                        Span::styled(execution_outcome_label(execution.outcome), outcome_style),
                        Span::styled(
                            format!(" · {}{pending}", duration_label(execution.duration_ms)),
                            palette.muted,
                        ),
                    ]));
                }
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
                        format!("{} · histórico", tool.name),
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
                lines.push(Line::from(Span::styled(
                    " Nenhuma atividade ainda",
                    palette.muted,
                )));
            }
        }
        InspectorKind::SessionTree => {
            for (index, block) in state.blocks().iter().enumerate() {
                let (label, text) = match block.kind() {
                    BlockKind::User(text) => ("Você", text.as_str()),
                    BlockKind::Assistant(text) => ("Slim", text.as_str()),
                    BlockKind::Thinking(text) => ("Pensamento", text.as_str()),
                    BlockKind::Tool(tool) => ("Ferramenta", tool.name.as_str()),
                    BlockKind::InteractionRequest(_) => ("Entrada", "solicitação"),
                    BlockKind::System(text) => ("Sistema", text.as_str()),
                    BlockKind::Error(text) => ("Erro", text.as_str()),
                    BlockKind::Activity(text) => ("Atividade", text.as_str()),
                    BlockKind::QueuedUser(text) => ("Fila", text.as_str()),
                };
                let summary = text.lines().next().unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!(" {:>2} ", index + 1), palette.muted),
                    Span::styled(format!("{label:<8}"), palette.secondary),
                    Span::styled(truncate(summary), palette.text),
                ]));
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(" Sessão vazia", palette.muted)));
            }
        }
        InspectorKind::Diagnostics => {
            let provider = state
                .auth_provider
                .map(|provider| format!("{provider:?}"))
                .unwrap_or_else(|| "nenhum".into());
            let rows = [
                ("provedor", provider),
                ("modelo", state.model.clone()),
                ("modo", mode_name(state.mode).to_owned()),
                ("blocos", state.blocks().len().to_string()),
                (
                    "ferramentas",
                    format!(
                        "leituras {}/{} · alterações {}/{}",
                        state.tools_used_read,
                        state.max_read_tool_calls,
                        state.tools_used_mutating,
                        state.max_mutating_tool_calls
                    ),
                ),
                (
                    "turnos",
                    format!("{}/{}", state.turns_used, state.max_turns),
                ),
                ("contexto", format_context(state, false)),
                ("latência", format_provider_timings(state)),
            ];
            for (label, value) in rows {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {label:<9}"), palette.secondary),
                    Span::styled(truncate(&value), palette.text),
                ]));
            }
            append_notification_history(&mut lines, state, palette, width);
        }
    }
    lines
}

fn semantic_clock_label(now_ms: u64, timestamp_ms: Option<u64>) -> String {
    let Some(timestamp_ms) = timestamp_ms else {
        return "--".into();
    };
    let elapsed = now_ms.saturating_sub(timestamp_ms);
    if elapsed == 0 {
        "agora".into()
    } else {
        format!("há {}", duration_label(elapsed))
    }
}

fn reasoning_classification_label(
    classification: Option<slim_core::ReasoningClassification>,
) -> &'static str {
    match classification {
        Some(slim_core::ReasoningClassification::Summary) => {
            "Resumo de raciocínio disponibilizado pelo provedor"
        }
        Some(slim_core::ReasoningClassification::Text) => {
            "Texto de raciocínio disponibilizado pelo provedor"
        }
        None => "Classificação não informada",
    }
}

fn append_notification_history(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    palette: &Palette,
    width: u16,
) {
    if state.notification_history().is_empty() {
        return;
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(" Notificações", palette.secondary)));
    let content_width = width.saturating_sub(4) as usize;
    for notice in state.notification_history().iter().rev().take(8) {
        let prefix = if notice.repeat_count > 1 {
            format!(" ×{} ", notice.repeat_count)
        } else {
            " ·  ".to_owned()
        };
        let prefix_width = UnicodeWidthStr::width(prefix.as_str());
        let rows = wrap_words(
            &sanitize_terminal_text(notice.as_str()),
            content_width.saturating_sub(prefix_width).max(1),
        );
        let message_style = match notice.priority {
            NotificationPriority::Error => palette.error,
            NotificationPriority::Warning => palette.warning,
            NotificationPriority::Info => palette.text,
        };
        for (index, row) in rows.into_iter().enumerate() {
            let indent = if index == 0 {
                prefix.clone()
            } else {
                " ".repeat(prefix_width)
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {indent}"), palette.muted),
                Span::styled(row, message_style),
            ]));
        }
    }
}

fn execution_outcome_label(outcome: crate::app::RunOutcomeKind) -> &'static str {
    match outcome {
        crate::app::RunOutcomeKind::Completed => "concluída",
        crate::app::RunOutcomeKind::Interrupted => "interrompida",
        crate::app::RunOutcomeKind::Failed => "falhou",
    }
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
    cursor_focused: bool,
) {
    let frame_area = frame.area();
    let width = ((u32::from(frame_area.width) * 9) / 10) as u16;
    let width = width.clamp(24, 72).min(frame_area.width);
    // The first inner row is always the editable filter.  Keeping its
    // placeholder painted gives the caret a stable home even before typing.
    let reserved = 2;
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
    const FILTER_PREFIX: &str = " Filtro: ";
    let inner_width = usize::from(area.width.saturating_sub(2));
    let filter_prefix_width = UnicodeWidthStr::width(FILTER_PREFIX);
    let filter_budget = inner_width.saturating_sub(filter_prefix_width);
    let filter_display_budget = filter_budget.saturating_sub(1);
    let safe_filter = sanitize_terminal_text(&overlay.filter);
    let filter_value = if safe_filter.is_empty() {
        crate::view_model::truncate_display_width("Digite para filtrar…", filter_display_budget)
    } else {
        truncate_search_query(&safe_filter, filter_display_budget)
    };
    lines.push(Line::from(vec![
        Span::styled(FILTER_PREFIX, palette.muted),
        Span::styled(
            filter_value.clone(),
            if safe_filter.is_empty() {
                palette.muted
            } else {
                palette.text
            },
        ),
    ]));
    // A filter that matches nothing leaves the list empty. Explain it here
    // instead of showing dead headers, and leave Esc as the way back.
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            crate::view_model::truncate_display_width(
                "Nenhum modelo corresponde ao filtro · Esc para voltar",
                filter_display_budget,
            ),
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
        &format!(" {position} · Espaço recolher · Enter selecionar · Esc cancelar"),
        area.width.saturating_sub(2) as usize,
    );
    lines.push(Line::from(Span::styled(footer, palette.muted)));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Selecionar modelo ", palette)),
        area,
    );
    if cursor_focused {
        let inner = ratatui::layout::Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let value_width = if safe_filter.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(filter_value.as_str())
        };
        set_cursor_in_rect(
            frame,
            inner,
            filter_prefix_width.saturating_add(value_width),
            0,
        );
    }
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
            format!("remover {name}? y/Enter confirma · n/Esc cancela"),
            palette.warning,
        ),
        None => (
            format!("{position} · Enter testar · r reconectar · x desconectar · d remover · Esc"),
            palette.muted,
        ),
    };
    let footer_rows = wrap_words(&footer, row_budget.max(1));
    let max_inner = frame_area.height.saturating_sub(4) as usize;
    let list_budget = max_inner.saturating_sub(footer_rows.len().max(1)).max(1);
    let mut entries: Vec<Vec<Line>> = Vec::new();
    if servers.is_empty() {
        entries.push(vec![Line::from(Span::styled(
            " nenhum servidor configurado — /mcp add <name> <command>",
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
        Paragraph::new(lines).block(modal_block(" Servidores MCP ", palette)),
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
                .map(|count| format!("{count} ferramentas"))
                .unwrap_or_else(|| "pronto".to_owned()),
            None,
        ),
        McpStatusView::Connecting => ('\u{25CC}', palette.warning, "conectando".to_owned(), None),
        McpStatusView::Disconnected => ('\u{25CC}', palette.muted, "desconectado".to_owned(), None),
        McpStatusView::Disabled => ('\u{25CB}', palette.muted, "desativado".to_owned(), None),
        McpStatusView::Failed => (
            '\u{2715}',
            palette.error,
            "falhou".to_owned(),
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
    let levels = overlay.levels();
    // The speed row only exists for Codex aliases; catalog steps are one row
    // shorter instead of advertising a toggle that does nothing.
    let speed = overlay.speed_toggle();
    let area = centered(
        frame.area(),
        72,
        (levels.len() * 2 + if speed { 6 } else { 5 }) as u16,
    );
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
    lines.push(Line::default());
    if speed {
        lines.push(Line::from(Span::styled(
            format!(
                " Velocidade: {} · Tab alternar",
                if overlay.fast {
                    "Rápida (maior uso)"
                } else {
                    "Normal"
                }
            ),
            palette.muted,
        )));
    }
    lines.push(Line::from(Span::styled(
        " Enter selecionar · Esc voltar",
        palette.muted,
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Selecionar esforço ", palette)),
        area,
    );
}

fn masked_api_key(key: &crate::api::SensitiveText, budget: usize) -> String {
    let length = key.char_len();
    if length == 0 || budget == 0 {
        return String::new();
    }
    let mask_length = length.max(4);
    let visible = mask_length.min(budget);
    if visible == mask_length {
        return "•".repeat(visible);
    }
    if visible == 1 {
        return "•".into();
    }
    format!("…{}", "•".repeat(visible - 1))
}

fn render_login_overlay(
    frame: &mut ratatui::Frame,
    overlay: &LoginOverlay,
    palette: &Palette,
    cursor_focused: bool,
) {
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
        let area = centered(frame.area(), 58, 8);
        let inner = ratatui::layout::Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let mask_budget = usize::from(inner.width).saturating_sub(2);
        let mask = masked_api_key(key, mask_budget);
        let field = if mask.is_empty() {
            "Cole sua API key…".to_owned()
        } else {
            mask.clone()
        };
        let status = if overlay.in_progress {
            "Salvando…".to_owned()
        } else {
            overlay
                .progress
                .as_ref()
                .map(|progress| sanitize_terminal_text(progress))
                .unwrap_or_default()
        };
        let status = truncate_cells(&status, usize::from(inner.width));
        let field = truncate_cells(&field, mask_budget);
        let mut lines = vec![
            Line::default(),
            Line::from(vec![
                Span::styled(" ", palette.muted),
                Span::styled(
                    field.clone(),
                    if mask.is_empty() {
                        palette.muted
                    } else {
                        palette.text
                    },
                ),
            ]),
            Line::default(),
        ];
        if !status.is_empty() {
            lines.push(Line::from(Span::styled(
                status,
                if overlay.in_progress {
                    palette.warning
                } else {
                    palette.error
                },
            )));
        }
        lines.push(Line::from(Span::styled(
            " Enter salvar · Esc voltar",
            palette.muted,
        )));
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines).block(modal_block(title, palette)),
            area,
        );
        if cursor_focused && !overlay.in_progress {
            set_cursor_in_rect(
                frame,
                inner,
                1usize.saturating_add(if mask.is_empty() {
                    0
                } else {
                    UnicodeWidthStr::width(field.as_str())
                }),
                1,
            );
        }
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
        " Esc cancelar · Ctrl+C cancelar"
    } else {
        " Enter conectar · Esc cancelar"
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
        Paragraph::new(lines).block(modal_block(" Conectar provedor ", palette)),
        area,
    );
}

/// Key hint pinned to the bottom of the slash popup (§17.2): the same
/// vocabulary the model overlay and search bar already use.
const SLASH_POPUP_HINT: &str = "Tab completar · Enter executar · Esc";

fn render_slash_popup(
    frame: &mut ratatui::Frame,
    composer_area: ratatui::layout::Rect,
    suggestions: &SlashSuggestions,
    matches: &[String],
    palette: &Palette,
) {
    if matches.is_empty() || composer_area.y < 3 || composer_area.width < 4 {
        return;
    }
    // The list rests on the composer's top border at the composer's width,
    // so it reads as a dropdown of the input. One row holds the key hint.
    let capacity =
        usize::from(composer_area.y.saturating_sub(2).max(1)).min(PICKER_NOMINAL_CAPACITY);
    let visible = visible_window(matches.len(), suggestions.selected, capacity, 0);
    let inner_width = usize::from(composer_area.width.saturating_sub(2));
    let name_width = matches[visible.clone()]
        .iter()
        .map(|command| UnicodeWidthStr::width(command.as_str()))
        .max()
        .unwrap_or(0);
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
            let head = format!("{marker}{command:<name_width$}");
            let mut spans = vec![Span::styled(head.clone(), style)];
            let detail = crate::reducer::palette_description(command);
            let budget = inner_width.saturating_sub(UnicodeWidthStr::width(head.as_str()) + 2);
            if !detail.is_empty() && budget > 3 {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(truncate_cells(detail, budget), palette.muted));
            }
            menu_line(spans, selected, inner_width, palette)
        })
        .collect();
    // The hint aligns with the command names, past the selection marker.
    rows.push(Line::from(Span::styled(
        format!(
            "  {}",
            truncate_cells(SLASH_POPUP_HINT, inner_width.saturating_sub(2))
        ),
        palette.muted,
    )));
    let height = (rows.len() as u16 + 1).min(composer_area.y);
    let area = ratatui::layout::Rect {
        x: composer_area.x,
        y: composer_area.y - height,
        width: composer_area.width,
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(rows).block(
            RatatuiBlock::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_type(BorderType::Rounded)
                .border_style(palette.border_focus)
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
    cursor_focused: bool,
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
    let inner_width = usize::from(area.width.saturating_sub(2));
    let query_budget = inner_width.saturating_sub(2);
    let safe_query = sanitize_terminal_text(query);
    let query_value = if safe_query.is_empty() {
        crate::view_model::truncate_display_width("Digite para filtrar…", query_budget)
    } else {
        truncate_search_query(&safe_query, query_budget)
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(" ", palette.muted),
        Span::styled(
            query_value.clone(),
            if safe_query.is_empty() {
                palette.muted
            } else {
                palette.text
            },
        ),
    ])];
    lines.extend(rows[start..end].iter().map(|row| row.line.clone()));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Comandos ", palette)),
        area,
    );
    if cursor_focused {
        let inner = ratatui::layout::Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let value_width = if safe_query.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(query_value.as_str())
        };
        set_cursor_in_rect(frame, inner, 1usize.saturating_add(value_width), 0);
    }
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

/// Command palette modal width 42 minus its borders.
const PALETTE_INNER_WIDTH: usize = 40;

fn palette_command_line(command: &str, selected_here: bool, palette: &Palette) -> Line<'static> {
    const MODAL_INNER_WIDTH: usize = PALETTE_INNER_WIDTH;
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
        let name = match *group {
            "help" => "ajuda",
            "session" => "sessão",
            "runtime" => "execução",
            "integrations" => "integrações",
            "inspect" => "inspeção",
            other => other,
        };
        lines.push(palette_heading(name, palette));
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
        lines.push(palette_heading("habilidades", palette));
        for (index, command) in ungrouped {
            lines.push(PaletteRow {
                command_index: Some(index),
                line: palette_command_line(command, selected == Some(index), palette),
            });
        }
    }
    lines
}

/// Group heading: a rule to the modal edge sets it apart from its commands.
fn palette_heading(name: &str, palette: &Palette) -> PaletteRow {
    let rule = PALETTE_INNER_WIDTH.saturating_sub(UnicodeWidthStr::width(name) + 1);
    PaletteRow {
        command_index: None,
        line: Line::from(vec![
            Span::styled(name.to_owned(), palette.secondary),
            Span::styled(format!(" {}", "─".repeat(rule)), palette.border),
        ]),
    }
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

/// Recedes everything already painted so a centered modal reads as the only
/// active surface. Foregrounds fade and lose weight; backgrounds (user band,
/// code, diff) keep their shape.
fn dim_backdrop(frame: &mut ratatui::Frame, palette: &Palette) {
    let area = frame.area();
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            if let Some(fg) = palette.backdrop.fg {
                cell.set_fg(fg);
            }
            cell.modifier.remove(Modifier::BOLD);
            cell.modifier.insert(palette.backdrop.add_modifier);
        }
    }
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
mod tests;
