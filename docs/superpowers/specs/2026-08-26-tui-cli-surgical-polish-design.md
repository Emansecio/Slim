# Slim TUI/CLI Surgical Polish Design

**Status:** approved direction, awaiting written-spec review

## Goal

Refine Slim's TUI and CLI without changing their established visual language:
restore the event-loop performance contract, remove Windows alignment hazards,
make user-facing copy consistent, and make command-line help easier to scan.

## Scope

### Runtime scheduling

- Replace the unconditional `crossterm::event::poll(16 ms)` loop with the
  existing Windows `WakeSignal` plus console-input wait.
- Wait until the nearest real deadline: motion at 83 ms, visible semantic
  status at 1 second, or indefinitely when no visual deadline exists.
- Derive the animation frame from monotonic elapsed time instead of advancing
  one frame per delayed poll.
- Drain at most 32 control events and 1,024 ordered stream events per batch.
- Re-arm the wake signal when either budget is exhausted so a coalesced
  auto-reset notification cannot strand queued work.
- Preserve control-first ordering, stream ordering, delta coalescing,
  cancellation, terminal outcomes, final flush and disconnect behavior.

### Motion

- Preserve the current visual vocabulary: one animated activity/tool glyph,
  assistant streaming caret and the existing restrained composer focus pulse.
- Keep the nominal 12 fps motion ceiling and deterministic reduced-motion
  behavior.
- Do not add animated text, moving layouts, decorative rails or new status
  surfaces.
- Quiescent state must perform no periodic polling or redraw.

### Windows alignment and copy

- Replace U+203A selection markers with the ASCII `>` anywhere their ambiguous
  console width can shift rows, including tool/thinking selection and command
  menus.
- Keep the composer prompt and overflow hints unchanged.
- Use English consistently in the existing English UI. Replace the remaining
  mixed-language hints and renderer fallback while preserving meaning.
- Do not alter lifecycle terminology or provider error classification.

### CLI help

- Replace the single compressed usage line with a structured help screen.
- Document TUI launch, headless input, provider/model selection, operating
  modes, session/recovery options, output formats and common examples.
- Keep all existing flags, defaults, aliases, exit codes and execution paths.
- `--version` and non-help stdout/JSONL remain byte-compatible.

## Preserved visual contract

- Exactly one blank physical row before each user turn after the first.
- Three-row inset composer on supported sizes and one-row emergency composer.
- Existing palette, surface hierarchy, breakpoints, footer placement,
  SessionRail, ActivityRail, tool grouping and prompt-anchored page fill.
- No cards, sidebar, dashboard, extra persistent rail or bottom-anchored chat.

## Failure and degradation behavior

- A failed Windows wait returns an I/O error and follows the existing terminal
  restoration path.
- Reduced motion uses no animation deadline; an ActivityRail with visible
  elapsed time may still receive its semantic 1 Hz update.
- Narrow and emergency layouts keep their current content and degradation
  order.
- ASCII selection markers remain width-stable under Unicode, no-color and
  Windows legacy console modes.

## Verification

- Add a deterministic RED/GREEN scheduler seam proving idle indefinite wait,
  exact motion/status deadlines, elapsed-derived frames and wake interruption.
- Add RED/GREEN regressions for control event 33 and stream event 1,025.
- Add golden tests for ASCII selectors and English-only tool/fallback copy.
- Update CLI contracts for the structured help while pinning `--version`,
  default text and JSONL behavior.
- Run the long-session release benchmark and retain the 16 ms p95 budget.
- Run focused TUI/CLI suites, full workspace tests, formatting, check, Clippy,
  documentation synchronization and `refresh-slim.ps1 -Test`.
- Compare release/PATH binary size, timestamp, SHA-256 and `slim --version`.

## Non-goals

- No provider protocol, authentication, compaction, tool execution or session
  format changes.
- No new animation style, theme, keybinding, output format or CLI flag.
- No speculative cache rewrite or optimization without a measured regression.
