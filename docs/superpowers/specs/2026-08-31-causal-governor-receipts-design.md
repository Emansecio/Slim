# Causal Governor Receipts Design

## Goal

Remove the Causal Governor's independent filesystem investigation. Each tool call is prepared once, and the governor consumes only evidence observed by the tool while doing its real work.

## Design

- `PreparedToolInvocation` owns parsed/defaulted arguments, canonical workspace and target paths, operational policy, workspace revision, preparation timing, and the canonical call fingerprint.
- Native execution returns an internal `ToolExecutionOutcome { result, receipt }`; the public `ToolResult` contract stays unchanged.
- `ToolExecutionReceipt` records dependencies actually observed, mutations actually performed, workspace revisions, bytes read, and phase timings.
- `read` stamps the opened file handle with metadata and counts bytes read for the requested page.
- `list` digests the names observed during its existing directory enumeration, without per-entry metadata calls.
- `write` and `patch` digest content already held in memory and add only the target's post-write metadata stamp.
- Search, code intelligence, validation, interaction, and internal tools report only evidence they can prove. Missing evidence lowers causal confidence; it never triggers a compensating scan.
- Fast stamps use kind, size, modified time, file identity where available, and an operation digest when the tool already has the relevant bytes/names. Full file hashing is not performed by the governor.

## Non-goals

- No effective cache, replay, blocking, or reuse.
- No watcher, journal, Git scan, or new persistence format.
- No public `ToolResult` change.
- No broad test-suite expansion.

## Acceptance

- Governor code contains no file reads, directory walks, metadata refresh, or `spawn_blocking` pre/post investigation.
- Native read/list/write/patch calls share one prepared invocation with the governor and produce receipts from their real operation.
- Sequential and parallel provider-tool paths forward receipts to the governor.
- Focused governor/tool/runtime tests, formatting, check, and clippy pass.
