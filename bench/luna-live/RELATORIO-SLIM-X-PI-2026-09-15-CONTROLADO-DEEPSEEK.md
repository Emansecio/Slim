# Slim x Pi — comparativo rastreavel (2026-09-15 21:37:02Z)

> **Controle indisponível: 0/8 pares válidos.** Os dois programas receberam limite mensal do OpenCode Go em todas as tentativas. Os zeros de tokens abaixo são uso indisponível/sem geração, não economia; os tempos são tempos de falha, não desempenho da tarefa. Nenhum desses registros integra o resultado Luna. A agenda terminou com oito falhas retidas e exit 1.

Modelo: `deepseek-v4-flash`; provider: `opencode-go`; effort high.

## Resumo (todos os bracos com registros, incluindo falhas; uso pode ser parcial)

| Metrica | Slim | Pi | Delta (Slim-Pi) |
|---|---:|---:|---:|
| Chamadas ao modelo | 8 | 8 | 0 |
| Ferramentas executadas | 0 | 0 | 0 |
| Falhas de ferramenta | 0 | 0 | 0 |
| Tokens totais (in+out) | 0 | 0 | 0 |
| Entrada (inclui cache) | 0 | 0 | 0 |
| Cache lido | 0 | 0 | 0 |
| Saida (inclui reasoning) | 0 | 0 | 0 |
| Reasoning | 0 | 0 | 0 |
| Arquivos modificados | 0 | 0 | 0 |
| Arquivos criados | 0 | 0 | 0 |
| Tempo wall soma (ms) | 6792 ms | 11279 ms | -4487 ms |
| Tempo provider soma (ms) | 4184 ms | 4511 ms | -327 ms |
| Tempo tools soma (ms) | 0 ms | 0 ms | 0 ms |

Wall = processo inteiro por braco; provider = soma das latencias informadas pelo harness; tools = soma das duracoes. Residuo = wall-provider-tools: diferenca aritmetica que nao isola startup, pois duracoes de ferramentas podem se sobrepor. Arquivos = diff do workspace vs fixtures originais.
## Por campanha

| Campanha | Cenario | Slim (chamadas/falhas/tokens/wall/arq) | Pi (chamadas/falhas/tokens/wall/arq) |
|---|---|---|---|
| `20260915-213509Z-opencode-go-config_migration` | config_migration | 1/0/0/1677ms/0mod+0novos | 1/0/0/1064ms/0mod+0novos |
| `20260915-213515Z-opencode-go-sqlite_balances` | sqlite_balances | 1/0/0/1651ms/0mod+0novos | 1/0/0/1026ms/0mod+0novos |
| `20260915-213521Z-opencode-go-repo_wide_repair` | repo_wide_repair | 1/0/0/647ms/0mod+0novos | 1/0/0/2167ms/0mod+0novos |
| `20260915-213527Z-opencode-go-json_cli` | json_cli | 1/0/0/627ms/0mod+0novos | 1/0/0/3184ms/0mod+0novos |
| `20260915-213535Z-opencode-go-repo_wide_repair` | repo_wide_repair | 1/0/0/530ms/0mod+0novos | 1/0/0/955ms/0mod+0novos |
| `20260915-213538Z-opencode-go-sqlite_balances` | sqlite_balances | 1/0/0/572ms/0mod+0novos | 1/0/0/972ms/0mod+0novos |
| `20260915-213542Z-opencode-go-config_migration` | config_migration | 1/0/0/533ms/0mod+0novos | 1/0/0/979ms/0mod+0novos |
| `20260915-213545Z-opencode-go-json_cli` | json_cli | 1/0/0/555ms/0mod+0novos | 1/0/0/932ms/0mod+0novos |

## Por ferramenta (uso agregado por braco)

Nenhuma ferramenta registrada.

## Erros de ferramenta (para achar fraqueza)

### slim: 0 falha(s) em 0 execucoes

Nenhuma falha registrada.

### pi: 0 falha(s) em 0 execucoes

Nenhuma falha registrada.

## Por turno (crescimento de contexto)

### `20260915-213509Z-opencode-go-config_migration`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 961 | 548 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 216 | 578 |  |

### `20260915-213515Z-opencode-go-sqlite_balances`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 933 | 514 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 217 | 551 |  |

### `20260915-213521Z-opencode-go-repo_wide_repair`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 1290 | 546 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 239 | 567 |  |

### `20260915-213527Z-opencode-go-json_cli`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 929 | 508 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 235 | 547 |  |

### `20260915-213535Z-opencode-go-repo_wide_repair`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 1290 | 499 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 239 | 571 |  |

### `20260915-213538Z-opencode-go-sqlite_balances`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 933 | 540 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 217 | 563 |  |

### `20260915-213542Z-opencode-go-config_migration`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 961 | 502 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 216 | 570 |  |

### `20260915-213545Z-opencode-go-json_cli`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 929 | 527 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 0 | 0 | 0 | 235 | 564 |  |

## Rastreabilidade

### `20260915-213509Z-opencode-go-config_migration` — config_migration (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213515Z-opencode-go-sqlite_balances` — sqlite_balances (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213521Z-opencode-go-repo_wide_repair` — repo_wide_repair (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213527Z-opencode-go-json_cli` — json_cli (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213535Z-opencode-go-repo_wide_repair` — repo_wide_repair (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213538Z-opencode-go-sqlite_balances` — sqlite_balances (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213542Z-opencode-go-config_migration` — config_migration (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213545Z-opencode-go-json_cli` — json_cli (ordem: ["pi", "slim"])

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
- Instrumentacao assimetrica: Pi via extensao observadora, Slim via ledger/sessao; contagens sao turnos do modelo, nao TCP/retries de transporte. Residuo nao isola startup: duracoes de tools podem se sobrepor.
- history_bytes exclui resultados de tools nos dois bracos; tool_result_bytes os registra separadamente. Campanhas Pi antigas reconstroem bytes do payload com reasoning opaco omitido, portanto seus bytes de historico sao parciais.
- Tokens sao usos informados pelos providers; sem inferencia de custo monetario.
- Outcomes Slim: facts estruturados tool.v1 quando presentes; senao heuristica sobre o texto da tool.
- Falhas por causa: usage-incompleto=8, processo=8
- Gates com problema: 20260915-213509Z-opencode-go-config_migration/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213509Z-opencode-go-config_migration/slim: gate falhou (exit=21, valid=1, fixtures=True); 20260915-213515Z-opencode-go-sqlite_balances/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213515Z-opencode-go-sqlite_balances/slim: gate falhou (exit=21, valid=1, fixtures=True); 20260915-213521Z-opencode-go-repo_wide_repair/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213521Z-opencode-go-repo_wide_repair/slim: gate falhou (exit=21, valid=1, fixtures=True); 20260915-213527Z-opencode-go-json_cli/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213527Z-opencode-go-json_cli/slim: gate falhou (exit=21, valid=1, fixtures=True); 20260915-213535Z-opencode-go-repo_wide_repair/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213535Z-opencode-go-repo_wide_repair/slim: gate falhou (exit=21, valid=1, fixtures=True); 20260915-213538Z-opencode-go-sqlite_balances/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213538Z-opencode-go-sqlite_balances/slim: gate falhou (exit=21, valid=1, fixtures=True); 20260915-213542Z-opencode-go-config_migration/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213542Z-opencode-go-config_migration/slim: gate falhou (exit=21, valid=1, fixtures=True); 20260915-213545Z-opencode-go-json_cli/pi: gate falhou (exit=0, valid=1, fixtures=True); 20260915-213545Z-opencode-go-json_cli/slim: gate falhou (exit=21, valid=1, fixtures=True)
