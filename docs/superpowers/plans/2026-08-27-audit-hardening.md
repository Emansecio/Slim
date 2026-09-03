# Slim audit hardening — 8 confirmed findings

**Goal:** Close the four P2 and four P3 findings from the 2026-08-27 local audit without changing headless Plan abort (exit 10) or adding sandbox/jail.

**Architecture:** Shared helpers only. One Anthropic default, one auth lock, OAuth preferred over leftover `api_key`, rg `--` separator, shell timeout cap, TUI Plan runs the existing read-only loop, Codex `incomplete` classified as truncated.

**Tech stack:** Rust 2021, existing Cargo workspace tests, no new crates.

---

### Task 1 — Anthropic default (P2 / N6)

**Files:** `crates/slim-cli/src/cli.rs`, `crates/slim-cli/src/tui.rs`, `crates/slim-cli/src/lib.rs`

- [ ] `default_provider_model(Anthropic)` = `claude-sonnet-4-6` (TUI overlay).
- [ ] `tui::defaults()` delegates to `default_provider_endpoint` + `default_provider_model`.
- [ ] Test: TUI Anthropic default equals `default_provider_model`.

### Task 2 — Canonical auth lock + orphan lock (P2 + P3)

**Files:** `crates/slim-cli/src/auth.rs`, `crates/slim-cli/src/oauth/store.rs`, `crates/slim-cli/tests/auth_security.rs`

- [ ] Shared `lock_auth_store` on `parent/.auth.lock` with Windows exclusive `share_mode(0)` and 30s retry.
- [ ] `update_auth_file` and `OAuthStore::lock_exclusive` both use it. Drop `auth.json` → `auth.lock` `create_new`.
- [ ] Leftover `.auth.lock` file without a live holder does not block `save_api_key_file`.

### Task 3 — OAuth shadows leftover api_key (P2)

**Files:** `crates/slim-cli/src/auth.rs`, `crates/slim-cli/src/oauth/store.rs`, tests

- [ ] `save_locked` removes `api_key` when inserting `oauth`.
- [ ] `load_auth_credential` prefers `oauth` when both fields exist.
- [ ] Env keys still win over the file.

### Task 4 — rg flag injection (P2)

**Files:** `crates/slim-core/src/tools/rg_search.rs`

- [ ] `.arg("--")` before query and root.
- [ ] Test: query `--help` / `-u` finds literal text, not rg help.

### Task 5 — shell timeout cap (P3)

**Files:** `crates/slim-core/src/tools/mod.rs`

- [ ] Cap `timeout_ms` at 120_000; schema `maximum: 120000`.
- [ ] Unit test on the clamp helper.

### Task 6 — Plan in TUI runs the loop (P3 / N4)

**Files:** `crates/slim-cli/src/headless.rs`, `crates/slim-cli/src/tui.rs`, `crates/slim-cli/tests/tui_runtime.rs`, `crates/slim-core/src/runtime/mod.rs`

- [ ] `ProviderRunOptions.allow_plan_loop` (default false). TUI sets true.
- [ ] Headless Plan still `approval_required` / exit 10.
- [ ] TUI Plan advertises read/list/search only; no `ask_question`; no toast `approval_required`.
- [ ] Redact test currently using Plan switches to ReadOnly.

### Task 7 — Codex incomplete without details (P3)

**Files:** `crates/slim-core/src/runtime/mod.rs`, `crates/slim-core/tests/provider_adapters.rs`

- [ ] `classify_stop_reason("incomplete")` → Truncated.
- [ ] Adapter fixture without `incomplete_details` still stops cleanly.

### Task 8 — Gate, docs, deploy

- [ ] `cargo test --workspace`, tracker §7, DESIGN §1.1, test counts, `.\refresh-slim.ps1 -Test`.
