# Streaming, Thinking and Tools TUI Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Make the deployed Slim TUI present reasoning, tool execution, multi-tool runs, interaction requests, streaming cadence, and ConPTY evidence as one causally correct, inspectable timeline.

**Architecture:** Preserve the core as domain authority and the TUI as a pure projection. Add explicit lifecycle and stable identities at the core event boundary, carry them through the bounded bridge, and keep progressive disclosure in typed transcript blocks. Keep tool execution serial; model a provider turn as a stable tool batch so future concurrency does not require a presentation rewrite. Measure cadence and queue pressure before changing scheduling, then apply only the bounded/coalesced behavior justified by that baseline.

**Tech Stack:** Rust 2021, std/tokio channels, Ratatui `TestBackend`, Crossterm, portable-pty/Windows ConPTY, PowerShell deployment.

**Approved source:** `analysis_outputs/AUDIT-STREAMING-THINKING-TOOLS-TUI.md` §9. This plan resolves the report's two conditional decisions as follows:

- `Responding` means assistant text has arrived, not merely that a provider request is outstanding.
- after a completed tool and before the next reasoning/text delta, the activity is `AwaitingProvider`.
- tools in one provider response remain serial; the contract records stable `call_id` and `batch_id` and never implies parallel execution.
- the `<40×8` `Working…` fallback and the existing fullscreen/restore architecture remain unchanged.

**Workspace note:** This checkout contains broad user WIP, including the target files. Do not create a worktree, reset, checkout, clean, or commit. Patch only the files required by a slice and the mandatory live documentation. Re-read overlapping hunks immediately before each patch.

---

## File map

- Modify `crates/slim-core/src/events.rs`: canonical thinking boundaries; correlated tool lifecycle; typed interaction requests.
- Modify `crates/slim-core/src/runtime/mod.rs`: infer thinking boundaries once in the stream normalizer; emit serial tool call/batch identity and duration.
- Modify `crates/slim-core/src/model.rs`: bounded event-sender seam used by the TUI bridge.
- Modify `crates/slim-core/tests/agent_loop.rs`, `runtime_abort.rs`, provider/runtime protocol tests as required by the event contract.
- Modify `crates/slim-tui/src/api.rs`: lossless event projection, request/page identity, commands and acknowledgements.
- Modify `crates/slim-tui/src/app.rs`: causal activity state, block selection, tool pages, request state and idempotence.
- Modify `crates/slim-tui/src/block.rs`: inspectable tool/request state with fold and page metadata.
- Modify `crates/slim-tui/src/reducer.rs`: `ToggleBlock`, selection, paging, answer/approve/reject effects.
- Modify `crates/slim-tui/src/render.rs`: temporal coalescing metrics, three-line Thinking preview, batch-aware tool grouping.
- Modify `crates/slim-tui/src/runtime.rs`: frame deadline scheduling, selection key routing and inline detail rendering.
- Modify `crates/slim-tui/src/view_model.rs`: `AwaitingProvider` and new typed block projections.
- Modify/add focused tests under `crates/slim-tui/tests/`: lifecycle, expansion, tool detail, multi-tool, interactions and cadence goldens.
- Modify `crates/slim-cli/src/tui.rs`: bounded core→projector queue, content registry/paging, correlated interaction command handling.
- Modify `crates/slim-cli/src/headless.rs`: generic bounded/unbounded event sender compatibility.
- Modify `crates/slim-cli/tests/tui_bridge.rs`: bridge identity, paging, interaction, queue and cancellation contracts.
- Modify `crates/slim-cli/tests/tui_pty.rs`: explicit deployed executable, offline fixture, size/capability matrix and persisted VT evidence.
- Modify `Documentações - Projeto/DESIGN-SLIM-TUI.md` before each normative behavior change.
- Modify `Documentações - Projeto/RUST-CLI.md` when freezing serial multi-tool behavior.
- Modify `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` §5 before fixing newly discovered bugs and §7 after every closed slice.
- Modify test-count references in `README.md`, `Documentações - Projeto/README.md`, `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`, `release/README.md`, and the tracker only from the final fresh gate.
- Create `analysis_outputs/streaming-thinking-tools-tui/` only for reproducible Slice 6 metrics and Slice 8 deployed-binary VT captures.

### Task 0: Establish the current baseline and protect WIP

**Files:**
- Create: `docs/superpowers/plans/2026-08-24-streaming-thinking-tools-tui.md`
- Read: `RULES.md`, `AGENTS.md`, the approved audit, current target code and live docs

- [x] Record `git status --short` without modifying or cleaning user files.
- [x] Set the active MSVC rustc explicitly and run `cargo test --workspace` with an unfiltered exit code.
- [x] If baseline failures predate this plan, isolate them with exact test output; repair only if they overlap a required slice and log them in tracker §5 before patching. (No baseline failure.)
- [x] Review this plan against all eight audit recommendations and mark Task 0 complete only when every recommendation has a testable owner.

### Task 1: Add causal Thinking lifecycle and truthful post-tool phase

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §§7.2, 10.1, 10.2, 15.4, 16.1
- Modify: `crates/slim-core/src/events.rs`
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Add/modify: focused core normalizer and `crates/slim-tui/tests/motion_activity_golden.rs`

- [x] First update the spec: `ThinkingStarted → ThinkingDelta* → ThinkingEnded`; infer the end before the first text/tool/stop; define `AwaitingProvider` and define `Responding` as first assistant text.
- [x] RED: cover reasoning→answer, reasoning→tool, reasoning-only stop, two rounds, ToolEnded→silence→reasoning and ToolEnded→silence→answer. Assert no Thinking block remains streaming after its end and late deltas do not revive a terminal run.
- [x] Run the focused RED tests and capture the non-zero exit.
- [x] GREEN: add core/UI boundary variants, one `reasoning_open` authority in `ProviderStreamNormalizer`, boundary-safe flushing, and the reducer/activity transitions.
- [x] Run focused core/TUI tests, then `cargo test -p slim-core -p slim-tui`; require 0 failures and 0 warnings.
- [x] Review every new exhaustive match, event ordering, redaction flush, replay compatibility and terminal-tail guard. Fix and rerun until clean.
- [x] Append Slice 1 RED/GREEN/review evidence to tracker §7.

### Task 2: Make Thinking selectable and expandable inline

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §§9.1, 11.3, 15.2, 17.2
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/block.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Add: `crates/slim-tui/tests/thinking_expansion_golden.rs`

- [x] Specify selection as the stable block at the scroll anchor; Up/Down/Page keys move the anchor, Enter with an empty composer toggles the selected fold, and composer input retains priority.
- [x] RED: keyboard sequence collapsed→expanded→collapsed at `120×30`, `80×24`, `60×16`, `40×10`; assert collapsed preview uses at most three physical lines, expansion is inline, and `(BlockId,row_offset)` remains valid after reflow.
- [x] Run and capture RED.
- [x] GREEN: add `Action::ToggleBlock(BlockId)`, stable selected block state/anchor derivation, fold invalidation and the three-line preview; do not add an overlay or nested scroll.
- [x] Run the focused golden plus scroll/property tests and `cargo test -p slim-tui`.
- [x] Review focus priority, empty composer behavior, Unicode wrapping, cache keys and live-edge/unseen semantics. Fix and rerun until clean.
- [x] Append Slice 2 evidence to tracker §7.

### Task 3: Carry tool identity, sanitized arguments, duration and paged output

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §§7.1–7.3, 11.4, 24.4
- Modify: `crates/slim-core/src/events.rs`
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/block.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-cli/src/tui.rs`
- Modify/add: `crates/slim-cli/tests/tui_bridge.rs`, `crates/slim-tui/tests/tool_details_golden.rs`

- [x] Specify stable `ToolCallId`, `ToolBatchId`, redacted argument summary, duration milliseconds, `ContentHandle`, request ID and cursor semantics. A page applies only to the matching handle/request/cursor.
- [x] RED: two homonymous calls with different arguments are distinguishable; secrets never reach UI state/frame; Enter expands first page; subsequent Enter loads the next page; stale/duplicate/out-of-order pages do not mutate the block.
- [x] Run and capture RED.
- [x] GREEN: correlate all executor lifecycle events by call ID, measure duration around execution, register full redacted output in a bounded bridge content store, project only bounded preview/handle, and implement 16 KiB pages with bounded retained content.
- [x] Run focused core, bridge and golden tests, then `cargo test -p slim-core -p slim-tui -p slim-cli`.
- [x] Review secret handling, output memory bounds, request correlation, cancellation after ToolStarted and same-name lookup removal. Fix and rerun until clean.
- [x] Append Slice 3 evidence to tracker §7.

### Task 4: Freeze and render the serial multi-tool contract

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §§11.4, 16.2
- Modify: `Documentações - Projeto/RUST-CLI.md` multi-tool section
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Modify: `crates/slim-tui/src/block.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Add: `crates/slim-tui/tests/multi_tool_golden.rs`

- [x] Specify current execution as serial in provider order. One provider response owns one batch ID; presentation may collapse only consecutive successful members of that same batch.
- [x] RED: equal and different names, mixed success/failure, two adjacent provider rounds, and cancellation during the second call preserve order and member identity; failed/cancelled members remain individual.
- [x] Run and capture RED.
- [x] GREEN: group by batch ID rather than name, render `✓ N tools · duration`, expand the group to member rows by call ID, and preserve original order. Do not introduce parallel execution.
- [x] Run focused core/TUI tests and `cargo test -p slim-core -p slim-tui`.
- [x] Review ordering, grouping across turns, duration aggregation, failed/cancelled visibility and scroll anchors. Fix and rerun until clean.
- [x] Append Slice 4 evidence to tracker §7.

### Task 5: Complete the typed input/approval surface

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §§7.2–7.4, 11.2, 15.2, 16, 17.2
- Modify: `crates/slim-core/src/events.rs`
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/block.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-cli/src/tui.rs`
- Add: `crates/slim-tui/tests/interaction_roundtrip.rs`
- Modify: `crates/slim-cli/tests/tui_bridge.rs`

- [x] Specify request IDs, prompt/options/approval summary, persisted pending state, `AnswerInput`, `Approve`, `Reject`, and typed accepted/rejected acknowledgement. Never display a legacy request that has no response route.
- [x] RED: request→render→answer/approve/reject→ack, duplicate request, stale response, rejected response and replay into a fresh AppState are deterministic and never submit the answer as a normal provider prompt.
- [x] Run and capture RED.
- [x] GREEN: add typed request blocks and commands; composer answers input requests, `Y/N` handles approval, and only a matching acknowledgement completes/clears the request. The CLI bridge must visibly reject unbound requests instead of simulating success.
- [x] Run focused TUI/bridge tests and `cargo test -p slim-core -p slim-tui -p slim-cli`.
- [x] Review authorization semantics, request replay/idempotence, composer priority, terminal-run interaction and unsupported-host rejection. Fix and rerun until clean.
- [x] Append Slice 5 evidence to tracker §7.

### Task 6: Measure cadence and queue pressure reproducibly

**Files:**
- Modify/add tests in `crates/slim-tui/src/render.rs`, `crates/slim-tui/src/runtime.rs`, `crates/slim-cli/tests/tui_bridge.rs`
- Create: `analysis_outputs/streaming-thinking-tools-tui/baseline.md`

- [x] Add a fake monotonic clock and render counter around the event drain/coalescer without changing production scheduling.
- [x] Add sustained producer/retained-consumer tests that report first-frame delay, frames per burst, p95 event→frame, core queue high-water, retained bytes and Windows working-set delta when available. Assertions must use deterministic queue/frame limits; RSS is recorded, not used as a flaky exact threshold.
- [x] Run each measurement at least three times with the same corpus and record commands, hardware-independent counts, median timing/RSS observations and variance in `baseline.md`.
- [x] Define the Slice 7 thresholds from the deterministic evidence: first delta immediate, later frame delay ≤16 ms, terminal loss 0, core queue ≤configured capacity, and bounded retained bytes.
- [x] Review the harness for sleep-based races, hidden network/provider use and self-fulfilling assertions. Repair and rerun until repeatable.
- [x] Append Slice 6 evidence and the conditional decision to tracker §7.

### Task 7: Enforce measured temporal coalescing and bounded backpressure

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §§10.1–10.3, 26, 27
- Modify: `crates/slim-core/src/model.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Modify: `crates/slim-cli/src/tui.rs`
- Modify/add: cadence and bridge tests from Slice 6

- [x] RED: encode the Slice 6 deterministic thresholds; prove the current per-drain coalescer/unbounded core queue violates at least the cadence or capacity contract.
- [x] Run and capture RED.
- [x] GREEN: retain the coalescer across loop iterations, render a stream's first delta immediately, flush later compatible deltas at/before 16 ms, flush before lifecycle terminals, coalesce Thinking by stream and tool progress by call ID, and use a 1,024-capacity interruptible core→projector queue.
- [x] Preserve control/data fairness, cancel tombstones, terminal accounting suffixes and disconnect final flush.
- [x] Run focused stress/cadence/cancellation tests three times and compare to `baseline.md`; require terminal loss 0 and bounded high-water.
- [x] Run `cargo test -p slim-core -p slim-tui -p slim-cli`.
- [x] Review for deadlock, producer starvation, timer polling, reordered lifecycle, cancel latency and retained-buffer growth. Fix and rerun until clean.
- [x] Append Slice 7 evidence to tracker §7.

### Task 8: Gate the deployed binary through ConPTY and an offline provider

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §28.4
- Modify: `crates/slim-cli/tests/tui_pty.rs`
- Create: `analysis_outputs/streaming-thinking-tools-tui/conpty/` captures

- [x] Refactor the harness to resolve `SLIM_E2E_EXE` first and fall back to `CARGO_BIN_EXE_slim`; fail with the exact resolved path and hash when startup fails.
- [x] Add an in-process loopback provider fixture that emits controlled reasoning→tool→reasoning→answer with two homonymous calls, paged output and delays. No real credential or commercial provider is allowed.
- [x] RED: prove the old ignored `80×24` Cargo-binary test cannot satisfy the explicit-path/matrix contract.
- [x] GREEN: drive the deployed `C:\Users\User\bin\Slim.exe` at `120×30`, `80×24`, `60×16`, `40×10`, `32×10`; cover normal/reduced-motion/NO_COLOR where observable; type prompts/selection/Enter; assert alternate-screen enter/restore, causal labels, call identity and detail expansion. The current host emitted only the ConPTY probe, so frame assertions remain runnable but physically unobserved here.
- [x] Persist sanitized raw VT plus normalized text/diffs under the ConPTY evidence directory. Keep the real-console limitation explicit if the current host cannot create ConPTY output; the harness itself must remain runnable by explicit command.
- [x] Run the ConPTY gate on the current host and capture its truthful outcome. Do not remove `#[ignore]` unless it is reliable in ordinary non-console `cargo test`.
- [x] Review watchdogs, cleanup, secret isolation, executable identity and dimension matrix. Fix and rerun until clean.
- [x] Append Slice 8 evidence to tracker §7.

### Task 9: Full regression, documentation, deployment and visual smoke

**Files:**
- Modify: live docs and test-count references listed in the file map

- [x] Re-read the complete diff against the audit's eight findings and the preserved normative constraints. Search for name-only tool correlation, implicit Thinking closure, legacy untyped request projection, unbounded core projector channels and unused coalescer deadlines.
- [x] Run `cargo fmt --all -- --check` and fix formatting only in touched Rust files.
- [x] Set the active rustc and run `cargo test --workspace`; record exact suite/test/ignored/warning totals from fresh output and verify exit code without a filtering pipeline.
- [x] Run the reproducible Slice 6/7 measurements and the Slice 8 explicit deployed-binary harness after deployment.
- [x] Update `DESIGN-SLIM-TUI.md` §1.1 and tracker §7 with final behavior/evidence. Search every mandated documentation file for stale test totals and update only verified numbers.
- [x] Run `.\refresh-slim.ps1 -Test`; require the script's final `OK:`. Record deployed path, size, timestamp, version and SHA-256 and compare it to `target\release\slim.exe`.
- [x] Run the offline visual smoke against the deployed binary. The portable fixture/goldens cover the wide/narrow matrix and normalized artifacts; on this host, the physical ConPTY emitted only the 4-byte cursor-position probe (`ESC[6n`), no frame, so the real-console hierarchy inspection remains explicitly unobserved and the test stays `#[ignore]`.
- [x] Fill and copy the complete `RULES.md` §4 checklist in the final response. Do not mark the goal complete while any checkbox, failed test, warning, deploy mismatch or undocumented gap remains.
