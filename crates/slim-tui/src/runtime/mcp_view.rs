//! `/mcp` manager and its OAuth sign-in panel (DESIGN-SLIM-TUI §15.7.2).
//!
//! Pure rendering: the list shows one entry per server (title, metrics,
//! target, state help), a notice row with the host's latest message (toasts
//! are hidden while the modal is open) and a footer with only the actions the
//! selected server accepts. The sign-in panel replaces the list while a
//! `/mcp login` waits for the browser: the full authorization URL, the
//! pasted-redirect field and the same notice row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::api::{McpServerView, McpStatusView};
use crate::app::{McpConfirm, McpOverlay, McpSignIn};
use crate::block::wrap_words;
use crate::markdown::sanitize_terminal_text;
use crate::picker::truncate_cells;

use super::{centered, focus_text, modal_block, set_cursor_in_rect, Palette};

/// Below this many rows only the selected server and the footer are shown.
const COMPACT_HEIGHT: u16 = 12;

/// Draws the manager, or the sign-in panel when one is waiting.
pub(super) fn render(
    frame: &mut ratatui::Frame,
    overlay: &McpOverlay,
    servers: &[McpServerView],
    palette: &Palette,
    cursor_focused: bool,
) {
    match &overlay.signin {
        Some(signin) => render_signin(frame, overlay, signin, palette, cursor_focused),
        None => render_list(frame, overlay, servers, palette),
    }
}

fn modal_width(frame_width: u16) -> u16 {
    let width = ((u32::from(frame_width) * 9) / 10) as u16;
    width
        .clamp(40, 96)
        .min(frame_width.saturating_sub(2).max(1))
}

/// Glyph, style and label of a status; color is complementary to the label.
fn state_look(status: McpStatusView, palette: &Palette) -> (char, Style, &'static str) {
    match status {
        McpStatusView::Ready => ('\u{25CF}', palette.success, "pronto"),
        McpStatusView::Connecting => ('\u{25CC}', palette.warning, "conectando"),
        McpStatusView::Disconnected => ('\u{25CC}', palette.muted, "desconectado"),
        McpStatusView::Disabled => ('\u{25CB}', palette.muted, "desativado"),
        McpStatusView::Failed => ('\u{2715}', palette.error, "falhou"),
        McpStatusView::Untrusted => ('!', palette.warning, "projeto sem confiança"),
        McpStatusView::NeedsAuth => ('!', palette.warning, "requer login"),
    }
}

fn count_label(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Metrics row: the known counts, then the exposure.
fn metrics(server: &McpServerView) -> String {
    let mut parts = Vec::new();
    if let Some(tools) = server.tools {
        parts.push(count_label(tools, "ferramenta", "ferramentas"));
    }
    if let Some(resources) = server.resources {
        parts.push(count_label(resources, "recurso", "recursos"));
    }
    if let Some(templates) = server.resource_templates.filter(|count| *count > 0) {
        parts.push(count_label(templates, "modelo", "modelos"));
    }
    if !server.exposure.is_empty() {
        parts.push(format!("exposição {}", server.exposure));
    }
    parts.join(" \u{b7} ")
}

/// Help row under the target: the error, or what to do next.
fn help(server: &McpServerView) -> Option<String> {
    match server.status {
        McpStatusView::Failed => server.error.clone(),
        McpStatusView::Untrusted => Some(
            "definido pelo slim.toml do projeto \u{b7} t confia no projeto (ou /mcp trust)".into(),
        ),
        McpStatusView::NeedsAuth => {
            let reason = server
                .error
                .clone()
                .unwrap_or_else(|| "servidor HTTP com OAuth".into());
            Some(format!(
                "{reason} \u{b7} l entra (ou /mcp login {})",
                server.name
            ))
        }
        McpStatusView::Disabled => Some("desativado no slim.toml \u{b7} a ativa".into()),
        McpStatusView::Ready | McpStatusView::Connecting | McpStatusView::Disconnected => None,
    }
}

fn entry_lines(
    server: &McpServerView,
    selected: bool,
    row_budget: usize,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let marker = if selected { "> " } else { "  " };
    let (glyph, state_style, label) = state_look(server.status, palette);
    let name = sanitize_terminal_text(&server.name);
    let plain = format!(
        "{marker}{glyph} {name} \u{b7} {} \u{b7} {label}",
        server.transport
    );
    let title = if UnicodeWidthStr::width(plain.as_str()) <= row_budget {
        Line::from(vec![
            Span::styled(format!("{marker}{glyph} "), state_style),
            Span::styled(name, focus_text(selected, palette)),
            Span::styled(
                format!(" \u{b7} {} \u{b7} ", server.transport),
                palette.muted,
            ),
            Span::styled(label.to_owned(), state_style),
        ])
    } else {
        Line::from(Span::styled(
            truncate_cells(&plain, row_budget),
            focus_text(selected, palette),
        ))
    };
    let mut lines = vec![title];
    let indent = "    ";
    let body_width = row_budget.saturating_sub(indent.len()).max(1);
    let metrics = metrics(server);
    if !metrics.is_empty() {
        for row in wrap_words(&metrics, body_width) {
            lines.push(Line::from(Span::styled(
                format!("{indent}{row}"),
                palette.muted,
            )));
        }
    }
    for row in wrap_words(&sanitize_terminal_text(&server.target), body_width) {
        lines.push(Line::from(Span::styled(
            format!("{indent}{row}"),
            palette.muted,
        )));
    }
    if let Some(help) = help(server) {
        let style = if matches!(server.status, McpStatusView::Disabled) {
            palette.muted
        } else {
            state_style
        };
        for row in wrap_words(&sanitize_terminal_text(&help), body_width) {
            lines.push(Line::from(Span::styled(format!("{indent}{row}"), style)));
        }
    }
    lines
}

/// Footer: position then only the actions the selected server accepts.
fn footer(overlay: &McpOverlay, servers: &[McpServerView], palette: &Palette) -> (String, Style) {
    match &overlay.confirm {
        Some(McpConfirm::Remove(name)) => (
            format!(
                "remover {}? y/Enter confirma \u{b7} n/Esc cancela",
                sanitize_terminal_text(name)
            ),
            palette.warning,
        ),
        Some(McpConfirm::Logout(name)) => (
            format!(
                "sair de {}? apaga as credenciais salvas \u{b7} y/Enter confirma \u{b7} n/Esc cancela",
                sanitize_terminal_text(name)
            ),
            palette.warning,
        ),
        None => {
            let position = if servers.is_empty() {
                "0/0".to_owned()
            } else {
                format!("{}/{}", overlay.selected.saturating_add(1), servers.len())
            };
            let selected = servers.get(overlay.selected);
            let mut hints = vec!["Enter testar", "r reconectar", "x desconectar"];
            if let Some(server) = selected {
                hints.push(if server.status == McpStatusView::Disabled {
                    "a ativar"
                } else {
                    "a desativar"
                });
                if server.transport == "http" {
                    hints.push("l entrar");
                    hints.push("o sair");
                }
                if server.status == McpStatusView::Untrusted {
                    hints.push("t confiar");
                }
                hints.push("d remover");
            }
            hints.push("R recarregar");
            hints.push("Esc fechar");
            (
                format!("{position} \u{b7} {}", hints.join(" \u{b7} ")),
                palette.muted,
            )
        }
    }
}

/// Indexes of the entries drawn: whole entries from the first one that keeps
/// the selected entry fully visible, as many as the budget holds.
fn visible_entries(
    heights: &[usize],
    selected: usize,
    preferred_start: usize,
    budget: usize,
    only_selected: bool,
) -> Vec<usize> {
    if heights.is_empty() {
        return Vec::new();
    }
    let selected = selected.min(heights.len() - 1);
    if only_selected {
        return vec![selected];
    }
    let span = |start: usize| -> usize {
        heights[start..=selected].iter().sum::<usize>() + (selected - start)
    };
    let mut start = preferred_start.min(selected);
    while start < selected && span(start) > budget {
        start += 1;
    }
    let mut used = span(start);
    let mut indexes: Vec<usize> = (start..=selected).collect();
    for (index, height) in heights.iter().enumerate().skip(selected + 1) {
        if used + 1 + height > budget {
            break;
        }
        used += 1 + height;
        indexes.push(index);
    }
    indexes
}

fn notice_line(notice: &str, row_budget: usize, palette: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            " {}",
            truncate_cells(&sanitize_terminal_text(notice), row_budget)
        ),
        palette.secondary,
    ))
}

fn render_list(
    frame: &mut ratatui::Frame,
    overlay: &McpOverlay,
    servers: &[McpServerView],
    palette: &Palette,
) {
    let frame_area = frame.area();
    let width = modal_width(frame_area.width);
    let row_budget = width.saturating_sub(4) as usize;
    let (footer_text, footer_style) = footer(overlay, servers, palette);
    let footer_rows = wrap_words(&footer_text, row_budget.max(1));
    let notice = overlay.notice.as_deref().filter(|text| !text.is_empty());
    let max_inner = frame_area.height.saturating_sub(4) as usize;
    // Footer, the blank row above it and the notice row come off the list.
    let reserved = footer_rows.len().max(1) + 1 + usize::from(notice.is_some());
    let list_budget = max_inner.saturating_sub(reserved).max(1);
    let mut entries: Vec<Vec<Line<'static>>> = Vec::new();
    if servers.is_empty() {
        entries.push(vec![Line::from(Span::styled(
            " nenhum servidor configurado \u{2014} /mcp add <nome> <comando>",
            palette.muted,
        ))]);
    } else {
        for (index, server) in servers.iter().enumerate() {
            entries.push(entry_lines(
                server,
                index == overlay.selected,
                row_budget,
                palette,
            ));
        }
    }
    let heights: Vec<usize> = entries.iter().map(Vec::len).collect();
    let shown = visible_entries(
        &heights,
        overlay.selected,
        overlay.viewport_start,
        list_budget,
        frame_area.height < COMPACT_HEIGHT,
    );
    let mut lines: Vec<Line<'static>> = Vec::new();
    for index in shown {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(entries[index].clone());
    }
    lines.truncate(list_budget);
    lines.push(Line::default());
    if let Some(notice) = notice {
        lines.push(notice_line(notice, row_budget, palette));
    }
    for row in footer_rows {
        lines.push(Line::from(Span::styled(format!(" {row}"), footer_style)));
    }
    let height = u16::try_from(lines.len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(8, frame_area.height.saturating_sub(2).max(8));
    let area = centered(frame_area, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(" Servidores MCP ", palette)),
        area,
    );
}

/// The last `budget` cells of `text`, with a leading `…` when it was cut.
fn tail_cells(text: &str, budget: usize) -> String {
    if UnicodeWidthStr::width(text) <= budget {
        return text.to_owned();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 1_usize;
    for grapheme in text.graphemes(true).rev() {
        let cells = UnicodeWidthStr::width(grapheme);
        if used + cells > budget {
            break;
        }
        kept.push(grapheme);
        used += cells;
    }
    kept.reverse();
    format!("\u{2026}{}", kept.concat())
}

fn render_signin(
    frame: &mut ratatui::Frame,
    overlay: &McpOverlay,
    signin: &McpSignIn,
    palette: &Palette,
    cursor_focused: bool,
) {
    let frame_area = frame.area();
    let width = modal_width(frame_area.width);
    let row_budget = (width.saturating_sub(4) as usize).max(1);
    let intro = if signin.browser_opened {
        "Aprove o acesso no navegador que acabou de abrir. Se ele não abriu, abra esta URL:"
    } else {
        "Não foi possível abrir o navegador. Abra esta URL para aprovar o acesso:"
    };
    let intro_rows = wrap_words(intro, row_budget);
    let paste_rows = wrap_words(
        "Navegador em outra máquina? Cole a URL para onde ele redirecionou:",
        row_budget,
    );
    let footer_rows = wrap_words(
        "Enter enviar \u{b7} Ctrl+Y copiar URL \u{b7} Esc cancelar",
        row_budget,
    );
    let notice = overlay.notice.as_deref().filter(|text| !text.is_empty());
    // intro + url + blank + paste prompt + field + blank + notice + footer
    let fixed = intro_rows.len()
        + 1
        + paste_rows.len()
        + 1
        + 1
        + usize::from(notice.is_some())
        + footer_rows.len();
    let max_inner = frame_area.height.saturating_sub(4) as usize;
    let url_budget = max_inner.saturating_sub(fixed).max(2);
    let mut url_rows = wrap_words(&sanitize_terminal_text(signin.url.expose()), row_budget);
    if url_rows.len() > url_budget {
        url_rows.truncate(url_budget);
        if let Some(last) = url_rows.last_mut() {
            *last = truncate_cells(&format!("{last}\u{2026}"), row_budget);
        }
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    for row in intro_rows {
        lines.push(Line::from(Span::styled(format!(" {row}"), palette.muted)));
    }
    for row in url_rows {
        lines.push(Line::from(Span::styled(format!(" {row}"), palette.link)));
    }
    lines.push(Line::default());
    for row in paste_rows {
        lines.push(Line::from(Span::styled(format!(" {row}"), palette.muted)));
    }
    let input_row = lines.len();
    let input_budget = row_budget.saturating_sub(2).max(1);
    let (field, field_style) = if signin.input.is_empty() {
        ("cole a URL aqui".to_owned(), palette.muted)
    } else {
        (
            tail_cells(&sanitize_terminal_text(signin.input.expose()), input_budget),
            palette.text,
        )
    };
    let field_cells = if signin.input.is_empty() {
        0
    } else {
        UnicodeWidthStr::width(field.as_str())
    };
    lines.push(Line::from(vec![
        Span::styled(" > ", palette.muted),
        Span::styled(field, field_style),
    ]));
    lines.push(Line::default());
    if let Some(notice) = notice {
        lines.push(notice_line(notice, row_budget, palette));
    }
    for row in footer_rows {
        lines.push(Line::from(Span::styled(format!(" {row}"), palette.muted)));
    }
    let height = u16::try_from(lines.len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(8, frame_area.height.saturating_sub(2).max(8));
    let area = centered(frame_area, width, height);
    let inner = ratatui::layout::Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let title = format!(" Entrar em {} ", sanitize_terminal_text(&signin.name));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(modal_block(&title, palette)),
        area,
    );
    if cursor_focused {
        set_cursor_in_rect(frame, inner, 3 + field_cells, input_row);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::app::{AppState, McpConfirm, McpOverlay};
    use crate::render::WrapCache;
    use crate::runtime::render_frame;
    use crate::theme::{Capabilities, ColorDepth};

    fn caps(color_depth: ColorDepth) -> Capabilities {
        Capabilities {
            color_depth,
            mouse: false,
            clipboard: false,
            images: false,
            reduced_motion: false,
        }
    }

    fn draw(
        state: &AppState,
        width: u16,
        height: u16,
        depth: ColorDepth,
    ) -> (Vec<String>, (u16, u16)) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render_frame(frame, state, caps(depth), &mut WrapCache::default()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect();
        let cursor = terminal.get_cursor_position().expect("cursor");
        (rows, (cursor.x, cursor.y))
    }

    fn frame(state: &AppState, width: u16, height: u16) -> String {
        draw(state, width, height, ColorDepth::TrueColor)
            .0
            .join("\n")
    }

    fn view(name: &str, transport: &'static str, status: McpStatusView) -> McpServerView {
        McpServerView {
            name: name.into(),
            transport,
            target: if transport == "http" {
                "https://mcp.example.com/mcp".into()
            } else {
                "npx -y some-server".into()
            },
            status,
            exposure: "gateway",
            ..McpServerView::default()
        }
    }

    fn state_with(servers: Vec<McpServerView>) -> AppState {
        let mut state = AppState::new();
        state.authenticated = true;
        state.mcp_overlay = Some(McpOverlay::default());
        state.mcp_servers = servers;
        state
    }

    #[test]
    fn every_state_has_a_glyph_and_a_textual_label() {
        let cases = [
            (McpStatusView::Ready, "\u{25CF}", "pronto"),
            (McpStatusView::Connecting, "\u{25CC}", "conectando"),
            (McpStatusView::Disconnected, "\u{25CC}", "desconectado"),
            (McpStatusView::Disabled, "\u{25CB}", "desativado"),
            (McpStatusView::Failed, "\u{2715}", "falhou"),
            (McpStatusView::Untrusted, "!", "projeto sem confiança"),
            (McpStatusView::NeedsAuth, "!", "requer login"),
        ];
        for (status, glyph, label) in cases {
            let state = state_with(vec![view("srv", "http", status)]);
            let text = frame(&state, 100, 30);
            let title = text
                .lines()
                .find(|line| line.contains("srv"))
                .unwrap_or_else(|| panic!("{status:?}: no title row in\n{text}"));
            assert!(title.contains(glyph), "{status:?}: {title}");
            assert!(title.contains(label), "{status:?}: {title}");
            assert!(title.contains("http"), "{status:?}: {title}");
        }
    }

    #[test]
    fn each_server_shows_counts_exposure_and_target() {
        let mut server = view("docs", "http", McpStatusView::Ready);
        server.tools = Some(12);
        server.resources = Some(3);
        server.resource_templates = Some(1);
        server.exposure = "direct";
        let mut single = view("one", "stdio", McpStatusView::Ready);
        single.tools = Some(1);
        single.resources = Some(1);
        single.exposure = "hidden";
        let state = state_with(vec![server, single]);
        let text = frame(&state, 100, 30);
        assert!(
            text.contains(
                "12 ferramentas \u{b7} 3 recursos \u{b7} 1 modelo \u{b7} exposição direct"
            ),
            "{text}"
        );
        assert!(
            text.contains("1 ferramenta \u{b7} 1 recurso \u{b7} exposição hidden"),
            "{text}"
        );
        assert!(text.contains("https://mcp.example.com/mcp"), "{text}");
        assert!(text.contains("npx -y some-server"), "{text}");
    }

    #[test]
    fn unknown_counts_are_left_out_instead_of_shown_as_zero() {
        let state = state_with(vec![view("quiet", "stdio", McpStatusView::Disconnected)]);
        let text = frame(&state, 100, 30);
        assert!(text.contains("exposição gateway"), "{text}");
        assert!(!text.contains("ferramenta"), "{text}");
        assert!(!text.contains("recurso"), "{text}");
    }

    #[test]
    fn state_help_names_the_key_and_the_typed_alternative() {
        let state = state_with(vec![
            view("proj", "stdio", McpStatusView::Untrusted),
            view("off", "stdio", McpStatusView::Disabled),
            McpServerView {
                error: Some("sign-in required".into()),
                ..view("web", "http", McpStatusView::NeedsAuth)
            },
        ]);
        let text = frame(&state, 100, 40);
        assert!(
            text.contains("t confia no projeto (ou /mcp trust)"),
            "{text}"
        );
        assert!(text.contains("desativado no slim.toml"), "{text}");
        assert!(text.contains("a ativa"), "{text}");
        assert!(text.contains("sign-in required"), "{text}");
        assert!(text.contains("l entra (ou /mcp login web)"), "{text}");
    }

    #[test]
    fn the_footer_only_lists_actions_the_selected_server_accepts() {
        let footer_of = |server: McpServerView| -> String {
            let state = state_with(vec![server]);
            frame(&state, 120, 30)
        };
        let http = footer_of(view("web", "http", McpStatusView::Ready));
        assert!(http.contains("Enter testar"), "{http}");
        assert!(
            http.contains("l entrar") && http.contains("o sair"),
            "{http}"
        );
        assert!(http.contains("a desativar"), "{http}");
        assert!(!http.contains("t confiar"), "{http}");

        let stdio = footer_of(view("fs", "stdio", McpStatusView::Disabled));
        assert!(stdio.contains("a ativar"), "{stdio}");
        assert!(
            !stdio.contains("l entrar") && !stdio.contains("o sair"),
            "{stdio}"
        );

        let untrusted = footer_of(view("proj", "stdio", McpStatusView::Untrusted));
        assert!(untrusted.contains("t confiar"), "{untrusted}");
        assert!(untrusted.contains("Esc fechar"), "{untrusted}");
    }

    #[test]
    fn confirmation_footers_name_the_server_and_the_effect() {
        let mut state = state_with(vec![view("web", "http", McpStatusView::Ready)]);
        state.mcp_overlay.as_mut().unwrap().confirm = Some(McpConfirm::Logout("web".into()));
        let text = frame(&state, 100, 30);
        assert!(
            text.contains("sair de web? apaga as credenciais salvas"),
            "{text}"
        );
        assert!(text.contains("y/Enter confirma"), "{text}");
        state.mcp_overlay.as_mut().unwrap().confirm = Some(McpConfirm::Remove("web".into()));
        let text = frame(&state, 100, 30);
        assert!(text.contains("remover web? y/Enter confirma"), "{text}");
    }

    #[test]
    fn the_notice_row_shows_the_hosts_latest_message() {
        let mut state = state_with(vec![view("web", "http", McpStatusView::Ready)]);
        state.mcp_overlay.as_mut().unwrap().notice =
            Some("mcp web: ativado (gravado no slim.toml do projeto)".into());
        let text = frame(&state, 100, 30);
        assert!(
            text.contains("mcp web: ativado (gravado no slim.toml do projeto)"),
            "{text}"
        );
    }

    #[test]
    fn hostile_text_cannot_inject_escape_sequences_into_the_list() {
        let mut server = view("web\u{1b}[31m", "http", McpStatusView::Failed);
        server.target = "https://x/\u{1b}]0;title\u{7}".into();
        server.error = Some("boom\u{1b}[2J".into());
        let state = state_with(vec![server]);
        let text = frame(&state, 100, 30);
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }

    #[test]
    fn narrow_and_short_terminals_keep_the_selected_server_and_the_footer() {
        let servers: Vec<McpServerView> = (0..8)
            .map(|index| {
                view(
                    &format!("server-number-{index}"),
                    "http",
                    McpStatusView::Ready,
                )
            })
            .collect();
        let mut state = state_with(servers);
        state.mcp_overlay.as_mut().unwrap().selected = 5;
        for (width, height) in [(40_u16, 24_u16), (40, 10), (60, 14), (120, 11)] {
            let (rows, _) = draw(&state, width, height, ColorDepth::TrueColor);
            let text = rows.join("\n");
            assert!(text.contains("server-number-5"), "{width}x{height}\n{text}");
            assert!(text.contains("Esc fechar"), "{width}x{height}\n{text}");
            assert!(
                rows.iter()
                    .all(|row| row.chars().count() <= usize::from(width)),
                "{width}x{height}"
            );
        }
        // Below the compact height only the selected server is listed.
        let (rows, _) = draw(&state, 80, 11, ColorDepth::TrueColor);
        let text = rows.join("\n");
        assert!(!text.contains("server-number-4"), "{text}");
        assert!(!text.contains("server-number-6"), "{text}");
    }

    #[test]
    fn no_color_keeps_the_same_rows_and_the_labels() {
        let state = state_with(vec![
            view("a", "http", McpStatusView::NeedsAuth),
            view("b", "stdio", McpStatusView::Failed),
        ]);
        let colored = draw(&state, 100, 30, ColorDepth::TrueColor).0;
        let plain = draw(&state, 100, 30, ColorDepth::None).0;
        assert_eq!(colored, plain);
        assert!(plain.join("\n").contains("requer login"));
    }

    fn panel_state(url: &str, input: &str) -> AppState {
        let mut state = AppState::new();
        state.authenticated = true;
        state.mcp_overlay = Some(McpOverlay {
            signin: Some(McpSignIn {
                name: "notion".into(),
                url: url.to_owned().into(),
                browser_opened: true,
                input: input.to_owned().into(),
            }),
            signin_only: true,
            ..McpOverlay::default()
        });
        state
    }

    /// Text of the panel rows without the border and padding, concatenated.
    fn squeezed(rows: &[String]) -> String {
        rows.iter()
            .map(|row| {
                row.chars()
                    .filter(|character| !matches!(character, '\u{2502}' | ' '))
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_panel_shows_the_whole_url_wrapped_by_cells() {
        let url = format!(
            "https://auth.example.com/authorize?response_type=code&client_id=abc&state={}",
            "s".repeat(400)
        );
        let state = panel_state(&url, "");
        let (rows, _) = draw(&state, 80, 40, ColorDepth::TrueColor);
        let text = rows.join("\n");
        assert!(text.contains("Entrar em notion"), "{text}");
        assert!(text.contains("navegador que acabou de abrir"), "{text}");
        assert!(
            squeezed(&rows).contains(&url),
            "the URL must appear whole, rows concatenated:\n{text}"
        );
        assert!(!text.contains('\u{2026}'), "nothing was cut:\n{text}");
        assert!(text.contains("Ctrl+Y copiar URL"), "{text}");
        assert!(text.contains("Esc cancelar"), "{text}");
        assert!(text.contains("Navegador em outra máquina?"), "{text}");
        assert!(
            rows.iter().all(|row| row.chars().count() <= 80),
            "no row overflows"
        );
    }

    #[test]
    fn a_url_that_cannot_fit_ends_in_an_ellipsis_and_ctrl_y_still_has_it_all() {
        let url = format!("https://auth.example.com/authorize?{}", "x".repeat(2_000));
        let state = panel_state(&url, "");
        let (rows, _) = draw(&state, 60, 18, ColorDepth::TrueColor);
        let text = rows.join("\n");
        assert!(text.contains('\u{2026}'), "{text}");
        assert!(text.contains("Ctrl+Y copiar URL"), "{text}");
        assert!(text.contains("Esc cancelar"), "{text}");
        assert!(rows.len() == 18);
    }

    #[test]
    fn the_panel_says_when_the_browser_could_not_be_opened() {
        let mut state = panel_state("https://auth.example.com/a", "");
        state
            .mcp_overlay
            .as_mut()
            .unwrap()
            .signin
            .as_mut()
            .unwrap()
            .browser_opened = false;
        let text = frame(&state, 100, 30);
        assert!(
            text.contains("Não foi possível abrir o navegador"),
            "{text}"
        );
    }

    #[test]
    fn the_input_field_shows_a_placeholder_then_the_tail_with_the_caret_after_it() {
        let state = panel_state("https://auth.example.com/a", "");
        let (rows, _) = draw(&state, 80, 30, ColorDepth::TrueColor);
        assert!(rows.join("\n").contains("cole a URL aqui"));

        let pasted = format!(
            "http://127.0.0.1:5000/callback?code={}&state=END",
            "c".repeat(200)
        );
        let state = panel_state("https://auth.example.com/a", &pasted);
        let (rows, cursor) = draw(&state, 80, 30, ColorDepth::TrueColor);
        let input_row = rows
            .iter()
            .position(|row| row.contains(" > "))
            .expect("input row");
        assert!(rows[input_row].contains("state=END"), "{}", rows[input_row]);
        assert!(rows[input_row].contains('\u{2026}'), "cut at the left");
        assert_eq!(usize::from(cursor.1), input_row, "caret on the field row");
        let after_text = rows[input_row]
            .trim_end_matches([' ', '\u{2502}'])
            .chars()
            .count();
        assert_eq!(
            usize::from(cursor.0),
            after_text,
            "caret right after the text"
        );
    }

    #[test]
    fn the_panel_replaces_the_list_and_shows_the_notice() {
        let mut state = panel_state("https://auth.example.com/a", "");
        state.mcp_servers = vec![view("notion", "http", McpStatusView::NeedsAuth)];
        state.mcp_overlay.as_mut().unwrap().notice =
            Some("mcp notion: não deu para ler a URL".into());
        let text = frame(&state, 100, 30);
        assert!(!text.contains("Servidores MCP"), "{text}");
        assert!(
            text.contains("mcp notion: não deu para ler a URL"),
            "{text}"
        );
    }

    #[test]
    fn the_authorization_url_is_sanitized_for_display_only() {
        let state = panel_state("https://auth.example.com/a\u{1b}[31m?x=1", "");
        let text = frame(&state, 100, 30);
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }

    #[test]
    fn window_keeps_the_selected_entry_whole() {
        // Heights 3, 4, 3: a budget of 8 cannot hold entries 0..=2.
        let shown = visible_entries(&[3, 4, 3], 2, 0, 8, false);
        assert_eq!(shown, vec![1, 2]);
        // A selected entry taller than the budget is still returned (cut by
        // the caller).
        assert_eq!(visible_entries(&[3, 9], 1, 0, 5, false), vec![1]);
        assert_eq!(visible_entries(&[3, 4, 3], 0, 0, 100, true), vec![0]);
    }

    #[test]
    fn tail_keeps_the_end_of_long_text() {
        assert_eq!(tail_cells("abcdef", 10), "abcdef");
        assert_eq!(tail_cells("abcdef", 4), "\u{2026}def");
    }
}
