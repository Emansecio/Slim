# OpenCode Go Provider Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Add secure, model-aware OpenCode Go support to Slim headless and TUI, including masked API-key login and bounded live catalog refresh.

**Architecture:** `ProviderKind::OpenCodeGo` stays the logical provider while `OpenCodeGoAdapter` delegates Chat Completions and Messages to existing serializers/parsers and reuses extracted Responses wire helpers. A static 23-model registry owns protocol/capabilities; the public `/models` response only changes availability. Existing auth JSON atomic writer stores the API key, and the current TUI overlays become data-driven without changing run lifecycle lanes.

**Tech Stack:** Rust 2021, Tokio, Reqwest, Serde/serde_json, Ratatui, Crossterm, existing Slim provider/runtime/auth abstractions.

**Execution note:** Current `main` worktree contains required uncommitted work (73 modified, 86 untracked). Do not create an isolated worktree and do not commit; preserve unrelated changes. Execute inline with TDD and review each slice before continuing.

---

## File structure

### New files

- `crates/slim-core/src/provider/opencode_go.rs` — 23-model registry, endpoint routing and one logical adapter over three existing wire protocols.
- `crates/slim-cli/src/opencode_go_catalog.rs` — bounded public catalog fetch, validation, cache and fallback intersection.
- `crates/slim-core/tests/opencode_go_provider.rs` — registry and three protocol wire contracts.
- `crates/slim-cli/tests/opencode_go_catalog.rs` — local HTTP fixtures, cache safety and bounds.

### Existing files changed

- `crates/slim-core/src/provider.rs` — new provider kind/config/auth profile, normalizer protocol seam, shared Messages helpers.
- `crates/slim-core/src/provider/codex.rs` — extract reusable Responses request/parser helpers while preserving Codex headers and URL.
- `crates/slim-core/src/runtime/mod.rs` — use adapter wire kind instead of logical provider kind when normalizing tool streams.
- `crates/slim-core/tests/provider_adapters.rs`, `provider_http.rs`, `agent_loop.rs` — non-regression and OpenCode execution fixtures.
- `crates/slim-cli/src/auth.rs` — `OPENCODE_API_KEY` and auth-file resolution.
- `crates/slim-cli/src/oauth/store.rs`, `oauth/mod.rs` — generic API-key save/remove over existing locked atomic auth document.
- `crates/slim-cli/src/cli.rs`, `headless.rs`, `tui.rs`, `lib.rs` — parsing/defaults, provider dispatch, catalog and TUI bridge commands.
- `crates/slim-cli/tests/auth_security.rs`, `oauth_contract.rs`, `provider_cli.rs`, `tui_bridge.rs` — auth, wire and bridge regressions.
- `crates/slim-tui/src/api.rs`, `app.rs`, `reducer.rs`, `runtime.rs`, `view_model.rs` — third login option, masked key stage, dynamic model choices and rendering.
- `crates/slim-tui/tests/m2_integration.rs`, `layout_golden.rs`, `properties.rs` — reducer/render/security contracts.
- `README.md`, `Documentações - Projeto/{README.md,PLANO-IMPLEMENTACAO.md,DESIGN-SLIM-TUI.md,AUDIT-SLIM-TUI-TRACKER.md}`, `release/README.md` — living documentation and fresh test count.

---

## Slice 1 — logical provider and model registry

### Task 1: Register OpenCode Go and its documented models

**Files:**
- Create: `crates/slim-core/src/provider/opencode_go.rs`
- Create: `crates/slim-core/tests/opencode_go_provider.rs`
- Modify: `crates/slim-core/src/provider.rs`
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` (§5/provider contracts)
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` (§7)

- [x] **Step 1: Update normative design before behavior**

Add an OpenCode Go provider subsection defining logical provider identity, three wire protocols, the 23 documented IDs, `deepseek-v4-flash` default, unknown metadata semantics and live-catalog availability rules. Add the slice-start row to tracker §7.

- [x] **Step 2: Write provider-kind and registry RED tests**

Create tests that demand a logical kind, exact default and total lookup:

```rust
use slim_core::provider::{
    open_code_model, open_code_models, OpenCodeApi, ProviderKind,
};

#[test]
fn documented_registry_contains_23_unique_models() {
    let models = open_code_models();
    let unique = models.iter().map(|model| model.id).collect::<std::collections::HashSet<_>>();
    assert_eq!((models.len(), unique.len()), (23, 23));
}

#[test]
fn default_model_uses_chat_completions() {
    let model = open_code_model("deepseek-v4-flash").expect("documented default");
    assert_eq!(model.api, OpenCodeApi::ChatCompletions);
}

#[test]
fn documented_protocols_follow_official_table() {
    assert_eq!(open_code_model("gpt-5.6-luna").map(|m| m.api), Some(OpenCodeApi::Responses));
    assert_eq!(open_code_model("minimax-m3").map(|m| m.api), Some(OpenCodeApi::AnthropicMessages));
    assert_eq!(open_code_model("glm-5.3").map(|m| m.api), Some(OpenCodeApi::ChatCompletions));
}

#[test]
fn endpoint_only_unknown_model_is_not_supported() {
    assert!(open_code_model("kimi-k2.5").is_none());
}

#[test]
fn logical_provider_is_distinct_from_wire_protocol() {
    assert_ne!(ProviderKind::OpenCodeGo, ProviderKind::OpenAiCompatible);
}
```

- [x] **Step 3: Run RED and record expected reason**

Run with active scoop toolchain:

```powershell
$env:RUSTC='C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'
cargo test -p slim-core --test opencode_go_provider documented_ -- --nocapture
```

Expected: compile failure because `OpenCodeGo`, `OpenCodeApi`, `open_code_model` and `open_code_models` do not exist.

- [x] **Step 4: Implement minimal registry**

In `provider.rs`:

```rust
mod opencode_go;
pub use opencode_go::{
    open_code_model, open_code_models, OpenCodeApi, OpenCodeModel,
    OpenCodeGoAdapter, OPENCODE_GO_BASE_URL, OPENCODE_GO_DEFAULT_MODEL,
    OPENCODE_GO_MODELS_URL,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderKind {
    OpenAiCompatible,
    OpenAiCodex,
    Anthropic,
    OpenCodeGo,
}
```

In `opencode_go.rs`, define no dynamic allocation for the registry:

```rust
use super::{ProviderError, ProviderKind};

pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const OPENCODE_GO_MODELS_URL: &str = "https://opencode.ai/zen/go/v1/models";
pub const OPENCODE_GO_DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenCodeApi {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenCodeModel {
    pub id: &'static str,
    pub name: &'static str,
    pub api: OpenCodeApi,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub accepts_images: bool,
    pub reasoning_levels: &'static [&'static str],
}

pub fn open_code_models() -> &'static [OpenCodeModel] { MODELS }

pub fn open_code_model(id: &str) -> Option<&'static OpenCodeModel> {
    MODELS.iter().find(|model| model.id == id)
}
```

Populate `MODELS` with all 23 IDs and protocols listed in spec §4.2. Use only verified context/output values; unresolved fields are `None`. Do not add endpoint-only IDs.

Update every exhaustive `ProviderKind` match in core/CLI to compile, using explicit OpenCode branches rather than wildcard fallbacks.

- [x] **Step 5: Run GREEN and non-regression**

```powershell
cargo test -p slim-core --test opencode_go_provider -- --nocapture
cargo test -p slim-core --test provider_adapters -- --nocapture
```

Expected: both exit 0; provider registry tests pass; existing provider tests remain green.

- [x] **Step 6: Review slice**

Check:

```powershell
git diff --check -- crates/slim-core/src/provider.rs crates/slim-core/src/provider/opencode_go.rs crates/slim-core/tests/opencode_go_provider.rs
```

No warnings/errors. Mark Task 1 checkboxes and tracker row only after GREEN.

---

## Slice 2 — three wire protocols and headless execution

### Task 2: Route each registered model through correct wire adapter

**Files:**
- Modify: `crates/slim-core/src/provider.rs`
- Modify: `crates/slim-core/src/provider/codex.rs`
- Modify: `crates/slim-core/src/provider/opencode_go.rs`
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Modify: `crates/slim-core/tests/opencode_go_provider.rs`
- Modify: `crates/slim-core/tests/provider_adapters.rs`
- Modify: `crates/slim-core/tests/provider_http.rs`
- Modify: `crates/slim-cli/src/headless.rs`
- Modify: `crates/slim-cli/src/cli.rs`
- Modify: `crates/slim-cli/tests/provider_cli.rs`
- Modify: `Documentações - Projeto/{DESIGN-SLIM-TUI.md,AUDIT-SLIM-TUI-TRACKER.md}`

- [x] **Step 1: Write three request-contract RED tests**

Extend `opencode_go_provider.rs`:

```rust
#[test]
fn chat_model_uses_bearer_chat_completions() {
    let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "glm-5.3", "go-secret", None)
        .expect("adapter");
    let request = adapter.build_request("hello");
    assert!(request.url.ends_with("/v1/chat/completions"));
    assert!(request.headers.iter().any(|(name, value)| name == "Authorization" && value == "Bearer go-secret"));
}

#[test]
fn responses_model_omits_codex_subscription_headers() {
    let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "gpt-5.6-luna", "go-secret", Some("high"))
        .expect("adapter");
    let request = adapter.build_request("hello");
    assert!(request.url.ends_with("/v1/responses"));
    assert!(!request.headers.iter().any(|(name, _)| matches!(name.as_str(), "chatgpt-account-id" | "OpenAI-Beta" | "originator")));
}

#[test]
fn messages_model_uses_bearer_without_claude_oauth_headers() {
    let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "minimax-m3", "go-secret", None)
        .expect("adapter");
    let request = adapter.build_request("hello");
    assert!(request.url.ends_with("/v1/messages"));
    assert!(!request.headers.iter().any(|(name, _)| matches!(name.as_str(), "x-api-key" | "anthropic-beta" | "x-app")));
}
```

Add parser/tool/usage tests by feeding one fixture event per protocol and asserting common `ProviderEvent` values.

- [x] **Step 2: Run RED**

```powershell
cargo test -p slim-core --test opencode_go_provider -- --nocapture
```

Expected: compile failure because `OpenCodeGoAdapter::new` and OpenCode request routing do not exist.

- [x] **Step 3: Add logical/wire kind seam**

Extend `ProviderAdapter`:

```rust
pub trait ProviderAdapter {
    fn kind(&self) -> ProviderKind;
    fn wire_kind(&self) -> ProviderKind { self.kind() }
    // existing methods unchanged
}
```

Change normalizer construction sites from `adapter.kind()` to `adapter.wire_kind()`. `OpenCodeGoAdapter::kind()` returns `OpenCodeGo`; `wire_kind()` returns `OpenAiCompatible`, `OpenAiCodex` or `Anthropic` based on registry API. This prevents Anthropic tool blocks from being normalized as Chat deltas while preserving logical provider reporting/cache partitioning.

- [x] **Step 4: Extract reusable Responses helpers**

In `codex.rs`, keep Codex constructor and headers unchanged. Extract:

```rust
pub(super) fn responses_request(
    config: &ProviderConfig,
    url: String,
    headers: Vec<(String, String)>,
    messages: &[ProviderMessage],
    tools: &[Value],
) -> HttpRequest { /* current OpenAiCodexAdapter::request body */ }

pub(super) fn parse_responses_event(
    value: &Value,
    fallback_error: &str,
) -> Result<Vec<ProviderEvent>, ProviderError> { /* current parser match */ }
```

`OpenAiCodexAdapter` calls these helpers with existing subscription headers and `codex_url`. OpenCode calls them with:

```rust
vec![
    ("Authorization".into(), format!("Bearer {}", config.auth().secret())),
    ("accept".into(), "text/event-stream".into()),
    ("content-type".into(), "application/json".into()),
]
```

Do not reintroduce `max_output_tokens` into Codex subscription requests.

- [x] **Step 5: Add bearer auth profile for Messages**

Add `ProviderAuth::Bearer(String)` and make `secret()` include it. `anthropic_headers` handles profiles explicitly:

```rust
match config.auth() {
    ProviderAuth::ApiKey(secret) => vec![("x-api-key".into(), secret.clone())],
    ProviderAuth::Bearer(secret) => vec![("Authorization".into(), format!("Bearer {secret}"))],
    ProviderAuth::OAuth { access_token, .. } => { /* current OAuth headers */ }
}
```

Add crate-private constructors for OpenCode Chat/Messages configs. Do not treat OpenCode key as Anthropic OAuth.

- [x] **Step 6: Implement `OpenCodeGoAdapter`**

Use one enum, not one adapter per model:

```rust
enum WireAdapter {
    Chat(OpenAiCompatibleAdapter),
    Messages(AnthropicAdapter),
    Responses(ProviderConfig),
}

pub struct OpenCodeGoAdapter {
    model: &'static OpenCodeModel,
    wire: WireAdapter,
}
```

Constructor rejects unknown model before network. Endpoint helper uses `reqwest::Url` to replace/append exactly one protocol path while preserving localhost overrides for tests. `ProviderAdapter` delegates build/parse to selected wire implementation and overrides `kind`/`wire_kind`.

- [x] **Step 7: Add headless RED fixture**

In `provider_cli.rs`, add a local SSE server asserting:

```rust
let output = slim_cli::run_cli(vec![
    "--provider".into(), "opencode-go".into(),
    "--endpoint".into(), base_url,
    "--model".into(), "deepseek-v4-flash".into(),
    "--prompt".into(), "hello".into(),
]);
assert_eq!(output.code, ExitCode::Success);
assert!(output.stdout.contains("bench-done"));
```

Set `OPENCODE_API_KEY`; clear `SLIM_API_KEY`. Add one local fixture per protocol, including tools and usage.

- [x] **Step 8: Run RED then implement CLI/headless dispatch**

RED:

```powershell
cargo test -p slim-cli --test provider_cli opencode_go -- --nocapture
```

Expected: unsupported provider.

Then add aliases/defaults in `cli.rs`/`tui.rs`, JSONL provider name in `headless.rs`, and headless match:

```rust
ProviderKind::OpenCodeGo => {
    let adapter = OpenCodeGoAdapter::new(
        &request.endpoint,
        &request.model,
        &api_key,
        (!reasoning_effort.is_empty()).then_some(reasoning_effort.as_str()),
    )?;
    let client = HttpProviderClient::new(adapter, request.timeout)?;
    runtime.run_agent_loop_with_messages(
        &client, &initial_messages, request.mode, &cwd, 1, loop_config,
    ).await
}
```

Use registry context/output metadata before constructing `AgentLoopConfig`; explicit run options win within documented caps. Reject images for non-image models before reading/sending content.

- [x] **Step 9: Run focused GREEN**

```powershell
cargo test -p slim-core --test opencode_go_provider -- --nocapture
cargo test -p slim-core --test provider_adapters -- --nocapture
cargo test -p slim-core --test provider_http -- --nocapture
cargo test -p slim-cli --test provider_cli opencode_go -- --nocapture
```

Expected: all exit 0, no warnings.

---

## Slice 3 — auth resolution and secure API-key persistence

### Task 3: Resolve, save and remove OpenCode credentials

**Files:**
- Modify: `crates/slim-cli/src/auth.rs`
- Modify: `crates/slim-cli/src/oauth/store.rs`
- Modify: `crates/slim-cli/src/oauth/mod.rs`
- Modify: `crates/slim-cli/tests/auth_security.rs`
- Modify: `crates/slim-cli/tests/oauth_contract.rs`
- Modify: `Documentações - Projeto/{DESIGN-SLIM-TUI.md,AUDIT-SLIM-TUI-TRACKER.md}`

- [x] **Step 1: Write env/file precedence RED test**

```rust
#[test]
fn opencode_key_precedence_is_global_then_provider_then_file() {
    let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().expect("env lock");
    let _env = EnvGuard::capture(&["SLIM_API_KEY", "OPENCODE_API_KEY", "SLIM_AUTH_FILE"]);
    let temp = TempDir::new("opencode-priority");
    let path = auth_file(&temp, r#"{"version":1,"providers":{"opencode-go":{"api_key":"file-key"}}}"#);
    std::env::set_var("SLIM_AUTH_FILE", path);
    std::env::set_var("OPENCODE_API_KEY", "provider-key");
    std::env::set_var("SLIM_API_KEY", "global-key");
    assert_eq!(resolve_api_key(ProviderKind::OpenCodeGo).expect("key").as_deref(), Some("global-key"));
    std::env::remove_var("SLIM_API_KEY");
    assert_eq!(resolve_api_key(ProviderKind::OpenCodeGo).expect("key").as_deref(), Some("provider-key"));
    std::env::remove_var("OPENCODE_API_KEY");
    assert_eq!(resolve_api_key(ProviderKind::OpenCodeGo).expect("key").as_deref(), Some("file-key"));
}
```

- [x] **Step 2: Write atomic store RED tests**

In `oauth_contract.rs`:

```rust
#[test]
fn api_key_save_and_remove_preserve_sibling_oauth_entries() {
    let store = OAuthStore::at(auth_path);
    store.save_api_key("opencode-go", "go-secret").expect("save key");
    assert_eq!(store.api_key("opencode-go").expect("read").as_deref(), Some("go-secret"));
    store.remove_api_key("opencode-go").expect("remove key");
    assert!(store.credential(OAuthProvider::Anthropic).expect("sibling").is_some());
}
```

Add rejects-empty, symlink/directory, concurrent writer and secret-free Debug/error assertions.

- [x] **Step 3: Run RED**

```powershell
cargo test -p slim-cli --test auth_security opencode -- --nocapture
cargo test -p slim-cli --test oauth_contract api_key -- --nocapture
```

Expected: OpenCode enum match/API-key store methods missing.

- [x] **Step 4: Extend auth loader**

Add `opencode_go: Option<AuthProvider>` with `#[serde(rename = "opencode-go", alias = "opencode_go", default)]`. Map `ProviderKind::OpenCodeGo` to `OPENCODE_API_KEY` and this entry. Update auth docs from “Slim never writes” to “Slim writes TUI-managed provider credentials atomically; env always wins.”

- [x] **Step 5: Reuse existing atomic writer**

Add methods to `OAuthStore` rather than duplicate filesystem security:

```rust
pub fn api_key(&self, provider: &str) -> Result<Option<String>, OAuthError>;
pub fn save_api_key(&self, provider: &str, key: &str) -> Result<(), OAuthError>;
pub fn remove_api_key(&self, provider: &str) -> Result<(), OAuthError>;
pub fn active_provider_key(&self) -> Result<Option<String>, OAuthError>;
```

Only accept provider key `opencode-go` for mutation. `save_api_key` trims only for emptiness, preserves exact non-empty secret, sets `active_provider`, preserves sibling fields and uses `lock_exclusive` + `STORE_LOCK` + `write_document`. `remove_api_key` removes only `api_key`, deletes empty entry, and clears matching active provider.

Expose narrow proxy methods on `OAuthService`; do not expose store internals.

- [x] **Step 6: Run GREEN and ACL regression**

```powershell
cargo test -p slim-cli --test auth_security -- --nocapture
cargo test -p slim-cli --test oauth_contract -- --nocapture
```

Expected: exit 0, Windows ACL/security tests green, no secret in output.

---

## Slice 4 — bounded live model catalog

### Task 4: Fetch and cache model availability

**Files:**
- Create: `crates/slim-cli/src/opencode_go_catalog.rs`
- Create: `crates/slim-cli/tests/opencode_go_catalog.rs`
- Modify: `crates/slim-cli/src/lib.rs`
- Modify: `crates/slim-cli/Cargo.toml` only if existing reqwest features are insufficient; add no new crate.
- Modify: `Documentações - Projeto/{DESIGN-SLIM-TUI.md,AUDIT-SLIM-TUI-TRACKER.md}`

- [x] **Step 1: Write parser RED tests**

```rust
#[test]
fn catalog_intersects_live_ids_with_documented_registry() {
    let body = br#"{"object":"list","data":[{"id":"deepseek-v4-flash"},{"id":"unknown-new-model"}]}"#;
    let models = parse_catalog(body).expect("catalog");
    assert_eq!(models, vec!["deepseek-v4-flash"]);
}

#[test]
fn duplicate_or_invalid_ids_fail_closed() {
    let duplicate = br#"{"object":"list","data":[{"id":"glm-5.3"},{"id":"glm-5.3"}]}"#;
    assert!(parse_catalog(duplicate).is_err());
}
```

Add >1 MiB, >256 entries, >128-byte ID, invalid chars and unknown schema cases.

- [x] **Step 2: Write cache/fetch RED tests**

Use local `TcpListener` fixtures for success, timeout, redirect and oversized body. Assert cache survives failed refresh and symlink/directory paths are rejected.

- [x] **Step 3: Run RED**

```powershell
cargo test -p slim-cli --test opencode_go_catalog -- --nocapture
```

Expected: module/functions absent.

- [x] **Step 4: Implement bounded parser and cache**

Public-to-crate API:

```rust
pub(crate) const DEFAULT_CATALOG_URL: &str = slim_core::provider::OPENCODE_GO_MODELS_URL;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogSnapshot {
    pub model_ids: Vec<String>,
    pub source: CatalogSource,
}

pub(crate) struct OpenCodeCatalog {
    url: String,
    cache_path: PathBuf,
    client: reqwest::Client,
}

impl OpenCodeCatalog {
    pub(crate) fn production() -> Result<Self, CatalogError>;
    pub(crate) fn at(url: impl Into<String>, cache_path: impl Into<PathBuf>) -> Result<Self, CatalogError>;
    pub(crate) fn load_or_fallback(&self) -> CatalogSnapshot;
    pub(crate) async fn refresh(&self) -> Result<CatalogSnapshot, CatalogError>;
}
```

Build client with 15-second timeout and redirect `none`. Stream bytes, fail before exceeding 1 MiB, parse strict `{object:"list",data:[{id}]}`, validate IDs, reject duplicates, intersect registry, sort in embedded registry order, then atomic-write versioned public cache. Never put auth headers in catalog request.

Cache path: `%USERPROFILE%\.slim\opencode-go-models.json`, with `SLIM_OPENCODE_MODELS_FILE` test/advanced override only if non-empty. Reuse safe path patterns; do not reuse auth ACL because cache is public.

- [x] **Step 5: Run GREEN**

```powershell
cargo test -p slim-cli --test opencode_go_catalog -- --nocapture
```

Expected: all parser/network/cache tests pass without external network.

---

## Slice 5 — TUI key login and dynamic models

### Task 5: Make login and model overlays data-driven

**Files:**
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Modify: `crates/slim-tui/tests/m2_integration.rs`
- Modify: `crates/slim-tui/tests/layout_golden.rs`
- Modify: `crates/slim-tui/tests/properties.rs`
- Modify: `crates/slim-cli/src/tui.rs`
- Modify: `crates/slim-cli/tests/tui_bridge.rs`
- Modify: `Documentações - Projeto/{DESIGN-SLIM-TUI.md,AUDIT-SLIM-TUI-TRACKER.md}`

- [x] **Step 1: Write login reducer RED tests**

Demand third provider and secret-only state:

```rust
#[test]
fn opencode_login_collects_masked_key_and_emits_typed_command() {
    let mut state = AppState::new();
    state.login_overlay = Some(LoginOverlay::default());
    reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)));
    let effects = reduce(&mut state, Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(matches!(effects.as_slice(), [Effect::Send(UiCommand::SaveApiKey { provider: LoginProvider::OpenCodeGo, .. }), Effect::RequestRender]));
    assert!(!format!("{state:?}").contains('s'));
}
```

Separate tests: empty Enter stays in form, Backspace removes Unicode scalar, paste never reaches composer, Esc clears secret, Debug/event output is redacted.

- [x] **Step 2: Write dynamic model RED tests**

Define desired data types in tests:

```rust
let choices = vec![ModelChoice {
    id: "deepseek-v4-flash".into(),
    label: "DeepSeek V4 Flash".into(),
    available: true,
    efforts: vec![ReasoningEffort::High],
}];
state.apply_event(UiEvent::ModelCatalogLoaded { choices: choices.clone(), stale: false });
assert_eq!(state.model_overlay.as_ref().map(|overlay| overlay.selected_id()), Some("deepseek-v4-flash"));
```

Add refresh-preserves-ID, unavailable-cannot-submit, Codex fixed aliases still render, and active run only updates next request.

- [x] **Step 3: Run RED**

```powershell
cargo test -p slim-tui --test m2_integration opencode -- --nocapture
cargo test -p slim-tui --test layout_golden opencode -- --nocapture
```

Expected: missing `LoginProvider::OpenCodeGo`, key stage, `ModelChoice`, catalog event and commands.

- [x] **Step 4: Add secret-safe API types**

Extend `SensitiveText` with `Default`, `is_empty`, `push`, `pop`, `clear`, and `len_chars`; keep custom redacted `Debug`.

Add:

```rust
pub enum LoginProvider { Anthropic, OpenAiCodex, OpenCodeGo }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub efforts: Vec<ReasoningEffort>,
}

pub enum UiCommand {
    // existing variants
    SaveApiKey { provider: LoginProvider, api_key: SensitiveText },
    RefreshModels,
    SetSelectedModel { model: String, effort: ReasoningEffort },
}

pub enum UiEvent {
    // existing variants
    ModelCatalogLoaded { choices: Vec<ModelChoice>, stale: bool },
}
```

`UiCommand` debug remains safe because `SensitiveText` controls formatting.

- [x] **Step 5: Generalize overlay state minimally**

Use runtime data, not parallel Codex/Go overlay implementations:

```rust
pub enum LoginStage { Providers, ApiKey(SensitiveText) }

pub struct LoginOverlay {
    pub selected: usize,
    pub stage: LoginStage,
    // current progress/url fields
}

pub struct ModelOverlay {
    pub choices: Vec<ModelChoice>,
    pub selected: usize,
    pub stale: bool,
}

pub struct EffortOverlay {
    pub model: String,
    pub levels: Vec<ReasoningEffort>,
    pub selected: usize,
}
```

Provider selection count becomes 3. Enter on OpenCode moves to `ApiKey`; OAuth choices keep current `StartLogin`. Model Enter rejects `available == false`; effort Enter emits generic `SetSelectedModel`.

- [x] **Step 6: Render masked key and dynamic choices**

Login overlay lists all three provider labels. In key stage render exactly `•` repeated by character count, capped to modal width, plus `Enter save · Esc cancel`; never call `expose()` while rendering.

Model overlay renders visible rows from `choices`, marks unavailable with `offline`, adds `refreshing/offline` status, and remains bounded by viewport. Preserve reduced-motion/modal suspension.

- [x] **Step 7: Wire TUI bridge**

In `tui.rs` worker:

- `SaveApiKey(OpenCodeGo, key)` calls `oauth.save_api_key("opencode-go", key.expose())`;
- on success builds OpenCode request with defaults/current overrides, updates `startup.request`, clears OAuth session and emits `AuthStateChanged`;
- `RefreshModels` emits cached/fallback choices immediately, then spawns one async refresh and emits fresh choices;
- `SetSelectedModel` validates registry and current catalog availability, then updates next request, reasoning, context/output options and UI events;
- OpenCode logout removes only persisted key; env-sourced key produces explicit precedence notice;
- `401/403` maps to auth failure text ending `Use /login to replace the OpenCode Go key.` without exposing response credentials.

Do not mutate current `ActiveRun`; active-run command handling retains current “run already active” behavior, so selection applies after completion.

- [x] **Step 8: Run focused GREEN**

```powershell
cargo test -p slim-tui --test m2_integration -- --nocapture
cargo test -p slim-tui --test layout_golden -- --nocapture
cargo test -p slim-tui --test properties -- --nocapture
cargo test -p slim-cli --test tui_bridge opencode -- --nocapture
```

Expected: all pass, no secret text in failures/snapshots.

- [x] **Step 9: Review TUI slice**

Inspect actual diff for modal precedence, active-run behavior, secret lifetime and exhaustive event lanes. Run:

```powershell
cargo clippy -p slim-tui --all-targets -- -D warnings
```

Expected: exit 0, zero warnings.

---

## Slice 6 — E2E, docs and deploy

### Task 6: Prove all paths, synchronize docs and deploy PATH binary

**Files:**
- Modify: `tests/e2e_offline.rs`
- Modify: `README.md`
- Modify: `Documentações - Projeto/README.md`
- Modify: `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`
- Modify: `release/README.md`
- Modify: `docs/superpowers/specs/2026-08-24-opencode-go-provider-design.md`
- Modify: this plan checkboxes

- [ ] **Step 1: Add offline E2E RED/then GREEN per protocol**

Use localhost servers only. Each fixture asserts request path, Bearer header, model, tool schema, output text and usage. Add auth-file OpenCode fixture and `--provider go` alias. Confirm unsupported catalog ID fails before server accepts connection.

Focused command:

```powershell
cargo test --test e2e_offline opencode_go -- --nocapture
```

Expected after implementation: all OpenCode E2Es pass.

- [ ] **Step 2: Run formatting and focused Clippy**

```powershell
cargo fmt --all -- --check
cargo clippy -p slim-core --all-targets -- -D warnings
cargo clippy -p slim-cli --all-targets -- -D warnings
cargo clippy -p slim-tui --all-targets -- -D warnings
```

Expected: all exit 0. If formatter fails, run `cargo fmt --all`, then repeat checks. Do not weaken lints.

- [ ] **Step 3: Run canonical workspace tests freshly**

```powershell
cargo test --workspace -j 1 --no-fail-fast
```

Expected: exit 0, 0 failed, 0 compiler warnings, only existing physical ConPTY ignore. Count every `test result: ok. N passed` line from this run; do not reuse historical counts.

- [ ] **Step 4: Run performance gate**

```powershell
cargo bench -p slim-tui --bench long_session
```

Expected: 3,200 blocks / ~5 MiB and nearest-rank warm p95 input→frame ≤16 ms.

- [ ] **Step 5: Synchronize living docs**

Document:

- provider aliases/default/model routes;
- key precedence and masked `/login` flow;
- public catalog refresh/cache/fallback limitations;
- examples for env and saved key;
- fresh test count in every file that cites it;
- tracker §7 row with commands/results;
- spec status `implementado e verificado` only after gates pass.

Search old count across repository docs before replacing:

```powershell
rg -n "[0-9]+ passed|[0-9]+ testes|[0-9]+ tests|suítes|suites" README.md "Documentações - Projeto" release docs/superpowers
```

- [ ] **Step 6: Diff hygiene**

```powershell
git diff --check
```

Expected: no whitespace errors. CRLF conversion notices alone are not errors.

- [ ] **Step 7: Canonical release build and deploy**

```powershell
.\refresh-slim.ps1 -Test
```

Expected: script prints `OK:` and smoke succeeds. This is mandatory after Rust/test/Cargo changes.

- [ ] **Step 8: Verify deployed binary identity**

```powershell
$release = Get-FileHash .\target\release\slim.exe -Algorithm SHA256
$path = Get-FileHash C:\Users\User\bin\Slim.exe -Algorithm SHA256
slim --version
"RELEASE=$($release.Hash) PATH=$($path.Hash) MATCH=$($release.Hash -eq $path.Hash)"
```

Expected: `slim 0.1.0`, exit 0, `MATCH=True`.

- [ ] **Step 9: Final completion gate**

Run `git status --short`, ensure only intended changes plus pre-existing unrelated edits. Complete tracker/task statuses only with all evidence fresh. Final response must include RULES.md §4 checklist, commands/output counts, `OK:`, PATH hash match, and explicit limitation that live OpenCode network inference was not exercised unless user-supplied credential was actually used.
