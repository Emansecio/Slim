# Sugestões performance CHAMADAS/TOOLS/LEITURAS — 2026-09-04

> **Não é o documento canônico.** Notas curtas da mesma data. Para outro
> agente executar (mapa de código, harness, SPD-READ-01, falsificador de
> path absoluto):
> `analysis_outputs/AUDIT-SPEED-CALLS-TOOLS-READS-2026-09-04.md`.
> SPD-READ-01 está **feito** (2026-09-04); o canônico tem before/after em §7.1.
> Aquele arquivo tem o micro de `read`/`search` no `ToolRegistry` e prevalece
> em caso de conflito.

Só investigação, zero edits. Base: código lido hoje (RULES R3/R5), 2 benches executados.
Labels: **[verificado]** = lido/medido hoje · **[hipótese]** = sem número · **[não verifiquei]** = fora do lido.
Estado árvore: 23 arquivos com sujeira pré-existente (ToolLimit, loop_guard, manual_drive de sessão anterior); nada meu.

Medidas cruas desta sessão:

```text
cargo bench -p slim-core --bench tool_setup
mode=auto tools=7 serialized_bytes=3469 iterations=20000 samples=11
per_turn_median_total_ns=467149200 per_turn_min_total_ns=458651100
cached_median_total_ns=22000 cached_min_total_ns=21500
per_turn_median_ns_per_iteration=23357.460 cached_median_ns_per_iteration=1.100 speedup_x=21234.1

cargo bench -p slim-core --bench compaction_selector
selector_10k_p95_ms=1.441 selector_20k_p95_ms=2.915 scaling_x=2.02
```

Divergência achada: bench hoje = **7 tools / 3469 B** vs `bench/token-economy/v2/RESULTS.md` §2 (6 tools / 1418 B).
Causa: `code_intel` entrou nas definições (`crates/slim-core/src/tools/mod.rs:396`, 7 names em 348–402).
Doc stale; próxima campanha `s1_read` mostra T1 tools maior que 1592 B. Sem ação código.

---

## S1. finalize +1 roundtrip em stops capped — DESCARTAR

O quê: em `TurnLimit|ToolLimit|NoProgress`, loop clona `messages`, anexa `BUDGET_FINALIZE_PROMPT`,
chama provider com `tools=[]` (`crates/slim-core/src/runtime/mod.rs:~2063–2095`;
consts em `:284` `BUDGET_FINALIZE_PROMPT`, `:289` `MAX_BUDGET_STEERS`, `:290` `BUDGET_STEER_PROMPT`;
steer em `:2043–2048`).
Justificativa contra: request extra carrega histórico cheio, mas produz resposta final da task.
Remover = some resposta, muda comportamento (restrição 1). Custo de qualidade, não overhead.
Refs: `mod.rs:~2063 final_messages = messages.clone()`, `:2064 push FINALIZE`, chamada
`run_provider_messages_with_tools` logo abaixo; `mod.rs:2111 self.conversation = messages` (move, sem clone).
**[verificado]** leitura.

## S2. `prepare_batch` clona validator O(sessão) por batch — ADIAR (bench primeiro)

O quê: `crates/slim-core/src/session/repository.rs:172–196`; `:177 let mut next = self.clone()`.
Struct (`:102–110`) = `DurableState` + `BTreeMap` lifecycles + `AttemptLedger` + `DurableQueue` +
`ToolPhaseLedger`: cresce com sessão → clone O(n) por batch, O(n²) total em sessão longa.
Frequência real: headless 1 batch/run (`crates/slim-cli/src/headless.rs:1351 append_batch(&events)`);
manual 2 batches/op (`crates/slim-core/src/session/manual_drive.rs:~209 prefix`, `persist_manual_success`
suffix); `jsonl_repo.rs:220–260` prova 1 `sync_data` por batch (`:350 sync_append_data`), não por record.
Justificativa adiar: sem sessão longa real no ambiente (mesmo rationale TOK-02/06), custo **[hipótese]**.
Caminho correto: micro-bench sessão sintética (10k records) medindo `append_batch`; só então validar
read-only + aplicar (path correção-crítica, erro = corrupção sessão).
**[verificado]** leitura; custo **[hipótese]**.

## S3. `patch` retém arquivo inteiro em `synced_text` — ADIAR (medir antes)

O quê: `tools/mod.rs:899 synced_text: Some(content.text)`; `patch.rs:1–70` mostra `text: updated`
(arquivo todo pós-replace); loop clona em `runtime/mod.rs:3034–3035`
(`PreparedToolArguments::Write content.clone()` / `Patch synced_text.clone()`) para
`notify_file_changed`, e receipt carrega o `String` no `completed` do turno.
Justificativa adiar: 1 alloc O(arquivo) por escrita (não por turno); micro-opt seria guardar só
quando `code_intel.is_some()`, mas receipt é tocado por governor/evidence
(`observe_after :218–279`, `reused_evidence :304–308`) — risco contrato interno.
Medir: patch em arquivo 1 MiB, heap antes/depois. Ganho provável pequeno.
**[verificado]** leitura; impacto **[hipótese]**.

## S4. search fresco sempre rescan; query repetida entre turnos não reusa snapshot — ADIAR

O quê: `tools/search.rs:~185–260 page()`; `offset>1` reusa snapshot (`reuse_snapshot`),
`offset==1` sempre `search_with_walker` (comentário intencional: observar edição externa);
cursor path `page_from_cursor`; cache máx 8 snapshots, TTL 120 s (`:17–30` consts).
`list.rs:1–120` idêntico (64 snapshots, TTL 120 s). `read.rs` tem checkpoints incrementais
(`:16–19` consts, `index_for`/`merge_checkpoints`) — paginação alta já O(página).
Justificativa adiar: reuso cross-turn precisaria checar revisão/stamp do diretório; evidence cache
(`tools/mod.rs:660–700 lookup_cached_evidence` com `stamp_matches`) já mitiga repetição idêntica.
Sem telemetria de repetição real (cf. TOK-02 rationale), é especulativo tipo TOK-07.
**[verificado]** leitura.

## S5. code_intel serial? Não existe barreira — DESCARTAR (já paralelo)

O quê: `tool_call_is_parallel_snapshot_read` inclui `name == "code_intel"` (`runtime/mod.rs:172–177`);
segmento read-only executa via `buffer_unordered(READ_ONLY_BATCH_CONCURRENCY)` (`:2742`),
com `code_intel` no futuro async (`:2683 run_code_intel_request`); serial só para mutating (contrato).
LSP: `ensure_document` checa `document_content_snapshot_async` + `stamp_matches_path` antes de
disco (`crates/slim-lsp/src/manager.rs:423–452`); first-open manda `didOpen`, depois `didChange`
(`instance.rs:342–482`); `notify_file_changed` pós-write é ordenação exigida
(`runtime/mod.rs:~3010–3040`, warm-only fail-open em `codeintel.rs:141–143`).
**[verificado]** leitura. Nada a fazer.

## S6. build duplo de request por turno? Não — DESCARTAR (PERF-04 segura)

O quê: por turno, `estimate_unprepared_request_chars` (`mod.rs:4688–4760`, walk O(chars) barato)
→ 1 `prepare_messages_with_tools`; reuse em `:1427–1429`
(`Some(request) => request, None => client.prepare...`); rebuilds em `:1189` (preflight
non-text), `:1282`/`:1387` (pós-compaction — mensagens mudaram, obrigatório), `:4029` (compactação).
`debug_assert!(serialized_chars <= preflight_chars)` ancora estimate vs real.
**[verificado]** leitura. Nada a fazer.

## S7. clones O(n) restantes no loop — DESCARTAR (justificados)

O quê: `clone_from` só em `:1134` (init, publica estado inicial) e `:2001` (branch
`RepeatedFailedTool`, publica antes de retornar); fim do loop é move (`:2111`);
`final_messages` clone só no caminho finalize (S1); `calls.clone()` por turno (`:1724`,`:1908`)
é vet pequeno + `stub_mutating_tool_arguments` (`:4840–4870`, JSON parse só de args mutating);
`prompt_output` (`:4914–4926`) clona ≤16 KiB ou trunca; `tool_output_hash` (`:4871–4876`) é hash
rápido p/ dedup TOK-03; redact 1× por call na produção (`redact_sensitive :828–830`,
`redact_message :3753–3764`), resto borrows.
Branch ToolLimit já sem clones redundantes (diff pré-existente em `truncate_calls_for_budget`,
`mod.rs:~208–260` + call site `~1638`, com `max_total_tool_calls` 256).
**[verificado]** leitura+grep. Tentar zerar = churn com delta em ns–µs, invisível no ruído 1,16 s Start-Job.

## S8. persistência/aparelho eventos — DESCARTAR backend (já batch); TUI fica residual

O quê: `append_batch` escreve N records + 1 fsync (`jsonl_repo.rs:220–260`, `:350`);
headless persiste 1 batch/run (`headless.rs:1351`); `manual_drive` 2 batches/op (prefix/suffix
separados por `execute()` — infundíveis). `AppHandle.events` é `Vec` unbounded
(`model.rs:348–400`), drenado no fim (headless) — crescimento intra-run proporcional a eventos,
sem clone por turno (slices `events()[..]` + collect só de deltas desde `event_start`).
Justificativa: P2-backlog "batch persistência" já realidade no hot path; resta retenção TUI
(channel ilimitado core→TUI), que exige decisão contrato — residual conhecido, não desta missão.
**[verificado]** leitura.

## S9. bloco tools no fio cresceu (TOK-07 voltando) — ADIAR (quantificar na campanha)

O quê: 6→7 definições, 1418→3469 B bench (§medidas). T1 tools `RESULTS.md` (1592 B/6) stale.
Corte dinâmico do bloco = TOK-07: risco esconder capability → turno extra (pior que bytes).
Justificativa adiar: sem A/B `run_benchmark.ps1 -VariantTag` mostrando bytes vs turnos, qualquer
seleção é especulação (anti-goal explícito). Próxima campanha `s1_read` quantifica.
**[verificado]** medida.

## S10. compactação — DESCARTAR (já econômica)

O quê: gate break-even (`projected_savings vs estimated_cost + margem/4`, `mod.rs:~2400–2470`),
evento `CompactionSkippedBelowBreakEven` quando não paga; foreground só over_hard/manual;
selector linear (bench: 10k p95 1,44 ms, 20k 2,02×). `estimate_provider_message_tokens`,
`build_background_compaction_plan`, `compact_before_send` (`:3860+`) lidos.
TOK-02/06 seguem sem telemetria real (TUI não persiste; headless só com `--session`; nenhum log
real no ambiente) — sem limiar, sem code. **[verificado]** leitura+bench.

---

## Já conquistado (não reabrir sem nova evidência)

PERF-01–07 (setup único, shell drena pipes, 1 build, SSE 14,2×, Base64 std, read streaming) +
`buffer_unordered(8)`, prepare batch único, dedup distinct (`mod.rs:2501–2531`), evidence aliasing
(`:195–207`,`:2781–2800`), dedup reads TOK-03, `preflight` early-exit (`manual_drive.rs:~386–388`,
diff sessão anterior), 2 `clone_from` ToolLimit removidos (diff pré-existente).
Prova: `bench/token-economy/v2/RESULTS.md`.

## Backlog herdado (vereditos mantidos)

- TOK-02/TOK-06: sem telemetria → sem limiar. TOK-07: especulativo → campanha primeiro.
- WrapCache >4096: benchmark cobre 3200; ampliar bench antes de code.
- Bounded TUI retention: exige contrato. Descartados com rationale: TOK-13/15/17/18.

## Falsificadores aplicados a todas

µs somem no ruído 1,16 s Start-Job (falsifica H7 e qualquer micro-opt CPU); byte-corte que
adiciona turno perde (falsifica H1/H9); zero edits → zero quebra teste (R10 não acionado);
doc-vs-código: código ganhou (RESULTS.md 6 tools vs bench 7 tools hoje).
