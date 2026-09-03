# Benchmarks v2 — Slim x Pi x Pit: economia de tokens + velocidade

Sucessor do harness v1 (`../`). Mesma ideia (fixture localhost adaptativo que
dirige cada agente com as **próprias tools**), com quatro acréscimos:

1. **4 cenários** em vez de 1 (leitura, geração de código, multistep, sessão longa).
2. **N repetições** por cenário×agente; análise reporta medianas e mínimos.
3. **Timing**: estimativas de startup/TTFC, gaps entre turnos e duração total.
   O total é medido diretamente; startup/TTFC são derivados e podem incluir
   teardown após o último request (ver `PLAN.md`).
4. **Compliance gate**: todos os agentes são pinados em `gpt-5.6-luna` com
   thinking `high`; o servidor grava modelo/effort reais de cada request e a
   análise marca violação.

## Execução

```powershell
python bench/token-economy/v2/capture_server.py   # terminal 1 (porta 8931)
powershell -File bench/token-economy/v2/run_benchmark.ps1 -Runs 3
python bench/token-economy/v2/analyze.py          # terminal 2 ou depois
```

Parâmetros do runner: `-SlimPath`, `-Port`, `-Runs`, `-Scenarios`, `-Agents`.

## Pré-requisito antes da primeira execução

Slim headless consome `SLIM_EFFORT`/`slim.toml` e repassa o valor por
`ProviderRunOptions::with_reasoning_effort`. O runner exporta `high`; o servidor
confirma o campo no wire e invalida qualquer agente não conforme.

Pi e Pit recebem `models.json` isolado com `thinkingLevelMap` e rodam com
`--thinking high`.

## Arquivos

| Arquivo | Papel |
|---|---|
| `PLAN.md` | decisões de design, cenários, métricas, limitações |
| `capture_server.py` | fixture v2 (multi-cenário, timing, compliance) |
| `run_benchmark.ps1` | runner da matriz cenário × run × agente |
| `analyze.py` | tabelas markdown + `summary_v2.json` |
| `RESULTS.template.md` | esqueleto para novas campanhas |
| `RESULTS.md` | resultados consolidados atuais e PERF-02 |

## Campanha atual — 2026-08-23

A matriz fresca executou 4 cenários × 3 runs × 3 agentes: 36 processos, 78
requests conformes (`gpt-5.6-luna`/`high`) e 36 arquivos de timing. Somente
`s1_read` é comparação cross-agent estrita: os braços Pi/Pit atuais não
expuseram ao discovery uma tool de escrita compatível e encerraram cedo nos
outros cenários. Resultados e percentuais: `RESULTS.md`; tabela gerada:
`analysis-current.md`; dados: `summary_v2.json`.

A v1 permanece intocada em `../`.
