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

## 4. Próximas oportunidades verificadas estaticamente

Nenhuma destas foi implementada ou teve ganho medido. Cada uma exige benchmark
e commit separado.

| Prioridade | Oportunidade | Evidência atual | Esforço/risco | Veredito |
|---|---|---|---|---|
| P0 | Drenar stdout/stderr do shell durante execução | pipes são lidos só após `try_wait()` indicar término; output grande pode bloquear o filho | M / baixo-médio | **Corrigir como bug**, com repro de 10 MiB |
| P1 | Construir payload provider uma vez | caminho sem cache constrói/serializa request para validação, calcula cache key e constrói novamente | S/M / baixo | **Medir e fazer** |
| P1 | Não clonar eventos quando cache está desligado | todo delta é clonado e retido embora clientes normais usem `cache: None` | S / baixo | **Medir e fazer** |
| P1 | Tornar parser SSE linear | `find + to_owned + drain` frontal por linha; três revisões independentes confirmaram O(n²) em chunks agrupados | S/M / baixo-médio | **Medir e fazer** |
| P1 | Eliminar cliff do WrapCache acima de 4.096 blocos | `HeightIndex` varre todos os blocos; benchmark cobre apenas 3.200 e o LRU pode thrash acima da capacidade | M / médio | **Ampliar benchmark primeiro** |
| P2 | Usar crate `base64` já instalada | encoder manual escalar no caminho de imagens de até 20 MiB | S / baixo | **Medir e simplificar** |
| P2 | Paginar `read` sem carregar arquivo inteiro | `read_to_string` aloca todo o arquivo para uma janela limitada | M / baixo | **Medir RSS/latência** |
| P2 | Batch de persistência JSONL | `--session` chama `sync_data()` por evento após o run | S/M / médio | **Deferir** até fechar contrato de durabilidade |
| P2 | Limitar fila core→TUI e retenção de eventos | há `mpsc::channel` upstream ilimitado e `AppHandle` mantém o vetor completo | M/L / médio | **Exige design**, não correção adjacente |
| P3 | Paralelizar tools somente-leitura | runtime executa chamadas serialmente | M / médio-alto | **Deferir**: ordem/eventos/snapshot podem mudar |

O cache do analisador e a remoção do cooldown do runner acelerariam apenas o
harness, não o Slim implantado; não são prioridade de produto.

## 5. Itens deliberadamente adiados

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
- O benchmark E2E prova não-regressão, não o ganho de microssegundos do PERF-02.
- Nenhum provider live foi chamado.
- `cargo fmt --all -- --check` tem drift preexistente fora de PERF-02; Clippy
  workspace encontra exatamente 6 diagnósticos preexistentes em `slim-tui`.
  O Clippy focado em `slim-core --all-targets -D warnings` ficou limpo.

## Reprodução

```powershell
python bench/token-economy/v2/capture_server.py
powershell -NoProfile -File bench/token-economy/v2/run_benchmark.ps1 `
  -Runs 15 -Scenarios s4_long -Agents slim
python3 bench/token-economy/v2/analyze.py
powershell -NoProfile -File refresh-slim.ps1 -Test
```
