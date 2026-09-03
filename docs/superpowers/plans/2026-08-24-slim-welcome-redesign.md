# Slim Welcome Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the decorative green welcome with a static, sober hierarchy containing only the Slim name, connection state, and one contextual next action.

**Architecture:** Keep authentication state and layout ownership unchanged. Replace only the private welcome projection in `runtime.rs`, remove the now-unused `welcome.rs` identity/motion module, and prove content, color scope, compact rendering, `NO_COLOR`, and idle scheduling through focused Ratatui `TestBackend` tests before the workspace/deploy gates.

**Tech Stack:** Rust, Ratatui `TestBackend`, Crossterm capabilities, Cargo workspace tests, PowerShell deployment and Windows console screenshots.

**Workspace note:** This checkout contains broad user WIP. Do not create a worktree, reset files, or commit; patch only the listed welcome slices and required live documentation.

---

## File map

- Create `crates/slim-tui/tests/welcome_golden.rs`: focused visual/content/color contract at the three approved sizes.
- Modify `crates/slim-tui/src/runtime.rs`: static welcome projection and zero welcome-only motion ticks.
- Modify `crates/slim-tui/src/lib.rs`: remove the unused public `welcome` module.
- Delete `crates/slim-tui/src/welcome.rs`: remove braille/pulse code with no remaining consumer.
- Modify `crates/slim-tui/tests/motion_activity_golden.rs`: remove obsolete welcome phase coverage while preserving activity/reduced-motion coverage.
- Modify `Documentações - Projeto/DESIGN-SLIM-TUI.md`: update the normative welcome, color, motion, and performance contract.
- Modify `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`: append fresh RED/GREEN/deploy/screenshot evidence to §7.
- Modify `README.md`, `Documentações - Projeto/README.md`, `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`, and `release/README.md` only where welcome behavior or fresh test totals are cited.
- Modify `docs/superpowers/specs/2026-08-24-slim-welcome-redesign-design.md`: mark the approved design implemented and link the captured evidence.
- Create `analysis_outputs/welcome-redesign/*.png`: real Windows console captures at `120×30`, `60×16`, and `32×10`.

### Task 1: Add RED welcome contracts

**Files:**
- Create: `crates/slim-tui/tests/welcome_golden.rs`
- Modify: `crates/slim-tui/src/runtime.rs:2729-2739,2927-2975`
- Modify: `crates/slim-tui/tests/motion_activity_golden.rs:1-63`

- [ ] **Step 1: Add a focused TestBackend harness and the approved assertions**

Create a test file with this structure and assertions:

```rust
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use slim_tui::api::{LoginProvider, UiEvent};
use slim_tui::app::AppState;
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};
use unicode_width::UnicodeWidthStr;

fn caps(color_depth: ColorDepth, reduced_motion: bool) -> Capabilities {
    Capabilities { color_depth, mouse: false, clipboard: false, images: false, reduced_motion }
}

fn render(state: &AppState, width: u16, height: u16, capabilities: Capabilities) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| render_frame(frame, state, capabilities, &mut WrapCache::default()))
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    (0..buffer.area.height).map(|y| {
        let mut row = String::new();
        for x in 0..buffer.area.width { row.push_str(buffer[(x, y)].symbol()); }
        row
    }).collect::<Vec<_>>().join("\n")
}

fn cell_at_token<'a>(buffer: &'a Buffer, token: &str) -> &'a ratatui::buffer::Cell {
    for y in 0..buffer.area.height {
        let row = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>();
        if let Some(byte_index) = row.find(token) {
            let x = UnicodeWidthStr::width(&row[..byte_index]) as u16;
            return &buffer[(x, y)];
        }
    }
    panic!("token not rendered: {token}");
}
```

Add tests that prove:

```rust
#[test]
fn wide_disconnected_welcome_has_only_name_state_and_action() {
    let buffer = render(&AppState::new(), 120, 30, caps(ColorDepth::TrueColor, false));
    let frame = text(&buffer);
    assert!(frame.contains("SLIM"));
    assert!(frame.contains("○  Not connected"));
    assert!(frame.contains("Run /login to connect"));
    assert!(!frame.contains("Native coding agent"));
    assert!(!frame.chars().any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)));
    assert_eq!(cell_at_token(&buffer, "SLIM").fg, Color::Rgb(0xc6, 0xcd, 0xd5));
    assert!(cell_at_token(&buffer, "SLIM").modifier.contains(Modifier::BOLD));
    assert_eq!(cell_at_token(&buffer, "/login").fg, Color::Rgb(0x78, 0xd9, 0x9b));
}

#[test]
fn ansi16_connected_welcome_colors_only_the_status_dot() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AuthStateChanged {
        provider: Some(LoginProvider::OpenAiCodex), authenticated: true,
    });
    let buffer = render(&state, 60, 16, caps(ColorDepth::Ansi16, false));
    let frame = text(&buffer);
    assert!(frame.contains("Connected · OpenAI Codex — ChatGPT Plus/Pro"));
    assert!(frame.contains("Describe a task to begin"));
    assert_eq!(cell_at_token(&buffer, "●").fg, Color::Green);
    assert_ne!(cell_at_token(&buffer, "Connected").fg, Color::Green);
}

#[test]
fn no_color_and_reduced_motion_keep_the_same_compact_content_without_color() {
    let normal = render(&AppState::new(), 32, 10, caps(ColorDepth::None, false));
    let reduced = render(&AppState::new(), 32, 10, caps(ColorDepth::None, true));
    assert_eq!(text(&normal), text(&reduced));
    for token in ["SLIM", "○", "/login"] {
        assert_eq!(cell_at_token(&normal, token).fg, Color::Reset);
    }
}

#[test]
fn very_short_welcome_removes_spacing_before_content() {
    let two_rows = text(&render(&AppState::new(), 24, 6, caps(ColorDepth::TrueColor, false)));
    assert!(two_rows.contains("SLIM"));
    assert!(two_rows.contains("Not connected"));
    assert!(two_rows.contains("/login"));
    let one_row = text(&render(&AppState::new(), 24, 5, caps(ColorDepth::TrueColor, false)));
    assert!(one_row.contains("SLIM"));
    assert!(one_row.contains("Not connected"));
}
```

- [ ] **Step 2: Replace obsolete inline expectations with the new static contract**

Change `motion_scheduler_is_idle_or_reduced_without_ticks` so both `80×24` and `35×10` welcome states assert `!motion_active(...)`. Replace the old braille/tagline tests with assertions for `SLIM`, the provider-aware status, the contextual hint, and absence of braille/tagline. Remove `welcome_phase_takes_five_seconds` and its `welcome`/`FrameClock` imports from `motion_activity_golden.rs`.

- [ ] **Step 3: Run the RED gate**

Run:

```powershell
$env:RUSTC='C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'
cargo test -p slim-tui --test welcome_golden --lib --test motion_activity_golden
"EXIT=$LASTEXITCODE"
```

Expected before implementation: non-zero exit because the current render still contains the braille wordmark/tagline, colors the whole connected status, and schedules welcome motion.

### Task 2: Implement the static precision welcome

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs:759-775,1100-1186`
- Modify: `crates/slim-tui/src/lib.rs:1-22`
- Delete: `crates/slim-tui/src/welcome.rs`

- [ ] **Step 1: Stop scheduling welcome-only animation**

Remove the `full_welcome` branch from `motion_active_with_cache`. Keep only visible activity spinner and visible streaming caret as motion sources:

```rust
let regions = layout_for_state(state, width, height);
let activity_spinner =
    (state.working || state.activity.is_some()) && regions.activity_rail.height > 0;
activity_spinner || streaming_assistant_visible(state, width, height, cache)
```

- [ ] **Step 2: Replace `render_welcome` with the approved hierarchy**

Use `palette.text.add_modifier(Modifier::BOLD)` for `SLIM`; use accent only for the connected `●` or disconnected `/login`; when `ColorDepth::None`, use `Style::default()` for all welcome spans. Replace `render_welcome` with this complete projection:

```rust
fn render_welcome(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    palette: &Palette,
    capabilities: Capabilities,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (connection, hint) = welcome_copy(state);
    let no_color = capabilities.color_depth == ColorDepth::None;
    let text = if no_color { Style::default() } else { palette.text };
    let secondary = if no_color { Style::default() } else { palette.secondary };
    let muted = if no_color { Style::default() } else { palette.muted };
    let accent = if no_color { Style::default() } else { palette.accent };
    let name_style = text.add_modifier(Modifier::BOLD);

    let name = || Line::from(Span::styled("SLIM", name_style));
    let status = || {
        if state.authenticated {
            Line::from(vec![
                Span::styled("●  ", accent),
                Span::styled(connection.clone(), secondary),
            ])
        } else {
            Line::from(vec![
                Span::styled("○  ", secondary),
                Span::styled(connection.clone(), secondary),
            ])
        }
    };
    let hint_line = || {
        if state.authenticated {
            Line::from(Span::styled(hint, muted))
        } else {
            Line::from(vec![
                Span::styled("Run ", muted),
                Span::styled("/login", accent),
                Span::styled(" to connect", muted),
            ])
        }
    };
    let compact_status = || {
        if state.authenticated {
            status()
        } else {
            Line::from(vec![
                Span::styled("○ Not connected · ", secondary),
                Span::styled("/login", accent),
            ])
        }
    };
    let one_line = || {
        let mut spans = vec![
            Span::styled("SLIM", name_style),
            Span::styled(" · ", muted),
        ];
        if state.authenticated {
            spans.push(Span::styled("● ", accent));
            spans.push(Span::styled(connection.clone(), secondary));
        } else {
            spans.push(Span::styled("○ Not connected", secondary));
        }
        Line::from(spans)
    };

    let lines = match area.height {
        1 => vec![one_line()],
        2 => vec![name(), compact_status()],
        3 => vec![name(), status(), hint_line()],
        _ => vec![name(), Line::default(), status(), hint_line()],
    };
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(1)
        .min(area.width as usize) as u16;
    let centered_area = centered(area, width.max(1), lines.len() as u16);
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        centered_area,
    );
}
```

The state copy must be:

```rust
fn welcome_copy(state: &AppState) -> (String, &'static str) {
    if state.authenticated {
        (
            format!(
                "Connected · {}",
                state.auth_provider.map_or("provider", LoginProvider::label)
            ),
            "Describe a task to begin",
        )
    } else {
        ("Not connected".into(), "Run /login to connect")
    }
}
```

Keep glyph and label in separate spans so a connected line cannot become entirely green. Do not add a border, version, metric, shortcut row, tagline, or replacement animation.

- [ ] **Step 3: Remove the unused decorative module**

Delete `pub mod welcome;` from `lib.rs` and delete `welcome.rs`. Confirm no source reference remains:

```powershell
rg -n "welcome::|wordmark|PULSE_ASCII|Native coding agent" crates/slim-tui
```

Expected: no production reference; only explicit negative assertions may remain in tests.

- [ ] **Step 4: Run the GREEN focused gate**

Run the exact Task 1 command again. Expected: exit `0`, all focused welcome/motion tests pass, and no compiler warnings.

### Task 3: Synchronize the normative documentation

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify: `README.md`
- Modify: `Documentações - Projeto/README.md`
- Modify: `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`
- Modify: `release/README.md`
- Modify: `docs/superpowers/specs/2026-08-24-slim-welcome-redesign-design.md`

- [ ] **Step 1: Update the visual and motion contract**

In `DESIGN-SLIM-TUI.md`, replace every claim that the welcome has a braille wordmark, five-second phase, pulse, or welcome-only redraw budget. Record the approved static hierarchy, green-as-functional-accent rule, provider-aware connection line, contextual hint, compact/no-color behavior, and `0` welcome-only ticks. Keep ActivityRail/caret motion unchanged.

- [ ] **Step 2: Update product summaries without broad rewrites**

Use:

```powershell
rg -n -i "welcome|wordmark|braille|pulso|pulse|5 s|5-second|redraw" README.md 'Documentações - Projeto' release/README.md
```

Change only statements that describe the old welcome. Mark the design spec status as implemented only after the GREEN focused gate; do not alter unrelated OpenCode/Harness text.

### Task 4: Run canonical gates, deploy, and record fresh totals

**Files:**
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`
- Modify count citations in the five mandatory documentation files only if they differ from the fresh gate.

- [ ] **Step 1: Run formatting and focused quality checks**

```powershell
cargo fmt --all -- --check
cargo clippy -p slim-tui --all-targets -- -D warnings
git diff --check
```

Expected: each command exits `0`.

- [ ] **Step 2: Run the canonical workspace test and capture exact totals**

```powershell
cargo test --workspace 2>&1 | Tee-Object -FilePath "$env:TEMP\slim-welcome-workspace-test.log"
$testExit=$LASTEXITCODE
"EXIT=$testExit"
rg "test result:" "$env:TEMP\slim-welcome-workspace-test.log"
```

Expected: `EXIT=0`, zero failed, zero compiler warnings. Sum the fresh `passed` values from all `test result:` lines; count the result lines as the suite total and preserve the physical ConPTY ignore separately.

- [ ] **Step 3: Synchronize every mandatory count citation**

```powershell
rg -n "[0-9]+ suítes|[0-9]+ passed|[0-9]+ testes|ConPTY" README.md 'Documentações - Projeto/README.md' 'Documentações - Projeto/PLANO-IMPLEMENTACAO.md' release/README.md 'Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md'
```

Replace stale current-state totals with the values from Step 2; keep historical log rows historical. Append one §7 row containing the focused RED, focused GREEN, canonical totals, screenshot paths, and deployment identity.

- [ ] **Step 4: Build, retest, and deploy the PATH binary**

```powershell
.\refresh-slim.ps1 -Test
"EXIT=$LASTEXITCODE"
Get-FileHash -Algorithm SHA256 '.\target\release\slim.exe','C:\Users\User\bin\Slim.exe'
Get-Item '.\target\release\slim.exe','C:\Users\User\bin\Slim.exe' | Select-Object FullName,Length,LastWriteTime
slim --version
```

Expected: script prints `OK:`, exit `0`, both hashes and sizes match, and the smoke prints `slim 0.1.0`.

### Task 5: Capture and inspect real terminal screenshots

**Files:**
- Create: `analysis_outputs/welcome-redesign/welcome-120x30-disconnected.png`
- Create: `analysis_outputs/welcome-redesign/welcome-60x16-connected.png`
- Create: `analysis_outputs/welcome-redesign/welcome-32x10-no-color.png`
- Modify: `docs/superpowers/specs/2026-08-24-slim-welcome-redesign-design.md`
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`

- [ ] **Step 1: Launch isolated real console scenarios**

Launch the deployed `C:\Users\User\bin\Slim.exe` in a new visible console for each case. For disconnected cases, point `SLIM_AUTH_FILE` to a nonexistent temporary path and clear `SLIM_API_KEY`, `OPENAI_API_KEY`, `CODEX_ACCESS_TOKEN`, `ANTHROPIC_API_KEY`, and `OPENCODE_API_KEY` only in that child process. For the connected case, set `SLIM_PROVIDER=opencode-go` and `SLIM_API_KEY=visual-fixture-not-a-secret`; do not type a prompt, so no provider request occurs. Set `NO_COLOR=1` and `SLIM_REDUCED_MOTION=1` only for the `32×10` child.

Use `mode con cols=<width> lines=<height>` before starting Slim and verify the visible content corresponds to the requested dimensions.

- [ ] **Step 2: Capture each visible window to PNG**

Capture the actual console window rectangle with a desktop/window screenshot facility into the three exact output paths. Close each child with `Ctrl+C` after capture; do not modify or remove the user's real auth store.

- [ ] **Step 3: Inspect every PNG**

Open all three PNG files at original detail and confirm: no braille/pulse/tagline; no clipping or wrap; hierarchy remains centered; wide view has no added chrome; narrow view remains balanced; `NO_COLOR` has no green text; connection and action remain readable. If any image contradicts the contract, fix the smallest responsible render rule and repeat focused tests, deployment, and only the affected capture.

- [ ] **Step 4: Link evidence and run the final diff gate**

Add the three relative screenshot paths and the verified scenarios to the design spec and tracker row, then run:

```powershell
git diff --check
rg -n -i "wordmark braille|pulso ambiente|welcome 5 s|welcome-only.*12" README.md 'Documentações - Projeto' release/README.md crates/slim-tui
```

Expected: diff check exits `0`; old behavior appears only in clearly historical records or negative regression assertions.
