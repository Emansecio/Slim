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
use slim_core::{EventKind, OperatingMode, SessionEvent};
use slim_tui::api::{LoginProvider, SessionId, UiEvent};
use slim_tui::app::{AppState, FrameClock};
use slim_tui::block::{Block, BlockKind, BlockLifecycle};
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

/// A long agent turn: thinking, grouped reads, prose between tool runs, a
/// failed check and a structured final answer. This is the scene that shows
/// how the agent's work and words are organized on screen.
fn agent_turn() -> AppState {
    let mut state = connected();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "O parser quebra com CRLF e a lista de erros sai duplicada. Corrija os dois problemas e cubra com testes.".into(),
    });
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "Dois sintomas, possivelmente uma causa só. Vou ler o parser e os testes antes de mexer.".into(),
    });
    state.clock.elapsed_ms += 3_200;
    state.apply_event(UiEvent::ThinkingEnded);
    for (id, name, arguments, output) in [
        ("1", "read", r#"{"path":"src/parser.rs"}"#, "fn parse()"),
        ("2", "read", r#"{"path":"src/errors.rs"}"#, "fn collect()"),
        ("3", "read", r#"{"path":"tests/parser.rs"}"#, "#[test]"),
        ("4", "search", r#"{"pattern":"split\\("}"#, "3 matches"),
    ] {
        tool(&mut state, id, name, arguments, output, true);
    }
    state.apply_event(UiEvent::AssistantDelta {
        text: "Encontrei a causa. Há dois pontos, ligados entre si:\n\n1. `parser.rs` divide por `\\n` e deixa o `\\r` no fim da linha.\n2. `errors.rs` acumula o mesmo erro por linha em vez de por arquivo.\n\nVou corrigir o parser primeiro e validar antes de tocar nos erros.".into(),
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
        text: "Um teste ainda falha: o segundo problema aparece agora. Corrigindo `errors.rs`."
            .into(),
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
        text: "## Resultado\n\nOs dois problemas tinham a mesma origem e estão corrigidos.\n\n| Arquivo | Mudança |\n| --- | --- |\n| `src/parser.rs` | `split('\\n')` por `lines()` |\n| `src/errors.rs` | erro agregado por arquivo |\n\n### Verificação\n\n- `cargo test`: **passou**\n- Casos novos: CRLF e erro duplicado\n\n> Não rodei o build release.\n".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    state.clock.elapsed_ms += 12_000;
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
    let mut read_only = session(false);
    read_only.mode = OperatingMode::ReadOnly;
    let mut plan = session(false);
    plan.mode = OperatingMode::Plan;
    // The turn marker follows how the turn ended.
    let ended = |lifecycle| {
        let mut state = connected();
        state.apply_event(UiEvent::UserMessageAdded {
            text: "Rode a suíte e corrija o que falhar.".into(),
        });
        state.append_block(Block::new(
            "tail",
            BlockKind::Assistant("Rodei a suíte e três testes falharam. Comecei pelo".into()),
            lifecycle,
        ));
        state
    };

    let mut mention = session(false);
    for character in "veja @lib".chars() {
        key(&mut mention, KeyCode::Char(character), KeyModifiers::NONE);
    }
    mention.apply_event(UiEvent::WorkspaceFiles {
        request_id: 1,
        paths: [
            "crates/slim-core/src/lib.rs",
            "crates/slim-tui/src/lib.rs",
            "crates/slim-lsp/src/lib.rs",
            "crates/slim-tui/src/reducer/slash_tests.rs",
            "docs/DESIGN-SLIM-TUI.md",
            "README.md",
        ]
        .map(String::from)
        .to_vec(),
        truncated: false,
    });
    let mut resume = session(false);
    for character in "/resume".chars() {
        key(&mut resume, KeyCode::Char(character), KeyModifiers::NONE);
    }
    key(&mut resume, KeyCode::Enter, KeyModifiers::NONE);
    let now_ms = 900_000_000u64;
    let entry = |id: &str, title: Option<&str>, prompt: &str, minutes_ago: u64| {
        slim_tui::api::SessionListItem {
            id: id.into(),
            title: title.map(str::to_owned),
            first_prompt: prompt.into(),
            updated_ms: now_ms - minutes_ago * 60_000,
            bytes: 4_300 + minutes_ago * 97,
            in_use: false,
            current: false,
        }
    };
    let mut open = entry("tui-open", None, "Rode a suíte e corrija o que falhar.", 1);
    open.current = true;
    let mut locked = entry(
        "tui-locked",
        Some("Migração do banco"),
        "trocar sqlite por postgres",
        35,
    );
    locked.in_use = true;
    resume.apply_event(UiEvent::SessionsListed {
        request_id: 1,
        now_ms,
        items: vec![
            open,
            locked,
            entry(
                "tui-login",
                Some("Refatorar login"),
                "corrigir o bug do token expirado",
                130,
            ),
            entry(
                "tui-docs",
                None,
                "escrever a documentação da API pública",
                1_900,
            ),
            entry(
                "tui-perf",
                None,
                "por que o build incremental está lento?",
                9_400,
            ),
        ],
        error: None,
    });
    let mut rewind = session(false);
    for character in "/rewind".chars() {
        key(&mut rewind, KeyCode::Char(character), KeyModifiers::NONE);
    }
    key(&mut rewind, KeyCode::Enter, KeyModifiers::NONE);
    rewind.apply_event(UiEvent::TurnsListed {
        request_id: 1,
        items: [
            "Explique como o parser trata CRLF",
            "Corrija o split para lidar com CRLF",
            "Adicione um teste de regressão",
            "Rode a suíte e corrija o que falhar",
        ]
        .iter()
        .enumerate()
        .map(|(index, prompt)| slim_tui::api::TurnListItem {
            index,
            first_seq: index as u64 * 12 + 2,
            prompt: (*prompt).into(),
        })
        .collect(),
        error: None,
    });
    let mut named = session(false);
    named.apply_event(UiEvent::SessionTitleChanged {
        title: Some("Corrigir CRLF no parser".into()),
    });
    let mut user_shell = session(false);
    user_shell.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("user-shell-1".into()),
        call_id: slim_tui::api::ToolCallId("user-shell-1".into()),
        name: "shell".into(),
        arguments_summary: "! cargo test -q parser".into(),
    });
    user_shell.apply_event(UiEvent::ToolOutput {
        batch_id: slim_tui::api::ToolBatchId("user-shell-1".into()),
        call_id: slim_tui::api::ToolCallId("user-shell-1".into()),
        name: "shell".into(),
        output: "exit 0\nstdout:\nrunning 3 tests\ntest parser::crlf ... ok\ntest result: ok. 3 passed\nstderr:\n".into(),
        content_handle: None,
    });
    user_shell.apply_event(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("user-shell-1".into()),
        call_id: slim_tui::api::ToolCallId("user-shell-1".into()),
        name: "shell".into(),
        success: true,
        duration_ms: 2_300,
    });

    vec![
        ("welcome", connected(), 120, 30),
        ("session-running", session(true), 120, 44),
        ("session-done-80", session(false), 80, 40),
        ("agent-turn", agent_turn(), 100, 60),
        ("agent-turn-80", agent_turn(), 80, 60),
        ("thinking-streaming", thinking, 120, 24),
        ("turn-interrupted", ended(BlockLifecycle::Cancelled), 80, 14),
        ("turn-failed", ended(BlockLifecycle::Failed), 80, 14),
        ("mode-read-only", read_only, 80, 24),
        ("mode-plan", plan, 80, 24),
        ("palette", palette, 120, 40),
        ("slash", slash, 120, 40),
        ("model", model, 120, 40),
        ("mention", mention, 120, 40),
        ("resume", resume, 120, 40),
        ("rewind", rewind, 120, 40),
        ("session-named", named, 100, 30),
        ("user-shell", user_shell, 100, 40),
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

fn html(name: &str, state: &AppState, width: u16, height: u16, reduced_motion: bool) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    let mut cache = WrapCache::default();
    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion,
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
        let _ = writeln!(page, "<span style=\"{run_style}\">{}</span>", escape(&run));
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
        let page = html(name, &state, width, height, true);
        assert!(page.contains("<span"), "{name} rendered nothing");
        let path = directory.join(format!("{name}.html"));
        std::fs::write(&path, page).expect("write snapshot");
        eprintln!("{}", path.display());
    }
    // The thinking display with motion on, a few moments apart.
    for (name, state) in thinking_motion() {
        let page = html(&name, &state, 100, 10, false);
        assert!(page.contains("<span"), "{name} rendered nothing");
        let path = directory.join(format!("{name}.html"));
        std::fs::write(&path, page).expect("write snapshot");
        eprintln!("{}", path.display());
    }
}

/// A thought that just received text, shown 0, 166, 332, 498, 664 and 830 ms
/// later: the sweep moves along the label, the glow at the edge of the newest
/// row fades out, and the older preview row sits a step back.
fn thinking_motion() -> Vec<(String, AppState)> {
    const ARRIVED_MS: u64 = 3_400;
    [0u64, 166, 332, 498, 664, 830]
        .into_iter()
        .map(|later| {
            let mut state = connected();
            state.apply_event(UiEvent::UserMessageAdded {
                text: "Refatore o cache para LRU com peso por bytes.".into(),
            });
            state.apply_event(UiEvent::run_started(1));
            state.apply_event(UiEvent::ThinkingStarted);
            state.clock = FrameClock {
                frame: ARRIVED_MS / 83,
                elapsed_ms: ARRIVED_MS,
            };
            state.apply_event(UiEvent::ThinkingDelta {
                text: "O cache atual conta entradas, não bytes. Preciso ver quem chama insert e se o tamanho é conhecido no ponto de inserção antes de escolher entre o crate lru e um wrapper.".into(),
            });
            let elapsed_ms = ARRIVED_MS + later;
            state.clock = FrameClock {
                frame: elapsed_ms / 83,
                elapsed_ms,
            };
            (format!("thinking-motion-{later:03}"), state)
        })
        .collect()
}
