use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::api::{BlockId, LoginProvider, ModelAlias, ReasoningEffort, UiCommand, UiEvent};
use crate::app::{
    AppState, EffortOverlay, FollowMode, FrameClock, LoginOverlay, LoginStage, ModelOverlay,
    ModelRow, NotificationPriority, ScrollAnchor,
};
use crate::block::{BlockKind, InteractionRequestKind};
use crate::composer::ComposerError;
use crate::input::{classify_enter, cycle_mode, normalize, EnterIntent};
use crate::inspector::{search_match_indices_filtered, InspectorKind, SearchState};
use crate::picker::{ensure_visible_start, move_selection, PICKER_NOMINAL_CAPACITY};
use crate::render::ScrollMetrics;

const MAX_QUEUED_PROMPTS: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    UiEventReceived(UiEvent),
    Key(KeyEvent),
    Paste(String),
    /// Composer paste from the system clipboard: the image attachment and the
    /// clipboard text are read together so the reducer keeps precedence (§20)
    /// and the modal gates in a single place.
    ClipboardPull {
        image: Option<String>,
        text: Option<String>,
    },
    /// A clipboard image was detected but could not be materialized.
    ClipboardImageFailed {
        message: String,
    },
    Resize,
    ToggleTodoDock,
    ToggleBlock(BlockId),
    Scroll {
        intent: ScrollIntent,
        metrics: ScrollMetrics,
    },
    InspectorScroll {
        intent: ScrollIntent,
        total_rows: usize,
        capacity: usize,
    },
    /// Scrolls the content of a pending approval/question before a decision
    /// key is accepted. Runtime supplies the painted row geometry.
    ScrollApproval {
        intent: ScrollIntent,
        total_rows: usize,
        capacity: usize,
    },
    /// Updates the reducer's decision gate from the current painted viewport.
    /// The runtime computes this boolean; the reducer only records it.
    SetApprovalContentAccessible(bool),
    /// Clears a mouse selection after runtime verifies that its painted
    /// geometry no longer maps to the frozen text snapshot.
    ClearScreenSelection,
    /// Synchronizes event timestamps without scheduling a frame.
    SyncClock(FrameClock),
    /// Motion clock (§10.3): the loop sends ticks only while animating.
    Tick(FrameClock),
    /// One-second semantic timer for elapsed labels under reduced motion.
    StatusTick(FrameClock),
    ClipboardCompleted {
        success: bool,
    },
    StartScreenSelection {
        x: u16,
        y: u16,
        area: Option<ratatui::layout::Rect>,
    },
    /// Starts a transcript selection while pinning the viewport to the row
    /// anchor captured by the runtime painter.
    StartPinnedScreenSelection {
        x: u16,
        y: u16,
        area: Option<ratatui::layout::Rect>,
        anchor: ScrollAnchor,
    },
    UpdateScreenSelection {
        x: u16,
        y: u16,
    },
    FinishScreenSelection,
    MouseSecondary,
    RequestClipboardPaste,
    RequestShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollIntent {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    LiveEdge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    Send(UiCommand),
    CopyToClipboard(String),
    /// Pulls the system clipboard once: image and/or text (§20). The runtime
    /// only performs the IO; `Action::ClipboardPull` keeps the decision.
    PasteFromClipboard,
    RequestRender,
}

fn clear_screen_selection(state: &mut AppState) {
    state.selection = None;
    state.selection_area = None;
    state.selection_text.clear();
}

/// Single mutation route (DESIGN-SLIM-TUI §4.1/§9.2): every state change flows
/// through here; the runtime only executes the returned effects.
pub fn reduce(state: &mut AppState, action: Action) -> Vec<Effect> {
    match action {
        Action::UiEventReceived(event) => {
            // G244 (§7.4): a provider terminal or locally handled prompt is a
            // queue boundary. After applying it, drain exactly one queued
            // prompt, FIFO, into SendPrompt.
            let local_skill_selection = matches!(
                &event,
                UiEvent::RestoreDraft { text } if selected_skill_draft(state, text)
            );
            let prompt_boundary = local_skill_selection
                || matches!(
                    &event,
                    UiEvent::RunCompleted { .. }
                        | UiEvent::RunStopped { .. }
                        | UiEvent::RunCancelled { .. }
                        | UiEvent::RunFailed { .. }
                );
            let skills_changed = matches!(
                &event,
                UiEvent::WorkspaceChanged { .. }
                    | UiEvent::SessionSnapshot { .. }
                    | UiEvent::SessionRestored { .. }
            );
            state.apply_event(event);
            if skills_changed {
                sync_slash_suggestions(state);
            }
            let mut effects = vec![Effect::RequestRender];
            if prompt_boundary && !state.working && !state.queue_paused {
                if let Some(prompt) = state.pop_queued_prompt() {
                    effects.push(Effect::Send(UiCommand::SendPrompt(prompt)));
                }
            }
            effects
        }
        Action::Key(key) => {
            if key.code == KeyCode::Esc && state.selection.is_some() {
                clear_screen_selection(state);
                return vec![Effect::RequestRender];
            }
            if !is_ctrl_c(&key) {
                clear_screen_selection(state);
            }
            reduce_key(state, key)
        }
        Action::Paste(payload) => {
            // G250: paste never edits the composer underneath a stacked modal
            // (model/effort/mcp overlays do not accept pulls). Search and the
            // palette are also capturing input surfaces: a pull reaching the
            // composer while they own the keyboard edits a hidden draft.
            if state.model_overlay.is_some()
                || state.effort_overlay.is_some()
                || state.mcp_overlay.is_some()
                || state.search.is_some()
                || state.palette_query.is_some()
            {
                return vec![Effect::RequestRender];
            }
            if let Some(LoginStage::ApiKey(api_key)) = state
                .login_overlay
                .as_mut()
                .map(|overlay| &mut overlay.stage)
            {
                if !api_key.push_str_bounded(&payload, 4_096) {
                    state.push_notification_with_priority(
                        "Chave de API grande demais (limite de 4096 caracteres).".into(),
                        NotificationPriority::Warning,
                    );
                }
                state.revisions.status += 1;
                return vec![Effect::RequestRender];
            }
            if paste_blocked(state) {
                return vec![Effect::RequestRender];
            }
            match state.composer.try_paste(payload) {
                Ok(_) => {
                    state.revisions.content += 1;
                    sync_slash_suggestions(state);
                }
                Err(ComposerError::DraftTooLarge) => {
                    state.push_notification_with_priority(
                        "Rascunho grande demais (limite de 1 MiB).".into(),
                        NotificationPriority::Warning,
                    );
                    state.revisions.status += 1;
                }
            }
            vec![Effect::RequestRender]
        }
        Action::ClipboardPull { image, text } => {
            // The login API-key field is a text target: an image on the
            // clipboard must not steal its paste. Model/effort overlays and a
            // pending interaction keep the historical no-op.
            let modal = state.login_overlay.is_some()
                || state.model_overlay.is_some()
                || state.effort_overlay.is_some()
                || state.mcp_overlay.is_some()
                || state.search.is_some()
                || state.palette_query.is_some()
                || paste_blocked(state);
            if let (Some(path), false) = (image, modal) {
                return vec![
                    Effect::Send(UiCommand::AttachImage(path)),
                    Effect::RequestRender,
                ];
            }
            match text.filter(|text| !text.is_empty()) {
                Some(text) => reduce(state, Action::Paste(text)),
                None => vec![Effect::RequestRender],
            }
        }
        Action::ClipboardImageFailed { message } => {
            state.push_notification_with_priority(message, NotificationPriority::Error);
            state.revisions.status += 1;
            vec![Effect::RequestRender]
        }
        Action::Resize => {
            clear_screen_selection(state);
            vec![Effect::RequestRender]
        }
        Action::ToggleTodoDock => {
            state.todo_dock_open = !state.todo_dock_open;
            state.todo_dock_user_preference = Some(state.todo_dock_open);
            state.revisions.status += 1;
            vec![Effect::RequestRender]
        }
        Action::ToggleBlock(id) => {
            clear_screen_selection(state);
            let (changed, command) = state.activate_block(&id);
            let mut effects = command.into_iter().map(Effect::Send).collect::<Vec<_>>();
            if changed {
                effects.push(Effect::RequestRender);
            }
            effects
        }
        Action::Scroll { intent, metrics } => {
            clear_screen_selection(state);
            reduce_scroll(state, intent, &metrics);
            vec![Effect::RequestRender]
        }
        Action::InspectorScroll {
            intent,
            total_rows,
            capacity,
        } => {
            clear_screen_selection(state);
            reduce_inspector_scroll(state, intent, total_rows, capacity)
        }
        Action::ScrollApproval {
            intent,
            total_rows,
            capacity,
        } => {
            clear_screen_selection(state);
            reduce_approval_scroll(state, intent, total_rows, capacity)
        }
        Action::SetApprovalContentAccessible(accessible) => {
            if state.approval_content_accessible != accessible {
                state.approval_content_accessible = accessible;
                state.revisions.status += 1;
            }
            vec![Effect::RequestRender]
        }
        Action::ClearScreenSelection => {
            clear_screen_selection(state);
            vec![Effect::RequestRender]
        }
        Action::SyncClock(clock) => {
            state.clock = clock;
            state.prune_notifications();
            Vec::new()
        }
        Action::Tick(clock) | Action::StatusTick(clock) => {
            state.clock = clock;
            state.prune_notifications();
            vec![Effect::RequestRender]
        }
        Action::ClipboardCompleted { success } => {
            if success {
                // Repeated copies refresh one quiet confirmation, not a stack.
                state
                    .notifications
                    .retain(|notice| notice.message != "Copiado");
            }
            if success {
                state.push_notification("Copiado".into());
            } else {
                state.push_notification_with_priority(
                    "Área de transferência indisponível".into(),
                    NotificationPriority::Error,
                );
            }
            state.revisions.status += 1;
            vec![Effect::RequestRender]
        }
        Action::StartScreenSelection { x, y, area } => {
            let pos = crate::selection::ScreenPos::new(x, y);
            state.selection = area.map(|_| crate::selection::ScreenSelection {
                anchor: pos,
                head: pos,
            });
            state.selection_area = area;
            state.selection_text.clear();
            vec![Effect::RequestRender]
        }
        Action::StartPinnedScreenSelection { x, y, area, anchor } => {
            state.scroll.mode = FollowMode::Pinned(anchor);
            state.revisions.viewport += 1;
            reduce(state, Action::StartScreenSelection { x, y, area })
        }
        Action::UpdateScreenSelection { x, y } => {
            if let Some(selection) = &mut state.selection {
                selection.head = crate::selection::ScreenPos::new(x, y);
            }
            vec![Effect::RequestRender]
        }
        Action::FinishScreenSelection => {
            if state
                .selection
                .is_some_and(|selection| selection.is_empty())
            {
                state.selection = None;
                state.selection_area = None;
                state.selection_text.clear();
            }
            vec![Effect::RequestRender]
        }
        Action::MouseSecondary => {
            if crate::selection::has_copyable_text(&state.selection_text) {
                vec![
                    Effect::CopyToClipboard(state.selection_text.clone()),
                    Effect::RequestRender,
                ]
            } else if state.selection.is_some() {
                // An empty/invalidated drag is not a request to paste.
                vec![Effect::RequestRender]
            } else {
                vec![Effect::PasteFromClipboard]
            }
        }
        Action::RequestClipboardPaste => vec![Effect::PasteFromClipboard],
        Action::RequestShutdown => {
            state.shutdown = true;
            vec![Effect::Send(UiCommand::Shutdown), Effect::RequestRender]
        }
    }
}

const PALETTE_COMMANDS: [&str; 14] = [
    "/help",
    "/login",
    "/logout",
    "/resume",
    "/queue",
    "/model",
    "/mode",
    "/compact",
    "/image",
    "/mcp",
    "/diff",
    "/activity",
    "/session",
    "/diagnostics",
];

fn is_ctrl_c(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{3}'))
        || (matches!(key.code, KeyCode::Char('c' | 'C'))
            && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn is_ctrl_v(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('v' | 'V')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_shift_insert(key: &KeyEvent) -> bool {
    key.code == KeyCode::Insert && key.modifiers.contains(KeyModifiers::SHIFT)
}

/// Visual groups for Ctrl+P and the slash popup. Selection still walks
/// commands only; headers are presentation.
pub const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("help", &["/help"]),
    (
        "session",
        &["/login", "/logout", "/resume", "/queue", "/compact"],
    ),
    ("runtime", &["/model", "/mode", "/image"]),
    ("integrations", &["/mcp"]),
    (
        "inspect",
        &["/diff", "/activity", "/session", "/diagnostics"],
    ),
];

/// Commands matching the palette query (G243): the query may or may not carry
/// the leading `/`; matching is a substring on the bare command name, so the
/// palette behaves as a search (`odel` finds `/model`). Slash completion keeps
/// prefix matching; the model overlay filter is likewise substring-based.
pub fn palette_matches(query: &str) -> Vec<&'static str> {
    let needle = query.trim_start_matches('/');
    PALETTE_COMMANDS
        .iter()
        .copied()
        .filter(|command| command.trim_start_matches('/').contains(needle))
        .collect()
}

/// One-line purpose per palette command, shown muted next to the name so the
/// palette stays discoverable without reading docs. Vocabulary mirrors the
/// footer (`signed out`), the login modal (`Connect provider`) and the model
/// overlay (`Select model`).
pub fn palette_description(command: &str) -> &'static str {
    match command {
        "/help" => "atalhos e comandos",
        "/login" => "conectar provedor",
        "/logout" => "sair",
        "/resume" => "retomar sessão",
        "/queue" => "gerenciar prompts pendentes",
        "/model" => "selecionar modelo",
        "/mode" => "alternar modo",
        "/compact" => "resumir contexto",
        "/image" => "anexar imagem",
        "/mcp" => "servidores MCP",
        "/diff" => "ver alterações",
        "/activity" => "ver atividade",
        "/session" => "árvore da sessão",
        "/diagnostics" => "tempos do provedor",
        _ => "",
    }
}

/// Commands matching the token under edit (W7): prefix match on the text
/// after the `/`. Empty query lists every command.
pub fn slash_matches(query: &str) -> Vec<&'static str> {
    PALETTE_COMMANDS
        .iter()
        .filter(|command| command.trim_start_matches('/').starts_with(query))
        .copied()
        .collect()
}

pub fn is_native_slash_command(name: &str) -> bool {
    name == "models"
        || PALETTE_COMMANDS
            .iter()
            .any(|command| command.trim_start_matches('/') == name)
}

pub(crate) fn slash_matches_with_skills(state: &AppState, query: &str) -> Vec<String> {
    let mut matches = slash_matches(query)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for skill in state.skill_names() {
        if is_native_slash_command(skill) {
            continue;
        }
        let command = format!("/{skill}");
        if command.trim_start_matches('/').starts_with(query) && !matches.contains(&command) {
            matches.push(command);
        }
    }
    matches
}

fn selected_skill_draft(state: &AppState, text: &str) -> bool {
    let trimmed = text.trim();
    let Some(name) = trimmed.strip_prefix('/') else {
        return false;
    };
    !name.contains(char::is_whitespace)
        && !is_native_slash_command(name)
        && state.skill_names().iter().any(|skill| skill == name)
}

/// The `/`-token under the cursor (whitespace-delimited span containing the
/// cursor), so the popup works whether the slash ends the draft or sits
/// mid-sentence. Returns the char span plus the text after `/`.
fn slash_token_span(composer: &crate::composer::Composer) -> Option<(usize, usize, String)> {
    let cursor = composer.cursor();
    if cursor > composer.char_count() {
        return None;
    }
    // Single pass over the payload: the whitespace-delimited token holding
    // the cursor, its first char, and the text after `/`. The previous
    // four-iterator version traversed the whole draft several times per
    // keystroke — costly once a paste chip grows the draft toward 1 MiB.
    let mut start = 0usize;
    let mut first = None;
    let mut end = None;
    let mut query = String::new();
    for (index, character) in composer.payload_chars().enumerate() {
        if index >= cursor {
            if character.is_whitespace() {
                end = Some(index);
                break;
            }
        } else if character.is_whitespace() {
            start = index + 1;
            first = None;
            query.clear();
            continue;
        }
        if index == start {
            first = Some(character);
        } else if index > start {
            query.push(character);
        }
    }
    let end = end.unwrap_or_else(|| composer.char_count());
    (first == Some('/')).then_some((start, end, query))
}

fn replace_slash_token(state: &mut AppState, command: &str) {
    let payload = state.composer.payload();
    let (start, end) = slash_token_span(&state.composer).map_or_else(
        || {
            let cursor = state.composer.cursor().min(payload.chars().count());
            (cursor, cursor)
        },
        |(start, end, _)| (start, end),
    );
    // Char indices → byte offsets without materializing a Vec<char>.
    let byte_at = |index: usize| {
        payload
            .char_indices()
            .nth(index)
            .map_or(payload.len(), |(byte, _)| byte)
    };
    let mut rebuilt = String::with_capacity(payload.len() + command.len() + 1);
    rebuilt.push_str(&payload[..byte_at(start)]);
    rebuilt.push_str(command);
    let tail = &payload[byte_at(end)..];
    // At end of draft the trailing space separates the completed command from
    // the next word; mid-sentence an existing space is reused, never doubled.
    if tail.is_empty() || !tail.starts_with(char::is_whitespace) {
        rebuilt.push(' ');
    }
    rebuilt.push_str(tail);
    state.composer.clear();
    state.composer.insert_text(rebuilt);
    // Completion types at the end of the token: park the cursor right after
    // the inserted command (plus its separator) instead of the draft end.
    let target = start + command.chars().count() + 1;
    while state.composer.cursor() > target {
        if !state.composer.move_left() {
            break;
        }
    }
}

fn sync_slash_suggestions(state: &mut AppState) {
    if state.pending_interaction().is_some() {
        state.slash_suggestions = None;
        return;
    }
    state.slash_suggestions = slash_token_span(&state.composer).and_then(|(_, _, query)| {
        let commands = slash_matches_with_skills(state, &query);
        (!commands.is_empty()).then_some(crate::app::SlashSuggestions { query, selected: 0 })
    });
}

/// Slash autocomplete key handling (W7): arrows navigate, Tab completes into
/// the draft, Enter completes and executes, Esc dismisses. Any other key
/// falls through to normal composer editing (which re-syncs the popup).
fn reduce_slash_key(state: &mut AppState, key: KeyEvent) -> Option<Vec<Effect>> {
    let suggestions = state.slash_suggestions.as_ref()?;
    let selected = suggestions.selected;
    let matches = slash_matches_with_skills(state, &suggestions.query);
    let effects = match key.code {
        KeyCode::Up => {
            state.slash_suggestions.as_mut()?.selected =
                move_selection(selected, matches.len(), -1);
            vec![Effect::RequestRender]
        }
        KeyCode::Down => {
            state.slash_suggestions.as_mut()?.selected = move_selection(selected, matches.len(), 1);
            vec![Effect::RequestRender]
        }
        KeyCode::Home => {
            state.slash_suggestions.as_mut()?.selected = 0;
            vec![Effect::RequestRender]
        }
        KeyCode::End => {
            state.slash_suggestions.as_mut()?.selected = matches.len().saturating_sub(1);
            vec![Effect::RequestRender]
        }
        KeyCode::Tab => {
            if let Some(command) = matches.get(selected) {
                replace_slash_token(state, command);
                state.slash_suggestions = None;
                state.revisions.content += 1;
            }
            vec![Effect::RequestRender]
        }
        KeyCode::Esc => {
            state.slash_suggestions = None;
            vec![Effect::RequestRender]
        }
        KeyCode::Enter => {
            let selected_is_skill = matches
                .get(selected)
                .is_some_and(|command| !is_native_slash_command(command.trim_start_matches('/')));
            if let Some(command) = matches.get(selected) {
                replace_slash_token(state, command);
            }
            state.slash_suggestions = None;
            if state.working && selected_is_skill {
                return Some(enqueue_queued(state));
            }
            if state.working && state.composer.payload().trim() == "/resume" {
                state.push_notification(
                    "Há uma execução ativa; aguarde ou cancele antes de retomar".into(),
                );
                state.revisions.status += 1;
                return Some(vec![Effect::RequestRender]);
            }
            submit_composer(state)
        }
        _ => return None,
    };
    Some(effects)
}

/// G244 (§7.4): while a run is active, Submit/Steer enqueue the draft as a
/// visible `QueuedUser` block (FIFO) instead of dropping it (core has no
/// mid-run steer). Drained one-prompt-per-run by `UiEventReceived`.
fn enqueue_queued(state: &mut AppState) -> Vec<Effect> {
    let prompt = state.composer.payload();
    if prompt.trim().is_empty() {
        return vec![];
    }
    if state.queued_prompts.len() >= MAX_QUEUED_PROMPTS {
        state.push_notification_with_priority(
            format!("Fila de prompts cheia ({MAX_QUEUED_PROMPTS})."),
            NotificationPriority::Warning,
        );
        return vec![Effect::RequestRender];
    }
    state.composer.clear();
    state.slash_suggestions = None;
    state.enqueue_queued_prompt(prompt);
    vec![Effect::RequestRender]
}

/// G220: a pending approval or structured question owns the keyboard, so paste
/// never edits the composer underneath it.
fn paste_blocked(state: &AppState) -> bool {
    state.pending_interaction().is_some_and(|interaction| {
        interaction.response_pending
            || matches!(&interaction.kind, InteractionRequestKind::Approval { .. })
            || matches!(
                &interaction.kind,
                InteractionRequestKind::Question { options, .. }
                    if !options.is_empty() && !interaction.custom_question_answer
            )
    })
}

fn reduce_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    // G250: Ctrl+P must not open the palette over a stacked modal (login,
    // effort, model) — those gates dispatch first, so add the guard here.
    // A pending interaction owns the keyboard (G220/§16.4): palette, search,
    // inspector and overlays must not open on top of it.
    let interaction_pending = state.pending_interaction().is_some();
    let overlays_open = state.login_overlay.is_some()
        || state.effort_overlay.is_some()
        || state.model_overlay.is_some()
        || state.mcp_overlay.is_some()
        || interaction_pending;
    if is_ctrl_c(&key) && crate::selection::has_copyable_text(&state.selection_text) {
        return vec![
            Effect::CopyToClipboard(state.selection_text.clone()),
            Effect::RequestRender,
        ];
    }
    if is_ctrl_c(&key) && state.selection.is_some() {
        return vec![Effect::RequestRender];
    }
    if is_ctrl_v(&key) || is_shift_insert(&key) {
        return vec![Effect::PasteFromClipboard];
    }
    if is_ctrl_c(&key) {
        if state.login_overlay.is_some() {
            return reduce_login_key(state, key);
        }
        if state.working {
            state.request_cancel_active_run();
            return vec![Effect::Send(UiCommand::CancelRun), Effect::RequestRender];
        }
        if state.composer.payload().is_empty() {
            return reduce(state, Action::RequestShutdown);
        }
        state.composer.clear();
        state.slash_suggestions = None;
        state.revisions.content += 1;
        return vec![Effect::RequestRender];
    }
    // Command palette (§15.7-style overlay): Ctrl+P or F1 toggles; typing filters;
    // Enter submits the top match as a composer submission.
    if ((key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::F(1) && key.modifiers == KeyModifiers::NONE))
        && !overlays_open
    {
        state.palette_query = match state.palette_query.take() {
            Some(_) => None,
            None => {
                state.palette_selected = 0;
                state.palette_viewport_start = 0;
                Some(String::new())
            }
        };
        state.revisions.focus += 1;
        return vec![Effect::RequestRender];
    }
    if state.palette_query.is_some() {
        return reduce_palette_key(state, key);
    }
    if key.code == KeyCode::Char('f')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !overlays_open
    {
        state.search = state.search.take().is_none().then(SearchState::default);
        state.inspector.active = None;
        state.slash_suggestions = None;
        state.revisions.focus += 1;
        return vec![Effect::RequestRender];
    }
    if state.search.is_some() {
        return reduce_search_key(state, key);
    }
    if state.slash_suggestions.is_some() {
        if let Some(effects) = reduce_slash_key(state, key) {
            return effects;
        }
    }
    if state.login_overlay.is_some() {
        return reduce_login_key(state, key);
    }
    // Effort sits on top of the model overlay (G239): keeping the parent
    // alive lets Esc return with selection, filter and folds intact.
    if state.effort_overlay.is_some() {
        return reduce_effort_key(state, key);
    }
    if state.model_overlay.is_some() {
        return reduce_model_key(state, key);
    }
    if state.mcp_overlay.is_some() {
        return reduce_mcp_key(state, key);
    }
    if state.inspector.active.is_some() {
        if let Some(effects) = reduce_inspector_key(state, key) {
            return effects;
        }
    }
    let inspector = if !interaction_pending && key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => Some(InspectorKind::Diff),
            KeyCode::Char('j') => Some(InspectorKind::Activity),
            KeyCode::Char('r') => Some(InspectorKind::SessionTree),
            KeyCode::Char('g') => Some(InspectorKind::Diagnostics),
            _ => None,
        }
    } else {
        None
    };
    if let Some(kind) = inspector {
        state.search = None;
        state.inspector.toggle(kind);
        state.revisions.focus += 1;
        return vec![Effect::RequestRender];
    }
    if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let selected = state.selected_block_id().and_then(|id| {
            state
                .blocks()
                .iter()
                .find(|block| &block.id == id)
                .and_then(copyable_block_text)
        });
        let text = selected.or_else(|| {
            state
                .blocks()
                .iter()
                .rev()
                .find_map(|block| match block.kind() {
                    BlockKind::Assistant(text) => Some(text.clone()),
                    _ => None,
                })
        });
        return text.map_or_else(
            || {
                state.push_notification("Nada para copiar".into());
                state.revisions.status += 1;
                vec![Effect::RequestRender]
            },
            |text| vec![Effect::CopyToClipboard(text), Effect::RequestRender],
        );
    }
    if key.code == KeyCode::Esc && state.inspector.active.take().is_some() {
        state.revisions.focus += 1;
        return vec![Effect::RequestRender];
    }
    // G250: spec §17.2 bindings that were missing. Ctrl+L opens the model
    // overlay (same as /model); Ctrl+T toggles the Todo dock. Both sit after
    // the overlay gates, so they never fire beneath a stacked modal.
    if key.code == KeyCode::Char('l')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !interaction_pending
    {
        let mut effects = open_model_overlay(state);
        effects.push(Effect::RequestRender);
        return effects;
    }
    if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return reduce(state, Action::ToggleTodoDock);
    }
    if key.code == KeyCode::Esc && state.working {
        state.request_cancel_active_run();
        return vec![Effect::Send(UiCommand::CancelRun), Effect::RequestRender];
    }
    if key.code == KeyCode::BackTab {
        return vec![
            Effect::Send(UiCommand::SetMode(cycle_mode(state.mode))),
            Effect::RequestRender,
        ];
    }
    if let Some(effects) = reduce_interaction_key(state, key) {
        return effects;
    }
    if key.code == KeyCode::Enter
        && key.kind == KeyEventKind::Press
        && key.modifiers == KeyModifiers::NONE
        && state.composer.is_empty()
    {
        if let Some(id) = state.selected_block_id().cloned() {
            return reduce(state, Action::ToggleBlock(id));
        }
    }
    if key.code == KeyCode::Enter
        && key.kind == KeyEventKind::Press
        && state.working
        && state.composer.payload().trim() == "/resume"
    {
        state
            .push_notification("Há uma execução ativa; aguarde ou cancele antes de retomar".into());
        state.revisions.status += 1;
        return vec![Effect::RequestRender];
    }
    if key.code == KeyCode::Enter
        && key.kind == KeyEventKind::Press
        && state.working
        && state.composer.payload().trim().starts_with("/queue")
    {
        return submit_composer(state);
    }
    // Native slash commands that stay local or hit the worker's read-only
    // arms must not queue as prompt text mid-run: /compact defers to a safe
    // boundary; /mcp opens the overlay (its mutating UiCommands are rejected
    // by the active-run catch-all with a notification).
    if key.code == KeyCode::Enter && key.kind == KeyEventKind::Press && state.working {
        let payload = state.composer.payload();
        let payload = payload.trim();
        if payload.starts_with("/compact") || payload == "/mcp" || payload.starts_with("/mcp ") {
            return submit_composer(state);
        }
    }
    match classify_enter(normalize(key), state.working) {
        EnterIntent::Submit if state.working => return enqueue_queued(state),
        EnterIntent::Submit => return submit_composer(state),
        EnterIntent::Steer => return enqueue_queued(state),
        EnterIntent::Newline => {
            state.composer.insert_text("\n");
            state.revisions.content += 1;
            sync_slash_suggestions(state);
            return vec![Effect::RequestRender];
        }
        EnterIntent::Ignore => {}
    }
    match key.code {
        KeyCode::Char(character)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            // G245: a draft over the 1 MiB limit is reported, not silently
            // dropped (mirrors the paste arm).
            if state
                .composer
                .try_insert_text(character.to_string())
                .is_err()
            {
                state.push_notification_with_priority(
                    "Rascunho grande demais (limite de 1 MiB).".into(),
                    NotificationPriority::Warning,
                );
                state.revisions.status += 1;
            }
            state.revisions.content += 1;
            sync_slash_suggestions(state);
        }
        KeyCode::Backspace if state.composer.backspace() => {
            state.revisions.content += 1;
            sync_slash_suggestions(state);
        }
        KeyCode::Delete if state.composer.delete() => {
            state.revisions.content += 1;
            sync_slash_suggestions(state);
        }
        KeyCode::Left if state.composer.move_left() => {
            state.revisions.focus += 1;
            sync_slash_suggestions(state);
        }
        KeyCode::Right if state.composer.move_right() => {
            state.revisions.focus += 1;
            sync_slash_suggestions(state);
        }
        KeyCode::Home if state.composer.move_home() => {
            state.revisions.focus += 1;
            sync_slash_suggestions(state);
        }
        KeyCode::End if state.composer.move_end() => {
            state.revisions.focus += 1;
            sync_slash_suggestions(state);
        }
        _ => {}
    }
    vec![Effect::RequestRender]
}

fn copyable_block_text(block: &crate::block::Block) -> Option<String> {
    match block.kind() {
        BlockKind::User(text)
        | BlockKind::Assistant(text)
        | BlockKind::Thinking(text)
        | BlockKind::System(text)
        | BlockKind::Error(text)
        | BlockKind::Activity(text)
        | BlockKind::QueuedUser(text) => Some(text.clone()),
        BlockKind::Tool(tool) => (!tool.materialized_output.is_empty())
            .then(|| tool.materialized_output.clone())
            .or_else(|| (!tool.preview.is_empty()).then(|| tool.preview.clone())),
        BlockKind::InteractionRequest(request) => Some(request.display_text()),
    }
}

fn reduce_inspector_key(state: &mut AppState, key: KeyEvent) -> Option<Vec<Effect>> {
    let _kind = state.inspector.active?;
    match key.code {
        KeyCode::Esc => {
            state.inspector.active = None;
            state.inspector.scroll.top();
        }
        KeyCode::Up => state.inspector.scroll.up(1),
        KeyCode::Down => state.inspector.scroll.down(1),
        KeyCode::PageUp => state.inspector.scroll.up(PICKER_NOMINAL_CAPACITY),
        KeyCode::PageDown => state.inspector.scroll.down(PICKER_NOMINAL_CAPACITY),
        KeyCode::Home => state.inspector.scroll.top(),
        KeyCode::End => state.inspector.scroll.end(),
        _ => return None,
    }
    state.revisions.focus += 1;
    Some(vec![Effect::RequestRender])
}

fn reduce_inspector_scroll(
    state: &mut AppState,
    intent: ScrollIntent,
    total_rows: usize,
    capacity: usize,
) -> Vec<Effect> {
    if state.inspector.active.is_none() {
        return Vec::new();
    }
    match intent {
        ScrollIntent::Up => state.inspector.scroll.up_bounded(1, total_rows, capacity),
        ScrollIntent::Down => state.inspector.scroll.down_bounded(1, total_rows, capacity),
        ScrollIntent::PageUp => {
            state
                .inspector
                .scroll
                .up_bounded(PICKER_NOMINAL_CAPACITY, total_rows, capacity)
        }
        ScrollIntent::PageDown => {
            state
                .inspector
                .scroll
                .down_bounded(PICKER_NOMINAL_CAPACITY, total_rows, capacity)
        }
        ScrollIntent::Top => state.inspector.scroll.top(),
        ScrollIntent::LiveEdge => state.inspector.scroll.end(),
    }
    state.revisions.focus += 1;
    vec![Effect::RequestRender]
}

fn reduce_approval_scroll(
    state: &mut AppState,
    intent: ScrollIntent,
    total_rows: usize,
    capacity: usize,
) -> Vec<Effect> {
    if state.pending_interaction().is_none() {
        return Vec::new();
    }
    match intent {
        ScrollIntent::Up => state.approval_scroll.up_bounded(1, total_rows, capacity),
        ScrollIntent::Down => state.approval_scroll.down_bounded(1, total_rows, capacity),
        ScrollIntent::PageUp => {
            state
                .approval_scroll
                .up_bounded(PICKER_NOMINAL_CAPACITY, total_rows, capacity)
        }
        ScrollIntent::PageDown => {
            state
                .approval_scroll
                .down_bounded(PICKER_NOMINAL_CAPACITY, total_rows, capacity)
        }
        ScrollIntent::Top => state.approval_scroll.top(),
        ScrollIntent::LiveEdge => state.approval_scroll.end(),
    }
    state.revisions.focus += 1;
    vec![Effect::RequestRender]
}

fn reduce_search_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let mut search = state.search.take().unwrap_or_default();
    match key.code {
        KeyCode::Esc => {
            state.revisions.focus += 1;
            return vec![Effect::RequestRender];
        }
        KeyCode::Tab => {
            search.filter = search.filter.next();
            search.selected = 0;
        }
        KeyCode::Backspace => {
            search.query.pop();
            search.selected = 0;
        }
        KeyCode::Char(character)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            search.query.push(character);
            search.selected = 0;
        }
        _ => {}
    }
    // One transcript scan per key: match navigation and pinning share it
    // (navigation used to rescan the identical query a second time).
    let matches = search_match_indices_filtered(state.blocks(), &search.query, search.filter);
    match key.code {
        KeyCode::Enter | KeyCode::Down if !matches.is_empty() => {
            search.selected = (search.selected + 1) % matches.len();
        }
        KeyCode::Up if !matches.is_empty() => {
            search.selected = search.selected.checked_sub(1).unwrap_or(matches.len() - 1);
        }
        _ => {}
    }
    if matches.is_empty() {
        search.selected = 0;
    } else if search.selected >= matches.len() {
        search.selected = matches.len() - 1;
    }
    if let Some(index) = matches.get(search.selected).copied() {
        state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
            block_id: state.blocks()[index].id.clone(),
            row_offset: 0,
        });
        state.revisions.viewport += 1;
    }
    state.search = Some(search);
    state.revisions.focus += 1;
    vec![Effect::RequestRender]
}

fn reduce_interaction_key(state: &mut AppState, key: KeyEvent) -> Option<Vec<Effect>> {
    let interaction = state.pending_interaction()?.clone();
    if interaction.response_pending {
        return Some(Vec::new());
    }
    match interaction.kind {
        InteractionRequestKind::Input { .. }
            if key.code == KeyCode::Enter
                && key.kind == KeyEventKind::Press
                && key.modifiers == KeyModifiers::NONE =>
        {
            Some(submit_pending_input(state))
        }
        InteractionRequestKind::Approval { .. }
            if key.modifiers == KeyModifiers::NONE
                && matches!(key.code, KeyCode::Char('y' | 'Y' | 'n' | 'N')) =>
        {
            if key.kind != KeyEventKind::Press {
                return Some(Vec::new());
            }
            if !state.approval_content_accessible {
                return Some(vec![Effect::RequestRender]);
            }
            let approved = matches!(key.code, KeyCode::Char('y' | 'Y'));
            if !state.mark_interaction_response_pending(&interaction.request_id) {
                return Some(vec![Effect::RequestRender]);
            }
            let command = if approved {
                UiCommand::Approve {
                    request_id: interaction.request_id,
                }
            } else {
                UiCommand::Reject {
                    request_id: interaction.request_id,
                }
            };
            Some(vec![Effect::Send(command), Effect::RequestRender])
        }
        InteractionRequestKind::Approval { .. }
            if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE =>
        {
            Some(vec![Effect::RequestRender])
        }
        InteractionRequestKind::Approval { .. } => Some(Vec::new()),
        InteractionRequestKind::Question { .. }
            if key.kind == KeyEventKind::Press
                && key.modifiers == KeyModifiers::NONE
                && matches!(key.code, KeyCode::Up | KeyCode::Down) =>
        {
            state.move_question_selection(&interaction.request_id, key.code == KeyCode::Down);
            Some(vec![Effect::RequestRender])
        }
        InteractionRequestKind::Question { ref options, .. }
            if key.kind == KeyEventKind::Press
                && key.modifiers == KeyModifiers::NONE
                && matches!(key.code, KeyCode::Char('1'..='9'))
                && !interaction.custom_question_answer =>
        {
            let KeyCode::Char(digit) = key.code else {
                return Some(Vec::new());
            };
            let index = digit.to_digit(10).unwrap_or_default() as usize - 1;
            if index < options.len() {
                state.select_question_option(&interaction.request_id, index);
            }
            Some(vec![Effect::RequestRender])
        }
        InteractionRequestKind::Question { options, .. }
            if key.code == KeyCode::Enter
                && key.kind == KeyEventKind::Press
                && key.modifiers == KeyModifiers::NONE =>
        {
            if options.is_empty() || interaction.custom_question_answer {
                return Some(submit_pending_question_custom(state));
            }
            if interaction.selected_question_option == options.len() {
                state.activate_custom_question_answer(&interaction.request_id);
                return Some(vec![Effect::RequestRender]);
            }
            let index = interaction.selected_question_option;
            let Some(option) = options.get(index) else {
                return Some(vec![Effect::RequestRender]);
            };
            let Ok(answer) = slim_core::QuestionAnswer::option(index, option.label.clone()) else {
                return Some(vec![Effect::RequestRender]);
            };
            if !state.mark_interaction_response_pending(&interaction.request_id) {
                return Some(vec![Effect::RequestRender]);
            }
            state.record_question_answer(&interaction.request_id, option.label.clone());
            Some(vec![
                Effect::Send(UiCommand::AnswerQuestion {
                    request_id: interaction.request_id,
                    answer,
                }),
                Effect::RequestRender,
            ])
        }
        InteractionRequestKind::Question { options, .. }
            if options.is_empty() || interaction.custom_question_answer =>
        {
            None
        }
        InteractionRequestKind::Question { .. } => Some(Vec::new()),
        _ => None,
    }
}

fn submit_pending_question_custom(state: &mut AppState) -> Vec<Effect> {
    let Some(interaction) = state.pending_interaction().cloned() else {
        return vec![Effect::RequestRender];
    };
    if !matches!(interaction.kind, InteractionRequestKind::Question { .. })
        || interaction.response_pending
    {
        return vec![Effect::RequestRender];
    }
    let Ok(answer) = slim_core::QuestionAnswer::custom(state.composer.payload().trim().to_owned())
    else {
        return vec![Effect::RequestRender];
    };
    if !state.mark_interaction_response_pending(&interaction.request_id) {
        return vec![Effect::RequestRender];
    }
    let submitted = state.composer.payload().trim().to_owned();
    state.record_question_answer(&interaction.request_id, submitted);
    state.composer.clear();
    state.slash_suggestions = None;
    state.revisions.content += 1;
    vec![
        Effect::Send(UiCommand::AnswerQuestion {
            request_id: interaction.request_id,
            answer,
        }),
        Effect::RequestRender,
    ]
}

fn submit_pending_input(state: &mut AppState) -> Vec<Effect> {
    let Some(interaction) = state.pending_interaction().cloned() else {
        return vec![Effect::RequestRender];
    };
    if !matches!(interaction.kind, InteractionRequestKind::Input { .. })
        || interaction.response_pending
    {
        return vec![Effect::RequestRender];
    }
    let answer = state.composer.payload();
    if answer.trim().is_empty() {
        return vec![Effect::RequestRender];
    }
    if !state.mark_interaction_response_pending(&interaction.request_id) {
        return vec![Effect::RequestRender];
    }
    state.composer.clear();
    state.slash_suggestions = None;
    state.revisions.content += 1;
    vec![
        Effect::Send(UiCommand::AnswerInput {
            request_id: interaction.request_id,
            answer,
        }),
        Effect::RequestRender,
    ]
}

/// G250: opens the grouped model overlay (both catalogs) and requests a
/// refresh of each. Backs both `/model`/`/models` and the Ctrl+L shortcut so
/// the two entry points stay in lockstep.
fn open_model_overlay(state: &mut AppState) -> Vec<Effect> {
    // Unified grouped overlay: both provider catalogs are always
    // listed; selecting a model of a non-connected provider is
    // rejected by the CLI handler with a notification.
    state.model_overlay = Some(ModelOverlay::for_current(
        &state.model,
        state.auth_provider,
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    ));
    state.revisions.status += 1;
    vec![
        Effect::Send(UiCommand::RefreshOpenCodeModels),
        Effect::Send(UiCommand::RefreshClinePassModels),
        Effect::Send(UiCommand::RefreshCommandCodeModels),
        Effect::Send(UiCommand::RefreshZenModels),
    ]
}

/// `/mcp <args>` forms: `add <name> <command> [args…] [--global]`,
/// `add <name> --url <url> [--global]`, `remove|rm <name>`,
/// `reconnect|disconnect <name>`, `reload`. HTTP headers are not settable
/// here — they belong in slim.toml where secrets stay out of transcripts.
fn parse_mcp_args(
    state: &mut AppState,
    args: &str,
    effects: &mut Vec<Effect>,
    keep_draft: &mut bool,
) {
    const USAGE: &str =
        "Uso: /mcp [add <nome> <comando..>|--url <url>] [--global] | remove <nome> | reconnect <nome> | disconnect <nome> | reload";
    let mut tokens = args.split_whitespace().peekable();
    match tokens.next() {
        Some("add") => {
            let Some(name) = tokens.next() else {
                state.push_notification(USAGE.into());
                *keep_draft = true;
                return;
            };
            let rest: Vec<&str> = tokens.collect();
            let global = rest.contains(&"--global");
            let rest: Vec<&str> = rest
                .into_iter()
                .filter(|token| *token != "--global")
                .collect();
            let (command, url, args) = match rest.as_slice() {
                ["--url", url] => (None, Some((*url).to_owned()), Vec::new()),
                [command, args @ ..] if !command.is_empty() && !command.starts_with('-') => (
                    Some((*command).to_owned()),
                    None,
                    args.iter().map(|s| (*s).to_owned()).collect(),
                ),
                _ => {
                    state.push_notification(USAGE.into());
                    *keep_draft = true;
                    return;
                }
            };
            let valid_name = !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
            if !valid_name {
                state.push_notification(
                    "Nome de servidor inválido (use letras, dígitos, _ e -).".into(),
                );
                *keep_draft = true;
                return;
            }
            effects.push(Effect::Send(UiCommand::McpAdd {
                name: name.to_owned(),
                command,
                args,
                url,
                global,
            }));
        }
        Some("remove") | Some("rm") => {
            if let Some(name) = tokens.next() {
                effects.push(Effect::Send(UiCommand::McpRemove {
                    name: name.to_owned(),
                }));
            } else {
                state.push_notification(USAGE.into());
                *keep_draft = true;
            }
        }
        Some("reconnect") => {
            if let Some(name) = tokens.next() {
                effects.push(Effect::Send(UiCommand::McpReconnect {
                    name: name.to_owned(),
                }));
            } else {
                state.push_notification(USAGE.into());
                *keep_draft = true;
            }
        }
        Some("disconnect") => {
            if let Some(name) = tokens.next() {
                effects.push(Effect::Send(UiCommand::McpDisconnect {
                    name: name.to_owned(),
                }));
            } else {
                state.push_notification(USAGE.into());
                *keep_draft = true;
            }
        }
        Some("reload") => {
            effects.push(Effect::Send(UiCommand::McpRefresh));
        }
        _ => {
            state.push_notification(USAGE.into());
            *keep_draft = true;
        }
    }
}

fn submit_composer(state: &mut AppState) -> Vec<Effect> {
    // One reverse scan over the blocks; the two dispatch arms used to run
    // `pending_interaction` once each.
    if let Some(interaction) = state.pending_interaction() {
        match &interaction.kind {
            InteractionRequestKind::Input { .. } => return submit_pending_input(state),
            InteractionRequestKind::Question { .. } => {
                return submit_pending_question_custom(state);
            }
            _ => {}
        }
    }
    let prompt = state.composer.payload();
    let command = prompt.trim().to_owned();
    let mut effects = Vec::new();
    // Locally rejected slash input (unknown model, missing path, signed-out
    // compaction) keeps the draft so the user can fix and resubmit it instead
    // of retyping after the 5 s toast expires.
    let mut keep_draft = false;
    match command.as_str() {
        "" => {}
        "/help" => {
            state.push_notification(
                "F1 / Ctrl+P comandos · Shift+Tab modo · Ctrl+F buscar · Ctrl+T tarefas · Esc cancelar"
                    .into(),
            );
            state.revisions.status += 1;
        }
        "/login" => {
            state.login_overlay = Some(LoginOverlay::default());
            state.revisions.status += 1;
        }
        "/login anthropic" => {
            state.login_overlay = Some(LoginOverlay {
                in_progress: true,
                ..LoginOverlay::default()
            });
            effects.push(Effect::Send(UiCommand::StartLogin(
                LoginProvider::Anthropic,
            )));
            state.revisions.status += 1;
        }
        "/login codex" | "/login openai-codex" => {
            state.login_overlay = Some(LoginOverlay {
                selected: 1,
                in_progress: true,
                ..LoginOverlay::default()
            });
            effects.push(Effect::Send(UiCommand::StartLogin(
                LoginProvider::OpenAiCodex,
            )));
            state.revisions.status += 1;
        }
        "/login opencode" | "/login opencode-go" => {
            state.login_overlay = Some(LoginOverlay {
                selected: 2,
                stage: LoginStage::ApiKey(Default::default()),
                ..LoginOverlay::default()
            });
            state.revisions.status += 1;
        }
        "/login clinepass" | "/login cline-pass" => {
            state.login_overlay = Some(LoginOverlay {
                selected: 3,
                stage: LoginStage::ApiKey(Default::default()),
                ..LoginOverlay::default()
            });
            state.revisions.status += 1;
        }
        "/login command-code" | "/login commandcode" | "/login cmd" => {
            state.login_overlay = Some(LoginOverlay {
                selected: 4,
                stage: LoginStage::ApiKey(Default::default()),
                ..LoginOverlay::default()
            });
            state.revisions.status += 1;
        }
        "/login xai" | "/login grok" => {
            state.login_overlay = Some(LoginOverlay {
                selected: 5,
                in_progress: true,
                ..LoginOverlay::default()
            });
            effects.push(Effect::Send(UiCommand::StartLogin(LoginProvider::Xai)));
            state.revisions.status += 1;
        }
        "/login opencode-zen" | "/login zen" => {
            state.login_overlay = Some(LoginOverlay {
                selected: 6,
                stage: LoginStage::ApiKey(Default::default()),
                ..LoginOverlay::default()
            });
            state.revisions.status += 1;
        }
        "/logout" => {
            effects.push(Effect::Send(UiCommand::Logout));
            state.revisions.status += 1;
        }
        "/resume" => {
            effects.push(Effect::Send(UiCommand::ResumePrevious));
            state.revisions.status += 1;
        }
        _ if command == "/queue" || command.starts_with("/queue ") => {
            if !reduce_queue_command(state, &command, &mut effects) {
                keep_draft = true;
            } else if command.starts_with("/queue edit ") {
                // The selected queued text is deliberately returned to the
                // composer for editing; do not clear that draft as a slash
                // command side effect.
                keep_draft = true;
            }
        }
        "/mode" => {
            effects.push(Effect::Send(UiCommand::SetMode(cycle_mode(state.mode))));
            state.revisions.status += 1;
        }
        "/image" => {
            state.push_notification("Uso: /image CAMINHO".into());
            keep_draft = true;
            state.revisions.status += 1;
        }
        _ if command.starts_with("/image ") => {
            let path = command.trim_start_matches("/image ").trim();
            if path.is_empty() {
                state.push_notification("Uso: /image CAMINHO".into());
                keep_draft = true;
            } else {
                effects.push(Effect::Send(UiCommand::AttachImage(path.to_owned())));
            }
            state.revisions.status += 1;
        }
        "/diff" | "/activity" | "/session" | "/diagnostics" => {
            let kind = match command.as_str() {
                "/diff" => InspectorKind::Diff,
                "/activity" => InspectorKind::Activity,
                "/session" => InspectorKind::SessionTree,
                _ => InspectorKind::Diagnostics,
            };
            state.inspector.toggle(kind);
            state.revisions.focus += 1;
        }
        "/compact" => {
            if state.authenticated {
                effects.push(Effect::Send(UiCommand::Compact {
                    instructions: String::new(),
                }));
            } else {
                state.push_notification("Nenhum provedor conectado. Use /login.".into());
                keep_draft = true;
            }
            state.revisions.status += 1;
        }
        _ if command.starts_with("/compact ") => {
            if state.authenticated {
                effects.push(Effect::Send(UiCommand::Compact {
                    instructions: command.trim_start_matches("/compact ").trim().to_owned(),
                }));
            } else {
                state.push_notification("Nenhum provedor conectado. Use /login.".into());
                keep_draft = true;
            }
            state.revisions.status += 1;
        }
        "/mcp" => {
            // Search owns the keyboard before overlays do (reduce_key order):
            // clear it so the new overlay is actually reachable.
            state.search = None;
            state.mcp_overlay = Some(crate::app::McpOverlay::default());
            effects.push(Effect::Send(UiCommand::McpRefresh));
            effects.push(Effect::Send(UiCommand::McpWatch { on: true }));
            state.revisions.status += 1;
        }
        _ if command.starts_with("/mcp ") => {
            parse_mcp_args(
                state,
                command.trim_start_matches("/mcp "),
                &mut effects,
                &mut keep_draft,
            );
            state.revisions.status += 1;
        }
        "/model" | "/models" => {
            // G250: the shared helper also backs Ctrl+L, so the two paths
            // stay in lockstep (same overlay, same refresh commands).
            effects.extend(open_model_overlay(state));
        }
        _ if command.starts_with("/model ") => {
            let value = command.trim_start_matches("/model ").trim();
            match state.auth_provider {
                Some(LoginProvider::OpenAiCodex) => {
                    if let Some(alias) = ModelAlias::parse(value) {
                        let effort = if ReasoningEffort::supported(alias).contains(&state.effort) {
                            state.effort
                        } else {
                            ReasoningEffort::default_for(alias)
                        };
                        effects.push(Effect::Send(UiCommand::SetModel {
                            model: alias,
                            effort,
                            fast: state.codex_fast,
                        }));
                    } else {
                        state.push_notification(
                            "Modelo desconhecido. Use sol, terra ou luna.".into(),
                        );
                        keep_draft = true;
                    }
                }
                Some(LoginProvider::OpenCodeGo) => {
                    if let Some(model) = state
                        .open_code_models
                        .iter()
                        .find(|model| model.id == value)
                    {
                        let effort = if model.reasoning_levels.contains(&state.effort) {
                            state.effort
                        } else {
                            model
                                .reasoning_levels
                                .first()
                                .copied()
                                .unwrap_or(ReasoningEffort::High)
                        };
                        effects.push(Effect::Send(UiCommand::SetOpenCodeModel {
                            model: model.id.clone(),
                            effort,
                        }));
                    } else {
                        state.push_notification(
                            "Modelo OpenCode Go desconhecido. Use /models.".into(),
                        );
                        keep_draft = true;
                    }
                }
                Some(LoginProvider::OpenCodeZen) => {
                    if let Some(model) = state.zen_models.iter().find(|model| model.id == value) {
                        let effort = if model.reasoning_levels.contains(&state.effort) {
                            state.effort
                        } else {
                            model
                                .reasoning_levels
                                .first()
                                .copied()
                                .unwrap_or(ReasoningEffort::High)
                        };
                        effects.push(Effect::Send(UiCommand::SetZenModel {
                            model: model.id.clone(),
                            effort,
                        }));
                    } else {
                        state.push_notification(
                            "Modelo OpenCode Zen desconhecido. Use /models.".into(),
                        );
                        keep_draft = true;
                    }
                }
                Some(LoginProvider::ClinePass) => {
                    // G249: textual `/model <id>` also works while ClinePass is
                    // connected; effort defaults to High on the static catalog.
                    if let Some(model) = state
                        .cline_pass_models
                        .iter()
                        .find(|model| model.id == value)
                    {
                        effects.push(Effect::Send(UiCommand::SetClinePassModel {
                            model: model.id.clone(),
                            effort: ReasoningEffort::High,
                        }));
                    } else {
                        state.push_notification(
                            "Modelo ClinePass desconhecido. Use /models.".into(),
                        );
                        keep_draft = true;
                    }
                }
                Some(LoginProvider::CommandCode) => {
                    if let Some(model) = state
                        .command_code_models
                        .iter()
                        .find(|model| model.id == value)
                    {
                        effects.push(Effect::Send(UiCommand::SetCommandCodeModel {
                            model: model.id.clone(),
                            effort: ReasoningEffort::High,
                        }));
                    } else {
                        state.push_notification(
                            "Modelo Command Code desconhecido. Use /models.".into(),
                        );
                        keep_draft = true;
                    }
                }
                Some(LoginProvider::Xai) => {
                    if slim_core::provider::is_xai_model_id(value) {
                        effects.push(Effect::Send(UiCommand::SetXaiModel {
                            model: value.to_owned(),
                            effort: ReasoningEffort::High,
                        }));
                    } else {
                        state.push_notification(
                            "Modelo xAI desconhecido. Use grok-4.3, grok-4.5, grok-4.6 ou grok-build-0.1."
                                .into(),
                        );
                        keep_draft = true;
                    }
                }
                _ => {
                    state.push_notification(
                        "Conecte OpenAI Codex, OpenCode Go, ClinePass, Command Code ou xAI antes de selecionar um modelo."
                            .into(),
                    );
                    keep_draft = true;
                }
            }
            state.revisions.status += 1;
        }
        // Let the worker resolve dynamic skill commands. This also keeps
        // unknown slash commands fail-closed without losing the draft.
        _ if command.starts_with('/') => {
            effects.push(Effect::Send(UiCommand::SendPrompt(prompt)));
        }
        _ if !state.authenticated => {
            state.push_notification("Nenhum provedor conectado. Use /login.".into());
            state.revisions.status += 1;
            return effects;
        }
        _ => {
            effects.push(Effect::Send(UiCommand::SendPrompt(prompt)));
        }
    }
    // Draft is cleared on every accepted submission (slash command or send);
    // preserved when signed out with a plain prompt (spec §15.3) and when a
    // slash command is rejected locally (`keep_draft`) so the user can fix it.
    if (command.starts_with('/') || state.authenticated) && !keep_draft {
        state.composer.clear();
        state.revisions.content += 1;
    }
    state.slash_suggestions = None;
    effects.push(Effect::RequestRender);
    effects
}

/// Handles local queue controls. Queue positions are intentionally parsed as
/// one-based human positions and resolved against the current deque, so an
/// item removed earlier cannot leave a stale index embedded in the transcript.
fn reduce_queue_command(state: &mut AppState, command: &str, effects: &mut Vec<Effect>) -> bool {
    let mut parts = command.split_whitespace();
    let _queue = parts.next();
    let action = parts.next().unwrap_or("status");
    let position = parts.next();
    if parts.next().is_some() {
        state.push_notification("Uso: /queue [status|pause|resume|edit N|remove N]".into());
        state.revisions.status += 1;
        return false;
    }

    match action {
        "status" if position.is_none() => {
            let status = if state.queue_paused {
                "pausada"
            } else {
                "ativa"
            };
            state.push_notification(format!("Fila {status} · {} pendente(s)", state.queue_len()));
            state.revisions.status += 1;
            true
        }
        "pause" if position.is_none() => {
            state.queue_paused = true;
            state.push_notification(format!("Fila pausada · {} pendente(s)", state.queue_len()));
            state.revisions.status += 1;
            true
        }
        "resume" if position.is_none() => {
            if state.working {
                state.push_notification("A fila pode ser retomada após a execução atual".into());
                state.revisions.status += 1;
                return true;
            }
            state.queue_paused = false;
            if let Some(prompt) = state.pop_queued_prompt() {
                effects.push(Effect::Send(UiCommand::SendPrompt(prompt)));
                state.push_notification("Fila retomada".into());
            } else {
                state.push_notification("Fila vazia".into());
            }
            state.revisions.status += 1;
            true
        }
        "remove" if position.is_some() => {
            let Some(index) = parse_queue_position(position.unwrap()) else {
                state.push_notification("Posição de fila inválida".into());
                state.revisions.status += 1;
                return false;
            };
            if state.remove_queued_prompt(index).is_some() {
                state.push_notification(format!("Item {} removido da fila", position.unwrap()));
                state.revisions.status += 1;
                true
            } else {
                state.push_notification("Posição de fila inexistente".into());
                state.revisions.status += 1;
                false
            }
        }
        "edit" if position.is_some() => {
            let Some(index) = parse_queue_position(position.unwrap()) else {
                state.push_notification("Posição de fila inválida".into());
                state.revisions.status += 1;
                return false;
            };
            let Some(prompt) = state.take_queued_prompt_for_edit(index) else {
                state.push_notification("Posição de fila inexistente".into());
                state.revisions.status += 1;
                return false;
            };
            state.composer.clear();
            state.composer.insert_text(prompt);
            sync_slash_suggestions(state);
            state.revisions.content += 1;
            true
        }
        _ => {
            state.push_notification("Uso: /queue [status|pause|resume|edit N|remove N]".into());
            state.revisions.status += 1;
            false
        }
    }
}

fn parse_queue_position(value: &str) -> Option<usize> {
    let position = value.parse::<usize>().ok()?;
    position.checked_sub(1)
}

fn reduce_model_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(mut overlay) = state.model_overlay.clone() else {
        return vec![];
    };
    let rows = overlay.rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    );
    match key.code {
        KeyCode::Esc => {
            state.model_overlay = None;
            return vec![Effect::RequestRender];
        }
        KeyCode::Up => overlay.selected = move_selection(overlay.selected, rows.len(), -1),
        KeyCode::Down => overlay.selected = move_selection(overlay.selected, rows.len(), 1),
        KeyCode::Home => overlay.selected = 0,
        KeyCode::End => overlay.selected = rows.len().saturating_sub(1),
        KeyCode::Char(' ') => {
            // Space toggles the collapsed state of the group under the cursor.
            let group = match rows.get(overlay.selected) {
                Some(row) => match row {
                    ModelRow::Header(g) => *g,
                    ModelRow::Alias(_) => 0,
                    ModelRow::Catalog(_) => 1,
                    ModelRow::ClinePass(_) => 2,
                    ModelRow::CommandCode(_) => 3,
                    ModelRow::Zen(_) => 4,
                },
                None => return vec![Effect::RequestRender],
            };
            overlay.toggle_collapsed(group);
            overlay.selected = overlay.selected.min(
                overlay
                    .rows(
                        &state.open_code_models,
                        &state.cline_pass_models,
                        &state.command_code_models,
                        &state.zen_models,
                    )
                    .len()
                    .saturating_sub(1),
            );
        }
        KeyCode::Backspace => {
            if overlay.filter.pop().is_some() {
                overlay.selected = 0;
                overlay.viewport_start = 0;
            }
        }
        KeyCode::Char(character)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            overlay.filter.push(character);
            overlay.selected = 0;
            overlay.viewport_start = 0;
        }
        KeyCode::Enter => {
            let Some(row) = rows.get(overlay.selected) else {
                return vec![Effect::RequestRender];
            };
            match row {
                ModelRow::Header(_) => {
                    state.model_overlay = Some(overlay);
                    return vec![Effect::RequestRender];
                }
                ModelRow::Alias(alias) => {
                    let levels = ReasoningEffort::supported(*alias);
                    let preferred = if levels.contains(&state.effort) {
                        state.effort
                    } else {
                        ReasoningEffort::default_for(*alias)
                    };
                    let selected = levels
                        .iter()
                        .position(|level| *level == preferred)
                        .unwrap_or(0);
                    state.effort_overlay = Some(EffortOverlay {
                        model: *alias,
                        selected,
                        fast: state.codex_fast,
                    });
                }
                ModelRow::Catalog(model_index) => {
                    let Some(model) = state.open_code_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![
                            Effect::Send(UiCommand::RefreshOpenCodeModels),
                            Effect::RequestRender,
                        ];
                    };
                    state.model_overlay = None;
                    let effort = if model.reasoning_levels.contains(&state.effort) {
                        state.effort
                    } else {
                        model
                            .reasoning_levels
                            .first()
                            .copied()
                            .unwrap_or(ReasoningEffort::High)
                    };
                    return vec![
                        Effect::Send(UiCommand::SetOpenCodeModel {
                            model: model.id,
                            effort,
                        }),
                        Effect::RequestRender,
                    ];
                }
                ModelRow::ClinePass(model_index) => {
                    let Some(model) = state.cline_pass_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![Effect::RequestRender];
                    };
                    state.model_overlay = None;
                    return vec![
                        Effect::Send(UiCommand::SetClinePassModel {
                            model: model.id,
                            effort: ReasoningEffort::High,
                        }),
                        Effect::RequestRender,
                    ];
                }
                ModelRow::CommandCode(model_index) => {
                    let Some(model) = state.command_code_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![
                            Effect::Send(UiCommand::RefreshCommandCodeModels),
                            Effect::RequestRender,
                        ];
                    };
                    state.model_overlay = None;
                    return vec![
                        Effect::Send(UiCommand::SetCommandCodeModel {
                            model: model.id,
                            effort: ReasoningEffort::High,
                        }),
                        Effect::RequestRender,
                    ];
                }
                ModelRow::Zen(model_index) => {
                    let Some(model) = state.zen_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![
                            Effect::Send(UiCommand::RefreshZenModels),
                            Effect::RequestRender,
                        ];
                    };
                    state.model_overlay = None;
                    let effort = if model.reasoning_levels.contains(&state.effort) {
                        state.effort
                    } else {
                        model
                            .reasoning_levels
                            .first()
                            .copied()
                            .unwrap_or(ReasoningEffort::High)
                    };
                    return vec![
                        Effect::Send(UiCommand::SetZenModel {
                            model: model.id,
                            effort,
                        }),
                        Effect::RequestRender,
                    ];
                }
            }
            return vec![Effect::RequestRender];
        }
        _ => return vec![],
    }
    let updated_rows = overlay.rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    );
    overlay.viewport_start = ensure_visible_start(
        overlay.viewport_start,
        overlay.selected,
        updated_rows.len(),
        PICKER_NOMINAL_CAPACITY,
    );
    state.model_overlay = Some(overlay);
    state.revisions.status += 1;
    vec![Effect::RequestRender]
}

/// `/mcp` overlay keys: ↑↓/Home/End navigate, Enter tests (connects lazily),
/// `r` reconnects, `x` disconnects, `d`/`Delete` arms removal (`y`/`Enter`
/// confirms, anything else cancels), `R` refreshes, `Esc` closes and stops
/// the worker's status watch.
fn reduce_mcp_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(mut overlay) = state.mcp_overlay.clone() else {
        return vec![];
    };
    // Destructive bindings are plain key presses: a Ctrl/Alt-modified char
    // must not confirm a removal or fire a reconnect underneath the overlay.
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return vec![];
    }
    if let Some(name) = overlay.confirm_remove.clone() {
        overlay.confirm_remove = None;
        state.mcp_overlay = Some(overlay);
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => vec![
                Effect::Send(UiCommand::McpRemove { name }),
                Effect::RequestRender,
            ],
            _ => vec![Effect::RequestRender],
        };
    }
    let count = state.mcp_servers.len();
    let selected_name = state
        .mcp_servers
        .get(overlay.selected)
        .map(|server| server.name.clone());
    match key.code {
        KeyCode::Esc => {
            state.mcp_overlay = None;
            return vec![
                Effect::Send(UiCommand::McpWatch { on: false }),
                Effect::RequestRender,
            ];
        }
        KeyCode::Up => overlay.selected = move_selection(overlay.selected, count, -1),
        KeyCode::Down => overlay.selected = move_selection(overlay.selected, count, 1),
        KeyCode::Home => overlay.selected = 0,
        KeyCode::End => overlay.selected = count.saturating_sub(1),
        KeyCode::Enter => {
            state.mcp_overlay = Some(overlay);
            return match selected_name {
                Some(name) => vec![
                    Effect::Send(UiCommand::McpTest { name }),
                    Effect::RequestRender,
                ],
                None => vec![Effect::RequestRender],
            };
        }
        KeyCode::Char('r') => {
            state.mcp_overlay = Some(overlay);
            return match selected_name {
                Some(name) => vec![
                    Effect::Send(UiCommand::McpReconnect { name }),
                    Effect::RequestRender,
                ],
                None => vec![Effect::RequestRender],
            };
        }
        KeyCode::Char('R') => {
            return vec![Effect::Send(UiCommand::McpRefresh), Effect::RequestRender];
        }
        KeyCode::Char('x') => {
            state.mcp_overlay = Some(overlay);
            return match selected_name {
                Some(name) => vec![
                    Effect::Send(UiCommand::McpDisconnect { name }),
                    Effect::RequestRender,
                ],
                None => vec![Effect::RequestRender],
            };
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if let Some(name) = selected_name {
                overlay.confirm_remove = Some(name);
            }
        }
        _ => return vec![],
    }
    overlay.viewport_start = ensure_visible_start(
        overlay.viewport_start,
        overlay.selected,
        count,
        PICKER_NOMINAL_CAPACITY,
    );
    state.mcp_overlay = Some(overlay);
    state.revisions.status += 1;
    vec![Effect::RequestRender]
}

fn reduce_effort_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(overlay) = state.effort_overlay.clone() else {
        return vec![];
    };
    let levels = ReasoningEffort::supported(overlay.model);
    match key.code {
        KeyCode::Esc => {
            // Back to the model list underneath (G239): selection, filter and
            // folds were never torn down.
            state.effort_overlay = None;
        }
        KeyCode::Up => {
            if let Some(current) = state.effort_overlay.as_mut() {
                current.selected = current.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if let Some(current) = state.effort_overlay.as_mut() {
                current.selected = (current.selected + 1).min(levels.len() - 1);
            }
        }
        KeyCode::Tab => {
            if let Some(current) = state.effort_overlay.as_mut() {
                current.fast = !current.fast;
            }
        }
        KeyCode::Enter => {
            let effort = overlay.effort();
            state.effort_overlay = None;
            return vec![
                Effect::Send(UiCommand::SetModel {
                    model: overlay.model,
                    effort,
                    fast: overlay.fast,
                }),
                Effect::RequestRender,
            ];
        }
        _ => return vec![],
    }
    state.revisions.status += 1;
    vec![Effect::RequestRender]
}

fn reduce_login_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(overlay) = state.login_overlay.clone() else {
        return vec![];
    };
    if key.code == KeyCode::Esc || is_ctrl_c(&key) {
        if key.code == KeyCode::Esc
            && !overlay.in_progress
            && matches!(overlay.stage, LoginStage::ApiKey(_))
        {
            if let Some(current) = state.login_overlay.as_mut() {
                current.stage = LoginStage::Providers;
            }
            state.revisions.status += 1;
            return vec![Effect::RequestRender];
        }
        let mut effects = Vec::new();
        if overlay.in_progress {
            effects.push(Effect::Send(UiCommand::CancelLogin));
        }
        state.login_overlay = None;
        state.revisions.status += 1;
        effects.push(Effect::RequestRender);
        return effects;
    }
    if overlay.in_progress {
        return vec![];
    }
    if let LoginStage::ApiKey(ref api_key) = overlay.stage {
        match key.code {
            KeyCode::Backspace => {
                if let Some(LoginOverlay {
                    stage: LoginStage::ApiKey(current),
                    ..
                }) = state.login_overlay.as_mut()
                {
                    current.pop();
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if api_key.char_len() < 4_096 {
                    if let Some(LoginOverlay {
                        stage: LoginStage::ApiKey(current),
                        ..
                    }) = state.login_overlay.as_mut()
                    {
                        current.push(character);
                    }
                }
            }
            KeyCode::Enter if !api_key.is_empty() => {
                if let Some(current) = state.login_overlay.as_mut() {
                    current.in_progress = true;
                }
                let provider = overlay.provider();
                return vec![
                    Effect::Send(UiCommand::SaveApiKey {
                        provider,
                        api_key: api_key.clone(),
                    }),
                    Effect::RequestRender,
                ];
            }
            _ => return vec![],
        }
        state.revisions.status += 1;
        return vec![Effect::RequestRender];
    }
    match key.code {
        KeyCode::Up => {
            if let Some(current) = state.login_overlay.as_mut() {
                current.selected = current.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if let Some(current) = state.login_overlay.as_mut() {
                current.selected = (current.selected + 1).min(6);
            }
        }
        KeyCode::Home => {
            if let Some(current) = state.login_overlay.as_mut() {
                current.selected = 0;
            }
        }
        KeyCode::End => {
            if let Some(current) = state.login_overlay.as_mut() {
                current.selected = 6;
            }
        }
        KeyCode::Enter
            if overlay.provider() == LoginProvider::OpenCodeGo
                || overlay.provider() == LoginProvider::OpenCodeZen
                || overlay.provider() == LoginProvider::ClinePass
                || overlay.provider() == LoginProvider::CommandCode =>
        {
            if let Some(current) = state.login_overlay.as_mut() {
                current.stage = LoginStage::ApiKey(Default::default());
            }
            state.revisions.status += 1;
            return vec![Effect::RequestRender];
        }
        KeyCode::Enter => {
            let provider = overlay.provider();
            if let Some(current) = state.login_overlay.as_mut() {
                current.in_progress = true;
            }
            return vec![
                Effect::Send(UiCommand::StartLogin(provider)),
                Effect::RequestRender,
            ];
        }
        _ => return vec![],
    }
    state.revisions.status += 1;
    vec![Effect::RequestRender]
}

fn reduce_palette_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let mut query = state.palette_query.clone().unwrap_or_default();
    let matches = palette_matches(&query);
    match key.code {
        KeyCode::Esc | KeyCode::F(1) => state.palette_query = None,
        KeyCode::Backspace => {
            query.pop();
            state.palette_query = Some(query);
            state.palette_selected = 0;
            state.palette_viewport_start = 0;
        }
        KeyCode::Up => {
            state.palette_selected = move_selection(state.palette_selected, matches.len(), -1)
        }
        KeyCode::Down => {
            state.palette_selected = move_selection(state.palette_selected, matches.len(), 1)
        }
        KeyCode::Home => state.palette_selected = 0,
        KeyCode::End => state.palette_selected = matches.len().saturating_sub(1),
        KeyCode::Enter => {
            state.palette_query = None;
            let command = matches
                .get(state.palette_selected)
                .copied()
                .unwrap_or_default();
            if command.is_empty() {
                state.revisions.focus += 1;
                return vec![Effect::RequestRender];
            }
            if command == "/resume" {
                state.revisions.focus += 1;
                if state.working {
                    state.push_notification(
                        "Há uma execução ativa; aguarde ou cancele antes de retomar".into(),
                    );
                    state.revisions.status += 1;
                    return vec![Effect::RequestRender];
                }
                return vec![
                    Effect::Send(UiCommand::ResumePrevious),
                    Effect::RequestRender,
                ];
            }
            state.composer.clear();
            state.composer.insert_text(command);
            let effects = submit_composer(state);
            let mut all = effects;
            all.insert(0, Effect::RequestRender);
            return all;
        }
        KeyCode::Char(character)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            query.push(character);
            state.palette_query = Some(query);
            state.palette_selected = 0;
            state.palette_viewport_start = 0;
        }
        _ => {}
    }
    let updated_total = state
        .palette_query
        .as_deref()
        .map(palette_matches)
        .map_or(0, |matches| matches.len());
    if updated_total > 0 {
        state.palette_selected = state.palette_selected.min(updated_total - 1);
    } else {
        state.palette_selected = 0;
    }
    state.palette_viewport_start = ensure_visible_start(
        state.palette_viewport_start,
        state.palette_selected,
        updated_total,
        PICKER_NOMINAL_CAPACITY,
    );
    state.revisions.focus += 1;
    vec![Effect::RequestRender]
}

fn reduce_scroll(state: &mut AppState, intent: ScrollIntent, metrics: &ScrollMetrics) {
    if metrics.viewport_rows == 0 {
        return;
    }
    let was_live = state.scroll.is_live_edge();
    let live_mode = state.scroll.mode.clone();
    let pin = |anchor: Option<crate::app::ScrollAnchor>| {
        anchor.map_or(FollowMode::Top, FollowMode::Pinned)
    };
    let fold_navigation = matches!(
        intent,
        ScrollIntent::Up | ScrollIntent::Down | ScrollIntent::PageUp | ScrollIntent::PageDown
    );
    let fitted_fold_mode = if was_live && intent == ScrollIntent::Up {
        metrics
            .last_visible_foldable_anchor
            .clone()
            .map(FollowMode::Pinned)
    } else if fold_navigation && !was_live && metrics.total_rows <= metrics.viewport_rows {
        fitted_fold_navigation(state, intent)
    } else {
        None
    };
    state.scroll.mode = if let Some(mode) = fitted_fold_mode {
        mode
    } else {
        match intent {
            ScrollIntent::LiveEdge => FollowMode::LiveEdge { prompt_id: None },
            ScrollIntent::Top => FollowMode::Top,
            ScrollIntent::Up => pin(metrics.up_anchor.clone()),
            ScrollIntent::PageUp => pin(metrics.page_up_anchor.clone()),
            ScrollIntent::Down => {
                if was_live {
                    live_mode
                } else if metrics.viewport_start.saturating_add(1) >= metrics.bottom_start {
                    FollowMode::LiveEdge { prompt_id: None }
                } else {
                    pin(metrics.down_anchor.clone())
                }
            }
            ScrollIntent::PageDown => {
                if was_live {
                    live_mode
                } else if metrics
                    .viewport_start
                    .saturating_add(metrics.viewport_rows.max(1))
                    >= metrics.bottom_start
                {
                    FollowMode::LiveEdge { prompt_id: None }
                } else {
                    pin(metrics.page_down_anchor.clone())
                }
            }
        }
    };
    if state.scroll.is_live_edge() {
        state.scroll.unseen = 0;
    }
    state.revisions.viewport += 1;
}

fn fitted_fold_navigation(state: &AppState, intent: ScrollIntent) -> Option<FollowMode> {
    let foldable: Vec<_> = state
        .blocks()
        .iter()
        .filter(|block| matches!(block.kind(), BlockKind::Thinking(_)))
        .map(|block| block.id.clone())
        .collect();
    if foldable.is_empty() {
        return None;
    }
    let current = match &state.scroll.mode {
        FollowMode::Pinned(anchor) => foldable.iter().position(|id| id == &anchor.block_id),
        FollowMode::Top | FollowMode::LiveEdge { .. } => None,
    };
    let selected = match intent {
        ScrollIntent::Up => current.map_or(foldable.len() - 1, |index| index.saturating_sub(1)),
        ScrollIntent::Down => current.map_or(0, |index| (index + 1).min(foldable.len() - 1)),
        ScrollIntent::PageUp => 0,
        ScrollIntent::PageDown => foldable.len() - 1,
        ScrollIntent::Top | ScrollIntent::LiveEdge => return None,
    };
    Some(FollowMode::Pinned(ScrollAnchor {
        block_id: foldable[selected].clone(),
        row_offset: 0,
    }))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{reduce, Action, Effect};
    use crate::api::{
        LoginProvider, ModelAlias, OpenCodeCatalogSource, OpenCodeModelView, ReasoningEffort,
        UiCommand, UiEvent,
    };
    use crate::app::{
        ActivityPhase, AppState, CancellationPhase, ConfirmedSetting, FrameClock, LoginStage,
        NotificationPriority, RunOutcomeKind,
    };

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    #[test]
    fn login_command_opens_selector_and_selected_provider_is_sent() {
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.login_overlay.is_some());

        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::StartLogin(
            LoginProvider::OpenAiCodex
        ))));
    }

    #[test]
    fn login_failure_releases_overlay_instead_of_sticking_in_progress() {
        // G251: a terminal failure must reset `in_progress` — while set, the
        // reducer swallows every key, freezing the dialog on the error.
        let mut state = AppState::new();
        state.composer.insert_text("/login codex");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.login_overlay.as_ref().unwrap().in_progress);

        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::LoginFailed {
                message: "key save task failed boom".into(),
            }),
        );

        let overlay = state.login_overlay.as_ref().unwrap();
        assert!(
            !overlay.in_progress,
            "failure must release the overlay for retry"
        );
        assert_eq!(
            overlay.progress.as_deref(),
            Some("key save task failed boom"),
            "the error stays visible in the dialog"
        );
        // Esc now closes the dialog without issuing a bogus CancelLogin.
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(state.login_overlay.is_none());
        assert!(!effects.contains(&Effect::Send(UiCommand::CancelLogin)));
    }

    #[test]
    fn opencode_login_collects_secret_and_emits_typed_command() {
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        for _ in 0..2 {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            );
        }
        reduce(&mut state, Action::Key(enter()));
        for character in "needle-secret".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }

        let effects = reduce(&mut state, Action::Key(enter()));

        assert!(matches!(
            effects.as_slice(),
            [
                Effect::Send(UiCommand::SaveApiKey {
                    provider: LoginProvider::OpenCodeGo,
                    api_key,
                }),
                Effect::RequestRender,
            ] if api_key.expose() == "needle-secret"
        ));
        assert!(!format!("{state:?}").contains("needle-secret"));
    }

    #[test]
    fn opencode_login_paste_never_enters_composer() {
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        for _ in 0..2 {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            );
        }
        reduce(&mut state, Action::Key(enter()));

        reduce(&mut state, Action::Paste("pasted-secret".into()));

        assert!(state.composer.payload().is_empty());
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(matches!(
            effects.first(),
            Some(Effect::Send(UiCommand::SaveApiKey { api_key, .. }))
                if api_key.expose() == "pasted-secret"
        ));
    }

    #[test]
    fn clipboard_pull_prefers_the_image_attachment() {
        // §20: one clipboard pull carries image and text; the composer keeps
        // the attachment and never echoes the raw clipboard text.
        let mut state = AppState::new();
        let effects = reduce(
            &mut state,
            Action::ClipboardPull {
                image: Some(r"C:\Temp\slim-paste-42\clipboard-0.png".into()),
                text: Some("also on the clipboard".into()),
            },
        );
        assert_eq!(
            effects,
            vec![
                Effect::Send(UiCommand::AttachImage(
                    r"C:\Temp\slim-paste-42\clipboard-0.png".into()
                )),
                Effect::RequestRender,
            ]
        );
        assert!(state.composer.payload().is_empty());
    }

    #[test]
    fn clipboard_pull_without_an_image_pastes_the_text() {
        let mut state = AppState::new();
        let effects = reduce(
            &mut state,
            Action::ClipboardPull {
                image: None,
                text: Some("pasted text".into()),
            },
        );
        assert_eq!(effects, vec![Effect::RequestRender]);
        assert_eq!(state.composer.payload(), "pasted text");

        // An empty clipboard stays a no-op: no empty paste segment.
        let mut empty = AppState::new();
        let effects = reduce(
            &mut empty,
            Action::ClipboardPull {
                image: None,
                text: None,
            },
        );
        assert_eq!(effects, vec![Effect::RequestRender]);
        assert!(empty.composer.payload().is_empty());
    }

    #[test]
    fn clipboard_image_never_lands_on_the_login_api_key_field() {
        // The login field is a text target: an image on the clipboard must not
        // hijack the secret input.
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        for _ in 0..2 {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            );
        }
        reduce(&mut state, Action::Key(enter()));

        let effects = reduce(
            &mut state,
            Action::ClipboardPull {
                image: Some("clipboard-0.png".into()),
                text: Some("needle-secret".into()),
            },
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Send(UiCommand::AttachImage(_)))),
            "an image must not replace the API key paste"
        );
        let saved = reduce(&mut state, Action::Key(enter()));
        assert!(matches!(
            saved.first(),
            Some(Effect::Send(UiCommand::SaveApiKey { api_key, .. }))
                if api_key.expose() == "needle-secret"
        ));
    }

    #[test]
    fn clipboard_image_is_ignored_under_a_stacked_modal() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenCodeGo);
        state.authenticated = true;
        state.composer.insert_text("/models");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.model_overlay.is_some());

        let effects = reduce(
            &mut state,
            Action::ClipboardPull {
                image: Some("clipboard-0.png".into()),
                text: None,
            },
        );
        assert_eq!(effects, vec![Effect::RequestRender]);
    }

    #[test]
    fn clipboard_image_failures_are_visible_notifications() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::ClipboardImageFailed {
                message: "Clipboard image: bitmap is not supported".into(),
            },
        );
        assert_eq!(
            state.notifications.last().map(|notice| notice.as_str()),
            Some("Clipboard image: bitmap is not supported")
        );
    }

    #[test]
    fn opencode_models_refresh_and_select_dynamic_catalog() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenCodeGo);
        state.authenticated = true;
        state.composer.insert_text("/models");

        let effects = reduce(&mut state, Action::Key(enter()));

        assert!(effects.contains(&Effect::Send(UiCommand::RefreshOpenCodeModels)));
        assert!(effects.contains(&Effect::Send(UiCommand::RefreshClinePassModels)));
        assert!(effects.contains(&Effect::Send(UiCommand::RefreshCommandCodeModels)));
        assert!(state.model_overlay.is_some());
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::OpenCodeCatalogLoaded {
                models: vec![
                    OpenCodeModelView {
                        id: "deepseek-v4-flash".into(),
                        name: "DeepSeek V4 Flash".into(),
                        context_window_tokens: 1_000_000,
                        max_output_tokens: 384_000,
                        reasoning_levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
                        accepts_images: false,
                    },
                    OpenCodeModelView {
                        id: "glm-5.3".into(),
                        name: "GLM 5.3".into(),
                        context_window_tokens: 202_752,
                        max_output_tokens: 131_072,
                        reasoning_levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
                        accepts_images: false,
                    },
                ],
                source: OpenCodeCatalogSource::Live,
            }),
        );
        // Filter down to the OpenCode group and select GLM.
        for character in "glm".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );

        let effects = reduce(&mut state, Action::Key(enter()));

        assert!(effects.contains(&Effect::Send(UiCommand::SetOpenCodeModel {
            model: "glm-5.3".into(),
            effort: ReasoningEffort::High,
        })));
    }

    #[test]
    fn textual_model_command_selects_clinepass_model() {
        // G249: `/model <id>` works while ClinePass is connected, validating
        // against the static catalog and sending SetClinePassModel (High).
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::ClinePass);
        state.authenticated = true;
        state.composer.insert_text("/model cline-pass/kimi-k3");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(
            effects.contains(&Effect::Send(UiCommand::SetClinePassModel {
                model: "cline-pass/kimi-k3".into(),
                effort: ReasoningEffort::High,
            }))
        );
    }

    #[test]
    fn textual_model_command_rejects_unknown_clinepass_model() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::ClinePass);
        state.authenticated = true;
        state.composer.insert_text("/model not-a-model");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(
            !effects.contains(&Effect::Send(UiCommand::SetClinePassModel {
                model: "not-a-model".into(),
                effort: ReasoningEffort::High,
            }))
        );
        // Unknown selection still surfaces a notification and does not hang.
        assert!(effects.contains(&Effect::RequestRender));
    }

    #[test]
    fn unknown_model_command_preserves_draft_for_correction() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::ClinePass);
        state.authenticated = true;
        state.composer.insert_text("/model not-a-model");
        let _ = reduce(&mut state, Action::Key(enter()));
        assert_eq!(state.composer.payload(), "/model not-a-model");
        assert!(
            state.notifications.iter().any(|notification| notification
                .as_str()
                .contains("Modelo ClinePass desconhecido")),
            "rejection must stay visible: {:?}",
            state.notifications
        );
    }

    #[test]
    fn command_code_login_collects_secret_and_emits_typed_command() {
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        for _ in 0..4 {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            );
        }
        reduce(&mut state, Action::Key(enter()));
        for character in "cmd-secret".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }

        let effects = reduce(&mut state, Action::Key(enter()));

        assert!(matches!(
            effects.as_slice(),
            [
                Effect::Send(UiCommand::SaveApiKey {
                    provider: LoginProvider::CommandCode,
                    api_key,
                }),
                Effect::RequestRender,
            ] if api_key.expose() == "cmd-secret"
        ));
        assert!(!format!("{state:?}").contains("cmd-secret"));
    }

    #[test]
    fn login_command_code_slash_opens_api_key_stage() {
        let mut state = AppState::new();
        state.composer.insert_text("/login command-code");
        reduce(&mut state, Action::Key(enter()));
        let overlay = state.login_overlay.expect("overlay");
        assert_eq!(overlay.provider(), LoginProvider::CommandCode);
        assert!(matches!(overlay.stage, LoginStage::ApiKey(_)));
    }

    #[test]
    fn textual_model_command_selects_command_code_model() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::CommandCode);
        state.authenticated = true;
        state
            .composer
            .insert_text("/model deepseek/deepseek-v4.1-flash");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(
            effects.contains(&Effect::Send(UiCommand::SetCommandCodeModel {
                model: "deepseek/deepseek-v4.1-flash".into(),
                effort: ReasoningEffort::High,
            }))
        );
    }

    #[test]
    fn ctrl_p_does_not_open_palette_over_login() {
        // G250: palette is gated behind the stacked overlays.
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.login_overlay.is_some());
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        );
        assert!(
            state.palette_query.is_none(),
            "Ctrl+P must not open the palette over a modal"
        );
    }

    #[test]
    fn paste_does_not_edit_composer_under_model_overlay() {
        // G250: paste never reaches the composer beneath a stacked modal.
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenCodeGo);
        state.authenticated = true;
        state.composer.insert_text("/models");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.model_overlay.is_some());
        reduce(&mut state, Action::Paste("snuck in".into()));
        assert!(
            state.composer.payload().is_empty(),
            "paste must be ignored under a modal overlay"
        );
    }

    #[test]
    fn ctrl_l_opens_model_overlay() {
        // G250: Ctrl+L is the missing spec §17.2 binding for the model overlay.
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenAiCodex);
        state.authenticated = true;
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
        );
        assert!(
            state.model_overlay.is_some(),
            "Ctrl+L must open the model overlay"
        );
        assert!(effects.contains(&Effect::Send(UiCommand::RefreshOpenCodeModels)));
    }

    #[test]
    fn ctrl_t_toggles_todo_dock() {
        // G250: Ctrl+T is the missing spec §17.2 binding for the Todo dock.
        let mut state = AppState::new();
        assert!(!state.todo_dock_open);
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
        );
        assert!(state.todo_dock_open);
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
        );
        assert!(!state.todo_dock_open);
    }

    #[test]
    fn model_command_requires_effort_before_updating_runtime() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenAiCodex);
        state.authenticated = true;
        state.composer.insert_text("/model");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.model_overlay.is_some());
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        reduce(&mut state, Action::Key(enter()));
        assert!(
            state.model_overlay.is_some(),
            "parent model overlay survives the effort step (G239)"
        );
        assert!(state.effort_overlay.is_some());
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::SetModel {
            model: ModelAlias::Terra,
            effort: ReasoningEffort::High,
            fast: false,
        })));
    }

    #[test]
    fn effort_esc_returns_to_model_overlay_on_the_same_model() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenAiCodex);
        state.authenticated = true;
        state.composer.insert_text("/model");
        reduce(&mut state, Action::Key(enter()));
        // Rows: Header, Sol, Terra — one Down lands on Terra.
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        reduce(&mut state, Action::Key(enter()));
        assert!(state.effort_overlay.is_some());
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        let overlay = state.model_overlay.expect("parent overlay survives Esc");
        assert_eq!(
            overlay.selected, 2,
            "highlight stays on the chosen model row"
        );
    }

    #[test]
    fn altgr_printable_slash_reaches_composer() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(
                KeyCode::Char('/'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
        );
        assert_eq!(state.composer.payload(), "/");
    }

    #[test]
    fn signed_out_prompt_preserves_draft_without_sending() {
        let mut state = AppState::new();
        state.composer.insert_text("keep this prompt");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert_eq!(state.composer.payload(), "keep this prompt");
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
        assert_eq!(
            state.notifications.last().map(|notice| notice.as_str()),
            Some("Nenhum provedor conectado. Use /login.")
        );
    }

    #[test]
    fn enter_during_active_run_enqueues_draft_instead_of_sending() {
        // G244 (§7.4): submit while a run is active queues the draft as a
        // visible QueuedUser block instead of silently dropping it.
        let mut state = AppState::new();
        state.working = true;
        state.composer.insert_text("keep this draft");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert_eq!(state.composer.payload(), "");
        assert_eq!(state.queued_prompts.len(), 1);
        assert_eq!(
            state.queued_prompts.front().map(String::as_str),
            Some("keep this draft")
        );
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
    }

    #[test]
    fn ctrl_c_with_screen_selection_copies_instead_of_shutdown() {
        let mut state = AppState::new();
        state.selection_text = "copied from the frame".into();
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(!state.shutdown);
        assert!(effects.contains(&Effect::CopyToClipboard("copied from the frame".into())));
    }

    #[test]
    fn right_click_copies_selection_or_requests_paste() {
        let mut state = AppState::new();
        let paste = reduce(&mut state, Action::MouseSecondary);
        assert_eq!(paste, vec![Effect::PasteFromClipboard]);
        state.selection_text = "block".into();
        let copy = reduce(&mut state, Action::MouseSecondary);
        assert!(copy.contains(&Effect::CopyToClipboard("block".into())));
    }

    #[test]
    fn clipboard_confirmation_is_success_only_coalesced_and_transient() {
        let mut state = AppState::new();
        state.selection_text = "selected".into();
        reduce(&mut state, Action::MouseSecondary);
        assert!(
            state.notifications.is_empty(),
            "scheduling a copy is not success"
        );
        state.clock.elapsed_ms = 100;
        reduce(&mut state, Action::ClipboardCompleted { success: true });
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(state.notifications[0].message, "Copiado");
        state.clock.elapsed_ms = 200;
        reduce(&mut state, Action::ClipboardCompleted { success: true });
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(state.notifications[0].created_ms, 200);
        state.clock.elapsed_ms = 200 + crate::app::INFO_TOAST_TTL_MS;
        state.prune_notifications();
        assert!(state.notifications.is_empty());
        reduce(&mut state, Action::ClipboardCompleted { success: false });
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(
            state.notifications[0].message,
            "Área de transferência indisponível"
        );
    }

    #[test]
    fn click_without_drag_clears_the_screen_selection() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::StartScreenSelection {
                x: 4,
                y: 1,
                area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
            },
        );
        assert!(state.selection.is_some());
        reduce(&mut state, Action::FinishScreenSelection);
        assert_eq!(state.selection, None);
    }

    #[test]
    fn empty_selection_neither_pastes_nor_cancels_and_resize_clears_it() {
        let mut state = AppState::new();
        state.working = true;
        reduce(
            &mut state,
            Action::StartScreenSelection {
                x: 4,
                y: 1,
                area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
            },
        );
        reduce(&mut state, Action::UpdateScreenSelection { x: 8, y: 1 });
        assert_eq!(
            reduce(&mut state, Action::MouseSecondary),
            vec![Effect::RequestRender]
        );
        assert_eq!(
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            ),
            vec![Effect::RequestRender]
        );
        assert_eq!(
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            ),
            vec![Effect::RequestRender]
        );
        assert!(state.selection.is_none());
        reduce(
            &mut state,
            Action::StartScreenSelection {
                x: 4,
                y: 1,
                area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
            },
        );
        reduce(&mut state, Action::Resize);
        assert!(state.selection.is_none() && state.selection_area.is_none());
    }

    #[test]
    fn new_content_preserves_coordinate_selection_snapshot() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::StartScreenSelection {
                x: 4,
                y: 1,
                area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
            },
        );
        state.selection_text = "old content".into();
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::AssistantDelta {
                text: "new content".into(),
            }),
        );
        assert!(state.selection.is_some() && state.selection_area.is_some());
        assert_eq!(state.selection_text, "old content");
    }

    #[test]
    fn ctrl_c_idle_with_empty_draft_requests_shutdown_once() {
        let mut state = AppState::new();
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(state.shutdown);
        assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
    }

    #[test]
    fn ctrl_c_while_working_cancels_run_and_stays_alive() {
        let mut state = AppState::new();
        state.working = true;
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(!state.shutdown);
        assert!(effects.contains(&Effect::Send(UiCommand::CancelRun)));
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    #[test]
    fn login_api_key_esc_returns_to_provider_list() {
        let mut state = AppState::new();
        state.composer.insert_text("/login opencode");
        reduce(&mut state, Action::Key(enter()));
        assert!(matches!(
            state.login_overlay.as_ref().map(|overlay| &overlay.stage),
            Some(LoginStage::ApiKey(_))
        ));
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        let overlay = state
            .login_overlay
            .as_ref()
            .expect("Esc back keeps the login overlay open");
        assert!(
            matches!(overlay.stage, LoginStage::Providers),
            "API-key Esc returns to the provider list"
        );
        assert!(!state.shutdown);
    }

    #[test]
    fn login_providers_esc_closes_overlay() {
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(state.login_overlay.is_none());
        assert!(!state.shutdown);
    }

    #[test]
    fn ctrl_c_on_model_overlay_still_exits_when_idle() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.auth_provider = Some(LoginProvider::OpenAiCodex);
        state.composer.insert_text("/model");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.model_overlay.is_some());
        let effects = reduce(&mut state, Action::Key(ctrl_c()));
        assert!(state.shutdown);
        assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
    }

    #[test]
    fn ctrl_c_on_palette_still_exits_when_idle() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        );
        assert!(state.palette_query.is_some());
        let effects = reduce(&mut state, Action::Key(ctrl_c()));
        assert!(state.shutdown);
        assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
    }

    #[test]
    fn ctrl_c_etx_idle_with_empty_draft_requests_shutdown() {
        let mut state = AppState::new();
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('\u{3}'), KeyModifiers::NONE)),
        );
        assert!(state.shutdown);
        assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
    }

    #[test]
    fn ctrl_c_idle_with_draft_clears_before_exit() {
        let mut state = AppState::new();
        state.composer.insert_text("do not lose this silently");
        let first = reduce(&mut state, Action::Key(ctrl_c()));
        assert!(!state.shutdown);
        assert!(state.composer.payload().is_empty());
        assert!(
            first
                .iter()
                .all(|effect| !matches!(effect, Effect::Send(UiCommand::Shutdown))),
            "first Ctrl+C with a draft must not quit"
        );
        let second = reduce(&mut state, Action::Key(ctrl_c()));
        assert!(state.shutdown);
        assert!(second.contains(&Effect::Send(UiCommand::Shutdown)));
    }

    #[test]
    fn backtab_cycles_mode_through_boundary() {
        let mut state = AppState::new();
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
        );
        assert!(effects.contains(&Effect::Send(UiCommand::SetMode(
            slim_core::OperatingMode::ReadOnly
        ))));
    }

    #[test]
    fn mode_slash_command_cycles_like_shift_tab() {
        let mut state = AppState::new();
        state.composer.insert_text("/mode");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::SetMode(
            slim_core::OperatingMode::ReadOnly
        ))));
        assert!(state.composer.payload().is_empty());
    }

    #[test]
    fn resume_command_dispatches_only_while_idle() {
        let mut state = AppState::new();
        state.composer.insert_text("/resume");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::ResumePrevious)));

        state.working = true;
        for character in "/re".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
        assert!(state.slash_suggestions.is_some());
        let effects = reduce(&mut state, Action::Key(enter()));
        assert_eq!(state.composer.payload().trim(), "/resume");
        assert!(!effects.contains(&Effect::Send(UiCommand::ResumePrevious)));
    }

    #[test]
    fn compact_command_keeps_optional_instructions_out_of_user_prompt() {
        let mut state = AppState::new();
        state.authenticated = true;
        state
            .composer
            .insert_text("/compact preserve build evidence");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::Compact {
            instructions: "preserve build evidence".into(),
        })));
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
    }

    #[test]
    fn compact_command_during_run_goes_to_safe_boundary_not_prompt_queue() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.working = true;
        state.composer.insert_text("/compact");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::Compact {
            instructions: String::new(),
        })));
        assert!(state.queued_prompts.is_empty());
    }

    #[test]
    fn paste_too_large_is_visible_error_not_silent_drop() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::Paste("x".repeat(crate::composer::MAX_DRAFT_CHARS + 1)),
        );
        assert!(!state.notifications.is_empty());
    }

    #[test]
    fn info_toasts_expire_after_five_seconds_on_tick() {
        let mut state = AppState::new();
        state.clock.elapsed_ms = 1_000;
        state.apply_event(UiEvent::Notification {
            message: "Connected: OpenAI Codex — ChatGPT Plus/Pro".into(),
        });
        assert_eq!(state.notifications.len(), 1);

        reduce(
            &mut state,
            Action::Tick(FrameClock {
                frame: 60,
                elapsed_ms: 6_000,
            }),
        );
        assert!(
            state.notifications.is_empty(),
            "info toast must expire at 5s: {:?}",
            state.notifications
        );
    }

    #[test]
    fn mode_command_via_alias_sends_set_model() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenAiCodex);
        state.authenticated = true;
        state.composer.insert_text("/model luna");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::SetModel {
            model: ModelAlias::Luna,
            effort: ReasoningEffort::High,
            fast: false,
        })));
        assert!(state.composer.payload().is_empty());
    }
    #[test]
    fn mcp_command_opens_overlay_and_starts_watch() {
        let mut state = AppState::new();
        state.composer.insert_text("/mcp");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(state.mcp_overlay.is_some());
        assert!(effects.contains(&Effect::Send(UiCommand::McpRefresh)));
        assert!(effects.contains(&Effect::Send(UiCommand::McpWatch { on: true })));
        // Esc closes the overlay and stops the worker-side watch.
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(state.mcp_overlay.is_none());
        assert!(effects.contains(&Effect::Send(UiCommand::McpWatch { on: false })));
    }

    #[test]
    fn mcp_overlay_remove_requires_confirmation() {
        let mut state = AppState::new();
        state.mcp_servers = vec![crate::api::McpServerView {
            name: "fs".into(),
            transport: "stdio",
            target: "npx fs".into(),
            status: crate::api::McpStatusView::Ready,
            tools: Some(3),
            error: None,
        }];
        state.composer.insert_text("/mcp");
        reduce(&mut state, Action::Key(enter()));
        // d arms confirmation; nothing is sent yet.
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        );
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::McpRemove { .. }))));
        // A non-confirming key cancels the armed removal.
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        );
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        );
        let _ = effects;
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        );
        assert!(effects.contains(&Effect::Send(UiCommand::McpRemove { name: "fs".into() })));
    }

    #[test]
    fn mcp_overlay_enter_tests_selected_server() {
        let mut state = AppState::new();
        state.mcp_servers = vec![
            crate::api::McpServerView {
                name: "fs".into(),
                transport: "stdio",
                target: "npx fs".into(),
                status: crate::api::McpStatusView::Disconnected,
                tools: None,
                error: None,
            },
            crate::api::McpServerView {
                name: "web".into(),
                transport: "http",
                target: "https://mcp.example.com".into(),
                status: crate::api::McpStatusView::Disconnected,
                tools: None,
                error: None,
            },
        ];
        state.composer.insert_text("/mcp");
        reduce(&mut state, Action::Key(enter()));
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::McpTest { name: "web".into() })));
    }

    #[test]
    fn mcp_add_parses_stdio_and_http_forms() {
        let mut state = AppState::new();
        state.composer.insert_text("/mcp add fs npx -y fs-server");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::McpAdd {
            name: "fs".into(),
            command: Some("npx".into()),
            args: vec!["-y".into(), "fs-server".into()],
            url: None,
            global: false,
        })));

        let mut state = AppState::new();
        state
            .composer
            .insert_text("/mcp add web --url https://mcp.example.com --global");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::McpAdd {
            name: "web".into(),
            command: None,
            args: Vec::new(),
            url: Some("https://mcp.example.com".into()),
            global: true,
        })));
    }

    fn mcp_server(name: &str) -> crate::api::McpServerView {
        crate::api::McpServerView {
            name: name.into(),
            transport: "stdio",
            target: "cmd".into(),
            status: crate::api::McpStatusView::Disconnected,
            tools: None,
            error: None,
        }
    }

    fn open_mcp(state: &mut AppState) {
        state.composer.insert_text("/mcp");
        reduce(state, Action::Key(enter()));
    }

    #[test]
    fn mcp_overlay_ignores_modified_destructive_keys() {
        let mut state = AppState::new();
        state.mcp_servers = vec![mcp_server("fs")];
        open_mcp(&mut state);
        for code in [
            KeyCode::Char('r'),
            KeyCode::Char('x'),
            KeyCode::Char('d'),
            KeyCode::Char('y'),
        ] {
            let effects = reduce(
                &mut state,
                Action::Key(KeyEvent::new(code, KeyModifiers::CONTROL)),
            );
            assert!(
                effects.iter().all(|effect| !matches!(
                    effect,
                    Effect::Send(UiCommand::McpReconnect { .. })
                        | Effect::Send(UiCommand::McpDisconnect { .. })
                        | Effect::Send(UiCommand::McpRemove { .. })
                )),
                "Ctrl+{code:?} must not fire an MCP action"
            );
        }
        assert!(state.mcp_overlay.is_some());
    }

    #[test]
    fn mcp_command_during_run_opens_overlay_instead_of_queuing() {
        let mut state = AppState::new();
        state.working = true;
        state.composer.insert_text("/mcp");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(state.mcp_overlay.is_some());
        assert!(state.queued_prompts.is_empty());
        assert!(effects.contains(&Effect::Send(UiCommand::McpRefresh)));
        assert!(effects.contains(&Effect::Send(UiCommand::McpWatch { on: true })));
    }

    #[test]
    fn mcp_mutating_subcommand_during_run_is_dispatched_not_queued() {
        // The worker's active-run catch-all rejects it with a notification;
        // what must never happen is the text reaching the prompt queue.
        let mut state = AppState::new();
        state.working = true;
        state.composer.insert_text("/mcp reconnect fs");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(state.queued_prompts.is_empty());
        assert!(effects.contains(&Effect::Send(UiCommand::McpReconnect { name: "fs".into() })));
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
    }

    #[test]
    fn mcp_overlay_open_clears_search() {
        // Search captures keys before overlays do; the palette (Ctrl+P) is
        // the only route that can stack /mcp over it — the overlay must win.
        let mut state = AppState::new();
        state.search = Some(crate::inspector::SearchState::default());
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        );
        for character in "mcp".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
        reduce(&mut state, Action::Key(enter()));
        assert!(state.search.is_none());
        assert!(state.mcp_overlay.is_some());
    }

    #[test]
    fn mcp_selection_follows_server_name_across_snapshots() {
        let mut state = AppState::new();
        state.mcp_servers = vec![mcp_server("a"), mcp_server("b"), mcp_server("c")];
        open_mcp(&mut state);
        // Select "c" (index 2).
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        assert_eq!(state.mcp_overlay.as_ref().unwrap().selected, 2);
        // Snapshot with "a" removed: "c" now sits at index 1 and the cursor
        // must follow the name, not the numeric position.
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::McpServersChanged {
                servers: vec![mcp_server("b"), mcp_server("c")],
            }),
        );
        let overlay = state.mcp_overlay.as_ref().unwrap();
        assert_eq!(overlay.selected, 1);
        assert_eq!(state.mcp_servers[overlay.selected].name, "c");
        // Selected server removed entirely: clamp into range.
        reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::McpServersChanged {
                servers: vec![mcp_server("b")],
            }),
        );
        assert_eq!(state.mcp_overlay.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn mcp_malformed_subcommand_shows_usage_and_keeps_draft() {
        let mut state = AppState::new();
        state.composer.insert_text("/mcp frobnicate");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(state.composer.payload().contains("/mcp frobnicate"));
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
        assert!(state
            .notifications
            .iter()
            .any(|notification| notification.message.starts_with("Uso: /mcp")));
    }

    #[test]
    fn parallel_tool_activity_keeps_running_call_until_each_identity_ends() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::RunStarted {
            run_id: 1,
            max_mutating_tool_calls: 8,
            max_read_tool_calls: 8,
            max_turns: 4,
        });
        let batch_a = crate::api::ToolBatchId("batch-a".into());
        let call_a = crate::api::ToolCallId("call-a".into());
        let batch_b = crate::api::ToolBatchId("batch-b".into());
        let call_b = crate::api::ToolCallId("call-b".into());

        state.apply_event(UiEvent::ToolAdmitted {
            batch_id: batch_a.clone(),
            call_id: call_a.clone(),
            name: "search".into(),
        });
        assert_eq!(
            state.activity.as_ref().map(|activity| &activity.phase),
            Some(&ActivityPhase::QueuedTool("search".into()))
        );
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch_a.clone(),
            call_id: call_a.clone(),
            name: "search".into(),
            arguments_summary: "{}".into(),
        });
        state.apply_event(UiEvent::ToolAdmitted {
            batch_id: batch_b.clone(),
            call_id: call_b.clone(),
            name: "write".into(),
        });
        assert_eq!(
            state.activity.as_ref().map(|activity| &activity.phase),
            Some(&ActivityPhase::RunningTool("search".into()))
        );
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch_b,
            call_id: call_b,
            name: "write".into(),
            arguments_summary: "{}".into(),
        });
        state.apply_event(UiEvent::ToolEnded {
            batch_id: batch_a,
            call_id: call_a,
            name: "search".into(),
            success: true,
            duration_ms: 10,
        });
        assert_eq!(
            state.activity.as_ref().map(|activity| &activity.phase),
            Some(&ActivityPhase::RunningTool("write".into()))
        );
    }

    #[test]
    fn retry_state_is_cleared_by_provider_phase_and_late_retry_is_ignored() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::RunStarted {
            run_id: 7,
            max_mutating_tool_calls: 1,
            max_read_tool_calls: 1,
            max_turns: 1,
        });
        state.apply_event(UiEvent::RetryScheduled {
            attempt: 1,
            limit: 3,
            wait_ms: 250,
            reason: Some("timeout".into()),
        });
        assert!(state.retry.is_some());
        state.apply_event(UiEvent::ProviderPhaseChanged {
            phase: slim_core::ProviderPhase::HeadersReceived,
            label: "headers".into(),
            elapsed_ms: 11,
        });
        assert!(state.retry.is_none());
        state.apply_event(UiEvent::RunCompleted { run_id: 7 });
        state.apply_event(UiEvent::RetryScheduled {
            attempt: 2,
            limit: 3,
            wait_ms: 500,
            reason: None,
        });
        assert!(state.retry.is_none());
    }

    #[test]
    fn explicit_cancel_pauses_queue_until_deliberate_resume() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::RunStarted {
            run_id: 3,
            max_mutating_tool_calls: 1,
            max_read_tool_calls: 1,
            max_turns: 1,
        });
        state.enqueue_queued_prompt("first".into());
        state.enqueue_queued_prompt("second".into());
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert_eq!(
            state.cancellation.map(|cancellation| cancellation.phase),
            Some(CancellationPhase::Requested)
        );
        let effects = reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::RunStopped {
                run_id: 3,
                message: "cancelled".into(),
            }),
        );
        assert!(state.queue_paused);
        assert_eq!(state.queue_len(), 2);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));

        state.composer.insert_text("/queue resume");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(!state.queue_paused);
        assert!(effects.contains(&Effect::Send(UiCommand::SendPrompt("first".into()))));
        assert_eq!(state.queue_len(), 1);

        state.composer.insert_text("/queue edit 1");
        reduce(&mut state, Action::Key(enter()));
        assert_eq!(state.composer.payload(), "second");
        assert_eq!(state.queue_len(), 0);
    }

    #[test]
    fn explicit_cancel_keeps_queue_paused_even_if_completion_wins_race() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::RunStarted {
            run_id: 4,
            max_mutating_tool_calls: 1,
            max_read_tool_calls: 1,
            max_turns: 1,
        });
        state.enqueue_queued_prompt("after cancel".into());
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        let effects = reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::RunCompleted { run_id: 4 }),
        );
        assert!(state.queue_paused);
        assert_eq!(state.queue_len(), 1);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
        assert_eq!(
            state.last_execution.as_ref().map(|summary| summary.outcome),
            Some(RunOutcomeKind::Completed)
        );
    }

    #[test]
    fn execution_summary_survives_next_run_and_tracks_outcome() {
        let mut state = AppState::new();
        state.clock.elapsed_ms = 100;
        state.apply_event(UiEvent::RunStarted {
            run_id: 1,
            max_mutating_tool_calls: 1,
            max_read_tool_calls: 1,
            max_turns: 1,
        });
        state.clock.elapsed_ms = 275;
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        let summary = state.last_execution.as_ref().expect("completed summary");
        assert_eq!(summary.run_id, 1);
        assert_eq!(summary.duration_ms, 175);
        assert_eq!(summary.outcome, RunOutcomeKind::Completed);
        state.apply_event(UiEvent::RunStarted {
            run_id: 2,
            max_mutating_tool_calls: 1,
            max_read_tool_calls: 1,
            max_turns: 1,
        });
        assert_eq!(
            state.last_execution.as_ref().map(|summary| summary.run_id),
            Some(1)
        );
        state.clock.elapsed_ms = 300;
        state.apply_event(UiEvent::RunStopped {
            run_id: 2,
            message: "stopped".into(),
        });
        assert_eq!(state.execution_history.len(), 2);
        assert_eq!(
            state.last_execution.as_ref().map(|summary| summary.outcome),
            Some(RunOutcomeKind::Interrupted)
        );
    }

    #[test]
    fn todo_dock_preference_survives_todo_updates() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::TodoChanged {
            items: vec![crate::api::TodoItemView {
                title: "pending".into(),
                status: crate::api::TodoItemStatus::Pending,
            }],
        });
        assert!(state.todo_dock_open);
        reduce(&mut state, Action::ToggleTodoDock);
        assert!(!state.todo_dock_open);
        state.apply_event(UiEvent::TodoChanged {
            items: vec![crate::api::TodoItemView {
                title: "still pending".into(),
                status: crate::api::TodoItemStatus::Pending,
            }],
        });
        assert!(!state.todo_dock_open);
    }

    #[test]
    fn confirmed_setting_records_effective_changes_only() {
        let mut state = AppState::new();
        state.clock.elapsed_ms = 10;
        state.apply_event(UiEvent::ModeChanged {
            mode: slim_core::OperatingMode::Auto,
        });
        assert!(state.confirmed_setting.is_none());
        state.apply_event(UiEvent::ModeChanged {
            mode: slim_core::OperatingMode::ReadOnly,
        });
        assert_eq!(state.confirmed_setting, Some((ConfirmedSetting::Mode, 10)));
        state.clock.elapsed_ms = 20;
        state.apply_event(UiEvent::EffortChanged {
            effort: ReasoningEffort::Low,
        });
        assert_eq!(
            state.confirmed_setting,
            Some((ConfirmedSetting::Effort, 20))
        );
        state.clock.elapsed_ms = 30;
        state.apply_event(UiEvent::ModelChanged {
            model: "gpt-5.6-terra".into(),
        });
        assert_eq!(state.confirmed_setting, Some((ConfirmedSetting::Model, 30)));
    }

    #[test]
    fn notification_coalescing_preserves_priority_and_history() {
        let mut state = AppState::new();
        state.clock.elapsed_ms = 1;
        state.push_notification("same".into());
        state.clock.elapsed_ms = 2;
        state.push_notification("same".into());
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(state.notifications[0].repeat_count, 2);
        assert_eq!(state.notification_history()[0].repeat_count, 2);
        state.push_notification_with_priority("same".into(), NotificationPriority::Warning);
        state.push_notification_with_priority("failure".into(), NotificationPriority::Error);
        let toast = state.visible_toast_tail(3);
        assert_eq!(toast.len(), 1);
        assert_eq!(toast[0].message, "failure");
        assert_eq!(toast[0].priority, NotificationPriority::Error);
    }

    #[test]
    fn thinking_preview_retention_uses_thinking_start_and_releases_at_boundary() {
        let mut state = AppState::new();
        state.clock.elapsed_ms = 10;
        state.apply_event(UiEvent::RunStarted {
            run_id: 9,
            max_mutating_tool_calls: 1,
            max_read_tool_calls: 1,
            max_turns: 1,
        });
        state.clock.elapsed_ms = 20;
        state.apply_event(UiEvent::ThinkingStarted);
        state.clock.elapsed_ms = 50;
        state.apply_event(UiEvent::ThinkingDelta {
            text: "reason".into(),
        });
        let thinking_id = state
            .blocks()
            .iter()
            .find(|block| matches!(block.kind(), crate::block::BlockKind::Thinking(_)))
            .map(|block| block.id.clone())
            .expect("thinking block");
        let thinking = state
            .blocks()
            .iter()
            .find(|block| block.id == thinking_id)
            .unwrap();
        assert_eq!(thinking.started_ms, Some(20));
        state.clock.elapsed_ms = 60;
        state.apply_event(UiEvent::ThinkingEnded);
        assert!(state
            .blocks()
            .iter()
            .find(|block| block.id == thinking_id)
            .is_some_and(|block| block.preview_retained));
        state.clock.elapsed_ms = 70;
        state.apply_event(UiEvent::AssistantDelta {
            text: "answer".into(),
        });
        assert!(state
            .blocks()
            .iter()
            .find(|block| block.id == thinking_id)
            .is_some_and(|block| !block.preview_retained));
    }

    #[test]
    fn approval_decision_waits_until_runtime_marks_content_accessible() {
        let mut state = AppState::new();
        let request_id = crate::api::InteractionRequestId("approval-1".into());
        state.apply_event(UiEvent::ApprovalRequired {
            request_id: request_id.clone(),
            summary: "run command".into(),
            persisted: false,
        });
        let blocked = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        );
        assert!(!blocked
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::Approve { .. }))));
        reduce(&mut state, Action::SetApprovalContentAccessible(true));
        let approved = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        );
        assert!(approved
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::Approve { request_id: id }) if id == &request_id)));
    }
}

#[cfg(test)]
mod palette_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{reduce, Action};
    use crate::app::AppState;

    fn ctrl_p() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
    }

    #[test]
    fn palette_filters_and_submits_top_match() {
        let mut state = AppState::new();
        state.auth_provider = Some(crate::api::LoginProvider::OpenAiCodex);
        state.authenticated = true;
        reduce(&mut state, Action::Key(ctrl_p()));
        assert!(state.palette_query.is_some());

        for character in "mod".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
        assert_eq!(state.palette_query.as_deref(), Some("mod"));

        // Top match is "/model": submitted as a slash command and executed.
        let _ = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(state.palette_query.is_none());
        assert!(
            state.model_overlay.is_some(),
            "palette submit must execute /model (G243)"
        );
    }

    #[test]
    fn palette_query_without_slash_matches_and_executes_login() {
        let mut state = AppState::new();
        reduce(&mut state, Action::Key(ctrl_p()));
        for character in "log".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
        let _ = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(
            state.login_overlay.is_some(),
            "bare 'log' must run /login without the leading slash"
        );
    }

    #[test]
    fn esc_dismisses_palette_without_submitting() {
        let mut state = AppState::new();
        reduce(&mut state, Action::Key(ctrl_p()));
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(state.palette_query.is_none());
    }

    #[test]
    fn palette_matches_by_substring_not_only_prefix() {
        let mut state = AppState::new();
        reduce(&mut state, Action::Key(ctrl_p()));
        for character in "agn".chars() {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
        let _ = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(
            state.palette_query.is_none(),
            "enter must submit the substring match"
        );
        assert_eq!(
            state.inspector.active,
            Some(crate::inspector::InspectorKind::Diagnostics),
            "bare 'agn' must run /diagnostics"
        );
    }

    #[test]
    fn f1_toggles_palette_open_and_closed() {
        let mut state = AppState::new();
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
        );
        assert!(state.palette_query.is_some(), "F1 must open palette");

        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
        );
        assert!(state.palette_query.is_none(), "F1 must toggle palette off");
    }

    #[test]
    fn help_slash_command_shows_shortcuts_toast() {
        let mut state = AppState::new();
        state.composer.insert_text("/help");
        let _ = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(
            state
                .notifications
                .iter()
                .any(|n| n.message.contains("F1 / Ctrl+P")),
            "help command must display shortcuts notification"
        );
    }

    #[test]
    fn palette_selection_clamps_when_narrowed() {
        let mut state = AppState::new();
        state.palette_query = Some(String::new());
        state.palette_selected = 100;
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        let total = super::palette_matches("").len();
        assert!(state.palette_selected < total, "must clamp selection");
    }

    #[test]
    fn search_selection_clamps_when_matches_are_fewer() {
        let mut state = AppState::new();
        state.search = Some(crate::inspector::SearchState {
            query: "test".into(),
            selected: 99,
            filter: crate::inspector::SearchFilter::All,
        });
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        assert_eq!(state.search.as_ref().unwrap().selected, 0);
    }
}

#[cfg(test)]
mod slash_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{reduce, slash_matches, slash_matches_with_skills, Action, Effect};
    use crate::api::UiCommand;
    use crate::app::AppState;

    fn type_text(state: &mut AppState, text: &str) {
        for character in text.chars() {
            reduce(
                state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
    }

    fn press_key(state: &mut AppState, code: KeyCode) {
        reduce(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn slash_opens_at_start_and_mid_prompt() {
        let mut state = AppState::new();
        type_text(&mut state, "/");
        assert!(
            state.slash_suggestions.is_some(),
            "bare slash lists every command"
        );

        let mut state = AppState::new();
        type_text(&mut state, "explain this /mo");
        let suggestions = state
            .slash_suggestions
            .as_ref()
            .expect("mid-prompt slash opens");
        assert_eq!(suggestions.query, "mo");
    }

    #[test]
    fn slash_opens_on_token_under_cursor_mid_draft() {
        let mut state = AppState::new();
        type_text(&mut state, "run /logi now");
        for _ in 0..5 {
            press_key(&mut state, KeyCode::Left);
        }
        let suggestions = state
            .slash_suggestions
            .as_ref()
            .expect("slash under cursor opens mid-draft");
        assert_eq!(suggestions.query, "logi");
    }

    #[test]
    fn tab_completes_mid_draft_token_preserving_tail() {
        let mut state = AppState::new();
        type_text(&mut state, "run /logi now");
        for _ in 0..5 {
            press_key(&mut state, KeyCode::Left);
        }
        press_key(&mut state, KeyCode::Tab);
        assert_eq!(state.composer.payload(), "run /login now");
        assert!(state.slash_suggestions.is_none());
    }

    #[test]
    fn moving_cursor_off_slash_token_closes_popup() {
        let mut state = AppState::new();
        type_text(&mut state, "run /lo");
        assert!(state.slash_suggestions.is_some());
        // Caret back onto "run": the token under the cursor no longer starts
        // with `/`, so the popup must close.
        for _ in 0..4 {
            press_key(&mut state, KeyCode::Left);
        }
        assert!(state.slash_suggestions.is_none());
    }

    #[test]
    fn plain_text_never_opens() {
        let mut state = AppState::new();
        type_text(&mut state, "explain this mode");
        assert!(state.slash_suggestions.is_none());
    }

    #[test]
    fn filter_narrows_to_prefix() {
        assert_eq!(slash_matches("lo"), vec!["/login", "/logout"]);
        assert_eq!(slash_matches("logi"), vec!["/login"]);
        assert_eq!(slash_matches("he"), vec!["/help"]);
        assert_eq!(slash_matches("").len(), 14);
        assert!(slash_matches("zzz").is_empty());
    }

    #[test]
    fn discovered_skill_autocompletes_and_dispatches_as_a_prompt() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.set_skill_names_for_test(vec!["review-code".into()]);
        type_text(&mut state, "/rev");
        assert_eq!(
            slash_matches_with_skills(&state, "rev"),
            vec!["/review-code"]
        );
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        type_text(&mut state, "check this change");
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(effects.contains(&Effect::Send(UiCommand::SendPrompt(
            "/review-code check this change".into()
        ))));
    }

    #[test]
    fn tab_completes_selected_command_into_draft() {
        let mut state = AppState::new();
        type_text(&mut state, "run /lo");
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        assert_eq!(state.composer.payload(), "run /logout ");
        assert!(state.slash_suggestions.is_none());
    }

    #[test]
    fn backspace_after_tab_complete_removes_one_grapheme_not_the_draft() {
        let mut state = AppState::new();
        type_text(&mut state, "run /lo");
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
        assert_eq!(state.composer.payload(), "run /logout");
    }

    #[test]
    fn enter_executes_highlighted_command() {
        let mut state = AppState::new();
        state.auth_provider = Some(crate::api::LoginProvider::OpenAiCodex);
        state.authenticated = true;
        type_text(&mut state, "/mo");
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(state.model_overlay.is_some());
        assert!(state.composer.payload().is_empty());
        assert!(state.slash_suggestions.is_none());
    }

    #[test]
    fn esc_dismisses_without_touching_draft() {
        let mut state = AppState::new();
        type_text(&mut state, "/mo");
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(state.slash_suggestions.is_none());
        assert_eq!(state.composer.payload(), "/mo");
    }

    #[test]
    fn editing_reopens_and_closes_the_popup() {
        let mut state = AppState::new();
        type_text(&mut state, "/mode");
        assert!(state.slash_suggestions.is_some());
        for _ in 0..5 {
            reduce(
                &mut state,
                Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            );
        }
        assert!(
            state.slash_suggestions.is_none(),
            "empty draft closes the popup"
        );
    }

    #[test]
    fn paste_with_slash_token_opens() {
        let mut state = AppState::new();
        reduce(&mut state, Action::Paste("run /log".into()));
        let suggestions = state
            .slash_suggestions
            .as_ref()
            .expect("paste triggers the popup too");
        assert_eq!(suggestions.query, "log");
    }
}

#[cfg(test)]
mod queued_prompt_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{reduce, Action, Effect};
    use crate::api::{UiCommand, UiEvent};
    use crate::app::AppState;
    use crate::block::BlockKind;

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn alt_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
    }

    fn run_started(run_id: u64) -> Action {
        Action::UiEventReceived(UiEvent::run_started(run_id))
    }

    fn run_completed(run_id: u64) -> Action {
        Action::UiEventReceived(UiEvent::RunCompleted { run_id })
    }

    fn queued_texts(state: &AppState) -> Vec<String> {
        state
            .blocks()
            .iter()
            .filter_map(|block| match block.kind() {
                BlockKind::QueuedUser(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn type_text(state: &mut AppState, text: &str) {
        for character in text.chars() {
            reduce(
                state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
    }

    #[test]
    fn submit_during_run_enqueues_visible_block_and_clears_composer() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        assert!(state.working, "RunStarted must flip the run active flag");

        type_text(&mut state, "second question");
        let effects = reduce(&mut state, Action::Key(enter()));

        assert_eq!(effects, vec![Effect::RequestRender]);
        assert!(state.composer.payload().is_empty());
        assert_eq!(queued_texts(&state), vec!["second question"]);
        assert_eq!(state.queued_prompts.len(), 1);
    }

    #[test]
    fn second_enqueue_increments_fifo_position() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        type_text(&mut state, "first");
        reduce(&mut state, Action::Key(enter()));
        type_text(&mut state, "second");
        reduce(&mut state, Action::Key(enter()));

        assert_eq!(queued_texts(&state), vec!["first", "second"]);
    }

    #[test]
    fn ninth_queued_prompt_is_rejected_without_losing_the_draft() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        for index in 0..8 {
            type_text(&mut state, &format!("queued-{index}"));
            reduce(&mut state, Action::Key(enter()));
        }
        type_text(&mut state, "keep ninth");

        let effects = reduce(&mut state, Action::Key(enter()));

        assert_eq!(state.queued_prompts.len(), 8);
        assert_eq!(state.composer.payload(), "keep ninth");
        assert_eq!(effects, vec![Effect::RequestRender]);
        assert!(state
            .notifications
            .iter()
            .any(|notification| notification.message == "Fila de prompts cheia (8)."));
    }

    #[test]
    fn alt_enter_during_run_enqueues_too() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        type_text(&mut state, "steered");
        let effects = reduce(&mut state, Action::Key(alt_enter()));

        assert_eq!(effects, vec![Effect::RequestRender]);
        assert_eq!(queued_texts(&state), vec!["steered"]);
    }

    #[test]
    fn empty_draft_never_enqueues() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.is_empty());
        assert_eq!(state.queued_prompts.len(), 0);
    }

    #[test]
    fn run_terminal_drains_exactly_one_prompt() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        type_text(&mut state, "queued prompt");
        reduce(&mut state, Action::Key(enter()));

        let effects = reduce(&mut state, run_completed(1));

        assert!(effects.contains(&Effect::Send(UiCommand::SendPrompt("queued prompt".into()))));
        assert_eq!(state.queued_prompts.len(), 0);
        assert!(queued_texts(&state).is_empty(), "drain removes the block");
    }

    #[test]
    fn two_queued_two_terminals_fifo_order() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        type_text(&mut state, "alpha");
        reduce(&mut state, Action::Key(enter()));
        type_text(&mut state, "beta");
        reduce(&mut state, Action::Key(enter()));

        let first = reduce(&mut state, run_completed(1));
        assert!(first.contains(&Effect::Send(UiCommand::SendPrompt("alpha".into()))));
        assert_eq!(state.queued_prompts.len(), 1);
        assert_eq!(queued_texts(&state), vec!["beta"]);

        let second = reduce(&mut state, run_completed(2));
        assert!(second.contains(&Effect::Send(UiCommand::SendPrompt("beta".into()))));
        assert_eq!(state.queued_prompts.len(), 0);
        assert!(queued_texts(&state).is_empty());
    }

    #[test]
    fn locally_handled_skill_selection_drains_the_next_queued_prompt() {
        let mut state = AppState::new();
        state.set_skill_names_for_test(vec!["review-code".into()]);
        reduce(&mut state, run_started(1));
        type_text(&mut state, "/review-code");
        reduce(&mut state, Action::Key(enter()));
        type_text(&mut state, "after selection");
        reduce(&mut state, Action::Key(enter()));

        let first = reduce(&mut state, run_completed(1));
        assert!(first.contains(&Effect::Send(UiCommand::SendPrompt("/review-code ".into()))));
        assert_eq!(state.queued_prompts.len(), 1);

        let second = reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::RestoreDraft {
                text: "/review-code ".into(),
            }),
        );
        assert!(second.contains(&Effect::Send(UiCommand::SendPrompt(
            "after selection".into()
        ))));
        assert!(state.queued_prompts.is_empty());
    }

    #[test]
    fn non_terminal_event_does_not_drain() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        type_text(&mut state, "kept while running");
        reduce(&mut state, Action::Key(enter()));
        assert_eq!(state.queued_prompts.len(), 1);

        // A non-terminal event (tick) must not drain the queue.
        reduce(&mut state, Action::Tick(Default::default()));
        assert_eq!(state.queued_prompts.len(), 1);
        assert_eq!(queued_texts(&state).len(), 1);
    }
}
