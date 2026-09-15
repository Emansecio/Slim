# Slim Ask Question Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** adicionar uma tool `ask_question` que pausa um run TUI Auto/ReadOnly, oferece escolhas e devolve a resposta ao mesmo agent loop.

**Architecture:** o core valida o payload e mantém um roteador assíncrono correlacionado pelo call ID. A CLI conecta esse roteador ao `ActiveRun`; a TUI projeta uma request específica, seleciona opção ou “Outro...” e envia uma resposta estruturada. Headless, Plan e resume durável preservam os contratos atuais e não anunciam a tool.

**Tech Stack:** Rust 2021, Tokio `oneshot`, Serde/JSON, Ratatui/Crossterm, provider fake/localhost, Cargo test/Clippy/rustfmt.

**Worktree policy:** o checkout já contém WIP amplo do usuário. Não criar branch, worktree ou commit; cada GREEN e revisão registrada substitui o checkpoint de commit.

---

## File map

- Create `crates/slim-core/src/interaction.rs`: tipos, limites, parsing e roteador de pergunta/resposta.
- Create `crates/slim-core/tests/ask_question.rs`: contrato público de validação/roteamento.
- Modify `crates/slim-core/src/lib.rs`: exportar o contrato de interação.
- Modify `crates/slim-core/src/events.rs`: adicionar `QuestionRequired` com opção estruturada.
- Modify `crates/slim-core/src/runtime/mod.rs`: catálogo condicional, espera assíncrona e lifecycle da tool.
- Modify `crates/slim-core/src/tools/mod.rs`: expor a definição JSON da tool sem misturá-la às tools de workspace.
- Modify `crates/slim-core/tests/agent_loop.rs`: provar pergunta → resposta → continuação do provider.
- Modify `crates/slim-cli/src/headless.rs`: aceitar uma rota somente na execução interna TUI; wrappers headless passam `None`.
- Modify `crates/slim-cli/src/tui.rs`: manter o responder no `ActiveRun` e rotear `AnswerQuestion` pelo run ativo.
- Modify `crates/slim-cli/tests/tui_bridge.rs`: correlação, stale response e cancelamento da bridge.
- Modify `crates/slim-tui/src/api.rs`: projetar `QuestionRequired` e declarar `AnswerQuestion`.
- Modify `crates/slim-tui/src/block.rs`: estado visual de seleção/custom sem contaminar a identidade idempotente.
- Modify `crates/slim-tui/src/app.rs`: aplicar request, mover seleção e alternar modo custom.
- Modify `crates/slim-tui/src/reducer.rs`: setas, números, Enter, texto livre e bloqueio após envio.
- Modify `crates/slim-tui/src/runtime.rs`: materialização vertical do bloco.
- Modify `crates/slim-tui/src/render.rs`: altura exata das alternativas com wrap.
- Modify `crates/slim-tui/src/view_model.rs`: projeção headless segura da pergunta.
- Modify `crates/slim-tui/tests/interaction_roundtrip.rs`: golden comportamental completo.
- Create `crates/slim-cli/tests/ask_question_tui.rs`: fixture localhost que executa o run TUI real sem provider externo.
- Modify docs vivos e referências de contagem listadas em `RULES.md`.

### Task 1: Core schema, validation and correlated route

- [x] **Step 1: write the public RED tests**

Create `crates/slim-core/tests/ask_question.rs` with tests through public APIs:

```rust
#[test]
fn question_accepts_zero_or_two_to_five_options() {
    let open = AskQuestion::parse(r#"{"question":"Explain the constraint"}"#).unwrap();
    assert!(open.options.is_empty());

    let choices = AskQuestion::parse(r#"{
        "question":"Choose a crate",
        "options":[
            {"label":"core","description":"Protocol and runtime"},
            {"label":"tui","description":"Reducer and render"}
        ]
    }"#).unwrap();
    assert_eq!(choices.options.len(), 2);
}

#[test]
fn question_rejects_one_duplicate_or_oversized_option() {
    assert!(matches!(
        AskQuestion::parse(r#"{"question":"Choose","options":[{"label":"core","description":"only"}]}"#),
        Err(InteractionError::InvalidOptionCount { count: 1 })
    ));
}

#[tokio::test]
async fn responder_delivers_exactly_once_to_matching_request() {
    let (route, responder) = interaction_route();
    let pending = route.register(InteractionRequestId::new("call-1")).unwrap();
    responder.answer(InteractionRequestId::new("call-1"), QuestionAnswer::custom("details")).unwrap();
    assert_eq!(pending.await.unwrap().answer, "details");
    assert!(matches!(
        responder.answer(InteractionRequestId::new("call-1"), QuestionAnswer::custom("late")),
        Err(InteractionError::StaleRequest { .. })
    ));
}
```

- [x] **Step 2: run RED**

Run:

```powershell
$env:RUSTC='C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'
cargo test -p slim-core --test ask_question
```

Expected: exit `101` because the public interaction types/module do not exist.

- [x] **Step 3: implement the minimal validated types and route**

`interaction.rs` must expose these stable shapes:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct InteractionRequestId(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuestionAnswerSource {
    Option { index: usize },
    Custom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionAnswer {
    pub answer: String,
    pub source: QuestionAnswerSource,
}

pub fn interaction_route() -> (InteractionRoute, InteractionResponder);
```

Use `Arc<Mutex<HashMap<InteractionRequestId, oneshot::Sender<QuestionAnswer>>>>`; registration rejects duplicate IDs, `answer` removes before send, and the pending receiver removes itself on cancellation/drop. No `unwrap`/`expect` in production.

Validation uses character limits from the spec, rejects control/newline content and compares trimmed labels with `eq_ignore_ascii_case`.

- [x] **Step 4: expose the core event and tool schema**

Add:

```rust
QuestionRequired {
    request_id: String,
    question: String,
    options: Vec<QuestionOption>,
    #[serde(default)]
    persisted: bool,
},
```

Expose `ask_question_definition()` returning the provider-compatible schema with `question`, optional `options`, `maxItems: 5` and `additionalProperties: false` at both object levels. Runtime validation remains authoritative for the zero-or-2..5 invariant.

- [x] **Step 5: run GREEN and review the slice**

Run `cargo test -p slim-core --test ask_question` and `cargo test -p slim-core --test protocol_golden`. Inspect the diff for unbounded collections, panic paths, ambiguous IDs and accidental registration of the tool in `ToolRegistry::default`.

### Task 2: Runtime lifecycle and same-turn provider continuation

- [x] **Step 1: write the agent-loop RED**

Extend `crates/slim-core/tests/agent_loop.rs` with a provider fake that emits `ask_question`, then records the tool result before returning final assistant text. The test must assert this causal suffix:

```rust
assert!(matches!(events[question_index].kind, EventKind::QuestionRequired { .. }));
assert!(matches!(events[ack_index].kind, EventKind::InteractionAcknowledged { accepted: true, .. }));
assert!(question_index < ack_index && ack_index < output_index && output_index < finished_index);
assert!(final_text.contains("selected core"));
```

A second RED test cancels while awaiting the answer and asserts no late answer is accepted and `ToolFinished.success == false`.

- [x] **Step 2: run RED**

Run the two named tests with `cargo test -p slim-core --test agent_loop <name> -- --exact --nocapture`. Expected: missing conditional schema/route and no async ask execution.

- [x] **Step 3: add the optional route to Runtime**

Add `interaction: Option<InteractionRoute>` plus `set_interaction_route`. Build provider definitions through one runtime method:

```rust
fn provider_tool_definitions(&self, mode: OperatingMode) -> Vec<Value> {
    let mut definitions = self.tools.definitions_for_mode(mode);
    if self.interaction.is_some() {
        definitions.push(ask_question_definition());
    }
    definitions
}
```

Do not change `ToolRegistry::execute*` or the public synchronous `Runtime::execute_tool` path.

- [x] **Step 4: implement async provider-tool dispatch**

Create an async dispatcher used only by provider calls:

```rust
async fn execute_provider_tool_call(
    &mut self,
    mode: OperatingMode,
    cwd: &Path,
    invocation: ToolInvocation<'_>,
    next_seq: u64,
) -> Result<(ToolResult, u64), ProviderError>;
```

Native names delegate to the existing lifecycle. `ask_question` parses before opening a request; valid calls emit `ToolStarted`, `QuestionRequired`, await the matching response with `tokio::select!` against `CancellationToken::cancelled`, emit acknowledgement/output/finish, and serialize the result with `serde_json`.

Update every provider-loop call site to `.await` this dispatcher. Preserve serial batch order and count the question against `max_tool_calls`.

- [x] **Step 5: run GREEN and regression**

Run the named agent-loop tests, then `cargo test -p slim-core`. Run core Clippy with the repository toolchain and `-D warnings`. Review event sequence overflow checks, cancellation closure and absence-route failure.

### Task 3: ActiveRun interaction bridge

- [x] **Step 1: write CLI bridge RED tests**

Add tests proving:

```rust
// Active run: scoped UI id is routed to the unscoped core call exactly once.
UiCommand::AnswerQuestion {
    request_id: InteractionRequestId("run-7:call-1".into()),
    answer: QuestionAnswer::option(0, "core"),
}
```

`UiCommand` continua usando o ID visual de `slim-tui`; após remover o namespace, a CLI cria o ID core opaco com `slim_core::InteractionRequestId::new("call-1")`.

Also prove wrong-run IDs, duplicate answers and answers after terminal state receive rejected acknowledgement; cancellation wakes the awaiting runtime.

- [x] **Step 2: run RED**

Run `cargo test -p slim-cli --test tui_bridge`. Expected: `AnswerQuestion`/active responder route absent.

- [x] **Step 3: thread the route only through interactive execution**

Extend the internal async execution function with `Option<InteractionRoute>`. All headless wrappers and durable resume pass `None`; ordinary `start_active_run` creates `interaction_route()` and passes `Some(route)`.

Add `interaction_responder: Option<InteractionResponder>` to `ActiveRun`. Do not add channel state to public `ProviderRunOptions`, preserving its `Eq/PartialEq` contract.

- [x] **Step 4: route commands by exact run namespace**

When active, accept only `AnswerQuestion`. Strip `run-{run_id}:` with `strip_prefix`; forward the remainder as the core ID. On route error, send a rejected `InteractionAcknowledged`. Existing `AnswerInput`/approval behavior remains unchanged.

- [x] **Step 5: run GREEN and review the slice**

Run `cargo test -p slim-cli --test tui_bridge` plus `cargo test -p slim-cli --lib`. Review lifecycle during `abort_active`, sender drop, terminal delivery and stale IDs.

### Task 4: TUI question state, keyboard UX and rendering

- [x] **Step 1: write the reducer/render RED tests**

Extend `interaction_roundtrip.rs` with separate tracer bullets:

```rust
// Down selects the second option; Enter sends it without SendPrompt.
// Number 1 selects but does not submit until Enter.
// Selecting Outro then Enter activates custom mode; typed text + Enter sends Custom.
// Zero-option question accepts composer text directly.
// ResponsePending blocks key, paste and duplicate Enter until matching ack.
// 40x10 render retains question, selected marker and Outro without panic.
```

Assert `UiCommand::AnswerQuestion` structurally, not private reducer state.

- [x] **Step 2: run RED**

Run `cargo test -p slim-tui --test interaction_roundtrip`. Expected: missing event/command/variant and selection behavior.

- [x] **Step 3: add the projected API and immutable request identity**

Add `UiEvent::QuestionRequired` and `UiCommand::AnswerQuestion`. Add `InteractionRequestKind::Question { question, options }` and mutable fields on `InteractionRequestState`:

```rust
pub selected_question_option: usize,
pub custom_question_answer: bool,
```

`same_request` compares only ID, immutable kind and persisted flag. Clamp projected options to five and apply the core text limits again at the UI boundary.

- [x] **Step 4: implement keyboard behavior**

Capture question keys before transcript scroll/composer submission. `Up`/`Down` wrap over `options.len() + 1`; digits select only valid options; Enter submits an option or enters custom mode. Free text uses the existing composer path and emits `QuestionAnswer::custom` after trim/size validation.

Keep approval and generic input behavior byte-for-byte unchanged. `response_pending` remains the shared duplicate-send guard.

- [x] **Step 5: render vertical choices with exact height**

Generate deterministic display lines from the question state, including marker, 1-based index, label/description and synthetic `Outro...`. Use the same display function in runtime materialization, `block_height` and `ViewModel` so scroll metrics equal rendered rows at 120x30, 60x16, 40x10 and 32x10.

- [x] **Step 6: run GREEN and UI regression**

Run `interaction_roundtrip`, `properties`, `scroll_golden`, `layout_golden`, `golden_matrix`, then all `cargo test -p slim-tui`. Run scoped Clippy and rustfmt check. Review narrow wrap, Unicode limits, composer ownership and replay conflict handling.

### Task 5: Offline full bridge fixture and hardening

- [x] **Step 1: write the localhost RED fixture**

Create `crates/slim-cli/tests/ask_question_tui.rs`. The server sequence is:

```text
user prompt
-> assistant tool call ask_question(call-question-1)
-> TUI QuestionRequired with two options
-> test sends AnswerQuestion(option 1)
-> server receives tool result {answer, source, option_index}
-> assistant final answer
```

Use sentinel credentials, bind only `127.0.0.1`, finite deadlines and RAII cleanup. Assert no provider-real route, no secret in projected events and no second answer accepted.

- [x] **Step 2: run RED then GREEN**

Run the test before bridge completion to record RED, finish only the minimal wiring it exposes, then rerun until GREEN. Do not broaden to physical ConPTY; that limitation remains tracked separately.

- [x] **Step 3: cancellation and malformed-call hardening**

Add fixture cases for malformed one-option payload and `CancelRun` while waiting. Assert finite completion, failed/cancelled tool lifecycle and no continuation request after cancellation.

- [x] **Step 4: focused cross-crate gate and self-review**

Run `cargo test -p slim-core -p slim-tui -p slim-cli`. Search for `ask_question`, `QuestionRequired` and `AnswerQuestion` to verify all match arms, redaction, bounded queues and terminal paths are handled. Run `git diff --check` and review only touched hunks against the design.

### Task 6: Living docs, full regression and deployment

- [x] **Step 1: update normative/current docs**

Update `DESIGN-SLIM-TUI.md` before claiming changed behavior, append the slice to tracker §7, and update current README/plan descriptions. State explicitly: Auto/ReadOnly ordinary TUI only; headless/Plan/durable resume do not advertise the tool.

- [x] **Step 2: run formatting and Clippy**

Run `cargo fmt --all -- --check`, scoped `-D warnings` Clippy for core/TUI/CLI, and `git diff --check`. Fix only touched code.

- [x] **Step 3: run the canonical full test gate**

Set the active Scoop/MSVC `RUSTC`, run `cargo test --workspace` without a filtering pipeline, capture `$LASTEXITCODE`, and count fresh suite/passed/failed/ignored/compiler-warning totals.

- [x] **Step 4: reconcile every mandated count**

Search `README.md`, project README, `PLANO-IMPLEMENTACAO.md`, release README, tracker and DESIGN for previous current totals. Update only current checkpoints; preserve dated historical rows.

- [x] **Step 5: deploy the current binary**

Run `.\refresh-slim.ps1 -Test` and require final `OK:`. Compare size, timestamp and SHA-256 of `target\release\slim.exe` and `C:\Users\User\bin\Slim.exe`; run `slim --version` and an offline Ask Question smoke.

- [x] **Step 6: final review and RULES checklist**

Re-read the complete Ask Question diff, ensure no open plan checkbox or undocumented gap, and copy the filled `RULES.md` §4 checklist in the final response.
