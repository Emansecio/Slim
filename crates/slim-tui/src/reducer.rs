use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::{LoginProvider, ModelAlias, ReasoningEffort, UiCommand, UiEvent};
use crate::app::{AppState, EffortOverlay, LoginOverlay, ModelOverlay};
use crate::composer::ComposerError;
use crate::input::{classify_enter, cycle_mode, normalize, EnterIntent};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    UiEventReceived(UiEvent),
    Key(KeyEvent),
    Paste(String),
    Resize,
    ToggleTodoDock,
    Scroll(ScrollIntent),
    /// Motion clock (§10.3): the loop sends ticks only while animating.
    Tick(u64),
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
    RequestRender,
}

/// Single mutation route (DESIGN-SLIM-TUI §4.1/§9.2): every state change flows
/// through here; the runtime only executes the returned effects.
pub fn reduce(state: &mut AppState, action: Action) -> Vec<Effect> {
    match action {
        Action::UiEventReceived(event) => {
            state.apply_event(event);
            vec![Effect::RequestRender]
        }
        Action::Key(key) => reduce_key(state, key),
        Action::Paste(payload) => {
            match state.composer.try_paste(payload) {
                Ok(_) => {
                    state.revisions.content += 1;
                    sync_slash_suggestions(state);
                }
                Err(ComposerError::DraftTooLarge) => {
                    state
                        .push_notification("Draft too large (limit 1 MiB).".into());
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
        Action::Scroll(intent) => {
            reduce_scroll(state, intent);
            vec![Effect::RequestRender]
        }
        Action::Tick(frame) => {
            state.spinner_frame = frame;
            vec![Effect::RequestRender]
        }
        Action::RequestShutdown => {
            state.shutdown = true;
            vec![Effect::Send(UiCommand::Shutdown), Effect::RequestRender]
        }
    }
}

const PALETTE_COMMANDS: [&str; 4] = ["/login", "/logout", "/model", "/mode"];

/// Commands matching the token under edit (W7): prefix match on the text
/// after the `/`. Empty query lists every command.
pub fn slash_matches(query: &str) -> Vec<&'static str> {
    PALETTE_COMMANDS
        .iter()
        .filter(|command| command.trim_start_matches('/').starts_with(query))
        .copied()
        .collect()
}

/// The `/`-token currently being typed (last whitespace-delimited word), so
/// the popup works whether the slash starts the draft or sits mid-sentence.
fn current_slash_query(payload: &str) -> Option<String> {
    let token = payload.split_whitespace().last()?;
    token.strip_prefix('/').map(str::to_owned)
}

fn replace_slash_token(state: &mut AppState, command: &str) {
    let payload = state.composer.payload();
    let head_end = payload.rfind(char::is_whitespace).map_or(0, |index| index + 1);
    let head = payload[..head_end].to_owned();
    state.composer.clear();
    state.composer.insert_text(format!("{head}{command} "));
}

fn sync_slash_suggestions(state: &mut AppState) {
    state.slash_suggestions = current_slash_query(&state.composer.payload())
        .filter(|query| !slash_matches(query).is_empty())
        .map(|query| crate::app::SlashSuggestions {
            query,
            selected: 0,
        });
}

/// Slash autocomplete key handling (W7): arrows navigate, Tab completes into
/// the draft, Enter completes and executes, Esc dismisses. Any other key
/// falls through to normal composer editing (which re-syncs the popup).
fn reduce_slash_key(state: &mut AppState, key: KeyEvent) -> Option<Vec<Effect>> {
    let suggestions = state.slash_suggestions.as_mut()?;
    let matches = slash_matches(&suggestions.query);
    let effects = match key.code {
        KeyCode::Up => {
            suggestions.selected = suggestions.selected.saturating_sub(1);
            vec![Effect::RequestRender]
        }
        KeyCode::Down => {
            suggestions.selected =
                (suggestions.selected + 1).min(matches.len().saturating_sub(1));
            vec![Effect::RequestRender]
        }
        KeyCode::Tab => {
            if let Some(command) = matches.get(suggestions.selected) {
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
            if let Some(command) = matches.get(suggestions.selected) {
                replace_slash_token(state, command);
            }
            state.slash_suggestions = None;
            submit_composer(state)
        }
        _ => return None,
    };
    Some(effects)
}

fn reduce_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    // Command palette (§15.7-style overlay): Ctrl+P toggles; typing filters;
    // Enter submits the top match as a composer submission.
    if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.palette_query = match state.palette_query.take() {
            Some(_) => None,
            None => Some(String::new()),
        };
        state.revisions.focus += 1;
        return vec![Effect::RequestRender];
    }
    if state.palette_query.is_some() {
        return reduce_palette_key(state, key);
    }
    if state.slash_suggestions.is_some() {
        if let Some(effects) = reduce_slash_key(state, key) {
            return effects;
        }
    }
    if state.login_overlay.is_some() {
        return reduce_login_key(state, key);
    }
    if state.model_overlay.is_some() {
        return reduce_model_key(state, key);
    }
    if state.effort_overlay.is_some() {
        return reduce_effort_key(state, key);
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.working {
            return vec![
                Effect::Send(UiCommand::CancelRun),
                Effect::RequestRender,
            ];
        }
        if state.composer.payload().is_empty() {
            return reduce(
                state,
                Action::RequestShutdown,
            );
        }
        return vec![];
    }
    if key.code == KeyCode::Esc && state.working {
        return vec![
            Effect::Send(UiCommand::CancelRun),
            Effect::RequestRender,
        ];
    }
    if key.code == KeyCode::BackTab {
        return vec![
            Effect::Send(UiCommand::SetMode(cycle_mode(state.mode))),
            Effect::RequestRender,
        ];
    }
    match key.code {
        KeyCode::Up => return scroll_key(state, ScrollIntent::Up),
        KeyCode::Down => return scroll_key(state, ScrollIntent::Down),
        KeyCode::PageUp => return scroll_key(state, ScrollIntent::PageUp),
        KeyCode::PageDown => return scroll_key(state, ScrollIntent::PageDown),
        KeyCode::Home => return scroll_key(state, ScrollIntent::Top),
        KeyCode::End => return scroll_key(state, ScrollIntent::LiveEdge),
        _ => {}
    }
    match classify_enter(normalize(key), state.working) {
        EnterIntent::Submit if state.working => return vec![],
        EnterIntent::Submit => return submit_composer(state),
        EnterIntent::Steer => return vec![],
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
            state.composer.insert_text(character.to_string());
            state.revisions.content += 1;
            sync_slash_suggestions(state);
        }
        KeyCode::Backspace if state.composer.remove_last_element().is_some() => {
            state.revisions.content += 1;
            sync_slash_suggestions(state);
        }
        _ => {}
    }
    vec![Effect::RequestRender]
}

fn scroll_key(state: &mut AppState, intent: ScrollIntent) -> Vec<Effect> {
    reduce(state, Action::Scroll(intent))
}

fn submit_composer(state: &mut AppState) -> Vec<Effect> {
    let prompt = state.composer.payload();
    let command = prompt.trim().to_owned();
    let mut effects = Vec::new();
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
            effects.push(Effect::Send(UiCommand::StartLogin(LoginProvider::Anthropic)));
            state.revisions.status += 1;
        }
        "/login codex" | "/login openai-codex" => {
            state.login_overlay = Some(LoginOverlay {
                selected: 1,
                in_progress: true,
                ..LoginOverlay::default()
            });
            effects.push(Effect::Send(UiCommand::StartLogin(LoginProvider::OpenAiCodex)));
            state.revisions.status += 1;
        }
        "/logout" => {
            effects.push(Effect::Send(UiCommand::Logout));
            state.revisions.status += 1;
        }
        "/model" | "/models" => {
            if state.auth_provider == Some(LoginProvider::OpenAiCodex) {
                state.model_overlay = Some(ModelOverlay {
                    selected: ModelAlias::parse(&state.model).map_or(0, ModelAlias::index),
                });
            } else {
                state.push_notification("Connect OpenAI Codex before selecting GPT-5.6.".into());
            }
            state.revisions.status += 1;
        }
        _ if command.starts_with("/model ") => {
            if state.auth_provider != Some(LoginProvider::OpenAiCodex) {
                state.push_notification("Connect OpenAI Codex before selecting GPT-5.6.".into());
            } else {
                let value = command.trim_start_matches("/model ").trim();
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
                }
            }
            state.revisions.status += 1;
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
    // preserved when signed out with a plain prompt (spec §15.3).
    if command.starts_with('/') || state.authenticated {
        state.composer.clear();
        state.revisions.content += 1;
    }
    state.slash_suggestions = None;
    effects.push(Effect::RequestRender);
    effects
}

fn reduce_model_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(overlay) = state.model_overlay.as_mut() else {
        return vec![];
    };
    match key.code {
        KeyCode::Esc => state.model_overlay = None,
        KeyCode::Up => overlay.selected = overlay.selected.saturating_sub(1),
        KeyCode::Down => overlay.selected = (overlay.selected + 1).min(ModelAlias::ALL.len() - 1),
        KeyCode::Enter => {
            let alias = overlay.alias();
            state.model_overlay = None;
            let levels = ReasoningEffort::supported(alias);
            let preferred = if levels.contains(&state.effort) {
                state.effort
            } else {
                ReasoningEffort::default_for(alias)
            };
            let selected = levels
                .iter()
                .position(|level| *level == preferred)
                .unwrap_or(0);
            state.effort_overlay = Some(EffortOverlay {
                model: alias,
                selected,
            });
        }
        _ => return vec![],
    }
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
            state.effort_overlay = None;
            state.model_overlay = Some(ModelOverlay {
                selected: overlay.model.index(),
            });
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
    if key.code == KeyCode::Esc
        || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
    {
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
    match key.code {
        KeyCode::Up => {
            if let Some(current) = state.login_overlay.as_mut() {
                current.selected = current.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if let Some(current) = state.login_overlay.as_mut() {
                current.selected = (current.selected + 1).min(1);
            }
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
    match key.code {
        KeyCode::Esc => state.palette_query = None,
        KeyCode::Backspace => {
            query.pop();
            state.palette_query = Some(query);
        }
        KeyCode::Enter => {
            state.palette_query = None;
            let command = PALETTE_COMMANDS
                .iter()
                .find(|command| command.starts_with(&query))
                .copied()
                .unwrap_or_default();
            if command.is_empty() {
                state.revisions.focus += 1;
                return vec![Effect::RequestRender];
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
        }
        _ => {}
    }
    state.revisions.focus += 1;
    vec![Effect::RequestRender]
}

fn reduce_scroll(state: &mut AppState, intent: ScrollIntent) {
    const PAGE_ROWS: usize = 10;
    match intent {
        ScrollIntent::LiveEdge | ScrollIntent::Top => {
            state.scroll.pinned = false;
            state.scroll.offset_from_end = 0;
            state.scroll.unseen = 0;
        }
        ScrollIntent::Up => {
            state.scroll.pinned = true;
            state.scroll.offset_from_end += 1;
        }
        ScrollIntent::Down => {
            if state.scroll.offset_from_end <= 1 {
                state.scroll.pinned = false;
                state.scroll.offset_from_end = 0;
                state.scroll.unseen = 0;
            } else {
                state.scroll.offset_from_end -= 1;
            }
        }
        ScrollIntent::PageUp => {
            state.scroll.pinned = true;
            state.scroll.offset_from_end += PAGE_ROWS;
        }
        ScrollIntent::PageDown => {
            if state.scroll.offset_from_end <= PAGE_ROWS {
                state.scroll.pinned = false;
                state.scroll.offset_from_end = 0;
                state.scroll.unseen = 0;
            } else {
                state.scroll.offset_from_end -= PAGE_ROWS;
            }
        }
    }
    state.revisions.viewport += 1;
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{reduce, Action, Effect};
    use crate::api::{LoginProvider, ModelAlias, ReasoningEffort, UiCommand};
    use crate::app::AppState;

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    #[test]
    fn login_command_opens_selector_and_selected_provider_is_sent() {
        let mut state = AppState::new();
        state.composer.insert_text("/login");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.login_overlay.is_some());

        reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::StartLogin(LoginProvider::OpenAiCodex))));
    }

    #[test]
    fn model_command_requires_effort_before_updating_runtime() {
        let mut state = AppState::new();
        state.auth_provider = Some(LoginProvider::OpenAiCodex);
        state.authenticated = true;
        state.composer.insert_text("/model");
        reduce(&mut state, Action::Key(enter()));
        assert!(state.model_overlay.is_some());
        reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        reduce(&mut state, Action::Key(enter()));
        assert!(state.model_overlay.is_none());
        assert!(state.effort_overlay.is_some());
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.contains(&Effect::Send(UiCommand::SetModel {
            model: ModelAlias::Terra,
            effort: ReasoningEffort::High,
        })));
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
            state.notifications.last().map(String::as_str),
            Some("No provider connected. Use /login.")
        );
    }

    #[test]
    fn enter_during_active_run_preserves_draft_without_sending() {
        let mut state = AppState::new();
        state.working = true;
        state.composer.insert_text("keep this draft");
        reduce(&mut state, Action::Key(enter()));
        assert_eq!(state.composer.payload(), "keep this draft");
    }

    #[test]
    fn ctrl_c_idle_with_empty_draft_requests_shutdown_once() {
        let mut state = AppState::new();
        let effects = reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(state.shutdown);
        assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
    }

    #[test]
    fn ctrl_c_while_working_cancels_run_and_stays_alive() {
        let mut state = AppState::new();
        state.working = true;
        let effects = reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!state.shutdown);
        assert!(effects.contains(&Effect::Send(UiCommand::CancelRun)));
    }

    #[test]
    fn backtab_cycles_mode_through_boundary() {
        let mut state = AppState::new();
        let effects = reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)));
        assert!(effects.contains(&Effect::Send(UiCommand::SetMode(slim_core::OperatingMode::ReadOnly))));
    }

    #[test]
    fn paste_too_large_is_visible_error_not_silent_drop() {
        let mut state = AppState::new();
        reduce(&mut state, Action::Paste("x".repeat(crate::composer::MAX_DRAFT_CHARS + 1)));
        assert!(!state.notifications.is_empty());
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

        // Top match is "/mode": submitted as a slash command.
        let _ = reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(state.palette_query.is_none());
        assert!(state.model_overlay.is_some() || state.composer.payload().is_empty());
    }

    #[test]
    fn esc_dismisses_palette_without_submitting() {
        let mut state = AppState::new();
        reduce(&mut state, Action::Key(ctrl_p()));
        reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(state.palette_query.is_none());
    }
}

#[cfg(test)]
mod slash_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{reduce, slash_matches, Action};
    use crate::app::AppState;

    fn type_text(state: &mut AppState, text: &str) {
        for character in text.chars() {
            reduce(
                state,
                Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
            );
        }
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
    fn plain_text_never_opens() {
        let mut state = AppState::new();
        type_text(&mut state, "explain this mode");
        assert!(state.slash_suggestions.is_none());
    }

    #[test]
    fn filter_narrows_to_prefix() {
        assert_eq!(slash_matches("lo"), vec!["/login", "/logout"]);
        assert_eq!(slash_matches("logi"), vec!["/login"]);
        assert_eq!(slash_matches("").len(), 4);
        assert!(slash_matches("zzz").is_empty());
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
