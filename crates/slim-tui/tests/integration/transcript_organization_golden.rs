//! How the transcript is organized: the work of a finished turn folded under
//! one row, what small groups say, how an open group is laid out, a failed
//! run's reason, code against quotes, the prompt's marker, and the three
//! style-only motions (a settling tool row, a running command's last line, the
//! glow of an answer being written). DESIGN-SLIM-TUI §1.2, revision of
//! 05/10/2026.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use slim_core::{EventKind, SessionEvent};
use slim_tui::api::{
    InteractionRequestId, LoginProvider, SessionId, ToolBatchId, ToolCallId, TranscriptMessage,
    TranscriptRole, UiEvent,
};
use slim_tui::app::{AppState, FollowMode, FrameClock, ScrollAnchor};
use slim_tui::block::{Block, BlockKind, FoldState};
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::render::{HeightIndex, WrapCache};
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

const MUTED: Color = Color::Rgb(0x99, 0x97, 0x8E);
const SECONDARY: Color = Color::Rgb(0xBC, 0xB9, 0xAF);
const ERROR: Color = Color::Rgb(0xE8, 0x79, 0x73);
const TEXT: Color = Color::Rgb(0xE8, 0xE5, 0xDB);
const CODE_RAIL: Color = Color::Rgb(0x68, 0x66, 0x5A);
const CODE_BG: Color = Color::Rgb(0x14, 0x14, 0x14);

struct Rendered {
    rows: Vec<String>,
    foreground: Vec<Vec<Color>>,
    background: Vec<Vec<Color>>,
    modifiers: Vec<Vec<Modifier>>,
}

impl Rendered {
    fn text(&self) -> String {
        self.rows.join("\n")
    }

    /// Row and column (in cells) of the first occurrence of `needle`.
    fn find(&self, needle: &str) -> Option<(usize, usize)> {
        self.rows.iter().enumerate().find_map(|(y, row)| {
            row.find(needle)
                .map(|x| (y, unicode_width::UnicodeWidthStr::width(&row[..x])))
        })
    }

    fn row(&self, needle: &str) -> usize {
        self.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing\n{}", self.text()))
            .0
    }

    fn fg(&self, needle: &str) -> Color {
        let (y, x) = self.find(needle).unwrap_or_else(|| panic!("{needle:?}"));
        self.foreground[y][x]
    }

    fn bg(&self, needle: &str) -> Color {
        let (y, x) = self.find(needle).unwrap_or_else(|| panic!("{needle:?}"));
        self.background[y][x]
    }

    fn modifier(&self, needle: &str) -> Modifier {
        let (y, x) = self.find(needle).unwrap_or_else(|| panic!("{needle:?}"));
        self.modifiers[y][x]
    }

    /// Rows strictly between the first row containing `from` and the first
    /// containing `until` after it.
    fn between(&self, from: &str, until: &str) -> Vec<String> {
        let start = self.row(from) + 1;
        let end = self.rows[start..]
            .iter()
            .position(|row| row.contains(until))
            .map(|offset| start + offset)
            .unwrap_or_else(|| panic!("{until:?} missing\n{}", self.text()));
        self.rows[start..end]
            .iter()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }
}

fn caps(color_depth: ColorDepth, reduced_motion: bool) -> Capabilities {
    Capabilities {
        color_depth,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion,
    }
}

fn truecolor() -> Capabilities {
    caps(ColorDepth::TrueColor, false)
}

fn render(state: &AppState, width: u16, height: u16) -> Rendered {
    render_with(state, width, height, truecolor(), &mut WrapCache::default())
}

fn render_with(
    state: &AppState,
    width: u16,
    height: u16,
    capabilities: Capabilities,
    cache: &mut WrapCache,
) -> Rendered {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, capabilities, cache))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let (mut rows, mut foreground, mut background, mut modifiers) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for y in 0..buffer.area.height {
        let mut row = String::new();
        let (mut row_fg, mut row_bg, mut row_mod) = (Vec::new(), Vec::new(), Vec::new());
        let mut text_x = 0;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if x >= text_x {
                row.push_str(cell.symbol());
                text_x = x + (unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16);
            }
            row_fg.push(cell.fg);
            row_bg.push(cell.bg);
            row_mod.push(cell.modifier);
        }
        rows.push(row);
        foreground.push(row_fg);
        background.push(row_bg);
        modifiers.push(row_mod);
    }
    Rendered {
        rows,
        foreground,
        background,
        modifiers,
    }
}

fn project(state: &mut AppState, kind: EventKind) {
    if let Some(event) = UiEvent::from_core(SessionEvent::new(1, kind)) {
        state.apply_event(event);
    }
}

/// A finished call: started, 1.2 s of work, ended.
fn tool(state: &mut AppState, id: &str, name: &str, arguments: &str, output: &str, ok: bool) {
    let (batch_id, call_id) = (format!("batch-{id}"), format!("call-{id}"));
    project(
        state,
        EventKind::ToolStarted {
            batch_id: batch_id.clone(),
            call_id: call_id.clone(),
            name: name.into(),
            arguments: arguments.into(),
        },
    );
    project(
        state,
        EventKind::ToolOutput {
            batch_id: batch_id.clone(),
            call_id: call_id.clone(),
            name: name.into(),
            output: output.into(),
        },
    );
    state.clock.elapsed_ms += 1_200;
    project(
        state,
        EventKind::ToolFinished {
            batch_id,
            call_id,
            name: name.into(),
            success: ok,
            duration_ms: 1_200,
        },
    );
}

fn connected() -> AppState {
    let mut state = AppState::new();
    state.apply_event(UiEvent::SessionSnapshot {
        session_id: SessionId("organization".into()),
        cwd: r"C:\Projects\demo".into(),
        skill_names: Vec::new(),
    });
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::Anthropic),
        authenticated: true,
    });
    state
}

/// The turn that is the reference for the whole file: thinking, reads, notes
/// between calls, an edit, a failed command, a second edit, a passing command
/// and the answer. `finish` ends the run.
fn turn(finish: bool) -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "O parser quebra com CRLF. Corrija e cubra com testes.".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "Vou ler o parser e os testes antes de mexer.".into(),
    });
    state.clock.elapsed_ms += 3_200;
    state.apply_event(UiEvent::ThinkingEnded);
    for (id, name, arguments) in [
        ("1", "read", r#"{"path":"src/parser.rs"}"#),
        ("2", "read", r#"{"path":"src/errors.rs"}"#),
        ("3", "read", r#"{"path":"tests/parser.rs"}"#),
        ("4", "search", r#"{"pattern":"split\\("}"#),
    ] {
        tool(&mut state, id, name, arguments, "ok", true);
    }
    state.apply_event(UiEvent::AssistantDelta {
        text: "Encontrei a causa: o split deixa o CR no fim da linha.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    tool(
        &mut state,
        "5",
        "patch",
        r#"{"path":"src/parser.rs","edits":[{"expected":"t.split('\\n')","replacement":"t.lines()"}]}"#,
        "patched src/parser.rs:3; replaced 13 bytes with 9 bytes; bytes=90; sha256=ab",
        true,
    );
    tool(
        &mut state,
        "6",
        "shell",
        r#"{"command":"cargo test parser"}"#,
        "exit 101\nstderr:\nerror: test failed",
        false,
    );
    state.apply_event(UiEvent::AssistantDelta {
        text: "Um teste ainda falha. Corrigindo errors.rs.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    tool(
        &mut state,
        "7",
        "patch",
        r#"{"path":"src/errors.rs","edits":[{"expected":"push(line)","replacement":"push(file)"}]}"#,
        "patched src/errors.rs:9; replaced 10 bytes with 10 bytes; bytes=120; sha256=cd",
        true,
    );
    tool(
        &mut state,
        "8",
        "shell",
        r#"{"command":"cargo test"}"#,
        "exit 0\nstdout:\ntest result: ok",
        true,
    );
    state.apply_event(UiEvent::AssistantDelta {
        text: "Resultado: os dois problemas tinham a mesma origem e estão corrigidos.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    if finish {
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        state.clock.elapsed_ms += 12_000;
    }
    state
}

fn works(state: &AppState) -> Vec<&Block> {
    state
        .blocks()
        .iter()
        .filter(|block| matches!(block.kind(), BlockKind::Work(_)))
        .collect()
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Action {
    Action::Key(KeyEvent::new(code, modifiers))
}

fn pin(state: &mut AppState, id: &slim_tui::api::BlockId) {
    state.scroll.mode = FollowMode::Pinned(ScrollAnchor {
        block_id: id.clone(),
        row_offset: 0,
    });
}

// ---- 1. the work of a finished turn --------------------------------------

#[test]
fn a_finished_turn_folds_its_work_into_one_row_under_the_header() {
    let state = turn(true);
    let rendered = render(&state, 80, 40);

    // One row says what the work was; the failure keeps a row of its own.
    let header = rendered.row("● Slim");
    assert_eq!(
        rendered.rows[header + 1].trim_end(),
        "  ▸ Trabalhou · 3 leituras, 1 busca, 2 edições, 2 comandos",
        "{}",
        rendered.text()
    );
    assert!(
        rendered.rows[header + 2].starts_with("    ✕ $ cargo test parser · exit 101"),
        "{}",
        rendered.text()
    );
    // Nothing of the work shows; the answer and the receipt do.
    for hidden in ["Pensou", "Encontrei a causa", "Corrigindo errors"] {
        assert!(
            !rendered.text().contains(hidden),
            "{hidden}\n{}",
            rendered.text()
        );
    }
    assert_eq!(rendered.text().matches("● Slim").count(), 1);
    assert!(rendered.text().contains("Resultado: os dois problemas"));
    assert!(
        rendered.text().contains("2 arquivos"),
        "{}",
        rendered.text()
    );
    // The folded blocks are still in the transcript, in order, untouched.
    let work = works(&state);
    assert_eq!(work.len(), 1);
    let row = state
        .blocks()
        .iter()
        .position(|block| block.id == work[0].id)
        .unwrap();
    assert_eq!(slim_tui::work::span_end(state.blocks(), row) - row - 1, 11);
    assert_eq!(
        state
            .blocks()
            .iter()
            .filter(|block| matches!(block.kind(), BlockKind::Tool(_)))
            .count(),
        8
    );
    // A breathing row separates the folded work from the answer.
    let answer = rendered.row("Resultado: os dois");
    assert!(rendered.rows[answer - 1].trim().is_empty());
}

#[test]
fn enter_on_the_row_restores_the_unfolded_presentation_and_folds_again() {
    let mut state = turn(true);
    let folded = render(&state, 80, 70);
    let work = works(&state)[0].id.clone();
    pin(&mut state, &work);
    reduce(&mut state, key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(works(&state)[0].fold, FoldState::Expanded);
    let open = render(&state, 80, 70);

    // The open row points down and keeps what it said; below it the transcript
    // reads as it did before the fold: the same blocks, the same rows.
    assert!(
        open.rows[open.row("● Slim") + 1]
            .trim_end()
            .starts_with("> ▾ Trabalhou · 3 leituras"),
        "{}",
        open.text()
    );
    let mut unfolded = AppState::new();
    for block in state.blocks() {
        if !matches!(block.kind(), BlockKind::Work(_)) {
            unfolded.append_block(block.clone());
        }
    }
    unfolded.clock = state.clock;
    let reference = render(&unfolded, 80, 70);
    let expected = reference.between("● Slim", "✓ 2 arquivos");
    let actual = open.between("▾ Trabalhou", "✓ 2 arquivos");
    assert_eq!(
        actual,
        expected,
        "open:\n{}\nreference:\n{}",
        open.text(),
        reference.text()
    );
    assert!(open.text().contains("Encontrei a causa"));
    assert!(open.text().contains("▸ Pensou"));
    assert!(open.text().contains("✓ 3 leituras, 1 busca"));

    // Enter again folds it back to the very same rows.
    reduce(&mut state, key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(works(&state)[0].fold, FoldState::Collapsed);
    state.scroll.mode = FollowMode::LiveEdge { prompt_id: None };
    let again = render(&state, 80, 70);
    assert_eq!(
        again.between("● Slim", "✓ 2 arquivos"),
        folded.between("● Slim", "✓ 2 arquivos")
    );
    // At the live edge the same Enter, with an empty composer, reaches the
    // last foldable row on screen: the work row.
    state.scroll.mode = FollowMode::LiveEdge { prompt_id: None };
    let action = slim_tui::runtime::terminal_action(
        crossterm::event::Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &state,
        (80, 40),
        &mut WrapCache::default(),
    );
    assert!(matches!(action, Some(Action::ToggleBlock(id)) if id == work));
}

#[test]
fn a_turn_still_running_failed_or_interrupted_keeps_its_work_in_view() {
    let running = turn(false);
    assert!(works(&running).is_empty());
    assert!(render(&running, 80, 40)
        .text()
        .contains("✓ 3 leituras, 1 busca"));

    for end in [
        UiEvent::RunFailed {
            run_id: Some(1),
            message: "provedor indisponível".into(),
        },
        UiEvent::RunStopped {
            run_id: 1,
            message: "parada".into(),
        },
        UiEvent::RunCancelled { run_id: 1 },
    ] {
        let mut state = turn(false);
        state.apply_event(end.clone());
        assert!(works(&state).is_empty(), "{end:?}");
        let rendered = render(&state, 80, 50);
        assert!(rendered.text().contains("Encontrei a causa"), "{end:?}");
        assert!(!rendered.text().contains("Trabalhou"), "{end:?}");
    }
}

#[test]
fn there_is_no_row_when_nothing_sits_between_the_header_and_the_answer() {
    let answer = |state: &mut AppState| {
        state.apply_event(UiEvent::AssistantDelta {
            text: "Pronto.".into(),
        });
        state.apply_event(UiEvent::AssistantEnded);
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    };
    let start = || {
        let mut state = connected();
        state.apply_event(UiEvent::UserMessageAdded { text: "oi".into() });
        state.apply_event(UiEvent::run_started(1));
        state
    };
    // Only the answer.
    let mut bare = start();
    answer(&mut bare);
    assert!(works(&bare).is_empty());
    // A lone thought or a lone call is already one row.
    let mut thought = start();
    thought.apply_event(UiEvent::ThinkingStarted);
    thought.apply_event(UiEvent::ThinkingDelta { text: "hm".into() });
    thought.apply_event(UiEvent::ThinkingEnded);
    answer(&mut thought);
    assert!(works(&thought).is_empty());
    assert!(render(&thought, 80, 20).text().contains("▸ Pensou"));
    let mut call = start();
    tool(&mut call, "1", "read", r#"{"path":"a.rs"}"#, "x", true);
    answer(&mut call);
    assert!(works(&call).is_empty());
    // A thought and a call are not.
    let mut both = start();
    both.apply_event(UiEvent::ThinkingStarted);
    both.apply_event(UiEvent::ThinkingDelta { text: "hm".into() });
    both.apply_event(UiEvent::ThinkingEnded);
    tool(&mut both, "1", "read", r#"{"path":"a.rs"}"#, "x", true);
    answer(&mut both);
    assert_eq!(works(&both).len(), 1);
    // A turn that ends on a call has no answer to fold toward.
    let mut open = start();
    open.apply_event(UiEvent::ThinkingStarted);
    open.apply_event(UiEvent::ThinkingDelta { text: "hm".into() });
    open.apply_event(UiEvent::ThinkingEnded);
    tool(&mut open, "1", "read", r#"{"path":"a.rs"}"#, "x", true);
    open.apply_event(UiEvent::RunCompleted { run_id: 1 });
    assert!(works(&open).is_empty());
}

#[test]
fn a_restored_session_folds_each_finished_turn_like_a_live_one() {
    let call = |name: &str, id: &str| TranscriptMessage {
        role: TranscriptRole::Tool {
            batch_id: ToolBatchId(format!("b-{id}").into()),
            call_id: ToolCallId(format!("c-{id}").into()),
            name: name.into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
        },
        text: "saved result".into(),
    };
    let text = |role, text: &str| TranscriptMessage {
        role,
        text: text.into(),
    };
    let mut state = AppState::new();
    state.apply_event(UiEvent::SessionRestored {
        session_id: SessionId("restored".into()),
        cwd: r"C:\Projects\demo".into(),
        messages: vec![
            text(TranscriptRole::User, "primeira pergunta"),
            text(TranscriptRole::Assistant, "Vou olhar."),
            call("read", "1"),
            call("shell", "2"),
            text(TranscriptRole::Assistant, "Primeira resposta."),
            text(TranscriptRole::User, "segunda pergunta"),
            text(TranscriptRole::Assistant, "Segunda resposta."),
            text(TranscriptRole::User, "terceira pergunta"),
            call("read", "3"),
            call("read", "4"),
            text(TranscriptRole::Assistant, "Terceira resposta."),
        ],
        skill_names: Vec::new(),
    });
    // The turns with work fold; the one with nothing between prompt and
    // answer does not. Restored calls carry no timing, so the row has none.
    let summaries: Vec<String> = works(&state)
        .iter()
        .map(|block| match block.kind() {
            BlockKind::Work(work) => work.summary(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        summaries,
        ["Trabalhou · 1 leitura, 1 comando", "Trabalhou · 2 leituras"]
    );
    let rendered = render(&state, 80, 40);
    assert!(rendered
        .text()
        .contains("▸ Trabalhou · 1 leitura, 1 comando"));
    assert!(rendered.text().contains("▸ Trabalhou · 2 leituras"));
    for hidden in ["Vou olhar.", "histórico"] {
        assert!(
            !rendered.text().contains(hidden),
            "{hidden}\n{}",
            rendered.text()
        );
    }
    for answer in [
        "Primeira resposta.",
        "Segunda resposta.",
        "Terceira resposta.",
    ] {
        assert!(rendered.text().contains(answer), "{answer}");
    }
}

#[test]
fn search_scroll_anchors_selection_and_details_keep_working_on_folded_work() {
    let mut state = turn(true);
    let work = works(&state)[0].id.clone();
    let note = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Assistant(text) if text.contains("Encontrei")))
        .expect("folded note")
        .id
        .clone();

    // Scroll anchors: a folded block resolves to the row that stands for it,
    // and the rows of the turn round-trip through anchors.
    let mut cache = WrapCache::default();
    let index = HeightIndex::build(state.blocks(), 80, &mut cache);
    assert_eq!(index.prefix_for_block(&note), index.prefix_for_block(&work));
    // Every row resolves to itself; a blank row that leads a block resolves
    // to the block's first row.
    for row in index.prefix_for_block(&work).unwrap()..index.total_rows {
        let anchor = index.anchor_for_row(row).expect("anchor");
        let resolved = index.row_for_anchor(&anchor).expect("resolves");
        assert!(
            resolved == row || resolved == row + 1,
            "row {row} -> {resolved}"
        );
    }
    // Folded, the turn is much shorter than open.
    let folded_rows = index.total_rows;
    let mut open = state.clone();
    reduce(&mut open, Action::ToggleBlock(work.clone()));
    let open_rows = HeightIndex::build(open.blocks(), 80, &mut WrapCache::default()).total_rows;
    assert!(open_rows > folded_rows + 8, "{open_rows} vs {folded_rows}");

    // Search finds text inside the folded work and points at its row.
    reduce(&mut state, key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for character in "Encontrei".chars() {
        reduce(
            &mut state,
            key(KeyCode::Char(character), KeyModifiers::NONE),
        );
    }
    let matches = slim_tui::inspector::search_match_indices_filtered(
        state.blocks(),
        "Encontrei",
        slim_tui::inspector::SearchFilter::All,
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(state.blocks()[matches[0]].id, note);
    let found = render(&state, 80, 40);
    let row = found.row("Trabalhou");
    assert!(
        found.rows[row].contains("> ▸ Trabalhou"),
        "{}",
        found.text()
    );
    assert_eq!(
        found.background[row][10],
        Color::Rgb(0x18, 0x18, 0x18),
        "match highlight"
    );
    assert_eq!(state.selected_block_id(), Some(&work));

    // The block the view rests on is copied as itself; Enter opens the work.
    state.search = None;
    let effects = reduce(&mut state, key(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert!(
        effects.iter().any(|effect| matches!(effect,
            Effect::CopyToClipboard(text) if text.contains("Encontrei a causa"))),
        "{effects:?}"
    );
    reduce(&mut state, key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(works(&state)[0].fold, FoldState::Expanded);
    assert!(render(&state, 80, 70).text().contains("Encontrei a causa"));

    // Selection copies what is painted: the row, with its text.
    let folded = turn(true);
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, &folded, truecolor(), &mut WrapCache::default()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let painted = render(&folded, 80, 40);
    let y = painted.row("Trabalhou") as u16;
    let selection = slim_tui::selection::ScreenSelection {
        anchor: slim_tui::selection::ScreenPos::new(2, y),
        head: slim_tui::selection::ScreenPos::new(79, y),
    };
    let copied = slim_tui::selection::extract_selected_text(
        buffer,
        selection,
        ratatui::layout::Rect::new(2, 0, 78, 40),
    );
    assert!(copied.contains("Trabalhou · 3 leituras"), "{copied:?}");

    // Details still list what the folded work changed.
    let mut details = turn(true);
    reduce(&mut details, key(KeyCode::Char('d'), KeyModifiers::CONTROL));
    let panel = render(&details, 100, 40).text();
    assert!(
        panel.contains("src/parser.rs") && panel.contains("src/errors.rs"),
        "{panel}"
    );
}

#[test]
fn a_view_resting_on_a_block_when_its_work_folds_keeps_its_place() {
    let mut state = turn(false);
    let read = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Tool(tool) if tool.name == "read"))
        .expect("a read")
        .id
        .clone();
    pin(&mut state, &read);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    let work = works(&state)[0].id.clone();
    // The anchor still resolves, to the row that now stands for the block, and
    // that row is the one the view shows as selected.
    let index = HeightIndex::build(state.blocks(), 80, &mut WrapCache::default());
    assert_eq!(
        index.row_for_anchor(&anchor_of(&state)),
        index.prefix_for_block(&work)
    );
    assert_eq!(state.selected_block_id(), Some(&work));
    let rendered = render(&state, 80, 24);
    assert!(
        rendered.text().contains("> ▸ Trabalhou"),
        "{}",
        rendered.text()
    );
    // Opening the work leaves the view on the block it rested on.
    reduce(&mut state, key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(anchor_of(&state).block_id, read);
    let open = render(&state, 80, 24);
    assert!(open.text().contains("▾ Trabalhou"), "{}", open.text());
}

fn anchor_of(state: &AppState) -> ScrollAnchor {
    match &state.scroll.mode {
        FollowMode::Pinned(anchor) => anchor.clone(),
        other => panic!("not pinned: {other:?}"),
    }
}

// ---- 2. small groups name what they did ---------------------------------

#[test]
fn small_groups_name_their_calls_and_larger_ones_count() {
    let group = |calls: &[(&str, &str, &str)]| {
        let mut state = connected();
        state.apply_event(UiEvent::UserMessageAdded { text: "vai".into() });
        state.apply_event(UiEvent::run_started(1));
        for (index, (name, arguments, _)) in calls.iter().enumerate() {
            tool(&mut state, &index.to_string(), name, arguments, "ok", true);
        }
        state.clock.elapsed_ms += 1_000;
        state
    };
    // Two calls: their own words, in order, then the time.
    let two = group(&[
        (
            "patch",
            r#"{"path":"src/parser.rs","edits":[{"expected":"a","replacement":"b"}]}"#,
            "",
        ),
        ("shell", r#"{"command":"cargo test parser"}"#, ""),
    ]);
    let text = render(&two, 100, 20).text();
    assert!(
        text.contains("✓ Editou src/parser.rs · $ cargo test parser · 2.4s"),
        "{text}"
    );
    // Another pair reads differently, which is the point.
    let other = group(&[
        ("read", r#"{"path":"src/errors.rs"}"#, ""),
        ("search", r#"{"pattern":"push"}"#, ""),
    ]);
    let text = render(&other, 100, 20).text();
    assert!(
        text.contains(r#"✓ Leu src/errors.rs · Buscou "push" · 2.4s"#),
        "{text}"
    );
    // A long label is shortened, never past what still reads...
    let tight = render(&two, 50, 20).text();
    assert!(
        tight.contains("✓ Editou src/") && tight.contains("… · $ cargo test parser · 2.4s"),
        "{tight}"
    );
    // Both labels give up what they can toward their floor of 14 cells...
    let narrower = render(&two, 44, 20).text();
    assert!(
        narrower.contains("✓ Editou src/") && narrower.contains("… · $ cargo test"),
        "{narrower}"
    );
    // ...and when the words do not fit even so, the counts take over.
    let narrow = render(&two, 36, 20).text();
    assert!(narrow.contains("✓ 1 edição, 1 comando · 2.4s"), "{narrow}");
    // Three or more are counted, as before.
    let three = group(&[
        ("read", r#"{"path":"a.rs"}"#, ""),
        ("read", r#"{"path":"b.rs"}"#, ""),
        ("shell", r#"{"command":"ls"}"#, ""),
    ]);
    let text = render(&three, 100, 20).text();
    assert!(text.contains("✓ 2 leituras, 1 comando · 3.6s"), "{text}");
    // The row is still one row.
    let rows = HeightIndex::build(two.blocks(), 100, &mut WrapCache::default()).total_rows;
    assert_eq!(rows, 5, "prompt (3 rows), the agent header and the group");
}

// ---- 3. an open group ----------------------------------------------------

#[test]
fn an_open_group_points_down_and_steps_its_members_in() {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded { text: "vai".into() });
    state.apply_event(UiEvent::run_started(1));
    tool(
        &mut state,
        "1",
        "read",
        r#"{"path":"src/parser.rs"}"#,
        "linha curta",
        true,
    );
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "O split conserva o CR e a ordem das linhas continua a mesma depois.".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);
    tool(
        &mut state,
        "2",
        "patch",
        r#"{"path":"src/parser.rs"}"#,
        "patched src/parser.rs:2",
        true,
    );
    state.clock.elapsed_ms += 1_000;
    let closed = render(&state, 80, 24);
    assert!(
        closed
            .text()
            .contains("✓ Leu src/parser.rs · Editou src/parser.rs"),
        "{}",
        closed.text()
    );
    // Opening the thought opens the group it sits in; the members keep their
    // own details closed.
    let thought = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Thinking(_)))
        .expect("thought")
        .id
        .clone();
    reduce(&mut state, Action::ToggleBlock(thought));
    for width in [80u16, 36] {
        let open = render(&state, width, 40);
        // The header names the calls where they fit and counts them where not.
        let named = if width == 80 {
            "▾ Leu src/parser.rs · Editou src/parser.rs"
        } else {
            "▾ 1 leitura, 1 edição"
        };
        let header = open.row(named);
        assert!(
            open.rows[header].starts_with(&format!("  {named}")),
            "{}",
            open.text()
        );
        // Members and the thought sit one step in from the header, with
        // their own text one step further.
        assert!(
            open.rows[header + 1].starts_with("    ✓ Leu"),
            "{}",
            open.text()
        );
        let thought = open.row("▾ Pensou");
        assert!(
            open.rows[thought].starts_with("    ▾ Pensou"),
            "{}",
            open.text()
        );
        assert!(
            open.rows[thought + 1].starts_with("      "),
            "{}",
            open.text()
        );
        let edit = open.row("✓ Editou");
        assert!(
            open.rows[edit].starts_with("    ✓ Editou"),
            "{}",
            open.text()
        );
        // The measured height is what is drawn, from the prompt to the last
        // member.
        let drawn = edit - open.row("● Você") + 1;
        let measured = HeightIndex::build(state.blocks(), width, &mut WrapCache::default());
        assert_eq!(
            drawn as u64,
            measured.total_rows,
            "at {width}\n{}",
            open.text()
        );
    }
}

#[test]
fn an_open_group_of_identical_failures_steps_its_members_in_too() {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded { text: "vai".into() });
    state.apply_event(UiEvent::run_started(1));
    for id in ["1", "2", "3"] {
        tool(
            &mut state,
            id,
            "shell",
            r#"{"command":"cargo test"}"#,
            "exit 101\nstderr:\nerror: test failed",
            false,
        );
    }
    state.clock.elapsed_ms += 1_000;
    let closed = render(&state, 80, 20);
    let header = closed.row("✕ $ cargo test");
    assert!(closed.rows[header].contains("×3"), "{}", closed.text());
    let leader = state.blocks()[1].id.clone();
    reduce(&mut state, Action::ToggleBlock(leader));
    let open = render(&state, 80, 20);
    let header = open.row("▾ $ cargo test");
    assert!(
        open.rows[header].starts_with("  ▾ $ cargo test"),
        "{}",
        open.text()
    );
    for member in 1..=3 {
        assert!(
            open.rows[header + member].starts_with("    ✕ $ cargo test"),
            "{}",
            open.text()
        );
    }
    let measured = HeightIndex::build(state.blocks(), 80, &mut WrapCache::default());
    assert_eq!(
        (header + 3 - open.row("● Você") + 1) as u64,
        measured.total_rows
    );
}

// ---- 4. a failed run -----------------------------------------------------

#[test]
fn a_failed_run_shows_its_reason_and_the_way_on_under_the_partial_answer() {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "Rode a suíte.".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta {
        text: "Rodei a suíte e três testes falharam. Comecei pelo".into(),
    });
    state.apply_event(UiEvent::RunFailed {
        run_id: Some(1),
        message: "provedor indisponível: http 503 após 3 tentativas".into(),
    });
    let rendered = render(&state, 80, 20);
    let text = rendered.text();
    let header = rendered.row("✕ Slim");
    let partial = rendered.row("Rodei a suíte e três testes");
    let reason = rendered.row("provedor indisponível: http 503");
    assert!(header < partial && partial < reason, "{text}");
    assert!(rendered.rows[reason].starts_with("  ✕ "), "{text}");
    assert!(
        rendered.rows[reason + 1].contains("Envie uma nova mensagem"),
        "{text}"
    );
    // `/retry` resumes a connection that is paused inside a live run; once the
    // run has failed there is nothing for it to resume, so it is not offered.
    assert!(!state.working);
    assert!(!text.contains("/retry"), "{text}");
    assert_eq!(works(&state).len(), 0);
}

// ---- 5. code against quotes ---------------------------------------------

fn answer(markdown: &str) -> AppState {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "mostre".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::AssistantDelta {
        text: markdown.into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    state.clock.elapsed_ms = 5_000;
    state
}

#[test]
fn fenced_code_names_its_language_and_quotes_keep_a_rail_of_their_own() {
    let state = answer("Antes.\n\n```rust\nfn main() {}\n```\n\n> Não rodei o build.\n");
    let rendered = render(&state, 60, 24);
    let label = rendered.row("│ rust");
    // The tag is a quiet first row on the code surface; the code follows.
    assert_eq!(rendered.rows[label + 1].trim_end(), "    │ fn main() {}");
    assert_eq!(rendered.fg("rust"), CODE_RAIL);
    assert_eq!(rendered.bg("rust"), CODE_BG);
    assert_eq!(rendered.fg("fn main"), TEXT);
    // The quote has its own rail, its own tone and no surface.
    let quote = rendered.row("Não rodei");
    assert!(
        rendered.rows[quote].starts_with("    ┆ Não rodei o build."),
        "{}",
        rendered.text()
    );
    assert_eq!(rendered.fg("Não rodei"), SECONDARY);
    assert_ne!(rendered.bg("Não rodei"), CODE_BG);
    assert!(!rendered.rows[quote].contains('│'));
    assert!(!rendered.rows[label + 1].contains('┆'));
    // The measured height is what is drawn, tag row included.
    let measured = HeightIndex::build(state.blocks(), 60, &mut WrapCache::default()).total_rows;
    assert_eq!(
        (quote - rendered.row("● Você") + 1) as u64,
        measured,
        "{}",
        rendered.text()
    );

    // Without color the two stay apart by the rail itself, and the tag stays
    // apart from the code by weight.
    for depth in [ColorDepth::Ansi16, ColorDepth::None] {
        let plain = render_with(&state, 60, 24, caps(depth, true), &mut WrapCache::default());
        assert!(plain.text().contains("    │ fn main() {}"), "{depth:?}");
        assert!(
            plain.text().contains("    ┆ Não rodei o build."),
            "{depth:?}"
        );
    }
    let none = render_with(
        &state,
        60,
        24,
        caps(ColorDepth::None, true),
        &mut WrapCache::default(),
    );
    assert!(none.modifier("rust").contains(Modifier::DIM));
    assert!(!none.modifier("fn main").contains(Modifier::DIM));
}

#[test]
fn only_a_fence_that_names_a_language_gets_a_tag() {
    let tags = |markdown: &str| {
        let rendered = render(&answer(markdown), 60, 24);
        rendered
            .rows
            .iter()
            .filter(|row| row.starts_with("    │ "))
            .map(|row| row.trim_end().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(tags("```\nplain\n```"), ["    │ plain"]);
    assert_eq!(tags("```rust,ignore\nx\n```"), ["    │ rust", "    │ x"]);
    assert_eq!(
        tags("```diff\n-a\n+b\n```"),
        ["    │ diff", "    │ -a", "    │ +b"]
    );
    assert_eq!(
        tags("```c++ title=x\nint a;\n```"),
        ["    │ c++", "    │ int a;"]
    );
    // A list item shares its row with the code that starts in it.
    assert!(tags("- item\n\n  ```rust\n  x\n  ```")
        .iter()
        .all(|row| row != "    │ rust"));
}

// ---- 6. the prompt's marker ----------------------------------------------

#[test]
fn the_prompt_is_found_by_its_band_or_by_a_marker_of_its_own() {
    let state = turn(true);
    for (depth, user, agent) in [
        (ColorDepth::TrueColor, "● Você", "● Slim"),
        (ColorDepth::Ansi256, "● Você", "● Slim"),
        (ColorDepth::Ansi16, "> Você", "● Slim"),
        (ColorDepth::None, "> Você", "* Slim"),
    ] {
        let rendered = render_with(&state, 80, 40, caps(depth, true), &mut WrapCache::default());
        assert!(
            rendered.text().contains(&format!("  {user}")),
            "{depth:?}\n{}",
            rendered.text()
        );
        assert!(rendered.text().contains(&format!("  {agent}")), "{depth:?}");
        // The two sides never share a marker where the band cannot show.
        if matches!(depth, ColorDepth::Ansi16 | ColorDepth::None) {
            assert_ne!(user.chars().next(), agent.chars().next(), "{depth:?}");
        }
    }
    // Where shades show, the header and every row of the prompt sit on the
    // band, edge to edge, and nothing else in the turn does.
    let rendered = render(&state, 80, 40);
    let band = Color::Rgb(0x14, 0x14, 0x14);
    let header = rendered.row("● Você");
    let body = rendered.row("O parser quebra");
    for row in [header, body] {
        assert!(
            rendered.background[row][..78].iter().all(|bg| *bg == band),
            "row {row} is not on the band\n{}",
            rendered.text()
        );
    }
    for needle in ["● Slim", "Trabalhou", "Resultado"] {
        assert_ne!(rendered.bg(needle), band, "{needle}");
    }
}

// ---- 7. a tool row settles ------------------------------------------------

fn settling(later: u64) -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "leia".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    project(
        &mut state,
        EventKind::ToolStarted {
            batch_id: "b-ok".into(),
            call_id: "c-ok".into(),
            name: "read".into(),
            arguments: r#"{"path":"src/parser.rs"}"#.into(),
        },
    );
    project(
        &mut state,
        EventKind::ToolStarted {
            batch_id: "b-bad".into(),
            call_id: "c-bad".into(),
            name: "shell".into(),
            arguments: r#"{"command":"cargo test parser"}"#.into(),
        },
    );
    state.clock.elapsed_ms += 1_200;
    for (id, name, success) in [("ok", "read", true), ("bad", "shell", false)] {
        project(
            &mut state,
            EventKind::ToolFinished {
                batch_id: format!("b-{id}"),
                call_id: format!("c-{id}"),
                name: name.into(),
                success,
                duration_ms: 1_200,
            },
        );
    }
    state.clock.elapsed_ms += later;
    state
}

fn brightness(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => u32::from(r) + u32::from(g) + u32::from(b),
        other => panic!("not truecolor: {other:?}"),
    }
}

#[test]
fn a_tool_row_eases_from_a_brighter_tone_to_its_resting_one() {
    let mut cache = WrapCache::default();
    let mut previous: Option<(u32, u32, u32, u32)> = None;
    let mut rows_at_rest = None;
    for later in [0u64, 83, 166, 249] {
        let rendered = render_with(&settling(later), 80, 24, truecolor(), &mut cache);
        let tones = (
            brightness(rendered.fg("✓")),
            brightness(rendered.fg("Leu")),
            brightness(rendered.fg("✕")),
            brightness(rendered.fg("$ cargo test")),
        );
        if let Some(before) = previous {
            // Each step is quieter than the last, on the glyph and the verb of
            // the call that worked and of the one that failed.
            assert!(
                tones.0 < before.0 && tones.1 < before.1,
                "{later}: {tones:?} vs {before:?}"
            );
            assert!(tones.2 < before.2, "{later}: {tones:?} vs {before:?}");
        }
        previous = Some(tones);
        rows_at_rest = Some(rendered.fg("✓"));
        // Style only: the same rows at every moment of the window.
        let reference = render(&settling(0), 80, 24);
        assert_eq!(
            rendered.rows[2..8],
            reference.rows[2..8],
            "{later} ms moved text"
        );
    }
    // At the end of the window the glyph is back to the muted resting tone of
    // a call that only observes; the failure is the error color again.
    assert_eq!(rows_at_rest, Some(MUTED));
    let rest = render_with(&settling(300), 80, 24, truecolor(), &mut cache);
    assert_eq!(rest.fg("✕"), ERROR);
    // Reduced motion shows the resting tone from the first frame.
    let reduced = render_with(
        &settling(0),
        80,
        24,
        caps(ColorDepth::TrueColor, true),
        &mut WrapCache::default(),
    );
    assert_eq!(reduced.fg("✓"), MUTED);
    assert_eq!(reduced.fg("✕"), ERROR);
}

#[test]
fn a_motion_frame_of_the_settle_does_not_invalidate_the_height_or_wrap_caches() {
    let mut cache = WrapCache::default();
    let mut state = settling(0);
    let start = state.clock.elapsed_ms;
    render_with(&state, 80, 24, truecolor(), &mut cache);
    let (heights, bodies) = (cache.height_misses(), cache.body_misses());
    for later in [40u64, 83, 120, 166, 200] {
        state.clock.elapsed_ms = start + later;
        render_with(&state, 80, 24, truecolor(), &mut cache);
        assert_eq!(cache.height_misses(), heights, "{later} ms");
        assert_eq!(cache.body_misses(), bodies, "{later} ms");
    }
}

// ---- 8. the last line of a running command --------------------------------

fn running(progress: &[&str], elapsed_ms: u64) -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "rode".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    project(
        &mut state,
        EventKind::ToolStarted {
            batch_id: "b".into(),
            call_id: "c".into(),
            name: "shell".into(),
            arguments: r#"{"command":"cargo clippy --all-targets"}"#.into(),
        },
    );
    state.clock.elapsed_ms += elapsed_ms;
    for preview in progress {
        state.apply_event(UiEvent::ToolProgress {
            batch_id: ToolBatchId("b".into()),
            call_id: ToolCallId("c".into()),
            name: "shell".into(),
            preview: (*preview).into(),
            content_handle: None,
        });
    }
    state
}

#[test]
fn a_running_command_shows_its_last_output_line_after_the_clock() {
    // Nothing yet: the row is what it was.
    let silent = render(
        &running(&["no output yet · out 0 B · err 0 B"], 5_300),
        100,
        20,
    );
    let row = silent.row("○ $ cargo clippy");
    assert_eq!(
        silent.rows[row].trim_end(),
        "  ○ $ cargo clippy --all-targets · limit 600s · out 0 B · err 0 B · 5s"
    );
    let first = render(&running(&[], 5_300), 100, 20);
    assert_eq!(
        first.rows[first.row("○ $ cargo clippy")].trim_end(),
        "  ○ $ cargo clippy --all-targets · limit 600s · 5s"
    );

    // The newest line follows the clock, a step quieter than it.
    let live = render(
        &running(
            &["Checking slim-tui v0.1.0 (crates/slim-tui) · out 0 B · err 212 B"],
            5_300,
        ),
        120,
        20,
    );
    let text = &live.rows[live.row("○ $ cargo clippy")];
    assert!(
        text.trim_end()
            .ends_with("· 5s · Checking slim-tui v0.1.0 (crates/slim-tui)"),
        "{text}"
    );
    assert!(
        brightness(live.fg("Checking")) < brightness(live.fg("5s")),
        "dimmer than the clock"
    );
    assert_eq!(live.fg("5s"), MUTED);

    // A newer line replaces it; the row is still one row, nothing else moves.
    let mut state = running(
        &["Checking slim-tui v0.1.0 (crates/slim-tui) · out 0 B · err 212 B"],
        5_300,
    );
    let mut cache = WrapCache::default();
    let before = render_with(&state, 120, 20, truecolor(), &mut cache);
    let misses = cache.height_misses();
    let rows_before = HeightIndex::build(state.blocks(), 120, &mut WrapCache::default()).total_rows;
    state.apply_event(UiEvent::ToolProgress {
        batch_id: ToolBatchId("b".into()),
        call_id: ToolCallId("c".into()),
        name: "shell".into(),
        preview: "Compiling slim-core v0.1.0 · out 0 B · err 480 B".into(),
        content_handle: None,
    });
    let after = render_with(&state, 120, 20, truecolor(), &mut cache);
    assert!(
        after.text().contains("· 5s · Compiling slim-core v0.1.0"),
        "{}",
        after.text()
    );
    assert!(!after.text().contains("Checking slim-tui"));
    assert_eq!(
        HeightIndex::build(state.blocks(), 120, &mut WrapCache::default()).total_rows,
        rows_before
    );
    assert!(
        cache.height_misses() <= misses + 1,
        "only the running row is measured again"
    );
    for (y, row) in before.rows.iter().enumerate() {
        if !row.contains("cargo clippy") {
            assert_eq!(*row, after.rows[y], "row {y} moved");
        }
    }
}

#[test]
fn the_last_line_is_cut_at_a_word_and_gives_way_to_the_row() {
    let long = "warning: unused variable `bytes_discovered_in_the_workspace` in src/discover.rs";
    let preview = format!("{long} · out 0 B · err 212 B");
    let wide = render(&running(&[preview.as_str()], 5_300), 100, 20);
    let row = wide.rows[wide.row("○ $ cargo clippy")]
        .trim_end()
        .to_owned();
    assert!(row.ends_with('…'), "{row}");
    assert!(
        unicode_width::UnicodeWidthStr::width(row.as_str()) <= 100,
        "{row}"
    );
    let cut = row.rsplit(" · ").next().unwrap().trim_end_matches('…');
    assert!(
        long.starts_with(cut) && cut.split(' ').all(|word| long.contains(word)),
        "{cut:?}"
    );
    // Narrow: the call itself keeps the row and the line is dropped, never
    // wrapped onto a second one.
    let narrow = render(&running(&[preview.as_str()], 5_300), 52, 20);
    let rows: Vec<_> = narrow.rows.iter().filter(|row| row.contains('○')).collect();
    assert_eq!(rows.len(), 1, "{}", narrow.text());
    assert!(!rows[0].contains("warning"), "{}", rows[0]);
    // Other tools keep their progress as it was.
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded { text: "x".into() });
    state.apply_event(UiEvent::run_started(1));
    project(
        &mut state,
        EventKind::ToolStarted {
            batch_id: "p".into(),
            call_id: "p".into(),
            name: "patch".into(),
            arguments: r#"{"path":"a.rs","edits":[{"expected":"a","replacement":"b"}]}"#.into(),
        },
    );
    state.apply_event(UiEvent::ToolProgress {
        batch_id: ToolBatchId("p".into()),
        call_id: ToolCallId("p".into()),
        name: "patch".into(),
        preview: "Waiting for file lock".into(),
        content_handle: None,
    });
    assert!(render(&state, 100, 20)
        .text()
        .contains("Waiting for file lock"));
}

// ---- 9. the glow of an answer being written -------------------------------

fn arriving(later: u64) -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "explique".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.clock = FrameClock {
        frame: 40,
        elapsed_ms: 3_400,
    };
    state.apply_event(UiEvent::AssistantDelta {
        text: "O cache guarda cada resultado pelo peso em bytes e não pelo número de entradas, \
               de modo que uma entrada grande não expulsa dez pequenas sem motivo."
            .into(),
    });
    state.clock = FrameClock {
        frame: 40 + later / 83,
        elapsed_ms: 3_400 + later,
    };
    state
}

#[test]
fn the_newest_words_of_a_streaming_answer_glow_and_settle() {
    let peak = Color::Rgb(138, 209, 160);
    let caret = Color::Rgb(0x72, 0xCC, 0x91);
    let at = |later: u64| render(&arriving(later), 60, 20);
    let fresh = at(0);
    // The last row of the answer is the one lit, strongest at its edge.
    let last = fresh.row("sem motivo");
    let (_, word) = fresh.find("motivo.").expect("last word");
    let edge = word + 6;
    assert_eq!(
        fresh.foreground[last][edge], peak,
        "the final cell is at the peak"
    );
    let deeper = fresh.foreground[last][word];
    assert!(deeper != peak && deeper != TEXT, "still lit {deeper:?}");
    // The caret behind it keeps its own color, and nothing moves.
    assert_eq!(fresh.rows[last].chars().nth(edge + 1), Some('▌'));
    assert_eq!(fresh.foreground[last][edge + 1], caret);
    let steady = |rendered: &Rendered| {
        rendered.rows[..14]
            .iter()
            .map(|row| row.replace('▌', " "))
            .collect::<Vec<_>>()
    };
    assert_eq!(steady(&fresh), steady(&at(166)));
    // Lines already written are not touched.
    assert_ne!(fresh.row("O cache guarda"), last);
    assert_eq!(fresh.fg("O cache"), TEXT);
    // It fades: a little later it is dimmer, and after 450 ms it is gone.
    let later = at(166).foreground[last][edge];
    assert!(later != peak && later != TEXT, "{later:?}");
    assert!(brightness(later) != brightness(peak));
    let settled = at(450);
    for (x, character) in settled.rows[last].chars().enumerate().skip(4) {
        if !character.is_whitespace() {
            assert_eq!(settled.foreground[last][x], TEXT, "x={x} {character:?}");
        }
    }
    // Reduced motion shows no glow at all.
    let reduced = render_with(
        &arriving(0),
        60,
        20,
        caps(ColorDepth::TrueColor, true),
        &mut WrapCache::default(),
    );
    assert_eq!(reduced.foreground[last][edge], TEXT);
    // Where shades are scarce the newest words take weight instead.
    let sixteen = render_with(
        &arriving(0),
        60,
        20,
        caps(ColorDepth::Ansi16, false),
        &mut WrapCache::default(),
    );
    assert!(sixteen.modifier("motivo").contains(Modifier::BOLD));
    assert!(!sixteen.modifier("O cache").contains(Modifier::BOLD));
}

#[test]
fn a_motion_frame_of_the_answer_glow_does_not_invalidate_the_caches() {
    let mut cache = WrapCache::default();
    let mut state = arriving(0);
    render_with(&state, 60, 20, truecolor(), &mut cache);
    let (heights, bodies) = (cache.height_misses(), cache.body_misses());
    for later in [83u64, 166, 249, 332, 415] {
        state.clock = FrameClock {
            frame: 40 + later / 83,
            elapsed_ms: 3_400 + later,
        };
        render_with(&state, 60, 20, truecolor(), &mut cache);
        assert_eq!(cache.height_misses(), heights, "{later} ms");
        assert_eq!(cache.body_misses(), bodies, "{later} ms");
    }
}

// ---- the fold covers the whole stretch -------------------------------------

/// A turn in which something other than the agent working sits between the
/// prompt and the answer: a read, an approval that gets answered, a notice
/// from the harness, then an edit, a command and the answer.
fn turn_with_interruptions(queue: bool) -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "Corrija o parser.".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "Vou ler antes.".into(),
    });
    state.clock.elapsed_ms += 1_000;
    state.apply_event(UiEvent::ThinkingEnded);
    tool(
        &mut state,
        "r1",
        "read",
        r#"{"path":"src/a.rs"}"#,
        "ok",
        true,
    );
    tool(
        &mut state,
        "r2",
        "read",
        r#"{"path":"src/b.rs"}"#,
        "ok",
        true,
    );
    state.apply_event(UiEvent::ApprovalRequired {
        request_id: InteractionRequestId("approval-1".into()),
        summary: "Aplicar o patch em src/a.rs".into(),
        persisted: false,
    });
    state.apply_event(UiEvent::InteractionAcknowledged {
        request_id: InteractionRequestId("approval-1".into()),
        accepted: true,
        message: "aprovado".into(),
    });
    state.append_block(Block::new(
        "compaction-notice",
        BlockKind::System("contexto compactado".into()),
        slim_tui::block::BlockLifecycle::Complete,
    ));
    if queue {
        state.apply_event(UiEvent::QueuedUserAdded {
            text: "depois, rode o clippy".into(),
            position: 1,
        });
    }
    tool(
        &mut state,
        "p1",
        "patch",
        r#"{"path":"src/a.rs","edits":[{"expected":"a","replacement":"b"}]}"#,
        "patched src/a.rs:1; replaced 1 bytes with 1 bytes",
        true,
    );
    tool(
        &mut state,
        "s1",
        "shell",
        r#"{"command":"cargo test"}"#,
        "exit 0",
        true,
    );
    state.apply_event(UiEvent::AssistantDelta {
        text: "Pronto: corrigi o parser.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::RunCompleted { run_id: 1 }),
    );
    state.clock.elapsed_ms += 10_000;
    state
}

#[test]
fn approvals_notices_and_queued_prompts_inside_the_turn_are_folded_with_the_rest() {
    for queue in [false, true] {
        let state = turn_with_interruptions(queue);
        let rows = works(&state);
        assert_eq!(rows.len(), 1, "queue={queue}");
        let at = state
            .blocks()
            .iter()
            .position(|block| block.id == rows[0].id)
            .unwrap();
        // The row stands before the first thing the agent did, and the
        // stretch ends at the answer: nothing before it stays open.
        assert!(matches!(
            state.blocks()[at + 1].kind(),
            BlockKind::Thinking(_)
        ));
        let end = slim_tui::work::span_end(state.blocks(), at);
        assert!(matches!(
            state.blocks()[end].kind(),
            BlockKind::Assistant(_)
        ));
        let rendered = render(&state, 80, 40);
        let text = rendered.text();
        // One row for the whole turn, counted over the whole turn.
        assert!(
            text.contains("▸ Trabalhou 1s · 2 leituras, 1 edição, 1 comando")
                || text.contains("▸ Trabalhou · 2 leituras, 1 edição, 1 comando"),
            "{text}"
        );
        for hidden in [
            "Pensou",
            "src/a.rs",
            "contexto compactado",
            "Aplicar o patch",
            "aprovado",
        ] {
            assert!(!text.contains(hidden), "{hidden}\n{text}");
        }
        assert!(text.contains("Pronto: corrigi o parser."), "{text}");
        assert_eq!(text.matches("● Slim").count(), 1);
        // The queue pops the pending prompt once the run is over; the fold keeps
        // ending at the answer.
        if queue {
            assert_eq!(state.queue_len(), 0, "the queued prompt was consumed");
        }
        // Opened, the turn reads as it did: the approval and the notice are
        // there, in order.
        let mut open = state.clone();
        reduce(&mut open, Action::ToggleBlock(rows[0].id.clone()));
        let open_text = render(&open, 80, 60).text();
        let approval = open_text.find("aprovado").expect("approval record");
        let notice = open_text.find("contexto compactado").expect("notice");
        let answer = open_text.find("Pronto: corrigi").expect("answer");
        assert!(approval < notice && notice < answer, "{open_text}");
    }
}

#[test]
fn a_prompt_removed_after_the_fold_leaves_the_stretch_ending_at_the_answer() {
    let mut state = turn_with_interruptions(true);
    // The harness consumed the prompt as soon as the run ended, so it is gone.
    assert!(!state
        .blocks()
        .iter()
        .any(|block| matches!(block.kind(), BlockKind::QueuedUser(_))));
    let rows: Vec<_> = works(&state).iter().map(|block| block.id.clone()).collect();
    let at = state
        .blocks()
        .iter()
        .position(|block| block.id == rows[0])
        .unwrap();
    assert!(matches!(
        state.blocks()[slim_tui::work::span_end(state.blocks(), at)].kind(),
        BlockKind::Assistant(text) if text.contains("Pronto")
    ));
    // Queue another prompt where it can be removed at will: a paused queue.
    state.queue_paused = true;
    state.apply_event(UiEvent::QueuedUserAdded {
        text: "mais uma".into(),
        position: 1,
    });
    reduce(&mut state, Action::ToggleBlock(rows[0].clone()));
    reduce(&mut state, Action::ToggleBlock(rows[0].clone()));
    let text = render(&state, 80, 40).text();
    assert!(text.contains("Pronto: corrigi o parser."), "{text}");
}

#[test]
fn a_turn_with_a_receipt_leaves_the_duration_to_it_and_one_without_keeps_it() {
    // The reference turn changed files and ran commands: the receipt says 12s.
    let with = render(&turn(true), 80, 40).text();
    assert!(with.contains("▸ Trabalhou · 3 leituras"), "{with}");
    assert!(with.contains("2 comandos, 1 falhou · 12s"), "{with}");
    assert!(!with.contains("Trabalhou 12s"), "{with}");
    // A turn that only read has no receipt, so the row keeps its time.
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "leia".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta { text: "hm".into() });
    state.clock.elapsed_ms += 2_000;
    state.apply_event(UiEvent::ThinkingEnded);
    tool(&mut state, "1", "read", r#"{"path":"a.rs"}"#, "x", true);
    state.apply_event(UiEvent::AssistantDelta { text: "Li.".into() });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    state.clock.elapsed_ms += 2_000;
    let text = render(&state, 80, 20).text();
    assert!(text.contains("▸ Trabalhou 3s · 1 leitura"), "{text}");
    assert!(!text.contains("arquivo"), "no receipt: {text}");
}

// ---- navigation, scrollbar, labels ------------------------------------------

#[test]
fn up_and_down_from_a_block_hidden_under_a_row_move_from_that_row() {
    // Two finished turns, each folded: the fitted anchors are the two rows.
    let mut state = turn(true);
    state.apply_event(UiEvent::UserMessageAdded {
        text: "outra".into(),
    });
    state.apply_event(UiEvent::run_started(2));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta { text: "hm".into() });
    state.apply_event(UiEvent::ThinkingEnded);
    tool(&mut state, "z1", "read", r#"{"path":"z.rs"}"#, "ok", true);
    state.apply_event(UiEvent::AssistantDelta {
        text: "Feito.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RunCompleted { run_id: 2 });
    let rows: Vec<_> = works(&state).iter().map(|block| block.id.clone()).collect();
    assert_eq!(rows.len(), 2);
    let hidden = |row: usize| {
        let at = state
            .blocks()
            .iter()
            .position(|block| block.id == rows[row])
            .unwrap();
        state.blocks()[at + 1].id.clone()
    };
    let index = HeightIndex::build(state.blocks(), 80, &mut WrapCache::default());
    let pinned = |id| {
        FollowMode::Pinned(ScrollAnchor {
            block_id: id,
            row_offset: 0,
        })
    };
    // Resting on a block hidden under the second row: up goes to the first
    // row, down stays on the last; resting under the first: up stays, down
    // goes to the second.
    let second = index.metrics(&pinned(hidden(1)), 200);
    assert_eq!(
        second.up_anchor.map(|anchor| anchor.block_id),
        Some(rows[0].clone())
    );
    assert_eq!(
        second.down_anchor.map(|anchor| anchor.block_id),
        Some(rows[1].clone())
    );
    let first = index.metrics(&pinned(hidden(0)), 200);
    assert_eq!(
        first.up_anchor.map(|anchor| anchor.block_id),
        Some(rows[0].clone())
    );
    assert_eq!(
        first.down_anchor.map(|anchor| anchor.block_id),
        Some(rows[1].clone())
    );
    // A member of a collapsed tool group is a hidden block too.
    let mut grouped = connected();
    grouped.apply_event(UiEvent::UserMessageAdded { text: "g".into() });
    grouped.apply_event(UiEvent::run_started(1));
    for id in ["g1", "g2", "g3"] {
        tool(&mut grouped, id, "read", r#"{"path":"g.rs"}"#, "ok", true);
    }
    grouped.clock.elapsed_ms += 1_000;
    let member = grouped.blocks()[2].id.clone();
    let leader = grouped.blocks()[1].id.clone();
    let index = HeightIndex::build(grouped.blocks(), 80, &mut WrapCache::default());
    let metrics = index.metrics(&pinned(member), 200);
    assert_eq!(
        metrics.up_anchor.map(|anchor| anchor.block_id),
        Some(leader.clone())
    );
    assert_eq!(
        metrics.down_anchor.map(|anchor| anchor.block_id),
        Some(leader)
    );
}

#[test]
fn folded_blocks_do_not_count_toward_the_scrollbar() {
    // Four folded turns: 52 blocks, but only a handful of presented rows.
    let mut state = connected();
    for run in 1..=4u64 {
        state.apply_event(UiEvent::UserMessageAdded {
            text: format!("pergunta {run}"),
        });
        state.apply_event(UiEvent::run_started(run));
        for step in 0..5 {
            state.apply_event(UiEvent::ThinkingStarted);
            state.apply_event(UiEvent::ThinkingDelta { text: "hm".into() });
            state.apply_event(UiEvent::ThinkingEnded);
            tool(
                &mut state,
                &format!("{run}-{step}"),
                "read",
                &format!(r#"{{"path":"f{step}.rs"}}"#),
                "ok",
                true,
            );
        }
        state.apply_event(UiEvent::AssistantDelta {
            text: format!("resposta {run}"),
        });
        state.apply_event(UiEvent::AssistantEnded);
        state.apply_event(UiEvent::RunCompleted { run_id: run });
        state.clock.elapsed_ms += 1_000;
    }
    assert!(state.blocks().len() > 40, "{}", state.blocks().len());
    let width = 80;
    let measure = |height: u16| {
        slim_tui::runtime::measure_scrollback(&state, width, height, &mut WrapCache::default())
    };
    // A viewport that holds every presented row yet is shorter than the
    // block count, which the old lower bound took for overflow.
    let height = (20..80u16)
        .find(|height| {
            let metrics = measure(*height);
            metrics.total_rows <= metrics.viewport_rows
                && metrics.viewport_rows < state.blocks().len() as u64
        })
        .expect("a viewport that fits the folded transcript");
    let viewport = measure(height).viewport_rows as usize;
    let rendered = render(&state, width, height);
    // No scrollbar: the last column of the transcript is empty, and the
    // answers use the full width.
    for y in 0..viewport {
        let last = rendered.rows[y].chars().last().unwrap_or(' ');
        assert!(
            last == ' ' || !"┃│".contains(last),
            "row {y}: {:?}\n{}",
            rendered.rows[y],
            rendered.text()
        );
    }
}

#[test]
fn a_fence_label_takes_a_pandoc_class_and_is_measured_in_cells() {
    let tags = |info: &str| {
        let rendered = render(&answer(&format!("```{info}\nx\n```")), 70, 24);
        let y = rendered.row("│ x");
        rendered.rows[y - 1].trim_end().to_owned()
    };
    assert_eq!(tags("{.python}"), "    │ python");
    assert_eq!(tags("{.python .numberLines}"), "    │ python");
    assert_eq!(tags("{python}"), "    │ python");
    // 20 wide characters are 40 cells; the tag stops at 24.
    let wide = tags(&"字".repeat(20));
    let label = wide.trim_start().trim_start_matches("│ ");
    assert_eq!(label, "字".repeat(12), "{wide}");
    assert_eq!(unicode_width::UnicodeWidthStr::width(label), 24);
}

#[test]
fn restoring_a_long_session_folds_every_turn_in_one_pass() {
    const TURNS: usize = 3_000;
    let mut messages = Vec::new();
    for turn in 0..TURNS {
        messages.push(TranscriptMessage {
            role: TranscriptRole::User,
            text: format!("pergunta {turn}"),
        });
        for call in 0..10 {
            messages.push(TranscriptMessage {
                role: TranscriptRole::Tool {
                    batch_id: ToolBatchId(format!("b-{turn}-{call}").into()),
                    call_id: ToolCallId(format!("c-{turn}-{call}").into()),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                text: "saved".into(),
            });
        }
        messages.push(TranscriptMessage {
            role: TranscriptRole::Assistant,
            text: format!("resposta {turn}"),
        });
    }
    let mut state = AppState::new();
    let started = std::time::Instant::now();
    state.apply_event(UiEvent::SessionRestored {
        session_id: SessionId("long".into()),
        cwd: r"C:\Projects\demo".into(),
        messages,
        skill_names: Vec::new(),
    });
    let elapsed = started.elapsed();
    eprintln!("restored {TURNS} turns in {elapsed:?}");
    assert_eq!(works(&state).len(), TURNS);
    assert_eq!(state.blocks().len(), TURNS * 13);
    // Every row sits right after its prompt and ends at its own answer.
    for (turn, chunk) in state.blocks().chunks(13).enumerate().step_by(997) {
        assert!(matches!(chunk[0].kind(), BlockKind::User(_)), "{turn}");
        assert!(matches!(chunk[1].kind(), BlockKind::Work(_)), "{turn}");
        let row = turn * 13 + 1;
        assert_eq!(slim_tui::work::span_end(state.blocks(), row), row + 11);
    }
    // One pass is a few milliseconds; a pass per turn was several hundred.
    assert!(
        elapsed < std::time::Duration::from_millis(250),
        "{elapsed:?}"
    );
}
