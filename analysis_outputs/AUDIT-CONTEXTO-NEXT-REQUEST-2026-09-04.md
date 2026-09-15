# Auditoria — tool result → persistência → contexto → próxima request

Data: 2026-09-04  
Checkout: `D:\Slim`  
HEAD medido: `972b46e4e445de05b4021554b277710f3cfe9243`  
Provider fixado: Codex / `gpt-5.6-luna` / high

## Decisão

Nenhuma alteração de runtime nesta rodada. Não há ganho comprovado que justifique mudar persistência, estimativa, serialização, compactação, LSP, search ou prompt global.

## Evidência agentic existente

Traces em `bench/agentic-code-intel/` foram analisados sem reexecução:

- 10 execuções, 10/10 corretas.
- 150 tool calls, 58 batches/rodadas, 68 provider turns.
- 436,282 s de parede; 427,335 s em provider; 7,766 s em ferramentas.
- 305,976 input tokens e 16,511 output tokens.
- 68 requests: contexto `history_bytes + tool_result_bytes` mediano 14,780 B; máximo 57,194 B; soma 1,199,738 B.
- `code_intel`: 0 chamadas. Não há telemetria de requests LSP nesta amostra.

Referências: `results/summary.json`, `results/*/metrics.json`, `manual-review.json`, `README.md`.

## Pipeline atual confirmado

1. `run_agent_loop_inner` monta definições de workspace e calcula preflight estrutural O(histórico) (`crates/slim-core/src/runtime/mod.rs:1405`).
2. Quando preflight cabe, prepara request completa uma vez; o objeto preparado é reutilizado para budget, snapshot e transporte. Não há segunda serialização normal da mesma rodada.
3. `append_conversation_message` redige, registra no journal, atualiza transcript/conversation e adiciona mensagem local (`runtime/mod.rs:806`).
4. Eventos `ToolStarted`, `ToolOutput` e `ToolFinished` passam pelo journal; persistência usa `sync_data`, preservando durabilidade antes da próxima fronteira externa.
5. Resultados acima de `max_result_bytes` são enviados ao artifact store, mas o resultado recuperável continua disponível; `prompt_output` reinsere somente preview + referência (`runtime/mod.rs:4330`).
6. O adapter Codex constrói body JSON e serializa uma vez por request preparada (`crates/slim-core/src/provider.rs:1761`). Componentes de system/schema/history/result são registrados no snapshot.

## Repetição observada nos traces

Classificação manual cobre todas as 150 chamadas (`manual-review.json`):

- 43 leituras necessárias.
- 37 reconsultas/validações justificadas.
- 24 mutações necessárias.
- 17 explorações pertinentes.
- 14 leituras informativas, nem todas evitáveis.
- 5 chamadas estritamente dispensáveis/redundantes: uma shell de validação repetida, uma busca pós-estado sem mudança pertinente, dois reads sem estado novo e um patch com precondição já satisfeita.
- 1 busca parcialmente sobreposta.

Catálogo irrelevante apareceu em 4/10 execuções, 25,698 B no total. Caso mais claro de `search → read` redundante: C-2, 442 B. Isso é escolha do modelo/fixture; não demonstra falha determinística do runtime.

Oito erros foram observados. Todos tiveram causa de fixture/comando ou precondição e recuperação local: glob de rlib, ausência de Git, `$null`, diretório ausente, arquivo ainda não criado e expected inexistente. Nenhum indica perda de estado, request duplicada pelo runtime ou falha de durabilidade.

## Medição direta release, sem provider/rede

Fixture existente `runtime::performance::native_dispatch_and_next_request`:

| fase | mediana |
|---|---:|
| intent durável | 1.891 ms |
| preparar/agendar ferramenta | 0.194 ms |
| leitura nativa | 0.094 ms |
| residual incluindo journal do resultado | 1.909 ms |
| próxima request com contexto | 2.853 ms |
| sequência equivalente total | 7.056 ms |

Perfil temporário, removido após medição, mostrou crescimento linear da preparação:

| mensagens | body | preflight | prepare | preflight + prepare |
|---:|---:|---:|---:|---:|
| 10 | 20,397 B | 0.014 ms | 0.218 ms | 0.232 ms |
| 66 | 183,955 B | 0.101 ms | 0.930 ms | 0.983 ms |
| 258 | 1,039,023 B | 0.589 ms | 3.704 ms | 4.523 ms |
| 514 | 2,064,047 B | 1.142 ms | 7.630 ms | 8.768 ms |

Reconstrução de schema ficou em 0.043–0.052 ms. Redação/clonagem, usada na entrada/finalização e não em cada rodada normal, ficou em 0.003–0.697 ms. Os contextos reais medidos, até 57,194 B, ficam muito abaixo do ponto onde essa serialização seria candidata forte.

Perfil temporário de materialização de resultado grande:

| saída | materialização | prompt reinserido |
|---:|---:|---:|
| 8 KiB | 0.001 ms | 8,199 B; sem artifact |
| 64 KiB | 0.353 ms | 16,665 B |
| 256 KiB | 0.648 ms | 16,666 B |
| 1 MiB | 1.197 ms | 16,667 B |

Os traces agentic atuais não exercitam saída individual acima do limite de 16 KiB; essa tabela é apenas limite manual local, não evidência de gargalo em produção.

## Por que não otimizar

- Provider domina tempo observado: 427.335 s contra 7.766 s de ferramentas em 10 execuções.
- Contextos reais são pequenos para o custo medido de preflight/serialização.
- Schema cache, request incremental ou remoção de clones teriam risco de invalidar workspace definitions, redaction, cache key, snapshots ou durabilidade, sem economia observada suficiente.
- Alterar journal/sync para agrupar eventos reduziria garantia de recuperação e não foi autorizado pelo contrato.
- Converter `search` em navegação semântica não é suportado pela evidência: `code_intel` não foi exercitado e os casos textuais usaram `search` corretamente.
- Não existe baseline A/B que isole modelo, fixture e instrução; otimização de seleção agentic não pode ser atribuída ao runtime.

## Verificação executada

- `cargo --config build.rustc-wrapper="" test -p slim-core --release --lib runtime::performance::native_dispatch_and_next_request -- --ignored --nocapture` — passou.
- Perfil temporário release de contexto — passou; código de medição removido.
- Perfil temporário release de materialização 8 KiB–1 MiB — passou; código de medição removido.
- `provider::performance::request_preparation_costs` — falhou no assert histórico de hash (`left: 7875841342025817596`, `right: 3778339155770243714`); não foi alterado, pois o fixture esperado está stale em relação ao checkout existente.
- SHA-256 de `crates/slim-core/src/runtime/performance.rs` e `crates/slim-core/src/provider/performance.rs` voltou exatamente ao valor anterior às medições.
- Oracle/harness agentic já aprovados no ciclo anterior: critérios objetivos 10/10; não foram reexecutados.

## Limites e próximo experimento

- Sem A/B reproduzível.
- Sem chamadas reais `code_intel`/LSP.
- Sem saída grande nos traces reais.
- Próxima campanha útil: fixtures semânticas difíceis (trait/reexport, consumidores distribuídos), mesmas Luna/high/configuração, e uma única alteração de contrato por vez. Só implementar após padrão recorrente.

## RULES §4

- Evidência e referências: aplicável; listadas acima.
- Comandos, resultados e falha: aplicável; listados acima.
- Medições atuais versus históricas: aplicável; separados explicitamente.
- Deploy/binário: não aplicável; nenhum deploy autorizado ou executado.
- Documentação: este relatório criado; nenhum documento de status preexistente alterado.
- Incertezas/limites: aplicável; seção acima.
