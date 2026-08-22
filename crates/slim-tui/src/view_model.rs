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

/// Plain-text mirror of the W8 footer: critical state or real shortcuts on the
/// left, context/usage on the right. Model/effort/mode live in composer chrome.
pub fn status_line(state: &AppState) -> String {
    const CONTEXT_TOKENS: u64 = 128_000;
    let used = state.input_tokens.saturating_add(state.output_tokens);
    let pct = (used.saturating_mul(100) / CONTEXT_TOKENS).min(999);
    let left = if state.working {
        "Working · Esc/Ctrl+C:cancel".into()
    } else if state.scroll.pinned {
        if state.scroll.unseen > 0 {
            format!("{} new · End latest", state.scroll.unseen)
        } else {
            "End latest".into()
        }
    } else if !state.authenticated {
        "signed out · /login".into()
    } else {
        "Shift+Tab:mode │ Ctrl+C:exit │ Ctrl+P:commands".into()
    };
    format!(
        "{left}  ctx {pct}% · {}k/128k · ↑{} ↓{}",
        used.saturating_add(500) / 1_000,
        state.input_tokens,
        state.output_tokens
    )
}
