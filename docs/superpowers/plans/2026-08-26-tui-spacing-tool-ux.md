# TUI Spacing and Tool UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the approved compact turn rhythm, operational metadata placement, truthful page-fill, actionable tool grouping, and opt-in verbose headless tool timeline.

**Architecture:** Keep layout truth in `HeightIndex` and the existing renderer rather than adding a second layout system. Add one block-level turn-boundary bit so measurement and materialization agree, route Enter through the existing terminal action boundary, and keep verbose CLI data on the final headless result while leaving standard text and JSONL renderers unchanged.

**Tech Stack:** Rust, Ratatui, Crossterm, Cargo workspace tests, TestBackend goldens.

---

### Task 1: Turn rhythm, SessionRail and focus hierarchy

**Files:**
- Modify: `crates/slim-tui/src/block.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Modify: `crates/slim-tui/src/theme.rs`
- Test: `crates/slim-tui/tests/scroll_golden.rs`
- Test: `crates/slim-tui/tests/layout_golden.rs`
- Test: `crates/slim-tui/tests/golden_matrix.rs`

- [x] Add a failing golden with two user turns that expects exactly one blank row before the second user and no leading blank before the first.
- [x] Run `cargo test -p slim-tui --test scroll_golden turn_boundary_adds_one_row_only_before_later_user -- --exact --nocapture` and record RED.
- [x] Add private block metadata for a leading turn boundary, set it when appending every user after the first user, include it in clone/equality/cache generation, height measurement and rendered lines.
- [x] Keep user anchors on their content row so page-fill does not expose an orphan separator at the top.
- [x] Re-run the focused turn-boundary test to GREEN.
- [x] Add failing layout tests proving cwd `~` hides SessionRail and moves context to the footer while `D:\Slim` keeps the rail without duplication.
- [x] Run the two focused layout tests and record RED.
- [x] Add one shared `is_trivial_cwd` decision and use it in fullscreen layout planning; preserve the current breakpoints for useful cwd values.
- [x] Change `border_focus` from `#78D99B` to normative `#4F7D5E` and pin it in a theme test.
- [x] Re-run layout/golden focused suites to GREEN.

### Task 2: Make live-edge rendering use the authoritative prompt anchor

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/tests/scroll_golden.rs`

- [x] Add a failing regression with enough historical rows to overflow, followed by a short new prompt/answer suffix; assert the new prompt is at the first useful row and historical tail is absent above it.
- [x] Run `cargo test -p slim-tui --test scroll_golden latest_short_turn_page_fills_from_its_prompt_after_long_history -- --exact --nocapture` and record RED.
- [x] Replace the renderer's independent `LiveEdge => bottom` calculation with `HeightIndex::metrics(...).viewport_start` after final scrollbar width is known.
- [x] Re-run the regression and all scroll goldens to GREEN.

### Task 3: Actionable and informative tool groups

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/tests/multi_tool_golden.rs`
- Test: `crates/slim-tui/tests/tool_details_golden.rs`

- [x] Add a failing golden expecting `✓ 4 tools · read ×3, shell · 42ms`, first-seen ordering, and compact fallback without names at narrow width.
- [x] Run the focused group-summary test and record RED.
- [x] Implement a bounded sanitized name counter (maximum three distinct displayed names plus `+N`) and choose the richest header that fits without wrapping; omit duration when any member lacks one.
- [x] Re-run the group-summary test to GREEN.
- [x] Add a failing terminal-action test that presses Enter at live edge with an empty composer and expects the last visible tool group to expand without a preliminary Up.
- [x] Run the direct-Enter test and record RED.
- [x] At the terminal event boundary, use current scroll metrics' `last_visible_foldable_anchor` for plain Enter only when navigation is uncaptured, composer is empty and state is live edge; emit `Action::ToggleBlock`.
- [x] Re-run multi-tool/tool-details suites to GREEN and preserve failed/cancelled individual rows.

### Task 4: Opt-in verbose headless tool timeline

**Files:**
- Modify: `crates/slim-cli/src/main.rs`
- Modify: `crates/slim-cli/src/cli.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Modify: `crates/slim-cli/src/lib.rs`
- Test: `crates/slim-cli/tests/cli_contract.rs`
- Test: `crates/slim-cli/tests/headless_contract.rs`
- Test: `crates/slim-cli/tests/provider_cli.rs`

- [x] Add failing parser/contract tests for `--verbose`, explicit `--verbose --jsonl` rejection, and unchanged default help/stdout.
- [x] Run the focused CLI tests and record RED.
- [x] Add `verbose: bool` to parsed args and known stdin flags; document it in help and reject the JSONL combination before provider execution.
- [x] Add `tool_summary_lines` to `ProviderHeadlessResult`, populated from already-redacted `ToolFinished` events by consecutive batch and outcome; never include arguments or output.
- [x] Add `render_provider_verbose_text` while leaving `render_provider_text` and `render_provider_jsonl` byte-for-byte unchanged.
- [x] Wire the verbose renderer only for provider-backed human text output.
- [x] Re-run CLI/headless/provider suites to GREEN.

### Task 5: Contract documentation, full gates and deploy

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`
- Modify current-count references: `README.md`, `Documentações - Projeto/README.md`, `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`, `release/README.md`

- [x] Run `cargo fmt --all` and `cargo fmt --all -- --check` with the canonical Scoop/MSVC toolchain.
- [x] Run focused slim-tui and slim-cli suites, then `cargo test --workspace`; count suite/result lines, passed, failed, ignored and warnings from this session.
- [x] Update normative spacing/tool/footer/page-fill/headless contracts before finalizing behavior status; add the new tracker findings and §7 execution row.
- [x] Synchronize current test counts in every required status file and grep for stale current references.
- [x] Run `git diff --check` and inspect the scoped diff without disturbing unrelated WIP.
- [x] Run `.\refresh-slim.ps1 -Test` and require `OK:`.
- [x] Compare release and `C:\Users\User\bin\Slim.exe` version, size, timestamp and SHA-256; perform a final `slim --version` smoke test.
