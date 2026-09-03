# DESIGN-SLIM-TUI — especificação completa da interface terminal do Slim

> **Status de implementação:** fundação M0–M2 executada conforme os slices do
> tracker (`AUDIT-SLIM-TUI-TRACKER.md`). O binário tem reducer único normativo,
> scrollback virtualizado e navegável, surfaces estratificadas da paleta §21.3,
> lanes bounded com coalescer no caminho real, tool blocks tipados agregados,
> rails operacionais, composer adaptativo, command palette, motion semântico,
> property/fault/golden tests e benchmark com gate. O Slim Durable Harness v2
> concluiu as Etapas 8–10, incluindo resume headless/TUI, observabilidade e
> capability bridge durável para Skill/MCP/child/Todo/Plan/Goal. Pendências
> principais: PTY/ConPTY E2E em console físico, caches Parse/Layout dedicados
> (o WrapCache cobre alturas), Plan/Goal na TUI (Todo dock já recebe
> `TodoChanged`), métricas exportadas e a matriz física Windows. Inspectors,
> busca, cópia, anexos, code/diff e picker responsivo estão integrados. O detalhamento factual está na
> seção 1.1.

> Navegação: [Índice](README.md) · [Decisões finais](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) · [Paridade Rust](RUST-CLI.md) · [Pesquisa](INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md) · [Viabilidade](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)  
>
> Status: **design funcional e visual aprovado; grill fechado em 2026-08-20**  
> Data: 2026-08-20  
> Escopo: fullscreen Windows da v1; inline e Unix não planejados  
> Plataforma: Windows-only  
> Superfícies: fullscreen; headless usa o mesmo runtime  
> Completude: **contrato TUI/runtime v1 descrito; fatia vertical integrada;
> gates M0–M3 ainda não concluídos**  
> Base técnica: Ratatui + Crossterm com modelo, caches e renderização controlados pelo Slim  
> Viabilidade: [VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)

## 1. Objetivo deste documento

Este documento especifica o `slim-tui` sem depender do contexto da conversa que
o originou. Um agente de implementação deve conseguir:

1. entender o produto e seus limites;
2. criar os tipos e boundaries corretos;
3. implementar um milestone por vez;
4. saber exatamente o que testar;
5. reconhecer quando uma entrega está completa;
6. evitar decisões arquiteturais já rejeitadas.

O documento descreve o destino completo, mas **não autoriza uma implementação
big-bang**. A ordem dos milestones é normativa.

### 1.1 Checkpoint de implementação atual

Esta seção separa o **contrato-alvo** do que existe hoje. As demais seções
continuam normativas: um tipo, helper ou arquivo presente não significa que o
milestone correspondente passou seu gate.

| Área | Estado atual | Confirmado | Falta para o gate |
|---|---|---|---|
| M0 — contratos/testkit | **Quase completo** | `UiEvent`/`UiCommand` ampliados (Tool*/Input/Question tipados), `AppState` único com `FrameClock`/`ActivityState`, reducer como única rota de mutação, effects executados pelo runtime, blocos User/Assistant/Thinking/Tool/System/Error/Activity/QueuedUser, `RevisionSet` de 6 campos, IDs monotônicos, `MemorySurface` + frames determinísticos + proptest | `SurfaceBackend` trait compartilhada, variantes Plan/Compaction/Custom, sequences explícitas por stream |
| M1 — fullscreen Windows | **Fatia ampla; gate físico pendente** | TerminalGuard RAII + UTF-8/VT flags com restore exato inclusive se raw-mode falhar, alternate screen/raw mode/paste/cursor oculto, mouse capability-gated, composer adaptativo de 1–5 rows com cursor grapheme-safe, edição Left/Right/Home/End/Delete/Backspace e viewport horizontal (cauda visível com hint `<`, G232/G338; prompt ASCII `>` porque `›` é Ambiguous no Windows e estacionava o caret na última letra, G264), ActivityRail, scrollback navegável com pin/live-edge/unseen, paleta §21.3 estratificada em truecolor/256/16/no-color, layout de emergência, golden matrix via TestBackend; ContextRail removida por W2 e SessionRail conversacional reintroduzida no Slice 5 | PTY/ConPTY E2E físico (teste escrito, `#[ignore]`, requer console real) e validação manual Windows Terminal + alternativo |
| M2 — integração/performance | **Integração central + pipeline essencial** | bridge tipada com control lane imediata 256 + stream lane causal/lossless 1024; runtime drena lotes máximos de 32/1.024, rearma o wake ao esgotar o budget e bloqueia em console+wake+deadline visual, sem polling ocioso; `FrameClock` deriva do tempo monotônico; coalescer lossless no runtime real; SSE compartilhado preserva UTF-8 entre chunks, catálogos/OAuth/sessões têm leitura bruta bounded, tool blocks com lifecycle completo/agregação e progresso shell 1 Hz; captura shell bounded preserva início+fim por stream (8 MiB brutos e 8 KiB de contexto) com descarte exato, e a barreira global preserva início+fim do resultado `shell` antes dos adapters; lifecycle de reasoning no Responses, deadline pré-semântico independente de heartbeat, HeightIndex + WrapCache bounded generation/instance-keyed + render virtualizado (alturas 16.384 com chave `(cache_identity, content_generation, width, folded)`; corpos byte-weighted 4.096 entradas/32 MiB global/128 KiB por entrada conectados ao markdown estável antes dos overlays dinâmicos, G236/G335), Todo dock por eventos do loop (`todo` + `TodoChanged`), cancelamento provider/shell/OAuth, usage, modos, modelo/effort; bench misto ~5 MiB com nearest-rank p95 ≤16 ms; capability bridge no agent loop (Todo + dispatcher `skill` lazy) | ParseCache/LayoutCache dedicados, métricas §26 expostas |
| M3 — experiência completa | **Fatia ampla; gate final pendente** | overlays login/modelo/effort integrados, picker agrupado com filtro/viewport/Home/End, command palette Ctrl+P, Markdown com code rail e diff semântico, motion por `FrameClock` concentrado em atividade/caret, ActivityRail por fase/tempo, welcome estática, reduced motion, usage/contexto vivos, page-fill/anchor/scrollbar, inspectors Changes/Activity/Session/Diagnostics responsivos, busca Ctrl+F, cópia Unicode Ctrl+Y, `/image PATH` com chips e content blocks reais, footer sem métricas fictícias e headless answer-first; golden/fault/property tests via TestBackend | gate PTY/ConPTY físico, Plan/Goal completos na TUI, métricas §26 exportadas, resync por snapshot e matriz final de estados de domínio |

#### O que já funciona end-to-end

- entrada fullscreen normal, inclusive sem credencial;
- saída por Ctrl+C restaura o terminal com mouse habilitado ou no padrão desabilitado;
- `/login`, `/logout` e seleção de modelo/effort para os providers suportados;
- prompt do composer até provider, agent loop e tools reais;
- `ask_question` em runs TUI comuns Auto/ReadOnly: opções estruturadas, navegação
  por setas/números, alternativa `Outro...`, resposta livre e continuação no
  mesmo agent loop; headless, Plan e resume durável não anunciam a tool;
- primeiro delta SSE publicado antes do término da resposta;
- resume inicial transporta um único `SessionPreflight` até a abertura durável,
  preservando a comparação TOCTOU sem repetir leitura/parse do JSONL;
- fronteiras duráveis do provider persistem prefixo e sufixo em `append_batch`,
  com uma única sincronização física por lote;
- estimativa fixa de input deriva uma vez do request serializado completo, sem
  dupla contagem das tools;
- `PreparedProviderRequest` é o contrato único entre adapter, accounting,
  prompt/response cache e transporte: URL, headers, body final serializado,
  componentes, estimativa e fingerprints são preparados uma vez; o envio toma
  posse exatamente desses bytes. O preflight estrutural dos adapters internos
  decide compactação sem construir um request descartável; adapters externos
  sem limite declarado preparam uma vez e reutilizam o artefato. Segredos de
  headers, userinfo e query ficam fora de logs e fingerprints; cache e prompt
  routing usam um escopo opaco, com domínios disjuntos para adapters internos e
  externos;
- lotes mistos de ferramentas são divididos em segmentos causais contíguos:
  somente `SnapshotRead` usa a concorrência limitada existente; qualquer outro
  efeito é uma barreira sequencial. Cada segmento recebe preflight/commit próprio,
  sem antecipar reads posteriores a uma mutation e preservando a ordem do provider;
- batches read-only publicam conclusão na ordem real das execuções, mantendo a
  ordem original dos resultados enviados ao modelo;
- outputs grandes são materializados fora dos workers Tokio, com concorrência
  bounded 4 para artefatos independentes; a TUI toma posse do conteúdo bounded
  sem clonar o `String` integral;
- lifecycle básico de run/tool, resposta final e usage projetados para a TUI;
- troca `Auto → Read-only → Plan → Auto` no boundary de modo; Plan na TUI
  executa o loop com read/list/search (sem write/shell/`ask_question`);
  headless `--plan` continua `approval_required` / exit 10;
- cancelamento encerra request ativo e processo shell iniciado pelo run;
- shutdown explícito com restore RAII como fallback;
- transcript navegável: live edge segue o fim, setas/PageUp pinnam, End volta,
  contador de unseen na operational bar;
- tool blocks tipados com glyph de lifecycle, preview limitado e agregação
  `✓ N tools · nome ×N, ... · duração` para batches concluídos;
  falha/cancelamento em row própria;
- command palette (Ctrl+P) filtrando slash commands com Enter para executar;
- tela de boas-vindas estática em eixo central: `SLIM` neutro, estado de conexão
  provider-aware e um único hint contextual; verde aparece somente no dot
  conectado ou no comando `/login`; compacto, `NO_COLOR` e reduced motion
  preservam a mesma hierarquia sem scheduler próprio;
- footer Grok-style responsivo (W8, 2026-08-22): ActivityRail transitória
  imediatamente acima do composer; box completo insetado em uma célula em todo
  tamanho suportado; label `model (effort) · mode`; atalhos reais à esquerda e
  SessionRail conversacional no topo quando cabe; contexto/tokens retornam ao
  footer quando ela oculta; sem row vazia entre box e footer;
- paradas por budget (`tool_limit` / `turn_limit`): mensagem humanizada em bloco
  `System` colapsado (não toast efêmero); `turn_limit` inclui o teto `(N/N)`
  (default 128, `SLIM_MAX_TURNS` / `max_turns` em `slim.toml`, cap 1024);
  `max_read_tool_calls` (96) e `max_mutating_tool_calls` (32) são caps **por
  turn** (batch), não acumulados no run — exploração 1-tool/turn encontra
  `max_turns` primeiro; um batch paralelo acima do cap ainda aborta em
  `tool_limit`; ActivityRail mostra `turn N/M · read n/m mut n/m` quando
  largura ≥ 72 (`read`/`mut` deste turn); aviso one-shot ao cruzar 80% dos
  turns do run e 80% do batch deste turn; resume durável não executa tools
  (fail-closed) e diz isso explicitamente em vez de fingir 32/96;
- OpenCode Go integrado como provider lógico próprio: 24 modelos documentados,
  roteamento explícito por Chat Completions/Responses/Messages, chave por
  env/auth file, login TUI mascarado, `/models` com atualização background e
  cache/fallback bounded; seleção aplica contexto, output, reasoning e imagem
  sem heurística;
- fila de prompts durante run ativo (G244, §7.4): Enter/Alt+Enter enquanto o
  run trabalha enfileira o rascunho como bloco `QueuedUser` FIFO visível
  (`queued[i] texto`); cada fronteira de turno (run terminal aceito) drena um
  prompt por vez de volta ao provider; Ctrl+L abre o overlay de modelo e
  Ctrl+T alterna o Todo dock (§17.2); palette (Ctrl+P) e paste nunca sobem
  sobre modal (login/model/effort) — teclas-gate G250; draft >1 MiB é
  reportado, não descartado (G245); indicador multiline do composer conta a
  projeção exibida (paste tokenizado = 1 row) e usa linha-atual/total (G246,
  §15.3); modelo configurado é validado por provider (G248) e `/model <id>`
  textual funciona com ClinePass e Command Code (G249/G263);
- Command Code (`--provider command-code`, env `COMMANDCODE_API_KEY`/`CMD_API_KEY`)
  e ClinePass (`--provider clinepass`) detectam modelos via `GET …/models`
  (catálogo ao vivo como fonte da verdade; bundle só como fallback offline);
  Claude no Command Code usa `/messages`, o restante `/chat/completions`;
- Slim Durable Harness v2, Etapas 7–10 concluídas: queue bounded/FIFO/dedup,
  cancelamento durável, resume headless/TUI com fence/`PendingRun`, snapshot/
  watch/hooks/telemetria bounded e capability bridge durável para Skill/MCP
  selecionado/child/Todo/Plan/Goal; restore não reexecuta efeitos;
- Streaming/Thinking/Tools Slices 1–8: boundaries causais de Thinking e
  Assistant, fase `AwaitingProvider`, expansão inline, tools por batch/call ID
  com argumentos/duração/output paginado, requests de input/approval/question tipadas
  (G252: setas de `ask_question` estruturado vão ao reducer; `Outro...` alcançável;
  G253–G255 CLI: `--headless` lê stdin piped, `slim.toml` do projeto vence o global,
  Codex headless exige JWT `chatgpt_account_id`),
  coalescer temporal persistente de 16 ms e fila core→projector interruptível
  de 1.024 eventos. O normalizador infere `ThinkingStarted/Ended` para adapters
  que expõem apenas deltas; OpenAI Codex solicita `reasoning.summary=auto` e a
  TUI apresenta somente esse resumo público, nunca reasoning interno bruto.
  N1–N3 (2026-08-25): headless usa `oauth` do `auth.json`;
  o loop Auto anuncia `todo` e um dispatcher `skill` (sem catálogo no schema);
  `TodoChanged` abre o dock. Skills Slim em `%USERPROFILE%/.slim/skills`
  (override `cwd/.slim/skills`) só carregam com `list`/`name`. `~/.agents` fora.
  Motion (2026-08-25): composer usa `border_focus` com pulso BOLD no clock de
  12 fps durante run; spinner normativo `◒◓◑◐` também nas tools/thinking
  streaming; palette/slash agrupam `session` e `runtime`; `/mode` cicla o modo.
  Retoque visual (2026-08-25): user band `user_prompt_bg` inclusive no label;
  labels de papel usam capitalização consistente (`You`, `Slim`, `Thinking`);
  queued muted; grupo de tools com `Enter details` se couber; glyphs ASCII
  (`+ ~ o x >`) no `NO_COLOR` sem reflow. Sem drawer, crate ou superfície nova.
  Transcript Grok-like (2026-08-27, G318; revisado 2026-08-29, G360–G364):
  user band sem rail `│` e com uma row vazia depois do prompt; thinking
  completo colapsado em uma row muted (`Thought`), enquanto thinking em
  streaming mostra uma cauda muted de até duas rows físicas, calculada somente
  sobre os 256 grafemas finais para manter custo bounded por frame;
  assistant sem label `Slim` repetida; tools concluídas com cópia muted;
  Markdown com gap entre blocos raiz e tabelas GFM alinhadas na largura útil.
  Chrome compacto (2026-08-27, G328–G332): `FoldState::Auto` em thinking
  equivale a colapsado (corpo só expandido); bloco `Thought` omitido se vazio;
  `Enter details` só no grupo selecionado ou no último colapsado do run ativo;
  tool colapsada = nome + duração (sem `command=`/preview); shell humaniza
  `exit N` no wire; tools ativas e falhas permanecem em uma row bounded,
  priorizando nome/limite/progresso e razão curta redigida; footer `ctx` mostra
  ≥1% quando tokens > 0.
  Tools adjacentes (2026-08-27, G333–G334): calls concluídas consecutivas
  agregam visualmente mesmo com `ToolBatchId` distinto (`✓ 4 tools · shell ×4`);
  code inline Markdown usa `tool_accent` (`#7DCFFF`), não `code_rail`.
  Observabilidade de espera (2026-08-29, G344–G345): stream HTTP aberto sem
  conteúdo semântico usa label literal e deadline próprio; itens de reasoning do
  Responses acionam Thinking; Diagnostics preserva headers/primeiro byte/primeiro
  conteúdo. Shell ativo mantém comando bounded, limite e snapshot de saída 1 Hz
  visíveis, enquanto tools concluídas continuam compactas por padrão.
  Hardening de latência (2026-08-29, G346–G354): compaction background lenta
  deixa de ser aguardada obrigatoriamente pelo turno principal; batches formados
  apenas por `read`/`list`/`search`/`code_intel` executam com concorrência bounded
  4 e materializam resultados na ordem do provider, enquanto tools mutantes ou
  mistas permanecem seriais. O default operacional de output volta a 4.096 e o
  catálogo atua como teto; reasoning effort só entra no wire quando explícito.
  Append durável agrupa eventos com um `sync_data`; deltas adjacentes coalescem
  antes da saturação com telemetria de waits/high-watermark. A janela TUI de
  16 ms persiste entre batches, mas o primeiro conteúdo visível é imediato e
  uma barreira causal impede eventos migrados de ultrapassarem dados anteriores.
  OAuth mostra `Checking authentication`; gravação global de modelo é atômica.
  Latência Slim→provider (2026-08-27, G319): o client HTTP partilhado usa
  HTTP/2 (TLS) ou HTTP/1.1 keep-alive, `connect_timeout` 15s por endereço
  (failover IPv6→IPv4), ping HTTP/2 em idle, e o `send()` deixa de contar o
  upload do body nesse teto de connect — headers/upload seguem idle/wall.
  OAuth devolve a credencial em memória se restam mais de 30s e refresca em
  background abaixo de 10min; só espera o HTTP de refresh se o token já
  expirou ou resta ≤30s. Path ao vivo não anexa `ProviderCache` (sem hash
  canónico no POST). Hard threshold sem `PreparedCompaction` compacta local
  (extract bounded, sem POST de summary); `/compact` manual e overflow
  causal continuam no summary HTTP. Tool results no wire do próximo turno
  capam em 16 KiB (artifact handle acima disso). Projector em lane cheia
  espera `lane_space` (teto 50ms) em vez de `sleep(1ms)`. Gzip não entra no
  SSE. Gateways OpenCode Go/ClinePass/Command Code permanecem hops extras.
  Login (2026-08-26): toast `No provider connected` é descartado em
  `AuthStateChanged`/Connected; `SLIM_PROVIDER_CACHE` só vai ao stderr headless,
  nunca à superfície TUI (cursor do composer).
  Tool calls OpenAI-compatible (2026-08-27, G312–G313): `arguments: null` em
  fragmento inicial equivale a argumentos ainda vazios e pode ser completado
  pelos deltas seguintes; diagnósticos de payload malformado nunca escrevem
  diretamente em stderr enquanto a TUI fullscreen possui o terminal.
  Persistência de login (2026-08-26, G295–G297): startup sem `--provider`
  restaura o provider API-key marcado como ativo; Anthropic/Codex OAuth mantêm
  `oauth_session` e refresh; erro de leitura/schema do auth file é Auth visível.
  Precedência permanece env/API-key antes do store persistido.
  Contexto Codex (2026-08-26): o footer `ctx Nk/32k` lia o default genérico do
  agent loop (`AgentLoopConfig::context_window_tokens = 32_000`), não o modelo.
  GPT-5.6 Sol/Terra/Luna passam a usar o catálogo Codex
  (`GET /backend-api/codex/models?client_version=…`) no host ChatGPT, com
  fallback bundled de 272_000 tokens (janela de input do cliente de código;
  ~256k efetivos no fator 95% da CLI). A API pública documenta 1,05M; o backend
  Codex é autoritativo por conta e `originator`. Override:
  `SLIM_CONTEXT_WINDOW_TOKENS` ou `ProviderRunOptions`.
  Poluição visual (2026-08-26, G257): `ArtifactStored` deixa de virar toast;
  `CompactionCompleted` vira bloco `system` colapsado na data lane; argumentos
  de tool são resumidos (`path=…`, todo in_progress) em vez de JSON cru; toasts
  ocupam uma row cada, truncados pela largura, e info expira aos 5_000 ms no
  `FrameClock`.
  Uso geral (2026-08-26, G258–G261): default Codex headless e TUI compartilham
  `gpt-5.6-sol`; skills `run.ps1` lançam path DOS (jail canônico intacto);
  `--headless` sem provider exige `--fake` ou Auth; cada turno TUI Success
  anexa user+assistant em `ProviderRunOptions.history` para o POST seguinte
  (resume durável continua caminho separado).
  Esc/Ctrl+C (2026-08-26, G262): Esc no estágio API key do login volta à lista
  de providers; Ctrl+C é global (cancela run, sai se o composer está vazio,
  limpa o rascunho na primeira vez) e não é engolido por overlay de modelo,
  effort ou palette; ETX (`\u{3}`) conta como Ctrl+C. Login em progresso
  anuncia `Esc cancel · Ctrl+C cancel`.
  Command Code / ClinePass (2026-08-26, G263): quinto item do `/login`,
  quarto grupo no overlay `/model`, catálogo ao vivo em
  `https://api.commandcode.ai/provider/v1/models` (público) e
  `https://api.cline.bot/api/v1/models` (Bearer); slugs `cline-pass/*` e IDs
  Command Code válidos são aceitos mesmo ausentes do bundle.
  Latência de provider (2026-08-26, G265–G272): a conversa canônica
  pós-compaction volta para a TUI e evita summary repetida; modelos conhecidos
  de ClinePass/Command Code/Anthropic usam suas janelas catalogadas antes do
  fallback de 32k; o catálogo Codex é lido cache-first e só atualiza em
  background. Os seis providers compartilham o pool HTTP do processo, com
  timeout separado de connect/idle/wall. A ActivityRail distingue compaction,
  conexão, headers, primeiro byte, primeiro evento semântico e preparação de
  tool sem argumentos. Summary usa reasoning low e output máximo de 2.048.
  Usage OpenCode Go (2026-08-26, G298): o wire Chat Completions pode publicar
  snapshots cumulativos terminais; o transporte mantém o máximo observado por
  componente e entrega um único total após o stream. Outros providers seguem
  fail-closed para terminais conflitantes.
  Troca simétrica de provider (2026-08-27, G314–G315): selecionar um modelo no
  overlay unificado reutiliza a credencial salva do grupo de destino entre
  Codex, OpenCode Go, ClinePass e Command Code, reconstrói a request e persiste
  o provider ativo. Codex conserva a sessão OAuth para refresh; os outros três
  usam suas API keys irmãs. `/login` só é exigido quando a credencial do
  destino não está salva.
  Tool calls em Chat Completions (2026-08-27, G316): somente um terminal
  `tool_calls`/`function_call` autoriza publicar e executar os deltas acumulados.
  Um `stop` normal descarta placeholders pendentes, inclusive `arguments:null`,
  sem invalidar o texto já concluído. Terminais de ferramenta continuam exigindo
  identidade, nome e argumentos JSON válidos; Codex/Responses preserva o terminal
  `completed` e Anthropic Messages preserva seu lifecycle de content blocks.
  Hardening terminal-authoritative (2026-08-27, G317): todos os formatos de
  placeholder/malformação Chat Completions (`null`, `{}`, `function:null`,
  container/tipo/índice/nome/argumentos inválidos e fragmentos conflitantes)
  ficam provisórios até `finish_reason`. Em `stop`, são inertes e nunca executam;
  em terminal de ferramenta, qualquer membro inválido contamina o lote inteiro.
  Placeholder anterior pode ser substituído por call válida posterior. As
  fronteiras estritas de Codex/Responses e Anthropic não mudam.
  O gate ConPTY resolve binário por caminho/hash, usa provider
  loopback e matriz 120/80/60/40/32; neste host conhost emitiu apenas o probe,
  portanto o teste físico permanece `#[ignore]`;
  Organização TUI/headless (2026-08-26, G273–G278): novos turnos têm uma row de
  respiro sem separar blocos internos; page-fill usa a métrica canônica do
  `prompt_id`; cwd trivial não ocupa SessionRail; `border_focus` volta ao token
  `#4F7D5E`; grupos mostram nomes/contagens e aceitam Enter direto no live edge;
  headless text oferece timeline opt-in por `--verbose`, genérica para todos os
  providers e sem argumentos/output;
  Hardening global pós-auditoria (2026-08-26, G279–G292): tools e entradas locais
  têm limites explícitos com drenagem/paginação; busca `rg` usa framing NUL
  compatível com caminhos Windows; restauração do console preserva code pages de
  entrada e saída separadamente; fila de prompts tem teto 8; composer mantém
  contagem incremental e coalesce de texto para latência constante em drafts
  longos. Esses contratos são provider-agnostic e não alteram protocolo de rede;
  Compactação Parity+ (2026-08-26, limiar G319 2026-08-27): política
  compartilhada aplica soft 60% e hard 85% em janelas abaixo de 1M, soft 30%
  e hard 50% em janelas maiores; mantém 20k tokens recentes sem separar pares
  assistant/tool e preserva a instrução raiz literalmente. A TUI prepara
  summary validada em background, mostra `preparing`/`ready` no contexto e
  expõe `/compact [instruções]`; o hard boundary reutiliza somente checkpoint
  com provider/modelo e fingerprint compatíveis. Sem prepared no hard, o loop
  aplica extract local bounded e envia o pedido do utilizador sem esperar
  summary HTTP. Overflow causalmente seguro compacta (HTTP) e repete uma
  única vez. Manual `/compact` continua no summary HTTP.
  Checkpoints schema v2 encadeiam resumo, anchor, fingerprint, usage, duração e
  inventários bounded; recovery inválido recua para o histórico integral. A
  configuração `[compaction]` é mesclada campo a campo. O mecanismo usa o mesmo
  adapter/modelo selecionado e vale para os seis providers. Accounting terminal
  repetido com valores idênticos é idempotente no runtime, cache e summary; uma
  repetição conflitante continua falhando de forma fechada;
- Usage Ledger v2 mantém input novo, cache write/read, output e reasoning
  separados por request; registra payload, latência, retry/cancelamento e
  compactação, marca economia de compactação como estimada e calibra requests de
  texto por provider/modelo via EWMA sem tokenizer; conclusão validada exige
  `ValidationGreen` posterior à última mutação e ausência de erro terminal;
- Causal Governor receipt-driven (2026-08-31): cada chamada produz um único
  `PreparedToolInvocation` tipado antes do governor e um `ToolExecutionReceipt`
  com dependências, stamps, mutações, revisões, bytes e tempos observados pela
  própria ferramenta. `read`, `list`, `search`, `write` e `patch` não sofrem
  segunda varredura causal; o caminho comum usa fast stamps e não hash integral.
  Validações são voláteis, não reutilizáveis e vinculadas à revisão causal em que
  começaram; lotes mutantes preparam cada chamada JIT e lotes read-only preparam
  uma vez antes da execução concorrente;
- snapshot imutável de `list` (2026-08-31): a primeira página enumera e ordena o
  diretório uma vez; continuações validam cursor, TTL e path, clonam somente o
  `Arc<Vec<PathBuf>>` sob o mutex e montam a fatia após liberar o lock, sem novo
  `read_dir`. Mudanças aparecem apenas em nova listagem sem cursor; snapshot
  ausente, expirado ou evicto pede nova consulta em vez de reconstrução silenciosa;
- code intelligence LSP sem releituras (2026-08-31): `DocumentStore` e o cache
  local de cada operação compartilham `Arc<str>`, índice de linhas e fast stamp;
  conversão de posições e contexto não acessam o filesystem, e staleness usa
  versão LSP, revisão do workspace, stamp e versão publicada dos diagnósticos.
  Discovery é bounded por workspace/configuração/PATH, com validação barata das
  entradas positivas fora do mutex;
- prompt cache nativo por capability (2026-09-01): adapters declaram suporte a
  chave, opções, breakpoints, TTL e métricas read/write. OpenAI oficial e os
  protocolos Responses emitem uma chave limitada à classe estável
  wire/model/system/tools/política, sem sessão, credencial ou histórico;
  Anthropic usa cache automático top-level e breakpoints explícitos após tools
  e system, inclusive após compaction. TTL estendido e replay local de respostas
  permanecem desligados;
- layout do transcript (2026-08-31): sem inspector docked, a conversa usa toda a
  largura disponível do scrollback; o cap de 144 células permanece somente no
  workspace compartilhado com inspector docked;
- verificação integral mais recente: workspace verde (87 suítes / 1087 passed /
  0 failed / 1 ignored — ConPTY físico / 0 compiler warnings; 1 teste preexistente
  quebrado filtrado via `--skip`, sem implementação em `crates/`), incluindo
  proptest, fault injection, golden matrix e E2E offline do bridge; Grok Slice 2
  fechado após 16 passes Fusion sem achados finais. Grok Slice 3 recebeu um
  único revisor independente final após G85–G179; seus achados G180–G183 foram
  corrigidos com gate integral verde: snapshots/usage u64 por request
  incluem summary e overhead system/tools; accounting normal permanece em uma
  ordem única e cancel aguarda identidade terminal antes da próxima run; cache
  bounded por entrada/bytes/total e cancelável particiona configuração/query/
  header/credential scope sem expor segredo; redaction cross-delta ordena
  secrets sobrepostos do mais longo ao mais curto; Codex preserva query,
  mantém output cap somente como reserva local e não serializa
  `max_output_tokens` no contrato subscription de `backend-api/codex/responses`;
  truncation recebida continua classificada; outcome não bem-sucedido e overflow nunca
  viram exato nem faturável; parsing tool/index/type/remote-error é fail-closed
  e bounded; summary/sequence/artifact preservam causalidade; footer 40×8
  preserva contexto+totais. No último gate integral, Clippy TUI ficou sem
  warnings e o corpus de 4.968.694 bytes mediu p95 warm input→frame 0,757 ms
  ≤16 ms e scroll 0,533 ms. Grok Slice 4 foi fechado após um revisor único:
  prompt page-fill até overflow, anchor `(BlockId,row_offset)`, Home/End
  distintos, prioridade de setas para overlays, lookup BlockId O(1), medida
  pré-reducer e scrollbar width-aware. Gate final: 63 suítes / 559 testes,
  0 falhas / 0 warnings / 1 ConPTY ignorado; p95 input→frame 1,148 ms e scroll
  1,202 ms. Grok Slice 5 foi fechado após um revisor único sem bloqueadores:
  SessionRail conversacional em transcript `≥80×12`, cwd one-row display-safe,
  projeção compartilhada fullscreen/ViewModel e contexto único rail/footer.
  Gate final: 63 suítes / 566 testes, 0 falhas / 0 warnings / 1 ConPTY ignorado;
  p95 input→frame 1,024 ms e scroll 0,773 ms. `refresh-slim.ps1 -Test`
  recompilou e implantou os Slices 1–5 com `OK:` em 2026-08-23 21:08:21.
  Limpeza pós-deploy removeu 22.515 artefatos / 12,4 GiB naquele checkpoint.
  G191 corrigiu o contrato Codex subscription: `max_output_tokens` permanece
  reserva local e não entra no wire; OpenAI-compatible/Anthropic preservam seus
  campos suportados. Gate fresco: 63 suítes / 566 testes, 0 falhas / 0 compiler
  warnings / 1 ConPTY ignorado; `refresh-slim.ps1 -Test` imprimiu `OK:` e
  implantou SHA-256 `DACD9EF9F4669518B8443A65C2E95FB7DD4EF442022A308C8D6A71DD82733BA8`.
  `CURRENT_SCHEMA_VERSION=1` e writer v1 default preservados. G320 (2026-08-27)
  alinha Skills ao shell: cancelamento e timeout encerram a árvore de processos
  antes do `wait` e da drenagem dos pipes, impedindo efeitos descendentes tardios;

#### O que precisa ser corrigido antes de continuar M3

A fila **atual** de wiring do agente (sem inspectors/caches/M0) está em
[PROXIMAS-ETAPAS-AGENTE.md](PROXIMAS-ETAPAS-AGENTE.md). Os itens 1–2 e 4–7
abaixo permanecem no contrato TUI e estão **WONTFIX** nesse ciclo de harness
leve, salvo prova de que são o menor caminho para o loop pedir input.

1. **P0 — provar o ciclo físico Windows.** PTY/ConPTY E2E está implementado
   (`crates/slim-cli/tests/tui_pty.rs`) mas ignorado neste ambiente sem console;
   rodar em console físico e validar restore/IME/mouse na matriz §25.
2. **P1 — completar contratos M0.** Sequences monotônicas por stream,
   `SurfaceBackend` trait compartilhada entre MemorySurface e Fullscreen,
   variantes Plan/Compaction/Custom do block model.
3. **P1 — Todo/Plan/Goal na superfície TUI.** O harness durável e o
   `RuntimeCapabilityBridge` persistem operações tipadas; falta projetar
   `TodoChanged`/`PlanChanged`/`GoalChanged` no runtime/TUI e fechar os fluxos
   visuais de aprovação/input de Plan/Goal/Input.
4. **P1 — ParseCache e LayoutCache dedicados.** O WrapCache cobre alturas;
   faltam os caches de Markdown parse e LayoutPlan das chaves §12.3.
5. **P2 — métricas §26.** Contadores de latência/cache/lane existem nos testes
   e no bench, mas não são exportados como telemetry de runtime.

Fechado em 2026-08-29: inspectors Changes/Activity/Session/Diagnostics,
busca no transcript, clipboard Unicode e anexos locais estão ligados ao fluxo
real, com degradação explícita e testes golden (G340–G341).

#### Regra de atualização deste checkpoint

- **Implementado:** usado pelo binário normal e coberto por teste observável.
- **Parcial:** existe uma fatia real, mas faltam entregas ou gate do milestone.
- **Scaffold:** tipos/helpers existem sem wiring completo no produto.
- **Concluído:** somente após executar o gate normativo inteiro; presença de
  arquivos ou testes unitários isolados não basta.

### 1.2 Direção visual revisada

A referência primária é a linguagem visual do **Universe TUI/Grok Build**:
profundidade por surfaces near-black, prompt elevado, composer contornado,
headings coloridos, metadata discreta, scrollbar, progress rail e motion de
atividade. A referência secundária é a energia operacional do **OpenCode**:
rails mais expressivas, foco forte e progresso evidente, sem copiar seu excesso
cromático ou densidade de status.

Proporção normativa: **80% contenção da primeira referência, 20% energia da
segunda**.

Para componentes individuais, **termcn é a principal referência de polimento**.
Ele não entra como dependência: seus componentes React/OpenTUI são estudados e
portados seletivamente para widgets Rust controlados sobre Ratatui. Tokscale
orienta padrões já comprovados em Ratatui; Critique orienta diff/responsividade;
OpenTUI orienta primitives e keymaps, não o runtime do Slim.

Preservar:

- transcript denso e legível, sem cards em cada mensagem;
- verde Slim como destaque funcional de identidade, não como cor dominante;
- azul frio reservado para headings, links e informação estrutural;
- warning/error somente para estado semântico;
- metadata e shortcuts discretos;
- foco visual forte no composer e atividade corrente.

Evitar:

- fundo uniforme sem hierarquia;
- laranja ou verde em toda metadata;
- rails grossas em todos os blocos;
- múltiplas status rows repetindo a mesma informação;
- animação de tela inteira ou motion que dificulte leitura;
- boxes em cada mensagem, canto decorativo ou chrome sem função.

A profundidade vem de cinco níveis controlados: `background`, `surface`,
`surface_alt`, `surface_elevated` e overlays. Motion vem de progress rail,
spinner, streaming caret, tool progress e pulso sutil de foco; nunca de conteúdo
textual se movendo.

## 2. Decisões fechadas

Estas decisões não devem ser reabertas durante a implementação sem uma revisão
explícita deste design:

| Tema | Decisão |
|---|---|
| Framework | Ratatui sobre Crossterm |
| Estratégia | híbrida: infraestrutura externa, modelo/render pipeline próprios |
| Estado | reducer único e effects assíncronos |
| Render | puro; sem I/O nem mutação de estado de aplicação |
| Superfície inicial | fullscreen em alternate screen |
| Segunda superfície | headless, reutilizando o mesmo AppHandle/ViewModel |
| Layout | conversa focada; SessionRail adaptativa substitui a antiga ContextRail fixa; Todo fixo; composer elevado; inspectors adaptativos |
| Tema default | near-black estratificado, verde Slim + azul estrutural, profundidade sem card soup |
| Mensagens | alinhadas à esquerda e fluidas; prompt do usuário pode usar surface elevada |
| Densidade | transcript compacto; composer boxed com três rows normais; rails operacionais com uma row |
| Tools | sucessos agrupados por nome/turno; running/falha/cancelamento em rows próprias |
| Code/diff | code em rail neutra; diff com highlight de linha e intraline |
| Plataforma | Windows-only |
| Unix/inline | não planejados |
| Histórico visual | blocos tipados com IDs e revisions estáveis |
| Streaming | primeiro delta imediato; demais coalescidos em até 16 ms |
| Motion | clock monotônico a 12 fps nominais (83 ms) somente quando run/stream visível anima; welcome é estática e não agenda ticks; reduced motion zera ticks de animação e congela glyph/caret, preservando apenas StatusTick semântico de 1 Hz enquanto a ActivityRail visível envelhece o elapsed |
| Event delivery | lifecycle lossless; deltas/ticks/resize coalescíveis |
| Caches | parse, wrap e layout separados por dependência |
| Extensões | custom blocks versionados somente após o core estabilizar |
| Crates | `slim-tui` com AppHandle tipado; core fica em `slim-core` |

## 3. O que será construído

### 3.1 Capacidades finais

As capacidades abaixo descrevem o renderer fullscreen da v1. Qualquer menção
posterior a `InlineBackend`, Unix ou parity entre surfaces é referência histórica
e não autorização de implementação.

- chat interativo com streaming;
- scrollback virtualizado;
- composer multiline em box de três rows, com uma row editável e fallback compacto;
- render de Markdown e código;
- blocos de user, assistant, thinking, tool, system, compaction e error;
- tools com progress, output expandível e cancelamento;
- operational bar adaptativa abaixo do composer;
- Todo em dock fixo acima do composer;
- drawers para diff, activity, session tree e diagnóstico;
- overlays/modals empilháveis;
- busca no histórico visível;
- themes semânticos;
- mouse opcional;
- clipboard e bracketed paste;
- reduced motion e fallback de glyphs;
- imagens quando o terminal oferecer protocolo suportado;
- fullscreen Windows;
- custom blocks/widgets somente se necessários ao core, sem plugin system;
- golden tests, PTY E2E, fault injection e benchmarks long-session.

### 3.2 Fora do escopo do `slim-tui`

O crate não implementa:

- provider de LLM;
- agent loop;
- tool execution;
- permissions/sandbox, ausentes do produto Slim por decisão normativa;
- compaction;
- persistência de sessão;
- filesystem/VCS;
- MCP;
- subagentes.

Skills, MCP e subagentes são requisitos do runtime da v1. Estão fora do
`slim-tui` apenas por ownership: chegam como `UiEvent`/ViewModel e aparecem no
transcript, status e activity views; a interface não executa skill, MCP call ou
child agent.

Esses subsistemas publicam eventos e recebem comandos por interfaces tipadas.
A TUI apresenta e coleta interação por um `AppHandle` tipado; ela não se torna a
autoridade de domínio nem executa providers/tools diretamente.

## 4. Princípios de arquitetura

### 4.1 Fluxo unidirecional

```text
TerminalEvent ─┐
AgentEvent ────┤
Tick ──────────┼─> Action ─> reduce(AppState, Action) ─> Effect[]
AsyncResult ───┘                    │                        │
                                    ▼                        ▼
                                ViewModel              EffectRunner
                                    │                        │
                                    ▼                        └─> Action
                               LayoutPlan
                                    │
                                    ▼
                                  Frame
                                    │
                                  │
                                  ▼
                          FullscreenBackend
```

### 4.2 Uma autoridade de estado

`AppState` é a única autoridade do estado visual e interativo. Componentes não
guardam cópias divergentes de session, focus, scroll ou tool status.

Caches podem ter mutabilidade interna, mas:

- derivam somente de dados versionados;
- podem ser descartados a qualquer momento;
- nunca são persistidos como estado de produto;
- um cache miss não muda a semântica do frame.

### 4.3 Render puro

Render recebe `&ViewModel`, `LayoutPlan`, theme e um buffer. Ele não pode:

- executar I/O;
- enviar commands;
- alterar `AppState`;
- aguardar futures;
- chamar relógio global diretamente;
- ler filesystem;
- iniciar timers.

Animações recebem um `FrameClock` já calculado pelo loop.

### 4.4 Effects explícitos

Toda ação externa é um `Effect`, por exemplo:

- enviar prompt;
- abortar run;
- cancelar tool;
- copiar texto;
- abrir link;
- carregar imagem;
- mudar backend;
- salvar setting;
- solicitar snapshot de sessão.

O effect runner executa o trabalho e publica `Action::EffectCompleted` ou
`Action::EffectFailed`.

## 5. Dependências técnicas

O snapshot do Grok Build estudado usa Ratatui 0.29 e Crossterm 0.28. Esses
números são referência de arquitetura, não obrigação de versão. O plano de
implementação deve fixar versões compatíveis atuais e registrar o lockfile.

### 5.1 Runtime

| Capacidade | Dependência pretendida |
|---|---|
| terminal buffer/layout/diff | `ratatui` |
| eventos, raw mode, alternate screen | `crossterm` |
| async/event loop | runtime async do Slim |
| cancelamento | token de cancelamento do runtime |
| Unicode width | `unicode-width` |
| grapheme boundaries | `unicode-segmentation` |
| Markdown | `pulldown-cmark` |
| syntax highlight | `syntect`, inicializado sob demanda |
| motion/transições complexas | `tachyonfx`, opcional e somente após spike |
| imagens capability-gated | `ratatui-image`, somente em M3 |
| session tree | `tui-tree-widget` somente se reduzir código sem criar estado paralelo |
| serialização de themes/API | `serde` |

### 5.2 Desenvolvimento

| Capacidade | Ferramenta pretendida |
|---|---|
| snapshots de buffers | `insta` ou equivalente |
| property tests | `proptest` ou equivalente |
| PTY | `portable-pty`; adapter ConPTY nativo somente se um teste demonstrar capability ausente |
| benchmarks | Criterion ou harness do workspace |
| fault injection | adapters fake do `slim-tui-testkit` |

### 5.3 Regra de dependência

Nenhuma dependência entra apenas porque o Grok Build a utiliza. Para cada crate:

1. identificar o boundary que ela resolve;
2. confirmar suporte Windows;
3. verificar impacto no binário e startup;
4. adicionar teste do comportamento esperado;
5. registrar licença.

### 5.4 Estratégia de componentes estilo termcn

Não existe equivalente direto do `termcn` para Rust/Ratatui no conjunto
avaliado. O `termcn` publica componentes React para Ink/OpenTUI; não oferece
crate Rust, widgets Ratatui ou adapter oficial. Portanto:

- não consumir registry `termcn` no build do Slim;
- não adicionar Bun, Node, React, OpenTUI ou Zig ao runtime/release;
- não usar sidecar JS nem segundo proprietário de stdin/raw mode/alternate screen;
- não migrar para `iocraft`, `tui-realm` ou outro framework apenas para obter
  componentes visuais;
- portar comportamento e linguagem visual para código Rust source-owned;
- preservar notices/licença MIT quando houver cópia substancial e registrar
  commit, arquivo estudado e proveniência.

#### Stack recomendada

| Necessidade | Decisão |
|---|---|
| layout, surfaces, borders, gauges, scrollbar | widgets/primitives nativos do Ratatui |
| spinner, streaming caret, progress conhecido | `FrameClock` + widget interno; sem crate de spinner |
| fades, dissolve, drawer e highlight temporário | considerar `tachyonfx` após benchmark |
| Markdown | `pulldown-cmark` com sanitização própria |
| syntax highlight | `syntect` lazy e bounded |
| imagens | `ratatui-image` somente no milestone capability-gated |
| tree | avaliar `tui-tree-widget`; rejeitar se duplicar focus/selection state |
| composer | implementação Slim; não adotar textarea genérico que perca paste atômico |

`tachyonfx` não é requisito para spinner, caret ou progress rail. Esses efeitos
são pequenos e determinísticos com `FrameClock`. A dependência só entra se
transições compostas demonstrarem ganho visual e mantiverem os budgets.

#### Kit interno source-owned

O Slim mantém um kit pequeno, equivalente à filosofia shadcn/termcn: código
copiado/portado pertence ao repositório e segue o theme/reducer local.

```text
Surface
Rail
ModalFrame
ComposerFrame
StateGlyph
ProgressTrack
StreamingText
ThinkingBlock
ToolBlock
CommandPalette
Scrollbar
```

Contrato de cada componente:

```text
ViewModel + Rect + Theme + FrameClock → Buffer
```

Componente não pode executar I/O, iniciar timer, consumir channel, consultar
relógio global, alterar `AppState` ou manter autoridade paralela. Interação gera
`Action`; estado volta pelo reducer.

#### Ordem de port

1. `ThinkingBlock` + `ProgressTrack`/spinner;
2. `StreamingText` + caret reduced-motion-safe;
3. `CommandPalette`;
4. lifecycle visual de tools;
5. componentes adicionais somente quando usados por fluxo real.

Primeiro spike usa somente Ratatui. `tachyonfx` entra numa segunda comparação,
contra a versão nativa, para fade de overlay, focus transition, dissolve,
drawer e highlight temporário.

Gate do spike:

- buffers reais em 40×8, 80×24 e 140×40;
- truecolor, ANSI16 e no-color;
- motion normal e reduced motion sem mudança de layout;
- Unicode/CJK/emoji/combining;
- input→frame p95 dentro de 16 ms;
- resize/shutdown/restore Windows sem segundo event loop;
- ganho visual claro contra implementação Ratatui simples.

## 6. Estrutura lógica do crate

O crate inicial permanece único, com módulos internos:

```text
slim-tui
├── api             tipos públicos e UiCommand/UiEvent
├── app             AppState, Action, reducer e ViewModel
├── runtime         event loop, scheduling, channels e EffectRunner
├── surface         trait + fullscreen
├── terminal        RAII guard, capabilities e platform adapters
├── block           modelo de blocos e renderers
├── component       primitives source-owned inspiradas em termcn
├── render          pipeline, buffers, caches e virtualização
├── layout          breakpoints, LayoutPlan e hit testing
├── composer        editor, history, paste e completion
├── todo_dock       projeção residente e expansão in-place do Todo
├── overlay         stack, focus e modal lifecycle
├── inspector       diff, activity, tree e diagnostics
├── input           keymap, mouse e normalization
├── theme           tokens, glyphs e capabilities
├── markdown        parse e highlight
├── image           protocols e fallback
├── telemetry       métricas sem conteúdo
└── testkit         clocks, backends, fixtures e PTY helpers
```

Não criar um crate separado para cada módulo. A extração exige um consumidor
independente ou redução comprovada de compile time/coupling.

## 7. API entre harness e TUI

### 7.1 Identificadores

Usar newtypes, nunca strings intercambiáveis:

```rust
pub struct SessionId(pub Arc<str>);
pub struct MessageId(pub Arc<str>);
pub struct BlockId(pub Arc<str>);
pub struct ToolCallId(pub Arc<str>);
pub struct ToolBatchId(pub Arc<str>);
pub struct ContentHandle(pub Arc<str>);
pub struct ContentRequestId(pub u64);
pub struct PageCursor(pub u64);
pub struct InteractionRequestId(pub Arc<str>);
pub struct OverlayId(pub u64);
pub struct EffectId(pub u64);
```

### 7.2 Eventos recebidos

```rust
pub enum UiEvent {
    SessionSnapshot(SessionSnapshot),
    SessionChanged(SessionMeta),
    RunStarted { run_id: RunId },
    RunEnded { run_id: RunId, outcome: RunOutcome },
    UserMessageAdded(UserMessageView),
    AssistantStarted(AssistantStart),
    AssistantDelta(AssistantDelta),
    AssistantEnded(AssistantEnd),
    ThinkingStarted(ThinkingStart),
    ThinkingDelta(ThinkingDelta),
    ThinkingEnded(ThinkingEnd),
    ToolStarted(ToolStart),
    ToolProgress(ToolProgress),
    ToolEnded(ToolEnd),
    CompactionStarted(CompactionView),
    CompactionEnded(CompactionView),
    ModeChanged(ModeView),
    CapabilitySurfaceChanged(CapabilitySurface),
    PlanChanged(PlanView),
    TodoChanged(TodoView),
    GoalChanged(GoalView),
    ActivityChanged(ActivityView),
    UsagePartial { input_tokens: u64, output_tokens: u64 },
    Usage { input_tokens: u64, output_tokens: u64 }, // terminal/final da request
    UsageEstimate { request_id: u64, context_tokens: u64, context_window_tokens: u64 },
    UsageEstimateForRun { run_id: u64, request_id: u64, context_tokens: u64, context_window_tokens: u64 },
    ApprovalRequired {
        request_id: InteractionRequestId,
        summary: Arc<str>,
        persisted: bool,
    },
    InputRequired {
        request_id: InteractionRequestId,
        prompt: Arc<str>,
        options: Arc<[Arc<str>]>,
        persisted: bool,
    },
    InteractionAcknowledged {
        request_id: InteractionRequestId,
        accepted: bool,
        message: Arc<str>,
    },
    StatusChanged(StatusSnapshot),
    QueueChanged(PromptQueueSnapshot),
    ContentPageLoaded(ContentPageView),
    Notification(NotificationView),
    FatalError(FatalUiError),
}
```

Regras:

- cada stream possui sequence monotônica;
- `Ended` carrega o snapshot final integral da entidade;
- delta pode ser coalescido;
- snapshot recupera qualquer gap;
- eventos não carregam secrets não redigidos;
- o lifecycle executor de tool preserva `ToolBatchId + ToolCallId`, argumento
  resumido já redigido, duração em milissegundos e `ContentHandle`; nome de tool
  nunca é chave de correlação;
- campos desconhecidos em protocolos externos falham soft quando seguro.
- reasoning possui fronteira causal canônica
  `ThinkingStarted → ThinkingDelta* → ThinkingEnded`. Protocolos sem eventos
  nativos de abertura/fechamento inferem a abertura no primeiro reasoning e o
  fechamento, uma única vez, antes do primeiro texto, tool call ou stop;
- `Responding` começa somente no primeiro `AssistantDelta`. Request em voo sem
  conteúdo recebido não é apresentado como resposta ativa;
- após `ToolEnded`, enquanto não chegou novo reasoning ou texto, a fase é
  `AwaitingProvider`. Essa espera termina apenas por evento causal posterior ou
  por um outcome terminal da run.
- input/approval só é projetado quando possui `InteractionRequestId` não vazio,
  bounded e ligado a uma rota de resposta. Eventos legados sem ID não criam
  bloco nem estado `WaitingForInput`;
- `persisted` informa se o pending pode ser reconstruído após reopen. O reducer
  mantém o request pendente durante outcome terminal e replay até receber um
  acknowledgement correlacionado;
- request duplicado com mesmo ID e payload é idempotente; payload conflitante
  não substitui o original e solicita resync. Ack desconhecido, stale ou
  duplicado não muta outro request.
- o bridge aplica namespace de `run_id` tanto ao request quanto ao ack antes
  de expor a identidade à TUI; reuso do ID bruto pelo provider em runs
  diferentes nunca cruza autorização nem lifecycle.

### 7.3 Comandos emitidos

```rust
pub enum UiCommand {
    SendPrompt(PromptDraft),
    EnqueuePrompt(PromptDraft),
    Steer(PromptDraft),
    AbortRun { run_id: RunId },
    CancelTool { tool_call_id: ToolCallId },
    RetryLastTurn,
    SelectModel(ModelSelection),
    SetMode(OperatingMode),
    AnswerInput { request_id: InteractionRequestId, answer: Arc<str> },
    Approve { request_id: InteractionRequestId },
    Reject { request_id: InteractionRequestId },
    SpawnSubagent(SubagentSpawnRequest),
    CancelSubagent { child_id: ChildId },
    ExecuteSlashCommand(SlashInvocation),
    RequestSessionSnapshot,
    NavigateSessionTree { entry_id: EntryId },
    SetTodoState(TodoMutation),
    ChangeSetting(SettingMutation),
    RequestContentPage {
        handle: ContentHandle,
        request_id: ContentRequestId,
        cursor: Option<PageCursor>,
    },
    Shutdown,
}
```

O harness pode rejeitar qualquer comando. Resposta enviada marca somente
`response_pending`; não conclui o bloco localmente. Apenas
`InteractionAcknowledged` com o mesmo request ID conclui (`accepted=true`) ou
falha (`accepted=false`) e limpa o pending. Host sem handler emite ack rejeitado
visível (`interaction route unavailable`); a TUI nunca simula sucesso local.

### 7.4 Contrato de interação da v1

- `CapabilitySurface` carrega o modo atual e o conjunto visível de capabilities:
  `Auto` pode expor tools irrestritas; `Plan`/`Read-only` expõem read/list/search,
  resources e prompts, mas não shell, writes, scripts ou MCP tools.
- `ActivityView` identifica `tool`, `mcp` ou `child`, com id, estado, duração,
  preview, cancelabilidade e expansão.
- `PlanView` carrega versão, passos, dependências, status e `approval_required`.
- `GoalView` carrega estado, budget restante, causa de bloqueio e assurance.
- `InputRequestView` carrega id, pergunta, opções e estado de persistência.

- A sessão começa em `Auto`.
- `Shift+Tab` percorre `Auto → Read-only → Plan → Auto`; `/mode` é o caminho
  textual equivalente.
- `Plan` e `Read-only` exibem o catálogo de capabilities recebido em
  `CapabilitySurfaceChanged`; a TUI não calcula permissões.
- Aprovar/rejeitar emite `Approve`/`Reject`; somente o acknowledgement do
  harness confirma o resultado. A transição de Mode continua dependente de
  `ModeChanged`.
- `InputRequired` abre um bloco com pergunta/opções; o composer envia
  `AnswerInput`. Em headless, o mesmo pending permanece persistido e
  correlacionado pelo request ID.
- se existirem vários requests pendentes, a interação captura o mais recente;
  requests anteriores permanecem visíveis e correlacionáveis.
- `ActivityChanged` cobre tool, MCP e subagente. Não existe Jobs manager na TUI;
  atividade transitória é o único inspector operacional da v1.
- reasoning aparece como `ThinkingBlock` recolhido por padrão; expandir é uma
  ação visual e não altera o contexto enviado ao provider.
- mensagens aguardando execução viram `QueuedUserBlock` visível no fim do
  transcript, em ordem FIFO, com estado `queued` até serem consumidas
  (implementado em G244, 2026-08-25: Enter/Alt+Enter durante run enfileira;
  a fronteira de turno drena um prompt por vez via `SendPrompt`).

### 7.5 Provider OpenCode Go

`ProviderKind::OpenCodeGo` é identidade lógica própria. O modelo selecionado
resolve um protocolo de wire documentado — Chat Completions, Responses ou
Messages — sem probing faturável e sem depender de arquivos do Pi. O modelo
padrão é `deepseek-v4-flash`; a base oficial é
`https://opencode.ai/zen/go/v1`.

O registro embutido contém somente os 24 modelos presentes na tabela oficial
OpenCode Go consultada em 2026-08-24, mais `muse-spark-1.3-contributor`
adicionado em 2026-09-03 (ao vivo no catálogo público; metadados espelham o
checkpoint 1.2-contributor no mesmo gateway). IDs retornados por `/v1/models` sem
protocolo documentado não ficam selecionáveis. Context window, output cap,
imagem e reasoning só são anunciados quando há metadata conhecida; campo
incerto permanece desconhecido e nunca vira valor exato inventado.

`/models` materializa cache/fallback imediatamente e atualiza disponibilidade
em background pelo catálogo público bounded. Refresh válido intersecta IDs com
o registro; falha preserva cache/fallback. O catálogo não valida chave e não
carrega segredo. Credenciais obedecem `SLIM_API_KEY` → `OPENCODE_API_KEY` →
`auth.json`; a TUI coleta API key mascarada em `SensitiveText`, persiste pelo
writer atômico existente e nunca envia o segredo por transcript/notification.

### 7.6 Provider Command Code

`ProviderKind::CommandCode` é identidade lógica própria. A base oficial é
`https://api.commandcode.ai/provider/v1`. O modelo padrão é
`deepseek/deepseek-v4-flash`. Chat Completions em `/chat/completions`; modelos
cujo id começa com `claude` ou contém `/claude` usam Anthropic Messages em
`/messages`. Credenciais: `SLIM_API_KEY` → `COMMANDCODE_API_KEY` → `CMD_API_KEY`
→ `auth.json` (`command-code`).

`GET /models` é público e é a fonte da verdade: qualquer id com charset válido
fica selecionável, inclusive modelos ausentes do bundle. O bundle (~12 modelos)
só entra como fallback offline. Cache em `%USERPROFILE%\.slim\command-code-models.json`
(override `SLIM_COMMANDCODE_MODELS_FILE`). Aliases CLI: `command-code`,
`commandcode`, `cmd`.

### 7.7 Provider ClinePass

`ProviderKind::ClinePass` fala Chat Completions em
`https://api.cline.bot/api/v1/chat/completions`. O modelo padrão é
`cline-pass/qwen3.7-max`. Somente slugs `cline-pass/…` com charset válido são
aceitos — o catálogo ao vivo (`GET https://api.cline.bot/api/v1/models`, Bearer)
é a fonte da verdade; o bundle estático é fallback quando o GET falha ou volta
vazio. Credenciais: `SLIM_API_KEY` → `CLINEPASS_API_KEY` → `auth.json`.
Aliases CLI: `clinepass`, `cline-pass`, `cp`.

## 8. Estado da aplicação

### 8.1 AppState

```rust
pub struct AppState {
    pub session: SessionView,
    pub scrollback: ScrollbackState,
    pub composer: ComposerState,
    pub todo_dock: TodoDockState,
    pub overlays: OverlayStack,
    pub inspector: InspectorState,
    pub focus: FocusState,
    pub viewport: ViewportState,
    pub surface: SurfaceState,
    pub capabilities: SurfaceCapabilities,
    pub theme: ThemeState,
    pub notifications: NotificationState,
    pub keymap: Keymap,
    pub pending_effects: HashMap<EffectId, PendingEffect>,
    pub clock: FrameClock,
    pub activity: Option<ActivityState>,
    pub revisions: RevisionSet,
}
```

### 8.2 SessionView

Contém somente dados necessários à apresentação:

- session id e display name;
- cwd display-safe;
- modelo/provider/profile/effort;
- Mode e capability surface; não existe Permission state;
- context usage estimado/exato, window configurada, totais billable e cost;
- run state;
- prompt queue;
- Todo/Plan/Goal summary, incluindo version, budget e assurance;
- activity summary;
- diagnostics counters.

O transcript visual vive em `ScrollbackState`, não duplicado em `SessionView`.

### 8.3 RevisionSet

Revisions determinam invalidação:

```rust
pub struct RevisionSet {
    pub content: u64,
    pub fold: u64,
    pub theme: u64,
    pub viewport: u64,
    pub focus: u64,
    pub status: u64,
}
```

Não usar timestamps como revision. Uma mutação que não muda visualmente não
incrementa a revision correspondente.

## 9. Actions, reducer e effects

### 9.1 Action

```rust
pub enum Action {
    UiEventReceived(UiEvent),
    Key(KeyEvent),
    Paste(PasteEvent),
    Mouse(MouseEvent),
    Resize { width: u16, height: u16 },
    Tick(FrameClock),
    OpenOverlay(OverlaySpec),
    CloseTopOverlay,
    ToggleInspector(InspectorKind),
    ToggleTodoDock,
    ToggleBlock(BlockId),
    Scroll(ScrollIntent),
    Composer(ComposerAction),
    SubmitComposer(SubmitMode),
    EffectCompleted { id: EffectId, output: EffectOutput },
    EffectFailed { id: EffectId, error: EffectError },
    SurfaceLost(SurfaceError),
    RequestShutdown(ShutdownReason),
}
```

`ToggleBlock(BlockId)` valida o ID no reducer e alterna somente blocos com
display recolhível. A mutação incrementa `revisions.fold`, preserva o mesmo
`BlockId` e normaliza para zero o `row_offset` da âncora daquele bloco quando
o novo estado reduz sua altura.

### 9.2 Reducer

Assinatura conceitual:

```rust
fn reduce(state: &mut AppState, action: Action) -> Vec<Effect>;
```

Invariantes:

- reducer nunca bloqueia;
- reducer nunca chama async;
- reducer nunca escreve terminal;
- reducer não ignora erro silenciosamente;
- reducer valida IDs/revisions antes de aplicar delta;
- toda mudança visual incrementa revision apropriada;
- actions duplicadas são idempotentes quando o protocolo permitir.

### 9.3 Effect

```rust
pub enum Effect {
    Send(UiCommand),
    CopyToClipboard { text: Arc<str> },
    OpenUrl { url: Arc<str> },
    LoadImage { image_id: ImageId, source: ImageSource },
    PersistUiSetting(SettingMutation),
    RequestRender(RenderUrgency),
    ExitProcess(ExitIntent),
}
```

`RequestRender` é hint; o scheduler pode coalescer. `Send`, completion e errors
não podem ser descartados.

## 10. Event loop e scheduling

### 10.1 Canais

Usar um canal core→projector bounded de 1.024 eventos e, depois da projeção,
dois lanes bounded:

- **control lane imediata:** input, abort/cancel, resize, auth e estado global;
- **stream lane ordenada:** `RunStarted`, user echo, deltas de texto/reasoning,
  boundaries de thinking, lifecycle e progress de tool, accounting causal
  (`ContextSnapshot`/`Usage*`), activity/input, fatal causal, `AssistantEnded`
  e terminais de run.

A stream lane é bounded e lossless; somente deltas adjacentes compatíveis podem
ser coalescidos. Start, conteúdo e terminal do mesmo run compartilham a mesma
ordem causal, evitando que `RunStarted` ou completion ultrapassem seus próprios
deltas. O control lane continua drenado primeiro para cancelamento responsivo,
com tombstone até o próximo `RunStarted` ordenado. Fairness por iteração: no
máximo 32 control events e depois no máximo 1.024 stream events; se o budget
control esgotar, o batch seguinte continua control antes de tocar data, porque
a posição 33 ainda pode conter um terminal causal; um terminal consumido
exatamente na posição 32 também não libera seu sufixo até o próximo batch
drenar o prefixo data já enfileirado. Ao atingir o
budget, flushar o chunk, desenhar se dirty e devolver controle ao loop/input
antes do próximo batch; disconnect nunca pula flush/draw final. O projector usa backpressure interruptível por `CancellationToken`: durante execução, request inteira permanece lossless e ordenada na stream lane. Após cancel, deltas visuais de texto/reasoning e snapshots intermediários de tool progress podem ser abandonados; accounting causal, boundaries de tool, `AssistantEnded` e o protocolo de interação tipado (request/ack) migram para control e permanecem lossless. O consumer retém qualquer sufixo migrado sem drenar data até receber a identidade terminal (mesmo se o budget control quebrar o batch), depois drena apenas até o próximo `RunStarted` monotônico, aplica o causal antigo e só então ativa a próxima run; tombstone não bem-sucedido permanece imediato, enquanto `RunCompleted` expedido sempre é deferido até o fence data e fecha o fim do próprio sufixo para promover exatidão somente depois do accounting. ACK sem rota que chega durante `PendingRun` nunca ocupa o lugar do terminal: o terminal real é localizado por tipo e o ACK migra depois dele, preservando request→ack mesmo com a stream cheia. A task recebe cancelamento cooperativo e só sofre abort de segurança após timeout; assim usage e interação já observados não são perdidos, enquanto o desfecho verdadeiro segue no control lane para a stream cheia não ocultar o cancel.
`RunStarted` e todos os desfechos carregam `run_id` monotônico: um terminal que
ultrapassou a fila rejeita depois o start do mesmo ID, sem bloquear o próximo.
O tombstone preserva também o outcome real — Complete, Failed ou Cancelled —
para que assistant/tool tardios nunca sejam reclassificados como cancelados.
Boundaries de tool já iniciada não são deltas visuais descartáveis:
`ToolStarted/ToolEnded` permanecem lossless e migram para control após cancel;
`ToolProgress` é snapshot intermediário e pode ser descartado somente depois do
cancelamento para não transformar uma data lane cheia em deadlock.
Boundaries migradas entram no sufixo causal e só são reduzidas depois do
prefixo já enfileirado na data lane, evitando `ToolEnded` órfão.
Completion normal usa `try_send`; com stream exatamente cheia, `PendingRun`
continua atendendo Cancel/Shutdown em vez de bloquear o worker. Cada producer
sinaliza o wake event compartilhado após publicar e fecha sua cópia dos senders
antes do wake final de disconnect. Como `SetEvent` auto-reset coalesce sinais,
batch que esgota 32 control ou 1.024 data rearma o evento para o próximo batch.
O event loop espera atomicamente console input, wake das lanes ou deadline
visual, sem poll idle; outcome aceito para um `run_id` torna-se imutável.

Capacidades iniciais sugeridas:

- control: 256;
- data: 1.024;
- effects in-flight: 128.

Valores são configuráveis no testkit e viram métricas. Overflow do control lane
é fatal e visível. A data lane aplica backpressure e o coalescer devolve chunks
lossless ao atingir capacity; nenhum lifecycle/data event é descartado.

O canal core→projector usa `sync_channel(1_024)`. O sender faz `try_send` em
loop curto e consulta o mesmo `CancellationToken` da run: antes do cancel todo
evento aplica backpressure; depois do cancel texto/reasoning visual e
`ToolOutput` ainda não entregue podem ser abandonados. Accounting, boundaries
de tool, `AssistantEnded` e interação continuam lossless. O receiver permanece
ativo até o sender fechar. Se a fila estiver cheia no instante do cancel, um
evento causal pode substituir o payload visual mais antigo já retido; assim o
handoff permanece bounded, não bloqueia atrás de apresentação descartável e
não corta o sufixo causal.

### 10.2 Coalescing

| Evento | Regra |
|---|---|
| assistant text delta | concatenar apenas deltas adjacentes da stream corrente; `AssistantEnded` fecha a identidade |
| thinking delta | concatenar apenas deltas adjacentes entre `ThinkingStarted` e `ThinkingEnded` |
| tool progress | manter o snapshot mais recente somente para o mesmo `ToolBatchId + ToolCallId` |
| usage estimate | manter maior `context_tokens` somente para o mesmo `run_id` + `request_id` + window; run/request/window nova (inclusive window 0) substitui |
| resize | manter apenas dimensões mais recentes |
| tick | no máximo um pendente |
| mouse move/scroll | acumular delta dentro da frame window |
| lifecycle start/end | nunca coalescer |
| errors | nunca coalescer |

Antes de aplicar um `Ended`, flushar deltas pendentes da mesma entidade.
O coalescer pertence ao estado do event loop, não a uma chamada de drain:
persiste entre wakes, concatena thinking por stream, mantém somente o progress
mais recente da mesma `ToolCallId` e é descarregado por capacity, boundary,
disconnect ou deadline. Uma mudança de entidade nunca atravessa o flush causal.

### 10.3 Frame scheduling

- input local pede frame imediato;
- primeiro delta de cada stream de assistant/thinking e o primeiro progress de
  cada tool pede frame imediato; o drain devolve ao draw antes de consumir o
  delta seguinte;
- deltas seguintes respeitam janela máxima de 16 ms;
- cada iteração reduz no máximo 32 eventos de controle e 1.024 de stream; ao
  atingir o teto, rearma o wake antes de devolver o controle ao input/render;
- completion/error pede frame imediato;
- spinner, streaming caret, progress rail e tool progress usam `FrameClock`;
- motion ativo usa 12 fps nominais (tick de 83 ms); não tentar 60 fps sem medição;
- welcome-only é estática e não agenda redraws periódicos;
- reduced motion ou motion desativado elimina ticks ambientais e caret animado;
  enquanto a ActivityRail estiver visível, um `StatusTick` semântico de 1 Hz
  ainda atualiza elapsed; modal cobrindo a superfície suspende ambos;
- sem animação/status visível e sem estado dirty, não existe timer periódico:
  no Windows, o runtime bloqueia em `WaitForMultipleObjects` sobre input do
  console + evento waitable sinalizado pelos producers das lanes; input/resize
  e bridge events acordam imediatamente, sem polling;
- toast visível participa somente com seu deadline exato de expiração; não
  reintroduz polling periódico no estado ocioso;
- quando há delta coalescido pendente, seu deadline restante participa do mesmo
  wait do console/wake; timeout descarrega o chunk antes de qualquer novo wait;
- backpressure suspende frames não urgentes e reduz primeiro motion ambiental;
- ao drenar stdout, emitir o frame mais recente, não cada frame perdido.

### 10.4 Pseudocódigo

```rust
loop {
    let action = scheduler.next_action().await;
    let effects = reduce(&mut state, action);
    effect_runner.spawn_all(effects);

    if render_scheduler.should_render(&state.revisions) {
        let view = ViewModel::derive(&state);
        let plan = layout_engine.plan(&view, surface.size());
        surface.draw(&view, &plan, &mut render_context)?;
        render_scheduler.commit(state.revisions);
    }

    if state.surface.shutdown_ready() {
        break;
    }
}
```

## 11. Modelo de blocos

### 11.1 Estrutura comum

```rust
pub struct Block {
    pub id: BlockId,
    pub source: BlockSource,
    pub created_seq: u64,
    content_generation: u64,
    cache_identity: u64,
    pub display: BlockDisplayState,
    kind: BlockKind,
}
```

`kind`, `content_generation` e `cache_identity` não são superfícies públicas de
mutação. Leitura usa getters imutáveis; toda alteração de conteúdo passa por
métodos tipados que incrementam `content_generation`. `cache_identity` distingue
instâncias criadas separadamente mesmo quando ID e geração coincidem. A coleção
de blocos em `AppState` também é append-only fora do reducer e rejeita IDs
duplicados.

### 11.2 Variantes

```rust
pub enum BlockKind {
    User(UserBlock),
    QueuedUser(QueuedUserBlock),
    Assistant(AssistantBlock),
    Thinking(ThinkingBlock),
    ToolCall(ToolCallBlock),
    ToolResult(ToolResultBlock),
    System(SystemBlock),
    Compaction(CompactionBlock),
    Plan(PlanBlock),
    InteractionRequest(InteractionRequestBlock),
    Activity(ActivityBlock),
    Error(ErrorBlock),
    Custom(CustomBlock),
}
```

`QueuedUserBlock` preserva o texto integral, posição FIFO, timestamp e estado
`queued`; ele vira `UserBlock` somente quando o harness emitir o evento de
consumo. `PlanBlock` mostra a versão aguardando aprovação ou o resultado da
execução. `InteractionRequestBlock` preserva ID, tipo input/approval,
pergunta/summary, opções, flag `persisted` e `response_pending`; seu lifecycle
só termina por acknowledgement correlacionado. `ActivityBlock` é a projeção
comum de tool, MCP e child.

### 11.3 Display state

```rust
pub struct BlockDisplayState {
    pub fold: FoldState,
    pub lifecycle: BlockLifecycle,
    pub selected: bool,
    pub search_match: Option<SearchMatchState>,
}

pub enum FoldState { Auto, Collapsed, Expanded }
pub enum BlockLifecycle { Pending, Streaming, Complete, Failed, Cancelled }
```

`Auto` é resolvido por tipo e contexto:

- user: expanded;
- queued user: expanded with muted `queued` state and FIFO position;
- assistant: expanded;
- thinking: collapsed, header + preview de no máximo três linhas físicas; o
  header não conta nesse limite e o preview usa o mesmo wrap Unicode da
  medição; no contrato corrente, esse preview é a cauda de no máximo duas rows
  e só aparece enquanto o lifecycle está `Streaming`; antes do wrap, a entrada
  é limitada aos 256 grafemas finais sem cortar clusters Unicode;
- tool call: summary de uma linha;
- tool result: collapsed quando grande, expanded quando curto/error relevante;
- system/compaction: collapsed com summary;
- plan: expanded when awaiting approval, collapsed after approval;
- activity: one-line summary, expanded in-place;
- error: expanded.

### 11.4 Regras de tool blocks

Cada tool call mostra:

- glyph de lifecycle;
- tool name;
- argumento resumido seguro;
- duração;
- status;
- progress curto;
- hint de expand/cancel quando aplicável.

Output integral não é duplicado na TUI. `ToolResultBlock` mantém um
`ContentHandle`; o renderer acessa apenas preview materializado. Expandir pode
emitir `UiCommand::RequestContentPage`. O harness responde com
`UiEvent::ContentPageLoaded`; páginas são anexadas somente se handle/cursor
e request ID corresponderem ao request pendente. Uma resposta stale, duplicada
ou fora de ordem é ignorada. Cada página possui no máximo 16 KiB em fronteira
UTF-8; o store do harness é limitado a 128 outputs, 2 MiB por output e 8 MiB
no total, sempre depois da redação. A TUI retém somente preview e páginas
explicitamente materializadas, limitadas a 2 MiB por bloco.

A identidade efetiva na TUI é `(run_id, ToolBatchId, ToolCallId)`: o bridge
aplica namespace a batch e call por run tanto no worker assíncrono quanto na API síncrona.
Logs legados sem IDs recebem identidade sintética determinística ancorada no
`seq` do `ToolStarted`; o executor legado emitia Start/Output/Finished em três
sequências consecutivas. Sufixo órfão ou terminal duplicado não altera um bloco:
gera diagnóstico e solicita resync; somente o primeiro terminal fixa duração e
outcome visual.

#### 11.4.1 Agregação visual por turno

Cada resposta do provider possui um `ToolBatchId`; suas calls executam
serialmente na ordem recebida. Calls concluídas com sucesso são agregadas
**somente para apresentação** quando são consecutivas no transcript
(independente do `ToolBatchId` e do nome), porque um turno com uma tool
por batch empilhava `✓ shell` em rows separadas:

```text
✓ 4 tools · read ×3, shell · 42ms               Enter details
```

O modelo de blocos continua preservando cada call, argumento, resultado,
duração e `ContentHandle` individual. A projeção agregada não concatena outputs
e não altera replay, telemetria ou contabilidade.

Ao expandir, o header permanece e os membros aparecem em ordem original com
`ToolCallId`, nome, argumentos seguros, duração e preview. A expansão não busca
outputs automaticamente; cada `ContentHandle` continua pertencendo ao membro.

Regras normativas:

- o grupo termina quando aparece um bloco que não é tool concluída com sucesso
  (thinking, assistant, user, failed ou cancelled); `ToolBatchId` distinto
  **não** quebra o grupo;
- argumento/target não aparece na row agregada; fica nos detalhes;
- nomes são resumidos na ordem da primeira ocorrência, com `×N`, no máximo três
  nomes distintos e `+N` para o restante; se a largura não comportar, degrada
  para `✓ N tools · duração`;
- duração total só aparece quando todos os membros possuem duração conhecida;
- pending e running ocupam row própria enquanto estiverem ativos;
- failed e cancelled sempre ocupam row própria e mostram o comando/target
  acionável; nunca ficam escondidos em uma contagem;
- quando uma call running conclui com sucesso, ela pode ser incorporada ao grupo
  do próprio batch sem atravessar failure/cancel;
- o glyph ocupa a coluna da rail; o texto da tool começa no mesmo eixo das
  mensagens;
- `Enter` expande o grupo inline, sem overlay e sem scroll aninhado; `Enter`
  novamente recolhe;
- em live edge, `Enter` simples com composer vazio ativa diretamente o último
  bloco recolhível visível; não exige um `Up` preparatório e não muda o scroll;
- a expansão é completa e pode crescer verticalmente porque só ocorre por ação
  explícita; nenhum dado é truncado.

### 11.5 Custom blocks

Somente em M5:

```rust
pub struct CustomBlock {
    pub schema_version: u16,
    pub renderer_id: Arc<str>,
    pub payload: serde_json::Value,
    pub fallback_text: Arc<str>,
}
```

Sem renderer registrado, usar `fallback_text`. Custom renderer não recebe
terminal handle nem `AppState` mutável.

## 12. Pipeline de renderização

### 12.1 Estágios

```text
Block content
→ parse Markdown/ANSI control-safe
→ syntax highlight
→ logical lines
→ wrap por largura
→ BlockOutput + metadata
→ Entry chrome/padding/accent
→ viewport clipping
→ overlay composition
→ frame buffer
→ backend diff/flush
```

### 12.2 BlockOutput

```rust
pub struct BlockOutput {
    pub lines: Arc<[Line<'static>]>,
    pub logical_line_map: Arc<[LogicalLineRef]>,
    pub hit_regions: Arc<[HitRegion]>,
    pub copy_joiners: Arc<[CopyJoiner]>,
    pub measured_width: u16,
    pub generation: u64,
}
```

`copy_joiners` preserva a diferença entre quebra lógica e quebra visual. Copiar
texto reconstruirá linhas originais, não incluirá glyph de continuação.

### 12.3 Caches

#### ParseCache

Chave:

```text
(BlockId, content_generation, markdown_mode, highlight_theme)
```

Valor: logical styled lines antes de wrap.

#### WrapCache

Chave:

```text
(ParseKey, available_width, fold_state)
```

Valor: `BlockOutput`.

#### LayoutCache

Chave:

```text
(viewport_width, viewport_height, theme_revision, fold_revision,
 todo_dock_state, inspector_state, overlay_revision)
```

Valor: `LayoutPlan` e heights conhecidos.

### 12.4 Limites de cache

Todos os caches são LRU/bounded:

- ParseCache: por bytes aproximados;
- WrapCache: por número de rows + bytes;
- LayoutCache: poucas entradas recentes;
- image decode cache: por bytes;
- syntax definitions: inicialização lazy única.

Eviction nunca remove conteúdo fonte, apenas derivação reproduzível.

Budgets iniciais:

| Cache | High-water | Low-water |
|---|---:|---:|
| ParseCache | 16 MiB | 12 MiB |
| WrapCache | 32 MiB ou 100.000 rows | 24 MiB ou 75.000 rows |
| LayoutCache | 16 plans | 8 plans |
| image decode cache | 64 MiB | 48 MiB |

Ao cruzar qualquer high-water, evictar LRU até o low-water correspondente. Se
um único item exceder o budget completo, não cacheá-lo.

### 12.5 Matriz de invalidação

| Mudança | Parse | Wrap | Layout |
|---|---|---|---|
| novo delta de texto | invalida bloco | invalida bloco | atualiza height |
| largura | preserva | invalida | invalida |
| theme de syntax | invalida code blocks | invalida dependentes | invalida |
| theme só de chrome | preserva | preserva quando linhas não embutem cor | invalida |
| fold | preserva | invalida bloco | invalida |
| scroll offset | preserva | preserva | recalcula clipping |
| focus | preserva | preserva | repaint de chrome |
| overlay | preserva | preserva | recompõe overlay |

## 13. Virtualização e scrollback

### 13.1 M1

M1 pode somar heights linearmente porque o corpus inicial é pequeno. Mesmo
assim, só renderiza blocos que intersectam a viewport.

### 13.2 M2 final

Introduzir `HeightIndex` com prefix sums:

- key por `BlockId` e wrap width;
- height exata quando medida;
- estimate conservadora quando ainda não medida;
- busca do primeiro bloco visível em `O(log n)`;
- lookup de prompt/anchor em `O(1)` por mapa `BlockId → (prefix,height)` criado
  durante o mesmo rebuild `O(n)` do índice;
- updates de height em `O(log n)`;
- anchor estável por `(BlockId, row_offset)`.

Ao mudar largura:

1. capturar anchor atual;
2. invalidar heights daquela width;
3. medir primeiro a região do anchor;
4. reconstruir prefix sums em budget de trabalho;
5. reposicionar anchor;
6. medir off-screen apenas sob demanda.

Isso evita salto para o início durante resize.

### 13.3 Estados de scroll

```rust
pub struct ScrollAnchor {
    pub block_id: BlockId,
    pub row_offset: u64,
}

pub enum FollowMode {
    LiveEdge { prompt_id: Option<BlockId> },
    Pinned(ScrollAnchor),
    Top,
}
```

Novo turno em `LiveEdge` grava o prompt corrente. Enquanto prompt + resposta
cabem na viewport, o prompt permanece no topo útil e linhas restantes ficam
livres; após overflow, `LiveEdge` acompanha a cauda. O primeiro scroll para cima
saindo desse page-fill entra em uma página histórica completamente preenchida,
para append posterior não ocupar linhas vazias de uma viewport pinned.
O renderer usa obrigatoriamente `HeightIndex::metrics(...).viewport_start`; não
recalcula `LiveEdge` como bottom. A row de separação entre turnos conta na altura
total, mas a âncora do `prompt_id` começa no label do usuário, nunca no separador.

`Pinned` resolve `(BlockId,row_offset)` contra o `HeightIndex`, nunca distância
do fim. Append incrementa `unseen`, mas não move o anchor. Resize reconstrói
heights na largura final — incluindo a coluna reservada pela scrollbar — e
clampa `row_offset` à altura medida; anchor removido degrada para `Top`. `Home`
vai a `Top`; `End` volta a `LiveEdge` sem prompt preso.

| Estado | Evento | Próximo estado |
|---|---|---|
| LiveEdge | novo bloco/delta que cabe | LiveEdge, prompt no topo útil |
| LiveEdge | overflow | LiveEdge, cauda visível |
| LiveEdge | scroll para cima | Pinned(anchor histórico, 0 unseen) |
| Pinned/Top | novo bloco | mesmo anchor/top, unseen +1 |
| Pinned/Top | End | LiveEdge |
| Pinned | scroll até final | LiveEdge |
| qualquer | session replace | LiveEdge |

## 14. Layout responsivo

### 14.1 Regiões fullscreen

```text
  cwd display-safe ─────────────────────── context atual
│ Scrollback                                      │ Inspector? │
│                                                 │            │
├ Todo dock? ──────────────────────────────────────────────────┤
  Activity rail? · working/retry/cancel
 ╭ Composer: draft ─────────────────── model (effort) · mode ╮
 ╰────────────────────────────────────────────────────────────╯
  shortcuts/status ─────────────────────── context/tokens
```

Sem inspector docked, o transcript usa toda a largura disponível do scrollback;
não existe coluna de leitura fixa nem centralização lateral. O limite de 144
células aplica-se apenas ao workspace compartilhado com inspector docked.

`SessionRail` existe somente quando há transcript, largura `≥80`, altura `≥12` e
cwd informativo. Cwd vazio, `~` ou o diretório home resolvido são triviais e não
consomem uma row.
Ocupa uma row no topo com cwd display-safe à esquerda e contexto atual à direita.
A projeção compartilhada sanitiza controles, normaliza newline para espaço e
trunca por grapheme/célula antes de posicionar contexto; fullscreen e ViewModel
usam a mesma largura real. Welcome e layout de emergência nunca a exibem. Quando visível, remove contexto
do footer sem remover totais `↑/↓`; quando oculta, o footer volta a projetar o
contexto compacto. Não há branding permanente no topo.

`ActivityRail` ocupa zero ou uma row imediatamente acima do composer, depois do
Todo dock. Só aparece durante run/retry/tool longa/input requerido; mostra o
spinner existente e cancelamento sem duplicar transcript. Se a altura exige
removê-la, estado crítico migra para o footer como `Working…` com Esc/Ctrl+C.
Atalhos são contextuais: autenticado ocioso anuncia `Ctrl+C:exit`; durante run,
`Ctrl+C:cancel`. A borda inferior do composer é o separador: `op_divider`
reserva zero rows.

### 14.2 Breakpoints

| Largura | SessionRail | Inspector | Operational bar |
|---:|---|---|---|
| `<80` | oculta | overlay central/full-height | contexto compacto + usage |
| `80–99` | transcript + altura ≥12 | overlay central/full-height | usage sem contexto |
| `100–139` | transcript + altura ≥12 | drawer temporário que reduz scrollback | usage sem contexto |
| `≥140` | transcript + altura ≥12 | drawer simultâneo opcional | usage sem contexto |

Os breakpoints acima valem para Diff, Activity, Session Tree e Diagnostics. Eles
**não alteram o Todo**.

### 14.3 Todo dock

Quando existe Todo ativo, o dock ocupa toda a largura imediatamente acima do
composer. Não é sidebar, drawer nem overlay.

| Condição | Altura do dock | Conteúdo |
|---|---:|---|
| sem Todo ativo | 0 rows | dock ausente |
| Todo ativo, estado compacto | 2 rows | progresso + item ativo; resumo dos demais |
| terminal com altura `<10` | 1 row | progresso + item ativo truncado |
| estado expandido | até 6 rows | lista completa com scroll interno se necessário |

Regras:

- mesma posição em qualquer largura;
- expandir faz reflow do scrollback; nunca cobre conteúdo;
- items não fazem wrap no modo compacto; usam ellipsis;
- `Ctrl+T` alterna compacto/expandido;
- quando expanded, o dock pode receber foco e setas; `Esc` volta ao compacto;
- o dock é projeção de `TodoState`, nunca autoridade paralela;
- mudança de Todo chega como `UiEvent::TodoChanged`, incrementa
  `todo_dock_revision` e persiste no event log do harness; não exige um novo
  bloco de chat a cada atualização.

### 14.4 Altura

Prioridade quando a altura diminui dentro do mínimo suportado (`40×8`):

1. preservar ao menos uma row editável do composer e a operational bar mínima;
2. ocultar SessionRail e devolver contexto compacto à operational bar;
3. ocultar ActivityRail e mover estado ativo para a operational bar;
4. remover divider do Todo;
5. reduzir Todo dock de duas para uma row, depois ocultá-lo;
6. entregar todo espaço restante ao scrollback;
7. overlays aplicam scroll interno.

Em altura `>=16`, o composer cresce sem animação até cinco rows de conteúdo
(sete com borda). Entre `8–15`, preserva uma row de conteúdo no box de três
rows. A row compacta sem box existe somente no layout de emergência abaixo de
`40×8`. Largura/altura suportadas abreviam label e atalhos antes de consumir o
scrollback.
Nunca renderizar área negativa ou usar `saturating_sub` para esconder erro de
layout. O planner valida invariantes e retorna layout de emergência explícito.

### 14.5 Layout de emergência

Para terminal menor que 40×8:

- esconder SessionRail, inspector e ActivityRail;
- manter Todo ativo em uma linha truncada;
- uma linha de operational bar mínima;
- uma linha de composer sem box;
- scrollback no restante;
- remover ambient motion;
- toast: “terminal muito pequeno”; sem panic.

## 15. Componentes

### 15.1 Footer operacional

O footer usa uma row imediatamente abaixo da borda do composer, sem
divider/row vazia intermediária. Sem SessionRail ele mostra contexto + usage:

```text
Shift+Tab:mode │ Ctrl+C:cancel │ Ctrl+P:commands  ctx ~42% · 9.4k/22.4k · ↑0 ↓0
```

Shortcuts realmente acionáveis ficam à esquerda; contexto/usage à direita. Com
SessionRail visível, o contexto aparece somente nela e o footer preserva os
totais `↑/↓` somente depois de algum token observado. `ctx --`, `↑0` e `↓0`
não aparecem no estado inicial. A window vem da configuração real da request — nunca de literal
universal. Prefixo `~` identifica estimativa; usage final seguido de `AssistantEnded` fecha o accounting ainda aproximado, e somente `RunCompleted` confirma o contexto da request e remove `~`. Estimativas correlacionadas são monotônicas por `(run_id, request_id, window)`; identidade anterior, conflitante ou pós-terminal é ignorada. Headless/JSONL expõem `usage_complete` e `usage_overflowed` junto aos totais.
`model (effort) · mode` vive no label inferior do composer. Estado crítico
(signed-out `/login`, working/cancel, error/retry, pinned/unseen) substitui
shortcuts antes de qualquer truncamento. Em largura reduzida, usar variantes
abreviadas medidas em células; nenhum grupo faz wrap ou invade o outro.
Quando o cwd é trivial, a SessionRail fica oculta e o contexto retorna ao footer.
No headless texto, a resposta é a primeira saída humana. `--verbose` acrescenta
depois dela stop, usage e timeline de tools derivada dos eventos já redigidos
(nome, status, contagem e duração), sem argumentos nem outputs; a única exceção
é a linha de falha (`✕`), que inclui a primeira linha redigida do output/preview
do mesmo call_id como razão curta (mesmo conteúdo que a TUI projeta na row de
tool falha). JSONL permanece
byte-stable; `--verbose --jsonl` é inválido.

```text
^C stop                                      ctx ~42% · ↑0 ↓0
```

`SLIM` aparece no empty state, não como branding permanente no footer. O footer
idle é estático; somente o spinner operacional existente anima, congelando sob
reduced motion sem alterar layout.

### 15.2 ScrollbackView

Responsabilidades:

- localizar faixa visível;
- pedir outputs ao cache;
- compor block chrome;
- produzir hit regions;
- mostrar live-edge/unseen indicator;
- fornecer seleção/copy/search.
- renderizar input/approval como bloco inline: pergunta/summary, opções ou hint
  `Y approve · N reject`, estado persisted/ephemeral e ack aceito/rejeitado.

Não interpreta `AgentEvent`; recebe blocks já materializados.

A seleção de teclado é o `BlockId` recolhível na `ScrollAnchor` estável,
nunca um índice efêmero. `Up`/`Down`/`PageUp`/`PageDown` continuam movendo a
âncora; o primeiro `Up` vindo de live edge ancora somente o último bloco
recolhível ainda visível. Quando todo o transcript cabe no viewport, os
movimentos seguintes percorrem esses blocos sem deslocar artificialmente a
tela. Sem bloco recolhível visível, o scroll row/page existente é preservado e
nunca salta para conteúdo histórico. `End` limpa o foco e retorna
ao live edge. Reflow por fold/resize preserva o `BlockId` selecionado e mantém
o `row_offset` dentro da nova altura.

#### 15.2.1 Grade, largura e espaçamento

- chrome conversacional segue a hierarquia Grok CLI: user em faixa elevada,
  metadata (thinking/tools) em `muted`, resposta em `text` com headings
  cromáticos; sem card completo por mensagem;
- prompt do usuário usa `user_prompt_bg` em toda largura útil, com padding
  horizontal de duas células; não usa borda, cantos nem rail `│`; label `You`
  em row própria, corpo imediatamente abaixo, alinhados no mesmo eixo;
- após o corpo do user há exatamente uma row vazia (fora da faixa) antes de
  thinking, tools ou assistant; o `HeightIndex` conta essa row;
- assistant permanece sobre `surface`, com padding de duas células em todas as
  rows e sem label `Slim` por bloco; H1 em `assistant_accent` carrega a
  identidade do papel;
- thinking colapsado ocupa uma row muted `{glyph} Thought` quando completo;
  em streaming, `{spinner} Thinking` é seguido pela cauda atualizada de até
  duas rows físicas dos 256 grafemas finais do reasoning, com `…` quando o
  início ficou oculto; o corpo completo só aparece com `fold == Expanded`;
  `FoldState::Auto` conta
  como colapsado; bloco omitido se reasoning vazio;
- tools colapsados ocupam uma row: glyph de lifecycle colorido, nome(s) e
  duração em `muted`; concluídos não exibem `command=` nem preview, mas
  tools ativas mantêm nome, limite e progresso e tools falhas mantêm uma razão
  curta redigida. Toda summary ocupa exatamente uma row física: o comando perde
  largura primeiro. Args e output completos só aparecem expandidos; `Enter
  details` à direita quando cabe **somente** no grupo selecionado ou no último
  grupo colapsado do run ativo;
- prosa Markdown faz soft-wrap no último limite de palavra disponível; código,
  URLs e tokens sem ponto de quebra maior que a largura continuam em hard-wrap;
- o primeiro fragmento nomeado de tool call troca a atividade para
  `Preparing tool · <name>` e fecha apenas o caret visual do assistant; a
  publicação/execução da tool continua dependente do terminal autoritativo;
- footer `ctx`: percentual inteiro com piso de 1% quando `tokens > 0`;
- dentro de um turno, thinking, tools e assistant são adjacentes (sem row
  vazia extra entre eles);
- antes de todo `User` posterior ao primeiro há exatamente uma row física vazia,
  medida pelo `HeightIndex` e materializada pelo renderer; o primeiro user não
  recebe espaçamento superior;
- mensagens usam 100% da largura útil do scrollback: sem `max-width`, coluna
  central ou balões estreitos;
- headings Markdown removem os marcadores visuais `#`/`##`/`###`: H1 usa
  `assistant_accent`, H2 usa `heading_accent`, H3 usa `thinking_accent` e níveis
  maiores usam `heading_accent`; links usam `link_accent`; prosa comum permanece
  em `text`;
- blocos Markdown de nível raiz (heading, parágrafo, lista, código, quote,
  tabela, hr) têm exatamente uma row vazia entre si; itens de lista não ganham
  gap extra; tabelas GFM alinham colunas na largura útil e não quebram no wrap;
- texto longo quebra na borda direita; linhas de continuação alinham com o
  texto, nunca com um rail;
- inspector aberto e scrollbar reduzem a largura útil; não mudam a regra;
- timestamp aparece dim e alinhado à direita quando largura `≥100`; some abaixo
  disso. O instante continua metadata para replay/diagnóstico;
- duração de reasoning/tool pode aparecer junto do lifecycle; tokens/custo ficam
  agregados nas rails operacionais;
- scrollbar ocupa uma célula somente quando `total_rows > viewport_rows`; a
  largura útil é então remedida com `width - 1`; track é quase invisível, thumb
  tem mínimo de uma row e usa estilo muted em live edge/normal em pinned;
- code e diff preservam linhas e usam viewport horizontal.

### 15.3 Composer

Estado:

- rope/string buffer;
- cursor por grapheme;
- selection;
- preferred column;
- undo/redo bounded;
- history de prompts;
- completion state;
- paste state;
- attachments;
- draft revision.

Limites iniciais:

- draft/paste textual: 1 MiB; acima disso, recusar com mensagem e oferecer
  importação como attachment pelo harness;
- undo/redo: menor entre 200 operações e 4 MiB de deltas;
- prompt history: 100 entradas, sem attachments binários;
- completion candidates: 500 antes de filtro/viewport;
- attachments: apenas metadata/handles na TUI.

Regras:

- em dimensão suportada (`width ≥ 40`, `height ≥ 8`), box arredondado completo,
  insetado em uma célula horizontal; contém uma row editável entre `8–15` rows
  de viewport e cresce até cinco rows de conteúdo em `height ≥16`;
- uma row sem box existe somente no layout de emergência abaixo de `40×8`;
- composer/footer usam `surface` do transcript, sem faixa pesada, sombra ou
  gradiente;
- borda neutra (`border`) em todos os estados; foco sinalizado pelo
  glifo ASCII `>` em `accent` (muted quando ocioso) e pela borda
  `border_focus`; `›` (U+203A) é East-Asian Ambiguous e no console
  Windows avança duas células enquanto unicode-width/Ratatui contam uma,
  o que estaciona o caret sobre a última letra digitada (G264) — o
  composer não usa esse glifo;
- `model (effort) · mode` ocupa o label inferior direito em `muted`; abrevia por
  largura sem tocar os cantos;
- topo, conteúdo e base usam o mesmo `Rect`; `Block::title_bottom` substitui
  células internas, nunca encurta a base ou desloca `╯`;
- texto digitado usa viewport horizontal em torno do cursor; cada linha lógica
  longa não faz wrap e mantém o hint de overflow;
  o recorte à esquerda usa hint ASCII `<` (não `‹`, G264);
- draft multiline continua permitido; a viewport vertical mantém a linha do
  cursor visível e mostra `N lines` quando houver mais de uma;
- Enter envia quando completion/modal não captura;
- Shift+Enter insere newline quando distinguível;
- Ctrl+Enter também insere newline como fallback Windows configurável;
- Alt+Enter envia como steer quando run ativo;
- paste multiline nunca envia automaticamente;
- draft é preservado se send falhar e quando um slash command é rejeitado
  localmente (modelo desconhecido, path ausente, compact sem auth), para
  correção e reenvio sem redigitar após o toast expirar;
- secret-looking paste não entra em telemetry.

Bracketed paste é armazenado como `PasteSegment`, separado do texto digitado.
O renderer mostra o conteúdo literal apenas quando ele cabe em uma row e não
contém newline. Caso contrário, mostra um segmento atômico:

```text
[Pasted Content 1925 chars]
```

`chars` conta Unicode scalar values, nunca bytes. O texto real permanece no
draft e é enviado integralmente. Cursor pula o segmento como uma unidade;
Backspace/Delete sobre ele remove o paste inteiro; `Ctrl+Z` restaura/remove a
operação inteira. Resize pode alternar entre literal e token sem alterar o
estado armazenado.

Quando o composer tem foco, `>`, cursor e borda ativa usam
`assistant_accent`/`border_focus`. O background muda de `surface_alt` para
`composer_bg`; essa mudança é semântica e estável, não um flash. A borda nunca
pulsa nem alterna bold durante run. Streaming ativo
reserva uma célula no fim do assistant block para o caret `▌`; reduced motion e
estado completo mantêm a célula como espaço, sem reflow. O caret nunca entra no draft.

### 15.4 Sinais operacionais e ActivityRail

`ActivityRail` materializa a fase transitória corrente: Thinking, Responding,
tool em execução, AwaitingProvider, awaiting input, fase External, elapsed e
ação de cancelamento. `Thinking` começa em `ThinkingStarted`; `Responding`
começa no primeiro `AssistantDelta`; `ToolEnded` entra em `AwaitingProvider`
até reasoning/texto novo ou outcome terminal. Silêncio e timers nunca inventam
uma transição de domínio.
`Retrying` só pode aparecer por evento real. Possui uma row e desaparece quando
deixa de ser acionável; `ActivityBlock` fica reservado a histórico persistido:

```text
◒ Responding · 15s                         ctx ~9.45k/22.4k · Ctrl+C stop
```

Spinner usa sequência estável de glyphs e 12 fps nominais em motion normal. O
elapsed deriva de `FrameClock.elapsed_ms - ActivityState.started_ms`, nunca de
`Instant` no AppState. Tool com
progresso conhecido usa `progress_track/progress_fill`; sem progresso conhecido,
usa spinner e elapsed, nunca barra falsa.

`FirstByte` significa apenas que o stream HTTP abriu e deve ser apresentado como
`Stream open · waiting for content`, nunca como resposta semântica. Headers,
primeiro byte e primeiro conteúdo preservam seus `elapsed_ms` em Diagnostics.
No protocolo Responses, a abertura/fechamento de um item `reasoning` publica
`ThinkingStarted`/`ThinkingEnded`. Heartbeats e comentários SSE não satisfazem o
deadline pré-semântico; após o primeiro evento semântico, continuam valendo os
limites idle e wall normais.

Enquanto `shell` está `Streaming`, a row expandida de forma transitória sempre
mostra resumo redigido e bounded do comando, o limite efetivo e um snapshot no
máximo a 1 Hz com última linha e bytes drenados de stdout/stderr. A drenagem
continua bounded e o callback não pode bloquear o processo. Ao concluir, a tool
volta ao contrato compacto de nome + duração, salvo expansão explícita.

Durante run, a ActivityRail pode mostrar o mesmo snapshot contextual vivo sem duplicá-lo em outra rail. Cada chamada ao provider — inclusive summary de compaction — publica antes do envio um `ContextSnapshot` com `request_id` derivado da sequence, window configurada e estimativa conservadora das mensagens + system prompt nativo + schema das tools. Chars de reasoning/assistant recebidos aumentam a estimativa monotonicamente dentro dessa request. `UsagePartial` soma contadores sem remover `~`; `Usage` terminal + `AssistantEnded` ainda permanecem aproximados, e somente o outcome final `RunCompleted` confirma o total exato. Usage null/incompleto mantém `~`; componentes numéricos isolados permanecem visíveis via `UsagePartial`, nunca são promovidos a exatos, e cache nunca armazena/reproduz usage billable. Completeness por componente acompanha o protocolo e também é persistida em `EventKind::UsagePartial` como knownness retrocompatível: no Anthropic, cache tokens sem `input_tokens` base não completam input; input e output completos geram marcador terminal aditivo zero. Usage é u64 end-to-end; overflow marca `UsageTotals.overflowed` em turns diretos e merges de compaction, mantém headless sem custo e TUI aproximada. Custo usa aritmética checked em u128 antes de uma divisão única e só é publicado com accounting terminal completo, outcome bem-sucedido e resultado representável em u64. Cache (máx. 4.096 eventos/entrada, 2 MiB/entrada, 128 entradas e 8 MiB total; replay cancelável) abandona captura incremental ao atingir o cap, compacta strings/vetores e contabiliza capacities realmente retidas, inclusive spare capacity do `Vec`; inclui configuração wire, query não secreta, headers semânticos e credential scope apenas hasheados, além de endpoint/model/messages/tools; tool-required não é cacheável. Codex preserva query no URL, envia output cap e normaliza `response.incomplete/max_output_tokens` como truncation. Provider sem usage final mantém `~`. Nova request é identificada por `run_id + request_id + window`, substitui o snapshot anterior e pode refletir queda após compaction; estimativa atrasada de run cancelada não cruza a próxima run. APIs diretas sem `AgentLoopConfig` também emitem snapshot, com window 0 (`ctx --`). Summary de compaction só substitui transcript após exatamente um stop textual normal canônico, nunca stop/evento de tool nem conteúdo pós-stop; usage já observado é publicado mesmo quando summary falha ou é cancelado. Um único Usage terminal tardio é permitido após stop e antes do fechamento; qualquer outro evento pós-stop ou usage após accounting terminal é inválido. Tool calls fragmentadas usam o mesmo normalizer no runtime e no bridge livre; cada call exige `id` ou `index` fornecido pelo provider, siblings malformados invalidam o payload inteiro e argumentos residuais precisam formar JSON válido. Sequence pública usa avanço checked e falha antes do request/evento quando não há espaço para snapshot e terminal obrigatório; ArtifactStore reserva a sequence antes de qualquer escrita em disco. Streams SSE de sucesso são limitadas a 64 MiB e 1 MiB por linha/payload; erros públicos do adapter são limitados a 512 caracteres. Credenciais em query entram no redactor cross-delta antes de runtime/TUI/persistência. Saída headless texto e JSONL expõe explicitamente `usage_complete` e `usage_overflowed`, além dos componentes conhecidos, para parcial/saturado nunca parecer exato.

O footer mantém shortcuts, contexto/usage, estado crítico, Goal curto e unseen
count. Mode/model/effort vivem no label do composer. Quando há Goal ativo,
mostra `active`, `paused` ou `blocked`, budget restante e
`verified`/`unverified`. Goal completo expande in-place no transcript, não vira
painel permanente.

### 15.5 TodoDock

`TodoDockState`:

```rust
pub struct TodoDockState {
    pub todo: Option<TodoView>,
    pub expanded: bool,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub revision: u64,
}
```

Render compacto:

```text
TODO 2/4  ◌ Rodar testes afetados
✓ Mapear fluxo · ✓ Corrigir deduplicação · ○ Revisar diff
```

O renderer corta cada row na largura disponível; não empurra o composer para
fora e não altera a largura do chat.

O Todo não possui cor de identidade própria. Contador é neutro; `✓` concluído
usa success, `◌` ativo usa warning, `○` pendente usa muted e `✕` falho usa error.

### 15.6 Inspector

`InspectorKind`:

```rust
pub enum InspectorKind { Diff, Activity, SessionTree, Diagnostics }
```

Cada inspector tem ViewModel próprio e comandos tipados. Nenhum lê estado
global por singleton.

### 15.7 OverlayStack

```rust
pub struct OverlayEntry {
    pub id: OverlayId,
    pub kind: OverlayKind,
    pub capture: CapturePolicy,
    pub anchor: OverlayAnchor,
    pub size: OverlaySize,
    pub restore_focus: FocusTarget,
}
```

Stack visual e focus order são iguais. Overlay não-capturante não recebe tecla,
mas pode receber hit test quando explicitamente permitido.

Máximo de 16 overlays simultâneos. Ao atingir o limite, rejeitar o novo overlay,
registrar diagnóstico e manter o stack existente.

### 15.8 Toasts

- máximo três visíveis;
- fila bounded em 100; ao exceder, descartar primeiro info expirável, nunca
  error não reconhecido;
- error não expira automaticamente enquanto não reconhecido;
- info/success expiram em 5_000 ms no `FrameClock` injetado
  (`Tick`/`StatusTick`/`SyncClock`);
- cada toast visível ocupa uma row truncada pela largura do scrollback;
- reduced motion desativa transições, não duração.
- `tool_limit` / `turn_limit`: mensagem humanizada via bloco `System` colapsado (não toast efêmero); `turn_limit` mostra o teto `(N/N)` (default 128); `tool_limit` com caps 0/0 (resume durável) usa mensagem dedicada, não 32/96; ActivityRail mostra `turn N/M · read n/m mut n/m` quando largura ≥ 72 (`read`/`mut` deste turn); aviso one-shot ao atingir 80% dos turns do run e 80% do batch deste turn.

## 16. State machines

### 16.1 Assistant e Thinking streaming

```text
Absent
→ Streaming
→ Complete
  ├→ Failed
  └→ Cancelled
```

O reasoning usa a sequência explícita:

```text
Absent → ThinkingStarted → ThinkingDelta* → ThinkingEnded → Complete
                                                   ├→ Failed
                                                   └→ Cancelled
```

Regras:

- delta antes de start cria placeholder diagnosticado e pede snapshot;
- sequence repetida é ignorada;
- gap de sequence pede snapshot;
- end sempre substitui conteúdo parcial pelo snapshot final;
- end flusha coalescer.
- o normalizer mantém uma única autoridade `reasoning_open`; texto, tool ou
  stop fecham reasoning antes de publicar o novo domínio;
- `ThinkingEnded` completa o bloco corrente e entra em `AwaitingProvider`; o
  evento seguinte decide entre `Thinking`, `Responding`, tool ou terminal;
- evento tardio depois de outcome terminal pode completar somente a cauda já
  terminalizada; nunca reabre lifecycle nem ActivityRail.

### 16.2 Tool lifecycle

```text
Pending → Running → Complete
                  ├→ Failed
                  └→ CancelRequested → Cancelled|Complete|Failed
```

Cancel é request, não fato consumado. A UI só mostra `Cancelled` ao receber
evento final.

### 16.3 Input/approval lifecycle

```text
Absent → Pending → ResponsePending → Accepted
                              └────→ Rejected
```

`AnswerInput`, `Approve` e `Reject` não promovem o bloco: apenas registram que a
resposta aguarda ack e bloqueiam reenvio duplicado. Somente ack com o mesmo ID
termina o lifecycle. Ack stale/duplicado é no-op; request duplicado idêntico é
idempotente; conflito de payload pede snapshot. Outcome terminal da run não
apaga request pendente persistido. Replay dos mesmos eventos em `AppState` novo
reconstrói o mesmo pending/resultado sem emitir comandos.
Enquanto `ResponsePending`, teclas e paste não alteram o composer. Approval
captura `Y/N` diretamente e preserva qualquer draft preexistente; não aceita
edição textual até o ack. Assim texto digitado durante a espera nunca vira o
próximo prompt por acidente.

### 16.4 Focus

Prioridade:

```text
capturing overlay > inspector/drawer > Todo dock focado > composer > scrollback
```

Setas/PageUp/PageDown/Home/End chegam primeiro ao overlay/autocomplete
capturante; somente sem captura ativa viram intents do scrollback.

Ao fechar overlay, restaurar `restore_focus` se ainda existir; caso contrário,
composer.

### 16.5 Surface lifecycle

```text
Stopped → Entering → Active → Leaving → Stopped
                    └→ Failed → Restoring → Stopped
```

Na v1 existe somente `FullscreenBackend`; não há troca de surface nem comando
`/surface`. Uma futura surface não pode ser adicionada sem nova decisão de
produto e novo contrato de lifecycle.

## 17. Input e keybindings

### 17.1 Normalização

Input bruto é normalizado em:

```rust
pub enum InputEvent {
    Key(NormalizedKey),
    Paste(Arc<str>),
    Mouse(NormalizedMouse),
    FocusGained,
    FocusLost,
    Resize(Size),
}
```

Kitty/CSI-u é capability, não requisito. O mesmo `KeyAction` pode ter aliases
por terminal.

### 17.2 Keymap default

| Ação | Default |
|---|---|
| enviar | Enter |
| newline | Shift+Enter; Ctrl+Enter fallback |
| steer durante run | Alt+Enter |
| fechar/cancelar nível atual | Esc |
| abortar run | Ctrl+C quando run ativo e nenhum modal captura |
| sair | Ctrl+C quando idle e composer vazio; confirmação se necessário |
| help | F1 ou `?` via command palette |
| command palette | Ctrl+P |
| trocar Mode | Shift+Tab; `/mode` |
| model | Ctrl+L |
| expandir/recolher Todo dock | Ctrl+T |
| diff inspector | Ctrl+D |
| activity inspector | Ctrl+J |
| session tree | Ctrl+R |
| diagnostics | Ctrl+G |
| search scrollback | Ctrl+F |
| filtrar busca (todos/erros/tools) | Tab com a busca aberta |
| copiar bloco selecionado/último | Ctrl+Y |
| mover cursor | Left / Right |
| início/fim da linha | Home / End |
| apagar à frente | Delete |
| live edge | End |
| expand/collapse block | Enter sobre bloco focado |
| responder input pendente | digitar no composer e Enter |
| aprovar/rejeitar request | Y / N, sem modificadores |

Com a viewport pinned (posição de leitura explícita), `Home`/`End` navegam
para `Top`/live edge mesmo com draft preenchido — o draft é preservado e só
a viewport se move; no live edge com draft, continuam editando o cursor. É
isso que torna a promessa `End latest` do footer verdadeira com draft.

Atalhos são configuráveis por ação sem permitir conflito silencioso. Config
duplicada gera diagnóstico e mantém o primeiro binding válido.

O composer tem prioridade: `Enter` alterna o bloco focado somente com composer
vazio, sem overlay/autocomplete capturante e sem modificadores. Com draft,
`Enter` mantém a semântica de submit; `Shift+Enter`/`Ctrl+Enter` mantêm newline
e `Alt+Enter` mantém steer. Expansão ocorre inline no scrollback, sem modal nem
scroll aninhado.

Input pendente captura o submit do composer antes de prompt/slash/steer e não
envia a resposta ao provider. Approval pendente captura `Y/N` e bloqueia edição
do composer; `Ctrl+C` continua abortando a run. Enquanto `response_pending`,
todo input de composer e paste é ignorado até ack aceito ou rejeitado.

### 17.3 Esc cascade

1. fechar autocomplete;
2. fechar modal capturante;
3. fechar drawer/inspector;
4. recolher Todo dock expandido;
5. cancelar seleção/search;
6. pedir cancelamento de tool focada;
7. pedir abort do run;
8. não sair silenciosamente.

## 18. Mouse e hit testing

Mouse é opcional e desligável.

Suportar:

- wheel/trackpad scroll;
- click em block/tool/Todo dock/inspector/tab;
- drag de scrollbar (deferido após o Slice 4; a primeira entrega cobre somente
  visualização e wheel/teclado);
- seleção interna somente se não conflitar com seleção nativa;
- Shift como escape para seleção do emulador quando aplicável.

`LayoutPlan` produz `HitRegion { rect, target, z_index }`. Hit test percorre por
z-index. Nunca reconstruir layout dentro do handler de mouse.

Scroll events são acumulados por frame e limitados a metade da viewport para
evitar saltos extremos. No Slice 4, wheel, setas, PageUp/PageDown, Home e End
usam o mesmo `ScrollMetrics` puro medido antes do reducer; drag permanece fora
do escopo.

## 19. Markdown, ANSI e Unicode

### 19.1 Sanitização

Texto de modelo/tool é não confiável. Antes do parse:

- remover terminal control sequences não permitidas;
- permitir somente SGR/links quando o source é explicitamente ANSI-safe;
- impedir OSC clipboard/title vindos de conteúdo;
- limitar tamanho por bloco/materialização;
- preservar texto original para copy em store seguro;
- error body HTTP é lido em streaming até 4 KiB, nunca integralmente;
- credenciais em headers e query entram na mesma redaction antes de erro público;
- índices de tool são convertidos com bounds checked; required ausente falha fechado;
- stop tool-required sem call normalizada retorna erro, nunca completion.

### 19.2 Unicode

- cursor move por grapheme, não byte/char;
- wrap usa display width;
- nunca dividir wide char no limite;
- combining mark acompanha base;
- tabs expandem com tab stop configurado;
- invalid UTF-8 recebido de processo vira replacement controlado no adapter,
  antes da TUI.

### 19.3 Highlight

- carregar syntax set sob demanda;
- cachear por language/content generation/theme;
- desconhecido usa plain code style;
- erro de highlighter volta plain, não quebra bloco;
- blocos colapsados não fazem highlight/wrap integral antes de expandir.

### 19.4 Code blocks e diffs

Code block comum usa `code_bg`, ligeiramente distinto do transcript. Uma rail
neutra e label dim separam o bloco da prosa:

```text
│ rust · src/auth/session.rs
│ let token = refresh.lock().await;
│ refresh_once(token).await?;
```

Não usar cantos decorativos nem box completo por padrão. `code_bg` e a rail
fornecem profundidade suficiente. A label contém somente linguagem e path curto.
Overflow horizontal usa viewport; nunca reflow de código.

Diff real é a única exceção com background. O renderer usa:

- background verde/vermelho quase preto em toda row adicionada/removida;
- background mais intenso somente no trecho intraline alterado;
- `+`/`-` e line number sempre presentes, para não depender só de cor;
- duas linhas de contexto antes e depois de cada hunk por padrão;
- gaps como `⋯ N linhas sem alteração`;
- `Enter` expande/recolhe hunk ou arquivo inline;
- o diff integral permanece disponível no Diff inspector.

Código inline sugerido pelo modelo que não corresponde a um diff conhecido não
recebe background de add/remove.

### 19.5 Prosa e headings

Prosa Markdown usa `text`; ênfase, listas e quote variam peso/estilo sem
colorir parágrafos inteiros. Code inline (`paths`, comandos) usa `tool_accent`
(`#7DCFFF`) — `code_rail` é só o trilho de blocos de código, ilegível como
texto sobre fundo near-black. Blocos fenceados usam `text`. Links usam
`link_accent`. Marcadores Markdown estruturais não aparecem no buffer: H1 usa `assistant_accent`, H2 usa
`heading_accent`, H3 usa `thinking_accent` e H4–H6 usam `heading_accent`, sem
introduzir token novo. Markdown parcial durante streaming permanece legível e
converge para a projeção final. Medição e renderização consomem a mesma projeção
Markdown para que marker removal, Unicode e wrap não divirjam.

## 20. Imagens

M3 adiciona suporte capability-gated:

```rust
pub enum ImageProtocol { Kitty, ITerm2, Sixel, Placeholder }
```

Regras:

- detectar capability uma vez por surface enter;
- resize antes de encode;
- limitar bytes/dimensões;
- cachear imagem processada por hash + tamanho em células;
- deletar imagem terminal quando frame que a contém deixa de existir;
- inline backend pode usar placeholder mesmo quando fullscreen suporta imagem;
- alt text sempre existe;
- falha de protocolo mostra placeholder e diagnóstico.

Entrada de imagem na TUI:

- path digitado, clipboard e drag-and-drop que o terminal normalize como path
  produzem um attachment chip no composer;
- o runtime copia/deduplica a imagem por hash na sessão; a TUI guarda apenas
  `ImageId` e preview;
- modelo sem visão produz erro explícito no transcript, sem descartar o anexo;
- o chip pode ser removido antes do envio e sua remoção é uma ação reversível do
  composer.

Windows M1 não depende de imagem inline.

## 21. Themes e glyphs

### 21.1 Tokens semânticos

```rust
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub surface_alt: Color,
    pub surface_elevated: Color,
    pub composer_bg: Color,
    pub user_prompt_bg: Color,
    pub text: Color,
    pub muted: Color,
    pub secondary_text: Color,
    pub accent: Color,
    pub heading_accent: Color,
    pub link_accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub border: Color,
    pub border_focus: Color,
    pub operational_divider: Color,
    pub progress_track: Color,
    pub progress_fill: Color,
    pub scrollbar_track: Color,
    pub scrollbar_thumb: Color,
    pub code_rail: Color,
    pub selection: Color,
    pub user_accent: Color,
    pub assistant_accent: Color,
    pub thinking_accent: Color,
    pub tool_accent: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    pub diff_add_bg: Color,
    pub diff_remove_bg: Color,
    pub diff_add_emphasis_bg: Color,
    pub diff_remove_emphasis_bg: Color,
    pub code_bg: Color,
}
```

Componentes não referenciam cores literais.

### 21.2 Capabilities

- truecolor;
- 256 colors;
- 16 colors;
- no-color;
- Unicode glyphs;
- ASCII fallback;
- reduced motion.

Theme resolve para capabilities na entrada da surface. A cache guarda o theme
resolvido, não `auto`. Em 16 colors/no-color, níveis de surface podem colapsar;
bordas, labels, padding e glyphs preservam hierarquia sem depender do RGB.

### 21.3 Theme default aprovado

Paleta truecolor normativa:

| Token | Valor | Uso |
|---|---|---|
| `background` | `#080A0D` | fundo mais profundo do terminal |
| `surface` | `#0D1014` | transcript e região principal |
| `surface_alt` | `#12161B` | rails, docks e regiões secundárias |
| `surface_elevated` | `#191E24` | overlays, drawers e prompt destacado |
| `composer_bg` | `#151A20` | composer focado/ativo |
| `user_prompt_bg` | `#1B2026` | faixa integral da mensagem do usuário |
| `text` | `#C6CDD5` | texto principal (cinza-suave; branco puro sofre halation e parece negrito) |
| `muted` | `#747B84` | metadata, pendente e hints |
| `secondary_text` | `#A9B0B8` | labels, counters e paste token |
| `accent` | `#78D99B` | identidade principal Slim |
| `heading_accent` | `#82AFFF` | headings e estrutura de resposta |
| `link_accent` | `#8CB4FF` | links e targets navegáveis |
| `border` | `#2B3139` | separadores e boxes funcionais |
| `border_focus` | `#4F7D5E` | composer/drawer focado |
| `operational_divider` | `#252B33` | rails operacionais |
| `progress_track` | `#262D35` | trilho de progresso |
| `progress_fill` | `#78D99B` | progresso conhecido e saudável |
| `scrollbar_track` | `#151A20` | trilho quase invisível da scrollbar |
| `scrollbar_thumb` | `#4B5563` | posição da viewport |
| `user_accent` | `#A9B0B8` | label/rail do usuário |
| `assistant_accent` | `#78D99B` | label Slim, cursor e foco |
| `thinking_accent` | `#9AA4AF` | reasoning e elapsed |
| `tool_accent` | `#7DCFFF` | tools e activity estrutural |
| `success` | `#78D99B` | conclusão/healthy |
| `warning` | `#E6B450` | running/ativo/atenção |
| `error` | `#F07178` | falha/cancelamento relevante |
| `code_rail` | `#46505C` | rail de code/output expandido |
| `selection` | `#263A30` | seleção/foco textual |
| `diff_add` | `#78D99B` | `+` e metadata de adição |
| `diff_remove` | `#F07178` | `-` e metadata de remoção |
| `diff_add_bg` | `#0B1A10` | row adicionada |
| `diff_remove_bg` | `#1C0D11` | row removida |
| `diff_add_emphasis_bg` | `#163D22` | trecho intraline adicionado |
| `diff_remove_emphasis_bg` | `#421820` | trecho intraline removido |
| `code_bg` | `#0B0E12` | code comum separado do transcript |

Componentes não podem inferir semântica a partir do RGB resolvido; usam tokens.

### 21.4 Bordas, densidade e glyphs

- sem borda externa da aplicação;
- boxes são funcionais: composer, overlays e drawers; não envolver cada mensagem;
- profundidade vem de surfaces near-black e borders, não de sombras simuladas;
- user prompt usa faixa elevada sem cantos nem rail; assistant usa surface
  principal sem label de papel;
- SessionRail, ActivityRail e OperationalBar têm uma row cada quando visíveis;
  SessionRail é conversacional e adaptativa, nunca aparece no welcome/emergência;
- composer tem uma a cinco rows de conteúdo conforme draft/altura e uma row no
  modo de emergência;
- rails verticais não aparecem em user/assistant; tool/code expandidos usam
  indentação de quatro células;
- uma row vazia depois do user band; zero rows vazias entre thinking, tools e
  assistant; exatamente uma row antes de cada novo user após o primeiro;
- thinking colapsado e tool rows colapsados têm uma row cada;
- números dinâmicos usam largura/tabulação estável para não deslocar layout;
- glyphs normativos: `✓` complete, `◒`/`◌` running, `○` pending, `✕` failed,
  `>` composer (sempre ASCII: `›` é Ambiguous no Windows, G264);
- fallback ASCII: `+`, `~`, `o`, `x`, `>` respectivamente;
- no-color mantém significado por glyph, label e texto de status;
- reduced motion congela spinner, remove caret e mantém status textual;
- motion nunca altera largura, alinhamento ou posição de texto.

O default é near-black estratificado com profundidade contida. Themes
alternativos podem mudar tokens, nunca remover hierarquia entre surfaces,
agregação de tools ou prioridades responsivas.

## 22. FullscreenBackend

### 22.1 Entrada

Ordem normativa:

1. capturar estado anterior;
2. ativar UTF-8/VT no Windows;
3. ativar raw mode;
4. entrar alternate screen;
5. ativar bracketed paste;
6. ativar focus events/mouse se configurados;
7. esconder cursor até o primeiro frame focável;
8. instalar guard de restore;
9. executar primeiro draw.

Se um passo falha, reverter os anteriores na ordem inversa.

### 22.2 Draw

- obter size atual;
- construir `LayoutPlan`;
- renderizar para buffer Ratatui;
- posicionar cursor do composer;
- `Terminal::flush()` envia diff;
- commit de revisions somente após write bem-sucedido.

### 22.3 Saída

Ordem inversa, idempotente:

1. mostrar cursor;
2. desativar mouse/focus/paste;
3. sair alternate screen;
4. desativar raw mode;
5. restaurar console flags;
6. flush final.

Drop pode tentar restore, mas shutdown explícito é o caminho principal.

## 23. InlineBackend (histórico; não planejado)

Este capítulo descreve uma possibilidade histórica do design. M4/inline não
pertence ao produto atual, não entra em M0–M3 e não pode ser usado como base para
criar um backend fullscreen.

Modelo:

- blocos completos e imutáveis podem ser committed ao scrollback nativo;
- região viva contém streaming, composer, operational bar e overlays
  compatíveis; não reintroduz header no topo;
- backend mantém anchor e número de linhas da região viva;
- renderiza offscreen para Buffer;
- converte região para ANSI;
- escreve somente diff da região viva;
- resize invalida região e faz redraw controlado;
- troca de backend nunca reaproveita cursor bookkeeping do outro backend.

Overlays que exigem screen coordinates podem degradar para selector inline.
Cada overlay declara fallback obrigatório.

## 24. Falhas e recuperação

### 24.1 Render fault por bloco

Se um renderer de bloco falha:

- capturar tipo do bloco e error class;
- registrar métrica sem conteúdo sensível;
- renderizar bloco fallback limitado;
- contar falhas consecutivas;
- desabilitar custom renderer após limite;
- manter restante do frame.

### 24.2 Falha global de frame

- não commit revisions;
- preservar último frame válido;
- invalidar caches envolvidos;
- tentar um full redraw sem feature opcional;
- se repetir, restaurar terminal e encerrar com stderr claro.

### 24.3 Falha de effect

- manter draft/estado autoritativo;
- publicar `EffectFailed`;
- mostrar error block/toast conforme severidade;
- oferecer retry somente quando operação for idempotente.

### 24.4 Gap de eventos

- detectar sequence gap;
- marcar entidade `desynced`;
- pedir `SessionSnapshot`;
- continuar mostrando último estado conhecido com indicador;
- snapshot substitui estado derivado;
- nunca inventar completion.

### 24.5 Panic/signal

Panic hook não escreve UI complexa. Ele:

1. aciona restore idempotente;
2. escreve resumo em stderr;
3. preserva crash artifact conforme policy do harness.

Signals usam o mesmo shutdown path; não duplicar sequência terminal.

## 25. Windows-only

M1 deve provar no Windows:

- Console UTF-8;
- VT input/output;
- raw mode restore;
- Shift+Tab e modificadores;
- bracketed paste;
- resize durante streaming;
- Ctrl+C/Esc;
- cursor/IME no composer;
- terminal close/crash;
- stdout síncrono sem falso backpressure;
- Windows Terminal e pelo menos um terminal alternativo documentado.

Não assumir que `stdout.write(false)` significa bytes pendentes em console
síncrono. O adapter verifica buffer real antes de entrar em estado de
backpressure.

## 26. Observabilidade

Sem capturar prompt/output por default.

Métricas:

- input-to-frame latency;
- render duration por estágio;
- frames requested/rendered/coalesced/dropped;
- cache hit/miss/eviction por cache;
- visible blocks/rows;
- event lane occupancy/high-water;
- core→projector occupancy/high-water e capacidade de payload retida na fila,
  separados do ledger append-only do core;
- snapshot resync count;
- render faults;
- terminal restore attempts/failures;
- bytes written por frame;
- full redraw reasons;
- RSS opcional no benchmark, fora do hot path.

Trace de frame usa correlation ID, surface kind e revisions, nunca conteúdo.

## 27. Budgets de desempenho

Budgets iniciais, medidos em release numa máquina Windows de referência:

| Métrica | Budget |
|---|---:|
| input→frame p95, cache warm | `≤16 ms` |
| janela máxima de streaming | `16 ms` |
| motion ativo | `12 fps` nominais (`83 ms`); degrada primeiro sob backpressure |
| welcome-only | `0` redraws periódicos; redesenha somente por evento/resize |
| primeiro delta | frame imediato |
| resize/full redraw p95, 3k blocos/5 MiB | `≤50 ms` |
| idle sem run/stream animado | `0` ticks; reduced motion: `0` ticks de animação, com StatusTick semântico `1 Hz` somente para elapsed visível da ActivityRail |
| control event loss | `0` |
| lifecycle event loss | `0` |
| core→projector queue | capacity/high-water `1.024`; payload retido na fila `≤ capacity × maior delta aceito no corpus` |
| TUI control/data queue | `256` / `1.024`, bounded em todos os caminhos |
| restore em fault tests | `100%` |

Se um budget não for atingido:

1. salvar benchmark com commit/máquina;
2. identificar estágio pelo trace;
3. corrigir gargalo medido;
4. não aumentar budget sem decisão registrada.

## 28. Testes

### 28.1 Unit

- reducer por Action;
- focus cascade;
- scroll transitions;
- block lifecycle;
- tool cancellation;
- overlay stack;
- breakpoints;
- cache keys/invalidation;
- Unicode width/graphemes;
- keymap conflicts;
- error fallback.

### 28.2 Property

Gerar sequências de Actions e garantir:

- focus sempre aponta para target existente;
- overlay stack e z-order não divergem;
- anchor pinned resolve dentro da altura medida do bloco;
- viewport pinned não ultrapassa `total_rows - viewport_rows`;
- End retorna ao live edge;
- terminal size pequeno não causa panic;
- sequence gaps nunca produzem completion;
- reducer não perde draft após effect failure;
- cache eviction não muda output.

### 28.3 Golden buffers

Cobrir page-fill curto após histórico longo, troca para cauda após overflow,
anchor pinned estável sob append, Home/End, separador físico entre turnos e
scrollbar somente com overflow.

Matriz:

- widths 40, 80, 99, 100, 139, 140, 200;
- heights 8, 12, 24, 40;
- light/dark/no-color;
- Unicode/ASCII;
- collapsed/expanded;
- pending/streaming/complete/failed/cancelled;
- overlay/drawer;
- search selection;
- long lines/CJK/emoji/combining.
- mensagens longas usando toda a largura útil e continuação alinhada ao texto;
- H1/H2/H3 sem marcadores visuais e com tokens semânticos distintos;
- listas, ênfase, quote, code inline, links e Markdown parcial de streaming;
- tools agregadas por nome, expansão completa e exception rows de falha;
- composer boxed com uma row editável, draft multiline e paste atômico em várias larguras;
- Todo compacto/expandido junto da operational bar inferior;
- diff com contexto dobrado, row tint, intraline emphasis e no-color fallback;
- cinco níveis de surface distinguíveis sem depender somente de cor;
- user prompt elevado, assistant sobre surface e code em `code_bg`;
- composer boxed normal e composer compacto em baixa altura;
- ActivityRail/OperationalBar em estados wide/compact/hidden; SessionRail em limiares `79/80` e `11/12`;
- snapshots de spinner, streaming caret e progress rail em clocks determinísticos;
- reduced motion sem diferenças de layout ou conteúdo;
- scrollbar em live edge, pinned, hover/focus e no-color;
- SessionRail somente com transcript em `≥80×12`, ausente no welcome/emergência,
  sem duplicar contexto no footer e com cwd Unicode/UNC truncada por células.

### 28.4 PTY E2E

M1 Windows tem um gate ConPTY offline, mantido `#[ignore]` enquanto hosts sem
console real puderem abrir ConPTY sem produzir bytes. O comando explícito é
`cargo test -p slim-cli --test tui_pty -- --ignored --nocapture`.

- o executável vem primeiro de `SLIM_E2E_EXE`; sem override, usa
  `CARGO_BIN_EXE_slim`. Falhas citam caminho canônico, tamanho e SHA-256;
- a matriz obrigatória é `120×30`, `80×24`, `60×16`, `40×10` e
  `32×10`, incluindo reduced motion e `NO_COLOR` onde observáveis;
- cada caso usa provider HTTP loopback no mesmo processo e chave-sentinela,
  sem rede externa, segredo real ou provider comercial. A fixture emite
  reasoning→duas tools homônimas com IDs/outputs distintos→reasoning→answer;
- o driver espera marcadores observáveis com deadlines finitos, digita e envia
  o prompt, navega a seleção e usa Enter para expandir detalhe. Sleeps não são
  oráculo de sucesso;
- enter/leave do alternate screen, ordem causal, identidade das calls, detalhe
  expandido e restauração limpa são invariantes. Watchdog mata e coleta o
  filho em toda falha;
- cada execução persiste VT cru sanitizado e stream textual normalizado/diff em
  `analysis_outputs/streaming-thinking-tools-tui/conpty/`; chave-sentinela,
  auth temporário e caminho do workspace nunca podem aparecer nos artefatos;
- paste longo atômico, abort, crash restore, resize dinâmico e cursor position
  continuam gates PTY complementares, não substituídos pela matriz offline.

M4 Unix (referência histórica, não planejado):

- signals/suspend-resume;
- inline scrollback;
- Kitty keyboard;
- tmux/SSH matrix quando disponível.

### 28.5 Fault injection

- renderer panic/error;
- terminal write partial/failure;
- channel full;
- effect timeout;
- snapshot gap;
- malformed ANSI;
- image decode failure;
- clipboard unavailable;
- restore step failure.

## 29. Benchmarks

### 29.1 Corpus long-session

- 3.000–3.200 blocos;
- aproximadamente 5 MiB de conteúdo;
- Markdown, code fences, diffs, tools, thinking e errors;
- mistura de collapsed/expanded;
- Unicode realista.

### 29.2 Cenários

- first paint cold;
- steady-state sem mudança;
- append de delta;
- scroll contínuo;
- jump para live edge;
- expand/collapse;
- open/close drawer;
- overlay composition;
- resize 80↔200;
- theme switch;
- search highlight;
- stdout backpressure;
- cache eviction;
- fullscreen steady-state; inline permanece fora dos gates v1.

### 29.3 Metodologia

- separar setup/parse de steady-state;
- aquecer caches quando o cenário mede hot path;
- medir cold path separadamente;
- registrar allocations e bytes escritos;
- fixar corpus e seed;
- publicar máquina, OS, terminal, profile e commit;
- comparar p50/p95, não só média.

## 30. Milestones normativos

Cada milestone recebe seu próprio plano de implementação, execução e gate. A
TUI v1 cobre M0–M3. M4 não é planejado; M5 só pode tratar hardening interno,
sem criar extensibility/plugin system.

### M0 — contratos e testkit

> **Checkpoint atual — quase completo.** Reducer é a única rota de mutação no
> binário (`Action → reduce → Effect`), com EffectRunner mínimo executando
> `Send`; `RevisionSet` tem os 6 campos normativos; IDs de bloco são
> monotônicos; Tool* chegam tipados; proptest cobre scroll/draft/layout. Faltam
> `SurfaceBackend` trait compartilhada, sequences por stream e as variantes
> Plan/Compaction/Custom do block model.

Entregas:

- tipos públicos;
- AppState/Action/Effect;
- reducer;
- ViewModel;
- Block model;
- SurfaceBackend trait;
- MemorySurface fake;
- clocks e IDs determinísticos;
- unit/property tests.

Não inclui terminal real.

Gate: uma sessão fake inteira materializa frames determinísticos sem I/O.

### M1 — fullscreen Windows

> **Checkpoint atual — fatia ampla; gate físico pendente.** Paleta §21.3 aplicada
> com superfícies pintadas (background/surface/surface_alt/composer_bg/
> user_prompt_bg), SessionRail adaptativa sem a antiga ContextRail fixa,
> ActivityRail de uma row, composer
> boxed de três rows com label `model (effort) · mode` e borda neutra, scrollback
> navegável via offset pin/live-edge/unseen, layout de emergência, mouse
> capability-gated, UTF-8/VT console flags com restore exato e cursor por largura
> de células da linha sanitizada. Golden matrix roda sobre TestBackend nas
> larguras/alturas normativas.
> Pendente: PTY/ConPTY E2E físico (teste escrito e `#[ignore]`) e validação
> manual Windows Terminal + alternativo da matriz §25.

Entregas:

- TerminalGuard;
- FullscreenBackend;
- scrollback, ActivityRail, composer boxed e operational bar;
- user/assistant/system/error blocks;
- input Windows;
- resize;
- theme near-black estratificado e motion básico;
- golden + PTY Windows.

Não inclui tools reais, mouse, imagens ou drawer.

Gate: abrir, conversar com provider fake, provar hierarquia visual de
scrollback/ActivityRail/composer/operational bar em truecolor e fallback,
resize e sair sem corromper terminal.

### M2 — integração com harness e performance

> **Checkpoint atual — integração central + pipeline essencial entreges.** Lanes
> bounded control 256 / data 1024 com fairness 32 drenando control primeiro;
> coalescer de deltas participando do runtime real; tool blocks tipados com
> agregação por nome/turno e exception rows para falha/cancelamento;
> `HeightIndex` prefix sums com locate O(log n), WrapCache bounded com identidade
> de instância/generation e inserts pós-scan, e render virtualizado; bench misto
> de ~5 MiB desenha o pipeline Ratatui steady-state via TestBackend, usa
> nearest-rank p95 e impõe o budget §27 como exit code. Todo dock já recebe
> eventos reais. Pendentes: ParseCache/LayoutCache dedicados e métricas §26
> exportadas.

Entregas:

- UiEvent/UiCommand reais;
- streaming/coalescing;
- tool blocks/progress/cancel;
- Todo dock fixo compacto/expandido;
- scroll pin/live edge;
- Wrap/height cache bounded; Parse/Layout caches permanecem futuros;
- virtualização/HeightIndex;
- métricas locais e long-session bench.

Gate: budgets M2 atingidos ou blocker documentado com trace reproduzível; os
goldens também provam `UiEvent`/`UiCommand` para queue, compaction, tool,
cancelamento e Todo dock.

### M3 — experiência completa fullscreen

> **Checkpoint atual — fatia ampla; gate final pendente.** Theme semântico,
> superfícies estratificadas, overlays, command palette, Markdown sanitizado,
> motion elapsed-time, ActivityRail por fase, caret streaming estável, welcome
> estática e orientada a estado/ação, reduced motion, usage/contexto vivos e
> fault isolation, picker responsivo, code/diff semântico, composer adaptativo,
> inspectors, busca, clipboard e anexos locais estão integrados.
> Permanecem para os próximos slices: fluxos Plan/Goal completos, métricas
> exportadas, resync por snapshot e o gate físico. M3 ainda não está concluído.

Entregas:

- inspectors adaptativos diff/activity/tree/diagnostics;
- overlays/modals;
- command palette;
- themes completos com surfaces estratificadas;
- motion system, progress rail, spinner e streaming caret;
- mouse;
- reduced motion/glyph fallback;
- clipboard/search;
- imagens capability-gated.

Gate: matriz golden completa e fault injection sem terminal leak, incluindo:

- `Auto → Read-only → Plan → Auto` por `Shift+Tab`;
- Plan awaiting approval e transição confirmada para Auto;
- Goal active/paused/blocked/verified/unverified;
- queued user blocks em FIFO de oito itens;
- atividade de tools, MCP e subagentes em bloco expansível;
- reasoning recolhido, clipboard/path image attachment e erro de modelo sem
  visão.

### M4 — não planejado

Inline, Unix PTY, tmux e parity entre surfaces não fazem parte do produto atual.

### M5 — hardening interno opcional

Entregas:

- CustomBlock schema somente se exigido pelo próprio core;
- renderer registry interno;
- widgets limitados;
- API stability policy;
- benchmark regression gate;
- documentação interna de manutenção; não criar marketplace, ABI ou plugin host.

Gate: custom renderer não pode quebrar loop, terminal lifecycle ou outros
blocos.

## 31. Regras para o agente implementador

Esta seção é normativa e deliberadamente explícita.

1. Leia o milestone inteiro antes de editar.
2. Não implemente item de milestone posterior.
3. Não adicione crate sem justificar boundary e custo.
4. Não coloque I/O no reducer ou render.
5. Não crie singleton de AppState, terminal ou theme.
6. Não marque cancelado antes do evento final do harness.
7. Não descarte lifecycle event.
8. Não use `sleep` em testes quando um evento determinístico puder sinalizar.
9. Escreva primeiro o teste do contrato observável.
10. Faça o menor slice vertical que produz frame verificável.
11. Rode unit/golden do módulo tocado antes de PTY/full gate.
12. Preserve Windows como gate permanente do produto atual.
13. Se a biblioteca não suportar um requisito, escreva adapter; não vaze a
    limitação para AppState.
14. Se a especificação parecer contraditória, pare e reporte os dois trechos;
    não escolha silenciosamente.
15. Não declare performance sem benchmark do cenário correspondente.
16. Não altere budgets para fazer teste passar.
17. Não copie código do Grok Build sem auditoria de licença/notices.

## 32. Checklist de revisão por PR

- [ ] mudança pertence ao milestone atual;
- [ ] boundary público é menor que a implementação;
- [ ] reducer continua síncrono/puro de I/O;
- [ ] render continua puro;
- [ ] channels/caches continuam bounded;
- [ ] erros são visíveis e não inventam estado;
- [ ] lifecycle terminal permanece idempotente;
- [ ] Windows foi exercitado quando aplicável;
- [ ] golden novo cobre state visual, surface hierarchy e motion clock tocados;
- [ ] PTY cobre interação/lifecycle novo;
- [ ] benchmark cobre hot path alterado;
- [ ] telemetry não captura conteúdo sensível;
- [ ] docs e keymap foram atualizados;
- [ ] nenhuma dependência entrou sem licença registrada.

## 33. Definição de pronto final

> **Checkpoint atual: parcialmente atendida.** Fluxo normativo, fundação visual
> estratificada, scroll virtualizado, tool blocks tipados, lanes bounded,
> markdown com code/diff, palette/picker, motion semântico, composer adaptativo,
> inspectors/search/clipboard/anexos, proptest/fault/golden e benchmark com gate
> estão entregues e verdes. Permanecem pendentes: gate físico Windows
> (PTY/console real), Todo/Plan/Goal end-to-end pelo harness, métricas exportadas
> e resync por snapshot.

O design da TUI v1 está implementado quando:

- fullscreen Windows restaura terminal em todos os exits testados;
- streaming, tools e scroll não violam budgets;
- hierarquia visual entre background/surfaces/composer/overlays permanece
  legível em truecolor, 256, 16 e no-color;
- ActivityRail e composer boxed degradam sem perder informação; SessionRail adaptativa implementa o contrato conversacional do Slice 5;
- Todo dock permanece acima do composer em todos os tamanhos testados;
- drawers/overlays dos demais inspectors funcionam em todos os breakpoints;
- Markdown/Unicode/ANSI não quebram width/cursor;
- event gaps recuperam por snapshot;
- caches são bounded e semanticamente transparentes;
- custom blocks falham isolados;
- long-session benchmarks têm gate versionado;
- nenhuma feature de domínio tornou a TUI sua autoridade;
- documentação pública explica shortcuts, surface fullscreen, hierarchy visual,
  motion/reduced motion, themes e capabilities opcionais.

## 34. Referências de inspiração

- [Grok Build: dependências de rendering](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/Cargo.toml#L845-L883)
- [Grok Build: pipeline e caches de render](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/benches/bench.md#L209-L223)
- [Grok Build: crate principal da TUI](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/src/lib.rs)
- [Ratatui](https://github.com/ratatui/ratatui)
- [Crossterm](https://github.com/crossterm-rs/crossterm)
- [termcn](https://github.com/shadcn-labs/termcn) — referência principal de componentes; port seletivo, sem dependência direta
- [OpenTUI](https://github.com/anomalyco/opentui) — referência de primitives/keymaps; runtime não adotado
- [Tokscale](https://github.com/junhoyeo/tokscale) — referência operacional baseada em Ratatui
- [Critique](https://github.com/remorses/critique) — referência para diff responsivo e fuzzy picker
- [tachyonfx](https://github.com/junkdog/tachyonfx) — candidato opcional para transições compostas
- [ratatui-image](https://github.com/benjajaja/ratatui-image) — candidato M3 para imagens capability-gated

Estas referências orientam mecanismos e fronteiras. A implementação do Slim
deve permanecer Rust/Ratatui, source-owned, menor que as referências e guiada
por seus próprios benchmarks. Nenhuma delas autoriza segundo runtime visual ou
novo proprietário do terminal.
