# Slim Complete UX Polish Implementation Plan

**Status:** concluído em 2026-08-29; evidência final registrada no §7 de
`Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`.

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Deliver the accepted Slim TUI/CLI visual and interaction polish with real editor semantics, bounded pickers, code/diff surfaces, inspectors/search, contextual chrome and capability-safe clipboard/image handling.

**Architecture:** Preserve the existing `AppState -> reducer -> runtime` authority. Add pure viewport/editor/projection helpers and keep IO in runtime effects or the CLI boundary. Each task is a vertical RED→GREEN slice and must not refactor unrelated WIP.

**Tech Stack:** Rust 2021, Ratatui, Crossterm, pulldown-cmark, unicode-segmentation, unicode-width, TestBackend goldens, PowerShell deployment.

---

### Task 1: Bounded picker behavior

**Files:**
- Create: `crates/slim-tui/src/picker.rs`
- Modify: `crates/slim-tui/src/lib.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/tests/model_overlay_golden.rs`

- [x] **Step 1: Write the failing long-catalog test**

```rust
#[test]
fn selected_model_stays_visible_in_a_hundred_row_catalog() {
    let mut state = state_with_catalog_len(100);
    for _ in 0..90 { reduce(&mut state, Action::Key(press(KeyCode::Down))); }
    let frame = render_at(&state, 80, 24);
    assert!(frame.contains("> Model 86"));
    assert!(frame.contains("/104"));
}
```

- [x] **Step 2: Run RED**

Run: `cargo test -p slim-tui --test model_overlay_golden selected_model_stays_visible -- --nocapture`

Expected: FAIL because the selected row is clipped.

- [x] **Step 3: Implement pure picker windowing and responsive rendering**

```rust
pub fn visible_window(total: usize, selected: usize, capacity: usize) -> Range<usize> {
    let capacity = capacity.max(1).min(total.max(1));
    let start = selected.saturating_add(1).saturating_sub(capacity)
        .min(total.saturating_sub(capacity));
    start..start.saturating_add(capacity).min(total)
}
```

Add Up/Down/Home/End for model, command palette and slash picker, fixed filter/footer rows, compact IDs and current-provider expansion.

- [x] **Step 4: Run GREEN**

Run: `cargo test -p slim-tui --test model_overlay_golden`

Expected: all model overlay tests pass.

### Task 2: Semantic code and diff rows

**Files:**
- Modify: `crates/slim-tui/src/markdown.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/theme.rs`
- Test: `crates/slim-tui/tests/markdown_golden.rs`

- [x] **Step 1: Write the failing style tests**

```rust
#[test]
fn fenced_code_uses_rail_and_code_background() {
    let rendered = render("```rust\nlet x = 1;\n```", 40);
    assert!(rendered.text().contains("│ let x = 1;"));
    assert_eq!(rendered.word_style("let").1.bg, Some(Color::Rgb(0x0B, 0x0E, 0x12)));
}

#[test]
fn diff_rows_keep_prefixes_and_semantic_backgrounds() {
    let rendered = render("```diff\n-old\n+new\n```", 40);
    assert_ne!(rendered.word_style("old").1.bg, rendered.word_style("new").1.bg);
}
```

- [x] **Step 2: Run RED**

Run: `cargo test -p slim-tui --test markdown_golden fenced_code_uses_rail diff_rows_keep_prefixes`

Expected: FAIL because block code is projected as prose.

- [x] **Step 3: Implement semantic code/diff projection**

```rust
enum LineKind { Text, Code, DiffAdd, DiffRemove, DiffHeader, DiffContext }
```

Carry the kind through measurement and wrapping, reserve two cells for the rail,
pad code/diff rows to width and consume only semantic theme tokens.

- [x] **Step 4: Run GREEN**

Run: `cargo test -p slim-tui --test markdown_golden`

Expected: all Markdown goldens pass.

### Task 3: Grapheme-safe composer and adaptive height

**Files:**
- Modify: `crates/slim-tui/src/composer.rs`
- Modify: `crates/slim-tui/src/layout.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/tests/layout_golden.rs`
- Test: `crates/slim-tui/tests/properties.rs`

- [x] **Step 1: Write one RED editor contract**

```rust
#[test]
fn cursor_edits_before_the_tail_without_splitting_paste() {
    let mut composer = Composer::default();
    composer.insert_text("ac");
    composer.move_left();
    composer.insert_text("b");
    assert_eq!(composer.payload(), "abc");
}
```

- [x] **Step 2: Run RED**

Run: `cargo test -p slim-tui cursor_edits_before_the_tail -- --nocapture`

Expected: compile failure because cursor movement does not exist.

- [x] **Step 3: Implement cursor and editing**

Store a raw character offset, snap navigation across `Paste` elements, and add
`move_left`, `move_right`, `move_home`, `move_end`, `delete`, cursor-aware insert
and `display_snapshot(width)`.

- [x] **Step 4: Add adaptive composer RED then GREEN**

```rust
assert_eq!(composer_height_for_lines(24, 5), 7);
assert_eq!(composer_height_for_lines(10, 5), 3);
```

Render up to five logical lines around the cursor and keep the physical cursor
inside the content rect. Run `cargo test -p slim-tui --test layout_golden`.

### Task 4: Motion, footer and responsive overlays

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/tests/motion_activity_golden.rs`
- Modify: `crates/slim-tui/tests/welcome_golden.rs`
- Modify: `crates/slim-tui/tests/golden_matrix.rs`

- [x] **Step 1: Pin stable border and clean welcome footer RED**

```rust
assert_eq!(composer_border_at_frame(0), composer_border_at_frame(24));
assert!(!welcome.contains("ctx --"));
assert!(!welcome.contains("↑0 ↓0"));
```

- [x] **Step 2: Remove border pulse and zero-value chrome**

Keep ActivityRail as the primary animated locus; preserve reduced-motion layout.
Make login/effort/palette widths relative to the frame and truncate secondary IDs.

- [x] **Step 3: Run GREEN matrix**

Run: `cargo test -p slim-tui --test motion_activity_golden --test welcome_golden --test golden_matrix`

### Task 5: Inspectors and transcript search

**Files:**
- Modify: `crates/slim-tui/src/inspector.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Create: `crates/slim-tui/tests/inspector_search_golden.rs`

- [x] **Step 1: Write the failing keyboard/render slice**

```rust
#[test]
fn ctrl_d_opens_latest_diff_and_ctrl_f_selects_a_match() {
    let mut state = state_with_diff_and_repeated_text();
    reduce(&mut state, Action::Key(ctrl('d')));
    assert!(render(&state, 120, 30).contains("Diff"));
    reduce(&mut state, Action::Key(ctrl('f')));
    type_text(&mut state, "needle");
    assert!(render(&state, 80, 24).contains("1/2"));
}
```

- [x] **Step 2: Run RED**

Run: `cargo test -p slim-tui --test inspector_search_golden -- --nocapture`

- [x] **Step 3: Implement ViewModels and routing**

Add bounded inspector offset, search query/matches/current, Ctrl+D/J/R/G/F,
Esc cascade and right drawer for width >=100 with overlay fallback otherwise.

- [x] **Step 4: Run GREEN**

Run: `cargo test -p slim-tui --test inspector_search_golden`

### Task 6: Clipboard, image fallback and human headless output

**Files:**
- Modify: `crates/slim-tui/src/image.rs`
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Test: `crates/slim-tui/tests/m3_golden.rs`
- Test: `crates/slim-cli/tests/headless_contract.rs`

- [x] **Step 1: Write RED output and capability tests**

```rust
assert_eq!(render_provider_text(&result), "answer\n");
assert!(render_provider_verbose_text(&result).starts_with("answer\n"));
assert_eq!(attachment_for_path("missing.png", false), ImageAttachment::Unavailable(_));
```

- [x] **Step 2: Implement answer-first and bounded capability effects**

Keep JSONL unchanged. Add clipboard effect with toast fallback and image path
attachment/placeholder only where provider/model capability is known.

- [x] **Step 3: Run GREEN**

Run: `cargo test -p slim-cli --test headless_contract`

Run: `cargo test -p slim-tui --test m3_golden`

### Task 7: Finish gate, documentation and deployment

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`
- Check cited test totals in all files mandated by `RULES.md`.

- [x] **Step 1: Run formatting and focused suites**

Run: `cargo fmt --all -- --check`

Run: `cargo test -p slim-tui --no-fail-fast`

- [x] **Step 2: Run full gates and benchmark**

Run: `cargo test --workspace --no-fail-fast`

Run: `cargo bench -p slim-tui --bench long_session`

- [x] **Step 3: Inspect representative TestBackend/PTY surfaces**

Run welcome, long model picker, code/diff, search, inspector and multiline composer
in PTY/TestBackend at representative sizes; record ConPTY limitations honestly.

- [x] **Step 4: Update living docs and counts**

Update checkpoint and tracker with only session-confirmed results; grep every
legacy count across README/project plan/release/tracker.

- [x] **Step 5: Deploy**

Run: `.\refresh-slim.ps1 -Test`

Expected: final line beginning with `OK:` and identical release/PATH hashes.

## Plan self-review

- Every accepted recommendation maps to a task.
- No task depends on a provider call or fabricated image protocol.
- Public state/type names are consistent across tasks.
- Tests are vertical and each implementation begins only after observed RED.
- The recorded execution did not authorize commits, worktrees or subagents; a new task follows its own authorization and the current AGENTS.md.
