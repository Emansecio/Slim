# Documentação do Slim — índice canônico

> Limpeza de 16/09/2026: removidos os módulos isolados de observabilidade e o
> scheduler legado. O journal produtivo e o lifecycle durável de child no
> serviço de capabilities permanecem. Os checkpoints abaixo são históricos;
> a retirada de APIs está registrada no [checkout](../README.md).

> **Status de implementação: checkpoint de integração parcial; v1 ainda não
> concluída.** Atualizado em: 2026-08 (execução do tracker de auditoria TUI).
>
> Headless e TUI fullscreen usam provider, agent loop e tools reais. `Slim` abre
> a TUI normal mesmo deslogado; `/login` conecta OAuth nativo Claude Pro/Max ou
> ChatGPT Plus/Pro. `Slim --headless` seleciona a CLI headless. Na TUI: reducer
> único normativo, scrollback virtualizado/navegável, paleta §21.3 estratificada,
> lanes bounded com coalescer real, tool blocks tipados, rails operacionais,
> composer boxed, command palette, motion básico, proptest/fault/golden e bench
> com gate (p95 W8 2,384 ms ≤ 16 ms). O harness v2 concluiu resume/observabilidade
> e a capability bridge durável para Skill/MCP/child/Todo/Plan/Goal. A TUI também
> integra inspectors responsivos, busca no transcript, cópia nativa, anexos locais,
> composer adaptativo e código/diff semântico. Pendente: gate físico Windows
> (PTY/console real), projeção completa de Plan/Goal e métricas exportadas.

Este índice aponta o contrato, o plano e a evidência da versão `0.1.0`; não
declara que todos os contratos já estejam integrados.

## Matriz de implementação atual

| Área | Estado |
|---|---|
| Headless | Integrado: providers OpenAI-compatible, Anthropic, Codex subscription e OpenCode Go (Chat Completions/Responses/Messages), tools nativas, modos, auth DACL, imagem local, compaction, usage, artifacts e anti-loop. |
| TUI | Integrada ao mesmo provider/loop/tools: `/login` inclui OpenCode Go com chave mascarada; `/models` usa catálogo live/cache/fallback, grupos e viewport; `ask_question` oferece opções e `Outro...` em runs comuns Auto/ReadOnly; composer adaptativo com cursor real, stream incremental em lanes bounded, tool blocks tipados agregados, Markdown/código/diff semânticos, inspectors, busca, cópia, anexos locais, usage, modos, cancelamento, scroll pin/live-edge, command palette e restauração fullscreen; golden matrix via TestBackend; PTY E2E físico pendente (`#[ignore]`, requer console real); sem credencial/provider pago nos gates. |
| Cache/HTTP | Replay local de respostas (`ProviderCache`) desligado no produto; transporte Reqwest compartilhado reaproveita conexões sem compartilhar adapter ou autenticação. |
| Sessões | Writer/recovery/branch e `--session` existem; `--resume`/recovery explícitos existem headless e TUI; UX geral de seleção/fork continua limitada. |
| Skills/MCP | Tool `skill` lazy; `%USERPROFILE%/.slim/skills` (cwd `.slim/skills` vence). Sem catálogo no schema. MCP stdio/HTTP lazy via meta-tool `mcp` + overlay `/mcp`; configuração em `./slim.toml` ou `%APPDATA%\slim\config\slim.toml` (ver [seção MCP](../README.md#mcp-mcpservers)). |
| Subagentes | Scheduler/child-session duráveis têm fila bounded, cancelamento e reopen; não há child agent provider-backed ou processo externo real. |
| Todo/Plan/Goal | Tool `todo` + dock via `TodoChanged`. Plan/Goal sem UI; Plan headless aborta (N4). |
| Release/validação | Release determinístico/hash-valid do checkpoint; matriz física de terminal continua não verificada. |

## Referências por necessidade

Escolha apenas os documentos pertinentes à tarefa; a numeração organiza o
índice, não impõe uma sequência de leitura. Planos e auditorias datados
registram decisões e evidências de suas execuções. O
[checkpoint do checkout](../README.md#checkpoint-do-checkout) distingue a revisão
mais recente dos registros de validação e deploy.

0. [`../AGENTS.md`](../AGENTS.md) e [`../RULES.md`](../RULES.md) — regras
   de execução e consulta técnica por escopo, sem releitura obrigatória do histórico.
1. [DECISOES-GRILL-PRE-IMPLEMENTACAO.md](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) —
   decisões normativas e limites da v1.
2. [PLANO-IMPLEMENTACAO.md](PLANO-IMPLEMENTACAO.md) — implementação e gates,
   com o estado final registrado ao fim.
3. [RUST-CLI.md](RUST-CLI.md) — mapa de arquitetura e paridade.
4. [DESIGN-SLIM-TUI.md](DESIGN-SLIM-TUI.md) — contrato visual/funcional da TUI.
5. [VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)
   — riscos e POCs.
6. [POC-RESULTS.md](../POC-RESULTS.md) — evidência executada e limitações.
7. [AUDIT-SLIM-TUI-TRACKER.md](AUDIT-SLIM-TUI-TRACKER.md) — auditoria da TUI vs
   spec e tracker vivo de execução (slices A/B/C/L, gates e log).
8. [HARNESS-V2-TRACKER.md](HARNESS-V2-TRACKER.md) — tracker canônico do Slim
   Durable Harness v2, suas 10 etapas, fronteiras da Onda 1 e evidência.
9. [PROXIMAS-ETAPAS-AGENTE.md](PROXIMAS-ETAPAS-AGENTE.md) — fila ranqueada do
   ciclo “agente completo, harness leve” (N1–N7 ACEITOS, WONTFIX, três
   sessões imediatas). Prevalece sobre o PLANO §10.1 para o que fazer *agora*.
10. [`../analysis_outputs/AUDIT-SPEED-CALLS-TOOLS-READS-2026-09-04.md`](../analysis_outputs/AUDIT-SPEED-CALLS-TOOLS-READS-2026-09-04.md)
    — auditoria de **velocidade local** (CHAMADAS/TOOLS/LEITURAS), 2026-09-04.
    Zero patch; única fatia candidata SPD-READ-01. Distinta da economia de
    tokens (`AUDIT-ECONOMIA-TOKENS.md` / `bench/token-economy/v2/RESULTS.md`).

11. [Auditoria de loops, thinking e ferramentas — 2026-09-04](../analysis_outputs/AUDIT-AGENT-LOOPS-THINKING-TOOLS-2026-09-04.md)
    — controles nativos, configuração efetiva, problemas reproduzidos e propostas mínimas.

12. [Harness Slim — etapa 1: simplicidade interna e correção tok/s](../analysis_outputs/HARNESS-SLIM-ETAPA-1.md)
    — estado do streaming preservado no descarte do future, execução local unificada,
    limites sem desmontagem/reconstrução, evidência offline e pendências para continuidade.

13. [Harness Slim — etapa 2: loop e ferramentas](../analysis_outputs/HARNESS-SLIM-ETAPA-2.md)
    — falhas reais do shell, barreiras de lote, repetição com estado, compactação,
    cancelamento, validação offline e passagem objetiva para a etapa 3.

14. [Harness Slim — etapa 3: contexto e histórico](../analysis_outputs/HARNESS-SLIM-ETAPA-3.md)
    — argumentos e reasoning Responses preservados, checkpoint sem duplicação,
    pedido recente intacto, recuperação por artefatos e limites de retomada/transporte.

15. [Harness Slim — etapa 4: transporte dos providers](../analysis_outputs/HARNESS-SLIM-ETAPA-4.md)
    — término nativo sem esperar EOF, timeouts sem retry inseguro, parsing incremental,
    usage preservado em falhas e medições locais, sem chamadas pagas.

16. [Harness Slim — etapa 5: configuração e providers](../analysis_outputs/HARNESS-SLIM-ETAPA-5.md)
    — precedência até o request, controles nativos, catálogo por conta/endpoint,
    evidências offline e síntese das cinco etapas com limitações explícitas.

17. [Otimização de testes e build](../analysis_outputs/OTIMIZACAO-TESTES-E-BUILD-SLIM.md)
    — fixtures de finalização sem espera TCP incidental, integração TUI agrupada,
    medições comparáveis e cobertura das cinco etapas preservada.

18. [Revisão visual da TUI](../analysis_outputs/REVISAO-VISUAL-TUI-SLIM.md)
    — contraste, hierarquia, atividade, modelos longos e modo inicial coerente;
    evidência por PTY local, gate/deploy e limites da validação física.

19. [Quick wins de agilidade nativa](../analysis_outputs/QUICK-WINS-AGILIDADE-NATIVA-SLIM.md)
    — trecho LF para patch CRLF uniforme, erro com localizações, recibo de edição
    e schemas acionáveis; evidência mecânica, gate e deploy local.

20. [Revisão de economia de tokens nativa](../analysis_outputs/REVISAO-ECONOMIA-TOKENS-NATIVA-SLIM.md)
    — listagem relativa, legenda local de padrões quando menor e correção de LF;
    medição da tarefa em dez combinações dos sete providers, contratos e limites.

21. [Índice de evidência e auditorias](../analysis_outputs/README.md)
    — relatórios datados, probes locais e o que fica fora do git.

22. [Auditoria estática do workspace](../analysis_outputs/RELATORIO-AUDITORIA-ESTATICA-2026-08-30.md)
    — achados de segurança e qualidade confirmados por leitura direta (2026-08-30).

Quando houver conflito, prevalecem as decisões finais, depois este plano e os
contratos específicos da TUI. Pesquisa histórica não sobrescreve comportamento
implementado. Para o próximo wiring do agent loop (não M3 visual), use o
item 9.

## Contratos implementados no headless

- OpenCode Go usa provider lógico próprio e registro fechado de 25 modelos. O
  modelo define wire (Chat Completions, Responses ou Messages), contexto,
  output, reasoning e imagem. Precedência da chave: `SLIM_API_KEY` >
  `OPENCODE_API_KEY` > auth file. A TUI salva/remove somente essa entrada;
  no próximo startup, restaura automaticamente o provider API-key ativo.
  OAuth permanece tipado, com refresh preservado; auth file inválido é erro
  explícito, sem simular estado deslogado e sem superar uma credencial de env;
  `/models` consulta o catálogo público com timeout/redirect/bounds, persiste
  cache atômico e mantém fallback offline sem aceitar IDs desconhecidos.
  No wire Chat Completions, snapshots cumulativos de usage são consolidados
  antes de chegar ao runtime; os demais providers mantêm validação estrita.
- `auth.json` é somente leitura e fail-closed. No Windows, o arquivo é aberto
  por handle `windows-sys` e recebe DACL protegida com allowlist exata do owner
  atual, usuário atual, `SYSTEM` e `Administrators`; há teste de caminho Unicode.
  Symlink, reparse point, arquivo não regular, ACL divergente e schema inválido
  falham; headless não cria arquivo ausente. Login OpenCode Go explícito na TUI
  pode criá-lo atomicamente, preservando providers irmãos.
- Redirects HTTP são desabilitados. O cache bounded é em memória, por processo,
  com namespace seguro por provider/endpoint/modelo e mensagens/tools/conteúdo
  canonicalizados; headers e credenciais não entram na chave, e tool calls nunca
  são cacheadas. O cache local de respostas é opt-in e permanece desligado no produto;
  os caminhos normais usam transporte compartilhado e cache nativo de prompt
  conforme o adapter. Quando habilitado, o cache local inclui configuração
  semântica e escopo de credencial na chave.
- SSE exige terminação estrita. OpenAI-compatible e Anthropic preservam IDs de
  provider nos tool deltas quando fornecidos; chamadas legadas sem ID recebem
  identificador interno. Em Chat Completions, material de tool é provisório até
  o terminal: `stop` normal o descarta, enquanto terminal de ferramenta valida
  o lote integral e rejeita qualquer chamada malformada ou incompleta.
- O loop aplica budgets separados read-only vs mutating por run; esgotamento → `tool_limit` (exit 22). Teto de turns por run: 128 (`SLIM_MAX_TURNS` / `max_turns` em `slim.toml`, cap 1024); esgotamento → `turn_limit` (exit 12) com `Turn limit reached (N/N)`. `search` bounded (gitignore, skip de build dirs, cap/paginação).
- A API key fornecida é redigida exatamente antes de tool output, follow-up,
  renderização e sessão. Não há afirmação de detecção automática de segredos
  desconhecidos.
- `--image` é repetível para PNG/JPEG/GIF/WebP; exige arquivo regular
  não-symlink, não vazio e <=20 MiB. `SLIM_CONTEXT_WINDOW_TOKENS` e
  `SLIM_MAX_OUTPUT_TOKENS` são inteiros positivos; o cap/reserva padrão é
  4096 tokens. No Codex subscription ele permanece reserva local e não envia
  `max_output_tokens`; providers que suportam o campo continuam recebendo-o.
  O resultado de tool é limitado a 64 KiB.
- Headless text/JSONL expõe `stop`: `provider_completed`/exit `0`,
  `turn_limit` ou `repeated_failed_tool`/exit `12`, e `tool_limit`/exit `22`.
- `--verbose` no headless text acrescenta timeline humana de tools e o Usage
  Ledger v2; JSONL v2 expõe requests, cache, compactação, custos e validação. O
  padrão continua answer-first; `--verbose --jsonl` é inválido.

## Evidência e limites

As nove correções de confiabilidade cobrem histórico parcial, prazo de processos,
Todo persistido, Retry-After, correção de argumentos, compactação, explicação de
paradas, abandono explícito de pendências e falha do worker.
[Contratos e limites](DESIGN-SLIM-TUI.md#11-checkpoint-de-implementação-atual);
[como recuperar uma sessão interrompida](../README.md#continuação-de-sessões-com-ferramentas).

`/resume` após cancelamento manual foi corrigido e reproduzido em fixture local.
Seleção, restauração e renderização preservam as respostas salvas, sem replay ou
escrita na sessão. [Checkpoint](DESIGN-SLIM-TUI.md#11-checkpoint-de-implementação-atual).

Falhas do provider agora ficam na tela e na sessão, com segredos removidos. O loop
tem até duas novas tentativas para falhas transitórias antes de resposta/ferramenta,
preservando resultados anteriores, cancelamento e orçamento. Conclusão normal vazia
ou só com reasoning é erro explícito. [Comportamento e validação](DESIGN-SLIM-TUI.md#11-checkpoint-de-implementação-atual).

Codex integra GPT-6 Astra, reasoning low até max e Normal/Fast na seleção e na
requisição. [Configuração, uso e contrato oficial](../README.md#openai-codex--gpt-6-astra).

O gate atual do checkout é `cargo test --workspace` (via `refresh-slim.ps1 -Test`, Cargo offline), verde: 1211 passed /
0 failed / 3 ignored (ConPTY físico + duas fixtures auxiliares) em 74 suítes e 0 compiler warnings
(contagem de 2026-09-06, sem `--skip`). As correções de identidade no cache, cursor expirado e deduplicação pelo histórico
ativo foram validadas com fixtures locais. As sete melhorias LSP foram implementadas
e revistas sequencialmente; Clippy de slim-lsp/core/cli `--all-targets -- -D warnings`
verde. O checkout foi instalado com `refresh-slim.ps1` em 2026-09-06, após esse
gate. Em validação comercial anterior, Muse 1.3/xhigh respondeu `OK` no Go real pelo debug e pelo executável
instalado, exit 0; teste sintético sem ferramentas, sem erro HTTP 500.
A fixture localhost exercita o turno
completo pela API TUI, sem provider live. Em verificação anterior, o smoke ConPTY
emitiu só o probe `ESC[6n`, sem frame; a matriz
física completa de terminal, IME, mouse e clipboard permanece não observada.

Contexto inicial distribuído por pasta e prazos TCP/TLS separados da espera de cabeçalhos. A bateria anterior registrada no benchmark terminou com 32/32 pares e 64/64 braços aprovados: Luna -10,76% tokens e DeepSeek -21,31%, com vantagem em 6/8 e 7/8 cenários, respectivamente. Os oráculos passaram integralmente; a vantagem em qualquer provider/tarefa permanece não demonstrada. [Código e benchmark](../bench/luna-live/README.md#contexto-equilibrado-e-prazos-http).

No deploy atual, `target\release\slim.exe` e `C:\Users\User\bin\Slim.exe` têm
15.143.936 bytes e SHA-256 idêntico
`0D1CC3690B410B0DA945629AC11DC39569D1244683929E7A51216322F90C52E1`;
`slim --version` retorna `slim 0.1.0` com exit `0`.

O release contém somente `slim.exe`; o builder usa caminhos relativos ao próprio
script e metadados ZIP fixos. Verifique-o com `python3 release/build_release.py`
(duas vezes) e `sha256sum -c release/SHA256SUMS.txt`.

As fatias de core/headless e os componentes TUI estão no workspace; a fila
curta de wiring do agente (OAuth headless, Todo/Skill no loop, Plan TUI) está
em [PROXIMAS-ETAPAS-AGENTE.md](PROXIMAS-ETAPAS-AGENTE.md). A integração v1
ampla (MCP transporte, child real, E2E) permanece no plano. Limitações
físicas continuam separadas das lacunas de wiring do produto.

Continuação de sessões: novos arquivos `--session` usam v2; CLI e bridge TUI
retomam conversas com chamadas/resultados e novas ferramentas, preservando modo,
orçamentos e raiz salva. Interrupções ambíguas ficam bloqueadas sem replay; v1
não é migrado. [Validação offline e limites](../README.md#continuação-de-sessões-com-ferramentas).

A retomada TUI também restaura ferramentas em blocos `history` recolhidos; Enter
consulta argumentos e resultados salvos, sem replay ou inferência de sucesso.
