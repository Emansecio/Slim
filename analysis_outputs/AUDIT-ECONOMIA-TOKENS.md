# AUDIT-ECONOMIA-TOKENS.md — Backlog consolidado de economia de tokens

> Origem: auditoria executada em **2026-08-21** contra `target\release\slim.exe`
> (build 19:47) com captura real de payload via fixture localhost.
> Método, números medidos e evidências completas: ver resposta da sessão de
> auditoria. Este arquivo é o registro persistente dos itens; cada item tem ID
> estável `TOK-NN` para referência em slices do tracker (§7).
>
> Selo de confiança: **COMPROVADO** (medido nesta sessão) / **PROVÁVEL**
> (raciocínio sólido sobre o código) / **ESPECULATIVO** (vale experimentar).
> Esforço: S (<1 dia) / M (dias) / L (semanas).
>
> **Checkpoint atual (2026-08-22):** números históricos abaixo não são baseline
> do binário atual. A medição v2 e PERF-02 estão em
> `bench/token-economy/v2/RESULTS.md` e prevalecem quando houver divergência.

## Status de implementação (slice TOK, 2026-08-21)

Implementados **nativamente como default** (sem flags), comprovados por
medição localhost pós-deploy e suíte atual de 45 suítes / 215 passed /
0 failed / 1 ConPTY físico ignored:

| Item | Estado | Evidência |
|---|---|---|
| TOK-01 | ✅ implementado | `cache_control: {"type":"ephemeral"}` só na última tool do payload Anthropic (req capturada); testes `anthropic_request_marks_last_tool_with_cache_control` |
| TOK-03 | ✅ implementado | leitura repetida caiu de 17.409 B para **68 B** no fio (`[duplicate read result omitted…]`); teste `duplicate_tool_output_goes_on_the_wire_as_a_pointer…` |
| TOK-04 | ✅ implementado | schema do `read` ganhou `offset`; páginas truncadas trazem footer `[showing lines X-Y of Z; pass "offset": N…]`; teste de paginação |
| TOK-05 | ✅ implementado | estimador ÷3,5 conservador (`(chars*2).div_ceil(7)`) |
| TOK-10 | ✅ implementado | cap de 8 KiB por stream stdout/stderr com `[truncated N bytes]` |
| TOK-11 | ✅ implementado | instrução do resumo movida para DEPOIS do transcript |
| TOK-12 | ✅ implementado | evento `ContextSnapshot {tools_bytes, history_bytes}` a cada turno (session log; TUI ignora) |
| TOK-08 | ✅ implementado | `NATIVE_SYSTEM_PROMPT` compacto injetado em todos os adapters; `s1_read` atual mede 2.435 B de system |

Pendentes: TOK-02 e TOK-06 exigem telemetria real persistida; TOK-07 continua
especulativo pelo risco de turno extra/capability ausente. TOK-09 permanece
aberto com ganho marginal (descrições já curtas).

## Benchmark comparativo atual (Slim x Pi x Pit)

O benchmark autoritativo é `bench/token-economy/v2/`. No cenário `s1_read`,
modelo `gpt-5.6-luna` e effort `high`, as medianas atuais são:

| Medição | Slim | Pi | Pit |
|---|---:|---:|---:|
| T1 total | 4.238 B | 5.941 B | 11.870 B |
| System | 2.435 B | 2.739 B | 4.029 B |
| Tools | 1.592 B (6) | 2.900 B (4) | 7.434 B (12) |
| Soma dos requests | 11.354 B | 14.670 B | 26.528 B |

No `s4_long` (15 runs de Slim), T1 = 4.239 B, Tn = 8.519 B,
histórico Tn = 4.350 B e soma = 35.511 B. PERF-02 manteve **75/75 bodies
byte-idênticos** antes/depois. O baseline v1 de 2026-08-21 fica preservado em
`bench/token-economy/RESULTS.md` apenas como histórico.

Estado atual relevante:

- system prompt nativo presente nos adapters OpenAI, Anthropic e Codex;
- prompt caching Anthropic presente na última tool;
- cache HTTP do produto continua inativo nos construtores normais;
- tools internas serializadas medem 1.418 B; o wire OpenAI mede 1.592 B após o
  envelope do adapter.

---

## Backlog priorizado (impacto ÷ esforço)

### TOK-01 — Prompt caching Anthropic (`cache_control`) — ✅ IMPLEMENTADO
- **Selo:** COMPROVADO no payload capturado e por teste dedicado.
- **Estado:** a última tool recebe `cache_control: {"type":"ephemeral"}`.
- **Limite:** economia financeira depende do endpoint/pricing real; o fixture
  localhost comprova o wire, não desconto de cobrança.

### TOK-02 — Degradar tool outputs antigos (placeholder após N turnos)
- **Selo:** PROVÁVEL
- **Proposta:** manter íntegras apenas as últimas K leituras; antigas viram
  `[arquivo X, lido no turno T, N linhas — releia se precisar]`.
- **Ganho:** 60–80% do histórico em sessões longas (~estimativa).
- **Esforço:** M · **Risco:** Médio (exigir telemetria TOK-12 antes).

### TOK-03 — Deduplicação de leitura repetida
- **Selo:** PROVÁVEL
- **Proposta:** hash do output de `read`; leitura idêntica à anterior vira
  placeholder + hint. Hoje o loop empurra o resultado sem checagem
  (`runtime/mod.rs:460–466`).
- **Ganho:** elimina o caso "leu o mesmo arquivo 3× inteiro".
- **Esforço:** S/M · **Risco:** Baixo.

### TOK-04 — Range requests no `read` (`offset`/`limit` + `total_lines`)
- **Selo:** PROVÁVEL
- **Proposta:** acrescentar `offset` ao schema (hoje só `max_lines` desde a
  linha 1, `tools/mod.rs:175`; `tools/read.rs`) e retornar `total_lines` na
  primeira resposta para o modelo paginar consciente.
- **Ganho:** evita ler 500 linhas para usar 20; complementa TOK-02.
- **Esforço:** S · **Risco:** Baixo (mudança de schema — conferir spec DESIGN antes).


### TOK-05 — Estimador de tokens honesto
- **Selo:** PROVÁVEL
- **Proposta:** substituir `chars÷4` (`compact.rs:3`) por fator por classe
  (código ≈ ÷3,3) ou contador BPE leve.
- **Ganho:** corretude — evita estouro real de janela antes do gatilho de 85%.
- **Esforço:** S · **Risco:** Baixo.

### TOK-06 — Compaction progressiva
- **Selo:** PROVÁVEL
- **Proposta:** degradar tool outputs grandes quando histórico > 50% da
  janela, antes do gatilho duro atual (85%, `budget.rs:17–23`). A compaction
  hoje é all-or-nothing (`runtime/mod.rs:505–583`).
- **Ganho:** atrasa a compaction cara e preserva qualidade do contexto.
- **Esforço:** M · **Risco:** Médio.

### TOK-07 — Seleção dinâmica de tools por turno
- **Selo:** ESPECULATIVO
- **Proposta:** enviar write/patch/shell só quando a tarefa exige. Base já
  existe: filtro por modo em `tools/mod.rs:91–97` (medido: 1.552 B Auto →
  692 B ReadOnly).
- **Ganho:** ~55% do bloco tools/turno.
- **Esforço:** M · **Risco:** Médio (turno extra se prever errado).

### TOK-08 — System prompt nativo compacto — ✅ IMPLEMENTADO
- **Selo:** COMPROVADO no wire dos três adapters.
- **Estado:** `NATIVE_SYSTEM_PROMPT` está ativo; `s1_read` mede 2.435 B.
- **Próximo passo:** nenhum corte adicional sem provar pelo menos 300 B de
  economia e zero perda semântica em A/B.

### TOK-09 — Encurtar schemas/descrições das tools
- **Selo:** COMPROVADO (custo atual medido por tool)
- **Proposta:** descriptions ≤ 6 palavras; schemas mínimos. Já são magras
  (`tools/mod.rs:247–291`); resta ~10–20%.
- **Ganho:** ~15% de ~390 tok/turno.
- **Esforço:** S · **Risco:** Baixo.

### TOK-10 — Cap separado e menor para output de `shell`
- **Selo:** PROVÁVEL
- **Proposta:** cap próprio < 64 KiB (`AgentLoopConfig.max_result_bytes`,
  `runtime/mod.rs:76`); artifact store já serve de fallback.
- **Ganho:** evita picos de ~16k tok por comando verboso.
- **Esforço:** S · **Risco:** Baixo.

### TOK-11 — Resumo de compaction com prefixo cacheável
- **Selo:** PROVÁVEL
- **Proposta:** mover o prefixo de instrução (`compact.rs:6`) para o fim do
  prompt de resumo, mantendo o transcript como prefixo estável.
- **Ganho:** habilita cache do transcript no request de resumo.
- **Esforço:** S · **Risco:** Baixo.

### TOK-12 — Telemetria de breakdown por seção do payload
- **Selo:** — (pré-requisito)
- **Proposta:** evento de usage com bytes/tokens por seção (tools, histórico,
  último resultado). O evento `Usage` já existe.
- **Ganho:** comprova TOK-01…TOK-11 com números do produto real.
- **Esforço:** S · **Risco:** Zero.

### TOK-13 — Reusar `ProviderCache` no produto — ❌ DESCARTADO
- **Motivo:** replay de resposta não reduz tokens cobrados; semanticamente
  errado para agente (mesmo prompt merece nova execução). Manter inativo
  (estado atual, `headless.rs:294,324,343`) e documentar a decisão.

### TOK-14 — Destilação de sessões antigas em memória LT — ⏸ ADIAR
- **Motivo:** alto valor a longo prazo, mas o produto ainda não tem resume de
  sessão na CLI/TUI (README §Sessões). Pré-condição: resume + telemetria.
- **Esforço:** L.

### TOK-15 — Protocolo binário/comprimido no transporte — ❌ DESCARTADO
- **Motivo:** billing conta tokens sobre o texto reconstruído, não os bytes
  transferidos; compressão economiza banda, não tokens.

---

## Ideias fora da caixa (vereditos)

### TOK-16 — Compressão semântica via modelo menor — ⏸ ADIAR
- A compaction self-hosted já faz isso com o próprio provider
  (`compact_before_send`). Segundo modelo = infra nova antes de o produto ter
  resume sequer. Revisitar após TOK-12 mostrar o custo real da compaction.

### TOK-17 — Budget negotiation (agente pede mais contexto) — ❌ DESCARTADO
- Providers não expõem essa primitiva. Equivalente simples: TOK-06.

### TOK-18 — Few-shot examples sob demanda — ❌ DESCARTADO (nada a remover)
- O system prompt nativo atual não contém few-shots. Manter exemplos fora.

### TOK-19 — Modo "contexto frio" para tarefas longas — 🔬 EXPERIMENTO FUTURO
- Iniciar sessão com resumo + só o arquivo-alvo; agente puxa o resto sob
  demanda. Depende de TOK-02 e TOK-04 maduros primeiro.

---

## Contabilidade

- **Entradas registradas:** 21 (15 do backlog + 6 da seção fora-da-caixa).
- **Ideias distintas:** **19** (TOK-08 e TOK-15 apareceram duplicadas nas duas
  seções do relatório original; consolidadas aqui).
- **Implementadas:** TOK-01, 03, 04, 05, 08, 10, 11 e 12.
- **Aberta marginal:** TOK-09.
- **Pendentes com evidência insuficiente/risco:** TOK-02, 06 e 07.
- **Adiadas:** TOK-14 e 16; **experimento futuro:** TOK-19.
- **Descartadas:** TOK-13, 15, 17 e 18.

## Slices de performance relacionados

| Item | Estado | Evidência |
|---|---|---|
| PERF-01 | ✅ benchmark versionado | `tool_setup`: 11 × 20.000 iterações; setup repetido 9.305,065 ns, cacheado 0,730 ns |
| PERF-02 | ✅ implementado e medido | setup das tools uma vez por agent loop; ~9,3 µs poupados por turno adicional; 75/75 payloads idênticos |

O A/B E2E `s4_long` variou de 1.168 para 1.170 ms de processo mediano
(+0,17%), ruído esperado num harness dominado por `Start-Job`. Não há claim de
ganho visível ao usuário.

## Convenção de uso

Todo slice que implementar um item deve referenciar o ID (`TOK-NN`) na linha
correspondente do §7 do `AUDIT-SLIM-TUI-TRACKER.md`, e atualizar o status
neste arquivo. Itens COMPROVADO têm números de referência acima; re-medir
contra fixture localhost após implementar (método da sessão de auditoria:
servidor SSE em `%TEMP%\slim-audit\capture.py` + análise de payload).
