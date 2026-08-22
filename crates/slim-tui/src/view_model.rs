use slim_core::runtime::mode_name;

use crate::app::AppState;
use crate::block::{BlockKind, BlockLifecycle, FoldState};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub lines: Vec<String>,
}

pub struct ViewModel;

/// Plain-text projection of the state. The fullscreen surface renders styled
/// chrome directly from `AppState`; this frame keeps headless/tests honest.
impl ViewModel {
    pub fn derive(state: &AppState) -> Frame {
        let mut lines = Vec::new();
        for block in &state.blocks {
            match &block.kind {
                BlockKind::User(text) => {
                    lines.push("you".into());
                    lines.push(format!("> {text}"));
                }
                BlockKind::Assistant(text) => {
                    lines.push("Slim".into());
                    lines.push(text.clone());
                }
                BlockKind::Thinking(text) if block.fold == FoldState::Collapsed => {
                    let preview = text.lines().next().unwrap_or_default();
                    lines.push(format!("thinking: {preview}"));
                }
                BlockKind::Thinking(text) => lines.push(format!("thinking: {text}")),
                BlockKind::Tool(state) => {
                    if block.lifecycle == BlockLifecycle::Streaming {
                        lines.push(format!("◌ {}: {}", state.name, state.preview));
                    } else {
                        lines.push(format!("✕ {} (failed)", state.name));
                    }
                }
                BlockKind::System(text) => lines.push(format!("system: {text}")),
                BlockKind::Error(text) => lines.push(format!("error: {text}")),
                BlockKind::Activity(text) => lines.push(format!("activity: {text}")),
                BlockKind::QueuedUser(text) => lines.push(format!("> {text}")),
            }
        }
        for notification in state.notifications.iter().rev().take(3).rev() {
            lines.push(format!("notice: {notification}"));
        }
        if state.todo_dock_open {
            let total = state.todo_items.len();
            let done = state
                .todo_items
                .iter()
                .filter(|item| {
                    item.status == crate::api::TodoItemStatus::Completed
                })
                .count();
            let active = state
                .todo_items
                .iter()
                .find(|item| {
                    item.status == crate::api::TodoItemStatus::InProgress
                })
                .map(|item| item.title.as_str())
                .unwrap_or("no active item");
            lines.push(format!("todo: {done}/{total} {active}"));
        }
        lines.push(format!(
            "composer: {}",
            state.composer.display_for_width(80)
        ));
        lines.push(status_line(state));
        Frame { lines }
    }
}

/// Operational bar content in normative groups (§15.1): left group carries
/// identity, Mode and transient state; right group carries model and token
/// counters. Context/cost appear only once the harness provides real data.
pub fn status_line(state: &AppState) -> String {
    let mut left = format!("SLIM  {}", mode_name(state.mode));
    if state.working {
        left.push_str(" · ◌ working");
    } else if !state.authenticated {
        left.push_str(" · signed out · /login");
    } else if let Some(provider) = state.auth_provider {
        left.push_str(&format!(" · {}", provider.label()));
    }
    let model = crate::api::ModelAlias::parse(&state.model)
        .map_or_else(|| state.model.clone(), |alias| alias.label().into());
    let effort = crate::api::ModelAlias::parse(&state.model)
        .map(|_| format!(" · {}", state.effort.id()))
        .unwrap_or_default();
    format!(
        "{left}  {model}{effort} · ↑{} ↓{}",
        state.input_tokens, state.output_tokens
    )
}
