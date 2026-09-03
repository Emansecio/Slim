# Compatibilidade do output cap no OpenAI Codex

**Data:** 2026-08-23  
**Status:** implementado, verificado e implantado em 2026-08-23  
**Origem:** erro real da TUI `HTTP 400: Unsupported parameter: 'max_output_tokens'`

## Objetivo

Restaurar requests do Slim via assinatura ChatGPT Plus/Pro sem enfraquecer o
budget local de contexto e sem alterar outros providers.

## Referência Pi

A implementação instalada do Pi em
`@earendil-works/pi-ai/dist/api/openai-codex-responses.js` usa um contrato
específico para `https://chatgpt.com/backend-api/codex/responses`.
`buildRequestBody()` envia model, store, stream, instructions, input, text,
include, cache, tools e reasoning, mas deliberadamente não envia
`max_output_tokens`. O adapter OpenAI Responses comum continua podendo enviar
o campo; os dois contratos não são intercambiáveis.

## Decisão

1. Remover `max_output_tokens` somente do JSON criado por
   `OpenAiCodexAdapter::request`.
2. Manter `ProviderConfig::max_output_tokens` e
   `ProviderRunOptions::max_output_tokens` inalterados.
3. Continuar usando o valor como `AgentLoopConfig::context_reserve_tokens` para
   orçamento e compaction locais.
4. Manter OpenAI-compatible (`max_tokens`) e Anthropic (`max_tokens`)
   inalterados.
5. Não adicionar retry após HTTP 400 nem allowlist por modelo.

## Fluxo

`SLIM_MAX_OUTPUT_TOKENS`/opção explícita → validação positiva → reserva local de
contexto → adapter Codex constrói body sem output cap → backend escolhe seu
limite suportado → usage/stop terminal seguem pelo parser existente.

## Testes

- RED: alterar regressão do adapter para exigir ausência de
  `max_output_tokens`; código atual falha porque envia `321`.
- GREEN: remover uma propriedade do body Codex.
- Preservar teste do cap explícito em OpenAI-compatible.
- Adicionar/ajustar fixture HTTP Codex que rejeita o campo se presente e retorna
  stream válida se ausente, cobrindo o caminho real de envio.
- Rodar teste focado, suíte `slim-core`, workspace, Clippy e deploy canônico.

## Documentação

Registrar novo bug no tracker antes da correção. Corrigir a conclusão histórica
G129: o contrato público de output cap permanece local para Codex, não implica
campo no wire da assinatura. Atualizar checkpoint e números do gate final.

## Fora de escopo

- mudar output cap de OpenAI API-key ou Anthropic;
- retry adaptativo por mensagem textual de erro;
- alterar modelo, effort, OAuth, URL, headers ou parser SSE;
- prometer limite remoto que o backend Codex não oferece nesse contrato.

## Critérios de aceite

- nenhum request Codex contém `max_output_tokens`;
- reserva local configurada continua ativa;
- request da screenshot deixa de receber o HTTP 400 por esse parâmetro;
- outros providers preservam seus campos atuais;
- testes, Clippy, deploy e smoke ficam verdes.

## Resultado executado

RED confirmou o campo no JSON do adapter e nos dois requests da fixture Codex.
A correção removeu uma única propriedade de `OpenAiCodexAdapter`; o reserve
local permaneceu intacto. Gate final: 63 suítes / 566 testes, 0 falhas, 0
compiler warnings e 1 ConPTY físico ignorado; Clippy `slim-core --lib` e TUI,
fmt e diff-check verdes. `refresh-slim.ps1 -Test` imprimiu `OK:` e implantou o
mesmo SHA-256 em release/PATH.
