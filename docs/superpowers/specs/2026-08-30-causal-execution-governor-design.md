# Causal Execution Governor Design

## Objective

Add a native, pre-execution causal governor to Slim that recognizes equivalent tool calls, tracks real progress, and records likely redundant work. The first delivery runs only in shadow mode: it observes and persists telemetry without reusing results, blocking calls, warning the model, or stopping the loop.

## Non-goals

- No active result reuse, rejection, warning, or `NoProgress` stop.
- No generic shell memoization.
- No prompt or tool-call token overhead.
- No cross-session or distributed cache.
- No workspace-wide content hash, filesystem watcher, or Git subprocess.
- No semantic intent inference from assistant prose.
- No replacement of the existing `LoopGuard` in this delivery.
- No new test framework or broad runtime refactor.

## Acceptance criteria

1. Every provider-driven tool call can be classified before execution from native operational metadata.
2. Equivalent calls use canonical arguments plus causal dependency state, not raw argument strings alone.
3. A per-agent-loop Progress Ledger records workspace revisions, observed dependencies, evidence, failures, validations, and stagnation.
4. Actual byte changes, new stable evidence, changed diagnostics, first distinct failures, external input, and externally changed observed dependencies count as progress.
5. Identical output, identical failures, unchanged successful writes, and textual todo/plan changes do not count as progress.
6. Unknown or volatile shell activity invalidates equivalence without being falsely labelled progress.
7. Shadow mode records anomalies and progress but preserves tool execution, tool results, model context, stop reasons, and current `LoopGuard` behavior.
8. Persisted governor events contain bounded identifiers and hashes, never new raw arguments or output.
9. Only directly relevant focused tests and checks run.

## Architecture

Add `crates/slim-core/src/runtime/governor.rs`. A `CausalGovernor` owns one `ProgressLedger` for one agent-loop execution. Runtime creates it at loop start and passes provider tool calls through a pre/post observation pair:

1. `observe_before` resolves the operational spec, canonicalizes the call, snapshots focused dependencies, and identifies prior equivalent evidence.
2. Existing tool execution proceeds unchanged.
3. `observe_after` fingerprints the redacted result and post-call dependencies, updates revisions and evidence, and emits zero or more shadow observations.
4. Turn completion records stagnation only when every call was classifiable and no causal boundary made the conclusion unsafe.

Parallel read batches are observed before launch and consolidated in original provider-call order after completion. The governor does not become a general tool middleware: runtime must retain turn context and cover runtime-only tools such as `ask_question`, `todo`, `skill`, and `code_intel`.

## Operational metadata

Extend native `ToolSpec` with:

- `effect_class`
- `cacheability`
- `volatility`
- `dependency_scope`
- `replay_policy`

Workspace mutation becomes a derived property of `effect_class`; existing capability metadata should consume the same native source when practical instead of maintaining another name-based mutation table.

Runtime-only tool classes:

| Tool | Effect class | Dependency scope | Policy |
| --- | --- | --- | --- |
| `ask_question` | Interaction | Interaction epoch | Never replay |
| `todo` | Internal state | Internal epoch | Never replay |
| `skill` | Potentially volatile | Unknown | Never replay |
| `code_intel` | Snapshot read | Observed workspace/LSP state | Evidence candidate |

Native baseline:

| Tool | Effect class | Dependency scope | Policy |
| --- | --- | --- | --- |
| `read` | Snapshot read | Target file | Evidence candidate |
| `list` | Snapshot read | Immediate directory | Evidence candidate |
| `search` | Snapshot read | Observed workspace | Evidence candidate |
| `write`, `patch` | Workspace mutation | Target file | Never replay |
| `shell` | Potentially volatile | Unknown | Never replay |

A conservative native shell classifier may override `shell` to Validation only for allowlisted command forms. Initial allowlist covers direct project checks such as Cargo test/check/clippy/fmt-check and equivalent existing project commands. Pipes, redirections, command chaining, command substitution, environment mutation, package installation, network commands, and unknown wrappers remain potentially volatile.

## Canonical calls

The call fingerprint is SHA-256 over a versioned encoding of:

```text
tool name
canonical arguments
effect class
dependency scope
dependency fingerprint
workspace revision
uncertainty epoch
```

Canonical arguments use recursively sorted JSON object keys and preserve array order. Tool-specific normalization materializes semantic defaults and normalizes declared path fields relative to the workspace. Existing paths may use filesystem canonicalization; missing targets use lexical normalization. Path separators are normalized. Generic shell whitespace, quoting, and command order are never rewritten.

Malformed or unsupported arguments are marked unclassifiable and execute normally.

## Causal workspace state

The ledger tracks:

- monotonic logical workspace revision;
- uncertainty epoch;
- interaction/internal epochs;
- focused fingerprints for observed files and directories;
- evidence records keyed by canonical call and causal state;
- normalized failure and validation records;
- consecutive stagnant-turn count.

Focused dependency rules:

- File tools hash the target bytes, with explicit absent/unreadable states.
- Directory listing fingerprints immediate entries and metadata.
- Search and validation use the logical revision plus fingerprints of already observed files; they do not trigger a fresh workspace scan.
- Code intelligence uses observed workspace state plus available LSP state in its result evidence.
- A changed focused dependency detected before execution advances the logical revision.
- Unknown shell or skill activity advances the uncertainty epoch. It invalidates later equivalence but does not claim workspace progress.

Broad-scope decisions remain shadow candidates because unobserved external changes cannot be ruled out. Future enforcement must not promote them without stronger dependency evidence.

## Evidence and progress

Evidence IDs are SHA-256 digests of bounded, redacted `ToolResult` semantics: tool, success state, normalized output or error, and causal class. The in-memory ledger may retain a bounded preview and existing artifact handle for future analysis; persisted events contain only IDs and bounded enums/counts.

Progress kinds:

- `WorkspaceChanged`: target bytes differ before and after a known mutation.
- `NewEvidence`: a stable read produced previously unseen evidence.
- `DiagnosticsChanged`: diagnostic evidence changed.
- `DistinctFailure`: first occurrence of a normalized failure in this causal state.
- `ExternalInput`: new user/interaction input.
- `DependencyChanged`: an observed dependency changed externally.

Not progress:

- repeated identical output;
- repeated identical normalized failure;
- successful write/patch with identical before/after bytes;
- unchanged green validation;
- todo/plan text changes.

Potentially volatile activity produces a causal boundary. A boundary suspends high-confidence stagnation classification for that turn.

## Shadow decisions

Anomaly kinds:

- `ReusableEvidence`
- `RepeatedFailure`
- `RedundantValidation`
- `StagnantTurn`
- `NoProgressCandidate`

Simulated escalation for one causal chain:

- first repetition: `WouldReuse` or `WouldReject`;
- second repetition: `WouldWarn`;
- third persistent repetition: `WouldStop`.

Turn escalation:

- first stagnant turn: observe;
- second consecutive stagnant turn: `WouldWarn`;
- third consecutive stagnant turn: `WouldStop(NoProgress)`.

Any real progress resets stagnation. Shadow decisions never alter execution in this delivery.

## Telemetry

Add persisted event variants:

- `CausalProgressObserved`
- `CausalBoundaryObserved`
- `CausalAnomalyDetected`

Payloads include bounded enums, tool name, call/evidence fingerprints, workspace revision, occurrence count, confidence, and simulated action. They contain no new raw command, argument, path content, result, or diagnostic text.

Persist only anomalies, real progress, and causal boundaries. Normal governor decisions remain in memory and rely on existing tool lifecycle events for surrounding context. Detailed events map to no new TUI surface.

## Failure handling

- Fingerprint or dependency read failure marks the call unclassifiable; tool execution proceeds.
- Poisoned internal locks are not needed because the ledger is execution-local and single-owner.
- Hashing is bounded by focused dependencies and existing tool output bounds.
- Unknown shell syntax remains volatile.
- Cancellation preserves existing runtime behavior; partial calls do not produce reusable evidence.
- Existing `LoopGuard`, budgets, cancellation, artifact materialization, LSP synchronization, and stop reasons remain authoritative.

## Rollout

This specification implements phase 1 only: native shadow telemetry. Later phases require a reviewed real-session corpus and separate acceptance:

1. opt-in exact snapshot reuse;
2. opt-in repeated-failure rejection;
3. model-visible warnings;
4. high-confidence `NoProgress` enforcement.

Each phase must be independently reversible. Broad-scope search/validation candidates require stronger dependency guarantees before active reuse.

## Files

Expected minimal changes:

- `crates/slim-core/src/runtime/governor.rs`: canonicalization, ledger, classification, observations.
- `crates/slim-core/src/runtime/mod.rs`: lifecycle integration.
- `crates/slim-core/src/tools/mod.rs`: operational metadata.
- `crates/slim-core/src/events.rs`: persisted event schema.
- `crates/slim-tui/src/api.rs`: exhaustive no-UI mapping.
- Directly affected existing tests only.

## Verification

Add only focused regressions that prove:

- semantically equivalent JSON/path inputs share a canonical identity;
- unchanged successful mutation is not progress while changed bytes are;
- repeated stable evidence/failure/validation produces the expected shadow anomaly;
- shadow integration executes every call and leaves results/stops unchanged.

Run those focused tests, relevant existing agent-loop/tool tests, formatter checks for touched files, `cargo check -p slim-core -p slim-tui`, and Clippy for touched crates. Use `env -u RUSTC -u CARGO` because inherited overrides currently reference a removed toolchain.
