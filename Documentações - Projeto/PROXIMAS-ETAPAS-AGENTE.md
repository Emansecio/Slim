# Slim — próximas etapas: agente completo, harness leve

> **Leitor:** agente executor. **N1–N3 estão feitos** (2026-08-25): OAuth da TUI
> alimenta o headless; tool `todo` + `TodoChanged` no loop; skills Slim em
> `%USERPROFILE%/.slim/skills` (cwd `.slim/skills` sobrescreve) via tool `skill`
> lazy — nada injetado até `list` ou `name`.
> Próxima fatia: **N5** (`ask_question` no resume TUI). N1–N4 e N6 feitos.
> Não reabrir N1–N4.
> **Data da análise:** 2026-08-25. N1–N3 implementados no código na mesma data.
> **Prevalência:** `RULES.md` > `AGENTS.md` > este arquivo > README > memória.
> Caminho:linha abaixo foi lido na sessão de 2026-08-25. Docs antigos
> (`analysis_outputs/`, DESIGN M3, PLANO §10.1 “P0 cache”) descrevem o
> passado — o código ganha.

Este arquivo é o backlog **ranqueado** do ciclo “agente mais completo sem
ficar mais pesado”. Não é carta branca para M3, não é clone de Cursor, não
é roadmap de milhas.

---

## 0. Identidade (não negociável)

Slim é um harness de coding agent leve em Rust (Windows x64/MSVC): headless +
TUI fullscreen, um agent loop, tools reais, providers já integrados.

Não é IDE, dashboard, nem “Cursor clone”. Melhoria neste ciclo = o **loop**
fazer o trabalho de ponta a ponta. Não = drawer, crate, cache, overlay,
inspector ou abstração que não feche um buraco real do loop.

Critério ACEITO:

- fecha buraco observável do agent loop ou do contrato CLI/TUI↔core;
- reutiliza tipo/bridge existente;
- não adiciona dependência;
- não adiciona superfície TUI permanente (drawer/rail/overlay) salvo o mínimo
  para capability já persistida (Todo dock já existe);
- não piora o budget: p95 input→frame ≤ 16 ms, filas bounded, sem canal extra,
  sem rewrap global.

Critério PESO (rejeitar): “a spec M3 pede”, “ficar mais completo visualmente”,
“como o Cursor/OpenCode”, “abstrair para o futuro”.

---

## 1. Veredito

O loop já executa um coding agent: provider + read/list/search/write/patch/shell
+ compact + anti-loop + cancel + fila de prompts + `ask_question` na TUI
não-durável.

O que falta é **ligar o que já está persistido/tipado** nas fatias restantes:

1. ~~OAuth da TUI não alimenta o headless.~~ **N1 feito.**
2. ~~Todo/Skills fora do run normal.~~ **N2–N3 feitos** (Plan/Goal UI e MCP/child continuam fora).
3. ~~Plan na TUI aborta o turno em vez de rodar o loop read-only.~~ **N4 feito.**

Inspectors, caches novos, `SurfaceBackend`, telemetry, transporte MCP e child
provider-backed são peso: tipo sem caller, stub de protocolo, ou contrato M3
— não buraco do agente.

G252–G255 (setas de pergunta, stdin headless, precedência `slim.toml`, Codex
JWT) já foram corrigidos. Não reabrir.

---

## 2. Tabela ranqueada (ACEITOS)

| ID | Fatia | Buraco | Evidência | Já existe | Peso | Por que não é pesado | Bloqueia se não fizer |
|---|---|---|---|---|---|---|---|
| N1 | Auth headless ← store OAuth da TUI | `/login` Claude/Codex grava `oauth`; headless só lê `api_key` | `auth.rs` 184–186; `oauth/store.rs` 173–177; `cli.rs` 227–247; `tui.rs` 172–181 e 319–349; `headless.rs` 733–737 | `OAuthStore`, `oauth_request`, `codex_account_id`, `anthropic_oauth` | S | Mesmo `auth.json`; zero dep; zero frame | Qualquer headless “igual à TUI” |
| N2 | Todo no loop + `TodoChanged` | Modelo não cria todos; dock `Ctrl+T` fica vazio | `tools/mod.rs` 58–86; `runtime/mod.rs` 276–281; `capability_bridge.rs` 480–510; `api.rs` 379 e 520–677; `app.rs` 1523–1526; slim-cli sem `RuntimeCapabilityBridge` | `TodoTracker`, `TaskMutation`, dock | S | Sem drawer; dock já existe | N3 (bridge no run) e “agente planeja o trabalho” |
| N3 | Skill: discover + `invoke_script` | Skills em disco não são tools; adapter falha de propósito | `discovery.rs` 53; `invocation.rs` 35–48; `capability_bridge.rs` 69–74 e 143–145; `runtime/mod.rs` 301–316 | `SkillEntry`, `discover`, `invoke_script`, `CapabilityKind::Skill` | M | Sem crate/MCP; trust via `ask_question` | Agente que deveria seguir skills do repo |
| N4 | Plan na TUI **roda** o loop | **FEITO 2026-08-27** | TUI `allow_plan_loop`; headless abort intacto |
| N5 | `ask_question` no resume TUI | `--resume` zera a rota; sessão longa não pergunta | `tui.rs` 1996–2001; `runtime/mod.rs` 276–280 | `interaction_route()`, `ask_question_definition` | S | Mesma rota do path não-durável | Turnos TUI pós-resume sem clarificação |
| N6 | Defaults Anthropic CLI vs TUI | **FEITO 2026-08-27** | `default_provider_model` = `claude-sonnet-4-6` |
| N7 | Worker: `Approve`/`Reject` no run ativo | Só `AnswerQuestion` chega à rota; Y/N é rejeitado | `tui.rs` 1059–1070; `interaction.rs` 150–181 | `UiCommand::Approve`/`Reject`, bloco inline | S | Reusar pergunta Y/N ou oneshot irmão | Trust Explicit (N3) e PlanApprove |

Não há 8º item ACEITO: o restante é peso ou bloqueio de ambiente.

Detalhe de cada ACEITO: seções 4–10. Ordem de execução imediata: seção 3.
O que **não** fazer neste ciclo: seção 11. P4–P8 revalidados: seção 12.

---

## 3. Três fatias imediatas (uma sessão cada)

Implementar nesta ordem. **N1–N4 e N6 feitos.** Não abrir N5/N7
antes de pedido explícito.

### Sessão 1 — N1: OAuth store → headless — **FEITO**

**Buraco:** usuário faz `/login` (Claude Pro/Max ou ChatGPT Plus/Pro). A TUI
grava `providers.<kind>.oauth` em `%USERPROFILE%\.slim\auth.json`. Headless
chama `resolve_api_key`, que exige `api_key` e, se só houver `oauth`, retorna
`None` (`let _ = provider.oauth`). Mensagem de Codex até cita `/login`, mas
login **não** preenche `api_key`.

**Fazer:**

- Um helper de credencial (não um fluxo OAuth novo) que, na ausência de env
  (`SLIM_API_KEY`, `CODEX_ACCESS_TOKEN`, `ANTHROPIC_API_KEY`, …) e de
  `api_key` no arquivo, leia `oauth.access` do mesmo store.
- Codex: preencher `account_id` via `codex_account_id` no access token JWT
  (já existe; G255 cobre o caso “key sem JWT”).
- Anthropic: se a credencial veio de OAuth, setar `account_id: Some(...)`
  (mesmo vazio como a TUI) para o braço `ProviderConfig::anthropic_oauth` em
  `execute_provider_turn_async`. TUI Codex **não** usa `api_key_request`
  (`kind != OpenAiCodex`).

**Não fazer:** tela de login no CLI; refresh token além do que
`OAuthService::fresh_credential` já faz na TUI; gravar o access token como
`api_key` (mistura os dois canais).

**Teste:** estender `cli_contract.rs`
`headless_codex_without_jwt_account_id_is_auth` e os testes de `oauth_contract`
/`auth`: fixture com `oauth` e **sem** `api_key` → headless obtém access +
account e **não** sai Auth 20. Fixture só `api_key` permanece first-wins env >
arquivo.

### Sessão 2 — N2: Todo no loop + `TodoChanged` — **FEITO**

**Buraco:** `ToolRegistry` do run real tem só read/list/search/write/patch/shell.
`ask_question` entra só com `interaction_route`. O modelo não consegue criar
todos. O dock TUI (`Ctrl+T`, `UiEvent::TodoChanged`, `app.rs` aplica e abre o
dock) nunca recebe evento do CLI: `from_core` não mapeia todo.

**Fazer:**

- Em `execute_provider_turn_async`, abrir `Runtime::open_capability_bridge`
  (MemoryRepo se não houver sessão; repo da sessão se `--session`/`--resume`).
- Anunciar tool `todo` (add / set_status) em `provider_tool_definitions`.
- `execute_provider_tool_call`: nomes de task → `apply_task_mutation`.
- Projector CLI: após mutação, `UiEvent::TodoChanged` com `TodoItemView`.
- Plan/Goal **no mesmo match** de dispatcher, sem dock/DAG/Goal UI.

**Não fazer:** inspector de Plan, superfície Goal, persistência além do repo
que o run já abre, `PlanChanged`/`GoalChanged` visuais.

**Teste:** `runtime_capabilities.rs` já cobre mutação/restore. Falta o mínimo
no caminho real: `agent_loop.rs` (payload contém `"name":"todo"` e o tracker
não fica vazio) + `tui_bridge` projetando `TodoChanged`. Goldens de dock
(`layout_golden`) já existem se o evento chegar.

### Sessão 3 — N3: Skill discover + invoke — **FEITO**

**Buraco:** `discover` e `invoke_script` existem e só testes os chamam.
`InProcessCapabilityAdapter` devolve `Failed` para Skill de propósito
(“precisa de adapter explícito”) — e `invoke_script` **é** esse adapter, não
ligado. CLI/TUI nunca chamam `open_capability_bridge`.

**Fazer:**

- No mesmo startup do bridge (N2): `discover` nos roots documentados em
  `RUST-CLI.md` — `<cwd>/.slim/skills`, `<cwd>/.agents/skills`,
  `~/.slim/skills`, `~/.agents/skills`.
- Anunciar `skill.<name>` nas tool definitions do provider.
- Adapter: Skill → `invoke_script` (PowerShell, timeout/output já bounded),
  não `Failed`.
- Trust `Explicit` (default de `add_skill`): uma pergunta Y/N via
  `ask_question` já wired. **Não** estender `Approve` nesta sessão (isso é N7).

**Não fazer:** transporte MCP, child agent, editor de skills, overlay novo,
anunciar skill não descoberta.

**Fecho (2026-08-25):** tool única `skill` (lazy). Catálogo **não** vai no
schema. Roots: `cwd/.slim/skills` depois `%USERPROFILE%/.slim/skills`.
`list=true` devolve nomes; `name` corre `run.ps1` ou carrega o corpo do
`SKILL.md`. `~/.agents` fora do loop. Testes `agent_loop` (invoke + list).

---

## 4. N1 — detalhe de evidência

```184:186:crates/slim-cli/src/auth.rs
    let Some(key) = provider.api_key else {
        let _ = provider.oauth;
        return Ok(None);
```

`OAuthStore::save_locked` grava a chave `"oauth"`, não `"api_key"`.
`prepare_tui` para Codex ignora `api_key_request` e só segue se houver OAuth
ativo. Headless Anthropic só usa `anthropic_oauth` quando
`request.account_id.is_some()`.

O usuário autenticado na TUI **deixa de conseguir** o mesmo provider no
`slim --headless`. Completar > criar.

---

## 5. N2 — detalhe de evidência

`RuntimeCapabilityBridge` é construído em testes (`runtime_capabilities.rs`) e
exposto em `Runtime::open_capability_bridge`. Grep em `crates/slim-cli`:
**zero** referências. Presença de arquivo ≠ wiring.

`provider_tool_definitions` só faz `definitions_for_mode` + `ask_question` se
houver rota. Todo/Plan/Goal são `TaskMutation` no ledger, não tools do
registry.

TUI: `TodoChanged` atualiza `todo_items` e abre o dock se a lista não for
vazia. Sem emissão, `Ctrl+T` só mostra dock vazio.

Plan/Goal entram no **mesmo** dispatcher para não criar três fatias. Goal
sem superfície; Plan sem DAG visual nesta fatia.

---

## 6. N3 — detalhe de evidência

`InProcessCapabilityAdapter` documenta que Skill retorna `Failed` porque
“lançar script é fronteira de processo”. `invoke_script` /
`invoke_script_with_limits` já implementam essa fronteira (path jailed,
timeout 30 s, cap 64 KiB, cancel, só `OperatingMode::Auto` + `trusted`).

`CapabilityCatalog::add_skill` default `trusted: false` →
`AuthorizationRequirement::Explicit`. Sem pergunta, o dispatch falha em
trust — anunciar skill sem trust é pior que não anunciar.

MCP **não** entra nesta fatia: `JsonLineFramer`, `authorize_http` e
`McpLifecycle` são stubs; `McpCatalog::call_tool` é contrato in-process, não
servidor.

---

## 7. N4 — Plan na TUI roda o loop

`execute_provider_turn_async` retorna `ExitCode::ApprovalRequired` e
`text: "approval_required"` **antes** de criar `Runtime`, se
`mode == Plan`. A TUI `SendPrompt` clona `startup.request` (mode incluso).
`execution_result_events` transforma isso em `Notification` + `RunStopped`.
O bloco tipado `ApprovalRequired` (reducer, Y/N) **nunca** é usado no
caminho de produção: o único `EventKind::ApprovalRequired` está em
`protocol_golden.rs`.

Headless/`run_fake_headless` abortar Plan é contrato testado
(`headless_contract.rs`, `provider_cli.rs`, `cli_contract.rs` → exit 10).
**Manter** o abort headless. Só o caminho TUI deixa de short-circuit e
roda o loop com tools read-only já filtradas por `names_for_mode`.

Armadilha: `tui_runtime.rs` `tui_projection_redacts_the_configured_key...`
usa `OperatingMode::Plan` precisamente para **não** bater na rede. Ao
mudar N4, esse teste deve passar a `ReadOnly` ou a um prompt vazio —
não “consertar” o produto para o teste antigo.

**Não fazer:** DAG visual, `PlanApprove` nesta sessão, mudar exit 10
headless.

**Teste:** fixture localhost TUI em Plan que **faz** request e só anuncia
read/list/search (sem write/shell) + ausência de toast `approval_required`
no primeiro evento de conteúdo.

---

## 8. N5 — `ask_question` no resume TUI

`start_active_run`: se `resume_path.is_some()`, `interaction_route` e
`interaction_responder` são `None`. O loop não anuncia `ask_question`.
Contrato atual (DESIGN/tracker): headless, Plan e resume durável não
anunciam — para não replay de efeitos.

O buraco real é só TUI resume **nova** pergunta no turno novo, sem
reexecutar tool pendente. Resume já reconstrói histórico e não replaya
efeitos (`run_provider_resume_with_events`).

**Fazer:** passar `interaction_route()` também no braço durável TUI.
Headless continua sem rota.

**Não fazer:** anunciar em headless; replay de `QuestionRequired`
persistido; mudar Plan.

**Teste:** estender `ask_question_tui.rs` / `headless_resume.rs` com spawn
`spawn_tui_runtime_with_resume` e assert de `"name":"ask_question"` no
primeiro request do turno novo.

---

## 9. N6 — default Anthropic (P4 confirmado)

Sem `--model` / `SLIM_MODEL` / `slim.toml`:

| Superfície | Modelo |
|---|---|
| Headless `cli.rs` | `claude-3-5-sonnet-latest` |
| TUI `tui.rs` `defaults()` | `claude-sonnet-4-6` |

Mesmo `--provider anthropic` diverge qualidade/custo. Unificar no helper
`defaults()` (TUI já tem o par endpoint+model). Escolher **um** id — o da
TUI é o que o usuário vê no overlay; headless deve seguir, ou ambos um
constante única em slim-core/cli.

**Não fazer:** catálogo Anthropic live; heurística de “melhor modelo”.

**Teste:** unit nos dois caminhos (`cli.rs` / `tui.rs` `defaults`) com o
mesmo `&'static str`.

---

## 10. N7 — Approve/Reject durante run ativo

Com run ativo, o worker trata `AnswerQuestion` e **rejeita**
`AnswerInput` / `Approve` / `Reject` via `reject_unbound_interaction`.
`InteractionRoute` só transporta `QuestionAnswer` (oneshot).

Hoje é dormante: o loop nunca dá `push_event(ApprovalRequired)`. Vira
bloqueio quando N3 usar trust Explicit **sem** `ask_question`, ou quando
PlanApprove precisar de Y/N nativo.

Preferência deste ciclo: N3 usa `ask_question` Y/N e **adiar** N7. Só
implementar N7 se a pergunta estruturada não couber no trust (dois canais
de interação no mesmo turno).

**Não fazer:** overlay de approval novo; protocol buffer extra.

---

## 11. WONTFIX / fora de escopo (uma frase cada)

| Item | Por quê |
|---|---|
| Inspectors, Diff drawer, SessionTree, Diagnostics, search (C7 / DESIGN P2) | Superfície permanente; o loop já mostra tools no transcript |
| ParseCache / LayoutCache novos | WrapCache já cobre altura e corpo; não fecha buraco do agente |
| `SurfaceBackend`, sequences M0, blocos Plan/Compaction/Custom | Contrato visual M0, não invocação |
| Telemetry §26 / métricas extra de runtime | Loop já usa compact/usage; export não completa o agente |
| Provider novo, protocolo novo, chrome, motion, tema, paleta | Recusado neste ciclo |
| Dependência nova, crate extra, plugin, trait genérica de backend | Recusado |
| Reabrir G252–G255, G244–G251 | Já no tracker §7 como GREEN |
| G235 malformed tool DeepSeek | Diagnóstico existe; sem repro → não “consertar o parser” |
| Transporte MCP | `JsonLineFramer`/`authorize_http`/`McpLifecycle` são stubs; spawn stdio/HTTP é protocolo novo |
| Child provider-backed | `Scheduler` sem nested `run_agent_loop`; anunciar spawn que falha piora o agente |
| ConPTY físico (C5) | Bloqueio de host (`#[ignore]`); não é fatia de produto agora |
| Cache HTTP “inativo no caminho normal” | Doc velho: G236 ligou `HttpProviderClient::with_cache` nos quatro braços |
| Compact, tool result bounded, `LoopGuard`, cancel, fila de prompts | Já no caminho real; não fatiar |
| Abrir o bridge **sem** anunciar tool | Zero observável; N2/N3 são o fecho, não “registrar por registrar” |

---

## 12. P4–P8 da auditoria 2026-08-25 (revalidados)

A auditoria em `analysis_outputs/` descreve o passado (P2 A1–A4 = G252–G255,
já fechados). Só P4–P8 foram reabertos no código:

| ID | Veredito | Motivo |
|---|---|---|
| P4 | **ACEITO = N6** | Defaults Anthropic ainda divergem |
| P5 | **Descartar** | `ContentBlockStop` com índice ausente em `anthropic_calls` retorna `Ok(())` (`runtime/mod.rs` 1826–1832) — stop de bloco de texto, não tool drop |
| P6 | **Descartar** | `AppHandle::push_event` só exige seq monotônica; um `Runtime` por turno; slim-cli não cruza sessões no mesmo handle |
| P7 | **Descartar** | `execute_ask_question` faz await **antes** do task terminar; `RunStopped` só em `execution_result_events` pós-task |
| P8 | **Descartar** | TUI mapeia `InvalidResponse` de spawn (cwd/wake/thread) para `ExitCode::Internal` (30); CLI provider é 21. Não é o bug G255 |

P1–P3 daquela auditoria (IO sync de tool, backpressure, cache hit/usage)
**não** foram remedidos aqui; não viram fatia sem medida nova.

---

## 13. O que já está no caminho real (não fatiar)

- Tools nativas + filtro de modo (`ToolRegistry`).
- Compactação bounded, `LoopGuard` one-strike, `max_turns` / `max_tool_calls`.
- Tool output > cap → artifact store (`materialize_results`).
- Cancel cooperativo provider/shell; fila TUI FIFO (G244).
- `ask_question` TUI Auto/ReadOnly não-durável, setas no reducer (G252).
- Resume sem replay de efeitos (Etapa 8).
- `ProviderCache` LRU no cliente HTTP do run normal (G236).
- Stdin piped headless (G253); projeto vence global no TOML (G254).

---

## 14. Como executar uma fatia (contrato de entrega)

Prevalece `RULES.md` / `AGENTS.md`:

1. Teste RED primeiro no caminho que o usuário vê (CLI binário ou TUI
   fixture), não só no tipo isolado.
2. Completar caller existente; não criar cache/trait/drawer.
3. `cargo test --workspace` verde (0 failed, 0 warnings).
4. `.\refresh-slim.ps1 -Test` imprime `OK:` (binário no PATH).
5. Atualizar este arquivo (marcar fatia feita), tracker §7, DESIGN §1.1 se
   o comportamento visível mudou, e números de testes em todos os READMEs
   que os citam.
6. Checklist da seção 4 de `RULES.md` na resposta final.

Ao marcar uma fatia feita neste arquivo: risco residual, teste que prova, e
o que **não** entrou. Não apagar a evidência de código — atualizar linhas se
o patch as moveu.

---

## 15. Incertezas da sessão de análise (2026-08-25)

- Não rodou `cargo test` nem providers comerciais; este arquivo **não** cita
  contagem de testes, latência ou SHA como verdade atual.
- Não abriu `auth.json` real do USERPROFILE — só o writer.
- Não observou no terminal o toast de Plan; o abort está no código.
- Prioridade/shadow dos roots de skill no **binário**: só testes +
  `RUST-CLI.md`, não o CLI.
- `invoke_script` assume PowerShell; skills sem `.ps1` não foram verificadas.
- G235 causa-raiz em DeepSeek live: não reproduzido.
- ConPTY físico: não observado.

---

## 16. Relação com outros docs

| Doc | Papel vs este arquivo |
|---|---|
| `DESIGN-SLIM-TUI.md` §§2–29 | Contrato de TUI. Itens M3 P0 ConPTY, M0 `SurfaceBackend`, ParseCache, inspectors, telemetry **não** sobem de prioridade por estarem na spec |
| `DESIGN-SLIM-TUI.md` §1.1 “antes de continuar M3” | Lista de *gate TUI*. Este arquivo corta essa lista para o ciclo do **agente** |
| `PLANO-IMPLEMENTACAO.md` §10.1 | Integração v1 ampla (MCP transporte, child real, E2E). Este arquivo é a fila **curta** de wiring |
| `AUDIT-SLIM-TUI-TRACKER.md` | Bugs G* e log de execução; C5/C7 continuam abertos e **fora** desta fila |
| `analysis_outputs/AUDIT-SLIM-AGENT-UI-TUI-CLI.md` | Auditoria 2026-08-25; P2 A1–A4 obsoletos; P4–P8 revalidados na §12 |

Quando este arquivo e o PLANO §10.1 divergirem no que fazer **agora**,
prevalece este arquivo para o ciclo harness-leve. MCP/child/E2E v1
continuam no plano, não nesta fila.
