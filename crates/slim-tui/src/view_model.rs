use crate::app::{ActivityPhase, AppState};
use crate::block::{BlockKind, BlockLifecycle, FoldState};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub lines: Vec<String>,
}

pub struct ViewModel;

fn safe(text: &str) -> String {
    crate::markdown::sanitize_terminal_text(text)
}

pub(crate) fn display_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim();
    let stripped = if let Some(rest) = trimmed.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = trimmed.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        trimmed.to_owned()
    };
    safe(&stripped).replace('\n', " ")
}

pub(crate) fn is_trivial_cwd(cwd: &str) -> bool {
    fn normalized(path: &str) -> String {
        display_cwd(path)
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    }

    let cwd = normalized(cwd);
    if cwd.is_empty() || cwd == "~" {
        return true;
    }
    [std::env::var("USERPROFILE"), std::env::var("HOME")]
        .into_iter()
        .flatten()
        .any(|home| !home.trim().is_empty() && cwd == normalized(&home))
}

pub(crate) fn run_status_label(state: &AppState) -> &'static str {
    if state.working {
        "RUNNING"
    } else if state.authenticated {
        "READY"
    } else {
        "SIGNED OUT"
    }
}

pub(crate) fn activity_label(state: &AppState) -> String {
    match state.activity.as_ref().map(|activity| &activity.phase) {
        Some(ActivityPhase::Thinking) => "Thinking".into(),
        Some(ActivityPhase::Responding) => "Responding".into(),
        Some(ActivityPhase::AwaitingProvider) => "Waiting for provider".into(),
        Some(ActivityPhase::RunningTool(name)) => safe(name),
        Some(ActivityPhase::WaitingForInput) => "Waiting for input".into(),
        Some(ActivityPhase::External(label)) => safe(label),
        None => "Working".into(),
    }
}

pub(crate) fn activity_elapsed(state: &AppState) -> u64 {
    let started = state
        .activity
        .as_ref()
        .map(|activity| activity.started_ms)
        .or(state.run_started_ms)
        .unwrap_or(state.clock.elapsed_ms);
    state.clock.elapsed_ms.saturating_sub(started) / 1_000
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionRailProjection {
    pub identity: String,
    pub status: String,
    pub gap: usize,
}

impl SessionRailProjection {
    fn line(&self) -> String {
        format!("{}{}{}", self.identity, " ".repeat(self.gap), self.status)
    }
}

pub(crate) fn truncate_display_width(text: &str, max_width: usize) -> String {
    let mut width = 0usize;
    unicode_segmentation::UnicodeSegmentation::graphemes(text, true)
        .take_while(|grapheme| {
            let next = unicode_width::UnicodeWidthStr::width(*grapheme);
            if width.saturating_add(next) > max_width {
                false
            } else {
                width += next;
                true
            }
        })
        .collect()
}

pub(crate) fn session_rail_projection(
    state: &AppState,
    available: usize,
    activity_rail_visible: bool,
) -> SessionRailProjection {
    let status_label = run_status_label(state);
    let context_full = format_context(state, false);
    let context_compact = format_context(state, true);
    let context_known = context_full != "ctx --";
    let append_context = |base: String, context: &str| {
        if context_known {
            format!("{base} · {context}")
        } else {
            base
        }
    };
    let active = state.working || state.activity.is_some();
    let candidates = if active && !activity_rail_visible {
        let phase = activity_label(state);
        let elapsed = activity_elapsed(state);
        [
            append_context(
                format!("{status_label} · {phase} · {elapsed}s"),
                &context_full,
            ),
            append_context(format!("{status_label} · {phase}"), &context_compact),
            append_context(status_label.into(), &context_compact),
        ]
    } else {
        [
            append_context(status_label.into(), &context_full),
            append_context(status_label.into(), &context_compact),
            status_label.into(),
        ]
    };
    let status_budget = available.saturating_sub(5);
    let status = candidates
        .into_iter()
        .find(|candidate| {
            unicode_width::UnicodeWidthStr::width(candidate.as_str()) <= status_budget
        })
        .unwrap_or_else(|| truncate_display_width(status_label, status_budget));
    let status_width = unicode_width::UnicodeWidthStr::width(status.as_str());
    let identity_budget = available.saturating_sub(status_width + 1);
    let identity = if is_trivial_cwd(&state.cwd) {
        truncate_display_width("SLIM", identity_budget)
    } else {
        let safe_cwd = display_cwd(&state.cwd);
        let prefix = "SLIM · ";
        let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
        let cwd_budget = identity_budget.saturating_sub(prefix_width);
        let cwd = if unicode_width::UnicodeWidthStr::width(safe_cwd.as_str()) > cwd_budget
            && cwd_budget > 0
        {
            format!(
                "{}…",
                truncate_display_width(&safe_cwd, cwd_budget.saturating_sub(1))
            )
        } else {
            truncate_display_width(&safe_cwd, cwd_budget)
        };
        truncate_display_width(&format!("{prefix}{cwd}"), identity_budget)
    };
    let gap = available
        .saturating_sub(unicode_width::UnicodeWidthStr::width(identity.as_str()) + status_width)
        .max(1);
    SessionRailProjection {
        identity,
        status,
        gap,
    }
}

/// Plain-text projection of the state. The fullscreen surface renders styled
/// chrome directly from `AppState`; this frame keeps headless/tests honest.
impl ViewModel {
    pub fn derive(state: &AppState) -> Frame {
        Self::derive_with_session_rail(state, false, 80)
    }

    pub fn derive_with_session_rail(
        state: &AppState,
        session_rail_visible: bool,
        width: u16,
    ) -> Frame {
        let mut lines = Vec::new();
        if session_rail_visible {
            lines.push(session_rail_projection(state, width as usize, state.working).line());
        }
        for block in state.blocks() {
            if block.turn_boundary_before() {
                lines.push(String::new());
            }
            match block.kind() {
                BlockKind::User(text) => {
                    lines.push("You".into());
                    lines.push(format!("> {}", safe(text)));
                }
                BlockKind::Assistant(text) => {
                    lines.push("Slim".into());
                    lines.push(safe(text));
                }
                BlockKind::Thinking(text) if block.fold == FoldState::Collapsed => {
                    let text = safe(text);
                    let preview = text.lines().next().unwrap_or_default();
                    lines.push(format!("Thinking: {preview}"));
                }
                BlockKind::Thinking(text) => lines.push(format!("Thinking: {}", safe(text))),
                BlockKind::Tool(state) => {
                    let name = safe(&state.name);
                    let preview = safe(&state.preview);
                    let line = match block.lifecycle {
                        BlockLifecycle::Pending => format!("○ {name}: {preview}"),
                        BlockLifecycle::Streaming => format!("◌ {name}: {preview}"),
                        BlockLifecycle::Complete => format!("✓ {name}: {preview}"),
                        BlockLifecycle::Failed => format!("✕ {name} (failed): {preview}"),
                        BlockLifecycle::Cancelled => {
                            format!("■ {name} (cancelled): {preview}")
                        }
                    };
                    lines.push(line);
                }
                BlockKind::InteractionRequest(state) => {
                    lines.extend(
                        safe(&state.display_text())
                            .lines()
                            .map(|line| format!("? {line}")),
                    );
                }
                BlockKind::System(text) => lines.push(format!("system: {}", safe(text))),
                BlockKind::Error(text) => lines.push(format!("error: {}", safe(text))),
                BlockKind::Activity(text) => lines.push(format!("activity: {}", safe(text))),
                BlockKind::QueuedUser(text) => lines.push(format!("> {}", safe(text))),
            }
        }
        if let Some(activity) = &state.activity {
            let label = match &activity.phase {
                ActivityPhase::Thinking => "Thinking".into(),
                ActivityPhase::Responding => "Responding".into(),
                ActivityPhase::AwaitingProvider => "Waiting for provider".into(),
                ActivityPhase::RunningTool(name) => format!("Running {}", safe(name)),
                ActivityPhase::WaitingForInput => "Waiting for input".into(),
                ActivityPhase::External(label) => safe(label),
            };
            lines.push(format!("activity: {label}"));
        }
        for notification in state.visible_toast_tail(3) {
            lines.push(format!("notice: {}", safe(notification)));
        }
        if state.todo_dock_open {
            let total = state.todo_items.len();
            let done = state
                .todo_items
                .iter()
                .filter(|item| item.status == crate::api::TodoItemStatus::Completed)
                .count();
            let active = state
                .todo_items
                .iter()
                .find(|item| item.status == crate::api::TodoItemStatus::InProgress)
                .map(|item| safe(&item.title))
                .unwrap_or_else(|| "no active item".into());
            lines.push(format!("todo: {done}/{total} {active}"));
        }
        lines.push(format!(
            "composer: {}",
            safe(&state.composer.display_for_width(80))
        ));
        lines.push(status_line(state, session_rail_visible));
        Frame { lines }
    }
}

pub(crate) fn format_token_count(tokens: u64) -> String {
    const UNITS: [&str; 7] = ["", "k", "M", "G", "T", "P", "E"];
    let mut divisor = 1_u64;
    let mut unit = 0_usize;
    while unit + 1 < UNITS.len() && tokens / divisor >= 1_000 {
        divisor = divisor.saturating_mul(1_000);
        unit += 1;
    }
    if unit == 0 {
        return tokens.to_string();
    }
    let whole = tokens / divisor;
    let tenth = ((tokens % divisor) as u128 * 10 / divisor as u128) as u64;
    if tenth == 0 || whole >= 100 {
        format!("{whole}{}", UNITS[unit])
    } else {
        format!("{whole}.{tenth}{}", UNITS[unit])
    }
}

/// One context formatter shared by fullscreen/headless projections. `~` means
/// the active request is still estimated; zero window means genuinely unknown.
pub fn format_context(state: &AppState, compact: bool) -> String {
    if state.context_window_tokens == 0 {
        return "ctx --".into();
    }
    let pct = {
        let raw = ((state.context_tokens as u128 * 100) / state.context_window_tokens as u128)
            .min(999) as u64;
        if state.context_tokens > 0 && raw == 0 {
            1
        } else {
            raw
        }
    };
    let estimate = if state.context_exact { "" } else { "~" };
    let compaction = match state.compaction_status {
        slim_core::context::CompactionStatus::Preparing => " preparing",
        slim_core::context::CompactionStatus::Ready => " ready",
        _ => "",
    };
    if compact {
        format!("ctx {estimate}{pct}%{compaction}")
    } else {
        format!(
            "ctx {estimate}{pct}%{compaction} · {}/{}",
            format_token_count(state.context_tokens),
            format_token_count(state.context_window_tokens)
        )
    }
}

/// Plain-text mirror of the W8 footer: critical state or real shortcuts on the
/// left, context/usage on the right. Model/effort/mode live in composer chrome.
pub fn status_line(state: &AppState, context_in_session_rail: bool) -> String {
    let left = if state.working && state.activity.is_some() {
        "Shift+Tab mode · Ctrl+C cancel · Ctrl+P commands".into()
    } else if state.working {
        "Working · Esc stop · Esc×2 force".into()
    } else if state.scroll.is_pinned() {
        if state.scroll.unseen > 0 {
            format!("{} new · End latest", state.scroll.unseen)
        } else {
            "End latest".into()
        }
    } else if !state.authenticated {
        "signed out · /login".into()
    } else {
        "Shift+Tab mode · Ctrl+C exit · Ctrl+P commands".into()
    };
    let formatted_context = format_context(state, false);
    let context = if context_in_session_rail || formatted_context == "ctx --" {
        String::new()
    } else {
        format!("{formatted_context} · ")
    };
    let usage_visible = state.input_tokens > 0
        || state.output_tokens > 0
        || state.input_tokens_overflowed
        || state.output_tokens_overflowed;
    let usage = usage_visible.then(|| {
        format!(
            "↑{}{} ↓{}{}",
            state.input_tokens,
            if state.input_tokens_overflowed {
                "+"
            } else {
                ""
            },
            state.output_tokens,
            if state.output_tokens_overflowed {
                "+"
            } else {
                ""
            }
        )
    });
    match (context.is_empty(), usage) {
        (true, None) => left,
        (_, Some(usage)) => format!("{left}  {context}{usage}"),
        (false, None) => format!("{left}  {}", context.trim_end_matches(" · ")),
    }
}
