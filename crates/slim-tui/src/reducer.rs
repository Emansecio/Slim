use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::api::{
    BlockId, LoginProvider, ModelAlias, PromptOrigin, ReasoningEffort, UiCommand, UiEvent,
};
use crate::app::{
    AppState, EffortOverlay, EffortTarget, FollowMode, FrameClock, LoginOverlay, LoginStage,
    ModelOverlay, ModelRow, NotificationPriority, ScrollAnchor,
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
    /// CLI startup prompt routed through the same admission reducer as an
    /// interactive submission.
    SubmitInitialPrompt(String),
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

fn prepare_prompt(state: &mut AppState, prompt: String, origin: PromptOrigin) -> Option<Effect> {
    let admission = state.begin_prompt_preparation(prompt.clone(), origin)?;
    Some(Effect::Send(UiCommand::PreparePrompt { prompt, admission }))
}

/// Single mutation route (DESIGN-SLIM-TUI §4.1/§9.2): every state change flows
/// through here; the runtime only executes the returned effects.
pub fn reduce(state: &mut AppState, action: Action) -> Vec<Effect> {
    match action {
        Action::UiEventReceived(event) => {
            // G244 (§7.4): a provider terminal or locally handled prompt is a
            // queue boundary. After applying it, admit exactly one queued
            // prompt, FIFO, into preparation.
            let local_skill_selection = matches!(
                &event,
                UiEvent::RestoreDraft { text } if selected_skill_draft(state, text)
            );
            let prompt_boundary = local_skill_selection
                || matches!(
                    &event,
                    UiEvent::PromptPreparationHandled { .. }
                        | UiEvent::PromptRunCompleted { .. }
                        | UiEvent::PromptRunStopped { .. }
                        | UiEvent::PromptRunCancelled { .. }
                        | UiEvent::PromptRunFailed {
                            run_id: Some(_),
                            ..
                        }
                        | UiEvent::RunCompleted { .. }
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
            let cancel_started_run = state.take_cancelled_prompt_run_start();
            if cancel_started_run.is_some() {
                state.request_cancel_active_run();
                return vec![Effect::Send(UiCommand::CancelRun), Effect::RequestRender];
            }
            if skills_changed {
                sync_slash_suggestions(state);
            }
            let mut effects = vec![Effect::RequestRender];
            if prompt_boundary && !state.working && !state.prompt_is_busy() && !state.queue_paused {
                if let Some(prompt) = state.pop_queued_prompt() {
                    if let Some(effect) = prepare_prompt(state, prompt, PromptOrigin::Queued) {
                        effects.push(effect);
                    }
                }
            }
            effects
        }
        Action::SubmitInitialPrompt(prompt) => {
            if prompt.trim().is_empty() {
                return vec![Effect::RequestRender];
            }
            if !state.composer.is_empty() {
                let draft = state.composer.payload();
                state.enqueue_queued_prompt_front(draft);
                state.queue_paused = true;
                state.composer.clear();
            }
            state.composer.insert_text(prompt);
            state.revisions.content += 1;
            submit_composer(state)
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
            state.todo_focused = false;
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

const PALETTE_COMMANDS: [&str; 16] = [
    "/help",
    "/login",
    "/logout",
    "/resume",
    "/queue",
    "/model",
    "/model --default",
    "/mode",
    "/compact",
    "/retry",
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
        &[
            "/login", "/logout", "/resume", "/queue", "/compact", "/retry",
        ],
    ),
    (
        "runtime",
        &["/model", "/model --default", "/mode", "/image"],
    ),
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
        "/model" => "modelo desta sessão",
        "/model --default" => "salvar modelo atual como padrão",
        "/mode" => "alternar modo",
        "/compact" => "resumir contexto",
        "/image" => "anexar imagem",
        "/mcp" => "servidores MCP",
        "/diff" => "ver alterações",
        "/activity" => "ver atividade",
        "/session" => "árvore da sessão",
        "/diagnostics" => "tempos do provedor",
        "/retry" => "retomar conexão pausada",
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
    if key.code == KeyCode::Esc && state.prompt_is_busy() {
        return state.cancel_prompt_preparation().map_or_else(
            || vec![Effect::RequestRender],
            |admission| {
                vec![
                    Effect::Send(UiCommand::CancelPromptPreparation { admission }),
                    Effect::RequestRender,
                ]
            },
        );
    }
    if is_ctrl_v(&key) || is_shift_insert(&key) {
        return vec![Effect::PasteFromClipboard];
    }
    if is_ctrl_c(&key) {
        if state.login_overlay.is_some() {
            return reduce_login_key(state, key);
        }
        if state.prompt_is_busy() {
            return state.cancel_prompt_preparation().map_or_else(
                || vec![Effect::RequestRender],
                |admission| {
                    vec![
                        Effect::Send(UiCommand::CancelPromptPreparation { admission }),
                        Effect::RequestRender,
                    ]
                },
            );
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
    if !interaction_pending && state.inspector.active.is_none() {
        if key.code == KeyCode::Char('t')
            && key.modifiers == KeyModifiers::ALT
            && !state.todo_items.is_empty()
        {
            state.todo_focused = !state.todo_focused;
            state.todo_dock_open = true;
            state.todo_dock_user_preference = Some(true);
            state.revisions.focus += 1;
            return vec![Effect::RequestRender];
        }
        if state.todo_focused {
            return reduce_todo_key(state, key);
        }
    }
    if key.code == KeyCode::Esc && state.working {
        state.request_cancel_active_run();
        return vec![Effect::Send(UiCommand::CancelRun), Effect::RequestRender];
    }
    if (key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::ALT))
        && !interaction_pending
    {
        if state.working || state.prompt_is_busy() {
            state.push_notification("Aguarde ou cancele a execução antes de trocar o modo".into());
            state.revisions.status += 1;
            return vec![Effect::RequestRender];
        }
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
        && (state.working || state.prompt_is_busy())
        && state.composer.payload().trim() == "/resume"
    {
        state
            .push_notification("Há uma execução ativa; aguarde ou cancele antes de retomar".into());
        state.revisions.status += 1;
        return vec![Effect::RequestRender];
    }
    if key.code == KeyCode::Enter
        && key.kind == KeyEventKind::Press
        && (state.working || state.prompt_is_busy())
        && state.composer.payload().trim().starts_with("/queue")
    {
        return submit_composer(state);
    }
    // Native slash commands that stay local or hit the worker's read-only
    // arms must not queue as prompt text mid-run: /compact defers to a safe
    // boundary; /mcp opens the overlay (its mutating UiCommands are rejected
    // by the active-run catch-all with a notification).
    if key.code == KeyCode::Enter
        && key.kind == KeyEventKind::Press
        && (state.working || state.prompt_is_busy())
    {
        let payload = state.composer.payload();
        let payload = payload.trim();
        if payload == "/retry"
            || payload == "/model --default"
            || payload.starts_with("/compact")
            || payload == "/mcp"
            || payload.starts_with("/mcp ")
        {
            return submit_composer(state);
        }
    }
    match classify_enter(normalize(key), state.working || state.prompt_is_busy()) {
        EnterIntent::Submit if state.working || state.prompt_is_busy() => {
            return enqueue_queued(state)
        }
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
        KeyCode::Char('z' | 'Z')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            let changed = if key.modifiers.contains(KeyModifiers::SHIFT) {
                state.composer.redo()
            } else {
                state.composer.undo()
            };
            if changed {
                state.revisions.content += 1;
                sync_slash_suggestions(state);
            }
        }
        KeyCode::Left | KeyCode::Right
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            if state.composer.move_word(key.code == KeyCode::Right) {
                state.revisions.focus += 1;
                sync_slash_suggestions(state);
            }
        }
        KeyCode::Backspace | KeyCode::Delete
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            if state.composer.delete_word(key.code == KeyCode::Delete) {
                state.revisions.content += 1;
                sync_slash_suggestions(state);
            }
        }
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

fn reduce_todo_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let indices = crate::todo::ordered_indices(state);
    let current = indices
        .iter()
        .position(|index| *index == state.todo_selected)
        .unwrap_or(0);
    let last = indices.len().saturating_sub(1);
    let next = match key.code {
        KeyCode::Up => current.saturating_sub(1),
        KeyCode::Down => current.saturating_add(1).min(last),
        KeyCode::PageUp => current.saturating_sub(4),
        KeyCode::PageDown => current.saturating_add(4).min(last),
        KeyCode::Home => 0,
        KeyCode::End => last,
        KeyCode::Left => {
            state.todo_title_offset = state.todo_title_offset.saturating_sub(8);
            current
        }
        KeyCode::Right => {
            let length = state.todo_items.get(state.todo_selected).map_or(0, |item| {
                crate::todo::title(&crate::todo::item_text(item))
                    .graphemes(true)
                    .count()
            });
            state.todo_title_offset = state
                .todo_title_offset
                .saturating_add(8)
                .min(length.saturating_sub(1));
            current
        }
        KeyCode::Esc => {
            state.todo_focused = false;
            state.todo_dock_open = false;
            state.todo_dock_user_preference = Some(false);
            current
        }
        KeyCode::Tab | KeyCode::Enter => {
            state.todo_focused = false;
            current
        }
        _ => return vec![],
    };
    if next != current {
        state.todo_title_offset = 0;
    }
    if let Some(index) = indices.get(next) {
        state.todo_selected = *index;
    }
    state.revisions.focus += 1;
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
    state.effort_overlay = None;
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
    if state.prompt_is_busy() && command.starts_with('/') && !command.starts_with("/queue") {
        state
            .push_notification("Aguarde ou cancele a preparação antes de executar comandos".into());
        state.revisions.status += 1;
        return vec![Effect::RequestRender];
    }
    let mut effects = Vec::new();
    // Locally rejected slash input (unknown model, missing path, signed-out
    // compaction) keeps the draft so the user can fix and resubmit it instead
    // of retyping after the 5 s toast expires.
    let mut keep_draft = false;
    match command.as_str() {
        "" => {}
        "/retry" => effects.push(Effect::Send(UiCommand::RetryProvider)),
        "/model --default" => {
            if state.working {
                state.push_notification(
                    "Aguarde ou cancele a execução antes de salvar o padrão".into(),
                );
                keep_draft = true;
            } else {
                effects.push(Effect::Send(UiCommand::SaveModelDefault));
            }
        }
        "/help" => {
            state.push_notification(
                "F1 / Ctrl+P comandos · Ctrl+Z desfazer · Ctrl+Shift+Z refazer · Ctrl+←/→ palavra · Ctrl+Backspace/Delete apagar palavra · /model --default salvar padrão · /retry retomar conexão · Esc cancelar"
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
        _ if command == "/mode" || command.starts_with("/mode ") => {
            let mode = match command.strip_prefix("/mode").unwrap_or_default().trim() {
                "" => Some(cycle_mode(state.mode)),
                "auto" => Some(slim_core::OperatingMode::Auto),
                "read-only" | "readonly" => Some(slim_core::OperatingMode::ReadOnly),
                "plan" => Some(slim_core::OperatingMode::Plan),
                _ => None,
            };
            if state.working {
                state.push_notification(
                    "Aguarde ou cancele a execução antes de trocar o modo".into(),
                );
                keep_draft = true;
            } else if let Some(mode) = mode {
                effects.push(Effect::Send(UiCommand::SetMode(mode)));
            } else {
                state.push_notification("Uso: /mode auto|read-only|plan".into());
                keep_draft = true;
            }
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
                        let effort = catalog_effort(&model.reasoning_levels, state.effort);
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
                        let effort = catalog_effort(&model.reasoning_levels, state.effort);
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
                    // connected; use the same effort step as the model picker.
                    if let Some(model) = state
                        .cline_pass_models
                        .iter()
                        .find(|model| model.id == value)
                    {
                        if model.reasoning_levels.is_empty() {
                            effects.push(Effect::Send(UiCommand::SetClinePassModel {
                                model: model.id.clone(),
                                effort: state.effort,
                            }));
                        } else {
                            state.effort_overlay = Some(EffortOverlay::for_catalog(
                                EffortTarget::ClinePass(model.id.clone()),
                                model.reasoning_levels.clone(),
                                state.effort,
                            ));
                        }
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
                        if model.reasoning_levels.is_empty() {
                            effects.push(Effect::Send(UiCommand::SetCommandCodeModel {
                                model: model.id.clone(),
                                effort: state.effort,
                            }));
                        } else {
                            state.effort_overlay = Some(EffortOverlay::for_catalog(
                                EffortTarget::CommandCode(model.id.clone()),
                                model.reasoning_levels.clone(),
                                state.effort,
                            ));
                        }
                    } else {
                        state.push_notification(
                            "Modelo Command Code desconhecido. Use /models.".into(),
                        );
                        keep_draft = true;
                    }
                }
                Some(LoginProvider::Xai) => {
                    if slim_core::provider::is_xai_model_id(value) {
                        let levels = slim_core::provider::gateway_reasoning_levels(
                            slim_core::provider::ProviderKind::Xai,
                            value,
                        )
                        .iter()
                        .filter_map(|level| ReasoningEffort::parse(level))
                        .collect();
                        state.effort_overlay = Some(EffortOverlay::for_catalog(
                            EffortTarget::Xai(value.to_owned()),
                            levels,
                            state.effort,
                        ));
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
            if let Some(effect) = prepare_prompt(state, prompt, PromptOrigin::Direct) {
                effects.push(effect);
            } else {
                state.push_notification_with_priority(
                    "Não foi possível admitir o prompt agora; tente novamente".into(),
                    NotificationPriority::Warning,
                );
                keep_draft = true;
            }
        }
        _ if !state.authenticated => {
            state.push_notification("Nenhum provedor conectado. Use /login.".into());
            state.revisions.status += 1;
            return effects;
        }
        _ => {
            if let Some(effect) = prepare_prompt(state, prompt, PromptOrigin::Direct) {
                effects.push(effect);
            } else {
                state.push_notification_with_priority(
                    "Não foi possível admitir o prompt agora; tente novamente".into(),
                    NotificationPriority::Warning,
                );
                keep_draft = true;
            }
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
            if state.working || state.prompt_is_busy() {
                state.push_notification(
                    "A fila pode ser retomada após a preparação ou execução atual".into(),
                );
                state.revisions.status += 1;
                return true;
            }
            state.queue_paused = false;
            if let Some(prompt) = state.pop_queued_prompt() {
                if let Some(effect) = prepare_prompt(state, prompt.clone(), PromptOrigin::Queued) {
                    effects.push(effect);
                    state.push_notification("Fila retomada".into());
                } else {
                    state.enqueue_queued_prompt_front(prompt);
                    state.queue_paused = true;
                    state.push_notification("Não foi possível admitir o item da fila".into());
                }
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

/// Effort sent when a model is selected without the interactive step (textual
/// `/model <id>`) or when the model declares no levels: the session choice when
/// the model supports it, otherwise its first level.
fn catalog_effort(levels: &[ReasoningEffort], current: ReasoningEffort) -> ReasoningEffort {
    if levels.contains(&current) {
        current
    } else {
        levels.first().copied().unwrap_or(ReasoningEffort::High)
    }
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
                    state.effort_overlay = Some(EffortOverlay::for_alias(
                        *alias,
                        preferred,
                        state.codex_fast,
                    ));
                }
                ModelRow::Catalog(model_index) => {
                    let Some(model) = state.open_code_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![
                            Effect::Send(UiCommand::RefreshOpenCodeModels),
                            Effect::RequestRender,
                        ];
                    };
                    if !model.reasoning_levels.is_empty() {
                        // The step sits on top of the model overlay (G239), so
                        // Esc returns to the same row, filter and folds.
                        state.effort_overlay = Some(EffortOverlay::for_catalog(
                            EffortTarget::OpenCodeGo(model.id),
                            model.reasoning_levels,
                            state.effort,
                        ));
                    } else {
                        state.model_overlay = None;
                        return vec![
                            Effect::Send(UiCommand::SetOpenCodeModel {
                                model: model.id,
                                effort: catalog_effort(&model.reasoning_levels, state.effort),
                            }),
                            Effect::RequestRender,
                        ];
                    }
                }
                ModelRow::ClinePass(model_index) => {
                    let Some(model) = state.cline_pass_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![Effect::RequestRender];
                    };
                    if !model.reasoning_levels.is_empty() {
                        state.effort_overlay = Some(EffortOverlay::for_catalog(
                            EffortTarget::ClinePass(model.id),
                            model.reasoning_levels,
                            state.effort,
                        ));
                    } else {
                        state.model_overlay = None;
                        return vec![
                            Effect::Send(UiCommand::SetClinePassModel {
                                model: model.id,
                                effort: state.effort,
                            }),
                            Effect::RequestRender,
                        ];
                    }
                }
                ModelRow::CommandCode(model_index) => {
                    let Some(model) = state.command_code_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![
                            Effect::Send(UiCommand::RefreshCommandCodeModels),
                            Effect::RequestRender,
                        ];
                    };
                    if !model.reasoning_levels.is_empty() {
                        state.effort_overlay = Some(EffortOverlay::for_catalog(
                            EffortTarget::CommandCode(model.id),
                            model.reasoning_levels,
                            state.effort,
                        ));
                    } else {
                        state.model_overlay = None;
                        return vec![
                            Effect::Send(UiCommand::SetCommandCodeModel {
                                model: model.id,
                                effort: state.effort,
                            }),
                            Effect::RequestRender,
                        ];
                    }
                }
                ModelRow::Zen(model_index) => {
                    let Some(model) = state.zen_models.get(*model_index).cloned() else {
                        state.model_overlay = Some(overlay);
                        return vec![
                            Effect::Send(UiCommand::RefreshZenModels),
                            Effect::RequestRender,
                        ];
                    };
                    if !model.reasoning_levels.is_empty() {
                        state.effort_overlay = Some(EffortOverlay::for_catalog(
                            EffortTarget::Zen(model.id),
                            model.reasoning_levels,
                            state.effort,
                        ));
                    } else {
                        state.model_overlay = None;
                        return vec![
                            Effect::Send(UiCommand::SetZenModel {
                                model: model.id,
                                effort: catalog_effort(&model.reasoning_levels, state.effort),
                            }),
                            Effect::RequestRender,
                        ];
                    }
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
    let levels = overlay.levels();
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
                current.selected = (current.selected + 1).min(levels.len().saturating_sub(1));
            }
        }
        // Tab only toggles the Codex service tier; catalog models have no
        // speed knob, so the key stays inert instead of flipping a hidden flag.
        KeyCode::Tab if overlay.speed_toggle() => {
            if let Some(current) = state.effort_overlay.as_mut() {
                current.fast = !current.fast;
            }
        }
        KeyCode::Tab => return vec![],
        KeyCode::Enter => {
            let effort = overlay.effort();
            state.effort_overlay = None;
            let command = match overlay.target {
                EffortTarget::Alias(model) => UiCommand::SetModel {
                    model,
                    effort,
                    fast: overlay.fast,
                },
                EffortTarget::OpenCodeGo(model) => UiCommand::SetOpenCodeModel { model, effort },
                EffortTarget::Zen(model) => UiCommand::SetZenModel { model, effort },
                EffortTarget::ClinePass(model) => UiCommand::SetClinePassModel { model, effort },
                EffortTarget::CommandCode(model) => {
                    UiCommand::SetCommandCodeModel { model, effort }
                }
                EffortTarget::Xai(model) => UiCommand::SetXaiModel { model, effort },
            };
            return vec![Effect::Send(command), Effect::RequestRender];
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
mod tests;

#[cfg(test)]
mod palette_tests;

#[cfg(test)]
mod slash_tests;

#[cfg(test)]
mod queued_prompt_tests;
