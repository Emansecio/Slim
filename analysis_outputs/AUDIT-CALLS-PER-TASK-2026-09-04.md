# AUDIT — CALLS POR TAREFA REAL (Slim)

> **Leitor:** agente frio. **Investigação só — zero patch.**
> **Data sessão:** 2026-09-04. **Prevalência:** `RULES.md` > `AGENTS.md` > este prompt > `AUDIT-SPEED-CALLS-TOOLS-READS-2026-09-04.md` > docs antigos.
> Código lido nesta sessão ganha de auditoria (R3/R5). Números só com comando+saída desta sessão (R1/R2).
> Labels: **[verificado]** lido/medido nesta sessão · **[inferido]** de código lido sem número isolado · **[hipótese]** sem prova.
> **Veredito em uma linha:** corpus de ouro (event log) **vazio em disco**; TUI v2 tem 8 arquivos / 9 ops single-attempt sem tools — **bloqueio declarado**, sem prevalência de produto. Entrega = números do que existe + receita de captura + parser pronto.

## 0. Plano mínimo (reafirmado antes de agir)

- **Objetivo:** medir, em tarefas reais persistidas, quantas chamadas de provider o Slim dispara por tarefa; fração extra (finalize / steer / overflow retry / compactação); se isso explica parede, não µs locais.
- **Não objetivos:** patch; remover finalize/steer; TOK-02/06/07; TOK-07 sem A/B; reabrir PERF-01..07 / SPD-READ-01 / micros; rewrite do loop; campanha `bench/token-economy/v2` como substituto; provider live; `refresh-slim.ps1`.
- **Aceite:** este arquivo (corpus, método, histogramas, tabela priorizada, não-tocado, incertezas). Zero diff `crates/`, zero teste novo.
- **Escopo intocado:** comportamento, schemas JSONL, prompts, tools, TUI, wire format.

## 1. Corpus (inventário [verificado])

### 1.1 Ouro: event log (SessionWriter) — N = 0

Comando + saída desta sessão:

```text
grep -rl "ContextSnapshot" --include="*.jsonl" D:/Slim/.slim D:/Slim/bench D:/Slim/tests
(fim lista ouro; vazio = zero event log em disco)
```

**[verificado]** nenhum JSONL em disco contém `ContextSnapshot` / `request_kind` / `serialized_chars`. Headless `--session` persiste `SessionEvent` via `SessionWriter::append_batch(&events)` (`crates/slim-cli/src/headless.rs` ~1348–1355, `runtime.app.drain_events()` lido nesta sessão) — mas nenhum arquivo desse formato foi produzido/arquivado. Sem ouro, sem `UsageTotals::from_events`, sem `tool_schema_bytes`, sem `provider_latency_ms`.

### 1.2 TUI v2 durável (JsonlRepo) — 8 arquivos / 9 ops

`D:\Slim\.slim\sessions\`, schema `{"type":"session","schema_version":2,...}` **[verificado]** (primeira linha dos 8 arquivos). Bytes totais 31.683, datas 01–03/09/2026. Parser one-shot `%TEMP%` (`/tmp/slim-calls-per-task.py`, fora do repo), saída:

```text
files=8
name | lines | bytes | ops | attempts | toolrecs | compactions | att_failed | usage
tui-1788295687763874100-9728-1.jsonl | 7 | 1735 | 1 | 1 | 0 | 0 | 0 | 0
tui-1788299497196290500-28600-1.jsonl | 8 | 16388 | 1 | 1 | 0 | 0 | 0 | 1
tui-1788329687592963000-19516-1.jsonl | 8 | 4731 | 1 | 1 | 0 | 0 | 0 | 1
tui-1788419334555772700-40424-1.jsonl | 10 | 2323 | 2 | 2 | 0 | 0 | 0 | 0
tui-1788419608483586800-37232-1.jsonl | 7 | 1581 | 1 | 1 | 0 | 0 | 0 | 0
tui-1788474589323370000-34820-1.jsonl | 7 | 1574 | 1 | 1 | 0 | 0 | 0 | 0
tui-1788480872757921300-22440-1.jsonl | 8 | 1777 | 1 | 1 | 0 | 0 | 0 | 1
tui-1788489355110965900-29344-1.jsonl | 7 | 1574 | 1 | 1 | 0 | 0 | 0 | 1
TOTALS ops=9 attempts=9 toolrecs=0 compactions=0 att_failed=0 usage=3
```

Records por arquivo: `session:1, entry:2, operation:4 (+started/attempt_started/attempt_finished/finished), usage:0–1`. Exceção: `...40424...` tem 2 ops (3 entries / 6 operations — segunda op truncada, sem `finished`). Outcomes `finished`: **failed 6, success 2**, 1 op sem finish **[verificado]**. Conteúdo assistant: 4× `provider error: http 500`, 1× `provider transport failed`, 1× `provider returned a malformed tool call`, 2 respostas de revisão manual longas. **Zero** `ToolPhase*`/`ToolIntent`, **zero** `Compaction`, **zero** `ProviderAttemptFailed`, **zero** texto `Budget exhausted` / `duplicate omitted` **[verificado]** (único hit de "compaction" é prosa dentro de resposta de revisão, não record).

### 1.3 Exclusões declaradas (não contam como tarefa real)

| Candidato | Por que fora |
|---|---|
| `bench/token-economy/v2/.../captures/*/slim/compliance.jsonl` | harness de tokens: `{"scenario","run","agent","req",...}` por req, sem snapshots/calls de loop; missão proíbe usar como substituto **[verificado]** (lido `s1_read/run1/slim/compliance.jsonl`, 2 linhas) |
| `bench/.../workspaces/*/.slim/` | só `artifacts/`, sem sessions **[verificado]** |
| `tests/fixtures/v1_fake_provider.jsonl` | fixture sintética `schema_version:1`, 3 linhas **[verificado]**; missão proíbe `--fake` como real |
| testes `agent_loop.rs` / `usage_ledger.rs` / `provider_http.rs` | sintéticos, sem tarefa real; citam `ContextSnapshot` só como construtor |
| `D:/.slim/`, `crates/slim-cli/.slim/` | só `artifacts/`, sem sessions **[verificado]** |
| `.lock` (8 arquivos 0 bytes) | lockfiles, não dados |

Nenhum dump copiado para `analysis_outputs/`; nenhum segredo tocado.

## 2. Método (como cada classe seria contada; onde falha no v2)

Âncoras lidas nesta sessão **[verificado]**: `runtime/mod.rs` loop ~1140 (`overflow_retry_used`, `budget_steers_used`), snapshot ProviderTurn ~1488–1508, overflow `continue` ~1571–1599, steer ~2041–2047 (`BUDGET_STEER_PROMPT`, cap `MAX_BUDGET_STEERS=2`), finalize ~2056–2067 (`BUDGET_FINALIZE_PROMPT`, `tools=[]`, só `TurnLimit|ToolLimit|NoProgress` + não-cancelado); `run_provider_messages_with_tools` ~902–917 (snapshot ProviderTurn antes do send); compactação ~3908–3918 (`RequestKind::Compaction`, `tool_schema_bytes:0`); `events.rs` `RequestKind::{ProviderTurn,Compaction}`, `ContextSnapshot`, `CompactionSkippedBelowBreakEven` (não é call), `RequestCompleted`; `usage.rs` `UsageTotals::from_events` (agregador canônico com event log); `schema_v2.rs` `DurableOperationKind::{ProviderAttemptStarted,Finished,Failed}`, `DurableUsage{input_tokens,output_tokens}` (sem `request_kind`), `CompactionCheckpoint`; `effects.rs` `planned_provider_effects`; `headless.rs` drain+`append_batch`; `tui.rs` `persist_sessions:true`, `workspace_sessions_dir` (`<workspace>/.slim/sessions/`).

Unidade de análise: **um agent loop / um prompt até stop** (= 1 `operation_id` no v2, não o arquivo multi-resume inteiro).

| Classe | Ouro (event log) | v2 (este corpus) — perda declarada |
|---|---|---|
| Turno normal | `ContextSnapshot` ProviderTurn + `tool_schema_bytes>0` | `ProviderAttemptStarted` ≈ 1 call; sem tools/schema **[inferido]** |
| Steer | texto `BUDGET_STEER_PROMPT` em user entry, ≤2/loop | **incontável**: prompts podem estar redigidos/ausentes; zero hits |
| Finalize | ProviderTurn `tool_schema_bytes==0` após stop capped | **incontável**: `==0` não existe no v2; proibir vender `attempt` sem tools como finalize |
| Overflow retry | `RequestUsage.retry_count` / 2 snapshots 1 turno | **incontável**: `DurableUsage` sem `retry_count`; zero `ProviderAttemptFailed` |
| Compactação call | snapshot `RequestKind::Compaction` | só `Compaction` checkpoint (ausente aqui); skip break-even nunca foi call |
| Stop | `AgentLoopStop` (8 variantes; default `max_turns=128` **[verificado]** `mod.rs:134`) | só `DurableOutcome{success,failed,cancelled,unknown}` — **não** é stop do loop |
| Latência | `provider_latency_ms` | ausente (`duration_ms` só em checkpoint de compactação) |

Falsificador aplicado (R10): `finished outcome=failed` no v2 ≠ `AgentLoopStop` nem `ProviderAttemptFailed` — é outcome durável da operação resume (aqui: http 500 / transport). Não cruzar com regra do finalize.

## 3. Números

N = 9 ops em 8 arquivos (1 arquivo com 2 ops). **N < 10 → relatório exploratório; sem conclusão de prevalência de produto** (contrato da missão).

| Métrica | Valor [verificado] |
|---|---|
| `C_total` (attempts por op) | min 1 / p50 1 / p95 1 / max 1 (histograma: 9×1) |
| `C_steer / C_finalize / C_overflow / C_compact` | **não mensuráveis** no schema v2 (tabela §2) |
| `C_extra` | **desconhecido** (não zero — desconhecido) |
| `sum(C_extra)/sum(C_total)` | indefinido |
| % tarefas com cada classe extra | indefinido |
| tools executadas por tarefa | 0 em 9/9 ops (zero ToolPhase records) |
| latência provider extra | sem `provider_latency_ms` no v2 |
| `DurableUsage` tokens (3 ops com usage) | 4.922.379/52.965; 69.868/7.576; 49.735/1.014 (in/out) — tokens de resume, não calls |

Comando: `python3 /tmp/slim-calls-per-task.py` (EXIT=0) + `grep -rl ContextSnapshot` (vazio) — saídas em §1.

## 4. Tabela priorizada

| ID | Hot path + prevalência | Evidência (medida, comando, N) | Economia esperada | Esforço | Risco qualidade | Veredito |
|---|---|---|---|---|---|---|
| CALL-PREV-01 | distribuição `C_total`/`C_extra` | N=9 ops, todas `C_total`=1, `C_extra` desconhecido (`/tmp/slim-calls-per-task.py`, EXIT=0) | nenhuma quantificável | — | — | **adiar** até haver ouro; N<10 não sustenta prevalência |
| CALL-FIN-01 | finalize: +1 RTT quando dispara (3 stops capped) | código `mod.rs` ~2056–2067 **[verificado]**; prevalência **não medida** (v2 não distingue) | 1 RTT/tarefa capped | médio | **alto** (muda texto final; restrição 1) | **descartar** mexer nesta missão; só reportar custo |
| CALL-STEER-01 | steer por duplicate, ≤2/loop | código ~2041–2047 **[verificado]**; zero hits no corpus | ≤2 RTT/loop quando dispara | médio | médio (muda comportamento de loop) | **adiar** (sem bytes de prevalência) |
| CALL-OVF-01 | overflow retry 1×/turno | código ~1571–1599 **[verificado]**; zero `ProviderAttemptFailed` no corpus | 1 RTT no turno estourado | médio | alto (caminho de sobrevivência) | **adiar** |
| CALL-CMP-01 | compactação com call vs skip break-even | snapshot `Compaction` ~3908–3918 + `CompactionSkippedBelowBreakEven` ≠ call **[verificado]**; zero `Compaction` records no corpus | 1 call quando compacta | médio | alto | **adiar**; skip já evita call sem custo |
| CALL-STOP-01 | mix `AgentLoopStop` (explica se extra é budget ou tarefa curta) | 8 variantes `mod.rs` ~250–261 **[verificado]**; v2 só tem `DurableOutcome` (6 failed/2 success/1 sem finish) — **não** é stop | — | — | — | **adiar**; stop real só no ouro |

Nenhum "fazer": nenhum extra barato sem mudança funcional apareceu — o valor desta missão é o número (aqui: o bloqueio), não patch.

## 5. Deliberadamente não tocado

`crates/` (zero diff); testes (nenhum novo, suíte não rodada — investigação); finalize/steer (não removidos); TOK-02/06/07; TOK-07 sem A/B; PERF-01..07 / SPD-READ-01 / micros (`chars().count`, hex, `spawn_blocking`, `names_for_mode`); rewrite do loop; campanha `bench/token-economy/v2` como substituto; provider live; `refresh-slim.ps1`; `run_benchmark.ps1`; `cargo test --workspace`; RTT stale de `RESULTS.md` (não usado); `RESULTS.md` de tokens (não reescrito); contagem de testes (não atualizada).

## 6. Incertezas

- TUI v2 vs event log: v2 perde `request_kind`, `tool_schema_bytes`, `retry_count`, latência, stop real, e pode redigir prompts — steer/finalize invisíveis por construção.
- Este corpus nem é loop de agente: são ops resume single-attempt (`manual_drive`), 6/9 failed por http 500/transport — não representam tarefa com tools.
- Resume multi-loop: recorte por `operation_id` adotado; arquivo `...40424...` tem 2 ops (1 truncada sem `finished`).
- N=9 < 10: exploratório por contrato.
- `input_tokens` 4,9 M numa op é token de resume, não custo de loop.

## 7. Receita de captura (bloqueio: pedir JSONL reais ao humano)

```text
# ouro: event log (único formato que conta calls por classe)
slim --headless --session <path.jsonl> ...   # persiste SessionEvent via SessionWriter

# TUI: <workspace>/.slim/sessions/*.jsonl    # v2; calls = operations, não snapshots
```

Parser pronto: `/tmp/slim-calls-per-task.py` (one-shot, fora do repo) — conta attempts/tools/compactions por `operation_id` no v2. Com ouro em mãos: `UsageTotals::from_events` (`runtime/usage.rs`) sobre os events drenados; classificar snapshots por `request_kind` + `tool_schema_bytes` + texto `BUDGET_*` + `retry_count` (§2). Não simular com `--fake`.

## ✅ Verificação de Entrega (missão de investigação — adaptações declaradas)

- [x] Rodei o que afirmo (comando + saída em §1/§3)
- [x] Números contados nesta sessão (R2); paths:linhas lidos nesta sessão (R3)
- [ ] `cargo test --workspace` — **não rodado** (zero patch; missão proíbe)
- [ ] `.\refresh-slim.ps1` — **não executado** (zero patch)
- [x] Docs: este audit + 1 linha no tracker §7; `RESULTS.md`/testes não tocados (proibido)
- [x] Incertezas declaradas (§6); inferências rotuladas (R4)
