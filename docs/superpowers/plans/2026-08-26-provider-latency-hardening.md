# Provider Latency Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove Slim-owned hidden waits by preserving compacted history, honoring known model limits, making provider phases visible, and eliminating avoidable catalog, transport, and timeout latency.

**Architecture:** Keep the provider loop authoritative for the canonical conversation returned to the TUI. Resolve model limits in one CLI function, serve Codex metadata cache-first, reuse one process HTTP transport, and publish content-free provider phase/timing events through the existing ordered event stream.

**Tech Stack:** Rust, Tokio, Reqwest, Ratatui event reducer, hermetic TCP fixtures, Cargo workspace tests.

---

### Task 1: Persist canonical post-compaction conversation

**Files:**
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Modify: `crates/slim-cli/src/tui.rs`
- Test: `crates/slim-cli/tests/tui_bridge.rs`

- [x] Add a failing two-prompt TUI regression whose provider fixture forces one compaction and proves the next prompt starts from the returned summary instead of the original oversized history.
- [x] Run `cargo test -p slim-cli --test tui_bridge <test-name> -- --exact --nocapture` and record RED with the repeated summary request.
- [x] Make `Runtime` retain the canonical provider conversation and append the final assistant turn:

```rust
pub fn conversation(&self) -> &[ProviderMessage] {
    &self.conversation
}
```

- [x] Add canonical `history: Option<Vec<ProviderMessage>>` to `ProviderExecution` (synthetic paths remain `None`) and replace the TUI's reconstructed user/assistant pair when present.
- [x] Re-run the focused regression and existing TUI bridge tests to GREEN.

### Task 2: Resolve known model limits centrally

**Files:**
- Modify: `crates/slim-cli/src/headless.rs`
- Test: `crates/slim-cli/tests/provider_cli.rs`
- Test: `crates/slim-cli/tests/tui_bridge.rs`

- [x] Add failing tests for ClinePass and Command Code asserting their selected model metadata reaches `ContextSnapshot` instead of the 32,000-token fallback.
- [x] Run both tests and record RED.
- [x] Implement one precedence-ordered resolver: explicit option, environment override, OpenCode metadata, Codex live/bundled metadata, ClinePass metadata, Command Code metadata, then the safe generic default.
- [x] Re-run focused provider and TUI tests to GREEN.

### Task 3: Cache Codex metadata without blocking prompts

**Files:**
- Modify: `crates/slim-cli/src/codex_catalog.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Test: `crates/slim-cli/tests/codex_catalog.rs`

- [x] Add a failing offline test that seeds a valid disk cache and loads it while the network endpoint is unavailable.
- [x] Run the test and record RED.
- [x] Add bounded, schema-validated disk reads and return cached entries immediately; refresh live metadata only outside the prompt-critical path.
- [x] Re-run the catalog tests to GREEN and verify malformed/oversized caches fail closed to bundled metadata.

### Task 4: Reuse transport and split timeout policies

**Files:**
- Modify: `crates/slim-core/src/provider.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Test: `crates/slim-core/tests/provider_http.rs`

- [x] Add a failing keep-alive fixture proving two separately constructed provider adapters can share one TCP connection.
- [x] Add a failing periodic-chunk fixture proving active streams are governed by idle timeout rather than the old 120-second wall deadline.
- [x] Add `ProviderTimeouts::production(idle)` with bounded connect, independent idle, and a longer configurable wall deadline.
- [x] Add a shared Reqwest transport constructor and use it from every headless provider branch while keeping adapter/auth state per request.
- [x] Re-run `provider_http` and provider CLI tests to GREEN.

### Task 5: Publish truthful provider phases and early tool preparation

**Files:**
- Modify: `crates/slim-core/src/provider.rs`
- Modify: `crates/slim-core/src/events.rs`
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Test: `crates/slim-core/tests/provider_http.rs`
- Test: `crates/slim-core/tests/agent_loop.rs`
- Test: `crates/slim-tui/tests/m2_integration.rs`

- [x] Add failing tests for `Compacting`, `Connecting`, first-byte timing, and `Preparing tool` on the first identity fragment without exposing arguments.
- [x] Run focused tests and record RED.
- [x] Add serializable content-free provider phase/timing events and project them to `UiEvent::ActivityChanged`.
- [x] Emit `Compacting` before the summary call, transport phases around headers/first byte/first semantic event, and `Preparing tool` on the first tool identity fragment.
- [x] Re-run focused core/TUI tests to GREEN.

### Task 6: Documentation, full validation, deploy, and cleanup

**Files:**
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify test-count references only where current counts are present: `README.md`, `Documentações - Projeto/README.md`, `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`, `release/README.md`

- [x] Run formatting, focused suites, `cargo test --workspace`, and count current result lines/passes/ignored/warnings from this session.
- [x] Update the tracker execution log, behavioral checkpoint, and every current test-count reference found by grep.
- [x] Run `./refresh-slim.ps1 -Test` and require `OK:`.
- [x] Compare release and deployed binary version, size, timestamp, and SHA-256.
- [x] Inventory generated old build directories and crash logs, preserve the newly deployed `C:\Users\User\bin\Slim.exe`, remove only verified generated predecessors, and re-check that no old build/crash artifact remains.
