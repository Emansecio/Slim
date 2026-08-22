# Plano de implementação do Slim

> **Status de implementação:** checkpoint parcial; definição de v1 ainda não
> satisfeita. Consulte o [status atual](README.md).

> **For agentic workers:** REQUIRED SUB-SKILL: Use `subagent-driven-development` or `executing-plans` to implement this plan task-by-task.

**Goal:** Construir a v1 diária do Slim em Rust, Windows-only, com runtime único para headless/TUI, sessões, compaction, skills, MCP, subagentes, Todo/Plan/Goal e execução irrestrita em `Auto`.

**Architecture:** Monólito modular com três crates próprios: `slim-core`,
`slim-tui` e `slim-cli`. O core é a única autoridade de sessão, provider,
tools, contexto, skills, MCP e subagentes; headless e TUI apenas compõem o mesmo
`AppHandle` e consomem o mesmo stream de eventos.

**Tech Stack:** Rust stable + MSVC, Tokio, Ratatui, Crossterm, serde, serde_json,
JSONL append-only, ACL nativa Windows via `windows-sys`, provider adapters
tipados, MCP stdio/Streamable HTTP e testes unit/property/golden/PTY.

## Estado de implementação (atualizado em 2026-08-22)

O workspace tem 215 passed / 0 failed / 1 ConPTY físico ignored em 45 suítes
e build/deploy release aprovado por `refresh-slim.ps1 -Test`. Clippy
focado em `slim-core` está verde. `cargo fmt --all -- --check` e Clippy workspace
ainda expõem drift/6 diagnósticos preexistentes fora do slice PERF-02. Isso
comprova componentes, headless e a bridge TUI central offline, não integração
v1 completa.

Integrado no headless: provider OpenAI-compatible/Anthropic SSE; read/list/search,
write/patch/shell; filtragem de capabilities; auth nativa com DACL; `--image`
local; compaction; usage; artifact handles; anti-loop; e gravação JSONL nova via
`--session`.

Integrado também na TUI: composer, auth/config, provider SSE incremental, agent
loop, tools, usage, modos, cancelamento por abort do request e restauração
fullscreen. Após a execução do tracker de auditoria (AUDIT-SLIM-TUI-TRACKER.md),
integram-se ainda: reducer único normativo (`Action → reduce → Effect`), lanes
bounded control/data com fairness e coalescer no runtime real, scrollback
virtualizado via HeightIndex/WrapCache com pin/live-edge/unseen, paleta §21.3
estratificada pintada por região, ActivityRail com spinner animado e medidor de
contexto na operational bar (W2, 2026-08-21: ContextRail superior removida),
composer boxed de três rows, tool blocks tipados agregados por nome/turno,
command palette Ctrl+P, markdown-light, UTF-8/VT console flags com restore exato,
proptest/fault injection/golden matrix via TestBackend e benchmark long-session
com gate §27 (p95 ~2 ms ≤ 16 ms em release). Estado re-verificado em
2026-08-22 (rustc 1.97.1): 45 suítes / 215 passed / 0 failed / 1 ConPTY ignored e
0 warnings no build — detalhes e achados de ambiente no tracker §§7–8.

Ainda não integrado no caminho normal: cache HTTP usa implementação/testes,
mas não o construtor normal; CLI/TUI não oferecem resume/recovery selection/branch;
Skills, MCP e subagentes não entram no startup/loop/catálogo; Todo/Plan/Goal não
são tools vivas e Plan retorna `approval_required` antes de gerar plano via provider.
PTY/ConPTY E2E físico permanece `#[ignore]` (requer console Windows real) e a
matriz física de terminais continua não verificada. Release determinístico/
hash-valid empacota este checkpoint parcial.

---

## 1. Como usar este plano

Leitor: agente executor que não conhece a conversa anterior. Após ler, ele deve
conseguir criar o workspace, executar cada fatia, parar em blockers reais e
provar a v1 sem inventar capabilities.

Fontes normativas, em ordem:

1. [DECISOES-GRILL-PRE-IMPLEMENTACAO.md](DECISOES-GRILL-PRE-IMPLEMENTACAO.md);
2. [RUST-CLI.md](RUST-CLI.md);
3. [DESIGN-SLIM-TUI.md](DESIGN-SLIM-TUI.md);
4. [VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md);
5. [INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md](INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md), somente como evidência.

O checkpoint final vence qualquer trecho histórico. Não implementar Fusion,
worktrees, servidor remoto, RPC persistente, sidecar Node, memória global,
plugins, marketplace, Unix/macOS, inline ou LSP/DAP/browser na v1.

Regra de trabalho:

- uma tarefa por vez;
- teste falhando antes do código quando houver contrato observável;
- nenhuma etapa declara sucesso sem comando e saída esperada;
- se um POC falhar, registrar evidência e parar a fatia dependente;
- não alterar `CHANGELOG.md`;
- não criar crate adicional sem justificar boundary e custo de compilação.

## 2. Pronto antes de escrever código

### Task 0: pré-flight Windows e POCs de risco

**Arquivos:**

- Create: `D:\Slim\POC-RESULTS.md`
- Create: `D:\Slim\poc\README.md`
- Create: `D:\Slim\poc\toolchain\`
- Create: `D:\Slim\poc\search\`
- Create: `D:\Slim\poc\tui\`
- Modify: `D:\Slim\Documentações - Projeto\VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md`
- Modify: `D:\Slim\Documentações - Projeto\README.md`

- [ ] **Step 1: registrar o ambiente sem instalar nada**

  Run in PowerShell:

  ```powershell
  rustc --version
  cargo --version
  cl.exe 2>&1 | Select-Object -First 1
  git --version
  ```

  Expected: versões de Rust/Cargo, linker MSVC utilizável e Git. Se qualquer
  item faltar, registrar `BLOCKED_TOOLCHAIN` em `POC-RESULTS.md` e não iniciar
  as Tasks 1–9.

- [ ] **Step 2: provar compilação mínima MSVC**

  Criar um crate descartável dentro de `poc/toolchain`, executar:

  ```powershell
  cargo check
  cargo build --release
  ```

  Expected: exit code 0 em debug e release. Registrar versões, target e tempo.

- [ ] **Step 3: provar lifecycle fullscreen**

  O POC deve entrar/sair 100 vezes, restaurar cursor/raw mode/alternate screen
  após saída normal, Ctrl+C e panic, e sobreviver a resize durante streaming
  fake. Expected: zero terminal leak e zero processo órfão.

- [ ] **Step 4: comparar busca**

  Medir `fff-search` contra `ripgrep` no Windows em: primeira busca, warmup,
  cem buscas repetidas, criação/rename/remoção, `.gitignore`, binário e Unicode.
  Expected: relatório com p50/p95, RSS, correção e decisão. Até o resultado,
  `ripgrep` é baseline e nenhum backend fica congelado.

- [ ] **Step 5: registrar e relinkar o resultado**

  Escrever em `POC-RESULTS.md`: ambiente, comando, resultado, artefatos,
  limitações e blocker. Atualizar o índice sem transformar medição em claim.

## 3. Estrutura do workspace

### Task 1: criar o monólito modular e o testkit

**Arquivos:**

- Create: `D:\Slim\Cargo.toml`
- Create: `D:\Slim\crates\slim-core\Cargo.toml`
- Create: `D:\Slim\crates\slim-core\src\lib.rs`
- Create: `D:\Slim\crates\slim-tui\Cargo.toml`
- Create: `D:\Slim\crates\slim-tui\src\lib.rs`
- Create: `D:\Slim\crates\slim-cli\Cargo.toml`
- Create: `D:\Slim\crates\slim-cli\src\main.rs`
- Create: `D:\Slim\tests\fixtures\README.md`
- Create: `D:\Slim\tests\support\fake_clock.rs`
- Create: `D:\Slim\tests\support\fake_provider.rs`

Dependências iniciais:

- `slim-core` não depende de Ratatui/Crossterm;
- `slim-tui` depende de `slim-core` e recebe apenas `AppHandle`/eventos;
- `slim-cli` compõe core e TUI;
- protocolo permanece módulo de `slim-core` até existir consumidor independente.

- [ ] **Step 1: declarar somente os três members**

  Run:

  ```powershell
  cargo metadata --no-deps --format-version 1
  ```

  Expected: exatamente `slim-core`, `slim-tui` e `slim-cli`.

- [ ] **Step 2: criar teste de composição mínima**

  Teste: `tests/smoke_workspace.rs` deve importar os três crates, construir um
  `AppHandle` fake e processar um evento `SessionSnapshot`.

  Run:

  ```powershell
  cargo test --workspace smoke_workspace
  ```

  Expected: PASS sem provider real, terminal ou filesystem externo.

- [ ] **Step 3: aplicar os gates básicos**

  ```powershell
  cargo fmt --all -- --check
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  ```

  Expected: três comandos com exit code 0.

## 4. Contratos do core

### Task 2: protocolo, provider fake e perfis

**Arquivos:**

- Create: `crates/slim-core/src/protocol.rs`
- Create: `crates/slim-core/src/events.rs`
- Create: `crates/slim-core/src/model.rs`
- Create: `crates/slim-core/src/provider.rs`
- Create: `crates/slim-core/src/profiles.rs`
- Test: `crates/slim-core/tests/protocol_golden.rs`
- Test: `crates/slim-core/tests/provider_fake.rs`

Contrato mínimo:

- `OperatingMode = Auto | ReadOnly | Plan`;
- `OperatingMode` inicial é `Auto`;
- profiles `default`, `fast`, `deep`, `compact` carregam provider/model/effort;
- provider gera stream normalizado de texto, reasoning, tool call, usage,
  stop reason e erro;
- retry automático existe apenas para transporte seguro;
- nenhuma troca silenciosa de provider/modelo;
- reasoning textual é persistido, mas a TUI o recolhe.

- [ ] **Step 1: escrever goldens de eventos**

  Cobrir session start, mode change, assistant stream, tool lifecycle,
  compaction, `approval_required`, `input_required`, subagent activity e
  terminal error. Cada evento recebe `seq` monotônico.

- [ ] **Step 2: implementar provider fake determinístico**

  O fake deve emitir uma sequência controlável sem rede, clock real ou sleep.
  Testar success, malformed tool call, transport retry e cancellation.

- [ ] **Step 3: implementar perfis**

  Perfil ausente falha com erro configurável; subagente herda perfil do pai e
  pode receber override explícito.

- [ ] **Step 4: gates**

  ```powershell
  cargo test -p slim-core protocol_golden provider_fake
  cargo clippy --workspace --all-targets -- -D warnings
  ```

  Expected: PASS e nenhum warning.

### Task 3: sessão JSONL, snapshots e índice

**Arquivos:**

- Create: `crates/slim-core/src/session/mod.rs`
- Create: `crates/slim-core/src/session/event_log.rs`
- Create: `crates/slim-core/src/session/snapshot.rs`
- Create: `crates/slim-core/src/session/index.rs`
- Create: `crates/slim-core/src/session/recovery.rs`
- Test: `crates/slim-core/tests/session_recovery.rs`
- Test: `crates/slim-core/tests/session_branch.rs`

Contrato:

- JSONL append-only nativo Slim;
- um writer por sessão;
- schema atual + duas versões Slim anteriores;
- branch cria novo session ID com parent e ponto de corte;
- snapshot e índice são caches rebuildable;
- linha parcial preserva prefixo válido e move sufixo para quarantine;
- nenhuma leitura/escrita de sessão do Pit.

- [ ] **Step 1: testar replay e crash recovery**
- [ ] **Step 2: implementar writer atomicamente**
- [ ] **Step 3: testar branch/fork e parent relation**
- [ ] **Step 4: implementar snapshot/index rebuildable**
- [ ] **Step 5: executar**

  ```powershell
  cargo test -p slim-core session_
  ```

  Expected: PASS, incluindo processo interrompido após cada tipo de evento.

## 5. Runtime headless e economia de contexto

### Task 4: dispatcher, modos, tools e anti-loop

**Arquivos:**

- Create: `crates/slim-core/src/runtime/mod.rs`
- Create: `crates/slim-core/src/runtime/app_handle.rs`
- Create: `crates/slim-core/src/runtime/mode.rs`
- Create: `crates/slim-core/src/tools/mod.rs`
- Create: `crates/slim-core/src/tools/read.rs`
- Create: `crates/slim-core/src/tools/write.rs`
- Create: `crates/slim-core/src/tools/patch.rs`
- Create: `crates/slim-core/src/tools/list.rs`
- Create: `crates/slim-core/src/tools/search.rs`
- Create: `crates/slim-core/src/tools/shell.rs`
- Create: `crates/slim-core/src/runtime/loop_guard.rs`
- Test: `crates/slim-core/tests/tool_contracts.rs`
- Test: `crates/slim-core/tests/mode_capabilities.rs`
- Test: `crates/slim-core/tests/anti_loop.rs`

Regras:

- Auto expõe tools irrestritas;
- Plan/Read-only expõem read/list/search e resources/prompts, nunca shell,
  write, patch, scripts ou MCP tools;
- delete/rename/mkdir não são tools nativas;
- `read` é numerado e bounded;
- edit ambíguo falha sem mutar;
- write overwrite exige precondition e troca atômica;
- somente reads conhecidas executam em paralelo;
- shell usa cwd de workspace, herda ambiente completo e roda foreground;
- chamada de tool estritamente idêntica que falhou é bloqueada no turno;
- output grande vira preview head/tail + handle integral.

- [ ] **Step 1: escrever testes de capability surface**
- [ ] **Step 2: implementar registry e dispatcher único**
- [ ] **Step 3: implementar read/list/search com interface estruturada**
- [ ] **Step 4: implementar write/patch com stale checks**
- [ ] **Step 5: implementar shell streaming/cancel/teardown**
- [ ] **Step 6: implementar fingerprint de erro e bloqueio de repetição**
- [ ] **Step 7: executar**

  ```powershell
  cargo test -p slim-core tool_contracts mode_capabilities anti_loop
  ```

  Expected: PASS; nenhuma tool mutante disponível em Plan/Read-only.

### Task 5: compaction e budget de contexto

**Arquivos:**

- Create: `crates/slim-core/src/context/mod.rs`
- Create: `crates/slim-core/src/context/budget.rs`
- Create: `crates/slim-core/src/context/compact.rs`
- Create: `crates/slim-core/src/context/artifacts.rs`
- Test: `crates/slim-core/tests/context_budget.rs`
- Test: `crates/slim-core/tests/compaction.rs`

Contrato:

- `/compact` manual;
- auto-compaction pre-send por reserva projetada, somente quando há histórico
  antigo;
- threshold 85% geral e 50% para janela de pelo menos 1M;
- estimador heurístico (não tokenizer-exato) e mesmo adapter/modelo produz o
  resumo;
- original JSONL permanece preservado;
- o prompt mais recente e pares assistant/tool completos são preservados;
- a janela e a reserva são revalidadas antes e depois da compactação;
- output integral vive em artifact handle;
- falhas de resumo ou estouro remanescente são explícitos; não implementar
  live/proactive/mid-turn ou memória global.

- [ ] **Step 1: escrever casos de janela 32k, 128k e 1M**
- [ ] **Step 2: implementar cálculo de reserva**
- [ ] **Step 3: implementar resumo e preservação de anchors**
- [ ] **Step 4: testar overflow e recuperação**
- [ ] **Step 5: executar**

  ```powershell
  cargo test -p slim-core context_budget compaction
  ```

  Expected: PASS e nenhuma chamada provider quando o contexto ainda couber.

### Task 6: canais headless e steering

**Arquivos:**

- Create: `crates/slim-cli/src/config.rs`
- Create: `crates/slim-cli/src/auth.rs`
- Create: `crates/slim-cli/src/headless.rs`
- Create: `crates/slim-cli/src/jsonl.rs`
- Create: `crates/slim-cli/src/exit_codes.rs`
- Test: `crates/slim-cli/tests/headless_contract.rs`

Contrato:

- prompt por argumento ou stdin;
- saída `text` ou JSONL versionado;
- Plan sem UI emite `approval_required` e encerra sem executar;
- ask sem UI emite `input_required`, persiste pergunta e permite resume;
- exit codes distinguem success, approval, blocked, cancelled, auth, provider,
  tool e internal;
- configuração segue CLI > env > projeto > global;
- `auth.json` opcional, somente leitura, schema estrito `version: 1`, com
  prioridade `SLIM_API_KEY` > variável específica do provider >
  `SLIM_AUTH_FILE` > `%USERPROFILE%\.slim\auth.json`;
- em Windows, arquivo existente é aberto por handle `windows-sys` e recebe DACL
  protegida com allowlist exata do owner atual, usuário atual, `SYSTEM` e
  `Administrators`; o teste inclui caminho Unicode. Ausente não é criado;
  symlink, reparse point, schema, caminho ou ACL inseguros falham fechando; não
  há OAuth;
- telemetria somente local.

- [ ] **Step 1: escrever testes de stdin/argumento e exit codes**
- [ ] **Step 2: implementar renderer text**
- [ ] **Step 3: implementar renderer JSONL**
- [ ] **Step 4: implementar approval/input/resume**
- [ ] **Step 5: implementar redaction e configuração**
- [ ] **Step 6: executar**

  ```powershell
  cargo test -p slim-cli headless_contract
  cargo run -p slim-cli -- --help
  ```

  Expected: help retorna 0; fixtures headless passam sem API key.

## 6. Skills, MCP e subagentes

### Task 7: skills nativas

**Arquivos:**

- Create: `crates/slim-core/src/skills/mod.rs`
- Create: `crates/slim-core/src/skills/discovery.rs`
- Create: `crates/slim-core/src/skills/metadata.rs`
- Create: `crates/slim-core/src/skills/invocation.rs`
- Test: `crates/slim-core/tests/skills_roots.rs`

Contrato:

- roots: `--skills-dir`, projeto Slim, projeto compatível, global Slim, global
  compatível;
- root mais específico vence colisão e shadowing é diagnosticado;
- startup carrega metadata; corpo/recursos on-demand;
- skill inválida gera aviso e não bloqueia startup;
- trust revalida somente skill/script/MCP executável nova ou alterada;
- scripts rodam somente em Auto; instruções/referências podem carregar em todos.

- [ ] **Step 1: testar precedência, colisão e skill inválida**
- [ ] **Step 2: implementar metadata-first**
- [ ] **Step 3: implementar invocation e modo Auto**
- [ ] **Step 4: executar**

  ```powershell
  cargo test -p slim-core skills_roots
  ```

### Task 8: MCP mínimo

**Arquivos:**

- Create: `crates/slim-core/src/mcp/mod.rs`
- Create: `crates/slim-core/src/mcp/stdio.rs`
- Create: `crates/slim-core/src/mcp/http.rs`
- Create: `crates/slim-core/src/mcp/catalog.rs`
- Create: `crates/slim-core/src/mcp/lifecycle.rs`
- Test: `crates/slim-core/tests/mcp_contract.rs`

Contrato:

- stdio e Streamable HTTP;
- tools/resources/prompts;
- namespace `mcp.<server>.<tool>`;
- resources/prompts só por seleção explícita;
- lazy init, timeout, cancellation, caps e teardown;
- server config de projeto vence global;
- uma conexão por config, compartilhada com subagentes;
- falha isola o server; reconnect só na próxima chamada;
- Plan/Read-only escondem MCP tools;
- sem OAuth MCP na v1.

- [ ] **Step 1: escrever fixtures de framing fragmentado**
- [ ] **Step 2: implementar initialize/catalog**
- [ ] **Step 3: implementar tool/resource/prompt calls**
- [ ] **Step 4: implementar cancel/restart/teardown**
- [ ] **Step 5: executar**

  ```powershell
  cargo test -p slim-core mcp_contract
  ```

### Task 9: Todo, Plan, Goal e subagentes

**Arquivos:**

- Create: `crates/slim-core/src/task/mod.rs`
- Create: `crates/slim-core/src/task/todo.rs`
- Create: `crates/slim-core/src/task/plan.rs`
- Create: `crates/slim-core/src/task/goal.rs`
- Create: `crates/slim-core/src/agents/mod.rs`
- Create: `crates/slim-core/src/agents/child_session.rs`
- Create: `crates/slim-core/src/agents/scheduler.rs`
- Create: `crates/slim-core/src/agents/activity.rs`
- Test: `crates/slim-core/tests/task_contract.rs`
- Test: `crates/slim-core/tests/subagent_contract.rs`

Contrato:

- Todo: pending/in_progress/completed/blocked/cancelled; um in_progress por
  agente;
- Plan: DAG versionado, revisão imutável, aprovação explícita e receipts
  opcionais;
- Goal: active/paused/complete/blocked, budget opcional e assurance
  verified/unverified;
- não-progresso comprovado pausa/bloqueia com evidência;
- subagente depth-1, quatro ativos, fila FIFO 32;
- herda contexto selecionado e profile, com override explícito;
- filhos read-only em paralelo; mutação serial no workspace;
- spawn/status/join/cancel/list, child session e resultado estruturado;
- activity cobre tool/MCP/child; cancelamento do pai não deixa órfãos.

- [ ] **Step 1: escrever testes de estados Todo/Plan/Goal**
- [ ] **Step 2: implementar DAG, aprovação e receipts opcionais**
- [ ] **Step 3: implementar Goal budget/anti-loop**
- [ ] **Step 4: implementar child session/scheduler bounded**
- [ ] **Step 5: implementar mutation lease serial do workspace**
- [ ] **Step 6: executar**

  ```powershell
  cargo test -p slim-core task_contract subagent_contract
  ```

## 7. TUI fullscreen M0–M3

### Task 10: `slim-tui` M0 — reducer fake

**Arquivos:**

- Modify: `crates/slim-tui/src/api.rs`
- Create: `crates/slim-tui/src/app.rs`
- Create: `crates/slim-tui/src/reducer.rs`
- Create: `crates/slim-tui/src/view_model.rs`
- Create: `crates/slim-tui/src/block.rs`
- Create: `crates/slim-tui/src/testkit.rs`
- Test: `crates/slim-tui/tests/m0_frames.rs`

- [ ] **Step 1: materializar `UiEvent`/`UiCommand` do design**
- [ ] **Step 2: escrever golden de sessão fake**
- [ ] **Step 3: implementar reducer puro e ViewModel**
- [ ] **Step 4: testar queued user, Plan, Goal, Activity e thinking collapsed**
- [ ] **Step 5: executar**

  ```powershell
  cargo test -p slim-tui m0_frames
  ```

  Expected: frames determinísticos sem I/O ou terminal real.

### Task 11: `slim-tui` M1 — fullscreen Windows

**Arquivos:**

- Create: `crates/slim-tui/src/terminal.rs`
- Create: `crates/slim-tui/src/fullscreen.rs`
- Create: `crates/slim-tui/src/input.rs`
- Create: `crates/slim-tui/src/composer.rs`
- Test: `crates/slim-tui/tests/pty_windows.rs`

- [ ] **Step 1: implementar TerminalGuard RAII**
- [ ] **Step 2: implementar alternate screen/raw mode/cursor restore**
- [ ] **Step 3: implementar composer, paste atômico e IME**
- [ ] **Step 4: implementar layout fullscreen e operational bar**
- [ ] **Step 5: testar Ctrl+C, Esc, resize e panic restore**
- [ ] **Step 6: executar POC-1/POC-2/POC-3**

  Expected: zero terminal leak em 100 ciclos e goldens aprovados.

### Task 12: `slim-tui` M2 — integração e performance

**Arquivos:**

- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Create: `crates/slim-tui/src/render.rs`
- Create: `crates/slim-tui/src/layout.rs`
- Create: `crates/slim-tui/src/cache.rs`
- Create: `crates/slim-tui/benches/long_session.rs`
- Test: `crates/slim-tui/tests/m2_integration.rs`

- [ ] **Step 1: conectar `AppHandle` e stream de eventos reais**
- [ ] **Step 2: implementar coalescing de 16 ms e backpressure**
- [ ] **Step 3: implementar tool/MCP/child Activity**
- [ ] **Step 4: implementar Todo dock, queue blocks e content handles**
- [ ] **Step 5: implementar Parse/Wrap/Layout caches e virtualização**
- [ ] **Step 6: executar**

  ```powershell
  cargo test -p slim-tui m2_integration
  cargo bench -p slim-tui --bench long_session
  ```

  Expected: events de controle não são perdidos; p95 de input→frame ≤16 ms
  no corpus aprovado.

### Task 13: `slim-tui` M3 — experiência completa fullscreen

**Arquivos:**

- Modify: `crates/slim-tui/src/layout.rs`
- Modify: `crates/slim-tui/src/input.rs`
- Create: `crates/slim-tui/src/inspector.rs`
- Create: `crates/slim-tui/src/theme.rs`
- Create: `crates/slim-tui/src/image.rs`
- Test: `crates/slim-tui/tests/m3_golden.rs`
- Test: `crates/slim-tui/tests/fault_injection.rs`

- [ ] **Step 1: implementar command palette inline**
- [ ] **Step 2: implementar Diff/Activity/SessionTree/Diagnostics**
- [ ] **Step 3: implementar Shift+Tab e approval/input flows**
- [ ] **Step 4: implementar imagens capability-gated e fallback**
- [ ] **Step 5: implementar mouse opcional, clipboard e glyph fallback**
- [ ] **Step 6: executar POC-4/POC-6 e gates M3**

  Expected: todos os fluxos de task cognition e atividade são auditáveis na
  TUI, sem overlay permanente e sem terminal leak.

## 8. Integração final e release

### Task 14: smoke integrado da v1

**Arquivos:**

- Create: `tests/e2e_v1.rs`
- Create: `tests/fixtures/v1_fake_provider.jsonl`
- Modify: `D:\Slim\Documentações - Projeto\RUST-CLI.md`
- Modify: `D:\Slim\Documentações - Projeto\README.md`
- Modify: `D:\Slim\Documentações - Projeto\VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md`

Fluxo obrigatório, com provider fake:

1. iniciar sessão em Auto;
2. criar Todo;
3. executar read/search;
4. criar/revisar/aprovar Plan;
5. mudar para Auto;
6. usar write/apply_patch com stale check;
7. compactar e preservar Todo/Plan/Goal;
8. carregar skill metadata/body;
9. chamar MCP resource/prompt e tool em Auto;
10. spawnar subagente read-only e aguardar resultado;
11. enviar segunda mensagem e verificar FIFO;
12. pausar em `input_required`, responder via resume;
13. fechar, reabrir e criar branch;
14. iniciar TUI e verificar os mesmos eventos do headless.

- [ ] **Step 1: executar E2E hermético completo da v1**

  ```powershell
  cargo test --workspace --test e2e_v1
  ```

  Expected: PASS sem API key, rede externa ou terminal interativo, cobrindo
  também TUI real, cache habilitado, resume/branch, Skills, MCP, child agent e
  Todo/Plan/Goal. O E2E offline existente cobre somente providers, auth e
  headless/localhost; não é o fluxo completo acima.

- [x] **Step 2: executar gates automatizados existentes**

  ```powershell
  cargo fmt --all -- --check
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  ```

  Expected: todos exit code 0.

- [x] **Step 3: manter provider live fora do gate**

  A rota HTTP existe, mas esta validação usa apenas fixtures localhost. Não
  registrar integração live, OAuth ou credenciais como evidência do release.

### Task 15: release Windows (checkpoint, não gate de v1)

**Arquivos:**

- Create: `D:\Slim\release\build_release.py`
- Create: `D:\Slim\release\SHA256SUMS.txt`
- Create: `D:\Slim\release\README.md`
- Modify: `D:\Slim\README.md`
- Modify: `D:\Slim\Documentações - Projeto\README.md`

- [x] **Step 1: gerar binário release MSVC**

  ```powershell
  cargo build --workspace --release
  ```

- [x] **Step 2: validar execução portátil**

  ```powershell
  .\target\release\slim.exe --version
  .\target\release\slim.exe --help
  .\target\release\slim.exe --unknown-option
  ```

  Expected: `--version`/`--help` exit `0`, opção desconhecida exit `30` e nenhum
  runtime Node.

- [x] **Step 3: publicar EXE/zip/checksum/manifests**

  Executar `python3 release/build_release.py`. O builder deve incluir somente
  `target/release/slim.exe` como `slim.exe`, fixar timestamp/metadados ZIP,
  calcular hashes dos bytes reais e escrever `manifest.json` e checksums LF.
  Reexecutar duas vezes e comparar o SHA-256 do ZIP; validar com
  `sha256sum -c release/SHA256SUMS.txt`. Não incluir secrets, `.slim/auth.json`,
  sessões privadas, prompts, docs ou artefatos de workspace. O script deve
  resolver caminhos a partir do próprio arquivo e não criar diretórios
  temporários fora de `release/`.

## 9. Gates de parada

Parar e reportar, sem improvisar, se ocorrer:

- toolchain MSVC ausente;
- dependência exige Linux/macOS/WSL para o contrato Windows-only;
- provider exige um fluxo OAuth que não faz parte do contrato v1;
- `fff-search` não compila ou não supera o baseline/correção de `ripgrep` no
  cenário medido;
- Ratatui/Crossterm não restauram terminal no POC;
- schema de sessão não recupera prefixo válido após crash;
- Plan/Read-only expõe shell/write/MCP tool;
- chamada de tool idêntica é repetida após erro no mesmo turno;
- cancelamento deixa child/MCP/processo órfão;
- TUI precisa executar domínio diretamente em vez do `AppHandle`;
- qualquer feature rejeitada reaparece como “atalho temporário”.

## 10. Definição de v1 (ainda não satisfeita)

Só publicar 1.0 quando todos forem verdadeiros; o release atual não satisfaz
essa lista:

- Task 0 verde ou blockers documentados e aceitos;
- Tasks 1–9 entregam core headless testável;
- Tasks 10–13 entregam TUI M0–M3 Windows;
- Task 14 passa E2E fake sem rede **com todas as integrações**, não apenas os
  testes headless atuais;
- Task 15 gera EXE/zip/checksums/manifests;
- sessões, compaction, skills, MCP, subagentes, Todo/Plan/Goal e headless
  funcionam juntos;
- runtime Auto é irrestrito e Plan/Read-only não expõem mutações;
- nenhum gate depende de Fusion, worktree, Node, RPC, memória global,
  servidor remoto, Unix/macOS, inline ou LSP/DAP/browser.

## 10.1 Trabalho de integração restante (prioridade)

1. **Concluído — TUI central real:** `slim --tui` usa composer, auth/config,
   provider SSE, agent loop, tools, usage e cancelamento; fixtures provam delta
   antes do término, round-trip de tool e fechamento da conexão cancelada.
2. **P0 — cache:** construir o cliente normal com cache habilitado e provar
   hit/miss, invalidação e ausência de cache para tool calls.
3. **P0 — sessões:** expor resume, seleção de recovery e branch/fork na CLI e
   TUI, preservando JSONL e relações parent/leaf.
4. **P0 — Skills e MCP:** ligar descoberta/invocação e catálogo MCP ao startup,
   dispatcher e modos; exercitar tools/resources/prompts reais.
5. **P0 — subagentes:** ligar scheduler/activity/child-session a chamadas de
   modelo reais, com cancel/join e resultado estruturado.
6. **P0 — Todo/Plan/Goal:** registrar as operações como provider tools e fechar
   fluxo Plan → aprovação → execução, Goal e Todo end-to-end.
7. **P0 — E2E e gates:** executar o fluxo integrado completo, depois repetir
   gates automatizados, matriz física de terminal e o release determinístico.

## 11. Autoverificação do plano

- Cada requisito v1 do checkpoint aparece em pelo menos uma Task e em um gate.
- Não há passo “implementar depois” sem tarefa, arquivo e comando.
- O backend de busca, headless, trust/segredos e TUI possuem gates explícitos.
- O plano não exige código antes do toolchain/POCs mínimos.
- O plano preserva as fronteiras `slim-core`/`slim-tui`/`slim-cli`.

## 12. Estado de execução local

Atualizado após a execução local em 2026-08-20. O checklist acima permanece a
especificação das tarefas; esta seção registra evidência do worktree atual.

| Área | Estado | Evidência/limite |
|---|---|---|
| Tasks 1–9 + tracker TUI | componentes/contratos, headless e fundação TUI M0–M2 implementados em fatias | suíte atual: 215 passed / 0 failed / 1 ignored / 45 suítes; PTY físico `#[ignore]`; Skills, MCP, subagentes e Todo/Plan/Goal não estão ligados ao loop/catálogo |
| Task 10 | componentes M0/reducer/testes | usados pela aplicação TUI real |
| Task 11 | lifecycle/input M1 integrado | `--tui` usa composer/fullscreen real e restauração RAII |
| Task 12 | integração M2 central concluída | `AppHandle` publica SSE incremental para bridge; tools/usage/cancel funcionam; cache HTTP normal inativo |
| Task 13 | componentes M3/inspectors/fallback/testes | UX base integrada; matriz física completa continua pendente |
| Task 14 | **não concluída** | E2E offline cobre headless/localhost; fluxo v1 completo ainda não existe |
| Task 15 | checkpoint determinístico produzido | EXE/ZIP/manifest/checksums hash-valid; não representa v1 completa |
| Providers/headless | integrado localmente | SSE, tools nativas, modos, auth, multimodal, loop bounded e stops text/JSONL |
| Context/usage/artifacts | integrado localmente | compaction bounded, Usage, caps e handles; sem alegar cobertura das superfícies não ligadas |
| Cache | implementação/testes presentes, **inativo no caminho normal** | construtor normal `HttpProviderClient::new` |
| Sessões | writer e `--session` presentes | sem resume/recovery selection/branch UX |
| POC-0 | verde com nota de ambiente | Rust/MSVC via Developer Command Prompt |
| POC-1 | parcial comprovado | 100 ciclos e panic em ConPTY |
| POC-2 | verde no testkit | sete tamanhos normativos |
| POC-3 | parcial comprovado | key Press/Release, decoder bruto e paste simples; IME/multiline físico pendente |
| POC-4 | preliminar | pipeline sintético e `Terminal::draw` ConPTY abaixo de 16 ms neste hardware |
| POC-6 | fallback comprovado | detecção segura; matriz física multi-terminal pendente |
| Busca | baseline + candidato medidos | ripgrep versus `fff-search`, Git-ignore, watcher e RSS |

### Critério de publicação

Os artefatos de release são reproduzíveis e os gates automatizados estão verdes,
mas a v1 não deve ser declarada validada em produção ou fisicamente universal:
não houve provider live nem matriz correspondente de emulador, mouse, IME,
clipboard e terminais. O detalhe de cada limitação está em
[POC-RESULTS.md](../POC-RESULTS.md).
