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

## Status de implementação (slice TOK, 2026-08-21)

Implementados **nativamente como default** (sem flags), comprovados por
medição localhost pós-deploy e suíte 45/212 verde:

| Item | Estado | Evidência |
|---|---|---|
| TOK-01 | ✅ implementado | `cache_control: {"type":"ephemeral"}` só na última tool do payload Anthropic (req capturada); testes `anthropic_request_marks_last_tool_with_cache_control` |
| TOK-03 | ✅ implementado | leitura repetida caiu de 17.409 B para **68 B** no fio (`[duplicate read result omitted…]`); teste `duplicate_tool_output_goes_on_the_wire_as_a_pointer…` |
| TOK-04 | ✅ implementado | schema do `read` ganhou `offset`; páginas truncadas trazem footer `[showing lines X-Y of Z; pass "offset": N…]`; teste de paginação |
| TOK-05 | ✅ implementado | estimador ÷3,5 conservador (`(chars*2).div_ceil(7)`) |
| TOK-10 | ✅ implementado | cap de 8 KiB por stream stdout/stderr com `[truncated N bytes]` |
| TOK-11 | ✅ implementado | instrução do resumo movida para DEPOIS do transcript |
| TOK-12 | ✅ implementado | evento `ContextSnapshot {tools_bytes, history_bytes}` a cada turno (session log; TUI ignora) |

Pendentes (próximos slices, exigem telemetria acumulada primeiro):
TOK-02, TOK-06, TOK-07, TOK-08. Reavaliação: TOK-09 permanece aberto com
ganho marginal (descrições já ≤ 8 palavras).

## Benchmark comparativo (Slim x Pi x Pit)

`bench/token-economy/` mede o payload real no fio dos três agentes na mesma
tarefa contra fixture localhost. Resultado de 2026-08-21: Slim vence nos dois
turnos (T1 1.747 B vs 5.908/11.732; T2 ~1.156 ~tok vs ~2.174/~3.630) — ver
`bench/token-economy/RESULTS.md`.

## Números-base medidos (resumo)

| Medição | Valor |
|---|---|
| Payload turno 1 (OpenAI-compatível, Auto) | 1.667 B, dos quais tools = 1.552 B |
| Payload turno 1 (ReadOnly, 3 tools) | 806 B, tools = 692 B |
| Payload turno 1 (Anthropic, 6 tools direto) | 1.493 B, tools = 1.378 B |
| Payload turno 2 (com leitura de 500 linhas) | 19.809 B; tool result = **17.341 B (87,5%)**; tools re-enviadas = 1.552 B |
| System prompt | **0 B** (Anthropic/OpenAI); 33 B no Codex |
| Prompt caching Anthropic | **inexistente** (sem `cache_control` no payload) |
| Cache HTTP local no produto | **inativo** (`HttpProviderClient::new` em headless.rs:294,324,343) |

Evidências principais: `headless.rs:274–275`, `provider.rs:1688–1710`,
`codex.rs:88`, `tools/mod.rs:91–97,173–177,247–291`, `tools/read.rs:8–9`,
`runtime/mod.rs:76–77,352–403,1254–1278`, `budget.rs:17–27`, `compact.rs:3–6`.

---

## Backlog priorizado (impacto ÷ esforço)

### TOK-01 — Prompt caching Anthropic (`cache_control`)
- **Selo:** COMPROVADO (hoje = zero caching; grep negativo em `src/`)
- **Proposta:** injetar `cache_control` marcando o bloco de tools e o início
  estável das messages no `build_messages_request_with_tools` do
  `AnthropicAdapter`.
- **Ganho:** até ~90% de desconto no input dos turnos 2+ (~estimativa,
  depende do pricing do endpoint). Maior vitória financeira da lista.
- **Esforço:** S · **Risco:** Baixo (campo opcional; validar que endpoints
  sem suporte ignoram sem erro 400).

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

### TOK-08 — System prompt mínimo anti-desperdício
- **Selo:** ESPECULATIVO
- **Proposta:** ~150 tokens condicionais ensinando economia ("cite
  `linha:N-M`; releia trechos em vez de colar; não releia o arquivo todo").
  Hoje NÃO há system prompt algum (`headless.rs:274–275`; adapters sem campo
  `system`; só Codex tem 1 linha, `codex.rs:88`).
- **Ganho:** indireto — ataca o consumidor nº 1 (leituras redundantes).
- **Esforço:** S · **Risco:** Médio (exige A/B de qualidade).

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
- Não há system prompt nem few-shots hoje (verificado). Se TOK-08 criar um,
  manter exemplos fora.

### TOK-19 — Modo "contexto frio" para tarefas longas — 🔬 EXPERIMENTO FUTURO
- Iniciar sessão com resumo + só o arquivo-alvo; agente puxa o resto sob
  demanda. Depende de TOK-02 e TOK-04 maduros primeiro.

---

## Contabilidade

- **Entradas registradas:** 21 (15 do backlog + 6 da seção fora-da-caixa).
- **Ideias distintas:** **19** (TOK-08 e TOK-15 apareceram duplicadas nas duas
  seções do relatório original; consolidadas aqui).
- **Status:** 9 acionáveis curtas (TOK-01, 03, 04, 05, 08, 09, 10, 11, 12),
  3 médias (TOK-02, 06, 07), 2 adiadas (TOK-14, 16), 1 experimento futuro
  (TOK-19), 4 descartadas com justificativa (TOK-13, 15, 17, 18).
- **Quick wins <1 dia:** TOK-01, TOK-03, TOK-05, TOK-12.

## Convenção de uso

Todo slice que implementar um item deve referenciar o ID (`TOK-NN`) na linha
correspondente do §7 do `AUDIT-SLIM-TUI-TRACKER.md`, e atualizar o status
neste arquivo. Itens COMPROVADO têm números de referência acima; re-medir
contra fixture localhost após implementar (método da sessão de auditoria:
servidor SSE em `%TEMP%\slim-audit\capture.py` + análise de payload).
