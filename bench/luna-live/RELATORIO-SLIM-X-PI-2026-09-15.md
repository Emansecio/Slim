# Slim x Pi — comparativo rastreavel (2026-09-15 19:47:02Z)

Modelo: `gpt-5.6-luna`; provider: `openai-codex`; effort high.

## Resumo (soma das campanhas aproveitadas)

| Metrica | Slim | Pi | Delta (Slim-Pi) |
|---|---:|---:|---:|
| Chamadas ao modelo | 32 | 35 | -3 |
| Ferramentas executadas | 46 | 48 | -2 |
| Falhas de ferramenta | 1 | 3 | -2 |
| Tokens totais (in+out) | 108612 | 83300 | +25312 |
| Entrada (inclui cache) | 101249 | 76659 | +24590 |
| Cache lido | 15872 | 20480 | -4608 |
| Saida (inclui reasoning) | 7363 | 6641 | +722 |
| Reasoning | 3483 | 2853 | +630 |
| Arquivos modificados | 8 | 8 | 0 |
| Arquivos criados | 4 | 4 | 0 |
| Tempo wall soma (ms) | 238790 ms | 213341 ms | +25449 ms |
| Tempo provider soma (ms) | 212285 ms | 169642 ms | +42643 ms |
| Tempo tools soma (ms) | 22882 ms | 17875 ms | +5007 ms |
| Taxa de acerto de cache | 15.7% | 26.7% | -11.0 pp |

Wall = processo inteiro por braco; provider = soma das latencias informadas pelo harness; tools = soma das duracoes; residuo = wall-provider-tools (startup/teardown). Arquivos = diff do workspace vs fixtures originais.

## Pareamento (somente pares com gate aprovado nos dois bracos)

| Medida | Valor |
|---|---:|
| Pares aproveitados | 7 |
| Slim mais economico em tokens | 2/7 |
| Mediana razao tokens Slim/Pi | 1.1872 |
| Mediana razao wall Slim/Pi | 1.1691 |

| Cenario | Pares | Tokens Slim | Tokens Pi | Delta Slim | Razao min/med/max |
|---|---:|---:|---:|---:|---:|
| config_migration | 1 | 21704 | 12831 | +69.15% | 1.69/1.69/1.69 |
| js_pagination | 1 | 11687 | 13342 | -12.40% | 0.88/0.88/0.88 |
| json_cli | 1 | 28259 | 14323 | +97.30% | 1.97/1.97/1.97 |
| ledger_audit | 1 | 11971 | 14103 | -15.12% | 0.85/0.85/0.85 |
| merge_ranges | 2 | 23790 | 18836 | +26.30% | 1.19/1.27/1.35 |
| repair_catalog | 1 | 11201 | 9865 | +13.54% | 1.14/1.14/1.14 |

## Por campanha

| Campanha | Cenario | Slim (chamadas/falhas/tokens/wall/arq) | Pi (chamadas/falhas/tokens/wall/arq) |
|---|---|---|---|
| `20260915-193525Z` | merge_ranges | 4/0/12030/33517ms/0mod+1novos | 5/0/10133/27551ms/0mod+1novos |
| `20260915-193920Z` | merge_ranges | 4/0/11760/22545ms/0mod+1novos | 4/1/8703/27753ms/0mod+1novos |
| `20260915-194013Z-repair_catalog` | repair_catalog | 4/0/11201/20138ms/2mod+0novos | 5/1/9865/20353ms/2mod+0novos |
| `20260915-194056Z-json_cli` | json_cli | 7/0/28259/70440ms/0mod+1novos | 5/0/14323/60249ms/0mod+1novos |
| `20260915-194323Z-js_pagination` | js_pagination | 4/0/11687/21813ms/2mod+0novos | 5/1/13342/29509ms/2mod+0novos |
| `20260915-194416Z-ledger_audit` | ledger_audit | 3/0/11971/36758ms/1mod+1novos | 5/0/14103/25715ms/1mod+1novos |
| `20260915-194520Z-config_migration` | config_migration | 6/1/21704/33579ms/3mod+0novos | 6/0/12831/22211ms/3mod+0novos |

## Por ferramenta (uso agregado por braco)

| Ferramenta | Slim calls/falhas/ms | Pi calls/falhas/ms |
|---|---:|---:|
| read | 23/0/147ms | 25/1/163ms |
| bash | - | 16/2/17692ms |
| shell | 9/1/22674ms | - |
| patch | 7/0/57ms | - |
| write | 3/0/3ms | 3/0/8ms |
| edit | - | 4/0/12ms |
| list | 4/0/1ms | - |

## Erros de ferramenta (para achar fraqueza)

### slim: 1 falha(s) em 46 execucoes

| Classe | Qtd |
|---|---:|
| shell-syntax | 1 |

| Campanha | Turno | Tool | Args | Classe | Trecho do erro | Rastro |
|---|---|---|---|---|---|---|
| `20260915-194520Z-config_migration` | 3 | shell | `{"command":"python","args":["check.py"],"timeout_ms":120000}` | shell-syntax | exit 1 stdout: stderr: Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-91qirkyc\slim\check.py", line ... | slim.session.jsonl:call=...96TsfWEK |

### pi: 3 falha(s) em 48 execucoes

| Classe | Qtd |
|---|---:|
| read-missing | 1 |
| other-error | 1 |
| patch-reject | 1 |

| Campanha | Turno | Tool | Args | Classe | Trecho do erro | Rastro |
|---|---|---|---|---|---|---|
| `20260915-193920Z` | 2 | read | `{"path": "ranges.py"}` | read-missing | ENOENT: no such file or directory, access 'C:\Users\User\AppData\Local\Temp\slim-pi-luna-0y_pji68\pi\ranges.py' | pi.audit.jsonl:ee03803345fc |
| `20260915-194013Z-repair_catalog` | 2 | bash | `{"command": "git status --short"}` | other-error | fatal: not a git repository (or any of the parent directories): .git Command exited with code 128 | pi.audit.jsonl:7e6e8c4f0c50 |
| `20260915-194323Z-js_pagination` | 5 | bash | `{"command": "git diff -- pager.cjs SPEC.md check.py"}` | patch-reject | warning: Limiting comparison with pathspecs is only supported if both paths are directories. usage: git diff --no-index [<options>] <path> <... | pi.audit.jsonl:6049b7af59ba |

## Por turno (crescimento de contexto)

### `20260915-193525Z`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2006 | 99 | 21 | 924 | 7989 | read,read,list |
| 2 | 2567 | 644 | 335 | 3022 | 15652 | write |
| 3 | 3255 | 25 | 0 | 8262 | 2009 | shell |
| 4 | 3324 | 110 | 74 | 8478 | 4805 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1152 | 92 | 8 | 247 | 3865 | read,bash |
| 2 | 1513 | 31 | 11 | 3204 | 1928 | read |
| 3 | 1841 | 562 | 268 | 5969 | 11356 | write |
| 4 | 2418 | 24 | 0 | 10637 | 1897 | bash |
| 5 | 2472 | 28 | 0 | 11005 | 1655 |  |

### `20260915-193920Z`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2006 | 93 | 15 | 924 | 4565 | read,read,list |
| 2 | 2561 | 577 | 261 | 2972 | 12592 | write |
| 3 | 3185 | 37 | 10 | 7543 | 2168 | shell |
| 4 | 3266 | 35 | 0 | 9283 | 2045 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1152 | 93 | 13 | 247 | 4487 | read,read,read,bash |
| 2 | 1846 | 591 | 216 | 4769 | 12489 | write |
| 3 | 2452 | 24 | 0 | 9615 | 2940 | bash |
| 4 | 2506 | 39 | 0 | 9983 | 2377 |  |

### `20260915-194013Z-repair_catalog`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2008 | 102 | 23 | 935 | 4945 | read,read,read,read |
| 2 | 2573 | 330 | 155 | 3125 | 7550 | patch |
| 3 | 3002 | 37 | 10 | 6528 | 2892 | shell |
| 4 | 3077 | 72 | 0 | 8274 | 2554 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1147 | 100 | 9 | 225 | 3272 | read,bash,bash |
| 2 | 1551 | 80 | 15 | 3537 | 2331 | read,read,read |
| 3 | 1996 | 265 | 85 | 7401 | 6029 | edit |
| 4 | 2282 | 33 | 7 | 10414 | 1604 | bash |
| 5 | 2339 | 72 | 0 | 12223 | 2144 |  |

### `20260915-194056Z-json_cli`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2016 | 106 | 22 | 960 | 5017 | read,read,list,list |
| 2 | 2697 | 40 | 10 | 3154 | 2050 | read |
| 3 | 2829 | 1268 | 497 | 4919 | 24139 | write |
| 4 | 4143 | 41 | 10 | 13314 | 3553 | shell |
| 5 | 4223 | 690 | 606 | 15079 | 13997 | patch |
| 6 | 5006 | 42 | 11 | 22979 | 2389 | shell |
| 7 | 5077 | 81 | 14 | 24760 | 3314 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1151 | 87 | 13 | 243 | 3388 | read,bash |
| 2 | 1530 | 76 | 21 | 3354 | 2575 | read,read |
| 3 | 2092 | 1546 | 738 | 7433 | 29951 | write |
| 4 | 3654 | 35 | 9 | 19338 | 1961 | bash |
| 5 | 3714 | 438 | 398 | 21231 | 9193 |  |

### `20260915-194323Z-js_pagination`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 129 | 25 | 935 | 7601 | read,read,read,read |
| 2 | 2732 | 327 | 70 | 3208 | 7041 | patch |
| 3 | 3157 | 41 | 10 | 6181 | 2576 | shell |
| 4 | 3235 | 55 | 0 | 7942 | 3728 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1148 | 118 | 30 | 228 | 3591 | bash,read,read |
| 2 | 1930 | 60 | 8 | 4858 | 2027 | read,read |
| 3 | 2098 | 535 | 271 | 7285 | 11220 | edit |
| 4 | 2655 | 85 | 20 | 11737 | 2778 | bash,bash |
| 5 | 4448 | 265 | 219 | 21614 | 5769 |  |

### `20260915-194416Z-ledger_audit`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 114 | 32 | 939 | 3142 | read,read,read |
| 2 | 3847 | 977 | 717 | 3112 | 19835 | shell |
| 3 | 4909 | 113 | 45 | 11734 | 11877 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1154 | 91 | 19 | 247 | 3209 | bash,read |
| 2 | 1516 | 65 | 15 | 3255 | 2308 | read,read |
| 3 | 3193 | 430 | 223 | 10351 | 9079 | bash |
| 4 | 3695 | 60 | 40 | 14387 | 3337 | read |
| 5 | 3818 | 81 | 36 | 16600 | 2654 |  |

### `20260915-194520Z-config_migration`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2017 | 155 | 28 | 992 | 5076 | read,read,read,read,read |
| 2 | 2770 | 519 | 260 | 3452 | 11987 | patch,patch |
| 3 | 3436 | 53 | 22 | 7761 | 2863 | shell |
| 4 | 3784 | 271 | 101 | 9630 | 6494 | patch,patch |
| 5 | 4206 | 63 | 32 | 12599 | 2587 | shell |
| 6 | 4313 | 117 | 67 | 14518 | 3253 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1146 | 98 | 24 | 224 | 3175 | read,bash |
| 2 | 1509 | 99 | 16 | 3266 | 2581 | read,read,read,read |
| 3 | 2097 | 250 | 111 | 7691 | 5404 | edit |
| 4 | 2370 | 111 | 0 | 10732 | 2869 | edit |
| 5 | 2504 | 36 | 10 | 11391 | 2540 | bash |
| 6 | 2570 | 41 | 0 | 13310 | 1659 |  |

## Rastreabilidade

### `20260915-193525Z` — merge_ranges (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-193920Z` — merge_ranges (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-194013Z-repair_catalog` — repair_catalog (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-194056Z-json_cli` — json_cli (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-194323Z-js_pagination` — js_pagination (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-194416Z-ledger_audit` — ledger_audit (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-194520Z-config_migration` — config_migration (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
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
