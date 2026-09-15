# Real TUI Integration Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Replace demo route with fullscreen TUI connected to existing provider/agent/tool runtime, including live event projection, usage, cancellation, and terminal restoration.

**Architecture:** `slim-cli` remains composition root. `slim-core` publishes normalized `SessionEvent`s while SSE arrives; `slim-cli` projects them to TUI channels; `slim-tui` owns input/reducer/render only. Existing headless and TUI use one provider execution helper.

**Tech Stack:** Rust stable MSVC, Tokio, std channels, Reqwest SSE, Ratatui, Crossterm, localhost TCP fixtures.

**Cargo setup:** before commands, run:

```bash
export PATH=/c/Users/User/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin:$PATH
unset RUSTC
export RUSTC_WRAPPER=
```

Every Cargo command below uses `cargo.exe` from that MSVC toolchain.

**Historical workspace note:** the original execution described a non-Git workspace. The current checkout uses Git; inspect its status and preserve WIP before editing. The toolchain commands above also belong to that execution; use current `RULES.md` R8 when Cargo is needed.

---

### Task 1: Complete existing RED projection slice

**Files:**
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Create: `crates/slim-cli/src/tui.rs`
- Modify: `crates/slim-cli/src/lib.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Test: `crates/slim-cli/tests/tui_runtime.rs`

- [ ] **Step 1: Preserve and run existing failing test**

Run:

```bash
cargo.exe test -p slim-cli --test tui_runtime
```

Expected: compile failure for missing `run_provider_tui_turn` and `UiEvent::Usage`.

- [ ] **Step 2: Add usage event/state**

Add API shape:

```rust
UiEvent::Usage { input_tokens: u32, output_tokens: u32 }
```

Map `EventKind::Usage` directly to it. Add `usage_input_tokens` and `usage_output_tokens` to `AppState`; saturating-add values; show totals in operational line.

- [ ] **Step 3: Extract shared provider execution result**

Create internal execution shape in `headless.rs`:

```rust
pub(crate) struct ProviderExecution {
    pub result: ProviderHeadlessResult,
    pub events: Vec<SessionEvent>,
}

pub(crate) fn execute_provider_turn(
    request: ProviderRequest,
    session_path: Option<&Path>,
    options: ProviderRunOptions,
) -> Result<ProviderExecution, ProviderError>;
```

Move current runtime/provider/tool logic from `run_provider_headless_inner` into this function. Existing headless wrappers return `.result`; session writer receives same event vector before rendering. Do not change headless output or exit codes.

- [ ] **Step 4: Implement projection helper**

In `tui.rs`:

```rust
pub fn run_provider_tui_turn(
    request: ProviderRequest,
    options: ProviderRunOptions,
) -> Result<Vec<UiEvent>, ProviderError>
```

Return `UserMessageAdded` followed by every projectable core event in sequence. Do not synthesize provider/tool success.

- [ ] **Step 5: Run RED test green plus headless regressions**

```bash
cargo.exe test -p slim-cli --test tui_runtime
cargo.exe test -p slim-cli --test provider_cli --test headless_contract
```

Expected: PASS.

### Task 2: Publish normalized runtime events during SSE

**Files:**
- Modify: `crates/slim-core/src/model.rs`
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Test: `crates/slim-core/tests/provider_http.rs`
- Test: `crates/slim-core/tests/agent_loop.rs`

- [ ] **Step 1: Write observer-before-terminal failing test**

Fixture flow uses channels, not sleep:

1. server sends first content delta;
2. server waits on release channel before sending usage/stop/`[DONE]`;
3. runtime runs on worker thread;
4. test requires observer receive `AssistantTextDelta` before releasing server.

Expected before implementation: timeout waiting for observer because runtime currently normalizes after full response.

- [ ] **Step 2: Add best-effort AppHandle observer**

Add:

```rust
pub fn set_event_sender(&mut self, sender: std::sync::mpsc::Sender<SessionEvent>);
pub fn clear_event_sender(&mut self);
```

`push_event` validates sequence, stores event, then sends clone. Disconnected receiver clears sender and does not fail authoritative run. Implement `Debug`/`PartialEq` manually if sender prevents derives; equality ignores observer transport.

- [ ] **Step 3: Convert normalizer to stateful streaming**

Replace `normalize_provider_events(app, kind, Vec<ProviderEvent>, next_seq)` with internal state:

```rust
struct ProviderStreamNormalizer {
    kind: ProviderKind,
    next_seq: u64,
    openai_calls: Vec<BufferedToolCall>,
    anthropic_calls: Vec<BufferedToolCall>,
    standalone_calls: Vec<BufferedToolCall>,
    stopped: bool,
    error: Option<ProviderError>,
}
impl ProviderStreamNormalizer {
    fn push(&mut self, app: &mut AppHandle, event: ProviderEvent);
    fn finish(self) -> Result<u64, ProviderError>;
}
```

`Runtime::run_provider_messages()` calls `client.stream_messages(...)`. Text, reasoning and usage publish immediately. Tool calls publish only after complete valid JSON with preserved IDs. `finish` rejects missing stop/open fragments/recorded errors.

- [ ] **Step 4: Run focused tests**

```bash
cargo.exe test -p slim-core --test provider_http
cargo.exe test -p slim-core --test agent_loop
```

Expected: observer test PASS; existing malformed/incomplete/tool-ID tests PASS.

### Task 3: Add cancellable CLI-to-TUI runtime bridge

**Files:**
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-cli/Cargo.toml`
- Modify: `crates/slim-cli/src/tui.rs`
- Test: `crates/slim-cli/tests/tui_bridge.rs`

- [ ] **Step 1: Write failing command/event bridge test**

Required public boundary:

```rust
pub struct TuiRuntimeHandle {
    pub commands: std::sync::mpsc::Sender<UiCommand>,
    pub events: std::sync::mpsc::Receiver<UiEvent>,
}

pub fn spawn_tui_runtime(
    request: ProviderRequest,
    options: ProviderRunOptions,
) -> Result<TuiRuntimeHandle, ProviderError>;
```

Test sends `UiCommand::SendPrompt`, then asserts user event, first assistant delta, tool lifecycle, final text and usage in order.

- [ ] **Step 2: Add lifecycle API**

Add:

```rust
UiCommand::CancelRun
UiEvent::RunStarted
UiEvent::RunCancelled
UiEvent::RunFailed { message: String }
```

State transitions: started → `working=true`; ended/cancelled/failed → `working=false`.

- [ ] **Step 3: Implement worker with Tokio select**

Enable Tokio `sync` feature in `slim-cli`. Worker owns runtime future and command receiver. One run active. `CancelRun` aborts active JoinHandle; `Shutdown` aborts and exits. Duplicate `SendPrompt` while active emits visible notification and does not start second mutator.

AppHandle event sender feeds projector as events arrive. No sleeps or detached processes.

- [ ] **Step 4: Write and pass deterministic cancellation test**

Server sends headers then waits for socket close. Test waits for `RunStarted`, sends `CancelRun`, requires `RunCancelled`, then server observes EOF. Use channel deadlines only as deadlock guards.

```bash
cargo.exe test -p slim-cli --test tui_bridge
```

Expected: PASS, no hanging worker/server.

### Task 4: Run fullscreen application instead of demo

**Files:**
- Create: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/lib.rs`
- Modify: `crates/slim-tui/src/composer.rs`
- Modify: `crates/slim-tui/src/demo.rs`
- Modify: `crates/slim-tui/src/fullscreen.rs`
- Test: `crates/slim-tui/tests/m2_integration.rs`
- Test: `crates/slim-tui/tests/pty_windows.rs`

- [ ] **Step 1: Write reducer/composer failing tests**

Add `Composer::take_payload()` that returns exact payload and clears draft atomically. Test char input, atomic paste, backspace, Enter submission, newline fallback and draft preservation when command send fails.

- [ ] **Step 2: Add reusable app loop boundary**

```rust
pub struct UiChannels {
    pub commands: Sender<UiCommand>,
    pub events: Receiver<UiEvent>,
}

pub fn run_app(channels: UiChannels) -> io::Result<()>;
```

Loop drains events, maps key/paste/resize, reduces state, sends effects and draws. Poll interval is bounded for input responsiveness; no `sleep`.

- [ ] **Step 3: Implement key behavior**

- chars append;
- Backspace removes latest element/grapheme supported by current composer;
- Enter sends non-empty payload;
- Ctrl/Shift+Enter newline;
- Shift+Tab cycles mode and sends `SetMode`;
- Ctrl+C active cancels;
- Ctrl+C idle + empty draft shuts down;
- resize redraws;
- disconnected event channel exits through explicit shutdown.

- [ ] **Step 4: Keep terminal restore path**

`run_app` always invokes `FullscreenBackend::shutdown`; `TerminalGuard::Drop` remains fallback. If enablement after guard fails, restore before returning.

- [ ] **Step 5: Run TUI tests**

```bash
cargo.exe test -p slim-tui
```

Expected: PASS, including PTY/restore tests.

### Task 5: Share argument parsing and wire binary

**Files:**
- Modify: `crates/slim-cli/src/cli.rs`
- Modify: `crates/slim-cli/src/lib.rs`
- Modify: `crates/slim-cli/src/tui.rs`
- Modify: `crates/slim-cli/src/main.rs`
- Test: `crates/slim-cli/tests/cli_contract.rs`
- Test: `crates/slim-cli/tests/tui_runtime.rs`

- [ ] **Step 1: Write failing parse/routing tests**

Test parsed `--tui --provider openai-compatible --endpoint http://127.0.0.1:PORT --model fixture --session PATH --image PATH`; ensure `--tui` is not unknown and main no longer references `run_demo`.

- [ ] **Step 2: Extract parsed args**

Use one internal `ParsedCli` structure for headless/TUI. Preserve help/version/unknown option text and current provider/auth defaults. TUI starts without prompt; prompt comes from composer.

- [ ] **Step 3: Wire composition root**

`main.rs` calls `slim_cli::run_tui(args)` for TUI mode. `run_tui` resolves auth/images/options, spawns bridge, then calls `slim_tui::run_app`.

- [ ] **Step 4: Run CLI contracts**

```bash
cargo.exe test -p slim-cli --test cli_contract --test tui_runtime --test tui_bridge
```

Expected: PASS; grep finds no `slim_tui::demo::run_demo()` in binary path.

### Task 6: Verify milestone, reconcile docs, regenerate release

**Files:**
- Modify: `README.md`
- Modify: `POC-RESULTS.md`
- Modify: `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`
- Modify: `Documentações - Projeto/README.md`
- Modify: `release/manifest.json`
- Modify: `release/SHA256SUMS.txt`
- Modify: `release/slim-0.1.0-windows-x64.zip`

- [ ] **Step 1: Run focused local fixture proof**

```bash
cargo.exe test -p slim-cli --test tui_runtime --test tui_bridge
```

Expected: prompt → streamed delta/tool → result → final → usage; cancellation test PASS.

- [ ] **Step 2: Run full gates**

```bash
cargo.exe fmt --all -- --check
cargo.exe clippy --workspace --all-targets -- -D warnings
cargo.exe test --workspace
cargo.exe build --workspace --release
```

Expected: all exit 0. Any failure blocks completion claim.

- [ ] **Step 3: Update factual docs**

State TUI milestone exactly: real provider/tool/stream/cancel integration complete; cache, resume/branch, Skills, MCP, subagents and Todo/Plan/Goal remain pending. Update observed test count from actual output only.

- [ ] **Step 4: Rebuild deterministic release twice**

```bash
python3.exe release/build_release.py
sha256sum release/slim-0.1.0-windows-x64.zip
python3.exe release/build_release.py
sha256sum release/slim-0.1.0-windows-x64.zip
sha256sum -c release/SHA256SUMS.txt
```

Expected: both ZIP hashes identical; checksum verification `OK`.
