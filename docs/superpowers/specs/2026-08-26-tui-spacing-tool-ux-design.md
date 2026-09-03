# TUI Spacing and Tool UX Design

**Status:** approved direction, pending implementation plan

## Goal

Make Slim's conversation surface easier to scan without turning it into a
dashboard: keep each turn dense, separate adjacent turns once, consolidate
operational metadata, and make tool groups both informative and directly
actionable.

## Scope

### Vertical rhythm

- Preserve the fixed bottom composer and the empty scrollback area available
  for a response to grow.
- Keep `user -> thinking/tools -> assistant` adjacent inside one turn.
- Insert exactly one blank physical row before every user block except the
  first user block in the transcript, even when system/tool history precedes it.
- Include that row in height measurement, anchors, scrollbars and clipping; it
  must never be a render-only row.

### Session and footer metadata

- A SessionRail is useful only when its cwd conveys project context.
- Treat an empty cwd, `~`, and the resolved user home as trivial. With trivial
  cwd, hide SessionRail and project context plus token totals in the footer.
- Preserve SessionRail for non-trivial cwd at the existing `>=80x12`
  breakpoint. Do not duplicate context between top and bottom.
- Keep model, effort and mode in the composer label.

### Focus hierarchy

- Align `border_focus` with the normative `#4F7D5E` token.
- Keep the bright green accent for Slim identity, success, cursor and known
  progress; the full-width composer border must not compete with them.

### Live-edge page fill

- Rendering and navigation must resolve `FollowMode::LiveEdge { prompt_id }`
  through the same `HeightIndex::metrics` viewport start.
- If the current prompt plus its response fit, the prompt begins at the top of
  the useful transcript and historical tail is not inserted above it.
- Once that suffix overflows, live edge follows the newest rows as before.

### Tool presentation

- Continue grouping only consecutive successful calls from the same
  `ToolBatchId`; failed, cancelled, pending and running calls stay visible.
- A collapsed header prefers a bounded name summary, for example
  `✓ 4 tools · read ×3, shell · 42ms`; when it does not fit, fall back to
  `✓ 4 tools · 42ms`.
- Preserve first-seen name order, sanitize names, count repeats, and cap the
  name summary rather than wrapping it.
- With an empty composer at live edge, Enter activates the last visible
  foldable tool/thinking block advertised by the UI. No preliminary Up key is
  required. Enter keeps its current submit/modal behavior in every other case.
- Expanded members, failures, pagination, call identity and output redaction
  remain unchanged.

### Headless CLI

- Add opt-in `--verbose` for human text output only.
- Standard text output and `--jsonl` remain byte-compatible.
- `--verbose --jsonl` is rejected explicitly instead of silently changing the
  JSONL contract.
- Verbose text adds a compact tool timeline derived from already-redacted
  lifecycle events. It contains name counts, success/failure and aggregate
  duration, but no raw arguments or tool output.
- Help text documents the new flag.

## Non-goals

- No bottom-anchored chat, floating composer, cards, dashboard, sidebar or new
  persistent rail.
- No change to provider protocols, tool execution order, replay or accounting.
- No automatic expansion or fetching of tool outputs.
- No JSONL event-stream redesign.

## Error and degradation behavior

- Narrow widths use the existing compact tool header and footer variants.
- A trivial cwd never consumes a blank SessionRail row.
- Emergency layouts below `40x8` retain their current one-row composer.
- Missing durations omit the duration suffix; overflow uses saturating sums.
- Sanitization happens before measuring or rendering any tool name.

## Verification

- RED/GREEN goldens for turn spacing at first and subsequent prompts.
- RED/GREEN regression with prior history proving renderer and scroll metrics
  share the prompt-anchored viewport start.
- RED/GREEN tests for trivial versus project cwd metadata placement.
- RED/GREEN tool goldens for name counts, narrow fallback, failures outside a
  group, and direct Enter expansion from live edge.
- RED/GREEN CLI tests proving `--verbose` output, unchanged default/JSONL and
  explicit `--verbose --jsonl` rejection.
- Existing 40/80/120-column matrices, full workspace suite, formatter, docs,
  release deploy and PATH identity gate.
