# Caçada GOV / PRC / LSP / TUI / OAU — rendimento médio

Data: 2026-09-02  
Repo: Slim (código atual, Windows). Somente leitura. Nenhum patch.

Rendimento: 4 achados na tabela principal. GOV e o resto quente de PRC/LSP foram para notas.

## Veredito por sistema

**GOV — limpo (residual sombra).** `CausalGovernor` só consome receipts. Não há I/O de disco, não há `refresh_observed`, não clona o `BTreeMap` observed, não invalida o evidence cache das tools (SLOW-07/12 intactos: invalidação por path+stamp nas tools). Observações são eventos sombra; a TUI mapeia Causal* para `None`. O cap 256 e o `HashMap` evidence ilimitado continuam, mas não mudam o loop que o usuário espera.

**PRC — residual, não quente.** Shell cancela por `is_cancelled()` a cada 5 ms e `taskkill /T /F` (Windows) / `kill -- -pid` (Unix, process group). `taskkill` no core passa por `ExecutableResolver` (CWD hijack STALE). O hang se o utilitário não resolver é raro. `cancelled().await` ainda não chama `enable()`; a janela é intra-poll. Progresso de shell só emite a 1 Hz.

**LSP — residual.** Warm + notify com texto do write + early-return se o doc está fechado permanecem. O furo que sobrou no caminho que o usuário sente: **patch** notifica com `text: None` e, se o documento já está aberto, relê via `read_capped`. Header byte-a-byte, `document_sync` durante `notify`, Lease Drop fora de runtime, `max_servers` contando idle e `taskkill` síncrono no Drop do LSP são reais e vão a notas.

**TUI — residual no frame; leak no AppHandle.** WrapCache (4096 bodies, 32 MiB) cobre assistant congelado. Streaming assistant, thinking expandido e tool output expandido rewrapam todo frame — soak antigo já mediu p95 baixo; não reclassificar “TUI lenta”. O buraco observável é `AppHandle.events`: clone de todo evento, sem teto, `drain_events` só no headless depois do run.

**OAU — residual no expiry.** TUI chama `fresh_credential` antes de cada prompt. Headless/CLI carrega o access token uma vez e nunca refresca. Refresh bloqueante segura o lock exclusivo do store durante o HTTP. Falha de persistência vira `Notification` (não some). 401 no meio do turno não é recuperado. Não falsificável offline contra provider comercial; prova em código + `oauth_contract`.

## Tabela principal

| ID | Classe | Hot path | Prevalência | Evidência | Trigger | Impacto | Conserto mínimo | Explicitamente não fazer |
|---|---|---|---|---|---|---|---|---|
| TUI-01 | leak | sim (loop, não o frame) | sessão longa | `crates/slim-core/src/model.rs:391-400,436-438`; `runtime/mod.rs:973,1071`; `headless.rs:1315` | cada `push_event` / `push_transient_event` (deltas, tools, causal sombra) | `Vec` cresce sem teto na TUI; headless só drena no fim. Pico ∝ eventos × output. | Buffer da janela do turno (o loop já usa `event_start`); persistir o resto no writer que já existe. `drain` por turno, não no shutdown. | Não tocar na fila TUI 1024 / `try_send` de ToolOutput (SLOW-09). Não cache/overlay novo. |
| OAU-01 | correctness | sim (TUI OAuth / headless OAuth) | login/expiry | `oauth/mod.rs:157-165`; `tui.rs:2164-2194`; `cli.rs:278-290`; headless sem `fresh_credential` | token com remaining ∈ (30 s, 10 min): TUI devolve o access velho e só refresca em background no disco; headless nunca refresca | turno longo 401; próximo prompt TUI relê o store, o atual não. Headless OAuth de sessão longa falha até o processo morrer. | Headless: `fresh_credential` uma vez no start (mesmo serviço da TUI). TUI: após background refresh, não deixar o `ProviderRequest.api_key` do run preso ao access pré-refresh — ou bloquear se remaining < duração típica do turno. | Não chamar provider comercial. Não redesenhar OAuth. G295–G297 ficam. |
| OAU-02 | wait | sim (prompt TUI com expiry ≤ 30 s) | login/expiry | `oauth/mod.rs:192-216` (`lock_exclusive` depois `anthropic::refresh` / `codex::refresh` await) | remaining ≤ `OAUTH_BLOCKING_REFRESH_MS` | lock de arquivo durante HTTP (timeout do client 30 s). Outro Slim no mesmo auth.json espera. UI: “Checking authentication”. | Soltar o lock de arquivo antes do HTTP; re-adquirir só no `save_locked`. O re-read pós-lock já existe. | Não aumentar timeouts. Não inflight cross-process novo além do lock que já existe. |
| LSP-01 | slow | sim (pós-patch com LSP quente e doc aberto) | só LSP quente | `runtime/mod.rs:2833-2838`; `manager.rs:1524-1535`; `document.rs:108-118`; `patch.rs:56-67` | `write` passa `content`; `patch` passa `None`. Se `document_is_open`, `read_capped` (metadata + `read_to_string`). | relê o arquivo no await do loop antes da próxima tool. Write já não faz isso. `replacement` **não** é o arquivo — é o hunk. | Guardar o `updated` que `apply_exact_patch_with_content` já materializa e passar esse texto no notify, como o write. | Não reabrir SLOW-04/06 (warm, doc fechado, texto do write). Não segundo cache. |

## Pistas 26–30/08 (só as relidas)

| Pista | Veredito | Notas |
|---|---|---|
| audit_runtime #4 governor I/O 32 MiB / `refresh_observed` | STALE | receipts only; teste `receipts_drive_reuse_and_dependency_changes_without_governor_io` |
| audit_runtime #18 `store_dependency` cap 256 | CONFIRMED | `governor.rs:651-664`; sombra; TUI descarta Causal*; não está na tabela |
| audit_runtime #19 evidence ilimitada + bytes de diretório | PARCIAL | evidence HashMap ainda cresce; `refresh_observed` STALE |
| audit_runtime #20 clone do BTreeMap | STALE | `scoped_state_digest` itera e hasheia, não clona o mapa |
| audit_runtime #2 `cancelled()` sem `enable()` | CONFIRMED | `runtime/mod.rs:85-91`; janela intra-poll; nota, não tabela |
| audit_runtime #10 `AppHandle.events` | CONFIRMED | = TUI-01 |
| audit_tools #3 `taskkill` via CWD | STALE no core | `process.rs:518-543` usa resolver. **CONFIRMED** leftover em `slim-lsp/src/process.rs:88-91` (`Command::new("taskkill")`) |
| audit_tools Unix orphan kill | STALE | `kill -KILL -- -{pid}` + `process_group(0)` |
| audit_runtime #13 Lease Drop fora de runtime | CONFIRMED | `pool.rs:279-297`; raro; nota |
| audit_runtime #14 taskkill sync no Drop LSP | CONFIRMED | shutdown, não o loop; nota |
| audit_runtime #15 `max_servers` conta idle | CONFIRMED | 4 roots × 15 min; Slim típico 1 root; nota |
| audit_runtime #16 `read_capped` TOCTOU | CONFIRMED (moveu) | `document.rs:108-118`; hot path restante = LSP-01 e 1º `ensure_document` |
| audit_runtime #28 header LSP byte-a-byte | CONFIRMED | ~20–40 B/mensagem no read task; ruído vs rust-analyzer quente |
| audit_runtime #29 `document_sync` durante `notify` | CONFIRMED | serializa syncs, não hover/definition; RA stdin já é um pipe |
| AUDIT-STREAMING thinking/tool não expansível | STALE | `FoldState::Expanded` + `render_plain` no frame path |
| AUDIT-STREAMING fase `Responding` pós-tool | OUT-OF-SCOPE | fidelidade UI, não bug/wait/leak do frame |
| G295–G297 auth startup | OUT-OF-SCOPE | `api_key_request` ainda recusa OAuth (G296). Furo novo é expiry em voo (OAU-01), não startup |

## Ordem de correção mínima (depois; não nesta caçada)

1. **TUI-01** — esvaziar `AppHandle.events` por turno.  
2. **OAU-01** — refresh no headless + não servir o run com access que o background já substituiu.  
3. **LSP-01** — texto do patch no notify, sem reler.  
4. **OAU-02** — não segurar lock de arquivo no HTTP (mesmo sistema que 2; só se 2 não cobrir o wait).  
5. *(GOV/PRC sem item hot-path.)* `cancelled().enable()` é um one-liner se for mexer no token; não é o rendimento desta lista.

## Não objetivos (reafirmados)

SLOW-01..12; mixed-batch; SES/CTX/PRV/INT; executor novo; async-fs geral; segundo cache; host persistente de PowerShell; Rayon; MCP/child/inspector; campanha de tokens; redesenho de TUI; cobertura histórica; implementar os achados.

## Esta caçada não cobre

JSONL/resume/fila (SES), compactação/budget (CTX), HTTP/streaming/cópias de transcript (PRV), `ask_question`/Approve/Reject (INT), MCP, child agents, inspector.

## Notas, não caçar agora

- GOV cap 256 + evidence HashMap: ledger sombra; SHA-256 por chamada é CPU irrelevante.  
- PRC `cancelled()` sem `enable()`; `recv_status` sem timeout se taskkill falhar; progresso 1 Hz.  
- LSP header 4 KiB; `document_sync`; Lease Drop; idle 15 min vs `max_servers`; `kill_process_tree` LSP ainda resolve `taskkill` pelo nome.  
- TUI: thinking/tool expandido bypass do WrapCache; clone de `Line` no viewport.  
- OAU: `store.credential` Err engolido em `fresh_credential`; Codex sem skew de 5 min (Anthropic tem); warning de persistência vai a `Notification`, não à status bar de auth.  
- 401 mid-turn: não falsificável offline contra Anthropic/Codex reais; `oauth_contract` cobre refresh concorrente e persistência falha em localhost.
