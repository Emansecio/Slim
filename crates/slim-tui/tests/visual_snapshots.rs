//! Manual visual review: renders canonical scenes through the production
//! renderer and writes one colored HTML page per scene, so grey levels,
//! surfaces and weights can be compared in a browser. No terminal, provider
//! or extra dependency is involved; text goldens stay the regression gate.
//!
//! `cargo test -p slim-tui --test visual_snapshots -- --ignored`
//! writes `target/tui-snapshots/*.html` (override with `SLIM_SNAPSHOT_DIR`).

use std::fmt::Write as _;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Cell;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use slim_core::{EventKind, SessionEvent};
use slim_tui::api::{LoginProvider, SessionId, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action};
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn project(state: &mut AppState, kind: EventKind) {
    if let Some(event) = UiEvent::from_core(SessionEvent::new(1, kind)) {
        state.apply_event(event);
    }
}

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
        session_id: SessionId("snapshot".into()),
        cwd: r"C:\Projects\demo".into(),
        skill_names: Vec::new(),
    });
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::Anthropic),
        authenticated: true,
    });
    state
}

fn session(running: bool) -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "O parser falha com CRLF. Pode investigar e corrigir?".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "Preciso ler o parser e o teste que falha antes de mexer.".into(),
    });
    state.clock.elapsed_ms += 2_000;
    state.apply_event(UiEvent::ThinkingEnded);
    state.apply_event(UiEvent::AssistantEnded);
    tool(
        &mut state,
        "1",
        "read",
        r#"{"path":"src/parser.rs"}"#,
        "fn parse()",
        true,
    );
    state.apply_event(UiEvent::AssistantDelta {
        text: "A causa: `split('\\n')` deixa o `\\r` no fim de cada linha.".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    tool(
        &mut state,
        "2",
        "patch",
        r#"{"path":"src/parser.rs","edits":[{"expected":"fn split(t: &str) {\n    t.split('\\n')\n}","replacement":"fn split(t: &str) {\n    // lines() also drops the CR of CRLF.\n    t.lines()\n}"}]}"#,
        "patched src/parser.rs:3; replaced 30 bytes with 70 bytes; bytes=90; sha256=ab; do not re-read",
        true,
    );
    tool(
        &mut state,
        "3",
        "shell",
        r#"{"command":"cargo test parser"}"#,
        "exit 101\nstderr:\nerror: test failed",
        false,
    );
    tool(
        &mut state,
        "4",
        "shell",
        r#"{"command":"cargo test parser"}"#,
        "exit 0\nstdout:\ntest result: ok",
        true,
    );
    state.apply_event(UiEvent::AssistantDelta {
        text: "## Correção\n\nTroquei `split('\\n')` por `lines()`.\n\n- **Arquivo:** `src/parser.rs`\n- **Testes:** passaram\n\n```rust\nfn split(t: &str) -> Lines<'_> {\n    t.lines()\n}\n```\n".into(),
    });
    if running {
        state.apply_event(UiEvent::AssistantEnded);
        project(
            &mut state,
            EventKind::ToolStarted {
                batch_id: "batch-5".into(),
                call_id: "call-5".into(),
                name: "shell".into(),
                arguments: r#"{"command":"cargo clippy --all-targets"}"#.into(),
            },
        );
        state.clock.elapsed_ms += 1_500;
    } else {
        state.apply_event(UiEvent::AssistantEnded);
        state.apply_event(UiEvent::RunCompleted { run_id: 1 });
        state.clock.elapsed_ms += 10_000;
    }
    state
}

fn key(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) {
    reduce(state, Action::Key(KeyEvent::new(code, modifiers)));
}

fn scenes() -> Vec<(&'static str, AppState, u16, u16)> {
    let mut thinking = connected();
    thinking.apply_event(UiEvent::UserMessageAdded {
        text: "Refatore o cache para LRU com peso por bytes.".into(),
    });
    thinking.apply_event(UiEvent::run_started(1));
    thinking.apply_event(UiEvent::ThinkingStarted);
    thinking.apply_event(UiEvent::ThinkingDelta {
        text: "O cache atual conta entradas, não bytes. Preciso ver quem chama insert e se o tamanho é conhecido no ponto de inserção antes de escolher entre o crate lru e um wrapper.".into(),
    });
    thinking.clock.elapsed_ms += 3_400;

    let mut palette = session(false);
    key(&mut palette, KeyCode::Char('p'), KeyModifiers::CONTROL);
    let mut slash = session(false);
    key(&mut slash, KeyCode::Char('/'), KeyModifiers::NONE);
    key(&mut slash, KeyCode::Char('m'), KeyModifiers::NONE);
    let mut model = session(false);
    key(&mut model, KeyCode::Char('l'), KeyModifiers::CONTROL);

    vec![
        ("welcome", connected(), 120, 30),
        ("session-running", session(true), 120, 44),
        ("session-done-80", session(false), 80, 40),
        ("thinking-streaming", thinking, 120, 24),
        ("palette", palette, 120, 40),
        ("slash", slash, 120, 40),
        ("model", model, 120, 40),
    ]
}

fn hex(color: Color, fallback: &str) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => fallback.to_owned(),
    }
}

fn cell_style(cell: &Cell) -> String {
    let (mut fg, mut bg) = (hex(cell.fg, "#e8e5db"), hex(cell.bg, "#000000"));
    if cell.modifier.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut style = format!("color:{fg};background:{bg}");
    for (modifier, css) in [
        (Modifier::BOLD, ";font-weight:bold"),
        (Modifier::ITALIC, ";font-style:italic"),
        (Modifier::DIM, ";opacity:.55"),
        (Modifier::UNDERLINED, ";text-decoration:underline"),
    ] {
        if cell.modifier.contains(modifier) {
            style.push_str(css);
        }
    }
    style
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn html(name: &str, state: &AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    let mut cache = WrapCache::default();
    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: true,
    };
    terminal
        .draw(|frame| render_frame(frame, state, capabilities, &mut cache))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut page = format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title>\
         <body style=\"background:#1b1b1b;color:#ddd;font-family:sans-serif\">\
         <p>{name} · {width}×{height}</p>\
         <pre style=\"font-family:'Cascadia Mono',Consolas,monospace;font-size:14px;\
         line-height:1.25;display:inline-block;margin:0\">"
    );
    for y in 0..height {
        // Adjacent cells with one style share a span to keep pages small.
        let mut run = String::new();
        let mut run_style = String::new();
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let style = cell_style(cell);
            if style != run_style && !run.is_empty() {
                let _ = write!(page, "<span style=\"{run_style}\">{}</span>", escape(&run));
                run.clear();
            }
            run_style = style;
            run.push_str(if cell.symbol().is_empty() {
                " "
            } else {
                cell.symbol()
            });
        }
        let _ = write!(
            page,
            "<span style=\"{run_style}\">{}</span>\n",
            escape(&run)
        );
    }
    page.push_str("</pre></body>");
    page
}

#[test]
#[ignore = "manual visual review; writes HTML pages under target/tui-snapshots"]
fn write_visual_snapshots() {
    let directory = std::env::var_os("SLIM_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tui-snapshots")
        });
    std::fs::create_dir_all(&directory).expect("snapshot directory");
    for (name, state, width, height) in scenes() {
        let page = html(name, &state, width, height);
        assert!(page.contains("<span"), "{name} rendered nothing");
        let path = directory.join(format!("{name}.html"));
        std::fs::write(&path, page).expect("write snapshot");
        eprintln!("{}", path.display());
    }
}
