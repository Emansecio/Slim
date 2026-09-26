# Insights para o Slim — mini-swe-agent e grok-build

> **Status de implementação:** pesquisa histórica e recomendações de design,
> não evidência de produto completo. O headless tem integração local; a TUI
> continua demo e Skills/MCP/subagentes/Todo-Plan-Goal ainda não estão ligados.
> Veja o [status atual](README.md).

> Navegação: [Índice](README.md) · [Decisões finais](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) · [Paridade Rust](RUST-CLI.md) · [Design TUI](DESIGN-SLIM-TUI.md) · [Viabilidade TUI](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)  
>
> Decisão posterior prevalente: recomendações de permissions/sandbox/guards
> restritivos neste estudo **não** são destino do Slim. O runtime aprovado é
> irrestrito conforme [RUST-CLI.md §1.1](RUST-CLI.md#11-exceção-normativa-de-paridade-execução-irrestrita).
>
> Revisão técnica dos snapshots públicos de 2026-08-19, orientada a um novo
> harness Rust. O objetivo é permitir que um engenheiro escolha o que implementar
> no Slim sem precisar reler os dois repositórios nem confiar em claims de README.
>
> **Precedência final:** esta pesquisa é evidência e não contrato. O escopo
> aprovado está em [DECISOES-GRILL-PRE-IMPLEMENTACAO.md](DECISOES-GRILL-PRE-IMPLEMENTACAO.md);
> recomendações sobre worktrees, Fusion, plugins, servidor remoto, memória
> global, Unix/inline ou fallback de provider não devem ser implementadas.

## 1. Resposta curta

Sim. Há mecanismos interessantes nos dois projetos, mas eles ensinam coisas
quase opostas:

- **mini-swe-agent** prova o valor de um núcleo curto, linear e substituível. O
  ganho vem de menos policy e menos superfície, não de uma otimização local
  sofisticada.
- **grok-build** mostra como um harness Rust grande trata sessões longas:
  compaction, pruning, filas, índices, worktrees, TUI, backpressure e benchmarks.

A melhor direção para o Slim é híbrida:

1. manter o composition root e o loop próximos da simplicidade do mini;
2. manter tools, guards e eventos tipados — não reduzir tudo a bash;
3. copiar do grok-build as políticas de contexto e os métodos de benchmark;
4. adiar worktree pool, leader process e tuning de allocator até existirem
   medições que justifiquem a complexidade;
5. não copiar a granularidade de 92 crates do grok-build.

Os seis insights de maior valor imediato são:

| Prioridade | Insight | Origem | Benefício esperado |
|---|---|---|---|
| P0 | core mínimo com adapters `agent/model/environment` | mini | startup, legibilidade e testabilidade |
| P0 | compaction independente do host, com limites e invariantes | grok-build | sessões longas sem corromper tool history |
| P0 | orçamento high-water/low-water para contexto pesado | grok-build | menos cache churn e menos erros de limite |
| P0 | fila versionada que combina follow-ups compatíveis | grok-build | menos round-trips e melhor agilidade |
| P1 | busca/indexação em background com degradação graciosa | grok-build | navegação rápida sem bloquear boot |
| P1 | benchmarks de steady-state com sessões grandes | grok-build | otimização baseada em gargalo real |

## 2. Escopo, método e nível de evidência

Foram revisados os fluxos de entrada, loop, providers/models, execução,
persistência, contexto, busca, TUI, extensibilidade, testes, benchmarks e
licenças. Nos pontos de desempenho, a leitura começou pelos padrões e
subsystems relevantes e depois foi validada nos símbolos que executam o fluxo.

Escala importa: “ponta a ponta” aqui significa cobrir os boundaries e caminhos
operacionais completos. Não significa alegar leitura manual de cada uma das
mais de duas mil unidades Rust do grok-build.

Os rótulos usados são:

- **Confirmado:** mecanismo presente no código ou teste do snapshot.
- **Evidência limitada:** intenção, comentário ou benchmark existente, mas sem
  resultado reproduzido nesta revisão.
- **Inferência:** impacto provável derivado do mecanismo; precisa de benchmark
  no Slim.
- **Claim:** afirmação pública do projeto não provada pelo snapshot sozinho.

Snapshots:

| Projeto | Commit revisado | Escala observada | Licença |
|---|---|---:|---|
| mini-swe-agent | [`25941c89`](https://github.com/SWE-agent/mini-swe-agent/tree/25941c89cfbc91eb40b3f8756348c91d9977d57e) | 221 arquivos; 59 fontes Python e 53 testes Python | MIT |
| grok-build | [`d92c5b0b`](https://github.com/xai-org/grok-build/tree/d92c5b0b8582fda358de1f97446aa74af44a464f) | 92 workspace members; cerca de 3.299 arquivos | Apache-2.0 no first-party |

O grok-build é um sync público de monorepo. O arquivo
[`SOURCE_REV`](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/SOURCE_REV#L1)
registra o commit de origem. Há código portado/vendorizado com notices próprios;
isso exige cuidado adicional se o Slim copiar implementação, não apenas ideia.

Não foi feito build completo do grok-build: seria desproporcional para uma
revisão estática de 92 crates. No mini, subsets focados confirmaram cache
control, tratamento de truncation e progresso batch. A reprodução local também
confirmou uma diferença de shell no Windows descrita adiante.

## 3. mini-swe-agent de ponta a ponta

### 3.1 Arquitetura

O projeto é construído em quatro abstrações:

```text
runner
  → escolhe Agent + Model + Environment
  → Agent.run(task)
      → model.query(messages)
      → environment.execute(action)
      → append observation
      → repetir até exit/limit
```

O contrato público de `Agent`, `Model` e `Environment` é pequeno. O CLI faz
merge de configuração, instancia os três objetos e entrega a execução ao agente.
O loop default inicializa system + user, repete `query → execute_actions`, salva
a trajetória em `finally` e encerra quando a última mensagem tem role `exit`.

Fontes:

- [protocolos centrais](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/src/minisweagent/__init__.py#L23-L92);
- [composition do CLI](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/src/minisweagent/run/mini.py#L53-L105);
- [loop e persistência](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/src/minisweagent/agents/default.py#L88-L190).

Implementações alternativas continuam simples:

- modelos tool-calling, Responses e text-based;
- ambientes local, Docker, Singularity, Bubblewrap, Contree e SWE-ReX;
- runners interativo, benchmark, ProgramBench e utilitários;
- Inspector/TUI separado do loop principal.

### 3.2 Onde ele ganha velocidade e agilidade

#### Núcleo pequeno

**Confirmado:** não existe uma cadeia grande de middleware no loop default. O
modelo produz ação, o ambiente executa e a observação volta ao histórico.

Impacto provável:

- menos tempo de boot e menos imports;
- menor custo cognitivo para modificar o harness;
- testes com menos doubles e estados intermediários;
- troca fácil de ambiente em benchmarks e sandboxes.

O README afirma startup mais rápido que Claude Code, mas não há artefato
versionado no snapshot que compare os dois. Trate como claim, não como número.

#### Uma única capability de execução

**Confirmado:** a superfície central é apenas `bash`, tanto no modo text-based
quanto no tool-calling. Isso quase elimina custo de schemas e incompatibilidade
entre providers.

O insight não é “toda tool deve virar shell”. O insight útil é que a superfície
wire deve ser menor que a superfície instalada. No Slim, isso favorece:

- quatro ou cinco tools essenciais sempre ativas;
- tool discovery para capabilities especializadas;
- `exec` como escape hatch universal;
- schemas curtos e estáveis.

#### Environment stateless

**Confirmado:** cada ação local abre um subprocesso novo; Docker usa `docker
exec` por ação. Não existe shell state escondido entre turns.

Vantagens:

- reproduzir uma ação é simples;
- trocar local por container é uma mudança de adapter;
- cwd/env persistentes precisam estar explícitos;
- uma ação ruim não envenena uma sessão de shell longa.

Tradeoff: processos curtos pagam spawn repetido e não atendem watchers, servers
ou jobs interativos. Para o Slim, o melhor desenho é stateless por default e um
subsystem explícito de jobs persistentes quando necessário.

#### Trajetória linear

**Confirmado:** a mesma lista append-only serve como histórico enviado e como
trajetória persistida. Isso torna debugging, replay e fine-tuning transparentes.

O Slim pode copiar a propriedade “evento persistido explica o request”, sem
copiar a ausência de compaction. Um materializer determinístico deve reconstruir
o request a partir de eventos + transforms versionados.

#### Paralelismo de benchmark

**Confirmado:** SWE-bench paraleliza instâncias com `ThreadPoolExecutor`. Isso
melhora throughput de avaliação, mas não a latência de um único agente. O model
pode emitir várias actions, porém `execute_actions()` executa a lista
sequencialmente.

Esse detalhe impede uma conclusão falsa: `parallel_tool_calls: true` não prova
execução paralela das tools.

### 3.3 Economia de tokens e contexto

O mini usa três mecanismos simples:

1. uma única tool;
2. observações acima de 10.000 chars viram head 5.000 + tail 5.000 + contagem
   de chars elididos;
3. cache control manual no fim do histórico para rotas Anthropic compatíveis.

Fontes:

- [tool bash única](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/src/minisweagent/models/utils/actions_toolcall.py#L11-L27);
- [truncação head/tail](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/src/minisweagent/config/mini.yaml#L111-L149);
- [cache marker](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/src/minisweagent/models/utils/cache_control.py#L49-L67).

Limite confirmado: não há compaction/summarization do histórico default. O
contexto cresce linearmente. Truncar cada observação impede um resultado gigante
de dominar o prompt, mas não resolve sessões longas.

### 3.4 Resiliência

Mecanismos confirmados:

- step, cost e wall-time limits;
- retries de modelo com backoff e classes não-retryable;
- kill do process group em timeout local;
- trajetória salva no `finally`;
- custo de FormatError contabilizado mesmo quando o parser rejeita a resposta;
- resposta/provider payload retido no erro para diagnóstico.

O Slim deve copiar os limits e a persistência, mas tornar budget session-owned.
O mini mantém também agregação global de custo; uma global mutável não deve ser
a autoridade de budget num runtime multi-sessão.

### 3.5 Limites e o que não copiar

#### Sentinel por stdout

O término depende de a primeira linha ser exatamente
`COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT` com exit code zero. No Windows, esta
revisão reproduziu:

```text
comando: echo 'COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT' + newline + echo 'done'
output:  "'COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT'\n"
exit:    0
```

As aspas simples viraram texto e o segundo comando não apareceu. A forma sem
aspas recomendada pelo prompt funciona, portanto isso não prova que o fluxo
principal sempre falha; prova que o protocolo depende de semântica de shell e é
frágil a variações legítimas. O Slim deve ter um evento/tool `complete` tipado.

Fonte do detector: [LocalEnvironment](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/src/minisweagent/environments/local.py#L24-L92).

#### Política crítica no prompt

Invariantes como exatamente uma action e o comando de submissão vivem no YAML.
Isso é ótimo para experimentação, mas fraco para segurança e lifecycle. No Slim:

- tom e workflow podem ficar em templates;
- abort, complete, permissions, budgets e path policy devem estar em código.

#### Shell-only

Shell-only favorece portabilidade de provider, mas perde:

- schemas e side-effect classification;
- guards por path/tool;
- outputs estruturados;
- cancelamento granular;
- paralelismo seguro de reads;
- observabilidade consistente;
- edição atômica e preconditions.

Conclusão: copiar a disciplina de superfície mínima, não a ausência de tools.

## 4. grok-build de ponta a ponta

### 4.1 Arquitetura

O fluxo principal é dividido em:

```text
xai-grok-pager-bin
  → TUI / headless / leader / stdio
  → xai-grok-shell (runtime + sessão)
  → xai-grok-agent / chat-state / sampler
  → xai-grok-tools + xai-grok-workspace
  → provider / filesystem / VCS / processos
```

Subsystems paralelos cuidam de compaction, MCP, hooks/plugins, config, auth,
memory, session search, codebase graph, worktrees, telemetry e ACP.

Fontes:

- [layout oficial](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/README.md#L95-L106);
- [composition root](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager-bin/src/main.rs#L1823-L1847);
- [entrypoints stdio/headless/leader](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-shell/src/agent/app.rs#L250-L323).

A separação por crate é clara, mas a escala é um alerta. O Slim não precisa de
um crate por feature pequena para aproveitar esses boundaries.

### 4.2 Context economy

#### Core desacoplado por traits

**Confirmado:** compaction é transport-agnostic e não depende do tipo concreto
de conversa. Traits representam item, role, builder, token counter, sampler,
commit e observador.

Essa é uma excelente base para o Slim: regras de seleção e fidelity podem ser
testadas sem provider, sessão ou TUI.

Fontes:

- [seams do compaction core](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/common/xai-grok-compaction/src/lib.rs#L1-L39);
- [seleção tail-keep](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/common/xai-grok-compaction/src/select.rs#L27-L147).

#### Split seguro de tools

**Confirmado:** a seleção caminha de trás para frente e ajusta o split para não
separar assistant tool requests dos tool results correspondentes. Também recusa
compactar quando a região recuperável é pequena demais.

Esse invariant deve existir no nível de tipo/teste do Slim. Um compactador não
pode produzir tool result órfão.

#### Threshold, piso e redução mínima

Defaults observados:

- trigger em 85% da context window;
- pelo menos 5.000 tokens compactáveis;
- summary só é aceito quando reduz para no máximo 80% do conteúdo substituído;
- target de 50% nos modos tail-keep;
- timeout de sampling 120 s e delay de retry 3 s;
- summary menor que 500 chars é tratado como degenerado.

Esses números são bons pontos de partida para um bench, não constantes para
copiar cegamente. Diferentes modelos, schemas e providers mudam o ótimo.

Fonte: [configuração compartilhada](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/common/xai-grok-compaction/src/intra_compaction/config.rs#L99-L242).

#### Pruning em escada

**Confirmado:** quando o request ainda não cabe, a ordem de sacrifício é:

1. history antigo;
2. tool results oversized;
3. step turns;
4. truncar o último item como último recurso.

Outra camada ativa pruning de tool results após 50% de ocupação, preserva turns
recentes, aplica head/tail no conteúdo grande e hard-clear no muito antigo.

Esse ordering é mais valioso que um threshold isolado: perde primeiro o conteúdo
mais recuperável e menos recente.

#### Summary útil ou retry

**Confirmado:** falhas determinísticas são separadas das transitórias; context
overflow não entra num retry inútil com o mesmo payload. Summary muito curto é
rejeitado e o texto rejeitado é retido com cap para diagnóstico.

Além do resumo, o runtime preserva active todos/subagents em reminder. O
princípio para o Slim é: estado operacional não pode depender de o summarizer
lembrá-lo em prosa.

#### Transcript recall

O summary inclui ponteiro para transcript integral quando detalhes foram
perdidos. Isso combina compaction agressiva com recuperação on-demand.

O Slim deve generalizar a ideia para:

- transcript compactado;
- outputs grandes;
- imagens removidas;
- artefatos de subagentes.

#### Orçamento de imagens cache-aware

**Confirmado:** o grok-build mede bytes JSON por um writer contador, sem alocar o
JSON inteiro. Quando o body chega a 47 MiB, remove imagens antigas até 25 MiB.
Esse high-water/low-water cria hysteresis: uma limpeza compra vários turns e
evita invalidar o prefix cache em toda request.

O placeholder diz explicitamente ao modelo que a imagem não está mais visível,
reduzindo hallucination sobre pixels removidos.

Fontes:

- [thresholds e contador](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-chat-state/src/image_budget.rs#L9-L90);
- [aplicação do budget](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-chat-state/src/image_budget.rs#L132-L181).

Esse padrão também serve para output stores e telemetry buffers: disparar no
high-water e recuperar até um low-water sensivelmente menor.

### 4.3 Agilidade do loop e da UI

#### Fila versionada e combine

**Confirmado:** prompts enfileirados possuem id, revision, owner e last editor.
Updates stale não precisam vencer. Follow-ups plain e compatíveis podem ser
fundidos numa única mensagem, preservando os textos originais para a UI.

O merge para em bash, command, skill expandida, input sintético, imagem de
follower ou item em edição. Isso evita que a otimização altere semântica.

Fontes:

- [regras puras de combine](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-prompt-queue/src/combine.rs#L1-L70);
- [wire/versionamento](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-prompt-queue/src/types.rs#L11-L79).

No Slim, isso pode cortar chamadas extras quando o usuário envia três adições
antes do próximo turn, sem misturar uma ação destrutiva ou imagem no batch.

#### Search em background e fail-soft

**Confirmado:** fuzzy file search combina walker respeitando ignore + matcher
Nucleo em background. Se não houver recursos para threads, degrada para browse
only ou disabled em vez de abortar o harness.

**Confirmado:** busca de sessões usa SQLite FTS5 rebuildable, triggers para
sincronizar o índice, lease single-flight cross-process e debounce de 500 ms por
sessão.

Fontes:

- [fuzzy search e degradação](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-fuzzy-file-search/src/lib.rs#L184-L220);
- [FTS de sessões](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-session-search/src/fts.rs#L170-L259);
- [debounce/single-flight](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-session-search/src/manager.rs#L238-L260).

Lição: índice é cache rebuildable, nunca fonte única. Boot não deve depender de
um índice perfeito.

#### Runtime compartilhado

O leader usa lock/socket para um runtime compartilhado entre clientes. É
plausível que isso reduza processos e caches duplicados, mas o snapshot mostra
o mecanismo, não números. Classificação: inferência até um A/B no Slim.

### 4.4 Worktrees e paralelismo

`xai-fast-worktree` oferece:

- `git worktree add --no-checkout`;
- copy paralelo sharded com channels bounded;
- clone CoW/reflink e snapshots quando o filesystem permite;
- cache warm de `git status`;
- sync de dirty state pré-computado;
- BTRFS/overlay detection;
- registry SQLite que diferencia `busy` de `corrupt`;
- cancelamento e GC.

Há bench do lifecycle create→warm→sync→use→release→cleanup. Um comentário afirma
economia aproximada de 800 ms ao pular `git clean` em repos grandes; não foi
reproduzida nesta revisão e não deve virar claim do Slim.

Fontes:

- [design do fast worktree](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-fast-worktree/src/lib.rs#L1-L88);
- [sync otimizado](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-fast-worktree/src/sync.rs#L135-L260);
- [bench de lifecycle](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-fast-worktree/src/bin/pool_perf_bench.rs#L1-L80).

Workflows usam semáforo e limitam fan-out pela capacidade da máquina. Default
alto não deve ser copiado sem medir provider limits, RAM e processo.

Importante: o próprio `WorktreePool` público informa uso de produção limitado.
Logo, copiar o pool completo hoje seria copiar uma hipótese, não uma vantagem
consolidada.

### 4.5 TUI e performance local

Pipeline confirmado:

1. Markdown parse + syntax highlight;
2. wrap por largura;
3. block output;
4. entry render;
5. viewport clipping;
6. ratatui diff escreve só células alteradas.

Parsing, highlight e wrap são cacheados por geração/largura. Scratch buffers são
reutilizados para entradas parcialmente visíveis. O mouse coalesce eventos em
cadência próxima de 60 fps e limita o delta por frame.

O insight principal não é escolher Ratatui. É separar:

- compute de conteúdo, cacheável;
- layout dependente de largura;
- composição visível;
- diff terminal.

O repositório contém benches com aproximadamente 3.000–3.200 entries e cerca de
5 MiB para medir steady-state depois de aquecer caches. Isso é evidência de boa
metodologia. Os valores “esperados” do documento `bench.md` não são resultados
medidos e não devem ser citados como prova.

Fontes:

- [bench de render](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/benches/render.rs#L1-L40);
- [cache warm no bench](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/benches/render.rs#L217-L259);
- [bench de resize](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/benches/resize.rs#L3-L30).

### 4.6 Resiliência e memória

Mecanismos interessantes:

- circuit breaker com janela bounded de 10.000 samples e contador incremental
  de falhas: error rate O(1);
- fsnotify com debounce e shutdown determinístico;
- channels bounded em hot paths;
- SQLite busy separado de corrupção;
- jemalloc e purge de arenas após memory cliffs em Unix;
- heap/tracing hooks;
- PTY E2E para auto-compaction em terminal pequeno.

Fontes:

- [sliding window O(1)](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/common/xai-circuit-breaker/src/window.rs#L1-L67);
- [fsnotify](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-fsnotify/src/watcher.rs#L1-L122);
- [classificação SQLite](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-fast-worktree/src/db/mod.rs#L173-L194);
- [hooks de allocator](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager-bin/src/main.rs#L1668-L1786).

Allocator tuning é P2: existe, mas ganho líquido não está quantificado e parte é
Unix-only. No Windows, o README classifica build host como best-effort.

## 5. Comparação direta

| Eixo | mini-swe-agent | grok-build | Direção para o Slim |
|---|---|---|---|
| core loop | linear e muito pequeno | distribuído em vários subsystems | core pequeno, serviços separados |
| tool surface | apenas bash | ampla e tipada | poucas ativas + discovery |
| providers | adapters via LiteLLM/OpenRouter | sampler/clients próprios | trait comum + adapters |
| environment | uma action stateless | workspace/exec complexo | stateless default + jobs explícitos |
| history | linear, sem compaction | pruning + várias compactações | event log + materializer + compaction |
| output grande | head/tail de 10k chars | pruning em escada + recall | head/tail + store recuperável |
| cache | marker simples no fim | políticas cache-aware, inclusive imagens | policy por provider e estabilidade medida |
| budgets | steps/custo/tempo | tokens/context/memória + semáforos | ledger único session-owned |
| follow-ups | append linear | queue versionada + combine seguro | copiar combine com invariantes |
| busca | shell | fuzzy background + FTS + graph | índices rebuildable, fail-soft |
| sandbox | environment adapters | sandbox/workspace/worktree | adapter clean; otimizar depois |
| TUI | Inspector separado | TUI completa e benchmarkada | renderer incremental por fases |
| avaliação | batch por instância | benches por subsystem e PTY | ambos: throughput + hot paths |
| complexidade | muito baixa | muito alta | 8–12 crates, não 92 |
| Windows | shell local frágil | build host best-effort | Windows como gate de primeira classe |

## 6. Arquitetura recomendada para o Slim

### 6.1 Quantidade de crates

Começar com boundaries amplos:

```text
slim-protocol   mensagens, events, tool schemas
slim-ai         providers, streaming, cache, retry
slim-agent      loop, queues, dispatcher, cancelamento
slim-context    ledger, pruning, compaction, recall
slim-session    event log, materializer, persistence, search
slim-tools      tools + Environment/ExecutionBackend
slim-tui        render/input
slim-cli        composition root e channels
slim-testkit    providers, clocks, PTY e golden fixtures
```

Separar novos crates só quando houver consumidor independente, compile-time
mensurável ou boundary de segurança real.

### 6.2 Loop central

O loop deve caber mentalmente numa página:

```text
materializar contexto
→ aplicar budget/pruning/compaction
→ provider stream
→ validar tool calls
→ executar batch pelo dispatcher
→ persistir eventos/resultados
→ repetir ou encerrar
```

Toda complexidade adicional entra por traits/serviços com contrato explícito.

### 6.3 ExecutionBackend

Adotar a seam do mini:

```text
Local | Container | Remote | Sandbox | Worktree
```

Mas o contrato deve devolver:

- stdout/stderr estruturados;
- exit code e timing;
- truncation metadata e full-output handle;
- cancellation;
- side-effect class;
- process ownership.

Comandos comuns podem ser stateless. Watchers e servers usam uma API de jobs
persistentes separada.

### 6.4 ContextBudgetEngine

Pipeline recomendado:

1. medir prompt, schemas, messages, imagens e headroom de output;
2. abaixo do soft floor: não tocar no histórico;
3. acima do prune floor: supersede/trim de tool outputs antigos;
4. acima do compaction trigger: escolher split seguro;
5. recusar compaction pequena ou sem redução material;
6. resumir com model role próprio e retry classificado;
7. preservar estado estruturado fora da prosa;
8. guardar transcript/output integral para recall;
9. reestimar o request final antes de enviar.

Começar testando, não fixando, os defaults 50% prune / 85% compact / 50%
target / 5k mínimo / 20% redução mínima.

### 6.5 High-water/low-water genérico

Implementar uma primitive reutilizável:

```text
se uso < high_water: não fazer nada
se uso >= high_water: recuperar até low_water
```

Aplicações:

- imagens inline;
- deferred outputs;
- diagnostics ring;
- TUI scrollback;
- session search queue;
- subagent result cache.

O gap precisa ser grande o bastante para evitar limpeza em todo turn.

### 6.6 Prompt queue

Cada prompt recebe:

- id estável;
- revision monotônica;
- owner/last editor opcional;
- kind;
- attachments;
- policy de drain.

Combinar apenas o prefixo contíguo de prompts textuais plain. Não combinar
command, bash, synthetic input, expanded skill, follower com imagem ou prompt em
edição. Persistir os textos originais para a UI.

### 6.7 Índices

Princípios:

- índices são rebuildable;
- bootstrap é single-flight;
- updates são debounced;
- reads durante contenção têm deadline;
- `busy`, `corrupt` e `unavailable` são estados diferentes;
- falta de worker degrada feature, não derruba sessão.

Aplicar primeiro a file search; adicionar FTS de sessões quando o volume
justificar.

### 6.8 Renderer

Separar caches por dependência:

- parse/highlight por generation de conteúdo;
- wrap por `(generation, width)`;
- layout por viewport/theme;
- frame diff por células;
- animations/input coalescidos por tick.

Evitar computar wrap integral de bloco colapsado se apenas três linhas são
visíveis — o próprio material de benchmark do grok-build aponta essa
oportunidade.

## 7. Backlog priorizado

| Ordem | Item | Impacto | Esforço | Gate de aceite |
|---:|---|---|---|---|
| 1 | protocol + core loop mínimo | alto | médio | provider/tool fake completa um turn determinístico |
| 2 | event log + materializer | alto | médio | replay gera request byte-equivalente |
| 3 | tool dispatcher único | alto | médio | normal/code/subagent passam pelos mesmos guards |
| 4 | ContextBudgetEngine | muito alto | alto | overflow, fidelity e reduction benches verdes |
| 5 | output store + recall | alto | médio | payload integral recuperável após head/tail |
| 6 | provider cache policy | alto | médio | prefix hash estável entre turns equivalentes |
| 7 | budgets step/token/cost/time | alto | baixo | nenhum limite pode ser bypassado por retry |
| 8 | prompt queue versionada/combine | médio-alto | médio | N follow-ups viram uma call sem perder display |
| 9 | file search background | médio | médio | boot não espera índice; fallback funciona |
| 10 | TUI cache + diff benches | médio | alto | long-session p95 dentro do budget definido |
| 11 | session FTS | médio | médio | rebuild/lease/debounce testados |
| 12 | fast worktree | condicional | alto | A/B prova ganho no workload real do Slim |
| 13 | leader/shared runtime | condicional | alto | reduz RSS/processos em medição multi-client |
| 14 | allocator purge | baixo inicialmente | médio | heap profile prova cliff e recuperação |

## 8. Plano de benchmarks do Slim

### 8.1 Startup

Medir cold e warm, p50/p95:

- `--help`;
- dry-run;
- primeira pintura TUI;
- tempo até aceitar input;
- tempo até construir primeiro request;
- RSS no idle.

Comparar com e sem discovery/index warmup. Nenhum índice opcional deve estar no
critical path.

### 8.2 Contexto

Por cenário:

- tokens de system prompt;
- tokens de schemas ativos;
- wire tokens antes/depois de pruning;
- cache prefix hash e hit ratio;
- tokens recuperados por mecanismo;
- latency/custo da compaction;
- reduction ratio;
- recall de constraints, errors, decisions e paths;
- fabricated facts/paths.

Cenários mínimos:

1. 100 turns pequenos;
2. 20 tool outputs grandes;
3. um turn gigante;
4. várias imagens;
5. compaction repetida;
6. subagentes e Todo/Plan ativos;
7. provider overflow e retry.

### 8.3 Runtime e tools

- spawn stateless vs job persistente;
- reads paralelos e writes sequenciais;
- cancelamento e process-tree cleanup;
- file search cold/warm;
- worktree create/acquire/release apenas se implementado;
- throughput de N tarefas independentes;
- semáforo sob saturação.

### 8.4 TUI

Usar corpus de sessão longa inspirado no grok-build:

- 3.000+ entries;
- cerca de 5 MiB de Markdown/tool output;
- caches frios e aquecidos separados;
- scroll, streaming append, resize, expand/collapse e theme change;
- CPU/frame, allocations/frame e p95;
- PTY em terminal pequeno e Windows.

Não usar os valores esperados de `bench.md` como baseline; rodar e gravar a
máquina, commit, perfil e corpus do Slim.

## 9. O que explicitamente não copiar

### Do mini-swe-agent

- bash como única capability interna;
- completion por string em stdout;
- dependência implícita da shell do host;
- histórico linear sem compaction;
- execução sequencial escondida atrás de `parallel_tool_calls`;
- budget global como autoridade;
- policies críticas apenas em YAML/prompt;
- claims de benchmark sem artefato preso ao commit.

### Do grok-build

- 92 crates antes de haver pressão real por essa divisão;
- WorktreePool tratado como feature madura sem validar callers;
- BTRFS/overlay/reflink como requisito cross-platform;
- jemalloc/purge antes de heap profiling;
- default alto de fan-out sem considerar provider/RAM;
- valores “esperados” em documentos como se fossem medição;
- cópia de código portado sem preservar notices e alterações;
- Windows como best-effort — para o Slim deve ser gate de primeira classe.

## 10. Licenciamento e uso responsável

- mini-swe-agent: MIT. Código pode ser reutilizado respeitando copyright e
  texto da licença.
- grok-build first-party: Apache-2.0. Reuso exige licença, notices e, quando
  aplicável, marcação de modificações.
- grok-build contém ports e vendored code sob licenças próprias. Antes de copiar
  uma função, verificar o `THIRD-PARTY-NOTICES` geral e o notice do crate.

Recomendação: copiar primeiro **mecanismos e invariantes** em implementação
original do Slim. Copiar código só quando a economia justificar o custo de
provenance e compliance.

Fontes de licença:

- [mini-swe-agent LICENSE](https://github.com/SWE-agent/mini-swe-agent/blob/25941c89cfbc91eb40b3f8756348c91d9977d57e/LICENSE.md);
- [grok-build LICENSE](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/LICENSE);
- [grok-build notices](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/THIRD-PARTY-NOTICES);
- [notices de tools portadas](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md).

## 11. Decisão recomendada

Começar o Slim com o espírito estrutural do mini-swe-agent e a disciplina de
contexto do grok-build:

```text
mini core
+ typed tool dispatcher
+ grok-style context engine
+ event-sourced session
+ versioned prompt queue
+ benchmark-first TUI/search
```

Não implementar worktree pool, shared leader ou allocator tuning na primeira
milestone. Esses itens devem ficar atrás de um A/B que demonstre impacto no
workload real.

O primeiro milestone está correto quando o loop continua pequeno, mas já possui
contratos de contexto, cancellation, persistência e medição que evitam a dívida
que o mini deliberadamente aceita e que o grok-build resolve com grande
complexidade.
