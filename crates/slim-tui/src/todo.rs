//! Shared Todo projection. The task tracker remains the only status authority.
use crate::{api::TodoItemStatus, app::AppState};

pub fn ordered_indices(state: &AppState) -> Vec<usize> {
    let mut indices: Vec<_> = (0..state.todo_items.len()).collect();
    indices.sort_by_key(|index| match state.todo_items[*index].status {
        TodoItemStatus::InProgress => 0,
        TodoItemStatus::Blocked => 1,
        TodoItemStatus::Pending => 2,
        TodoItemStatus::Cancelled => 3,
        TodoItemStatus::Completed => 4,
    });
    indices
}

pub fn summary(state: &AppState) -> String {
    let count = |status| {
        state
            .todo_items
            .iter()
            .filter(|item| item.status == status)
            .count()
    };
    let cancelled = count(TodoItemStatus::Cancelled);
    let total = state.todo_items.len().saturating_sub(cancelled);
    let done = count(TodoItemStatus::Completed);
    let active = count(TodoItemStatus::InProgress);
    let blocked = count(TodoItemStatus::Blocked);
    let mut label = if total == 0 && cancelled > 0 {
        "TODO encerrado".into()
    } else {
        format!("TODO {done}/{total} concluídas")
    };
    if active > 0 {
        label.push_str(if state.working {
            " · 1 em andamento"
        } else {
            " · execução parada"
        });
    }
    if blocked > 0 {
        label.push_str(&format!(
            " · {}",
            count_label(blocked, "bloqueada", "bloqueadas")
        ));
    }
    if cancelled > 0 {
        label.push_str(&format!(
            " · {}",
            count_label(cancelled, "cancelada", "canceladas")
        ));
    }
    label
}

fn count_label(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Compact mode keeps blockers before the active title that may be truncated.
pub fn compact_summary(state: &AppState) -> String {
    if state.todo_focused {
        if let Some(item) = state.todo_items.get(state.todo_selected) {
            let order = ordered_indices(state);
            let position = order
                .iter()
                .position(|index| *index == state.todo_selected)
                .unwrap_or(0)
                + 1;
            return format!(
                "TODO {position}/{} >{}",
                order.len(),
                visible_title(&item_text(item), state.todo_title_offset)
            );
        }
    }
    let done = state
        .todo_items
        .iter()
        .filter(|item| item.status == TodoItemStatus::Completed)
        .count();
    let total = state
        .todo_items
        .iter()
        .filter(|item| item.status != TodoItemStatus::Cancelled)
        .count();
    if let Some(active) = state
        .todo_items
        .iter()
        .find(|item| item.status == TodoItemStatus::InProgress)
    {
        let phase = if state.working { "em curso" } else { "parada" };
        let blocked = state
            .todo_items
            .iter()
            .filter(|item| item.status == TodoItemStatus::Blocked)
            .count();
        let mut label = format!("TODO {done}/{total}");
        if blocked > 0 {
            label.push_str(&format!(
                " · {}",
                count_label(blocked, "bloqueada", "bloqueadas")
            ));
        }
        label.push_str(&format!(" · {phase} · {}", title(&active.title)));
        label
    } else {
        summary(state)
    }
}

pub fn item_text(item: &crate::api::TodoItemView) -> String {
    match &item.reason {
        Some(reason) => format!("{} · Motivo: {reason}", item.title),
        None => item.title.clone(),
    }
}

pub fn title(raw: &str) -> String {
    crate::markdown::sanitize_terminal_text(raw).replace(['\n', '\r'], " ")
}

pub fn visible_title(raw: &str, offset: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let text = title(raw);
    let offset = offset.min(text.graphemes(true).count().saturating_sub(1));
    let suffix: String = text.graphemes(true).skip(offset).collect();
    format!("{}{suffix}", if offset > 0 { "<" } else { " " })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{TodoItemView, UiEvent},
        reducer::{reduce, Effect},
        render::WrapCache,
        runtime::{render_frame, terminal_action},
        theme::{Capabilities, ColorDepth},
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};

    fn item(id: u64, status: TodoItemStatus) -> TodoItemView {
        TodoItemView {
            reason: None,
            id: Some(id),
            title: format!("Tarefa {id} 漢字 {} FIM", "longa ".repeat(20)),
            status,
        }
    }

    fn key(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
        let action = terminal_action(
            Event::Key(KeyEvent::new(code, modifiers)),
            state,
            (80, 24),
            &mut WrapCache::default(),
        )
        .unwrap();
        reduce(state, action)
    }

    fn draw(state: &AppState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    state,
                    Capabilities {
                        color_depth: ColorDepth::None,
                        mouse: false,
                        clipboard: false,
                        images: false,
                        reduced_motion: true,
                    },
                    &mut WrapCache::default(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn todo_navigation_reaches_hidden_titles_without_scrolling_transcript_or_submitting() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.apply_event(UiEvent::run_started(1));
        state.apply_event(UiEvent::TodoChanged {
            items: (0..9)
                .map(|id| {
                    item(
                        id,
                        if id == 0 {
                            TodoItemStatus::InProgress
                        } else {
                            TodoItemStatus::Pending
                        },
                    )
                })
                .collect(),
        });
        state.composer.insert_text("preservar rascunho");
        assert!(!state.todo_dock_open);
        key(&mut state, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let before = draw(&state, 80, 24);
        assert!(before.contains("+5 fora da vista"));
        assert!(before.contains('…'));
        assert!(!before.contains("Tarefa 8"));
        key(&mut state, KeyCode::Char('t'), KeyModifiers::ALT);
        key(&mut state, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(state.todo_selected, 8);
        assert!(draw(&state, 80, 24).contains("Tarefa 8"));
        for _ in 0..12 {
            key(&mut state, KeyCode::Right, KeyModifiers::NONE);
        }
        assert!(draw(&state, 80, 24).contains("FIM"));
        assert_eq!(state.composer.payload(), "preservar rascunho");
        let effects = key(&mut state, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(_))));
        assert!(state.working);
        assert!(!state.todo_focused && !state.todo_dock_open);
        // Tiny terminals still expose a compact summary, without a panic.
        assert!(draw(&state, 40, 8).contains("TODO 0/9"));
    }

    #[test]
    fn todo_block_reason_and_all_cancelled_summary_are_visible() {
        let mut state = AppState::new();
        state.authenticated = true;
        let mut blocked = item(1, TodoItemStatus::Blocked);
        blocked.title = "Deploy".into();
        blocked.reason = Some("aguardando aprovação".into());
        state.apply_event(UiEvent::TodoChanged {
            items: vec![blocked],
        });
        key(&mut state, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(draw(&state, 80, 24).contains("Motivo: aguardando aprovação"));
        state.apply_event(UiEvent::TodoChanged {
            items: vec![item(1, TodoItemStatus::Cancelled)],
        });
        assert_eq!(summary(&state), "TODO encerrado · 1 cancelada");
    }

    #[test]
    fn compact_todo_keeps_blocked_count_before_the_active_title_on_narrow_screens() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.apply_event(UiEvent::TodoChanged {
            items: vec![
                item(1, TodoItemStatus::InProgress),
                item(2, TodoItemStatus::Blocked),
                item(3, TodoItemStatus::Completed),
            ],
        });
        assert!(!state.todo_dock_open);
        for working in [false, true] {
            state.working = working;
            let phase = if working { "em curso" } else { "parada" };
            assert!(compact_summary(&state)
                .starts_with(&format!("TODO 1/3 · 1 bloqueada · {phase} · Tarefa 1")));
            for (width, height) in [(40, 8), (80, 24)] {
                let frame = draw(&state, width, height);
                let row = frame
                    .lines()
                    .find(|row| row.trim_start().starts_with("TODO"))
                    .unwrap_or_else(|| panic!("missing Todo at {width}x{height}\n{frame}"));
                assert!(row.contains("1 bloqueada ·"), "{row}");
                assert!(row.trim_end().ends_with('…'), "{row}");
                assert!(!frame.contains("Tarefa 2"), "{frame}");
            }
        }
    }

    #[test]
    fn todo_updates_preserve_identity_and_age_and_stop_does_not_complete_tasks() {
        let mut state = AppState::new();
        state.apply_event(UiEvent::run_started(1));
        state.clock.elapsed_ms = 100;
        let first = vec![
            item(10, TodoItemStatus::InProgress),
            item(20, TodoItemStatus::Pending),
        ];
        state.apply_event(UiEvent::TodoChanged {
            items: first.clone(),
        });
        state.todo_selected = 1;
        state.clock.elapsed_ms = 500;
        state.apply_event(UiEvent::TodoChanged { items: first });
        assert_eq!(state.todo_updated_ms, Some(100));
        state.apply_event(UiEvent::TodoChanged {
            items: vec![
                item(20, TodoItemStatus::InProgress),
                item(10, TodoItemStatus::Completed),
            ],
        });
        assert_eq!(state.todo_selected, 0);
        assert_eq!(state.todo_updated_ms, Some(500));
        assert!(summary(&state).contains("1 em andamento"));
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        assert!(summary(&state).contains("execução parada"));
        assert_eq!(state.todo_items[0].status, TodoItemStatus::InProgress);
        state.apply_event(UiEvent::TodoChanged {
            items: vec![
                item(20, TodoItemStatus::Cancelled),
                item(10, TodoItemStatus::Completed),
            ],
        });
        assert_eq!(summary(&state), "TODO 1/1 concluídas · 1 cancelada");
        assert!(!state.todo_dock_open);
    }
}
