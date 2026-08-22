# Benchmarks v2 — Slim x Pi x Pit: economia de tokens + velocidade

Sucessor do harness v1 (`../`). Mesma ideia (fixture localhost adaptativo que
dirige cada agente com as **próprias tools**), com quatro acréscimos:

1. **4 cenários** em vez de 1 (leitura, geração de código, multistep, sessão longa).
2. **N repetições** por cenário×agente; análise reporta medianas e mínimos.
3. **Timing**: startup até o 1º request, TTFC (tempo até o primeiro código
   gerado), gaps entre turnos, duração total — tudo gravado pelo servidor e
   pelo runner.
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

O caminho headless do Slim não plumva reasoning effort hoje (verificado:
`crates/slim-cli/src/cli.rs` constrói `ProviderRunOptions::default()`; só o TUI
lê `SLIM_EFFORT`, `crates/slim-cli/src/tui.rs:109`). O adapter já sabe enviar
`reasoning_effort` (`slim-core/src/provider.rs:1409`). Opções:

- Patch mínimo no Slim: consumir `SLIM_EFFORT`/slim.toml em headless e repassar
  via `ProviderRunOptions::with_reasoning_effort`; ou
- Aceitar que o gate marque o Slim como violação e não comparar seus números.

Pi e Pit já suportam `--thinking high` (verificado na doc do Pi e no source
do Pit) e recebem `models.json` isolado com `thinkingLevelMap`.

## Arquivos

| Arquivo | Papel |
|---|---|
| `PLAN.md` | decisões de design, cenários, métricas, limitações |
| `capture_server.py` | fixture v2 (multi-cenário, timing, compliance) |
| `run_benchmark.ps1` | runner da matriz cenário × run × agente |
| `analyze.py` | tabelas markdown + `summary_v2.json` |
| `RESULTS.template.md` | esqueleto para os resultados |

Resultados consolidados: preencher `RESULTS.template.md` → renomear para
`RESULTS.md`. A v1 permanece intocada em `../`.
