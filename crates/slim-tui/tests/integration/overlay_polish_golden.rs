//! Modal backdrop, palette headings and the composer-attached slash list.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::reducer::{palette_matches, reduce, slash_matches, Action};
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

fn draw(state: &AppState) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
    let mut cache = WrapCache::default();
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                state,
                Capabilities {
                    color_depth: ColorDepth::TrueColor,
                    mouse: false,
                    clipboard: false,
                    images: false,
                    reduced_motion: true,
                },
                &mut cache,
            )
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect()
        })
        .collect()
}

fn find(rows: &[String], needle: &str) -> (u16, u16) {
    rows.iter()
        .enumerate()
        .find_map(|(y, row)| {
            row.find(needle)
                .map(|byte| (row[..byte].chars().count() as u16, y as u16))
        })
        .unwrap_or_else(|| panic!("{needle} not rendered\n{}", rows.join("\n")))
}

fn key(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) {
    reduce(state, Action::Key(KeyEvent::new(code, modifiers)));
}

fn conversation() -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::UserMessageAdded {
        text: "pergunta".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "resposta visível".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state
}

#[test]
fn centered_modal_recedes_the_transcript_behind_it() {
    let mut state = conversation();
    let before = draw(&state);
    let (x, y) = find(&rows(&before), "resposta");
    assert_ne!(before[(x, y)].fg, Color::Rgb(0x4A, 0x4A, 0x4A));

    key(&mut state, KeyCode::Char('p'), KeyModifiers::CONTROL);
    let after = draw(&state);
    let text = rows(&after);
    let (x, y) = find(&text, "resposta");
    assert_eq!(after[(x, y)].fg, Color::Rgb(0x4A, 0x4A, 0x4A), "backdrop");
    assert!(!after[(x, y)].modifier.contains(Modifier::BOLD));
    // The modal itself keeps its own styling.
    let (x, y) = find(&text, "Comandos");
    assert_ne!(after[(x, y)].fg, Color::Rgb(0x4A, 0x4A, 0x4A));
    // Group headings carry a rule to the modal edge.
    let heading = text
        .iter()
        .find(|row| row.contains("conversa"))
        .expect("conversation heading");
    assert!(heading.contains("conversa ───"), "{heading}");
}

#[test]
fn slash_list_rests_on_the_composer_at_its_width() {
    let mut state = conversation();
    key(&mut state, KeyCode::Char('/'), KeyModifiers::NONE);
    key(&mut state, KeyCode::Char('m'), KeyModifiers::NONE);
    let buffer = draw(&state);
    let text = rows(&buffer);

    // The composer is the last `│> /m` row; the selected list row matches too.
    let prompt_y = text
        .iter()
        .rposition(|row| row.contains("│> /m"))
        .expect("composer row") as u16;
    let composer_top = prompt_y - 1;
    let (left, _) = find(&text[composer_top as usize..], "╭");
    // The hint row sits directly on the composer's top border: no bottom
    // border of its own between them.
    let (_, hint_y) = find(&text, "Tab completar");
    assert_eq!(hint_y + 1, composer_top, "{}", text.join("\n"));
    // Same left edge and width as the composer.
    let list_top = text[..hint_y as usize]
        .iter()
        .rposition(|row| row.contains('╭'))
        .expect("list top border") as u16;
    let list_row = &text[list_top as usize];
    assert_eq!(list_row.chars().position(|c| c == '╭'), Some(left as usize));
    let composer_row = &text[composer_top as usize];
    assert_eq!(
        list_row.chars().position(|c| c == '╮'),
        composer_row.chars().position(|c| c == '╮'),
        "{}",
        text.join("\n")
    );
    // Commands carry their description.
    let model = text
        .iter()
        .find(|row| row.contains("/model ") && !row.contains("--default"))
        .expect("model row");
    assert!(model.contains("modelo desta sessão"), "{model}");
}

#[test]
fn mention_list_rests_on_the_composer_with_name_first_and_muted_directory() {
    let mut state = conversation();
    key(&mut state, KeyCode::Char('@'), KeyModifiers::NONE);
    key(&mut state, KeyCode::Char('l'), KeyModifiers::NONE);
    // Loading placeholder before the host answers.
    let loading = rows(&draw(&state));
    assert!(
        loading.iter().any(|row| row.contains("Buscando arquivos")),
        "{}",
        loading.join("\n")
    );

    state.apply_event(UiEvent::WorkspaceFiles {
        request_id: 1,
        paths: vec![
            "README.md".into(),
            "crates/slim-tui/src/lib.rs".into(),
            "crates/slim-core/src/lib.rs".into(),
        ],
        truncated: false,
    });
    let buffer = draw(&state);
    let text = rows(&buffer);

    let prompt_y = text
        .iter()
        .rposition(|row| row.contains("│> @l"))
        .expect("composer row") as u16;
    let composer_top = prompt_y - 1;
    let (_, hint_y) = find(&text, "Tab/Enter completar");
    assert_eq!(hint_y + 1, composer_top, "{}", text.join("\n"));

    // File name first, directory after it; selection marker on the first hit.
    let selected = text
        .iter()
        .find(|row| row.contains("> lib.rs"))
        .unwrap_or_else(|| panic!("selected row missing\n{}", text.join("\n")));
    assert!(selected.contains("crates/slim-"), "{selected}");
    let (x, y) = find(&text, "lib.rs");
    let directory_x = find(&text[y as usize..=y as usize], "crates/").0;
    assert!(directory_x > x, "directory must follow the name");
    let directory_style = buffer[(directory_x, y)].style();
    let name_style = buffer[(x, y)].style();
    assert_ne!(directory_style.fg, name_style.fg, "directory is muted");
}

fn listed_sessions(state: &mut AppState) {
    use slim_tui::api::SessionListItem;
    key(state, KeyCode::Char('/'), KeyModifiers::NONE);
    for character in "resume".chars() {
        key(state, KeyCode::Char(character), KeyModifiers::NONE);
    }
    // The slash popup completes and executes on Enter.
    key(state, KeyCode::Enter, KeyModifiers::NONE);
    let item = |id: &str, title: Option<&str>, prompt: &str, minutes_ago: u64| SessionListItem {
        id: id.into(),
        title: title.map(str::to_owned),
        first_prompt: prompt.into(),
        updated_ms: 400_000_000 - minutes_ago * 60_000,
        bytes: 4_300,
        in_use: false,
        current: false,
    };
    let mut current = item("tui-current", None, "conversa aberta agora", 1);
    current.current = true;
    let mut busy = item("tui-busy", Some("Migração"), "trocar o banco", 30);
    busy.in_use = true;
    state.apply_event(UiEvent::SessionsListed {
        request_id: 1,
        now_ms: 400_000_000,
        items: vec![
            current,
            busy,
            item(
                "tui-login",
                Some("Refatorar login"),
                "corrigir o bug do token",
                90,
            ),
            item("tui-docs", None, "escrever a documentação da API", 3_000),
        ],
        error: None,
    });
}

#[test]
fn resume_picker_lists_sessions_with_titles_ages_and_states() {
    let mut state = conversation();
    listed_sessions(&mut state);
    let buffer = draw(&state);
    let text = rows(&buffer);
    let screen = text.join("\n");

    assert!(screen.contains("Retomar sessão"), "{screen}");
    // The filter sits in the title; empty, it invites typing.
    assert!(
        screen.contains("Retomar sessão · digite para filtrar"),
        "{screen}"
    );
    // Title leads, the first prompt follows it.
    let login = text
        .iter()
        .find(|row| row.contains("Refatorar login"))
        .unwrap_or_else(|| panic!("titled row missing\n{screen}"));
    assert!(login.contains("corrigir o bug do token"), "{login}");
    assert!(
        login.contains("há 1 h") && login.contains("4,2 KB"),
        "{login}"
    );
    // Without a title the first prompt is the label.
    assert!(
        screen.contains("escrever a documentação da API"),
        "{screen}"
    );
    // State tags; the open and the locked sessions are not the initial focus.
    let open = text
        .iter()
        .find(|row| row.contains("conversa aberta"))
        .unwrap();
    assert!(open.contains("atual"), "{open}");
    let busy = text.iter().find(|row| row.contains("Migração")).unwrap();
    assert!(busy.contains("em uso"), "{busy}");
    let focused = text
        .iter()
        .find(|row| row.contains("> "))
        .expect("focus marker");
    assert!(focused.contains("Refatorar login"), "{focused}");
    // Detail line names the focused session; footer counts rows.
    assert!(screen.contains("tui-login"), "{screen}");
    assert!(
        screen.contains("3/4 · ↑↓ navegar · Enter retomar · Esc fechar"),
        "{screen}"
    );
}

#[test]
fn resume_picker_filters_and_shows_the_filtered_count() {
    let mut state = conversation();
    listed_sessions(&mut state);
    for character in "api".chars() {
        key(&mut state, KeyCode::Char(character), KeyModifiers::NONE);
    }
    let screen = rows(&draw(&state)).join("\n");
    assert!(screen.contains("Retomar sessão · api"), "{screen}");
    assert!(
        screen.contains("escrever a documentação da API"),
        "{screen}"
    );
    assert!(!screen.contains("Refatorar login"), "{screen}");
    assert!(screen.contains("1/1 de 4"), "{screen}");
    for character in "zzz".chars() {
        key(&mut state, KeyCode::Char(character), KeyModifiers::NONE);
    }
    let screen = rows(&draw(&state)).join("\n");
    assert!(screen.contains("Nada corresponde ao filtro"), "{screen}");
}

#[test]
fn picker_shows_loading_and_errors_in_place() {
    let mut state = conversation();
    for character in "/resume".chars() {
        key(&mut state, KeyCode::Char(character), KeyModifiers::NONE);
    }
    key(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    let screen = rows(&draw(&state)).join("\n");
    assert!(screen.contains("Carregando"), "{screen}");
    state.apply_event(UiEvent::SessionsListed {
        request_id: 1,
        now_ms: 1,
        items: Vec::new(),
        error: Some("não foi possível ler as sessões".into()),
    });
    let screen = rows(&draw(&state)).join("\n");
    assert!(
        screen.contains("não foi possível ler as sessões"),
        "{screen}"
    );
}

#[test]
fn rewind_picker_lists_turns_newest_first_with_the_conversation_only_note() {
    use slim_tui::api::TurnListItem;
    let mut state = conversation();
    for character in "/rewind".chars() {
        key(&mut state, KeyCode::Char(character), KeyModifiers::NONE);
    }
    key(&mut state, KeyCode::Enter, KeyModifiers::NONE);
    state.apply_event(UiEvent::TurnsListed {
        request_id: 1,
        items: (0..3)
            .map(|index| TurnListItem {
                index,
                first_seq: index as u64 * 10 + 2,
                prompt: format!("pedido número {index}"),
            })
            .collect(),
        error: None,
    });
    let text = rows(&draw(&state));
    let screen = text.join("\n");
    assert!(screen.contains("Voltar a um turno"), "{screen}");
    let newest = text
        .iter()
        .position(|row| row.contains("#3 pedido número 2"))
        .expect("newest");
    let oldest = text
        .iter()
        .position(|row| row.contains("#1 pedido número 0"))
        .expect("oldest");
    assert!(newest < oldest, "{screen}");
    assert!(
        text[newest].contains("> "),
        "newest is focused: {}",
        text[newest]
    );
    assert!(text[newest].contains("volta 1 turno"), "{}", text[newest]);
    assert!(text[oldest].contains("volta 3 turnos"), "{}", text[oldest]);
    assert!(
        screen.contains("Só a conversa volta; arquivos alterados ficam como estão"),
        "{screen}"
    );
    assert!(
        screen.contains("1/3 · ↑↓ navegar · Enter voltar · Esc fechar"),
        "{screen}"
    );
}

#[test]
fn session_name_leads_the_rail_and_reads_as_text() {
    let mut state = conversation();
    state.apply_event(UiEvent::WorkspaceChanged {
        cwd: r"C:\Users\dev\projeto".into(),
        skill_names: Vec::new(),
    });
    state.apply_event(UiEvent::SessionTitleChanged {
        title: Some("Refatorar login".into()),
    });
    let buffer = draw(&state);
    let text = rows(&buffer);
    let (x, y) = find(&text, "Refatorar login");
    assert!(
        text[y as usize].contains("SLIM · Refatorar login · "),
        "{}",
        text[y as usize]
    );
    let name = buffer[(x, y)].style();
    let (rest_x, _) = find(&text[y as usize..=y as usize], "projeto");
    let rest = buffer[(rest_x, y)].style();
    assert_ne!(name.fg, rest.fg, "the name is brighter than the directory");
    // Clearing the name restores the plain rail.
    state.apply_event(UiEvent::SessionTitleChanged { title: None });
    let text = rows(&draw(&state));
    assert!(!text.iter().any(|row| row.contains("Refatorar login")));
}

#[test]
fn slash_popup_offers_rename_and_rewind_with_descriptions() {
    let mut state = conversation();
    key(&mut state, KeyCode::Char('/'), KeyModifiers::NONE);
    key(&mut state, KeyCode::Char('r'), KeyModifiers::NONE);
    key(&mut state, KeyCode::Char('e'), KeyModifiers::NONE);
    let screen = rows(&draw(&state)).join("\n");
    assert!(
        screen.contains("/rename") && screen.contains("nomear esta sessão"),
        "{screen}"
    );
    assert!(
        screen.contains("/rewind") && screen.contains("voltar a um turno"),
        "{screen}"
    );
}

#[test]
fn slash_popup_groups_commands_and_keeps_each_command_visible() {
    let mut state = conversation();
    key(&mut state, KeyCode::Char('/'), KeyModifiers::NONE);
    let text = rows(&draw(&state));
    let screen = text.join("\n");
    for heading in ["ajuda ─", "conversa ─", "execução ─"] {
        assert!(screen.contains(heading), "{screen}");
    }
    assert!(find(&text, "/compact").1 < find(&text, "/model").1);
    assert!(find(&text, "/model").1 < find(&text, "/queue").1);

    for command in slash_matches("") {
        let (text, _) = draw_sized(&state, WIDTH, 14);
        assert!(
            text.iter().any(|row| row.contains(&format!("> {command}"))),
            "{command} must stay visible despite the headings:\n{}",
            text.join("\n")
        );
        key(&mut state, KeyCode::Down, KeyModifiers::NONE);
    }
}

/// Frame at an explicit size and where the caret landed.
fn draw_sized(state: &AppState, width: u16, height: u16) -> (Vec<String>, Position) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    let mut cache = WrapCache::default();
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                state,
                Capabilities {
                    color_depth: ColorDepth::TrueColor,
                    mouse: false,
                    clipboard: false,
                    images: false,
                    reduced_motion: true,
                },
                &mut cache,
            )
        })
        .expect("draw");
    let cursor = terminal.get_cursor_position().expect("cursor");
    (rows(terminal.backend().buffer()), cursor)
}

#[test]
fn command_palette_is_a_bordered_modal_like_the_pickers() {
    let mut state = conversation();
    key(&mut state, KeyCode::Char('p'), KeyModifiers::CONTROL);
    let total = palette_matches("").len();
    let (text, _) = draw_sized(&state, WIDTH, HEIGHT);
    let screen = text.join("\n");

    // The title lives in the rounded border, not in a content row.
    assert!(
        text.iter()
            .any(|row| row.contains("╭") && row.contains(" Comandos ")),
        "{screen}"
    );
    assert!(
        text.iter()
            .any(|row| row.contains('╰') && row.contains('╯')),
        "{screen}"
    );
    // Same filter-in-title as the model and session pickers.
    assert!(
        screen.contains("Comandos · digite para filtrar"),
        "{screen}"
    );
    assert!(!screen.contains("Buscar:"), "{screen}");
    // Same `n/total · hints` footer, closing with `Esc fechar`.
    assert!(
        screen.contains(&format!(
            "1/{total} · ↑↓ navegar · Enter executar · Esc fechar"
        )),
        "{screen}"
    );

    // Filtering narrows the counter and the caret follows the typed text.
    for character in "mod".chars() {
        key(&mut state, KeyCode::Char(character), KeyModifiers::NONE);
    }
    let matching = palette_matches("mod").len();
    let (text, cursor) = draw_sized(&state, WIDTH, HEIGHT);
    let screen = text.join("\n");
    let (x, y) = find(&text, "Comandos · mod");
    assert_eq!(
        cursor,
        Position::new(x + "Comandos · mod".chars().count() as u16, y),
        "{screen}"
    );
    assert!(screen.contains(&format!("1/{matching} · ")), "{screen}");
}

#[test]
fn command_palette_keeps_the_focused_command_visible_past_its_capacity() {
    let mut state = AppState::new();
    key(&mut state, KeyCode::Char('p'), KeyModifiers::CONTROL);
    let commands = palette_matches("");
    // 14 rows tall leaves a 12 row modal: fewer rows than commands plus headings.
    for (index, command) in commands.iter().enumerate() {
        let (text, _) = draw_sized(&state, WIDTH, 14);
        let screen = text.join("\n");
        assert!(
            text.iter().any(|row| row.contains(&format!("> {command}"))),
            "{command} must stay visible:\n{screen}"
        );
        assert!(
            screen.contains(&format!("{}/{} · ", index + 1, commands.len())),
            "{screen}"
        );
        key(&mut state, KeyCode::Down, KeyModifiers::NONE);
    }
}
