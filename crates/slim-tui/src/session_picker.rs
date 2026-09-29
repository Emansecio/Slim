//! State of the `/resume` and `/rewind` pickers (DESIGN-SLIM-TUI §15.8).
//!
//! Pure data: the host lists sessions and turns, the reducer feeds them in,
//! `runtime.rs` draws them. Nothing here reads a clock or the disk.

use crate::api::{SessionListItem, TurnListItem};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerKind {
    /// `/resume`: choose a previous session of this workspace.
    Resume,
    /// `/rewind`: choose the finished turn to go back to.
    Rewind,
}

/// One filtered row, in display order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerRow<'a> {
    Session(&'a SessionListItem),
    Turn(&'a TurnListItem),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPicker {
    pub kind: PickerKind,
    /// Matches the host's answer to this open picker; older answers are ignored.
    pub request_id: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub sessions: Vec<SessionListItem>,
    /// Host clock when `sessions` was listed (Unix epoch ms), for ages.
    pub now_ms: u64,
    pub turns: Vec<TurnListItem>,
    /// Case-insensitive; every whitespace-separated term must appear.
    pub filter: String,
    /// Index into the filtered rows.
    pub selected: usize,
    pub viewport_start: usize,
}

impl SessionPicker {
    pub fn new(kind: PickerKind, request_id: u64) -> Self {
        Self {
            kind,
            request_id,
            loading: true,
            error: None,
            sessions: Vec::new(),
            now_ms: 0,
            turns: Vec::new(),
            filter: String::new(),
            selected: 0,
            viewport_start: 0,
        }
    }

    pub fn set_sessions(&mut self, now_ms: u64, mut sessions: Vec<SessionListItem>) {
        // Newest first regardless of how the host ordered them.
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
        self.now_ms = now_ms;
        self.sessions = sessions;
        self.loading = false;
        self.error = None;
        self.select_first_resumable();
    }

    pub fn set_turns(&mut self, mut turns: Vec<TurnListItem>) {
        // Rewinding usually targets a recent turn: newest first.
        turns.sort_by_key(|turn| std::cmp::Reverse(turn.index));
        self.turns = turns;
        self.loading = false;
        self.error = None;
        self.selected = 0;
        self.viewport_start = 0;
    }

    pub fn fail(&mut self, message: String) {
        self.loading = false;
        self.error = Some(message);
    }

    /// Lands on the first session that can actually be resumed, so Enter on a
    /// freshly opened list does what `/resume` always did.
    fn select_first_resumable(&mut self) {
        self.selected = self
            .rows()
            .iter()
            .position(|row| match row {
                PickerRow::Session(session) => !session.current && !session.in_use,
                PickerRow::Turn(_) => true,
            })
            .unwrap_or(0);
        self.viewport_start = 0;
    }

    pub fn rows(&self) -> Vec<PickerRow<'_>> {
        let terms: Vec<String> = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let matches = |haystack: &str| {
            let haystack = haystack.to_lowercase();
            terms.iter().all(|term| haystack.contains(term.as_str()))
        };
        match self.kind {
            PickerKind::Resume => self
                .sessions
                .iter()
                .filter(|session| {
                    terms.is_empty()
                        || matches(&format!(
                            "{} {} {}",
                            session.title.as_deref().unwrap_or_default(),
                            session.first_prompt,
                            session.id
                        ))
                })
                .map(PickerRow::Session)
                .collect(),
            PickerKind::Rewind => self
                .turns
                .iter()
                .filter(|turn| terms.is_empty() || matches(&turn.prompt))
                .map(PickerRow::Turn)
                .collect(),
        }
    }

    /// Rows currently shown and the total before filtering.
    pub fn counts(&self) -> (usize, usize) {
        let total = match self.kind {
            PickerKind::Resume => self.sessions.len(),
            PickerKind::Rewind => self.turns.len(),
        };
        (self.rows().len(), total)
    }

    pub fn selected_row(&self) -> Option<PickerRow<'_>> {
        self.rows().get(self.selected).copied()
    }

    /// Keeps `selected` inside the filtered rows after the filter changed.
    pub fn clamp_selection(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.selected = 0;
            self.viewport_start = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

/// `"agora"`, `"há 5 min"`, `"há 3 h"`, `"há 2 d"`, `"há 4 sem"`, `"há 7 meses"`,
/// or an empty string when the clock is unknown or the time is in the future.
pub fn relative_age(now_ms: u64, then_ms: u64) -> String {
    if now_ms == 0 || then_ms > now_ms {
        return String::new();
    }
    let seconds = (now_ms - then_ms) / 1000;
    match seconds {
        0..=44 => "agora".to_owned(),
        45..=3_599 => format!("há {} min", (seconds / 60).max(1)),
        3_600..=86_399 => format!("há {} h", seconds / 3_600),
        86_400..=1_209_599 => format!("há {} d", seconds / 86_400),
        1_209_600..=5_183_999 => format!("há {} sem", seconds / 604_800),
        _ => format!("há {} meses", seconds / 2_592_000),
    }
}

/// `"812 B"`, `"4,2 KB"`, `"1,3 MB"`.
pub fn compact_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0).replace('.', ",")
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)).replace('.', ",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, title: Option<&str>, prompt: &str, updated_ms: u64) -> SessionListItem {
        SessionListItem {
            id: id.into(),
            title: title.map(str::to_owned),
            first_prompt: prompt.into(),
            updated_ms,
            bytes: 2048,
            in_use: false,
            current: false,
        }
    }

    fn turn(index: usize, prompt: &str) -> TurnListItem {
        TurnListItem {
            index,
            first_seq: index as u64 * 10 + 2,
            prompt: prompt.into(),
        }
    }

    #[test]
    fn sessions_are_newest_first_and_start_on_the_first_resumable() {
        let mut picker = SessionPicker::new(PickerKind::Resume, 1);
        let mut current = session("tui-c", None, "atual", 9_000);
        current.current = true;
        let mut busy = session("tui-b", None, "ocupada", 8_000);
        busy.in_use = true;
        picker.set_sessions(
            10_000,
            vec![
                session("tui-a", Some("Refactor"), "antiga", 1_000),
                current,
                busy,
            ],
        );
        let ids: Vec<&str> = picker
            .rows()
            .iter()
            .map(|row| match row {
                PickerRow::Session(session) => session.id.as_str(),
                PickerRow::Turn(_) => unreachable!(),
            })
            .collect();
        assert_eq!(ids, ["tui-c", "tui-b", "tui-a"]);
        assert_eq!(picker.selected, 2, "skips the open and the locked sessions");
    }

    #[test]
    fn filter_requires_every_term_across_title_prompt_and_id() {
        let mut picker = SessionPicker::new(PickerKind::Resume, 1);
        picker.set_sessions(
            0,
            vec![
                session("tui-1", Some("Login flow"), "corrigir o bug do token", 3),
                session("tui-2", None, "escrever testes do login", 2),
                session("tui-3", None, "revisar docs", 1),
            ],
        );
        picker.filter = "LOGIN token".into();
        assert_eq!(picker.rows().len(), 1);
        picker.filter = "login".into();
        assert_eq!(picker.rows().len(), 2);
        picker.filter = "tui-3".into();
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.counts(), (1, 3));
    }

    #[test]
    fn turns_are_newest_first_and_filter_by_prompt() {
        let mut picker = SessionPicker::new(PickerKind::Rewind, 2);
        picker.set_turns(vec![
            turn(0, "primeiro"),
            turn(1, "segundo"),
            turn(2, "terceiro"),
        ]);
        assert!(matches!(picker.selected_row(), Some(PickerRow::Turn(t)) if t.index == 2));
        picker.filter = "segu".into();
        picker.clamp_selection();
        assert!(matches!(picker.selected_row(), Some(PickerRow::Turn(t)) if t.index == 1));
    }

    #[test]
    fn clamp_selection_follows_a_shrinking_filter() {
        let mut picker = SessionPicker::new(PickerKind::Rewind, 1);
        picker.set_turns((0..5).map(|index| turn(index, "x")).collect());
        picker.selected = 4;
        picker.filter = "zzz".into();
        picker.clamp_selection();
        assert_eq!((picker.selected, picker.rows().len()), (0, 0));
    }

    #[test]
    fn relative_age_and_size_read_naturally() {
        assert_eq!(relative_age(100_000, 99_000), "agora");
        assert_eq!(relative_age(1_000_000, 700_000), "há 5 min");
        assert_eq!(relative_age(20_000_000, 9_200_000), "há 3 h");
        assert_eq!(relative_age(400_000_000, 227_200_000), "há 2 d");
        assert_eq!(relative_age(0, 5), "");
        assert_eq!(relative_age(10, 20), "");
        assert_eq!(compact_size(812), "812 B");
        assert_eq!(compact_size(4300), "4,2 KB");
        assert_eq!(compact_size(1_400_000), "1,3 MB");
    }
}
