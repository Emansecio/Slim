# Streaming cadence and queue-pressure baseline

Date: 2026-08-24  
Platform: Windows, `stable-x86_64-pc-windows-msvc` (rustc 1.97.1)

## Method

The harness is offline and deterministic: it uses no provider, network request,
or wall-clock sleep. `render.rs` exposes the existing coalescer logic to a fake
monotonic timestamp in unit tests while production still derives timestamps
from `Instant`. `runtime.rs` counts actual dirty drains with distinct logical
emission/render times.

Cadence corpus: 4 bursts of 24 uniquely identified deltas (576 transcript
bytes). The retained consumer renders the first delta immediately and drains
later deltas every four logical milliseconds, followed by exactly one
`AssistantEnded` and one `RunCompleted`.

Queue corpus: 4,096 uniquely identified 1,024-byte core deltas plus one terminal
event through the same `AppHandle`/`std::sync::mpsc::channel` contract used by
the current core-to-projector bridge. The consumer is retained until all sends
complete. Queue depth counts successful sends minus receives. String-capacity
figures cover payload buffers in the core ledger and channel separately; they
exclude event envelopes, queue nodes, and allocator metadata. Windows working
set is the median of three samples before and after the corpus and is
observational only.

Commands, each run three times with `RUSTC` set to the active toolchain:

```powershell
cargo test -p slim-tui runtime::tests::cadence_baseline_reports_current_per_drain_frame_pressure -- --exact --nocapture
cargo test -p slim-cli --test tui_bridge retained_consumer_reports_unbounded_core_queue_pressure -- --exact --nocapture --test-threads=1
```

## Results

| Cadence run | First frame (ms) | Frames | Frames/burst | p95 (ms) | Max (ms) | Terminal loss | Transcript bytes | Wall time (us) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0 | 29 | 7 | 3 | 3 | 0 | 576 | 373 |
| 2 | 0 | 29 | 7 | 3 | 3 | 0 | 576 | 243 |
| 3 | 0 | 29 | 7 | 3 | 3 | 0 | 576 | 246 |

Cadence invariants had zero variance. Wall time median was 246 us
(min 243, max 373, range 130, population variance 3,670.89 us²).

| Queue run | High-water events | Logical payload | Ledger string capacity | Queue string capacity | Total string capacity | Terminal loss | Working-set delta | Wall time (us) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 4,097 | 4,194,304 B | 4,194,304 B | 4,194,304 B | 8,388,608 B | 0 | 10,760,192 B | 5,420 |
| 2 | 4,097 | 4,194,304 B | 4,194,304 B | 4,194,304 B | 8,388,608 B | 0 | 10,772,480 B | 5,792 |
| 3 | 4,097 | 4,194,304 B | 4,194,304 B | 4,194,304 B | 8,388,608 B | 0 | 10,772,480 B | 5,570 |

Hardware-independent queue counts and capacities had zero variance. Wall time
median was 5,570 us (min 5,420, max 5,792, range 372, population variance
23,352 us²). Working-set delta median was 10,772,480 B (min 10,760,192,
max 10,772,480, range 12,288, population variance 33,554,432 B²).

## Slice 7 decision and thresholds

The current cadence preserves the first frame and terminal, but cadence is a
property of the retained consumer schedule because the coalescer is recreated
and flushed by every drain. More importantly, the core queue accepted all
4,097 events even though the downstream TUI data lane is configured for 1,024.
Slice 7 is therefore required.

The deterministic acceptance thresholds are:

- first delta delay exactly 0 logical ms;
- every later event-to-frame delay at most 16 logical ms (p95 also at most 16 ms);
- exactly one terminal delivered, terminal loss 0, and byte-exact ordered text;
- core queue high-water at most its configured capacity of 1,024 events;
- queue-retained string capacity at most 1,048,576 B for this 1 KiB corpus,
  excluding the separate append-only core ledger and allocator metadata.

RSS remains a recorded comparison, never a pass/fail threshold.

## Slice 7 measured comparison

Commands, each run three times with the same corpus and active `RUSTC`:

```powershell
cargo test -p slim-tui --lib runtime::tests::cadence_slice7_enforces_immediate_first_frame_and_sixteen_ms_window -- --exact --nocapture
cargo test -p slim-cli --test tui_bridge bounded_core_queue_applies_backpressure_at_capacity -- --exact --nocapture
```

| Cadence run | First frame (ms) | Frames | Frames/burst | p95 (ms) | Max (ms) | Terminal loss | Transcript bytes | Wall time (us) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0 | 13 | 3 | 15 | 16 | 0 | 576 | 264 |
| 2 | 0 | 13 | 3 | 15 | 16 | 0 | 576 | 255 |
| 3 | 0 | 13 | 3 | 15 | 16 | 0 | 576 | 267 |

All deterministic cadence invariants had zero variance. Wall time median was
264 us (min 255, max 267, range 12, population variance 26 us²). Relative to
the baseline, the retained scheduler reduced frames from 29 to 13 while
preserving immediate first frames, byte-exact transcript, terminal delivery
and the 16 ms maximum window.

| Queue run | High-water events | Logical payload | Queue string capacity | Full rejections | Terminal loss | Working-set delta | Wall time (us) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1,024 | 4,194,304 B | 1,048,576 B | 4 | 0 | 5,345,280 B | 4,161 |
| 2 | 1,024 | 4,194,304 B | 1,048,576 B | 4 | 0 | 5,353,472 B | 4,173 |
| 3 | 1,024 | 4,194,304 B | 1,048,576 B | 4 | 0 | 5,345,280 B | 4,446 |

Hardware-independent queue counts and capacities had zero variance. Wall time
median was 4,173 us (min 4,161, max 4,446, range 285, population variance
17,322 us²). Working-set delta median was 5,345,280 B (range 8,192,
population variance 14,913,080.89 B²). RSS is not directly attributable to the
queue because the process also retains the generated corpus and received
events; it remains observational. The enforceable result is the reduction of
queue high-water from 4,097 to exactly 1,024 events and of queue-retained
string capacity from 4,194,304 B to 1,048,576 B, with ordered terminal loss 0.

## Final post-deploy rerun

After `refresh-slim.ps1 -Test` printed `OK:`, the same commands were run three
more times. Deterministic cadence stayed at first frame 0 ms, 13 frames, 3
frames/burst, p95 15 ms, max 16 ms, 576 transcript bytes and terminal loss 0;
wall times were 496/283/264 us (median 283, range 232, population variance
11,061.56 us²). The queue stayed at high-water 1,024, retained string capacity
1,048,576 B, 4 full rejections and terminal loss 0; wall times were
4,145/4,042/4,027 us (median 4,042, range 118, population variance 2,750.89
us²). Working-set deltas were 5,349,376/5,341,184/5,345,280 B (median
5,345,280, range 8,192, population variance 11,184,810.67 B²), still
observational only.
