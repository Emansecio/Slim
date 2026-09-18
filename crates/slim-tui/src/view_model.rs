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
        "EM EXECUÇÃO"
    } else if state.authenticated {
        "PRONTO"
    } else {
        "DESCONECTADO"
    }
}

pub(crate) fn activity_label(state: &AppState) -> String {
    if let Some(cancel) = &state.cancellation {
        return match cancel.phase {
            crate::app::CancellationPhase::Requested => "Interrupção solicitada",
            crate::app::CancellationPhase::InProgress => "Interrompendo",
        }
        .into();
    }
    if let Some(retry) = &state.retry {
        let remaining = retry
            .scheduled_ms
            .saturating_add(retry.wait_ms)
            .saturating_sub(state.clock.elapsed_ms)
            .div_ceil(1_000);
        return format!(
            "Nova tentativa · {}/{} · em {remaining} s",
            retry.attempt, retry.limit
        );
    }
    match state.activity.as_ref().map(|activity| &activity.phase) {
        Some(ActivityPhase::Thinking) => "Pensando".into(),
        Some(ActivityPhase::AwaitingProvider) => "Aguardando provedor".into(),
        Some(ActivityPhase::Responding) => "Respondendo".into(),
        Some(ActivityPhase::PreparingTool(name)) => preparation_label(name),
        Some(ActivityPhase::QueuedTool(name)) => format!("Aguardando execução · {}", safe(name)),
        Some(ActivityPhase::RunningTool(name)) => {
            let live = live_tool_names(state);
            if live.is_empty() {
                tool_activity_phrase(&[name.as_str()])
            } else {
                tool_activity_phrase(&live)
            }
        }
        Some(ActivityPhase::WaitingForInput) => "Aguardando resposta".into(),
        Some(ActivityPhase::External(label)) => external_activity_label(label),
        None => "Em atividade".into(),
    }
}

fn external_activity_label(label: &str) -> String {
    let label = label.trim();
    let known = match label {
        "Connecting to provider" => Some("Conectando ao provedor"),
        "Waiting for first byte" => Some("Aguardando primeiro byte"),
        "Stream open · waiting for content" => Some("Conexão aberta · aguardando conteúdo"),
        "Provider responding" => Some("Provedor respondendo"),
        "Preparing tool" => Some("Preparando ferramenta"),
        "Compacting context" => Some("Compactando contexto"),
        _ => None,
    };
    if let Some(known) = known {
        return known.into();
    }
    if label.starts_with("Retrying") || label.starts_with("Compacting") {
        return safe(label);
    }
    if let Some(name) = label.strip_prefix("Preparing tool · ") {
        let name = name.trim();
        if !name.is_empty() {
            return preparation_label(name);
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
            "Pensando".into()
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
        1 => parts.push(if completed { "Leu" } else { "Lendo" }.into()),
        n => parts.push(if completed {
            format!("{n} chamadas de leitura")
        } else {
            format!("Lendo · {n} chamadas")
        }),
    }
    match searching {
        0 => {}
        1 => parts.push(if completed {
            "1 busca".into()
        } else {
            "Buscando".into()
        }),
        n => parts.push(format!("{n} chamadas de busca")),
    }
    match editing {
        0 => {}
        1 => parts.push(if completed { "Editou" } else { "Editando" }.into()),
        n => parts.push(if completed {
            format!("{n} chamadas de edição")
        } else {
            format!("Editando · {n} chamadas")
        }),
    }
    match shell {
        0 => {}
        1 => parts.push(if completed {
            "Executou 1 comando".into()
        } else {
            "Executando comando".into()
        }),
        n => parts.push(if completed {
            format!("Executou {n} comandos")
        } else {
            format!("Executando · {n} comandos")
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
        "Slim · interrompido · parcial"
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

fn preparation_label(name: &str) -> String {
    let action = match name {
        "read" => "leitura",
        "search" => "busca",
        "write" | "patch" => "edição",
        "shell" => "comando",
        other => other,
    };
    format!("Preparando {}", safe(action))
}

pub(crate) fn activity_phase_label(phase: &ActivityPhase) -> String {
    match phase {
        ActivityPhase::Thinking => "Pensando".into(),
        ActivityPhase::Responding => "Respondendo".into(),
        ActivityPhase::AwaitingProvider => "Aguardando provedor".into(),
        ActivityPhase::PreparingTool(name) => preparation_label(name),
        ActivityPhase::QueuedTool(name) => format!("Aguardando execução · {}", safe(name)),
        ActivityPhase::RunningTool(name) => tool_activity_phrase(&[name]),
        ActivityPhase::WaitingForInput => "Aguardando resposta".into(),
        ActivityPhase::External(label) => external_activity_label(label),
    }
}

pub(crate) fn truncate_middle(text: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let basename = text.rsplit(['/', '\\']).next().unwrap_or(text);
    let tail_budget = (max_width.saturating_sub(1) * 2 / 3)
        .max(UnicodeWidthStr::width(basename).min(max_width.saturating_sub(1)));
    let mut used = 0;
    let tail: Vec<_> = text
        .graphemes(true)
        .rev()
        .take_while(|g| {
            let next = UnicodeWidthStr::width(*g);
            if used + next > tail_budget {
                false
            } else {
                used += next;
                true
            }
        })
        .collect();
    format!(
        "{}…{}",
        truncate_display_width(text, max_width - 1 - used),
        tail.into_iter().rev().collect::<String>()
    )
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
        let cwd = truncate_middle(&safe_cwd, cwd_budget);
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
                    lines.push("Você".into());
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
                    lines.push(format!("Pensamento: {preview}"));
                }
                BlockKind::Thinking(text) => lines.push(format!("Pensamento: {}", safe(text))),
                BlockKind::Tool(state) => {
                    let name = safe(&state.name);
                    let preview = safe(&state.preview);
                    let line = match block.lifecycle {
                        BlockLifecycle::Pending => format!("○ {name}: {preview}"),
                        BlockLifecycle::Streaming => format!("◌ {name}: {preview}"),
                        BlockLifecycle::Complete if state.historical => {
                            format!("- {name} · histórico")
                        }
                        BlockLifecycle::Complete => format!("✓ {name}: {preview}"),
                        BlockLifecycle::Failed => format!("✕ {name} (falhou): {preview}"),
                        BlockLifecycle::Cancelled => {
                            format!("■ {name} (interrompida): {preview}")
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
                .unwrap_or_else(|| "sem tarefa ativa".into());
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
        slim_core::context::CompactionStatus::Preparing => " preparando",
        slim_core::context::CompactionStatus::Ready => " pronto",
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
    let mode = if state.mode == slim_core::OperatingMode::Jev {
        "Jev · reasoning OFF"
    } else {
        mode_name(state.mode)
    };
    let controls = if state.working {
        let phase = if activity_visible {
            String::new()
        } else {
            format!("{} · ", activity_label(state))
        };
        let base_variants = vec![
            format!("{phase}Esc parar · Esc×2 forçar · Ctrl+C cancelar"),
            format!("{phase}Esc parar · Ctrl+C cancelar"),
            format!("{phase}Ctrl+C cancelar"),
            format!("{phase}Ctrl+C"),
            format!("{phase}^C"),
            "Ctrl+C cancelar".into(),
            "^C".into(),
        ];
        if state.scroll.is_pinned() {
            let edge_variants = if state.scroll.unseen > 0 {
                vec![
                    format!("{} novas · End recentes", state.scroll.unseen),
                    format!("{} novas · End", state.scroll.unseen),
                    format!("{} novas", state.scroll.unseen),
                    format!("{}↑", state.scroll.unseen),
                ]
            } else {
                vec!["End recentes".into(), "End".into()]
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
            format!("{} novas · ", state.scroll.unseen)
        } else {
            String::new()
        };
        fit(vec![
            format!("{unseen}End recentes"),
            "End recentes".into(),
            "End".into(),
        ])
    } else if !state.authenticated {
        fit(vec!["desconectado · /login".into(), "/login".into()])
    } else if let Some(execution) = &state.last_execution {
        let outcome = match execution.outcome {
            crate::app::RunOutcomeKind::Completed => "concluída",
            crate::app::RunOutcomeKind::Interrupted => "interrompida",
            crate::app::RunOutcomeKind::Failed => "falhou",
        };
        let duration = if execution.duration_ms < 1_000 {
            "<1s".to_owned()
        } else {
            format!("{}s", execution.duration_ms / 1_000)
        };
        let pending = if execution.pending_count > 0 {
            format!(" · {} pendente(s)", execution.pending_count)
        } else {
            String::new()
        };
        let summary = format!("Execução {outcome} · {duration}{pending}");
        fit(vec![
            format!("{summary} · Ctrl+J detalhes · F1 ajuda"),
            format!("{summary} · Ctrl+J"),
            summary,
            format!("{outcome} · {duration}{pending}"),
            truncate_display_width(&format!("{outcome}{pending}"), width),
        ])
    } else {
        fit(vec![
            "F1 ajuda · / comandos · Shift+Tab · Ctrl+P · Ctrl+C sair".into(),
            "F1 ajuda · / comandos · Ctrl+P · Ctrl+C sair".into(),
            "F1 ajuda · Shift+Tab · Ctrl+P comandos".into(),
            "/ comandos · Ctrl+P · Ctrl+C sair".into(),
            "F1 ajuda · Ctrl+P comandos".into(),
            "Ctrl+P comandos".into(),
            "^P".into(),
        ])
    };
    let mode_line = if state.authenticated && !state.working && !state.scroll.is_pinned() {
        fit(vec![format!("{mode} · Shift+Tab modo"), mode.to_owned()])
    } else {
        truncate_display_width(mode, width)
    };
    match rows {
        1 => {
            let mut line = if state.working
                || state.scroll.is_pinned()
                || !state.authenticated
                || state.last_execution.is_some()
            {
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
    let effort = if state.mode == slim_core::OperatingMode::Jev {
        "(reasoning OFF)".to_owned()
    } else {
        format!("({})", state.effort.id())
    };
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

#[cfg(test)]
mod minimal_footer_tests {
    use super::{completed_tool_phrase, footer_lines};
    use crate::app::AppState;
    #[test]
    fn summaries_count_operations_without_claiming_unique_files() {
        assert_eq!(
            completed_tool_phrase(&["read", "read"]),
            "2 chamadas de leitura"
        );
        assert_eq!(
            completed_tool_phrase(&["read", "list", "code_intel"]),
            "Leu, list, code_intel"
        );
        assert_eq!(
            completed_tool_phrase(&["patch", "patch"]),
            "2 chamadas de edição"
        );
        assert_eq!(
            completed_tool_phrase(&["shell", "shell"]),
            "Executou 2 comandos"
        );
    }
    #[test]
    fn idle_footer_keeps_observed_run_result_without_claiming_task_success() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.last_execution = Some(crate::app::ExecutionSummary {
            run_id: 1,
            started_ms: 10,
            ended_ms: 3210,
            duration_ms: 3200,
            outcome: crate::app::RunOutcomeKind::Completed,
            pending_count: 2,
        });
        for rows in [1, 2, 3] {
            let lines = footer_lines(&state, 98, rows, false).join("\n");
            assert!(
                lines.contains("Execução concluída · 3s · 2 pendente(s)"),
                "{lines}"
            );
            assert!(lines.contains("Ctrl+J"));
        }
        state.working = true;
        let active = footer_lines(&state, 98, 1, false).join("\n");
        assert!(active.contains("Ctrl+C"));
        assert!(!active.contains("Execução concluída"));
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
            assert!(lines.contains("^C") || lines.contains("Ctrl+C"), "{lines}");
        }
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
        assert_eq!(label(ActivityPhase::Thinking), "Pensando");
        assert_eq!(
            label(ActivityPhase::AwaitingProvider),
            "Aguardando provedor"
        );
    }

    #[test]
    fn provider_phase_labels_do_not_fake_reasoning() {
        for (phase, expected) in [
            ("Connecting to provider", "Conectando ao provedor"),
            ("Waiting for first byte", "Aguardando primeiro byte"),
            (
                "Stream open · waiting for content",
                "Conexão aberta · aguardando conteúdo",
            ),
            ("Provider responding", "Provedor respondendo"),
            ("Preparing tool", "Preparando ferramenta"),
        ] {
            assert_eq!(label(ActivityPhase::External(phase.into())), expected);
        }
    }

    #[test]
    fn shortened_path_preserves_workspace_and_cell_budget() {
        let path = "C:\\muito-longo\\caminho\\workspace-日本語";
        let short = super::truncate_middle(path, 24);
        assert!(short.ends_with("workspace-日本語"));
        assert!(unicode_width::UnicodeWidthStr::width(short.as_str()) <= 24);
        assert_eq!(super::truncate_middle(path, 0), "");
    }

    #[test]
    fn typed_retry_uses_deadline_and_cancellation_takes_priority() {
        let mut state = AppState::new();
        state.retry = Some(crate::app::RetryState {
            attempt: 1,
            limit: 2,
            wait_ms: 30_000,
            scheduled_ms: 1_000,
            reason: None,
        });
        state.clock.elapsed_ms = 8_000;
        assert_eq!(activity_label(&state), "Nova tentativa · 1/2 · em 23 s");
        state.clock.elapsed_ms = 40_000;
        assert_eq!(activity_label(&state), "Nova tentativa · 1/2 · em 0 s");
        state.cancellation = Some(crate::app::CancellationState {
            run_id: 1,
            phase: crate::app::CancellationPhase::Requested,
            requested_ms: 40_000,
        });
        assert_eq!(activity_label(&state), "Interrupção solicitada");
    }
}
