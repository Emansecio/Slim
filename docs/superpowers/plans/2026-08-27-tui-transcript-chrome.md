# Compactar chrome do transcript

**Goal:** Comprimir o chrome do transcript (thinking, tools, footer) sem mudar layout, rails ou caps.

**Architecture:** Ajustes pontuais em `AppState` (fold/thinking), `runtime.rs` (render de tools/thinking), `view_model.rs` (ctx %), e `slim-core/tools/mod.rs` (shell header humanizado). Sem novas superfícies.

**Tech Stack:** Rust, Ratatui, Cargo workspace tests, TestBackend goldens.

---

### Task 1: Spec + Thinking

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §1.1, §15.2.1
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` §5, §7
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Test: `crates/slim-tui/tests/thinking_expansion_golden.rs`
- Test: `crates/slim-tui/tests/markdown_golden.rs`

- [ ] Thinking body only when `fold == Expanded`; `Auto` treated as collapsed
- [ ] Skip empty/whitespace-only thinking blocks
- [ ] Goldens green

### Task 2: ctx mínimo 1%

**Files:**
- Modify: `crates/slim-tui/src/view_model.rs`
- Test: `crates/slim-tui/tests/m2_integration.rs`

### Task 3: Enter details no foco / live edge

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/tests/multi_tool_golden.rs`

### Task 4: Tool colapsada sem args/preview

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/tests/tool_details_golden.rs`
- Test: `crates/slim-tui/tests/multi_tool_golden.rs`

### Task 5: Shell header humanizado

**Files:**
- Modify: `crates/slim-core/src/tools/mod.rs`
- Test: `crates/slim-core/tests/tool_contracts.rs` (or new shell test)

### Task 6: Gate

- [ ] `cargo test --workspace`
- [ ] `.\refresh-slim.ps1 -Test`
- [ ] Sync test counts in README docs
