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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use slim_core::runtime::mode_name;

use crate::api::{BlockId, LoginProvider, TodoItemStatus, UiChannels, UiCommand};
use crate::app::{
    ActivityPhase, AppState, EffortOverlay, LoginOverlay, LoginStage, ModelOverlay, ModelRow,
    NotificationPriority, SlashSuggestions, INFO_TOAST_TTL_MS,
};
use crate::block::{
    question_option_marker, wrap_words, Block, BlockKind, BlockLifecycle, DiffRowKind, FoldState,
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
    next_visual_deadline, runtime_clock, wait_for_runtime_signal, WaitOutcome, MOTION_INTERVAL_MS,
};
use crate::theme::{
    detect_capabilities, glyph, mix_rgb, resolve_theme, to_terminal_color, Capabilities,
    ColorDepth, MENU_FOCUS_FLASH_BG, MENU_SELECTION_BG,
};

const MENU_FOCUS_FLASH_MS: u64 = 166;
use crate::view_model::{
    activity_elapsed, activity_label, activity_phase_label, assistant_label, budget_near_limit,
    completed_tool_phrase, display_cwd, format_context, is_trivial_cwd, run_status_label,
    session_rail_projection, tool_effect, tool_target, tool_title, truncate_display_width,
    ToolEffect,
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
        let motion_visible = motion_on_screen(&state, capabilities, &mut render_cache, &regions);
        // The footer's activity row needs one-second semantic redraws for
        // retry countdowns, cancellation labels and elapsed metadata. This
        // remains a status tick (not an animation loop) and is disabled only
        // while a blocking modal owns the frame.
        let status_visible = status_clock_visible(&state, activity_rows(&state, &regions));
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
            &mut render_cache,
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

/// A centered modal that dims the whole frame (`dim_backdrop`). Everything
/// behind it is backdrop, so motion and the status clock may rest meanwhile.
fn blocking_modal_open(state: &AppState) -> bool {
    state.login_overlay.is_some()
        || state.model_overlay.is_some()
        || state.effort_overlay.is_some()
        || state.mcp_overlay.is_some()
        || state.jobs_overlay.is_some()
        || state.job_exit_confirm.is_some()
        || state.session_picker.is_some()
        || state.palette_query.is_some()
}

fn navigation_captured(state: &AppState) -> bool {
    mouse_navigation_captured(state) || state.inspector.active.is_some() || state.todo_focused
}

/// The run's activity is a row of the footer while anything is in flight.
fn activity_in_footer(state: &AppState) -> bool {
    state.working || state.activity.is_some()
}

fn activity_rows(state: &AppState, regions: &crate::layout::LayoutRegions) -> u16 {
    u16::from(activity_in_footer(state) && regions.operational.height > 0)
}

/// The status tick pauses only behind a blocking modal. Popups, search, a
/// pending question, todo focus and inspectors capture input but leave the
/// frame readable, so elapsed times and retry countdowns keep advancing.
fn status_clock_visible(state: &AppState, activity_rows: u16) -> bool {
    (state.jobs_overlay.is_some() && state.running_jobs() > 0)
        || (state.working && state.todo_dock_open && !state.todo_items.is_empty())
        || (!blocking_modal_open(state)
            && (activity_rows > 0 || state.working || state.retry.is_some()))
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
                // Up/Down recall sent prompts when the composer is empty at the
                // live edge (and while a recalled prompt is untouched). Scrolling
                // the transcript stays on PageUp/PageDown, and Up/Down navigate
                // its blocks once the view is pinned.
                if key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Up | KeyCode::Down)
                    && state.history_recall_applies(key.code == KeyCode::Up)
                {
                    return Some(Action::HistoryRecall {
                        older: key.code == KeyCode::Up,
                    });
                }
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
            Some(Action::WheelScroll {
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
    blocking_modal_open(state)
        || state.search.is_some()
        || state.slash_suggestions.is_some()
        || state.mention_suggestions.is_some()
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
    state.inspector.active.is_some() && !mouse_navigation_captured(state)
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

/// Whether the transcript certainly needs a scrollbar: a lower bound of the
/// rows it presents already exceeds the viewport. Blocks folded under a
/// collapsed work row present nothing of their own, so the row stands for all
/// of them.
fn scrollbar_is_guaranteed(blocks: &[Block], viewport: u64) -> bool {
    if viewport == 0 {
        return false;
    }
    let vp = viewport as usize;
    let mut min_rows = 0usize;
    let mut index = 0;
    while index < blocks.len() {
        let block = &blocks[index];
        let base = match block.kind() {
            BlockKind::User(_) => 2,
            _ => 1,
        };
        min_rows = min_rows.saturating_add(
            base + usize::from(block.turn_boundary_before())
                + crate::block::leading_rows(blocks, index),
        );
        if min_rows > vp {
            return true;
        }
        index = if matches!(block.kind(), BlockKind::Work(_)) && block.fold != FoldState::Expanded {
            crate::work::span_end(blocks, index)
        } else {
            index + 1
        };
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

/// A light that rises from a resting shade to a peak. Truecolor terminals get
/// every shade in between, so the light slides; other depths step through
/// four shades, the upper ones carried by weight where shades blur together.
#[derive(Clone, Copy)]
struct Ramp {
    steps: [Style; 4],
    /// Resting and peak RGB, only where any shade between them can be shown.
    ends: Option<[Rgb; 2]>,
}

type Rgb = (u8, u8, u8);

impl Ramp {
    /// The style at `strength`, from 0.0 (resting) to 1.0 (peak).
    fn at(&self, strength: f32) -> Style {
        let strength = strength.clamp(0.0, 1.0);
        match self.ends {
            Some([from, peak]) => {
                let (r, g, b) = mix_rgb(from, peak, strength);
                Style::default().fg(Color::Rgb(r, g, b))
            }
            None => self.steps[(strength * 3.0).round() as usize],
        }
    }
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
    user: Style,
    assistant: Style,
    assistant_bold: Style,
    accent: Style,
    accent_bold: Style,
    thinking: Style,
    /// Thought rows and the Plan mode (violet); `thinking` stays the H3 tone.
    reasoning: Style,
    /// The older of the two preview rows: one step back toward the surface.
    reasoning_dim: Style,
    /// The thinking sweep, from a shade below the resting violet up to its
    /// peak.
    shimmer: Ramp,
    /// The glow at the edge of arriving text, up to the same peak: anything
    /// above rest is brighter than the text it lights.
    glow: Ramp,
    /// The sweep across the agent name while it has not answered yet: the
    /// identity green, from dim to bright.
    presence: Ramp,
    /// The glow at the edge of the answer being written: from the text color
    /// up to the identity green, so fresh words read as wet ink.
    arrival: Ramp,
    /// The last output line of a running command, a step quieter than the
    /// clock it follows.
    tail: Style,
    heading: Style,
    h1: Style,
    link: Style,
    quote: Style,
    tool: Style,
    inline_code: Style,
    code_block: Style,
    code_rail: Style,
    code_label: Style,
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
    /// The backdrop while its modal is still arriving: lighter than the
    /// resting one, settling toward it. Only used at depths with shades.
    backdrop_entrance: [Style; BACKDROP_ENTRANCE_STEPS as usize],
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
        // Four steps from `from` up to the peak of the reasoning hue, for the
        // sweep and the glow. Where shades cannot be told apart, weight
        // carries the upper steps.
        let violet_peak = mix_rgb(theme.foreground, theme.reasoning_accent, 0.25);
        let green_peak = mix_rgb(theme.foreground, theme.assistant_accent, 0.30);
        let truecolor = capabilities.color_depth == ColorDepth::TrueColor;
        let scale = |from: (u8, u8, u8), peak: (u8, u8, u8)| Ramp {
            steps: std::array::from_fn(|level| {
                let style = base.fg(color(mix_rgb(from, peak, level as f32 / 3.0)));
                if level >= 2
                    && matches!(
                        capabilities.color_depth,
                        ColorDepth::Ansi16 | ColorDepth::None
                    )
                {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                }
            }),
            ends: truecolor.then_some([from, peak]),
        };
        let reasoning_dim_rgb = mix_rgb(theme.reasoning_accent, theme.background, 0.25);
        Palette {
            background: base.bg(color(theme.background)),
            surface: base.bg(color(theme.surface)),
            surface_alt: base.bg(color(theme.surface_alt)),
            composer_bg: base.bg(color(theme.composer_bg)),
            user_prompt_bg: base.bg(color(theme.user_prompt_bg)),
            text: base.fg(color(theme.foreground)),
            muted: base.fg(color(theme.muted)),
            secondary: base.fg(color(theme.secondary_text)),
            user: base.fg(color(theme.user_accent)),
            assistant: base.fg(color(theme.assistant_accent)),
            assistant_bold: base
                .fg(color(theme.assistant_accent))
                .add_modifier(Modifier::BOLD),
            accent: base.fg(color(theme.accent)),
            accent_bold: base.fg(color(theme.accent)).add_modifier(Modifier::BOLD),
            thinking: base.fg(color(theme.thinking_accent)),
            reasoning: base.fg(color(theme.reasoning_accent)),
            reasoning_dim: match capabilities.color_depth {
                ColorDepth::TrueColor | ColorDepth::Ansi256 => base.fg(color(reasoning_dim_rgb)),
                // Quantized depths cannot hold an in-between shade.
                ColorDepth::Ansi16 | ColorDepth::None => base
                    .fg(color(theme.reasoning_accent))
                    .add_modifier(Modifier::DIM),
            },
            shimmer: scale(
                mix_rgb(theme.reasoning_accent, theme.background, 0.15),
                violet_peak,
            ),
            // Steps start at the resting violet, and only those above it
            // are painted; the continuous light rises from the dimmer shade
            // of the words it lights, so it never jumps on.
            glow: Ramp {
                ends: truecolor.then_some([reasoning_dim_rgb, violet_peak]),
                ..scale(theme.reasoning_accent, violet_peak)
            },
            presence: scale(
                mix_rgb(theme.assistant_accent, theme.background, 0.35),
                green_peak,
            ),
            arrival: scale(
                theme.foreground,
                mix_rgb(theme.assistant_accent, theme.foreground, 0.2),
            ),
            tail: match capabilities.color_depth {
                ColorDepth::TrueColor | ColorDepth::Ansi256 => {
                    base.fg(color(mix_rgb(theme.muted, theme.background, 0.30)))
                }
                // Quantized depths cannot hold an in-between shade.
                ColorDepth::Ansi16 | ColorDepth::None => {
                    base.fg(color(theme.muted)).add_modifier(Modifier::DIM)
                }
            },
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
            // Without color the tag is told from a line of code by weight.
            code_label: if capabilities.color_depth == ColorDepth::None {
                base.fg(color(theme.code_rail)).add_modifier(Modifier::DIM)
            } else {
                base.fg(color(theme.code_rail))
            },
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
            backdrop_entrance: BACKDROP_ENTRANCE_LIFT
                .map(|lift| base.fg(color(mix_rgb(theme.backdrop, theme.foreground, lift)))),
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
    let modal_open = blocking_modal_open(state);
    let modal_opened_ms = cache.note_modal_frame(modal_open, state.clock.elapsed_ms);
    cache.selection_regions = [None, None];
    cache.selection_scroll_anchor = None;
    let mut palette = Palette::of(capabilities);
    if matches!(
        capabilities.color_depth,
        ColorDepth::TrueColor | ColorDepth::Ansi256
    ) && !capabilities.reduced_motion
        && state
            .menu_focus_at_ms
            .is_some_and(|at| state.clock.elapsed_ms.saturating_sub(at) < MENU_FOCUS_FLASH_MS)
        && (state.model_overlay.is_some() || state.palette_query.is_some())
    {
        palette.menu_selected = Style::default().bg(to_terminal_color(
            capabilities.color_depth,
            MENU_FOCUS_FLASH_BG,
        ));
    }
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
            capabilities,
        );
    }
    // The rail may suppress its spinner when it is hidden, idle, or otherwise
    // not useful.  Keep that decision separate from the user's motion
    // preference: completed blocks still receive the short transition
    // emphasis whenever reduced motion is not requested.
    let rail_motion_capabilities = Capabilities {
        reduced_motion: capabilities.reduced_motion
            || !motion_on_screen(state, capabilities, cache, &regions),
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
    let painted_phase = render_scrollback(
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
    if let Some(suggestions) = &state.mention_suggestions {
        render_mention_popup(frame, composer_area, suggestions, state, &palette);
    }
    render_interaction_overlay(frame, composer_area, state, &palette, capabilities);
    let activity = (activity_rows(state, &regions) > 0).then_some(FooterActivity {
        capabilities: Capabilities {
            reduced_motion: rail_motion_capabilities.reduced_motion
                || painted_phase.thinking_header,
            ..capabilities
        },
        painted: painted_phase,
        motion_allowed: !capabilities.reduced_motion,
    });
    render_operational_bar(
        frame,
        chrome_area(to_ratatui(regions.operational), band),
        state,
        &palette,
        capabilities,
        activity,
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
    if modal_open {
        // The backdrop settles in; the modal drawn after it is never delayed.
        let entrance = modal_opened_ms
            .and_then(|at| backdrop_entrance_phase(at, state.clock.elapsed_ms, capabilities));
        dim_backdrop(
            frame,
            entrance.map_or(palette.backdrop, |step| palette.backdrop_entrance[step]),
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
            state.auth_provider,
            state.effort,
            &state.open_code_models,
            &state.cline_pass_models,
            &state.command_code_models,
            &state.zen_models,
            &rows,
            &palette,
            capabilities,
            cursor_target == Some(InputCursorTarget::ModelFilter),
        );
    }
    if state.jobs_overlay.is_some() || state.job_exit_confirm.is_some() {
        render_jobs_overlay(frame, state, &palette);
    }
    if let Some(overlay) = &state.mcp_overlay {
        mcp_view::render(
            frame,
            overlay,
            &state.mcp_servers,
            &palette,
            cursor_target == Some(InputCursorTarget::McpSignIn),
        );
    }
    if let Some(picker) = &state.session_picker {
        render_session_picker(
            frame,
            picker,
            &palette,
            cursor_target == Some(InputCursorTarget::SessionFilter),
        );
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
    plan_with_session_rail_and_composer(width, height, todo_rows, show_session, composer_lines)
}

fn welcome_visible(state: &AppState) -> bool {
    state.blocks().is_empty() && state.visible_notifications().next().is_none() && !state.working
}

fn toast_row_count(state: &AppState, area_height: u16) -> u16 {
    if area_height == 0
        || state.mcp_overlay.is_some()
        || state.jobs_overlay.is_some()
        || state.job_exit_confirm.is_some()
    {
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
                if let Some(step_ms) = next_settle_boundary_ms(notification.created_ms, now_ms) {
                    return step_ms;
                }
            }
            expiry_ms
        })
        .min()
}

/// The next instant a timed visual changes without any motion tick: the next
/// step of a settle emphasis or of the modal backdrop entrance, or the end of
/// the menu focus flash. Block timestamps come from the per-revision memo, so
/// the cost follows the blocks still inside a window, not the session size.
fn next_transition_visual_deadline_ms(
    state: &AppState,
    now_ms: u64,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) -> Option<u64> {
    if capabilities.reduced_motion {
        return None;
    }
    let mut deadline: Option<u64> = None;
    let mut consider = |at_ms: Option<u64>| {
        let Some(next) = at_ms.and_then(|at_ms| next_settle_boundary_ms(at_ms, now_ms)) else {
            return;
        };
        deadline = Some(deadline.map_or(next, |current| current.min(next)));
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
    for &stamp in cache.transition_stamps(
        state.blocks(),
        state.revisions.content,
        now_ms,
        INFO_TOAST_HIGHLIGHT_MS,
    ) {
        consider(Some(stamp));
    }
    if let Some(next) = cache
        .modal_opened_ms()
        .filter(|_| backdrop_entrance_enabled(capabilities) && blocking_modal_open(state))
        .and_then(|opened| {
            next_step_boundary_ms(
                opened,
                now_ms,
                BACKDROP_ENTRANCE_STEP_MS,
                BACKDROP_ENTRANCE_MS,
            )
        })
    {
        deadline = Some(deadline.map_or(next, |current| current.min(next)));
    }
    if matches!(
        capabilities.color_depth,
        ColorDepth::TrueColor | ColorDepth::Ansi256
    ) && (state.model_overlay.is_some() || state.palette_query.is_some())
    {
        if let Some(at_ms) = state.menu_focus_at_ms {
            let expires = at_ms.saturating_add(MENU_FOCUS_FLASH_MS);
            if expires > now_ms {
                deadline = Some(deadline.map_or(expires, |current: u64| current.min(expires)));
            }
        }
    }
    deadline
}

/// Whether the run is animating at all. Only a blocking modal (dimmed
/// backdrop) pauses it; popups, search, todo focus, a pending question and
/// inspectors leave the animated surfaces readable, so they keep moving.
fn motion_needed(state: &AppState, capabilities: Capabilities, cache: &mut WrapCache) -> bool {
    if capabilities.reduced_motion
        || welcome_visible(state)
        || blocking_modal_open(state)
        || state.cancellation.is_some()
        || state.retry.is_some()
    {
        return false;
    }
    state.working || cache.has_streaming_block(state.blocks(), state.revisions.content)
}

/// Motion is worth a repaint only when something animated is on screen: the
/// footer's activity spinner, or the transcript's thinking header, presence
/// sweep and streaming caret. Those exist only while the run is working, and
/// a floating inspector covers the transcript, so it leaves just the footer.
fn motion_on_screen(
    state: &AppState,
    capabilities: Capabilities,
    cache: &mut WrapCache,
    regions: &crate::layout::LayoutRegions,
) -> bool {
    if !motion_needed(state, capabilities, cache) {
        return false;
    }
    activity_rows(state, regions) > 0
        || (state.working
            && (state.inspector.active.is_none()
                || workspace_regions(state, to_ratatui(regions.scrollback))
                    .inspector
                    .is_some()))
}

fn render_session_rail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    capabilities: Capabilities,
) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        ratatui::widgets::Block::default().style(palette.surface),
        area,
    );
    let projection = session_rail_projection(state, area.width as usize, state.working);
    let mut spans = session_rail_spans(
        &projection.identity,
        projection.title.clone(),
        projection.folder.clone(),
        palette,
        capabilities,
    );
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

/// The rail as spans: the brand (and a session title's lead-in) in the
/// secondary tone, the title and the project folder in the text tone, and the
/// rest (separators, the parent path) quiet. With `NO_COLOR` the tones
/// collapse into one, so the project folder is told apart by weight alone.
fn session_rail_spans(
    identity: &str,
    title: Option<std::ops::Range<usize>>,
    folder: Option<std::ops::Range<usize>>,
    palette: &Palette,
    capabilities: Capabilities,
) -> Vec<Span<'static>> {
    let on_boundary = |range: &std::ops::Range<usize>| {
        identity.is_char_boundary(range.start) && identity.is_char_boundary(range.end)
    };
    let title = title.filter(on_boundary);
    let folder = folder.filter(on_boundary);
    let folder_style = if capabilities.color_depth == ColorDepth::None {
        palette.text.add_modifier(Modifier::BOLD)
    } else {
        palette.text
    };
    // Where the secondary lead ends: before the title, or after `SLIM`.
    let lead_end = match &title {
        Some(range) => range.start,
        None if identity.starts_with("SLIM") => 4,
        None => 0,
    };
    let mut cuts = vec![0, lead_end.min(identity.len()), identity.len()];
    for range in title.iter().chain(folder.iter()) {
        cuts.extend([range.start, range.end]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter(|pair| pair[0] < pair[1])
        .map(|pair| {
            let (start, end) = (pair[0], pair[1]);
            let inside = |range: &Option<std::ops::Range<usize>>| {
                range
                    .as_ref()
                    .is_some_and(|range| range.start <= start && end <= range.end)
            };
            let style = if inside(&title) {
                palette.text
            } else if inside(&folder) {
                folder_style
            } else if end <= lead_end {
                palette.secondary
            } else {
                palette.muted
            };
            Span::styled(identity[start..end].to_owned(), style)
        })
        .collect()
}

/// Which live rows of the current phase the transcript painted this frame.
/// The footer's activity row drops the phase label it would only repeat.
#[derive(Clone, Copy, Debug, Default)]
struct PaintedPhase {
    /// The streaming `Pensando` header.
    thinking_header: bool,
    /// The row of a running tool call.
    live_tool: bool,
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
) -> PaintedPhase {
    if welcome_visible(state) {
        frame.render_widget(
            ratatui::widgets::Block::default().style(palette.surface),
            area,
        );
        render_welcome(frame, area, state, palette, capabilities);
        return PaintedPhase::default();
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
    let mut live_tool_visible = false;
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
        let response_header = crate::block::response_header(state.blocks(), block_index);
        let selected_member = selected.as_ref().filter(|id| {
            matches!(block.kind(), BlockKind::Tool(_))
                && members.len() > 1
                && block.group_expanded
                && (*id != &block.id
                    || matches!(&state.scroll.mode, crate::app::FollowMode::Pinned(anchor)
                        if anchor.block_id == block.id && anchor.row_offset > 0))
        });
        let is_selected = selected.as_ref() == Some(&block.id) && selected_member.is_none();
        let show_enter_hint = is_selected
            || (state.scroll.is_live_edge()
                && metrics
                    .last_visible_foldable_anchor
                    .as_ref()
                    .is_some_and(|anchor| anchor.block_id == block.id)
                && (matches!(block.kind(), BlockKind::QueuedUser(_))
                    || live_collapsed_group
                        .as_ref()
                        .is_some_and(|leader| leader == &block.id)));
        let transition_gap = crate::block::transition_gap(state.blocks(), block_index);
        // Row of the block's own first line inside the entry: below whatever
        // leads it. For a Thinking block that is its header.
        let lead_rows = usize::from(block.turn_boundary_before())
            + response_header.map_or(0, crate::block::ResponseHeader::rows)
            + usize::from(transition_gap);
        let thinking_header_index =
            if members.len() == 1 && matches!(block.kind(), BlockKind::Thinking(_)) {
                lead_rows
            } else {
                0
            };
        let live_tool_candidate = members.len() == 1
            && matches!(block.kind(), BlockKind::Tool(tool) if !tool.historical)
            && matches!(
                block.lifecycle,
                BlockLifecycle::Pending | BlockLifecycle::Streaming
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
                && thinking_header_candidate,
            caret: CaretPhase::of(state, capabilities),
        };
        // Key on everything the produced lines depend on: each member's
        // generation/lifecycle/fold/timing folded together, selection and the
        // enter hint on the leader, and the wrap width.  The animated Thinking
        // glyph is patched into the visible header after these static lines
        // are painted.  Only a member that is still running contributes the
        // current second (its clock), and only the message being written
        // contributes its caret phase, so ordinary clock ticks never
        // invalidate the rest of the transcript cache.
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
            if matches!(member.kind(), BlockKind::Thinking(_) | BlockKind::Tool(_))
                && member.lifecycle == BlockLifecycle::Streaming
            {
                member_state = member_state
                    .wrapping_mul(31)
                    .wrapping_add(state.clock.elapsed_ms / 1_000);
            }
            if matches!(member.kind(), BlockKind::Assistant(_))
                && member.lifecycle == BlockLifecycle::Streaming
            {
                member_state = member_state.wrapping_mul(31).wrapping_add(ctx.caret.tag());
            }
            if matches!(member.kind(), BlockKind::Tool(_)) {
                member_state =
                    member_state
                        .wrapping_mul(31)
                        .wrapping_add(u64::from(tool_settle_level(
                            member,
                            state.clock.elapsed_ms,
                            capabilities,
                        )));
            }
            // Transition emphasis is a short-lived visual state. Key the
            // memo on its settle step (zero outside the window) so the style
            // advances and expires without adding the full frame clock to
            // every block cache entry.
            let settle = settle_phase(
                member.ended_ms.or(member.started_ms),
                state.clock.elapsed_ms,
                capabilities,
            );
            member_state = member_state
                .wrapping_mul(31)
                .wrapping_add(settle.map_or(0, |phase| u64::from(phase) + 1));
        }
        if let Some(header) = response_header {
            member_state = member_state
                .wrapping_mul(31)
                .wrapping_add(header.cache_tag());
        }
        member_state = member_state
            .wrapping_mul(31)
            .wrapping_add(selected_member.map_or(0, |id| {
                members
                    .iter()
                    .position(|member| &member.id == id)
                    .map_or(0, |index| index as u64 + 1)
            }));
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
                caret: ctx.caret,
            };
            let mut built = if matches!(block.kind(), BlockKind::QueuedUser(_)) {
                grouped_queued_user_lines(block, members, show_enter_hint, &static_ctx)
            } else if matches!(block.kind(), BlockKind::Work(_)) {
                work_lines(block, members, &static_ctx, cache)
            } else if members.len() > 1 {
                if crate::block::is_failed_tool(block)
                    && !members.iter().any(crate::block::is_complete_tool)
                {
                    grouped_failed_tool_lines(
                        block,
                        members,
                        show_enter_hint,
                        selected_member,
                        &static_ctx,
                        cache,
                    )
                } else if crate::block::is_complete_thinking(block) {
                    grouped_thinking_lines(block, members, &static_ctx, cache)
                } else {
                    grouped_tool_lines(
                        block,
                        members,
                        show_enter_hint,
                        selected_member,
                        &static_ctx,
                        cache,
                    )
                }
            } else {
                safe_block_lines(block, &static_ctx, cache)
            };
            if transition_gap {
                built.insert(0, Line::default());
            }
            if let Some(header) = response_header {
                let mut lines = response_header_lines(header, &static_ctx);
                lines.append(&mut built);
                built = lines;
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
        if live_tool_candidate && skip <= lead_rows && skip.saturating_add(written) > lead_rows {
            live_tool_visible = true;
        }
        if ctx.animate_thinking && header_written {
            let glyph_x = text_area.x.saturating_add(2);
            let glyph_y =
                block_start_y.saturating_add((thinking_header_index.saturating_sub(skip)) as u16);
            if glyph_x < text_area.right() && glyph_y < text_area.bottom() {
                buf[(glyph_x, glyph_y)].set_char(spinner_glyph(ctx.frame, ctx.capabilities));
                // The label sits after the marker and its space; the sweep
                // touches the label only, never the clock beside it.
                for index in 0..THINKING_LABEL_CELLS {
                    let label_x = glyph_x.saturating_add(2 + index as u16);
                    if label_x < text_area.right() {
                        buf[(label_x, glyph_y)].set_style(palette.shimmer.at(sweep_strength(
                            index,
                            THINKING_LABEL_CELLS,
                            ctx.now_ms,
                        )));
                    }
                }
            }
        }
        // A prompt still waiting for its first agent output sweeps the name in
        // its pending header, in the last row of the block. `● Slim`: the name
        // starts in the fourth column.
        if spinner_motion_enabled
            && block.awaiting_agent()
            && written > 0
            && skip + written == block_lines.len()
        {
            for index in 0..AGENT_NAME_CELLS {
                let x = text_area.x.saturating_add(4 + index as u16);
                if x < text_area.right() {
                    buf[(x, y - 1)].set_style(palette.presence.at(sweep_strength(
                        index,
                        AGENT_NAME_CELLS,
                        ctx.now_ms,
                    )));
                }
            }
        }
        // Words that just arrived light the edge where the text is being
        // written, then settle. The preview keeps no caret, so this is what
        // shows the thought is still pouring in when it arrives in bursts.
        // A collapsed thought showing a headline has no such edge: its row
        // holds a settled sentence, not the words arriving. The answer has a
        // caret, and the glow adds to it.
        if spinner_motion_enabled
            && members.len() == 1
            && block.lifecycle == BlockLifecycle::Streaming
            && written > 0
            && skip + written == block_lines.len()
        {
            match block.kind() {
                BlockKind::Thinking(text) => {
                    if let Some(freshness) = glow_freshness(state).filter(|_| {
                        !text.trim().is_empty()
                            && (block.fold == FoldState::Expanded
                                || !crate::thought::has_headline(text))
                    }) {
                        apply_frontier_glow(buf, text_area, y - 1, &palette.glow, freshness, false);
                    }
                }
                // The answer being written lights its newest words the same
                // way; lines already settled above it keep their colors.
                BlockKind::Assistant(text) => {
                    if let Some(freshness) =
                        glow_freshness(state).filter(|_| !text.trim().is_empty())
                    {
                        apply_frontier_glow(
                            buf,
                            text_area,
                            y - 1,
                            &palette.arrival,
                            freshness,
                            matches!(ctx.caret, CaretPhase::On | CaretPhase::Stalled),
                        );
                    }
                }
                _ => {}
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
                let style = settled(
                    base_style,
                    notice_highlight_enabled
                        .then(|| {
                            settle_phase_since(
                                state.clock.elapsed_ms.saturating_sub(notice.created_ms),
                            )
                        })
                        .flatten(),
                    capabilities.color_depth,
                );
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
    PaintedPhase {
        thinking_header: streaming_header_visible,
        live_tool: live_tool_visible,
    }
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
        if state.todo_focused {
            palette.text
        } else {
            palette.secondary
        },
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
        let row = Line::from(vec![
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
        ]);
        rows.push(if selected {
            row.style(palette.menu_selected)
        } else {
            row
        });
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

/// Inputs of the footer's activity row, decided where the transcript painted.
#[derive(Clone, Copy, Debug)]
struct FooterActivity {
    capabilities: Capabilities,
    painted: PaintedPhase,
    // The user's motion preference. `capabilities.reduced_motion` also turns
    // on whenever the motion clock is idle (a retry wait, a modal), which is
    // not a request for stillness.
    motion_allowed: bool,
}

/// The run's phase as the footer reads it: indicator, phase (or the run total
/// when the transcript already shows the phase), elapsed, staleness and
/// near-limit budgets, fitted to `width` cells.
fn activity_spans(
    state: &AppState,
    width: u16,
    palette: &Palette,
    activity: FooterActivity,
) -> Vec<Span<'static>> {
    let FooterActivity {
        capabilities,
        painted,
        motion_allowed,
    } = activity;
    let elapsed = activity_elapsed(state);
    let cancellation = state.cancellation.as_ref();
    let retry = state.retry.as_ref();
    // The transcript shows the phase, the rail shows the run: while the live
    // row of the current phase is on screen the rail carries the run total
    // instead of repeating the phase label. A single running call is a
    // restatement of its row; several are summarized only by the label.
    let phase_in_transcript = match state.activity.as_ref().map(|activity| &activity.phase) {
        Some(ActivityPhase::Thinking) => painted.thinking_header,
        _ => painted.live_tool && tool_activity_is_active(state) && single_live_tool(state),
    };
    let run_total = state
        .run_started_ms
        .map(|started| state.clock.elapsed_ms.saturating_sub(started) / 1_000)
        .filter(|_| phase_in_transcript && cancellation.is_none() && retry.is_none());
    let semantic_style = if cancellation.is_some() || retry.is_some() {
        palette.warning
    } else {
        palette.text
    };
    let semantic_style = settled(
        semantic_style,
        freshest_phase([
            settle_phase(
                cancellation.map(|value| value.requested_ms),
                state.clock.elapsed_ms,
                capabilities,
            ),
            settle_phase(
                retry.map(|value| value.scheduled_ms),
                state.clock.elapsed_ms,
                capabilities,
            ),
            settle_phase(
                state.activity.as_ref().map(|value| value.started_ms),
                state.clock.elapsed_ms,
                capabilities,
            ),
        ]),
        capabilities.color_depth,
    );
    // Waiting on a retry or a cancellation is not a busy loop, so the
    // motion clock stays off. The label already says which wait it is and
    // the color marks it; the marker advances once a second with the
    // status tick, so the wait visibly is not stuck.
    let indicator = if cancellation.is_some() || retry.is_some() {
        if motion_allowed {
            slow_spinner_glyph(state.clock.elapsed_ms, capabilities)
        } else {
            glyph(
                capabilities,
                '\u{21bb}',
                if retry.is_some() { '~' } else { '!' },
            )
        }
    } else {
        spinner_glyph(state.clock.frame, capabilities)
    };
    let mut spans = if let Some(total) = run_total {
        // The indicator keeps the row alive without competing with the
        // phase row above it.
        vec![
            Span::styled(format!("{indicator} "), palette.muted),
            Span::styled(format!("execução {total}s"), palette.muted),
        ]
    } else {
        // While the model thinks and its header is out of view, the rail
        // carries the same sweep the header would.
        let sweeping = !capabilities.reduced_motion
            && cancellation.is_none()
            && retry.is_none()
            && matches!(
                state.activity.as_ref().map(|activity| &activity.phase),
                Some(ActivityPhase::Thinking)
            );
        let mut spans = vec![Span::styled(format!("{indicator} "), semantic_style)];
        if sweeping {
            spans.extend(shimmer_spans(
                &activity_label(state),
                state.clock.elapsed_ms,
                palette,
            ));
        } else {
            spans.push(Span::styled(activity_label(state), semantic_style));
        }
        if elapsed > 0 {
            spans.push(Span::styled(format!(" · {elapsed}s"), palette.muted));
        }
        spans
    };
    if let Some(age) = last_useful_update_age_ms(state) {
        if age >= 5_000 && cancellation.is_none() && retry.is_none() {
            let stale_label = if tool_activity_is_active(state) {
                "sem progresso"
            } else {
                "sem novo conteúdo"
            };
            let stale = format!(" · {stale_label} há {}", duration_label(age));
            let occupied: usize = spans.iter().map(|span| span.width()).sum();
            if occupied + UnicodeWidthStr::width(stale.as_str()) <= width as usize {
                spans.push(Span::styled(stale, palette.muted));
            }
        }
    }
    if width >= 72 {
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
            if occupied + UnicodeWidthStr::width(counter.as_str()) <= width as usize {
                spans.push(Span::styled(counter, palette.warning));
            }
        }
    }
    spans
}

/// Spans cut to `width` cells, closing with `…` when something was dropped.
fn truncate_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= width {
        return spans;
    }
    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    let mut kept = Vec::new();
    let mut last_style = Style::default();
    for span in spans {
        if used >= budget {
            break;
        }
        last_style = span.style;
        let room = budget - used;
        if span.width() <= room {
            used += span.width();
            kept.push(span);
        } else {
            let cut = truncate_cells(&span.content, room);
            used += UnicodeWidthStr::width(cut.as_str());
            kept.push(Span::styled(cut, span.style));
        }
    }
    if width > 0 {
        kept.push(Span::styled("…", last_style));
    }
    kept
}

fn last_useful_update_age_ms(state: &AppState) -> Option<u64> {
    let latest = [state.last_provider_content_ms, state.last_tool_progress_ms]
        .into_iter()
        .flatten()
        .max()?;
    Some(state.clock.elapsed_ms.saturating_sub(latest))
}

/// Exactly one tool call is in flight, so its transcript row says all that
/// the rail's phase label would.
fn single_live_tool(state: &AppState) -> bool {
    state
        .blocks()
        .iter()
        .filter(|block| {
            matches!(
                block.lifecycle,
                BlockLifecycle::Pending | BlockLifecycle::Streaming
            ) && matches!(block.kind(), BlockKind::Tool(tool) if !tool.historical)
        })
        .take(2)
        .count()
        == 1
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
        let group =
            crate::block::consecutive_presented_tool_span(blocks, index, now_ms, reduced_motion)
                .filter(|(start, end)| crate::block::is_tool_group(blocks, *start, *end));
        let span = if let Some((start, _)) = group {
            Some((start, true))
        } else if crate::block::is_failed_tool(block) {
            crate::block::consecutive_identical_failed_tool_span(blocks, index)
                .map(|(start, end)| (start, end.saturating_sub(start) > 1))
        } else {
            None
        };
        let Some((start, grouped)) = span else {
            continue;
        };
        if grouped && !blocks[start].group_expanded {
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
    selected_member: Option<&BlockId>,
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
    // An open group points down, like an open thought; closed it keeps the
    // check that says its calls settled.
    let complete = if leader.group_expanded {
        glyph(ctx.capabilities, '\u{25be}', 'v')
    } else {
        glyph(ctx.capabilities, '\u{2713}', '+')
    };
    let names: Vec<&str> = members
        .iter()
        .filter_map(|block| match block.kind() {
            // Failed calls count too: the work they took is part of the
            // group, and their own rows below say they failed.
            BlockKind::Tool(tool) if crate::block::is_settled_tool(block) => {
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
    let chrome = UnicodeWidthStr::width(marker)
        + UnicodeWidthStr::width(" ")
        + UnicodeWidthStr::width(complete.to_string().as_str());
    let named = (tool_count <= 2)
        .then(|| {
            named_group_detail(
                members,
                &duration,
                (ctx.width as usize).saturating_sub(chrome),
            )
        })
        .flatten();
    let detail = if let Some(named) = named {
        named
    } else if chrome + UnicodeWidthStr::width(detailed.as_str()) <= ctx.width as usize {
        detailed
    } else {
        compact
    };
    let hint = "Enter detalhes";
    let marker_width = UnicodeWidthStr::width(marker);
    let glyph_text = format!("{complete} ");
    let completion_phase = freshest_phase(
        members
            .iter()
            .filter(|member| matches!(member.lifecycle, BlockLifecycle::Complete))
            .map(|member| settle_phase(member.ended_ms, ctx.now_ms, ctx.capabilities)),
    );
    // The row takes the weight of the most consequential call it folds.
    let strongest = members
        .iter()
        .filter(|block| crate::block::is_complete_tool(block))
        .filter_map(|block| match block.kind() {
            BlockKind::Tool(tool) => Some(tool_effect(&tool.name)),
            _ => None,
        })
        .max()
        .unwrap_or(ToolEffect::Observes);
    let completion_style = if completion_phase.is_some() {
        settled(
            ctx.palette.success,
            completion_phase,
            ctx.capabilities.color_depth,
        )
    } else {
        settled_marker_style(strongest, ctx.palette)
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
            if completion_phase.is_some() {
                settled(
                    ctx.palette.secondary,
                    completion_phase,
                    ctx.capabilities.color_depth,
                )
            } else {
                settled_verb_style(strongest, ctx.palette)
            },
        ),
    ];
    if !leader.group_expanded
        && show_enter_hint
        && marker_width + header_width + 1 + hint_width <= content_width
    {
        let gap = content_width.saturating_sub(marker_width + header_width + hint_width);
        header_spans.push(Span::raw(" ".repeat(gap)));
        header_spans.push(Span::styled(hint.to_owned(), ctx.palette.muted));
    }
    let mut lines = vec![Line::from(header_spans)];
    if leader.group_expanded {
        lines.extend(grouped_tool_member_lines(
            members,
            selected_member,
            ctx,
            cache,
        ));
    } else {
        lines.extend(group_failure_lines(members, selected_member, ctx, cache));
    }
    lines
}

/// What a small group did, in its own words: the short label of each call
/// (`Editou src/parser.rs · $ cargo test parser`) so consecutive groups can be
/// told apart. A long label is shortened with the usual ellipsis down to a
/// readable minimum; when the labels still do not fit `room` cells the group
/// reads as counts instead.
fn named_group_detail(members: &[Block], duration: &str, room: usize) -> Option<String> {
    const MIN_LABEL_CELLS: usize = 14;
    let mut labels: Vec<String> = members
        .iter()
        .filter(|block| crate::block::is_settled_tool(block))
        .map(tool_short_label)
        .collect::<Option<_>>()?;
    let separators = UnicodeWidthStr::width(" · ") * labels.len().saturating_sub(1);
    let fixed = separators + UnicodeWidthStr::width(duration);
    let total = |labels: &[String]| -> usize {
        labels
            .iter()
            .map(|label| UnicodeWidthStr::width(label.as_str()))
            .sum::<usize>()
            + fixed
    };
    // The longest label gives up what it can (down to the floor), then the
    // next one, until everything fits or every label is at its floor.
    while total(&labels) > room {
        let excess = total(&labels) - room;
        let longest = labels
            .iter()
            .enumerate()
            .filter(|(_, label)| UnicodeWidthStr::width(label.as_str()) > MIN_LABEL_CELLS)
            .max_by_key(|(_, label)| UnicodeWidthStr::width(label.as_str()))
            .map(|(index, _)| index)?;
        let current = UnicodeWidthStr::width(labels[longest].as_str());
        let target = current.saturating_sub(excess).max(MIN_LABEL_CELLS);
        labels[longest] = truncate_cells(&labels[longest], target);
    }
    Some(format!("{}{duration}", labels.join(" · ")))
}

/// One call as its collapsed row names it, without status or timing: the verb
/// and target (`Leu src/parser.rs`), or `$ command` for a shell call. A call
/// with no target says nothing a count does not, so it has no label.
fn tool_short_label(block: &Block) -> Option<String> {
    let BlockKind::Tool(tool) = block.kind() else {
        return None;
    };
    let name = sanitize_terminal_text(&tool.name);
    let args = sanitize_terminal_text(&tool.arguments_summary);
    let target = first_tool_segment(&args).map(|segment| tool_target(&segment))?;
    if is_command_target(&target) {
        return Some(target);
    }
    Some(format!("{} {target}", tool_title(&name, block.lifecycle)))
}

/// Rows a collapsed group keeps for its failures, one step in from the
/// header: the failed call as its own row reads it with `×N` for identical
/// repeats, or a short `depois passou` row when the same call later succeeded.
fn group_failure_lines(
    members: &[Block],
    selected_member: Option<&BlockId>,
    ctx: &BlockRender<'_>,
    cache: &mut WrapCache,
) -> Vec<Line<'static>> {
    crate::block::group_failure_rows(members)
        .into_iter()
        .map(|row| {
            let member = &members[row.member];
            if row.recovered {
                return recovered_failure_line(member, row.repeats, selected_member, ctx);
            }
            let suffix = if row.repeats > 1 {
                format!(" · ×{}", row.repeats)
            } else {
                String::new()
            };
            let width = ctx
                .width
                .saturating_sub(2)
                .saturating_sub(UnicodeWidthStr::width(suffix.as_str()) as u16)
                .max(1);
            let mut line = tool_member_lines(
                member,
                selected_member == Some(&member.id),
                ctx.palette,
                ctx.capabilities,
                width,
                ctx.frame,
                ctx.now_ms,
                false,
                cache,
            )
            .into_iter()
            .next()
            .unwrap_or_default();
            let mut spans = vec![Span::raw("  ")];
            spans.append(&mut line.spans);
            if !suffix.is_empty() {
                spans.push(Span::styled(suffix, ctx.palette.secondary));
            }
            Line::from(spans)
        })
        .collect()
}

/// A failure the group later fixed: what failed and how, in one short row.
/// Its duration and the rest of its output stay behind Enter.
fn recovered_failure_line(
    member: &Block,
    repeats: usize,
    selected_member: Option<&BlockId>,
    ctx: &BlockRender<'_>,
) -> Line<'static> {
    let BlockKind::Tool(tool) = member.kind() else {
        return Line::default();
    };
    let name = sanitize_terminal_text(&tool.name);
    let target = first_tool_segment(&sanitize_terminal_text(&tool.arguments_summary))
        .map(|segment| tool_target(&segment));
    let command_row = target.as_deref().is_some_and(is_command_target);
    let title = match target.as_deref() {
        Some(target) if command_row => target[..1].to_owned(),
        _ => tool_title(&name, BlockLifecycle::Failed),
    };
    let mut parts = vec![ToolDetailPart::fixed(title.clone())];
    if let Some(target) = target {
        parts.push(ToolDetailPart::flexible(command_text(target, command_row), 0, 1).attached());
    }
    if let Some(reason) = short_failure_reason(&tool.preview)
        .split(" · ")
        .next()
        .filter(|reason| !reason.is_empty())
    {
        parts.push(ToolDetailPart::flexible(reason.to_owned(), 1, 1));
    }
    if repeats > 1 {
        parts.push(ToolDetailPart::fixed(format!("×{repeats}")));
    }
    parts.push(ToolDetailPart::fixed("depois passou".into()));
    let marker = if selected_member == Some(&member.id) {
        "> "
    } else {
        "  "
    };
    let glyph_text = format!("{} ", glyph(ctx.capabilities, '\u{2715}', 'x'));
    let occupied = 2 + UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(glyph_text.as_str());
    let detail = fit_tool_detail(parts, (ctx.width as usize).saturating_sub(occupied));
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(marker.to_owned(), ctx.palette.muted),
        // Fixed later in the same group: the mark recedes instead of alarming.
        Span::styled(glyph_text, ctx.palette.muted),
    ];
    spans.extend(tool_detail_spans(
        &detail,
        &title,
        ctx.palette.muted,
        ctx.palette,
    ));
    Line::from(spans)
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
        for member in members {
            let BlockKind::Thinking(text) = member.kind() else {
                continue;
            };
            lines.extend(cache.wrapped_body(
                member,
                BodyKind::Thinking,
                ctx.width,
                member.lifecycle != BlockLifecycle::Streaming,
                || thinking_body_lines(text, ctx.width, ctx.palette),
                cached_lines_bytes,
            ));
        }
    }
    lines
}

/// An expanded thought under its header: italic in the reasoning hue, apart
/// from the answer, with the titles of a summary upright and bold.
fn thinking_body_lines(text: &str, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let prose = palette.reasoning.add_modifier(Modifier::ITALIC);
    let title = palette.reasoning.add_modifier(Modifier::BOLD);
    crate::thought::body_rows(text, thinking_body_width(width))
        .into_iter()
        .map(|(row, is_title)| {
            Line::from(Span::styled(
                format!("    {row}"),
                if is_title { title } else { prose },
            ))
        })
        .collect()
}

fn grouped_failed_tool_lines(
    leader: &Block,
    members: &[Block],
    show_enter_hint: bool,
    selected_member: Option<&BlockId>,
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
    let command_row = target.as_deref().is_some_and(is_command_target);
    let title = match target.as_deref() {
        Some(target) if command_row => target[..1].to_owned(),
        _ => tool_title(&name, BlockLifecycle::Failed),
    };
    let mut parts = vec![ToolDetailPart::fixed(title)];
    if let Some(target) = target {
        parts.push(ToolDetailPart::flexible(command_text(target, command_row), 0, 1).attached());
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
    let failed = if leader.group_expanded {
        glyph(ctx.capabilities, '\u{25be}', 'v')
    } else {
        glyph(ctx.capabilities, '\u{2715}', 'x')
    };
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
    if !leader.group_expanded
        && show_enter_hint
        && marker_width + header_width + 1 + hint_width <= content_width
    {
        let gap = content_width.saturating_sub(marker_width + header_width + hint_width);
        header_spans.push(Span::raw(" ".repeat(gap)));
        header_spans.push(Span::styled(hint.to_owned(), ctx.palette.muted));
    }
    let mut lines = vec![Line::from(header_spans)];
    if leader.group_expanded {
        lines.extend(grouped_tool_member_lines(
            members,
            selected_member,
            ctx,
            cache,
        ));
    }
    lines
}

fn grouped_tool_member_lines(
    members: &[Block],
    selected: Option<&BlockId>,
    ctx: &BlockRender<'_>,
    cache: &mut WrapCache,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // The members of an open group sit one step in from its header; so do
    // the details and the thoughts that belong to them.
    let width = ctx
        .width
        .saturating_sub(crate::render::GROUP_MEMBER_INDENT)
        .max(1);
    for member in members {
        let member_ctx = BlockRender {
            selected: selected == Some(&member.id),
            width,
            ..*ctx
        };
        let rows = if matches!(member.kind(), BlockKind::Tool(_)) {
            tool_member_lines(
                member,
                member_ctx.selected,
                ctx.palette,
                ctx.capabilities,
                width,
                ctx.frame,
                ctx.now_ms,
                true,
                cache,
            )
        } else {
            safe_block_lines(member, &member_ctx, cache)
        };
        lines.extend(rows.into_iter().map(|mut row| {
            row.spans.insert(
                0,
                Span::raw(" ".repeat(usize::from(crate::render::GROUP_MEMBER_INDENT))),
            );
            row
        }));
    }
    lines
}

fn short_failure_reason(preview: &str) -> String {
    let line = tool_preview_line(preview);
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

/// Marker of a settled call: green only where a change was applied.
fn settled_marker_style(effect: ToolEffect, palette: &Palette) -> Style {
    if effect == ToolEffect::Changes {
        palette.success
    } else {
        palette.muted
    }
}

/// Verb of a settled call: reads recede; commands and edits read one step up.
fn settled_verb_style(effect: ToolEffect, palette: &Palette) -> Style {
    if effect == ToolEffect::Observes {
        palette.muted
    } else {
        palette.secondary
    }
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
    expanded_member: bool,
    cache: &mut WrapCache,
) -> Vec<Line<'static>> {
    let BlockKind::Tool(state) = block.kind() else {
        return Vec::new();
    };
    let effect = tool_effect(&state.name);
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
            settled_marker_style(effect, palette),
            settled_verb_style(effect, palette),
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
    // The glyph and the verb ease from a brighter tone to their resting one
    // right after the call settles, success or failure, inside the window
    // the row keeps before it joins a group.
    let settle = tool_settle_level(block, now_ms, capabilities);
    let glyph_style = tool_settled(glyph_style, settle, capabilities.color_depth);
    let name_style = tool_settled(name_style, settle, capabilities.color_depth);
    let marker = if selected { "> " } else { "  " };
    let name = sanitize_terminal_text(&state.name);
    let args = sanitize_terminal_text(&state.arguments_summary);
    // A running command's last output line rides after the clock instead of
    // among the call's own segments; its byte counts stay where they were,
    // and the line takes only the width they leave.
    let (live_tail, preview) = if block.lifecycle == BlockLifecycle::Streaming
        && !state.historical
        && state.name == "shell"
    {
        split_shell_progress(&tool_preview_line(&state.preview))
    } else {
        (None, tool_preview_line(&state.preview))
    };
    let first_target = first_tool_segment(&args).map(|segment| tool_target(&segment));
    let command_row = !state.historical && first_target.as_deref().is_some_and(is_command_target);
    let show_args = !state.historical
        && (block.lifecycle == BlockLifecycle::Streaming
            || block.fold == FoldState::Expanded
            || expanded_member);
    let collapsed_failure = matches!(block.lifecycle, BlockLifecycle::Failed)
        && block.fold != FoldState::Expanded
        && !expanded_member;
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
        .map(|(target, _)| command_text(target, command_row));
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
    // A preview segment that only restates the target (`src/lib.rs`,
    // `patched src/lib.rs:2`) says nothing the title row does not.
    let preview_segments: Vec<&str> = preview
        .split(" · ")
        .filter(|value| !value.is_empty())
        .filter(|value| {
            !show_args
                || command_row
                || first_target
                    .as_deref()
                    .is_none_or(|target| target.chars().count() < 3 || !value.contains(target))
        })
        .collect();
    let preview_shown = (show_preview && !preview_segments.is_empty())
        || failure_reason.is_some()
        || compact_result.is_some();
    // Restored calls keep their raw name: a past-tense verb would imply an
    // outcome that history does not record. A command reads as itself: `$`
    // or `!` already says it runs, and the glyph says how it settled.
    let title = if state.historical {
        name
    } else if command_row {
        first_target
            .as_deref()
            .and_then(|target| target.split(' ').next())
            .unwrap_or("$")
            .to_owned()
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
                ToolDetailPart::flexible(command_text(tool_target(value), command_row), 0, 1)
                    .attached()
            } else {
                ToolDetailPart::fixed(value.to_owned())
            });
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
        for (index, value) in preview_segments.into_iter().enumerate() {
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
    } else if block.lifecycle == BlockLifecycle::Streaming {
        // Same slot the settled duration takes, so the row keeps its shape and
        // a parallel batch shows which call is the slow one.
        if let Some(elapsed) = running_elapsed_label(block.started_ms, now_ms) {
            parts.push(ToolDetailPart::fixed(elapsed));
        }
    }
    if let Some(value) = status.filter(|_| !preview_shown) {
        parts.push(ToolDetailPart::fixed(value.to_owned()));
    }
    let occupied = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(glyph_text.as_str());
    let available = (width as usize).saturating_sub(occupied);
    let detail = fit_tool_detail(parts, available);
    let mut header_spans = vec![
        Span::styled(marker.to_owned(), palette.muted),
        Span::styled(glyph_text, glyph_style),
    ];
    header_spans.extend(tool_detail_spans(&detail, &title, name_style, palette));
    if let Some(tail) = live_tail {
        // Whatever the row leaves free, never more: nothing else shrinks for
        // it and the row keeps its single line.
        let room = available.saturating_sub(
            UnicodeWidthStr::width(detail.as_str()) + UnicodeWidthStr::width(" · "),
        );
        if room >= LIVE_TAIL_MIN_CELLS {
            header_spans.push(Span::styled(" · ", palette.muted));
            header_spans.push(Span::styled(crate::thought::fit(&tail, room), palette.tail));
        }
    }
    let mut lines = vec![Line::from(header_spans)];
    if block.fold == FoldState::Expanded && state.has_expanded_body() {
        let body_width = width.saturating_sub(4).max(1);
        lines.extend(cache.wrapped_body(
            block,
            BodyKind::ToolOutput,
            width,
            block.lifecycle != BlockLifecycle::Streaming,
            || {
                // The typed projection preserves the text measured by layout
                // and keeps tool output independent of diff markers.
                let diff_rows = state.diff_rows();
                state
                    .expanded_body()
                    .split('\n')
                    .enumerate()
                    .flat_map(|(index, source)| {
                        let style = match diff_rows.get(index).map(|(kind, _)| kind) {
                            Some(DiffRowKind::Added) => palette.diff_add.patch(palette.diff_add_bg),
                            Some(DiffRowKind::Removed) => {
                                palette.diff_remove.patch(palette.diff_remove_bg)
                            }
                            Some(DiffRowKind::Header) => palette.muted,
                            None => palette.secondary,
                        };
                        render_plain(source, body_width)
                            .into_iter()
                            .map(move |row| {
                                Line::from(vec![
                                    Span::raw("    "),
                                    Span::styled(pad_cells(&row, body_width as usize), style),
                                ])
                            })
                    })
                    .collect()
            },
            cached_lines_bytes,
        ));
    }
    lines
}

/// Narrowest tail worth showing; below it the row would only show a stub.
const LIVE_TAIL_MIN_CELLS: usize = 8;

/// Splits the progress of a running command (`<last line> · out N B · err M B`)
/// into its last output line, whitespace folded, and the byte counts. Nothing
/// has been printed yet when the line is the harness's `no output yet`; a
/// preview in any other shape is left alone.
fn split_shell_progress(preview: &str) -> (Option<String>, String) {
    let segments: Vec<&str> = preview.split(" · ").collect();
    let [line @ .., out, err] = segments.as_slice() else {
        return (None, preview.to_owned());
    };
    if line.is_empty() || !out.starts_with("out ") || !err.starts_with("err ") {
        return (None, preview.to_owned());
    }
    let line = line.join(" · ");
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let counts = format!("{out} · {err}");
    (
        (!line.is_empty() && line != "no output yet").then_some(line),
        counts,
    )
}

/// First line of a tool's preview as a summary row reads it. Runtime
/// annotations (`[note: …]`) belong to the details view, and a managed job's
/// control text (`job_id=… state=… elapsed_ms=…`) reads as a sentence; the
/// row already carries the elapsed time in its own slot.
fn tool_preview_line(preview: &str) -> String {
    let line = sanitize_terminal_text(preview.lines().next().unwrap_or_default());
    let line = match line.find("[note:") {
        Some(start) => line[..start].trim_end_matches([' ', '·']).to_owned(),
        None => line,
    };
    let mut words = line.split_whitespace();
    if let (Some(id), Some(state), Some(elapsed), None) =
        (words.next(), words.next(), words.next(), words.next())
    {
        if let (Some(id), Some(state), Some(_)) = (
            id.strip_prefix("job_id="),
            state.strip_prefix("state="),
            elapsed.strip_prefix("elapsed_ms="),
        ) {
            return format!("{id} {}", job_state_label(state));
        }
    }
    line
}

fn first_tool_segment(value: &str) -> Option<String> {
    value
        .split(" · ")
        .map(str::trim)
        .find(|part| !part.is_empty())
        .map(str::to_owned)
}

/// A model command (`$ cmd`) or a user command (`! cmd`).
fn is_command_target(target: &str) -> bool {
    target.starts_with("$ ") || target.starts_with("! ")
}

/// The command without its sigil, which the row already shows as its title.
fn command_text(target: String, command_row: bool) -> String {
    if command_row && is_command_target(&target) {
        target[2..].to_owned()
    } else {
        target
    }
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
        || trimmed.strip_suffix('s').is_some_and(|value| {
            numeric(value)
                || value
                    .split_once('m')
                    .is_some_and(|(minutes, seconds)| numeric(minutes) && numeric(seconds))
        })
}

/// Whole-second clock for a call that is still running (`12s`, `1m05s`).
/// Nothing is shown in the first second, so quick calls never flash a `0s`.
fn running_elapsed_label(started_ms: Option<u64>, now_ms: u64) -> Option<String> {
    let seconds = now_ms.saturating_sub(started_ms?) / 1_000;
    match seconds {
        0 => None,
        1..=59 => Some(format!("{seconds}s")),
        _ => Some(format!("{}m{:02}s", seconds / 60, seconds % 60)),
    }
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

/// The emphasis of a fresh change is a brightness settle: the foreground
/// starts lifted toward white and steps back to its resting color over
/// [`INFO_TOAST_HIGHLIGHT_MS`], one step per motion frame. The lift of each
/// step is the share of the way to white.
const SETTLE_STEPS: u64 = 3;
const SETTLE_STEP_MS: u64 = INFO_TOAST_HIGHLIGHT_MS / SETTLE_STEPS;
const SETTLE_LIFT: [f32; SETTLE_STEPS as usize] = [0.55, 0.30, 0.12];

/// Settle step for something that happened `elapsed_ms` ago: `0` is the
/// brightest, `None` once the style is back at rest.
fn settle_phase_since(elapsed_ms: u64) -> Option<u8> {
    (elapsed_ms < INFO_TOAST_HIGHLIGHT_MS)
        .then(|| (elapsed_ms / SETTLE_STEP_MS).min(SETTLE_STEPS - 1) as u8)
}

/// The next instant after `now_ms` at which a stepped transition that began
/// at `at_ms` moves to its next step; the last boundary ends the `total_ms`
/// window. `None` once the window is over.
fn next_step_boundary_ms(at_ms: u64, now_ms: u64, step_ms: u64, total_ms: u64) -> Option<u64> {
    let elapsed = now_ms.saturating_sub(at_ms);
    (elapsed < total_ms).then(|| {
        let next = (elapsed / step_ms + 1) * step_ms;
        at_ms.saturating_add(next.min(total_ms))
    })
}

fn next_settle_boundary_ms(at_ms: u64, now_ms: u64) -> Option<u64> {
    next_step_boundary_ms(at_ms, now_ms, SETTLE_STEP_MS, INFO_TOAST_HIGHLIGHT_MS)
}

/// The backdrop of a modal settles to its resting tone in two steps (the same
/// pace as the settle emphasis) after the modal opens.
const BACKDROP_ENTRANCE_STEPS: u64 = 2;
const BACKDROP_ENTRANCE_STEP_MS: u64 = SETTLE_STEP_MS;
const BACKDROP_ENTRANCE_MS: u64 = BACKDROP_ENTRANCE_STEP_MS * BACKDROP_ENTRANCE_STEPS;
/// Share of the way from the resting backdrop to the text color, per step.
const BACKDROP_ENTRANCE_LIFT: [f32; BACKDROP_ENTRANCE_STEPS as usize] = [0.5, 0.25];

/// Only depths that can tell shades apart show the entrance; at the others
/// the backdrop is the resting one from the first frame.
fn backdrop_entrance_enabled(capabilities: Capabilities) -> bool {
    !capabilities.reduced_motion
        && matches!(
            capabilities.color_depth,
            ColorDepth::TrueColor | ColorDepth::Ansi256
        )
}

/// Entrance step (`0` is the lightest) of a modal opened at `opened_ms`.
fn backdrop_entrance_phase(
    opened_ms: u64,
    now_ms: u64,
    capabilities: Capabilities,
) -> Option<usize> {
    let elapsed = now_ms.saturating_sub(opened_ms);
    (backdrop_entrance_enabled(capabilities) && elapsed < BACKDROP_ENTRANCE_MS)
        .then(|| (elapsed / BACKDROP_ENTRANCE_STEP_MS).min(BACKDROP_ENTRANCE_STEPS - 1) as usize)
}

fn settle_phase(at_ms: Option<u64>, now_ms: u64, capabilities: Capabilities) -> Option<u8> {
    if capabilities.reduced_motion {
        return None;
    }
    settle_phase_since(now_ms.saturating_sub(at_ms?))
}

/// The freshest settle step among several changes.
fn freshest_phase(phases: impl IntoIterator<Item = Option<u8>>) -> Option<u8> {
    phases.into_iter().flatten().min()
}

/// `style` with the emphasis of a fresh change. Truecolor and 256-color
/// terminals get the lifted foreground of the current step; depths that
/// cannot tell shades apart keep the weight.
fn settled(style: Style, phase: Option<u8>, depth: ColorDepth) -> Style {
    let Some(phase) = phase else {
        return style;
    };
    if matches!(depth, ColorDepth::TrueColor | ColorDepth::Ansi256) {
        if let Some(fg) = style
            .fg
            .and_then(|fg| crate::theme::lift_color(fg, depth, SETTLE_LIFT[usize::from(phase)]))
        {
            return style.fg(fg);
        }
    }
    style.add_modifier(Modifier::BOLD)
}

/// Steps the tool settle is quantized to: the lines of a row are memoized per
/// level, so a motion frame inside the window only re-lays that one row.
const TOOL_SETTLE_LEVELS: u8 = 8;
/// Share of the way to white the glyph starts from.
const TOOL_SETTLE_LIFT: f32 = 0.55;

/// How far a tool row that just settled still is from its resting tone, from
/// `TOOL_SETTLE_LEVELS` (just settled) down to `0` (at rest). The window is
/// the hold the row keeps before it joins a group, so one timing serves both,
/// and the curve is a smoothstep: it leaves quickly and lands softly.
/// Reduced motion shows the resting tone at once.
fn tool_settle_level(block: &Block, now_ms: u64, capabilities: Capabilities) -> u8 {
    if capabilities.reduced_motion || !crate::block::is_settled_tool(block) {
        return 0;
    }
    let Some(ended) = block.ended_ms else {
        return 0;
    };
    let elapsed = now_ms.saturating_sub(ended);
    if elapsed >= crate::block::TOOL_GROUP_HOLD_MS {
        return 0;
    }
    let progress = elapsed as f32 / crate::block::TOOL_GROUP_HOLD_MS as f32;
    let remaining = 1.0 - progress * progress * (3.0 - 2.0 * progress);
    ((remaining * f32::from(TOOL_SETTLE_LEVELS)).ceil() as u8).clamp(1, TOOL_SETTLE_LEVELS)
}

/// `style` with the tone of settle `level`: truecolor and 256-color terminals
/// get the exact shade between the lifted and the resting foreground; depths
/// that cannot tell shades apart carry the first moments in weight.
fn tool_settled(style: Style, level: u8, depth: ColorDepth) -> Style {
    if level == 0 {
        return style;
    }
    if matches!(depth, ColorDepth::TrueColor | ColorDepth::Ansi256) {
        let lift = TOOL_SETTLE_LIFT * f32::from(level) / f32::from(TOOL_SETTLE_LEVELS);
        if let Some(fg) = style
            .fg
            .and_then(|fg| crate::theme::lift_color(fg, depth, lift))
        {
            return style.fg(fg);
        }
    }
    style.add_modifier(Modifier::BOLD)
}

fn block_transition_phase(block: &Block, ctx: &BlockRender<'_>) -> Option<u8> {
    settle_phase(
        block.ended_ms.or(block.started_ms),
        ctx.now_ms,
        ctx.capabilities,
    )
}

/// Marker of the prompt's header. The raised band tells the prompt from the
/// answer wherever shades show; at 16 colors and under `NO_COLOR` the band
/// collapses into the background, so the shape does it: the prompt takes the
/// composer's own ASCII `>` (what you typed), while the agent keeps its dot
/// (`*` without color).
fn user_marker(capabilities: Capabilities) -> char {
    match capabilities.color_depth {
        ColorDepth::TrueColor | ColorDepth::Ansi256 => '●',
        ColorDepth::Ansi16 | ColorDepth::None => '>',
    }
}

fn user_message_lines(
    text: &str,
    ctx: &BlockRender<'_>,
    settle: Option<u8>,
    awaiting_agent: bool,
) -> Vec<Line<'static>> {
    let surface = ctx.palette.user_prompt_bg;
    let marker_style = if settle.is_some() {
        settled(ctx.palette.user, settle, ctx.capabilities.color_depth)
    } else {
        ctx.palette.user.add_modifier(Modifier::DIM)
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
            user_marker(ctx.capabilities).to_string(),
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
    if awaiting_agent {
        // The agent has not produced anything yet. Its header is already
        // here, in the row the first block will take over.
        lines.extend(response_header_lines(
            crate::block::ResponseHeader {
                cancelled: false,
                failed: false,
                lead_blank: false,
            },
            ctx,
        ));
    }
    lines
}

fn question_block_lines(
    state: &InteractionRequestState,
    ctx: &BlockRender<'_>,
    settle: Option<u8>,
) -> Vec<Line<'static>> {
    let width = ctx.width.saturating_sub(2).max(8) as usize;
    let depth = ctx.capabilities.color_depth;
    let Some((question, answer)) = state.question_record_rows(width) else {
        return state
            .layout_lines(width)
            .into_iter()
            .map(|row| Line::from(Span::styled(row, ctx.palette.secondary)))
            .collect();
    };
    // The question recedes once answered; the answer is what the turn keeps.
    let answer_style = if state
        .acknowledgement
        .as_ref()
        .is_some_and(|ack| ack.accepted)
    {
        settled(ctx.palette.text, settle, depth)
    } else {
        ctx.palette.muted
    };
    let rows = |rows: Vec<String>, marker: &'static str, marker_style: Style, body_style: Style| {
        rows.into_iter()
            .map(move |row| match row.strip_prefix(marker) {
                Some(body) => Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(body.to_owned(), body_style),
                ]),
                None => Line::from(Span::styled(row, body_style)),
            })
    };
    rows(
        question,
        crate::block::QUESTION_RECORD_MARKER,
        ctx.palette.accent,
        ctx.palette.secondary,
    )
    .chain(rows(
        answer,
        crate::block::QUESTION_ANSWER_MARKER,
        ctx.palette.muted,
        answer_style,
    ))
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
    caret: CaretPhase,
}

/// Streaming caret cadence: on for six motion frames (498 ms), then off.
const CARET_BLINK_FRAMES: u64 = 6;
/// Provider silence after which the caret stops blinking and dims, a few
/// seconds before the ActivityRail says it in words.
const CARET_STALL_MS: u64 = 2_000;

/// What the streaming caret of the message being written looks like this
/// frame. It occupies the cell reserved at the end of the message in every
/// phase, so blinking never moves text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaretPhase {
    /// Reduced motion: no caret at all.
    Hidden,
    On,
    Off,
    /// No content for [`CARET_STALL_MS`]: steady and dim instead of blinking.
    Stalled,
}

impl CaretPhase {
    fn of(state: &AppState, capabilities: Capabilities) -> Self {
        if capabilities.reduced_motion {
            return Self::Hidden;
        }
        let silent_ms = state
            .last_provider_content_ms
            .map(|at| state.clock.elapsed_ms.saturating_sub(at));
        if silent_ms.is_some_and(|silent| silent >= CARET_STALL_MS) {
            Self::Stalled
        } else if (state.clock.frame / CARET_BLINK_FRAMES).is_multiple_of(2) {
            Self::On
        } else {
            Self::Off
        }
    }

    fn tag(self) -> u64 {
        match self {
            Self::Hidden => 0,
            Self::On => 1,
            Self::Off => 2,
            Self::Stalled => 3,
        }
    }
}

/// `● Slim` opens the agent's side of a turn, in the same gutter as the
/// user's `● Você`. Cancellation or failure of any prose segment of the turn
/// is reported here and mutes the label.
fn response_header_lines(
    header: crate::block::ResponseHeader,
    ctx: &BlockRender<'_>,
) -> Vec<Line<'static>> {
    let label_style = if header.cancelled || header.failed {
        ctx.palette.secondary
    } else {
        ctx.palette.assistant_bold
    };
    // The marker is the turn's state: green while it runs or after it
    // succeeded, amber when interrupted, red when a segment failed.
    let marker_style = if header.failed {
        ctx.palette.error
    } else if header.cancelled {
        ctx.palette.warning
    } else {
        ctx.palette.assistant_bold
    };
    // Color alone cannot tell the outcomes apart (16 colors, NO_COLOR), so a
    // failed or interrupted turn also takes the shape tool rows use for it.
    // Every marker is one cell: the pending header's name sweep starts in the
    // fourth column.
    let marker = if header.failed {
        glyph(ctx.capabilities, '\u{2715}', 'x')
    } else if header.cancelled {
        glyph(ctx.capabilities, '\u{25a0}', '!')
    } else {
        glyph(ctx.capabilities, '●', '*')
    };
    let lifecycle = if header.cancelled {
        BlockLifecycle::Cancelled
    } else {
        BlockLifecycle::Complete
    };
    let mut lines = Vec::with_capacity(header.rows());
    if header.lead_blank {
        lines.push(Line::default());
    }
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(assistant_label(lifecycle), label_style),
    ]));
    lines
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
    // Same grammar as a tool row: the progressive while it runs, the past
    // once it settled.
    let phase_label = match block.lifecycle {
        BlockLifecycle::Streaming => THINKING_LABEL.into(),
        BlockLifecycle::Cancelled => "Pensamento interrompido".into(),
        BlockLifecycle::Failed => "Pensamento falhou".into(),
        _ if count > 1 => format!("Pensou ×{count}"),
        _ => "Pensou".into(),
    };
    let label = if streaming {
        // A running clock counts whole seconds, like a running tool; the
        // decimal appears once the thought has a measured duration.
        running_elapsed_label(block.started_ms, ctx.now_ms).map_or(phase_label.clone(), |clock| {
            format!("{phase_label} · {clock}")
        })
    } else {
        thinking_duration_ms(block, ctx.now_ms)
            .filter(|_| count <= 1)
            .filter(|duration| *duration > 0)
            .map_or(phase_label.clone(), |duration| {
                format!("{phase_label} · {}", duration_label(duration))
            })
    };
    let label = truncate_cells(&label, ctx.width.saturating_sub(4) as usize);
    let end_phase = (!streaming)
        .then(|| block_transition_phase(block, ctx))
        .flatten();
    // The thought reads in the reasoning hue whether it streams or has
    // settled; only the marker of a streaming one keeps the blue of every
    // live indicator. While motion runs, a highlight sweeps the label.
    let label_style = settled(
        ctx.palette.reasoning,
        end_phase,
        ctx.capabilities.color_depth,
    );
    let mut spans = vec![
        Span::styled(if ctx.selected { "> " } else { "  " }, ctx.palette.muted),
        Span::styled(
            format!("{indicator} "),
            if streaming {
                ctx.palette.accent
            } else {
                ctx.palette.reasoning
            },
        ),
        Span::styled(label.clone(), label_style),
    ];
    let hint = ctx.selected.then_some(if expanded {
        "Enter recolher"
    } else {
        "Enter expandir"
    });
    let mut used = 4 + UnicodeWidthStr::width(label.as_str());
    // A streaming thought says what it is on about on the same row, so it
    // takes one row however long it gets; the full text is behind Enter.
    if streaming && !expanded {
        if let BlockKind::Thinking(text) = block.kind() {
            let reserved = hint.map_or(0, |hint| hint.len() + 2);
            let room = (ctx.width as usize)
                .saturating_sub(used + reserved + 3)
                .min(THINKING_STATUS_CELLS);
            // A headline is a settled statement and reads upright; words
            // still arriving keep the italic of the thought itself.
            let status = crate::thought::status(text, room).map(|status| match status {
                crate::thought::Status::Headline(text) => (text, ctx.palette.secondary),
                crate::thought::Status::Tail(text) => (
                    text,
                    ctx.palette.reasoning_dim.add_modifier(Modifier::ITALIC),
                ),
            });
            if let Some((status, style)) = status {
                used += 3 + UnicodeWidthStr::width(status.as_str());
                spans.push(Span::styled(" · ", ctx.palette.muted));
                spans.push(Span::styled(status, style));
            }
        }
    }
    if let Some(hint) = hint {
        if used + 2 + hint.len() <= ctx.width as usize {
            spans.push(Span::raw(
                " ".repeat(ctx.width as usize - used - hint.len()),
            ));
            spans.push(Span::styled(hint, ctx.palette.secondary));
        }
    }
    Line::from(spans)
}

/// Widest the status of a streaming thought gets, so the row reads as a
/// glance rather than a paragraph.
const THINKING_STATUS_CELLS: usize = 96;

fn thinking_duration_ms(block: &Block, now_ms: u64) -> Option<u64> {
    let started = block.started_ms?;
    let ended = block
        .ended_ms
        .or_else(|| (block.lifecycle == BlockLifecycle::Streaming).then_some(now_ms))?;
    Some(ended.saturating_sub(started))
}

fn grouped_queued_user_lines(
    leader: &Block,
    members: &[Block],
    show_enter_hint: bool,
    ctx: &BlockRender<'_>,
) -> Vec<Line<'static>> {
    let expanded = leader.fold == FoldState::Expanded;
    let indicator = if expanded {
        glyph(ctx.capabilities, '\u{25be}', 'v')
    } else {
        glyph(ctx.capabilities, '\u{25b8}', '>')
    };
    let hint = if show_enter_hint && !expanded && ctx.width >= 27 {
        "  Enter textos"
    } else {
        ""
    };
    // The row owns the queue's count; the composer border keeps only what
    // asks for action (a paused queue, Enter queues).
    let queued = members
        .iter()
        .filter(|member| matches!(member.kind(), BlockKind::QueuedUser(_)))
        .count();
    let mut label = if queued > 1 {
        format!("{queued} na fila")
    } else {
        String::from("Na fila")
    };
    if !expanded {
        if let BlockKind::QueuedUser(text) = leader.kind() {
            let safe = sanitize_terminal_text(text);
            let preview = safe.lines().next().unwrap_or_default().trim();
            if !preview.is_empty() {
                label.push_str(" · ");
                label.push_str(preview);
            }
        }
    }
    let label = truncate_cells(
        &label,
        usize::from(ctx.width.saturating_sub(4)).saturating_sub(UnicodeWidthStr::width(hint)),
    );
    let mut lines = vec![Line::from(vec![
        Span::styled(if ctx.selected { "> " } else { "  " }, ctx.palette.muted),
        Span::styled(format!("{indicator} {label}{hint}"), ctx.palette.muted),
    ])];
    if expanded {
        for member in members {
            let BlockKind::QueuedUser(text) = member.kind() else {
                continue;
            };
            let phase = block_transition_phase(member, ctx);
            let style = if phase.is_some() {
                settled(ctx.palette.warning, phase, ctx.capabilities.color_depth)
            } else {
                ctx.palette.muted
            };
            lines.extend(
                render_plain(text, ctx.width.saturating_sub(4).max(1))
                    .into_iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let prefix = if index == 0 { "  … " } else { "    " };
                        Line::from(Span::styled(format!("{prefix}{row}"), style))
                    }),
            );
        }
    }
    lines
}

fn block_lines(block: &Block, ctx: &BlockRender<'_>, cache: &mut WrapCache) -> Vec<Line<'static>> {
    let mut lines = match block.kind() {
        BlockKind::User(text) => user_message_lines(
            text,
            ctx,
            block_transition_phase(block, ctx),
            block.awaiting_agent(),
        ),
        BlockKind::Assistant(text) => assistant_body(
            text,
            ctx,
            block.lifecycle == BlockLifecycle::Streaming,
            cache,
            block,
        ),
        BlockKind::Thinking(text) => {
            let streaming = block.lifecycle == BlockLifecycle::Streaming;
            let mut lines = vec![thinking_header(block, 1, ctx)];
            if block.fold == FoldState::Expanded {
                // Expanded bodies are wrapped once per generation and shared
                // with the height probe instead of re-wrapping every frame.
                lines.extend(cache.wrapped_body(
                    block,
                    BodyKind::Thinking,
                    ctx.width,
                    !streaming,
                    || thinking_body_lines(text, ctx.width, ctx.palette),
                    cached_lines_bytes,
                ));
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
                question_block_lines(state, ctx, block_transition_phase(block, ctx))
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
        BlockKind::QueuedUser(_) => {
            grouped_queued_user_lines(block, std::slice::from_ref(block), ctx.selected, ctx)
        }
        BlockKind::Receipt(receipt) => vec![receipt_line(receipt, ctx)],
        BlockKind::Work(_) => work_lines(block, std::slice::from_ref(block), ctx, cache),
    };
    if block.turn_boundary_before() {
        lines.insert(0, Line::default());
    }
    lines
}

/// The row that folds the work of a finished turn: `▸ Trabalhou 12s · 4
/// leituras, 2 edições, 2 comandos`, then one row per failure, stepped in as a
/// collapsed group steps them. Opened, it is only the row (`▾`): the blocks it
/// folded are back in the transcript as they were. `members` is the row alone
/// when open, or the row followed by what it folds.
fn work_lines(
    leader: &Block,
    members: &[Block],
    ctx: &BlockRender<'_>,
    cache: &mut WrapCache,
) -> Vec<Line<'static>> {
    let BlockKind::Work(work) = leader.kind() else {
        return Vec::new();
    };
    let expanded = leader.fold == FoldState::Expanded;
    let palette = ctx.palette;
    let marker = if ctx.selected { "> " } else { "  " };
    let indicator = if expanded {
        glyph(ctx.capabilities, '\u{25be}', 'v')
    } else {
        glyph(ctx.capabilities, '\u{25b8}', '>')
    };
    let strongest = members[1..]
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::Tool(tool) => Some(tool_effect(&tool.name)),
            _ => None,
        })
        .max()
        .unwrap_or(ToolEffect::Observes);
    let width = usize::from(ctx.width);
    let hint = ctx.selected.then_some(if expanded {
        "Enter recolher"
    } else {
        "Enter expandir"
    });
    let mut used =
        UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(indicator.to_string().as_str());
    used += 1;
    let mut spans = vec![
        Span::styled(marker.to_owned(), palette.muted),
        Span::styled(format!("{indicator} "), palette.muted),
    ];
    let mut push = |text: String, style: Style, used: &mut usize| {
        *used += UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, style));
    };
    push(
        crate::work::WORK_LABEL.to_owned(),
        settled_verb_style(strongest, palette),
        &mut used,
    );
    if let Some(duration) = work.duration_ms {
        push(
            format!(" {}", crate::receipt::format_duration(duration)),
            palette.muted,
            &mut used,
        );
    }
    if !work.tally.is_empty() {
        let reserved = hint.map_or(0, |hint| hint.len() + 2);
        let room = width.saturating_sub(used + reserved + UnicodeWidthStr::width(" · "));
        if room > 0 {
            push(" · ".into(), palette.muted, &mut used);
            push(truncate_cells(&work.tally, room), palette.muted, &mut used);
        }
    }
    if let Some(hint) = hint {
        if used + 2 + hint.len() <= width {
            spans.push(Span::raw(" ".repeat(width - used - hint.len())));
            spans.push(Span::styled(hint, palette.secondary));
        }
    }
    let mut lines = vec![Line::from(spans)];
    if !expanded {
        lines.extend(group_failure_lines(&members[1..], None, ctx, cache));
    }
    lines
}

/// The row that closes a turn: `✓ 3 arquivos · +42 -7 · 2 comandos · 6s  Ctrl+D`.
/// The marker follows the turn: an interrupted run is amber, a last command
/// that failed is red, a run that changed files is green, and one that only
/// ran commands stays quiet. Pieces are dropped from the right when the row
/// is too narrow, the shortcut first.
fn receipt_line(receipt: &crate::receipt::ReceiptState, ctx: &BlockRender<'_>) -> Line<'static> {
    use crate::receipt::{ReceiptOutcome, ReceiptSegment};
    let palette = ctx.palette;
    let (marker, marker_style) = if receipt.outcome == ReceiptOutcome::Interrupted {
        (glyph(ctx.capabilities, '\u{25a0}', 'x'), palette.warning)
    } else if receipt.last_command_ok == Some(false) {
        (glyph(ctx.capabilities, '\u{2715}', 'x'), palette.error)
    } else if receipt.files > 0 {
        (glyph(ctx.capabilities, '\u{2713}', '+'), palette.success)
    } else {
        (glyph(ctx.capabilities, '\u{2713}', '+'), palette.muted)
    };
    let mut pieces: Vec<Vec<(String, Style)>> = Vec::new();
    if receipt.outcome == ReceiptOutcome::Interrupted {
        pieces.push(vec![("interrompido".into(), palette.warning)]);
    }
    for segment in receipt.segments() {
        pieces.push(match segment {
            ReceiptSegment::Text(text) => vec![(text, palette.muted)],
            ReceiptSegment::Lines {
                added,
                removed,
                partial,
            } => {
                let mut parts = Vec::new();
                if partial {
                    parts.push(("~".to_owned(), palette.muted));
                }
                parts.push((format!("+{added}"), palette.diff_add));
                parts.push((" ".to_owned(), palette.muted));
                parts.push((format!("-{removed}"), palette.diff_remove));
                parts
            }
        });
    }
    let width = |parts: &[(String, Style)]| -> usize {
        parts
            .iter()
            .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
            .sum()
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(format!("{marker} "), marker_style),
    ];
    let mut used = 4usize;
    let limit = usize::from(ctx.width);
    for (index, piece) in pieces.iter().enumerate() {
        let separator = if index == 0 { 0 } else { 3 };
        if used + separator + width(piece) > limit {
            break;
        }
        if separator > 0 {
            spans.push(Span::styled(" · ", palette.muted));
        }
        used += separator + width(piece);
        spans.extend(
            piece
                .iter()
                .map(|(text, style)| Span::styled(text.clone(), *style)),
        );
    }
    const HINT: &str = "Ctrl+D";
    if receipt.files > 0 && used + 3 + HINT.len() <= limit {
        spans.push(Span::styled(format!("   {HINT}"), palette.muted));
    }
    Line::from(spans)
}

fn assistant_body(
    text: &str,
    ctx: &BlockRender<'_>,
    streaming: bool,
    cache: &mut WrapCache,
    block: &Block,
) -> Vec<Line<'static>> {
    let text_width = crate::render::assistant_text_width(ctx.width);
    let styles = MarkdownStyles {
        text: ctx.palette.text,
        h1: ctx.palette.h1,
        h2: ctx.palette.heading,
        h3: ctx.palette.thinking,
        link: ctx.palette.link,
        code: ctx.palette.inline_code,
        code_block: ctx.palette.code_block,
        code_rail: ctx.palette.code_rail,
        code_label: ctx.palette.code_label,
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
    // The reserved cell always exists; only what fills it changes.
    let (caret, caret_style) = match (streaming, ctx.caret) {
        (true, CaretPhase::On) => ("▌", ctx.palette.assistant),
        (true, CaretPhase::Stalled) => ("▌", ctx.palette.muted),
        _ => (" ", ctx.palette.assistant),
    };
    if let Some(last) = lines.last_mut() {
        last.spans.push(Span::styled(caret, caret_style));
    } else {
        lines.push(Line::from(Span::styled(caret, caret_style)));
    }
    let indent = " ".repeat(crate::render::ASSISTANT_PREFIX_COLS as usize);
    lines
        .into_iter()
        .map(|mut line| {
            let mut spans = vec![Span::styled(indent.clone(), ctx.palette.surface)];
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
        && state.jobs_overlay.is_none()
        && state.job_exit_confirm.is_none()
        && state.session_picker.is_none()
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
    SessionFilter,
    Palette,
    McpSignIn,
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
    if state
        .mcp_overlay
        .as_ref()
        .is_some_and(|overlay| overlay.signin.is_some())
    {
        return Some(InputCursorTarget::McpSignIn);
    }
    if state.effort_overlay.is_some()
        || state.mcp_overlay.is_some()
        || state.jobs_overlay.is_some()
        || state.job_exit_confirm.is_some()
    {
        return None;
    }
    if state.session_picker.is_some() {
        return Some(InputCursorTarget::SessionFilter);
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

/// Rows of wrapped interaction text never run wider than this, however wide
/// the dropdown itself is.
const INTERACTION_TEXT_WIDTH: usize = 72;
/// The interaction dropdown rests on the composer: only its top border takes
/// a row.
const INTERACTION_CHROME_ROWS: u16 = 1;

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
    let width = composer_area.width;
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
    let bordered = |content: usize| (content as u16).saturating_add(INTERACTION_CHROME_ROWS);
    if bordered(lines.len()) > geometry.max_height {
        lines.retain(line_has_text);
    }
    let height = bordered(lines.len()).min(geometry.max_height);
    let inner_rows = height.saturating_sub(INTERACTION_CHROME_ROWS) as usize;
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
    let style = if selected {
        style.patch(palette.menu_selected)
    } else {
        style
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
    let body_width = inner_width
        .min(INTERACTION_TEXT_WIDTH)
        .saturating_sub(hang_width)
        .max(1);
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
    let mut lines = Vec::new();
    let push_wrapped = |lines: &mut Vec<Line<'static>>, text: &str, style: Style| {
        for row in wrap_words(text, inner_width.min(INTERACTION_TEXT_WIDTH)) {
            lines.push(Line::from(Span::styled(
                format!(" {}", pad_cells(&sanitize_terminal_text(&row), inner_width)),
                style,
            )));
        }
    };
    match &interaction.kind {
        InteractionRequestKind::Question { question, options } => {
            push_wrapped(&mut lines, question, palette.heading);
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
                // Typing a free answer keeps the focus on the row that
                // opened it; the composer below holds the caret.
                let other_selected = interaction.custom_question_answer
                    || interaction.selected_question_option == options.len();
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
            if !interaction.response_pending {
                lines.push(Line::default());
                push_wrapped(&mut lines, "Y aprovar · N rejeitar", palette.muted);
            }
        }
        InteractionRequestKind::Input { prompt, options } => {
            push_wrapped(&mut lines, prompt, palette.heading);
            if !options.is_empty() {
                push_wrapped(
                    &mut lines,
                    &format!("opções: {}", options.join(" · ")),
                    palette.muted,
                );
            }
        }
    }
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
    let interaction_phase = freshest_phase(
        state
            .blocks()
            .iter()
            .filter(|block| {
                block.lifecycle == BlockLifecycle::Pending
                    && matches!(block.kind(), BlockKind::InteractionRequest(_))
            })
            .map(|block| settle_phase(block.started_ms, state.clock.elapsed_ms, capabilities)),
    );
    let mut lines = interaction_overlay_lines(interaction, inner_width, palette);
    let max_height = geometry.max_height;
    let bordered = |content: usize| (content as u16).saturating_add(INTERACTION_CHROME_ROWS);
    let approval = matches!(interaction.kind, InteractionRequestKind::Approval { .. });
    if bordered(lines.len()) > max_height {
        lines.retain(line_has_text);
    }
    if !approval && bordered(lines.len()) > max_height {
        if let Some(compact) = compact_question_lines(
            interaction,
            inner_width,
            max_height.saturating_sub(INTERACTION_CHROME_ROWS) as usize,
            palette,
        ) {
            lines = compact;
        }
    }
    let mut height = bordered(lines.len()).min(max_height);
    let inner_rows = height.saturating_sub(INTERACTION_CHROME_ROWS) as usize;
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
        height = bordered(lines.len()).min(max_height);
    }
    let area = ratatui::layout::Rect {
        x: composer_area.x,
        y: composer_area.y.saturating_sub(height),
        width,
        height,
    };
    let border_style = settled(
        palette.border_focus,
        interaction_phase,
        capabilities.color_depth,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            // Same silhouette as the `/` and `@` dropdowns: the list rests on
            // the composer's top border, whose label carries the key hint.
            RatatuiBlock::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
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
    // Key hints live in the footer only; the placeholder names the action.
    Some(if state.working {
        "Próxima mensagem (vai para a fila)…"
    } else {
        "Próxima tarefa…"
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
    activity: Option<FooterActivity>,
    cache: &mut WrapCache,
) {
    let activity_visible = activity.is_some();
    let lines = cache.footer_lines(state, area.width, area.height, activity_visible);
    let buf = frame.buffer_mut();
    buf.set_style(area, palette.surface);
    let last = lines.len().saturating_sub(1);
    for (index, text) in lines.iter().enumerate() {
        let line = match activity {
            // The last row is where the run reads: its phase, then the
            // controls that fit beside it. Controls win over the phase.
            Some(activity) if index == last => {
                activity_footer_line(state, area, palette, capabilities, activity, cache)
            }
            _ => footer_line(
                index,
                area.height,
                text,
                state,
                activity_visible,
                capabilities,
                palette,
            ),
        };
        buf.set_line(area.x, area.y + index as u16, &line, area.width);
    }
}

fn activity_footer_line(
    state: &AppState,
    area: ratatui::layout::Rect,
    palette: &Palette,
    capabilities: Capabilities,
    activity: FooterActivity,
    cache: &mut WrapCache,
) -> Line<'static> {
    const SEPARATOR: &str = " · ";
    let width = usize::from(area.width);
    let mut spans = activity_spans(state, area.width, palette, activity);
    let phase_width: usize = spans.iter().map(Span::width).sum();
    let separator_width = UnicodeWidthStr::width(SEPARATOR);
    let room = width.saturating_sub(phase_width + separator_width);
    let fitted = |cache: &mut WrapCache, room: usize| {
        cache
            .footer_lines(
                state,
                room.min(usize::from(u16::MAX)) as u16,
                area.height,
                true,
            )
            .last()
            .cloned()
            .unwrap_or_default()
    };
    let mut controls = if room > 0 {
        fitted(cache, room)
    } else {
        String::new()
    };
    if controls.is_empty() {
        // Not even the shortest control fits beside the phase: keep the
        // control and cut the phase.
        // Leave the indicator and a few cells of the phase.
        controls = fitted(cache, width.saturating_sub(separator_width + 4));
        let control_width = UnicodeWidthStr::width(controls.as_str());
        spans = truncate_spans(spans, width.saturating_sub(control_width + separator_width));
    }
    if !controls.is_empty() {
        spans.push(Span::styled(SEPARATOR, palette.muted));
        spans.extend(footer_line(1, 2, &controls, state, true, capabilities, palette).spans);
    }
    Line::from(spans)
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
    // The operating mode names itself by hue: Auto stays neutral, Read-only
    // takes the navigation blue and Plan the reasoning violet. A fresh change
    // still gets the short weight emphasis.
    if segment == mode_name(state.mode) {
        let hue = match state.mode {
            slim_core::OperatingMode::Auto => None,
            slim_core::OperatingMode::ReadOnly => Some(palette.accent),
            slim_core::OperatingMode::Plan => Some(palette.reasoning),
        };
        if let Some(hue) = hue {
            return if active {
                hue.add_modifier(Modifier::BOLD)
            } else {
                settled(
                    hue,
                    confirmed_setting_phase(segment, state, capabilities),
                    capabilities.color_depth,
                )
            };
        }
    }
    if lower.starts_with("execução ") {
        if let Some(execution) = &state.last_execution {
            let outcome_style = match execution.outcome {
                crate::app::RunOutcomeKind::Completed => palette.secondary,
                crate::app::RunOutcomeKind::Interrupted => palette.warning,
                crate::app::RunOutcomeKind::Failed => palette.error,
            };
            return settled(
                outcome_style,
                settle_phase(
                    Some(execution.ended_ms),
                    state.clock.elapsed_ms,
                    capabilities,
                ),
                capabilities.color_depth,
            );
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
    if let phase @ Some(_) = confirmed_setting_phase(segment, state, capabilities) {
        return settled(palette.secondary, phase, capabilities.color_depth);
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

/// Settle step of the footer segment that just confirmed a setting change.
fn confirmed_setting_phase(
    segment: &str,
    state: &AppState,
    capabilities: Capabilities,
) -> Option<u8> {
    let (setting, at_ms) = state.confirmed_setting.as_ref()?;
    let phase = settle_phase(Some(*at_ms), state.clock.elapsed_ms, capabilities)?;
    let segment = segment.to_ascii_lowercase();
    let matches = match setting {
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
    };
    matches.then_some(phase)
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
    let border_style = if kind.is_some() {
        palette.border_focus
    } else {
        palette.border
    };
    let block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(inspector_tabs(kind, area.width, palette));
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
    let last_run_phase = state.last_execution.as_ref().and_then(|execution| {
        settle_phase(
            Some(execution.ended_ms),
            state.clock.elapsed_ms,
            capabilities,
        )
    });
    if last_run_phase.is_some() {
        if let Some(line) = visible.iter_mut().find(|line| {
            line.spans
                .first()
                .is_some_and(|span| span.content.as_ref().trim_start().starts_with("Última"))
        }) {
            for span in &mut line.spans {
                span.style = settled(span.style, last_run_phase, capabilities.color_depth);
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
        InspectorKind::Diff => "Alterações",
        InspectorKind::Activity => "Atividade",
        InspectorKind::SessionTree => "Sessão",
        InspectorKind::Diagnostics => "Diagnósticos",
    }
}

/// Title of the Detalhes panel: every tab with the open one lit, and how to
/// move between them when it fits; a narrow panel names only the open tab
/// and its place. The four views are one panel, not four.
fn inspector_tabs(kind: Option<InspectorKind>, width: u16, palette: &Palette) -> Line<'static> {
    let Some(kind) = kind else {
        return Line::from(Span::styled(" Execução ", palette.accent_bold));
    };
    let budget = usize::from(width).saturating_sub(4);
    let labels = InspectorKind::TABS.map(inspector_title);
    let strip_width = labels
        .iter()
        .map(|label| UnicodeWidthStr::width(*label))
        .sum::<usize>()
        + 3 * (labels.len() - 1)
        + 2;
    const HINT: &str = " ←→ abas · Esc ";
    if strip_width <= budget {
        let mut spans = vec![Span::styled(" ", palette.muted)];
        for (index, tab) in InspectorKind::TABS.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", palette.muted));
            }
            spans.push(Span::styled(
                inspector_title(*tab),
                if *tab == kind {
                    palette.accent_bold
                } else {
                    palette.muted
                },
            ));
        }
        spans.push(Span::styled(
            if strip_width + UnicodeWidthStr::width(HINT) <= budget {
                HINT
            } else {
                " "
            },
            palette.muted,
        ));
        return Line::from(spans);
    }
    let position = InspectorKind::TABS
        .iter()
        .position(|tab| *tab == kind)
        .unwrap_or(0)
        + 1;
    let place = format!(" {position}/{} ←→ ", InspectorKind::TABS.len());
    let title = inspector_title(kind);
    let mut spans = vec![Span::styled(format!(" {title}"), palette.accent_bold)];
    if UnicodeWidthStr::width(title) + 1 + UnicodeWidthStr::width(place.as_str()) <= budget {
        spans.push(Span::styled(place, palette.muted));
    } else {
        spans.push(Span::styled(" ", palette.muted));
    }
    Line::from(spans)
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
            // The files first, with the lines each one gained and lost; the
            // operations that produced them follow.
            let changes = crate::receipt::file_changes(state.blocks());
            if !changes.is_empty() {
                lines.push(Line::from(Span::styled(" Arquivos", palette.secondary)));
                for change in &changes {
                    let stats = (change.added > 0 || change.removed > 0).then(|| {
                        format!(
                            "{}+{} -{}",
                            if change.exact { "" } else { "~" },
                            change.added,
                            change.removed
                        )
                    });
                    let stats_width = stats.as_ref().map_or(0, |text| text.len() + 2);
                    let path = crate::view_model::truncate_middle(
                        &sanitize_terminal_text(&change.path),
                        (width as usize).saturating_sub(4 + stats_width).max(4),
                    );
                    let mut spans = vec![
                        Span::styled(" ✓ ", palette.success),
                        Span::styled(path, palette.text),
                    ];
                    if stats.is_some() {
                        spans.push(Span::styled("  ", palette.muted));
                        if !change.exact {
                            spans.push(Span::styled("~", palette.muted));
                        }
                        spans.push(Span::styled(format!("+{}", change.added), palette.diff_add));
                        spans.push(Span::styled(" ", palette.muted));
                        spans.push(Span::styled(
                            format!("-{}", change.removed),
                            palette.diff_remove,
                        ));
                    }
                    lines.push(Line::from(spans));
                }
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(" Operações", palette.secondary)));
            }
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
                let receipt_text: String;
                let (label, text) = match block.kind() {
                    BlockKind::Receipt(receipt) => {
                        receipt_text = receipt.summary();
                        ("Recibo", receipt_text.as_str())
                    }
                    BlockKind::Work(work) => {
                        receipt_text = work.summary();
                        ("Trabalho", receipt_text.as_str())
                    }
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
/// Provider group names of the model picker, by group index.
fn model_group_title(index: usize) -> &'static str {
    match index {
        0 => "OpenAI Codex",
        1 => "OpenCode Go",
        2 => "ClinePass",
        3 => "Command Code",
        _ => "OpenCode Zen",
    }
}

fn model_group_of(provider: LoginProvider) -> usize {
    match provider {
        LoginProvider::OpenAiCodex | LoginProvider::Anthropic | LoginProvider::Xai => 0,
        LoginProvider::OpenCodeGo => 1,
        LoginProvider::ClinePass => 2,
        LoginProvider::CommandCode => 3,
        LoginProvider::OpenCodeZen => 4,
    }
}

/// Reasoning depth as a meter over the levels this model offers, then the
/// level's name: `▰▰▰▱ High`. The arrows in the footer move it. Bars, not
/// dots, so it never reads as the `●` that marks the active model.
fn effort_meter(
    levels: &[crate::api::ReasoningEffort],
    current: crate::api::ReasoningEffort,
    capabilities: Capabilities,
) -> String {
    let position = levels
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    let filled = glyph(capabilities, '\u{25b0}', '#');
    let empty = glyph(capabilities, '\u{25b1}', '-');
    let meter: String = (0..levels.len())
        .map(|index| if index <= position { filled } else { empty })
        .collect();
    format!("{meter} {}", current.label())
}

/// `/model`: provider groups as section headings with their model count,
/// models one step in under them, the focused model's effort as a meter, and
/// a detail pane set apart by a rule. Height stays fixed while filtering.
#[allow(clippy::too_many_arguments)]
fn render_model_overlay(
    frame: &mut ratatui::Frame,
    overlay: &ModelOverlay,
    active_model: &str,
    active_provider: Option<LoginProvider>,
    active_effort: crate::api::ReasoningEffort,
    opencode: &[crate::api::OpenCodeModelView],
    clinepass: &[crate::api::OpenCodeModelView],
    command_code: &[crate::api::OpenCodeModelView],
    zen: &[crate::api::OpenCodeModelView],
    rows: &[ModelRow],
    palette: &Palette,
    capabilities: Capabilities,
    cursor_focused: bool,
) {
    let frame_area = frame.area();
    let width = ((u32::from(frame_area.width) * 9) / 10) as u16;
    let width = width.clamp(28, 84).min(frame_area.width);
    // Keep the panel anchored while filtering or catalogs refresh. Only a
    // terminal resize changes its geometry; the list scrolls inside it.
    let height = frame_area.height.saturating_sub(2).clamp(1, 20);
    let area = centered(frame_area, width, height);
    let group_size = |group: usize| match group {
        0 => crate::api::ModelAlias::ALL.len(),
        1 => opencode.len(),
        2 => clinepass.len(),
        3 => command_code.len(),
        _ => zen.len(),
    };
    let inner_width = usize::from(area.width.saturating_sub(2));
    let inner_height = usize::from(area.height.saturating_sub(2));
    let detail_rows = match inner_height {
        11.. => 2,
        7..=10 => 1,
        _ => 0,
    };
    // Detail rows sit under a rule; the footer closes the panel.
    let reserved = detail_rows + usize::from(detail_rows > 0) + 1;
    let capacity = inner_height.saturating_sub(reserved).max(1);
    let window = visible_window(
        rows.len(),
        overlay.selected,
        capacity,
        overlay.viewport_start,
    );
    // Rows span the inner width less one cell of breathing room each side.
    let row_budget = inner_width.saturating_sub(1);
    let mut lines: Vec<Line> = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_cells(" Nenhum modelo corresponde ao filtro", inner_width),
            palette.muted,
        )));
    }
    for index in window {
        let row = &rows[index];
        let selected = index == overlay.selected;
        let marker = if selected { "> " } else { "  " };
        let style = focus_text(selected, palette);
        let spans = if let ModelRow::Header(group) = row {
            // `▾ OpenAI Codex ───── 4`: a heading, not a choice.
            let fold = if overlay.collapsed[*group] {
                glyph(capabilities, '\u{25b8}', '>')
            } else {
                glyph(capabilities, '\u{25be}', 'v')
            };
            let title = model_group_title(*group);
            let count = group_size(*group).to_string();
            let used = 2 + 2 + UnicodeWidthStr::width(title) + 1 + 1 + count.len();
            let rule = row_budget.saturating_sub(used);
            let heading = if selected { style } else { palette.secondary };
            let mut spans = vec![
                Span::styled(marker.to_owned(), style),
                Span::styled(format!("{fold} "), palette.muted),
                Span::styled(title.to_owned(), heading),
            ];
            if rule > 0 {
                spans.push(Span::styled(
                    format!(" {} ", "─".repeat(rule)),
                    palette.border,
                ));
                spans.push(Span::styled(count, palette.muted));
            }
            spans
        } else {
            let choice = row.choice(opencode, clinepass, command_code, zen);
            let active = choice.as_ref().is_some_and(|choice| {
                choice.id == active_model
                    && active_provider.is_none_or(|provider| choice.target.provider() == provider)
            });
            let name = choice
                .as_ref()
                .map_or_else(String::new, |choice| sanitize_terminal_text(&choice.name));
            // The right column says the one thing worth reading on this row:
            // the adjustable effort where the cursor is, the effort in use on
            // the active model, or the provider of a search result.
            let (right, right_style) = match &choice {
                Some(choice) if selected && !choice.levels.is_empty() => (
                    effort_meter(
                        &choice.levels,
                        overlay.pending_effort(choice, active_effort),
                        capabilities,
                    ),
                    palette.link,
                ),
                Some(choice) if active && !choice.levels.is_empty() => (
                    overlay
                        .pending_effort(choice, active_effort)
                        .label()
                        .to_owned(),
                    palette.muted,
                ),
                Some(choice) if !overlay.filter.is_empty() => (
                    model_group_title(model_group_of(choice.target.provider())).to_owned(),
                    palette.muted,
                ),
                _ => (String::new(), palette.muted),
            };
            let active_marker = if active {
                format!("{} ", glyph(capabilities, '\u{25cf}', '*'))
            } else {
                "  ".to_owned()
            };
            // Under a heading, models sit one step in.
            let indent = if overlay.filter.is_empty() { "  " } else { "" };
            let lead = 2 + indent.len() + 2;
            let right_width = UnicodeWidthStr::width(right.as_str());
            let name = truncate_cells(
                &name,
                row_budget.saturating_sub(lead + right_width + usize::from(right_width > 0) * 2),
            );
            let gap = row_budget
                .saturating_sub(lead + UnicodeWidthStr::width(name.as_str()) + right_width)
                .max(1);
            let mut spans = vec![
                Span::styled(marker.to_owned(), style),
                Span::raw(indent),
                Span::styled(active_marker, if active { palette.success } else { style }),
                Span::styled(name, style),
            ];
            if !right.is_empty() {
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(Span::styled(right, right_style));
            }
            spans
        };
        lines.push(menu_line(spans, selected, inner_width, palette));
    }
    while lines.len() < capacity {
        lines.push(Line::default());
    }
    if detail_rows > 0 {
        lines.push(Line::from(Span::styled(
            format!(" {}", "─".repeat(inner_width.saturating_sub(2))),
            palette.border,
        )));
        let focused_row = rows.get(overlay.selected);
        let focused =
            focused_row.and_then(|row| row.choice(opencode, clinepass, command_code, zen));
        let (title, facts): (Vec<Span>, String) = if overlay.selection_lost {
            (
                vec![Span::styled(
                    " Catálogo mudou · escolha um modelo novamente",
                    palette.warning,
                )],
                String::new(),
            )
        } else if let Some(choice) = &focused {
            let mut facts = Vec::new();
            if let Some(context) = choice.context_window_tokens {
                facts.push(format!(
                    "contexto {}",
                    crate::view_model::format_token_count(context)
                ));
            }
            if let Some(output) = choice.max_output_tokens {
                facts.push(format!(
                    "saída {}",
                    crate::view_model::format_token_count(output)
                ));
            }
            if choice.accepts_images == Some(true) {
                facts.push("imagens".to_owned());
            }
            if matches!(choice.target, crate::app::EffortTarget::Alias(_)) {
                facts.push(format!(
                    "velocidade {}",
                    if overlay.pending_fast {
                        "Rápida"
                    } else {
                        "Normal"
                    }
                ));
            }
            facts.push("só nesta sessão".to_owned());
            (
                vec![
                    Span::styled(
                        format!(" {}", sanitize_terminal_text(&choice.name)),
                        palette.text,
                    ),
                    Span::styled(
                        format!(
                            "  {} · {}",
                            sanitize_terminal_text(&choice.id),
                            model_group_title(model_group_of(choice.target.provider()))
                        ),
                        palette.muted,
                    ),
                ],
                facts.join(" · "),
            )
        } else if let Some(ModelRow::Header(group)) = focused_row {
            let count = group_size(*group);
            (
                vec![Span::styled(
                    format!(
                        " {} · {count} {}",
                        model_group_title(*group),
                        if count == 1 { "modelo" } else { "modelos" }
                    ),
                    palette.text,
                )],
                if overlay.collapsed[*group] {
                    "Enter expande o grupo".to_owned()
                } else {
                    "Enter recolhe o grupo".to_owned()
                },
            )
        } else {
            (Vec::new(), String::new())
        };
        lines.push(Line::from(truncate_spans(title, inner_width)));
        if detail_rows > 1 {
            lines.push(Line::from(Span::styled(
                truncate_cells(&format!(" {facts}"), inner_width),
                palette.muted,
            )));
        }
    }
    // The position counts models; headings are not choices.
    let models = rows
        .iter()
        .filter(|row| !matches!(row, ModelRow::Header(_)))
        .count();
    let position = match rows.get(overlay.selected) {
        Some(ModelRow::Header(_)) | None => format!("{models} modelos"),
        Some(_) => format!(
            "{}/{models}",
            rows[..=overlay.selected]
                .iter()
                .filter(|row| !matches!(row, ModelRow::Header(_)))
                .count()
        ),
    };
    let speed_hint = rows
        .get(overlay.selected)
        .is_some_and(|row| matches!(row, ModelRow::Alias(_)));
    let effort_hint = rows
        .get(overlay.selected)
        .and_then(|row| row.choice(opencode, clinepass, command_code, zen))
        .is_some_and(|choice| !choice.levels.is_empty());
    let hints: &[&str] = if !effort_hint {
        &[
            "↑↓ navegar · Enter aplicar · Esc fechar",
            "Enter aplicar · Esc fechar",
        ]
    } else if speed_hint {
        &[
            "↑↓ navegar · ←→ esforço · Tab velocidade · Enter aplicar · Esc fechar",
            "←→ esforço · Tab velocidade · Enter aplicar · Esc fechar",
            "←→ esforço · Tab velocidade · Enter aplicar",
            "Enter aplicar · Esc fechar",
        ]
    } else {
        &[
            "↑↓ navegar · ←→ esforço · Enter aplicar · Esc fechar",
            "←→ esforço · Enter aplicar · Esc fechar",
            "Enter aplicar · Esc fechar",
        ]
    };
    lines.push(modal_footer(&position, hints, inner_width, palette));
    let (block, cursor) = filter_modal_block("Modelo", &overlay.filter, area.width, palette);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if cursor_focused {
        set_filter_cursor(frame, area, cursor);
    }
}

/// `/resume` and `/rewind` lists: filter line, one row per session or turn,
/// a detail line for the focused row and a `n/total` footer (§15.8).
fn render_session_picker(
    frame: &mut ratatui::Frame,
    picker: &crate::session_picker::SessionPicker,
    palette: &Palette,
    cursor_focused: bool,
) {
    use crate::session_picker::{compact_size, relative_age, PickerKind, PickerRow};
    let frame_area = frame.area();
    let width = ((u32::from(frame_area.width) * 9) / 10) as u16;
    let width = width.clamp(28, 96).min(frame_area.width);
    // Sized by the unfiltered list so typing a filter never resizes the panel:
    // detail line, footer and borders around the rows (min 3).
    let listed = picker.counts().1.max(3);
    let height = u16::try_from(listed + 4)
        .unwrap_or(u16::MAX)
        .min(frame_area.height.saturating_sub(2).clamp(1, 20));
    let area = centered(frame_area, width, height);
    let inner_width = usize::from(area.width.saturating_sub(2));
    let inner_height = usize::from(area.height.saturating_sub(2));
    let rewind = picker.kind == PickerKind::Rewind;

    let mut lines: Vec<Line> = Vec::new();

    let rows = picker.rows();
    let (shown, total) = picker.counts();
    // Detail line and footer are fixed; the list gets the rest.
    // The rewind note is a promise about what does not come back, so it yields
    // last: it stays as long as the panel can also show one row.
    let detail_rows = usize::from(inner_height >= if rewind { 4 } else { 5 });
    let capacity = inner_height.saturating_sub(1 + detail_rows).max(1);
    let window =
        crate::picker::visible_window(rows.len(), picker.selected, capacity, picker.viewport_start);
    let placeholder = if picker.loading {
        Some((" Carregando…".to_owned(), palette.muted))
    } else if let Some(error) = &picker.error {
        Some((format!(" {}", sanitize_terminal_text(error)), palette.error))
    } else if rows.is_empty() {
        let text = match (picker.kind, total) {
            (_, 0) if rewind => " Nenhum turno concluído para voltar",
            (_, 0) => " Nenhuma sessão anterior neste diretório",
            _ => " Nada corresponde ao filtro",
        };
        Some((text.to_owned(), palette.muted))
    } else {
        None
    };
    let mut list_lines = 0usize;
    if let Some((text, style)) = placeholder {
        lines.push(Line::from(Span::styled(
            truncate_cells(&text, inner_width),
            style,
        )));
        list_lines += 1;
    } else {
        for position in window {
            let selected = position == picker.selected;
            let marker = if selected { "> " } else { "  " };
            let (label, label_style, meta, meta_style) = match rows[position] {
                PickerRow::Session(session) => {
                    let mut label = match (&session.title, session.first_prompt.is_empty()) {
                        (Some(title), false) => {
                            format!("{title} · {}", session.first_prompt)
                        }
                        (Some(title), true) => title.clone(),
                        (None, false) => session.first_prompt.clone(),
                        (None, true) => session.id.clone(),
                    };
                    label = sanitize_terminal_text(&label);
                    let mut meta = Vec::new();
                    if session.current {
                        meta.push("atual".to_owned());
                    } else if session.in_use {
                        meta.push("em uso".to_owned());
                    }
                    let age = relative_age(picker.now_ms, session.updated_ms);
                    if !age.is_empty() {
                        meta.push(age);
                    }
                    meta.push(compact_size(session.bytes));
                    let style = if session.current || session.in_use {
                        palette.muted
                    } else {
                        focus_text(selected, palette)
                    };
                    let meta_style = if session.in_use && !session.current {
                        palette.warning
                    } else {
                        palette.muted
                    };
                    (label, style, meta.join(" · "), meta_style)
                }
                PickerRow::Turn(turn) => {
                    let removed = picker.turns.len().saturating_sub(turn.index);
                    let label = format!(
                        "#{} {}",
                        turn.index + 1,
                        sanitize_terminal_text(&turn.prompt)
                    );
                    let style = focus_text(selected, palette);
                    (
                        label,
                        style,
                        format!(
                            "volta {removed} turno{}",
                            if removed == 1 { "" } else { "s" }
                        ),
                        palette.muted,
                    )
                }
            };
            let meta_width = UnicodeWidthStr::width(meta.as_str());
            let label_budget = inner_width.saturating_sub(2 + meta_width + 2).max(4);
            let label = truncate_cells(&label, label_budget);
            let used = 2 + UnicodeWidthStr::width(label.as_str());
            let gap = inner_width.saturating_sub(used + meta_width).max(1);
            let spans = vec![
                Span::styled(marker.to_owned(), label_style),
                Span::styled(label, label_style),
                Span::raw(" ".repeat(gap)),
                Span::styled(
                    meta,
                    if selected {
                        palette.secondary
                    } else {
                        meta_style
                    },
                ),
            ];
            lines.push(menu_line(spans, selected, inner_width, palette));
            list_lines += 1;
        }
    }
    while list_lines < capacity {
        lines.push(Line::default());
        list_lines += 1;
    }
    if detail_rows > 0 {
        let detail = match picker.selected_row() {
            Some(PickerRow::Session(session)) => format!(" {}", session.id),
            Some(PickerRow::Turn(_)) => {
                " Só a conversa volta; arquivos alterados ficam como estão".to_owned()
            }
            None if rewind => {
                " Só a conversa volta; arquivos alterados ficam como estão".to_owned()
            }
            None => String::new(),
        };
        lines.push(Line::from(Span::styled(
            truncate_cells(&detail, inner_width),
            palette.secondary,
        )));
    }
    let position = if shown == 0 {
        "0/0".to_owned()
    } else if shown == total {
        format!("{}/{shown}", picker.selected.saturating_add(1))
    } else {
        format!("{}/{shown} de {total}", picker.selected.saturating_add(1))
    };
    let action = if rewind { "voltar" } else { "retomar" };
    let long = format!("↑↓ navegar · Enter {action} · Esc fechar");
    let short = format!("Enter {action} · Esc fechar");
    lines.push(modal_footer(
        &position,
        &[long.as_str(), short.as_str()],
        inner_width,
        palette,
    ));
    let title = if rewind {
        "Voltar a um turno · nova sessão"
    } else {
        "Retomar sessão"
    };
    let (block, cursor) = filter_modal_block(title, &picker.filter, area.width, palette);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if cursor_focused {
        set_filter_cursor(frame, area, cursor);
    }
}

fn render_jobs_overlay(frame: &mut ratatui::Frame, state: &AppState, palette: &Palette) {
    let screen = frame.area();
    let width = screen.width.saturating_sub(2).clamp(1, 96);
    let max_height = screen.height.saturating_sub(2).max(1);
    let budget = width.saturating_sub(4).max(1) as usize;
    let footer = if width < 60 {
        "Enter saída · i/x · c ID · Esc"
    } else {
        "Enter saída · i interromper · x cancelar · c copiar ID · Esc voltar"
    };
    let footer_rows = wrap_words(footer, budget);
    let capacity = (max_height as usize)
        .saturating_sub(2)
        .saturating_sub(footer_rows.len() + 1)
        .max(1);
    let mut lines = Vec::<Line>::new();
    // The list and the confirmation take the rows they need; only a job's
    // output keeps the full height, since it scrolls.
    let mut fill = false;
    if state.job_exit_confirm.is_some() {
        let running = state.running_jobs();
        let consequence = if running == 1 {
            "1 job será encerrado".to_owned()
        } else {
            format!("{running} jobs serão encerrados")
        };
        lines.extend(
            wrap_words(
                &format!("{consequence}. Enter prossegue · Esc mantém a sessão"),
                budget,
            )
            .into_iter()
            .map(|s| Line::from(Span::styled(s, palette.warning))),
        );
    } else if let Some(overlay) = &state.jobs_overlay {
        if overlay.detail {
            fill = true;
            if let Some(job) = state.jobs.get(overlay.selected) {
                lines.push(Line::from(Span::styled(
                    format!("{} · {}", job.id, job_outcome_label(job)),
                    palette.secondary,
                )));
            }
            let rows = render_plain(&sanitize_terminal_text(&overlay.output), budget as u16);
            let start = overlay.scroll.start(rows.len(), capacity.saturating_sub(1));
            lines.extend(
                rows.into_iter()
                    .skip(start)
                    .take(capacity.saturating_sub(1))
                    .map(Line::from),
            );
            if overlay.output.is_empty() {
                lines.push(Line::from("Sem saída ainda"));
            }
        } else if state.jobs.is_empty() {
            lines.push(Line::from(Span::styled(
                "Nenhum job nesta sessão",
                palette.muted,
            )));
        } else {
            let start = overlay.selected.saturating_sub(capacity.saturating_sub(1));
            for (index, job) in state.jobs.iter().enumerate().skip(start).take(capacity) {
                let text = format!(
                    "{} {} · {} · {} · {}s · {}",
                    if index == overlay.selected { ">" } else { " " },
                    job.id,
                    job_outcome_label(job),
                    if job.origin == "user" {
                        "usuário"
                    } else {
                        "modelo"
                    },
                    job.elapsed_ms / 1000,
                    sanitize_terminal_text(&job.command)
                );
                lines.push(Line::from(Span::styled(
                    truncate_display_width(&text, budget),
                    if index == overlay.selected {
                        palette.secondary
                    } else {
                        palette.muted
                    },
                )));
            }
        }
        lines.push(Line::default());
        lines.extend(
            footer_rows
                .into_iter()
                .map(|s| Line::from(Span::styled(s, palette.muted))),
        );
    }
    let height = if fill {
        max_height
    } else {
        (lines.len() as u16).saturating_add(2).min(max_height)
    };
    let area = centered(screen, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Jobs da sessão ", palette)),
        area,
    );
}

/// State of a job as one phrase; the exit code joins it only when it says
/// something the state does not (a clean exit is already `concluído`).
fn job_outcome_label(job: &slim_core::runtime::ShellJobInfo) -> String {
    let label = job_state_label(&job.state);
    match job.exit_code {
        Some(code) if code != 0 => format!("{label} · exit {code}"),
        _ => label.to_owned(),
    }
}

fn job_state_label(state: &str) -> &str {
    match state {
        "running" => "rodando",
        "interrupting" => "interrompendo",
        "cancelling" => "cancelando",
        "completed" => "concluído",
        "failed" => "falhou",
        "cancelled" => "cancelado",
        "interrupted" => "interrompido",
        "lost" => "perdido",
        other => other,
    }
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
        let style = focus_text(selected, palette);
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
            if overlay.in_progress {
                " Esc cancelar · Ctrl+C cancelar"
            } else {
                " Enter salvar · Esc voltar"
            },
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
    let hint = if overlay.in_progress || status.is_some() {
        " Esc cancelar · Ctrl+C cancelar"
    } else {
        " Enter conectar · Esc fechar"
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
            let style = focus_text(selected_here, palette);
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
    let inner_width = usize::from(composer_area.width.saturating_sub(2));
    let commands = matches.iter().map(String::as_str).collect::<Vec<_>>();
    let rows = grouped_command_lines(
        &commands,
        Some(suggestions.selected),
        inner_width.saturating_sub(2),
        palette,
    );
    let selected_row = rows
        .iter()
        .position(|row| row.command_index == Some(suggestions.selected))
        .unwrap_or(0);
    let visible = visible_window(rows.len(), selected_row, capacity, 0);
    render_dropdown(
        frame,
        composer_area,
        rows[visible].iter().map(|row| row.line.clone()).collect(),
        SLASH_POPUP_HINT,
        inner_width,
        palette,
    );
}

/// Draws `rows` plus a key-hint row as a dropdown resting on the composer's
/// top border (shared by the `/` and `@` popups).
fn render_dropdown(
    frame: &mut ratatui::Frame,
    composer_area: ratatui::layout::Rect,
    mut rows: Vec<Line<'static>>,
    hint: &str,
    inner_width: usize,
    palette: &Palette,
) {
    // The hint aligns with the row labels, past the selection marker.
    rows.push(Line::from(Span::styled(
        format!("  {}", truncate_cells(hint, inner_width.saturating_sub(2))),
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

const MENTION_POPUP_HINT: &str = "Tab/Enter completar · Esc";

fn render_mention_popup(
    frame: &mut ratatui::Frame,
    composer_area: ratatui::layout::Rect,
    suggestions: &crate::app::MentionSuggestions,
    state: &AppState,
    palette: &Palette,
) {
    if composer_area.y < 3 || composer_area.width < 4 {
        return;
    }
    let capacity =
        usize::from(composer_area.y.saturating_sub(2).max(1)).min(PICKER_NOMINAL_CAPACITY);
    let inner_width = usize::from(composer_area.width.saturating_sub(2));
    let mut rows: Vec<Line<'static>> = Vec::new();
    if suggestions.matches.is_empty() {
        let text = if state.workspace_files_loaded {
            "Nenhum arquivo"
        } else {
            "Buscando arquivos…"
        };
        rows.push(Line::from(Span::styled(format!("  {text}"), palette.muted)));
    } else {
        let visible = visible_window(suggestions.matches.len(), suggestions.selected, capacity, 0);
        for position in visible {
            let Some(path) = state.workspace_files.get(suggestions.matches[position]) else {
                continue;
            };
            let selected = position == suggestions.selected;
            let (directory, name) = crate::mention::split_path(path);
            let marker = if selected { "> " } else { "  " };
            // The file name is the label; the directory yields first when the
            // row is too narrow, so the name is never the part that is cut.
            let budget = inner_width.saturating_sub(2);
            let name_cells = UnicodeWidthStr::width(name);
            let directory_budget = budget.saturating_sub(name_cells + 2);
            let directory = if directory_budget > 3 && !directory.is_empty() {
                truncate_cells(directory.trim_end_matches('/'), directory_budget)
            } else {
                String::new()
            };
            let name_style = focus_text(selected, palette);
            let mut spans = vec![
                Span::styled(marker.to_owned(), name_style),
                Span::styled(truncate_cells(name, budget), name_style),
            ];
            if !directory.is_empty() {
                spans.push(Span::styled(format!("  {directory}"), palette.muted));
            }
            rows.push(menu_line(spans, selected, inner_width, palette));
        }
    }
    render_dropdown(
        frame,
        composer_area,
        rows,
        MENTION_POPUP_HINT,
        inner_width,
        palette,
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
    let frame_area = frame.area();
    let width = (((u32::from(frame_area.width) * 4) / 5) as u16)
        .clamp(38, 76)
        .min(frame_area.width);
    let height = frame_area.height.saturating_sub(2).clamp(1, 18);
    let area = centered(frame_area, width, height);
    let inner = ratatui::layout::Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let inner_width = usize::from(inner.width);
    let row_width = usize::from(area.width.saturating_sub(4));
    let rows = grouped_command_lines(&matches, Some(selected), row_width, palette);
    // The viewport is measured in rendered rows, not command indices: group
    // headings consume the same vertical space as a command and therefore
    // must participate in keeping the focused command visible. The footer is
    // the only fixed row; the filter sits in the title.
    let capacity = usize::from(inner.height.saturating_sub(1)).max(1);
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
    let mut lines: Vec<Line> = rows[start..end]
        .iter()
        .map(|row| row.line.clone())
        .collect();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            " Nenhum comando encontrado",
            palette.muted,
        )));
    }
    while lines.len() < usize::from(inner.height.saturating_sub(1)) {
        lines.push(Line::default());
    }
    let position = if matches.is_empty() {
        "0/0".to_owned()
    } else {
        format!("{}/{}", selected.saturating_add(1), matches.len())
    };
    lines.push(modal_footer(
        &position,
        &[
            "↑↓ navegar · Enter executar · Esc fechar",
            "Enter executar · Esc fechar",
        ],
        inner_width,
        palette,
    ));
    let (block, cursor) = filter_modal_block("Comandos", query, area.width, palette);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if cursor_focused {
        set_filter_cursor(frame, area, cursor);
    }
}

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Same spinner for terminals told not to use color (`NO_COLOR`): ASCII, so it
/// survives fonts without braille, and half as fast because it has 4 frames.
const ASCII_SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];
/// Four frames of the braille spinner, one quarter turn apart, for states
/// that advance once per second (retry wait, cancellation).
const SLOW_SPINNER_FRAMES: [char; 4] = ['⠋', '⠹', '⠼', '⠧'];

/// Whether glyphs may animate. Color has nothing to do with it: `NO_COLOR`
/// removes hue, not movement; only the reduced-motion preference stops it.
fn spinner_glyph(frame: u64, capabilities: Capabilities) -> char {
    if capabilities.reduced_motion {
        glyph(capabilities, '\u{25cb}', '~')
    } else if capabilities.color_depth == ColorDepth::None {
        ASCII_SPINNER_FRAMES[(frame as usize / 2) % ASCII_SPINNER_FRAMES.len()]
    } else {
        SPINNER_FRAMES[(frame as usize) % SPINNER_FRAMES.len()]
    }
}

/// Label of a thought that is still streaming.
const THINKING_LABEL: &str = "Pensando";
/// Cells the sweep crosses (the label is ASCII).
const THINKING_LABEL_CELLS: usize = THINKING_LABEL.len();
/// Cells of the agent name in its header (`Slim`, ASCII).
const AGENT_NAME_CELLS: usize = 4;
/// Cells lit on each side of the sweep's peak, and the pause once it leaves.
const SHIMMER_HALO: usize = 2;
const SHIMMER_REST: usize = 2;
/// A burst of thought lights the edge of the newest row for this long,
/// fading out, and this many cells deep.
const GLOW_MS: u64 = 450;
const GLOW_CELLS: u16 = 14;

/// Strength (0.0 resting, 1.0 peak) of cell `index` in a label `width` cells
/// wide while a highlight sweeps across it at `elapsed_ms`. The peak crosses
/// a cell every two motion frames on short labels and every frame on long
/// ones, so a full pass takes about two seconds either way, then rests before
/// the next. Its position is continuous and its edge eases out, so the light
/// slides between frames instead of jumping a cell.
fn sweep_strength(index: usize, width: usize, elapsed_ms: u64) -> f32 {
    let ms_per_cell = if width <= 10 {
        2 * MOTION_INTERVAL_MS
    } else {
        MOTION_INTERVAL_MS
    };
    let travel = (width + 2 * SHIMMER_HALO + SHIMMER_REST) as u64;
    let peak = (elapsed_ms % (travel * ms_per_cell)) as f32 / ms_per_cell as f32;
    // The peak starts outside the label on the left and leaves on the right.
    let distance = ((index + SHIMMER_HALO) as f32 - peak).abs();
    let reach = 1.0 - distance / (SHIMMER_HALO + 1) as f32;
    let reach = reach.max(0.0);
    reach * reach * (3.0 - 2.0 * reach)
}

/// `text` as spans with the sweep applied; equal neighbors share a span.
fn shimmer_spans(text: &str, elapsed_ms: u64, palette: &Palette) -> Vec<Span<'static>> {
    let width = UnicodeWidthStr::width(text);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cell = 0usize;
    let mut run = String::new();
    let mut run_style = Style::default();
    for character in text.chars() {
        let style = palette.shimmer.at(sweep_strength(cell, width, elapsed_ms));
        if !run.is_empty() && style != run_style {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(character);
        cell += UnicodeWidthChar::width(character).unwrap_or(0);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    spans
}

/// How fresh the last content from the provider is, from 1.0 (just arrived)
/// to 0.0; `None` once it no longer glows.
fn glow_freshness(state: &AppState) -> Option<f32> {
    let arrived = state.last_provider_content_ms?;
    let age = state.clock.elapsed_ms.saturating_sub(arrived);
    (age < GLOW_MS).then(|| 1.0 - age as f32 / GLOW_MS as f32)
}

/// Lights the last cells of text on row `y`, strongest at the edge where the
/// text is being written and fading toward the start. Only styles change, so
/// nothing moves and nothing waits: the words are already on screen.
fn apply_frontier_glow(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    y: u16,
    ramp: &Ramp,
    freshness: f32,
    caret_visible: bool,
) {
    // Text starts in the fourth column of the transcript, after the gutter.
    let first = area.x.saturating_add(4);
    let last = area.right().saturating_sub(1);
    // The caret rides after the last word and keeps its own color.
    let Some(end) = (first..=last).rev().find(|x| {
        let symbol = buf[(*x, y)].symbol();
        !symbol.trim().is_empty() && !(caret_visible && symbol == "▌")
    }) else {
        return;
    };
    for depth in 0..GLOW_CELLS.min(end - first + 1) {
        let strength = (1.0 - f32::from(depth) / f32::from(GLOW_CELLS)) * freshness;
        // Below the first step the stepped light would only repaint the
        // words in their own color; the continuous one still rises.
        if ramp.ends.is_some() || strength >= 0.5 / 3.0 {
            buf[(end - depth, y)].set_style(ramp.at(strength));
        }
    }
}

/// One step per second, driven by the status tick instead of the motion
/// clock, so waiting states show life without arming a faster timer.
fn slow_spinner_glyph(elapsed_ms: u64, capabilities: Capabilities) -> char {
    let step = (elapsed_ms / 1_000) as usize % SLOW_SPINNER_FRAMES.len();
    if capabilities.color_depth == ColorDepth::None {
        ASCII_SPINNER_FRAMES[step]
    } else {
        SLOW_SPINNER_FRAMES[step]
    }
}

fn palette_command_line(
    command: &str,
    selected_here: bool,
    width: usize,
    palette: &Palette,
) -> Line<'static> {
    let marker = if selected_here { "> " } else { "  " };
    let style = focus_text(selected_here, palette);
    let detail = crate::reducer::palette_description(command);
    let head_width = if detail.is_empty() {
        width
    } else {
        (width / 3).clamp(13, 24).min(width)
    };
    let head = format!(
        "{marker}{}",
        truncate_cells(command, head_width.saturating_sub(2))
    );
    let hint = match command {
        "/model" => "Ctrl+L",
        "/mode" => "Shift+Tab",
        _ => "",
    };
    let hint_width = UnicodeWidthStr::width(hint);
    let description_gap = usize::from(!detail.is_empty() && head_width < width);
    let description_width = width.saturating_sub(head_width + description_gap + hint_width + 2);
    let description = truncate_cells(detail, description_width);
    let gap = width.saturating_sub(
        head_width + description_gap + UnicodeWidthStr::width(description.as_str()) + hint_width,
    );
    menu_line(
        vec![
            Span::styled(format!("{head:<head_width$}"), style),
            Span::raw(" ".repeat(description_gap)),
            Span::styled(description, palette.muted),
            Span::raw(" ".repeat(gap)),
            Span::styled(hint.to_owned(), palette.link),
        ],
        selected_here,
        width.saturating_add(2),
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
    width: usize,
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
            "conversation" => "conversa",
            "runtime" => "execução",
            "account" => "conta e integrações",
            "inspect" => "detalhes",
            other => other,
        };
        lines.push(palette_heading(name, width, palette));
        for (index, command) in visible {
            lines.push(PaletteRow {
                command_index: Some(index),
                line: palette_command_line(command, selected == Some(index), width, palette),
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
        lines.push(palette_heading("habilidades", width, palette));
        for (index, command) in ungrouped {
            lines.push(PaletteRow {
                command_index: Some(index),
                line: palette_command_line(command, selected == Some(index), width, palette),
            });
        }
    }
    lines
}

/// Group heading: a rule to the right edge of the command rows sets it apart
/// from its commands. Its name starts under the command names, past the marker.
fn palette_heading(name: &str, width: usize, palette: &Palette) -> PaletteRow {
    let rule = width.saturating_sub(UnicodeWidthStr::width(name) + 3);
    PaletteRow {
        command_index: None,
        line: Line::from(vec![
            Span::styled(format!("  {name}"), palette.secondary),
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
fn dim_backdrop(frame: &mut ratatui::Frame, backdrop: Style) {
    let area = frame.area();
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            if let Some(fg) = backdrop.fg {
                cell.set_fg(fg);
            }
            cell.modifier.remove(Modifier::BOLD);
            cell.modifier.insert(backdrop.add_modifier);
        }
    }
}

/// Text of a row in a centered modal: the focused row is bold in the normal
/// text color (marker and `menu_line` fill carry the rest); blue stays on
/// focus borders, links and shortcut hints.
fn focus_text(selected: bool, palette: &Palette) -> Style {
    if selected {
        palette.text.add_modifier(Modifier::BOLD)
    } else {
        palette.text
    }
}

/// Footer row of a filterable modal: `position` then the longest of `hints`
/// that fits, falling back to the bare close hint.
fn modal_footer(
    position: &str,
    hints: &[&str],
    inner_width: usize,
    palette: &Palette,
) -> Line<'static> {
    let hint = hints
        .iter()
        .copied()
        .find(|hint| {
            UnicodeWidthStr::width(*hint) + UnicodeWidthStr::width(position) + 4 <= inner_width
        })
        .unwrap_or("Esc fechar");
    Line::from(Span::styled(
        truncate_cells(&format!(" {position} · {hint}"), inner_width),
        palette.muted,
    ))
}

fn modal_block<'a>(title: &'a str, palette: &'a Palette) -> RatatuiBlock<'a> {
    RatatuiBlock::default()
        .title(title)
        .title_style(palette.heading)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(palette.border)
        .style(palette.surface_alt)
}

/// Border of a filterable list. The filter lives in the title
/// (` Comandos · api `), so it spends no row of its own; empty, it invites
/// typing. Returns the block and the caret column from the panel's left edge.
fn filter_modal_block(
    title: &str,
    filter: &str,
    panel_width: u16,
    palette: &Palette,
) -> (RatatuiBlock<'static>, usize) {
    let head = format!(" {title} · ");
    let head_width = UnicodeWidthStr::width(head.as_str());
    // Corners and one trailing space stay clear of the title.
    let budget = usize::from(panel_width)
        .saturating_sub(3)
        .saturating_sub(head_width);
    let safe = sanitize_terminal_text(filter);
    let (value, value_style, value_width) = if safe.is_empty() {
        (
            crate::view_model::truncate_display_width("digite para filtrar", budget),
            palette.muted,
            0,
        )
    } else {
        let value = truncate_search_query(&safe, budget.saturating_sub(1));
        let width = UnicodeWidthStr::width(value.as_str());
        (value, palette.text, width)
    };
    let title = Line::from(vec![
        Span::styled(format!(" {title}"), palette.heading),
        Span::styled(" · ", palette.muted),
        Span::styled(value, value_style),
        Span::styled(" ", palette.muted),
    ]);
    let block = RatatuiBlock::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(palette.border)
        .style(palette.surface_alt);
    (block, 1 + head_width + value_width)
}

/// Caret on the title row of a [`filter_modal_block`].
fn set_filter_cursor(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, column: usize) {
    set_cursor_in_rect(
        frame,
        ratatui::layout::Rect {
            height: area.height.min(1),
            ..area
        },
        column,
        0,
    );
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

mod mcp_view;

#[cfg(test)]
mod tests;
