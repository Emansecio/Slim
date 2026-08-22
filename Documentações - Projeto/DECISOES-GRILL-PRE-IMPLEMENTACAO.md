# Slim — decisões do grill pré-implementação

> **Status de implementação:** este documento é contrato/design normativo, não
> inventário de integração. Headless e TUI já compartilham provider/loop/tools,
> com composer, streaming, usage e cancelamento na TUI; cache normal, Skills,
> MCP, subagentes, sessões resume/branch e Todo/Plan/Goal ainda não estão
> integralmente ligados ao runtime. Veja o [status atual](README.md).
>
> Estado: **grill fechado — contrato normativo consolidado**  
> Data: 2026-08-20  
> Última decisão: **Q152**  
> Implementação: **parcial; os requisitos abaixo continuam sendo o destino da
> v1**

## 1. Finalidade

Este documento permite que uma pessoa ou agente retome o desenho do Slim sem
depender da conversa original. Ele registra apenas decisões já aprovadas e as
questões ainda abertas.

Este documento é a decisão normativa final do produto. Trechos históricos de
`RUST-CLI.md`, do design da TUI e das pesquisas só permanecem como mapa de
origem; quando divergirem, este contrato vence até serem removidos na
consolidação editorial.

O contrato está fechado. POCs, gates e testes validam componentes; não permitem
inferir que a implementação atual tenha conectado todo o contrato.

## 2. Direção do produto

- Slim é um **sucessor enxuto do Pit**, não uma tradução linha a linha.
- A compatibilidade prometida é **comportamental**. APIs, extensões, configs e
  arquivos de sessão do Pit não são contratos de compatibilidade.
- A v1 precisa ser adequada para **uso diário real**, não apenas demonstrar a
  arquitetura.
- A v1 e o produto atual são **Windows-only**. Linux, macOS, Unix PTY e inline
  não são compromissos do roadmap.
- O núcleo será um **monólito modular** com poucos crates. Um subsistema só deve
  virar crate separado quando houver reutilização ou isolamento comprovado.
- TUI e headless usam o mesmo runtime, os mesmos eventos e o mesmo estado.
- O runtime continua deliberadamente irrestrito em `Auto`: não haverá prompts
  por comando, allow/deny, sandbox interno ou destructive-command gate.

## 3. Escopo obrigatório da v1

### 3.1 Superfícies

- TUI fullscreen interativa;
- CLI headless não interativa;
- um runtime compartilhado pelas duas superfícies;
- modo inline e servidor remoto ficam fora da v1.

### 3.2 Modelos e providers

- provider OpenAI-compatible, cobrindo endpoints configuráveis;
- provider Anthropic nativo;
- API keys são o caminho garantido; OAuth só existe para providers com fluxo
  oficial/documentado para terceiros;
- texto e imagens locais como entradas;
- streaming, tool calls, usage e erro normalizados pelo runtime único;
- retry de falhas transitórias no mesmo modelo;
- nenhuma troca silenciosa de modelo ou provider;
- um modelo por turno; Fusion não é planejado.

### 3.3 Modos

A v1 terá três modos:

- `Plan`: pesquisa e produz um Plan sem mutar o sistema;
- `Read-only`: investiga e responde sem o ritual de Plan;
- `Auto`: pode executar tools irrestritamente, sem confirmações.

`Plan` e `Read-only` devem impedir mutação por catálogo de capacidades, não por
um permission engine dinâmico. Built-ins de escrita, remoção e shell geral não
ficam expostos nesses modos. Não haverá classificação allow/deny de cada
comando.

Essa decisão substitui o trecho anterior que tratava Mode apenas como
apresentação e dizia que Plan não bloqueava tools.

### 3.4 Todo, Plan e Goal

- Todo é o tracker universal para trabalho não trivial.
- Plan é um DAG versionado para trabalho complexo, com dependências e comandos
  de verificação.
- Todo e Plan permanecem sistemas distintos.
- TaskLink liga Todo, Plan e Goal por identidade/proveniência, sem propagar
  status automaticamente.
- Goal guarda objetivo e orçamento e pode estar `active`, `paused`, `complete` ou
  `blocked`;
- orçamento esgotado pausa o Goal e preserva a condição de retomada;
- A conclusão do Goal registra assurance `verified` ou `unverified`.
- O prompt de Goal deve orientar o agente a revisar a implementação e repetir
  o ciclo enquanto encontrar erros.
- `verify` e `self-review` são capacidades disponíveis ao agente ou usuário;
  não são gates mecânicos executados obrigatoriamente após toda mudança.

### 3.5 Sessões e contexto

- formato JSONL nativo, append-only e versionado;
- um writer por sessão;
- crash recovery, resume e branch/fork básicos;
- nenhum importador de sessão/config do Pit na v1;
- compactação manual e automática antes da chamada do modelo;
- overflow recovery;
- preservação de Todo, Plan, Goal e pares tool-call/tool-result;
- sem memória global, hindsight ou recall automático entre sessões;
- sem live/proactive/mid-turn compaction na v1.

### 3.6 Tools nativas

Catálogo mínimo:

- leitura de arquivo;
- escrita de arquivo;
- `apply_patch`;
- listagem de arquivos;
- busca textual;
- shell configurável;
- interação com usuário;
- Todo, Plan e Goal;
- controle de subagentes.

Contrato operacional:

- a tool de shell é genérica, stateless por chamada e recebe `cwd` explícito;
- no Windows, prefere PowerShell 7 e usa Windows PowerShell como fallback;
- processos da v1 rodam em foreground com streaming, timeout, cancelamento e
  teardown determinístico;
- jobs persistentes/em background ficam pós-v1;
- escrita usa preconditions, stale-read detection e troca atômica quando
  possível;
- somente built-ins comprovadamente read-only podem executar em paralelo;
- shell, writes e MCP tools executam em série;
- resultados entram no transcript em source order;
- outputs grandes entram no prompt como head/tail bounded e marcador; o bruto
  integral fica em artefato local recuperável por handle.

### 3.7 Busca rápida

`fff-search`, do projeto `dmtrKovalenko/fff`, é candidato para o backend em
processo. A ferramenta chamada `ffgrep` pertence a esse ecossistema; não foi
confirmado um pacote independente chamado `fffgrep`.

Essa escolha **não está fechada**. Alegações públicas de ganho de 20–50× não
foram reproduzidas. Antes de selecionar o backend, um POC no Windows deve medir:

- primeira busca e warmup;
- cem buscas repetidas;
- tempo, memória e correção contra `ripgrep`;
- atualização após criação, alteração, rename e remoção de arquivos;
- comportamento em árvore grande, `.gitignore`, arquivos binários e Unicode.

Até esse POC, `ripgrep` continua o baseline externo e `fff-search` é apenas o
candidato de índice quente.

### 3.8 Skills

- roots nativos globais e de projeto sob `.slim/skills`;
- compatibilidade de leitura com `.agents/skills`;
- `SKILL.md` com scripts, referências, assets e templates opcionais;
- startup carrega somente metadata;
- corpo e recursos carregam on-demand;
- usuário e agente podem ativar skills;
- trust do workspace ocorre uma vez, não por tool ou comando;
- não haverá plugin binário/dinâmico ou sidecar Node obrigatório na v1.

Skills, instruções e referências continuam disponíveis em todos os modos;
scripts executáveis de skills só rodam em `Auto`.

### 3.9 MCP

- transports `stdio` e Streamable HTTP;
- tools, resources e prompts;
- autenticação HTTP por headers/tokens estáticos;
- OAuth MCP fica pós-v1;
- inicialização lazy, timeout, cancelamento, caps e teardown;
- conexão/processo compartilhado pelo runtime raiz e subagentes;
- em `Plan`/`Read-only`, ficam disponíveis resources e prompts; MCP tools ficam
  ocultas;
- em `Auto`, MCP tools executam com os direitos do processo, sem approval.

### 3.10 Subagentes

- obrigatórios na v1;
- mesmo processo e runtime do agente raiz;
- profundidade máxima 1;
- herdam modelo e effort por padrão, com override explícito por spawn;
- cada filho recebe contexto, budget e sessão próprios;
- quatro filhos ativos por padrão e fila FIFO bounded de 32;
- limites configuráveis e erro explícito quando a fila estiver cheia;
- filhos read-only podem trabalhar em paralelo;
- sem worktrees, apenas um ator mutante executa por vez no workspace;
- stale checks continuam obrigatórios;
- operações mínimas: spawn, status, join, cancel e list;
- resultado final estruturado com status, session ID, usage e mensagem final;
- cancelamento do pai propaga aos filhos e não deixa processos órfãos;
- recursion, messaging lateral, Fusion e worktree pool não são planejados.

## 4. Explicitamente fora da v1

- Fusion multi-modelo e worktrees não planejados;
- LSP, DAP, AST, símbolos e repo map;
- browser/CDP, preview e web automation nativa;
- memória automática entre sessões;
- live/proactive/mid-turn compaction;
- background job manager persistente;
- inline TUI, servidor remoto, Linux/macOS e Unix PTY;
- worktrees para subagentes e subagentes recursivos;
- host de extensions TypeScript, sidecar Node obrigatório, plugins binários,
  ABI/WASM e marketplace de skills;
- OAuth MCP, salvo demanda futura com fluxo oficial;
- SQLite/FTS para sessões;
- PGO e otimizações dependentes de benchmark ainda não executado;
- compatibilidade de API, configuração ou armazenamento com o Pit.

## 5. Limites que ainda bloqueiam implementação

### 5.1 Contradição de scripts em modos sem mutação — resolvida

Skills, instruções e referências podem ser carregadas em qualquer modo. Scripts
executáveis de skills rodam **somente em `Auto`**. `Plan` e `Read-only` não
expõem shell, write, edit, patch ou delete/rename/mkdir; não existe executor
read-only de scripts na v1.

### 5.2 O que não é mais pergunta de produto

- timeouts, grace periods, caps numéricos e enumeração final de erros são
  defaults técnicos, cobertos por testes e POCs;
- capabilities de terminal são tratadas por fallback e matriz Windows;
- `fff-search` versus `ripgrep` é uma decisão de benchmark, não de preferência;
- a ligação entre fases e milestones é trabalho editorial de consolidação;
- POCs e gates podem ajustar números, mas não podem reabrir o contrato sem uma
  nova decisão explícita.

### 5.3 Contratos históricos rejeitados

- leitura/escrita compatível com JSONL v1–v3 do Pit;
- cinco famílias de providers como gate da v1;
- Fusion no loop inicial;
- aquecimento de repo map/LSP no boot;
- memory/hindsight na Fase 3;
- shim Node na v1;
- Kitty keyboard como requisito obrigatório;
- Plan sem bloqueio de tools;
- nomes `pit-*` para crates futuros.

## 6. Decisões registradas nas perguntas Q127–Q152

### 6.1 Instruções de projeto e trust

- Arquivos padrão: `.slim/instructions.md`, `AGENTS.md` e `CLAUDE.md`; a
  compatibilidade pode ser desativada.
- Hierarquia: carregar da raiz do repositório até o arquivo em foco; regras mais
  específicas complementam ou substituem conflitos.
- Arquivo grande usa budget próprio, marcador explícito e handle para o conteúdo
  integral.
- Trust inicial é por workspace; integração executável nova ou alterada pede
  nova confirmação. Não há confirmação por tool/comando.

### 6.2 Skills e MCP

- Colisão de skill: vence a root mais específica, nesta ordem: `--skills-dir`,
  projeto Slim, projeto compatível, global Slim, global compatível; shadowing é
  exibido.
- Skill inválida é ignorada com aviso diagnosticável; não bloqueia o startup.
- Tools MCP usam namespace canônico `mcp.<server>.<tool>`; a UI pode abreviar,
  mas logs e sessão preservam o nome completo.
- Definição MCP do projeto vence a global pelo mesmo ID e mostra a origem ativa.
- Catálogo MCP atualiza quando o servidor sinaliza `list_changed`.
- MCP resources/prompts só entram no contexto após seleção explícita.

### 6.3 Providers, reasoning e conclusão

- Reasoning textual fornecido pelo provider é persistido, mas aparece recolhido
  na TUI, separado da resposta final.
- Tool-call malformada recebe uma única tentativa de repair explícito.
- Schema de tools é estrito e aceita uma única correção; não há coerção silenciosa.
- Conclusão usa evento explícito de Goal/agente; não há sentinel de stdout nem
  inferência automática por exit code.

### 6.4 Filesystem e busca

- Não existem tools nativas de delete, rename ou mkdir na v1; essas mutações só
  ficam disponíveis via shell em `Auto`.
- `read` retorna linhas numeradas e respeita budget dinâmico.
- Edit com zero ou múltiplos matches falha com evidência e não altera nada.
- `write` exige precondition explícita para overwrite e faz troca atômica quando
  possível.
- `search` usa query estruturada: literal/regex, path, glob, case, contexto e
  limite.
- Backend de busca aquece lazy e atualiza incrementalmente; não há indexação
  global no startup.
- Hit de busca é revalidado no uso por path/hash/range antes de read/edit.

### 6.5 Shell

- cwd padrão é a raiz do workspace; chamadas podem declarar cwd explícito.
- Shell herda o ambiente completo do processo e aceita overrides por chamada.

### 6.6 Fechamento final do grill (Q151–Q152)

- Perfis nomeados `default`, `fast`, `deep` e `compact` configuram
  provider/model/effort; a sessão escolhe um perfil e subagentes herdam com
  override explícito.
- Telemetria é somente local na v1. Usage, latência, memória e eventos podem ser
  registrados localmente; não existe envio remoto, nem mesmo opt-in.

### 6.7 Decisões operacionais complementares

- Modo inicial: `Auto`; `Shift+Tab` alterna `Auto → Read-only → Plan → Auto`.
- Aprovar um Plan registra a versão e muda a sessão para `Auto`.
- Goal aceita budget de tokens opcional; esgotamento pausa, e não-progresso
  comprovado bloqueia com condição de retomada.
- Chamada de tool estritamente idêntica que falhou é sinalizada e bloqueada no
  mesmo turno; retry automático fica restrito a transporte seguro.
- Todo usa cinco estados (`pending`, `in_progress`, `completed`, `blocked`,
  `cancelled`) e no máximo um item `in_progress` por agente.
- Checks de Plan produzem receipts opcionais; não propagam status para Todo/Goal.
- Sessões usam JSONL append-only nativo, snapshots rebuildable, índice JSON
  rebuildable e leitura do schema atual + duas versões Slim anteriores.
- Branch cria nova sessão com `parent` e ponto de corte; crash recovery preserva
  prefixo válido e coloca o sufixo inválido em quarantine.
- Headless aceita argumento ou stdin, produz texto ou JSONL, e Plan termina com
  `approval_required` sem executar.
- Mensagens durante execução entram em FIFO visível de oito itens; `input_required`
  pausa headless de forma persistida.
- Compactação usa reserva projetada, `/compact` manual, threshold geral de 85%
  e 50% para janelas a partir de 1M; o mesmo modelo gera o resumo e o JSONL
  original permanece preservado.
- API keys ficam em `auth.json` com ACL; refresh tokens OAuth ficam no
  Credential Manager; `config.toml` segue CLI > env > projeto > global.
- Redaction remove valores conhecidos/headers de autenticação; retenção é sempre
  limpeza explícita do usuário.
- Subagentes herdam contexto selecionado, devolvem resultado estruturado, aparecem
  em bloco TUI expansível e fazem mutação serial no workspace.

## 7. Regra pós-grill

O `$batch-grill-me` está encerrado e não deve ser reaberto durante a
implementação. A próxima etapa é remover contradições históricas, escrever o
plano de POCs/gates e só então iniciar o código.
