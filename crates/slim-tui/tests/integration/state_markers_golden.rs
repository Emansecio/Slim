//! The marker and color that tell each case apart: who owns the turn and how
//! it ended, thought versus action, what a settled tool call did, and the
//! operating mode. Text goldens elsewhere pin the layout; these pin the hue.
//! The last section pins the cues that move: the running clock of a tool,
//! the streaming caret and the waiting spinner.

use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use slim_core::OperatingMode;
use slim_tui::api::{LoginProvider, ToolBatchId, ToolCallId, UiEvent};
use slim_tui::app::{AppState, FrameClock};
use slim_tui::block::{Block, BlockKind, BlockLifecycle};
use slim_tui::reducer::{reduce, Action};
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

const GREEN: Color = Color::Rgb(0x72, 0xCC, 0x91);
const AMBER: Color = Color::Rgb(0xE7, 0xC1, 0x5A);
const RED: Color = Color::Rgb(0xE8, 0x79, 0x73);
const BLUE: Color = Color::Rgb(0x8A, 0xAD, 0xD4);
const VIOLET: Color = Color::Rgb(0xA9, 0x9F, 0xD6);
const MUTED: Color = Color::Rgb(0x99, 0x97, 0x8E);
const SECONDARY: Color = Color::Rgb(0xBC, 0xB9, 0xAF);

struct Rendered {
    rows: Vec<String>,
    foreground: Vec<Vec<Color>>,
    modifiers: Vec<Vec<Modifier>>,
}

impl Rendered {
    fn row(&self, needle: &str) -> usize {
        self.rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row with {needle:?} missing\n{}", self.rows.join("\n")))
    }

    /// Column of the streaming caret: the half block that closes the message
    /// being written, past the rail of the turn. `None` while it is off.
    fn caret_column(&self, row: usize) -> Option<usize> {
        let text = self.rows[row].trim_end();
        if !text.ends_with('▏') {
            return None;
        }
        // A blank row of the turn carries only its rail, which is the same
        // glyph; the caret closes a line that has text on it.
        let body = text.trim_start_matches(['▏', ' ']).trim_end_matches('▏');
        (!body.trim().is_empty()).then(|| unicode_width::UnicodeWidthStr::width(text) - 1)
    }

    /// Foreground and modifiers at the first cell of `word` on `row`.
    fn style_at(&self, row: usize, word: &str) -> (Color, Modifier) {
        let text = &self.rows[row];
        let byte = text
            .find(word)
            .unwrap_or_else(|| panic!("{word:?} missing in {text:?}"));
        let column = unicode_width::UnicodeWidthStr::width(&text[..byte]);
        (self.foreground[row][column], self.modifiers[row][column])
    }
}

fn caps(color_depth: ColorDepth) -> Capabilities {
    Capabilities {
        color_depth,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: true,
    }
}

fn render(state: &AppState, width: u16, color_depth: ColorDepth) -> Rendered {
    render_with(state, width, caps(color_depth))
}

fn render_with(state: &AppState, width: u16, capabilities: Capabilities) -> Rendered {
    let mut terminal = Terminal::new(TestBackend::new(width, 40)).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, capabilities, &mut WrapCache::default()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rows = Vec::new();
    let mut foreground = Vec::new();
    let mut modifiers = Vec::new();
    for y in 0..buffer.area.height {
        let mut row = String::new();
        let mut row_foreground = Vec::new();
        let mut row_modifiers = Vec::new();
        let mut text_x = 0;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if x >= text_x {
                row.push_str(cell.symbol());
                text_x = x + (unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16);
            }
            row_foreground.push(cell.fg);
            row_modifiers.push(cell.modifier);
        }
        rows.push(row);
        foreground.push(row_foreground);
        modifiers.push(row_modifiers);
    }
    Rendered {
        rows,
        foreground,
        modifiers,
    }
}

fn connected() -> AppState {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::Anthropic),
        authenticated: true,
    });
    state.apply_event(UiEvent::UserMessageAdded { text: "oi".into() });
    state
}

fn finished_tool(state: &mut AppState, id: &str, name: &str, summary: &str) {
    let (batch_id, call_id) = (
        ToolBatchId(format!("b-{id}").into()),
        ToolCallId(format!("c-{id}").into()),
    );
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch_id.clone(),
        call_id: call_id.clone(),
        name: name.into(),
        arguments_summary: summary.into(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id,
        call_id,
        name: name.into(),
        success: true,
        duration_ms: 5,
    });
}

#[test]
fn the_turn_marker_carries_how_the_turn_went() {
    let turn = |lifecycle, glyph| {
        let mut state = connected();
        assert!(state.append_block(Block::new(
            "answer",
            BlockKind::Assistant("resposta".into()),
            lifecycle
        )));
        let rendered = render(&state, 60, ColorDepth::TrueColor);
        let header = rendered.row("Slim");
        let marker = rendered.style_at(header, glyph).0;
        let label = rendered.style_at(header, "Slim").0;
        (marker, label)
    };
    assert_eq!(turn(BlockLifecycle::Complete, "●"), (GREEN, GREEN));
    assert_eq!(turn(BlockLifecycle::Streaming, "●"), (GREEN, GREEN));
    // The marker turns amber or red and takes the shape tool rows use for
    // the same outcome; the name keeps a quiet tone instead of borrowing a
    // warning color.
    assert_eq!(turn(BlockLifecycle::Cancelled, "■"), (AMBER, SECONDARY));
    assert_eq!(turn(BlockLifecycle::Failed, "✕"), (RED, SECONDARY));
}

#[test]
fn failed_and_interrupted_turns_differ_from_a_finished_one_by_shape_not_only_color() {
    for (color_depth, shapes) in [
        (ColorDepth::TrueColor, ['●', '■', '✕']),
        (ColorDepth::Ansi16, ['●', '■', '✕']),
        (ColorDepth::None, ['*', '!', 'x']),
    ] {
        let markers: Vec<char> = [
            BlockLifecycle::Complete,
            BlockLifecycle::Cancelled,
            BlockLifecycle::Failed,
        ]
        .into_iter()
        .map(|lifecycle| {
            let mut state = connected();
            assert!(state.append_block(Block::new(
                "answer",
                BlockKind::Assistant("resposta".into()),
                lifecycle
            )));
            let rendered = render(&state, 60, color_depth);
            let header = &rendered.rows[rendered.row("Slim")];
            // One cell: the name stays in the fourth column, the text column.
            assert_eq!(
                header
                    .find("Slim")
                    .map(|byte| header[..byte].chars().count()),
                Some(4)
            );
            let marker = header.chars().nth(2).expect("marker");
            assert_eq!(
                unicode_width::UnicodeWidthChar::width(marker),
                Some(1),
                "{header:?}"
            );
            marker
        })
        .collect();
        assert_eq!(markers, shapes, "{color_depth:?}");
    }
}

#[test]
fn a_thought_reads_in_the_reasoning_hue_apart_from_the_tools_around_it() {
    let mut state = connected();
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "plano".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);
    finished_tool(&mut state, "1", "read", "path=src/lib.rs");
    state.clock.elapsed_ms = 10_000;
    let rendered = render(&state, 80, ColorDepth::TrueColor);

    let thought = rendered.row("Pensou");
    assert_eq!(rendered.style_at(thought, "▸").0, VIOLET);
    assert_eq!(rendered.style_at(thought, "Pensou").0, VIOLET);
    let read = rendered.row("Leu");
    assert_eq!(rendered.style_at(read, "Leu").0, MUTED);
    assert_ne!(
        rendered.style_at(thought, "Pensou").0,
        rendered.style_at(read, "Leu").0,
        "thought and action must not share a tone"
    );
}

#[test]
fn a_streaming_thought_keeps_the_blue_of_live_indicators() {
    let mut state = connected();
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "primeira linha\nsegunda linha".into(),
    });
    let rendered = render(&state, 80, ColorDepth::TrueColor);
    let header = rendered.row("Pensando");
    assert_eq!(rendered.style_at(header, "○").0, BLUE);
    // The newest words ride on the header row, italic and a step back from
    // the label's reasoning hue.
    assert_eq!(rendered.row("segunda linha"), header);
    let (color, modifiers) = rendered.style_at(header, "segunda");
    assert!(luma(color) < luma(VIOLET), "{color:?}");
    assert!(modifiers.contains(Modifier::ITALIC));
}

#[test]
fn only_an_applied_change_earns_the_green_check() {
    // Grouping only folds consecutive finished calls; prose between them
    // keeps each call on its own row.
    let mut state = connected();
    for (id, name, summary) in [
        ("1", "read", "path=src/a.rs"),
        ("2", "shell", "command=cargo test"),
        ("3", "patch", "path=src/b.rs"),
    ] {
        finished_tool(&mut state, id, name, summary);
        state.apply_event(UiEvent::AssistantDelta {
            text: format!("depois de {name}"),
        });
        state.apply_event(UiEvent::AssistantEnded);
    }
    let rendered = render(&state, 80, ColorDepth::TrueColor);

    let read = rendered.row("Leu");
    assert_eq!(
        rendered.style_at(read, "✓").0,
        MUTED,
        "a read only observes"
    );
    assert_eq!(rendered.style_at(read, "Leu").0, MUTED);

    // A command reads as itself: `$` takes the verb's slot and weight.
    let run = rendered.row("✓ $");
    assert_eq!(
        rendered.style_at(run, "✓").0,
        MUTED,
        "a command is not a change"
    );
    assert_eq!(
        rendered.style_at(run, "$").0,
        SECONDARY,
        "but it reads one step above a read"
    );

    let edit = rendered.row("Editou");
    assert_eq!(
        rendered.style_at(edit, "✓").0,
        GREEN,
        "an edit changed the tree"
    );
    assert_eq!(rendered.style_at(edit, "Editou").0, SECONDARY);
}

#[test]
fn a_folded_group_takes_the_weight_of_its_strongest_call() {
    // (row text, marker color, tone of the tally text)
    let group = |calls: &[(&str, &str)]| {
        let mut state = connected();
        for (index, (name, summary)) in calls.iter().enumerate() {
            finished_tool(&mut state, &index.to_string(), name, summary);
        }
        let rendered = render(&state, 80, ColorDepth::TrueColor);
        let row = rendered.row("✓");
        (
            rendered.rows[row].trim_end().to_owned(),
            rendered.style_at(row, "✓").0,
            rendered.style_at(row, "Leu a").0,
        )
    };
    let (text, marker, tally) = group(&[("read", "path=a"), ("search", "pattern=x")]);
    assert!(text.contains(r#"Leu a · Buscou "x""#), "{text}");
    assert_eq!((marker, tally), (MUTED, MUTED), "observing stays quiet");

    let (text, marker, tally) = group(&[("read", "path=a"), ("shell", "command=ls")]);
    assert!(text.contains("Leu a · $ ls"), "{text}");
    assert_eq!(
        (marker, tally),
        (MUTED, SECONDARY),
        "a command lifts the text but does not turn the check green"
    );

    let (text, marker, tally) = group(&[("read", "path=a"), ("patch", "path=b")]);
    assert!(text.contains("Leu a · Editou b"), "{text}");
    assert_eq!((marker, tally), (GREEN, SECONDARY), "an edit was applied");
}

#[test]
fn the_operating_mode_is_named_by_hue() {
    let footer = |mode: OperatingMode, depth: ColorDepth| {
        let mut state = connected();
        state.mode = mode;
        let rendered = render(&state, 80, depth);
        let row = (0..rendered.rows.len())
            .rev()
            .find(|y| {
                rendered.rows[*y].contains(match mode {
                    OperatingMode::Auto => "Auto ·",
                    OperatingMode::ReadOnly => "Read-only ·",
                    OperatingMode::Plan => "Plan ·",
                    OperatingMode::Code => "Code ·",
                })
            })
            .unwrap_or_else(|| panic!("mode row missing\n{}", rendered.rows.join("\n")));
        let word = match mode {
            OperatingMode::Auto => "Auto",
            OperatingMode::ReadOnly => "Read-only",
            OperatingMode::Plan => "Plan",
            OperatingMode::Code => "Code",
        };
        rendered.style_at(row, word)
    };
    let (auto, auto_modifiers) = footer(OperatingMode::Auto, ColorDepth::TrueColor);
    assert_eq!(auto, SECONDARY, "Auto is the neutral default");
    assert!(auto_modifiers.contains(Modifier::BOLD));
    assert_eq!(
        footer(OperatingMode::ReadOnly, ColorDepth::TrueColor).0,
        BLUE
    );
    assert_eq!(footer(OperatingMode::Plan, ColorDepth::TrueColor).0, VIOLET);
    // Code has its own hue: full access, but not the neutral Auto.
    let code = footer(OperatingMode::Code, ColorDepth::TrueColor);
    assert!(code.1.contains(Modifier::BOLD));
    assert_eq!(code.0, AMBER);
    // The two safe modes differ from each other and from Auto at every depth
    // that has color, and the name stays bold so it never depends on hue.
    for depth in [ColorDepth::Ansi256, ColorDepth::Ansi16] {
        let colors = [
            footer(OperatingMode::Auto, depth).0,
            footer(OperatingMode::ReadOnly, depth).0,
            footer(OperatingMode::Plan, depth).0,
        ];
        assert_ne!(colors[0], colors[1], "{depth:?}");
        assert_ne!(colors[1], colors[2], "{depth:?}");
        assert_ne!(colors[0], colors[2], "{depth:?}");
    }
    assert!(footer(OperatingMode::Plan, ColorDepth::TrueColor)
        .1
        .contains(Modifier::BOLD));
}

// -- Cues that move -------------------------------------------------------

fn moving(color_depth: ColorDepth) -> Capabilities {
    Capabilities {
        reduced_motion: false,
        ..caps(color_depth)
    }
}

fn tick(state: &mut AppState, frame: u64, elapsed_ms: u64) {
    reduce(state, Action::Tick(FrameClock { frame, elapsed_ms }));
}

fn start_tool(state: &mut AppState, id: &str, name: &str, summary: &str) {
    state.apply_event(UiEvent::ToolStarted {
        batch_id: ToolBatchId(format!("b-{id}").into()),
        call_id: ToolCallId(format!("c-{id}").into()),
        name: name.into(),
        arguments_summary: summary.into(),
    });
}

#[test]
fn a_running_tool_counts_whole_seconds_in_the_slot_its_duration_will_take() {
    let mut state = connected();
    state.apply_event(UiEvent::run_started(1));
    tick(&mut state, 0, 0);
    start_tool(&mut state, "1", "shell", "command=cargo test");
    let row_at = |state: &mut AppState, elapsed_ms: u64| {
        tick(state, elapsed_ms / 83, elapsed_ms);
        let rendered = render_with(state, 100, moving(ColorDepth::TrueColor));
        rendered.rows[rendered.row("$ cargo test")]
            .trim_end()
            .to_owned()
    };
    // Nothing in the first second, so a quick call never flashes `0s`.
    assert!(row_at(&mut state, 0).ends_with("cargo test"));
    assert!(row_at(&mut state, 999).ends_with("cargo test"));
    assert!(row_at(&mut state, 1_000).ends_with("cargo test · 1s"));
    assert!(row_at(&mut state, 12_400).ends_with("cargo test · 12s"));
    assert!(row_at(&mut state, 65_000).ends_with("cargo test · 1m05s"));

    // The clock is replaced by the real duration when the call settles.
    state.apply_event(UiEvent::ToolEnded {
        batch_id: ToolBatchId("b-1".into()),
        call_id: ToolCallId("c-1".into()),
        name: "shell".into(),
        success: true,
        duration_ms: 65_300,
    });
    tick(&mut state, 900, 75_000);
    let settled = render_with(&state, 100, moving(ColorDepth::TrueColor));
    let row = settled.rows[settled.row("$ cargo test")]
        .trim_end()
        .to_owned();
    assert!(row.ends_with("1m05s"), "{row}");
    assert!(!row.contains("1m15s"), "the running clock must stop: {row}");
}

#[test]
fn parallel_tools_each_keep_their_own_clock() {
    let mut state = connected();
    state.apply_event(UiEvent::run_started(1));
    tick(&mut state, 0, 0);
    start_tool(&mut state, "slow", "shell", "command=cargo build");
    tick(&mut state, 48, 4_000);
    start_tool(&mut state, "fast", "shell", "command=cargo fmt");
    tick(&mut state, 120, 10_000);
    let rendered = render_with(&state, 100, moving(ColorDepth::TrueColor));
    let slow = rendered.rows[rendered.row("cargo build")]
        .trim_end()
        .to_owned();
    let fast = rendered.rows[rendered.row("cargo fmt")]
        .trim_end()
        .to_owned();
    assert!(slow.ends_with("10s"), "{slow}");
    assert!(fast.ends_with("6s"), "{fast}");
    // The clock is text, not animation: reduced motion keeps it.
    let reduced = render_with(&state, 100, caps(ColorDepth::TrueColor));
    assert!(reduced.rows[reduced.row("cargo build")]
        .trim_end()
        .ends_with("10s"));
}

#[test]
fn the_streaming_caret_blinks_and_dims_when_the_provider_goes_quiet() {
    let mut state = connected();
    state.apply_event(UiEvent::run_started(1));
    tick(&mut state, 0, 0);
    state.apply_event(UiEvent::AssistantDelta {
        text: "resposta em curso".into(),
    });
    let mut text_column = None;
    // (frame, elapsed, expected caret color; None means the cell is blank)
    let phases = [
        (0, 0, Some(GREEN)),
        (5, 415, Some(GREEN)),
        (6, 498, None),
        (11, 913, None),
        (12, 996, Some(GREEN)),
        (18, 1_494, None),
        // Two seconds without content: steady and dim, whatever the phase.
        (30, 2_490, Some(MUTED)),
        (36, 2_988, Some(MUTED)),
    ];
    for (frame, elapsed_ms, expected) in phases {
        tick(&mut state, frame, elapsed_ms);
        let rendered = render_with(&state, 80, moving(ColorDepth::TrueColor));
        let row = rendered.row("resposta em curso");
        match expected {
            Some(color) => {
                let column = rendered.caret_column(row).unwrap_or_else(|| {
                    panic!("caret missing at frame {frame}: {:?}", rendered.rows[row])
                });
                assert_eq!(rendered.foreground[row][column], color, "frame {frame}");
            }
            None => assert!(
                rendered.caret_column(row).is_none(),
                "frame {frame}: {:?}",
                rendered.rows[row]
            ),
        }
        // The cell is reserved in every phase: text never moves or rewraps.
        let start = rendered.rows[row].find("resposta").unwrap();
        assert_eq!(*text_column.get_or_insert(start), start, "frame {frame}");
    }

    // New content brings the bright caret back.
    tick(&mut state, 36, 3_000);
    state.apply_event(UiEvent::AssistantDelta {
        text: " e continua".into(),
    });
    let resumed = render_with(&state, 80, moving(ColorDepth::TrueColor));
    let row = resumed.row("e continua");
    let column = resumed.caret_column(row).expect("caret back");
    assert_eq!(resumed.foreground[row][column], GREEN);
}

#[test]
fn the_caret_blinks_without_color_too_and_reduced_motion_removes_it() {
    let mut state = connected();
    state.apply_event(UiEvent::run_started(1));
    tick(&mut state, 0, 0);
    state.apply_event(UiEvent::AssistantDelta {
        text: "sem cor".into(),
    });
    let has_caret = |state: &AppState, capabilities: Capabilities| {
        // The caret closes the row the answer is being written on; the rail of
        // the turn stands at its start, in the same glyph.
        let rendered = render_with(state, 60, capabilities);
        (0..rendered.rows.len()).any(|row| rendered.caret_column(row).is_some())
    };
    tick(&mut state, 0, 0);
    assert!(has_caret(&state, moving(ColorDepth::None)));
    tick(&mut state, 6, 498);
    assert!(!has_caret(&state, moving(ColorDepth::None)));
    tick(&mut state, 12, 996);
    assert!(has_caret(&state, moving(ColorDepth::None)));
    for frame in [0, 6, 12] {
        tick(&mut state, frame, frame * 83);
        assert!(!has_caret(&state, caps(ColorDepth::None)), "reduced motion");
        assert!(
            !has_caret(&state, caps(ColorDepth::TrueColor)),
            "reduced motion"
        );
    }
}

#[test]
fn waiting_on_a_retry_or_a_cancellation_steps_once_a_second() {
    let mut state = connected();
    state.apply_event(UiEvent::run_started(1));
    tick(&mut state, 0, 1_000);
    state.apply_event(UiEvent::RetryScheduled {
        attempt: 2,
        limit: 4,
        wait_ms: 30_000,
        reason: Some("429".into()),
    });
    let marker = |state: &mut AppState, frame: u64, elapsed_ms: u64, capabilities| {
        tick(state, frame, elapsed_ms);
        let rendered = render_with(state, 80, capabilities);
        let row = rendered.row("Nova tentativa");
        rendered.rows[row].trim_start().chars().next().unwrap()
    };
    let moving_caps = moving(ColorDepth::TrueColor);
    // One step per second, following the status tick...
    let per_second: Vec<char> = (1..=5)
        .map(|second| marker(&mut state, second * 12, second * 1_000, moving_caps))
        .collect();
    assert_eq!(per_second, ['⠹', '⠼', '⠧', '⠋', '⠹']);
    // ...and nothing changes between two ticks of the 83 ms motion clock.
    let early = marker(&mut state, 60, 5_010, moving_caps);
    let late = marker(&mut state, 66, 5_600, moving_caps);
    assert_eq!(early, late);
    // Without color the same steps are ASCII; with reduced motion the
    // original static retry marker returns.
    assert_eq!(marker(&mut state, 60, 5_000, moving(ColorDepth::None)), '/');
    assert_eq!(marker(&mut state, 72, 6_000, moving(ColorDepth::None)), '-');
    assert_eq!(
        marker(&mut state, 60, 5_000, caps(ColorDepth::TrueColor)),
        '↻'
    );

    state.apply_event(UiEvent::CancellationRequested { run_id: 1 });
    let cancelling = |state: &mut AppState, elapsed_ms: u64| {
        tick(state, elapsed_ms / 83, elapsed_ms);
        let rendered = render_with(state, 80, moving(ColorDepth::TrueColor));
        let row = rendered.row("Interrupção solicitada");
        rendered.rows[row].trim_start().chars().next().unwrap()
    };
    assert_ne!(cancelling(&mut state, 7_000), cancelling(&mut state, 8_000));
}

// -- A thought in progress ---------------------------------------------------

/// Perceived brightness of a truecolor cell.
fn luma(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => 2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b),
        other => panic!("expected a truecolor cell, got {other:?}"),
    }
}

fn thinking(text: &str) -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::run_started(1));
    tick(&mut state, 0, 0);
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta { text: text.into() });
    state
}

#[test]
fn the_thinking_label_rests_in_the_reasoning_hue_with_no_sweep() {
    let mut state = thinking("plano");
    // The runtime clock: one motion frame every 83 ms.
    let label = |state: &mut AppState, elapsed_ms: u64, capabilities: Capabilities| {
        tick(state, elapsed_ms / 83, elapsed_ms);
        state.last_provider_content_ms = Some(elapsed_ms.saturating_sub(1_000));
        let rendered = render_with(state, 80, capabilities);
        let row = rendered.row("Pensando");
        // "  ⠋ Pensando · 3s": the label fills columns 4..12, the clock follows.
        let label: Vec<Color> = (4..16).map(|c| rendered.foreground[row][c]).collect();
        let text = rendered.rows[row][rendered.rows[row].find("Pensando").unwrap()..].to_owned();
        (label, text, rendered.modifiers[row][4..12].to_vec())
    };

    // The spinner beside the label is the row's only motion: whatever the
    // frame, the label and its clock keep the plain reasoning hue.
    for elapsed_ms in [4_648, 4_814, 4_897, 5_146, 5_644] {
        for capabilities in [moving(ColorDepth::TrueColor), caps(ColorDepth::TrueColor)] {
            let (label, text, _) = label(&mut state, elapsed_ms, capabilities);
            assert!(
                label.iter().all(|color| *color == VIOLET),
                "{elapsed_ms} ms: {label:?}"
            );
            assert!(text.starts_with("Pensando · "), "{text}");
        }
    }

    // Without color nothing takes weight in turns either.
    for elapsed_ms in [4_648, 5_146] {
        let (_, _, modifiers) = label(&mut state, elapsed_ms, moving(ColorDepth::None));
        assert!(
            modifiers
                .iter()
                .all(|modifier| !modifier.contains(Modifier::BOLD)),
            "{modifiers:?}"
        );
    }
}

#[test]
fn a_long_thought_stays_one_row_with_its_newest_words() {
    use slim_tui::render::{HeightIndex, WrapCache};
    let state = thinking(&format!(
        "{}fim do raciocínio",
        "uma frase longa ".repeat(40)
    ));
    // Reduced motion: no glow, so the row shows its resting shades.
    let rendered = render_with(&state, 50, caps(ColorDepth::TrueColor));
    let row = rendered.row("Pensando");
    let text = rendered.rows[row].trim_end();
    assert!(text.contains("Pensando · …"), "{text}");
    assert!(text.ends_with("fim do raciocínio"), "{text}");
    let (tail, modifiers) = rendered.style_at(row, "fim");
    assert!(luma(tail) < luma(VIOLET), "{tail:?}");
    assert!(modifiers.contains(Modifier::ITALIC));
    // However long the thought, it measures as many rows as a short one.
    let rows = HeightIndex::build(state.blocks(), 50, &mut WrapCache::default()).total_rows;
    let short = thinking("plano");
    let short_rows = HeightIndex::build(short.blocks(), 50, &mut WrapCache::default()).total_rows;
    assert_eq!(rows, short_rows, "the thought is one measured row");
}

#[test]
fn an_arriving_thought_keeps_its_resting_shade_from_the_first_frame() {
    let mut state = thinking("");
    tick(&mut state, 12, 1_000);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "primeira linha do pensamento que acabou de chegar".into(),
    });
    let edge = |state: &mut AppState, frame: u64, elapsed_ms: u64, capabilities: Capabilities| {
        tick(state, frame, elapsed_ms);
        let rendered = render_with(state, 100, capabilities);
        let row = rendered.row("chegar");
        let end = rendered.rows[row].trim_end().chars().count() - 1;
        let start = rendered.rows[row].find("primeira").unwrap();
        let start = rendered.rows[row][..start].chars().count();
        (
            rendered.foreground[row][end],
            rendered.foreground[row][start],
            // The spinner beside the label moves; the words never do.
            rendered.rows[row][rendered.rows[row].find("Pensando").unwrap()..]
                .trim_end()
                .to_owned(),
        )
    };

    let (resting, resting_start, text) = edge(&mut state, 12, 1_000, caps(ColorDepth::TrueColor));
    assert_eq!(resting, resting_start);
    // Words that have just arrived are not lit: the row reads the same the
    // moment it is written and half a second later, with or without motion.
    for (frame, elapsed_ms) in [(12, 1_000), (15, 1_250), (18, 1_500)] {
        let (newest, first, same_text) =
            edge(&mut state, frame, elapsed_ms, moving(ColorDepth::TrueColor));
        assert_eq!(newest, resting, "{elapsed_ms} ms");
        assert_eq!(first, resting, "{elapsed_ms} ms");
        assert_eq!(same_text, text);
    }
}

#[test]
fn a_finished_sentence_heads_the_row_upright_and_unlit() {
    let mut state = thinking("");
    tick(&mut state, 12, 1_000);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "Okay, vou conferir o cache de linhas. Depois leio o render".into(),
    });
    let rendered = render_with(&state, 100, moving(ColorDepth::TrueColor));
    let row = rendered.row("Pensando");
    let text = rendered.rows[row].trim_end();
    // The sentence still being written waits; the finished one stands,
    // without its empty opener.
    assert!(
        text.ends_with(" · Vou conferir o cache de linhas"),
        "{text}"
    );
    assert!(!text.contains("Depois"), "{text}");
    let (color, modifiers) = rendered.style_at(row, "Vou");
    assert_eq!(color, Color::Rgb(0xBC, 0xB9, 0xAF));
    assert!(!modifiers.contains(Modifier::ITALIC));
    // Content has just arrived, but the row holds no words arriving: no glow.
    let end = text.chars().count() - 1;
    assert_eq!(rendered.foreground[row][end], color);
}

// -- The agent is present from the moment the prompt is sent -----------------

fn prompt_sent() -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::run_started(1));
    tick(&mut state, 0, 0);
    state
}

fn header_rows(rendered: &Rendered) -> Vec<usize> {
    (0..rendered.rows.len())
        .filter(|row| rendered.rows[*row].starts_with("▏ ● Slim ─"))
        .collect()
}

#[test]
fn the_agent_header_waits_under_the_prompt_before_any_agent_output() {
    let state = prompt_sent();
    let rendered = render_with(&state, 80, caps(ColorDepth::TrueColor));
    let prompt = rendered.row("● oi");
    // The prompt beside its marker, the breathing row, then the agent's header.
    assert_eq!(
        header_rows(&rendered),
        vec![prompt + 2],
        "{}",
        rendered.rows.join("\n")
    );
    assert_eq!(rendered.style_at(prompt + 2, "●").0, GREEN);
    assert_eq!(rendered.style_at(prompt + 2, "Slim").0, GREEN);
}

#[test]
fn the_pending_header_becomes_the_first_blocks_header_without_moving_anything() {
    use slim_tui::render::{HeightIndex, WrapCache};
    let mut state = prompt_sent();
    let before = render_with(&state, 80, caps(ColorDepth::TrueColor));
    let rows_before = HeightIndex::build(state.blocks(), 80, &mut WrapCache::default()).total_rows;
    let prompt_before = before.row("● oi");
    let header_offset = header_rows(&before)[0] - prompt_before;

    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "plano".into(),
    });
    let after = render_with(&state, 80, caps(ColorDepth::TrueColor));
    // The screen is anchored to the bottom, so absolute rows shift as content
    // grows; the header stays the same distance under its prompt.
    let prompt_after = after.row("● oi");
    assert_eq!(
        header_rows(&after),
        vec![prompt_after + header_offset],
        "one header, same place under the prompt"
    );
    let rows_after = HeightIndex::build(state.blocks(), 80, &mut WrapCache::default()).total_rows;
    // The header is now the block's (label and rule on one row); only the
    // thought's own row is new.
    assert_eq!(
        rows_after,
        rows_before + 1,
        "header row is not counted twice"
    );
    assert!(
        after.rows[prompt_after + header_offset + 1].contains("Pensando"),
        "the thought follows the header row"
    );
}

#[test]
fn no_pending_header_when_nothing_is_running_or_the_run_ends_empty() {
    // A prompt with no run.
    let mut idle = connected();
    idle.apply_event(UiEvent::UserMessageAdded {
        text: "outro".into(),
    });
    assert!(header_rows(&render_with(&idle, 80, caps(ColorDepth::TrueColor))).is_empty());

    // The run fails before the agent says anything: the header goes away.
    let mut state = prompt_sent();
    assert_eq!(
        header_rows(&render_with(&state, 80, caps(ColorDepth::TrueColor))).len(),
        1
    );
    state.apply_event(UiEvent::RunFailed {
        run_id: Some(1),
        message: "provider indisponível".into(),
    });
    let failed = render_with(&state, 80, caps(ColorDepth::TrueColor));
    assert!(
        header_rows(&failed).is_empty(),
        "{}",
        failed.rows.join("\n")
    );
    assert!(failed
        .rows
        .iter()
        .any(|row| row.contains("provider indisponível")));

    // A restored session shows history, not a pending run.
    let mut restored = AppState::new();
    restored.apply_event(UiEvent::SessionRestored {
        session_id: slim_tui::api::SessionId("s".into()),
        cwd: r"C:\projeto".into(),
        messages: vec![slim_tui::api::TranscriptMessage {
            role: slim_tui::api::TranscriptRole::User,
            text: "antigo".into(),
        }],
        skill_names: Vec::new(),
    });
    assert!(header_rows(&render_with(&restored, 80, caps(ColorDepth::TrueColor))).is_empty());
}

#[test]
fn the_pending_name_stays_still_in_the_identity_green() {
    let mut state = prompt_sent();
    let cells = |state: &mut AppState, frame: u64, capabilities: Capabilities| {
        tick(state, frame, frame * 83);
        let rendered = render_with(state, 80, capabilities);
        let row = header_rows(&rendered)[0];
        (
            (4..8)
                .map(|column| rendered.foreground[row][column])
                .collect::<Vec<_>>(),
            rendered.foreground[row][2],
            rendered.rows[row].trim_end().to_owned(),
        )
    };
    // The footer says the run is alive; the name is not swept while it waits.
    let (_, _, resting) = cells(&mut state, 0, caps(ColorDepth::TrueColor));
    for frame in [0, 4, 6, 8, 10, 12] {
        for capabilities in [moving(ColorDepth::TrueColor), caps(ColorDepth::TrueColor)] {
            let (name, marker, text) = cells(&mut state, frame, capabilities);
            assert!(name.iter().all(|color| *color == GREEN), "frame {frame}");
            assert_eq!(marker, GREEN, "the marker stays the identity green");
            assert_eq!(text, resting, "frame {frame}");
        }
    }
}

// -- The receipt that closes a turn -------------------------------------------

fn edited(state: &mut AppState, id: &str, path: &str, added: usize, removed: usize) {
    start_tool(state, id, "patch", &format!("path={path}"));
    state.apply_event(UiEvent::ToolDiff {
        batch_id: ToolBatchId(format!("b-{id}").into()),
        call_id: ToolCallId(format!("c-{id}").into()),
        diff: slim_core::ToolEditDiff {
            path: path.into(),
            hunks: vec![slim_core::ToolEditHunk {
                start_line: 1,
                removed: vec!["antes".into(); removed],
                added: vec!["depois".into(); added],
            }],
            truncated: false,
        },
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: ToolBatchId(format!("b-{id}").into()),
        call_id: ToolCallId(format!("c-{id}").into()),
        name: "patch".into(),
        success: true,
        duration_ms: 3,
    });
}

fn ran(state: &mut AppState, id: &str, success: bool) {
    start_tool(state, id, "shell", "command=cargo test");
    state.apply_event(UiEvent::ToolEnded {
        batch_id: ToolBatchId(format!("b-{id}").into()),
        call_id: ToolCallId(format!("c-{id}").into()),
        name: "shell".into(),
        success,
        duration_ms: 40,
    });
}

fn receipt_row(rendered: &Rendered) -> Option<usize> {
    // The last such row: a folded group of tools reads similarly, above it.
    (0..rendered.rows.len()).rev().find(|row| {
        let text = rendered.rows[*row]
            .trim_start()
            .trim_start_matches('▏')
            .trim_start();
        ["✓ ", "✕ ", "■ "]
            .iter()
            .any(|glyph| text.starts_with(glyph))
            && (text.contains("arquivo") || text.contains("comando"))
    })
}

#[test]
fn a_turn_that_changed_files_ends_with_a_receipt() {
    let mut state = prompt_sent();
    edited(&mut state, "1", "src/a.rs", 2, 1);
    edited(&mut state, "2", "src/b.rs", 0, 3);
    ran(&mut state, "3", true);
    state.apply_event(UiEvent::AssistantDelta {
        text: "Pronto.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    tick(&mut state, 72, 6_000);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    let rendered = render_with(&state, 100, caps(ColorDepth::TrueColor));
    let row = receipt_row(&rendered).expect("receipt row");
    assert_eq!(
        rendered.rows[row].trim_end(),
        "▏ ✓ 2 arquivos · +2 -4   Ctrl+D diff"
    );
    // The receipt sits in the text column, set apart from the prose above it.
    assert!(
        rendered.rows[row - 1].trim().is_empty(),
        "{}",
        rendered.rows.join("\n")
    );
    assert_eq!(rendered.style_at(row, "✓").0, GREEN);
    assert_eq!(rendered.style_at(row, "+2").0, GREEN);
    assert_eq!(rendered.style_at(row, "-4").0, RED);
    assert_eq!(rendered.style_at(row, "arquivos").0, MUTED);
    assert_eq!(rendered.style_at(row, "Ctrl+D").0, MUTED);
}

#[test]
fn the_marker_follows_how_the_turn_ended() {
    let marker = |build: fn(&mut AppState), finish: UiEvent| {
        let mut state = prompt_sent();
        build(&mut state);
        tick(&mut state, 12, 1_000);
        state.apply_event(finish);
        let rendered = render_with(&state, 100, caps(ColorDepth::TrueColor));
        let row = receipt_row(&rendered).expect("receipt row");
        let glyph = rendered.rows[row]
            .trim_start()
            .trim_start_matches('▏')
            .trim_start()
            .chars()
            .next()
            .unwrap();
        (
            glyph,
            rendered.style_at(row, &glyph.to_string()).0,
            rendered.rows[row].trim_end().to_owned(),
        )
    };
    // The last command failed: red, even though a file changed.
    let (glyph, color, text) = marker(
        |state| {
            edited(state, "1", "a.rs", 1, 0);
            ran(state, "2", false);
        },
        UiEvent::RunCompleted { run_id: 1 },
    );
    assert_eq!((glyph, color), ('✕', RED), "{text}");
    assert!(text.contains("último comando falhou"), "{text}");
    // Only commands: nothing changed, so there is no result to close with.
    let mut commands = prompt_sent();
    ran(&mut commands, "1", true);
    tick(&mut commands, 12, 1_000);
    commands.apply_event(UiEvent::RunCompleted { run_id: 1 });
    assert!(receipt_row(&render_with(&commands, 100, caps(ColorDepth::TrueColor))).is_none());
    // Interrupted: amber, and the word says so.
    let (glyph, color, text) = marker(
        |state| edited(state, "1", "a.rs", 1, 0),
        UiEvent::RunCancelled { run_id: 1 },
    );
    assert_eq!((glyph, color), ('■', AMBER), "{text}");
    assert!(text.contains("interrompido · 1 arquivo"), "{text}");
}

#[test]
fn a_turn_that_only_read_or_failed_has_no_receipt() {
    let mut reading = prompt_sent();
    finished_tool(&mut reading, "1", "read", "path=x.rs");
    reading.apply_event(UiEvent::RunCompleted { run_id: 1 });
    assert!(receipt_row(&render_with(&reading, 100, caps(ColorDepth::TrueColor))).is_none());

    let mut failed = prompt_sent();
    edited(&mut failed, "1", "a.rs", 1, 1);
    failed.apply_event(UiEvent::RunFailed {
        run_id: Some(1),
        message: "provider caiu".into(),
    });
    let rendered = render_with(&failed, 100, caps(ColorDepth::TrueColor));
    assert!(
        receipt_row(&rendered).is_none(),
        "the error block already closes the turn"
    );
}

#[test]
fn a_narrow_receipt_drops_pieces_from_the_right_and_never_overflows() {
    let mut state = prompt_sent();
    edited(&mut state, "1", "src/a.rs", 12, 7);
    ran(&mut state, "2", true);
    tick(&mut state, 72, 6_000);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    for width in [60u16, 40, 30, 24] {
        let rendered = render_with(&state, width, caps(ColorDepth::TrueColor));
        let row = receipt_row(&rendered).unwrap_or_else(|| panic!("no receipt at {width}"));
        let text = rendered.rows[row].trim_end();
        assert!(
            unicode_width::UnicodeWidthStr::width(text) <= usize::from(width),
            "{width}: {text:?}"
        );
        assert!(text.starts_with("▏ ✓ 1 arquivo"), "{width}: {text:?}");
    }
    let wide = render_with(&state, 100, caps(ColorDepth::TrueColor));
    assert!(wide.rows[receipt_row(&wide).unwrap()].contains("Ctrl+D"));
    let narrow = render_with(&state, 30, caps(ColorDepth::TrueColor));
    assert!(!narrow.rows[receipt_row(&narrow).unwrap()].contains("Ctrl+D"));
}

#[test]
fn the_diff_inspector_lists_each_file_with_its_lines_and_the_diff_event_rides_the_data_lane() {
    let mut state = prompt_sent();
    edited(&mut state, "1", "src/a.rs", 2, 1);
    edited(&mut state, "2", "src/a.rs", 3, 0);
    start_tool(&mut state, "3", "write", "path=notas.md");
    state.apply_event(UiEvent::ToolEnded {
        batch_id: ToolBatchId("b-3".into()),
        call_id: ToolCallId("c-3".into()),
        name: "write".into(),
        success: true,
        duration_ms: 2,
    });
    state.inspector.active = Some(slim_tui::inspector::InspectorKind::Diff);
    let rendered = render_with(&state, 140, caps(ColorDepth::TrueColor));
    let text = rendered.rows.join("\n");
    assert!(text.contains("Arquivos"), "{text}");
    let a = rendered.row("src/a.rs  ");
    assert!(
        rendered.rows[a].contains("~+5 -1") || rendered.rows[a].contains("+5 -1"),
        "{}",
        rendered.rows[a]
    );
    assert!(
        rendered.rows.iter().any(|row| row.contains("notas.md")),
        "{text}"
    );

    // A diff must never overtake the ToolStarted that precedes it: it belongs
    // to the data lane, which the coalescer flushes in order.
    let diff = UiEvent::ToolDiff {
        batch_id: ToolBatchId("b".into()),
        call_id: ToolCallId("c".into()),
        diff: slim_core::ToolEditDiff::default(),
    };
    assert!(!diff.is_control());
}

fn preparing(state: &mut AppState, label: &str, elapsed_ms: u64) {
    state.apply_event(UiEvent::ProviderPhaseChanged {
        phase: slim_core::ProviderPhase::PreparingTool,
        label: label.into(),
        elapsed_ms,
    });
}

#[test]
fn a_call_still_being_written_shows_its_size_and_keeps_one_phase_clock() {
    let mut state = prompt_sent();
    tick(&mut state, 12, 1_000);
    preparing(&mut state, "Preparing tool · write · 27 B", 0);
    let started = state.activity.as_ref().expect("activity").started_ms;
    let rendered = render_with(&state, 100, caps(ColorDepth::TrueColor));
    assert!(
        rendered
            .rows
            .iter()
            .any(|row| row.contains("Preparando edição · 27 B")),
        "{}",
        rendered.rows.join("\n")
    );

    // The size grows for several seconds: same phase, same start, no new
    // timeline entry per KiB.
    let entries = state.activity_timeline.len();
    tick(&mut state, 60, 5_000);
    preparing(&mut state, "Preparing tool · write · 3,0 KB", 0);
    preparing(&mut state, "Preparing tool · write · 9,4 KB", 0);
    assert_eq!(state.activity.as_ref().unwrap().started_ms, started);
    assert_eq!(state.activity_timeline.len(), entries);
    let rendered = render_with(&state, 100, caps(ColorDepth::TrueColor));
    assert!(
        rendered
            .rows
            .iter()
            .any(|row| row.contains("Preparando edição · 9,4 KB")),
        "{}",
        rendered.rows.join("\n")
    );
    assert!(
        !rendered.rows.iter().any(|row| row.contains("3,0 KB")),
        "the previous size is replaced, not stacked"
    );
}

#[test]
fn a_different_tool_is_a_new_phase_and_the_final_announcement_drops_the_size() {
    let mut state = prompt_sent();
    tick(&mut state, 12, 1_000);
    preparing(&mut state, "Preparing tool · write · 3,0 KB", 0);
    let started = state.activity.as_ref().unwrap().started_ms;
    tick(&mut state, 24, 2_000);
    // The held call is announced at the end of the stream, without a size.
    preparing(&mut state, "Preparing tool · write", 0);
    let rendered = render_with(&state, 100, caps(ColorDepth::TrueColor));
    assert!(rendered
        .rows
        .iter()
        .any(|row| row.contains("Preparando edição")));
    assert!(
        !rendered.rows.iter().any(|row| row.contains("KB")),
        "{}",
        rendered.rows.join("\n")
    );
    assert_eq!(
        state.activity.as_ref().unwrap().started_ms,
        started,
        "same tool: still the same phase"
    );
    // Another tool starts a fresh phase and clock.
    preparing(&mut state, "Preparing tool · shell · 120 B", 0);
    assert!(state.activity.as_ref().unwrap().started_ms > started);
    let rendered = render_with(&state, 100, caps(ColorDepth::TrueColor));
    assert!(rendered
        .rows
        .iter()
        .any(|row| row.contains("Preparando comando · 120 B")));
}

#[test]
fn text_arriving_while_the_call_is_written_does_not_bounce_the_phase() {
    let mut state = prompt_sent();
    tick(&mut state, 12, 1_000);
    preparing(&mut state, "Preparing tool · write · 3,0 KB", 0);
    state.apply_event(UiEvent::AssistantDelta {
        text: "Vou editar".into(),
    });
    let rendered = render_with(&state, 100, caps(ColorDepth::TrueColor));
    assert!(
        rendered
            .rows
            .iter()
            .any(|row| row.contains("Preparando edição · 3,0 KB")),
        "{}",
        rendered.rows.join("\n")
    );
}

#[test]
fn a_program_shows_each_call_it_made_with_its_verb_and_diff() {
    let mut state = connected();
    state.mode = OperatingMode::Code;
    let batch = ToolBatchId("b-code".into());
    let started = |state: &mut AppState, id: &str, name: &str, summary: &str| {
        state.apply_event(UiEvent::ToolStarted {
            batch_id: batch.clone(),
            call_id: ToolCallId(id.into()),
            name: name.into(),
            arguments_summary: summary.into(),
        });
    };
    started(
        &mut state,
        "code",
        "codemode",
        "const files = await slim.list({});…",
    );
    started(&mut state, "code:1", "read", "src/a.rs");
    state.apply_event(UiEvent::ToolStepEnded {
        batch_id: batch.clone(),
        call_id: ToolCallId("code:1".into()),
        name: "read".into(),
        success: true,
        duration_ms: 2,
        output: "fn a() {}".into(),
        diff: None,
    });
    started(&mut state, "code:2", "patch", "src/a.rs · +1 -1");
    state.apply_event(UiEvent::ToolStepEnded {
        batch_id: batch.clone(),
        call_id: ToolCallId("code:2".into()),
        name: "patch".into(),
        success: true,
        duration_ms: 4,
        output: "patched".into(),
        diff: Some(slim_core::ToolEditDiff {
            path: "src/a.rs".into(),
            hunks: vec![slim_core::ToolEditHunk {
                start_line: 1,
                removed: vec!["fn a() {}".into()],
                added: vec!["fn a() -> u8 { 1 }".into()],
            }],
            truncated: false,
        }),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch.clone(),
        call_id: ToolCallId("code".into()),
        name: "codemode".into(),
        success: true,
        duration_ms: 9,
    });
    let screen = render(&state, 80, ColorDepth::TrueColor).rows.join("\n");
    assert!(
        screen.contains("✓ 1 programa, 1 leitura, 1 edição"),
        "a settled program folds its calls under one tally\n{screen}"
    );
    let program = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Tool(tool) if tool.name == "codemode"))
        .expect("program row")
        .id
        .clone();
    reduce(&mut state, Action::ToggleBlock(program));
    let screen = render(&state, 80, ColorDepth::TrueColor).rows.join("\n");
    assert!(screen.contains("Executou programa"), "{screen}");
    assert!(screen.contains("Leu src/a.rs"), "{screen}");
    assert!(screen.contains("Editou src/a.rs"), "{screen}");
    let edit = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), BlockKind::Tool(tool) if tool.name == "patch"))
        .expect("edit row")
        .id
        .clone();
    reduce(&mut state, Action::ToggleBlock(edit));
    let screen = render(&state, 80, ColorDepth::TrueColor).rows.join("\n");
    assert!(
        screen.contains("fn a() -> u8 { 1 }"),
        "the edit opens on its diff\n{screen}"
    );
}
