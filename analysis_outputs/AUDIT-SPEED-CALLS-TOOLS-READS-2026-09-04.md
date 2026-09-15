# AUDIT — velocidade de CHAMADAS, TOOLS e LEITURAS

> **Leitor:** agente executor que caiu frio neste repo.
> **Ação depois de ler:** (1) não reabrir PERF-01..07 nem os descartes abaixo;
> (2) **SPD-READ-01 feito** em 2026-09-04 (`tools/read.rs`: `writeln!` +
> offset via `bytes_read`, sem `stream_position` por linha). Não reabrir
> sem evidência nova de regressão.
> (3) Números de micro da investigação (2026-09-03) estão em §5; before/after
> do patch está em §7.
>
> **Data da investigação:** 2026-09-03 (sessão Grok). Documento gravado em
> 2026-09-04. Patch de produto: só SPD-READ-01, mesma data, após aprovação.
>
> **Prevalência:** `RULES.md` > `AGENTS.md` > este arquivo > `RESULTS.md` /
> `AUDIT-ECONOMIA-TOKENS.md` / memória. Docs descrevem o passado; o código
> ganha depois de relido (R5).
>
> Labels: **[verificado]** lido ou medido nesta investigação · **[inferido]**
> conclusão a partir de código lido sem número isolado · **[hipótese]** sem
> prova. Inferência disfarçada de fato é proibida (R4).

Irmão mais curto, mesma data, **sem** o micro de `read`/`search` do loop:
`analysis_outputs/SUGESTOES-PERFORMANCE-2026-09-04.md` (S1–S10). Onde
divergir, **este arquivo prevalece** para velocidade local. Economia de
**tokens** continua em `analysis_outputs/AUDIT-ECONOMIA-TOKENS.md` e
`bench/token-economy/v2/RESULTS.md` — são missões diferentes.

---

## 0. Como usar este documento

1. Restrições da missão (§1). Violar = falha, mesmo com bench verde.
2. O que já está feito e **não reabrir** (§2).
3. Como repetir as medidas (§3). Sem número novo, não implemente.
4. Mapa do código lido (§4). `arquivo:linha` só do que foi lido na sessão.
5. Saída crua (§5). Copie o método, não o número antigo.
6. Tabela SPD-* (§6). Um veredito por linha.
7. Receita de SPD-READ-01 (§7) — única fatia candidata.
8. O que não tocar e porquê (§8). Incertezas (§9).

Não rode `bench/token-economy/v2` para decidir SPD-READ-01: aquele harness
mede payload/RTT de provider, não o CPU de `read`/`search`.

---

## 1. Missão e restrições (contrato)

Objetivo: ganhos **reais de velocidade** em

1. **CHAMADAS** — roundtrips do provider e custo local prepare→serialize→stream→parse
2. **TOOLS** — batch read-only vs barreiras mutating, prepare, `workspace_revision`, `code_intel`, shell, `materialize_results`
3. **LEITURAS** — `read` índice/paginação, `search` rescan, `patch`/`synced_text`, LSP `ensure_document`

sem perder qualidade e **sem mudar comportamento**.

Não objetivos (anti-goals): rewrite do loop/trait provider/event system;
executors/pools/cache layers novas; UX visível (prompts, wording, layout TUI,
campos JSONL); transporte binário; reuse `ProviderCache`; tokenizer novo;
response cache; hasher canônico; adapter paralelo; overlay TUI; campanha de
tokens sem A/B (`run_benchmark.ps1 -VariantTag`); flag/crate nova sem
necessidade atual.

Restrições duras:

1. Zero mudança funcional: comandos, flags, outputs, schemas sessão/JSONL,
   tool behavior, exit codes, wire format.
2. Zero regressão: cada mudança com medida não-piora. Trade-off → parar e
   reportar, não implementar.
3. Edits cirúrgicos. Sem refactor drive-by.
4. Preservar API pública: `NATIVE_SYSTEM_PROMPT`, `with_system_prompt()`,
   `without_system_prompt()`, `with_reasoning_effort()`, todos `pub` do core.
5. Âncoras: system prompt em todos os adapters; `reasoning_effort` via
   env/toml; `cache_control: ephemeral`; dedup reads; footer de paginação;
   shell cap 8 KiB/stream no prompt; compaction summary; telemetria
   `ContextSnapshot`.

Mutating continua **sequencial** (contrato). Read-only já é paralelo.

---

## 2. Já conquistado — não reabrir sem evidência nova

| ID | O quê | Prova histórica |
|---|---|---|
| PERF-01/02 | Setup de tools uma vez por loop, não por turno | `crates/slim-core/benches/tool_setup.rs`; `RESULTS.md` §2–3 |
| PERF-03 | Shell drena stdout/stderr em threads | `crates/slim-core/src/process.rs` |
| PERF-04 | Um build de request no caminho quente | `PreparedProviderRequest` |
| PERF-05 | SSE linear (cursor + um drain/chunk) | `provider.rs` `drain_sse` |
| PERF-06 | Base64 std | — |
| PERF-07 | `read` paginado com `BufReader` | `tools/read.rs` |
| — | Read-only `buffer_unordered(8)` | `runtime/mod.rs` `READ_ONLY_BATCH_CONCURRENCY` |
| — | `prepare_invocations` em lote + workspace canônico 1× | `tools/mod.rs` |
| — | Dedup de pares idênticos no prepare | `runtime/mod.rs` `prepare_provider_tool_invocations` |
| TOK-03 | Dedup de output byte-idêntico no fio | `runtime/mod.rs` ponteiro `[duplicate …]` |
| — | Evidence aliasing no segmento paralelo | `evidence_reuse_aliases` |
| — | Snapshot `search`/`list` para `offset > 1` **no `ToolRegistry` vivo** | `tools/search.rs`, `tools/list.rs` |
| — | Checkpoints de `read` no `ReadService` persistente | `tools/read.rs` |
| **SPD-READ-01** | `writeln!` + offset `checkpoint + bytes_read`; sem `File::stream_position` por linha | §7 before/after 2026-09-04: 4096 **1,738 ms → 0,299 ms (5,81×)** |
| — | `preflight` manual_drive early-exit | `session/manual_drive.rs` |
| — | 2 `conversation.clone_from` redundantes no branch ToolLimit removidos | diff pré-existente na árvore suja |

Startup ~11 ms e overhead de loop ~2 ms/turno: tratados no backlog anterior.
Não reabrir.

Backlog de **tokens** (não desta missão de CPU): TOK-02, TOK-06, TOK-07
(especulativo — quantificar com campanha A/B antes). Descartados com rationale
em `AUDIT-ECONOMIA-TOKENS.md`: TOK-13, TOK-15, TOK-17, TOK-18.

Payload `s1_read` (mediana histórica, **não revalidada nesta sessão**): Slim T1
4.238 B vs Pi 14.670 B / Pit 26.528 B na soma. Comparação cross-agent válida
só em `s1_read`. Ver `RESULTS.md`.

---

## 3. Ambiente e como re-medir

### 3.1 Toolchain **[verificado]**

Nesta sessão:

```
rustc 1.98.0 (88d9e12ae 2026-08-18)
cargo 1.98.0 (797e8a9bc 2026-08-05)
```

Binário: `C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe`

`RULES.md` R8 ainda cita 1.97.1. **Código/binário ganham:** a ativa agora é
1.98.0. A 1.95.0 em `C:\Users\User\.rustup` continua a instalação velha.
`RUSTC`/`CARGO` de usuário apontam para `C:\Users\User\.cargo\bin` (inexistente).
Há `rustc-wrapper = "sccache"` em `C:\Users\User\.cargo\config.toml`; o
`CARGO_HOME` do scoop persist **não** tem sccache. Fora do workspace Slim,
desligar o wrapper (`--config "build.rustc-wrapper=''"`).

Trocou de compilador → limpar `target\debug` (E0514). Exit code: `$LASTEXITCODE`
**sem** pipe PowerShell (R9).

Deploy de produto: `.\refresh-slim.ps1` tem de imprimir `OK:` depois de
qualquer patch. Esta investigação **não** rodou o script (zero patch).

### 3.2 Comandos desta sessão (EXIT=0, sem pipe)

```powershell
$tc = "C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin"
$env:RUSTC = Join-Path $tc "rustc.exe"
$env:CARGO = Join-Path $tc "cargo.exe"
$env:RUSTUP_HOME = "C:\Users\User\scoop\persist\rustup-msvc\.rustup"
$env:CARGO_HOME = "C:\Users\User\scoop\persist\rustup-msvc\.cargo"
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-msvc"
$env:PATH = "$tc;" + $env:PATH

Set-Location D:\Slim
& $env:CARGO bench -p slim-core          # EXIT=0
& $env:CARGO bench -p slim-tui --bench long_session  # EXIT=0
```

Harness extra (fora do repo, jogável):
`C:\Users\User\AppData\Local\Temp\slim-hot-paths-20260903\`

- perfil release = opt-level 3, `lto = "thin"`, `codegen-units = 1` (igual ao
  workspace Slim)
- `slim-core` via path
- N = median-of-7 + min, warmup 2; medidas one-shot de registry usam N=7 × 1
  iteração e `ToolRegistry` **fresco** quando o cache contaminaria

Não commitar esse harness. Recriar se precisar; a receita do truque está em §3.3.

### 3.3 Falsificador obrigatório do harness **[verificado]**

`ToolRegistry::execute` **rejeita path absoluto**:

```
crates/slim-core/src/tools/mod.rs
resolve_workspace_path_from_root → "absolute paths are not allowed; use a workspace-relative path"
```

A primeira leva do harness usou `PathBuf` absoluto no JSON → `success=false`,
output de **61 bytes** (a mensagem acima), ~25 µs. Isso **não** é o custo de
`read`/`search`. Números de registry em §5 são a segunda leva, com path
relativo ao `cwd` do registry e `assert!(result.success)`.

`read_file` / `read_file_range` / `search_bounded` **públicos** aceitam
absoluto. O agent loop **não** usa esses wrappers: usa `ToolRegistry` com
`ReadService`/`SearchService` persistentes.

Outro veneno: `search_bounded` e `read_file_range` públicos fazem
`SearchService::default()` / `ReadService::default()` **por chamada** — snapshot
e checkpoint morrem. Medir paginação no caminho real = `ToolRegistry` vivo.

`execute` no mesmo registry com os **mesmos** args bate no evidence cache.
Para custo de I/O, usar registry fresco ou args distintos (path/offset).

---

## 4. Mapa do código (lido nesta investigação)

Todas as âncoras abaixo foram lidas de verdade (R3). Se a linha mudou, reler
o símbolo, não copiar o número.

### 4.1 CHAMADAS — `crates/slim-core/src/runtime/mod.rs` + `provider.rs`

| Trecho | O quê |
|---|---|
| `runtime/mod.rs` `provider_tool_definitions` | schemas nativos + todo/skill/ask_question |
| `:1149` `let tools = self.provider_tool_definitions(mode)` | **uma vez por loop**, não por turno |
| `:1178–1198` | `estimate_unprepared_request_chars`; se `None`, serializa já no preflight |
| `:1427–1429` | um `prepare_messages_with_tools` se ainda não houver request |
| `:4688–4758` | estimate estrutural (walk de chars, envelope) |
| `:2057–2067` | **+1 roundtrip** em `TurnLimit \| ToolLimit \| NoProgress` com `BUDGET_FINALIZE_PROMPT` e `tools=[]` |
| `:2042–2048` | budget steer (user extra, cap `MAX_BUDGET_STEERS`) |
| `:1571–1599` | overflow retry (continue o while, nova call) |
| `provider.rs` `from_http_body` / `from_http_request_and_value` | OpenAI serializa `Value` e **não** re-parseia; default trait ainda faz `from_str` |
| `provider.rs` `serialized_chars = request.body.chars().count()` | Unicode walk do body |
| `provider.rs` `drain_sse` / `parse_sse_line` | linear; `data:` → JSON → `parse_event` |

### 4.2 TOOLS — `runtime/mod.rs` + `tools/mod.rs` + `process.rs`

| Trecho | O quê |
|---|---|
| `tool_call_is_read_only` / `tool_call_is_parallel_read` | read/list/search/`code_intel`; validation shells entram no paralelo **depois** do parse |
| `READ_ONLY_BATCH_CONCURRENCY = 8` | `buffer_unordered(8)` no segmento |
| `execute_provider_tool_batch` | prepare o lote inteiro; segmentos contíguos; mutating = 1 call |
| `prepare_provider_tool_invocations` | dedup `(name, arguments)` + `spawn_blocking` + clone na ordem |
| `execute_read_only_tool_segment` | alias de evidence; `code_intel` async; resto `spawn_blocking` |
| `tools/mod.rs` `prepare_invocations` | `canonical_workspace` 1× |
| `execute_prepared_…` `:551–553` | `names_for_mode(mode).contains` aloca `Vec` **toda execute** |
| `workspace_revision` | `AtomicU64` |
| `materialize_results` | no-op sem `artifact_store`; senão só `output > max_result_bytes` |
| `headless.rs` `Runtime::with_artifact_store` | headless liga store |
| `process.rs` `spawn_pipe_reader` | stdout/stderr drenados (PERF-03) |

### 4.3 LEITURAS — `tools/read.rs`, `search.rs`, `list.rs`, `patch.rs`

| Trecho | O quê |
|---|---|
| `read.rs` `format!("{current_line}: {line}\n")` | alloc por linha |
| `read.rs` `reader.stream_position()` no loop da página **e** no skip | syscall por linha, mesmo quando o checkpoint não grava |
| `record_checkpoint` | só se `(line-1) % 256 == 0` |
| `ReadService` | HashMap por path, 32 arquivos, 4096 checkpoints |
| `read_file_range` **público** | `ReadService::default()` por call → índice morre |
| `search.rs` offset==1 | **sempre** `search_with_walker` (governor / edits externos) |
| `search.rs` offset>1 / cursor | snapshot TTL 120 s, cap 8 |
| `search_bounded` **público** | `SearchService::default()` por call → snapshot morre |
| `bytes_contains` | scan naive byte-a-byte (prefiltro antes do UTF-8) |
| `list.rs` | mesmo padrão de snapshot que search |
| `patch.rs` `apply_exact_patch_with_content` | lê o arquivo, `text: updated` inteiro |
| `tools/mod.rs` execute_patch | `synced_text: Some(content.text)` |
| `slim-lsp` `ensure_document` | snapshot+stamp; senão `didOpen`/`didChange` |
| `document.rs` `read_capped` | path LSP, **não** usado pelo patch nativo |

### 4.4 SESSÃO / LOOP

| Trecho | O quê |
|---|---|
| `jsonl_repo.rs` `parse` | `read_to_end` + `split_inclusive` + JSON duas vezes (Value + typed) |
| `resume.rs` `preflight_session` | parse read-only |
| `headless.rs` resume | preflight **depois** open (segundo parse) |
| `repository.rs` `prepare_batch` | `let mut next = self.clone()` |
| `manual_drive.rs` `preflight` | early-exit quando todos os flags batem |
| `runtime/mod.rs` `:1134` | `conversation.clone_from` no start |
| `:2001` | `clone_from` no `RepeatedFailedTool` |
| `:2111` | `self.conversation = messages` (move) |
| `:3780` | `redact_messages` clona o slice inicial |
| `model.rs` `push_event` | `events.push(event.clone())` depois envia o original |
| `discard_projected_payloads` | zera strings; não encolhe o `Vec` |
| `slim-tui` `WrapCache` | bodies: 4096 entradas / 32 MiB / 128 KiB por body |

---

## 5. Medidas cruas desta sessão

Não copie estes números para um README de produto. Re-rode o comando.

### 5.1 Benches versionados no repo

`cargo bench -p slim-core` → `EXIT=0`

```
selector_10k_p95_ms=1.694 selector_20k_p95_ms=3.200 scaling_x=1.89
mode=auto tools=7 serialized_bytes=3469 iterations=20000 samples=11
per_turn_median_total_ns=476875900 per_turn_min_total_ns=468617700
cached_median_total_ns=22400 cached_min_total_ns=21700
per_turn_median_ns_per_iteration=23843.795 cached_median_ns_per_iteration=1.120
speedup_x=21289.1
```

Divergência vs `RESULTS.md` §2 (6 tools / 1418 B / ~9,3 µs): **hoje 7 tools /
3469 B / ~23,8 µs**. Causa **[verificado]**: `code_intel` entrou em
`definitions_for_mode`. O doc de tokens está stale no bloco de tools; a
próxima campanha `s1_read` deve mostrar T1 tools > 1592 B. Sem ação de
código nesta missão (TOK-07 continua especulativo).

`cargo bench -p slim-tui --bench long_session` → `EXIT=0`

```
blocks=3200 bytes=4968694 rows=40
scroll_locate_p95_ms=0.351 input_to_frame_p95_ms=1.106
SLIM_BODY_CACHE hits=49 misses=1 hit_rate=98.00% evictions=0 retained_bytes=99 bypasses=0
```

Gate C6: p95 ≤ 16 ms. 1,106 ms cabe folgado. WrapCache **não** é hotspot neste corpus.

### 5.2 Harness hot-path (segunda leva, paths relativos, success=true)

Fixture: arquivo 20 000 linhas × 80 chars; árvore search 200 arquivos × 80 linhas.

| Nome | N | mediana | min |
|---|---:|---:|---:|
| `read_file_range` offset 1 / 80 linhas | 40 iters × 7 | **102,7 µs** | 97,7 µs |
| `read_file_range` offset 1 / 4096 linhas | 8 × 7 | **1,724 ms** | 1,709 ms |
| `read_file_range` offset 10000 / 80 (serviço novo/call) | 40 × 7 | **2,854 ms** | 2,774 ms |
| réplica `format!` + `stream_position` / 80 | 40 × 7 | 48,9 µs | 47,5 µs |
| réplica `write!` **sem** `stream_position` / 80 | 40 × 7 | 23,9 µs | 23,1 µs |
| réplica `format!` + `stream_position` / 4096 | 8 × 7 | **1,767 ms** | 1,752 ms |
| réplica `write!` **sem** `stream_position` / 4096 | 8 × 7 | **0,339 ms** | 0,316 ms |
| registry read 80, registry fresco | 8 × 7 | 122,7 µs | 120,9 µs |
| registry read 80, 48 arquivos distintos | 48 × 7 | 71,0 µs | 70,7 µs |
| registry read 80, evidence cache hit | 40 × 7 | 63,7 µs | 61,1 µs |
| registry read offset 10000 **first** (8092 B, success) | 7 × 1 | **2,994 ms** | 2,930 ms |
| registry read offset 10000 **repeat mesmos args** | 7 × 1 | 97,1 µs | 82,1 µs |
| `search_bounded` offset 1 (serviço novo) | 8 × 7 | 7,90 ms | 7,35 ms |
| `search_bounded` offset 2 (serviço novo) | 40 × 7 | 7,95 ms | 7,57 ms |
| registry search offset 1, registry fresco | 7 × 1 | **8,10 ms** | 7,68 ms |
| registry search offset 2 após offset 1 no **mesmo** registry | 7 × 1 | **0,111 ms** | 0,099 ms |
| `list_directory` 200 | 40 × 7 | 132 µs | 130 µs |
| `prepare_messages_with_tools` 8 turns, body 14 712 B | 80 × 7 | **131 µs** | 124 µs |
| `body.chars().count()` | 2000 × 7 | 399 ns | 398 ns |
| `body.len()` | 20000 × 7 | 0,2 ns | 0,2 ns |
| `estimate_json_string_chars` no body | 400 × 7 | 9,9 µs | 9,6 µs |
| `serde_json::to_string` | 80 × 7 | 8,3 µs | 7,9 µs |
| `serde_json::from_str` | 80 × 7 | 27,3 µs | 25,9 µs |
| hex `write!("{byte:02x}")` 32 B | 20000 × 7 | 273 ns | 259 ns |
| hex tabela 32 B | 20000 × 7 | 43,5 ns | 42,0 ns |
| `tokio::spawn_blocking` noop | 400 × 7 | 1,04 µs | 0,60 µs (ruído; outro run chegou a 4 µs) |
| `names_for_mode().contains("read")` | 20000 × 7 | 64 ns | 63 ns |
| `definitions_for_mode` + serialize | 2000 × 7 | 24,4 µs | 22,5 µs |
| `JsonlRepo::open` 200 entries | 8 × 7 | 500 µs | 472 µs |
| `preflight_session` 200 entries | 8 × 7 | 425 µs | 413 µs |

Interpretação rápida **[verificado]**:

- Slim 4096-line (1,72 ms) ≈ réplica com `format!`+`stream_pos` (1,77 ms). O
  custo da página grande **é** esse loop.
- Réplica sem `stream_pos` + `write!`: 0,34 ms → **5,2×** na página 4096.
- Página default 80: ~100 µs. O mesmo patch economiza ~25–80 µs — abaixo de
  JND humano e do ruído de um roundtrip.
- Search offset=1 no loop: **8,1 ms / 200 arquivos**. Offset=2 no registry
  vivo: **0,11 ms (73×)**. Público `search_bounded` offset=2 **não** ganha
  (serviço novo).
- Repeat offset 10000 no mesmo registry (97 µs) é **evidence cache** (mesmos
  args), **não** prova isolada de seek por checkpoint.
- Prepare local 131 µs. `chars().count` 399 ns. Roundtrip de provider **não
  foi medido** nesta sessão — não use 1208 ms de `RESULTS.md` como RTT atual.
- Resume preflight+open ≈ 0,93 ms / 200 records.

---

## 6. Tabela priorizada (SPD)

| ID | Hot path + prevalência | Evidência | Economia esperada | Esforço | Risco | Veredito |
|---|---|---|---|---|---|---|
| **SPD-READ-01** | `read` página grande: `format!` + `stream_position` **por linha**. Default 80; cap 4096. Cada `read` do modelo. | §5.2 (pré-patch): Slim 4096 = 1,724 ms. Re-medida 2026-09-04: 1,738 → **0,299 ms (5,81×)**. 80: 92,5 → 63,4 µs. | ~1,4 ms/página 4096. Default 80: dezenas de µs. | baixo | médio-baixo: footer/UTF-8/offset. Teste `high_offsets_reuse_incremental_checkpoints`. | **feito** 2026-09-04. |
| **SPD-READ-02** | offset alto sem `ReadService` persistente. API pública recria o serviço. Loop usa registry. | Público skip 10000: 2,85 ms. Registry first: 2,99 ms. Repeat mesmos args: 97 µs = cache, não seek. | First page alta ~3 ms. Seguintes no loop já baratas se fingerprint bate. | — | — | **adiar**. Checkpoint já existe no registry. Não vender seek com o número de cache. |
| **SPD-SEARCH-01** | offset=1 sempre rescan (`search.rs`). Snapshot só com serviço vivo. | Registry offset1 8,10 ms → offset2 0,111 ms (**73×**). Público offset2 continua ~8 ms. | Snapshot já entrega o ganho de paginação. Pular rescan de offset=1 muda o governor. | — | alto se pular rescan | **descartar** mudar offset=1. Snapshot no loop **já feito**. |
| **SPD-CALL-01** | prepare/serialize/chars 1× por turno. | 131 µs prepare; 399 ns `chars().count`; OpenAI não re-parseia. | Trocar `chars().count` por `len`: 0,4 µs. | baixo | baixo (estimate usa chars) | **descartar**. Some no RTT. |
| **SPD-CALL-02** | +1 call no finalize; steer; overflow retry. | Código §4.1. Prevalência em tarefas reais **não medida** aqui. | 1 RTT quando dispara. | médio | **quebra comportamento** | **descartar** (restrição 1). = S1 do irmão. |
| **SPD-TOOL-01** | `names_for_mode().contains` por execute. | 64 ns vs read 71 µs. | dezenas de ns | baixo | baixo | **descartar**. |
| **SPD-TOOL-02** | `spawn_blocking` por tool read-only. | noop ~1 µs vs search 8 ms. | <0,02% | — | — | **descartar**. |
| **SPD-TOOL-03** | hex `write!` + `FastStamp::digest` 1×/arquivo no search. | 273 vs 43 ns; ×200 arquivos ≈ 46 µs vs 8,1 ms (0,6%). | <50 µs/search médio | baixo | baixo | **adiar**. Some no I/O. |
| **SPD-SESS-01** | resume parseia JSONL duas vezes; `prepare_batch` clona validator. | open 500 µs + preflight 425 µs / 200 records. Clone O(n) **[hipótese]** sem bench 10k. | ~0,4 ms se reusar parse. 10k records **[hipótese]** ~25 ms. | médio | alto (TOCTOU resume) | **adiar**. = S2 do irmão. Não é hot path de call. |
| **SPD-TUI-01** | WrapCache bodies cap 4096. | long_session p95 1,106 ms; 98% hit; 0 evictions. | nenhuma no gate | — | — | **descartar** como hotspot atual. |
| **SPD-LOOP-01** | clones residuais (`clone_from` start + RepeatedFailedTool; redact inicial; `push_event` clona). | lido; custo de clone **não isolado**. | **[hipótese]** ns–µs/turno | baixo | baixo | **adiar**. = S7 do irmão. |
| **SPD-PATCH-01** | `synced_text` retém o arquivo pós-patch. | código lido; heap **não medido**. | 1 alloc O(arquivo)/write | baixo | médio (governor/LSP) | **adiar**. = S3 do irmão. Medir 1 MiB antes. |
| **SPD-INTEL-01** | `code_intel` “serial”? | `name == "code_intel"` entra no segmento paralelo. `ensure_document` não medi (precisa server). | — | — | — | **descartar** a barreira (já paralelo). LSP first-touch: adiar sem medida. = S5. |
| TOK-02/06/07 | tokens, não latência local | backlog TOK | tokens; TOK-07 pode **adicionar turno** | alto | qualidade | **adiar**. TOK-07 só com `run_benchmark.ps1 -VariantTag`. = S9. |

Achado principal, em uma frase: o CPU local de uma **call** (~0,13 ms) e de um
**read de 80 linhas** (agora 63 µs) é ruído ao lado do provider; o único custo
local grande recorrente medido é **search com rescan** (~8 ms / 200 arquivos),
já mitigado na paginação; **SPD-READ-01** (páginas grandes) está feito:
4096 linhas 1,738 ms → 0,299 ms (5,81×).

---

## 7. Receita — SPD-READ-01 (**feito** 2026-09-04)

Arquivo único: `crates/slim-core/src/tools/read.rs`.

Hoje, no loop da página (e o análogo no skip até `start_line`):

- `writeln!(&mut output, "{current_line}: {line}")` (o check de
  `MAX_READ_PAGE_BYTES` via `decimal_digits` permanece).
- Offset de checkpoint = `checkpoint.offset + bytes_read` após o `seek`
  inicial. **Não** chama `stream_position` por linha. `record_checkpoint`
  continua só a cada 256 linhas.
- Footer de paginação, CRLF, UTF-8 inválido, cancel, cap de 1 MiB/página:
  inalterados (testes existentes verdes).

Não mudou: schema `offset`/`max_lines`, texto do footer, `DEFAULT_MAX_READ_LINES=80`,
`MAX_READ_LINES_CAP=4096`, API `read_file` / `read_file_range`.

### 7.1 Before/after **[verificado]** 2026-09-04

Mesmo harness `%TEMP%\slim-hot-paths-20260903` (`--read`), release
opt-level 3 / `lto=thin` / `codegen-units=1`, N = median-of-7 + min, warmup 2.
Fixture: 20 000 linhas × 80 chars.

| Nome | Antes (mediana / min) | Depois (mediana / min) | Δ |
|---|---|---|---|
| `read_file_range` offset 1 / 80 | 92,5 µs / 91,1 µs | **63,4 µs / 62,2 µs** | 1,46×, não-piora |
| `read_file_range` offset 1 / 4096 | 1,738 ms / 1,727 ms | **0,299 ms / 0,297 ms** | **5,81×** |
| `read_file_range` offset 10000 / 80 (skip sem índice) | 2,811 ms / 2,761 ms | **0,391 ms / 0,391 ms** | 7,19× (mesmo patch: skip também parou o `stream_position`) |
| réplica `write!` sem `stream_pos` / 4096 | 0,236 ms | 0,301 ms | ruído entre runs; Slim patched **empata** a réplica (0,299 vs 0,301) |

Aceite da receita: 4096 não piorou e caiu para ~0,3 ms; 80 não piorou
(melhorou ~31%, acima do ruído de 10%).

Testes existentes (sem teste novo): `high_offsets_reuse_incremental_checkpoints`;
`read_range_paginates_and_appends_offset_footer_when_truncated`;
`read_range_matches_lines_semantics_for_crlf_and_past_eof`;
`read_rejects_a_page_that_exceeds_the_byte_budget`.

Gate: `cargo test --workspace` EXIT=0, 87 suítes / **1093 passed** / 0 failed /
1 ignored (ConPTY). Clippy `slim-core --all-targets -D warnings` EXIT=0.
`.\refresh-slim.ps1` imprimiu `OK:`.

---

## 8. Deliberadamente não tocado

- Loop / `ProviderAdapter` / sistema de eventos — anti-goal.
- `chars().count`, `names_for_mode`, `spawn_blocking`, tabela hex — medidos;
  ganho some no N real ou é <1% do hot path.
- Rescan search offset=1 — contrato do governor (edits externos).
- Extra roundtrip de finalize/steer — mudança funcional.
- JSONL parse duplo no resume — TOCTOU; ~1 ms/200 records.
- WrapCache 4096 — gate TUI 1,1 ms p95.
- TOK-02/06/07, tokenizer, response cache, reuse `ProviderCache`.
- `code_intel` / `ensure_document` — sem servidor LSP nesta sessão.
- Isolation checkpoint vs evidence cache em offset alto — número não fechado.
- Árvore Slim real no `search` (só fixture 200 arquivos).
- RTT de provider / nº de roundtrips por tarefa real.
- `cargo test --workspace` / `refresh-slim.ps1` — zero patch de produto.
- Harness em `%TEMP%` — não pertence ao repo.

Árvore Git na investigação: sujeira pré-existente (ToolLimit, `loop_guard`,
`manual_drive`, docs). Não misturar com SPD-READ-01.

---

## 9. Incertezas

- RTT real e quantos % das tarefas disparam finalize/steer: **não medido**.
- Search 8,1 ms é fixture 200×80, não `D:\Slim`. Tree maior → rescan maior e
  snapshot de offset>1 também (mais I/O evitado).
- Evidence cache de read 80: 64 µs vs distinct 71 µs — prepare/path dominam a
  página default, não o disco.
- `spawn_blocking` noop oscilou 0,60–4 µs entre runs.
- Clone O(n) do histórico longo: selector medido; clone de `messages` não.
- rustc desta máquina: **1.98.0**, não 1.97.1 das regras.
- Irmão `SUGESTOES-PERFORMANCE-2026-09-04.md` mediu `tool_setup` 23 357 ns e
  selector 1,44 / 2,92 ms. Esta sessão: 23 844 ns e 1,69 / 3,20 ms. Mesma
  ordem de grandeza; **não misturar** as duas linhas como um único baseline.

---

## 10. Checklist para o próximo agente

```
[x] Li RULES.md e AGENTS.md
[x] Não reabrir PERF-01..07 / paralelo read-only / prepare batch / SSE linear
[x] Não implementar TOK-07 sem campanha A/B
[x] Não remover o finalize roundtrip nem o rescan offset=1
[x] SPD-READ-01 feito 2026-09-04 — não remexer em read.rs sem regressão medida
[x] Medido 80 e 4096, suíte, refresh-slim (§7.1)
[ ] Harness de ToolRegistry usa path RELATIVO e assert success (ainda vale)
[ ] Números de §5 são da sessão 2026-09-03; §7.1 é o after do patch
```
