# Auditoria estática — 20 agentes em paralelo (bugs + otimizações)

**Data:** 2026-11-13 · **Método:** 20 subagentes read-only com o mesmo prompt de
auditoria sobre o workspace Rust (~190k linhas: `slim-core` 94k, `slim-tui` 43k,
`slim-cli` 40k, `slim-lsp` 11k). Achados deduplicados por arquivo:linha +
descrição; contradições entre agentes resolvidas por leitura direta do código.
**Nenhum teste, build ou benchmark foi executado** — análise 100% estática;
severidades são estimativas de código, não medições.

> Registro histórico datado. Não substitui RULES.md nem os contratos em
> `docs/` — revalide cada achado no código antes de agir.

---

## Resumo

- **~21 bugs confirmados**: 2 ALTA, ~7 MÉDIA, restante BAIXA.
- **~42 otimizações** após dedup, agrupadas por subsistema.
- O código é consistentemente defensivo: limites de bytes em toda fronteira,
  `spawn_blocking` nas tools nativas, publicação atômica com backup,
  redação de segredos em streaming, `unsafe` justificado. Os achados
  concentram-se em poucos pontos quentes (sessão durável, shell jobs,
  pipeline de apresentação, cancelamento).

---

## Bugs confirmados

### ALTA

#### B1. Lost-wakeup em `CancellationToken` — cancelamento pode travar para sempre
`crates/slim-core/src/runtime/cancellation.rs:61-67, 94-102, 106-116`
*(reportado por 5 agentes; verificado por leitura direta)*

`cancelled()`, `wait_for_foreground_work()` e `wait_for_native_work()` criam
`notified()` mas nunca chamam `enable()` antes de checar a condição.
`Notify::notify_waiters()` (linhas 29 e 48) **não armazena permissão**: se o
notify disparar entre o check (`is_cancelled()` / contadores) e o primeiro
poll do `Notified`, o wakeup se perde e o await pende **permanentemente**.
Agravante: `cancel()` só notifica na primeira chamada (`swap` retorna true
uma vez) e `NativeWorkGuard::drop` só notifica na transição 1→0 — não há
re-notificação. O padrão correto (`pin!` + `enable()` antes do check) já
existe no próprio codebase: `mcp/spec.rs` (`McpCancellation::cancelled`),
`mcp/stdio.rs` (`wait_cleanup`), `slim-lsp/transport.rs`. Blast radius:
`cancelled_or_pending` alimenta `select!`s do agent loop, provider stream,
codemode e recovery; `wait_for_native_work` é awaited no fim de batches e no
ack de cancelamento. **Fix: ~3 linhas por função.**

#### B2. `shell_job input` faz `write_all` bloqueante no executor Tokio
`crates/slim-core/src/runtime/shell_jobs.rs:743-776` (callers `:1346-1354`,
`tool_batch.rs:1038-1042`)
*(4 agentes)*

`ShellJobs::input` executa `stdin.write_all(text).and_then(flush)` —
I/O síncrono em `ChildStdin`, até 64 KiB — **direto na task async** do agent
loop: sem `spawn_blocking`, sem timeout, sem checagem de cancelamento e fora
de `track_native_work`. Filho que não drena stdin → pipe enche → o worker do
executor bloqueia indefinidamente; a task nunca cede a `.await`, então
cancelamento não é observado; o mutex `stdin_slot` fica retido e o caminho de
conclusão do worker (`*lock_mutex(slot) = None` sob o mutex do mapa de jobs)
pode congelar o mapa inteiro; `shutdown()` (`while running { wait().await }`,
sem timeout) pode girar para sempre. Única recuperação: matar o processo.
**Fix: `spawn_blocking` + `track_native_work`, ou writer dedicado com canal
limitado / `poll` + deadline.**

### MÉDIA

#### B3. `verify_opened_path` é no-op no macOS (TOCTOU)
`crates/slim-core/src/tools/execution.rs:2321-2331`
*(3 agentes)*

O fallback `#[cfg(all(unix, not(any(target_os="android", target_os="linux")))))]`
— que cobre **macOS**, plataforma primária — ignora o fd (`_file`) e só faz
`canonicalize(expected)? == expected`, trivialmente verdade para path já
canônico. Linux usa `/proc/self/fd/N`; Windows usa
`GetFinalPathNameByHandleW`. Troca de componente para symlink entre
validação e `open` não é detectada — a defesa-em-profundidade documentada
pela mensagem de erro é silenciosamente ausente no alvo principal.
Sugestão: `fcntl(fd, F_GETPATH, ...)` no macOS.

#### B4. Publicação falha permanentemente em filesystems sem hard link
`crates/slim-core/src/tools/write.rs:588-592` (`publish_new`),
`:644-652` (`publish_replacing`, ramo Unix) e
`crates/slim-core/src/session/jsonl_repo.rs:674-681` (`publish_temp_file`)
*(3 agentes)*

`fs::hard_link` incondicional, sem fallback para `rename`: exFAT/FAT32
(pendrives SD/extras no macOS), alguns mounts de rede/FUSE → **todo**
`write`/`patch` e publish de snapshots/compaction falham. Em
`publish_replacing` a ordem `rename(target→backup)` → `hard_link` ainda
deixa o alvo ausente quando o `hard_link` falha (reportado como
`effects_uncertain`). No Windows o caminho usa `SetFileInformationByHandle`
(rename) — portabilidade assimétrica.

#### B5. Leitura não limitada entre `metadata()` e `read_to_end`
`crates/slim-core/src/tools/write.rs:409-426`
*(1 agente)*

`read_existing_file_observed` checa `metadata.len() > MAX_MUTATING_FILE_BYTES`
e só depois faz `file.read_to_end(&mut bytes)`. Arquivo que cresce na janela
(escritor concorrente — jobs de shell gravam logs enquanto o modelo escreve)
produz `Vec` ilimitado. `try_lock` é advisory (só cooperantes). Janela
estreita, consequência = alocação não limitada.

#### B6. Log de job em disco cresce sem teto
`crates/slim-core/src/runtime/shell_jobs.rs:362-398` +
`crates/slim-core/src/context/artifacts.rs:163` (`create_job_log`)
*(1 agente)*

`memory_bytes` (máx. 16 MiB) limita apenas o `tail` em memória e o carry de
redação; uma vez `tail.len() + text.len() > cap/2`, **todo** chunk é anexado
via `file.write_all` sem teto total de bytes nem TTL para o job. Job em
background com saída contínua (`tail -f`, dev server, `yes`, build verboso)
exaure o disco. `ArtifactStore` não impõe quota.

#### B7. Executável relativo de MCP validado contra o CWD errado
`crates/slim-core/src/process.rs:124-134` via `mcp/stdio.rs:504-513`
*(1 agente)*

`has_path_component` trata `./server`, `bin/foo`, `../x` como caminho
explícito; `resolve_explicit_path` chama `is_executable_file` →
`path.metadata()` que resolve contra o **CWD do processo Slim**, não contra
`request.cwd`. Depois `Command::new(program).current_dir(&request.cwd)`
executa o mesmo path relativo contra o **CWD do filho**. Dois modos de
falha para `command: "./servidor"` em config MCP stdio: falso "executable
not found" ou validação de arquivo diferente do executado.

#### B8. `read` retorna UTF-16/UTF-32 sem BOM com NULs intercalados
`crates/slim-core/src/tools/read.rs:644-678`
*(1 agente)*

`read_utf8_line` valida apenas UTF-8 via `str::from_utf8`. Arquivos UTF-16/32
**sem BOM** são sequências de bytes válidas em UTF-8 (NULs intercalados) → a
tool retorna texto com `\0` silenciosamente. A detecção de BOM
(`non_utf8_message`, `:577-603`) só roda quando `from_utf8` falha. Inconsistente
com `search.rs:1250`, que descarta arquivos com `0x00` como binários.
Correção natural: NUL-check no primeiro buffer.

#### B9. `run_observed` espera ambos os pipe readers — descendente vivo trava o run
`crates/slim-core/src/process.rs:511-515`
*(2 agentes — um classificou como trade-off documentado)*

Só retorna quando `stdout_reader` **e** `stderr_reader` terminam (EOF/erro).
Filho que sai mas deixa neto herdando stdout/stderr (`sleep 600 &`, `nohup`,
daemon) nunca dá EOF → run bloqueia até `request.timeout` (default shell:
600 s, máx. 1 h) e `terminate_process_tree` mata o grupo inteiro — para um
comando que completou instantaneamente, reportando `timed_out` e matando o
background que o usuário queria. Doc da tool diz "do not detach", mas `cmd &`
em script é erro comum de modelo.

#### B10. `FileNameLength` inclui o terminador NUL no publish Windows
`crates/slim-core/src/session/jsonl_repo.rs:625-631`
*(1 agente — somente Windows)*

`wide_name` faz `.chain(Some(0))` e `file_name_length = wide_name.len() * 2`
conta os 2 bytes do NUL — contradiz o próprio comentário ("excludes it") e a
doc Microsoft (`FileNameLength` = tamanho sem terminador obrigatório).
`SetFileInformationByHandle(FileRenameInfo)` pode rejeitar o rename →
criação do session file durável falha no Windows. **NÃO CONFIRMADO** se falha
em todos os SKUs (o campo só é usado em alguns code paths; NTFS moderno pode
ignorar em favor da string terminada). Valor objetivamente errado — testar em
Windows real.

### BAIXA

| # | Local | Achado |
|---|-------|--------|
| B11 | `process.rs:648-655` | `UnixTree::drop`: `unregister` **antes** do `kill(-pgid, SIGKILL)` abre janela em que sinal fatal ao Slim não encontra o grupo na tabela → descendentes órfãos; e SIGKILL incondicional ao grupo pode atingir PGID reutilizado por líder inocente |
| B12 | `mcp/stdio.rs:997, 822` | `outbound.try_send(...).let _ =` descarta **silenciosamente** respostas a requests server→client (ping, roots/list) e notificações quando a fila (cap 256) está cheia — servidor espera até o próprio timeout |
| B13 | `mcp/sse.rs:86-96, 141` | Decoder SSE só reconhece `\n`: linhas terminadas em `\r` puro nunca despacham → buffer cresce até 16 MiB e a conexão morre com `too_large`; `event:` vazio é dropado em vez de despachar como `"message"` (spec WHATWG) |
| B14 | `mcp/http.rs:1192-1204` | `classify_inbound` só aceita resposta com `id.as_u64() == expected` — servidor que ecoa id como string tem a resposta classificada como `Other` e ignorada até timeout |
| B15 | `write.rs:648` (`publish_replacing` Unix) | `fs::rename(target, backup)` sobrescreve silenciosamente um `.displaced` preexistente de publicação anterior falha — destrói a única cópia do conteúdo deslocado (janela estreita, exige resíduo de crash) |
| B16 | `write.rs:395-408` | `try_lock` + `sleep(10ms)` em loop **sem deadline próprio** — só `check_cancellation` interrompe; holder vivo travado segura a tool até cancelamento manual |
| B17 | `runtime/events.rs:26-29` | `push_runtime_event` emite o evento ao app e consome o seq **antes** de `persistence?` — falha de append no journal deixa evento na UI/memória sem registro durável → divergência no resume |
| B18 | `runtime/events.rs:136-142` | `lock_journal` propaga `PoisonError` como erro fatal (`"durable run lock poisoned"`) em vez de `into_inner()` como `lock_mutex` — panic transitório envenena o mutex e mata a sessão permanentemente (talvez fail-closed deliberado na camada de durabilidade) |
| B19 | `runtime/history_elision.rs:219` | `turn_calls.insert(call.id, key)` sobrescreve `tool_call.id` duplicado no mesmo turno assistant — providers "reciclam ids curtos"; se dois calls têm mesmo id e paths diferentes, `plan_elisions` pode elidir a saída do call errado → perda de evidência enviada ao provider |
| B20 | `tools/search.rs:1396-1410` | `hit_text` calculado uma vez por linha ancorado no match **mais antigo** entre todos os padrões — linha >~8 KiB com matches de padrões diferentes separados além da janela ±256 B exibe trecho que não contém o próprio match |
| B21 | `tools/search.rs:1630-1650` | `find_ignoring_case`: o teste `matched == needle.len()` só ocorre após consumir expansões de `to_lowercase()` — padrão terminando no meio de expansão multi-char (`İ` → `i`+`\u{307}`) não é detectado; afeta só a nota consultiva `note_case_variants` |
| B22 | `mcp/stdio.rs:953` | Checagem de tamanho soma `pending_bytes() + read` (chunk inteiro de até 8 KiB) em vez de bytes até o `\n` — mensagem que completaria dentro do limite de 16 MiB é rejeitada se o chunk cruzar o teto |
| B23 | `mcp/stdio.rs:916-920` | Writer thread faz `write_all`/`flush` bloqueante no stdin do servidor MCP; se o servidor não drenar, a thread fica presa dentro da syscall — `close_once` não a interrompe (vazamento de thread + `ChildStdin` por conexão) |
| B24 | `slim-tui/clipboard.rs:300-304` | `pbpaste .output()` lê o pasteboard inteiro sem cap de bytes nem timeout — clipboard patológico (centenas de MB) → pico de memória; `pbpaste` travado deixa a thread dedicada pendurada |
| B25 | `session/event_log.rs:152` | Nome do lock file via `format!("{}.lock", path.display())` — `display()` é lossy; paths não-UTF8 distintos colidem no mesmo nome → falso `ResourceBusy`/"session is already in use" |
| B26 | `session/jsonl_repo.rs:582-597` | `create_temp_file` tenta `create_new` infinitamente em `AlreadyExists` sem backoff nem `yield` — colisões patológicas fazem spin de syscalls (espaço de sufixos é na prática infinito; higiene apenas) |
| B27 | `tools/write.rs:363-368` | `current_file_matches` compara stamp calculado sobre o **fd já aberto** — rename atômico de terceiro entre observação e publish não é detectado; limitação documentada ("best-effort preconditions, not compare-and-swap"), sem perda silenciosa pois backup preserva a versão deslocada |

---

## Otimizações (deduplicadas)

### Assintóticas / caminho quente

#### O1. `DurableAppendValidator::prepare_batch` clona o estado inteiro por append → O(n²)
`crates/slim-core/src/session/repository.rs:176` via `jsonl_repo.rs:295`
*(5 agentes — maior consenso)*

`let mut next = self.clone()` clona EntryIndex (BTreeMap por entrada),
lifecycle, AttemptLedger (4 BTreeMaps), DurableQueue e ToolPhaseLedger
(~1 alocação por nó de BTreeMap) **a cada registro durável**. Custo total da
sessão: O(n²) em clones — sessões longas (dezenas de milhares de records,
limite 64 MiB) pagam caro. `commit` descarta o original depois. Fix:
aplicação in-place com rollback/undo, clone-on-write das estruturas
tocadas, ou `append_batch` único por operação.

#### O2. Pipeline de apresentação re-clona e re-estima a conversa ~10× por batch
`crates/slim-core/src/runtime/presentation.rs:292, 411-431` +
`runtime/request_estimate.rs`
*(3 agentes)*

`largest_fitting_scale` faz busca binária (~10 iterações); cada
`request_fits` clona `plan.base_messages.to_vec()` — deep clone de **toda a
conversa** (Strings, tool_calls, content_blocks) — e depois estima
estruturalmente ou serializa o request inteiro. Por batch de tools:
~10 clones profundos + ~10 estimativas/serializações. Pior para adapters sem
`request_envelope_upper_bound_chars` (trait default `None`; só
OpenAI/Anthropic retornam `Some(1024)` — **`provider/devin.rs` é o único
adapter sem override**, pagando ~10 `prepare` protobuf Connect completos por
batch). Fix: estimativa incremental sobre o histórico, ou bound conservador
por adapter.

#### O3. `tool_output_already_in_context` varre todo o histórico por resultado
`crates/slim-core/src/runtime/history_elision.rs:19-24`
*(5 agentes)*

`messages.iter().rev().any(|m| m.content == output)` por resultado de tool —
chamada 2× por call em `record_batch_results` (`tool_batch.rs:271-278`) e até
~20× por batch via `present_call` dentro do loop de escala. Complexidade
efetiva O(escala × calls × mensagens) por batch. Fix: `HashSet<(tool_name,
hash/len)>` de outputs já emitidos, computado uma vez por batch/run.

#### O4. `JobOutput::append`/`publish` — pipeline pesada sob locks, por chunk de 16 KB
`crates/slim-core/src/runtime/shell_jobs.rs:171-398, 977-999` +
`crates/slim-core/src/process.rs:994-1004`
*(6 agentes — maior cluster de otimização)*

O callback de cada chunk de pipe executa `state.jobs()` (mutex global) e,
dentro dele: `pending.to_ascii_lowercase()` (cópia do buffer), rebuild de
`names: Vec<String>` via `format!("{s}:")` **por iteração do loop**,
`lower.rfind(name)` por nome sensível, `file.seek(End)+write_all` por unidade
publicada (sem BufWriter — 2 syscalls/linha), `tail.drain(..remove)` memmove
O(cap/2) por linha, `preview` = `tail.lines().last()` re-varre o tail inteiro
O(tail) **por chunk** (~quadrático no volume), `normalize_shell_text(&tail)`
idem (`:956`). Casos patológicos: saída single-line de ~1 MB → dezenas de
scans de ~1 MB por append (O(n²)); `json_value` re-parseia o pending inteiro
por append (O(n²) de parse). Adicional: `read_pipe` chama o observer
**enquanto segura** `Mutex<PipeProgress>` (`process.rs:994`) → stdout e stderr
serializam um ao outro por chunk e `snapshot()` de progresso bloqueia atrás
de I/O de disco. E `read_window` (`shell_jobs.rs:430-436`) faz I/O de arquivo
síncrono sob o mesmo mutex. Fix: coletar dados sob lock e chamar observer
depois; BufWriter; preview incremental (`rsplit('\n')`); names pré-computados;
scan de segredos incremental.

#### O5. `record_batch_results` faz 3 varreduras dos eventos do batch por call
`crates/slim-core/src/runtime/tool_batch.rs:259-263, 334-338, 351-370`
*(5 agentes)*

Por `(call, result)`: `.iter().any(...)` para `mutation_changed`, outro para
`volatile_boundary`, `find_map` para `causal_identity` — tudo sobre
`self.app.events()[event_start..]` (que cresce com `ToolProgress` por KiB).
O(calls × eventos) ~quadrático no batch. Fix: um índice `call_id →
fingerprints` construído uma vez por batch.

#### O6. `fit_count` re-renderiza o resultado inteiro ~log₂(n) vezes
`crates/slim-core/src/tools/execution.rs:221-233, 268-271, 295-322`
*(4 agentes)*

Busca binária do "maior prefixo que cabe no budget": cada sonda chama
`render(count)` que realoca e recopia o prefixo inteiro
(`records[..count].concat()`, `lines.join`, `page.clone()` com até 500 hits)
— ~9-17 renders × até ~1 MiB por apresentação oversized. Fix: soma de
tamanhos por record (já são `String`) + um único render final.

#### O7. `JsonlRepo::parse` — double-parse por linha + arquivo inteiro em memória
`crates/slim-core/src/session/jsonl_repo.rs:344-405`
*(5 agentes)*

`file.read_to_end` de até 64 MiB e, por linha, `serde_json::from_slice::<Value>`
(árvore DOM completa) + `from_value::<DurableRecord>` — ~2× custo de parse e
alocações; `records` retém todo o histórico parseado pela sessão.
Caminho frio (uma vez por open) mas doeu no resume de sessões grandes.
Fix: probe mínimo do campo `type` (ou enum `#[serde(tag)]` espelho) +
`from_slice` direto em `DurableRecord`; considerar `StreamDeserializer`.

#### O8. Streaming markdown do TUI degrada para reparse O(n) por frame
`crates/slim-tui/src/render.rs:1435-1518` + `markdown.rs:512-548`
*(2 agentes)*

`stable_prefix_len` só avança dentro de `if fence.is_none() &&
previous_blank && opens_block`; para parágrafo único longo / texto corrido
sem linhas em branco (comum em respostas narrativas), `advance == 0` →
`project_markdown` re-executa sanitize + pulldown-cmark sobre **todo** o texto
acumulado a cada frame → O(n²) no stream. Além disso `streaming_body`
(`render.rs:1435-1439`) faz `prefix.wrapped…lines.clone()` — deep clone de
todas as `Line`/`Span` do prefixo estável por frame; e o cache `projections`
é indexado por `content_generation` (muda a cada delta → miss garantido).

### Médias / moderadas

| # | Local | Achado |
|---|-------|--------|
| O9 | `tools/search.rs:1291-1313` | `context_lines>0` aloca `String` (`to_owned`) + `Arc` **por linha varrida** mesmo em arquivos sem hit — materializar contexto sob demanda (ring buffer de offsets) |
| O10 | `tools/search.rs:1342-1344, 1761-1779` | Prefilter por linha: até N padrões × substring search ingênua byte-a-byte (`position`+memcmp) antes da validação UTF-8 — `memchr::memmem`/Two-Way no loop mais quente; pior caso O(n·m) por padrão (limitado por 64 MiB/scan) |
| O11 | `tools/list.rs:265` | `entry.is_dir()` = syscall `stat` por entrada **por página** (até 500), repetido em snapshots imutáveis de 120 s — `DirEntry::file_type()` (d_type, ~grátis) foi descartado em `collect_directory`; cachear no snapshot (atenção: `is_dir` segue symlinks, `file_type` não — semântica de symlink-para-dir muda) |
| O12 | `tools/read.rs:410-430, 303-309` | `index_for` clona `ReadIndex` inteiro (checkpoints até 4096, ~64 KB) por leitura + `merge_checkpoints` re-ordena o vec completo por página — `Arc`/`RwLock`; lookahead de paginação usa `read_until` para testar 1 byte (linha gigante = MiBs lidos à toa) |
| O13 | `tools/mod.rs:1577-1583` + `shell.rs:333-377` | stderr de comando com exit≠0 passa por `from_utf8_lossy` + `normalize_shell_text` (String com cap = len + scan char-a-char) até **3×** por chamada (checagem de vazio + exibição + nota PowerShell) — reutilizar o `cleaned` já produzido ou testar bytes crus |
| O14 | `tools/shell.rs:409-444` | `remove_wrapped` janela deslizante O(n×m) por chamada de normalize (2× quando texto contém `"$utf8"`/`"= 'utf8'; "`) — skip no primeiro char de `wanted` |
| O15 | `runtime/request_estimate.rs:44-133` + `context/pi_compaction/estimate.rs:25-36,196-200` | Estimativa de tokens re-varre todo o histórico (bytes/chars de cada campo string) por turno sem âncora de uso — prefix sums ou cache por mensagem invalidado só em append |
| O16 | `session/jsonl_repo.rs:321-323` + `runtime/events.rs:151-190` | Appends do journal executam `write_all` + `sync_data`/`barrier_sync` (fdatasync real no Linux) **dentro de funções async** (`persist_provider_call`, `persist_tool_batch`, `persist_provider_validation_failure`, `record_event`) — bloqueia worker Tokio por record; `spawn_blocking` isola a latência |
| O17 | `process.rs:994-1004` | `String::from_utf8_lossy(&chunk)` materializa cópia do chunk de 16 KB só para extrair a última linha do preview — scan reverso por `\n` evita ~2× memcpy/chunk (~12 mil chunks para output de 100 MB) |
| O18 | `mcp/client.rs:317-340` | Cada `notifications/message` MCP faz open+write+close síncrono em `std::fs` por linha de log, no caminho async do transporte HTTP — writer persistente ou `spawn_blocking` |
| O19 | `session/event_log.rs` + `slim-cli/headless.rs:100-110` | Hash do próprio binário (`executable_identity`) faz `fs::read` do arquivo inteiro p/ memória (binário LTO = dezenas de MB) — `BufReader`+`Sha256::update` em blocos, O(1) memória (OnceLock, 1×/processo) |
| O20 | `runtime/tool_schedule.rs:155-188` | `phase1_snapshot_indices_ready` faz `pending_mutations.iter().any(dep)` por índice → O(N×M), re-executada após cada mutação via `anticipate_ready_snapshots` → O(batch²) |
| O21 | `runtime/history_elision.rs:206-221` | `plan_elisions` executa `serde_json::from_str::<PathOnly>` sobre arguments **inteiros** de cada call (exceto `shell`), inclusive nomes que jamais elidem (`list`, `search`, `codemode`, `mcp.*`) — filtrar por `call.name` antes de parsear |
| O22 | `runtime/agent_loop.rs:1521-1555` | `plan_tool_presentations` executa **duas vezes** por batch com artifacts — o segundo passe refaz o trabalho de todos os resultados, não só dos que ganharam artifact |
| O23 | `runtime/stream_normalizer.rs` (`append_openai_delta`) | `position` linear por índice e por ID por delta → O(chamadas² × deltas) em streams com muitas tool calls — `HashMap` auxiliar |
| O24 | `runtime/codemode.rs:197-321` | Cada `tools.call` em cell gera 2 `persist_codemode_fact` (started+completed) → até ~2048 appends sincronizados por cell (MAX_HOST_REQUESTS=1024); calls de discovery não persistem — agrupar facts por cell |
| O25 | `context/pi_compaction/cut.rs:185-197` | `keep_tool_pairs_together` re-varre `messages[cut_index..end]` por iteração do loop → O(n·m); mapa reverso call_id→menor índice resolve (só roda em compactação) |
| O26 | `context/compact.rs:256-276` | `channel_anchors` executa `compaction_prefix_fingerprint` (redact_credentials_cow + SHA-256 do conteúdo) por âncora retida — até 256 âncoras × mensagens grandes, sem memoização entre chamadas |

### Baixas (seleção)

- `runtime/tool_batch.rs:1136` — `unbounded_channel` para progresso de tools; consumidor async atrasado → fila cresce sem teto (limitado pelo volume de saída da tool)
- `runtime/tool_batch.rs:259` — ver O5 (dedup)
- `tools/tool_schedule.rs:17-27` — `serde_json::from_str::<Value>` completo dos arguments por call a cada passe de scheduling/orçamento (só testa `then_run.is_object()`) — deserialize dirigido estilo `PathOnly`
- `tools/mod.rs:782-788` — `names_for_mode` aloca `Vec<&str>` + scan por execução de tool — `LazyLock<HashSet>`/`phf` por modo ou comparação direta
- `tools/mod.rs:1130, 1159-1175` — cache hit clona `ToolExecutionOutcome` inteiro incl. `output` grande; `invalidate_cached_evidence_for_paths` recalcula `path_identity` (String alloc) no loop interno entries×deps×paths — pré-computar identidades
- `tools/execution.rs:922-929` — `hex_digest` formata byte a byte (64 `write!`) — tabela lookup `HEX[256]`; micro, mas em caminho de stamps/verificação de dependência
- `tools/execution.rs` (`path_identity`) — `to_string_lossy().replace()` aloca String por chamada em loops de invalidação
- `tools/patch.rs:148-318` — até 64 edições sequenciais × O(tamanho do arquivo) em `match_indices`+`replace_range` (máx ~640 MB de varredura teórica; limitado por `MAX_PATCH_EDITS`)
- `tools/write.rs:131`, `patch.rs:160,166` — normalização CRLF `replace("\r\n","\n").replace('\n',"\r\n")` — 2 alocações intermediárias + 3 varreduras (até ~30 MB transitório p/ arquivo de 10 MB) — passe único
- `tools/write.rs:183-195, 395-408` — spin-lock `try_lock`+`sleep(10ms)` em locks de mutação — correto em `spawn_blocking`, mas ~10 ms de latência por handoff; Condvar/file-lock com deadline seria mais limpo
- `runtime/run_guards.rs:353-358` — `insert_bounded` faz `map.clear()` tudo-ou-nada ao atingir `MAX_TRACKED` — evicção parcial preservaria mais sinal das proteções
- `session/tool_phases.rs:271-280` — `append`/`ingest` clona `records` completo + replay `from_records` por chamada → O(n²) latente; **sem caller quente** (footgun de API `pub`)
- `session/attempts.rs:333-334` — `usage` aloca 2 Strings por lookup (chave `(String,String)`)
- `session/jsonl_repo.rs` — `records: Vec<DurableRecord>` retém histórico parseado por sessão (by design, limitado a 64 MiB; registros parseados podem exceder o arquivo em RAM)
- `runtime/process.rs:69-103` — `ExecutableResolver`: 3 lock acquisitions por resolução + `stat` (`is_executable_file`) em cache hit; explicit paths (`./x`) sem cache → stat por reconnect/retry
- `slim-lsp/src/process.rs:70-75` — `push_tail` faz `buf.split_off(...)` alocando String novo por chunk após 16 KiB — `drain(..n)` evitaria cópia
- `slim-cli/src/headless.rs:1722-1738` — `provider_runtime_block_on` spawna thread com stack de **8 MiB por chamada de provider** (cada turno headless) — worker persistente amortizaria (verificar se isolamento é intencional p/ debug)
- `slim-tui/src/app.rs:1805,1829` — `Vec::remove(0)` O(n) em `notifications`/`notification_history` (cap 100) a cada push quando cheio — `VecDeque`
- `slim-tui/src/clipboard.rs` — (ver B24)
- `mcp/sse.rs:91-94` — `.to_owned()` aloca String por linha SSE; `process_line` aceita `&str`
- `mcp/stdio.rs:72-73` — `chars().count()` + `take(160)` varrem linha ruidosa 2× (até 16 MiB) — `char_indices().nth(160)` com early-exit
- `mcp/stdio.rs` + `runtime/shell_jobs.rs:595-599` — `record_snapshot` segura mutex `jobs` durante I/O de journal (fsync) por job retido — coletar `Vec<ShellJobInfo>`, dropar guard, gravar
- `tools/search.rs:1542`, `tools/read.rs:184` — `BufReader::new` (8 KB) vs `SEARCH_READ_BUFFER_BYTES` (64 KB) do scan principal — mais syscalls nos mesmos arquivos grandes
- `tools/read.rs` checkpoints — `record_checkpoint` só a cada `CHECKPOINT_INTERVAL_LINES`; `merge_checkpoints` ordena ≤4096+descobertos — aceitável
- `process.rs` runner — pipes com budget head+tail, `discarded_bytes` contabilizado, kill de árvore em timeout/cancel com `wait_for_exit` 500 ms — correto

---

## Suspeitas NÃO CONFIRMADAS (precisam de verificação runtime ou leitura mais profunda)

- `tool_batch.rs:1203-1204` — `progress_error` retorna `Err` sem `finish_tool`: tool já executou (efeitos) mas não emite `ToolPhaseFinished`; ledger trata como interrompida (fail-safe) — impacto depende do caminho de erro do `agent_loop`
- `tool_batch.rs:479-485` — `wait_for_native_work` após erro/cancel não tem bound global (confia nos timeouts internos; shell pode ser 1 h; tool bloqueada em I/O não-interruptível — NFS — atrasaria encerramento)
- `codemode.rs:220-225` — `pending.await` pós-cancel/deadline sem timeout próprio (depende de `McpManager::call_cancellable` ser bounded — não verificado)
- `write.rs:363-368` (ver B27) + `tools/list.rs:350-371` + `execution.rs` — `stamp_matches` de diretório depende só de `modified_nanos`; FS com granularidade grossa pode servir evidência obsoleta (rename mantendo contagem no mesmo tick)
- `jsonl_repo.rs:363-369` — `torn_tail` só cobre `error.is_eof()`; cauda corrompida de forma não-eof falha o `open` inteiro sem quarentena (pode ser fail-closed intencional)
- `events.rs:17-22` + `slim-cli/headless.rs:1722-1737` — `push_runtime_event` chama `block_in_place` quando `on_multi_thread_runtime()` (só checa flavor, não se a thread é worker); `provider_runtime_block_on` dirige o future via `runtime.block_on()` em thread dedicada — se `run_journal` existir no headless, `block_in_place` poderia rodar em thread não-worker (combo parece inalcançável)
- `mcp/http.rs:847` — `Handle::spawn` em `Drop` pode panicar se drop ocorrer após shutdown do runtime (ordem de drop vs runtime não confirmada)
- `tool_batch.rs:464-486` — `execute_provider_tool_batch` restaura `self.cancellation` só no fim; future dropado no meio deixaria token stale (nenhum caller que drope o future encontrado)
- `write.rs:404` — `try_lock` com sleep sem bound temporal (ver B16 — mitigado por cancelamento cooperativo)
- `shell.rs` epílogo PowerShell — erro nativo antigo em `$Error` pode fazer falha posterior ser lida como sucesso (limitação reconhecida em comentário)
- `mcp/stdio.rs` `notifications/cancelled` via `try_send` — drop silencioso impede notificação de cancel ao servidor (backpressure aceitável, mas sem log)

---

## Verificado como saudável (consenso dos agentes — sem achado)

- `slim-lsp`: `pool.rs` (singleflight, leases atômicos, idle-shutdown com `Notify`, `kill_on_drop`+`terminate_process_tree`), `transport.rs` (frames limitados, mailbox com eviction por prioridade, `enable()` correto, guards de cancelamento)
- `tools/write.rs`: temp+`sync_data`+rename atômico com backup `.displaced`, `PATH_LOCKS` limitado com eviction, preservação de modo Unix, `effects_uncertain` em falha de publicação
- `tools/patch.rs`: matching único, CRLF/BOM, hunks limitados, `expect`s guardados por invariantes
- `tools/read.rs`: paginação/checkpoints/digest incremental corretos; `tools/search.rs`: cursores com TTL, dedup de diagnósticos, bounds de bytes/arquivos/linhas
- `process.rs`: process groups/job objects, kill de árvore, pipes com budget head+tail, `capture_may_be_incomplete`, poll de cancelamento 5 ms, `live_groups` lock-free
- `provider.rs`: `MAX_SSE_LINE_BYTES`/`MAX_PROVIDER_STREAM_BYTES`/`MAX_CONNECT_FRAME_PAYLOAD`, cache LRU bounded, redação de segredos, `drain_sse`/`drain_connect_frames` corretos, `spawn_blocking` nas fronteiras
- `stream_normalizer.rs`: hold-back de prefixos de segredos (`safe_stream_split`), gate de segredos em tool args
- `mcp/stdio.rs`: máquina de estados de requests, fila bounded, `notify_one` com permissão (sem lost-wakeup), reaper com `REAPER_WAIT_LIMIT`, threads rastreadas
- `mcp/http.rs`: limites de mensagem, sessão/reconnect/OAuth-bound
- `session/jsonl_repo.rs`: append em batch único, poisoned em falha, `F_BARRIERFSYNC` com fallback, quarentena de torn tail + `set_len`+`seek(End)` correto (sem gap de zeros), `open_no_repair*` fail-closed
- `runtime/tool_batch.rs`: `expect`s são invariantes provadas (`buffer_unordered` produz 1 item/future; aliases preenchidos antes); dedup de calls idênticas; `spawn_blocking` correto
- `runtime/codemode/engine.rs`: QuickJS com memory/stack limit, interrupt handler, canais limitados, bridge `blocking_send` correta
- `runtime/events.rs`: `block_in_place` corretamente protegido por `on_multi_thread_runtime()` nos caminhos async
- `slim-tui/render.rs`: `EventCoalescer` merge O(1); `HeightIndexMemo`/`WrapCache` compartilham índice via `Arc`; clipboard despacha para threads
- `slim-cli`: OAuth callback valida `is_loopback`/path/`state`, store com lock de arquivo + escrita atômica + checagem de symlink; `leave_session` `block_in_place`+`Handle::block_on` correto em runtime multi_thread
- `unsafe`: concentra-se em FFI de plataforma (libc/process-group/Windows Job/signal) com guardas e comentários de segurança adequados

---

## Cobertura — o que ficou de fora

Auditoria profunda cobriu majoritariamente `slim-core` (runtime, tools,
session, process, mcp parcial, provider parcial) e trechos de `slim-lsp`,
`slim-tui`, `slim-cli`. **Não cobertos exaustivamente**: `slim-cli` (~40k:
auth, oauth, tui wiring, headless run-loop — só greps), `slim-tui` reducer/
app/terminal lifecycle/markdown restante, `slim-lsp` instance/manager/
document/diagnostics/workspace, provider adapters individuais
(openai/anthropic/codex/xai/devin wire), `mcp/{manager,http,sse,oauth}`,
`context/` restante, `codemode/engine`, `codeintel`, `skills`,
`workspace_files`. ~9.000 ocorrências de `unwrap`/`expect` triadas por
padrão — as auditadas estavam em invariantes provadas ou testes, mas nem
todas foram lidas uma a uma.

**Limitação central:** análise 100% estática — nenhum teste, build ou
benchmark executado; todas as severidades e custos são derivadas de leitura
de código, não medidos. Recomenda-se validar os TOP achados com reprodução
(runtime) antes de priorizar correções.

---

## Top 3 por custo-benefício

1. **B1** — `enable()` em 3 funções de `cancellation.rs`: fix de ~9 linhas
   que elimina uma classe inteira de hangs raros porém permanentes.
2. **B2** — `spawn_blocking`/`track_native_work` em `ShellJobs::input`:
   elimina o único caminho que congela o executor sem nenhuma válvula de
   escape.
3. **O1** — eliminar o clone O(estado) por append do
   `DurableAppendValidator`: maior ganho assintótico do subsistema de
   sessão; sessões longas são o caso real de uso.
