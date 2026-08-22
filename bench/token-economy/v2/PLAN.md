# PLAN — Benchmark v2: economia de tokens + velocidade (Slim x Pi x Pit)

Status: **estrutura preparada, NÃO executada** (decisão do usuário, 2026-08-21).
Modelo fixo para todos os agentes: **gpt-5.6-luna**, thinking **HIGH**.

## Objetivos

1. Robustez: múltiplos cenários, N repetições, compliance gate, tempos medidos.
2. Novas dimensões: **velocidade de início** (startup até o 1º request) e
   **tempo até o primeiro código gerado** (TTFC) e duração total da tarefa.

## Cenários

| ID | Nome | Turnos | Fluxo dirigido pelo fixture | O que mede |
|---|---|---|---|---|
| s1_read | leitura baseline | 2 | read → stop | custo fixo (system+tools), igual ao v1 |
| s2_codegen | geração de código | 2 | write fizzbuzz.py (~25 linhas) → stop | TTFC, custo do fluxo de escrita |
| s3_multistep | escrever + verificar | 3 | write → read-back → stop | custo/latência de ciclo com verificação |
| s4_long | sessão longa | 5 | read → read → write → read → stop | crescimento de histórico acumulado |

O fixture continua **adaptativo**: descobre as tools de cada agente pelo array
`tools` do próprio request (regex por nome + chaves de schema), então cada
agente usa as próprias ferramentas — nenhum formato é hardcodado.

## Métricas novas (timing)

Gravados em `captures/<cenario>/run<N>/<agente>/req_<k>.meta.json`
(epoch ms + monotonic do servidor) e `runs/<cenario>_run<N>_<agente>_timing.json`
(início/fim do processo, medidos no runner):

- `startup_ms` = chegada do request 1 − início do processo → **latência de início**
- `ttfc_ms` = chegada do request 2 − início do processo (no s2/s3/s4 o request 2
  só acontece depois de o agente ter escrito o código no disco) → **tempo até o
  primeiro código gerado**
- `total_ms` = fim do processo − início
- `turn_gap_ms` = intervalo entre requests consecutivos (overhead de loop)
- `sum_bytes_all_requests` = custo total de contexto da tarefa inteira
  (soma dos payloads), não só o último turno

## Pinagem do modelo + compliance gate

Todos rodam contra o fixture com model `gpt-5.6-luna` e expecting
`reasoning_effort: "high"` no body. O servidor grava cada request em
`compliance.jsonl` com: model declarado, campo de reasoning encontrado
(`reasoning_effort` / `reasoning.effort` / ausente). O `analyze.py` emite tabela
de compliance; **qualquer agente sem `high` aparece como violação** — o número
dele não pode ser comparado até corrigir.

Configuração por agente:

- **Slim**: `--model gpt-5.6-luna`. ⚠️ **PRÉ-REQUISITO (gap verificado no
  código)**: o caminho headless da CLI constrói `ProviderRunOptions::default()`
  (`crates/slim-cli/src/cli.rs`) e não consome `effort` do `slim.toml` nem env;
  só o TUI lê `SLIM_EFFORT` (`crates/slim-cli/src/tui.rs:109`). O adapter já
  suporta enviar o campo (`slim-core/src/provider.rs:1409`). Patch mínimo
  sugerido (decidir antes de rodar): aceitar `SLIM_EFFORT` (ou consumir
  `layered_config.effort`) em `run_cli` e repassar via
  `ProviderRunOptions::with_reasoning_effort`. Sem isso, o gate marca o Slim.
- **Pi**: `models.json` isolado com modelo `gpt-5.6-luna`,
  `"reasoning": true` + `"thinkingLevelMap": {"high": "high"}`; CLI com
  `--thinking high` (docs/usage.md do Pi).
- **Pit**: mesmo models.json; CLI com `--thinking high`
  (`packages/coding-agent/src/cli/args.ts:282`).

## Repetições e agregação

`-Runs N` (default 3) por cenário×agente. O `analyze.py` reporta **mediana** e
mínimo (para timing, mediana é o resumo; min expõe o melhor caso). Capturas de
runs individuais ficam preservadas para auditoria.

## A/B TOK-08 — instrução de economia no prompt

O relatório de context engineering (Anthropic "new rules of context
engineering", OpenAI model guidance) muda o formato do TOK-08 original: não é
um **system prompt** de ~150 tokens, e sim uma instrução **delimitada** anexada
à tarefa. Frases abertas ("tenha certeza", "explore possibilidades") induzem
desperdício medido (2,4–7,4× reasoning sem ganho); instruções delimitadas com
condição de parada preservam qualidade.

O runner suporta isso nativamente:

```powershell
# baseline (já coberto pela execução padrão)
powershell -File bench/token-economy/v2/run_benchmark.ps1

# variante TOK-08: mesma matriz, prompt com instrução delimitada
powershell -File bench/token-economy/v2/run_benchmark.ps1 `
  -VariantTag econ `
  -ExtraPrompt 'Be token-efficient: cite line ranges instead of re-reading; never re-read a whole file you already read.'
```

Com `-VariantTag econ`, as capturas ficam sob `slim_econ`/`pi_econ`/`pit_econ`
e o `analyze.py` as trata como linhas separadas, comparáveis contra os mesmos
agentes na execução baseline.

## Limitações conhecidas (herdadas + novas)

- Fixture responde instantâneo: a "velocidade de geração" medida aqui é
  **overhead do agente** (loop, parsing, execução de tool), não tokens/s do
  modelo — o modelo é o mesmo para todos por construção. Para tokens/s reais,
  seria preciso endpoint real (fora do escopo desta versão).
- TTFC inclui a execução da tool de escrita do agente (disco local), que é o
  próprio objeto da medição (overhead incluído de propósito).
- Cenários continuam determinísticos via fixture; nº de turnos é controlado.
- `~tokens` segue sendo estimativa (÷4).

## Arquivos

```
v2/
├── PLAN.md              ← este arquivo
├── README.md            ← método e execução
├── capture_server.py    ← fixture v2 (multi-cenário, timing, compliance)
├── run_benchmark.ps1    ← runner v2 (matriz cenário×run×agente)
├── analyze.py           ← análise v2 (tokens + timing + compliance)
└── RESULTS.template.md  ← esqueleto para a próxima execução
```

A v1 (`../`) permanece intocada para comparação histórica.
