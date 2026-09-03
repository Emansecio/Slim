# Caçada SES / CTX / PRV / INT — 2026-09-02

Somente leitura. Nenhum patch. Linhas lidas neste dia.

## Veredito

**SES** está limpo no que a auditoria de 30/08 chamava de hot path da fila: `enqueue_persisted` (reconstrução O(n) do prefixo a cada enqueue) só existe em testes; o append JSONL v2 já é incremental (`prepare_batch` + um `sync_data` por lote). O que ainda dói no loop persistido é outro: cada turno TUI/`--resume` chama `persist_manual_prefix` → `restore_records` + varredura de todos os IDs no repo, e `--resume` headless restaura transcript+checkpoint mas zera tools. `--session` v1 só grava no fim do loop.

**CTX** não bloqueia o turno no LLM no hard threshold (extract local) nem no soft (background). O custo que o usuário paga em *todo* turno é `select_compaction_history(...).is_ok()`: clona prefixo/kept e reestima tokens só para saber se dá para compactar. Task de background vazada foi em grande parte fechada; restam `?` depois do spawn.

**PRV** limpou o que a auditoria apontava como canônico+tools: CLI de produção não liga response cache (`with_shared_transport`), adapters built-in montam `tools` no `Value` de uma vez. O que resta no request é clonar o transcript na redação, serializar o body e o `from_http_request` re-parsear o JSON inteiro. SSE tem teto 64 MiB, cancel via `select!`, G298 coalescido no OpenCode Go.

**INT** é o buraco observável do contrato: `completed` nunca é podado; a 65ª `ask_question` no mesmo run (teto 128 turnos) devolve `RouteCapacity`. TUI resume **já** cria rota nova (N5 stale). Y/N (`Approve`/`Reject`) no worker ativo não responde pergunta — perguntas usam `AnswerQuestion`; `ApprovalRequired` não é emitido pelo runtime hoje.

---

## Tabela principal (hot path + prova)

| ID | Classe | Hot path | Evidência | Trigger | Impacto | Conserto mínimo | Explicitamente não fazer |
|---|---|---|---|---|---|---|---|
| **SES-01** | slow | sim | `manual_drive.rs:351-390` `preflight` chama `restore_records(repo.records())` e depois varre todos os records para HashSets, em *todo* `drive_manual_async` | TUI com sessão persistida ou `--resume`; cada prompt do usuário | O(n) reduce+clone do transcript durável por turno, além do clone do validador no append | Reusar `DurableState`/IDs já no validator do `JsonlRepo`; `preflight` só checa o spec contra o último seq e um set incremental | Segundo índice, cache de records, reescrever o driver |
| **SES-02** | correctness | sim | `headless.rs:452-453` descarta `_plan`; `783-787` zera tools se não há rota; `575-613` rejeita pending/claimed/suspended; `634-656` recusa tool-call/tool-role | `slim --resume PATH --prompt …` headless (sem TUI) | Transcript volta; tools não; fila in-flight não continua; usuário vê “cannot execute tools yet” | No headless, ou falhar cedo com essa mensagem *antes* do request, ou passar rota nula documentada; no TUI o plano já não é necessário para perguntar | Replay de tools, worker de fila, migrar v1 |
| **CTX-01** | slow | sim | `runtime/mod.rs:1158-1165` `has_compactable = select_compaction_history(...).is_ok()`; `compact.rs:508-521` clona `summarized` e `kept` e chama `estimate_provider_message_tokens` | Todo turno do agent loop com compactação ligada (default) | O(transcript) clone + `chars().count()` mesmo abaixo do soft; cresce com a sessão | Trocar o predicado por `has_compactable_history(&messages)` (já existe, `compact.rs:726`) | Estimador incremental, tokenizer, segunda policy |
| **CTX-02** | leak | sim (só no erro pós-spawn) | Spawn `runtime/mod.rs:1721-1738`; `?` sem cancel em `1877` (`ToolEvidenceReused`), `1813`/`1931` (`finish_background_if_ready`), `1463` (snapshot com pending do turno anterior). `PendingBackgroundCompaction` sem Drop | Soft threshold dispara background; depois um `push_runtime_event` falha ou `finish_background` erra | Task HTTP de compactação continua após o loop retornar; request extra / conexão pendurada | `cancel_pending_background` antes de todo `return Err` / `?` após o spawn; ou `Drop` que dá abort | Supervisor, JoinSet, canal extra |
| **PRV-01** | slow | sim | `runtime/mod.rs:3536-3566` `redact_messages` clona todas as mensagens; `provider.rs:3467-3468` `messages_body`→`to_string`; `624-650` `from_http_request` faz `serde_json::from_str` no body + `chars().count()` | Todo prepare de turno (e compactação) | 3–4 cópias do transcript por request (clone + Value + String + árvore parseada) | `from_http_body` já tem o `Value`: calcular components/fingerprints nele e serializar uma vez; redigir só campos novos do último turno | Response cache, hasher canônico, adapter paralelo |
| **INT-01** | bug | sim | `interaction.rs:16` `MAX=64`; `155` `completed: HashSet`; `237-238` soma pending+completed; `268` e `287` (Drop) inserem e nunca removem | 65ª `ask_question` no mesmo agent loop (`DEFAULT_MAX_TURNS=128`) | Tool falha com `RouteCapacity`; o modelo não consegue mais perguntar naquele run | Não contar `completed` (só `pending.len()`), ou `completed.clear()` após answer/drop | Rota persistida, replay de pergunta, fila de UI |

Falsificadores baratos (não rodados como suíte; citados se existem):

- SES-01: `session_durable_run.rs` / `drive_manual` — nenhum teste conta `restore_records` por turno.
- SES-02: `headless_resume.rs`; `format_run_stop_message` em `headless.rs:2704-2714`.
- CTX-01: testes de compactação não assertam que seleção *não* corre abaixo do limiar.
- CTX-02: `runtime/mod.rs:5588` `cancel_pending_background_harvests_finished_task` cobre cancel explícito, não o `?` de `ToolEvidenceReused`.
- PRV-01: `provider_http.rs` / prepare — não mede parse duplo.
- INT-01: `ask_question.rs` e `agent_loop.rs` cobrem 1 pergunta; `cancelling_while_ask_question_waits_closes_the_tool_and_rejects_a_late_answer` cobre cancel+resposta tardia (esse caminho está limpo).

---

## STALE / CONFIRMED (pistas 26–30/08 realmente relidas)

| Pista | Arquivo antigo | Veredito | Por quê |
|---|---|---|---|
| Fila `enqueue_persisted` O(n²) | `audit_runtime_session.md` #7 | **STALE como hot path** / código **CONFIRMED** | `from_repo` ainda reconstrói (`queue.rs:333-345`). Únicos callers: `tests/session_queue.rs`. Loop usa `drive_manual`, não a fila persistida. |
| `prepare_batch` clona validador | #8 | **CONFIRMED parcial** | Clone incremental, não revalida o prefixo (`repository.rs:516-537` testa isso). Custo O(estado) por append, não O(n²) de parse. |
| Parse JSONL inteiro + duplo parse | #21 | **CONFIRMED no open/resume** | `jsonl_repo.rs:292-316` `Value` depois `DurableRecord`. Resume: preflight parse + `open_no_repair_expected` parse de novo. Não é por turno. |
| `acquire_lock` Unix sem flock | #11 | **CONFIRMED Unix** / Windows **ok** | `event_log.rs:141-148`: Unix só `create+write`; Windows `FILE_SHARE_READ`. User nesta máquina: Windows. |
| TOCTOU `resolve_session_path` | #26 | **CONFIRMED, não hot path** | `exists` → `canonicalize` (`event_log.rs:221-232`). Open v2 pina o handle antes. |
| `claim_next` descarta id | #27 | **CONFIRMED código, não produção** | `queue.rs:189-194`. Produção não chama. |
| Background compaction leak | #12 | **CONFIRMED residual** | Muitos `cancel_pending_background` foram adicionados. Resta CTX-02. |
| `completed` never cleared | #6 | **CONFIRMED** | INT-01. |
| `estimate_provider_message_tokens` todo turno | `audit_tools_context_provider.md` #23 | **CONFIRMED via seleção** | Preflight de tokens agora usa `estimate_unprepared_request_chars`. O clone caro é `select_compaction_history().is_ok()` (CTX-01). |
| `canonical_messages` por request | #24 | **STALE no CLI** | `prepare_messages_with_tools` só hasheia se `self.cache.is_some()` (`provider.rs:1497`). Headless: `with_shared_transport`, cache `None`. Função vive para testes/`cache_key_with_tools`. |
| Serializar → reparse para injetar tools | #25 | **STALE nos adapters built-in** | OpenAI/Anthropic/Codex/OpenCode Go montam tools no `messages_body`. Residual é PRV-01 (`from_http_request`). |
| `cache_namespace` serializa body vazio | #26 provider | **STALE no prepare de produção** | Só entra via `cache_key_with_tools`. |
| G298 usage OpenCode Go | (não reabrir) | **STALE / corrigido** | `provider.rs:1756-1816` coalesces terminal usage. Sem evidência nova. |
| N5 `ask_question` no resume TUI | `PROXIMAS-ETAPAS-AGENTE.md` | **STALE** | `tui.rs:2956-2966` sempre faz `interaction_route()` e passa a rota no braço durável. Headless resume continua sem rota (intencional). |
| Hard threshold = LLM em foreground | hipótese CTX | **STALE** | Hard sem prepared: `local_emergency_summary` (`runtime/mod.rs:1327-1338`). LLM foreground: só manual ou overflow retry. Soft: background. |
| `bounded_transcript` a cada busca | hipótese CTX | **CONFIRMED só no prompt de resumo** | Binary search `compact.rs:569-584` quando o transcript não cabe. Não é todo turno. Nota, não tabela. |
| `take_prepared` re-hash | hipótese CTX | **CONFIRMED, necessário** | SHA-256 do prefixo `compact.rs:240-241`. Uma vez ao aplicar prepared. Nota. |
| Y/N no run ativo descarta pergunta | hipótese INT | **STALE para `ask_question`** | Ativo: `AnswerQuestion` → `answer_active_question` (`tui.rs:1507-1513`). `Approve`/`Reject` → `reject_unbound_interaction` (`1514-1518`). Reducer de pergunta emite `AnswerQuestion` (`reducer.rs:748`). `ApprovalRequired` não é emitido pelo runtime. |

---

## Ordem de correção mínima (não implementar agora)

1. **INT-01** — parar de somar `completed` na capacidade (ou limpar no `answer`/`Drop`). Uma função, um comportamento quebrado (65ª pergunta).
2. **CTX-01** — `has_compactable_history` no predicado do turno; deixar `select_compaction_history` só quando for compactar de verdade.
3. **SES-01** — `persist_manual` `preflight` contra estado incremental do repo, não `restore_records` completo.
4. **PRV-01** — components a partir do `Value` já montado; uma serialização.

Um item por sistema. CTX-02 e SES-02 ficam atrás: leak residual só em erro; headless-sem-tools é contrato “yet”, não regressão silenciosa.

---

## Notas, não caçar agora

- `enqueue_persisted` / `from_repo` em testes; fila durável não está no agent loop.
- `--session` v1 (`SessionWriter`): um `append_batch` de *todos* os eventos (incluindo deltas) só depois do loop (`headless.rs:1315-1321`); crash no meio perde o arquivo. Caminho legado, paralelo ao JSONL v2 da TUI.
- Fsync por lote no v2: duas vezes por turno persistido (prefixo + sufixo). Custo de durabilidade, não bug.
- `prepare_batch` clone do `DurableState` (entries com conteúdo): real, mas já incremental; consertar SES-01 rende mais.
- `bounded_transcript` O(log n · n) chars no summary request; some no wait do LLM.
- `estimate_provider_message_tokens` usa `AdaptiveTokenEstimator::default()` (`compact.rs:449`), não o estimador calibrado do runtime — possível compactar cedo/tarde; não medido.
- `apply_compaction_selection` valida tamanho com `CompactionPolicy::default()` (`compact.rs:601`).
- Resume TUI `ResumePrevious` substitui `CompactionHandle` por um novo (`tui.rs:1584-1585`); `previous_summary` do handle some; o checkpoint no JSONL ainda entra no histórico via `durable_provider_history`.
- Pergunta in-flight não é persistida (`QuestionRequired { persisted: false }`). Resume não restaura pergunta aberta — esperado.
- Cancel durante pergunta + resposta tardia: coberto e correto (`StaleRequest`).
- Headless oneshot sem rota: `ask_question unavailable` imediato — contrato.
- Catálogos Codex/OpenCode Go/Clinepass/Command Code: fora do request path desta caçada.

---

## Não objetivos (reafirmados)

SLOW-01..12 (tools, mixed-batch, warm LSP, ToolOutput try_send, stamp de list, cmd.exe). Executor novo, async-fs geral, segundo cache, host persistente de PowerShell, Rayon, MCP/child agents/inspector, campanha de economia de tokens. Completar cobertura histórica. Completar auditorias antigas linha a linha. Corrigir os achados — o entregável é este mapa, não o patch.

Critério de concluído: quatro sistemas lidos no hot path; cada linha da tabela tem trigger e conserto mínimo; nenhum patch aplicado.
