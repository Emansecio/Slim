# RESULTS — Benchmark v2: Slim x Pi x Pit (tokens + velocidade)

> **TEMPLATE — preencher na execução.** Modelo fixo: `gpt-5.6-luna`,
> thinking `high` para os três. Execução: __/__/____, N runs por cenário.
> Método e limitações: `PLAN.md`.

## 0. Compliance gate

| Agente | modelo no fio | reasoning_effort | ok? |
|---|---|---|---|
| slim | _ | _ | _ |
| pi | _ | _ | _ |
| pit | _ | _ | _ |

⚠️ Qualquer ❌ invalida a comparação daquele agente. Gap conhecido do Slim:
headless não plumva effort (ver PLAN.md) — corrigir ANTES de rodar.

## 1. Tokens — s1_read (baseline leitura)

| Métrica | Slim | Pi | Pit |
|---|---|---|---|
| T1 total | | | |
| T1 system / tools | | | |
| Tn total / histórico | | | |
| Soma de todos os requests | | | |
| ~tokens Tn | | | |

_(repetir blocos por cenário: s2_codegen, s3_multistep, s4_long)_

## 2. Velocidade

| Cenário | Métrica | Slim | Pi | Pit |
|---|---|---|---|---|
| todos | startup_ms (mediana/min) | | | |
| s2_codegen | TTFC_ms (mediana/min) | | | |
| todos | gap entre turnos | | | |
| todos | tempo total do processo | | | |

**Definições**: startup = 1º request − início do processo; TTFC = chegada do
request pós-escrita-do-código − início do processo; gaps = overhead do loop
entre requests; tempo total = fim − início do processo.

## 3. Leitura dos números

_(preencher após execução)_

## Limitações

Ver PLAN.md §Limitações. Destaque: o fixture responde instantâneo, então
"velocidade" aqui = overhead do próprio agente (loop, parsing, execução de
tools), não tokens/s do modelo — que é idêntico por construção.

## 5. A/B TOK-08 — system prompt de economia

O runner aceita `-ExtraPrompt '<instrução>'` + `-VariantTag econ`: a instrução
é anexada ao prompt da tarefa e as capturas ficam sob `slim_econ`, `pi_econ`,
`pit_econ` — comparáveis contra os mesmos agentes sem o sufixo.

```powershell
# variante com instrução anti-desperdício (TOK-08)
powershell -File bench/token-economy/v2/run_benchmark.ps1 -VariantTag econ `
  -ExtraPrompt 'Be token-efficient: cite line ranges instead of re-reading; never re-read a whole file you already read.'
```

| Cenário | Métrica | baseline | com economia | Δ |
|---|---|---|---|---|
| _(preencher)_ | | | | |

Nota do relatório de contexto (Anthropic/OpenAI): instruções de eficiência
**delimitadas** preservam diagnóstico/validação; frases abertas ("tenha
certeza absoluta", "explore possibilidades") induzem desperdício. A instrução
acima segue esse formato delimitado.

Reproduzir:
```powershell
python bench/token-economy/v2/capture_server.py
powershell -File bench/token-economy/v2/run_benchmark.ps1
python bench/token-economy/v2/analyze.py
```
