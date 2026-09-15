use slim_core::runtime::mode_name;

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
    static HOMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let homes = HOMES.get_or_init(|| {
        [std::env::var("USERPROFILE"), std::env::var("HOME")]
            .into_iter()
            .flatten()
            .filter(|home| !home.trim().is_empty())
            .map(|home| normalized(&home))
            .collect()
    });
    homes.iter().any(|home| &cwd == home)
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
        Some(ActivityPhase::AwaitingProvider) => "Waiting for provider".into(),
        Some(ActivityPhase::Responding) => "Responding".into(),
        Some(ActivityPhase::RunningTool(name)) => {
            let live = live_tool_names(state);
            if live.is_empty() {
                tool_activity_phrase(&[name.as_str()])
            } else {
                tool_activity_phrase(&live)
            }
        }
        Some(ActivityPhase::WaitingForInput) => "Waiting for input".into(),
        Some(ActivityPhase::External(label)) => external_activity_label(state, label),
        None => "Working".into(),
    }
}

fn external_activity_label(state: &AppState, label: &str) -> String {
    let label = label.trim();
    if label.starts_with("Retrying") || label.starts_with("Compacting") {
        return safe(label);
    }
    if let Some(name) = label.strip_prefix("Preparing tool · ") {
        let name = name.trim();
        if !name.is_empty() {
            let live = live_tool_names(state);
            if live.is_empty() {
                return tool_activity_phrase(&[name]);
            }
            return tool_activity_phrase(&live);
        }
    }
    safe(label)
}

fn live_tool_names(state: &AppState) -> Vec<&str> {
    state
        .blocks()
        .iter()
        .filter(|block| {
            matches!(
                block.lifecycle,
                BlockLifecycle::Pending | BlockLifecycle::Streaming
            )
        })
        .filter_map(|block| match block.kind() {
            BlockKind::Tool(tool) if !tool.historical => Some(tool.name.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_activity_phrase(names: &[&str]) -> String {
    tool_phrase(names, false)
}

pub(crate) fn completed_tool_phrase(names: &[&str]) -> String {
    tool_phrase(names, true)
}

fn tool_phrase(names: &[&str], completed: bool) -> String {
    if names.is_empty() {
        return if completed {
            String::new()
        } else {
            "Thinking".into()
        };
    }
    let mut reading = 0usize;
    let mut searching = 0usize;
    let mut editing = 0usize;
    let mut shell = 0usize;
    let mut others: Vec<(&str, usize)> = Vec::new();
    for name in names {
        match *name {
            "read" => reading += 1,
            "search" => searching += 1,
            "write" | "patch" => editing += 1,
            "shell" => shell += 1,
            other => {
                if let Some((_, count)) = others.iter_mut().find(|(existing, _)| *existing == other)
                {
                    *count += 1;
                } else {
                    others.push((other, 1));
                }
            }
        }
    }
    let mut parts = Vec::new();
    match reading {
        0 => {}
        1 => parts.push(if completed { "Read" } else { "Reading" }.into()),
        n => parts.push(if completed {
            format!("{n} reads")
        } else {
            format!("Reading · {n} calls")
        }),
    }
    match searching {
        0 => {}
        1 => parts.push(if completed {
            "1 search".into()
        } else {
            "Searching".into()
        }),
        n => parts.push(format!("{n} searches")),
    }
    match editing {
        0 => {}
        1 => parts.push(if completed { "Edited" } else { "Editing" }.into()),
        n => parts.push(if completed {
            format!("{n} edits")
        } else {
            format!("Editing · {n} calls")
        }),
    }
    match shell {
        0 => {}
        1 => parts.push(if completed {
            "Ran 1 command".into()
        } else {
            "Running command".into()
        }),
        n => parts.push(if completed {
            format!("Ran {n} commands")
        } else {
            format!("Running · {n} commands")
        }),
    }
    for (name, count) in others {
        let name = safe(name);
        if count == 1 {
            parts.push(name);
        } else {
            parts.push(format!("{name} ×{count}"));
        }
    }
    parts.join(", ")
}

pub(crate) fn budget_near_limit(used: usize, limit: usize) -> bool {
    used > 0 && limit > 0 && used.saturating_mul(5) >= limit.saturating_mul(4)
}

pub(crate) fn assistant_label(lifecycle: BlockLifecycle) -> &'static str {
    if lifecycle == BlockLifecycle::Cancelled {
        "Slim · interrupted · partial"
    } else {
        "Slim"
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
    _activity_rail_visible: bool,
) -> SessionRailProjection {
    let identity = if is_trivial_cwd(&state.cwd) {
        truncate_display_width("SLIM", available)
    } else {
        let safe_cwd = display_cwd(&state.cwd);
        let prefix = "SLIM · ";
        let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
        let cwd_budget = available.saturating_sub(prefix_width);
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
        truncate_display_width(&format!("{prefix}{cwd}"), available)
    };
    let gap = available.saturating_sub(unicode_width::UnicodeWidthStr::width(identity.as_str()));
    SessionRailProjection {
        identity,
        status: String::new(),
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
                    if !lines.is_empty() {
                        lines.push(String::new());
                    }
                    lines.push(assistant_label(block.lifecycle).into());
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
                        BlockLifecycle::Complete if state.historical => {
                            format!("- {name} · history")
                        }
                        BlockLifecycle::Complete => format!("✓ {name}: {preview}"),
                        BlockLifecycle::Failed => format!("✕ {name} (failed): {preview}"),
                        BlockLifecycle::Cancelled => {
                            format!("■ {name} (cancelled): {preview}")
                        }
                    };
                    lines.push(line);
                    if state.historical && block.fold == FoldState::Expanded {
                        lines.push(safe(&state.materialized_output));
                    }
                }
                BlockKind::InteractionRequest(state) => {
                    lines.extend(safe(&state.display_text()).lines().map(str::to_owned));
                }
                BlockKind::System(text) => lines.push(format!("system: {}", safe(text))),
                BlockKind::Error(text) => lines.push(format!("error: {}", safe(text))),
                BlockKind::Activity(text) => lines.push(format!("activity: {}", safe(text))),
                BlockKind::QueuedUser(text) => lines.push(format!("> {}", safe(text))),
            }
        }
        if state.activity.is_some() {
            lines.push(format!("activity: {}", activity_label(state)));
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
        lines.extend(footer_lines(
            state,
            width as usize,
            2,
            state.activity.is_some(),
        ));
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

/// Format tokens per second (stored as tenths of tok/s, e.g. 854 = 85.4 tok/s).
pub fn format_tokens_per_sec(tenths: u64) -> String {
    if tenths == 0 {
        return String::new();
    }
    if tenths < 1_000 {
        format!("{}.{} tok/s", tenths / 10, tenths % 10)
    } else if tenths < 100_000 {
        format!("{} tok/s", (tenths + 5) / 10)
    } else {
        format!("{}.{}k tok/s", tenths / 10_000, (tenths % 10_000) / 1_000)
    }
}

/// Compact tokens per second for narrow terminals (stored as tenths of tok/s).
pub fn format_tokens_per_sec_compact(tenths: u64) -> String {
    if tenths == 0 {
        return String::new();
    }
    if tenths < 100 {
        format!("{}.{} t/s", tenths / 10, tenths % 10)
    } else if tenths < 100_000 {
        format!("{} t/s", (tenths + 5) / 10)
    } else {
        format!("{}k t/s", (tenths + 5_000) / 10_000)
    }
}

/// Vertical fractional block characters (U+2581..=U+2588) for 8-level sparklines.
pub const FRACTIONAL_VERTICAL: [char; 8] = [' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Horizontal fractional block characters (U+258F..=U+2588) for 8x sub-character precision.
pub const FRACTIONAL_HORIZONTAL: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Render a sparkline from a series of values using vertical fractional blocks (tui-design visual-catalog).
/// Returns an empty string if values is empty. Scaled between 0 and the maximum observed value.
pub fn render_sparkline(values: &[u64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let max = *values.iter().max().unwrap_or(&0);
    if max == 0 {
        return " ".repeat(values.len());
    }
    values
        .iter()
        .map(|&v| {
            let level = ((v as u128 * 7) / max as u128) as usize;
            FRACTIONAL_VERTICAL[level.min(7)]
        })
        .collect()
}

/// Render a horizontal progress bar using sub-character fractional blocks (U+258F..=U+2588).
/// Provides 8x resolution per character cell.
pub fn render_fractional_bar(fraction: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped = fraction.clamp(0.0, 1.0);
    let total_eighths = (clamped * width as f64 * 8.0).round() as usize;
    let full_cells = (total_eighths / 8).min(width);
    let remainder = total_eighths % 8;

    let mut bar = String::with_capacity(width * 4);
    for _ in 0..full_cells {
        bar.push('█');
    }
    if full_cells < width {
        if remainder > 0 {
            bar.push(FRACTIONAL_HORIZONTAL[remainder - 1]);
            for _ in (full_cells + 1)..width {
                bar.push('░');
            }
        } else {
            for _ in full_cells..width {
                bar.push('░');
            }
        }
    }
    bar
}

/// High-resolution sub-character visual gauge of context window tokens used (tui-design visual-catalog).
/// e.g. `ctx [████▍░░░░░] 45%`
pub fn format_context_bar(state: &AppState, bar_width: usize) -> String {
    if state.context_window_tokens == 0 {
        return "ctx --".into();
    }
    let fraction = (state.context_tokens as f64) / (state.context_window_tokens as f64);
    let bar = render_fractional_bar(fraction, bar_width);
    let estimate = if state.context_exact { "" } else { "~" };
    let pct = ((fraction * 100.0).min(999.0)).round() as u64;
    format!("ctx [{bar}] {estimate}{pct}%")
}

/// A single projection for styled and plain footers. Each row fits in cells;
/// critical controls win over metadata when space is scarce.
pub(crate) fn footer_lines(
    state: &AppState,
    width: usize,
    rows: u16,
    activity_visible: bool,
) -> Vec<String> {
    if rows == 0 || width == 0 {
        return Vec::new();
    }
    let fit = |variants: Vec<String>| {
        variants
            .into_iter()
            .find(|s| unicode_width::UnicodeWidthStr::width(s.as_str()) <= width)
            .unwrap_or_default()
    };
    let mode = mode_name(state.mode);
    let controls = if state.working {
        let phase = if activity_visible {
            String::new()
        } else {
            format!("{} · ", activity_label(state))
        };
        let base_variants = vec![
            format!("{phase}Esc stop · Esc×2 force · Ctrl+C cancel"),
            format!("{phase}Esc stop · Ctrl+C cancel"),
            format!("{phase}Ctrl+C cancel"),
            format!("{phase}Ctrl+C"),
            format!("{phase}^C"),
            "Ctrl+C cancel".into(),
            "^C".into(),
        ];
        if state.scroll.is_pinned() {
            let edge_variants = if state.scroll.unseen > 0 {
                vec![
                    format!("{} new · End latest", state.scroll.unseen),
                    format!("{} new · End", state.scroll.unseen),
                    format!("{} new", state.scroll.unseen),
                    format!("{}↑", state.scroll.unseen),
                ]
            } else {
                vec!["End latest".into(), "End".into()]
            };
            let mut variants = Vec::with_capacity(
                base_variants
                    .len()
                    .saturating_mul(edge_variants.len())
                    .saturating_add(base_variants.len()),
            );
            for base in &base_variants {
                for edge in &edge_variants {
                    variants.push(format!("{base} · {edge}"));
                }
            }
            variants.extend(base_variants);
            fit(variants)
        } else {
            fit(base_variants)
        }
    } else if state.scroll.is_pinned() {
        let unseen = if state.scroll.unseen > 0 {
            format!("{} new · ", state.scroll.unseen)
        } else {
            String::new()
        };
        fit(vec![
            format!("{unseen}End latest"),
            "End latest".into(),
            "End".into(),
        ])
    } else if !state.authenticated {
        fit(vec!["signed out · /login".into(), "/login".into()])
    } else {
        fit(vec![
            "F1 help · / commands · Shift+Tab · Ctrl+P · Ctrl+C exit".into(),
            "F1 help · / commands · Ctrl+P · Ctrl+C exit".into(),
            "F1 help · Shift+Tab · Ctrl+P commands".into(),
            "/ commands · Ctrl+P · Ctrl+C exit".into(),
            "F1 help · Ctrl+P commands".into(),
            "Ctrl+P commands".into(),
            "^P".into(),
        ])
    };
    let mode_line = if state.authenticated && !state.working && !state.scroll.is_pinned() {
        fit(vec![format!("{mode} · Shift+Tab mode"), mode.to_owned()])
    } else {
        truncate_display_width(mode, width)
    };
    match rows {
        1 => {
            let mut line = if state.working || state.scroll.is_pinned() || !state.authenticated {
                controls
            } else {
                fit(vec![
                    format!("{mode} · Shift+Tab · F1/Ctrl+P"),
                    format!("{mode} · Shift+Tab · Ctrl+P"),
                    format!("{mode} · ^P"),
                    mode.to_owned(),
                ])
            };
            let context = format_context(state, true);
            if context != "ctx --"
                && unicode_width::UnicodeWidthStr::width(line.as_str())
                    + 3
                    + unicode_width::UnicodeWidthStr::width(context.as_str())
                    <= width
            {
                line.push_str(" · ");
                line.push_str(&context);
            }
            vec![line]
        }
        2 => {
            let prefix = format!("{mode} · ");
            let metadata = model_metadata(
                state,
                width.saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str())),
            );
            vec![
                if metadata.is_empty() {
                    truncate_display_width(mode, width)
                } else {
                    format!("{prefix}{metadata}")
                },
                controls,
            ]
        }
        _ => vec![mode_line, model_metadata(state, width), controls],
    }
}

pub(crate) fn model_metadata(state: &AppState, width: usize) -> String {
    let context = format_context(state, true);
    let context = if context == "ctx --" {
        String::new()
    } else {
        context
    };
    if !state.authenticated {
        return truncate_display_width(&context, width);
    }
    let model = crate::api::ModelAlias::parse(&state.model)
        .map_or_else(|| safe(&state.model), |alias| alias.label().to_owned());
    let effort = format!("({})", state.effort.id());
    let suffix = if context.is_empty() {
        effort.clone()
    } else {
        format!("{effort} · {context}")
    };
    let suffix_width = unicode_width::UnicodeWidthStr::width(suffix.as_str());
    if width >= suffix_width + 5 {
        return format!(
            "{} {suffix}",
            crate::picker::truncate_cells(&model, width - suffix_width - 1)
        );
    }
    if suffix_width <= width {
        suffix
    } else if unicode_width::UnicodeWidthStr::width(context.as_str()) <= width
        && !context.is_empty()
    {
        context
    } else {
        truncate_display_width(&effort, width)
    }
}

/// Compact footer projection retained for callers that request one row.
pub fn status_line(state: &AppState, _context_in_session_rail: bool) -> String {
    footer_lines(state, 80, 1, state.working && state.activity.is_some())
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[cfg(test)]
mod minimal_footer_tests {
    use super::{completed_tool_phrase, footer_lines};
    use crate::app::AppState;
    #[test]
    fn summaries_count_operations_without_claiming_unique_files() {
        assert_eq!(completed_tool_phrase(&["read", "read"]), "2 reads");
        assert_eq!(
            completed_tool_phrase(&["read", "list", "code_intel"]),
            "Read, list, code_intel"
        );
        assert_eq!(completed_tool_phrase(&["patch", "patch"]), "2 edits");
        assert_eq!(completed_tool_phrase(&["shell", "shell"]), "Ran 2 commands");
    }
    #[test]
    fn footer_degrades_by_cells_and_preserves_critical_controls() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.model = "model-宇宙-with-a-long-provider-name".into();
        state.context_window_tokens = 100_000;
        state.context_tokens = 40_000;
        for width in [2, 12, 24, 38, 78, 98] {
            for rows in [1, 2, 3] {
                let lines = footer_lines(&state, width, rows, false);
                assert_eq!(lines.len(), rows as usize);
                assert!(lines
                    .iter()
                    .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= width));
            }
        }
        state.working = true;
        for rows in [1, 2, 3] {
            let lines = footer_lines(&state, 24, rows, false).join("\n");
            assert!(
                lines.contains("^C") || lines.contains("Ctrl+C cancel"),
                "{lines}"
            );
        }
    }

    #[test]
    fn sparkline_renders_vertical_blocks_proportional_to_values() {
        use super::render_sparkline;
        assert_eq!(render_sparkline(&[]), "");
        assert_eq!(render_sparkline(&[0, 0, 0]), "   ");
        let spark = render_sparkline(&[0, 10, 20, 40, 70]);
        assert_eq!(spark.chars().count(), 5);
        assert_eq!(spark.chars().next(), Some(' '));
        assert_eq!(spark.chars().last(), Some('█'));
    }

    #[test]
    fn fractional_bar_sub_character_precision() {
        use super::render_fractional_bar;
        assert_eq!(render_fractional_bar(0.0, 0), "");
        assert_eq!(render_fractional_bar(0.0, 5), "░░░░░");
        assert_eq!(render_fractional_bar(1.0, 5), "█████");
        let half = render_fractional_bar(0.5, 4);
        assert_eq!(half, "██░░");
        let eighth = render_fractional_bar(0.125, 8);
        assert_eq!(eighth, "█░░░░░░░");
    }

    #[test]
    fn context_bar_renders_clean_gauge() {
        use super::format_context_bar;
        let mut state = AppState::new();
        assert_eq!(format_context_bar(&state, 8), "ctx --");
        state.context_window_tokens = 100;
        state.context_tokens = 50;
        state.context_exact = true;
        let bar = format_context_bar(&state, 6);
        assert_eq!(bar, "ctx [███░░░] 50%");
    }
}

#[cfg(test)]
mod activity_label_tests {
    use super::activity_label;
    use crate::app::{ActivityPhase, ActivityState, AppState};

    fn label(phase: ActivityPhase) -> String {
        let mut state = AppState::new();
        state.activity = Some(ActivityState {
            phase,
            started_ms: 0,
        });
        activity_label(&state)
    }

    #[test]
    fn awaiting_provider_is_not_presented_as_reasoning() {
        assert_eq!(label(ActivityPhase::Thinking), "Thinking");
        assert_eq!(
            label(ActivityPhase::AwaitingProvider),
            "Waiting for provider"
        );
    }

    #[test]
    fn provider_phase_labels_do_not_fake_reasoning() {
        for phase in [
            "Connecting to provider",
            "Waiting for first byte",
            "Stream open · waiting for content",
            "Provider responding",
            "Preparing tool",
        ] {
            assert_eq!(label(ActivityPhase::External(phase.into())), phase);
        }
    }
}
