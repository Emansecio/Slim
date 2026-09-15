# Slim x Pi — comparativo rastreavel (2026-09-09 14:03:26Z)

Modelo: `gpt-5.6-luna`; provider: `openai-codex`; effort high.

## Resumo (soma das campanhas aproveitadas)

| Metrica | Slim | Pi | Delta (Slim-Pi) |
|---|---:|---:|---:|
| Chamadas ao modelo | 8 | 11 | -3 |
| Ferramentas executadas | 10 | 12 | -2 |
| Falhas de ferramenta | 0 | 2 | -2 |
| Tokens totais (in+out) | 21686 | 22723 | -1037 |
| Entrada (inclui cache) | 19531 | 20914 | -1383 |
| Cache lido | 1536 | 4096 | -2560 |
| Saida (inclui reasoning) | 2155 | 1809 | +346 |
| Reasoning | 1265 | 805 | +460 |
| Tempo wall soma (ms) | 54515 ms | 59431 ms | -4916 ms |
| Tempo provider soma (ms) | 53667 ms | 51788 ms | +1879 ms |
| Tempo tools soma (ms) | 690 ms | 640 ms | +50 ms |

Wall = processo inteiro por braco; provider = soma das latencias informadas pelo harness; tools = soma das duracoes; residuo = wall-provider-tools (startup/teardown).

## Por campanha

| Campanha | Cenario | Slim (chamadas/falhas/tokens/wall) | Pi (chamadas/falhas/tokens/wall) |
|---|---|---|---|
| `20260909-134056Z` | merge_ranges | 4/0/10714/23391ms | 7/1/13731/34873ms |
| `20260909-134154Z` | merge_ranges | 4/0/10972/31124ms | 4/1/8992/24558ms |

## Erros de ferramenta (para achar fraqueza)

### slim: 0 falha(s) em 10 execucoes

Nenhuma falha registrada.

### pi: 2 falha(s) em 12 execucoes

| Classe | Qtd |
|---|---:|
| read-missing | 2 |

| Campanha | Turno | Tool | Args | Classe | Trecho do erro | Rastro |
|---|---|---|---|---|---|---|
| `20260909-134056Z` | 3 | read | `{"path": "ranges.py"}` | read-missing | ENOENT: no such file or directory, access 'C:\Users\User\AppData\Local\Temp\slim-pi-luna-2cf3dzd3\pi\ranges.py' | pi.audit.jsonl:e3afec57043d |
| `20260909-134154Z` | 2 | read | `{"path": "ranges.py"}` | read-missing | ENOENT: no such file or directory, access 'C:\Users\User\AppData\Local\Temp\slim-pi-luna-8ebozprt\pi\ranges.py' | pi.audit.jsonl:d83189494fa6 |

## Por turno (crescimento de contexto)

### `20260909-134056Z`

#### slim

| turno | in | out | reasoning | provider_ms | tools |
|---|---:|---:|---:|---:|---|
| 1 | 1701 | 102 | 24 | 4463 | read,read,list |
| 2 | 2265 | 596 | 273 | 12543 | write |
| 3 | 2904 | 37 | 10 | 2086 | shell |
| 4 | 2985 | 124 | 84 | 3826 |  |

#### pi

| turno | in | out | reasoning | provider_ms | tools |
|---|---:|---:|---:|---:|---|
| 1 | 1153 | 33 | 13 | 4143 | read |
| 2 | 1333 | 18 | 0 | 3562 | read |
| 3 | 1402 | 42 | 7 | 2048 | bash |
| 4 | 1569 | 32 | 12 | 1676 | read |
| 5 | 1898 | 767 | 430 | 14940 | write |
| 6 | 2680 | 33 | 7 | 2688 | bash |
| 7 | 2743 | 28 | 0 | 1821 |  |

### `20260909-134154Z`

#### slim

| turno | in | out | reasoning | provider_ms | tools |
|---|---:|---:|---:|---:|---|
| 1 | 1701 | 104 | 26 | 3876 | read,read,list |
| 2 | 2267 | 505 | 225 | 10901 | write |
| 3 | 2814 | 36 | 9 | 2357 | shell |
| 4 | 2894 | 651 | 614 | 13615 |  |

#### pi

| turno | in | out | reasoning | provider_ms | tools |
|---|---:|---:|---:|---:|---|
| 1 | 1151 | 93 | 13 | 3564 | read,read,read,bash |
| 2 | 1848 | 674 | 316 | 13207 | write |
| 3 | 2537 | 33 | 7 | 1796 | bash |
| 4 | 2600 | 56 | 0 | 2343 |  |

## Rastreabilidade

### `20260909-134056Z` — merge_ranges (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `c4e8c1d9c06eb0df6a5dbdc82bc52b7e6bca119b1abe471aa60c28a90dec50bf`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260909-134154Z` — merge_ranges (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `c4e8c1d9c06eb0df6a5dbdc82bc52b7e6bca119b1abe471aa60c28a90dec50bf`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

## Reproducao

```powershell
python bench/luna-live/daily.py --rounds 1
python bench/luna-live/report.py <campanhas...> --output-md RELATORIO.md --output-json report.json
```

## Limitacoes

- Amostra pequena, host nao exclusivo, caches do servidor nao controlados, ordem alternada mas sem randomizacao plena.
- Instrumentacao assimetrica: Pi via extensao observadora, Slim via ledger/sessao; contagens sao turnos do modelo, nao TCP/retries de transporte.
- Tokens sao usos informados pelos providers; sem inferencia de custo monetario.
- Outcomes Slim: facts estruturados tool.v1 quando presentes; senao heuristica sobre o texto da tool.
- Todos os bracos aproveitaram passaram no gate (exit 0, validacao externa PASS, fixtures intactos).
