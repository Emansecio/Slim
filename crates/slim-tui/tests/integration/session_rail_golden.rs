//! The workspace identity in the top rail (DESIGN-SLIM-TUI §1.2, revision of
//! 05/10/2026): the home directory reads `~`, the project folder is the one
//! thing in the path set in the text tone, and a short rail drops the parent
//! first. The rendering is checked end to end against the real home directory
//! of the machine running the tests; the arithmetic of widths is pinned by the
//! unit tests of `view_model`.

use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use slim_tui::api::{LoginProvider, SessionId, UiEvent};
use slim_tui::app::AppState;
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

const TEXT: Color = Color::Rgb(0xE8, 0xE5, 0xDB);
const MUTED: Color = Color::Rgb(0x99, 0x97, 0x8E);
const SECONDARY: Color = Color::Rgb(0xBC, 0xB9, 0xAF);

struct Row {
    text: String,
    foreground: Vec<Color>,
    modifiers: Vec<Modifier>,
}

impl Row {
    /// The rail's words, without the chrome inset.
    fn line(&self) -> &str {
        self.text.trim()
    }

    fn column(&self, needle: &str) -> usize {
        let byte = self
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing in {:?}", self.text));
        unicode_width::UnicodeWidthStr::width(&self.text[..byte])
    }

    fn fg(&self, needle: &str) -> Color {
        self.foreground[self.column(needle)]
    }

    fn modifier(&self, needle: &str) -> Modifier {
        self.modifiers[self.column(needle)]
    }
}

fn home() -> Option<String> {
    ["USERPROFILE", "HOME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|home| home.trim_end_matches(['\u{5c}', '/']).to_owned())
        .find(|home| !home.is_empty())
}

fn state(cwd: &str, title: Option<&str>) -> AppState {
    let mut state = AppState::new();
    state.apply_event(UiEvent::SessionSnapshot {
        session_id: SessionId("rail".into()),
        cwd: cwd.into(),
        skill_names: Vec::new(),
    });
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::Anthropic),
        authenticated: true,
    });
    state.apply_event(UiEvent::UserMessageAdded { text: "oi".into() });
    state.apply_event(UiEvent::SessionTitleChanged {
        title: title.map(str::to_owned),
    });
    state
}

fn rail(state: &AppState, width: u16, depth: ColorDepth) -> Row {
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).expect("terminal");
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                state,
                Capabilities {
                    color_depth: depth,
                    mouse: false,
                    clipboard: false,
                    images: false,
                    reduced_motion: true,
                },
                &mut WrapCache::default(),
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut row = Row {
        text: String::new(),
        foreground: Vec::new(),
        modifiers: Vec::new(),
    };
    let mut skip = 0;
    for x in 0..buffer.area.width {
        let cell = &buffer[(x, 0)];
        if skip == 0 {
            row.text.push_str(cell.symbol());
            skip = unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1);
        }
        skip -= 1;
        row.foreground.push(cell.fg);
        row.modifiers.push(cell.modifier);
    }
    row.text = row.text.trim_end().to_owned();
    row
}

#[test]
fn the_home_reads_as_a_tilde_and_the_project_folder_is_the_one_in_text_tone() {
    let Some(home) = home() else { return };
    let cwd = format!(r"{home}\Projects\Slim");
    let row = rail(&state(&cwd, None), 100, ColorDepth::TrueColor);
    assert_eq!(row.line(), r"SLIM · ~\Projects\Slim");
    // The brand a step above the quiet path, the folder in the text tone.
    assert_eq!(row.fg("SLIM"), SECONDARY);
    assert_eq!(row.fg("~"), MUTED);
    assert_eq!(row.fg("Projects"), MUTED);
    assert_eq!(row.fg(r"\Slim"), MUTED, "the separator stays quiet");
    assert_eq!(row.fg("Slim"), TEXT);
    // Lower-case spelling of the same home is the same directory.
    let lower = format!(r"{}\Projects\Slim", home.to_lowercase());
    assert_eq!(
        rail(&state(&lower, None), 100, ColorDepth::TrueColor).line(),
        r"SLIM · ~\Projects\Slim"
    );
    // The session title still leads and keeps the text tone too.
    let row = rail(
        &state(&cwd, Some("Corrigir CRLF")),
        100,
        ColorDepth::TrueColor,
    );
    assert_eq!(row.line(), r"SLIM · Corrigir CRLF · ~\Projects\Slim");
    assert_eq!(row.fg("Corrigir"), TEXT);
    assert_eq!(row.fg("Projects"), MUTED);
    assert_eq!(row.fg("Slim"), TEXT);
}

#[test]
fn a_path_outside_the_home_is_shown_as_it_is_with_the_same_emphasis() {
    let row = rail(&state(r"D:\Work\Slim", None), 100, ColorDepth::TrueColor);
    assert_eq!(row.line(), r"SLIM · D:\Work\Slim");
    assert_eq!(row.fg("Work"), MUTED);
    assert_eq!(row.fg(r"\Slim"), MUTED);
    assert_eq!(row.fg("Slim"), TEXT);
}

#[test]
fn a_short_rail_drops_the_parent_and_keeps_the_folder() {
    // The rail shows from 80 columns; a title takes the room it needs and the
    // path loses whole parents, never its folder.
    let deep = r"D:\Work\Group\Team\Projects\Area\Another\Slim";
    let row = rail(
        &state(deep, Some("Uma conversa com um título bem comprido")),
        80,
        ColorDepth::TrueColor,
    );
    assert!(row.line().contains(" · …\\"), "{}", row.text);
    assert!(row.line().ends_with("Slim"), "{}", row.text);
    assert!(unicode_width::UnicodeWidthStr::width(row.text.as_str()) <= 80);
    assert_eq!(row.fg(r"Slim"), TEXT);
    assert_eq!(row.fg("…"), MUTED);
}

#[test]
fn without_color_the_folder_is_told_apart_by_weight_alone() {
    let Some(home) = home() else { return };
    let cwd = format!(r"{home}\Projects\Slim");
    let row = rail(&state(&cwd, Some("Corrigir CRLF")), 100, ColorDepth::None);
    assert_eq!(row.line(), r"SLIM · Corrigir CRLF · ~\Projects\Slim");
    // No tone at all: every cell keeps the terminal's own color...
    assert!(row.foreground.iter().all(|fg| *fg == Color::Reset));
    // ...and only the project folder is bold.
    assert!(row.modifier("Slim").contains(Modifier::BOLD));
    assert!(row.modifier(r"\Slim").is_empty());
    for quiet in ["SLIM", "Corrigir", "~", "Projects"] {
        assert!(!row.modifier(quiet).contains(Modifier::BOLD), "{quiet}");
    }
    // With 16 colors the tones still differ, so no weight is added.
    let row = rail(&state(&cwd, None), 100, ColorDepth::Ansi16);
    assert_ne!(row.fg("Slim"), row.fg("Projects"));
    assert!(!row.modifier("Slim").contains(Modifier::BOLD));
}
