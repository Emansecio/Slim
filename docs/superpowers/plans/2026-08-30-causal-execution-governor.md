# Causal Execution Governor Implementation Plan

**Goal:** Add native per-agent-loop causal observation in shadow mode without changing tool execution, results, prompts, budgets, or stop behavior.

**Architecture:** `runtime/governor.rs` owns canonicalization and an execution-local Progress Ledger. Native `ToolSpec` supplies operational metadata. Runtime wraps provider-driven calls with pre/post observations and persists only progress, boundaries, and anomalies.

**Spec:** `docs/superpowers/specs/2026-08-30-causal-execution-governor-design.md`

## Constraints

- Preserve all unrelated dirty workspace changes.
- Shadow only; existing `LoopGuard` remains authoritative.
- No new dependency, config, prompt, service, watcher, Git subprocess, or workspace-wide scan.
- No generic shell normalization or memoization.
- No broad formatting or source commit.
- Add only tests needed to prove changed behavior.
- Run Cargo through `env -u RUSTC -u CARGO`.

## Task 1: Add operational metadata and governor core

**Files:**
- Modify `crates/slim-core/src/tools/mod.rs`
- Create `crates/slim-core/src/runtime/governor.rs`
- Modify `crates/slim-core/src/runtime/mod.rs` only to declare the module

1. Add focused unit tests first for canonical JSON/path/default equivalence, changed-vs-unchanged mutation progress, and repeated stable evidence/failure/validation classification.
2. Run only governor unit tests and confirm red state.
3. Replace `ToolSpec.mutates` with approved operational fields and derive mode mutation behavior.
4. Implement bounded SHA-256 fingerprints, focused file/directory snapshots, observed-workspace revision refresh, shell validation allowlist, evidence records, and turn stagnation.
5. Run governor and existing tool metadata tests.

## Task 2: Add persisted telemetry and runtime shadow integration

**Files:**
- Modify `crates/slim-core/src/events.rs`
- Modify `crates/slim-core/src/lib.rs`
- Modify `crates/slim-core/src/runtime/mod.rs`
- Modify `crates/slim-tui/src/api.rs`
- Modify directly affected `crates/slim-core/tests/agent_loop.rs`

1. Add typed progress, boundary, anomaly, confidence, and simulated-action enums plus three `EventKind` variants.
2. Map detailed governor events to no TUI event.
3. Create one governor per agent-loop execution; create an isolated one for direct one-turn execution.
4. Observe sequential calls immediately around execution. Observe parallel read calls before launch and consolidate results in provider order.
5. Emit returned observations with existing monotonic runtime sequencing.
6. Finish one governor turn after each executed provider batch.
7. Extend the existing repeated-failed-tool regression to prove shadow anomaly telemetry while preserving execution count and `RepeatedFailedTool` stop.
8. Run only governor unit tests and that focused integration test.

## Task 3: Verify surgical change

1. Run relevant existing tool/agent-loop tests affected by metadata and batch integration.
2. Run formatter check for touched Rust files.
3. Run `cargo check -p slim-core -p slim-tui`.
4. Run Clippy for touched crates with warnings denied.
5. Run `git diff --check` and inspect only governor-related hunks; do not alter unrelated dirty files.
