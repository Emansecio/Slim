# Slim TUI/CLI Surgical Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore Slim's event-driven Windows TUI scheduler and finish the approved low-risk CLI/TUI polish without changing product behavior.

**Architecture:** Put deadline calculation and Windows multi-handle waiting in a focused `runtime_wait` module, leaving reducer and renderer contracts unchanged. Refactor `run_loop` only enough to apply bounded batches, elapsed-derived clocks and wake-driven waiting; keep visual changes to ASCII selectors, consistent copy and structured help.

**Tech Stack:** Rust 2021, Crossterm, Ratatui, windows-sys, Cargo workspace tests, TestBackend goldens.

---

### Task 1: Deterministic runtime deadlines and Windows wait

**Files:**
- Create: `crates/slim-tui/src/runtime_wait.rs`
- Modify: `crates/slim-tui/src/lib.rs`

- [x] Add unit tests for elapsed-derived 83 ms frames, idle-without-deadline, nearest motion/status deadline and wake interruption.
- [x] Run `cargo test -p slim-tui runtime_wait::tests --lib -- --nocapture` and record RED because the module does not exist.
- [x] Implement `runtime_clock`, `next_visual_deadline` and `wait_for_runtime_signal`; on Windows wait over console input plus `WakeSignal`, map input/wake/timeout explicitly and propagate `WAIT_FAILED`.
- [x] Re-run the focused tests and require GREEN.

### Task 2: Bounded wake-driven TUI loop

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/src/runtime.rs`

- [x] Add deterministic batch tests proving event 33 and event 1,025 leave backlog and request another wake.
- [x] Run the focused runtime tests and record RED against the unbounded/current helpers.
- [x] Extract bounded lane drains, keep one coalescer per batch, re-arm `WakeSignal` on exhaustion and retain disconnect state until both lanes close.
- [x] Replace `poll(16 ms)` with `wait_for_runtime_signal`, compute `FrameClock` from elapsed time, and schedule only real motion/status deadlines.
- [x] Re-run runtime, motion, fault-injection and TUI bridge suites to GREEN.

### Task 3: Windows-stable selectors and consistent copy

**Files:**
- Modify: `crates/slim-tui/src/block.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Test: `crates/slim-tui/tests/thinking_expansion_golden.rs`
- Test: `crates/slim-tui/tests/multi_tool_golden.rs`
- Test: `crates/slim-tui/tests/interaction_roundtrip.rs`
- Test: `crates/slim-tui/tests/model_overlay_golden.rs`

- [x] Change golden expectations to ASCII `>` and English `Enter details`; run focused tests and record RED.
- [x] Replace every interactive U+203A marker with width-stable ASCII `>` and translate the remaining Portuguese renderer fallback to English.
- [x] Re-run the focused goldens and require GREEN; grep active TUI source for remaining U+203A and mixed-language strings.

### Task 4: Structured CLI help without output drift

**Files:**
- Modify: `crates/slim-cli/src/cli.rs`
- Test: `crates/slim-cli/tests/cli_contract.rs`

- [x] Replace the exact help expectation with a structured contract covering usage, TUI/headless, modes, providers, sessions, output, general flags and examples.
- [x] Run `cargo test -p slim-cli --test cli_contract help_version_and_unknown_flags_are_stable -- --exact --nocapture` and record RED.
- [x] Add one static help string and return it for `-h`/`--help`; do not touch argument parsing or non-help renderers.
- [x] Re-run CLI contracts and prove `--version`, default text and JSONL behavior remain GREEN.

### Task 5: Performance, documentation, complete gates and deploy

**Files:**
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify current-count references: `README.md`, `Documentações - Projeto/README.md`, `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`, `release/README.md`

- [x] Run `cargo bench -p slim-tui --bench long_session` and require input-to-frame p95 at or below 16 ms.
- [x] Run formatter, workspace check, Clippy all targets and `cargo test --workspace -j 1 --no-fail-fast`; count current suites/passed/failed/ignored.
- [x] Update tracker findings/execution log, design checkpoint and every current test-count reference; grep stale current values.
- [x] Run `git diff --check` and inspect only scoped changes while preserving unrelated WIP.
- [x] Run `./refresh-slim.ps1 -Test` and require `OK:`.
- [x] Compare release/PATH executable size, timestamp and SHA-256; run `slim --version` and confirm only one canonical executable per location.
