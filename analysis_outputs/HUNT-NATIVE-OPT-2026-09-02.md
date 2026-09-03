# Caçada — otimizações nativas — 2026-09-02

Somente leitura. Código de hoje. Nenhum patch.

## 1. Veredito por superfície

**Loop — residual.** O predicado de compactação já é `has_compactable_history` (`runtime/mod.rs:1158`); `select_compaction_history` só corre quando o plano de background passa do soft (`2210–2214`). `discard_projected_payloads` (`1912`) esvazia strings após turno com tools; o `Vec` de eventos continua a crescer no run. Todo turno ainda clona o transcript em `redact_messages` (`1118`, `3529–3559`) e de novo em `conversation.clone_from`. CTX-02 residual: um `?` após o spawn (`1871`, `ToolEvidenceReused`) sem `cancel_pending_background`.

**Provider — residual.** Adapters built-in preparam com `from_http_body` (`provider.rs:3574–3591`, `3961+`): `Value` já montado, uma `to_string`, sem reparse. O default do trait ainda faz `from_http_request` → `from_str` (`818–824`) — só fallback de terceiro. O custo que resta no prepare de produção é clonar o transcript na redação e materializar o JSON.

**Sessão — quente.** TUI persiste por default (`tui.rs:598`). O primeiro prompt usa `resume_preflight`; os seguintes fazem `take()` (`2293`) e nunca devolvem o preflight. Cada prompt seguinte: `preflight_session` (parse + `restore_records`) + `open_no_repair_expected` (parse + `from_records`) + `persist_manual` `preflight` (HashSet de todos os IDs). O transcript já está em `execution.history` / `startup.options.history` (`tui.rs:1527–1528`). `--resume` headless paga a mesma pilha uma vez por processo.

**Tools / processo — residual, não quente.** `patch` já entrega `synced_text` (`tools/mod.rs:899`; notify em `runtime/mod.rs:2829`). `search` guarda snapshot por cursor; sem cursor, rescan é o contrato. `read` tem índice. `process.rs` resolve `taskkill` pelo resolver. `read_capped` no 1º `ensure_document` (`manager.rs:419` → `126–134`) só se o doc não está no snapshot — primeiro toque, não o loop após write/patch.

**TUI frame — residual.** WrapCache (4096 / 32 MiB) cobre body estável; hit ainda `clone` das linhas (`render.rs:718`). Streaming/thinking expandido bypassa de propósito. Sem clone/rewrap *além* do cache que mude p95 de sessão longa. Não reclassificar “TUI lenta”.

---

## 2. Tabela principal

| ID | Classe | Hot path | Prevalência | Evidência | Trigger | Impacto | Conserto mínimo | Explicitamente não fazer |
|---|---|---|---|---|---|---|---|---|
| **SES-01** | slow | persist → prepare | sessão longa TUI (default `persist_sessions`); `--resume` | `tui.rs:2293` `resume_preflight.take()`; `2968–2979` `preflight_session` de novo; `resume.rs:206–228` `parse` + `restore_records`; `474` `open_no_repair_expected`; `jsonl_repo.rs:292–316` `Value` depois `DurableRecord`; `144` `from_records`; `manual_drive.rs:351–385` HashSets; `headless.rs:451–477` reconstrói history e pisa `options.history`; `tui.rs:1527–1528` history já materializado | 2º+ prompt TUI persistido, ou cada `--resume` | Usuário espera o parse/reduce do JSONL (até 64 MiB) **duas vezes** e um scan que clona todos os IDs **antes** do 1º token. Cresce com a sessão. `--session` v1 não é este caminho. | (1) Após `drive_manual`, montar `SessionPreflight` a partir de `repo.records()` (já em memória) e devolver a `startup.resume_preflight`. (2) `preflight` do persist: testar os 4 IDs + parent contra `DurableState` do `validator` (`repository.rs:103`), ou um `any` sem HashSet. `DurableState` / `SessionPreflight` já existem. | Repo host persistente, segundo índice, migrar v1, replay de tools, abrir o arquivo a cada linha |
| **PREP-01** | leak | prepare de todo turno | todo agent loop (TUI e headless) | `runtime/mod.rs:1074` já redige `initial_messages`; `1118` `redact_messages(&messages)` clona **tudo** de novo; `3529–3559` `.cloned()` + collect; `1380–1382` `prepare_messages_with_tools` no `Value`/`to_string`; `1906` `conversation.clone_from` (cópia extra até `1936`) | cada turno, compactação default ou não | Pico ∝ tamanho do transcript no await de prepare. Prefixos já redigidos são recopiados; só o sufixo do turno (assistant + tools) é texto novo. Some no spinner antes do stream. | Redigir in-place só as mensagens novas desde o último prepare (o `messages` do loop já é owned). `conversation` só no fim (`1936` já atribui). | Tokenizer, response cache, hasher canônico, adapter paralelo, estimador incremental |

Dois itens. Residual que não entra: `AppHandle.events` ainda cresce (TUI-01 payloads já limpos); `prepare_batch` clona o `DurableState` com conteúdo (`repository.rs:177`) — mesmo wait que SES-01, mas exige separar IDs de conteúdo (tipo novo → nota); `from_http_body` `chars().count()` (`provider.rs:668`) é CPU irrelevante.

---

## 3. Pistas antigas relidas (código de hoje)

| Pista | Veredito | Por quê |
|---|---|---|
| SES-01 `restore_records` no `persist_manual` `preflight` | **PARCIAL** | `preflight` (`manual_drive.rs:351–385`) **não** chama `restore_records`. Ainda varre todos os records e clona IDs. O `restore_records` moveu para `preflight_session` (`resume.rs:228`) e `from_records` (`repository.rs:153`), e a TUI chama os dois **por prompt** depois do `take()`. |
| SES-02 headless zera tools / descarta `_plan` | **OUT-OF-SCOPE** | Contrato, não opt. `_plan` ainda descartado (`headless.rs:452`). Não reabrir. |
| CTX-01 `select_compaction_history().is_ok()` todo turno | **STALE** | `has_compactable_history` em `1158`. `select` só no plano de background acima do soft (`2208–2214`) ou hard/manual. |
| CTX-02 leak de task após `?` pós-spawn | **PARCIAL** | A maioria dos `return Err` pós-`1714` cancela. Resta `ToolEvidenceReused` `?` em `1871`. Erro raro; não tabela. |
| PRV-01 `to_string` → `from_http_request` reparse | **STALE** nos built-in / **CONFIRMED** no default do trait | `from_http_body` (`609–626`, `3579–3591`) passa o `Value`. Default `818–824` ainda reparseia. Residual de cópia = PREP-01. |
| INT-01 `completed` na capacidade | **STALE** (bug) | `pending.len()` só (`interaction.rs:237`). `completed` ainda cresce (IDs, `268`/`287`) — leak miúdo, não `RouteCapacity`. INT não é opt. |
| TUI-01 `AppHandle.events` sem teto | **PARCIAL** | `discard_projected_payloads` (`model.rs:440–456`; loop `1912`) limpa payloads. `push_event` ainda `clone`+`push` (`391–400`). `Vec` cresce no run. Pedido: não relistar. |
| OAU-01 headless sem `fresh_credential` / expiry s | **STALE** | `cli.rs:458` `fresh_credential`; `oauth/mod.rs:308–313` `expiry_ms` trata segundos. |
| OAU-02 lock de arquivo no HTTP | **STALE** | HTTP sob `refresh_gate` (`194–222`); `lock_exclusive` só em `load_credential_locked` / `persist_refreshed` (`232`, `247`). |
| LSP-01 patch `read_capped` | **STALE** | `synced_text` no receipt (`tools/mod.rs:899`); notify usa esse texto (`runtime/mod.rs:2829`; `manager.rs:1527–1528`). `read_capped` só se `text` é `None` (`1530–1531`). |
| `enqueue_persisted` O(n²) | **OUT-OF-SCOPE** | Testes; loop usa `drive_manual`. |
| `cancelled().enable()` | **OUT-OF-SCOPE** | Pedido: não item principal. |
| GOV cap 256 / evidence HashMap | **OUT-OF-SCOPE** | Sombra; TUI descarta Causal*. |
| Parse JSONL `Value`+`DurableRecord` | **CONFIRMED** e agora **por turno TUI** | Era “só no open”. Com `take()` + re-preflight, é o wait de SES-01. |
| `prepare_batch` clona validator | **CONFIRMED** | Incremental (não revalida prefixo). Clona entries com conteúdo, 2× por persist. Nota; tipo novo para evitar. |
| `--session` v1 drain no fim | **OUT-OF-SCOPE** | `headless.rs:1315–1321`. Legado; TUI/v2 não usa. |
| 1º `ensure_document` `read_capped` | **CONFIRMED** nota | Primeiro toque se o doc não está no snapshot. Write/patch quente já notificam com texto. |
| `taskkill` por nome em `slim-lsp` | **CONFIRMED** nota | `slim-lsp/src/process.rs:88–91`. Shutdown, não o loop. Core usa resolver (`process.rs:518–543`). |
| WrapCache / assistant congelado | **STALE** como “TUI lenta” | Cache no sítio. Clone no hit é o retorno owned. Sem overlay. |

---

## 4. Ordem de correção mínima (não implementar)

1. **SES-01** — devolver `SessionPreflight` in-memory após o persist TUI; `preflight` do prefixo contra estado já reduzido (ou 4 IDs sem HashSet). Um wait que o usuário já paga em todo prompt persistido.
2. **PREP-01** — redigir só o sufixo novo. Um arquivo (`runtime/mod.rs`). Só depois de SES-01; o ganho é pico, não I/O.

Um item por vez. CTX-02 (`1871`) e o `Vec` de eventos ficam atrás: erro raro / residual já nomeado.

---

## 5. Não objetivos + o que esta caçada não cobre

Não implementar. Não PR. Não suíte. Não `refresh-slim.ps1`. Não Rayon, async-fs geral, segundo cache, host persistente de PowerShell, tokenizer, response cache, executor/JoinSet, overlay TUI, campanha de tokens, flag nova, crate nova.

Não reabrir SLOW-01..12, mixed-batch, MCP/child/inspector, redesenho TUI/OAuth, `cancelled().enable()` como item. Não tratar GOV sombra como hot path. Não relistar TUI-01/OAU-01/OAU-02/LSP-01 como backlog — só o veredito STALE/PARCIAL acima.

Não cobre: CLI parse, catálogos, OAuth login/PKCE, testes, benches, docs, `--session` v1 como persist do run TUI, SHA-256 do governor, header LSP byte-a-byte.

Falsificadores (não rodados): nenhum teste conta `preflight_session` por prompt TUI; `session_durable_run` não conta HashSets no `preflight`; testes de compactação não proíbem `select` abaixo do soft (o código já não faz); `provider_http` não mede clone de redação.

Critério de concluído: cinco superfícies lidas no código de hoje; tabela só com hot path + prevalência real; pistas antigas com veredito, sem copiar a tabela de 02/09.
