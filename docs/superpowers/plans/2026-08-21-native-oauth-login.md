# Native OAuth Login Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Make `Slim` open its normal TUI while signed out and provide native `/login` flows for Anthropic Claude Pro/Max and OpenAI Codex ChatGPT Plus/Pro.

**Architecture:** Keep rendering/input in `slim-tui`, OAuth/browser/store orchestration in `slim-cli`, and provider wire protocols in `slim-core`. TUI startup carries optional authentication; `/login` opens an in-TUI selector, then a cancellable OAuth task obtains and atomically persists credentials before constructing the normal shared runtime request.

**Tech Stack:** Rust stable/MSVC, Tokio, Reqwest/Rustls, Ratatui/Crossterm, Windows ACL + ShellExecuteW, PKCE SHA-256, OAuth loopback/device flows, JSON/SSE.

---

## File map

### New files

- `crates/slim-core/src/provider/codex.rs` — ChatGPT Codex Responses request/stream adapter.
- `crates/slim-cli/src/oauth/mod.rs` — public OAuth coordinator and provider dispatch.
- `crates/slim-cli/src/oauth/types.rs` — credential, provider, endpoint and progress contracts with redacted `Debug`.
- `crates/slim-cli/src/oauth/pkce.rs` — cryptographic verifier/challenge/state generation.
- `crates/slim-cli/src/oauth/callback.rs` — cancellable loopback callback server and callback parser.
- `crates/slim-cli/src/oauth/browser.rs` — Windows browser launch through `ShellExecuteW`.
- `crates/slim-cli/src/oauth/store.rs` — backward-compatible, atomic auth-file persistence with ACL.
- `crates/slim-cli/src/oauth/anthropic.rs` — Claude Pro/Max authorize/exchange/refresh flow.
- `crates/slim-cli/src/oauth/codex.rs` — ChatGPT browser/device authorize/exchange/refresh flow.
- `crates/slim-cli/tests/oauth_contract.rs` — offline OAuth fixtures and secret-persistence checks.
- `crates/slim-tui/tests/login_overlay.rs` — selector/input/render contracts.

### Modified files

- `crates/slim-core/Cargo.toml`, `crates/slim-cli/Cargo.toml` — minimal crypto/HTTP/Windows features.
- `crates/slim-core/src/provider.rs` — typed auth, third provider kind, OAuth Anthropic headers, Codex module export.
- `crates/slim-core/src/runtime/mod.rs` — include Codex stream normalization where provider-specific.
- `crates/slim-core/tests/provider_adapters.rs`, `provider_http.rs`, `agent_loop.rs` — OAuth and Codex wire regression tests.
- `crates/slim-cli/src/auth.rs` — share safe ACL/path helpers and preserve API-key resolution.
- `crates/slim-cli/src/tui.rs` — optional startup auth, active login lifecycle, refresh-before-run and logout.
- `crates/slim-cli/src/lib.rs` — exports needed by tests.
- `crates/slim-tui/src/api.rs`, `app.rs`, `runtime.rs`, `view_model.rs` — login commands/events/state/overlay.
- `README.md`, `POC-RESULTS.md`, `release/README.md`, `Documentações - Projeto/README.md` — actual signed-out/login behavior and evidence.

## Task 1: Typed provider authentication

**Files:**
- Modify: `crates/slim-core/src/provider.rs`
- Modify: `crates/slim-core/tests/provider_adapters.rs`

- [ ] **Step 1: Write failing redaction and Anthropic OAuth request tests**

Add tests that construct typed API-key and OAuth configs and assert:

```rust
let oauth = ProviderAuth::OAuth {
    access_token: "oauth-secret".into(),
    account_id: None,
};
assert!(!format!("{oauth:?}").contains("oauth-secret"));

let adapter = AnthropicAdapter::new(
    ProviderConfig::anthropic_oauth("https://api.anthropic.com/v1/messages", "claude-test", "oauth-secret")
).expect("adapter");
let request = adapter.build_request("hello");
assert!(request.headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer oauth-secret"));
assert!(!request.headers.iter().any(|(k, _)| k == "x-api-key"));
assert!(request.headers.iter().any(|(k, v)| k == "anthropic-beta" && v.contains("oauth-2025-04-20")));
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```bash
cargo.exe test -p slim-core --test provider_adapters anthropic_oauth
```

Expected: compile failure for missing `ProviderAuth`/`anthropic_oauth`.

- [ ] **Step 3: Add minimal typed auth**

Add:

```rust
#[derive(Clone, Eq, PartialEq)]
pub enum ProviderAuth {
    ApiKey(String),
    OAuth { access_token: String, account_id: Option<String> },
}
```

Implement manual `Debug` that emits only variant and account-presence metadata. Replace `ProviderConfig.api_key` with `auth`, preserve `openai`/`anthropic` constructors, and add `anthropic_oauth`. Add `ProviderKind::OpenAiCodex` and update every exhaustive provider-kind match.

For Anthropic OAuth, emit:

```text
Authorization: Bearer <access>
anthropic-beta: claude-code-20250219,oauth-2025-04-20
user-agent: claude-cli/2.1.75
x-app: cli
```

API-key mode continues emitting `x-api-key` only.

- [ ] **Step 4: Run focused tests**

```bash
cargo.exe test -p slim-core --test provider_adapters
```

Expected: PASS.

## Task 2: OpenAI Codex Responses adapter

**Files:**
- Create: `crates/slim-core/src/provider/codex.rs`
- Modify: `crates/slim-core/src/provider.rs`
- Modify: `crates/slim-core/tests/provider_adapters.rs`
- Modify: `crates/slim-core/tests/provider_http.rs`

- [ ] **Step 1: Write failing request golden**

Create a test using `OpenAiCodexAdapter` with an OAuth credential carrying `account_id`. Parse request JSON and assert:

```rust
assert_eq!(body["model"], "gpt-5.3-codex");
assert_eq!(body["store"], false);
assert_eq!(body["stream"], true);
assert_eq!(body["parallel_tool_calls"], true);
assert!(body["input"].is_array());
assert!(body["tools"].is_array());
assert_header(&request, "Authorization", "Bearer codex-secret");
assert_header(&request, "chatgpt-account-id", "account-1");
assert_header(&request, "OpenAI-Beta", "responses=experimental");
```

- [ ] **Step 2: Write failing stream normalization test**

Feed fixture events:

```json
{"type":"response.output_text.delta","delta":"hello"}
{"type":"response.reasoning_summary_text.delta","delta":"thinking"}
{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read","arguments":""}}
{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"README.md\"}"}
{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"path\":\"README.md\"}"}}
{"type":"response.completed","response":{"usage":{"input_tokens":4,"output_tokens":2}}}
```

Assert normalized text, reasoning, identity-preserving tool fragments, usage-before-stop and completion.

- [ ] **Step 3: Run tests and verify RED**

```bash
cargo.exe test -p slim-core --test provider_adapters codex
```

Expected: missing adapter/module.

- [ ] **Step 4: Implement adapter**

`OpenAiCodexAdapter` implements `ProviderAdapter`, posts SSE to `/codex/responses`, converts `ProviderMessage` into Responses `input` items, converts canonical tool schemas to Responses function tools, and maps event types listed above. Reject OAuth credentials without account ID before TCP send.

No WebSocket, compression, connector tools or private token import.

- [ ] **Step 5: Add localhost round-trip test**

Fixture server must inspect headers/body, emit chunked SSE, verify a tool result is sent as:

```json
{"type":"function_call_output","call_id":"call-1","output":"..."}
```

- [ ] **Step 6: Run provider suites**

```bash
cargo.exe test -p slim-core --test provider_adapters --test provider_http --test agent_loop
```

Expected: PASS.

## Task 3: PKCE and callback boundary

**Files:**
- Modify: `crates/slim-cli/Cargo.toml`
- Create: `crates/slim-cli/src/oauth/types.rs`
- Create: `crates/slim-cli/src/oauth/pkce.rs`
- Create: `crates/slim-cli/src/oauth/callback.rs`
- Create: `crates/slim-cli/src/oauth/mod.rs`
- Create: `crates/slim-cli/tests/oauth_contract.rs`

- [ ] **Step 1: Add minimal dependencies**

Add:

```toml
base64 = "0.22"
getrandom = "0.3"
sha2 = "0.10"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
tokio = { version = "1", features = ["macros", "net", "rt-multi-thread", "sync", "time"] }
```

- [ ] **Step 2: Write PKCE failing tests**

Assert verifier length/charset, S256 challenge against RFC 7636 vector, unique state, no padding and redacted credential debug.

- [ ] **Step 3: Implement PKCE contracts**

```rust
pub struct Pkce { pub verifier: String, pub challenge: String }
pub fn generate_pkce() -> Result<Pkce, OAuthError>;
pub fn generate_state() -> Result<String, OAuthError>;
```

Use `getrandom::fill`, SHA-256 and URL-safe base64 without padding.

- [ ] **Step 4: Write callback failing tests**

Bind ephemeral localhost fixture and assert valid `code/state`, 404 wrong path, 400 missing fields, rejection of wrong state, timeout, and cancellation.

- [ ] **Step 5: Implement callback listener**

```rust
pub async fn await_callback(
    listener: tokio::net::TcpListener,
    path: &str,
    expected_state: &str,
    cancel: watch::Receiver<bool>,
    timeout: Duration,
) -> Result<String, OAuthError>;
```

Read at most 16 KiB, accept only one HTTP request at a time, validate loopback/path/state, HTML-escape browser responses, never echo code.

- [ ] **Step 6: Run OAuth primitive tests**

```bash
cargo.exe test -p slim-cli --test oauth_contract pkce
cargo.exe test -p slim-cli --test oauth_contract callback
```

Expected: PASS.

## Task 4: Secure credential store and browser launcher

**Files:**
- Modify: `crates/slim-cli/Cargo.toml`
- Modify: `crates/slim-cli/src/auth.rs`
- Create: `crates/slim-cli/src/oauth/store.rs`
- Create: `crates/slim-cli/src/oauth/browser.rs`
- Modify: `crates/slim-cli/tests/oauth_contract.rs`

- [ ] **Step 1: Enable direct Windows browser API**

Add `Win32_UI_Shell` and `Win32_UI_WindowsAndMessaging` to `windows-sys` features. `SystemBrowser::open` calls `ShellExecuteW` with operation `open`; never invokes shell/cmd.

- [ ] **Step 2: Write backward-compatibility/store RED tests**

Cover current API-key JSON, OAuth entries, `active_provider`, unknown fields, symlink/directory rejection, atomic overwrite and tokens absent from `Debug`/errors.

Credential contract:

```rust
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    pub account_id: Option<String>,
}
```

- [ ] **Step 3: Implement store**

`OAuthStore` uses `%USERPROFILE%\.slim\auth.json` unless `SLIM_AUTH_FILE` is set, preserves existing API-key entries, writes same-directory temporary file, flushes, applies user-only ACL using shared `auth.rs` helper, then renames atomically. A process mutex serializes read-modify-write.

- [ ] **Step 4: Run store tests**

```bash
cargo.exe test -p slim-cli --test oauth_contract store
```

Expected: PASS with no credential bytes in failure output.

## Task 5: Native Anthropic and Codex OAuth flows

**Files:**
- Create: `crates/slim-cli/src/oauth/anthropic.rs`
- Create: `crates/slim-cli/src/oauth/codex.rs`
- Modify: `crates/slim-cli/src/oauth/mod.rs`
- Modify: `crates/slim-cli/tests/oauth_contract.rs`

- [ ] **Step 1: Write Anthropic fixture RED test**

Use endpoint overrides to assert authorize parameters, callback state, JSON token exchange, refresh rotation and five-minute expiry skew. Expected constants:

```text
client_id=9d1c250a-e6ae-44d9-88ed-5944d1962f5e
authorize=https://claude.ai/oauth/authorize
token=https://api.anthropic.com/v1/oauth/token
scope includes user:inference and user:sessions:claude_code
```

- [ ] **Step 2: Implement Anthropic flow**

Bind preferred loopback port 54545, generate PKCE/state, publish URL, launch browser, await callback, exchange token, persist credential. Refresh sends `anthropic-beta: oauth-2025-04-20` and preserves old refresh token only if upstream omits replacement.

- [ ] **Step 3: Write Codex browser/device RED tests**

Assert browser flow port 1455 and token form body. Assert device fallback obtains `device_auth_id/user_code`, publishes code/URL, treats 403/404 as pending, obeys interval/timeout, receives authorization code/verifier, exchanges token, decodes `chatgpt_account_id`, and rejects malformed JWT/missing account ID.

- [ ] **Step 4: Implement Codex flow**

Use public client ID `app_EMoamEEZ73f0CkXaXp7hrann`, fixed redirect, browser PKCE and device fallback. JWT decoding is base64url payload parsing only; token authenticity remains enforced by TLS/provider when used. Never persist ID token.

- [ ] **Step 5: Add cancellation and secret-leak tests**

Cancel callback and device polling via `watch`; verify task exits promptly, listener closes, and access/refresh/code never appear in emitted progress/errors.

- [ ] **Step 6: Run OAuth suite**

```bash
cargo.exe test -p slim-cli --test oauth_contract
```

Expected: PASS entirely offline.

## Task 6: Login selector in normal TUI

**Files:**
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Create: `crates/slim-tui/tests/login_overlay.rs`

- [ ] **Step 1: Write signed-out startup frame RED test**

Assert normal header/composer remain and operational row contains `signed out · type /login`; no login overlay at startup.

- [ ] **Step 2: Write slash-command/selector RED tests**

Submit `/login`; assert no `SendPrompt`, overlay opens, options match approved labels, arrows move selection, Enter emits `StartLogin`, Esc closes, direct `/login codex` works, and `/logout` emits logout.

- [ ] **Step 3: Write signed-out prompt preservation RED test**

Enter `explain this`; assert draft remains, no command leaves UI, and notification says `No provider connected. Use /login.`

- [ ] **Step 4: Implement UI contracts**

Add shared provider enum and:

```rust
UiCommand::StartLogin(LoginProvider)
UiCommand::CancelLogin
UiCommand::Logout
UiEvent::AuthStateChanged { provider: Option<LoginProvider>, authenticated: bool }
UiEvent::LoginProgress { message: String }
UiEvent::LoginUrl { url: String, user_code: Option<String> }
```

`AppState` owns `auth_state` and `login_overlay`; no token fields.

- [ ] **Step 5: Render centered overlay**

After normal frame rendering, draw `Clear` plus bordered selector in a bounded centered `Rect`; preserve normal screen underneath. Tiny terminals fall back to full available area.

- [ ] **Step 6: Run TUI tests**

```bash
cargo.exe test -p slim-tui
```

Expected: all prior tests plus login overlay tests PASS.

## Task 7: Optional-auth TUI worker and OAuth lifecycle

**Files:**
- Modify: `crates/slim-cli/src/tui.rs`
- Modify: `crates/slim-cli/src/lib.rs`
- Modify: `crates/slim-cli/tests/tui_bridge.rs`
- Modify: `crates/slim-cli/tests/tui_runtime.rs`
- Modify: `crates/slim-cli/tests/cli_contract.rs`

- [ ] **Step 1: Write binary startup RED test**

Run `Slim` with all key variables removed and `SLIM_AUTH_FILE` pointing to a missing temp path. Use a test-only terminal adapter or startup preparation seam; assert preparation succeeds as signed out instead of returning `ExitCode::Auth`.

- [ ] **Step 2: Refactor startup config**

Introduce:

```rust
struct TuiStartup {
    request: Option<ProviderRequest>,
    provider_hint: Option<LoginProvider>,
    options: ProviderRunOptions,
    initial_prompt: Option<String>,
}
```

Keep `spawn_tui_runtime(request, options)` as authenticated compatibility wrapper for existing tests. Add `spawn_tui_session(startup, oauth_service)` for production.

- [ ] **Step 3: Add worker login state machine**

Worker state is exactly one of idle, logging-in or running. `StartLogin` launches cancellable OAuth task; progress maps to UI events; success persists credential, builds request, marks provider active; cancellation joins task before terminal auth event. Reject concurrent login/run with recoverable notification.

- [ ] **Step 4: Add refresh-before-run**

Before provider execution, if OAuth expiry is within five minutes, refresh and atomically persist rotated credential. On refresh failure, do not clear draft/send network; emit recoverable auth failure and `authenticated:false` only for definitive `invalid_grant`.

- [ ] **Step 5: Add logout**

Remove active OAuth entry, zero/drop in-memory access/refresh strings where practical, clear request, emit signed-out state. Do not modify env API keys.

- [ ] **Step 6: Add offline bridge tests**

Inject fake OAuth service and assert `/login` lifecycle, provider selection, prompt after login reaches existing agent loop, cancellation ordering, logout, no secret in events, and old authenticated streaming/cancel tests remain unchanged.

- [ ] **Step 7: Run CLI/TUI integration tests**

```bash
cargo.exe test -p slim-cli --test cli_contract --test tui_bridge --test tui_runtime --test oauth_contract
```

Expected: PASS.

## Task 8: Security review and documentation

**Files:**
- Modify: `README.md`
- Modify: `POC-RESULTS.md`
- Modify: `release/README.md`
- Modify: `Documentações - Projeto/README.md`
- Modify: `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`

- [ ] **Step 1: Update commands/status**

Document:

```text
Slim                  # normal TUI, signed-out allowed
/login                # in-TUI provider selector
/logout               # remove active OAuth credential
Slim --headless ...   # no interactive OAuth
```

State SSE-only Codex, no live-provider gate and provider-controlled billing policy.

- [ ] **Step 2: Run threat-model review**

Review callback spoofing/state, token disclosure, auth-file tampering, browser command injection, refresh races, malicious HTTP bodies, cancellation/resource leaks and provider endpoint overrides. Every high/critical finding must be fixed with a regression test before proceeding.

- [ ] **Step 3: Run complete gates**

```bash
cargo.exe fmt --all -- --check
cargo.exe clippy --workspace --all-targets -- -D warnings
cargo.exe test --workspace
cargo.exe build --workspace --release
```

Expected: all exit 0.

- [ ] **Step 4: Regenerate deterministic release twice**

```bash
python3.exe release/build_release.py
sha256sum release/slim-0.1.0-windows-x64.zip
python3.exe release/build_release.py
sha256sum release/slim-0.1.0-windows-x64.zip
sha256sum -c release/SHA256SUMS.txt
```

Expected: ZIP hashes identical; checksum verification OK.

- [ ] **Step 5: Install and smoke-test PATH binary**

```bash
cp target/release/slim.exe /c/Users/User/bin/Slim.exe
Slim.exe --version
Slim.exe --help
Slim.exe --headless --read-only --prompt hello
```

Expected: installed checksum equals release EXE; headless returns `success`; TUI startup contract test proves no-key startup no longer exits before fullscreen.

## Plan self-review

- Spec coverage: startup, normal composer, selector, direct commands, native PKCE, callback/device flows, storage/ACL, refresh, provider adapters, cancellation, logout, tests, docs and release all map to tasks.
- Scope: one vertical feature; Codex adapter is required by OAuth and not an independent product feature.
- Placeholder scan: no TBD/TODO or unspecified implementation step remains.
- Type consistency: `ProviderAuth`, `LoginProvider`, `OAuthCredential`, `TuiStartup`, commands/events and provider adapters have one definition each and are reused by later tasks.
- YAGNI: no WebSocket, external token import, provider expansion or headless browser login.
