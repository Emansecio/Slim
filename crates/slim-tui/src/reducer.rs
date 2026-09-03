use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::api::{BlockId, LoginProvider, ModelAlias, ReasoningEffort, UiCommand, UiEvent};
use crate::app::{
    AppState, EffortOverlay, FollowMode, FrameClock, LoginOverlay, LoginStage, ModelOverlay,
    ModelRow, ScrollAnchor,
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
    Resize,
    ToggleTodoDock,
    ToggleBlock(BlockId),
    Scroll {
        intent: ScrollIntent,
        metrics: ScrollMetrics,
    },
    /// Synchronizes event timestamps without scheduling a frame.
    SyncClock(FrameClock),
    /// Motion clock (§10.3): the loop sends ticks only while animating.
    Tick(FrameClock),
    /// One-second semantic timer for elapsed labels under reduced motion.
    StatusTick(FrameClock),
    ClipboardCompleted {
        success: bool,
    },
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
    RequestRender,
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
            if prompt_boundary && !state.working {
                if let Some(prompt) = state.pop_queued_prompt() {
                    effects.push(Effect::Send(UiCommand::SendPrompt(prompt)));
                }
            }
            effects
        }
        Action::Key(key) => reduce_key(state, key),
        Action::Paste(payload) => {
            // G250: paste never edits the composer underneath a stacked modal
            // (model/effort overlays do not accept pulls).
            if state.model_overlay.is_some() || state.effort_overlay.is_some() {
                return vec![Effect::RequestRender];
            }
            if let Some(LoginStage::ApiKey(api_key)) = state
                .login_overlay
                .as_mut()
                .map(|overlay| &mut overlay.stage)
            {
                if !api_key.push_str_bounded(&payload, 4_096) {
                    state.push_notification("API key too large (limit 4096 characters).".into());
                }
                state.revisions.status += 1;
                return vec![Effect::RequestRender];
            }
            if state.pending_interaction().is_some_and(|interaction| {
                interaction.response_pending
                    || matches!(&interaction.kind, InteractionRequestKind::Approval { .. })
                    || matches!(&interaction.kind,
                        InteractionRequestKind::Question { options, .. }
                            if !options.is_empty() && !interaction.custom_question_answer)
            }) {
                return vec![Effect::RequestRender];
            }
            match state.composer.try_paste(payload) {
                Ok(_) => {
                    state.revisions.content += 1;
                    sync_slash_suggestions(state);
                }
                Err(ComposerError::DraftTooLarge) => {
                    state.push_notification("Draft too large (limit 1 MiB).".into());
                    state.revisions.status += 1;
                }
            }
            vec![Effect::RequestRender]
        }
        Action::Resize => vec![Effect::RequestRender],
        Action::ToggleTodoDock => {
            state.todo_dock_open = !state.todo_dock_open;
            state.revisions.status += 1;
            vec![Effect::RequestRender]
        }
        Action::ToggleBlock(id) => {
            let (changed, command) = state.activate_block(&id);
            let mut effects = command.into_iter().map(Effect::Send).collect::<Vec<_>>();
            if changed {
                effects.push(Effect::RequestRender);
            }
            effects
        }
        Action::Scroll { intent, metrics } => {
            reduce_scroll(state, intent, &metrics);
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
            state.push_notification(if success {
                "Copied to clipboard".into()
            } else {
                "Clipboard unavailable".into()
            });
            state.revisions.status += 1;
            vec![Effect::RequestRender]
        }
        Action::RequestShutdown => {
            state.shutdown = true;
            vec![Effect::Send(UiCommand::Shutdown), Effect::RequestRender]
        }
    }
}

const PALETTE_COMMANDS: [&str; 11] = [
    "/login",
    "/logout",
    "/resume",
    "/model",
    "/mode",
    "/compact",
    "/image",
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

/// Visual groups for Ctrl+P and the slash popup. Selection still walks
/// commands only; headers are presentation.
pub const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("session", &["/login", "/logout", "/resume", "/compact"]),
    ("runtime", &["/model", "/mode", "/image"]),
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
        "/login" => "connect provider",
        "/logout" => "sign out",
        "/resume" => "resume session",
        "/model" => "select model",
        "/mode" => "cycle mode",
        "/compact" => "summarize context",
        "/image" => "attach image",
        "/diff" => "view changes",
        "/activity" => "view activity",
        "/session" => "session tree",
        "/diagnostics" => "provider timings",
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
    let chars: Vec<char> = composer.payload().chars().collect();
    if cursor > chars.len() {
        return None;
    }
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = cursor;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    let token: String = chars[start..end].iter().collect();
    token
        .strip_prefix('/')
        .map(|query| (start, end, query.to_owned()))
}

fn replace_slash_token(state: &mut AppState, command: &str) {
    let payload = state.composer.payload();
    let chars: Vec<char> = payload.chars().collect();
    let (start, end) = slash_token_span(&state.composer).map_or_else(
        || {
            let cursor = state.composer.cursor().min(chars.len());
            (cursor, cursor)
        },
        |(start, end, _)| (start, end),
    );
    let mut rebuilt = String::new();
    rebuilt.extend(chars[..start].iter());
    rebuilt.push_str(command);
    let tail: String = chars[end..].iter().collect();
    // At end of draft the trailing space separates the completed command from
    // the next word; mid-sentence an existing space is reused, never doubled.
    if tail.is_empty() || !tail.starts_with(char::is_whitespace) {
        rebuilt.push(' ');
    }
    rebuilt.push_str(&tail);
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
                state.push_notification("A run is active; wait or cancel before resuming".into());
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
        state.push_notification(format!("Prompt queue full ({MAX_QUEUED_PROMPTS})."));
        return vec![Effect::RequestRender];
    }
    state.composer.clear();
    state.slash_suggestions = None;
    state.enqueue_queued_prompt(prompt);
    vec![Effect::RequestRender]
}

fn reduce_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    // G250: Ctrl+P must not open the palette over a stacked modal (login,
    // effort, model) — those gates dispatch first, so add the guard here.
    let overlays_open = state.login_overlay.is_some()
        || state.effort_overlay.is_some()
        || state.model_overlay.is_some();
    if is_ctrl_c(&key) {
        if state.login_overlay.is_some() {
            return reduce_login_key(state, key);
        }
        if state.working {
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
    // Command palette (§15.7-style overlay): Ctrl+P toggles; typing filters;
    // Enter submits the top match as a composer submission.
    if key.code == KeyCode::Char('p')
        && key.modifiers.contains(KeyModifiers::CONTROL)
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
    let inspector = if key.modifiers.contains(KeyModifiers::CONTROL) {
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
                state.push_notification("Nothing to copy".into());
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
    if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let mut effects = open_model_overlay(state);
        effects.push(Effect::RequestRender);
        return effects;
    }
    if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return reduce(state, Action::ToggleTodoDock);
    }
    if key.code == KeyCode::Esc && state.working {
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
        state.push_notification("A run is active; wait or cancel before resuming".into());
        state.revisions.status += 1;
        return vec![Effect::RequestRender];
    }
    if key.code == KeyCode::Enter
        && key.kind == KeyEventKind::Press
        && state.working
        && state.composer.payload().trim().starts_with("/compact")
    {
        return submit_composer(state);
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
                state.push_notification("Draft too large (limit 1 MiB).".into());
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

fn reduce_search_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let mut search = state.search.take().unwrap_or_default();
    match key.code {
        KeyCode::Esc => {
            state.revisions.focus += 1;
            return vec![Effect::RequestRender];
        }
        KeyCode::Enter | KeyCode::Down => {
            let total =
                search_match_indices_filtered(state.blocks(), &search.query, search.filter).len();
            if total > 0 {
                search.selected = (search.selected + 1) % total;
            }
        }
        KeyCode::Up => {
            let total =
                search_match_indices_filtered(state.blocks(), &search.query, search.filter).len();
            if total > 0 {
                search.selected = search.selected.checked_sub(1).unwrap_or(total - 1);
            }
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
    let matches = search_match_indices_filtered(state.blocks(), &search.query, search.filter);
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
    ));
    state.revisions.status += 1;
    vec![
        Effect::Send(UiCommand::RefreshOpenCodeModels),
        Effect::Send(UiCommand::RefreshClinePassModels),
        Effect::Send(UiCommand::RefreshCommandCodeModels),
    ]
}

fn submit_composer(state: &mut AppState) -> Vec<Effect> {
    if matches!(
        state
            .pending_interaction()
            .map(|interaction| &interaction.kind),
        Some(InteractionRequestKind::Input { .. })
    ) {
        return submit_pending_input(state);
    }
    if matches!(
        state
            .pending_interaction()
            .map(|interaction| &interaction.kind),
        Some(InteractionRequestKind::Question { .. })
    ) {
        return submit_pending_question_custom(state);
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
        "/logout" => {
            effects.push(Effect::Send(UiCommand::Logout));
            state.revisions.status += 1;
        }
        "/resume" => {
            effects.push(Effect::Send(UiCommand::ResumePrevious));
            state.revisions.status += 1;
        }
        "/mode" => {
            effects.push(Effect::Send(UiCommand::SetMode(cycle_mode(state.mode))));
            state.revisions.status += 1;
        }
        "/image" => {
            state.push_notification("Usage: /image PATH".into());
            keep_draft = true;
            state.revisions.status += 1;
        }
        _ if command.starts_with("/image ") => {
            let path = command.trim_start_matches("/image ").trim();
            if path.is_empty() {
                state.push_notification("Usage: /image PATH".into());
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
                state.push_notification("No provider connected. Use /login.".into());
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
                state.push_notification("No provider connected. Use /login.".into());
                keep_draft = true;
            }
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
                        }));
                    } else {
                        state.push_notification("Unknown model. Use sol, terra, or luna.".into());
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
                        state.push_notification("Unknown OpenCode Go model. Use /models.".into());
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
                        state.push_notification("Unknown ClinePass model. Use /models.".into());
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
                        state.push_notification("Unknown Command Code model. Use /models.".into());
                        keep_draft = true;
                    }
                }
                _ => {
                    state.push_notification(
                        "Connect OpenAI Codex, OpenCode Go, ClinePass, or Command Code before selecting a model."
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
            state.push_notification("No provider connected. Use /login.".into());
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

fn reduce_model_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(mut overlay) = state.model_overlay.clone() else {
        return vec![];
    };
    let rows = overlay.rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
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
            }
            return vec![Effect::RequestRender];
        }
        _ => return vec![],
    }
    let updated_rows = overlay.rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
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
        KeyCode::Enter => {
            let effort = overlay.effort();
            state.effort_overlay = None;
            return vec![
                Effect::Send(UiCommand::SetModel {
                    model: overlay.model,
                    effort,
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
                current.selected = (current.selected + 1).min(4);
            }
        }
        KeyCode::Enter
            if overlay.provider() == LoginProvider::OpenCodeGo
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
        KeyCode::Esc => state.palette_query = None,
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
                        "A run is active; wait or cancel before resuming".into(),
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
    use crate::app::{AppState, FrameClock, LoginStage};

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
            state
                .notifications
                .iter()
                .any(|notification| notification.as_str().contains("Unknown ClinePass")),
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
            .insert_text("/model deepseek/deepseek-v4-flash");
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(
            effects.contains(&Effect::Send(UiCommand::SetCommandCodeModel {
                model: "deepseek/deepseek-v4-flash".into(),
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
            Some("No provider connected. Use /login.")
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
        })));
        assert!(state.composer.payload().is_empty());
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
        assert_eq!(slash_matches("").len(), 11);
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
        assert_eq!(queued_texts(&state), vec!["queued[0] second question"]);
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

        assert_eq!(
            queued_texts(&state),
            vec!["queued[0] first", "queued[1] second"]
        );
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
            .any(|notification| notification.message == "Prompt queue full (8)."));
    }

    #[test]
    fn alt_enter_during_run_enqueues_too() {
        let mut state = AppState::new();
        reduce(&mut state, run_started(1));
        type_text(&mut state, "steered");
        let effects = reduce(&mut state, Action::Key(alt_enter()));

        assert_eq!(effects, vec![Effect::RequestRender]);
        assert_eq!(queued_texts(&state), vec!["queued[0] steered"]);
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
        assert_eq!(queued_texts(&state), vec!["queued[1] beta"]);

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
