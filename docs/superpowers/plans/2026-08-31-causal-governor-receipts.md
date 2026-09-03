# Causal Governor Receipts Implementation Plan

**Goal:** Eliminate duplicated governor filesystem I/O with the smallest receipt-oriented change.

**Non-goals:** Effective replay/cache, watchers, persistence redesign, unrelated refactors, or historical test coverage.

**Acceptance:** A call is prepared once; native execution returns observed evidence; the governor performs no filesystem investigation; focused gates pass.

**Unchanged scope:** Public tool results, provider protocol, prompts, budgets, stop behavior, session/UI behavior, and unrelated dirty workspace files.

## Tasks

1. Add internal prepared-invocation, fast-stamp, receipt, and execution-outcome types to `slim-core::tools`.
2. Make read/list expose observations from their existing I/O and make write/patch expose in-memory before/after content evidence.
3. Route sequential and parallel provider calls through the prepared invocation and execution outcome.
4. Reduce the governor to an in-memory ledger consuming preparations and receipts; remove snapshots, refreshes, file hashing, directory metadata walks, and blocking preflight work.
5. Adapt only focused tests, run formatter/check/clippy, inspect the diff, fix findings, and repeat the focused review.
6. Build release and refresh the PATH binary after all checks pass.
