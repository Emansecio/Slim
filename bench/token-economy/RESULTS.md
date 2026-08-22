# RESULTS — Benchmark de economia de tokens: Slim x Pi x Pit

> Execução de **2026-08-21** com o harness deste diretório (método no README).
> Mesma tarefa, mesmo cwd, mesmo arquivo-alvo, mesmo endpoint de captura
> (OpenAI-compatível) para os três agentes. Bytes medidos no fio; `~tokens` =
> bytes ÷ 4 (**estimativa**).

## Cenário

Tarefa mínima de leitura: cwd temporário com `bench-target.txt` (40 linhas,
~2,7 KB de conteúdo), prompt único "Read bench-target.txt and tell me its
first line.", 2 turnos (tool call de leitura dirigida pelo fixture + resposta
final). Cada agente usou as **próprias tools e defaults**, com descoberta de
contexto externa desligada (`--no-context-files` etc.) para isolar o custo do
agente em si.

## Resultados medidos

| Métrica (bytes no fio) | Slim | Pi | Pit |
|---|---|---|---|
| Turno 1 — total | **1.747** | 5.908 | 11.732 |
| Turno 1 — system prompt | **0** | 2.733 | 3.924 |
| Turno 1 — bloco de tools | **1.592** (6 tools) | 2.900 (4 tools) | 7.434 (12 tools) |
| Turno 2 — total | **4.625** | 8.696 | 14.520 |
| Turno 2 — histórico | 2.953 | 2.888 | 3.017 |
| Maior tool result | 2.582 | 2.471 | 2.471 |
| ~tokens no turno 2 | **~1.156** | ~2.174 | ~3.630 |
| Crescimento T2/T1 | ×2,65 | ×1,47 | ×1,24 |

Tools anunciadas: Slim = `read, list, search, write, patch, shell`;
Pi = `read, bash, edit, write`;
Pit = `ask, bash, edit, exit_plan, fanout, memory_append, parallel, read,
search_tool_bm25, task, todo, write`.

## Leitura dos números

1. **Custo fixo por sessão (turno 1):** Slim é **3,4× menor** que Pi e
   **6,7× menor** que Pit. A diferença vem de dois componentes: Slim não envia
   system prompt (0 B; ver auditoria TOK-08) e mantém o bloco de tools magro.
   Pit carrega 12 tools por default (7.434 B só de schemas) — 4,7× o bloco do
   Slim e 2,6× o do Pi.
2. **Custo marginal por turno (turno 2):** o histórico dos três é
   praticamente igual (2,9–3,0 KB — dominado pelo mesmo arquivo lido). A
   vantagem do Slim no total (~1.156 ~tok vs ~2.174/~3.630) é o custo fixo
   **recorrente**: system prompt + tools são re-enviados a cada request por
   todos os agentes, e os do Slim são os menores.
3. **Crescimento ×2,65 do Slim não é desperdício**: o denominador (turno 1) é
   tão pequeno que qualquer leitura de arquivo domina o ratio. O número
   relevante para economia é o total absoluto por turno, onde Slim vence nos
   dois turnos.
4. Com `cache_control` (TOK-01) no provider Anthropic, o bloco tools/system do
   Slim (já o menor) vira cacheável — o desconto absoluto é menor que o dos
   concorrentes, mas o custo pós-cache permanece o menor dos três.

## Limitações

- Um único cenário (leitura simples, 2 turnos). Cenários de escrita/patch e
  sessões longas devem ser adicionados ao harness antes de generalizações.
- `~tokens` é estimativa (÷4); tokenizers reais variam por agente/modelo.
- Pi e Pit medidos no formato `openai-completions` com defaults de tools;
  `--profile minimal` do Pit (não medido) reduziria seu bloco de tools.
- O fixture dirige a tool call (determinismo); agentes reais podem variar o
  número de turnos por tarefa, o que domina o custo total em uso real.

Reproduzir: `python capture_server.py` + `powershell -File run_benchmark.ps1`
+ `python analyze.py`.
