# Codex Output-Cap Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop Slim Codex-subscription requests from sending the unsupported `max_output_tokens` field while preserving the local context reserve.

**Architecture:** Keep output-cap parsing and `AgentLoopConfig::context_reserve_tokens` unchanged. Change only `OpenAiCodexAdapter` wire serialization to match Pi's Codex adapter, which omits the field; OpenAI-compatible and Anthropic retain their existing caps.

**Tech Stack:** Rust 2021, serde_json, reqwest fixture server, Cargo test/Clippy, PowerShell deploy.

---

### Task 1: Record contract correction and prove RED

**Files:**
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` (§5)
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` (§1.1/provider contract)
- Modify: `crates/slim-core/tests/provider_adapters.rs:958-987`
- Modify: `crates/slim-core/tests/agent_loop.rs:520-590`

- [x] **Step 1: Register the discovered bug before correction**

Add G191 to tracker §5: Codex subscription sends `max_output_tokens`, while Pi's dedicated Codex contract omits it and the live backend rejects it with HTTP 400. Mark unresolved until GREEN.

- [x] **Step 2: Correct the normative design contract before code**

State that output cap remains a local reserve for Codex subscription; it is not serialized to `backend-api/codex/responses`.

- [x] **Step 3: Change the adapter regression to the desired contract**

Replace the Codex assertion in `output_cap_defaults_to_4096_and_can_be_set_explicitly` with:

```rust
let codex_body = serde_json::from_str::<serde_json::Value>(
    &codex.build_request("hello").body,
)
.expect("codex body");
assert!(codex_body.get("max_output_tokens").is_none());
```

Keep the OpenAI-compatible `"max_tokens":123` assertion unchanged.

- [x] **Step 4: Make the HTTP fixture reject the bad wire field**

In `codex_subscription_responses_execute_tool_and_send_function_output`, after separating headers/body, add:

```rust
let body = request
    .split_once("\r\n\r\n")
    .map(|(_, body)| body)
    .expect("HTTP body");
assert!(!body.contains("\"max_output_tokens\""));
```

- [x] **Step 5: Run RED and confirm the exact failure**

Run:

```powershell
$env:RUSTC='C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'
cargo test -p slim-core --test provider_adapters output_cap_defaults_to_4096_and_can_be_set_explicitly -- --exact
```

Expected: FAIL because current Codex JSON contains `max_output_tokens`.

### Task 2: Apply the minimal provider fix

**Files:**
- Modify: `crates/slim-core/src/provider/codex.rs:82-96`
- Test: `crates/slim-core/tests/provider_adapters.rs`
- Test: `crates/slim-core/tests/agent_loop.rs`

- [x] **Step 1: Remove only the unsupported wire property**

Delete this entry from the Codex JSON literal:

```rust
"max_output_tokens": self.config.max_output_tokens(),
```

Do not change `ProviderConfig`, `ProviderRunOptions`, `resolve_max_output_tokens`, or `context_reserve_tokens`.

- [x] **Step 2: Run focused GREEN**

```powershell
cargo test -p slim-core --test provider_adapters output_cap_defaults_to_4096_and_can_be_set_explicitly -- --exact
cargo test -p slim-core --test agent_loop codex_subscription_responses_execute_tool_and_send_function_output -- --exact
```

Expected: both PASS.

- [x] **Step 3: Run provider suites**

```powershell
cargo test -p slim-core --test provider_adapters
cargo test -p slim-core --test agent_loop
cargo test -p slim-core --test provider_http
```

Expected: 0 failed, 0 warnings.

### Task 3: Close docs, verify workspace, deploy

**Files:**
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` (§5/§7)
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` (§1.1)
- Modify as counts require: `README.md`, `Documentações - Projeto/README.md`, `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`, `release/README.md`
- Modify: `docs/superpowers/specs/2026-08-23-codex-output-cap-compat-design.md`
- Modify: this plan

- [x] **Step 1: Mark G191 resolved and correct historical G129 wording**

G129 must no longer claim Codex should serialize `max_output_tokens`; record that the public cap remains local budget state.

- [x] **Step 2: Run quality gates**

```powershell
cargo fmt --all -- --check
cargo clippy -p slim-core --lib -- -D warnings
cargo clippy -p slim-tui --all-targets -- -D warnings
cargo test --workspace -j 1 --no-fail-fast
git diff --check
```

Expected: all exit 0; zero compiler warnings; only documented physical ConPTY ignore remains. Full-workspace Clippy is not a project gate because unrelated historical test targets still carry lint debt; changed production/provider targets and the canonical TUI gate must be clean.

- [x] **Step 3: Count suites/tests from fresh output and synchronize every citation**

Search all living docs for old totals and update only from observed output.

- [x] **Step 4: Build, deploy, and smoke with canonical script**

```powershell
.\refresh-slim.ps1 -Test
```

Expected: script prints `OK:` and copies release to `C:\Users\User\bin\Slim.exe`.

- [x] **Step 5: Verify deployed artifact**

```powershell
slim --version
Get-FileHash target\release\slim.exe -Algorithm SHA256
Get-FileHash C:\Users\User\bin\Slim.exe -Algorithm SHA256
```

Expected: version exit 0 and hashes identical.

- [x] **Step 6: Final regression evidence**

Re-run the adapter omission assertion and
`codex_subscription_responses_execute_tool_and_send_function_output`. The
localhost HTTP fixture must inspect every received body, fail if
`max_output_tokens` exists, and complete two Codex turns otherwise. Do not call
a live provider in automated gates or expose OAuth credentials.
