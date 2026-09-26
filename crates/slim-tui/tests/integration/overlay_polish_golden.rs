//! Modal backdrop, palette headings and the composer-attached slash list.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action};
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
        .find(|row| row.contains("sessão"))
        .expect("session heading");
    assert!(heading.contains("sessão ───"), "{heading}");
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
