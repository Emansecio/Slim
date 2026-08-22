# RESULTS — Benchmark v2: Slim x Pi x Pit

> Checkpoint atual: **2026-08-22**. Modelo no fio: `gpt-5.6-luna`;
> reasoning effort: `high`. O código atual e as capturas atuais prevalecem sobre
> números históricos. Método: `PLAN.md`, `run_benchmark.ps1` e `analyze.py`.

## 0. Compliance gate

| Agente | Modelo no fio | Effort | Estado |
|---|---|---|---|
| Slim | `gpt-5.6-luna` | `high` | ✅ |
| Pi | `gpt-5.6-luna` | `high` | ✅ |
| Pit | `gpt-5.6-luna` | `high` | ✅ |

No braço pós-PERF-02, os **75 requests** de Slim (`15 runs × 5 turnos`) foram
validados separadamente: 75/75 mantiveram modelo e effort esperados.

## 1. Payload disponível

Medianas do `summary_v2.json`. Slim `s1_read` e `s4_long` são capturas atuais;
`s2_codegen`/`s3_multistep` e braços Pi/Pit não foram rerodados no PERF-02 e
ficam apenas como referência histórica comparativa:

| Cenário | Agente | T1 total | System | Tools | Tn total | Histórico Tn | Soma dos requests |
|---|---|---:|---:|---:|---:|---:|---:|
| `s1_read` | Slim | 4.238 B | 2.435 B | 1.592 B (6) | 7.116 B | 2.953 B | 11.354 B |
| `s1_read` | Pi | 5.941 B | 2.739 B | 2.900 B (4) | 8.729 B | 2.888 B | 14.670 B |
| `s1_read` | Pit | 11.870 B | 4.029 B | 7.434 B (12) | 14.658 B | 3.023 B | 26.528 B |
| `s2_codegen` | Slim | 5.488 B | 3.684 B | 1.592 B (6) | 6.242 B | 830 B | 11.730 B |
| `s3_multistep` | Slim | 5.488 B | 3.684 B | 1.592 B (6) | 9.120 B | 3.706 B | 20.850 B |
| `s4_long` | Slim | 4.239 B | 2.435 B | 1.592 B (6) | 8.519 B | 4.350 B | 35.511 B |
| `s4_long` | Pi | 5.942 B | 2.739 B | 2.900 B (4) | 11.518 B | 5.675 B | 26.190 B |
| `s4_long` | Pit | 12.684 B | 4.029 B | 7.434 B (12) | 15.908 B | 4.271 B | 44.064 B |

Os valores antigos de Slim `5.487/5.488 B` em `s1_read` e `13.852 B` na soma
foram substituídos pelo baseline atual de `4.238 B` e `11.354 B`.

## 2. PERF-01 — microbenchmark de setup das tools

Comando:

```bash
cargo bench -p slim-core --bench tool_setup
```

Medição desta sessão, 11 amostras × 20.000 iterações, release:

| Métrica | Valor |
|---|---:|
| Tools internas | 6 |
| Definições serializadas internas | 1.418 B |
| Setup repetido | 9.305,065 ns/turno |
| Acesso cacheado amortizado | 0,730 ns/turno |
| Speedup isolado | 12.746,7× |

Interpretação: PERF-02 poupa aproximadamente **9,3 µs por turno adicional**.
É uma otimização real de CPU/alocações, mas pequena demais para ser percebida
isoladamente pelo usuário ou pelo harness PowerShell.

## 3. PERF-02 — setup único por agent loop

Mudança: `definitions_for_mode(mode)` e a serialização usada por
`ContextSnapshot.tools_bytes` são calculadas preguiçosamente no primeiro turno
e reutilizadas até o fim da mesma execução do agent loop.

### Antes → depois (`s4_long`, Slim, N=15)

| Métrica | Antes | Depois | Delta |
|---|---:|---:|---:|
| T1 total | 4.239 B | 4.239 B | 0 |
| T1 system | 2.435 B | 2.435 B | 0 |
| T1 tools | 1.592 B | 1.592 B | 0 |
| Tn total | 8.519 B | 8.519 B | 0 |
| Histórico Tn | 4.350 B | 4.350 B | 0 |
| Soma dos 5 requests | 35.511 B | 35.511 B | 0 |
| Crescimento | 2,01× | 2,01× | 0 |
| Startup estimado mediana/min | 1.159 / 1.124 ms | 1.162 / 1.145 ms | +3 / +21 ms |
| TTFC estimado mediana/min | 1.160 / 1.125 ms | 1.164 / 1.146 ms | +4 / +21 ms |
| Gap mediano | 1 ms | 2 ms | +1 ms |
| Processo total mediano | 1.168 ms | 1.170 ms | +2 ms (+0,17%) |

`startup` e `TTFC` são aproximações do analisador (`process_total - span dos
requests`) e podem incluir teardown após o último request. Somente o tempo
total do processo é uma medição direta do runner.

Verificação programática:

- **75/75 payloads raw byte-idênticos** antes/depois;
- todas as métricas de payload idênticas;
- 75/75 novos registros de compliance verdes;
- nenhuma alteração de modelo, effort, schema, tools ou histórico;
- variação temporal mediana de +0,17%, abaixo do ruído do `Start-Job`; o
  benchmark E2E não tem resolução para demonstrar uma economia de ~9,3 µs.

Binário implantado no braço antes: SHA-256
`652300e1515784d3c626e1a67e01d87a10019d56aef86d3a914e5213e7299569`,
6.587.904 B. Candidato pós-mudança: SHA-256
`bc826e987097bb6dd49b680bd28d4e9fb3ce663a2937495a4fe27921a2552847`,
6.589.440 B: +1.536 B (+0,023%). A `Vec` de definições (~1,4 KB serializados)
permanece viva até o fim do loop em vez de ser recriada; isso estende sua vida,
mas não aumenta o pico já pago durante cada request.

## 4. PERF-03–07 — segunda onda implementada

| Slice | Mudança entregue | Evidência antes → depois |
|---|---|---|
| PERF-03 | stdout/stderr drenados concorrentemente durante o processo | 5 MiB por stream: timeout/falha em 10,26 s → sucesso em 0,76 s; buffers continuam separados e crus |
| PERF-04 | request HTTP construído uma vez; cache key/clones só com cache ativo | adapter contador: **3 → 1 builds** no caminho normal; suíte de cache permaneceu verde |
| PERF-05 | parser SSE por cursor e um único drain por chunk | 20.000 deltas num buffer: **117,029 ms → mediana 8,264 ms (14,2×)** |
| PERF-06 | encoder Base64 manual substituído pela crate já instalada | -20 linhas líquidas; fixtures OpenAI/Anthropic preservaram `AAEC` |
| PERF-07 | `read` paginado usa `BufReader` e buffer de linha reutilizado | memória retida: O(tamanho do arquivo) → O(maior linha + página); CRLF/footer/EOF preservados |

A/B do binário implantado no `s4_long`, N=5:

- **25/25 request bodies byte-idênticos** ao baseline preservado;
- T1 4.239 B, Tn 8.519 B e soma 35.511 B: deltas zero;
- 25/25 novos requests de compliance verdes;
- processo mediano 1.167 → 1.174 ms (+7 ms, +0,60%), ruído do `Start-Job`;
- suíte: 45 suítes / 220 passed / 0 failed / 1 ignored / 0 warnings;
- binário: 6.589.440 → 6.671.872 B (+82.432 B, +1,25%), trade-off explícito
  da drenagem concorrente/novos caminhos std; payload e pico de contexto não cresceram.

## 5. Oportunidades restantes

| Prioridade | Oportunidade | Evidência atual | Veredito |
|---|---|---|---|
| P1 | Eliminar cliff do WrapCache acima de 4.096 blocos | benchmark cobre 3.200; ainda não há repro acima da capacidade | ampliar benchmark antes |
| P2 | Batch de persistência JSONL | `--session` chama `sync_data()` por evento após o run | exige decisão de durabilidade |
| P2 | Limitar fila core→TUI e retenção de eventos | channel e vetor de eventos são ilimitados | exige design/contrato |
| P3 | Paralelizar tools somente-leitura | ordem de eventos e snapshots pode mudar | deferido por risco funcional |

O cache do analisador e a remoção do cooldown do runner acelerariam apenas o
harness, não o Slim implantado; não são prioridade de produto.

## 6. Itens deliberadamente adiados

- **TOK-02/TOK-06:** não há telemetria real persistida para escolher limiares.
  A TUI ainda não persiste sessões; headless só grava com `--session`, e nenhum
  log de sessão de uso real foi encontrado neste ambiente.
- **TOK-07:** reduzir o bloco de 1.592 B dinamicamente pode exigir turno extra
  ou esconder uma capability necessária; risco funcional supera evidência.
- **System prompt:** já está em 2.435 B no cenário de leitura. Novo corte exige
  A/B sem perda semântica; não foi reaberto.
- ProviderCache no produto, transporte comprimido e refactors do loop continuam
  fora de escopo pelos motivos registrados em `AUDIT-ECONOMIA-TOKENS.md`.

## Limitações

- O fixture responde instantaneamente; velocidade mede overhead local e inclui
  aproximadamente 1,16 s de `Start-Job`, não latência/tokens por segundo do modelo.
- O benchmark E2E prova não-regressão; ganhos pequenos ficam sob o ruído. Os
  ganhos shell/SSE foram medidos em regressões isoladas de carga.
- Nenhum provider live foi chamado.
- `cargo fmt --all -- --check` tem drift preexistente; Clippy workspace para em
  6 diagnósticos de `slim-tui`, e `slim-cli --no-deps` revela mais 20
  `let_unit_value` + 1 assert bool, todos preexistentes. Clippy focado em
  `slim-core --all-targets -D warnings` ficou limpo.

## Reprodução

```powershell
python bench/token-economy/v2/capture_server.py
powershell -NoProfile -File bench/token-economy/v2/run_benchmark.ps1 `
  -Runs 15 -Scenarios s4_long -Agents slim
python3 bench/token-economy/v2/analyze.py
powershell -NoProfile -File refresh-slim.ps1 -Test
```
