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
    if state.prompt_is_busy() {
        "PREPARANDO PROMPT"
    } else if state.working {
        "EM EXECUÇÃO"
    } else if state.authenticated {
        "PRONTO"
    } else {
        "DESCONECTADO"
    }
}

pub(crate) fn activity_label(state: &AppState) -> String {
    if state.prompt_is_busy() {
        return "Preparando prompt".into();
    }
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
        "Connecting to provider" => Some("Aguardando resposta do provedor"),
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
    if let Some(rest) = label.strip_prefix("Preparing tool · ") {
        // `<name>` or `<name> · <size>` while the call is still being written.
        let mut parts = rest.splitn(2, " · ");
        let name = parts.next().unwrap_or_default().trim();
        if !name.is_empty() {
            return match parts.next().map(str::trim).filter(|size| !size.is_empty()) {
                Some(size) => format!("{} · {}", preparation_label(name), safe(size)),
                None => preparation_label(name),
            };
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
    // A settled group is a tally of calls, one noun per kind, so it never
    // mixes verbs and counts in the same row.
    let tally = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    match reading {
        0 => {}
        1 if !completed => parts.push("Lendo".into()),
        n if completed => parts.push(tally(n, "leitura", "leituras")),
        n => parts.push(format!("Lendo · {n} chamadas")),
    }
    match searching {
        0 => {}
        1 if !completed => parts.push("Buscando".into()),
        n if completed => parts.push(tally(n, "busca", "buscas")),
        n => parts.push(format!("{n} chamadas de busca")),
    }
    match editing {
        0 => {}
        1 if !completed => parts.push("Editando".into()),
        n if completed => parts.push(tally(n, "edição", "edições")),
        n => parts.push(format!("Editando · {n} chamadas")),
    }
    match shell {
        0 => {}
        1 if !completed => parts.push("Executando comando".into()),
        n if completed => parts.push(tally(n, "comando", "comandos")),
        n => parts.push(format!("Executando · {n} comandos")),
    }
    for (name, count) in others {
        if name == "shell_job" {
            parts.push(if count == 1 && !completed {
                "Consultando job".into()
            } else {
                tally(count, "consulta de job", "consultas de job")
            });
            continue;
        }
        let name = safe(name);
        if count == 1 {
            parts.push(name);
        } else {
            parts.push(format!("{name} ×{count}"));
        }
    }
    parts.join(", ")
}

/// What a settled tool call did to the world, which sets the weight of its
/// row: reads recede, commands and edits stand out, and only an applied
/// change earns the green marker. Tools that are not native keep the
/// quietest weight.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ToolEffect {
    Observes,
    Runs,
    Changes,
}

pub(crate) fn tool_effect(name: &str) -> ToolEffect {
    match name {
        "patch" | "write" => ToolEffect::Changes,
        "shell" => ToolEffect::Runs,
        _ => ToolEffect::Observes,
    }
}

/// Row title: a verb in the progressive while the call runs (or was
/// cancelled) and in the past once it settled. Other tools keep their name.
pub(crate) fn tool_title(name: &str, lifecycle: BlockLifecycle) -> String {
    let settled = matches!(lifecycle, BlockLifecycle::Complete | BlockLifecycle::Failed);
    let verb = match name {
        "read" => ("Lendo", "Leu"),
        "search" => ("Buscando", "Buscou"),
        "list" => ("Listando", "Listou"),
        "patch" => ("Editando", "Editou"),
        "write" => ("Escrevendo", "Escreveu"),
        "shell" => ("Executando", "Executou"),
        "code_intel" => ("Analisando", "Analisou"),
        "shell_job" => ("Consultando job", "Consultou job"),
        _ => return name.to_owned(),
    };
    if settled { verb.1 } else { verb.0 }.to_owned()
}

/// Argument summary segment as read on a tool row: the `key=` prefix of
/// the projected summary is dropped and commands read as `$ command`.
pub(crate) fn tool_target(segment: &str) -> String {
    if let Some(command) = segment
        .strip_prefix("command=")
        .or_else(|| segment.strip_prefix("program="))
    {
        return format!("$ {command}");
    }
    for key in ["path=", "file=", "target=", "url=", "name=", "id="] {
        if let Some(value) = segment.strip_prefix(key) {
            return value.to_owned();
        }
    }
    for key in ["pattern=", "query=", "glob="] {
        if let Some(value) = segment.strip_prefix(key) {
            return format!("\"{value}\"");
        }
    }
    segment.to_owned()
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
    /// Byte range of the `/rename` title inside `identity`, when one is shown.
    pub title: Option<std::ops::Range<usize>>,
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
    let mut title_range = None;
    let identity = if is_trivial_cwd(&state.cwd) {
        truncate_display_width("SLIM", available)
    } else {
        let safe_cwd = display_cwd(&state.cwd);
        let prefix = "SLIM · ";
        let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
        // A session name leads the rail; the directory keeps whatever room is
        // left, so a long name never hides where the session runs.
        let title = state
            .session_title
            .as_deref()
            .map(crate::markdown::sanitize_terminal_text)
            .filter(|title| !title.trim().is_empty())
            .map(|title| truncate_display_width(title.trim(), (available / 2).clamp(8, 40)));
        match title {
            Some(title) => {
                let separator = " · ";
                let used = prefix_width
                    + unicode_width::UnicodeWidthStr::width(title.as_str())
                    + unicode_width::UnicodeWidthStr::width(separator);
                let cwd = truncate_middle(&safe_cwd, available.saturating_sub(used));
                title_range = Some(prefix.len()..prefix.len() + title.len());
                truncate_display_width(&format!("{prefix}{title}{separator}{cwd}"), available)
            }
            None => {
                let cwd_budget = available.saturating_sub(prefix_width);
                let cwd = truncate_middle(&safe_cwd, cwd_budget);
                truncate_display_width(&format!("{prefix}{cwd}"), available)
            }
        }
    };
    let title_range =
        title_range.map(|range| range.start.min(identity.len())..range.end.min(identity.len()));
    let gap = available.saturating_sub(unicode_width::UnicodeWidthStr::width(identity.as_str()));
    SessionRailProjection {
        identity,
        title: title_range.filter(|range| range.start < range.end),
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
        for (index, block) in state.blocks().iter().enumerate() {
            if block.turn_boundary_before() || crate::block::transition_gap(state.blocks(), index) {
                lines.push(String::new());
            }
            if let Some(header) = crate::block::response_header(state.blocks(), index) {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                let lifecycle = if header.cancelled {
                    BlockLifecycle::Cancelled
                } else {
                    BlockLifecycle::Complete
                };
                lines.push(assistant_label(lifecycle).into());
            }
            match block.kind() {
                BlockKind::User(text) => {
                    lines.push("Você".into());
                    lines.push(format!("> {}", safe(text)));
                    if block.awaiting_agent() {
                        // The agent's header, already waiting for its first block.
                        lines.push(String::new());
                        lines.push(assistant_label(BlockLifecycle::Complete).into());
                    }
                }
                BlockKind::Assistant(text) => lines.push(safe(text)),
                BlockKind::Thinking(text) if block.fold == FoldState::Collapsed => {
                    let text = safe(text);
                    let preview = text.lines().next().unwrap_or_default();
                    lines.push(format!("Pensamento: {preview}"));
                }
                BlockKind::Thinking(text) => lines.push(format!("Pensamento: {}", safe(text))),
                BlockKind::Tool(state) => {
                    let name = if state.historical {
                        safe(&state.name)
                    } else {
                        tool_title(&safe(&state.name), block.lifecycle)
                    };
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
                BlockKind::Receipt(receipt) => {
                    lines.push(format!("receipt: {}", receipt.summary()))
                }
                BlockKind::Work(work) => lines.push(format!("work: {}", work.summary())),
            }
        }
        if state.activity.is_some() {
            lines.push(format!("activity: {}", activity_label(state)));
        }
        for notification in state.visible_toast_tail(3) {
            lines.push(format!("notice: {}", safe(notification)));
        }
        if !state.todo_items.is_empty() {
            lines.push(crate::todo::compact_summary(state));
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
    if compact {
        format!("ctx {estimate}{pct}%")
    } else {
        format!(
            "ctx {estimate}{pct}% · {}/{}",
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
    let mode = mode_name(state.mode);
    let controls = if state.prompt_is_busy() {
        fit(vec![
            "Esc cancelar preparação".into(),
            "Esc cancelar".into(),
            "Esc".into(),
        ])
    } else if state.working {
        let phase = if activity_visible {
            String::new()
        } else {
            format!("{} · ", activity_label(state))
        };
        // One way to stop is shown at a time: forcing only exists once a
        // stop was asked for, so it is offered only then.
        let stop = if state.cancellation.is_some() {
            "Esc forçar"
        } else {
            "Esc parar"
        };
        let base_variants = vec![
            format!("{phase}{stop}"),
            format!("{phase}Esc"),
            stop.into(),
            "Esc".into(),
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
    } else if let Some(execution) = state
        .last_execution
        .as_ref()
        .filter(|execution| execution_needs_attention(execution))
    {
        // A run that simply finished is closed by its receipt in the
        // transcript; the footer only keeps an outcome that asks for action.
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
        let pending = match execution.pending_count {
            0 => String::new(),
            1 => " · 1 pendente".to_owned(),
            count => format!(" · {count} pendentes"),
        };
        let summary = format!("Execução {outcome} · {duration}{pending}");
        fit(vec![
            format!("{summary} · Ctrl+J detalhes"),
            format!("{summary} · Ctrl+J"),
            summary,
            format!("{outcome} · {duration}{pending}"),
            truncate_display_width(&format!("{outcome}{pending}"), width),
        ])
    } else {
        fit(vec![
            "/ comandos · Shift+Tab modo · Ctrl+C sair".into(),
            "/ comandos · Shift+Tab modo".into(),
            "/ comandos".into(),
            "/".into(),
        ])
    };
    let mode_line = if state.authenticated && !state.working && !state.scroll.is_pinned() {
        fit(vec![format!("{mode} · Shift+Tab modo"), mode.to_owned()])
    } else {
        truncate_display_width(mode, width)
    };
    let jobs = state.running_jobs();
    let controls = if jobs > 0 {
        let jobs = if jobs == 1 {
            "1 job".to_owned()
        } else {
            format!("{jobs} jobs")
        };
        fit(vec![
            format!("{controls} · {jobs} · /jobs"),
            format!("{jobs} · /jobs · {controls}"),
            format!(
                "{jobs} · {}",
                if (state.working || state.prompt_is_busy()) && state.scroll.is_pinned() {
                    "Esc · End"
                } else if state.working || state.prompt_is_busy() {
                    "Esc parar"
                } else if state.scroll.is_pinned() {
                    "End recentes"
                } else {
                    "/jobs"
                }
            ),
            controls,
        ])
    } else {
        controls
    };
    match rows {
        1 => {
            let mut line = if state.working
                || state.scroll.is_pinned()
                || !state.authenticated
                || state
                    .last_execution
                    .as_ref()
                    .is_some_and(execution_needs_attention)
                || jobs > 0
            {
                controls
            } else {
                fit(vec![
                    format!("{mode} · Shift+Tab · / comandos"),
                    format!("{mode} · / comandos"),
                    format!("{mode} · /"),
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

/// A finished run stays in the footer only when it did not simply complete
/// or left tasks open.
fn execution_needs_attention(execution: &crate::app::ExecutionSummary) -> bool {
    execution.outcome != crate::app::RunOutcomeKind::Completed || execution.pending_count > 0
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

#[cfg(test)]
mod minimal_footer_tests {
    use super::{completed_tool_phrase, footer_lines};
    use crate::app::AppState;
    #[test]
    fn summaries_count_operations_without_claiming_unique_files() {
        assert_eq!(completed_tool_phrase(&["read", "read"]), "2 leituras");
        assert_eq!(
            completed_tool_phrase(&["read", "list", "code_intel"]),
            "1 leitura, list, code_intel"
        );
        assert_eq!(completed_tool_phrase(&["patch", "patch"]), "2 edições");
        assert_eq!(completed_tool_phrase(&["shell", "shell"]), "2 comandos");
    }

    #[test]
    fn job_control_reads_as_a_noun_not_as_an_internal_tool_name() {
        assert_eq!(
            completed_tool_phrase(&["shell", "shell", "shell_job", "shell_job"]),
            "2 comandos, 2 consultas de job"
        );
        assert_eq!(completed_tool_phrase(&["shell_job"]), "1 consulta de job");
        assert_eq!(
            super::tool_activity_phrase(&["shell_job"]),
            "Consultando job"
        );
        assert_eq!(
            super::tool_title("shell_job", crate::block::BlockLifecycle::Complete),
            "Consultou job"
        );
    }

    #[test]
    fn settled_groups_are_a_tally_of_nouns_and_running_ones_keep_their_verbs() {
        assert_eq!(
            completed_tool_phrase(&["read", "search", "patch", "shell"]),
            "1 leitura, 1 busca, 1 edição, 1 comando"
        );
        assert_eq!(
            completed_tool_phrase(&["search", "search", "shell", "read", "read", "read"]),
            "3 leituras, 2 buscas, 1 comando"
        );
        assert_eq!(super::tool_activity_phrase(&["read"]), "Lendo");
        assert_eq!(
            super::tool_activity_phrase(&["read", "read"]),
            "Lendo · 2 chamadas"
        );
        assert_eq!(
            super::tool_activity_phrase(&["shell", "shell"]),
            "Executando · 2 comandos"
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
                lines.contains("Execução concluída · 3s · 2 pendentes"),
                "{lines}"
            );
            assert!(lines.contains("Ctrl+J"));
        }
        // A run that simply completed is closed by its receipt.
        if let Some(execution) = state.last_execution.as_mut() {
            execution.pending_count = 0;
        }
        let idle = footer_lines(&state, 98, 2, false).join("\n");
        assert!(!idle.contains("Execução concluída"), "{idle}");
        assert!(idle.contains("/ comandos"), "{idle}");
        state.working = true;
        let active = footer_lines(&state, 98, 1, false).join("\n");
        assert!(active.contains("Esc parar"), "{active}");
        assert!(!active.contains("Execução concluída"));
    }

    #[test]
    fn force_is_offered_only_after_a_stop_was_requested() {
        let mut state = AppState::new();
        state.authenticated = true;
        state.working = true;
        let running = footer_lines(&state, 98, 2, true).join("\n");
        assert!(running.contains("Esc parar"), "{running}");
        assert!(!running.contains("forçar"), "{running}");
        assert!(!running.contains("Ctrl+C"), "{running}");
        state.cancellation = Some(crate::app::CancellationState {
            run_id: 1,
            phase: crate::app::CancellationPhase::Requested,
            requested_ms: 0,
        });
        let stopping = footer_lines(&state, 98, 2, true).join("\n");
        assert!(stopping.contains("Esc forçar"), "{stopping}");
        assert!(!stopping.contains("Esc parar"), "{stopping}");
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
            assert!(lines.contains("Esc"), "{lines}");
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
            ("Connecting to provider", "Aguardando resposta do provedor"),
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
