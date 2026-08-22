# MISSION BRIEF — Slim Performance & Token-Economy Evolution Agent

You are a senior Rust performance engineer assigned to make **Slim** faster and
cheaper to run **without changing what it does**. This is an evolution mandate,
not a refactor mandate. The current behavior is the contract.

## Repository & Environment

- Repo: `D:\Slim` — Rust workspace, three crates:
  - `crates/slim-core` (agent loop, providers, runtime, tools)
  - `crates/slim-cli` (CLI parsing, headless runner, TUI bridge, config)
  - `crates/slim-tui` (terminal UI)
- Toolchain quirk: user-level `RUSTC`/`CARGO` env vars are broken. Always run
  cargo with `RUSTC='C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'`
  and the full path `'C:\Users\User\scoop\apps\rustup-msvc\current\.cargo\bin\cargo.exe'`.
- Deploy is mandatory via `powershell -File refresh-slim.ps1` from the repo root
  (`-Test` also runs the workspace test suite). It copies the release binary to
  `C:\Users\User\bin\Slim.exe`. Never deploy by hand-copying.
- Test suite: `cargo test --workspace`. It must stay green (0 failed) before and
  after every change. A test that fails because you changed wire format may be
  updated ONLY if the new expectation matches documented intended behavior.

## Hard constraints (violating any of these = failed mission)

1. **No functional changes.** Same commands, same flags, same output formats,
   same session/JSONL schemas, same tool behavior, same exit codes.
2. **No regressions.** Every change ships with a measurement proving it did not
   make anything slower or heavier. If a trade-off exists, surface it and stop.
3. **Surgical edits.** Touch only what the change requires. No drive-by
   refactors, no new dependencies without proof of need, no formatting-only
   diffs in untouched files.
4. **Preserve public API**: `NATIVE_SYSTEM_PROMPT`,
   `with_system_prompt()`, `without_system_prompt()`, `with_reasoning_effort()`
   and all `pub` items in slim-core are consumed elsewhere — do not rename or
   remove.
5. **Behavioral anchors that must not change** (verified working as of 2026-08-22):
   - Native system prompt injected in ALL adapters (OpenAI-compatible message,
     Anthropic top-level `system`, Codex `instructions`) — TUI and headless both.
   - `reasoning_effort` plumbed from env `SLIM_EFFORT` / slim.toml in headless;
     TUI has its own path with default High.
   - Prompt caching: `cache_control: ephemeral` on last Anthropic tool only.
   - Dedup of repeated reads on the wire; read pagination footer; shell output
     cap 8 KiB/stream; compaction summary instruction after transcript;
     ContextSnapshot telemetry event.

## Current measured baselines (revalidated 2026-08-22)

`bench/token-economy/v2/` measures real wire payloads against a local
OpenAI-compatible SSE server. The current `s1_read` baseline is median-of-3;
the PERF-02 `s4_long` arms use 15 runs each and the PERF-03–07 regression arm
uses 5 runs. Model and effort were pinned to `gpt-5.6-luna` / `high`:

| Metric | Slim | Pi | Pit |
|---|---|---|---|
| T1 total (`s1_read`) | 4.238 B | 5.941 B | 11.870 B |
| System prompt | 2.435 B | 2.739 B | 4.029 B |
| Tools block | 1.592 B (6 tools) | 2.900 B (4) | 7.434 B (12) |
| Sum all requests (`s1_read`, 2 turns) | 11.354 B | 14.670 B | 26.528 B |
| Process startup (real, out-of-harness) | ~11 ms | Node init, higher | Node init, higher |
| Agent loop overhead in the PowerShell harness | ~2 ms/turn | ~11 ms/turn | ~39 ms/turn |

The older Slim values `5.487 B` and `13.852 B` were stale and are superseded by
the table above. Additional measurements:

- Slim process spawn → first request: ~11 ms direct. The ~1.16 s fixture timing
  is dominated by PowerShell `Start-Job`, not Slim startup.
- PERF-01 (`tool_setup`): 6 tools, 1.418 B internal serialized definitions,
  repeated setup 9.305,065 ns/turn, cached access 0,730 ns/turn, isolated
  speedup 12.746,7× (11 × 20.000 iterations).
- PERF-02 preserves all 75/75 `s4_long` request bodies byte-for-byte and saves
  approximately 9,3 µs per additional agent turn.
- PERF-03–07 preserve another 25/25 bodies byte-for-byte; fix shell pipe
  deadlock, reduce uncached request builds 3→1, accelerate a 20k-line SSE
  buffer 14,2×, remove the manual Base64 loop, and bound paged-read memory.
- Est-token estimator: `(chars * 2).div_ceil(7)` (÷3.5 conservative).

Existing audit backlog: `analysis_outputs/AUDIT-ECONOMIA-TOKENS.md` (items
TOK-01..TOK-19 with status). Already implemented natively: TOK-01 (prompt
caching), TOK-03 (dedup reads), TOK-04 (offset pagination), TOK-05 (estimator),
TOK-10 (shell caps), TOK-11 (compaction prefix), TOK-12 (telemetry),
TOK-08 (native system prompt, now compact edition). Pending: TOK-02 (degrade old
tool outputs), TOK-06 (progressive compaction), TOK-07 (dynamic tool selection).
Discarded with rationale: TOK-13, TOK-15, TOK-17, TOK-18 — do not reopen them
without new evidence.

## Where the remaining money is (hypotheses to verify, in priority order)

1. **History weight in long sessions** (TOK-02/TOK-06 territory): tool results
   stay intact forever until hard compaction at 85% window. Measure actual
   distribution of history bytes by age in real sessions before proposing.
   Requires TOK-12 telemetry data — check whether sessions exist to mine.
2. **Tools block re-sent every turn** (1.59 KB/turn today): TOK-07 dynamic
   selection is ESPECULATIVO — quantify the real cost over typical session
   lengths BEFORE building anything; if a 10-turn session pays ~16 KB total,
   weigh against added complexity and extra-turn risk.
3. **System prompt**: already compacted once (3.7→2.4 KB, -34%). Further cuts
   risk behavior. Only propose if you can prove ≥300 B savings with zero
   semantic loss, and validate via A/B (`run_benchmark.ps1 -VariantTag`).
4. **Startup**: already ~11 ms — treat as solved. Do not spend time here.
5. **Scheduler/loop baseline**: ~2 ms/turn in the coarse harness. PERF-02–07
   removed the measured adjacent CPU/I/O waste; do not reopen without a new
   profile.

## Implemented performance wave (PERF-03–07)

| Slice | Result | Proof |
|---|---|---|
| PERF-03 | shell drains both pipes while running | 10 MiB combined: timeout at 10,26 s → success at 0,76 s |
| PERF-04 | one request build; no cache work/event clones when cache is off | counting adapter 3→1; cache tests green |
| PERF-05 | linear SSE cursor/drain | 117,029 ms → 8,264 ms median, 14,2× |
| PERF-06 | standard Base64 crate | same `AAEC` wire output; 20 net lines removed |
| PERF-07 | streaming paginated read | O(file) retained memory → O(max line + page), same output |

The deployed A/B preserved 25/25 `s4_long` request bodies. Remaining candidates
are WrapCache >4.096 blocks, persistence batching, bounded TUI retention, and
read-only tool parallelism; all require separate behavioral evidence/design.
Details live in `bench/token-economy/v2/RESULTS.md`.

## Required method (no exceptions)

1. **Measure before.** Reproduce the baseline number yourself with the v2 bench
   or a micro-benchmark committed under `bench/`. Cite the command and raw
   output in your report.
2. **One change per commit.** Commit messages reference the TOK id or
   `PERF-NN` for new items.
3. **Measure after, same method.** Report delta with median-of-N (N≥3) and min.
4. **Full suite green** after each commit (`cargo test --workspace`, RUSTC path
   above). Then `refresh-slim.ps1 -Test`.
5. **Update the docs you were given**: append findings/status to
   `AUDIT-ECONOMIA-TOKENS.md`; record final numbers in
   `bench/token-economy/v2/RESULTS.md` (fill from RESULTS.template.md).

## Anti-goals (explicitly out of scope)

- Rewriting the agent loop, provider trait, or event system.
- Adding async executors, connection pools, or caching layers not already there.
- Any UX-visible change: prompts, wording, TUI layout, JSONL fields.
- Binary/compressed transport (billing counts reconstructed text — TOK-15).
- ProviderCache reuse in the product (semantically wrong — TOK-13).

## Deliverables

1. Prioritized findings table: item, evidence (measured), expected saving,
   effort, risk, verdict (do / defer / discard).
2. Implemented items with before/after numbers and green-suite proof.
3. Updated AUDIT-ECONOMIA-TOKENS.md and RESULTS.md.
4. A short "what I deliberately did NOT touch and why" section.

If a measurement contradicts this document, stop and reconcile it against the
current source and fresh captures. Once reverified, update this document; stale
historical numbers never override the current implementation.
