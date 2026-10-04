# RUST-CLI — mapa de paridade para uma versão Rust do Pit

> **Status de implementação:** mapa de contrato e arquitetura-alvo; não é
> inventário de wiring atual. O headless integrado cobre providers SSE, tools
> nativas, modos, auth, compaction, usage, artifacts e anti-loop. `slim --tui`
> agora compartilha provider/loop/tools, com composer, streaming, usage e
> cancelamento; cache normal, sessões resume/branch, Skills, MCP *(superado: MCP
> já está integrado, ver [referência](reference/CLI-AND-RUNTIME.md#mcp))*, child
> agents reais e Todo/Plan/Goal end-to-end permanecem pendentes. Consulte o [status atual](README.md).

> Navegação: [Índice](README.md) · [Checkpoint do grill](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) · [Pesquisa](INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md) · [Design TUI](DESIGN-SLIM-TUI.md) · [Viabilidade TUI](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)  
>
> Documento de engenharia para portar o Pit atual para Rust sem reduzir o
> produto a um simples loop `prompt → modelo → shell`. `Taxonomia.md` e
> `CONTEXT.md`, lidos no checkout original do Pit, foram fontes de entrada; este
> documento consolida os contratos necessários ao Slim sem depender de links
> externos ao projeto.
>
> **Contrato consolidado:** o grill foi fechado em 2026-08-20. O checkpoint
> vinculado acima é a fonte normativa final; este mapa mantém detalhes de
> origem/paridade e não pode reintroduzir capabilities rejeitadas.

## 1. Objetivo e regra de paridade

O objetivo é construir um CLI em Rust capaz de substituir o Pit atual sem
perder os contratos que fazem dele um harness: providers, sessão, contexto,
tools, verificações de integridade, extensões, LSP, orquestração, TUI e canais
programáticos.

Este documento mapeia o estado observado no checkout em **2026-08-19**. O
checkout continha trabalho em andamento; números de tamanho e token são uma
fotografia, não uma API estável.

Paridade significa preservar comportamento observável:

- os mesmos tipos de mensagem, eventos, stop reasons e resultados de tools;
- a mesma ordem de rewrite, reparo, validação, execução e correção;
- JSONL nativo do Slim, com leitura do schema atual e das duas versões Slim
  anteriores; não há compatibilidade de armazenamento com o Pit;
- os mesmos modos de trabalho, canais e transições de estado, exceto pela
  remoção deliberada de permissions/sandbox;
- cancelamento, retry de transporte, timeout e recuperação sem deadlocks; não há
  fallback silencioso entre providers;
- economia de contexto equivalente, comprovada pelos mesmos benches;
- extensibilidade suficiente para não empurrar features opcionais para o core;
- comportamento Windows-only, com fallback entre PowerShell 7 e Windows
  PowerShell.

Não é paridade:

- somente reproduzir `read`, `write`, `edit` e `bash`;
- traduzir arquivos TypeScript linha a linha;
- trocar JSONL, RPC ou nomes de eventos por formatos “mais Rust”;
- atribuir economia de tokens à linguagem Rust. Rust pode reduzir startup,
  memória e overhead local; tokens só diminuem se os mecanismos de prompt,
  schema, pruning e compaction forem preservados ou melhorados com medição.

### 1.1 Exceção normativa de paridade: execução irrestrita

O Slim **não** portará o permission system, sandbox, hard deny floor, approval
prompts ou destructive-command gate do Pit. Esta é uma quebra consciente de
paridade aprovada pelo proprietário do projeto em 2026-08-19.

Contrato:

- toda tool executa com as permissões do processo e da conta que iniciou o
  Slim;
- filesystem, processos, rede e credenciais não recebem isolamento interno;
- não existe tool `allow`, tool `deny`, runtime `ask`, approval de tool
  persistida ou permission mode;
- dialogs de produto como `ask_user_question` e confirmação de um plano podem
  continuar; eles não autorizam ou bloqueiam a execução de tools;
- `Auto` continua sem autorização dinâmica, approval ou sandbox;
- `Plan` e `Read-only` expõem somente catálogos de capabilities sem mutação;
  isso é uma fronteira fixa de modo, não um permission engine por comando;
- trust de projeto pode decidir se código/configuração de extensions será
  carregado; não limita uma tool depois de carregada;
- validação de schema, integridade UTF-8, stale-read detection, caps de output,
  cancelamento e verificação continuam porque evitam corrupção, não porque
  restringem autoridade;
- isolamento continua possível somente fora do core, por container, VM,
  usuário do SO ou wrapper fornecido pelo operador.

O risco de comando destrutivo, exfiltração de credenciais, acesso à rede local e
alteração fora do workspace é aceito explicitamente. A documentação e a CLI
devem informar isso sem sugerir uma segurança inexistente.

### 1.2 Precedência das fontes

Para este mapa, a precedência usada foi:

1. contratos e comportamento no código atual;
2. tipos, testes e benches;
3. `CONTEXT.md` e `Taxonomia.md`;
4. documentação de subsistema;
5. README histórico.

Isso importa porque o README herdado do package de coding-agent ainda contém
frases como “No MCP” e “No built-in to-dos”, contrariadas pelo registry e pelas
built-in extensions atuais. Também há documentação Fusion que usa “judge” para
uma etapa interna; no domínio do produto, o termo canônico continua sendo
**Synthesizer**. A versão Rust deve portar o comportamento atual e preservar a
linguagem canônica, não fossilizar trechos históricos.

## 2. Escala atual

O monorepo tem quatro packages públicos. A contagem abaixo considera apenas os
fontes TypeScript/TSX sob cada package, sem testes:

| Package atual | Arquivos | Linhas aproximadas | Responsabilidade |
|---|---:|---:|---|
| `@pit/ai` | 63 | 14.919 | protocolo unificado de modelos e providers |
| `@pit/agent-core` | 16 | 5.545 | loop, estado, tools e eventos do agente |
| `@pit/coding-agent` | 516 | 163.791 | sessão, CLI, tools nativas, guards, extensões e modos |
| `@pit/tui` | 34 | 17.389 | terminal, input, componentes e render diferencial |

O repositório possui aproximadamente **898 arquivos de teste**. O tamanho mostra
que a migração deve ser feita por contratos e fatias verticais, não por uma
reescrita integral de uma vez.

## 3. Arquitetura executiva

```text
CLI headless / JSONL / TUI
            │
            ▼
      AgentSession
  configuração, recursos, sessão,
  compaction, retry, Solo, Goal
            │
            ▼
          Agent
  estado + filas + eventos + abort
            │
            ▼
        Agent loop
 contexto → provider → stream → tools
            │                 │
            │                 ├─ rewrite/repair/validate
            │                 ├─ integridade/preconditions
            │                 ├─ execução/cancelamento
            │                 └─ hints/after hooks
            │
            ▼
        @pit/ai
 adapters HTTP/SSE/WebSocket,
  auth, cache e usage
```

O corte mais importante é:

- `Agent` é o runtime genérico e não conhece arquivos de sessão, TUI ou LSP;
- `AgentSession` é o host do produto e compõe políticas e serviços;
- tools executam capacidades;
- preconditions de integridade envolvem chamadas de tools sem autorização;
- extensions adicionam comportamento sem aumentar o núcleo;
- channels traduzem os mesmos eventos para diferentes consumidores.

## 4. Tradução sugerida para um workspace Rust

Não é obrigatório copiar a divisão npm, e os nomes abaixo são o workspace final
do Slim, não uma transposição literal do Pit.

| Crate sugerido | Equivalente atual | Conteúdo |
|---|---|---|
| `slim-core` | `@pit/ai`, `@pit/agent-core` e runtime | providers, perfis, sessão, contexto, tools, skills, MCP, subagentes e eventos |
| `slim-tui` | `@pit/tui` + interactive | terminal, input, layout, componentes e render diferencial; acesso por `AppHandle` |
| `slim-cli` | entrypoint/canais | argumentos, boot, headless, fullscreen, config, auth e distribuição |

`slim-core` é um monólito modular. `slim-tui` não executa provider, tool, MCP ou
child diretamente; chama o `AppHandle` tipado. `slim-cli` compõe os dois. Um
`slim-protocol` separado só nasce se surgir consumidor independente real.

## 5. Fluxo completo de execução

### 5.1 Boot

1. Parsear argumentos e resolver o Channel: `interactive`, `text` ou `json`.
2. Executar fast paths (`--help`, `--version`, export, listagem de modelos) sem
   inicializar o runtime inteiro.
3. Resolver diretório do agente, cwd, confiança das integrações de projeto e
   escopos de configuração.
4. Carregar settings na precedência `global < project < overrides da sessão`.
5. Carregar `auth.json`, catálogo manual/descoberto, perfis nomeados e modelo
   inicial.
6. Abrir/criar/forkar sessão e materializar o branch ativo.
7. Descobrir instruções, skills, prompts e MCP; scripts só ficam executáveis em
   `Auto` e integrações novas/alteradas passam pelo trust gate.
8. Construir as definições de tools e o registry dinâmico.
9. Instalar built-in extensions e conectar os hooks ao `Agent`.
10. Construir o system prompt em prefixo estável + sufixo dinâmico.
11. Aquecer em background apenas provider e índice lazy de busca; repo map, AST e
    LSP não entram no boot da v1.
12. Iniciar o Channel escolhido.

`--dry-run` percorre resolução de configuração, auth e recursos, mas não chama o
modelo, não executa hooks e não abre conexões MCP.

### 5.2 Ciclo de prompt

1. Serializar prompts concorrentes pela fila de ciclos da sessão.
2. Limpar estado efêmero do ciclo: interrupção, mutation revision, riscos,
   verification, self-review e retry budget.
3. Rotear tools de alta confiança pela intenção do prompt.
4. Avaliar o gatilho de compactação: contexto estimado acima de
   `janela - reserve_tokens`, pedido manual ou overflow.
5. Compactar (método do Pi, em primeiro plano) quando o gatilho disparar.
6. Compor contexto dinâmico e aplicar extensions `before_agent_start`.
7. Executar somente o loop Solo na v1.
8. Persistir mensagens e eventos relevantes.
9. Executar continuação de Goal; verification/self-review são opcionais e
   explícitos, nunca gates automáticos de toda mudança.
10. Fazer flush de sessão, diagnósticos e estado durável.

### 5.3 Uma rodada do Agent loop

1. Drenar mensagens de steering; mensagens passivas só entram se outra rodada
   já ocorrer.
2. Atualizar a superfície ativa de tools sem reconstruir o histórico.
3. Aplicar `transformContext` com timeout fail-closed.
4. Converter mensagens de aplicação em mensagens compatíveis com o provider.
5. Resolver credencial novamente para suportar OAuth expirável e rotação.
6. Abrir stream do provider sob abort, timeout de conexão, idle timeout e
   watchdog wall-clock.
7. Emitir `message_start`, coalescer deltas em janelas de 16 ms e preservar a
   ordem antes de `message_end`.
8. Se uma tool call estritamente idêntica falhar no mesmo turno, sinalizar e
   bloquear a repetição; não forçar a mesma chamada novamente.
9. Tool calls completos podem iniciar preparação especulativa quando a tool é
   explicitamente segura.
10. Executar o batch de tools, anexar resultados ao contexto e emitir
    `turn_end`.
11. `prepareNextTurn` pode aplicar steering enfileirado e o perfil escolhido,
    mas não troca provider/modelo silenciosamente.
12. Parar quando não houver tools, steering ou follow-up; detector de não
    progresso pausa Goal com evidência em vez de repetir indefinidamente.

### 5.4 Pipeline de uma tool call

A ordem é contrato de produto:

```text
lookup da tool
  → bloqueio de argumentos elididos em mutações
  → prepareArguments / aliases
  → registry de rewrite (auto | suggest | block)
  → reparo estrutural e coerção orientada pelo schema
  → validação JSON Schema
  → preconditions de integridade / grounding
  → revalidação se uma precondition reescreveu argumentos
  → execução sob abort por run e por tool
  → updates parciais
  → error hints ou repair note
  → afterToolCall
  → tool_execution_end
  → ToolResultMessage
```

**Contrato ativo do Slim:** cada resposta do provider define um batch. Calls
read-only elegíveis executam em segmentos concorrentes (limite 8); mutações
permanecem serializadas. Os resultados são associados aos IDs originais e
apresentados ao provider na ordem das calls. Eventos `ToolStarted` e término
podem intercalar-se entre leituras concorrentes.

## 6. Mapa das 12 áreas

### 6.1 Harness / runtime

Componentes centrais:

- `AgentState`: system prompt, modelo, thinking, tools, transcript e estado de
  streaming;
- `Agent`: exclusão de runs concorrentes, abort, cancelamento por tool, filas de
  steering/follow-up/passive e subscriptions;
- `AgentLoop`: provider rounds, tool batches, eventos e limites;
- `AgentSession`: ciclo de vida, persistência, compaction, retry, Solo, Goal,
  resource loading e ligação das extensions;
- `UserInputBus`: requests estruturadas de tools para a UI;
- `TurnSteeringEngine`: doom-loop, retry budget, todo cadence e recovery;
- `SessionRecoveryController`: níveis `lean → guided → strict` de acordo com
  sinais reais de thrash.

Contratos que Rust deve preservar:

- uma sessão não aceita dois prompts ativos; o host deve usar steer/follow-up;
- `agent_end` é o último evento, mas idle só ocorre depois dos listeners
  ordenados concluírem;
- falhas síncronas viram um assistant error turn, não uma promise perdida;
- abort nunca depende de uma extension ou tool cooperar para desbloquear o loop;
- listener observacional falha isolado; hook de controle pode alterar fluxo;
- todo await load-bearing possui timeout ou sinal de cancelamento.

### 6.2 Providers / models

O piso da v1 possui dois caminhos de provider:

- Anthropic Messages;
- OpenAI-compatible, com cada endpoint nomeado como provider próprio.

Outros adapters só entram mediante demanda real e fixtures oficiais.

O catálogo de providers é maior porque várias empresas falam uma dessas APIs.
Modelos carregam `provider`, `api`, `baseUrl`, modalidades de input, reasoning,
context window, max tokens, custos e compatibilidade específica.

Características:

- stream normalizado em eventos `start`, deltas de texto/thinking/tool call,
  `done` e `error`;
- stop reasons comuns: `stop`, `length`, `toolUse`, `error`, `aborted`;
- payload e response hooks;
- SSE e, onde suportado, WebSocket com reaproveitamento de conexão;
- proxy de ambiente;
- timeout de conexão, idle timeout de body e wall-clock de rodada;
- retry de transporte no mesmo provider; não há fallback silencioso;
- API keys em `auth.json`, OAuth somente quando o fluxo oficial para terceiros
  existir, com refresh no Credential Manager;
- catálogo manual de modelos com descoberta opcional pelo endpoint;
- perfis `default`, `fast`, `deep` e `compact` para provider/model/effort sem
  confundir perfil com Mode;
- tokens sempre registrados; custo somente quando conhecido/configurado.

No Rust, o adapter deve ser um trait assíncrono que devolve um stream tipado. A
implementação não pode expor tipos particulares de um SDK no core.

### 6.3 Context economy

Os mecanismos listados abaixo são o inventário de paridade e pesquisa. Na v1,
o contrato efetivo é: budget dinâmico de output, reserva projetada, compactação
única do Pi (limite, manual, overflow), prompt cache nativo e artefatos por handle. Mid-turn,
live/proactive, memória global e recall BM25 não são implementados.

O contexto não é reduzido por uma técnica única. O stack atual contém:

- estimativa de tokens e ocupação wire;
- reserva adaptativa de output;
- compactação única do Pi: gatilho `janela - reserve_tokens`, `/compact` e recovery de overflow;
- supersede de reads/searches obsoletos;
- elisão de argumentos grandes de mutações históricas;
- head+tail de outputs grandes;
- defer de output integral com `recall_tool_output`;
- BM25 do histórico compactado com `recall_history`;
- cap de thinking antigo, protegendo turns recentes;
- sumarização delta na segunda compactação em diante;
- resumo estruturado em Markdown (formato do Pi) com listas de arquivos lidos e modificados;
- dedupe de read idêntico e delta quando o arquivo mudou;
- seleção on-demand de tools e skills;
- recall limitado a artefatos da sessão ativa;
- schemas wire compactos sem descrições aninhadas;
- prefixo estável separado do sufixo dinâmico;
- prompt cache nativo por provider;
- token governor inclusivo para root e subagentes;
- benches de tamanho, fidelity, recall e gasto por mecanismo.

#### Camadas de compactação

| Camada | Disparo | LLM |
|---|---|---|
| limite (`janela - reserve_tokens`) | contexto estimado passa do limite | sim |
| overflow recovery | provider devolve context overflow | sim, sem repetir tool |
| mid-turn pressure | pressão entre tool rounds | não planejado |
| live/proactive | resultados obsoletos ou piso configurado | não planejado |
| manual | `/compact` | sim |

#### Fotografia medida do checkout

`bench-prompt-size` em 2026-08-19:

| Métrica | Valor aproximado |
|---|---:|
| prefixo lógico | 98.526 chars / 26.629 tokens |
| prefixo no wire | 58.013 chars / 15.679 tokens |
| system prompt final | 35.777 chars / 9.669 tokens |
| schemas completos de parâmetros | 39.842 chars / 10.768 tokens |
| schemas wire de parâmetros | 18.784 chars / 5.077 tokens |
| descrições completas de tools | 22.907 chars / 6.191 tokens |
| descrições wire de tools | 3.452 chars / 933 tokens |
| memory on-demand, cenário sintético | 95% menos chars |
| hindsight on-demand, cenário sintético | 91% menos chars |

Esses valores dependem do catálogo de 103 skills e do checkout atual. O gate
Rust deve comparar mecanismos e cenários, não congelar os números acima como
constantes universais.

#### Prompt cache

O system prompt possui um marcador explícito:

- antes do marcador: identidade, guidelines, tool list, instructions de
  projeto, regras estáveis de Mode/Goal e hints estáveis;
- depois do marcador: data, cwd, branch, frequent files, contexto grounded e
  estado volátil.

Anthropic usa breakpoints em tools, system estático, compaction summary e último
user block. Providers de cache automático recebem o sufixo volátil movido para
o fim do payload. OpenAI usa uma chave estável de prefixo distinta do session id.

### 6.4 Tools

Esta seção mantém o catálogo observado do Pit para comparação. O catálogo v1 do
Slim é deliberadamente menor: read, write, apply_patch, list, search, shell,
interação, Todo/Plan/Goal, skills/MCP e subagentes. LSP, DAP, browser, eval,
code mode e web não são tools v1.

O registry estático tem **64 tools**. A superfície inicial enviada ao modelo não
é a lista inteira.

Com tool discovery ligada, o conjunto inicial é:

```text
read, bash, edit, write, ask, todo, search_tool_bm25
```

Intenção e BM25 ativam ferramentas especializadas para a rodada seguinte. Isso
reduz custo fixo de schema e preserva descoberta.

#### Catálogo estático por família

| Família | Tools |
|---|---|
| arquivos | `read`, `edit`, `edit_v2`, `write`, `undo` |
| shell e busca | `bash`, `grep`, `find`, `ls` |
| navegação estrutural | `symbol`, `find_symbol`, `impact`, `repo_map`, `ast_grep`, `ast_edit` |
| descoberta | `search_skills`, `search_tool_bm25` |
| interação/cognição | `ask`, `resolve`, `todo`, `plan`, `pin`, `goal_complete` |
| web | `web_search`, `web_fetch` |
| execução | `eval`, `code`, `calc`, `recipe` |
| memória | `retain`, `recall`, `reflect`, `forget`, `recall_tool_output`, `recall_history` |
| mídia | `inspect_image`, `render_mermaid` |
| inteligência de código | `lsp`, `debug` |
| segurança | `security_surface_map`, `security_static_scan`, `security_http_replay_diff`, `security_validate_finding`, `security_evidence` |
| browser | 19 tools `chrome_devtools_*` mais `preview` |

As tools Chrome cobrem páginas, seleção, navegação, fechamento, JavaScript,
screenshot, console, network, click, fill, teclas, texto, espera, hover, select,
upload, snapshot de acessibilidade, body de network e mapeamento de elemento
para source.

#### Tools adicionadas dinamicamente

Built-in extensions registram ainda:

- `exit_plan` quando o fluxo de Plan precisa de aprovação;
- `memory_append`;
- `task`, `parallel` e `fanout`;
- `message` dentro de subagentes com messaging habilitado;
- `list_mcp_resources` e `read_mcp_resource` quando MCP anuncia resources (Pit;
  no Slim, resources são ações do gateway `mcp`);
- tools MCP prefixadas por servidor;
- tools de extensions e do SDK.

Portanto, “64” é o registry nativo, não o total máximo de uma sessão.

#### Comportamentos importantes por tool

- `read`: limites de bytes/linhas, imagens, hashline anchors, dedupe e delta;
- `edit`/`write`: fila por arquivo, mtime precondition, snapshots, writethrough
  LSP e mutation guard;
- `edit_v2`: edição por content hash/hashline para arquivos grandes;
- `undo`: restaura o último snapshot pré-mutação;
- `shell`: streaming, timeout, kill de árvore e foreground cancelável;
- `search`: query estruturada, backend lazy e fallback para executáveis
  conhecidos;
- `ast_grep`: backend nativo in-process e fallback CLI;
- `code`: programa JavaScript chama tools pelo mesmo dispatcher do harness;
- `eval`: kernels persistentes Python e JavaScript por sessão;
- `web_fetch`: parsing de URL, redirects revalidados, byte caps,
  HTML→Markdown e fallback controlado; sem denylist de rede privada/localhost;
- security tools: ciclo candidate→reproduced→validated/retracted com evidência
  redigida em disco.

### 6.5 Integridade e verificação — sem permission system

O Slim preserva prevenção contra estado inválido e perda de trabalho, mas não
impõe autorização. Não existe firewall de permissions.

#### Antes da geração

- prompt estável e tool surface mínima por economia de contexto;
- context composer grounded por arquivos lidos, search e instruções de projeto;
- task rigor e clarify nudge;
- profile de prompt por capacidade do modelo;
- session contract, frequent files e convenções observadas;
- cache-prefix diagnostics.

#### Antes de executar uma tool

Ordem de integridade:

1. bloqueio de argumentos ausentes/elididos;
2. aliases/rewrite e reparo estrutural;
3. validação do schema;
4. stale-read/edit precondition;
5. grounding de símbolo/import/path/pattern quando aplicável;
6. canonicalização necessária para consistência;
7. dispatch irrestrito com as credenciais e direitos do processo.

Não incluir:

- approval prompt;
- allow/deny rule;
- hard deny floor;
- destructive-command gate;
- workspace access scope;
- sandbox interno;
- bloqueio de localhost/rede privada baseado em policy.

Uma chamada ainda pode falhar por schema inválido, path inexistente, conflito de
mtime, cancelamento ou erro real do SO. Isso é integridade/execução, não
permission denial.

#### Depois da tool e do turn

- tool error hints e repair notes;
- snapshot de mtime e patch audit;
- detector de chamada idêntica e não-progresso de Goal;
- verification `in-turn`, `post-turn` ou `off`;
- self-review e verification quando explicitamente acionados;
- recovery level adaptativo.

Falha de uma precondition deve ser diagnosticada. Nenhum evento ou status pode
afirmar que o Slim “protegeu” uma operação que foi executada irrestritamente.

### 6.6 Orchestration

#### Solo (v1) e Fusion histórico

Mode seleciona `Plan`, `Read-only` ou `Auto`. Auto não possui permission engine;
Plan/Read-only usam catálogos fixos sem mutação. Fusion é referência histórica e
não é planejado.

Pipeline Fusion atual:

```text
prompt
  → briefing opcional
  → Panel em paralelo
  → análise estruturada do Synthesizer
  → verificação read-only de claims não sustentadas
  → writer no modelo ativo
```

O pipeline Fusion acima pertence ao mapa de origem do Pit e não deve ser
implementado no Slim.

#### Coordinator e subagentes

##### Piso obrigatório da v1

Subagentes são feature fundadora do Slim, não extensão tardia. A v1 implementa:

- execução no mesmo processo/runtime do agente raiz;
- profundidade máxima 1, sem delegação recursiva;
- concorrência default 4, configurável, com fila bounded;
- child session durável com relação parent/child explícita;
- contexto e token budget próprios;
- herança filtrada de tools, skills e MCP;
- `spawn`, `status`, `join`, `cancel` e `list`;
- propagação de cancelamento e teardown;
- resultado final estruturado com `status`, `session_id`, `usage` e mensagem
  final.

Todos os children compartilham providers, registry e conexões MCP do
`AppRuntime`; não iniciar um processo, runtime ou MCP server por subagente.

Ficam fora da v1: recursion, worktree pool, leader process, lateral messaging,
fanout especializado, resume de sete dias e Fusion multi-modelo.

##### Paridade histórica não planejada

O Pit possui capacidades adicionais que não pertencem ao roadmap do Slim:

- `parallel` para N tarefas declaradas;
- `fanout` para scout → reviewers → worker;
- result schema validado;
- acceptance criteria e comando de check;
- worktree isolado e políticas de mutação por path;
- tipos reutilizáveis de agente;
- herança filtrada de tools e skills;
- registry com status, turns, usage, custo e manifest;
- outputs grandes como digest + leitura paginada;
- resume em memória por sete dias e messaging lateral;
- token accounting no governor da sessão.

Defaults v1 do Slim: profundidade 1, quatro filhos ativos, fila FIFO de 32,
contexto selecionado, retorno estruturado e mutação serial no workspace.

### 6.7 Task cognition

#### Todo

Tracker universal e flat. Operações: `set`, `create`, `update`, `list`, `get`,
`delete`, `clear`. O estado possui revision monotônica, owner e items. Reminders
stale são descartados. Em regra há um item `in_progress`, exceto trabalho
realmente paralelo.

#### Plan

DAG versionado com:

- `id`, `intent`, `depends_on`, `produces`;
- `verify_command` e `verify_description` separados;
- `brief` com constraints e decisões;
- operações `propose`, `revise`, `step_status`, `step_done`, `show`;
- validação de ciclos e dependências faltantes;
- `step_done` como caminho explícito para `done`; receipts de verify são
  opcionais;
- artefato durável após aprovação do Plan.

#### Goal

State machine explícita:

```text
active → complete
active → paused
active → blocked
```

Goals têm objetivo e orçamento de tokens opcional. Budget esgotado pausa o Goal;
dependência externa ou detector de não-progresso pode bloqueá-lo com evidência.
`goal_complete` registra `verified` ou `unverified`; self-review e checks são
opcionais, mas o prompt do Goal orienta o agente a revisar e corrigir antes de
concluir.

#### TaskLink

Identidade opcional conecta Todo, versão de Plan e receipt de Goal. É
provenance, não propagação automática de status. O estado é branch-relative.

### 6.8 Memory & learning

A v1 não possui memória global, hindsight, recall automático ou learned errors
entre sessões. O que pode ser recuperado é limitado à sessão ativa: event log,
snapshots, compactações e artefatos por handle.

Memória explícita entre sessões não é compromisso do produto atual. Não criar
superfícies `MEMORY.md`, bancos de hindsight ou ranking BM25 como dependência
oculta do prompt.

### 6.9 Extensibility

Esta lista é o mapa de paridade histórica, não o contrato da v1. A v1 oferece
skills nativas e MCP; não oferece host TypeScript, packages, hooks externos,
themes carregáveis, ABI, WASM ou marketplace. Esses caminhos não são planejados
sem uma necessidade que não possa caber em skills/MCP.

Recursos suportados:

- extensions TypeScript;
- skills com frontmatter e arquivos auxiliares;
- prompt templates com argumentos;
- themes;
- hooks externos;
- packages instaláveis por npm/git/path;
- custom providers e OAuth;
- MCP tools/resources/prompts;
- commands, shortcuts, flags, renderers, widgets, overlays e footer/header;
- custom session state e custom messages.

Eventos de extension cobrem:

- descoberta de recursos;
- start/shutdown e before switch/fork/compact/tree;
- context transform;
- before provider / after response;
- before agent, agent, turn e message lifecycle;
- tool call/result e execution lifecycle;
- model/thinking selection;
- input e user bash.

#### Skills nativas da v1

Skills não dependem do compatibility host TypeScript. Roots, em ordem de
precedência para colisões:

1. `--skills-dir` explícito, repetível;
2. `<workspace>/.slim/skills/`;
3. `<workspace>/.agents/skills/` para compatibilidade;
4. `~/.slim/skills/`;
5. `~/.agents/skills/` para compatibilidade.

Cada diretório de skill contém `SKILL.md` e pode conter `scripts/`,
`references/`, `assets/` e templates. No startup, o Slim carrega somente
frontmatter, nome, descrição e path. O corpo e os recursos entram em contexto
somente quando a skill é explicitamente invocada ou selecionada pelo mecanismo
de discovery.

Skills, instruções e referências podem ser carregadas em todos os modos. Scripts
de skill só executam em `Auto`, com os direitos irrestritos do processo. A v1
não inclui marketplace, instalação remota ou package manager de skills; apenas
discovery, validação, leitura e invocation.

#### MCP

> **Superado em 2026-10-02.** O contrato MCP vigente, verificado contra o
> código, está em [CLI-AND-RUNTIME.md#mcp](reference/CLI-AND-RUNTIME.md#mcp). Divergências deste
> mapa de origem, que fica como registro: **prompts MCP não são suportados**;
> resources existem só como ações do gateway `mcp` (`resources`,
> `resource_templates`, `uri`), não como tools `list_mcp_resources` /
> `read_mcp_resource`; as conexões abrem em segundo plano (`lazy` é opt-in);
> OAuth com registro dinâmico e PKCE, importador de configs de outros clientes,
> busca BM25, exposição `direct`/`hidden` e o CLI `slim mcp` estão
> implementados; SSE legado, sampling, elicitation e approval seguem fora.

##### Piso obrigatório da v1

- transports `stdio` e Streamable HTTP;
- JSON-RPC 2.0, initialize/initialized;
- list/call de tools, list/read de resources e list/get de prompts
  *(superado: sem prompts; resources pelo gateway `mcp`)*;
- prefixo estável por servidor;
- configuração global e por projeto;
- inicialização lazy, timeout e cancelamento *(superado: conexão em segundo
  plano; `lazy` é opt-in)*;
- output/body caps;
- CodeMode nativo para compor tools MCP e reduzir resultados estruturados;
  descoberta, estado de sessão, budgets e cancelamento seguem a
  [referência operacional](reference/CLI-AND-RUNTIME.md#codemode);
- reconnect somente para a próxima call;
- teardown completo de subprocessos;
- uma conexão/subprocesso por server config no `AppRuntime`, compartilhada com
  subagentes;
- execução sem allow/deny ou approval.

Ficam pós-v1: SSE legado, OAuth com dynamic registration/PKCE, importadores de
config de terceiros e catalogação BM25 avançada *(superado: OAuth, importador e
BM25 estão implementados; SSE legado segue fora)*.

##### Paridade completa posterior

MCP suporta:

- transports `stdio`, Streamable HTTP e SSE legado;
- JSON-RPC, initialize, tools, resources e prompts *(superado: sem prompts)*;
- OAuth 2.0 com discovery, registro dinâmico, PKCE, refresh e tokens persistidos;
- cinco escopos de configuração, com precedência; trust decide somente se a
  configuração/código de projeto será carregada;
- prefixo por servidor; após carregamento, não há allow/deny globs de execução;
- defer automático de servidores grandes para BM25;
- reconnect só para a próxima call após falha de transporte, sem reenviar a
  call que pode ter tido efeito colateral;
- notifications de mudança de catálogo;
- teardown completo de subprocessos;
- cap de body e output estruturalmente reduzido.

#### Compatibilidade de extensions em Rust

Uma versão Rust pura não executa extensions TypeScript diretamente. Esta seção é
somente um mapa de migração rejeitado para o produto atual; nenhum item abaixo
faz parte da v1 ou do roadmap comprometido.

Alternativas históricas:

1. **Fase de compatibilidade recomendada:** host Node separado, conectado por
   JSON-RPC local, preservando a API TypeScript enquanto o core vira Rust.
2. **API nativa Rust:** traits para integrations compiladas junto ou plugins
   com ABI explicitamente versionada.
3. **Isolamento externo opcional:** container, VM ou wrapper do operador fora do
   core; o Slim oficial não fornece sandbox/capabilities de autorização.

O compatibility sidecar pode ser estudado apenas como ferramenta externa de
migração, mas não é necessário nem planejado para o Slim. Skills nativas, MCP e
subagentes não dependem dele.

### 6.10 TUI / experience

Especificação implementável: [DESIGN-SLIM-TUI.md](DESIGN-SLIM-TUI.md).  
Validação técnica contra Ratatui/Crossterm e Grok Build:
[VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md).

`@pit/tui` é uma biblioteca de componentes, não apenas prints coloridos.

Contratos:

- `render(width) -> linhas`, `handleInput`, `invalidate` e focus opcional;
- nenhuma linha pode exceder a largura visível;
- ANSI e OSC 8 são resetados por linha;
- cursor marker posiciona o cursor físico para IME;
- overlays possuem âncoras, tamanhos, margens, visibilidade e focus stack;
- componentes incluem editor, input, Markdown, imagem, select/settings list,
  boxes, cards, side-by-side e virtualized container;
- themes são JSON e components devem invalidar conteúdo pré-colorido;
- imagens usam protocolos de terminal suportados;
- input entende bracketed paste, Kitty keyboard, modifyOtherKeys, mouse SGR,
  kill ring, undo e keybindings configuráveis.

Renderização:

- árvore de componentes produz um frame de linhas;
- overlays são compostos antes do diff;
- o renderer procura primeira/última linha alterada;
- só escreve a faixa necessária e limpa sobras;
- resize ou mudança fora do viewport sobe para full redraw;
- synchronized output evita frame intermediário;
- cursor lógico e cursor físico são rastreados separadamente;
- backpressure de stdout pausa frames não forçados;
- um ticker compartilhado dirige animações e coalesce renders;
- callbacks de animação com falha são isolados e eventualmente removidos;
- Kitty images antigas são deletadas quando a faixa muda.

O Rust pode usar uma biblioteca terminal para input/ANSI, mas deve manter um
renderer diferencial próprio ou provar, por golden frames, que o backend
escolhido preserva scrollback, overlays, cursor/IME e imagens.

### 6.11 Channels / embed

Channels do contrato v1:

- `interactive`: TUI persistente;
- `text`: resposta single-shot;
- `json`: JSONL single-shot, sem `message_update` para evitar custo O(n²);
- `dry-run`: readiness sem chamar provider, executar tools ou abrir MCP.

RPC persistente e SDK Node permanecem fora do produto atual.

Eventos básicos incluem agent, turn, message, tool execution, rewrite/reject,
queues, compaction, retry de transporte, subagents, Goal e max-wall. Deltas de
stream são efêmeros; a sessão persiste a mensagem final e boundaries.

#### Superfície CLI e comandos interativos

As opções de processo se dividem em:

- modelo/provider/thinking/role;
- channel, print e max-wall;
- continue/resume/session/fork/no-session;
- seleção/desativação de tools apenas para composição da tool surface e economia
  de contexto, nunca como boundary de segurança;
- skills, prompts e context files;
- trust de projeto somente para carregar código/configuração executável;
- dry-run, offline, export, list-models e verbose;
- subcomandos MCP de list/get/add/remove/enable/disable (Pit; no Slim, `slim mcp`
  tem add/remove/enable/disable/list/login/logout/import/trust/untrust, sem `get`).

O dispatcher interativo nativo cobre:

```text
    /mode, /model, /compact, /todo, /plan, /goal, /skills, /mcp,
    /agents, /session, /help, /quit
```

`/permission-mode`, `/permission-cycle`, `/fusion`, `/hindsight`, `/jobs` e
`/memory` não existem no Slim. Skills entram por seus comandos essenciais e
mantêm diagnóstico de colisão; prompts MCP não existem no Slim.

RPC persistente, package manager e SDK Node não são superfícies do produto
atual. Não criar esses caminhos no `slim-cli`; a API oficial é o CLI headless
text/JSONL e a TUI fullscreen.

### 6.12 Platform & quality

Plataforma e operações v1:

- Windows Terminal, VS Code terminal e ConHost como fallback;
- PowerShell 7 preferido, Windows PowerShell como fallback;
- console VT no Windows;
- atomic writes e file locks;
- migrações de config/auth/sessão nativas do Slim;
- offline mode e version checks;
- HTML export e compartilhamento;
- diagnósticos bounded em memória e sink JSONL redigido;
- runtime latency trace sem conteúdo;
- cache/verification/precondition efficacy summaries.

Gates atuais:

- formatter/lint;
- typecheck;
- unit, integration e CLI E2E;
- smoke checks de browser, exports, generated files e extensions;
- token-economy regression gate;
- benches Pit × Pi e cenários de harness.

O porte precisa de gates equivalentes em Rust e de uma suíte cross-language que
alimente os dois binários com os mesmos fixtures.

## 7. LSP nativo — Rust, JavaScript e TypeScript

A integração usa `slim-lsp`, o trait `CodeIntelligence` em `slim-core`
e a ferramenta `code_intel`, com seis ações de leitura. O host, o transporte
stdio e o pool são nativos do Slim. **Não é preciso ativar nada, instalar plugin,
passar flag ou escrever TOML para usar a integração.** Os dois perfis padrão são
`rust-analyzer` e `typescript-language-server`; uma consulta semântica inicia
automaticamente o servidor correspondente, quando suas dependências existem.

Os servidores de linguagem continuam externos. Rust depende de rust-analyzer;
JS/TS depende de Node.js, TypeScript Language Server e uma versão compatível
do TypeScript clássico, com `lib/tsserver.js`. TypeScript nativo sem esse motor
fica fora do contrato. O Slim não instala dependências nem altera o projeto.
A descoberta JS/TS prefere um override explícito, depois o pacote instalado em
`node_modules/typescript-language-server` na raiz do workspace e, por fim, PATH.
No Windows, launchers npm são resolvidos pelos metadados do pacote e executados
via Node com o entrypoint declarado.

JS/TS cobre `.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`, `.mts` e
`.cts`, incluindo declarações como `.d.ts`. A raiz da instância JS/TS é o
workspace autorizado canônico; o TypeScript escolhe os projetos e configs
aninhados. Arquivos avulsos podem usar um projeto inferido. Rust conserva a
descoberta por Cargo.toml. Ambos preservam a fronteira de caminhos e URIs do
workspace, inclusive para resultados provenientes de imports.

A configuração `[lsp]` é opcional e serve a overrides ou desativação explícita.
Cada entrada de `[lsp.servers."<id>"]` aceita `enabled`, `path`, `args`,
`initialization_options` e `settings`. Initialization options e settings são
objetos separados; settings representa o mapa completo de seções, como
`rust-analyzer`, `typescript` e `javascript`. Os campos omitidos preservam os
defaults e a precedência das camadas existentes. Desabilitar somente
rust-analyzer mantém o perfil JS/TS; a desativação global ou de ambos remove
o manager.

Sem override, JS/TS usa `hostInfo="slim"` e
`tsserver.useSyntaxServer="never"` nas initialization options, para consultar
o tsserver semântico. Um override substitui esse objeto inteiro.

### 7.1 Tool `code_intel`

As ações continuam `symbol`, `definition`, `references`, `hover`,
`diagnostics` e `status`. Posições humanas usam linha e coluna a partir de 1:

```json
{"action":"definition","path":"src/main.ts","line":3,"column":22,"max_results":20}
```

Definition, references e hover inferem o servidor pela extensão do arquivo.
Symbol com path consulta o outline; sem path exige query. Diagnostics aceita
um arquivo ou consulta o conjunto de publicações conhecidas de um servidor.
O filtro opcional `server` existe somente em symbol e diagnostics:

```json
{"action":"symbol","query":"parse","server":"typescript-language-server","max_results":20}
```

Com path, um filtro incompatível é erro. Sem path, um único perfil aplicável
é escolhido automaticamente; um workspace misto exige o filtro explícito e
informa os IDs disponíveis. Cada consulta continua ligada a um servidor.
O filtro seleciona a consulta; não é ativação nem configuração do usuário.

Para símbolos de workspace JS/TS, o manager abre uma fonte do snapshot limitado,
quando disponível: a mais próxima da raiz, com desempate por path. Isso carrega
o projeto dessa fonte antes da consulta fria; `completeness=unknown` permanece,
pois não comprova o carregamento de todos os projetos aninhados.

Toda resposta traz `server`, `state`, `completeness`, `document_version`,
`stale` e `elapsed_ms`. `max_results` limita os resultados. Referências e
símbolos usam `offset` e `revision` para continuar páginas; uma revisão antiga
é rejeitada após alteração do workspace ou da geração do servidor. Contexto e
previews são limitados e respeitam a fronteira de paths.

Status informa os IDs e as causas de indisponibilidade sem iniciar processos.
**Configurado/disponível não significa em execução.** O catálogo conserva
`code_intel` com um perfil ativo mesmo sem marcador ou binário, para permitir
diagnóstico de dependências ausentes. A disponibilidade respeita os modos já
existentes do runtime.

#### Atualização do workspace e diagnósticos pós-edição (30/09/2026)

Antes das consultas semânticas, cada instância reconcilia um snapshot limitado
de stamps de seus fontes e inputs de projeto. Alterações feitas por editor ou
shell são observadas nessa etapa. Notificações `workspace/didChangeWatchedFiles`
precedem a consulta; documentos abertos alterados passam pela sincronização
existente. Mudanças estruturais encerram a geração antiga, e a próxima consulta
semântica adquire um servidor novo.

Para Rust, os inputs continuam manifestos, Cargo.lock, rust-project.json e
configuração .cargo. Para JS/TS, todo arquivo .json/.jsonc é conservadoramente
um input estrutural, além dos lockfiles npm, Yarn, pnpm e Bun. Isso cobre configs
com nomes arbitrários e extends sem implementar outro parser JSONC; também pode
renovar o servidor por mudanças em JSON de dados. Criação e remoção de fontes
invalidam publicações dependentes. Uma publicação fresca do arquivo já
sincronizado por write/patch é preservada.

A varredura tem limite de 32.768 entradas e ignora `.git`, `.slim` e
`node_modules`. Rust ignora target; JS/TS também ignora target quando identificado
como saída Cargo em workspace misto, mas conserva declarações em diretórios como
dist/lib. A leitura roda no executor blocking do Tokio; não há watcher nem
polling em idle. Falha de leitura ou excesso de entradas torna o frescor
desconhecido. Stamps detectam mudanças usuais; não provam alteração cujo conteúdo
mudou preservando os mesmos metadados.

Depois de write/patch, o loop pode anexar **cobertura e erros novos** ao próximo
request do provider, por `CodeIntelligence::diagnostics_after_edits`:

- valida somente instâncias **já quentes**, sem iniciar processos. Arquivos sem
  servidor ativo são não verificados; uma renovação estrutural nesta etapa
  também não inicia a substituição;
- só certifica publicação sem stale e com a versão exata do documento após a
  edição. Publicação ausente, antiga ou sem versão exata não é arquivo limpo;
- usa um prazo global de até **1,5 s para o lote inteiro**, incluindo refresh,
  sincronização e até 200 ms de estabilização da publicação. As instâncias
  Rust e JS/TS são atendidas concorrentemente, de modo que um servidor silencioso
  não impeça aproveitar publicações frescas do outro;
- valida até **12 arquivos únicos elegíveis**, em sua ordem original, somando
  os dois perfis. Duplicatas, arquivos indisponíveis, linguagens não atendidas
  e instâncias frias não consomem essa cota. O relatório preserva os motivos
  dos arquivos restantes;
- compara erros por código e mensagem com a linha de base capturada antes da
  primeira edição do lote. Sem linha de base, informa que os erros podem ser
  anteriores. Armazenamento truncado não constitui linha de base completa;
- limita a nota a oito erros, quatro por arquivo e 160 caracteres por mensagem.
  Lotes mistos identificam o servidor por arquivo quando necessário; nomes de
  arquivos não verificados são limitados por motivo, com contagem dos demais;
- achata conteúdo do workspace, remove controles/bidi, passa por
  `redact_sensitive` e apresenta a nota como dados não confiáveis. Publicações
  do editor não substituem build nem testes.

No gate real, TypeScript Language Server publicou diagnósticos sem `version`,
campo opcional do protocolo. A consulta explícita mostra esses resultados com
`completeness=unknown` e conserva `diagnostic_version=null`; a versão local
sincronizada não é atribuída à publicação. Pós-edição permanece `Unverified`,
sem certificar arquivo limpo ou erros introduzidos. A nota informa
`no verifiable diagnostics`, inclusive quando houve publicação sem versão.

Diagnostics sem path consulta somente publicações conhecidas; não verifica
recursivamente todos os arquivos do projeto. Rust conserva `checkOnSave=false`
por padrão. A nota pós-edição acompanha alvos conhecidos de write/patch e só entra
se houver próximo request e algo acionável (erros ou verificação tentada e não
concluída; ver [CLI e runtime](reference/CLI-AND-RUNTIME.md)); alterações por shell são reconciliadas na próxima
consulta semântica. Os gates reais Rust e JS/TS foram executados; versões,
comandos, resultados e limites ficam em [release/README.md](../release/README.md).

### 7.2 Arquitetura

```text
Modelo
  ↓
code_intel (6 ações de leitura)
  ↓
CodeIntelligence trait (slim-core)
  ↓
LspCodeIntelligence (seleção de perfil, raiz, frescor e respostas)
  ↓
LspProcessPool (raiz + ID + comando/args/init/settings efetivos)
  ↓
LspServerInstance (capabilities, encoding, documentos e diagnósticos próprios)
```

O pool compartilha processos somente quando a identidade efetiva coincide;
aquisição normal e aquisição de instâncias quentes usam a mesma chave. O lease
permanece durante toda a operação. O host reutiliza o transporte, DocumentStore
e DiagnosticsStore existentes. Workspace folders e configuration são resolvidos
a partir da raiz e dos settings da instância.

### 7.3 Componentes

- `crates/slim-lsp/src/position.rs` — codec UTF-8/16/32 com testes de unicode
- `crates/slim-lsp/src/transport.rs` — framing Content-Length, JSON-RPC 2.0,
  timeouts, cancelamento, notificações
- `crates/slim-lsp/src/document.rs` — DocumentStore (LRU, versões monotônicas)
- `crates/slim-lsp/src/diagnostics.rs` — DiagnosticsStore limitado por URI
- `crates/slim-lsp/src/discovery.rs` — perfis, descoberta de servidor e raiz
- `crates/slim-lsp/src/workspace.rs` — snapshots e inputs por perfil
- `crates/slim-lsp/src/instance.rs` — handshake, settings/folders, sync de
  documentos e notificações
- `crates/slim-lsp/src/pool.rs` — identidade efetiva, leases, idle shutdown
  (15 min), circuit breaker com backoff exponencial
- `crates/slim-lsp/src/manager.rs` — implementação de CodeIntelligence
- `crates/slim-lsp/src/process.rs` — StdioPair, kill de árvore (taskkill /T /F),
  stderr ring buffer

### 7.4 Segurança e fail-open

- Nunca instala servidores automaticamente
- Servidor indisponível → tool retorna `state: unavailable` com mensagem clara
- Nenhum `request` genérico exposto; todas as operações são tipadas
- `workspace/applyEdit` respondido com `{"applied": false}`
- `workspace/executeCommand` desligado
- Processo mata árvore inteira no teardown
- Stderr mantido em ring buffer limitado

### 7.5 Próximos slices

- Python como próximo perfil, após validar compatibilidade e utilidade JS/TS
- Implementations, type definition, call hierarchy e rename preview
- Aplicação transacional de rename e code actions seguras

### 7.6 Referência

- [LSP 3.18](https://microsoft.github.io/language-server-protocol/)
  (lsp-types 0.97/3.17 na implementação atual, com negociação de
  capabilities; 3.18-specific methods adicionados conforme necessidade)
- [rust-analyzer](https://rust-analyzer.github.io/)

### 7.1 Descoberta

O Pit tem definições built-in para **52 servidores**, cobrindo Rust, C/C++, Zig,
Go, JS/TS, web, Python, Java/Kotlin/Scala, Haskell, OCaml, Erlang/Elixir/Gleam,
Ruby, shell, Lua, PHP, .NET, YAML, Terraform, Docker, Helm, Nix, Odin, Dart,
Markdown, TeX, GraphQL, Prisma, Vim, Emmet, Swift e linters associados.

```text
rust-analyzer, tlaplus, clangd, zls, gopls,
typescript-language-server, biome, eslint, denols,
vscode-html-language-server, vscode-css-language-server,
vscode-json-language-server, tailwindcss, svelte, vue-language-server,
astro, pyright, basedpyright, pylsp, ruff, jdtls, kotlin-lsp, metals,
hls, ocamllsp, elixirls, erlangls, gleam, solargraph, ruby-lsp,
rubocop, bashls, lua-language-server, intelephense, phpactor,
omnisharp, yamlls, terraformls, dockerls, helm-ls, nixd, nil, ols,
dartls, marksman, texlab, graphql, prismals, vimls,
emmet-language-server, sourcekit-lsp, swiftlint
```

Um servidor é elegível quando:

1. não está disabled por override;
2. existe um root marker no cwd;
3. o binário resolve em bins locais, ambiente virtual, package bins ou PATH.

Mais de um servidor pode atender o mesmo arquivo. Servidores de linguagem têm
preferência para navegação; linters ainda contribuem diagnostics.

### 7.2 Transporte e lifecycle

- processo por server/config/cwd, JSON-RPC 2.0 sobre stdio;
- framing `Content-Length` byte-safe;
- initialize/initialized, capabilities e settings;
- clients e locks evitam boot duplicado;
- circuit breaker de falha de boot;
- warmup background;
- open-files LRU de 64;
- didOpen/didChange/didSave/didClose e version tracking;
- request timeout e cancel request;
- idle shutdown, shutdown/exit e kill de árvore;
- server requests controladas, incluindo workspace/applyEdit;
- manager atual é process-global no TypeScript; o Rust deve torná-lo
  session-owned para permitir múltiplas sessões no processo.

### 7.3 Operações da tool `lsp`

```text
diagnostics, definition, references, hover, symbols,
rename, rename_file, code_actions, type_definition,
implementation, status, reload, capabilities, request
```

`request` permite métodos específicos, como expansão de macro. Renames e code
actions podem ser preview ou apply.

### 7.4 Edições e segurança

- positions e ranges são validados;
- text edits são aplicados de baixo para cima;
- overlap inválido é rejeitado;
- workspace edits suportam create/rename/delete e mudanças textuais;
- URIs precisam ser `file:` e ficar dentro do workspace;
- realpath/ancestral checks impedem escape por symlink;
- snapshots permitem rollback se uma operação composta falhar;
- rename usa transaction com recheck;
- formatting combina `.editorconfig`, sniff de indent e fallback;
- `rename_file` envia willRenameFiles para atualizar imports.

### 7.5 Diagnostics on write

O writethrough possui otimizações e salvaguardas que devem ser portadas juntas:

- baseline antes e resultado depois da escrita;
- wake por evento em vez de polling;
- reuse de baseline quando `(mtime, size, servers)` não mudou;
- memo de servidor silencioso;
- pull diagnostics LSP 3.17 em paralelo com push;
- quiescence window para publish sem version;
- ledger por identidade para não reportar o mesmo erro movido de linha;
- diagnostics cross-file novos, com limites;
- caps de espera e fail-open quando diagnostics não são conclusivos.

## 8. Debug/DAP, browser e execução local — mapa histórico

Este capítulo descreve capacidades do Pit que não pertencem ao produto atual.

### 8.1 DAP

Há 14 adapters auto-detectáveis: GDB, LLDB, CodeLLDB, debugpy, Delve,
js-debug, .NET, Kotlin, Ruby, PHP, Bash, Dart, Flutter e Elixir.

A tool `debug` suporta launch/attach, breakpoints de linha/função/instrução/dado,
watchpoint bisect, continue/step/pause, evaluate, threads, stack, scopes,
variables, disassembly, memória, modules, sources, custom request, output,
terminate e sessions. Só leitura de estado é marcada read-only; controle de
execução é exec-tier. Há uma sessão de debug ativa por vez.

### 8.2 Chrome DevTools

O manager fala CDP, encontra ou inicia Chrome com profile dedicado, mantém a
página selecionada, buffers de console/network e habilita domains sob demanda.
Screenshots usam compressão e caps. `element_to_source` combina event listeners,
Debugger e source maps, com refinamento LSP quando disponível.

### 8.3 Eval e code mode

Eval mantém kernels Python e JavaScript por sessão. Cada call tem timeout e cap
de output; timeout reinicia o kernel.

Code mode executa um programa JavaScript que chama `tools.x()`. Cada chamada
atravessa o dispatcher normal do harness. O bridge limita resultado, respeita
tools ativas, abort e eventos; não chama `execute` diretamente.

Um porte Rust pode inicialmente manter o kernel JavaScript como subprocesso. A
segurança vem do dispatcher do harness, não do fato de o programa estar no
mesmo processo.

## 9. Persistência e formatos

### 9.1 Sessão JSONL

Header:

```json
{"type":"session","schema_version":1,"id":"uuid","timestamp":"...","cwd":"..."}
```

Entradas formam uma árvore append-only por `id`/`parentId`:

- `message`;
- `model_change`;
- `thinking_level_change`;
- `compaction`;
- `branch_summary`;
- `custom`;
- `custom_message`;
- `label`;
- `session_info`.

Estados de Goal, Todo, Plan, pins e subagentes usam eventos tipados/customizados;
o leitor deve suportar o schema atual e as duas versões Slim anteriores. O
contexto do modelo é materializado caminhando root→leaf, aplicando
o último compaction anchor válido e convertendo summaries/custom messages.

Recovery atual:

- framing por bytes aceita UTF-8 dividido entre chunks;
- detecta linha malformada, duplicate id, parent faltante, cycle, leaf ausente e
  compaction anchor inválido;
- cria backup sibling byte-verificado antes de migration/recovery;
- falha de backup torna aquela sessão write-disabled, preservando o original;
- novas entradas ainda podem existir em memória;
- backups não são removidos automaticamente.

### 9.2 Settings e recursos

Principais arquivos duráveis:

- `config.toml` global/projeto e overrides de CLI;
- `auth.json` para API keys e Credential Manager para refresh tokens OAuth;
- sessões JSONL, snapshots e índice JSON rebuildable;
- MCP global/projeto e metadata de servers;
- Plans aprovados, Goals e child sessions;
- diagnostics locais redigidos.

Writes críticos precisam de lock, arquivo temporário, fsync/close conforme a
plataforma e rename atômico. Config inválida não deve ser sobrescrita
silenciosamente por defaults.

## 10. Contratos Rust mínimos

Os trechos são esqueleto, não decisão de biblioteca.

```rust
pub trait Provider: Send + Sync {
    fn api(&self) -> &str;
    fn stream(
        &self,
        model: Model,
        context: ProviderContext,
        options: StreamOptions,
    ) -> ProviderEventStream;
}
```

```rust
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn execution_mode(&self) -> ExecutionMode;
    fn side_effect(&self) -> SideEffect;
    fn execute(&self, call: ToolCall, ctx: ToolContext) -> ToolResultStream;
}
```

```rust
pub trait ToolMiddleware: Send + Sync {
    fn before(&self, call: &mut ValidatedToolCall, ctx: &ToolContext)
        -> PreparationDecision;
    fn after(&self, result: &mut ToolResult, ctx: &ToolContext);
}
```

`PreparationDecision` pode continuar, reescrever ou rejeitar input inválido.
Não possui variantes de approve/deny/ask e não consulta permission state.

```rust
pub trait SessionStore: Send + Sync {
    fn append(&self, entry: SessionEntry) -> Result<EntryId, SessionError>;
    fn materialize(&self, leaf: Option<EntryId>) -> MaterializedSession;
    fn recovery_state(&self) -> RecoveryState;
}
```

```rust
pub enum AgentEvent {
    AgentStart,
    AgentEnd { messages: Vec<AgentMessage> },
    TurnStart,
    TurnEnd { message: AgentMessage, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: AgentMessage },
    MessageDelta { delta: AssistantDelta },
    MessageEnd { message: AgentMessage },
    ToolStart { call: ToolCall },
    ToolUpdate { id: ToolCallId, partial: ToolResult },
    ToolEnd { id: ToolCallId, result: ToolResult },
    // demais eventos preservam os nomes wire atuais
}
```

Regras de ownership:

- `AgentSession` possui managers de sessão, busca, MCP, skills, subagentes e
  diagnostics locais;
- registry guarda `Arc<dyn Tool>` e troca a lista ativa por snapshot imutável;
- cada run tem um cancellation token pai e cada tool um filho;
- tool results e events passam por channels bounded;
- nenhuma global mutável representa “sessão atual”;
- subprocessos pertencem a um supervisor que conhece process group/job object;
- writes concorrentes para o mesmo arquivo passam por uma fila keyed por path
  canônico.

## 11. O que Rust pode melhorar de verdade

Benefícios prováveis, que ainda exigem benchmark:

- binário único para o core e startup sem carregar centenas de módulos;
- menor memória residente do harness;
- tipos fechados para messages/events e menos casts em boundaries;
- cancellation e ownership de subprocessos mais explícitos;
- concorrência sem singletons de sessão;
- parsing byte-safe de SSE, JSONL, LSP e DAP com buffers bounded;
- menos cópias de strings em render, framing e compaction;
- backpressure e channels bounded como padrão;
- melhor distribuição multiplataforma.

Rust não resolve sozinho:

- fidelidade de compaction;
- prompt cache instável;
- schema grande;
- provider incompatível;
- extensão sem boundary estável;
- erro de validação/precondition;
- qualidade do modelo.

## 12. Riscos principais

| Risco | Impacto | Tratamento |
|---|---|---|
| reescrita big-bang | regressões impossíveis de localizar | fatias verticais e dual-run |
| perder extensions TS | reduz compatibilidade externa | contrato explícito: skills/MCP substituem extensions na v1 |
| trocar formato de sessão | impede resume/fork | JSONL Slim versionado, snapshots e migração de duas versões |
| provider Rust incompleto | diferenças em cache, thinking e tool calls | raw protocol fixtures e replay offline |
| usar tokenizer diferente | thresholds divergem | mesma função de estimativa por adapter + tolerâncias medidas |
| compactar “melhor” sem fidelity gate | perda silenciosa de contexto | mesmos benches de fact recall e fabricated paths |
| execução irrestrita | comando destrutivo, acesso amplo e exfiltração | risco aceito; aviso explícito e isolamento somente externo |
| duplicar tool pipeline | divergência entre normal, code e subagents | um único dispatcher sem permission branches |
| globals de manager | sessões concorrentes se contaminam | ownership por `AgentSession` |
| TUI genérica | quebra scrollback, IME e resize | golden ANSI/PTY e renderer diferencial |
| kill de processo incompleto no Windows | jobs órfãos | Job Objects ou supervisor equivalente |
| reordenação de batch | transcript não determinístico | correlação por ID e apresentação dos resultados em source order; mutações serializadas |
| lock/atomic write diferente | corrupção de sessões e artefatos | testes de crash e concorrência cross-process |

## 13. Estratégia de migração

### Escopo obrigatório da v1 fundacional

A v1 não é apenas `prompt → modelo → bash`. Ela só pode ser chamada de v1
quando todas estas capacidades funcionarem juntas:

- sessão durável com resume e branch básico;
- compaction automática/manual e budget de contexto;
- pasta nativa de skills do Slim, com carregamento on-demand;
- cliente MCP com tools, resources e prompts *(superado: sem prompts; resources pelo gateway `mcp`; ver [MCP](#mcp))*;
- subagentes depth-1 com concorrência bounded e sessão própria;
- tools básicas `read`, `edit`, `write`, `bash` e search;
- runtime irrestrito conforme §1.1;
- TUI mínima fullscreen com o design aprovado.

Essas capacidades pertencem à mesma **versão**, não à mesma entrega atômica.
Cada fase abaixo é uma vertical slice com gate próprio.

### Fase 0 — congelar contratos

- exportar fixtures de mensagens, events, schemas wire e sessions;
- criar golden tests de provider request/stream normalization;
- registrar CLI help, text/JSONL e sessão como contratos;
- transformar os benches de token/fidelity em gates executáveis pelos dois
  runtimes.

### Fase 1 — `slim-core` providers e eventos

- portar tipos, schemas e event stream;
- implementar Anthropic e OpenAI-compatible;
- validar tool calls, thinking, usage, cache e errors com fixtures;
- adicionar auth, timeout e retry de transporte somente após o stream básico;
  não implementar fallback silencioso.

### Fase 2 — `slim-core` headless e tools básicas

- portar `AgentState`, filas e Agent loop;
- implementar o pipeline único de tools;
- implementar `read`, `edit`, `write`, `apply_patch`, list, search e shell;
- executar o POC do backend de busca antes de congelar `fff-search` ou `ripgrep`;
- provar abort, turn budget, batches seriais e event ordering;
- rodar com tools falsas e providers falsos.

### Fase 3 — sessão, compaction, context economy e CLI headless

- JSONL nativo, snapshots rebuildable, índice JSON e recovery;
- branch, fork, compaction entries e recovery;
- settings global/projeto/override;
- channels text/json e dry-run;
- wire schema compaction e tool discovery;
- budget dinâmico de output e artefatos por handle;
- compactação manual (`/compact`) e automática pelo trigger Pi (tokens > janela
  - `reserve_tokens`, default 16384), com retry único em overflow;
- prompt cache nativo por provider;
- token governor de root e subagentes;
- nenhum shim Node ou memória global.

### Fase 4 — skills nativas e MCP mínimo

- descobrir metadata em `.slim/skills` e roots compatíveis;
- carregar `SKILL.md` e recursos somente quando invocados;
- manter scripts executáveis somente em `Auto`;
- implementar MCP `stdio` e Streamable HTTP;
- suportar initialize, tools, resources e prompts *(superado: sem prompts;
  resources pelo gateway `mcp`)*;
- compartilhar uma conexão por server config entre root e subagentes;
- provar timeout, cancelamento, reconnect seguro e teardown de subprocessos.

### Fase 5 — subagentes fundacionais

- profundidade máxima 1;
- slots e fila bounded;
- child session durável por agente;
- herança filtrada de tools, skills e catálogo MCP;
- cancel, join, status, usage e resultado final estruturado;
- mesmo runtime/processo do agente raiz;
- sem recursion, worktree pool, leader process, fanout ou messaging lateral.

### Fase 6 — TUI mínima e primeira versão útil

- fullscreen Windows com renderer diferencial;
- transcript, composer, Todo dock e operational bar aprovados;
- visualização de skills, MCP, tools e atividade de subagentes;
- PTY tests em Windows;
- v1 é publicada somente quando os gates das Fases 0–6 e a TUI M0–M3 estiverem
  verdes.

### Pós-v1 Fase 7 — filesystem avançado e inteligência de código

- snapshots, atomic mutation e background jobs avançados;
- repo map/impact e AST aquecido;
- LSP lazy/shared em vertical slice read-only, depois mutations;
- DAP somente sob demanda, após LSP provar valor;
- web fetch/search, Chrome/CDP, preview, eval e code mode como capabilities
  separáveis.

### Pós-v1 Fase 8 — integridade avançada

- preconditions de integridade na ordem aprovada;
- verification/pending checks/self-review avançados;
- recovery/doom-loop/TTSR/overthink.

### Não planejado — extensões e orchestration

- host compatível de extensions, plugins, packages e marketplace;
- Fusion, coordinator, messaging lateral, worktrees e subagentes recursivos;
- MCP OAuth somente se uma demanda futura exigir fluxo oficial *(superado: OAuth
  MCP está implementado; ver [CLI-AND-RUNTIME.md#mcp](reference/CLI-AND-RUNTIME.md#mcp))*.

### Não planejado — superfícies fora do Windows fullscreen

- inline TUI, PTY Unix, Linux/macOS e servidor remoto;
- SQLite/FTS como store de sessão e embedding ABI/WASM;
- PGO só poderia existir como otimização interna posterior, nunca como requisito
  de produto.

## 14. Gates de aceitação

### Protocolo e sessão

- [ ] lê schema atual e duas versões Slim anteriores;
- [ ] escreve JSONL nativo byte-valid e preserva branch/compaction;
- [ ] text/JSON passam golden fixtures;
- [ ] event ordering é idêntico nos cenários cobertos;
- [ ] abort sempre fecha o ciclo com estado coerente.

### Headless, modos e steering

- [ ] `Plan` headless persiste o Plan, emite `approval_required` e não executa
  tool mutante;
- [ ] aprovação de Plan registra a versão e muda a sessão para `Auto`;
- [ ] `input_required` persiste a pergunta, encerra com código estável e permite
  retomar fornecendo a resposta;
- [ ] a FIFO de oito mensagens aparece no estado/event stream e entrega em
  boundary seguro, sem perder ou duplicar mensagens;
- [ ] sessão nova inicia em `Auto` e `Shift+Tab` percorre os três modos.

### Providers

Estado local (2026-08-20): adapters OpenAI-compatible e Anthropic, SSE
fragmentado, texto/reasoning/tool-call/usage/stop normalizados, redaction de
headers, retries apenas para transporte, mensagens estruturadas assistant/tool,
execução nativa de tool call no turno e timeouts connect/idle/wall já estão
implementados em `slim-core`. O caminho headless usa esses adapters quando
configurado, sem persistir segredo por padrão; sessão JSONL é opt-in e outputs
grandes usam artifact handles. Permanecem pendentes imagens/cache avançado e
OAuth/auth.json oficial.

- [ ] Anthropic e OpenAI-compatible cobertos;
- [ ] texto, thinking, imagens e tool calls normalizados;
- [ ] cache read/write e custos calculados corretamente;
- [ ] API key em `auth.json` e OAuth somente em fluxo oficial exercitado;
- [ ] não existe troca silenciosa de provider/modelo;
- [ ] connect, idle e wall-clock timeouts independentes.

### Trust, credenciais e redaction

- [ ] trust inicial permite carregar integrações executáveis do workspace;
- [ ] skill/script/MCP novo ou alterado exige nova confirmação, sem prompt por
  tool/comando;
- [ ] API key fica em `auth.json` com ACL e refresh token OAuth fica no
  Credential Manager;
- [ ] valores conhecidos e headers de autenticação não aparecem em transcript,
  logs ou artefatos;
- [ ] retenção só ocorre por limpeza explícita do usuário.

### Tools e integridade

- [ ] registry nativo, skills e MCP coexistem sem host Node;
- [ ] discovery mantém tools ocultas fora do wire;
- [ ] ordem rewrite→repair→validate→preconditions→execute→hints é preservada;
- [ ] tools nativas, MCP e subagentes usam o mesmo dispatcher;
- [ ] batches mistos mantêm ordem determinística;
- [ ] Auto não possui approval, sandbox interno ou policy block dinâmico;
- [ ] Plan/Read-only expõem apenas capabilities sem mutação;
- [ ] tools executam com os direitos reais do processo no Windows.

### Search backend

- [ ] POC `fff-search` versus `ripgrep` cobre primeira busca, warmup, cem buscas
  repetidas, atualização de árvore, ignore, binários e Unicode;
- [ ] backend só é congelado depois do POC; até lá `ripgrep` permanece baseline;
- [ ] resultados stale são revalidados por path/hash/range antes de read/edit.

### Context economy

- [ ] wire prefix não regride além da tolerância definida;
- [ ] prompt prefix permanece estável entre turns equivalentes;
- [ ] compactação manual/threshold/overflow (retry único) segue o trigger Pi
  janela - `reserve_tokens`;
- [ ] artefatos por handle recuperam output integral;
- [ ] token governor soma root + subagentes sem duplicar.

<a id="v1-skills-mcp-subagentes"></a>

### Skills, MCP e subagentes — gates obrigatórios da v1

- [ ] roots de skills obedecem precedência e isolam colisões/skills inválidas;
- [ ] startup injeta somente metadata; corpo/recursos entram on-demand;
- [ ] `stdio` e Streamable HTTP suportam framing fragmentado;
- [ ] tools, resources e prompts MCP passam fixtures *(superado: sem prompts)*;
- [ ] MCP inicializa lazy *(superado: segundo plano)* e não repete uma call possivelmente side-effectful;
- [ ] root e children compartilham uma conexão por server config;
- [ ] depth 1 e limite de slots/fila nunca são ultrapassados;
- [ ] cada child possui sessão, usage, cancelamento e resultado final próprios;
- [ ] cancel do root propaga e não deixa child ou MCP subprocess órfão;
- [ ] subagentes usam o mesmo runtime/processo e dispatcher do root.

### LSP/DAP/browser — gates pós-v1

- [ ] stdio framing suporta chunks arbitrários e UTF-8 dividido;
- [ ] workspace edits são transacionais e não escapam por symlink;
- [ ] diagnostics push/pull e writethrough passam fixtures;
- [ ] subprocess teardown não deixa filhos;
- [ ] CDP e DAP respeitam cancelamento e timeout.

### TUI e plataforma

- [ ] golden ANSI de diff, shrink, resize e overlay;
- [ ] cursor/IME, bracketed paste e mouse quando disponível;
- [ ] Kitty keyboard é capability opcional, nunca requisito de release;
- [ ] backpressure não congela nem explode memória;
- [ ] Windows VT e process tree testados;
- [ ] nenhuma falha de render deixa raw mode/cursor quebrado.

## 15. Decisões recomendadas antes de iniciar código

1. Usar `slim-core`, `slim-tui` e `slim-cli`; não criar crates por analogia com
   cada package Pit.
2. Começar headless; TUI conecta depois ao mesmo `AppHandle` tipado.
3. Portar economia de contexto com benches, nunca por simplificação visual.
4. Não prometer ganho de tokens sem A/B usando o mesmo modelo e prompt.
5. Executar Auto sem permission system/sandbox; Plan/Read-only usam catálogos
   fixos de capabilities sem mutação.
6. Tratar skills nativas, MCP mínimo, Todo/Plan/Goal e subagentes depth-1 como
   requisitos da v1.
7. Compartilhar runtime, providers e conexões MCP entre root e children.
8. Adiar LSP/repos inteligentes e jobs background; não planejar Fusion,
   worktrees, browser nativo, server remoto, SQLite de sessão ou WASM.

## 16. Definição de pronto da versão Rust

### 16.1 V1 fundacional

A v1 está pronta quando:

- as Fases 0–6 e a TUI M0–M3 estão verdes;
- sessão, resume, branch básico e compaction funcionam no runtime Rust;
- skills são descobertas nos roots canônicos e carregadas on-demand;
- MCP `stdio`/Streamable HTTP entrega tools/resources/prompts e encerra limpo
  *(superado: sem prompts)*;
- subagentes depth-1 executam em slots bounded com child sessions duráveis;
- TUI fullscreen exibe atividade do root, tools, MCP e children;
- Todo/Plan/Goal, modos e fila de steering funcionam de ponta a ponta;
- profiles, auth.json, JSONL/headless e output budget passam os gates;
- nenhum desses caminhos depende do compatibility host TypeScript;
- execução continua irrestrita conforme §1.1.

LSP/repos inteligentes, DAP, browser nativo, Fusion, worktrees, servidor remoto,
SQLite de sessão, inline, Linux/macOS e WASM não fazem parte desta definição.

### 16.2 Substituição completa do Pit

A versão Rust pode ser chamada de substituto de uso diário do Pit quando:

- um usuário consegue instalar e iniciar o binário sem runtime Node para o core;
- sessões Slim abrem, continuam, compactam, ramificam e exportam;
- providers, tools, Goal e canais mantêm os contratos Slim;
- skills e MCP funcionam sem compatibility host TypeScript;
- LSP/repos inteligentes só entram se forem posteriormente aprovados;
- benches de tokens/fidelity não mostram regressão material;
- testes PTY confirmam a experiência interativa no Windows;
- o full gate Rust está verde.

Compatibilidade de API, extensions, sessões e configs do Pit não é requisito do
Slim. Até os gates acima passarem, o projeto deve ser descrito como **runtime Rust
em construção**, não como substituto de uso diário.

## 17. Lições absorvidas de fx e pi

Esta seção registra a revisão dos concorrentes feita em 2026-08-19:

- `vercel-labs/fx` no commit
  `8a61ffa670d80729a2ff1e04d5605d8effae6170`;
- `earendil-works/pi` no commit
  `ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423`.

A revisão cobriu manifests, arquitetura, core/loop, sessão, contexto, tools,
permissions, TUI, SDK/protocol, testes, benchmarks, CI e supply chain. Não houve
build local: Zig não estava instalado e o clone do pi não possuía
`node_modules`.

### 17.1 O que “leve” significa no fx

O fx é leve no artefato e no startup, não na quantidade de código. O checkout
examinado possui centenas de arquivos Zig e ampla infraestrutura própria.

Mecanismos úteis:

- budget de startup executado em CI por comando;
- relatório de delta do binário por PR;
- prompt base dividido em seções e teste de tamanho `<8 KiB`;
- history budget derivado de context window menos output capacity;
- tool result sanitizado, com segredos mascarados, cap UTF-8 e marcador de
  truncamento;
- output integral preservado por handle quando o preview é reduzido;
- compaction bounded preservando recentes; o summary mantém contexto, mas não
  volta a valer como mensagem literal de autoridade do usuário;
- file index em background com cap, gerações imutáveis e top-N bounded;
- terminal tape record/replay sobre engine virtual;
- core embutível por ACP, native addon e WASM com effects fornecidos pelo host;
- budgets de performance tratados como produto, não como benchmark manual.

Fontes:

- [README e superfícies do fx](https://github.com/vercel-labs/fx/blob/8a61ffa670d80729a2ff1e04d5605d8effae6170/README.md)
- [prompt base](https://github.com/vercel-labs/fx/blob/8a61ffa670d80729a2ff1e04d5605d8effae6170/src/builtins/context.zig)
- [budget de contexto](https://github.com/vercel-labs/fx/blob/8a61ffa670d80729a2ff1e04d5605d8effae6170/src/core/agent/runtime/prompt_context.zig)
- [tool-result limits](https://github.com/vercel-labs/fx/blob/8a61ffa670d80729a2ff1e04d5605d8effae6170/src/core/tooling/tool_result_limits.zig)
- [budget de startup](https://github.com/vercel-labs/fx/blob/8a61ffa670d80729a2ff1e04d5605d8effae6170/benchmarks/check_budgets.py)
- [SDK embutível](https://github.com/vercel-labs/fx/blob/8a61ffa670d80729a2ff1e04d5605d8effae6170/sdk/README.md)

### 17.2 O que copiar do futuro do pi

O coding agent, TUI, compaction e SessionManager atuais são maduros. O novo
`AgentHarness`, entretanto, ainda é scaffold: `prompt`, `compact`, `resume`,
`abort`, queues, lanes e watch retornam `HarnessNotImplemented` no commit
examinado. Copiar a especificação como se fosse uma implementação comprovada é
proibido.

Mecanismos úteis:

- entradas imutáveis, registers mutáveis e usage ledger separados;
- estado total de operação usado como program counter durável;
- reducer puro que reconstrói estado por reads bounded;
- uma conformance suite para Memory, JSONL e SQLite;
- snapshots remotos autoritativos; progress nunca vira verdade durável;
- leases shared/exclusive para ownership de sessão remota;
- tool batch com preflight, mutações serializadas e segmentos read-only
  concorrentes limitados a 8;
- tool calls de mensagem truncada por output limit nunca executam;
- paste marker atômico no editor;
- main screen e alternate screen com componentes compartilhados;
- telemetry context explícito, tipado e sem global current span;
- dependências diretas pinadas, installs com `--ignore-scripts`, shrinkwrap do
  CLI e release smoke isolado;
- SQLite/FTS aparecem apenas como referência histórica, não como backend do
  produto atual.

Fontes:

- [AgentHarness scaffold](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/packages/agent/src/harness/agent-harness.ts)
- [teste das operações ainda não implementadas](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/packages/agent/test/harness/agent-harness-scaffold.test.ts)
- [especificação durável do Harness](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/packages/agent/docs/harness.md)
- [protocolo CBOR](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/packages/protocol/README.md)
- [ownership por leases](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/packages/client/README.md)
- [layout de TUI](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/tui-plan.md)
- [telemetria tipada](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/packages/telemetry/README.md)
- [supply-chain hardening](https://github.com/earendil-works/pi/blob/ee29aa118bdeb7d8c4fdafa81130e0c61f8e0423/README.md#supply-chain-hardening)

### 17.3 Backlog incorporado ao Slim

| Prioridade | Mecanismo | Decisão para o Slim |
|---|---|---|
| P0 | orçamento do prompt base | criar gate inicial de 8 KiB; alterar só com medição de qualidade/cache |
| P0 | tool-result pipeline | sanitize → mask secrets → cap UTF-8 → marker → handle do integral |
| P0 | batch determinístico | preflight, mutações serializadas e resultados correlacionados por ID em source order |
| P0 | tool truncada | nenhuma tool call de resposta `length`/truncada pode executar |
| P0 | durable operation state | reducer puro + estado total mínimo para resume/recovery |
| P0 | backend conformance | Memory e JSONL passam a mesma suíte desde M0 |
| P0 | performance governance | startup, frame latency e delta de binário entram no CI |
| P0 | terminal tape | gravar bytes/input/resize e reproduzir no testkit |
| P0 | unrestricted runtime | nenhuma permission, tool approval ou sandbox no core; risco aceito |
| P0 | skills nativas | `.slim/skills` + roots compatíveis, metadata-first e body on-demand |
| P0 | MCP mínimo | `stdio`/HTTP, tools/resources/prompts, lazy e compartilhado com children (superado: ver nota em [MCP](#mcp)) |
| P0 | subagentes v1 | depth-1, slots bounded, child session, cancel/join/result |
| P1 | context budget | derivar de capabilities reais, reservando output e cache invariants |
| P1 | file index | background, cap, generation swap e top-N bounded |
| P1 | telemetry | contexts explícitos + schemas tipados, somente local |
| P1 | remote state | não planejado; snapshots locais são suficientes |
| P1 | host adapters | não planejado; `slim-cli` é a superfície oficial |
| P2 | session leases | não planejado sem servidor multi-cliente |
| P2 | SQLite/FTS | não planejado; JSONL/índice JSON permanecem canônicos |
| P2 | WASM | não planejado |
| P2 | PGO | somente após workload representativo e budgets estáveis |

### 17.4 O que não copiar

- zero-dependency como objetivo independente de manutenção;
- dezenas de tools no M0;
- módulos gigantes ou infraestrutura reimplementada sem benchmark;
- os números de 2 ms/7,8 MiB do fx como budgets do Rust;
- extensão TypeScript hot-reload dentro do core Rust;
- a ampla extension API do pi antes da ABI mínima estabilizar;
- duas arquiteturas de runtime concorrentes;
- protocolo, server, SQLite, FTS, multi-lane e WASM na v1;
- qualquer claim de maturidade do novo AgentHarness do pi antes de suas
  operações centrais deixarem de ser scaffold.

### 17.5 Segurança deliberadamente ausente

A ausência de sandbox/permissions do pi foi escolhida para o Slim. Diferente de
outras recomendações desta seção, esta é decisão normativa e já está definida em
§1.1.

Implicações obrigatórias:

- remover approval de execução de tool e seus modelos de dados do core/TUI;
- não persistir permission grants;
- não anunciar “safe mode”, “protected workspace” ou equivalentes;
- mostrar no onboarding/`--help` que o processo possui acesso integral da conta;
- testes devem provar execução direta, não bloqueio;
- documentação de container/VM é opcional e externa ao Slim;
- qualquer host que imponha policy é outro produto/adaptador, não comportamento
  default do Slim.
