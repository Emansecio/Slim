# AUDIT-SLIM-AGENT-UI-TUI-CLI

Auditoria somente leitura do Slim Agent (core, TUI, CLI e contratos entre camadas).  
Data da sessão: 2026-08-25. Workspace: `D:\Slim`.  
Complexity: MEDIUM (score 5) — >10k LOC, monorepo de 3 crates, auth/sessão, providers HTTP.  
Mode: audit-only. Modelo: Cursor Grok 4.6.

Nenhuma alteração de código, teste, snapshot, lockfile ou documentação existente. Escritas restritas a este relatório e a `analysis_outputs/slim-agent-ui-tui-cli/`.

---

## 1. Resumo executivo e estado

**Estado: YELLOW**

Quatro bugs P2 confirmados no caminho real do usuário; zero P1; nenhum gargalo de desempenho confirmado (bench long_session desta sessão: p95 input→frame 1,108 ms ≤ 16 ms). O agent loop em `slim-core` está coerente e bem testado. Os defeitos estão nas fronteiras CLI/TUI: stdin headless, precedência de config, Codex headless sem `account_id`, e setas da pergunta estruturada capturadas pelo scrollback.

Não é GREEN: há P2 confirmados com reprodução nesta sessão.  
Não é RED: o fluxo principal (TUI com provider OAuth/API key, headless com `--prompt`, loop, tools, cancelamento) não está quebrado de ponta a ponta.

| Confiança | P1 | P2 | P3 |
|-----------|----|----|----|
| confirmed | 0 | 4 | 0 |
| partial / uncertain | — | — | ver §8 |

---

## 2. Escopo revisado e exclusões

**Revisado (código lido e/ou callers rastreados nesta sessão):**

- `crates/slim-cli/src/{main,lib,cli,headless,tui,config,auth,exit_codes}.rs` e OAuth/Codex account id
- `crates/slim-tui/src/{runtime,reducer,app,block,render,markdown,terminal,layout}.rs`
- `crates/slim-core/src/{runtime/mod,events,interaction,model,provider,provider/codex,tools/shell}.rs`
- Contratos TUI: `terminal_action` → reducer → `ask_question`
- Goldens TestBackend listados em §5
- Reproduções CLI no binário **debug** gerado nesta sessão

**Exclusões explícitas:**

- Providers comerciais / rede externa (proibido pelo pedido)
- `poc/`, `release/` builder, `bench/token-economy/`
- MCP transporte externo, child agents provider-backed, Todo/Plan/Goal como tools TUI (lacunas de produto documentadas, não caçadas como bugs de regressão)
- ConPTY físico (`tui_pty.rs` `#[ignore]`)
- Binário `C:\Users\User\bin\Slim.exe` como evidência visual (SHA distinto do debug atual; `target\release\slim.exe` ausente)
- Números de testes, hashes e latências de DESIGN/README/PLANO/auditorias antigas — não reutilizados

**Coverage ledger:**

| Área | Cobertura |
|------|-----------|
| slim-cli dispatch / headless / config / Codex | read + reprodução binária |
| slim-tui input frontier / reducer / restore / goldens | read + testes focados |
| slim-core agent loop / cancel / ask_question / resume types | read; testes da suíte workspace |
| slim-core session v2 (jsonl, reducer, queue, tool_phases) | searched + leitura de âncoras de resume; não linha-a-linha de todos os arquivos novos |
| slim-tui inspectors / image / fullscreen extras | searched-only (inspector stub) |
| OAuth PKCE detalhe criptográfico | searched; não auditoria cripto |

---

## 3. Estado inicial do worktree e ambiente

**WIP preservado.** `git status --short` no início: dezenas de arquivos modificados em core/cli/tui/docs + untracked (session v2, ask_question, opencode-go, goldens, plans). Nenhum `git reset` / `checkout` / `clean` / `restore` / `stash` / `pull` / `fetch`.

HEAD recente: `25146cb [verified] W8: implement Grok-style TUI footer` (WIP local não commitado por cima).

| Item | Valor verificado nesta sessão |
|------|-------------------------------|
| rustc efetivo | 1.97.1 (8bab26f4f 2026-07-14) via Scoop/MSVC |
| cargo | 1.97.1 (c980f4866 2026-06-30) |
| `RUSTC` herdado | `C:\Users\User\.cargo\bin\rustc.exe` **inexistente**; sessão usou toolchain Scoop persist |
| `Get-Command slim` | `C:\Users\User\bin\Slim.exe` |
| PATH Slim.exe | 2026-08-25 17:51:17, 9 799 168 B, SHA-256 `8AB5BF424E47D6A5B0D4562C8A62EEB1DE209D96AD6858E2BF04F9ADF4AC42F6` |
| `target\release\slim.exe` | **ausente** — PATH copy **não** prova o WIP |
| `target\debug\slim.exe` (cargo run desta sessão) | 2026-08-25 18:30:58, 16 720 384 B, SHA-256 `6AE3D7C3C3C653B261F678B22E36E887F15E64A75AF72E109EE70D1D0E416F8C` |
| Manifests | workspace `slim-core`, `slim-tui`, `slim-cli`; bin `slim` em `crates/slim-cli/src/main.rs` |
| LOC crates | 171 arquivos `.rs`, 59 552 linhas |

Reproduções CLI usaram o debug acima, não o PATH.

Evidência: `analysis_outputs/slim-agent-ui-tui-cli/baseline.txt`.

---

## 4. Mapa arquitetural e fluxo end-to-end

```
argv ──► slim-cli/src/main.rs
         ├─ sem --headless/--help/--version ──► run_tui (tui.rs:78)
         │     prepare_tui → spawn_tui_session (control 256 + data 1024)
         │     run_worker (tui.rs:839)
         │       UiCommand::SendPrompt → start_active_run (tui.rs:1965)
         │         interaction_route se NÃO durable (tui.rs:1997-2001)
         │         execute_provider_turn_async / resume blocking
         │         SessionEvent ──project_core_event──► UiEvent lanes
         │     slim_tui::run_app (runtime.rs:32)
         │       drain_runtime_events → reduce → render_frame
         │       terminal_action: overlays vs scroll vs Key
         │       shutdown() + TerminalGuard::Drop
         └─ --headless | --help | --version ──► run_cli (cli.rs:107)
               parse_cli_args → provider | fake headless
               execute_provider_turn_async
                 Runtime::run_agent_loop_with_messages (runtime/mod.rs:615)
                   compact → ContextSnapshot → provider SSE
                   ProviderStreamNormalizer → ToolStarted/Output/Finished
                   ask_question se interaction_route (somente TUI comum)
                   LoopGuard / max_turns / cancel
               stdout = text|jsonl ; stderr = erros
```

**Eventos core** (`events.rs`): deltas de texto/reasoning, thinking start/end, tools com `batch_id`/`call_id`, usage, `QuestionRequired` / `InputRequired` / `ApprovalRequired`, `InteractionAcknowledged`, erros terminais.

**Filas:** core `SessionEventSender` bounded + `send_interruptible` (espera 1 ms se cheio). TUI: control 256 + data 1024, lossless, coalescer de deltas no drain.

**Cancelamento:** `CancellationToken` no loop, provider `select`, tools e `ask_question`. TUI `CancelRun` cancela o token do `ActiveRun`.

**Resume:** preflight fail-closed; TUI durable **não** anuncia `ask_question` (por contrato). Restore não reexecuta efeitos no harness v2 (não revalidado linha-a-linha de todos os módulos novos).

Wiring comprovado: `run_tui` e `run_cli` compartilham `parse_cli_args` e `execute_provider_turn_async`. Presença de `Config::resolve` **não** é integração — callers reais usam `load_layered()`.

---

## 5. Matriz de cenários verificados

| Cenário | Método | Resultado |
|---------|--------|-----------|
| Dispatch TUI vs headless | leitura `main.rs` + `cli_contract` binary test (suíte) | TUI default; `--headless` CLI |
| Stdin pipe headless | `cargo run -- --headless --read-only` com pipe | **FAIL produto** exit 11 — A2 |
| `--prompt` headless fake | `cargo run -- --headless --read-only --prompt …` | success exit 0 |
| Codex headless com token dummy | `cargo run -- --headless --provider openai-codex --prompt hi` | **FAIL produto** exit 21 — A4 |
| Config project vs global | leitura + `merge_fills_gaps_*` | **FAIL contrato** — A3 |
| Setas pergunta ao vivo | `terminal_action` + reducer + goldens reducer-only | **FAIL produto** — A1 |
| Opções 1–9 / Enter reducer | `interaction_roundtrip` 5/5 | PASS (não prova o frontier) |
| Overlay setas (login) | `--lib overlay_arrow` | PASS |
| Welcome 120×30 / 60×16 / 32×10 | `welcome_golden` 4/4 | PASS TestBackend |
| Matriz tamanhos + emergência | `golden_matrix` 4/4 | PASS TestBackend |
| Markdown / ANSI untrusted | `markdown_golden` 18/18 | PASS |
| Thinking expand | `thinking_expansion_golden` 7/7 | PASS |
| Tools / cancel / group | `tool_details` + `multi_tool` + motion | PASS |
| Scroll pin/live-edge | `scroll_golden` 11/11 | PASS |
| NO_COLOR / reduced motion | welcome 32×10 + motion caret | PASS TestBackend |
| Terminal restore | leitura `terminal.rs` Drop + `run_app` always shutdown | código completo; panic path sem teste de unwind |
| ConPTY 120/80/60/32 | `tui_pty.rs` | **não executado** (`#[ignore]`) |
| JSONL stdout vs stderr | leitura `main.rs` print/eprint; cache `eprintln` | stdout JSON; cache em stderr |
| Agent loop cancel/tools | leitura + suíte workspace (em andamento ao redigir; ver §11) | sem bug confirmado no core |

Detalhe visual: `analysis_outputs/slim-agent-ui-tui-cli/visual-coverage.txt`.

---

## 6. Achados confirmados P1/P2/P3

### P1

Nenhum.

### P2

### [A1] Setas da pergunta estruturada viram scroll; `Outro...` é inalcançável no terminal real

- Severidade: P2
- Confiança: confirmed
- Área: TUI
- Evidência: `crates/slim-tui/src/runtime.rs:546-574` (`terminal_action`); `crates/slim-tui/src/reducer.rs:408-442`; `crates/slim-tui/src/block.rs:262-297`; DESIGN §1.1 (“navegação por setas/números” + `Outro...`); teste `runtime::tests::overlay_arrow_is_routed_to_reducer_instead_of_scrollback` (PASS, afirma Down→Scroll sem overlay); `interaction_roundtrip` usa `Action::Key` e portanto não pega o bug
- Alcance: TUI fullscreen, run Auto/ReadOnly não durable, tool `ask_question` com opções (rota ligada em `tui.rs:1997-2024`)
- Reprodução: 1) run TUI comum até `QuestionRequired` com ≥1 opção; 2) pressionar ↑/↓ — o transcript rola; 3) tentar chegar em `Outro...` — só Up/Down alteram `selected_question_option` até `options.len()`; dígitos 1–9 recusam esse índice (`select_question_option`)
- Esperado: ↑/↓ movem `›` entre opções e `Outro...`; Enter no `Outro...` abre resposta livre (reducer já implementa)
- Atual: `terminal_action` não consulta `pending_interaction()`; setas viram `Action::Scroll`. `Outro...` não tem atalho numérico
- Impacto: a alternativa livre anunciada na spec/tracker fica inacessível no teclado real; setas — affordance primária — fazem o contrário do indicador `›`
- Causa provável: G184 fechou overlays (login/model/effort/slash) e o teste cristalizou “sem overlay ⇒ scroll”. `ask_question` chegou depois como bloco de transcript, não overlay
- Correção recomendada: em `terminal_action`, se `pending_interaction()` for `Question` sem `custom_question_answer`, devolver `Action::Key` para Up/Down (e provavelmente não Page/Home/End). Espelhar o gate de overlay
- Regressão sugerida: teste através de `terminal_action` com `QuestionRequired` instalado: Down → `Action::Key`, não `Scroll`; após N Downs, Enter ativa custom. Não usar só `reduce(Action::Key)`
- Limitações: sem captura ConPTY nesta sessão; o caminho live está determinado pelo frontier puro

### [A2] Binário headless nunca lê stdin redirecionado

- Severidade: P2
- Confiança: confirmed
- Área: CLI
- Evidência: `crates/slim-cli/src/main.rs:15-21, 58-74`; reprodução nesta sessão com `target\debug\slim.exe` (SHA `6AE3D7C3…`); `analysis_outputs/slim-agent-ui-tui-cli/repro-stdin-headless.txt`
- Alcance: qualquer `slim --headless …` sem `--prompt` nem posicional, com pipe/redirect
- Reprodução:
  ```
  "hello from stdin" | cargo run -q -- --headless --read-only
  ```
  EXIT=11, saída `input_required`. Controle: `--prompt "hello from stdin"` → EXIT=0 `success`
- Esperado: stdin vira prompt quando não há `--prompt`/posicional (contrato de `run_cli` e teste `stdin_is_used_when_prompt_argument_is_absent`)
- Atual: `--headless` cai no `_ => return false` de `known_options_without_prompt`; `needs_stdin` fica falso; `run_cli` recebe `""`
- Impacto: scripts `echo … | slim --headless` quebram; o teste de contrato chama `run_cli` **sem** `--headless` e não vê o bug do binário
- Causa provável: detector de stdin não trata `--headless`/`--tui` como flags neutras; `--resume`/`--recover` também estão ausentes da lista
- Correção recomendada: aceitar `--headless` e `--tui` no match (no-op) e alinhar flags com valor (`--resume`, `--recover`) a `has_positional_prompt`; teste **binário** `Command::new(CARGO_BIN_EXE)` com stdin
- Regressão sugerida: `cli_contract` com `CARGO_BIN_EXE_slim`, args `--headless --read-only`, stdin `"hello from stdin"`, expect Success
- Limitações: nenhuma para o binário debug atual

### [A3] `slim.toml` de projeto perde para o config global

- Severidade: P2
- Confiança: confirmed
- Área: CLI / Integração
- Evidência: `crates/slim-cli/src/config.rs:31` (“project wins over global”); `96-121` (`load_layered` global primeiro + `merge_layer` first-wins); `238-250` (`Config::resolve` project>global); callers `cli.rs:223-257` e `tui.rs:145-154` usam **só** `load_layered`; `Config::resolve` sem callers de produto
- Alcance: TUI e headless após `save_global_model` (`%APPDATA%\slim\slim.toml`) + `./slim.toml` no repo
- Reprodução: código determinístico — global carrega primeiro e ocupa `model`/`endpoint`/`effort`; projeto só preenche `None`. Teste `merge_fills_gaps_without_overriding_earlier_layers` PASS e documenta first-wins
- Esperado: CLI → env → **projeto** → global (doc, `Config::resolve`, tracker W4, `headless_contract`)
- Atual: global vence o projeto nas chaves já preenchidas. `save_global_model` da TUI torna isso o caso comum
- Impacto: modelo/endpoint/effort por repositório são ignorados em silêncio
- Causa provável: ordem de load + merge “preenche lacunas” invertida em relação ao comentário da struct
- Correção recomendada: carregar projeto **depois** com overwrite, **ou** inverter a ordem de `paths` e manter first-wins; deletar ou passar a usar `Config::resolve`; um teste de `load_layered` com dois arquivos temporários
- Regressão sugerida: temp global `model=global` + temp project `model=project` → layered.model == project (via helper injetável de paths)
- Limitações: não escrevemos no `%APPDATA%` do auditor; o merge não depende disso

### [A4] Headless `--provider openai-codex` não consegue autenticar

- Severidade: P2
- Confiança: confirmed
- Área: CLI / Integração
- Evidência: `cli.rs:250-261` `account_id: None`; `headless.rs:698-705` exige `account_id`; `provider/codex.rs:19-26`; TUI `tui.rs:333-338` preenche via OAuth; `auth.rs:184-186` ignora blob `oauth`; reprodução offline EXIT=21 `provider error: Codex OAuth account id is required` (`repro-codex-headless.txt`)
- Alcance: `slim --headless --provider openai-codex|codex` com `CODEX_ACCESS_TOKEN` / `SLIM_API_KEY` / api_key em auth.json. Help lista o provider
- Reprodução:
  ```
  $env:CODEX_ACCESS_TOKEN='dummy-offline-token'
  cargo run -q -- --headless --provider openai-codex --prompt hi
  ```
  Falha no construtor, **antes** de HTTP
- Esperado: mesmo credencial OAuth da TUI funciona em headless, ou recusa Auth (20) com mensagem “use /login na TUI / extraia account id”
- Atual: com token, chega a InvalidResponse mapeado para Provider (21). Após login TUI só-oauth, `resolve_api_key` pode devolver None → Auth 20. `codex_account_id` existe e não é chamado neste caminho
- Impacto: Codex headless anunciado é inoperante; TUI OAuth continua o único caminho
- Causa provável: CLI monta `ProviderRequest` sem o campo que o adapter OAuth exige; auth file não lê `oauth`
- Correção recomendada: reutilizar `oauth::codex_account_id` no token JWT **ou** ler `OAuthCredential.account_id` do store; falhar em Auth 20 se ausente, não no meio do turn
- Regressão sugerida: `run_cli(["--headless","--provider","openai-codex","--prompt","hi"], "")` com token dummy sem account id → Auth, não Provider; com JWT fixture contendo `chatgpt_account_id` → passa da guarda `account_id` (HTTP pode permanecer mockado)
- Limitações: token dummy; nenhum request de rede

### P3

Nenhum confirmado.

---

## 7. Gargalos medidos

**Nenhum gargalo confirmado nesta auditoria.**

Medição desta sessão (`cargo bench -p slim-tui --bench long_session`, exit 0):

| Cenário | Amostra | Resultado |
|---------|---------|-----------|
| Corpus virtualizado 3 200 blocos | 4 968 694 bytes, 40 rows | `input_to_frame_p95_ms=1.108` |
| Scroll locate no mesmo corpus | p95 | `scroll_locate_p95_ms=0.920` |
| WrapCache de corpos | 49 hits / 1 miss | hit_rate=98.00% |
| Gate do próprio bench | p95 input→frame ≤ 16 ms | **PASS** (1.108 ≤ 16) |

Candidatos **não** elevados a finding (sem prova de impacto):

| Candidato | Âncora | Por que não confirmado |
|-----------|--------|------------------------|
| `thread::sleep(5ms)` no poll do shell | `tools/shell.rs:96` | cancel/timeout testados; sem p99 sob carga concorrente |
| Tools síncronas no worker Tokio | `runtime/mod.rs` `execute_tool_call` | sem profiler de starvation |
| `send_interruptible` wait 1 ms | `model.rs:78-108` | canal TUI 1024; sem saturamento medido |
| Parse Markdown dedicado ausente | `markdown.rs:170-185` | WrapCache de corpos cobre o bench acima |
| Cache HTTP ligado em headless (`with_cache`) | `headless.rs:682+` | docs antigos dizem inativo; não é prova de lentidão |

---

## 8. Itens parciais/incertos e evidência que falta

| ID | Confiança | Tema | Evidência até aqui | Falta |
|----|-----------|------|--------------------|-------|
| P1 | partial | IO de tool bloqueia runtime async | `execute_tool_call` síncrono + `sleep` 5 ms | medição de starvation / p99 de cancel |
| P2 | partial | Backpressure da fila core | `send_interruptible` loop 1 ms | teste capacity=1 vs TUI lenta |
| P3 | partial | Cache replay sem Usage | cache strip Usage; headless usa `with_cache` | hit real no 2º turn idêntico e `AgentLoopResult.usage` |
| P4 | partial | Defaults Anthropic TUI vs CLI | TUI `claude-sonnet-4-6` (`tui.rs:281`) vs CLI `claude-3-5-sonnet-latest` (`cli.rs:219`) | se a diferença muda qualidade/custo o bastante para ser defeito |
| P5 | uncertain | `ContentBlockStop` índice desconhecido ignorado | `runtime/mod.rs` (normalizer Anthropic) | fixture de stop órfão |
| P6 | uncertain | `AppHandle` não amarra `session_id` ao push | `model.rs` `push_event` só seq | caller cruzando sessões |
| P7 | uncertain | ActivityRail some com pergunta pendente pós-`RunStopped` | `interaction_roundtrip` replay | se o live `ask_question` emite `RunStopped` antes da resposta (o loop ainda está await — improvável) |
| P8 | uncertain | `InvalidResponse` → exit 30 na TUI vs 21 no CLI | `tui_provider_error` vs `provider_failure` | o spawn TUI usa InvalidResponse para cwd/wake/thread (interno). **Não é o mesmo evento de A4.** Tratamento: ver §9 |

---

## 9. Hipóteses investigadas e descartadas

| Hipótese | Por que não é finding |
|----------|------------------------|
| Eventos perdidos / seq wrap no loop | `checked_next_seq`; `push_event` monotônico |
| Tools reexecutadas no resume | `ReplayPlan` só SafePending; TUI durable não liga `ask_question` (contrato) |
| Cancel deixa tool sem Finished | `execute_tool_call` força `success=false` se cancelado |
| `ask_question` aceita resposta stale | Drop de `PendingQuestion` + `StaleRequest`; testes core |
| Loop infinito de retries | `max_turns` + `LoopGuard` one-strike; SSE loop é stream, não retry unbounded |
| Mutex do interaction across await | lock dropado antes de `receive().await` |
| JSONL contaminado por ANSI da TUI | headless não projeta TUI; `print` stdout / `eprint` stderr |
| Cache HTTP “inativo” no caminho normal | **docs antigos**; código headless/TUI usa `HttpProviderClient::with_cache` — desvio documental, não bug de fluidez |
| TUI `InvalidResponse`→30 vs CLI→21 como o mesmo bug de A4 | `spawn_tui_session` só devolve InvalidResponse para falhas internas (cwd, wake, thread). Mapear para Internal é coerente. Não confirmed |
| Restore do terminal ausente | `run_app` sempre `shutdown`; `TerminalGuard::Drop` restaura alt-screen/raw/CP |
| Injeção ANSI de conteúdo de modelo | `sanitize_terminal_text` + goldens markdown/tool |
| Goldens falhando = bug de layout | 94 testes golden/visual focados PASS nesta sessão |

---

## 10. Pontos que já estão corretos e não devem ser “consertados”

- Agent loop: teto de turns/tools, anti-loop, truncamento sem executar tools, seq checked, cancel cooperativo
- `ask_question` **não** anunciado em headless, Plan e resume durável — contrato DESIGN/tracker
- Lanes TUI bounded lossless + coalescer; overlays (login/model/effort) **já** capturam setas (G184)
- Dígitos 1–9 e Enter nas opções (exceto Outro) funcionam se as teclas chegarem como `Action::Key`
- Redaction de API key; auth.json DACL; fail-closed de schema/symlink
- `--session` / `--resume` / `--recover` mutuamente exclusivos
- Sanitizer de controles de terminal no markdown e labels
- WrapCache de corpos/alturas por identity+generation+width (não “rewrapar tudo” em blocos estáveis)
- Fake headless Plan/empty sem rede
- Help/version bypass fullscreen
- G248: modelo incompatível com o provider é descartado em favor do default

---

## 11. Verificações executadas, comandos e resultados

Exit codes confirmados com `$LASTEXITCODE` sem pipeline filtrado.

| Comando | Exit | Notas |
|---------|------|--------|
| `cargo fmt --all -- --check` | 0 | |
| `cargo check --workspace` | 0 | Finished sem warnings do compilador no output |
| `git diff --check` | 0 | avisos CRLF do Git, sem whitespace error |
| `cargo test -p slim-tui --lib overlay_arrow` | 0 | 1 passed |
| `cargo test -p slim-tui --test interaction_roundtrip` | 0 | 5 passed |
| `cargo test -p slim-cli --lib merge_fills` | 0 | 1 passed |
| `cargo test -p slim-cli --test cli_contract stdin_is_used…` | 0 | 1 passed (não pega A2) |
| Goldens TUI (matrix, welcome, layout, markdown, motion, multi_tool, scroll, thinking, tool_details) | 0 | 4+4+20+18+20+4+11+7+6 = **94 passed** |
| Pipe stdin headless (`cargo run`) | **11** | reproduz A2 |
| `--prompt` headless fake | 0 | controle A2 |
| Codex headless dummy token | **21** | reproduz A4 |
| `cargo test --workspace -j 1 --no-fail-fast` | **0** | 75 suítes; **713 passed / 0 failed / 1 ignored**; 0 linhas `warning:` de compilador no log |
| `cargo clippy --workspace --all-targets -- -W clippy::all` | **0** | lints de estilo pré-existentes (não `-D`); `slim-cli` lib 65 avisos, vários testes core 1 aviso cada; não tratados como bugs de produto |
| `cargo bench -p slim-tui --bench long_session` | **0** | p95 input→frame 1,108 ms; scroll 0,920 ms; cache corpos 98% |
| `.\refresh-slim.ps1` | N/A | auditoria sem mudança de código |

### `cargo test --workspace -j 1 --no-fail-fast`

Contagem **desta sessão** (regex nas linhas `test result:` do log, incluindo Doc-tests 0/0/0): **75 suítes, 713 passed, 0 failed, 1 ignored**. Ignored: `deployed_binary_offline_streaming_matrix` (ConPTY). `WORKSPACE_TEST_EXIT=0`. Evidência: `analysis_outputs/slim-agent-ui-tui-cli/workspace-test-count.txt`.

A coincidência com números em DESIGN/README **não** foi cópia: o total foi somado do log desta execução.

---

## 12. Risco residual e limitações de PTY/ConPTY/ambiente

- **ConPTY físico não observado.** `tui_pty.rs` permanece `#[ignore]`. Matriz 120×30 / 80×24 / 60×16 / 32×10 com streaming+tools+question no Windows Terminal **não** tem screenshot desta sessão. Goldens TestBackend cobrem idle/welcome e vários estados sintéticos, não o host conhost.
- **PATH `Slim.exe` desatualizado em relação ao WIP** (SHA e tamanho diferentes do debug). Qualquer validação visual com `slim` do PATH seria incidente I1 outra vez.
- **`target\release\slim.exe` ausente.**
- Session v2 / observability / capability bridge: searched + âncoras; não leitura integral de todos os untracked.
- Clippy com `-W clippy::all` produziu avisos de estilo pré-existentes (`slim-cli` lib 65, sobretudo `let _ = sink.send`); exit 0, sem `-D warnings`. Não são bugs de runtime.
- Sem provider live: Codex/OpenAI/Anthropic reais não exercitados (proibido e desnecessário para A4).
- Panic unwind de `run_loop` após `start()`: Drop está no código, sem teste que force panic.

---

## 13. Top 3 próximas ações

Somente o que esta auditoria sustenta:

1. **Corrigir `terminal_action` para perguntas estruturadas (A1)** — uma condição no frontier; desbloqueia `Outro...` e alinha spec, reducer e teclado. Teste no frontier, não só no reducer.
2. **Tratar `--headless`/`--tui` como flags neutras no detector de stdin (A2)** e acrescentar teste binário com pipe. Custo mínimo, quebra scripting hoje.
3. **Um helper de precedência e um de Codex account id (A3+A4)** — `load_layered` deve implementar o contrato que já está documentado/testado em `Config::resolve`; headless Codex deve ler JWT/`oauth` store ou falhar em Auth 20 com mensagem explícita.

Não implementar nesta auditoria.

---

## 14. Checklist do RULES.md

## ✅ Verificação de Entrega

- [x] Rodei o que afirmo ter rodado (comando + saída na resposta e em `analysis_outputs/slim-agent-ui-tui-cli/`)
- [x] Números citados contados nesta sessão (R2) — LOC 59 552 / 171 rs; goldens 94; workspace 713/75 do log desta execução; bench p95 1,108 ms; SHA dos dois binários
- [x] Caminho:linha citado foi lido nesta sessão (R3)
- [x] cargo test --workspace verde (0 failed, 0 compiler warnings no log; 1 ignored ConPTY) — 713 passed / 75 suítes, contados nesta sessão
- [ ] .\refresh-slim.ps1 executado, imprimiu OK: — **N/A — nenhuma mudança de código** (pedido da auditoria)
- [x] Docs de status/números atualizados e grep dos antigos vazio — **N/A — auditoria proibiu alterar documentação existente**; este relatório é arquivo novo em `analysis_outputs/`
- [x] Incertezas e limitações declaradas explicitamente (R4) — ConPTY, clippy, bench, PATH binário, suíte workspace

Deploy: **N/A — nenhuma mudança de código.**
