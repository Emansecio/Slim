# Slim Durable Harness v2 — Wave 1 Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Freeze the Slim durable-session contract, pin the current failure boundaries, and land the smallest Windows-only safeguards needed before the v2 repository and reducer are implemented.

**Architecture:** The existing `Runtime`, `AppHandle`, and v1 `SessionWriter` remain the only production path. Wave 1 adds an unwired v2 schema contract, pure legacy-format inspection, focused characterization tests, a conservative provider-length gate, and exclusive writer ownership plus torn-tail repair. It does not add resume, a second runtime, Memory/JSONL repositories, background work, or TUI integration.

**Tech Stack:** Rust 2021, `serde`, `serde_json`, Tokio/Reqwest fixtures already present in the workspace, Windows `OpenOptionsExt`, JSONL, Cargo test.

---

## File ownership

- Create `Documentações - Projeto/HARNESS-V2-TRACKER.md`: canonical 10-stage status and evidence log.
- Modify `Documentações - Projeto/README.md`: add one canonical link to the tracker.
- Create `crates/slim-core/src/session/schema_v2.rs`: v2 envelopes and durable record types only.
- Create `crates/slim-core/src/session/inspection.rs`: read-only v1/v2 format inspection.
- Modify `crates/slim-core/src/session/mod.rs`: exports without changing the v1 writer version.
- Create `crates/slim-core/tests/session_schema_v2.rs`: JSON goldens and legacy inspection contract.
- Modify `crates/slim-core/tests/session_recovery.rs`: writer exclusion and torn-tail reopen characterization.
- Modify `crates/slim-core/src/session/event_log.rs`: sibling lock handle and repair-before-append.
- Modify `crates/slim-core/src/session/recovery.rs`: pure prefix parse shared by recovery and writer repair.
- Modify `crates/slim-core/tests/agent_loop.rs`: `length` plus valid tool call must cause zero side effects.
- Modify `crates/slim-core/src/runtime/mod.rs`: conservative tool gate based on the observed stop reason.
- Update the tracker and existing status documents only with evidence produced by this execution.

`crates/slim-tui/**` is excluded because it contains unrelated WIP and the durable snapshot contract is not part of Wave 1.

### Task 1: Create the canonical tracker

- [x] **Step 1: Add the 10-stage checklist**

Create `HARNESS-V2-TRACKER.md` with one checkbox per stage, dependency gates, explicit exclusions, Wave 1 subchecks, and an append-only evidence table.

- [x] **Step 2: Link it from the canonical index**

Add the tracker after the existing TUI tracker in `Documentações - Projeto/README.md`; do not rewrite the current implementation summary.

- [x] **Step 3: Verify links and forbidden placeholders**

Run:

```powershell
$tracker = 'Documentações - Projeto\HARNESS-V2-TRACKER.md'
$index = 'Documentações - Projeto\README.md'
$plan = 'docs\superpowers\plans\2026-08-22-slim-harness-v2-wave-1.md'
foreach ($path in @($tracker, $index, $plan)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Missing expected documentation file: $path"
    }
}

$trackerText = Get-Content -Raw -LiteralPath $tracker
$indexText = Get-Content -Raw -LiteralPath $index
if (-not $trackerText.Contains('[plano Slim Durable Harness v2 — Onda 1](../docs/superpowers/plans/2026-08-22-slim-harness-v2-wave-1.md)')) {
    throw 'Tracker does not link to the implementation plan'
}
if (-not $indexText.Contains('[HARNESS-V2-TRACKER.md](HARNESS-V2-TRACKER.md)')) {
    throw 'Canonical index does not link to the harness tracker'
}

$placeholderPattern = '(?m)^\s*[-*]\s+.*(?:\bTBD\b|\bTODO\b|(?i:implement later|fill in details))'
$placeholderMatches = Select-String -CaseSensitive -Path $tracker, $index -Pattern $placeholderPattern
if ($placeholderMatches) {
    $placeholderMatches | ForEach-Object { Write-Error $_.Line }
    throw 'Placeholder found in the new tracker or canonical index'
}
Write-Output 'OK: documentation files, links, and placeholders'
```

Expected: the three files exist, both links resolve in their source text, and the placeholder scan examines only the new tracker and canonical index. It must not scan this plan, so the command's own literals and pre-existing TUI `TODO` text are outside the check.

### Task 2: Freeze the unwired v2 schema and legacy-inspection contract

- [x] **Step 1: Write the failing public-contract test**

Create `session_schema_v2.rs` using these public interfaces:

```rust
use slim_core::session::{
    inspect_session, DurableEntry, DurableEntryRole, DurableFact, DurableOperation,
    DurableOperationKind, DurableRecord, DurableSessionHeader, DurableUsage,
    ReplayPolicy, SessionFormat, DURABLE_SCHEMA_VERSION,
};
```

The test must serialize one record of each outer kind (`entry`, `operation`, `fact`, `usage`), assert a shared monotonic `seq` envelope, and prove a v1 file is classified without byte changes or `.quarantine` creation.

- [x] **Step 2: Run RED**

```powershell
cargo test -p slim-core --test session_schema_v2
```

Expected: compilation fails because the v2 public contract does not exist.

- [x] **Step 3: Add the minimum contract**

`schema_v2.rs` must define:

```rust
pub const DURABLE_SCHEMA_VERSION: u32 = 2;

pub struct DurableSessionHeader {
    pub schema_version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_id: Option<String>,
    pub cutoff_seq: Option<u64>,
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum DurableRecord {
    Entry { seq: u64, entry: DurableEntry },
    Operation { seq: u64, operation: DurableOperation },
    Fact { seq: u64, fact: DurableFact },
    Usage { seq: u64, usage: DurableUsage },
}
```

The nested types must use stable IDs, explicit operation/attempt correlation, `ReplayPolicy::{Never, Safe}`, optional token counts for unknown usage, and no lane/server/lease fields. `inspection.rs` must parse only the first JSONL line and return `SessionFormat::{LegacyV1, DurableV2}` without writing.

Do not change `CURRENT_SCHEMA_VERSION`: the v1 writer remains active until Stage 2 of the tracker.

- [x] **Step 4: Run GREEN**

```powershell
cargo test -p slim-core --test session_schema_v2
```

Expected: all tests in the target pass.

### Task 3: Enforce one writer and repair a torn tail before append

- [x] **Step 1: Write one failing writer-exclusion test**

Open a `SessionWriter`, then open a second writer for the same path while the first handle is alive. On Windows the second open must fail; after dropping the first, reopening must succeed.

- [x] **Step 2: Run RED for writer exclusion**

```powershell
cargo test -p slim-core --test session_recovery competing_writer_is_rejected_until_owner_drops
```

Expected: FAIL because the current writer allows a competing append handle.

- [x] **Step 3: Implement the sibling lock**

Acquire `<session>.lock` before recovery/open, use Windows `OpenOptionsExt::share_mode(FILE_SHARE_READ)`, and retain the lock `File` inside `SessionWriter`. Keep the sentinel file; ownership is the live handle, not file existence. Add no dependency.

- [x] **Step 4: Run GREEN for writer exclusion**

Run the same command. Expected: PASS.

- [x] **Step 5: Write one failing torn-tail reopen test**

Persist event 1, append an incomplete JSON fragment, reopen the writer, append event 2, and assert recovery returns both events while `.quarantine` contains the exact torn bytes.

- [x] **Step 6: Run RED for repair-before-append**

```powershell
cargo test -p slim-core --test session_recovery reopening_after_torn_tail_repairs_before_append
```

Expected: FAIL because the current writer appends behind the malformed suffix.

- [x] **Step 7: Implement pure prefix parsing and truncation**

Recovery must distinguish an incomplete final line from a malformed complete/interior line. Only the incomplete tail is quarantined. The writer truncates to the valid byte offset while holding the lock, then opens append and synchronizes subsequent records.

- [x] **Step 8: Run GREEN**

```powershell
cargo test -p slim-core --test session_recovery
```

Expected: all session recovery tests pass.

### Task 4: Block tools from a length-truncated provider turn

- [x] **Step 1: Write the failing side-effect test**

Add an offline SSE fixture returning a valid `write` call followed by `finish_reason: "length"`. Run the public agent-loop API in Auto mode and assert the destination file does not exist and no `ToolStarted` event was emitted.

- [x] **Step 2: Run RED**

```powershell
cargo test -p slim-core --test agent_loop length_stop_blocks_all_tool_side_effects
```

Expected: FAIL because the current loop executes calls based only on `calls.is_empty()`.

- [x] **Step 3: Add the minimum conservative gate**

Classify the raw provider stop reason before redaction and carry only a private blocking boolean alongside the next sequence number. If it is `length` or `max_tokens`, clear the pending calls before any tool limit or execution path while keeping `AssistantEnded.reason` redacted. Preserve the existing event stream and public result types; do not implement retry or a second provider abstraction in this task.

- [x] **Step 4: Run GREEN and adjacent loop tests**

```powershell
cargo test -p slim-core --test agent_loop
```

Expected: the new test and every pre-existing agent-loop test pass.

### Task 5: Integrate and record Wave 1 evidence

- [x] **Step 1: Format only touched Rust files**

Run formatting only for the Rust files owned by this plan, then inspect their diff:

```powershell
$rustFiles = @(
    'crates\slim-core\src\session\schema_v2.rs',
    'crates\slim-core\src\session\inspection.rs',
    'crates\slim-core\src\session\mod.rs',
    'crates\slim-core\tests\session_schema_v2.rs',
    'crates\slim-core\tests\session_recovery.rs',
    'crates\slim-core\src\session\event_log.rs',
    'crates\slim-core\src\session\recovery.rs',
    'crates\slim-core\tests\agent_loop.rs',
    'crates\slim-core\src\runtime\mod.rs'
)
rustfmt --edition 2021 $rustFiles
if ($LASTEXITCODE -ne 0) { throw 'rustfmt failed for an owned Rust file' }
git diff --check -- $rustFiles
if ($LASTEXITCODE -ne 0) { throw 'diff check failed for an owned Rust file' }
```

Do not format the workspace globally; inspect the diff and preserve unrelated user work.

- [x] **Step 2: Run focused gates**

```powershell
cargo test -p slim-core --test session_schema_v2 --test session_recovery --test session_branch --test provider_fake --test agent_loop
```

- [x] **Step 3: Run the workspace gate and count current results from output**

```powershell
cargo test --workspace
```

Expected: exit 0, zero failures, zero warnings. Count test results from this run before editing any documented total.

- [x] **Step 4: Update documentation from actual evidence**

Mark only completed Wave 1 checks, append commands/results to `HARNESS-V2-TRACKER.md`, and update every existing test-count reference identified by `rg` if the canonical count changed. Add the required Onda 1 execution line to §7 of `AUDIT-SLIM-TUI-TRACKER.md`; for each completed slice, record the real Rust changes made by that slice. If a slice is documentation-only, state that narrowly; state "sem mudança visual/TUI" only when applicable. Do not change TUI behavior claims.

- [x] **Step 5: Build, deploy, and smoke-test the actual executable**

```powershell
.\refresh-slim.ps1 -Test
```

Expected: the script prints `OK:` after tests, release copy, and smoke test.

- [x] **Step 6: Final self-review**

Verify no SQLite/server/multi-lane/background/public-extension fields or dependencies were introduced, v1 remains the active writer, `slim-tui/**` user changes are untouched, and every completed tracker checkbox has command or code evidence.
